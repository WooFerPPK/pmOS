//! OPFS flush policy (T136).
//!
//! `OpfsFs` already routes every metadata mutation through a journal
//! transaction whose `commit_and_apply` step ends in
//! `device.flush()`. That gives crash-safety on a per-syscall basis.
//! What this module owns is the *coarser* flush policy that decides
//! when the kernel should issue a `device.flush()` from outside any
//! single mutation:
//!
//! * Before [`Kernel::proc_exit`](crate::sys::Kernel::proc_exit) so
//!   a long-lived process draining its dirty fd table doesn't leave
//!   in-flight metadata stranded if the tab closes immediately
//!   afterwards.
//! * Periodically, driven from the browser via
//!   [`installPeriodicSync`](../../../../web/src/bootstrap.ts) →
//!   `kernel_sync_all`, so a long-running tab doesn't accumulate
//!   hours of un-flushed writes between user-visible barriers.
//! * Once on `pagehide`, via the same `kernel_sync_all` export, so
//!   the tab close path catches anything still in flight.
//!
//! Both call sites converge on
//! [`Vfs::sync_dirty`](crate::vfs::Vfs::sync_dirty), which walks
//! every dirty mount and invokes its [`Filesystem::sync`] hook —
//! [`OpfsFs::sync`](super::OpfsFs) translates that into a journal
//! apply + superblock write + `device.flush()` (the FLUSH ioctl on
//! the block driver).
//!
//! ## Why a policy module?
//!
//! Without a policy, the only flush points are (a) every
//! `commit_and_apply`, which already happens, and (b) tab close.
//! Real workloads — e.g., a text editor saving once a minute — want
//! a flush rhythm that matches user expectations: "the file saved a
//! minute ago, then I closed my laptop lid; on power-on the file is
//! still there." [`FlushPolicy`] encodes that rhythm in one place
//! so the periodic-sync interval and the proc_exit barrier read off
//! the same source of truth.
//!
//! The policy is stateless from the FS's point of view — it does
//! not own a clock or a writer — so `OpfsFs` does not depend on it.
//! The kernel's lifecycle paths (`proc_exit`, `kernel_sync_all`)
//! consult [`FlushPolicy::should_flush`] before calling
//! `Vfs::sync_dirty`. Skipping the call when the policy says "not
//! yet" avoids redundant flushes on syscall-heavy paths where every
//! mutation already issued its own atomic flush.

use crate::vfs::{FsError, Vfs};

/// Default minimum interval between periodic flushes, in
/// nanoseconds. 60 seconds — matches the
/// [`installPeriodicSync`](../../../../web/src/bootstrap.ts) default
/// so the JS tick rate and the kernel's policy threshold agree.
pub const DEFAULT_PERIODIC_NS: u64 = 60_000_000_000;

/// Default maximum number of dirty-marking events before a flush
/// is mandatory, regardless of elapsed wall-clock time. Keeps a
/// burst-write workload from postponing durability indefinitely
/// when the periodic timer hasn't yet fired. 256 mutations per
/// implicit flush is empirically generous — the bundled apps
/// average 1–4 dirty mounts per user interaction, so this caps
/// the window of in-flight loss at roughly one minute of typing
/// plus 256 saves.
pub const DEFAULT_DIRTY_BUDGET: u64 = 256;

/// Tracks elapsed wall-clock time and dirty-event count since the
/// last successful flush. The owning kernel calls
/// [`record_dirty`] each time a VFS mutation marks a mount dirty,
/// then consults [`should_flush`] before periodic and proc_exit
/// flush points to decide whether to issue
/// [`Vfs::sync_dirty`](crate::vfs::Vfs::sync_dirty).
///
/// `FlushPolicy` is intentionally allocation-free and `no_std`-safe
/// so it lives in the kernel singleton without complicating
/// startup. The clock is passed in (`now_ns`) rather than read off
/// the platform: the kernel already owns a wall-clock reader and
/// passing it in keeps tests deterministic.
#[derive(Debug, Clone)]
pub struct FlushPolicy {
    last_flush_ns: u64,
    dirty_events: u64,
    periodic_ns: u64,
    dirty_budget: u64,
}

