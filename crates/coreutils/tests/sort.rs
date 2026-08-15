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
    let dir = env::temp_dir().join(format!("pmos-sort-{}-{}-{}", tag, std::process::id(), n));
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
    assert_eq!(
        count, 1,
        "stderr = {stderr:?} (expected exactly one disorder line)"
    );
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
    assert_eq!(
        lex_only.status.code(),
        Some(1),
        "lex check should fail on numerically-sorted but lex-unsorted input"
    );
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
    assert_eq!(
        lex_only.status.code(),
        Some(1),
        "lex check should fail since lowercase b > uppercase C"
    );
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
    let path = write_file(&dir, "in.txt", "\u{00e9}apple\nbanana\n".as_bytes());

    let with_i = Command::new(SORT)
        .arg("-i")
        .arg(&path)
        .output()
        .expect("spawn sort");
    assert!(with_i.status.success(), "exit status: {:?}", with_i.status);
    assert_eq!(with_i.stdout, "\u{00e9}apple\nbanana\n".as_bytes());
    assert!(with_i.stderr.is_empty(), "stderr = {:?}", with_i.stderr);

    let without_i = Command::new(SORT).arg(&path).output().expect("spawn sort");
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

    assert!(
        with_di.status.success(),
        "exit status: {:?}",
        with_di.status
    );
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

