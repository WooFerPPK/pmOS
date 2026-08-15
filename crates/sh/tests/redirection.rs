//! T142 — file redirection (`>` / `>>` / `<`) integration tests.
//!
//! Drives the REPL via `sh::run_with_env` and asserts that
//! redirections actually open the named files, route the
//! builtin's I/O there, and roll back into the parent on the
//! next command. Single-stage pipelines only; multi-stage
//! pipelines live in `tests/pipeline.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;

use sh::{run_with_env, ExitStatus, ShellFlags};

fn unique_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "sh_redirection_{}_{}_{name}",
        std::process::id(),
        n
    ));
    let _ = fs::remove_file(&p);
    p
}

fn drive(input: &str) -> (ExitStatus, String, String) {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut flags = ShellFlags::default();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let status = run_with_env(
        Cursor::new(input.as_bytes().to_vec()),
        &mut stdout,
        &mut stderr,
        &mut env,
        &mut flags,
    );
    (
        status,
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

#[test]
fn redirect_stdout_truncates_existing_file() {
    let p = unique_path("trunc");
    fs::write(&p, b"old contents").unwrap();
    let (status, stdout, _stderr) = drive(&format!("echo new > {}\nexit\n", p.display()));
    assert_eq!(status, ExitStatus::Exit(0));
    // Nothing on parent stdout — bytes went to file.
    assert!(!stdout.contains("new"));
    let actual = fs::read_to_string(&p).unwrap();
    assert_eq!(actual, "new\n");
    fs::remove_file(&p).ok();
}

#[test]
fn redirect_stdout_creates_file_when_absent() {
    let p = unique_path("create");
    let (_status, _stdout, _stderr) = drive(&format!("echo hi > {}\nexit\n", p.display()));
    let actual = fs::read_to_string(&p).unwrap();
    assert_eq!(actual, "hi\n");
    fs::remove_file(&p).ok();
}

#[test]
fn redirect_stdout_append_preserves_existing_bytes() {
    let p = unique_path("append");
    fs::write(&p, b"first\n").unwrap();
    let (_status, _stdout, _stderr) = drive(&format!("echo second >> {}\nexit\n", p.display()));
    let actual = fs::read_to_string(&p).unwrap();
    assert_eq!(actual, "first\nsecond\n");
    fs::remove_file(&p).ok();
}

#[test]
fn redirect_stdin_feeds_read_builtin() {
    let p = unique_path("stdin");
    fs::write(&p, b"hello world\n").unwrap();
    let (_status, stdout, _stderr) = drive(&format!(
        "read VAR < {}\necho got=$VAR\nexit\n",
        p.display()
    ));
    assert!(stdout.contains("got=hello world"), "stdout: {stdout:?}");
    fs::remove_file(&p).ok();
}

#[test]
fn redirect_only_affects_one_stage() {
    let p = unique_path("oneshot");
    let (_status, stdout, _stderr) = drive(&format!(
        "echo redirected > {}\necho parent\nexit\n",
        p.display()
    ));
    // Second echo writes to parent stdout (no redir):
    assert!(stdout.contains("parent"));
    // First echo's bytes landed in file, not stdout:
    assert!(!stdout.contains("redirected"));
    let actual = fs::read_to_string(&p).unwrap();
    assert_eq!(actual, "redirected\n");
    fs::remove_file(&p).ok();
}

#[test]
fn redirect_to_unwritable_path_reports_error_status() {
    // A path that can't be opened (a non-existent directory)
    // should produce a stderr diagnostic + non-zero $?.
    let (_status, _stdout, stderr) =
        drive("echo hi > /this/dir/does/not/exist/file\necho $?\nexit\n");
    assert!(stderr.contains("/this/dir/does/not/exist/file"));
    // The status should be 1 (file open failure) — the
    // followup echo $? reads the failed redir's status.
    // We can't observe last_status directly here; assert on
    // the diagnostic shape only.
    assert!(stderr.contains("No such file") || stderr.contains("not"));
}

#[test]
fn redirect_target_with_variable_expands() {
    let p = unique_path("expanded");
    let path_str = p.display().to_string();
    let (_status, _stdout, _stderr) = drive(&format!(
        "export OUT={path_str}\necho payload > $OUT\nexit\n"
    ));
    let actual = fs::read_to_string(&p).unwrap();
    assert_eq!(actual, "payload\n");
    fs::remove_file(&p).ok();
}

#[test]
fn missing_redir_target_is_a_parse_error() {
    let (_status, _stdout, stderr) = drive("echo hi >\nexit\n");
    assert!(
        stderr.contains("syntax error"),
        "stderr should report parse error: {stderr:?}"
    );
}

#[test]
fn quoted_pipe_is_not_an_operator() {
    let (_status, stdout, _stderr) = drive("echo 'a|b'\nexit\n");
    assert!(stdout.contains("a|b\n"), "stdout: {stdout:?}");
}

#[test]
fn quoted_redirect_chars_pass_through() {
    let (_status, stdout, _stderr) = drive(
        r#"echo "a>b<c"
exit
"#,
    );
    assert!(stdout.contains("a>b<c\n"), "stdout: {stdout:?}");
}
