//! Integration tests for the `mkdir` coreutil binary.
//!
//! Driven through `std::process::Command` so the tests see the exact
//! bytes the userland binary emits. Temp dirs are placed under
//! `std::env::temp_dir()` with a per-test directory keyed by the
//! test name and process id; each test cleans up its directory on
//! success (a failing test leaves its scratch tree intact for
//! debugging).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const MKDIR: &str = env!("CARGO_BIN_EXE_mkdir");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("pmos-mkdir-{}-{}-{}", tag, std::process::id(), n));
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn cleanup(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn creates_a_directory() {
    let dir = scratch_dir("single");
    let target = dir.join("foo");

    let out = Command::new(MKDIR)
        .arg(&target)
        .output()
        .expect("spawn mkdir");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert!(
        target.is_dir(),
        "{} should exist and be a dir",
        target.display()
    );
    cleanup(&dir);
}

#[test]
fn creates_multiple_directories() {
    let dir = scratch_dir("multi");
    let a = dir.join("a");
    let b = dir.join("b");
    let c = dir.join("c");

    let out = Command::new(MKDIR)
        .arg(&a)
        .arg(&b)
        .arg(&c)
        .output()
        .expect("spawn mkdir");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert!(a.is_dir(), "{} should be a dir", a.display());
    assert!(b.is_dir(), "{} should be a dir", b.display());
    assert!(c.is_dir(), "{} should be a dir", c.display());
    cleanup(&dir);
}

#[test]
fn fails_when_parent_doesnt_exist() {
    let dir = scratch_dir("no-parent");
    let target = dir.join("missing").join("child");

    let out = Command::new(MKDIR)
        .arg(&target)
        .output()
        .expect("spawn mkdir");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("mkdir:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(target.to_str().unwrap()),
        "stderr = {stderr:?}"
    );
    assert!(!target.exists(), "target should not have been created");
    cleanup(&dir);
}

#[test]
fn fails_when_target_already_exists() {
    let dir = scratch_dir("exists");
    let target = dir.join("existing");
    fs::create_dir(&target).expect("pre-create target");

    let out = Command::new(MKDIR)
        .arg(&target)
        .output()
        .expect("spawn mkdir");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("mkdir:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(target.to_str().unwrap()),
        "stderr = {stderr:?}"
    );
    cleanup(&dir);
}

#[test]
fn partial_success_multi_arg() {
    let dir = scratch_dir("partial");
    let ok = dir.join("ok");
    let bad = dir.join("missing_parent").join("child");
    let ok2 = dir.join("ok2");

    let out = Command::new(MKDIR)
        .arg(&ok)
        .arg(&bad)
        .arg(&ok2)
        .output()
        .expect("spawn mkdir");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(ok.is_dir(), "{} should exist", ok.display());
    assert!(ok2.is_dir(), "{} should exist", ok2.display());
    assert!(!bad.exists(), "{} should not exist", bad.display());

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(bad.to_str().unwrap()),
        "stderr should mention the failing path: {stderr:?}"
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
    let out = Command::new(MKDIR).output().expect("spawn mkdir");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage:"), "stderr = {stderr:?}");
    assert!(stderr.contains("mkdir"), "stderr = {stderr:?}");
}

#[test]
fn dash_p_creates_intermediate_parents() {
    let dir = scratch_dir("dash-p-parents");
    let target = dir.join("a").join("b").join("c");

    let out = Command::new(MKDIR)
        .arg("-p")
        .arg(&target)
        .output()
        .expect("spawn mkdir");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert!(dir.join("a").is_dir(), "a should exist");
    assert!(dir.join("a").join("b").is_dir(), "a/b should exist");
    assert!(target.is_dir(), "{} should exist", target.display());
    cleanup(&dir);
}

#[test]
fn dash_p_is_idempotent_on_existing_dir() {
    let dir = scratch_dir("dash-p-idempotent");
    let target = dir.join("already").join("there");
    fs::create_dir_all(&target).expect("pre-create target");

    let out = Command::new(MKDIR)
        .arg("-p")
        .arg(&target)
        .output()
        .expect("spawn mkdir");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert!(target.is_dir(), "{} should still exist", target.display());
    cleanup(&dir);
}

#[test]
fn dash_p_with_multiple_paths() {
    let dir = scratch_dir("dash-p-multi");
    let first = dir.join("a").join("b");
    let second = dir.join("c").join("d");

    let out = Command::new(MKDIR)
        .arg("-p")
        .arg(&first)
        .arg(&second)
        .output()
        .expect("spawn mkdir");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert!(dir.join("a").is_dir(), "a should exist");
    assert!(first.is_dir(), "{} should exist", first.display());
    assert!(dir.join("c").is_dir(), "c should exist");
    assert!(second.is_dir(), "{} should exist", second.display());
    cleanup(&dir);
}

#[test]
fn unknown_flag_exits_two() {
    let out = Command::new(MKDIR)
        .arg("-x")
        .arg("foo")
        .output()
        .expect("spawn mkdir");

    assert_eq!(out.status.code(), Some(2), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown flag"), "stderr = {stderr:?}");
    assert!(stderr.contains("-x"), "stderr = {stderr:?}");
}
