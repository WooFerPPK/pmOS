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
    fd_symlink_ino, format_argv_cmdline, pid_cmdline_ino, pid_fd_dir_ino, pid_status_ino,
    pid_subtree_ino, KernelProcFsSource, ProcFdSnapshot, ProcFs, ProcFsSource, ProcStatusSnapshot,
    ProcStatusState, StaticProcFsSource, StorageSnapshot, PID_STRIDE,
};
use kernel::proc::{table::ProcessTable, ExitStatus, ProcState, Process};
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
                vm_size_bytes: p.vm_size_bytes,
                vm_peak_bytes: p.vm_peak_bytes,
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
fn status_reports_vm_size_and_peak_in_kib() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "mem")).unwrap();
    assert!(table.record_memory_size(pid, 8 * 1024));
    assert!(table.record_memory_size(pid, 6 * 1024 + 1));

    let mut vfs = vfs_with_snapshots(snapshots_from(&table));
    let bytes = read_status(&mut vfs, pid).unwrap();
    let text = core::str::from_utf8(&bytes).unwrap();

    assert!(
        text.contains("VmSize:\t7 kB\n"),
        "missing VmSize line, text was: {:?}",
        text,
    );
    assert!(
        text.contains("VmPeak:\t8 kB\n"),
        "missing VmPeak line, text was: {:?}",
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
fn status_ends_with_trailing_newline_and_six_fields() {
    // Six `Key: Value\n` lines, in the documented order. The
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
    assert_eq!(lines.len(), 6, "expected 6 lines, got {:?}", lines);
    assert!(lines[0].starts_with("Name:\t"), "line 0: {:?}", lines[0]);
    assert!(lines[1].starts_with("State:\t"), "line 1: {:?}", lines[1]);
    assert!(lines[2].starts_with("Pid:\t"), "line 2: {:?}", lines[2]);
    assert!(lines[3].starts_with("PPid:\t"), "line 3: {:?}", lines[3]);
    assert!(lines[4].starts_with("VmSize:\t"), "line 4: {:?}", lines[4]);
    assert!(lines[5].starts_with("VmPeak:\t"), "line 5: {:?}", lines[5]);
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
    assert_eq!(project_state(ProcState::Running).letter(), 'R',);
    assert_eq!(project_state(ProcState::Ready).letter(), 'S',);
    assert_eq!(project_state(ProcState::Starting).letter(), 'S',);
    assert_eq!(project_state(ProcState::BlockedOnSyscall).letter(), 'S',);
    assert_eq!(project_state(ProcState::BlockedOnIpc).letter(), 'S',);
    assert_eq!(project_state(ProcState::BlockedOnWait).letter(), 'S',);
    assert_eq!(project_state(ProcState::Zombie).letter(), 'Z',);
    assert_eq!(project_state(ProcState::Dead).letter(), 'Z',);
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

// ---- /proc/storage — structured StorageSnapshot seam (T169) --------

/// Local test source that exercises the *default* `storage()` impl
/// on the `ProcFsSource` trait — i.e. the formatter path that
/// projects a `StorageSnapshot` into `"quota used files\n"`.
/// `StaticProcFsSource` overrides `storage()` directly (for
/// backwards-compat with the pre-T169 canned-line tests), so a
/// separate type is required to cover the default arm.
struct StorageOnlyTestSource {
    snapshot: Option<StorageSnapshot>,
}

impl ProcFsSource for StorageOnlyTestSource {
    fn version(&self) -> String {
        String::from("test\n")
    }
    fn uptime(&self) -> String {
        String::from("0 0\n")
    }
    fn meminfo(&self) -> String {
        String::from("0 0 0\n")
    }
    fn loadavg(&self) -> String {
        String::from("0.00 0.00 0.00 0/0 0\n")
    }
    fn storage_info(&self) -> Option<StorageSnapshot> {
        self.snapshot.clone()
    }
}

#[test]
fn storage_info_none_on_default_static_source() {
    // Default source has no structured storage counters yet; the
    // placeholder `"0 0 0\n"` contract from pre-T169 callers must
    // still hold.
    let source = StaticProcFsSource::default();
    assert!(source.storage_info().is_none());
    assert_eq!(source.storage(), "0 0 0\n");
}

#[test]
fn storage_info_some_populates_storage_line() {
    // Populating `storage_info` does not change the
    // `StaticProcFsSource::storage()` override (which still returns
    // `storage_line`); the snapshot accessor is what the future
    // kernel-bridge projects. This test pins both paths so
    // overriding behaviour stays explicit.
    let source = StaticProcFsSource {
        storage_line: String::from("canned override\n"),
        storage_info: Some(StorageSnapshot {
            quota_bytes: 10_000,
            used_bytes: 500,
            file_count: 7,
        }),
        ..StaticProcFsSource::default()
    };
    assert_eq!(source.storage(), "canned override\n");
    assert_eq!(
        source.storage_info(),
        Some(StorageSnapshot {
            quota_bytes: 10_000,
            used_bytes: 500,
            file_count: 7,
        })
    );
}

#[test]
fn storage_info_direct_accessor() {
    // Round-trip: set a snapshot on the canned source, pull it back
    // out via the accessor, assert equality.
    let snap = StorageSnapshot {
        quota_bytes: 1 << 30,
        used_bytes: (1 << 20) * 42,
        file_count: 128,
    };
    let source = StaticProcFsSource {
        storage_info: Some(snap.clone()),
        ..StaticProcFsSource::default()
    };
    assert_eq!(source.storage_info(), Some(snap));
}

#[test]
fn custom_source_with_only_storage_info_returns_formatted_storage_line() {
    // A source that only overrides `storage_info` (and leaves the
    // default `storage()` in place) must format the snapshot into
    // the documented `"{quota} {used} {files}\n"` layout. This is
    // the contract the `KernelProcFsSource` boot-path bridge will
    // rely on when it projects the live block-driver counters
    // without re-implementing the formatter.
    let source = StorageOnlyTestSource {
        snapshot: Some(StorageSnapshot {
            quota_bytes: 10_000,
            used_bytes: 500,
            file_count: 7,
        }),
    };
    assert_eq!(source.storage(), "10000 500 7\n");

    // And the `None` arm of the default impl falls back to the
    // placeholder — same contract a pre-T169 caller saw.
    let empty = StorageOnlyTestSource { snapshot: None };
    assert_eq!(empty.storage(), "0 0 0\n");
}

// ---- /proc/<pid>/fd/ — open-fd directory (T168 follow-up) ----------

/// Build a Vfs with a ProcFs backed by a StaticProcFsSource
/// populated from both a status snapshot list *and* a per-pid fd
/// map. The canned `status` snapshot makes the pid directory live;
/// `pid_fds` then populates the `fd/` subtree.
fn vfs_with_fds(statuses: Vec<ProcStatusSnapshot>, fds: Vec<(Pid, Vec<ProcFdSnapshot>)>) -> Vfs {
    let mut source = StaticProcFsSource::default();
    for snap in statuses {
        source.set_pid_status(snap);
    }
    for (pid, pid_fds) in fds {
        source.set_pid_fds(pid, pid_fds);
    }
    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(kernel::fs::tmpfs::TmpFs::new()))
        .unwrap();
    vfs.mount("/proc", Box::new(ProcFs::new(Box::new(source))))
        .unwrap();
    vfs
}

/// Build a status snapshot for a bare-bones test process.
fn stub_status(pid: Pid, ppid: Pid, name: &str) -> ProcStatusSnapshot {
    ProcStatusSnapshot {
        pid,
        ppid,
        name: name.into(),
        state: ProcStatusState::Sleeping,
        vm_size_bytes: 0,
        vm_peak_bytes: 0,
    }
}

#[test]
fn fd_dir_readdir_lists_open_fds() {
    // Four open fds — stdin/stdout/stderr wired to /dev/console
    // plus a regular file at fd 3. readdir on `/proc/42/fd` must
    // yield exactly those four entries, all of type SymLink.
    let pid: Pid = 42;
    let fds = vec![
        ProcFdSnapshot {
            fd: 0,
            target: String::from("/dev/console"),
        },
        ProcFdSnapshot {
            fd: 1,
            target: String::from("/dev/console"),
        },
        ProcFdSnapshot {
            fd: 2,
            target: String::from("/dev/console"),
        },
        ProcFdSnapshot {
            fd: 3,
            target: String::from("/etc/preferences.toml"),
        },
    ];
    let mut vfs = vfs_with_fds(vec![stub_status(pid, 1, "sh")], vec![(pid, fds)]);

    let entries = vfs.readdir(&format!("/proc/{pid}/fd")).unwrap();
    assert_eq!(entries.len(), 4, "unexpected fd dir entries: {:?}", entries);

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["0", "1", "2", "3"]);
    for e in &entries {
        assert_eq!(e.ty, NodeType::SymLink, "entry {:?} not a symlink", e);
    }
}

