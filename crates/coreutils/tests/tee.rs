//! Integration tests for the `tee` coreutil binary.
//!
//! Driven through `std::process::Command` so the tests see the exact
//! bytes the userland binary emits. Each test pipes input via
//! `Stdio::piped()` and inspects stdout / stderr / exit code
//! byte-by-byte. File-using tests place outputs under
//! `std::env::temp_dir()` with a per-test directory keyed by the
//! test name and process id; each test cleans up its directory on
//! success.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const TEE: &str = env!("CARGO_BIN_EXE_tee");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("pmos-tee-{}-{}-{}", tag, std::process::id(), n));
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

fn run_with_stdin(
    args: &[&std::ffi::OsStr],
    stdin_bytes: &[u8],
) -> (Option<i32>, Vec<u8>, Vec<u8>) {
    let mut child = Command::new(TEE)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tee");

    if !stdin_bytes.is_empty() {
        child
            .stdin
            .as_mut()
            .expect("stdin pipe")
            .write_all(stdin_bytes)
            .expect("write stdin");
    }
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait tee");
    (out.status.code(), out.stdout, out.stderr)
}

#[test]
fn writes_stdin_to_stdout_when_no_files() {
    let (code, stdout, stderr) = run_with_stdin(&[], b"hello\n");
    assert_eq!(code, Some(0), "exit code: {code:?}");
    assert_eq!(stdout, b"hello\n");
    assert!(stderr.is_empty(), "stderr = {:?}", stderr);
}

#[test]
fn writes_stdin_to_stdout_and_one_file() {
    let dir = scratch_dir("one");
    let out_path = dir.join("out.txt");

    let (code, stdout, stderr) = run_with_stdin(&[out_path.as_os_str()], b"hello\n");
    assert_eq!(code, Some(0), "exit code: {code:?}");
    assert_eq!(stdout, b"hello\n");
    assert!(stderr.is_empty(), "stderr = {:?}", stderr);

    let file_bytes = fs::read(&out_path).expect("read out.txt");
    assert_eq!(file_bytes, b"hello\n");
    cleanup(&dir);
}

#[test]
fn writes_stdin_to_multiple_files() {
    let dir = scratch_dir("multi");
    let a_path = dir.join("a.txt");
    let b_path = dir.join("b.txt");

    let (code, stdout, stderr) =
        run_with_stdin(&[a_path.as_os_str(), b_path.as_os_str()], b"shared bytes\n");
    assert_eq!(code, Some(0), "exit code: {code:?}");
    assert_eq!(stdout, b"shared bytes\n");
    assert!(stderr.is_empty(), "stderr = {:?}", stderr);

    assert_eq!(fs::read(&a_path).expect("read a.txt"), b"shared bytes\n");
    assert_eq!(fs::read(&b_path).expect("read b.txt"), b"shared bytes\n");
    cleanup(&dir);
}

#[test]
fn dash_a_appends_to_existing_file() {
    let dir = scratch_dir("append");
    let path = write_file(&dir, "log.txt", b"old\n");

    let (code, stdout, stderr) =
        run_with_stdin(&[std::ffi::OsStr::new("-a"), path.as_os_str()], b"new\n");
    assert_eq!(code, Some(0), "exit code: {code:?}");
    assert_eq!(stdout, b"new\n");
    assert!(stderr.is_empty(), "stderr = {:?}", stderr);

    let file_bytes = fs::read(&path).expect("read log.txt");
    assert_eq!(file_bytes, b"old\nnew\n");
    cleanup(&dir);
}

#[test]
fn default_overwrites_existing_file() {
    let dir = scratch_dir("over");
    let path = write_file(&dir, "log.txt", b"old");

    let (code, stdout, stderr) = run_with_stdin(&[path.as_os_str()], b"new");
    assert_eq!(code, Some(0), "exit code: {code:?}");
    assert_eq!(stdout, b"new");
    assert!(stderr.is_empty(), "stderr = {:?}", stderr);

    let file_bytes = fs::read(&path).expect("read log.txt");
    assert_eq!(file_bytes, b"new");
    cleanup(&dir);
}

#[test]
fn bad_path_continues_writing_others() {
    let dir = scratch_dir("badpath");
    let good_path = dir.join("good.txt");
    let bad_path = std::path::PathBuf::from("/nonexistent/foo");

    let (code, stdout, stderr) =
        run_with_stdin(&[bad_path.as_os_str(), good_path.as_os_str()], b"payload\n");
    assert_eq!(code, Some(1), "exit code: {code:?}");
    assert_eq!(stdout, b"payload\n");
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(stderr.contains("tee:"), "stderr = {stderr:?}");

    let file_bytes = fs::read(&good_path).expect("read good.txt");
    assert_eq!(file_bytes, b"payload\n");
    cleanup(&dir);
}

#[test]
fn unknown_flag_exits_one() {
    let (code, stdout, stderr) = run_with_stdin(&[std::ffi::OsStr::new("-x")], b"");
    assert_eq!(code, Some(1), "exit code: {code:?}");
    assert!(stdout.is_empty(), "stdout = {:?}", stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(stderr.contains("unknown flag"), "stderr = {stderr:?}");
}
