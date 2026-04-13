//! Cooperative scheduler.
//!
//! "Cooperative" here means: the browser's runtime preempts
//! Workers on its own schedule, so from the kernel's point of view
//! every process always makes progress between syscall entries.
//! The kernel's job is not to time-slice — the runtime does that
//! for us — but to **decide which process is currently
//! "current"** for single-threaded resources like the syscall
//! dispatcher, the display server client list, and the
//! drawing-surface commit pipeline.
//!
//! The scheduler is therefore a ready queue: a round-robin pick
//! from processes whose state is `Ready`. `Running` is the
//! currently-dispatched process. When the current process blocks,
//! its Worker actually parks on `Atomics.wait`; the kernel then
//! picks the next `Ready` process for its own "who's current"
//! bookkeeping. The real CPU cycles are handled by the host
//! scheduler (Chromium / Firefox / WebKit) — the kernel only
//! tracks logical order so things like "which client is the
//! display server currently servicing" are unambiguous.

use alloc::collections::VecDeque;

use abi::ext::Pid;

use super::{ProcState, ProcessTable};

/// Ready-queue scheduler.
///
/// The scheduler owns a small FIFO of pids whose state the kernel
/// has promoted to `Ready`. A pop is a round-robin step; a push
/// to the back enqueues a newly-unblocked or newly-yielding
/// process. The actual state of each process lives in the
/// `ProcessTable`; the scheduler only holds pids.
pub struct Scheduler {
    ready_queue: VecDeque<Pid>,
    /// The currently "running" pid, if any — the one the kernel
    /// considers the holder of exclusive single-threaded resources.
    current: Option<Pid>,
}

impl Scheduler {
    pub const fn new() -> Self {
        Scheduler {
            ready_queue: VecDeque::new(),
            current: None,
        }
    }

    /// Push a pid onto the back of the ready queue.
    pub fn enqueue(&mut self, pid: Pid) {
        // Deliberate O(n) dedup: the queue never grows beyond tens
        // of entries in practice and having at most one copy of any
        // pid simplifies every caller.
        if !self.ready_queue.iter().any(|p| *p == pid) && self.current != Some(pid) {
            self.ready_queue.push_back(pid);
        }
    }

    /// Remove a pid from both the ready queue and the "current"
    /// slot, regardless of where it was. Used by `proc_kill` and
    /// the process-exit path.
    pub fn remove(&mut self, pid: Pid) {
        self.ready_queue.retain(|p| *p != pid);
        if self.current == Some(pid) {
            self.current = None;
        }
    }

    /// Pick the next pid to run. Transitions the picked process
    /// from `Ready` to `Running` in the process table. Returns
    /// `None` if no process is ready.
    ///
    /// The caller (syscall dispatcher, display-server servicer,
    /// etc.) uses the returned pid as "the current process" for
    /// the duration of its next unit of work.
    pub fn pick_next(&mut self, table: &mut ProcessTable) -> Option<Pid> {
        // Put the previously-current pid back on the queue if it's
        // still Ready (round-robin); the caller is responsible for
        // having set it to Ready if the process yielded.
        if let Some(prev) = self.current.take() {
            if let Some(proc) = table.get(prev) {
                if proc.state == ProcState::Ready {
                    self.ready_queue.push_back(prev);
                }
            }
        }

        // Find the first queued pid that is actually Ready in the
        // process table. Skip stale entries (may have been killed
        // or transitioned to Blocked between enqueue and pick).
        while let Some(pid) = self.ready_queue.pop_front() {
            match table.get_mut(pid) {
                Some(proc) if proc.state == ProcState::Ready => {
                    if proc.transition(ProcState::Running).is_err() {
                        continue;
                    }
                    self.current = Some(pid);
                    return Some(pid);
                }
                _ => continue,
            }
        }
        None
    }

    /// Return the currently-running pid, if any.
    pub fn current(&self) -> Option<Pid> {
        self.current
    }

    /// Number of ready processes queued.
    pub fn ready_len(&self) -> usize {
        self.ready_queue.len()
    }

    /// Reset the scheduler. Used by tests.
    pub fn clear(&mut self) {
        self.ready_queue.clear();
        self.current = None;
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Scheduler::new()
    }
}