#[test]
fn fd_symlink_read_returns_target_path() {
    // read on `/proc/42/fd/3` returns the target bytes — same
    // contract a readlink(2) would satisfy, but projected through
    // the byte-read path so a userland caller that doesn't
    // implement readlink can still surface the target.
    let pid: Pid = 42;
    let fds = vec![ProcFdSnapshot {
        fd: 3,
        target: String::from("/etc/preferences.toml"),
    }];
    let mut source = StaticProcFsSource::default();
    source.set_pid_status(stub_status(pid, 1, "sh"));
    source.set_pid_fds(pid, fds);
    let mut procfs = ProcFs::new(Box::new(source));

    // Drive the Filesystem trait directly: look up the symlink,
    // then read from its ino. No VFS resolve, so no auto-follow.
    use kernel::vfs::Filesystem as _;
    let proc_root = procfs.root();
    let pid_dir = procfs.lookup(proc_root, &pid.to_string()).unwrap();
    let fd_dir = procfs.lookup(pid_dir, "fd").unwrap();
    let fd3 = procfs.lookup(fd_dir, "3").unwrap();

    let mut buf = [0u8; 64];
    let n = procfs.read(fd3, 0, &mut buf).unwrap();
    let got = core::str::from_utf8(&buf[..n]).unwrap();
    assert_eq!(got, "/etc/preferences.toml");
}

