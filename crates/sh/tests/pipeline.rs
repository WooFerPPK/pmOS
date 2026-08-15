//! T143 — pipeline runner integration tests.
//!
//! Drives the full REPL via `sh::run_with_env` and asserts
//! the in-process serial pipeline runner correctly chains
//! stages: each non-last stage's stdout becomes the next
//! stage's stdin, the last stage writes to the parent's
//! stdout, and POSIX exit-status semantics are preserved.
//!
//! v1 pipelines connect builtins only — external commands
//! aren't yet wired through `proc_spawn`; that gap is
//! documented in the T143 slice note. The pipeline runner's
//! test surface here is therefore restricted to builtin
//! stages (`echo`, `read`, `cat`-style chains via the
//! `read` builtin's loop, `env`, etc.).

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
fn echo_through_read_pipeline_reads_first_line() {
    // `echo hello | read VAR` — the read builtin consumes
    // its stdin (which is the captured stdout of `echo
    // hello`), splits on the first newline, and assigns
    // "hello" to VAR. Then `echo got=$VAR` confirms VAR was
    // set.
    let (status, stdout, _stderr) = drive("echo hello | read VAR\necho got=$VAR\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stdout.contains("got=hello"), "stdout: {stdout:?}");
}

#[test]
fn three_stage_pipeline_exit_status_is_last_stage() {
    // `true | false | true` → the LAST stage's status is
    // the pipeline's status. POSIX-pipelines collapse to
    // the last stage; intermediate failures don't propagate
    // unless `pipefail` is set (not in v1).
    let (_status, stdout, _stderr) = drive("true | false | true\necho status=$?\nexit\n");
    assert!(stdout.contains("status=0"), "stdout: {stdout:?}");
}

#[test]
fn last_stage_failure_propagates_to_dollar_question() {
    let (_status, stdout, _stderr) = drive("true | false\necho status=$?\nexit\n");
    assert!(stdout.contains("status=1"), "stdout: {stdout:?}");
}

#[test]
fn pipe_outside_quotes_splits_stages() {
    // Adjacent operator chars don't need surrounding
    // whitespace — the tokenizer recognises `|` as its own
    // word even when glued to neighbouring text.
    let (_status, stdout, _stderr) = drive("echo a|read VAR\necho v=$VAR\nexit\n");
    assert!(stdout.contains("v=a"), "stdout: {stdout:?}");
}

#[test]
fn pipeline_with_quoted_pipe_is_one_stage() {
    // A quoted `|` is part of the argv word, NOT a stage
    // separator. The pipeline therefore has ONE stage with
    // the literal `|` as its echo argument.
    let (_status, stdout, _stderr) = drive("echo 'a|b'\nexit\n");
    assert!(stdout.contains("a|b\n"), "stdout: {stdout:?}");
}

#[test]
fn empty_stage_between_pipes_errors() {
    let (_status, _stdout, stderr) = drive("echo hi | | echo bye\nexit\n");
    assert!(
        stderr.contains("syntax error"),
        "stderr should be parse error: {stderr:?}"
    );
}

#[test]
fn pipeline_does_not_pollute_parent_stdout_with_intermediate_stages() {
    // `echo a | echo b` — first stage's output is captured
    // for the second stage; second stage ignores stdin
    // (echo doesn't read stdin) and writes "b" to parent.
    // Parent stdout must contain "b" but NOT "a" — the
    // first stage's bytes never reach the parent.
    let (_status, stdout, _stderr) = drive("echo a | echo b\nexit\n");
    assert!(stdout.contains("b\n"), "stdout: {stdout:?}");
    // The "a" from the first stage was captured for the
    // second stage's stdin, then dropped (echo doesn't
    // read stdin); it must NOT appear in parent stdout.
    let first_stage_bytes_visible = stdout.lines().any(|l| l == "a");
    assert!(
        !first_stage_bytes_visible,
        "first stage bytes leaked: {stdout:?}"
    );
}

#[test]
fn pipe_chained_with_redirect_writes_only_last_stage_to_file() {
    use std::fs;
    let mut p = std::env::temp_dir();
    p.push(format!("sh_pipe_redir_{}", std::process::id()));
    let _ = fs::remove_file(&p);
    let (_status, stdout, _stderr) = drive(&format!(
        "echo first | echo second > {}\nexit\n",
        p.display()
    ));
    assert!(!stdout.contains("first"));
    assert!(!stdout.contains("second"));
    let actual = fs::read_to_string(&p).unwrap();
    assert_eq!(actual, "second\n");
    fs::remove_file(&p).ok();
}
