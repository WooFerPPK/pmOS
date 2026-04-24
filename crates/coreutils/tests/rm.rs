//! Integration tests for the `rm` coreutil binary.
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

const RM: &str = env!("CARGO_BIN_EXE_rm");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "pmos-rm-{}-{}-{}",
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
fn removes_a_file() {
    let dir = scratch_dir("single");
    let target = write_file(&dir, "foo", b"contents");

    let out = Command::new(RM).arg(&target).output().expect("spawn rm");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert!(!target.exists(), "{} should be gone", target.display());
    cleanup(&dir);
}

#[test]
fn removes_multiple_files() {
    let dir = scratch_dir("multi");
    let a = write_file(&dir, "a", b"A");
    let b = write_file(&dir, "b", b"B");
    let c = write_file(&dir, "c", b"C");

    let out = Command::new(RM)
        .arg(&a)
        .arg(&b)
        .arg(&c)
        .output()
        .expect("spawn rm");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert!(!a.exists(), "{} should be gone", a.display());
    assert!(!b.exists(), "{} should be gone", b.display());
    assert!(!c.exists(), "{} should be gone", c.display());
    cleanup(&dir);
}

#[test]
fn fails_on_missing_file() {
    let dir = scratch_dir("missing");
    let missing = dir.join("does-not-exist");

    let out = Command::new(RM).arg(&missing).output().expect("spawn rm");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("rm:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr = {stderr:?}"
    );
    cleanup(&dir);
}

#[test]
fn refuses_to_remove_directory() {
    let dir = scratch_dir("isdir");
    let target = dir.join("subdir");
    fs::create_dir(&target).expect("create target dir");

    let out = Command::new(RM).arg(&target).output().expect("spawn rm");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("rm:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(target.to_str().unwrap()),
        "stderr = {stderr:?}"
    );
    assert!(
        target.is_dir(),
        "{} should still exist as a dir",
        target.display()
    );
    cleanup(&dir);
}

#[test]
fn partial_success_multi_arg() {
    let dir = scratch_dir("partial");
    let ok = write_file(&dir, "ok", b"ok");
    let missing = dir.join("missing");
    let ok2 = write_file(&dir, "ok2", b"ok2");

    let out = Command::new(RM)
        .arg(&ok)
        .arg(&missing)
        .arg(&ok2)
        .output()
        .expect("spawn rm");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(!ok.exists(), "{} should be gone", ok.display());
    assert!(!ok2.exists(), "{} should be gone", ok2.display());

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr should mention the missing path: {stderr:?}"
    );
    assert!(
        !stderr.contains(ok.to_str().unwrap()),
        "stderr should not mention the ok path: {stderr:?}"
    );
    assert!(
        !stderr.contains(ok2.to_str().unwrap()),
        "stderr should not mention the ok2 path: {stderr:?}"
    );
    cleanup(&dir);
}

#[test]
fn zero_args_exits_one_with_usage_line() {
    let out = Command::new(RM).output().expect("spawn rm");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage:"), "stderr = {stderr:?}");
    assert!(stderr.contains("rm"), "stderr = {stderr:?}");
}