#[test]
fn dash_capital_c_exits_zero_for_sorted_input() {
    let out = run_check(&["-C"], b"apple\nbanana\ncherry\n");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn dash_capital_c_exits_one_for_unsorted_input_silently() {
    let out = run_check(&["-C"], b"banana\napple\ncherry\n");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(
        out.stderr.is_empty(),
        "stderr should be empty under -C even on disorder, got {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn dash_capital_c_combines_with_n_for_numeric_silent_check() {
    let sorted_numeric = run_check(&["-Cn"], b"1\n2\n10\n");
    assert!(
        sorted_numeric.status.success(),
        "exit status: {:?}",
        sorted_numeric.status
    );
    assert!(sorted_numeric.stdout.is_empty());
    assert!(sorted_numeric.stderr.is_empty());

    let unsorted_numeric = run_check(&["-Cn"], b"10\n2\n1\n");
    assert_eq!(
        unsorted_numeric.status.code(),
        Some(1),
        "numeric check should fail on numerically-descending input"
    );
    assert!(unsorted_numeric.stdout.is_empty());
    assert!(
        unsorted_numeric.stderr.is_empty(),
        "stderr should be silent under -Cn, got {:?}",
        String::from_utf8_lossy(&unsorted_numeric.stderr)
    );
}

#[test]
fn dash_capital_c_combines_with_f_for_fold_silent_check() {
    let sorted_fold = run_check(&["-Cf"], b"Apple\nbanana\nCherry\n");
    assert!(
        sorted_fold.status.success(),
        "exit status: {:?}, stderr: {:?}",
        sorted_fold.status,
        String::from_utf8_lossy(&sorted_fold.stderr)
    );
    assert!(sorted_fold.stderr.is_empty());

    let unsorted_fold = run_check(&["-Cf"], b"banana\nApple\nCherry\n");
    assert_eq!(
        unsorted_fold.status.code(),
        Some(1),
        "fold check should fail since BANANA > APPLE"
    );
    assert!(
        unsorted_fold.stderr.is_empty(),
        "stderr should be silent under -Cf, got {:?}",
        String::from_utf8_lossy(&unsorted_fold.stderr)
    );
}

#[test]
fn dash_capital_c_combines_with_r_for_reverse_silent_check() {
    let sorted_rev = run_check(&["-Cr"], b"cherry\nbanana\napple\n");
    assert!(
        sorted_rev.status.success(),
        "exit status: {:?}",
        sorted_rev.status
    );
    assert!(sorted_rev.stderr.is_empty());

    let ascending_under_reverse = run_check(&["-Cr"], b"apple\nbanana\ncherry\n");
    assert_eq!(
        ascending_under_reverse.status.code(),
        Some(1),
        "reverse check should fail on ascending input"
    );
    assert!(
        ascending_under_reverse.stderr.is_empty(),
        "stderr should be silent under -Cr, got {:?}",
        String::from_utf8_lossy(&ascending_under_reverse.stderr)
    );
}

#[test]
fn dash_capital_c_with_lowercase_c_silent_dominates() {
    let cluster_lower_first = run_check(&["-cC"], b"banana\napple\n");
    assert_eq!(
        cluster_lower_first.status.code(),
        Some(1),
        "exit status: {:?}",
        cluster_lower_first.status
    );
    assert!(cluster_lower_first.stdout.is_empty());
    assert!(
        cluster_lower_first.stderr.is_empty(),
        "stderr should be silent when -cC is passed (silent dominates), got {:?}",
        String::from_utf8_lossy(&cluster_lower_first.stderr)
    );

    let cluster_capital_first = run_check(&["-Cc"], b"banana\napple\n");
    assert_eq!(
        cluster_capital_first.status.code(),
        Some(1),
        "exit status: {:?}",
        cluster_capital_first.status
    );
    assert!(cluster_capital_first.stdout.is_empty());
    assert!(
        cluster_capital_first.stderr.is_empty(),
        "stderr should be silent when -Cc is passed (silent dominates), got {:?}",
        String::from_utf8_lossy(&cluster_capital_first.stderr)
    );
}

#[test]
fn dash_capital_c_empty_input_exits_zero() {
    let out = run_check(&["-C"], b"");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn dash_capital_c_used_in_script_friendly_conditional() {
    let unsorted = run_check(&["-C"], b"banana\napple\ncherry\n");
    assert!(
        !unsorted.status.success(),
        "non-zero exit needed for `if !sort -C` script branch, got {:?}",
        unsorted.status
    );
    assert_eq!(unsorted.status.code(), Some(1));
    assert!(
        unsorted.stdout.is_empty(),
        "stdout must stay empty so callers can pipe to other tools, got {:?}",
        unsorted.stdout
    );
    assert!(
        unsorted.stderr.is_empty(),
        "stderr must stay empty so script callers can use `if sort -C f; then ...` without polluting their console, got {:?}",
        String::from_utf8_lossy(&unsorted.stderr)
    );

    let sorted = run_check(&["-C"], b"apple\nbanana\ncherry\n");
    assert!(
        sorted.status.success(),
        "zero exit needed for `if sort -C` true branch, got {:?}",
        sorted.status
    );
    assert!(sorted.stdout.is_empty());
    assert!(sorted.stderr.is_empty());
}

#[test]
fn dash_o_writes_sorted_output_to_file() {
    let dir = scratch_dir("o_basic");
    let out_path = dir.join("sorted.txt");

    let mut child = Command::new(SORT)
        .arg("-o")
        .arg(&out_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sort");
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"banana\napple\ncherry\n")
        .expect("write stdin");
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("wait sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty when -o redirects, got {:?}",
        out.stdout
    );
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let written = fs::read(&out_path).expect("read output file");
    assert_eq!(written, b"apple\nbanana\ncherry\n");
    cleanup(&dir);
}

#[test]
fn dash_o_no_space_form_works() {
    let dir = scratch_dir("o_nospace");
    let out_path = dir.join("sorted.txt");
    let arg = format!("-o{}", out_path.display());

    let mut child = Command::new(SORT)
        .arg(&arg)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sort");
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"banana\napple\n")
        .expect("write stdin");
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("wait sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let written = fs::read(&out_path).expect("read output file");
    assert_eq!(written, b"apple\nbanana\n");
    cleanup(&dir);
}

#[test]
fn dash_o_truncates_existing_file() {
    let dir = scratch_dir("o_truncate");
    let out_path = write_file(&dir, "out.txt", b"PRE-EXISTING JUNK\nMORE JUNK\n");
    let in_path = write_file(&dir, "in.txt", b"c\na\nb\n");

    let out = Command::new(SORT)
        .arg("-o")
        .arg(&out_path)
        .arg(&in_path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let written = fs::read(&out_path).expect("read output file");
    assert_eq!(
        written, b"a\nb\nc\n",
        "existing junk must be truncated, NOT appended"
    );
    cleanup(&dir);
}

#[test]
fn dash_o_in_place_sort_works() {
    let dir = scratch_dir("o_inplace");
    let path = write_file(&dir, "data.txt", b"cherry\nbanana\napple\n");

    let out = Command::new(SORT)
        .arg("-o")
        .arg(&path)
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let written = fs::read(&path).expect("read output file");
    assert_eq!(
        written, b"apple\nbanana\ncherry\n",
        "input must be fully read BEFORE the output file is opened so in-place sort works"
    );
    cleanup(&dir);
}

#[test]
fn dash_o_combines_with_n_for_numeric_output_to_file() {
    let dir = scratch_dir("o_n");
    let in_path = write_file(&dir, "in.txt", b"10\n2\n1\n");
    let out_path = dir.join("out.txt");

    let out = Command::new(SORT)
        .arg("-no")
        .arg(&out_path)
        .arg(&in_path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let written = fs::read(&out_path).expect("read output file");
    assert_eq!(
        written, b"1\n2\n10\n",
        "numeric sort must apply before the file is written"
    );
    cleanup(&dir);
}

#[test]
fn dash_o_combines_with_u_for_unique_output_to_file() {
    let dir = scratch_dir("o_u");
    let in_path = write_file(&dir, "in.txt", b"a\nb\na\nb\nc\n");
    let out_path = dir.join("out.txt");

    let out = Command::new(SORT)
        .arg("-uo")
        .arg(&out_path)
        .arg(&in_path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let written = fs::read(&out_path).expect("read output file");
    assert_eq!(
        written, b"a\nb\nc\n",
        "unique dedup must apply before the file is written"
    );
    cleanup(&dir);
}

#[test]
fn dash_o_missing_directory_returns_error() {
    let dir = scratch_dir("o_missing_dir");
    let in_path = write_file(&dir, "in.txt", b"b\na\n");
    let bogus = dir.join("does-not-exist").join("nested").join("out.txt");

    let out = Command::new(SORT)
        .arg("-o")
        .arg(&bogus)
        .arg(&in_path)
        .output()
        .expect("spawn sort");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("sort:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(&bogus.to_string_lossy().to_string()),
        "stderr should mention the path, got {stderr:?}"
    );
    assert!(
        !bogus.exists(),
        "no file should have been created at the bogus path"
    );
    cleanup(&dir);
}

#[test]
fn dash_co_combination_returns_usage_error() {
    let dir = scratch_dir("co_reject");
    let out_path = dir.join("out.txt");

    let out = Command::new(SORT)
        .arg("-c")
        .arg("-o")
        .arg(&out_path)
        .stdin(Stdio::null())
        .output()
        .expect("spawn sort");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot combine -c and -o"),
        "stderr = {stderr:?}"
    );
    assert!(
        !out_path.exists(),
        "no file should have been created when -co was rejected"
    );
    cleanup(&dir);
}

#[test]
fn dash_capital_co_combination_returns_usage_error() {
    let dir = scratch_dir("Co_reject");
    let out_path = dir.join("out.txt");

    let out = Command::new(SORT)
        .arg("-C")
        .arg("-o")
        .arg(&out_path)
        .stdin(Stdio::null())
        .output()
        .expect("spawn sort");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot combine -C and -o"),
        "stderr = {stderr:?}"
    );
    assert!(
        !out_path.exists(),
        "no file should have been created when -Co was rejected"
    );
    cleanup(&dir);
}

#[test]
fn dash_o_with_no_input_writes_empty_file() {
    let dir = scratch_dir("o_empty");
    let out_path = dir.join("out.txt");

    let out = Command::new(SORT)
        .arg("-o")
        .arg(&out_path)
        .stdin(Stdio::null())
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    let written = fs::read(&out_path).expect("read output file");
    assert!(
        written.is_empty(),
        "empty stdin under -o must produce empty file, got {written:?}"
    );
    cleanup(&dir);
}

// ---------- -k field-keyed sort ----------

#[test]
fn dash_k_selects_field_n_for_sort() {
    let dir = scratch_dir("k_basic");
    let path = write_file(&dir, "in.txt", b"alpha 3\nbravo 1\ncharlie 2\n");

    let out = Command::new(SORT)
        .arg("-k")
        .arg("2")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"bravo 1\ncharlie 2\nalpha 3\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_k_glued_form_works() {
    let dir = scratch_dir("k_glued");
    let path = write_file(&dir, "in.txt", b"alpha 3\nbravo 1\ncharlie 2\n");

    let glued = Command::new(SORT)
        .arg("-k2")
        .arg(&path)
        .output()
        .expect("spawn sort");
    let spaced = Command::new(SORT)
        .arg("-k")
        .arg("2")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(glued.status.success(), "exit status: {:?}", glued.status);
    assert!(spaced.status.success(), "exit status: {:?}", spaced.status);
    assert_eq!(glued.stdout, spaced.stdout);
    assert_eq!(glued.stdout, b"bravo 1\ncharlie 2\nalpha 3\n");
    cleanup(&dir);
}

#[test]
fn dash_k_zero_is_invalid_field() {
    let out = Command::new(SORT)
        .arg("-k")
        .arg("0")
        .stdin(Stdio::null())
        .output()
        .expect("spawn sort");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sort: invalid field specification: 0"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn dash_k_negative_is_invalid_field() {
    let out = Command::new(SORT)
        .arg("-k")
        .arg("-1")
        .stdin(Stdio::null())
        .output()
        .expect("spawn sort");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sort: invalid field specification: -1"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn dash_k_non_integer_is_invalid_field() {
    let out = Command::new(SORT)
        .arg("-k")
        .arg("foo")
        .stdin(Stdio::null())
        .output()
        .expect("spawn sort");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sort: invalid field specification: foo"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn dash_k_with_no_value_is_invalid_field() {
    let out = Command::new(SORT)
        .arg("-k")
        .stdin(Stdio::null())
        .output()
        .expect("spawn sort");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sort: invalid field specification: <missing>"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn dash_k_empty_string_is_invalid_field() {
    let out = Command::new(SORT)
        .arg("-k")
        .arg("")
        .stdin(Stdio::null())
        .output()
        .expect("spawn sort");
    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sort: invalid field specification: "),
        "stderr = {stderr:?}"
    );
}

#[test]
fn dash_k_missing_field_uses_empty_key() {
    let dir = scratch_dir("k_missing");
    let path = write_file(&dir, "in.txt", b"alpha\nbravo 1\ncharlie\n");

    let out = Command::new(SORT)
        .arg("-k")
        .arg("2")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"alpha\ncharlie\nbravo 1\n",
        "lines with no field 2 (empty key) sort first stably; bravo 1's field 2 is '1'"
    );
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_k_preserves_full_line_in_output() {
    let dir = scratch_dir("k_preserve");
    let path = write_file(&dir, "in.txt", b"keep all this z\nkeep all this a\n");

    let out = Command::new(SORT)
        .arg("-k")
        .arg("4")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"keep all this a\nkeep all this z\n",
        "output must be the FULL line, not just the field"
    );
    cleanup(&dir);
}

#[test]
fn dash_kn_combines_field_and_numeric() {
    let dir = scratch_dir("kn");
    let path = write_file(&dir, "in.txt", b"x 100\ny 9\nz 50\n");

    let out = Command::new(SORT)
        .arg("-k")
        .arg("2")
        .arg("-n")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"y 9\nz 50\nx 100\n",
        "numeric on field 2: 9 < 50 < 100"
    );
    cleanup(&dir);
}

#[test]
fn dash_kr_combines_field_and_reverse() {
    let dir = scratch_dir("kr");
    let path = write_file(&dir, "in.txt", b"alpha 3\nbravo 1\ncharlie 2\n");

    let out = Command::new(SORT)
        .arg("-k")
        .arg("2")
        .arg("-r")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"alpha 3\ncharlie 2\nbravo 1\n",
        "reverse of field-2 sort: 3 > 2 > 1"
    );
    cleanup(&dir);
}

