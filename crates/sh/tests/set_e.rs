//! `sh::run` REPL driver tests for `set -e` errexit mode
//! toggle (T142 follow-up).
//!
//! POSIX `set -e` (also `set -o errexit`) makes the shell
//! terminate with the failing command's exit status on any
//! non-zero result. PMos sh has no `if` / `while` / `until`
//! / `&&` / `||` / `!` constructs in v1, so there are no
//! POSIX exemption contexts — every non-zero status (after
//! `false`, after a `command not found` 127, after any
//! future `[`/`test`-style builtin that returns
//! `BuiltinOutcome::Status(N)`) terminates the REPL when
//! errexit is on. `set +e` (and `set +o errexit`) clears
//! the flag.
//!
//! These tests drive `run_with_env` directly so they can
//! observe the post-loop `flags` struct AND pin the
//! "REPL terminates with the failing command's exit status,
//! NOT a generic 1" semantic — the load-bearing property
//! that distinguishes `set -e` from a "did we just hit a
//! builtin error" path. The tests deliberately do NOT use
//! the `dollar_question.rs`-style wrapper because that
//! wrapper constructs flags fresh per call; here we want
//! either to seed or to inspect the flags directly.
//!
//! Note on a deferred test: `dollar_question_after_set_e_termination`
//! is NOT included — once errexit fires the REPL exits, so
//! there's no follow-up `echo $?` line that runs to test
//! against. The exit status itself is what `$?` would have
//! shown. The `ExitStatus::Exit(N)` assertion in each
//! termination test pins this directly.

use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};

use sh::{run_with_env, ExitStatus, ShellFlags};

/// Drive `run_with_env` with a byte-string stdin, a fresh
/// empty env map, and a fresh default `ShellFlags`; return
/// `(status, stdout, stderr, flags)` for assertion. The
/// returned flags lets a test inspect post-loop mutations
/// (e.g. did `set -e` actually flip the bit).
fn drive(input: &str) -> (ExitStatus, String, String, ShellFlags) {
    let stdin = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut flags = ShellFlags::default();
    let status = run_with_env(stdin, &mut stdout, &mut stderr, &mut env, &mut flags);
    let out = String::from_utf8(stdout).expect("stdout must be utf-8");
    let err = String::from_utf8(stderr).expect("stderr must be utf-8");
    (status, out, err, flags)
}

#[test]
fn set_e_alone_does_not_terminate_repl() {
    // `set -e` flips the flag but doesn't itself trigger
    // any termination — the builtin returns Continue
    // (status 0), so the post-dispatch errexit check sees
    // last_status == 0 and skips. The follow-up `exit`
    // line runs cleanly. Pins that `set` ITSELF cannot
    // accidentally trip its own check.
    let (status, _stdout, stderr, flags) = drive("set -e\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(flags.errexit, "set -e should have flipped the flag");
}

#[test]
fn set_e_then_false_terminates_with_status_one() {
    // The canonical `set -e` failure path: turn errexit on,
    // run `false`, the REPL terminates with Exit(1) and the
    // `echo unreached` line never runs. Pins that the
    // failing command's exit status BECOMES the shell's
    // exit status (not a generic 1 from the IoError arm or
    // similar).
    let (status, stdout, stderr, _flags) = drive("set -e\nfalse\necho unreached\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        !stdout.contains("unreached"),
        "echo after errexit-fire must NOT run: {stdout:?}"
    );
}

#[test]
fn set_e_then_true_does_not_terminate() {
    // `true` returns Continue (status 0); errexit fires only
    // on non-zero status, so the REPL continues past `true`
    // and the follow-up `echo` runs normally. Pins the
    // status==0 short-circuit in the post-dispatch check.
    let (status, stdout, stderr, _flags) = drive("set -e\ntrue\necho still here\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("still here\n"),
        "echo after `true` under errexit must run: {stdout:?}"
    );
}

#[test]
fn set_e_with_long_form_o_errexit() {
    // `set -o errexit` is the long-option form, semantically
    // identical to `set -e`. Same termination behaviour
    // applies. Pins the `-o NAME` parser path AND the
    // shared `flags.errexit = true` mutation.
    let (status, stdout, stderr, _flags) = drive("set -o errexit\nfalse\necho unreached\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        !stdout.contains("unreached"),
        "echo after errexit-fire (via -o errexit) must NOT run: {stdout:?}"
    );
}

#[test]
fn set_plus_e_clears_flag() {
    // `set +e` (and `set +o errexit`) reverts the flag.
    // Sequence: turn errexit on, immediately turn it off,
    // run `false`, then `echo`. The `false` no longer
    // triggers termination because errexit was cleared
    // before it ran. Pins both polarities of the toggle.
    let (status, stdout, stderr, flags) = drive("set -e\nset +e\nfalse\necho still here\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("still here\n"),
        "echo after `set +e` clears errexit must run: {stdout:?}"
    );
    assert!(
        !flags.errexit,
        "set +e should have cleared errexit, got {flags:?}"
    );
}

#[test]
fn set_e_then_unknown_command_terminates_with_127() {
    // `NotBuiltin` sets last_status to 127 (POSIX-mandated
    // "command not found" status), which under errexit
    // terminates the REPL with Exit(127). Pins that the
    // errexit check handles the NotBuiltin arm's status the
    // same way it handles Status(N), AND that 127 (not a
    // generic 1) is what surfaces — the shell's exit byte
    // identifies WHY it died.
    let (status, stdout, stderr, _flags) = drive("set -e\nno_such_command_here\necho unreached\n");
    assert_eq!(status, ExitStatus::Exit(127));
    assert!(
        stderr.contains("command not found"),
        "stderr missing not-found message: {stderr:?}"
    );
    assert!(
        !stdout.contains("unreached"),
        "echo after errexit-fire on NotBuiltin must NOT run: {stdout:?}"
    );
}

#[test]
fn set_with_no_args_is_no_op() {
    // POSIX `set` with no args prints all shell variables;
    // v1 defers this and the no-arg path is a no-op. The
    // REPL stays alive, the flag is unchanged, and the
    // follow-up `echo` runs. Pins the early-return arm in
    // `builtin_set` (so a future variable-listing slice can
    // wire stdout output here without breaking compatibility).
    let (status, stdout, stderr, flags) = drive("set\necho still here\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("still here\n"),
        "echo after bare `set` must run: {stdout:?}"
    );
    assert!(
        !flags.errexit,
        "bare `set` must not flip errexit: {flags:?}"
    );
}

#[test]
fn set_with_invalid_option_warns_but_continues() {
    // `set -X` is unrecognised; the builtin writes
    // `sh: set: -X: invalid option` to stderr and returns
    // Continue (status 0). The REPL stays alive AND
    // errexit isn't even on, so the follow-up `exit` line
    // runs. Pins the unknown-option diagnostic shape AND
    // that even if errexit WERE on, `set` itself can never
    // trigger a termination because it always returns
    // Continue.
    let (status, _stdout, stderr, flags) = drive("set -X\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("invalid option"),
        "stderr missing invalid-option diagnostic: {stderr:?}"
    );
    assert!(
        stderr.contains("-X"),
        "stderr missing the offending arg: {stderr:?}"
    );
    assert!(
        !flags.errexit,
        "invalid `set -X` must not flip errexit: {flags:?}"
    );
}