#[test]
fn fd_symlink_stat_reports_symlink_node_type() {
    // stat on `/proc/42/fd/3` returns SymLink with mode 0o777 —
    // the Linux procfs convention. Uses vfs.stat which routes
    // through resolve_nofollow, preserving the symlink's own
    // metadata rather than dereferencing to the target.
    let pid: Pid = 42;
    let target = String::from("/etc/preferences.toml");
    let fds = vec![ProcFdSnapshot {
        fd: 3,
        target: target.clone(),
    }];
    let mut vfs = vfs_with_fds(vec![stub_status(pid, 1, "sh")], vec![(pid, fds)]);

    let st = vfs.stat(&format!("/proc/{pid}/fd/3")).unwrap();
    assert_eq!(st.ty, NodeType::SymLink);
    assert_eq!(st.mode, 0o777);
    assert_eq!(st.size as usize, target.len());
}

#[test]
fn fd_dir_empty_for_pid_with_no_fds() {
    // pid 1 is live (has a status snapshot) but has no open fds
    // registered. readdir on `/proc/1/fd` must succeed and return
    // an empty list — not NotADirectory, not NotFound.
    let pid: Pid = 1;
    let mut vfs = vfs_with_fds(vec![stub_status(pid, 0, "init")], vec![]);

    let entries = vfs.readdir(&format!("/proc/{pid}/fd")).unwrap();
    assert!(
        entries.is_empty(),
        "expected empty fd dir, got: {:?}",
        entries
    );
}

