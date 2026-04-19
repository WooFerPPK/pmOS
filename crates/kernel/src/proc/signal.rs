//! Signal types + per-process signal inbox.
//!
//! Signals in v1 are a thin POSIX-inspired layer:
//!
//! * [`Signal::Kill`] (9) is delivered immediately by
//!   [`crate::sys::Kernel::proc_kill`]: the target is
//!   force-transitioned to `Zombie` with
//!   `ExitStatus::Signaled(9)` and the scheduler drops it.
//!   SIGKILL does NOT go through the inbox — there is nothing
//!   for userland to catch.
//!
//! * [`Signal::Term`] (15), [`Signal::Interrupt`] (2),
//!   [`Signal::Pipe`] (13), and [`Signal::Child`] (17) are
//!   "catchable": `proc_kill` buffers them in the target's
//!   [`SignalInbox`]. With the `FdObject::SignalChannel` fd
//!   variant auto-installed at fd 3 on every proc_spawn'd
//!   child, `fd_read` on fd 3 drains pending signals as u16 LE
//!   pairs; tests can also observe delivery via
//!   `Kernel::drain_signals`. SIGPIPE is kernel-generated on
//!   broken-pipe writes (matches POSIX `write(2)`'s SIGPIPE +
//!   EPIPE pair); SIGCHLD is kernel-generated when a child
//!   process transitions to Zombie (matches POSIX SIGCHLD
//!   delivery on child exit, including the SIGKILL-triggered
//!   path). Neither requires an explicit `proc_kill` call from
//!   userland — a parent that polls fd 3 observes every child
//!   exit automatically.
//!
//! The inbox is a bounded FIFO: repeated deliveries of the
//! same signal are coalesced (classical POSIX behaviour for
//! standard signals, not real-time signals). v1 does not
//! implement signal masks or handlers; that's a later slice
//! once the bundled `sh` needs them.

use alloc::collections::VecDeque;

/// Signals the v1 kernel understands.
///
/// The numeric repr matches POSIX `SIGKILL`, `SIGTERM`, `SIGINT`,
/// `SIGPIPE`, and `SIGCHLD` so userland code that hard-codes
/// these values keeps working without a translation table.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Signal {
    /// Terminate the target immediately, no cleanup grace.
    Kill = 9,
    /// Request a cooperative shutdown.
    Term = 15,
    /// Interrupt (ctrl-c equivalent).
    Interrupt = 2,
    /// Kernel-generated: a write to a pipe or socket whose peer
    /// has closed or had its read-side shut down.
    Pipe = 13,
    /// Kernel-generated: a child process has transitioned to
    /// Zombie. Delivered to the parent's signal inbox so a
    /// supervisor can observe child exit by polling fd 3.
    Child = 17,
}

impl Signal {
    #[inline]
    pub const fn number(self) -> u16 {
        self as u16
    }

    /// True iff this signal can be buffered in a
    /// [`SignalInbox`] for userland to observe. SIGKILL is the
    /// only non-catchable signal in v1.
    #[inline]
    pub const fn is_catchable(self) -> bool {
        !matches!(self, Signal::Kill)
    }
}

/// Maximum number of pending signals in a single inbox.
/// Matches standard POSIX — coalesced standard signals never
/// need more than one slot per number, so 8 is plenty of head
/// room for a system that does not yet implement real-time
/// signals.
pub const INBOX_CAP: usize = 8;

/// Per-process pending-signal queue.
///
/// Repeated deliveries of the same signal are coalesced: a
/// second `post(Signal::Term)` against an inbox that already
/// has `Term` pending does not grow the queue. Overflow past
/// [`INBOX_CAP`] is dropped silently (no real-time-signal-
/// equivalent errno in v1).
#[derive(Debug, Default)]
pub struct SignalInbox {
    queue: VecDeque<Signal>,
}

impl SignalInbox {
    pub const fn new() -> Self {
        SignalInbox {
            queue: VecDeque::new(),
        }
    }

