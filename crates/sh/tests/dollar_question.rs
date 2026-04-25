//! `sh::run` REPL driver tests for `$?` last-exit-status
//! parameter expansion (T142 follow-up).
//!
//! `$?` resolves to the most recent command's exit status as
//! a decimal string. Initially `0` (before any command runs);
//! `0` after a `Continue`-arm builtin; `N` after `Status(N)`
//! (e.g. `false` → `1`); `127` after a "command not found";
//! `1` after a builtin I/O failure (POSIX I/O failure
//! convention). Blank lines do NOT update the stash — they're
//! a no-op, no command ran. The braced form `${?}` is
//! semantically identical to the bare `$?`. `$?` is a
//! single-byte parameter (like `$1` in shells with positional
//! args), so `$?bar` expands to `<status>bar` with no need
//! for an explicit braced form.
//!
//! These tests drive `run_with_env` so they can pre-seed env
//! entries and observe end-to-end behaviour through the
//! `echo` builtin's stdout. The single-process test
//! pattern (one `drive` call, multi-line stdin) is the only
//! way to pin "the prior command's status leaks into the
//! NEXT command's expansion" — which is the load-bearing
//! property `$?` exists to surface.

use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};

use sh::{run_with_env, ExitStatus};

/// Drive `run_with_env` with a byte-string stdin and a
/// pre-seeded env map; return `(status, stdout, stderr,
/// env)` for assertion.
fn drive(
    input: &str,
    mut env: BTreeMap<String, String>,
) -> (ExitStatus, String, String, BTreeMap<String, String>) {
    let stdin = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let status = run_with_env(stdin, &mut stdout, &mut stderr, &mut env);
    let out = String::from_utf8(stdout).expect("stdout must be utf-8");
    let err = String::from_utf8(stderr).expect("stderr must be utf-8");
    (status, out, err, env)
}

#[test]
fn dollar_question_expands_to_zero_initially() {
    // No prior command ran: `echo $?` on the first line
    // expands to `0`. Pins the initial-state seed.
    let (status, stdout, stderr, _env) =
        drive("echo $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing initial-state status: {stdout:?}"
    );
}

#[test]
fn dollar_question_after_true_is_zero() {
    // `true` returns `Continue` → status 0; the next line's
    // `$?` expands to `0`. Pins the Continue arm of the
    // status-stashing match.
    let (status, stdout, stderr, _env) =
        drive("true\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-true status: {stdout:?}"
    );
}

#[test]
fn dollar_question_after_false_is_one() {
    // `false` returns `Status(1)` → status 1; the next
    // line's `$?` expands to `1`. Pins the Status(N) arm of
    // the status-stashing match — the central case `$?`
    // exists to surface.
    let (status, stdout, stderr, _env) =
        drive("false\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-false status: {stdout:?}"
    );
}

#[test]
fn dollar_question_after_unknown_command_is_127() {
    // Unknown command writes `sh: command not found` to
    // stderr and the REPL stays alive; the new
    // last_status is the POSIX-mandated `127`. Next line's
    // `$?` expands to `127`.
    let (status, stdout, stderr, _env) = drive(
        "no_such_command_here\necho $?\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("command not found"),
        "stderr missing not-found message: {stderr:?}"
    );
    assert!(
        stdout.contains("127\n"),
        "stdout missing post-not-found status: {stdout:?}"
    );
}

#[test]
fn dollar_question_via_braced_form_works() {
    // `${?}` is the explicit braced form of `$?`. After
    // `false`, both shapes resolve to `"1"`. Pin the
    // brace-arm short-circuit on `region == "?"`.
    let (status, stdout, stderr, _env) =
        drive("false\necho ${?}\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing braced post-false status: {stdout:?}"
    );
}

#[test]
fn dollar_question_followed_by_literal_chars() {
    // `$?bar` expands to `<status>bar`. After `false`, the
    // line `echo $?bar` outputs `1bar\n`. Pin that the `?`
    // is a single-byte parameter and the scanner resumes
    // literal copying after the digits — no need for braces
    // to disambiguate trailing chars.
    let (status, stdout, stderr, _env) =
        drive("false\necho $?bar\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1bar\n"),
        "stdout missing concatenated trailing-text: {stdout:?}"
    );
}

#[test]
fn blank_line_does_not_update_dollar_question() {
    // `false` → status 1; blank line is a no-op (no command
    // ran, so the status stash stays at 1); `echo $?` →
    // `1`. Pin that the `parts.is_empty() { continue; }`
    // path skips the dispatch arm AND the status stash, so
    // the prior status persists. POSIX requires this.
    let (status, stdout, stderr, _env) =
        drive("false\n\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing persisted status across blank line: {stdout:?}"
    );
}

#[test]
fn dollar_question_resets_to_zero_after_successful_command() {
    // After `false` (status 1), running `true` (status 0)
    // resets the stash; `$?` then expands to `0`. Pin that
    // the `Continue` arm OVERWRITES the prior `Status(N)`
    // — it doesn't only update on transitions.
    let (status, stdout, stderr, _env) = drive(
        "false\ntrue\necho $?\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-reset status: {stdout:?}"
    );
}

#[test]
fn dollar_question_persists_across_multiple_lines() {
    // After `false`, multiple `echo $?` lines all show `1`
    // — the `echo` builtin returns `Continue` AFTER its
    // expansion runs, so the line that READS `$?` doesn't
    // overwrite the value `$?` is reading. The stash
    // updates from `1` to `0` only after the first `echo`
    // dispatch finishes.
    let (status, stdout, stderr, _env) = drive(
        "false\necho $?\necho $?\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    // First `echo $?` after `false` sees `1`. Second
    // `echo $?` sees the `0` produced by the first echo's
    // own success. Both occurrences should appear in
    // stdout in order.
    let first_pos = stdout.find("1\n").expect("first echo missing 1");
    let second_pos = stdout.find("0\n").expect("second echo missing 0");
    assert!(
        first_pos < second_pos,
        "expected `1` before `0` in {stdout:?}"
    );
}
