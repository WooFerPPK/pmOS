//! `sh::run` REPL driver tests for the POSIX `false` builtin
//! (T144 follow-up).
//!
//! `false` is the POSIX failure no-op: it does nothing (no
//! stdout, no stderr, no env / cwd mutation), ignores every
//! argument, and exits with status 1. Used in patterns like
//! `until false; do ...` (loop body never starts), `cmd &&
//! false` (force fail a chain), or as a placeholder in
//! conditional logic. The semantic distinction from `true`
//! (exit 0) is the exit code only.
//!
//! Critical correctness pin: `false` MUST NOT terminate the
//! shell. The naive read of "always exits 1" would suggest
//! `BuiltinOutcome::Exit(1)`, but that variant ends the whole
//! REPL (run.rs returns `ExitStatus::Exit(code)` from the
//! match arm). The right shape is the
//! `BuiltinOutcome::Status(1)` variant added in this slice —
//! REPL continues, status byte is dropped pending a future
//! `$?` slice. Two of the three tests below explicitly pin
//! the "REPL stays alive" half of the contract by running a
//! follow-up command on the next line.

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
fn false_alone_exits_one_but_repl_can_continue() {
    // `false` on its own line writes nothing to stdout or
    // stderr and the REPL stays alive — the next line
    // (`exit`) runs and reports a clean Exit(0). The `false`
    // status byte (1) is dropped at the dispatch arm because
    // the v1 REPL has no `$?` surface yet; the only
    // observable effect is "no termination, no output".
    // Stdout shape is exactly two prompts with nothing
    // between them, the same shape `:` and `true` produce.
    let (status, stdout, stderr, _env) = drive("false\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(stdout, "$ $ ");
}

#[test]
fn false_with_extra_args_still_exits_one() {
    // `false foo bar baz` discards every arg and emits no
    // output. Same two-prompt shape as the bare-false case.
    // The exit-1 status is still produced internally but
    // dropped before observable surfacing.
    let (status, stdout, stderr, _env) = drive("false foo bar baz\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(stdout, "$ $ ");
}

#[test]
fn false_in_sequence_continues_to_next_command() {
    // `false` doesn't terminate the REPL or corrupt the
    // tokenizer — the `echo still here` line on the next
    // line produces its usual output. This is the load-bearing
    // assertion: it pins that the dispatch arm uses the
    // non-terminating `Status(1)` variant rather than the
    // REPL-terminating `Exit(1)` variant.
    let (status, stdout, stderr, _env) = drive("false\necho still here\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("still here\n"),
        "stdout missing echo output after false: {stdout:?}"
    );
}
