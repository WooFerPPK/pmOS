//! Integration tests for the `cp` coreutil binary.
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
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const CP: &str = env!("CARGO_BIN_EXE_cp");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "pmos-cp-{}-{}-{}",
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
fn copies_file_content_verbatim() {
    let dir = scratch_dir("verbatim");
    let mut payload: Vec<u8> = Vec::with_capacity(128);
    payload.extend_from_slice(b"the quick brown fox");
    payload.push(0x00);
    payload.extend_from_slice(b" jumps over ");
    payload.push(0xC3);
    payload.push(0xA9);
    payload.extend_from_slice(b" the lazy dog\n");
    while payload.len() < 100 {
        payload.push(b'.');
    }
    let src = write_file(&dir, "src.bin", &payload);
    let dst = dir.join("dst.bin");

    let out = Command::new(CP)
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let copied = fs::read(&dst).expect("read dst");
    assert_eq!(copied, payload);
    cleanup(&dir);
}

#[test]
fn overwrites_existing_dst() {
    let dir = scratch_dir("overwrite");
    let src = write_file(&dir, "src.txt", b"new content");
    let dst = write_file(&dir, "dst.txt", b"old content");

    let out = Command::new(CP)
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let copied = fs::read(&dst).expect("read dst");
    assert_eq!(copied, b"new content");
    cleanup(&dir);
}

#[test]
fn missing_src_exits_one_and_stderr_has_path() {
    let dir = scratch_dir("missing");
    let missing = dir.join("does-not-exist.txt");
    let dst = dir.join("dst.txt");

    let out = Command::new(CP)
        .arg(&missing)
        .arg(&dst)
        .output()
        .expect("spawn cp");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cp:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr = {stderr:?}"
    );
    assert!(!dst.exists(), "dst should not have been created");
    cleanup(&dir);
}

#[test]
fn wrong_arg_count_exits_one_with_usage() {
    let out = Command::new(CP).output().expect("spawn cp");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage"), "stderr = {stderr:?}");
    assert!(stderr.contains("cp <src> <dst>"), "stderr = {stderr:?}");

    let out_one = Command::new(CP).arg("only-one").output().expect("spawn cp");
    assert_eq!(
        out_one.status.code(),
        Some(1),
        "exit status: {:?}",
        out_one.status
    );
    let stderr_one = String::from_utf8_lossy(&out_one.stderr);
    assert!(stderr_one.contains("usage"), "stderr = {stderr_one:?}");
    assert!(
        stderr_one.contains("cp <src> <dst>"),
        "stderr = {stderr_one:?}"
    );
}
