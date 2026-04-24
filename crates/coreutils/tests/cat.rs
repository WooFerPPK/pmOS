//! Integration tests for the `cat` coreutil binary.
//!
//! Driven through `std::process::Command` so the tests see the exact
//! bytes the userland binary emits. Temp files are placed under
//! `std::env::temp_dir()` with a per-test directory keyed by the
//! test name and process id; each test cleans up its directory on
//! success (a failing test leaves its scratch tree intact for
//! debugging).

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const CAT: &str = env!("CARGO_BIN_EXE_cat");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "pmos-cat-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    let mut f = fs::File::create(&path).expect("create temp file");
    f.write_all(bytes).expect("write temp file");
    path
}

fn cleanup(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cat_single_file_writes_content_to_stdout() {
    let dir = scratch_dir("single");
    let payload: &[u8] = b"the quick brown fox\n";
    let path = write_file(&dir, "a.txt", payload);

    let out = Command::new(CAT)
        .arg(&path)
        .output()
        .expect("spawn cat");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, payload);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn cat_two_files_concatenates() {
    let dir = scratch_dir("concat");
    let a = write_file(&dir, "a.txt", b"AAA");
    let b = write_file(&dir, "b.txt", b"BBB");

    let out = Command::new(CAT)
        .arg(&a)
        .arg(&b)
        .output()
        .expect("spawn cat");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"AAABBB");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn cat_stdin_with_no_args_echoes() {
    let mut child = Command::new(CAT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cat");

    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"hello")
        .expect("write stdin");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait cat");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"hello");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn cat_missing_file_exits_one_and_stderr_has_path() {
    let dir = scratch_dir("missing");
    let missing = dir.join("does-not-exist.txt");

    let out = Command::new(CAT)
        .arg(&missing)
        .output()
        .expect("spawn cat");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cat:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr = {stderr:?}"
    );
    cleanup(&dir);
}

#[test]
fn cat_continues_after_missing_file_but_reports_error() {
    let dir = scratch_dir("partial");
    let good = write_file(&dir, "good.txt", b"OK\n");
    let missing = dir.join("nope.txt");

    let out = Command::new(CAT)
        .arg(&missing)
        .arg(&good)
        .output()
        .expect("spawn cat");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"OK\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nope.txt"), "stderr = {stderr:?}");
    cleanup(&dir);
}
