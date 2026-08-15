//! Capability table.
//!
//! Privilege in PMos is expressed exclusively as kernel-granted
//! capabilities. This module owns the per-process capability
//! set and exposes the three operations the syscall layer needs:
//! [`CapTable::check`], [`CapTable::list`], and
//! [`CapTable::grant`].
//!
//! The enforcement rules are non-negotiable per the project
//! constitution (Principle II + the 2026-04-13 delegation
//! clarification) and are covered by the isolation tests in
//! `crates/kernel/tests/cap.rs`:
//!
//! * Every process has exactly one [`CapSet`]. It is set at
//!   spawn time from the parent process's cap set and never
//!   widened except via an explicit `cap_grant` call from a
//!   holder of [`Cap::CapGrant`].
//! * `cap_grant` can only grant capabilities the caller holds.
//!   Widening a child beyond the caller's own cap set is the
//!   privilege-escalation failure mode this rule prevents.
//! * The initial cap sets for well-known roles (init, display
//!   server, desktop shell, sysmon, settings, ordinary apps)
//!   are defined in [`abi::cap::initial`] and are applied at
//!   process spawn time by the syscall layer — this module
//!   just owns the dynamic per-pid mapping.

use alloc::collections::BTreeMap;

use abi::cap::{Cap, CapSet};
use abi::ext::Pid;

/// Errors returned by [`CapTable`] operations.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CapError {
    /// The pid has no cap set in the table. Usually means the
    /// process was never registered, or has been reaped.
    NoSuchPid,
    /// `cap_grant`: caller did not hold [`Cap::CapGrant`].
    NotPermitted,
    /// `cap_grant`: caller tried to grant a cap it does not
    /// itself hold. This is the privilege-escalation guard.
    NotASubset,
}

/// Per-process capability state.
///
/// The kernel owns one instance, kept in sync with the process
/// table. Process creation calls [`CapTable::install`]; process
/// exit calls [`CapTable::remove`]. Every syscall that needs a
/// capability check goes through [`CapTable::check`].
pub struct CapTable {
    caps: BTreeMap<Pid, CapSet>,
}

impl CapTable {
    pub const fn new() -> Self {
        CapTable {
            caps: BTreeMap::new(),
        }
    }

    /// Install a fresh cap set for `pid`. Called by `proc_spawn`
    /// after the process table entry exists.
    pub fn install(&mut self, pid: Pid, caps: CapSet) {
        self.caps.insert(pid, caps);
    }

    /// Remove a process's cap set. Called by the kernel's reap
    /// path when a zombie is finally cleared.
    pub fn remove(&mut self, pid: Pid) -> Option<CapSet> {
        self.caps.remove(&pid)
    }

    /// How many processes are currently tracked.
    pub fn len(&self) -> usize {
        self.caps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.caps.is_empty()
    }

    /// `cap_check(pid, cap)`: does `pid` hold `cap`?
    pub fn check(&self, pid: Pid, cap: Cap) -> Result<bool, CapError> {
        let set = self.caps.get(&pid).ok_or(CapError::NoSuchPid)?;
        Ok(set.contains(cap))
    }

    /// `cap_list(pid)`: return the full cap set held by `pid`.
    pub fn list(&self, pid: Pid) -> Result<CapSet, CapError> {
        self.caps.get(&pid).copied().ok_or(CapError::NoSuchPid)
    }

    /// `cap_grant(granter_pid, target_pid, caps)`: add `caps`
    /// to `target_pid`'s set.
    ///
    /// The caller (`granter_pid`) must:
    ///
    /// 1. Hold [`Cap::CapGrant`]. (Only init holds it by
    ///    default in v1.)
    /// 2. Hold every capability in `caps` themselves. This is
    ///    the "no privilege escalation" rule — you cannot give
    ///    a child a cap you do not possess.
    ///
    /// On success, `target_pid`'s cap set is the union of its
    /// previous set and `caps`.
    pub fn grant(&mut self, granter: Pid, target: Pid, caps: CapSet) -> Result<(), CapError> {
        // Check that the granter holds CAP_GRANT.
        let granter_set = self.caps.get(&granter).ok_or(CapError::NoSuchPid)?;
        if !granter_set.contains(Cap::CapGrant) {
            return Err(CapError::NotPermitted);
        }
        // Check that the requested caps are a subset of what
        // the granter holds.
        if !granter_set.is_superset_of(caps) {
            return Err(CapError::NotASubset);
        }
        // Borrow the target set mutably; the granter check
        // above used an immutable borrow that's already dropped.
        let target_set = self.caps.get_mut(&target).ok_or(CapError::NoSuchPid)?;
        *target_set = target_set.union(caps);
        Ok(())
    }

    /// Revoke `caps` from `target_pid`. Only the target itself
    /// can shed its own capabilities in v1 (i.e. the caller
    /// must equal `target`). Used by the "drop privileges"
    /// idiom in userland programs. Returns `Err(NotPermitted)`
    /// if `caller != target` AND the caller lacks `CAP_GRANT`.
    pub fn drop_caps(&mut self, caller: Pid, target: Pid, caps: CapSet) -> Result<(), CapError> {
        if caller != target {
            // Only a CAP_GRANT holder can revoke someone else's caps.
            let caller_set = self.caps.get(&caller).ok_or(CapError::NoSuchPid)?;
            if !caller_set.contains(Cap::CapGrant) {
                return Err(CapError::NotPermitted);
            }
        }
        let target_set = self.caps.get_mut(&target).ok_or(CapError::NoSuchPid)?;
        // Remove every bit that is set in `caps`.
        target_set.0 &= !caps.0;
        Ok(())
    }
}

impl Default for CapTable {
    fn default() -> Self {
        CapTable::new()
    }
}
