//! Process-table and scheduler isolation tests (T046).
//!
//! Runs via `cargo test -p kernel --features native-platform`. No
//! browser involved. These tests are the Principle X gate for the
//! `proc` subsystem: any change that breaks lifecycle transitions,
//! PID allocation, scheduler fairness, or zombie reaping fails
//! here before it can reach integration.

#![cfg(feature = "native-platform")]

use std::collections::BTreeMap;

use abi::cap::{Cap, CapSet};
use abi::ext::Pid;

use kernel::proc::{
    table::{ExitError, InsertError, ProcessTable, TransitionError, ZombieTarget},
    BlockReason, ExitStatus, Process, ProcState, Scheduler,
};

fn make_process(pid: Pid, ppid: Pid, name: &str, caps: CapSet) -> Process {
    Process::new_starting(
        pid,
        ppid,
        name,
        Vec::new(),
        BTreeMap::new(),
        "/",
        caps,
        0,
        0,
        0,
    )
}

// ---- PID allocator --------------------------------------------------

#[test]
fn pid_allocator_starts_at_one_and_is_monotonic() {
    let mut table = ProcessTable::new();
    assert_eq!(table.allocate_pid(), 1);
    assert_eq!(table.allocate_pid(), 2);
    assert_eq!(table.allocate_pid(), 3);
}

#[test]
fn pid_allocator_never_reuses_within_a_boot() {
    // Spawn, exit, reap, spawn again — the reaped pid is NOT
    // recycled. This matches the data-model.md §1 invariant.
    let mut table = ProcessTable::new();

    let pid_a = table.allocate_pid();
    table.insert(make_process(pid_a, 1, "a", CapSet::EMPTY)).unwrap();
    table.transition(pid_a, ProcState::Ready).unwrap();
    table.transition(pid_a, ProcState::Running).unwrap();
    table.exit(pid_a, ExitStatus::Exited(0)).unwrap();
    table.reap(pid_a);

    let pid_b = table.allocate_pid();
    assert_ne!(pid_a, pid_b);
    assert_eq!(pid_b, 2);
}

// ---- Insert / lookup ------------------------------------------------

#[test]
fn insert_and_lookup_round_trip() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table
        .insert(make_process(pid, 1, "sh", CapSet::from_caps(&[Cap::DisplayClient])))
        .unwrap();

    let proc = table.get(pid).expect("process exists");
    assert_eq!(proc.pid, pid);
    assert_eq!(proc.name, "sh");
    assert!(proc.caps.contains(Cap::DisplayClient));
    assert!(table.is_alive(pid));
}

#[test]
fn insert_duplicate_pid_fails() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "a", CapSet::EMPTY)).unwrap();
    let err = table.insert(make_process(pid, 1, "b", CapSet::EMPTY)).unwrap_err();
    assert_eq!(err, InsertError::PidInUse);
}

#[test]
fn lookup_missing_pid_returns_none() {
    let table = ProcessTable::new();
    assert!(table.get(999).is_none());
    assert!(!table.is_alive(999));
}

// ---- Exit + reap ----------------------------------------------------

#[test]
fn exit_sets_zombie_and_retains_status() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "child", CapSet::EMPTY)).unwrap();
    table.transition(pid, ProcState::Ready).unwrap();
    table.transition(pid, ProcState::Running).unwrap();
    table.exit(pid, ExitStatus::Exited(7)).unwrap();

    let proc = table.get(pid).expect("still present as zombie");
    assert_eq!(proc.state, ProcState::Zombie);
    assert_eq!(proc.exit_status, Some(ExitStatus::Exited(7)));
    assert!(table.is_alive(pid)); // Zombie counts as alive until reaped.
}

#[test]
fn reap_removes_zombie_and_returns_status() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "child", CapSet::EMPTY)).unwrap();
    table.transition(pid, ProcState::Ready).unwrap();
    table.transition(pid, ProcState::Running).unwrap();
    table.exit(pid, ExitStatus::Signaled(9)).unwrap();

    let status = table.reap(pid);
    assert_eq!(status, Some(ExitStatus::Signaled(9)));
    assert!(table.get(pid).is_none());
    assert!(!table.is_alive(pid));
}

#[test]
fn reap_non_zombie_returns_none() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "running", CapSet::EMPTY)).unwrap();
    table.transition(pid, ProcState::Ready).unwrap();
    // Reaping a Ready process is a no-op.
    assert_eq!(table.reap(pid), None);
    assert!(table.is_alive(pid));
}

