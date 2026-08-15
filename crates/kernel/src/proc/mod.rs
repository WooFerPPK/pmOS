//! Process table, scheduler, and process lifecycle.
//!
//! Mirrors `data-model.md §1`. This module is the single owner of
//! every userland process's kernel-side state; every other kernel
//! module that needs process info (vfs, ipc, cap, syscall) reaches
//! through this one.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use abi::cap::CapSet;
use abi::ext::Pid;

pub mod loadavg;
pub mod sched;
pub mod signal;
pub mod table;

pub use sched::Scheduler;
pub use signal::{Signal, SignalInbox};
pub use table::{PidAllocator, ProcessTable, PROCESS_LIMIT_GLOBAL, PROCESS_LIMIT_PER_PARENT};

/// Exit status returned by `proc_wait`.
///
/// Distinguishes normal exit (with an 8-bit return code) from
/// signal-terminated (with the signal number). Modelled after POSIX
/// `waitpid` but narrower — PMos does not track "stopped by
/// signal" or "continued" in v1.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    /// Process called `proc_exit(code)`.
    Exited(i32),
    /// Process was terminated by signal.
    Signaled(u16),
    /// Process Worker itself crashed (host-side error).
    Crashed,
}

/// Life-cycle state of a process.
///
/// Transitions are documented in `data-model.md §1`:
///
/// ```text
/// Starting --(wasm instantiated)--> Ready
/// Ready    --(scheduler picks)----> Running
/// Running  --(syscall entry)------> BlockedOnSyscall
/// Running  --(exit / crash)-------> Zombie
/// BlockedOnSyscall --(result ready)--> Ready
/// BlockedOnIpc     --(data arrived)--> Ready
/// BlockedOnWait    --(child exited)--> Ready
/// Zombie   --(waitpid reaps it)----> Dead
/// ```
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProcState {
    /// Worker created, WASM still instantiating.
    Starting,
    /// Eligible to run.
    Ready,
    /// Currently executing on its Worker.
    Running,
    /// Parked on `Atomics.wait` in its own SAB, waiting for a syscall
    /// response.
    BlockedOnSyscall,
    /// Waiting on an IPC endpoint (pipe read, socket accept, etc.).
    BlockedOnIpc,
    /// Blocked in `proc_wait` on a child.
    BlockedOnWait,
    /// Exited but not yet reaped by the parent's `waitpid`.
    Zombie,
    /// Reaped; slot reclaimable.
    Dead,
}

impl ProcState {
    /// True iff a process in this state is eligible for the scheduler
    /// to pick up (i.e. can immediately run).
    #[inline]
    pub const fn is_runnable(self) -> bool {
        matches!(self, ProcState::Ready)
    }

    /// True iff the process is currently blocked on some event.
    #[inline]
    pub const fn is_blocked(self) -> bool {
        matches!(
            self,
            ProcState::BlockedOnSyscall | ProcState::BlockedOnIpc | ProcState::BlockedOnWait,
        )
    }

    /// True iff the process's kernel-side resources have already
    /// been released (the only thing retained is `exit_status`).
    #[inline]
    pub const fn is_terminal(self) -> bool {
        matches!(self, ProcState::Zombie | ProcState::Dead)
    }
}

/// Reason a process is currently blocked, used to wake it on the
/// correct event. Distinct from `ProcState` so the scheduler can
/// match on a single enum when delivering a wake.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockReason {
    /// Blocked in syscall dispatch on a specific request_id.
    Syscall { request_id: u32 },
    /// Blocked on an IPC endpoint.
    Ipc { endpoint_id: u32 },
    /// Blocked in `waitpid(pid, ...)`. `pid == -1` means "any child".
    Wait { pid: Pid },
}

/// A process control block. One per live process.
///
/// Fields mirror `data-model.md §1`. Browser-side handles (Worker
/// reference, SAB) are stored as opaque `u64` values so this type
/// stays no_std-friendly — the Platform maps them to real handles.
pub struct Process {
    pub pid: Pid,
    pub ppid: Pid,
    pub pgid: Pid,
    pub state: ProcState,
    pub block_reason: Option<BlockReason>,
    pub exit_status: Option<ExitStatus>,
    pub name: String,
    pub argv: Vec<String>,
    pub envp: BTreeMap<String, String>,
    pub cwd: String,
    pub caps: CapSet,
    /// Opaque host-side Worker handle. Meaning depends on Platform.
    pub worker_handle: u64,
    /// Opaque host-side SAB handle.
    pub sab_handle: u64,
    pub spawn_time_ns: u64,
    pub cpu_time_ns: u64,
    pub vm_size_bytes: u64,
    pub vm_peak_bytes: u64,
    pub mem_limit: Option<usize>,
}

