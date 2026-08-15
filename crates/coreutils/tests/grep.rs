//! Integration tests for the `grep` coreutil binary.
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

const GREP: &str = env!("CARGO_BIN_EXE_grep");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("pmos-grep-{}-{}-{}", tag, std::process::id(), n));
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
fn matches_single_line_in_single_file() {
    let dir = scratch_dir("single");
    let path = write_file(&dir, "a.txt", b"alpha\nhello world\nbeta\n");

    let out = Command::new(GREP)
        .arg("hello")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"hello world\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn no_match_exits_one() {
    let dir = scratch_dir("nomatch");
    let path = write_file(&dir, "a.txt", b"alpha\nbeta\ngamma\n");

    let out = Command::new(GREP)
        .arg("zzz")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn stdin_mode_when_no_files() {
    let mut child = Command::new(GREP)
        .arg("alpha")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grep");

    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"alpha\nbeta\n")
        .expect("write stdin");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait grep");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"alpha\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn multiple_files_prefix_filename() {
    let dir = scratch_dir("multi");
    let a = write_file(&dir, "a.txt", b"apple\nfruit-line\n");
    let b = write_file(&dir, "b.txt", b"banana\nfruit-line-two\n");

    let out = Command::new(GREP)
        .arg("fruit")
        .arg(&a)
        .arg(&b)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected_a = format!("{}:fruit-line\n", a.display());
    let expected_b = format!("{}:fruit-line-two\n", b.display());
    assert_eq!(stdout, format!("{expected_a}{expected_b}"));
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn missing_file_continues_and_exits_two() {
    let dir = scratch_dir("missing");
    let good = write_file(&dir, "good.txt", b"nothing\nmatching-line\n");
    let missing = dir.join("nope.txt");

    let out = Command::new(GREP)
        .arg("matching")
        .arg(&good)
        .arg(&missing)
        .output()
        .expect("spawn grep");

    assert_eq!(out.status.code(), Some(2), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("matching-line"), "stdout = {stdout:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("grep:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr = {stderr:?}"
    );
    cleanup(&dir);
}

#[test]
fn missing_pattern_arg_exits_two() {
    let out = Command::new(GREP).output().expect("spawn grep");

    assert_eq!(out.status.code(), Some(2), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage"), "stderr = {stderr:?}");
}

#[test]
fn dash_i_matches_uppercase_pattern_against_lowercase_line() {
    let dir = scratch_dir("ci-up");
    let path = write_file(&dir, "a.txt", b"alpha\nhello world\nbeta\n");

    let out = Command::new(GREP)
        .arg("-i")
        .arg("HELLO")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"hello world\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_i_matches_lowercase_pattern_against_uppercase_line() {
    let dir = scratch_dir("ci-down");
    let path = write_file(&dir, "a.txt", b"FOO BAR\nbaz\n");

    let out = Command::new(GREP)
        .arg("-i")
        .arg("foo")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"FOO BAR\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_i_preserves_match_count_on_mixed_case() {
    let dir = scratch_dir("ci-mixed");
    let path = write_file(&dir, "a.txt", b"Apple\napple pie\nBANANA\nApricot\n");

    let out = Command::new(GREP)
        .arg("-i")
        .arg("apple")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"Apple\napple pie\n");
    cleanup(&dir);
}

#[test]
fn without_dash_i_case_sensitive_still_holds() {
    let dir = scratch_dir("cs");
    let path = write_file(&dir, "a.txt", b"FOO\nfoo\n");

    let out = Command::new(GREP)
        .arg("foo")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"foo\n");
    cleanup(&dir);
}

#[test]
fn unknown_flag_exits_two_for_grep() {
    let dir = scratch_dir("badflag");
    let path = write_file(&dir, "a.txt", b"x\n");

    let out = Command::new(GREP)
        .arg("-x")
        .arg("foo")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert_eq!(out.status.code(), Some(2), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown flag"), "stderr = {stderr:?}");
    cleanup(&dir);
}

#[test]
fn dash_n_emits_line_numbers_for_single_file() {
    let dir = scratch_dir("ln-single");
    let path = write_file(&dir, "a.txt", b"first-line\nsecond-line\nthird-line\n");

    let out = Command::new(GREP)
        .arg("-n")
        .arg("second")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"2:second-line\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_with_multiple_files_includes_path_and_number() {
    let dir = scratch_dir("ln-multi");
    let a = write_file(&dir, "a.txt", b"apple\nfruit-line\n");
    let b = write_file(&dir, "b.txt", b"banana\nfruit-line-two\n");

    let out = Command::new(GREP)
        .arg("-n")
        .arg("fruit")
        .arg(&a)
        .arg(&b)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected_a = format!("{}:2:fruit-line\n", a.display());
    let expected_b = format!("{}:2:fruit-line-two\n", b.display());
    assert_eq!(stdout, format!("{expected_a}{expected_b}"));
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_combined_with_dash_i_works() {
    let dir = scratch_dir("ln-ci");
    let path = write_file(&dir, "a.txt", b"alpha\nhello world\nbeta\n");

    let out = Command::new(GREP)
        .arg("-i")
        .arg("-n")
        .arg("HELLO")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"2:hello world\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_n_in_stdin_mode_counts_from_one() {
    let mut child = Command::new(GREP)
        .arg("-n")
        .arg("alpha")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grep");

    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"alpha\nbeta\n")
        .expect("write stdin");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait grep");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"1:alpha\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn dash_v_emits_non_matching_lines() {
    let dir = scratch_dir("inv-basic");
    let path = write_file(&dir, "a.txt", b"foo\nbar\nfoo\n");

    let out = Command::new(GREP)
        .arg("-v")
        .arg("foo")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"bar\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_v_no_match_means_all_lines_pass() {
    let dir = scratch_dir("inv-allpass");
    let path = write_file(&dir, "a.txt", b"aaa\nbbb\n");

    let out = Command::new(GREP)
        .arg("-v")
        .arg("zzz")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"aaa\nbbb\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_v_with_dash_i_inverts_case_insensitive() {
    let dir = scratch_dir("inv-ci");
    let path = write_file(&dir, "a.txt", b"FOO\nbar\nfoo\nbaz\nFoo\n");

    let out = Command::new(GREP)
        .arg("-iv")
        .arg("foo")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"bar\nbaz\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_v_combines_with_dash_n() {
    let dir = scratch_dir("inv-ln");
    let path = write_file(&dir, "a.txt", b"foo\nbar\nfoo\n");

    let out = Command::new(GREP)
        .arg("-nv")
        .arg("foo")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"2:bar\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_v_no_lines_pass_exits_one() {
    let dir = scratch_dir("inv-empty");
    let path = write_file(&dir, "a.txt", b"always\nalways\nalways\n");

    let out = Command::new(GREP)
        .arg("-v")
        .arg("always")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_c_emits_match_count_for_single_file() {
    let dir = scratch_dir("count-single");
    let path = write_file(&dir, "a.txt", b"foo\nbar\nfoo\n");

    let out = Command::new(GREP)
        .arg("-c")
        .arg("foo")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"2\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_c_emits_zero_for_no_matches() {
    let dir = scratch_dir("count-zero");
    let path = write_file(&dir, "a.txt", b"alpha\nbeta\ngamma\n");

    let out = Command::new(GREP)
        .arg("-c")
        .arg("xxx")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"0\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_c_with_multi_file_prefixes_path() {
    let dir = scratch_dir("count-multi");
    let a = write_file(&dir, "a.txt", b"foo\nbar\n");
    let b = write_file(&dir, "b.txt", b"foo\nfoo\nbaz\n");

    let out = Command::new(GREP)
        .arg("-c")
        .arg("foo")
        .arg(&a)
        .arg(&b)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected_a = format!("{}:1\n", a.display());
    let expected_b = format!("{}:2\n", b.display());
    assert_eq!(stdout, format!("{expected_a}{expected_b}"));
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_c_combines_with_dash_v_for_non_matching_count() {
    let dir = scratch_dir("count-invert");
    let path = write_file(&dir, "a.txt", b"foo\nbar\nfoo\nbaz\n");

    let out = Command::new(GREP)
        .arg("-cv")
        .arg("foo")
        .arg(&path)
        .output()
        .expect("spawn grep");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"2\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_c_in_stdin_mode_emits_count() {
    let mut child = Command::new(GREP)
        .arg("-c")
        .arg("foo")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn grep");

    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"foo\nbar\nfoo\n")
        .expect("write stdin");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait grep");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"2\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}