#[test]
fn exit_dead_process_fails() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "zombie", CapSet::EMPTY)).unwrap();
    table.transition(pid, ProcState::Ready).unwrap();
    table.transition(pid, ProcState::Running).unwrap();
    table.exit(pid, ExitStatus::Exited(0)).unwrap();
    table.reap(pid);
    // The process is gone entirely; exit() on a reaped pid is NoSuchPid.
    let err = table.exit(pid, ExitStatus::Exited(1)).unwrap_err();
    assert_eq!(err, ExitError::NoSuchPid);
}

// ---- Illegal transitions are rejected --------------------------------

#[test]
fn illegal_transitions_are_errors_from_the_table() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "x", CapSet::EMPTY)).unwrap();
    // Starting -> Running is illegal (must pass through Ready).
    assert_eq!(
        table.transition(pid, ProcState::Running).unwrap_err(),
        TransitionError::Illegal
    );
    // Still in Starting.
    assert_eq!(table.get(pid).unwrap().state, ProcState::Starting);
}

#[test]
fn transition_unknown_pid_is_an_error() {
    let mut table = ProcessTable::new();
    let err = table.transition(42, ProcState::Ready).unwrap_err();
    assert_eq!(err, TransitionError::NoSuchPid);
}

// ---- Parent / child relationships ----------------------------------

#[test]
fn find_zombie_child_any_target() {
    let mut table = ProcessTable::new();

    let parent = table.allocate_pid();
    table.insert(make_process(parent, 0, "init", CapSet::ALL)).unwrap();

    let child_a = table.allocate_pid();
    table.insert(make_process(child_a, parent, "a", CapSet::EMPTY)).unwrap();
    let child_b = table.allocate_pid();
    table.insert(make_process(child_b, parent, "b", CapSet::EMPTY)).unwrap();

    // Neither is a zombie yet.
    assert!(table.find_zombie_child(parent, ZombieTarget::Any).is_none());

    // child_b exits first.
    table.transition(child_b, ProcState::Ready).unwrap();
    table.transition(child_b, ProcState::Running).unwrap();
    table.exit(child_b, ExitStatus::Exited(0)).unwrap();

    assert_eq!(
        table.find_zombie_child(parent, ZombieTarget::Any),
        Some(child_b)
    );

    // A specific target that matches.
    assert_eq!(
        table.find_zombie_child(parent, ZombieTarget::Specific(child_b)),
        Some(child_b)
    );

    // A specific target that does not match.
    assert!(table
        .find_zombie_child(parent, ZombieTarget::Specific(child_a))
        .is_none());
}

#[test]
fn child_count_excludes_dead_processes() {
    let mut table = ProcessTable::new();

    let parent = table.allocate_pid();
    table.insert(make_process(parent, 0, "init", CapSet::ALL)).unwrap();

    for _ in 0..3 {
        let pid = table.allocate_pid();
        table.insert(make_process(pid, parent, "c", CapSet::EMPTY)).unwrap();
    }
    assert_eq!(table.child_count(parent), 3);

    // Reap one.
    let victim = 4; // child pids are 2, 3, 4
    table.transition(victim, ProcState::Ready).unwrap();
    table.transition(victim, ProcState::Running).unwrap();
    table.exit(victim, ExitStatus::Exited(0)).unwrap();
    table.reap(victim);

    assert_eq!(table.child_count(parent), 2);
}

// ---- Block + wake ---------------------------------------------------

#[test]
fn block_on_syscall_then_wake_round_trip() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "x", CapSet::EMPTY)).unwrap();
    table.transition(pid, ProcState::Ready).unwrap();
    table.transition(pid, ProcState::Running).unwrap();

    table
        .block(
            pid,
            ProcState::BlockedOnSyscall,
            BlockReason::Syscall { request_id: 12345 },
        )
        .unwrap();
    assert_eq!(table.get(pid).unwrap().state, ProcState::BlockedOnSyscall);

    // A wake with the wrong request_id is a no-op.
    assert!(!table.wake_syscall(pid, 99));
    assert_eq!(table.get(pid).unwrap().state, ProcState::BlockedOnSyscall);

    // A wake with the right request_id moves to Ready.
    assert!(table.wake_syscall(pid, 12345));
    assert_eq!(table.get(pid).unwrap().state, ProcState::Ready);
    assert!(table.get(pid).unwrap().block_reason.is_none());
}

