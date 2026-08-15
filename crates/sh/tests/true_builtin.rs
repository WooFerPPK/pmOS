//! `sh::run` REPL driver tests for the POSIX `true` builtin
//! (T144 follow-up).
//!
//! `true` is the POSIX successful no-op: it does nothing,
//! ignores every argument, and exits with status 0. Used in
//! patterns like `while true; do ...` (infinite loop),
//! `cmd || true` (don't fail the script if cmd fails), and
//! `if true; then ...`. Functionally equivalent to `:` (the
//! null command, already pinned in `tests/colon.rs`); `true`
//! is the explicit-named form some scripts prefer.
//!
//! Coverage is intentionally smaller than `colon.rs` because
//! the semantics are identical to `:` — only the dispatch
//! token differs. Three tests are sufficient: alone, with
//! extra args, and in sequence with neighbouring commands.

use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};

use sh::{run_with_env, ExitStatus, ShellFlags};

/// Drive `run_with_env` with a byte-string stdin and a
/// pre-seeded env map; return `(status, stdout, stderr,
/// env)` for assertion. Constructs a fresh default
/// `ShellFlags` per call (errexit off) — tests that need
/// errexit pre-set live in `set_e.rs` and call
/// `run_with_env` directly.
fn drive(
    input: &str,
    mut env: BTreeMap<String, String>,
) -> (ExitStatus, String, String, BTreeMap<String, String>) {
    let stdin = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut flags = ShellFlags::default();
    let status = run_with_env(stdin, &mut stdout, &mut stderr, &mut env, &mut flags);
    let out = String::from_utf8(stdout).expect("stdout must be utf-8");
    let err = String::from_utf8(stderr).expect("stderr must be utf-8");
    (status, out, err, env)
}

#[test]
fn true_alone_is_no_op_with_exit_zero() {
    // `true` on its own line writes nothing to stdout or
    // stderr and the REPL stays alive — the next line
    // (`exit`) runs and reports a clean Exit(0). Stdout shape
    // is exactly two prompts with nothing between them, the
    // same shape `:` and a blank line both produce.
    let (status, stdout, stderr, _env) = drive("true\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(stdout, "$ $ ");
}

#[test]
fn true_with_extra_args_is_still_no_op() {
    // `true foo bar baz` discards every arg and emits no
    // output. Same two-prompt shape as the bare-true case.
    let (status, stdout, stderr, _env) = drive("true foo bar baz\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(stdout, "$ $ ");
}

#[test]
fn true_in_sequence_with_other_commands() {
    // `true` doesn't terminate the REPL or corrupt the
    // tokenizer — surrounding `echo` lines both produce their
    // usual output. The v1 sh has no `;` separator, so this
    // is the closest equivalent to the POSIX
    // `echo before; true; echo after` sequence test.
    let (status, stdout, stderr, _env) =
        drive("echo before\ntrue\necho after\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("before\n"),
        "stdout missing first echo output: {stdout:?}"
    );
    assert!(
        stdout.contains("after\n"),
        "stdout missing second echo output after true: {stdout:?}"
    );
}