#[test]
fn dash_ku_dedupes_by_field_key() {
    let dir = scratch_dir("ku");
    let path = write_file(&dir, "in.txt", b"red apple\nblue apple\ngreen banana\n");

    let out = Command::new(SORT)
        .arg("-k")
        .arg("2")
        .arg("-u")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"red apple\ngreen banana\n",
        "dedupe by field-2 key collapses duplicate 'apple' keys to the first"
    );
    cleanup(&dir);
}

#[test]
fn dash_kc_checks_with_field_keys() {
    let field_sorted = run_check(&["-k", "2", "-c"], b"red 1\nblue 2\ngreen 3\n");
    assert!(
        field_sorted.status.success(),
        "exit status: {:?}, stderr: {:?}",
        field_sorted.status,
        String::from_utf8_lossy(&field_sorted.stderr)
    );
    assert!(field_sorted.stderr.is_empty());

    let plain_check = run_check(&["-c"], b"red 1\nblue 2\ngreen 3\n");
    assert_eq!(
        plain_check.status.code(),
        Some(1),
        "plain -c should fail because 'red 1' > 'blue 2' under raw lex (r > b)"
    );
    let stderr = String::from_utf8_lossy(&plain_check.stderr);
    assert!(stderr.contains("disorder"), "stderr = {stderr:?}");
}

