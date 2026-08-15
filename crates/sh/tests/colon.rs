//! `sh::run` REPL driver tests for the POSIX `:` (colon)
//! null-command builtin (T144 follow-up).
//!
//! `:` is the POSIX null command: it does nothing, ignores
//! every argument, and exits with status 0. Used in patterns
//! like `while :; do ...`, `: ${VAR:=default}`, or as a
//! placeholder for an empty command body. The v1 sh has no
//! `;` separator yet, so each line in these scripts holds
//! exactly one command — `:` always appears alone or with
//! arg tokens, and follow-up commands appear on their own
//! line.
//!
//! Tests drive `sh::run_with_env` (rather than `sh::run`) so
//! the `: $UNSET` case can pre-seed the env map and assert
//! that variable expansion still feeds the colon dispatch
//! exactly the same way it does for any other builtin —
//! `:` doesn't short-circuit expansion, it just ignores the
//! result.

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
fn colon_alone_is_a_no_op() {
    // `:` on its own line writes nothing to stdout or stderr
    // and the REPL stays alive — the next line (`exit`)
    // runs and reports a clean Exit(0). Stdout shape is
    // exactly two prompts with nothing between them, the
    // same shape a blank line produces.
    let (status, stdout, stderr, _env) = drive(":\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(stdout, "$ $ ");
}

#[test]
fn colon_ignores_extra_args() {
    // `: foo bar baz` discards every arg and emits no
    // output. Same two-prompt shape as the bare-colon case.
    let (status, stdout, stderr, _env) = drive(": foo bar baz\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(stdout, "$ $ ");
}

#[test]
fn colon_followed_by_echo_on_next_line_runs_normally() {
    // `:` doesn't terminate the REPL or corrupt the
    // tokenizer — a follow-up `echo done` on the next line
    // produces its usual `done\n` output. The v1 sh has no
    // `;` separator, so this is the closest equivalent to
    // the POSIX `: ; echo done` sequence test.
    let (status, stdout, stderr, _env) = drive(":\necho done\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("done\n"),
        "stdout missing echo output after colon: {stdout:?}"
    );
}

#[test]
fn colon_with_unset_variable_does_not_error() {
    // `: $UNSET_VAR` — the expander turns `$UNSET_VAR` into
    // the empty string (POSIX `set -u`-off default), and the
    // colon arm then ignores the empty arg. No stderr, REPL
    // stays alive.
    let (status, _stdout, stderr, _env) = drive(": $UNSET_VAR\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.is_empty(),
        "colon should not error on unset var expansion: {stderr:?}"
    );
}

#[test]
fn colon_with_set_variable_still_no_op() {
    // Pre-seed `X=hello`; `: $X` expands to `: hello` then
    // the colon arm discards `hello`. Stdout stays at the
    // two-prompt shape — the expansion happens but produces
    // no observable output because `:` writes nothing.
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "hello".to_string());

    let (status, stdout, stderr, env) = drive(": $X\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(stdout, "$ $ ");
    // The env map is untouched — `:` is purely an ignore.
    assert_eq!(env.get("X"), Some(&"hello".to_string()));
}
