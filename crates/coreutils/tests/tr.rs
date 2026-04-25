//! Integration tests for the `tr` coreutil binary.
//!
//! Driven through `std::process::Command` so the tests see the exact
//! bytes the userland binary emits. `tr` reads only from stdin (POSIX
//! `tr` has no file args) so each test pipes input via `Stdio::piped()`
//! and inspects stdout / stderr / exit code byte-by-byte.

use std::io::Write;
use std::process::{Command, Stdio};

const TR: &str = env!("CARGO_BIN_EXE_tr");

fn run_with_stdin(args: &[&str], stdin_bytes: &[u8]) -> (Option<i32>, Vec<u8>, Vec<u8>) {
    let mut child = Command::new(TR)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tr");

    if !stdin_bytes.is_empty() {
        child
            .stdin
            .as_mut()
            .expect("stdin pipe")
            .write_all(stdin_bytes)
            .expect("write stdin");
    }
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait tr");
    (out.status.code(), out.stdout, out.stderr)
}

#[test]
fn translates_set1_to_set2_one_to_one() {
    let (code, stdout, stderr) = run_with_stdin(&["abc", "xyz"], b"abc\n");
    assert_eq!(code, Some(0), "exit code: {code:?}");
    assert_eq!(stdout, b"xyz\n");
    assert!(stderr.is_empty(), "stderr = {:?}", stderr);
}

#[test]
fn set2_shorter_pads_with_last_char() {
    let (code, stdout, stderr) = run_with_stdin(&["abc", "xy"], b"abc\n");
    assert_eq!(code, Some(0), "exit code: {code:?}");
    assert_eq!(stdout, b"xyy\n");
    assert!(stderr.is_empty(), "stderr = {:?}", stderr);
}

#[test]
fn dash_d_deletes_chars_in_set1() {
    let (code, stdout, stderr) = run_with_stdin(&["-d", "lo"], b"hello world\n");
    assert_eq!(code, Some(0), "exit code: {code:?}");
    assert_eq!(stdout, b"he wrd\n");
    assert!(stderr.is_empty(), "stderr = {:?}", stderr);
}

#[test]
fn chars_outside_set1_pass_through() {
    let (code, stdout, stderr) = run_with_stdin(&["abc", "xyz"], b"abXcd\n");
    assert_eq!(code, Some(0), "exit code: {code:?}");
    assert_eq!(stdout, b"xyXzd\n");
    assert!(stderr.is_empty(), "stderr = {:?}", stderr);
}

#[test]
fn wrong_arg_count_exits_one() {
    let (code, stdout, stderr) = run_with_stdin(&[], b"");
    assert_eq!(code, Some(1), "exit code: {code:?}");
    assert!(stdout.is_empty(), "stdout = {:?}", stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(stderr.contains("usage:"), "stderr = {stderr:?}");
}

#[test]
fn unknown_flag_exits_one() {
    let (code, stdout, stderr) = run_with_stdin(&["-x", "abc", "xyz"], b"");
    assert_eq!(code, Some(1), "exit code: {code:?}");
    assert!(stdout.is_empty(), "stdout = {:?}", stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(stderr.contains("unknown flag"), "stderr = {stderr:?}");
}

#[test]
fn dash_d_with_set2_arg_is_error() {
    let (code, stdout, stderr) = run_with_stdin(&["-d", "abc", "xyz"], b"");
    assert_eq!(code, Some(1), "exit code: {code:?}");
    assert!(stdout.is_empty(), "stdout = {:?}", stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(stderr.contains("usage:"), "stderr = {stderr:?}");
}