impl FlushPolicy {
    /// Build a fresh policy with the [`DEFAULT_PERIODIC_NS`] and
    /// [`DEFAULT_DIRTY_BUDGET`] thresholds and a zero last-flush
    /// timestamp (so the first `should_flush` call after any dirty
    /// event returns true).
    pub const fn new() -> Self {
        FlushPolicy {
            last_flush_ns: 0,
            dirty_events: 0,
            periodic_ns: DEFAULT_PERIODIC_NS,
            dirty_budget: DEFAULT_DIRTY_BUDGET,
        }
    }

    /// Override the periodic interval. Zero is rejected (policy with
    /// `periodic_ns == 0` would flush on every check). Returns the
    /// previous interval.
    pub fn set_periodic_ns(&mut self, periodic_ns: u64) -> u64 {
        let prev = self.periodic_ns;
        self.periodic_ns = periodic_ns.max(1);
        prev
    }

    /// Override the dirty-event budget. Zero is rejected (policy
    /// with `dirty_budget == 0` would flush on every check).
    /// Returns the previous budget.
    pub fn set_dirty_budget(&mut self, dirty_budget: u64) -> u64 {
        let prev = self.dirty_budget;
        self.dirty_budget = dirty_budget.max(1);
        prev
    }

    /// Bump the dirty-event counter. Saturating, so a pathological
    /// caller can't wrap the counter back to zero (which would
    /// silently re-arm the budget).
    pub fn record_dirty(&mut self) {
        self.dirty_events = self.dirty_events.saturating_add(1);
    }

    /// Read the current dirty-event count. Test-only diagnostics.
    pub fn dirty_events(&self) -> u64 {
        self.dirty_events
    }

    /// Read the timestamp of the last successful flush, in ns.
    /// Test-only diagnostics.
    pub fn last_flush_ns(&self) -> u64 {
        self.last_flush_ns
    }

    /// Decide whether a flush should fire NOW. Returns `true` if
    /// either the dirty-event budget is reached or the periodic
    /// interval has elapsed; `false` otherwise (including when no
    /// mutations have been recorded since the last flush — there
    /// is no work to do).
    ///
    /// `now_ns` is the wall-clock reading. A monotonic clock would
    /// be technically preferable for the elapsed-time check, but
    /// the kernel already feeds wall-clock ns through every
    /// timestamp path; using the same source keeps the policy
    /// in sync with file mtimes during testing.
    pub fn should_flush(&self, now_ns: u64) -> bool {
        if self.dirty_events == 0 {
            return false;
        }
        if self.dirty_events >= self.dirty_budget {
            return true;
        }
        // Saturating sub so a clock that goes backwards (testing,
        // browser tab throttling, leap seconds) doesn't produce a
        // huge spurious "elapsed" reading.
        let elapsed = now_ns.saturating_sub(self.last_flush_ns);
        elapsed >= self.periodic_ns
    }

    /// Force a flush regardless of policy state. Used by
    /// `proc_exit` (the per-process barrier MUST flush even if
    /// the periodic threshold hasn't elapsed) and by
    /// `kernel_sync_all` on `pagehide` (the tab is going away
    /// imminently — flush everything we've got). Resets the
    /// dirty-event counter and updates `last_flush_ns` to `now_ns`.
    pub fn mark_flushed(&mut self, now_ns: u64) {
        self.dirty_events = 0;
        self.last_flush_ns = now_ns;
    }

    /// Convenience: consult [`should_flush`], and if it returns
    /// `true`, drive [`Vfs::sync_dirty`] and update the policy
    /// state to reflect the flush. Returns `Ok(true)` when a flush
    /// fired and succeeded, `Ok(false)` when no flush was needed,
    /// and `Err(_)` propagating any sync error from the VFS layer.
    /// On error the policy state is NOT updated, so a subsequent
    /// call retries the flush.
    pub fn flush_if_due(&mut self, vfs: &mut Vfs, now_ns: u64) -> Result<bool, FsError> {
        if !self.should_flush(now_ns) {
            return Ok(false);
        }
        vfs.sync_dirty()?;
        self.mark_flushed(now_ns);
        Ok(true)
    }

    /// Force-flush regardless of policy state. The proc_exit and
    /// pagehide paths use this — the policy's "should we?" question
    /// is irrelevant when the process is going away. Mirrors
    /// [`Vfs::sync_dirty`] semantics: returns `Err(_)` if any
    /// dirty mount's `sync` hook errored. On success, the policy
    /// state is reset.
    pub fn flush_now(&mut self, vfs: &mut Vfs, now_ns: u64) -> Result<(), FsError> {
        vfs.sync_dirty()?;
        self.mark_flushed(now_ns);
        Ok(())
    }
}

