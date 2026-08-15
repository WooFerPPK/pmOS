//! Sysmon isolation tests against synthetic `/proc` and pure UI state.

use std::fs;
use std::path::{Path, PathBuf};

use sysmon::{
    collect_processes, collect_snapshot, read_status, MonitorAction, MonitorKey, MonitorMode,
    MonitorState, PointerTarget, ProcessCollection, ProcessScanStep, ProcessScanner,
    ProcessSnapshot, RefreshSchedule, PROC_FD_ENTRIES_PER_STEP, PROC_ROOT_ENTRIES_PER_STEP,
};

#[test]
fn process_scanner_bounds_root_status_and_fd_work_per_quantum() {
    let tmp = tempdir("sysmon-stepwise");
    for pid in 1..=PROC_ROOT_ENTRIES_PER_STEP as u32 + 1 {
        write_pid(
            &tmp,
            pid,
            &format!("proc-{pid}"),
            "S",
            1,
            1,
            1,
            if pid == 1 {
                PROC_FD_ENTRIES_PER_STEP * 2 + 1
            } else {
                0
            },
        );
    }

    let mut scanner = ProcessScanner::start(&tmp).expect("open synthetic proc");
    assert_eq!(scanner.step(), ProcessScanStep::Pending);
    assert_eq!(scanner.discovered_pid_count(), PROC_ROOT_ENTRIES_PER_STEP);
    assert_eq!(scanner.step(), ProcessScanStep::Pending);
    assert_eq!(
        scanner.discovered_pid_count(),
        PROC_ROOT_ENTRIES_PER_STEP + 1
    );

    // One quantum reads status and opens fd; later quanta count no more than
    // 32 descriptors. Completion of the 65th descriptor is a separate quantum.
    assert_eq!(scanner.step(), ProcessScanStep::Pending);
    assert_eq!(scanner.pending_fd_count(), Some(0));
    assert_eq!(scanner.step(), ProcessScanStep::Pending);
    assert_eq!(scanner.pending_fd_count(), Some(PROC_FD_ENTRIES_PER_STEP));
    assert_eq!(scanner.step(), ProcessScanStep::Pending);
    assert_eq!(
        scanner.pending_fd_count(),
        Some(PROC_FD_ENTRIES_PER_STEP * 2)
    );
    assert_eq!(scanner.step(), ProcessScanStep::Pending);
    assert_eq!(scanner.pending_fd_count(), None);

    let collection = loop {
        match scanner.step() {
            ProcessScanStep::Pending => {}
            ProcessScanStep::Complete(collection) => break collection,
        }
    };
    assert_eq!(collection.processes.len(), PROC_ROOT_ENTRIES_PER_STEP + 1);
    assert_eq!(collection.processes[0].open_fds, Some(65));
    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn fd_count_status_skips_fd_directories_in_the_production_scan_shape() {
    let tmp = tempdir("sysmon-fd-count");
    for fixed in ["loadavg", "meminfo", "storage", "uptime", "version"] {
        fs::write(tmp.join(fixed), b"fixture\n").unwrap();
    }
    for pid in 1..=11 {
        write_pid_with_fd_count(&tmp, pid, pid as usize + 2);
    }

    let mut scanner = ProcessScanner::start(&tmp).expect("open synthetic proc");
    let mut quanta = 0;
    let collection = loop {
        quanta += 1;
        match scanner.step() {
            ProcessScanStep::Pending => {}
            ProcessScanStep::Complete(collection) => break collection,
        }
    };

    assert_eq!(quanta, 14);
    assert_eq!(collection.processes.len(), 11);
    assert!(collection.warnings.is_empty());
    assert_eq!(collection.processes[0].open_fds, Some(3));
    assert_eq!(collection.processes[10].open_fds, Some(13));
    assert!(collection
        .processes
        .iter()
        .all(|process| process.open_fds.is_some()));
    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn fd_count_parser_is_optional_but_rejects_a_malformed_present_field() {
    let tmp = tempdir("sysmon-fd-count-parse");
    write_pid(&tmp, 7, "legacy", "S", 1, 1, 2, 0);
    assert_eq!(read_status(&tmp.join("7/status")).unwrap().open_fds, None);

    write_pid_with_fd_count(&tmp, 8, 9);
    assert_eq!(
        read_status(&tmp.join("8/status")).unwrap().open_fds,
        Some(9)
    );

    let malformed = tmp.join("9");
    fs::create_dir(&malformed).unwrap();
    fs::write(
        malformed.join("status"),
        b"Name:\tbad\nState:\tS\nPid:\t9\nPPid:\t1\nFDCount:\tnine\n",
    )
    .unwrap();
    assert!(read_status(&malformed.join("status"))
        .unwrap_err()
        .contains("bad FDCount"));

    fs::write(
        malformed.join("status"),
        b"Name:\tbad\nState:\tS\nPid:\t9\nPPid:\t1\nFDCount:\t9 trailing\n",
    )
    .unwrap();
    assert!(read_status(&malformed.join("status"))
        .unwrap_err()
        .contains("bad FDCount"));

    fs::write(
        malformed.join("status"),
        b"Name:\tbad\nState:\tS\nPid:\t9\nPPid:\t1\nFDCount:\t9\nFDCount:\t10\n",
    )
    .unwrap();
    assert!(read_status(&malformed.join("status"))
        .unwrap_err()
        .contains("duplicate FDCount"));

    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn collection_reports_memory_and_open_fds_in_pid_order() {
    let tmp = tempdir("sysmon-collect");
    write_pid(&tmp, 9, "edit", "S (sleeping)", 1, 256, 512, 4);
    write_pid(&tmp, 1, "init", "R (running)", 0, 64, 96, 3);
    write_pid(&tmp, 5, "shell", "S (sleeping)", 1, 128, 256, 5);

    let collection = collect_processes(&tmp).expect("collect synthetic proc");
    assert!(collection.warnings.is_empty());
    assert_eq!(
        collection
            .processes
            .iter()
            .map(|process| process.pid)
            .collect::<Vec<_>>(),
        vec![1, 5, 9]
    );
    assert_eq!(collection.processes[1].name, "shell");
    assert_eq!(collection.processes[1].vm_size_kib, 128);
    assert_eq!(collection.processes[1].vm_peak_kib, 256);
    assert_eq!(collection.processes[1].open_fds, Some(5));

    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn collection_surfaces_malformed_or_racing_rows() {
    let tmp = tempdir("sysmon-malformed");
    fs::create_dir(tmp.join("not-a-pid")).unwrap();
    fs::create_dir(tmp.join("7")).unwrap();
    fs::write(tmp.join("7/status"), b"not a status file\n").unwrap();
    write_pid(&tmp, 8, "wrong", "R", 1, 1, 1, 0);
    fs::write(
        tmp.join("8/status"),
        b"Name:\twrong\nState:\tR\nPid:\t99\nPPid:\t1\n",
    )
    .unwrap();

    let collection = collect_processes(&tmp).expect("proc root remains readable");
    assert!(collection.processes.is_empty());
    assert_eq!(collection.warnings.len(), 2);
    assert!(collection
        .warnings
        .iter()
        .any(|warning| warning.contains("pid 7")));
    assert!(collection
        .warnings
        .iter()
        .any(|warning| warning.contains("mismatched pid 99")));

    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn legacy_status_without_an_fd_directory_is_warned_and_skipped() {
    let tmp = tempdir("sysmon-missing-fd-dir");
    let pid_root = tmp.join("7");
    fs::create_dir(&pid_root).unwrap();
    fs::write(
        pid_root.join("status"),
        b"Name:\tlegacy\nState:\tS\nPid:\t7\nPPid:\t1\n",
    )
    .unwrap();

    let collection = collect_processes(&tmp).expect("proc root remains readable");
    assert!(collection.processes.is_empty());
    assert_eq!(collection.warnings.len(), 1);
    assert!(collection.warnings[0].contains("pid 7"));
    assert!(collection.warnings[0].contains("fd enumeration failed"));

    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn unreadable_proc_root_is_an_error_instead_of_an_empty_table() {
    let missing = temp_path("sysmon-missing");
    let error = collect_processes(&missing).expect_err("missing root must fail");
    assert!(error.contains("failed to open"));
    assert!(error.contains(missing.to_str().unwrap()));
}

#[test]
fn compatibility_rows_still_truncate_long_names() {
    let tmp = tempdir("sysmon-trunc");
    write_pid(
        &tmp,
        3,
        "a-name-longer-than-sixteen-chars",
        "R",
        1,
        20,
        24,
        1,
    );
    let rows = collect_snapshot(&tmp);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].contains('…'));
    assert!(rows[0].contains("20"));
    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn refresh_preserves_selection_by_pid_and_keeps_it_visible() {
    let mut state = MonitorState::new(Some(90), true);
    state.apply_refresh(Ok(collection(1..=8)), 3);
    for _ in 0..5 {
        state.handle_key(MonitorKey::Down, 3);
    }
    assert_eq!(state.selected_pid(), Some(6));
    assert_eq!(state.scroll(), 3);

    let reordered = ProcessCollection {
        processes: vec![process(2), process(4), process(6), process(8), process(10)],
        warnings: Vec::new(),
    };
    state.apply_refresh(Ok(reordered), 3);
    assert_eq!(state.selected_pid(), Some(6));
    assert_eq!(state.selected_index(), Some(2));
    assert!(state.scroll() <= 2);
}

#[test]
fn refresh_error_preserves_last_good_rows_and_is_visible() {
    let mut state = MonitorState::new(None, false);
    state.apply_refresh(Ok(collection(1..=2)), 4);
    state.apply_refresh(Err("/proc read failed".to_string()), 4);

    assert_eq!(state.processes().len(), 2);
    assert_eq!(state.selected_pid(), Some(1));
    assert_eq!(state.status(), "Error: /proc read failed");
}

#[test]
fn keyboard_and_pointer_navigation_scroll_without_losing_bounds() {
    let mut state = MonitorState::new(None, true);
    state.apply_refresh(Ok(collection(1..=9)), 3);
    assert_eq!(state.handle_key(MonitorKey::PageDown, 3), None);
    assert_eq!(state.selected_pid(), Some(4));
    assert_eq!(state.scroll(), 1);
    state.handle_pointer(PointerTarget::Row(7), 3);
    assert_eq!(state.selected_pid(), Some(8));
    assert_eq!(state.scroll(), 5);
    state.handle_key(MonitorKey::End, 3);
    state.handle_key(MonitorKey::Down, 3);
    assert_eq!(state.selected_pid(), Some(9));
    assert_eq!(state.scroll(), 6);
    state.handle_key(MonitorKey::Home, 3);
    assert_eq!(state.selected_pid(), Some(1));
    assert_eq!(state.scroll(), 0);
}

#[test]
fn terminate_is_read_only_without_runtime_capability() {
    let mut state = MonitorState::new(None, false);
    state.apply_refresh(Ok(collection(7..=7)), 4);
    assert_eq!(state.handle_key(MonitorKey::Terminate, 4), None);
    assert!(matches!(state.mode(), MonitorMode::Browse));
    assert_eq!(state.status(), "Read-only: PROC_KILL_ANY is not available");
}

#[test]
fn terminate_refuses_the_monitor_itself_even_with_capability() {
    let mut state = MonitorState::new(Some(7), true);
    state.apply_refresh(Ok(collection(7..=7)), 4);
    state.handle_key(MonitorKey::Terminate, 4);
    assert!(matches!(state.mode(), MonitorMode::Browse));
    assert_eq!(
        state.status(),
        "Refusing to terminate System Monitor itself"
    );
}

#[test]
fn terminate_requires_confirmation_and_targets_stable_pid() {
    let mut state = MonitorState::new(Some(99), true);
    state.apply_refresh(Ok(collection(7..=9)), 4);
    state.handle_key(MonitorKey::Down, 4);
    state.handle_key(MonitorKey::Terminate, 4);
    assert_eq!(
        state.mode(),
        &MonitorMode::ConfirmTerminate {
            pid: 8,
            name: "proc-8".to_string(),
        }
    );
    assert_eq!(
        state.handle_key(MonitorKey::Enter, 4),
        Some(MonitorAction::Terminate(8))
    );
    state.complete_termination(8, Ok(()));
    assert_eq!(state.status(), "Terminate requested for PID 8");
}

#[test]
fn periodic_refresh_preserves_confirmation_for_the_same_live_pid() {
    let mut state = MonitorState::new(Some(99), true);
    state.apply_refresh(Ok(collection(7..=9)), 4);
    state.handle_key(MonitorKey::Down, 4);
    state.handle_key(MonitorKey::Terminate, 4);

    state.apply_refresh(
        Ok(ProcessCollection {
            processes: vec![process(6), process(8), process(10)],
            warnings: Vec::new(),
        }),
        4,
    );

    assert_eq!(
        state.mode(),
        &MonitorMode::ConfirmTerminate {
            pid: 8,
            name: "proc-8".to_string(),
        }
    );
    assert_eq!(state.status(), "Confirm termination of PID 8");
    assert_eq!(
        state.handle_key(MonitorKey::Enter, 4),
        Some(MonitorAction::Terminate(8))
    );
}

#[test]
fn periodic_refresh_cancels_confirmation_when_the_target_exits() {
    let mut state = MonitorState::new(Some(99), true);
    state.apply_refresh(Ok(collection(7..=9)), 4);
    state.handle_key(MonitorKey::Down, 4);
    state.handle_key(MonitorKey::Terminate, 4);

    state.apply_refresh(
        Ok(ProcessCollection {
            processes: vec![process(7), process(9)],
            warnings: Vec::new(),
        }),
        4,
    );

    assert!(matches!(state.mode(), MonitorMode::Browse));
    assert_eq!(
        state.status(),
        "Process PID 8 exited before termination was confirmed"
    );
    assert_eq!(state.handle_key(MonitorKey::Enter, 4), None);
}

#[test]
fn terminate_confirmation_can_be_cancelled_and_errors_are_visible() {
    let mut state = MonitorState::new(None, true);
    state.apply_refresh(Ok(collection(3..=3)), 4);
    state.handle_pointer(PointerTarget::Terminate, 4);
    state.handle_key(MonitorKey::Escape, 4);
    assert!(matches!(state.mode(), MonitorMode::Browse));
    assert_eq!(state.status(), "Termination cancelled");

    state.complete_termination(3, Err("errno 76".to_string()));
    assert_eq!(state.status(), "Error: failed to terminate PID 3: errno 76");
}

#[test]
fn refresh_and_close_actions_are_explicit() {
    let mut state = MonitorState::new(None, false);
    assert_eq!(
        state.handle_key(MonitorKey::Refresh, 3),
        Some(MonitorAction::Refresh)
    );
    assert_eq!(
        state.handle_pointer(PointerTarget::Close, 3),
        Some(MonitorAction::Close)
    );
}

#[test]
fn refresh_schedule_fires_at_one_second_without_tick_count_guessing() {
    let mut schedule = RefreshSchedule::new(250, 1_000);
    assert_eq!(schedule.remaining_ms(250), 1_000);
    assert!(!schedule.take_due(1_249));
    assert_eq!(schedule.remaining_ms(1_249), 1);
    assert!(schedule.take_due(1_250));
    assert_eq!(schedule.remaining_ms(1_250), 1_000);
    assert!(!schedule.take_due(2_249));
    assert!(schedule.take_due(2_250));
    assert!(!schedule.take_due(2_250));
}

fn collection(pids: impl IntoIterator<Item = u32>) -> ProcessCollection {
    ProcessCollection {
        processes: pids.into_iter().map(process).collect(),
        warnings: Vec::new(),
    }
}

fn process(pid: u32) -> ProcessSnapshot {
    ProcessSnapshot {
        pid,
        name: format!("proc-{pid}"),
        state: "S (sleeping)".to_string(),
        ppid: 1,
        vm_size_kib: u64::from(pid) * 64,
        vm_peak_kib: u64::from(pid) * 128,
        open_fds: Some(4),
    }
}

fn temp_path(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pmos-{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn tempdir(prefix: &str) -> PathBuf {
    let dir = temp_path(prefix);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[allow(clippy::too_many_arguments)]
fn write_pid(
    root: &Path,
    pid: u32,
    name: &str,
    state: &str,
    ppid: u32,
    vm_size_kib: u64,
    vm_peak_kib: u64,
    fds: usize,
) {
    let dir = root.join(pid.to_string());
    fs::create_dir(&dir).unwrap();
    let body = format!(
        "Name:\t{name}\nState:\t{state}\nPid:\t{pid}\nPPid:\t{ppid}\nVmSize:\t{vm_size_kib} kB\nVmPeak:\t{vm_peak_kib} kB\n"
    );
    fs::write(dir.join("status"), body).unwrap();
    let fd_dir = dir.join("fd");
    fs::create_dir(fd_dir.as_path()).unwrap();
    for fd in 0..fds {
        fs::write(fd_dir.join(fd.to_string()), format!("fixture:[{fd}]")).unwrap();
    }
}

fn write_pid_with_fd_count(root: &Path, pid: u32, open_fds: usize) {
    let dir = root.join(pid.to_string());
    fs::create_dir(&dir).unwrap();
    let body = format!(
        "Name:\tproc-{pid}\nState:\tS (sleeping)\nPid:\t{pid}\nPPid:\t1\nVmSize:\t1 kB\nVmPeak:\t2 kB\nFDCount:\t{open_fds}\n"
    );
    fs::write(dir.join("status"), body).unwrap();
}
