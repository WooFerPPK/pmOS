//! Process table and PID allocator.
//!
//! The process table is the single source of truth for live
//! processes. It is a `BTreeMap<Pid, Process>` — O(log n) per
//! lookup, O(n) iteration — which is adequate for the
//! low-hundreds-of-processes workloads v1 targets.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use abi::ext::Pid;

use super::{BlockReason, Process, ProcState};

/// Monotonic PID allocator.
///
/// PIDs are allocated sequentially starting at 1 (init). PIDs are
/// not reused within a boot — the v1 rule is that a 32-bit counter
/// is plenty for any reasonable session. A tab that runs long
/// enough to wrap 2^31 pids has already blown every other budget.
#[derive(Debug)]
pub struct PidAllocator {
    next: Pid,
}

impl PidAllocator {
    /// Create a fresh allocator. The first allocated pid will be 1.
    pub const fn new() -> Self {
        PidAllocator { next: 1 }
    }

    /// Allocate a new pid.
    pub fn allocate(&mut self) -> Pid {
        let pid = self.next;
        self.next = self.next.checked_add(1).expect("pid space exhausted");
        pid
    }

    /// The next pid that `allocate` will return, for test assertions.
    pub fn peek(&self) -> Pid {
        self.next
    }
}

impl Default for PidAllocator {
    fn default() -> Self {
        PidAllocator::new()
    }
}

/// The process table.
///
/// Ownership model: the table owns every live process's `Process`
/// struct. Lookups return borrows; mutation goes through the table's
/// mutating methods so invariants (state transitions, parent/child
/// linkage on exit) can be enforced in one place.
pub struct ProcessTable {
    alloc: PidAllocator,
    procs: BTreeMap<Pid, Process>,
}

impl ProcessTable {
    /// Create an empty process table.
    pub const fn new() -> Self {
        ProcessTable {
            alloc: PidAllocator::new(),
            procs: BTreeMap::new(),
        }
    }

    /// Allocate a fresh pid without yet inserting a Process. Used
    /// by `proc_spawn` which needs the pid before the Worker is
    /// instantiated.
    pub fn allocate_pid(&mut self) -> Pid {
        self.alloc.allocate()
    }

    /// Insert a newly-created process. The pid must match
    /// `proc.pid` and must not already be in use.
    pub fn insert(&mut self, proc: Process) -> Result<(), InsertError> {
        if self.procs.contains_key(&proc.pid) {
            return Err(InsertError::PidInUse);
        }
        self.procs.insert(proc.pid, proc);
        Ok(())
    }

    /// Return true iff `pid` exists in the table and is not Dead.
    pub fn is_alive(&self, pid: Pid) -> bool {
        self.procs
            .get(&pid)
            .map(|p| p.state != ProcState::Dead)
            .unwrap_or(false)
    }

    /// Borrow a process by pid.
    pub fn get(&self, pid: Pid) -> Option<&Process> {
        self.procs.get(&pid)
    }

    /// Mutably borrow a process by pid.
    pub fn get_mut(&mut self, pid: Pid) -> Option<&mut Process> {
        self.procs.get_mut(&pid)
    }

    /// Number of processes (including zombies but excluding Dead).
    pub fn live_count(&self) -> usize {
        self.procs
            .values()
            .filter(|p| p.state != ProcState::Dead)
            .count()
    }

    /// Iterate over live pids in ascending order. Used by `/proc`
    /// directory enumeration and the scheduler.
    pub fn live_pids(&self) -> Vec<Pid> {
        self.procs
            .iter()
            .filter(|(_, p)| p.state != ProcState::Dead)
            .map(|(pid, _)| *pid)
            .collect()
    }

    /// Attempt a state transition on `pid`.
    pub fn transition(&mut self, pid: Pid, to: ProcState) -> Result<(), TransitionError> {
        let proc = self.procs.get_mut(&pid).ok_or(TransitionError::NoSuchPid)?;
        proc.transition(to).map_err(|_| TransitionError::Illegal)
    }