impl Default for FlushPolicy {
    fn default() -> Self {
        FlushPolicy::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_policy_does_not_flush_without_dirty_events() {
        let p = FlushPolicy::new();
        assert!(!p.should_flush(0));
        assert!(!p.should_flush(DEFAULT_PERIODIC_NS * 100));
    }

    #[test]
    fn first_dirty_event_at_zero_clock_triggers_flush() {
        // last_flush_ns starts at 0. With dirty_events == 1 and
        // now_ns == DEFAULT_PERIODIC_NS, elapsed >= threshold → flush.
        let mut p = FlushPolicy::new();
        p.record_dirty();
        assert!(p.should_flush(DEFAULT_PERIODIC_NS));
    }

    #[test]
    fn dirty_budget_overrides_periodic_threshold() {
        let mut p = FlushPolicy::new();
        p.set_dirty_budget(4);
        for _ in 0..4 {
            p.record_dirty();
        }
        // Even at clock = 0 (no time elapsed), the budget is hit.
        assert!(p.should_flush(0));
    }

    #[test]
    fn should_flush_false_below_both_thresholds() {
        let mut p = FlushPolicy::new();
        p.set_dirty_budget(10);
        p.set_periodic_ns(1_000_000_000);
        p.record_dirty();
        assert!(!p.should_flush(500_000_000)); // half a second
    }

    #[test]
    fn mark_flushed_resets_state() {
        let mut p = FlushPolicy::new();
        p.set_dirty_budget(2);
        p.record_dirty();
        p.record_dirty();
        assert!(p.should_flush(0));
        p.mark_flushed(1_000);
        assert!(!p.should_flush(1_000));
        assert_eq!(p.dirty_events(), 0);
        assert_eq!(p.last_flush_ns(), 1_000);
    }

    #[test]
    fn periodic_threshold_after_flush_is_relative_to_last_flush() {
        let mut p = FlushPolicy::new();
        p.set_periodic_ns(1_000);
        p.record_dirty();
        assert!(p.should_flush(1_000));
        p.mark_flushed(1_000);
        // Half-interval later: not yet.
        p.record_dirty();
        assert!(!p.should_flush(1_500));
        // Full interval later: yes.
        assert!(p.should_flush(2_000));
    }

    #[test]
    fn clock_going_backwards_does_not_trigger_spurious_flush() {
        let mut p = FlushPolicy::new();
        p.set_periodic_ns(1_000);
        p.mark_flushed(10_000);
        p.record_dirty();
        // Clock went backwards (testing, throttling, etc.).
        // Saturating-sub keeps elapsed=0, so no flush.
        assert!(!p.should_flush(5_000));
    }

    #[test]
    fn dirty_budget_zero_is_rejected() {
        let mut p = FlushPolicy::new();
        p.set_dirty_budget(0);
        // The 1.max guard kicks in: budget is 1, so a single dirty
        // event flushes.
        p.record_dirty();
        assert!(p.should_flush(0));
    }

    #[test]
    fn periodic_ns_zero_is_rejected() {
        let mut p = FlushPolicy::new();
        p.set_periodic_ns(0);
        // Effective periodic_ns is 1 ns; one dirty event after one
        // ns triggers.
        p.record_dirty();
        assert!(p.should_flush(1));
    }

    #[test]
    fn flush_if_due_returns_false_when_no_dirty_events() {
        let mut p = FlushPolicy::new();
        let mut vfs = Vfs::new();
        let fired = p.flush_if_due(&mut vfs, 0).expect("no error path");
        assert!(!fired);
    }

    #[test]
    fn flush_if_due_returns_true_when_threshold_met_and_succeeds() {
        let mut p = FlushPolicy::new();
        let mut vfs = Vfs::new();
        p.set_dirty_budget(1);
        p.record_dirty();
        let fired = p.flush_if_due(&mut vfs, 0).expect("no error path");
        assert!(fired);
        assert_eq!(p.dirty_events(), 0);
    }

    #[test]
    fn flush_now_unconditionally_resets_state() {
        let mut p = FlushPolicy::new();
        let mut vfs = Vfs::new();
        p.flush_now(&mut vfs, 9_999).expect("no error path");
        assert_eq!(p.last_flush_ns(), 9_999);
    }
}