#[test]
fn dash_kf_combines_field_and_fold() {
    let dir = scratch_dir("kf");
    let path = write_file(&dir, "in.txt", b"x BANANA\ny apple\nz Cherry\n");

    let out = Command::new(SORT)
        .arg("-k")
        .arg("2")
        .arg("-f")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"y apple\nx BANANA\nz Cherry\n",
        "fold of field 2: apple < BANANA < Cherry case-insensitively"
    );
    cleanup(&dir);
}

#[test]
fn dash_kd_combines_field_and_dictionary() {
    let dir = scratch_dir("kd");
    let path = write_file(&dir, "in.txt", b"x co-op\ny coop\nz ca!t\n");

    let out = Command::new(SORT)
        .arg("-k")
        .arg("2")
        .arg("-d")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout,
        b"z ca!t\nx co-op\ny coop\n",
        "dictionary filter on field 2: 'cat' < 'coop' (== 'coop' from co-op stripped) — stable, x before y"
    );
    cleanup(&dir);
}

// ---------- -t field separator ----------

#[test]
fn dash_t_comma_sorts_by_custom_separator_field() {
    let dir = scratch_dir("t_comma");
    let path = write_file(&dir, "in.txt", b"bravo,2\nalpha,3\ncharlie,1\n");

    let out = Command::new(SORT)
        .arg("-t")
        .arg(",")
        .arg("-k")
        .arg("2")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"charlie,1\nbravo,2\nalpha,3\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_t_glued_form_works() {
    let dir = scratch_dir("t_glued");
    let path = write_file(&dir, "in.txt", b"bravo:2\nalpha:3\ncharlie:1\n");

    let glued = Command::new(SORT)
        .arg("-t:")
        .arg("-k2")
        .arg(&path)
        .output()
        .expect("spawn sort");
    let spaced = Command::new(SORT)
        .arg("-t")
        .arg(":")
        .arg("-k")
        .arg("2")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(glued.status.success(), "exit status: {:?}", glued.status);
    assert!(spaced.status.success(), "exit status: {:?}", spaced.status);
    assert_eq!(glued.stdout, spaced.stdout);
    assert_eq!(glued.stdout, b"charlie:1\nbravo:2\nalpha:3\n");
    cleanup(&dir);
}