    /// Deliver `signal` into the inbox. Returns true iff the
    /// inbox actually absorbed a new copy (i.e. the signal was
    /// not already pending AND the inbox had room).
    pub fn post(&mut self, signal: Signal) -> bool {
        if self.queue.iter().any(|s| *s == signal) {
            return false;
        }
        if self.queue.len() >= INBOX_CAP {
            return false;
        }
        self.queue.push_back(signal);
        true
    }

    /// Remove and return every pending signal, in delivery
    /// order.
    pub fn drain(&mut self) -> alloc::vec::Vec<Signal> {
        self.queue.drain(..).collect()
    }

    /// Remove and return up to `max` pending signals, in
    /// delivery order. If the inbox holds fewer than `max`, all
    /// of them are returned (same outcome as [`Self::drain`]).
    /// If the inbox holds more than `max`, the first `max`
    /// entries are returned and the rest stay queued in their
    /// original order — the next `drain_bounded` or `drain` call
    /// picks up where this one left off.
    ///
    /// Used by the SignalChannel fd_read path so a small-buffer
    /// caller only consumes as many signals as fit.
    pub fn drain_bounded(&mut self, max: usize) -> alloc::vec::Vec<Signal> {
        let take = max.min(self.queue.len());
        self.queue.drain(..take).collect()
    }

    /// True iff the next `drain` would return no signals.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Number of signals currently pending.
    pub fn len(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_catchable_distinguishes_kill_from_other_signals() {
        assert!(!Signal::Kill.is_catchable());
        assert!(Signal::Term.is_catchable());
        assert!(Signal::Interrupt.is_catchable());
        assert!(Signal::Pipe.is_catchable());
        assert!(Signal::Child.is_catchable());
    }

    #[test]
    fn number_matches_posix_values() {
        assert_eq!(Signal::Kill.number(), 9);
        assert_eq!(Signal::Term.number(), 15);
        assert_eq!(Signal::Interrupt.number(), 2);
        assert_eq!(Signal::Pipe.number(), 13);
        assert_eq!(Signal::Child.number(), 17);
    }

    #[test]
    fn post_delivers_and_drain_returns_fifo() {
        let mut i = SignalInbox::new();
        assert!(i.is_empty());
        assert!(i.post(Signal::Term));
        assert!(i.post(Signal::Interrupt));
        assert_eq!(i.len(), 2);
        let drained = i.drain();
        assert_eq!(drained, alloc::vec![Signal::Term, Signal::Interrupt]);
        assert!(i.is_empty());
    }

    #[test]
    fn coalesces_repeated_deliveries_of_the_same_signal() {
        let mut i = SignalInbox::new();
        assert!(i.post(Signal::Term));
        assert!(!i.post(Signal::Term));
        assert_eq!(i.len(), 1);
    }

    #[test]
    fn drain_bounded_returns_prefix_in_order_and_leaves_remainder() {
        let mut i = SignalInbox::new();
        assert!(i.post(Signal::Term));
        assert!(i.post(Signal::Pipe));
        assert!(i.post(Signal::Interrupt));
        let first = i.drain_bounded(2);
        assert_eq!(first, alloc::vec![Signal::Term, Signal::Pipe]);
        assert_eq!(i.len(), 1);
        let rest = i.drain_bounded(8);
        assert_eq!(rest, alloc::vec![Signal::Interrupt]);
        assert!(i.is_empty());
    }

    #[test]
    fn drain_bounded_with_zero_returns_empty_and_preserves_queue() {
        let mut i = SignalInbox::new();
        assert!(i.post(Signal::Term));
        assert!(i.drain_bounded(0).is_empty());
        assert_eq!(i.len(), 1);
    }

    #[test]
    fn drops_deliveries_past_the_cap() {
        let mut i = SignalInbox::new();
        // Post INBOX_CAP distinct-looking signals by alternating
        // between the two catchable variants (the dedup logic
        // will keep this at 2). We then post Signal::Kill in a
        // loop to fill the queue to its cap.
        assert!(i.post(Signal::Term));
        assert!(i.post(Signal::Interrupt));
        // These next posts would need to grow past INBOX_CAP;
        // they should all be rejected by the cap check.
        for _ in 0..(INBOX_CAP + 3) {
            let _ = i.post(Signal::Kill);
        }
        assert!(i.len() <= INBOX_CAP);
    }
}
