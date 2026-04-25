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
fn about_prints_version_abi_license_credits() {
    let dir = env::temp_dir().join(format!(
        "pmos-settings-about-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).expect("mkdir about doc-root");
    fs::write(dir.join("LICENSE.txt"), b"MIT-licensed. See full text in repo root.\n")
        .expect("write LICENSE fixture");
    fs::write(dir.join("CREDITS.txt"), b"PMos contributors.\n").expect("write CREDITS fixture");

    let out = Command::new(SETTINGS)
        .args(["about", "--doc-root"])
        .arg(&dir)
        .output()
        .expect("spawn settings about");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");

    assert!(stdout.contains("PMos v0.1.0-alpha"), "stdout = {stdout:?}");
    assert!(
        stdout.contains("Kernel ABI version: 1.1"),
        "stdout = {stdout:?}"
    );
    assert!(stdout.contains("License:"), "stdout = {stdout:?}");
    assert!(
        stdout.contains("MIT-licensed. See full text in repo root."),
        "stdout = {stdout:?}"
    );
    assert!(stdout.contains("Credits:"), "stdout = {stdout:?}");
    assert!(stdout.contains("PMos contributors."), "stdout = {stdout:?}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn about_missing_doc_root_exits_one() {
    let missing = env::temp_dir().join(format!(
        "pmos-settings-about-missing-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let out = Command::new(SETTINGS)
        .args(["about", "--doc-root"])
        .arg(&missing)
        .output()
        .expect("spawn settings about missing");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("failed to read"),
        "stderr should explain read failure: {stderr:?}"
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

#[test]
fn set_theme_writes_new_theme_name_to_fresh_config() {
    let path = temp_file("set-theme-fresh");

    let out = Command::new(SETTINGS)
        .args(["set-theme", "dark", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-theme");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);

    let written = fs::read_to_string(&path).expect("read written config");
    assert!(
        written.contains("theme.name = \"dark\"")
            || written.contains("[theme]\nname = \"dark\""),
        "written config should contain theme.name = dark: {written:?}"
    );

    let _ = fs::remove_file(&path);
}

#[test]
fn set_theme_preserves_other_fields() {
    let original = b"[theme]\n\
        name = \"light\"\n\
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
        font = \"unifont-mono-14.pbm\"\n";
    let path = write_temp("set-theme-preserve", original);

    let out = Command::new(SETTINGS)
        .args(["set-theme", "dark", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-theme");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);

    let written = fs::read(&path).expect("read written config");
    let prefs = preferences::Preferences::parse(&written)
        .expect("written config must round-trip through parse");

    assert_eq!(prefs.theme_name.as_deref(), Some("dark"));
    assert_eq!(prefs.theme_fit.as_deref(), Some("stretch"));
    assert_eq!(prefs.wallpaper_name.as_deref(), Some("mountains.png"));
    assert_eq!(prefs.keyboard_layout.as_deref(), Some("us-qwerty"));
    assert_eq!(prefs.timezone_iana.as_deref(), Some("America/New_York"));
    assert_eq!(prefs.terminal_font.as_deref(), Some("unifont-mono-14.pbm"));

    let _ = fs::remove_file(&path);
}

#[test]
fn set_theme_overwrites_existing_theme_name() {
    let path = write_temp(
        "set-theme-overwrite",
        b"[theme]\nname = \"light\"\n",
    );

    let out = Command::new(SETTINGS)
        .args(["set-theme", "dark", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-theme");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);

    let written = fs::read_to_string(&path).expect("read written config");
    let dark_occurrences = written.matches("\"dark\"").count();
    let light_occurrences = written.matches("\"light\"").count();
    assert_eq!(
        dark_occurrences, 1,
        "theme.name = dark should appear exactly once: {written:?}"
    );
    assert_eq!(
        light_occurrences, 0,
        "old theme.name = light should be gone: {written:?}"
    );

    let prefs = preferences::Preferences::parse(written.as_bytes())
        .expect("written config round-trips");
    assert_eq!(prefs.theme_name.as_deref(), Some("dark"));

    let _ = fs::remove_file(&path);
}

#[test]
fn set_theme_empty_name_exits_one() {
    let path = temp_file("set-theme-empty");

    let out = Command::new(SETTINGS)
        .args(["set-theme", "", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-theme empty");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("set-theme"),
        "stderr should mention set-theme: {stderr:?}"
    );
}

#[test]
fn set_theme_missing_name_arg_exits_one_with_usage() {
    let path = temp_file("set-theme-missing");

    let out = Command::new(SETTINGS)
        .args(["set-theme", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-theme missing");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("usage:"),
        "stderr should contain 'usage:': {stderr:?}"
    );
}

#[test]
fn set_theme_invalid_toml_exits_one() {
    let path = write_temp(
        "set-theme-garbage",
        b"garbage bytes without any section\n",
    );

    let out = Command::new(SETTINGS)
        .args(["set-theme", "dark", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-theme garbage");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("failed to parse") || stderr.contains("set-theme"),
        "stderr should explain failure: {stderr:?}"
    );
}

#[test]
fn set_wallpaper_writes_new_value_to_fresh_config() {
    let path = temp_file("set-wallpaper-fresh");

    let out = Command::new(SETTINGS)
        .args(["set-wallpaper", "mountains.png", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-wallpaper");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);

    let written = fs::read_to_string(&path).expect("read written config");
    assert!(
        written.contains("[wallpaper]"),
        "written config should contain [wallpaper] section: {written:?}"
    );
    assert!(
        written.contains("name = \"mountains.png\""),
        "written config should contain wallpaper name = mountains.png: {written:?}"
    );

    let _ = fs::remove_file(&path);
}

#[test]
fn set_wallpaper_preserves_other_fields() {
    let original = b"[theme]\n\
        name = \"light\"\n\
        fit  = \"stretch\"\n\
        \n\
        [wallpaper]\n\
        name = \"sunset.png\"\n\
        \n\
        [keyboard]\n\
        layout = \"us-qwerty\"\n\
        \n\
        [timezone]\n\
        iana = \"America/New_York\"\n\
        \n\
        [terminal]\n\
        font = \"unifont-mono-14.pbm\"\n";
    let path = write_temp("set-wallpaper-preserve", original);

    let out = Command::new(SETTINGS)
        .args(["set-wallpaper", "mountains.png", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-wallpaper");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);

    let written = fs::read(&path).expect("read written config");
    let prefs = preferences::Preferences::parse(&written)
        .expect("written config must round-trip through parse");

    assert_eq!(prefs.theme_name.as_deref(), Some("light"));
    assert_eq!(prefs.theme_fit.as_deref(), Some("stretch"));
    assert_eq!(prefs.wallpaper_name.as_deref(), Some("mountains.png"));
    assert_eq!(prefs.keyboard_layout.as_deref(), Some("us-qwerty"));
    assert_eq!(prefs.timezone_iana.as_deref(), Some("America/New_York"));
    assert_eq!(prefs.terminal_font.as_deref(), Some("unifont-mono-14.pbm"));

    let _ = fs::remove_file(&path);
}

#[test]
fn set_wallpaper_overwrites_existing_value() {
    let path = write_temp(
        "set-wallpaper-overwrite",
        b"[wallpaper]\nname = \"sunset.png\"\n",
    );

    let out = Command::new(SETTINGS)
        .args(["set-wallpaper", "mountains.png", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-wallpaper");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);

    let written = fs::read_to_string(&path).expect("read written config");
    let new_occurrences = written.matches("\"mountains.png\"").count();
    let old_occurrences = written.matches("\"sunset.png\"").count();
    let section_occurrences = written.matches("[wallpaper]").count();
    assert_eq!(
        new_occurrences, 1,
        "wallpaper.name = mountains.png should appear exactly once: {written:?}"
    );
    assert_eq!(
        old_occurrences, 0,
        "old wallpaper.name = sunset.png should be gone: {written:?}"
    );
    assert_eq!(
        section_occurrences, 1,
        "[wallpaper] section should appear exactly once: {written:?}"
    );

    let prefs = preferences::Preferences::parse(written.as_bytes())
        .expect("written config round-trips");
    assert_eq!(prefs.wallpaper_name.as_deref(), Some("mountains.png"));

    let _ = fs::remove_file(&path);
}

#[test]
fn set_wallpaper_missing_value_arg_exits_one_with_usage() {
    let path = temp_file("set-wallpaper-missing");

    let out = Command::new(SETTINGS)
        .args(["set-wallpaper", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-wallpaper missing");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("usage:"),
        "stderr should contain 'usage:': {stderr:?}"
    );
    assert!(
        stderr.contains("set-wallpaper"),
        "stderr should mention set-wallpaper: {stderr:?}"
    );
}

#[test]
fn set_wallpaper_empty_value_exits_one() {
    let path = temp_file("set-wallpaper-empty");

    let out = Command::new(SETTINGS)
        .args(["set-wallpaper", "", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-wallpaper empty");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("set-wallpaper"),
        "stderr should mention set-wallpaper: {stderr:?}"
    );
}

#[test]
fn set_wallpaper_rejects_value_with_quote() {
    let path = temp_file("set-wallpaper-quote");

    let out = Command::new(SETTINGS)
        .args(["set-wallpaper", "bad\"name.png", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-wallpaper quote");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("quotes") || stderr.contains("newlines"),
        "stderr should explain illegal char: {stderr:?}"
    );
}

#[test]
fn set_keyboard_writes_new_value_to_fresh_config() {
    let path = temp_file("set-keyboard-fresh");

    let out = Command::new(SETTINGS)
        .args(["set-keyboard", "us-qwerty", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-keyboard");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);

    let written = fs::read_to_string(&path).expect("read written config");
    assert!(
        written.contains("[keyboard]"),
        "written config should contain [keyboard] section: {written:?}"
    );
    assert!(
        written.contains("layout = \"us-qwerty\""),
        "written config should contain keyboard.layout = us-qwerty: {written:?}"
    );

    let _ = fs::remove_file(&path);
}

#[test]
fn set_keyboard_preserves_other_fields() {
    let original = b"[theme]\n\
        name = \"light\"\n\
        fit  = \"stretch\"\n\
        \n\
        [wallpaper]\n\
        name = \"mountains.png\"\n\
        \n\
        [keyboard]\n\
        layout = \"de-qwertz\"\n\
        \n\
        [timezone]\n\
        iana = \"America/New_York\"\n\
        \n\
        [terminal]\n\
        font = \"unifont-mono-14.pbm\"\n";
    let path = write_temp("set-keyboard-preserve", original);

    let out = Command::new(SETTINGS)
        .args(["set-keyboard", "us-qwerty", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-keyboard");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);

    let written = fs::read(&path).expect("read written config");
    let prefs = preferences::Preferences::parse(&written)
        .expect("written config must round-trip through parse");

    assert_eq!(prefs.theme_name.as_deref(), Some("light"));
    assert_eq!(prefs.theme_fit.as_deref(), Some("stretch"));
    assert_eq!(prefs.wallpaper_name.as_deref(), Some("mountains.png"));
    assert_eq!(prefs.keyboard_layout.as_deref(), Some("us-qwerty"));
    assert_eq!(prefs.timezone_iana.as_deref(), Some("America/New_York"));
    assert_eq!(prefs.terminal_font.as_deref(), Some("unifont-mono-14.pbm"));

    let _ = fs::remove_file(&path);
}

#[test]
fn set_keyboard_overwrites_existing_value() {
    let path = write_temp(
        "set-keyboard-overwrite",
        b"[keyboard]\nlayout = \"de-qwertz\"\n",
    );

    let out = Command::new(SETTINGS)
        .args(["set-keyboard", "us-qwerty", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-keyboard");

    assert_eq!(out.status.code(), Some(0), "exit status: {:?}", out.status);

    let written = fs::read_to_string(&path).expect("read written config");
    let new_occurrences = written.matches("\"us-qwerty\"").count();
    let old_occurrences = written.matches("\"de-qwertz\"").count();
    let section_occurrences = written.matches("[keyboard]").count();
    assert_eq!(
        new_occurrences, 1,
        "keyboard.layout = us-qwerty should appear exactly once: {written:?}"
    );
    assert_eq!(
        old_occurrences, 0,
        "old keyboard.layout = de-qwertz should be gone: {written:?}"
    );
    assert_eq!(
        section_occurrences, 1,
        "[keyboard] section should appear exactly once: {written:?}"
    );

    let prefs = preferences::Preferences::parse(written.as_bytes())
        .expect("written config round-trips");
    assert_eq!(prefs.keyboard_layout.as_deref(), Some("us-qwerty"));

    let _ = fs::remove_file(&path);
}

#[test]
fn set_keyboard_missing_layout_arg_exits_one_with_usage() {
    let path = temp_file("set-keyboard-missing");

    let out = Command::new(SETTINGS)
        .args(["set-keyboard", "--config"])
        .arg(&path)
        .output()
        .expect("spawn settings set-keyboard missing");

    assert_eq!(out.status.code(), Some(1), "exit status: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("usage:"),
        "stderr should contain 'usage:': {stderr:?}"
    );
    assert!(
        stderr.contains("set-keyboard"),
        "stderr should mention set-keyboard: {stderr:?}"
    );
}

#[test]
fn set_keyboard_rejects_empty_or_illegal_value() {
    let path_empty = temp_file("set-keyboard-empty");

    let out = Command::new(SETTINGS)
        .args(["set-keyboard", "", "--config"])
        .arg(&path_empty)
        .output()
        .expect("spawn settings set-keyboard empty");

    assert_eq!(
        out.status.code(),
        Some(1),
        "empty layout should exit 1: {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("set-keyboard"),
        "stderr should mention set-keyboard: {stderr:?}"
    );

    let path_quote = temp_file("set-keyboard-quote");

    let out = Command::new(SETTINGS)
        .args(["set-keyboard", "bad\"layout", "--config"])
        .arg(&path_quote)
        .output()
        .expect("spawn settings set-keyboard quote");

    assert_eq!(
        out.status.code(),
        Some(1),
        "layout with quote should exit 1: {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("quotes") || stderr.contains("newlines"),
        "stderr should explain illegal char: {stderr:?}"
    );

    let path_newline = temp_file("set-keyboard-newline");

    let out = Command::new(SETTINGS)
        .args(["set-keyboard", "bad\nlayout", "--config"])
        .arg(&path_newline)
        .output()
        .expect("spawn settings set-keyboard newline");

    assert_eq!(
        out.status.code(),
        Some(1),
        "layout with newline should exit 1: {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("quotes") || stderr.contains("newlines"),
        "stderr should explain illegal char: {stderr:?}"
    );
}
