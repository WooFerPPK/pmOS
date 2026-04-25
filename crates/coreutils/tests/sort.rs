//! Integration tests for the `sort` coreutil binary.
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

const SORT: &str = env!("CARGO_BIN_EXE_sort");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "pmos-sort-{}-{}-{}",
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
fn sorts_lines_alphabetically() {
    let dir = scratch_dir("alpha");
    let path = write_file(&dir, "in.txt", b"c\na\nb\n");

    let out = Command::new(SORT).arg(&path).output().expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"a\nb\nc\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_r_reverses_order() {
    let dir = scratch_dir("rev");
    let path = write_file(&dir, "in.txt", b"c\na\nb\n");

    let out = Command::new(SORT)
        .arg("-r")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"c\nb\na\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_u_dedupes_duplicate_lines() {
    let dir = scratch_dir("uniq");
    let path = write_file(&dir, "in.txt", b"a\nb\na\nb\n");

    let out = Command::new(SORT)
        .arg("-u")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"a\nb\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_ru_combines_reverse_and_unique() {
    let dir = scratch_dir("ru");
    let path = write_file(&dir, "in.txt", b"a\nb\na\nb\n");

    let out = Command::new(SORT)
        .arg("-ru")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"b\na\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn multi_file_concatenates_then_sorts() {
    let dir = scratch_dir("multi");
    let a = write_file(&dir, "a.txt", b"delta\nbravo\n");
    let b = write_file(&dir, "b.txt", b"alpha\ncharlie\n");

    let out = Command::new(SORT)
        .arg(&a)
        .arg(&b)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"alpha\nbravo\ncharlie\ndelta\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn stdin_mode_reads_until_eof_and_sorts() {
    let mut child = Command::new(SORT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sort");

    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"zebra\napple\nmango\n")
        .expect("write stdin");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait sort");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"apple\nmango\nzebra\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn missing_file_continues_and_exits_one() {
    let dir = scratch_dir("missing");
    let good = write_file(&dir, "good.txt", b"banana\napple\n");
    let missing = dir.join("nope.txt");

    let out = Command::new(SORT)
        .arg(&missing)
        .arg(&good)
        .output()
        .expect("spawn sort");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"apple\nbanana\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("sort:"), "stderr = {stderr:?}");
    assert!(stderr.contains("nope.txt"), "stderr = {stderr:?}");
    cleanup(&dir);
}

#[test]
fn dash_n_sorts_numerically() {
    let dir = scratch_dir("n_basic");
    let path = write_file(&dir, "in.txt", b"10\n2\n1\n");

    let out = Command::new(SORT)
        .arg("-n")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"1\n2\n10\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_handles_negative_numbers() {
    let dir = scratch_dir("n_neg");
    let path = write_file(&dir, "in.txt", b"-5\n3\n-10\n0\n");

    let out = Command::new(SORT)
        .arg("-n")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"-10\n-5\n0\n3\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_with_dash_r_descending_numeric() {
    let dir = scratch_dir("nr");
    let path = write_file(&dir, "in.txt", b"1\n10\n2\n");

    let out = Command::new(SORT)
        .arg("-nr")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"10\n2\n1\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_with_dash_u_dedupes_after_numeric_sort() {
    let dir = scratch_dir("nu");
    let path = write_file(&dir, "in.txt", b"5\n5\n5\n3\n");

    let out = Command::new(SORT)
        .arg("-nu")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"3\n5\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_nru_combines_all_three() {
    let dir = scratch_dir("nru");
    let path = write_file(&dir, "in.txt", b"5\n5\n5\n3\n10\n");

    let out = Command::new(SORT)
        .arg("-nru")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"10\n5\n3\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_treats_non_numeric_as_zero() {
    let dir = scratch_dir("n_nonnum");
    let path = write_file(&dir, "in.txt", b"abc\n1\n");

    let out = Command::new(SORT)
        .arg("-n")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout.clone()).expect("utf8 stdout");
    assert!(
        stdout == "abc\n1\n" || stdout == "1\nabc\n",
        "stdout = {stdout:?} (expected abc and 1 in either order)"
    );
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_handles_lines_with_trailing_text() {
    let dir = scratch_dir("n_trailing");
    let path = write_file(&dir, "in.txt", b"100x\n50y\n");

    let out = Command::new(SORT)
        .arg("-n")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"50y\n100x\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_f_sorts_case_insensitively() {
    let dir = scratch_dir("f_basic");
    let path = write_file(&dir, "in.txt", b"Banana\napple\nCherry\n");

    let out = Command::new(SORT)
        .arg("-f")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"apple\nBanana\nCherry\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_f_preserves_original_case_in_output() {
    let dir = scratch_dir("f_preserve");
    let path = write_file(&dir, "in.txt", b"Apple\nbanana\n");

    let out = Command::new(SORT)
        .arg("-f")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"Apple\nbanana\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_fu_dedupes_case_insensitive_duplicates() {
    let dir = scratch_dir("fu");
    let path = write_file(&dir, "in.txt", b"Apple\napple\nAPPLE\nbanana\n");

    let out = Command::new(SORT)
        .arg("-fu")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"Apple\nbanana\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_fr_reverses_case_insensitive_sort() {
    let dir = scratch_dir("fr");
    let path = write_file(&dir, "in.txt", b"apple\nBanana\nCherry\n");

    let out = Command::new(SORT)
        .arg("-fr")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"Cherry\nBanana\napple\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_fn_combines_but_n_dominates() {
    let dir = scratch_dir("fn");
    let path = write_file(&dir, "in.txt", b"2\n10\n");

    let out = Command::new(SORT)
        .arg("-fn")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"2\n10\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_f_handles_non_ascii_bytes_passthrough() {
    let dir = scratch_dir("f_nonascii");
    let path = write_file(&dir, "in.txt", b"\xc3\xa9\nApple\nbanana\n");

    let out = Command::new(SORT)
        .arg("-f")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"Apple\nbanana\n\xc3\xa9\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_f_alone_with_no_input_is_empty_output() {
    let mut child = Command::new(SORT)
        .arg("-f")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sort");

    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait sort");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}