#[test]
fn fd_dir_lookup_rejects_nonexistent_fd_number() {
    // lookup on `/proc/42/fd/99` when the process has only fd 3
    // open must return NotFound. The stride-based inode scheme
    // would otherwise be happy to hand out a number; the
    // snapshot-set check is what gates the response.
    let pid: Pid = 42;
    let fds = vec![ProcFdSnapshot {
        fd: 3,
        target: String::from("/etc/preferences.toml"),
    }];
    let mut vfs = vfs_with_fds(vec![stub_status(pid, 1, "sh")], vec![(pid, fds)]);

    let err = vfs.stat(&format!("/proc/{pid}/fd/99")).unwrap_err();
    assert_eq!(err, FsError::NotFound);
}

#[test]
fn fd_ino_region_does_not_collide_with_pid_subtree() {
    // For a spread of pids and fds, the fd-region ino must never
    // collide with any stride slot in the same pid's per-pid
    // subtree — the stride-16 region and the fd-1024-per-pid
    // region must be disjoint across every legal input.
    for &pid in &[1 as Pid, 2, 42, 1024, 65_535, 100_000] {
        let mut pid_subtree = std::collections::HashSet::new();
        for offset in 0..PID_STRIDE {
            pid_subtree.insert(pid_subtree_ino(pid, offset));
        }
        for &fd in &[0u32, 1, 2, 3, 7, 42, 255, 1023] {
            let fd_ino = fd_symlink_ino(pid, fd);
            assert!(
                !pid_subtree.contains(&fd_ino),
                "fd ino {fd_ino} for (pid={pid}, fd={fd}) collides with pid subtree",
            );
        }
    }
}

#[test]
fn fd_dir_listed_in_pid_subtree_readdir() {
    // readdir on `/proc/42` must include both `status` and `fd`
    // entries; `status` stays a regular file, `fd` is a directory.
    let pid: Pid = 42;
    let fds = vec![ProcFdSnapshot {
        fd: 0,
        target: String::from("/dev/console"),
    }];
    let mut vfs = vfs_with_fds(vec![stub_status(pid, 1, "sh")], vec![(pid, fds)]);

    let entries = vfs.readdir(&format!("/proc/{pid}")).unwrap();

    let status_entry = entries
        .iter()
        .find(|e| e.name == "status")
        .expect("status entry present");
    assert_eq!(status_entry.ty, NodeType::RegularFile);

    let fd_entry = entries
        .iter()
        .find(|e| e.name == "fd")
        .expect("fd entry present");
    assert_eq!(fd_entry.ty, NodeType::Directory);
}

// ---- KernelProcFsSource — live-kernel bridge (T169 partial) --------

#[test]
fn kernel_procfs_source_pid_status_returns_spawned_process() {
    // Bridge reads `pid=42 name="edit"` from the live ProcessTable
    // via `pid_status(42)`. The returned snapshot mirrors the
    // process record byte-for-byte — same shape the VFS serves at
    // `/proc/42/status` under StaticProcFsSource.
    let mut table = ProcessTable::new();
    // PidAllocator hands out pids sequentially from 1; to land on
    // pid 42 we burn 1..=41 first. allocate_pid() returns the pid
    // to assign *this* call, so allocating 41 throwaways yields
    // the 42nd allocation next.
    for _ in 0..41 {
        let _ = table.allocate_pid();
    }
    let pid = table.allocate_pid();
    assert_eq!(pid, 42);
    table.insert(make_process(pid, 1, "edit")).unwrap();

    let source = KernelProcFsSource::new(&table);
    let snap = source.pid_status(42).expect("pid 42 is live");
    assert_eq!(
        snap,
        ProcStatusSnapshot {
            pid: 42,
            ppid: 1,
            name: "edit".into(),
            state: ProcStatusState::Sleeping,
            vm_size_bytes: 0,
            vm_peak_bytes: 0,
        },
    );
}

