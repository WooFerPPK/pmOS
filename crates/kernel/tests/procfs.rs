//! procfs isolation tests (T168 partial).
//!
//! Covers the per-pid `/proc/<pid>/status` generator. No browser
//! involved; these tests are the Principle X gate for procfs's
//! status-file surface. A `StaticProcFsSource` stands in for the
//! future kernel-bridge source so the fs layer can be exercised
//! without pulling in the full `Kernel` composition.
//!
//! The `ProcessTable` is still driven directly (the tests "spawn"
//! real `Process` records and flip their state through the same
//! `ProcState` transitions production code uses) — `pid_status`
//! snapshots are then projected into the `StaticProcFsSource` so
//! the VFS read path serves the exact bytes a kernel-bridge
//! source would eventually serve.
//!
//! Run via `cargo test -p kernel --features native-platform`.

#![cfg(feature = "native-platform")]

use std::collections::BTreeMap;

use abi::cap::CapSet;
use abi::ext::Pid;

use kernel::fs::procfs::{
    ProcFs, ProcFsSource, ProcStatusSnapshot, ProcStatusState, StaticProcFsSource,
};
use kernel::proc::{
    table::ProcessTable,
    ExitStatus, Process, ProcState,
};
use kernel::vfs::{FsError, NodeType, Vfs};

// ---- Helpers --------------------------------------------------------

fn make_process(pid: Pid, ppid: Pid, name: &str) -> Process {
    Process::new_starting(
        pid,
        ppid,
        name,
        Vec::new(),
        BTreeMap::new(),
        "/",
        CapSet::EMPTY,
        0,
        0,
        0,
    )
}

/// Map a live process's `ProcState` to the `/proc/<pid>/status`
/// letter-state. Intentionally duplicated in-test so the assertion
/// side of each case doesn't depend on the kernel bridge landing
/// first; the test enforces the exact mapping the bridge will
/// also use.
fn project_state(state: ProcState) -> ProcStatusState {
    match state {
        ProcState::Running => ProcStatusState::Running,
        ProcState::Starting
        | ProcState::Ready
        | ProcState::BlockedOnSyscall
        | ProcState::BlockedOnIpc
        | ProcState::BlockedOnWait => ProcStatusState::Sleeping,
        ProcState::Zombie | ProcState::Dead => ProcStatusState::Zombie,
    }
}

/// Build a snapshot of every live process in `table`.
fn snapshots_from(table: &ProcessTable) -> Vec<ProcStatusSnapshot> {
    table
        .live_pids()
        .into_iter()
        .filter_map(|pid| {
            table.get(pid).map(|p| ProcStatusSnapshot {
                pid: p.pid,
                ppid: p.ppid,
                name: p.name.clone(),
                state: project_state(p.state),
            })
        })
        .collect()
}

/// Build a Vfs with a ProcFs backed by a StaticProcFsSource
/// populated from `snapshots`.
fn vfs_with_snapshots(snapshots: Vec<ProcStatusSnapshot>) -> Vfs {
    let mut source = StaticProcFsSource::default();
    for snap in snapshots {
        source.set_pid_status(snap);
    }
    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(kernel::fs::tmpfs::TmpFs::new()))
        .unwrap();
    vfs.mount("/proc", Box::new(ProcFs::new(Box::new(source))))
        .unwrap();
    vfs
}

/// Read the entire `/proc/<pid>/status` file into a Vec<u8>.
fn read_status(vfs: &mut Vfs, pid: Pid) -> Result<Vec<u8>, FsError> {
    let path = format!("/proc/{pid}/status");
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    let mut off: u64 = 0;
    loop {
        let n = vfs.read(&path, off, &mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        off += n as u64;
    }
    Ok(out)
}

// ---- Tests ----------------------------------------------------------

#[test]
fn status_contains_name_for_spawned_process() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "sh")).unwrap();

    let mut vfs = vfs_with_snapshots(snapshots_from(&table));
    let bytes = read_status(&mut vfs, pid).unwrap();
    let text = core::str::from_utf8(&bytes).unwrap();

    // Match the exact byte layout: Name is the first line, tab
    // separator, trailing newline.
    assert!(
        text.starts_with("Name:\tsh\n"),
        "unexpected status prefix: {:?}",
        text,
    );
}

#[test]
fn status_reports_state_running_for_running_process() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "init")).unwrap();
    table.transition(pid, ProcState::Ready).unwrap();
    table.transition(pid, ProcState::Running).unwrap();

    let mut vfs = vfs_with_snapshots(snapshots_from(&table));
    let bytes = read_status(&mut vfs, pid).unwrap();
    let text = core::str::from_utf8(&bytes).unwrap();

    assert!(
        text.contains("State:\tR (running)\n"),
        "missing Running state line: {:?}",
        text,
    );
}

#[test]
fn status_reports_pid_and_ppid_correctly() {
    let mut table = ProcessTable::new();
    let parent_pid = table.allocate_pid();
    table.insert(make_process(parent_pid, 0, "init")).unwrap();
    let child_pid = table.allocate_pid();
    table
        .insert(make_process(child_pid, parent_pid, "child"))
        .unwrap();

    let mut vfs = vfs_with_snapshots(snapshots_from(&table));
    let bytes = read_status(&mut vfs, child_pid).unwrap();
    let text = core::str::from_utf8(&bytes).unwrap();

    let expected_pid_line = format!("Pid:\t{child_pid}\n");
    let expected_ppid_line = format!("PPid:\t{parent_pid}\n");
    assert!(
        text.contains(&expected_pid_line),
        "missing Pid line, text was: {:?}",
        text,
    );
    assert!(
        text.contains(&expected_ppid_line),
        "missing PPid line, text was: {:?}",
        text,
    );
}