#[test]
fn dash_t_preserves_empty_fields_from_repeated_separators() {
    let dir = scratch_dir("t_empty_fields");
    let path = write_file(&dir, "in.txt", b"alpha,,z\nbravo,a,z\ncharlie,,a\n");

    let out = Command::new(SORT)
        .arg("-t,")
        .arg("-k")
        .arg("2")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"alpha,,z\ncharlie,,a\nbravo,a,z\n",
        "repeated separators create empty field-2 keys; empty keys sort first stably"
    );
    cleanup(&dir);
}

#[test]
fn dash_t_trailing_separator_produces_empty_final_field() {
    let dir = scratch_dir("t_trailing");
    let path = write_file(&dir, "in.txt", b"a,b,z\nc,d,\ne,f,m\n");

    let out = Command::new(SORT)
        .arg("-t,")
        .arg("-k")
        .arg("3")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"c,d,\ne,f,m\na,b,z\n",
        "the trailing comma creates an empty third field, which sorts before m and z"
    );
    cleanup(&dir);
}

#[test]
fn dash_t_combines_with_numeric_field_sort() {
    let dir = scratch_dir("tn");
    let path = write_file(&dir, "in.txt", b"x:100\ny:9\nz:50\n");

    let out = Command::new(SORT)
        .arg("-t:")
        .arg("-k2")
        .arg("-n")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"y:9\nz:50\nx:100\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_tc_checks_with_custom_separator_field() {
    let sorted = run_check(&["-t", ",", "-k", "2", "-c"], b"red,1\nblue,2\ngreen,3\n");
    assert!(
        sorted.status.success(),
        "exit status: {:?}, stderr: {:?}",
        sorted.status,
        String::from_utf8_lossy(&sorted.stderr)
    );
    assert!(sorted.stderr.is_empty());

    let plain_check = run_check(&["-c"], b"red,1\nblue,2\ngreen,3\n");
    assert_eq!(
        plain_check.status.code(),
        Some(1),
        "plain -c should fail because red > blue under raw lex"
    );
}