#[test]
fn kernel_procfs_source_pid_status_projects_memory_counters() {
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "worker")).unwrap();
    assert!(table.record_memory_size(pid, 4 * 1024));
    assert!(table.record_memory_size(pid, 9 * 1024));
    assert!(table.record_memory_size(pid, 5 * 1024));

    let snap = KernelProcFsSource::new(&table)
        .pid_status(pid)
        .expect("pid live");
    assert_eq!(snap.vm_size_bytes, 5 * 1024);
    assert_eq!(snap.vm_peak_bytes, 9 * 1024);
}

#[test]
fn kernel_procfs_source_live_pids_lists_spawned() {
    // Two spawns → `live_pids()` surfaces them in ascending order.
    // Matches the `ProcessTable::live_pids` contract the bridge
    // delegates to; we pin the order here so a later re-arrangement
    // of the process table's iteration surface shows up as a test
    // failure rather than a silent breakage in `/proc` readdir.
    let mut table = ProcessTable::new();
    let pid1 = table.allocate_pid();
    table.insert(make_process(pid1, 0, "init")).unwrap();
    let pid2 = table.allocate_pid();
    table.insert(make_process(pid2, pid1, "sh")).unwrap();

    let source = KernelProcFsSource::new(&table);
    let pids = source.live_pids();
    assert_eq!(pids, vec![pid1, pid2]);
    assert!(pid1 < pid2, "pids must be ascending by construction");
}

#[test]
fn kernel_procfs_source_pid_status_returns_none_for_missing_pid() {
    // Empty table — pid 999 has never existed, so the bridge
    // returns `None`. This is what `/proc/999/status` uses to
    // decide FsError::NotFound.
    let table = ProcessTable::new();
    let source = KernelProcFsSource::new(&table);
    assert!(source.pid_status(999).is_none());
    assert!(source.live_pids().is_empty());
}

#[test]
fn kernel_procfs_source_state_reflects_process_state() {
    // Flip Running → Zombie on the ProcessTable; the bridge's
    // snapshot reflects the change without any cache — each
    // `pid_status` call projects fresh state from the live table.
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    table.insert(make_process(pid, 1, "worker")).unwrap();
    table.transition(pid, ProcState::Ready).unwrap();
    table.transition(pid, ProcState::Running).unwrap();

    let running = KernelProcFsSource::new(&table)
        .pid_status(pid)
        .expect("pid live");
    assert_eq!(running.state, ProcStatusState::Running);

    table.exit(pid, ExitStatus::Exited(0)).unwrap();

    let zombie = KernelProcFsSource::new(&table)
        .pid_status(pid)
        .expect("zombie still listed until reaped");
    assert_eq!(zombie.state, ProcStatusState::Zombie);
    assert_eq!(zombie.name, "worker");
    assert_eq!(zombie.pid, pid);
    assert_eq!(zombie.ppid, 1);
}

#[test]
fn kernel_procfs_source_version_uptime_meminfo_loadavg_are_placeholders() {
    // The version string carries the `-alpha (native-test)` marker
    // that flags the bridge as not-yet-using a build-time banner.
    // uptime / meminfo / loadavg are all zeroed until a clock
    // source + memory-accounting are plumbed through (separate
    // follow-up slices); the default-impl `storage()` falls back
    // to `"0 0 0\n"` because `storage_info()` returns `None`.
    let table = ProcessTable::new();
    let source = KernelProcFsSource::new(&table);
    assert_eq!(source.version(), "PMos 0.1.0-alpha (native-test)\n");
    assert_eq!(source.uptime(), "0 0\n");
    assert_eq!(source.meminfo(), "0 0 0\n");
    assert_eq!(source.loadavg(), "0.00 0.00 0.00 0/0 0\n");
    assert!(source.storage_info().is_none());
    assert_eq!(source.storage(), "0 0 0\n");
    // Default fd impl is untouched until block-driver + vfs reverse
    // lookup wiring lands; pid_fds on a live pid is still empty.
    assert!(source.pid_fds(1).is_empty());
}