impl Process {
    /// Create a new Process in `Starting` state. Used by
    /// `proc_spawn` before the Worker signals "ready".
    #[allow(clippy::too_many_arguments)]
    pub fn new_starting(
        pid: Pid,
        ppid: Pid,
        name: &str,
        argv: Vec<String>,
        envp: BTreeMap<String, String>,
        cwd: &str,
        caps: CapSet,
        worker_handle: u64,
        sab_handle: u64,
        spawn_time_ns: u64,
    ) -> Self {
        Process {
            pid,
            ppid,
            pgid: pid, // default to own pid; shell may change via setpgid-equivalent
            state: ProcState::Starting,
            block_reason: None,
            exit_status: None,
            name: name.to_owned(),
            argv,
            envp,
            cwd: cwd.to_owned(),
            caps,
            worker_handle,
            sab_handle,
            spawn_time_ns,
            cpu_time_ns: 0,
            vm_size_bytes: 0,
            vm_peak_bytes: 0,
            mem_limit: None,
        }
    }

    /// Record the current virtual memory size reported for this
    /// process. The high-water mark is monotonic for the lifetime
    /// of the process, matching `/proc/<pid>/status`'s VmPeak
    /// convention.
    pub fn record_memory_size(&mut self, bytes: u64) {
        self.vm_size_bytes = bytes;
        if bytes > self.vm_peak_bytes {
            self.vm_peak_bytes = bytes;
        }
    }

    /// Attempt a state transition. Returns `Err(())` if the
    /// transition is illegal (e.g. Dead -> Running). The kernel's
    /// syscall dispatch uses this to refuse stale wake events for
    /// a process that has already exited.
    #[allow(clippy::result_unit_err)]
    pub fn transition(&mut self, to: ProcState) -> Result<(), ()> {
        if !is_legal_transition(self.state, to) {
            return Err(());
        }
        self.state = to;
        // Clear block_reason whenever we leave a blocked state.
        if !self.state.is_blocked() {
            self.block_reason = None;
        }
        Ok(())
    }

    /// Mark the process as a zombie and record its exit status.
    /// All other resources (fd table, IPC endpoints, surfaces) are
    /// reclaimed by the kernel's reap path, which calls into this
    /// method as its final step.
    pub fn exit(&mut self, status: ExitStatus) {
        self.exit_status = Some(status);
        // Zombie is legal from any non-Dead state.
        self.state = ProcState::Zombie;
        self.block_reason = None;
    }
}