#[test]
fn dash_t_without_k_has_no_effect() {
    let dir = scratch_dir("t_no_k");
    let path = write_file(&dir, "in.txt", b"b,1\na,9\n");

    let with_t = Command::new(SORT)
        .arg("-t,")
        .arg(&path)
        .output()
        .expect("spawn sort");
    let plain = Command::new(SORT).arg(&path).output().expect("spawn sort");

    assert!(with_t.status.success(), "exit status: {:?}", with_t.status);
    assert!(plain.status.success(), "exit status: {:?}", plain.status);
    assert_eq!(with_t.stdout, plain.stdout);
    cleanup(&dir);
}

#[test]
fn dash_t_empty_separator_is_invalid() {
    let out = Command::new(SORT)
        .arg("-t")
        .arg("")
        .stdin(Stdio::null())
        .output()
        .expect("spawn sort");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sort: invalid field separator: "),
        "stderr = {stderr:?}"
    );
}

#[test]
fn dash_t_multi_character_separator_is_invalid() {
    let out = Command::new(SORT)
        .arg("-t")
        .arg("::")
        .stdin(Stdio::null())
        .output()
        .expect("spawn sort");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sort: invalid field separator: ::"),
        "stderr = {stderr:?}"
    );
}

#[test]
fn dash_t_with_no_value_is_usage_error() {
    let out = Command::new(SORT)
        .arg("-t")
        .stdin(Stdio::null())
        .output()
        .expect("spawn sort");

    assert_eq!(out.status.code(), Some(2), "exit status: {:?}", out.status);
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sort: option requires an argument: -t"),
        "stderr = {stderr:?}"
    );
}

// ---------- -V version sort ----------

#[test]
fn dash_v_sorts_versions_naturally() {
    let dir = scratch_dir("V_basic");
    let path = write_file(&dir, "in.txt", b"file1\nfile10\nfile2\n");

    let out = Command::new(SORT)
        .arg("-V")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"file1\nfile2\nfile10\n",
        "version sort: file2 < file10 (numeric digit-run compare)"
    );
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_v_handles_multi_segment_versions() {
    let dir = scratch_dir("V_multi");
    let path = write_file(&dir, "in.txt", b"1.0.10\n1.0.2\n1.0.1\n");

    let out = Command::new(SORT)
        .arg("-V")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"1.0.1\n1.0.2\n1.0.10\n",
        "semver-style: each digit run compared numerically"
    );
    cleanup(&dir);
}

#[test]
fn dash_v_compares_digit_runs_numerically() {
    let dir = scratch_dir("V_digits");
    let path = write_file(&dir, "in.txt", b"v1.2\nv1.10\nv1.1\n");

    let out = Command::new(SORT)
        .arg("-V")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"v1.1\nv1.2\nv1.10\n",
        "digit runs compare as integers, not bytes"
    );
    cleanup(&dir);
}

#[test]
fn dash_v_leading_zero_tiebreak() {
    let dir = scratch_dir("V_zeros");
    let path = write_file(&dir, "in.txt", b"01\n1\n001\n");

    let out = Command::new(SORT)
        .arg("-V")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout,
        b"1\n01\n001\n",
        "tiebreak rule: equal-value digit runs sort by SHORTER REPRESENTATION FIRST (fewer leading zeros first)"
    );
    cleanup(&dir);
}

#[test]
fn dash_v_pure_text_falls_back_to_lex() {
    let dir = scratch_dir("V_text");
    let path = write_file(&dir, "in.txt", b"banana\napple\ncherry\n");

    let out = Command::new(SORT)
        .arg("-V")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"apple\nbanana\ncherry\n",
        "no digit runs: every position is non-digit, so byte-order lex sort"
    );
    cleanup(&dir);
}

#[test]
fn dash_v_pure_digits_compares_numerically() {
    let dir = scratch_dir("V_pure_digits");
    let path = write_file(&dir, "in.txt", b"100\n2\n10\n");

    let out = Command::new(SORT)
        .arg("-V")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"2\n10\n100\n",
        "single digit-run per line: numeric compare (2 < 10 < 100)"
    );
    cleanup(&dir);
}

