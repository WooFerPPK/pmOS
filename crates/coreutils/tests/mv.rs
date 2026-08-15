//! Integration tests for the `mv` coreutil binary.
//!
//! Driven through `std::process::Command` so the tests see the exact
//! bytes the userland binary emits. Temp files are placed under
//! `std::env::temp_dir()` with a per-test directory keyed by the
//! test name and process id; each test cleans up its directory on
//! success (a failing test leaves its scratch tree intact for
//! debugging).
//!
//! The `ErrorKind::CrossesDevices` fallback arm is not directly
//! exercised here: `std::env::temp_dir()` is one mount on all our
//! test hosts. See `src/bin/mv.rs` module doc.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const MV: &str = env!("CARGO_BIN_EXE_mv");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("pmos-mv-{}-{}-{}", tag, std::process::id(), n));
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
fn moves_a_file_within_same_directory() {
    let dir = scratch_dir("same-dir");
    let src = write_file(&dir, "src", b"hello");
    let dst = dir.join("dst");

    let out = Command::new(MV)
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn mv");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert!(!src.exists(), "src should be gone");
    let moved = fs::read(&dst).expect("read dst");
    assert_eq!(moved, b"hello");
    cleanup(&dir);
}

#[test]
fn moves_a_file_across_directories() {
    let dir = scratch_dir("cross-dir");
    let a = dir.join("a");
    let b = dir.join("b");
    fs::create_dir(&a).expect("create a/");
    fs::create_dir(&b).expect("create b/");
    let src = write_file(&a, "src", b"payload bytes");
    let dst = b.join("dst");

    let out = Command::new(MV)
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn mv");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert!(!src.exists(), "src should be gone");
    let moved = fs::read(&dst).expect("read dst");
    assert_eq!(moved, b"payload bytes");
    cleanup(&dir);
}

#[test]
fn overwrites_existing_dst() {
    let dir = scratch_dir("overwrite");
    let src = write_file(&dir, "src", b"new content");
    let dst = write_file(&dir, "dst", b"old content");

    let out = Command::new(MV)
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn mv");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert!(!src.exists(), "src should be gone");
    let moved = fs::read(&dst).expect("read dst");
    assert_eq!(moved, b"new content");
    cleanup(&dir);
}

#[test]
fn fails_when_src_missing() {
    let dir = scratch_dir("no-src");
    let missing = dir.join("missing");
    let dst = dir.join("dst");

    let out = Command::new(MV)
        .arg(&missing)
        .arg(&dst)
        .output()
        .expect("spawn mv");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("mv:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr should mention src: {stderr:?}"
    );
    assert!(!dst.exists(), "dst should not have been created");
    cleanup(&dir);
}

#[test]
fn fails_when_dst_parent_missing() {
    let dir = scratch_dir("no-dst-parent");
    let src = write_file(&dir, "src", b"bytes");
    let dst = dir.join("missing").join("dst");

    let out = Command::new(MV)
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("spawn mv");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("mv:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(dst.to_str().unwrap()),
        "stderr should mention dst path: {stderr:?}"
    );
    assert!(src.exists(), "src should still exist after failed mv");
    assert!(!dst.exists(), "dst should not have been created");
    cleanup(&dir);
}

#[test]
fn wrong_arg_count_exits_one_with_usage() {
    let out_zero = Command::new(MV).output().expect("spawn mv");
    assert_eq!(
        out_zero.status.code(),
        Some(1),
        "exit status: {:?}",
        out_zero.status
    );
    assert!(out_zero.stdout.is_empty(), "stdout = {:?}", out_zero.stdout);
    let stderr_zero = String::from_utf8_lossy(&out_zero.stderr);
    assert!(stderr_zero.contains("usage:"), "stderr = {stderr_zero:?}");

    let out_one = Command::new(MV).arg("only").output().expect("spawn mv");
    assert_eq!(
        out_one.status.code(),
        Some(1),
        "exit status: {:?}",
        out_one.status
    );
    let stderr_one = String::from_utf8_lossy(&out_one.stderr);
    assert!(stderr_one.contains("usage:"), "stderr = {stderr_one:?}");

    let out_three = Command::new(MV)
        .arg("a")
        .arg("b")
        .arg("c")
        .output()
        .expect("spawn mv");
    assert_eq!(
        out_three.status.code(),
        Some(1),
        "exit status: {:?}",
        out_three.status
    );
    let stderr_three = String::from_utf8_lossy(&out_three.stderr);
    assert!(stderr_three.contains("usage:"), "stderr = {stderr_three:?}");
}
