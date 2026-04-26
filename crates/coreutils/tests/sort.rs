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

fn run_check(args: &[&str], stdin_bytes: &[u8]) -> std::process::Output {
    let mut child = Command::new(SORT)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sort");

    if !stdin_bytes.is_empty() {
        child
            .stdin
            .as_mut()
            .expect("stdin pipe")
            .write_all(stdin_bytes)
            .expect("write stdin");
    }
    drop(child.stdin.take());
    child.wait_with_output().expect("wait sort")
}

#[test]
fn dash_c_exits_zero_for_sorted_input() {
    let out = run_check(&["-c"], b"apple\nbanana\ncherry\n");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn dash_c_exits_one_for_unsorted_input() {
    let out = run_check(&["-c"], b"banana\napple\ncherry\n");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sort: -:2: disorder: apple"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn dash_c_emits_diagnostic_for_first_violation_only() {
    let out = run_check(&["-c"], b"a\nc\nb\nd\nf\ne\n");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let count = stderr.matches("disorder").count();
    assert_eq!(count, 1, "stderr = {stderr:?} (expected exactly one disorder line)");
    assert!(
        stderr.contains("sort: -:3: disorder: b"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn dash_cn_checks_numeric_order() {
    let sorted_numeric = run_check(&["-cn"], b"1\n2\n10\n");
    assert!(
        sorted_numeric.status.success(),
        "exit status: {:?}",
        sorted_numeric.status
    );
    assert!(sorted_numeric.stderr.is_empty());

    let lex_only = run_check(&["-c"], b"1\n2\n10\n");
    assert_eq!(lex_only.status.code(), Some(1), "lex check should fail on numerically-sorted but lex-unsorted input");
    let stderr = String::from_utf8_lossy(&lex_only.stderr);
    assert!(stderr.contains("disorder"), "stderr = {stderr:?}");
}

#[test]
fn dash_cf_checks_case_insensitive_order() {
    let sorted_fold = run_check(&["-cf"], b"Apple\nbanana\nCherry\n");
    assert!(
        sorted_fold.status.success(),
        "exit status: {:?}, stderr: {:?}",
        sorted_fold.status,
        String::from_utf8_lossy(&sorted_fold.stderr)
    );
    assert!(sorted_fold.stderr.is_empty());

    let lex_only = run_check(&["-c"], b"Apple\nbanana\nCherry\n");
    assert_eq!(lex_only.status.code(), Some(1), "lex check should fail since lowercase b > uppercase C");
}

#[test]
fn dash_cr_checks_reverse_order() {
    let sorted_rev = run_check(&["-cr"], b"cherry\nbanana\napple\n");
    assert!(
        sorted_rev.status.success(),
        "exit status: {:?}",
        sorted_rev.status
    );
    assert!(sorted_rev.stderr.is_empty());

    let ascending_under_reverse = run_check(&["-cr"], b"apple\nbanana\ncherry\n");
    assert_eq!(
        ascending_under_reverse.status.code(),
        Some(1),
        "reverse check should fail on ascending input"
    );
}

#[test]
fn dash_c_empty_input_exits_zero() {
    let out = run_check(&["-c"], b"");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn dash_c_single_line_exits_zero() {
    let out = run_check(&["-c"], b"only\n");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn dash_cu_treats_duplicate_as_violation() {
    let out = run_check(&["-cu"], b"apple\napple\n");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sort: -:2: disorder: apple"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn dash_b_sorts_ignoring_leading_blanks() {
    let dir = scratch_dir("b_basic");
    let path = write_file(&dir, "in.txt", b"   apple\nbanana\n  cherry\n");

    let out = Command::new(SORT)
        .arg("-b")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"   apple\nbanana\n  cherry\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_b_preserves_leading_blanks_in_output() {
    let dir = scratch_dir("b_preserve");
    let path = write_file(&dir, "in.txt", b"   apple\nbanana\n");

    let out = Command::new(SORT)
        .arg("-b")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"   apple\nbanana\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_b_treats_lines_with_only_blanks_as_empty_key() {
    let dir = scratch_dir("b_only_blanks");
    let path = write_file(&dir, "in.txt", b"   \n   apple\n");

    let out = Command::new(SORT)
        .arg("-b")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"   \n   apple\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_bu_dedupes_after_trim() {
    let dir = scratch_dir("bu");
    let path = write_file(&dir, "in.txt", b"   apple\napple\n   apple\n");

    let out = Command::new(SORT)
        .arg("-bu")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"   apple\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_bf_combines_trim_then_fold() {
    let dir = scratch_dir("bf");
    let path = write_file(&dir, "in.txt", b"   apple\nApple\n   APPLE\n");

    let out = Command::new(SORT)
        .arg("-bf")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"   apple\nApple\n   APPLE\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_bn_no_op_for_numeric() {
    let dir = scratch_dir("bn");
    let path = write_file(&dir, "in.txt", b"   10\n   2\n   1\n");

    let out = Command::new(SORT)
        .arg("-bn")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"   1\n   2\n   10\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_b_does_not_trim_trailing_whitespace() {
    let dir = scratch_dir("b_trailing");
    let path = write_file(&dir, "in.txt", b"apple\napple   \n");

    let out = Command::new(SORT)
        .arg("-b")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"apple\napple   \n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_cb_checks_with_trimmed_keys() {
    let out = run_check(&["-cb"], b"   apple\nbanana\n  cherry\n");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);

    let plain_check = run_check(&["-c"], b"   apple\nbanana\n  cherry\n");
    assert_eq!(
        plain_check.status.code(),
        Some(1),
        "plain check should fail because banana > '  cherry' under raw lex (space is 32 < b)"
    );
    let plain_stderr = String::from_utf8_lossy(&plain_check.stderr);
    assert!(
        plain_stderr.contains("disorder"),
        "stderr = {plain_stderr:?}"
    );
}

#[test]
fn dash_i_filters_non_printable_bytes_for_sort() {
    let dir = scratch_dir("i_basic");
    let path = write_file(&dir, "in.txt", b"\x07banana\napple\x08\n");

    let out = Command::new(SORT)
        .arg("-i")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"apple\x08\n\x07banana\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_i_preserves_original_bytes_in_output() {
    let dir = scratch_dir("i_preserve");
    let path = write_file(&dir, "in.txt", b"a\x01b\x02c\nzebra\n");

    let out = Command::new(SORT)
        .arg("-i")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"a\x01b\x02c\nzebra\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_if_combines_filter_then_fold() {
    let dir = scratch_dir("if");
    let path = write_file(&dir, "in.txt", b"Apple\x07\napple\x08\nbanana\n");

    let out = Command::new(SORT)
        .arg("-if")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"Apple\x07\napple\x08\nbanana\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_iu_dedupes_after_filter() {
    let dir = scratch_dir("iu");
    let path = write_file(&dir, "in.txt", b"apple\x07\napple\x08\napple\n");

    let out = Command::new(SORT)
        .arg("-iu")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"apple\x07\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_ib_combines_filter_and_trim() {
    let dir = scratch_dir("ib");
    let path = write_file(&dir, "in.txt", b"   apple\n\x07apple\n");

    let out = Command::new(SORT)
        .arg("-ibu")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"   apple\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_in_no_op_for_numeric() {
    let dir = scratch_dir("in_numeric");
    let path = write_file(&dir, "in.txt", b"   42\nfoo\x07\n2\n");

    let with_i = Command::new(SORT)
        .arg("-in")
        .arg(&path)
        .output()
        .expect("spawn sort");
    let without_i = Command::new(SORT)
        .arg("-n")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(with_i.status.success(), "exit status: {:?}", with_i.status);
    assert!(
        without_i.status.success(),
        "exit status: {:?}",
        without_i.status
    );
    assert_eq!(with_i.stdout, without_i.stdout);
    cleanup(&dir);
}

#[test]
fn dash_ic_checks_with_filtered_keys() {
    let out = run_check(&["-ic"], b"apple\x08\n\x07banana\ncherry\n");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);

    let plain_check = run_check(&["-c"], b"apple\x08\n\x07banana\ncherry\n");
    assert_eq!(
        plain_check.status.code(),
        Some(1),
        "plain check should fail: apple\\x08 (a=97) > \\x07banana (\\x07=7) under raw lex"
    );
    let plain_stderr = String::from_utf8_lossy(&plain_check.stderr);
    assert!(
        plain_stderr.contains("disorder"),
        "stderr = {plain_stderr:?}"
    );
}

#[test]
fn dash_i_alone_with_no_input_is_empty_output() {
    let out = run_check(&["-i"], b"");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn dash_i_handles_high_bit_bytes_as_nonprinting() {
    let dir = scratch_dir("i_highbit");
    let path = write_file(
        &dir,
        "in.txt",
        "\u{00e9}apple\nbanana\n".as_bytes(),
    );

    let with_i = Command::new(SORT)
        .arg("-i")
        .arg(&path)
        .output()
        .expect("spawn sort");
    assert!(with_i.status.success(), "exit status: {:?}", with_i.status);
    assert_eq!(with_i.stdout, "\u{00e9}apple\nbanana\n".as_bytes());
    assert!(with_i.stderr.is_empty(), "stderr = {:?}", with_i.stderr);

    let without_i = Command::new(SORT)
        .arg(&path)
        .output()
        .expect("spawn sort");
    assert!(
        without_i.status.success(),
        "exit status: {:?}",
        without_i.status
    );
    assert_eq!(without_i.stdout, "banana\n\u{00e9}apple\n".as_bytes());
    cleanup(&dir);
}

#[test]
fn dash_d_filters_punctuation_for_sort() {
    let dir = scratch_dir("d_basic");
    let path = write_file(&dir, "in.txt", b"co-op\ncoop\nca!t\n");

    let out = Command::new(SORT)
        .arg("-d")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"ca!t\nco-op\ncoop\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_d_preserves_punctuation_in_output() {
    let dir = scratch_dir("d_preserve");
    let path = write_file(&dir, "in.txt", b"co-op\nca-t\n");

    let out = Command::new(SORT)
        .arg("-d")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"ca-t\nco-op\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_d_collapses_punctuation_in_dedup() {
    let dir = scratch_dir("du");
    let path = write_file(&dir, "in.txt", b"co-op\ncoop\n");

    let out = Command::new(SORT)
        .arg("-du")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"co-op\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_di_d_dominates_i() {
    let dir = scratch_dir("di_dominance");
    let path = write_file(&dir, "in.txt", b"co-op\ncoop\nca!t\n");

    let with_di = Command::new(SORT)
        .arg("-di")
        .arg(&path)
        .output()
        .expect("spawn sort");
    let with_d = Command::new(SORT)
        .arg("-d")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(with_di.status.success(), "exit status: {:?}", with_di.status);
    assert!(with_d.status.success(), "exit status: {:?}", with_d.status);
    assert_eq!(with_di.stdout, with_d.stdout);
    cleanup(&dir);
}

#[test]
fn dash_df_combines_filter_then_fold() {
    let dir = scratch_dir("df");
    let path = write_file(&dir, "in.txt", b"Co-Op\nco-op\nCOOP\n");

    let out = Command::new(SORT)
        .arg("-df")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"Co-Op\nco-op\nCOOP\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_db_combines_filter_and_trim() {
    let dir = scratch_dir("db");
    let path = write_file(&dir, "in.txt", b"   co-op\nca-t\n");

    let out = Command::new(SORT)
        .arg("-db")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"ca-t\n   co-op\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_dn_no_op_for_numeric() {
    let dir = scratch_dir("dn");
    let path = write_file(&dir, "in.txt", b"   42\nfoo-bar\n2\n");

    let with_d = Command::new(SORT)
        .arg("-dn")
        .arg(&path)
        .output()
        .expect("spawn sort");
    let without_d = Command::new(SORT)
        .arg("-n")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(with_d.status.success(), "exit status: {:?}", with_d.status);
    assert!(
        without_d.status.success(),
        "exit status: {:?}",
        without_d.status
    );
    assert_eq!(with_d.stdout, without_d.stdout);
    cleanup(&dir);
}

#[test]
fn dash_dc_checks_with_dictionary_keys() {
    let out = run_check(&["-dc"], b"apple-1\napple!2\n");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);

    let plain_check = run_check(&["-c"], b"apple-1\napple!2\n");
    assert_eq!(
        plain_check.status.code(),
        Some(1),
        "plain check should fail: apple-1 (-=45) > apple!2 (!=33) under raw lex"
    );
    let plain_stderr = String::from_utf8_lossy(&plain_check.stderr);
    assert!(
        plain_stderr.contains("disorder"),
        "stderr = {plain_stderr:?}"
    );
}

#[test]
fn dash_d_alone_with_no_input_is_empty_output() {
    let out = run_check(&["-d"], b"");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}