#[test]
fn dash_v_combines_with_reverse() {
    let dir = scratch_dir("Vr");
    let path = write_file(&dir, "in.txt", b"file1\nfile10\nfile2\n");

    let out = Command::new(SORT)
        .arg("-Vr")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"file10\nfile2\nfile1\n",
        "version sort then reverse: file10 > file2 > file1"
    );
    cleanup(&dir);
}

#[test]
fn dash_v_combines_with_unique() {
    let dir = scratch_dir("Vu");
    let path = write_file(&dir, "in.txt", b"file1\nfile1\nfile2\nfile10\n");

    let out = Command::new(SORT)
        .arg("-Vu")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"file1\nfile2\nfile10\n",
        "duplicate file1 collapses; version order preserved"
    );
    cleanup(&dir);
}

#[test]
fn dash_v_combines_with_check() {
    let v_sorted_input: &[u8] = b"file1\nfile2\nfile10\n";
    let vc = run_check(&["-Vc"], v_sorted_input);
    assert!(
        vc.status.success(),
        "exit status: {:?}, stderr: {:?}",
        vc.status,
        String::from_utf8_lossy(&vc.stderr)
    );
    assert!(vc.stderr.is_empty());

    let plain_c = run_check(&["-c"], v_sorted_input);
    assert_eq!(
        plain_c.status.code(),
        Some(1),
        "plain -c on version-sorted (but lex-disordered) input must FAIL: file2 > file10 lexically"
    );
    let stderr = String::from_utf8_lossy(&plain_c.stderr);
    assert!(stderr.contains("disorder"), "stderr = {stderr:?}");
}

#[test]
fn dash_v_with_field_key() {
    let dir = scratch_dir("V_field");
    let path = write_file(
        &dir,
        "in.txt",
        b"alpha file10\nbravo file2\ncharlie file1\n",
    );

    let out = Command::new(SORT)
        .arg("-V")
        .arg("-k")
        .arg("2")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"charlie file1\nbravo file2\nalpha file10\n",
        "version compare on field 2: file1 < file2 < file10"
    );
    cleanup(&dir);
}

#[test]
fn dash_vn_numeric_dominates() {
    let dir = scratch_dir("Vn_dominates");
    let path = write_file(&dir, "in.txt", b"file10\nfile2\nfile1\n");

    let vn = Command::new(SORT)
        .arg("-Vn")
        .arg(&path)
        .output()
        .expect("spawn sort");
    let n_only = Command::new(SORT)
        .arg("-n")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(vn.status.success(), "exit status: {:?}", vn.status);
    assert!(n_only.status.success(), "exit status: {:?}", n_only.status);
    assert_eq!(
        vn.stdout, n_only.stdout,
        "-Vn must equal -n alone: numeric flag dominates over version sort (key_for checks numeric first)"
    );
    cleanup(&dir);
}

#[test]
fn dash_v_overflow_falls_back_to_lex() {
    let dir = scratch_dir("V_overflow");
    let huge_a = "9".repeat(25);
    let huge_b = "1".repeat(25);
    let input = format!("{huge_a}\n{huge_b}\n");
    let path = write_file(&dir, "in.txt", input.as_bytes());

    let out = Command::new(SORT)
        .arg("-V")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(
        out.status.success(),
        "exit status: {:?}, stderr: {:?}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let expected = format!("{huge_b}\n{huge_a}\n");
    assert_eq!(
        out.stdout,
        expected.as_bytes(),
        "digit runs > 20 chars overflow u64; fall back to byte-order lex compare ('1...' < '9...')"
    );
    cleanup(&dir);
}

#[test]
fn dash_v_combines_with_fold() {
    let dir = scratch_dir("Vf");
    let path = write_file(&dir, "in.txt", b"FILE2\nfile10\nFile1\n");

    let out = Command::new(SORT)
        .arg("-V")
        .arg("-f")
        .arg(&path)
        .output()
        .expect("spawn sort");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(
        out.stdout, b"File1\nFILE2\nfile10\n",
        "case-fold then version compare: File1 < FILE2 < file10 case-insensitively"
    );
    cleanup(&dir);
}
