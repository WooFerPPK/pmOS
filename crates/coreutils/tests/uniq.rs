//! Integration tests for the `uniq` coreutil binary.
//!
//! Driven through `std::process::Command` so the tests see the exact
//! bytes the userland binary emits. Temp files are placed under
//! `std::env::temp_dir()` with a per-test directory keyed by tag +
//! pid + atomic counter; each test cleans up its directory on
//! success (a failing test leaves its scratch tree intact for
//! debugging).

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const UNIQ: &str = env!("CARGO_BIN_EXE_uniq");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "pmos-uniq-{}-{}-{}",
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
fn default_collapses_adjacent_duplicates() {
    let dir = scratch_dir("default");
    let path = write_file(&dir, "in.txt", b"a\na\nb\na\n");

    let out = Command::new(UNIQ).arg(&path).output().expect("spawn uniq");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"a\nb\na\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_c_prefixes_count() {
    let dir = scratch_dir("count");
    let path = write_file(&dir, "in.txt", b"a\na\nb\na\n");

    let out = Command::new(UNIQ)
        .arg("-c")
        .arg(&path)
        .output()
        .expect("spawn uniq");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"2\ta\n1\tb\n1\ta\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_d_emits_only_duplicates() {
    let dir = scratch_dir("dup");
    let path = write_file(&dir, "in.txt", b"a\na\nb\na\n");

    let out = Command::new(UNIQ)
        .arg("-d")
        .arg(&path)
        .output()
        .expect("spawn uniq");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"a\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_u_emits_only_unique_lines() {
    let dir = scratch_dir("uniq-only");
    let path = write_file(&dir, "in.txt", b"a\na\nb\na\n");

    let out = Command::new(UNIQ)
        .arg("-u")
        .arg(&path)
        .output()
        .expect("spawn uniq");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"b\na\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_du_combined_emits_nothing() {
    let dir = scratch_dir("du");
    let path = write_file(&dir, "in.txt", b"a\na\nb\na\n");

    let out = Command::new(UNIQ)
        .arg("-du")
        .arg(&path)
        .output()
        .expect("spawn uniq");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn stdin_mode_works() {
    let mut child = Command::new(UNIQ)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn uniq");

    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"x\nx\ny\n")
        .expect("write stdin");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait uniq");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"x\ny\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn missing_file_exits_one() {
    let dir = scratch_dir("missing");
    let missing = dir.join("nope.txt");

    let out = Command::new(UNIQ)
        .arg(&missing)
        .output()
        .expect("spawn uniq");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("uniq:"), "stderr = {stderr:?}");
    assert!(stderr.contains("nope.txt"), "stderr = {stderr:?}");
    cleanup(&dir);
}
