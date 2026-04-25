//! Integration tests for the `head` coreutil binary.
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

const HEAD: &str = env!("CARGO_BIN_EXE_head");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "pmos-head-{}-{}-{}",
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
fn default_prints_first_ten_lines() {
    let dir = scratch_dir("default-ten");
    let mut content = String::new();
    for i in 1..=15 {
        content.push_str(&format!("line {i}\n"));
    }
    let path = write_file(&dir, "input.txt", content.as_bytes());

    let out = Command::new(HEAD)
        .arg(&path)
        .output()
        .expect("spawn head");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let expected = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\n";
    assert_eq!(out.stdout, expected.as_bytes());
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_overrides_count() {
    let dir = scratch_dir("dashn");
    let path = write_file(&dir, "input.txt", b"a\nb\nc\nd\ne\n");

    let out = Command::new(HEAD)
        .arg("-n")
        .arg("3")
        .arg(&path)
        .output()
        .expect("spawn head");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"a\nb\nc\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_zero_prints_nothing() {
    let dir = scratch_dir("zero");
    let path = write_file(&dir, "input.txt", b"alpha\nbravo\ncharlie\n");

    let out = Command::new(HEAD)
        .arg("-n")
        .arg("0")
        .arg(&path)
        .output()
        .expect("spawn head");

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

    let out = Command::new(HEAD)
        .arg("-n")
        .arg("1")
        .arg(&a)
        .arg(&b)
        .output()
        .expect("spawn head");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let expected = format!(
        "==> {} <==\nalpha\n\n==> {} <==\ncharlie\n",
        a.display(),
        b.display(),
    );
    assert_eq!(out.stdout, expected.as_bytes());
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn stdin_mode_no_header() {
    let mut child = Command::new(HEAD)
        .arg("-n")
        .arg("2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn head");

    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"first\nsecond\nthird\n")
        .expect("write stdin");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait head");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"first\nsecond\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn missing_file_continues_and_exits_one() {
    let dir = scratch_dir("missing");
    let good = write_file(&dir, "good.txt", b"OK\n");
    let missing = dir.join("nope.txt");

    let out = Command::new(HEAD)
        .arg("-n")
        .arg("1")
        .arg(&missing)
        .arg(&good)
        .output()
        .expect("spawn head");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("==>"), "stdout = {stdout:?}");
    assert!(stdout.contains("OK\n"), "stdout = {stdout:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("head:"), "stderr = {stderr:?}");
    assert!(stderr.contains("nope.txt"), "stderr = {stderr:?}");
    cleanup(&dir);
}

#[test]
fn negative_count_is_error_exits_one() {
    let dir = scratch_dir("neg");
    let path = write_file(&dir, "input.txt", b"a\nb\nc\n");

    let out = Command::new(HEAD)
        .arg("-n")
        .arg("-5")
        .arg(&path)
        .output()
        .expect("spawn head");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("head:"), "stderr = {stderr:?}");
    assert!(stderr.contains("invalid count"), "stderr = {stderr:?}");
    cleanup(&dir);
}

#[test]
fn bare_dash_n_form_works() {
    let dir = scratch_dir("bare");
    let path = write_file(&dir, "input.txt", b"a\nb\nc\nd\n");

    let out = Command::new(HEAD)
        .arg("-2")
        .arg(&path)
        .output()
        .expect("spawn head");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"a\nb\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}