#[test]
fn status_reports_state_zombie_after_proc_exit() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "worker")).unwrap();
    table.transition(pid, ProcState::Ready).unwrap();
    table.transition(pid, ProcState::Running).unwrap();
    table.exit(pid, ExitStatus::Exited(0)).unwrap();

    let mut vfs = vfs_with_snapshots(snapshots_from(&table));
    let bytes = read_status(&mut vfs, pid).unwrap();
    let text = core::str::from_utf8(&bytes).unwrap();

    assert!(
        text.contains("State:\tZ (zombie)\n"),
        "expected zombie state line, got: {:?}",
        text,
    );
}

// ---- Additional shape / layout assertions --------------------------

#[test]
fn status_ends_with_trailing_newline_and_four_fields() {
    // Four `Key: Value\n` lines, in the documented order. The
    // buffer ends on `\n` — no trailing whitespace.
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "sh")).unwrap();
    table.transition(pid, ProcState::Ready).unwrap();

    let mut vfs = vfs_with_snapshots(snapshots_from(&table));
    let bytes = read_status(&mut vfs, pid).unwrap();
    let text = core::str::from_utf8(&bytes).unwrap();

    assert!(text.ends_with('\n'));
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    assert_eq!(lines.len(), 4, "expected 4 lines, got {:?}", lines);
    assert!(lines[0].starts_with("Name:\t"), "line 0: {:?}", lines[0]);
    assert!(lines[1].starts_with("State:\t"), "line 1: {:?}", lines[1]);
    assert!(lines[2].starts_with("Pid:\t"), "line 2: {:?}", lines[2]);
    assert!(lines[3].starts_with("PPid:\t"), "line 3: {:?}", lines[3]);
}

#[test]
fn proc_root_readdir_lists_live_pid_dirs_alongside_canned_files() {
    let mut table = ProcessTable::new();
    let pid1 = table.allocate_pid();
    table.insert(make_process(pid1, 0, "init")).unwrap();
    let pid2 = table.allocate_pid();
    table.insert(make_process(pid2, pid1, "sh")).unwrap();

    let mut vfs = vfs_with_snapshots(snapshots_from(&table));
    let entries = vfs.readdir("/proc").unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

    // Top-level canned files are still present.
    assert!(names.contains(&"version"));
    assert!(names.contains(&"storage"));

    // Live pid directories show up with NodeType::Directory.
    let pid1_name = pid1.to_string();
    let pid2_name = pid2.to_string();
    let pid1_entry = entries
        .iter()
        .find(|e| e.name == pid1_name)
        .expect("pid1 dir present");
    assert_eq!(pid1_entry.ty, NodeType::Directory);
    let pid2_entry = entries
        .iter()
        .find(|e| e.name == pid2_name)
        .expect("pid2 dir present");
    assert_eq!(pid2_entry.ty, NodeType::Directory);
}

#[test]
fn lookup_of_nonexistent_pid_returns_not_found() {
    let table = ProcessTable::new();
    let mut vfs = vfs_with_snapshots(snapshots_from(&table));

    // No live pids -> `/proc/42` doesn't exist.
    let err = vfs.stat("/proc/42").unwrap_err();
    assert_eq!(err, FsError::NotFound);

    let err = vfs.read("/proc/42/status", 0, &mut [0u8; 16]).unwrap_err();
    assert_eq!(err, FsError::NotFound);
}

#[test]
fn status_file_size_in_stat_matches_byte_length() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "editor")).unwrap();

    let mut vfs = vfs_with_snapshots(snapshots_from(&table));
    let bytes = read_status(&mut vfs, pid).unwrap();
    let st = vfs.stat(&format!("/proc/{pid}/status")).unwrap();
    assert_eq!(st.ty, NodeType::RegularFile);
    assert_eq!(st.size as usize, bytes.len());
    assert_eq!(st.mode, 0o444);
}

#[test]
fn state_letter_mapping_covers_all_procstate_variants() {
    // Pin the ProcState → ProcStatusState mapping so a future
    // variant addition fails here rather than silently serving
    // the wrong letter. The projection mirrors the live code
    // path's `project_state`.
    assert_eq!(
        project_state(ProcState::Running).letter(),
        'R',
    );
    assert_eq!(
        project_state(ProcState::Ready).letter(),
        'S',
    );
    assert_eq!(
        project_state(ProcState::Starting).letter(),
        'S',
    );
    assert_eq!(
        project_state(ProcState::BlockedOnSyscall).letter(),
        'S',
    );
    assert_eq!(
        project_state(ProcState::BlockedOnIpc).letter(),
        'S',
    );
    assert_eq!(
        project_state(ProcState::BlockedOnWait).letter(),
        'S',
    );
    assert_eq!(
        project_state(ProcState::Zombie).letter(),
        'Z',
    );
    assert_eq!(
        project_state(ProcState::Dead).letter(),
        'Z',
    );
}

#[test]
fn source_with_no_pids_still_serves_top_level_files() {
    // Safety net: adding per-pid plumbing must not regress the
    // existing canned-file behaviour.
    let source = StaticProcFsSource::default();
    assert!(source.live_pids().is_empty());
    assert!(ProcFsSource::pid_status(&source, 1).is_none());

    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(kernel::fs::tmpfs::TmpFs::new()))
        .unwrap();
    vfs.mount("/proc", Box::new(ProcFs::with_static())).unwrap();

    let mut buf = [0u8; 64];
    let n = vfs.read("/proc/version", 0, &mut buf).unwrap();
    assert!(core::str::from_utf8(&buf[..n]).unwrap().starts_with("PMos"));
}