/// Is a transition from `from` to `to` legal?
fn is_legal_transition(from: ProcState, to: ProcState) -> bool {
    use ProcState::*;
    match (from, to) {
        // Starting: can become Ready (boot completed) or Zombie (crashed before run).
        (Starting, Ready) => true,
        (Starting, Zombie) => true,

        // Ready: scheduler dispatches, or a SIGKILL arrives before we run.
        (Ready, Running) => true,
        (Ready, Zombie) => true,

        // Running: can block on any blocking condition, exit, or be killed.
        (Running, BlockedOnSyscall) => true,
        (Running, BlockedOnIpc) => true,
        (Running, BlockedOnWait) => true,
        (Running, Ready) => true, // yield
        (Running, Zombie) => true,

        // Blocked: becomes ready when its event fires, or is killed.
        (BlockedOnSyscall, Ready) => true,
        (BlockedOnSyscall, Zombie) => true,
        (BlockedOnIpc, Ready) => true,
        (BlockedOnIpc, Zombie) => true,
        (BlockedOnWait, Ready) => true,
        (BlockedOnWait, Zombie) => true,

        // Zombie: only `proc_wait` reaps it to Dead.
        (Zombie, Dead) => true,

        // Everything else is illegal.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn sample_proc(pid: Pid) -> Process {
        Process::new_starting(
            pid,
            1,
            "test",
            Vec::new(),
            BTreeMap::new(),
            "/",
            CapSet::EMPTY,
            0,
            0,
            0,
        )
    }

    #[test]
    fn legal_transitions_pass() {
        let mut p = sample_proc(42);
        assert!(p.transition(ProcState::Ready).is_ok());
        assert!(p.transition(ProcState::Running).is_ok());
        assert!(p.transition(ProcState::BlockedOnSyscall).is_ok());
        assert!(p.transition(ProcState::Ready).is_ok());
        assert!(p.transition(ProcState::Running).is_ok());
        assert!(p.transition(ProcState::Zombie).is_ok());
        assert!(p.transition(ProcState::Dead).is_ok());
    }

    #[test]
    fn illegal_transitions_fail() {
        let mut p = sample_proc(42);
        // Starting -> Running is NOT legal (must go through Ready).
        assert!(p.transition(ProcState::Running).is_err());
        assert_eq!(p.state, ProcState::Starting);
        p.transition(ProcState::Ready).unwrap();
        // Ready -> BlockedOnSyscall is NOT legal (must be Running first).
        assert!(p.transition(ProcState::BlockedOnSyscall).is_err());
        p.transition(ProcState::Running).unwrap();
        p.transition(ProcState::Zombie).unwrap();
        // Zombie -> Running is NOT legal.
        assert!(p.transition(ProcState::Running).is_err());
    }

    #[test]
    fn exit_sets_zombie_and_status() {
        let mut p = sample_proc(42);
        p.transition(ProcState::Ready).unwrap();
        p.transition(ProcState::Running).unwrap();
        p.exit(ExitStatus::Exited(0));
        assert_eq!(p.state, ProcState::Zombie);
        assert_eq!(p.exit_status, Some(ExitStatus::Exited(0)));
    }

    #[test]
    fn leaving_blocked_state_clears_block_reason() {
        let mut p = sample_proc(42);
        p.transition(ProcState::Ready).unwrap();
        p.transition(ProcState::Running).unwrap();
        p.transition(ProcState::BlockedOnSyscall).unwrap();
        p.block_reason = Some(BlockReason::Syscall { request_id: 100 });
        p.transition(ProcState::Ready).unwrap();
        assert!(p.block_reason.is_none());
    }

    #[test]
    fn runnable_blocked_terminal_predicates() {
        assert!(ProcState::Ready.is_runnable());
        assert!(!ProcState::Running.is_runnable());

        assert!(ProcState::BlockedOnSyscall.is_blocked());
        assert!(ProcState::BlockedOnIpc.is_blocked());
        assert!(ProcState::BlockedOnWait.is_blocked());
        assert!(!ProcState::Ready.is_blocked());

        assert!(ProcState::Zombie.is_terminal());
        assert!(ProcState::Dead.is_terminal());
        assert!(!ProcState::Ready.is_terminal());
    }

    #[test]
    fn process_identity_fields_preserved_across_transitions() {
        let mut envp = BTreeMap::new();
        envp.insert("PATH".to_string(), "/bin".to_string());
        let p = Process::new_starting(
            5,
            1,
            "sh",
            alloc::vec!["sh".to_string(), "-c".to_string()],
            envp,
            "/home/user",
            CapSet::from_caps(&[abi::cap::Cap::DisplayClient]),
            0xDEAD_BEEF,
            0xCAFE_F00D,
            123_456_789,
        );
        assert_eq!(p.pid, 5);
        assert_eq!(p.ppid, 1);
        assert_eq!(p.pgid, 5);
        assert_eq!(p.name, "sh");
        assert_eq!(p.argv.len(), 2);
        assert_eq!(p.envp.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(p.cwd, "/home/user");
        assert!(p.caps.contains(abi::cap::Cap::DisplayClient));
        assert_eq!(p.worker_handle, 0xDEAD_BEEF);
        assert_eq!(p.sab_handle, 0xCAFE_F00D);
        assert_eq!(p.spawn_time_ns, 123_456_789);
        assert_eq!(p.vm_size_bytes, 0);
        assert_eq!(p.vm_peak_bytes, 0);
    }

    #[test]
    fn process_memory_accounting_tracks_current_and_peak() {
        let mut p = sample_proc(7);

        p.record_memory_size(8 * 1024);
        assert_eq!(p.vm_size_bytes, 8 * 1024);
        assert_eq!(p.vm_peak_bytes, 8 * 1024);

        p.record_memory_size(4 * 1024);
        assert_eq!(p.vm_size_bytes, 4 * 1024);
        assert_eq!(p.vm_peak_bytes, 8 * 1024);

        p.record_memory_size(16 * 1024);
        assert_eq!(p.vm_size_bytes, 16 * 1024);
        assert_eq!(p.vm_peak_bytes, 16 * 1024);
    }
}
