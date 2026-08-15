//! `sh::run` REPL driver tests for `set -u` nounset mode
//! toggle (T142 follow-up to `set -e`).
//!
//! POSIX `set -u` (also `set -o nounset`) makes the shell
//! treat references to unset variables as errors: any
//! `$NAME` or `${NAME}` (without the `${NAME:-default}`
//! exemption) for an unset name writes `sh: <name>:
//! parameter not set\n` to stderr and terminates the REPL
//! with status 1. The failing-expansion command never
//! runs. `set +u` (and `set +o nounset`) clears the flag.
//!
//! These tests drive `run_with_env` directly so they can
//! observe the post-loop `flags` struct AND pin the
//! "REPL terminates with status 1, NOT a generic command-
//! not-found 127" semantic — the load-bearing property
//! that distinguishes nounset failures from runtime
//! command failures. The `:-` default-value form is
//! explicitly tested for its POSIX-required exemption,
//! and `$?` is tested for its always-defined property.

use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};

use sh::{run_with_env, ExitStatus, ShellFlags};

/// Drive `run_with_env` with a byte-string stdin, a fresh
/// empty env map, and a fresh default `ShellFlags`; return
/// `(status, stdout, stderr, flags)` for assertion. The
/// returned flags lets a test inspect post-loop mutations
/// (e.g. did `set -u` actually flip the bit).
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
fn set_u_alone_does_not_terminate_repl() {
    // `set -u` flips the flag but doesn't itself reference
    // any variable, so the post-dispatch nounset check
    // (which lives in the expansion layer) can never fire
    // on the `set` line. The follow-up `exit` runs cleanly.
    // Pins that the `set` builtin's own arg processing
    // doesn't accidentally trip its own check.
    let (status, _stdout, stderr, flags) = drive("set -u\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(flags.nounset, "set -u should have flipped the flag");
}

#[test]
fn set_u_then_unset_var_terminates_with_status_one() {
    // The canonical `set -u` failure: turn nounset on,
    // run `echo $UNSET`, the REPL terminates with Exit(1)
    // on the expansion (the `echo` never runs because the
    // expansion fails before dispatch), and the
    // `echo unreached` line never gets the chance to run.
    // Pins that the failing-expansion command does NOT
    // produce output and the REPL terminates immediately.
    let (status, stdout, stderr, _flags) = drive("set -u\necho $UNSET\necho unreached\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(
        stderr.contains("UNSET"),
        "stderr missing var name: {stderr:?}"
    );
    assert!(
        stderr.contains("parameter not set"),
        "stderr missing diagnostic: {stderr:?}"
    );
    assert!(
        !stdout.contains("unreached"),
        "echo after nounset-fire must NOT run: {stdout:?}"
    );
}

#[test]
fn set_u_then_set_var_does_not_terminate() {
    // `set -u` is on, but the var IS set — the expansion
    // resolves to its value with no error, the `echo`
    // runs, the REPL stays alive, the follow-up `exit`
    // runs cleanly. Pins the set-var short-circuit before
    // the nounset check.
    let (status, stdout, stderr, _flags) = drive("set -u\nexport X=hello\necho $X\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("hello\n"),
        "echo with set var under nounset must run: {stdout:?}"
    );
}

#[test]
fn set_u_with_long_form_o_nounset() {
    // `set -o nounset` is the long-option form, semantically
    // identical to `set -u`. Same termination behavior
    // applies. Pins the `-o NAME` parser path AND the
    // shared `flags.nounset = true` mutation.
    let (status, _stdout, stderr, flags) = drive("set -o nounset\necho $UNSET\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(
        stderr.contains("parameter not set"),
        "stderr missing diagnostic via -o nounset: {stderr:?}"
    );
    assert!(
        flags.nounset,
        "set -o nounset should have flipped the flag: {flags:?}"
    );
}

#[test]
fn set_plus_u_clears_flag() {
    // `set +u` (and `set +o nounset`) reverts the flag.
    // Sequence: turn nounset on, immediately turn it off,
    // reference an unset var, then `exit`. The `$UNSET`
    // expansion no longer triggers termination because
    // nounset was cleared before it ran — it expands to
    // empty per the default. Pins both polarities of the
    // toggle.
    let (status, stdout, stderr, flags) =
        drive("set -u\nset +u\necho $UNSET\necho still here\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("still here\n"),
        "echo after `set +u` clears nounset must run: {stdout:?}"
    );
    assert!(
        !flags.nounset,
        "set +u should have cleared nounset, got {flags:?}"
    );
}

#[test]
fn set_u_with_default_value_form_does_not_terminate() {
    // POSIX-required exemption: `${UNSET:-fallback}` is the
    // fallback-for-unset-vars form, so it MUST NOT trip
    // nounset even when UNSET has no env entry. The
    // expansion produces "fallback" and the `echo` runs.
    // Pins the load-bearing semantic that lets the `:-`
    // form remain useful under `set -u`.
    let (status, stdout, stderr, _flags) = drive("set -u\necho ${UNSET:-fallback}\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("fallback\n"),
        "${{UNSET:-fallback}} under nounset must produce default: {stdout:?}"
    );
}

#[test]
fn set_u_with_set_var_via_default_form_uses_value() {
    // `${X:-fallback}` with X set + nounset on → value
    // wins, default discarded, REPL stays alive. Pins
    // that the set-var branch of `:-` doesn't accidentally
    // surface a nounset error when crossing through the
    // exempt path.
    let (status, stdout, stderr, _flags) =
        drive("set -u\nexport X=actual\necho ${X:-fallback}\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("actual\n"),
        "set var under :- must use value, not fallback: {stdout:?}"
    );
    assert!(
        !stdout.contains("fallback"),
        "default must NOT appear when var is set: {stdout:?}"
    );
}

#[test]
fn set_u_with_dollar_question_does_not_terminate() {
    // `$?` is always defined (last_status is an i32 with
    // a known initial value of 0) — `set -u` MUST NOT
    // fire on it. The `echo $?` line runs and prints "0";
    // the follow-up `exit` runs cleanly. Pins the
    // load-bearing exemption that keeps `$?` usable under
    // every flag combination.
    let (status, stdout, stderr, _flags) = drive("set -u\necho $?\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "$? under nounset must expand to status digit: {stdout:?}"
    );
}

#[test]
fn set_u_stderr_diagnostic_includes_var_name() {
    // The diagnostic shape MUST include the offending var
    // name so the user can fix the script. Pin both halves
    // of the message — the var name (so the user knows
    // WHICH var) and the "parameter not set" phrase (so the
    // user knows WHY the shell died).
    let (_status, _stdout, stderr, _flags) = drive("set -u\necho $MYVAR\n");
    assert!(
        stderr.contains("MYVAR"),
        "stderr missing var name: {stderr:?}"
    );
    assert!(
        stderr.contains("parameter not set"),
        "stderr missing diagnostic phrase: {stderr:?}"
    );
    // Pin the full shape: `sh: MYVAR: parameter not set\n`.
    assert!(
        stderr.contains("sh: MYVAR: parameter not set"),
        "stderr missing full diagnostic line: {stderr:?}"
    );
}

#[test]
fn set_u_with_braced_unset_var_terminates() {
    // The braced form `${MYVAR}` (without `:-`) follows
    // the same nounset rule as the bare form. Pins that
    // the brace arm checks the flag too — a regression
    // here would let `${UNSET}` silently expand to empty
    // even with nounset on.
    let (status, _stdout, stderr, _flags) = drive("set -u\necho ${MYVAR}\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(
        stderr.contains("MYVAR"),
        "stderr missing var name on braced form: {stderr:?}"
    );
    assert!(
        stderr.contains("parameter not set"),
        "stderr missing diagnostic on braced form: {stderr:?}"
    );
}

#[test]
fn set_u_unknown_command_does_not_trigger_nounset() {
    // Critical correctness regression guard: the
    // NotBuiltin / "command not found" path does NOT
    // reference any var; only the EXPANSION layer triggers
    // nounset. So a typo'd command name under `set -u`
    // produces the standard 127 (and under errexit it
    // would terminate, but here errexit is off) and does
    // NOT surface "parameter not set". Pins the boundary
    // between nounset (expansion-layer) and the dispatch-
    // layer error paths.
    let (status, _stdout, stderr, _flags) = drive("set -u\nno_such_command_here\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("command not found"),
        "stderr missing not-found message: {stderr:?}"
    );
    assert!(
        !stderr.contains("parameter not set"),
        "stderr should NOT contain nounset diagnostic: {stderr:?}"
    );
}