    /// Record that `pid` has exited with the given status. Sets
    /// state to Zombie. Callers should have already reclaimed
    /// file-descriptor / IPC / surface resources by the time this
    /// is invoked (via `crate::fd::drop_process`, `crate::ipc::*`,
    /// and the display server's client-destroyed event). This
    /// method only updates the process table.
    pub fn exit(&mut self, pid: Pid, status: super::ExitStatus) -> Result<(), ExitError> {
        let proc = self.procs.get_mut(&pid).ok_or(ExitError::NoSuchPid)?;
        if proc.state == ProcState::Dead {
            return Err(ExitError::AlreadyDead);
        }
        proc.exit(status);
        Ok(())
    }

    /// Reap a zombie. Removes the process from the table entirely
    /// and returns its exit status. Called by `proc_wait` after a
    /// successful match.
    pub fn reap(&mut self, pid: Pid) -> Option<super::ExitStatus> {
        let proc = self.procs.get_mut(&pid)?;
        if proc.state != ProcState::Zombie {
            return None;
        }
        let status = proc.exit_status;
        // Transition to Dead before removing so invariants hold
        // at every observation point.
        let _ = proc.transition(ProcState::Dead);
        // Now remove from the table. Post-reap, `is_alive` and
        // `get` both return None for this pid.
        self.procs.remove(&pid);
        status
    }

    /// Find any zombie child of `parent_pid`, optionally filtered
    /// by a specific child pid (`any = false`) or "any child"
    /// (`any = true`).
    pub fn find_zombie_child(
        &self,
        parent_pid: Pid,
        target: ZombieTarget,
    ) -> Option<Pid> {
        for (pid, proc) in &self.procs {
            if proc.state != ProcState::Zombie {
                continue;
            }
            if proc.ppid != parent_pid {
                continue;
            }
            match target {
                ZombieTarget::Any => return Some(*pid),
                ZombieTarget::Specific(want) if *pid == want => return Some(*pid),
                _ => continue,
            }
        }
        None
    }

    /// Count how many live processes have `parent_pid` as their parent.
    pub fn child_count(&self, parent_pid: Pid) -> usize {
        self.procs
            .values()
            .filter(|p| p.ppid == parent_pid && p.state != ProcState::Dead)
            .count()
    }

    /// Mark a process as blocked on some event. Convenience wrapper
    /// that transitions the state and records the block reason in
    /// one atomic operation.
    pub fn block(
        &mut self,
        pid: Pid,
        state: ProcState,
        reason: BlockReason,
    ) -> Result<(), TransitionError> {
        assert!(state.is_blocked(), "block() requires a blocked ProcState");
        let proc = self.procs.get_mut(&pid).ok_or(TransitionError::NoSuchPid)?;
        proc.transition(state).map_err(|_| TransitionError::Illegal)?;
        proc.block_reason = Some(reason);
        Ok(())
    }

    /// Set the block reason on `pid`. Called alongside a transition
    /// to BlockedOn*; no-op if pid is not in the table.
    pub fn set_block_reason(&mut self, pid: Pid, reason: BlockReason) {
        if let Some(p) = self.procs.get_mut(&pid) {
            p.block_reason = Some(reason);
        }
    }

    /// Clear any previously-set block reason on `pid`. Used by the
    /// syscall dispatcher when unblocking a parked process.
    pub fn clear_block_reason(&mut self, pid: Pid) {
        if let Some(p) = self.procs.get_mut(&pid) {
            p.block_reason = None;
        }
    }

    /// Wake a process that was blocked on a specific syscall.
    /// Returns true iff the process was indeed blocked on that
    /// request_id and is now Ready.
    pub fn wake_syscall(&mut self, pid: Pid, request_id: u32) -> bool {
        let Some(proc) = self.procs.get_mut(&pid) else {
            return false;
        };
        let matches = matches!(
            proc.block_reason,
            Some(BlockReason::Syscall { request_id: r }) if r == request_id,
        );
        if !matches {
            return false;
        }
        proc.transition(ProcState::Ready).is_ok()
    }
}

impl Default for ProcessTable {
    fn default() -> Self {
        ProcessTable::new()
    }
}

/// Which zombie child `find_zombie_child` should return.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ZombieTarget {
    /// Any zombie child with the given parent.
    Any,
    /// A specific zombie child pid.
    Specific(Pid),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InsertError {
    PidInUse,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TransitionError {
    NoSuchPid,
    Illegal,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExitError {
    NoSuchPid,
    AlreadyDead,
}