// ---- /proc/<pid>/cmdline — argv projection (T168 follow-up) --------

/// Build a Vfs with a ProcFs backed by a StaticProcFsSource
/// populated with both a status snapshot and a cmdline byte
/// sequence. The canned `status` snapshot makes the pid directory
/// live; `pid_cmdline` then populates the `cmdline` file.
fn vfs_with_cmdline(statuses: Vec<ProcStatusSnapshot>, cmdlines: Vec<(Pid, Vec<u8>)>) -> Vfs {
    let mut source = StaticProcFsSource::default();
    for snap in statuses {
        source.set_pid_status(snap);
    }
    for (pid, bytes) in cmdlines {
        source.set_pid_cmdline(pid, bytes);
    }
    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(kernel::fs::tmpfs::TmpFs::new()))
        .unwrap();
    vfs.mount("/proc", Box::new(ProcFs::new(Box::new(source))))
        .unwrap();
    vfs
}

#[test]
fn cmdline_lookup_returns_regular_file_for_pid_with_cmdline() {
    let pid: Pid = 42;
    let cmdline: Vec<u8> = b"/usr/bin/edit\0file.txt\0".to_vec();
    let mut vfs = vfs_with_cmdline(vec![stub_status(pid, 1, "edit")], vec![(pid, cmdline)]);

    let st = vfs.stat(&format!("/proc/{pid}/cmdline")).unwrap();
    assert_eq!(st.ty, NodeType::RegularFile);
}

#[test]
fn cmdline_read_returns_nul_separated_bytes() {
    let pid: Pid = 42;
    let expected: Vec<u8> = b"/usr/bin/edit\0file.txt\0".to_vec();
    let mut vfs = vfs_with_cmdline(
        vec![stub_status(pid, 1, "edit")],
        vec![(pid, expected.clone())],
    );

    let path = format!("/proc/{pid}/cmdline");
    let mut out = Vec::new();
    let mut buf = [0u8; 32];
    let mut off: u64 = 0;
    loop {
        let n = vfs.read(&path, off, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        off += n as u64;
    }
    assert_eq!(out, expected);
}

#[test]
fn cmdline_stat_reports_mode_0o444_and_correct_size() {
    let pid: Pid = 42;
    let cmdline: Vec<u8> = b"/usr/bin/edit\0file.txt\0".to_vec();
    let expected_size = cmdline.len() as u64;
    let mut vfs = vfs_with_cmdline(vec![stub_status(pid, 1, "edit")], vec![(pid, cmdline)]);

    let st = vfs.stat(&format!("/proc/{pid}/cmdline")).unwrap();
    assert_eq!(st.mode, 0o444);
    assert_eq!(st.size, expected_size);
}

#[test]
fn cmdline_lookup_rejects_pid_without_cmdline() {
    // pid 99 is live (has a status snapshot) but no cmdline set —
    // `/proc/99/cmdline` must report NotFound, not serve an empty
    // regular file.
    let pid: Pid = 99;
    let mut vfs = vfs_with_cmdline(vec![stub_status(pid, 1, "sh")], vec![]);

    let err = vfs.stat(&format!("/proc/{pid}/cmdline")).unwrap_err();
    assert_eq!(err, FsError::NotFound);

    let err = vfs
        .read(&format!("/proc/{pid}/cmdline"), 0, &mut [0u8; 16])
        .unwrap_err();
    assert_eq!(err, FsError::NotFound);
}

#[test]
fn cmdline_listed_in_pid_subtree_readdir() {
    // readdir on `/proc/42` must include `cmdline` alongside
    // `status` and `fd` when the source records a cmdline.
    let pid: Pid = 42;
    let mut vfs = vfs_with_cmdline(
        vec![stub_status(pid, 1, "edit")],
        vec![(pid, b"/usr/bin/edit\0".to_vec())],
    );

    let entries = vfs.readdir(&format!("/proc/{pid}")).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"status"), "missing status: {:?}", names);
    assert!(names.contains(&"fd"), "missing fd: {:?}", names);

    let cmdline_entry = entries
        .iter()
        .find(|e| e.name == "cmdline")
        .expect("cmdline entry present");
    assert_eq!(cmdline_entry.ty, NodeType::RegularFile);
}

