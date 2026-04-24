//! Integration tests for the `sysmon` binary CLI slice (T170 partial).
//!
//! Drive the compiled binary via `CARGO_BIN_EXE_sysmon` so the tests
//! observe the exact bytes userland emits. Each test builds a fake
//! `/proc` tree under `std::env::temp_dir()` keyed by tag + pid +
//! an atomic counter so parallel runners cannot collide. Successful
//! tests remove their tree; failures leave the tree in place for
//! post-mortem debugging.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const SYSMON: &str = env!("CARGO_BIN_EXE_sysmon");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!(
        "pmos-sysmon-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn write_pid_status(root: &PathBuf, pid: u32, body: &[u8]) {
    let dir = root.join(pid.to_string());
    fs::create_dir_all(&dir).expect("mkdir pid dir");
    fs::write(dir.join("status"), body).expect("write status");
}

fn run(proc_root: &PathBuf) -> std::process::Output {
    Command::new(SYSMON)
        .arg("--proc-root")
        .arg(proc_root)
        .output()
        .expect("spawn sysmon")
}

#[test]
fn empty_proc_root_prints_header_only() {
    let root = temp_dir("empty");
    let out = run(&root);

    assert_eq!(out.status.code(), Some(0), "exit: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");

    assert!(stdout.contains("PID"), "stdout = {stdout:?}");
    assert!(stdout.contains("NAME"), "stdout = {stdout:?}");
    assert!(stdout.contains("STATE"), "stdout = {stdout:?}");
    assert!(stdout.contains("PPID"), "stdout = {stdout:?}");

    for line in stdout.lines().skip(2) {
        assert!(
            line.trim().is_empty(),
            "unexpected pid row in empty /proc: {line:?}"
        );
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn single_pid_table_row_correct() {
    let root = temp_dir("single");
    write_pid_status(
        &root,
        1,
        b"Name:\tinit\nState:\tR (Running)\nPid:\t1\nPPid:\t0\n",
    );

    let out = run(&root);
    assert_eq!(out.status.code(), Some(0), "exit: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");

    let row = stdout
        .lines()
        .find(|l| l.starts_with("1 ") || l.starts_with("1\t"))
        .unwrap_or_else(|| panic!("no pid 1 row: {stdout:?}"));
    assert!(row.contains("init"), "row = {row:?}");
    assert!(row.contains("R (Running)"), "row = {row:?}");
    assert!(row.trim_end().ends_with(" 0"), "row = {row:?}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn multiple_pids_sorted_ascending() {
    let root = temp_dir("sorted");
    write_pid_status(
        &root,
        42,
        b"Name:\tedit\nState:\tS (Sleeping)\nPid:\t42\nPPid:\t1\n",
    );
    write_pid_status(
        &root,
        1,
        b"Name:\tinit\nState:\tR (Running)\nPid:\t1\nPPid:\t0\n",
    );
    write_pid_status(
        &root,
        100,
        b"Name:\tsh\nState:\tR (Running)\nPid:\t100\nPPid:\t1\n",
    );

    let out = run(&root);
    assert_eq!(out.status.code(), Some(0), "exit: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");

    let idx_1 = stdout.find("\n1 ").expect("pid 1 not found");
    let idx_42 = stdout.find("\n42 ").expect("pid 42 not found");
    let idx_100 = stdout.find("\n100 ").expect("pid 100 not found");

    assert!(idx_1 < idx_42, "pid 1 must precede pid 42 in {stdout:?}");
    assert!(idx_42 < idx_100, "pid 42 must precede pid 100 in {stdout:?}");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn malformed_status_skipped_with_stderr_warning() {
    let root = temp_dir("malformed");
    write_pid_status(&root, 5, b"this is not a valid status file\n");

    let out = run(&root);
    assert_eq!(out.status.code(), Some(0), "exit: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !stdout.contains("\n5 "),
        "pid 5 must not appear as a table row: {stdout:?}"
    );
    assert!(stderr.contains("pid 5"), "stderr = {stderr:?}");
    assert!(
        stderr.contains("failed to parse"),
        "stderr = {stderr:?}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn nonexistent_proc_root_exits_one() {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let missing = env::temp_dir().join(format!(
        "pmos-sysmon-missing-{}-{}",
        std::process::id(),
        n
    ));

    let out = run(&missing);
    assert_eq!(out.status.code(), Some(1), "exit: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr should mention path: stderr = {stderr:?}"
    );
}

#[test]
fn non_numeric_entries_ignored() {
    let root = temp_dir("nonnumeric");
    let foo_dir = root.join("foo");
    fs::create_dir_all(&foo_dir).expect("mkdir foo");
    fs::write(foo_dir.join("status"), b"garbage\n").expect("write foo status");
    write_pid_status(
        &root,
        42,
        b"Name:\tedit\nState:\tR (Running)\nPid:\t42\nPPid:\t1\n",
    );

    let out = run(&root);
    assert_eq!(out.status.code(), Some(0), "exit: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");

    assert!(stdout.contains("\n42 "), "pid 42 must appear: {stdout:?}");
    assert!(
        !stdout.contains("foo"),
        "non-numeric entry `foo` must not appear: {stdout:?}"
    );

    let _ = fs::remove_dir_all(&root);
}
