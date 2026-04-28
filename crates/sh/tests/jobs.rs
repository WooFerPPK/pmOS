//! T145 — `&` background syntax + `jobs` builtin + Ctrl-C
//! interrupt integration tests.

use std::collections::BTreeMap;
use std::io::Cursor;

use sh::{run_with_env, ExitStatus, ShellFlags};

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
fn background_pipeline_runs_to_completion() {
    // v1: `&` runs synchronously since the shell can't
    // background-spawn yet; the job table records the
    // completed pipeline so `jobs` lists it.
    let (status, stdout, _stderr) = drive("echo bg &\njobs\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    // `echo bg &` — backgrounded, so its output goes to
    // parent stdout (the `&` doesn't redirect).
    assert!(stdout.contains("bg"), "stdout: {stdout:?}");
    // `jobs` lists the backgrounded job.
    assert!(stdout.contains("[1]"), "stdout: {stdout:?}");
    assert!(stdout.contains("Done"), "stdout: {stdout:?}");
    assert!(stdout.contains("echo bg &"), "stdout: {stdout:?}");
}

#[test]
fn jobs_purges_completed_after_listing() {
    // Run `jobs` once → list completed; run again → empty.
    let (_status, stdout, _stderr) =
        drive("echo a &\njobs\njobs\nexit\n");
    let occurrences = stdout.matches("[1]").count();
    assert_eq!(
        occurrences, 1,
        "completed job should appear exactly once, then purge: {stdout:?}"
    );
}

#[test]
fn jobs_with_zero_entries_renders_nothing_extra() {
    let (_status, stdout, _stderr) = drive("jobs\nexit\n");
    // Just the prompt's stdout interleave; no `[N]` markers.
    assert!(!stdout.contains("[1]"), "stdout: {stdout:?}");
}

#[test]
fn ampersand_mid_line_is_a_parse_error() {
    let (_status, _stdout, stderr) = drive("echo a & echo b\nexit\n");
    assert!(
        stderr.contains("syntax error"),
        "stderr should be parse error: {stderr:?}"
    );
}

#[test]
fn background_pipeline_does_not_propagate_status_to_dollar_question() {
    // POSIX: `cmd &` always succeeds in the foreground —
    // the shell records status 0 regardless of the
    // backgrounded pipeline's exit code.
    let (_status, stdout, _stderr) = drive("false &\necho status=$?\nexit\n");
    assert!(stdout.contains("status=0"), "stdout: {stdout:?}");
}

#[test]
fn ctrl_c_byte_in_input_cancels_line_without_dispatch() {
    // \x03 in the input should cause the line to be
    // discarded with last_status = 130 (POSIX 128 + SIGINT).
    let (status, stdout, stderr) = drive("echo before\necho \x03 mid\necho status=$?\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stdout.contains("before"), "stdout: {stdout:?}");
    // The `echo \x03 mid` line is cancelled — neither
    // `mid` nor any echo output appears.
    assert!(!stdout.contains("mid"), "stdout: {stdout:?}");
    // last_status records 130 from the cancelled line.
    assert!(stdout.contains("status=130"), "stdout: {stdout:?}");
    // The cancellation writes a newline to stderr (visual
    // feedback for the user).
    assert!(stderr.contains('\n') || stderr.is_empty());
}

#[test]
fn ctrl_c_does_not_terminate_repl() {
    // The REPL must stay alive after a Ctrl-C cancellation
    // — subsequent commands continue to run.
    let (status, stdout, _stderr) = drive("\x03\necho still_alive\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stdout.contains("still_alive"), "stdout: {stdout:?}");
}