#[test]
fn cmdline_ino_does_not_collide_with_status_or_fd_dir() {
    // Pin the stride-offset numbering: cmdline must live at a slot
    // distinct from both the status file and the fd/ directory.
    // Spread across several pids so a future stride change shows up
    // as a test failure.
    for &pid in &[1 as Pid, 2, 42, 1024, 65_535] {
        let cmdline = pid_cmdline_ino(pid);
        let status = pid_status_ino(pid);
        let fd_dir = pid_fd_dir_ino(pid);
        assert_ne!(cmdline, status, "cmdline/status collision at pid {pid}");
        assert_ne!(cmdline, fd_dir, "cmdline/fd-dir collision at pid {pid}");
    }
}

#[test]
fn cmdline_empty_argv_yields_empty_bytes() {
    // Safety net: a pid with an empty argv (kernel thread or a
    // process before exec) serves an empty cmdline file — the
    // file exists (lookup succeeds) but reads as zero bytes. The
    // Some(vec![]) vs None distinction is deliberate.
    let pid: Pid = 7;
    let mut vfs = vfs_with_cmdline(
        vec![stub_status(pid, 1, "kthread")],
        vec![(pid, Vec::new())],
    );

    let st = vfs.stat(&format!("/proc/{pid}/cmdline")).unwrap();
    assert_eq!(st.size, 0);

    let mut buf = [0u8; 16];
    let n = vfs
        .read(&format!("/proc/{pid}/cmdline"), 0, &mut buf)
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn format_argv_cmdline_joins_with_nul_and_trailing_nul() {
    // Pin the byte layout the bridge relies on: each argv entry
    // followed by a single NUL, including the last, so readers can
    // split on 0x00 without special-casing the terminator.
    let argv = vec!["/usr/bin/edit".to_string(), "file.txt".to_string()];
    let bytes = format_argv_cmdline(&argv);
    assert_eq!(bytes, b"/usr/bin/edit\0file.txt\0".to_vec());

    // Empty argv yields zero bytes.
    assert!(format_argv_cmdline(&[]).is_empty());
}

#[test]
fn kernel_procfs_source_pid_cmdline_projects_argv() {
    // Live bridge: spawn a process with argv = ["/bin/sh", "-c",
    // "echo hi"] and assert `pid_cmdline` returns the matching
    // NUL-joined bytes. This is the KernelProcFsSource wiring
    // decision for this slice: Process already owns `argv`, so
    // projecting it is free.
    let mut table = ProcessTable::new();
    let pid = table.allocate_pid();
    let argv = vec![
        String::from("/bin/sh"),
        String::from("-c"),
        String::from("echo hi"),
    ];
    let proc = Process::new_starting(
        pid,
        1,
        "sh",
        argv.clone(),
        BTreeMap::new(),
        "/",
        CapSet::EMPTY,
        0,
        0,
        0,
    );
    table.insert(proc).unwrap();

    let source = KernelProcFsSource::new(&table);
    let got = source.pid_cmdline(pid).expect("live pid has cmdline");
    assert_eq!(got, b"/bin/sh\0-c\0echo hi\0".to_vec());
}

#[test]
fn kernel_procfs_source_pid_cmdline_none_for_missing_pid() {
    let table = ProcessTable::new();
    let source = KernelProcFsSource::new(&table);
    assert!(source.pid_cmdline(999).is_none());
}
