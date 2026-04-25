//! Integration tests for the `wc` coreutil binary.
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

const WC: &str = env!("CARGO_BIN_EXE_wc");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "pmos-wc-{}-{}-{}",
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
fn default_emits_lines_words_bytes() {
    let dir = scratch_dir("default");
    let path = write_file(&dir, "a.txt", b"hello world\nfoo bar baz\n");

    let out = Command::new(WC).arg(&path).output().expect("spawn wc");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = format!("2\t5\t24\t{}\n", path.display());
    assert_eq!(stdout, expected);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_l_emits_lines_only() {
    let dir = scratch_dir("lonly");
    let path = write_file(&dir, "a.txt", b"alpha\nbeta\ngamma\n");

    let out = Command::new(WC).arg("-l").arg(&path).output().expect("spawn wc");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = format!("3\t{}\n", path.display());
    assert_eq!(stdout, expected);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_w_emits_words_only() {
    let dir = scratch_dir("wonly");
    let path = write_file(&dir, "a.txt", b"the quick brown fox\n");

    let out = Command::new(WC).arg("-w").arg(&path).output().expect("spawn wc");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = format!("4\t{}\n", path.display());
    assert_eq!(stdout, expected);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn dash_c_emits_bytes_only() {
    let dir = scratch_dir("conly");
    let path = write_file(&dir, "a.txt", b"hello\n");

    let out = Command::new(WC).arg("-c").arg(&path).output().expect("spawn wc");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = format!("6\t{}\n", path.display());
    assert_eq!(stdout, expected);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn multi_file_appends_total_row() {
    let dir = scratch_dir("multi");
    let a = write_file(&dir, "a.txt", b"hello\n");
    let b = write_file(&dir, "b.txt", b"a b c\n");

    let out = Command::new(WC).arg(&a).arg(&b).output().expect("spawn wc");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected_a = format!("1\t1\t6\t{}\n", a.display());
    let expected_b = format!("1\t3\t6\t{}\n", b.display());
    let expected_total = "2\t4\t12\ttotal\n";
    assert_eq!(stdout, format!("{expected_a}{expected_b}{expected_total}"));
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}

#[test]
fn stdin_mode_omits_filename() {
    let mut child = Command::new(WC)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wc");

    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(b"foo bar\nbaz\n")
        .expect("write stdin");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait wc");
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert_eq!(out.stdout, b"2\t3\t12\n");
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
}

#[test]
fn missing_file_continues_and_exits_one() {
    let dir = scratch_dir("missing");
    let good = write_file(&dir, "good.txt", b"hi\n");
    let missing = dir.join("nope.txt");

    let out = Command::new(WC)
        .arg(&good)
        .arg(&missing)
        .output()
        .expect("spawn wc");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("1\t1\t3\t{}\n", good.display())),
        "stdout = {stdout:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("wc:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr = {stderr:?}"
    );
    cleanup(&dir);
}

#[test]
fn dash_lc_combines_lines_and_bytes() {
    let dir = scratch_dir("lc");
    let path = write_file(&dir, "a.txt", b"foo\nbar\n");

    let out = Command::new(WC).arg("-lc").arg(&path).output().expect("spawn wc");

    assert!(out.status.success(), "exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = format!("2\t8\t{}\n", path.display());
    assert_eq!(stdout, expected);
    assert!(out.stderr.is_empty(), "stderr = {:?}", out.stderr);
    cleanup(&dir);
}
