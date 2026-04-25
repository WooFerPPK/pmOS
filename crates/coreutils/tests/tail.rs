//! Integration tests for the `tail` coreutil binary.
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

const TAIL: &str = env!("CARGO_BIN_EXE_tail");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "pmos-tail-{}-{}-{}",
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
fn default_prints_last_ten_lines() {
    let dir = scratch_dir("default-ten");
    let mut content = String::new();
    for i in 1..=15 {
        content.push_str(&format!("line {i}\n"));
    }
    let path = write_file(&dir, "input.txt", content.as_bytes());

    let out = Command::new(TAIL)
        .arg(&path)
        .output()
        .expect("spawn tail");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let expected = "line 6\nline 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13\nline 14\nline 15\n";
    assert_eq!(out.stdout, expected.as_bytes());
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_overrides_count() {
    let dir = scratch_dir("dashn");
    let path = write_file(&dir, "input.txt", b"a\nb\nc\nd\ne\n");

    let out = Command::new(TAIL)
        .arg("-n")
        .arg("3")
        .arg(&path)
        .output()
        .expect("spawn tail");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"c\nd\ne\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_zero_prints_nothing() {
    let dir = scratch_dir("zero");
    let path = write_file(&dir, "input.txt", b"alpha\nbravo\ncharlie\n");

    let out = Command::new(TAIL)
        .arg("-n")
        .arg("0")
        .arg(&path)
        .output()
        .expect("spawn tail");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn multi_file_prefixes_each_with_filename_header() {
    let dir = scratch_dir("multi");
    let a = write_file(&dir, "a.txt", b"alpha\nbravo\n");
    let b = write_file(&dir, "b.txt", b"charlie\ndelta\n");

    let out = Command::new(TAIL)
        .arg("-n")
        .arg("1")
        .arg(&a)
        .arg(&b)
        .output()
        .expect("spawn tail");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let expected = format!(
        "==> {} <==\nbravo\n\n==> {} <==\ndelta\n",
        a.display(),
        b.display(),
    );
    assert_eq!(out.stdout, expected.as_bytes());
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn stdin_mode_no_header() {
    let mut child = Command::new(TAIL)
        .arg("-n")
        .arg("2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tail");

    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"first\nsecond\nthird\n")
        .expect("write stdin");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait tail");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"second\nthird\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn missing_file_continues_and_exits_one() {
    let dir = scratch_dir("missing");
    let good = write_file(&dir, "good.txt", b"OK\n");
    let missing = dir.join("nope.txt");

    let out = Command::new(TAIL)
        .arg("-n")
        .arg("1")
        .arg(&missing)
        .arg(&good)
        .output()
        .expect("spawn tail");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("==>"), "stdout = {stdout:?}");
    assert!(stdout.contains("OK\n"), "stdout = {stdout:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("tail:"), "stderr = {stderr:?}");
    assert!(stderr.contains("nope.txt"), "stderr = {stderr:?}");
    cleanup(&dir);
}

#[test]
fn negative_count_is_error_exits_one() {
    let dir = scratch_dir("neg");
    let path = write_file(&dir, "input.txt", b"a\nb\nc\n");

    let out = Command::new(TAIL)
        .arg("-n")
        .arg("-5")
        .arg(&path)
        .output()
        .expect("spawn tail");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("tail:"), "stderr = {stderr:?}");
    assert!(stderr.contains("invalid count"), "stderr = {stderr:?}");
    cleanup(&dir);
}

#[test]
fn dash_c_prints_last_n_bytes() {
    let dir = scratch_dir("dashc");
    let path = write_file(&dir, "input.txt", b"hello world\n");

    let out = Command::new(TAIL)
        .arg("-c")
        .arg("5")
        .arg(&path)
        .output()
        .expect("spawn tail");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"orld\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_c_with_multiple_files_prefixes_each() {
    let dir = scratch_dir("dashc-multi");
    let a = write_file(&dir, "a.txt", b"alphabravo");
    let b = write_file(&dir, "b.txt", b"charliedelta");

    let out = Command::new(TAIL)
        .arg("-c")
        .arg("5")
        .arg(&a)
        .arg(&b)
        .output()
        .expect("spawn tail");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let expected = format!(
        "==> {} <==\nbravo\n==> {} <==\ndelta",
        a.display(),
        b.display(),
    );
    assert_eq!(out.stdout, expected.as_bytes());
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_c_zero_prints_nothing() {
    let dir = scratch_dir("dashc-zero");
    let path = write_file(&dir, "input.txt", b"some bytes here\n");

    let out = Command::new(TAIL)
        .arg("-c")
        .arg("0")
        .arg(&path)
        .output()
        .expect("spawn tail");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_c_larger_than_input_prints_whole_input() {
    let dir = scratch_dir("dashc-large");
    let path = write_file(&dir, "input.txt", b"abc\n");

    let out = Command::new(TAIL)
        .arg("-c")
        .arg("100")
        .arg(&path)
        .output()
        .expect("spawn tail");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"abc\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_c_via_stdin() {
    let mut child = Command::new(TAIL)
        .arg("-c")
        .arg("4")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tail");

    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"abcdefghij")
        .expect("write stdin");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait tail");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"ghij");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn dash_c_overrides_dash_n_when_both_given() {
    let dir = scratch_dir("dashc-overrides");
    let path = write_file(&dir, "input.txt", b"line1\nline2\nline3\n");

    let out = Command::new(TAIL)
        .arg("-n")
        .arg("2")
        .arg("-c")
        .arg("3")
        .arg(&path)
        .output()
        .expect("spawn tail");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"e3\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}