// ---- Scheduler ------------------------------------------------------

#[test]
fn scheduler_picks_ready_in_fifo_order() {
    let mut table = ProcessTable::new();
    let mut sched = Scheduler::new();

    let a = table.allocate_pid();
    let b = table.allocate_pid();
    let c = table.allocate_pid();
    for (pid, name) in [(a, "a"), (b, "b"), (c, "c")] {
        table.insert(make_process(pid, 1, name, CapSet::EMPTY)).unwrap();
        table.transition(pid, ProcState::Ready).unwrap();
        sched.enqueue(pid);
    }

    assert_eq!(sched.pick_next(&mut table), Some(a));
    // After pick, a is Running.
    assert_eq!(table.get(a).unwrap().state, ProcState::Running);
    // Yield a: return it to Ready so the next pick advances fairly.
    table.transition(a, ProcState::Ready).unwrap();
    sched.enqueue(a);

    assert_eq!(sched.pick_next(&mut table), Some(b));
    assert_eq!(table.get(b).unwrap().state, ProcState::Running);
}

#[test]
fn scheduler_skips_stale_entries_for_killed_processes() {
    let mut table = ProcessTable::new();
    let mut sched = Scheduler::new();

    let a = table.allocate_pid();
    let b = table.allocate_pid();
    table.insert(make_process(a, 1, "a", CapSet::EMPTY)).unwrap();
    table.insert(make_process(b, 1, "b", CapSet::EMPTY)).unwrap();
    table.transition(a, ProcState::Ready).unwrap();
    table.transition(b, ProcState::Ready).unwrap();
    sched.enqueue(a);
    sched.enqueue(b);

    // Kill `a` while it is still on the ready queue — simulating a
    // SIGKILL arriving between enqueue and pick.
    table.transition(a, ProcState::Zombie).unwrap();

    // pick_next must skip the zombie and return b.
    assert_eq!(sched.pick_next(&mut table), Some(b));
}

#[test]
fn scheduler_returns_none_when_no_process_is_ready() {
    let mut table = ProcessTable::new();
    let mut sched = Scheduler::new();

    let a = table.allocate_pid();
    table.insert(make_process(a, 1, "a", CapSet::EMPTY)).unwrap();
    // Still in Starting — not Ready.
    sched.enqueue(a);
    assert_eq!(sched.pick_next(&mut table), None);
}

#[test]
fn scheduler_dedups_enqueue() {
    let mut sched = Scheduler::new();
    sched.enqueue(1);
    sched.enqueue(1);
    sched.enqueue(1);
    assert_eq!(sched.ready_len(), 1);
}

#[test]
fn scheduler_remove_clears_from_queue_and_current() {
    let mut table = ProcessTable::new();
    let mut sched = Scheduler::new();

    let a = table.allocate_pid();
    let b = table.allocate_pid();
    for (pid, name) in [(a, "a"), (b, "b")] {
        table.insert(make_process(pid, 1, name, CapSet::EMPTY)).unwrap();
        table.transition(pid, ProcState::Ready).unwrap();
        sched.enqueue(pid);
    }
    // Start running `a`.
    assert_eq!(sched.pick_next(&mut table), Some(a));
    assert_eq!(sched.current(), Some(a));

    // Remove both.
    sched.remove(a);
    sched.remove(b);

    // Neither current nor queued.
    assert_eq!(sched.current(), None);
    assert_eq!(sched.ready_len(), 0);
}

// ---- Live count / live pids ----------------------------------------

#[test]
fn live_count_and_live_pids_exclude_dead() {
    let mut table = ProcessTable::new();

    for i in 1..=3 {
        let pid = table.allocate_pid();
        assert_eq!(pid, i);
        table.insert(make_process(pid, 0, "p", CapSet::EMPTY)).unwrap();
    }
    assert_eq!(table.live_count(), 3);
    assert_eq!(table.live_pids(), vec![1, 2, 3]);

    // Exit + reap #2. Zombies are live; Dead (reaped) are not.
    table.transition(2, ProcState::Ready).unwrap();
    table.transition(2, ProcState::Running).unwrap();
    table.exit(2, ExitStatus::Exited(0)).unwrap();
    assert_eq!(table.live_count(), 3, "zombies count as live");
    table.reap(2);
    assert_eq!(table.live_count(), 2);
    assert_eq!(table.live_pids(), vec![1, 3]);
}
