//! Integration tests for the `ls` coreutil binary.
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

const LS: &str = env!("CARGO_BIN_EXE_ls");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "pmos-ls-{}-{}-{}",
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
fn lists_empty_directory_with_no_output() {
    let dir = scratch_dir("empty");

    let out = Command::new(LS).arg(&dir).output().expect("spawn ls");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn lists_populated_directory_sorted_alphabetically() {
    let dir = scratch_dir("populated");
    write_file(&dir, "charlie", b"c");
    write_file(&dir, "alpha", b"a");
    write_file(&dir, "bravo", b"b");

    let out = Command::new(LS).arg(&dir).output().expect("spawn ls");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert_eq!(out.stdout, b"alpha\nbravo\ncharlie\n");
    cleanup(&dir);
}

#[test]
fn lists_a_file_arg_by_name() {
    let dir = scratch_dir("file-arg");
    let path = write_file(&dir, "only.txt", b"x");

    let out = Command::new(LS).arg(&path).output().expect("spawn ls");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert_eq!(out.stdout, b"only.txt\n");
    cleanup(&dir);
}

#[test]
fn missing_path_exits_one_with_stderr_shape() {
    let dir = scratch_dir("missing");
    let missing = dir.join("does-not-exist");

    let out = Command::new(LS).arg(&missing).output().expect("spawn ls");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ls:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr = {stderr:?}"
    );
    cleanup(&dir);
}

#[test]
fn multi_path_args_emit_headers_and_blank_separator() {
    let dir = scratch_dir("multi");
    let a = dir.join("a");
    let b = dir.join("b");
    fs::create_dir(&a).expect("mkdir a");
    fs::create_dir(&b).expect("mkdir b");
    write_file(&a, "one", b"1");
    write_file(&b, "two", b"2");

    let out = Command::new(LS)
        .arg(&a)
        .arg(&b)
        .output()
        .expect("spawn ls");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = format!("{a_str}:\none\n\n{b_str}:\ntwo\n", a_str = a.display(), b_str = b.display());
    assert_eq!(stdout, expected, "stdout = {stdout:?}");
    cleanup(&dir);
}

#[test]
fn no_args_defaults_to_cwd() {
    let dir = scratch_dir("cwd");
    write_file(&dir, "marker", b"m");

    let out = Command::new(LS)
        .current_dir(&dir)
        .output()
        .expect("spawn ls");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    assert_eq!(out.stdout, b"marker\n");
    cleanup(&dir);
}

#[test]
fn continues_after_missing_path_but_exits_one() {
    let dir = scratch_dir("partial");
    let good = dir.join("good");
    fs::create_dir(&good).expect("mkdir good");
    write_file(&good, "kept", b"k");
    let missing = dir.join("nope");

    let out = Command::new(LS)
        .arg(&missing)
        .arg(&good)
        .output()
        .expect("spawn ls");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("kept\n"), "stdout = {stdout:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr = {stderr:?}"
    );
    cleanup(&dir);
}
