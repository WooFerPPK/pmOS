//! Integration tests for the `settings` binary CLI slice (T184 partial).
//!
//! Drive the compiled binary via `CARGO_BIN_EXE_settings` so the tests
//! see the exact bytes userland emits. Temp files live under
//! `std::env::temp_dir()` keyed by tag + pid + an atomic counter so
//! parallel runners cannot collide. Each happy-path test removes its
//! temp file on success; failures intentionally leave files behind
//! to aid post-mortem debugging.

use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const SETTINGS: &str = env!("CARGO_BIN_EXE_settings");

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_file(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!(
        "pmos-settings-{}-{}-{}.toml",
        tag,
        std::process::id(),
        n
    ))
}

fn write_temp(tag: &str, contents: &[u8]) -> PathBuf {
    let path = temp_file(tag);
    let mut f = fs::File::create(&path).expect("create temp file");
    f.write_all(contents).expect("write temp file");
    path
}

#[test]
fn reads_full_config_and_prints_all_six_fields() {
    let path = write_temp(
        "full",
        b"[theme]\n\
          name = \"dark\"\n\
          fit  = \"stretch\"\n\
          \n\
          [wallpaper]\n\
          name = \"mountains.png\"\n\
          \n\
          [keyboard]\n\
          layout = \"us-qwerty\"\n\
          \n\
          [timezone]\n\
          iana = \"America/New_York\"\n\
          \n\
          [terminal]\n\
          font = \"unifont-mono-14.pbm\"\n",
    );

    let out = Command::new(SETTINGS)
        .arg(&path)
        .output()
        .expect("spawn settings");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");

    assert!(
        stdout.contains(r#"theme.name      = "dark""#),
        "stdout = {stdout:?}"
    );
    assert!(
        stdout.contains(r#"theme.fit       = "stretch""#),
        "stdout = {stdout:?}"
    );
    assert!(
        stdout.contains(r#"wallpaper.name  = "mountains.png""#),
        "stdout = {stdout:?}"
    );
    assert!(
        stdout.contains(r#"keyboard.layout = "us-qwerty""#),
        "stdout = {stdout:?}"
    );
    assert!(
        stdout.contains(r#"timezone.iana   = "America/New_York""#),
        "stdout = {stdout:?}"
    );
    assert!(
        stdout.contains(r#"terminal.font   = "unifont-mono-14.pbm""#),
        "stdout = {stdout:?}"
    );

    let _ = fs::remove_file(&path);
}

#[test]
fn empty_config_prints_six_unset_lines() {
    let path = write_temp("empty", b"# comment only\n");

    let out = Command::new(SETTINGS)
        .arg(&path)
        .output()
        .expect("spawn settings");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");

    assert!(stdout.contains("theme.name      = (unset)"), "stdout = {stdout:?}");
    assert!(stdout.contains("theme.fit       = (unset)"), "stdout = {stdout:?}");
    assert!(stdout.contains("wallpaper.name  = (unset)"), "stdout = {stdout:?}");
    assert!(stdout.contains("keyboard.layout = (unset)"), "stdout = {stdout:?}");
    assert!(stdout.contains("timezone.iana   = (unset)"), "stdout = {stdout:?}");
    assert!(stdout.contains("terminal.font   = (unset)"), "stdout = {stdout:?}");

    let line_count = stdout.matches('\n').count();
    assert_eq!(line_count, 6, "expected 6 newline-terminated lines, got {line_count}: {stdout:?}");

    let _ = fs::remove_file(&path);
}

#[test]
fn missing_file_exits_one_and_stderr_has_path() {
    let path = temp_file("missing");

    let out = Command::new(SETTINGS)
        .arg(&path)
        .output()
        .expect("spawn settings");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("settings:"), "stderr = {stderr:?}");
    assert!(
        stderr.contains(path.to_str().unwrap()),
        "stderr should mention path: stderr = {stderr:?}"
    );
}

#[test]
fn malformed_toml_exits_one_and_stderr_has_error_class() {
    let path = write_temp("malformed", b"[unterminated\nfoo = \"bar\"\n");

    let out = Command::new(SETTINGS)
        .arg(&path)
        .output()
        .expect("spawn settings");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("failed to parse"),
        "stderr should contain 'failed to parse': {stderr:?}"
    );
    assert!(
        stderr.contains("MalformedSection") || stderr.contains("MalformedLine"),
        "stderr should contain error variant name: {stderr:?}"
    );
}
