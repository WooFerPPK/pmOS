//! `sh::run` REPL driver tests for `set -n` noexec mode
//! toggle (T142 follow-up to `set -e`, `set -u`, and
//! `set -x`).
//!
//! POSIX `set -n` (also `set -o noexec`) makes the shell
//! PARSE and TOKENISE each line but NOT DISPATCH any
//! command — every command is a no-op syntax-check. Useful
//! for "lint" mode: feed a script through `sh -n` to catch
//! quote / expansion errors without executing anything.
//!
//! Critical exemption: the `set` builtin itself ALWAYS
//! runs even under noexec (without that escape hatch
//! `set +n` could never disable the flag once enabled —
//! practical necessity). Every other builtin including
//! `exit` is silently skipped; the script terminates only
//! on EOF (the validated-successfully exit path) or on an
//! expansion-layer error (the `set -u` short-circuit
//! that fires BEFORE the noexec dispatch check).
//!
//! These tests drive `run_with_env` directly so they can
//! observe the post-loop env map AND assert NEGATIVE
//! properties — that the dispatched command's expected
//! output (echo on stdout, env mutations) does NOT appear.
//! The negative shape is load-bearing: the whole point of
//! `set -n` is that commands DON'T run, so each test pins
//! the absence of a side-effect that would have happened
//! without the flag.

use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};

use sh::{run_with_env, ExitStatus, ShellFlags};

/// Drive `run_with_env` with a byte-string stdin, a fresh
/// empty env map, and a fresh default `ShellFlags`; return
/// `(status, stdout, stderr, env, flags)` for assertion. The
/// returned env / flags lets a test inspect post-loop
/// mutations (e.g. did `export X=1` actually mutate the env
/// map under noexec — it must NOT).
fn drive(
    input: &str,
) -> (
    ExitStatus,
    String,
    String,
    BTreeMap<String, String>,
    ShellFlags,
) {
    let stdin = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut flags = ShellFlags::default();
    let status = run_with_env(stdin, &mut stdout, &mut stderr, &mut env, &mut flags);
    let out = String::from_utf8(stdout).expect("stdout must be utf-8");
    let err = String::from_utf8(stderr).expect("stderr must be utf-8");
    (status, out, err, env, flags)
}

#[test]
fn set_n_alone_flips_flag_and_eof_exits_clean() {
    // Just `set -n\n` then EOF — the REPL flips the flag,
    // sees EOF, returns Eof (exit code 0). Pins the
    // canonical "script validated successfully" path: a
    // file fed through `sh -n` with no syntax errors exits
    // cleanly via EOF.
    let (status, _stdout, _stderr, _env, flags) = drive("set -n\n");
    assert_eq!(status, ExitStatus::Eof);
    assert!(flags.noexec, "set -n should have flipped the flag");
}

#[test]
fn set_n_then_echo_does_not_run() {
    // The canonical `set -n` happy path: turn noexec on,
    // run `echo hi`, the echo line is silently skipped, EOF
    // exits clean. Pins that stdout is EMPTY — without the
    // noexec short-circuit `echo hi` would write `hi\n` to
    // stdout. The absence of that output IS the test.
    let (status, stdout, _stderr, _env, _flags) = drive("set -n\necho hi\n");
    assert_eq!(status, ExitStatus::Eof);
    assert!(
        !stdout.contains("hi"),
        "echo under set -n must NOT emit output: {stdout:?}"
    );
}

#[test]
fn set_n_then_export_does_not_mutate_env() {
    // Pins that env-mutating builtins are also short-
    // circuited under noexec — `export X=1` would normally
    // insert `X=1` into the env map, but under `set -n` the
    // dispatch_builtin call is skipped so the mutation
    // never happens. Inspect the post-loop env map directly.
    let (status, _stdout, _stderr, env, _flags) = drive("set -n\nexport X=1\n");
    assert_eq!(status, ExitStatus::Eof);
    assert!(
        !env.contains_key("X"),
        "export under set -n must NOT mutate env: {env:?}"
    );
}

#[test]
fn set_n_with_long_form_o_noexec() {
    // `set -o noexec` is the long-option form, semantically
    // identical to `set -n`. Same skip behaviour applies.
    // Pins the `-o NAME` parser path AND the shared
    // `flags.noexec = true` mutation.
    let (status, stdout, _stderr, _env, flags) = drive("set -o noexec\necho hi\n");
    assert_eq!(status, ExitStatus::Eof);
    assert!(
        !stdout.contains("hi"),
        "echo under set -o noexec must NOT emit output: {stdout:?}"
    );
    assert!(
        flags.noexec,
        "set -o noexec should have flipped the flag: {flags:?}"
    );
}

#[test]
fn set_plus_n_clears_flag_and_subsequent_commands_run() {
    // `set +n` (and `set +o noexec`) reverts the flag.
    // Critical exemption: the `set +n` line itself MUST
    // run under noexec (else the user is permanently stuck
    // in syntax-check mode). After `set +n` the next
    // `echo hi` line should produce its normal output.
    // Pins the load-bearing exemption AND the post-clear
    // command-runs-normally property.
    let (status, stdout, _stderr, _env, flags) = drive("set -n\nset +n\necho hi\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stdout.contains("hi\n"),
        "echo after set +n must run normally: {stdout:?}"
    );
    assert!(
        !flags.noexec,
        "set +n should have cleared noexec, got {flags:?}"
    );
}

#[test]
fn set_n_set_command_itself_still_runs() {
    // Direct restatement of the load-bearing exemption:
    // even with noexec on, the `set` builtin runs. Without
    // this property `set +n` would be inert — the test
    // proves the exemption fires by observing that the
    // post-loop flags state matches the LAST `set`
    // invocation (the +n clearing), not the FIRST (the -n
    // setting). If the set builtin were also skipped under
    // -n then `set +n` would be silently ignored and the
    // post-loop flags would still have noexec=true.
    let (_status, _stdout, _stderr, _env, flags) = drive("set -n\nset +n\n");
    assert!(
        !flags.noexec,
        "set +n under set -n must clear the flag: {flags:?}"
    );
}

#[test]
fn set_n_quote_error_still_surfaces() {
    // Quote errors are syntax errors — they fire in the
    // tokeniser BEFORE the noexec short-circuit (which
    // sits AFTER expansion in the dispatch loop). So under
    // `set -n` an unterminated single quote still writes
    // the diagnostic to stderr. Pins that the
    // syntax-checking aspect of `set -n` actually works:
    // the whole point of "lint mode" is that syntax errors
    // are caught, even though commands don't run.
    let (status, _stdout, stderr, _env, _flags) = drive("set -n\necho 'unclosed\n");
    assert_eq!(status, ExitStatus::Eof);
    assert!(
        stderr.contains("unterminated single quote"),
        "stderr missing quote-error diagnostic: {stderr:?}"
    );
}

#[test]
fn set_n_with_set_u_unset_var_still_terminates() {
    // The expansion-layer `set -u` short-circuit fires
    // BEFORE the noexec dispatch check — so under `set -nu`
    // a reference to an unset var still writes the
    // nounset diagnostic and terminates with Exit(1).
    // Pins the precedence: expansion errors trump noexec.
    // POSIX-aligned because the whole purpose of `set -n`
    // is to surface syntax errors at parse-and-tokenise
    // time; expansion-layer errors that the parser CAN
    // detect must still surface. Note: v1's `set` builtin
    // doesn't recognise clustered short flags (`set -nu`
    // would be parsed as one invalid option), so we issue
    // the two flags on separate lines.
    let (status, _stdout, stderr, _env, _flags) = drive("set -u\nset -n\necho $UNSET\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(
        stderr.contains("parameter not set"),
        "stderr missing nounset diagnostic: {stderr:?}"
    );
}

#[test]
fn set_n_does_not_trace_under_set_x() {
    // `set -nx`: noexec short-circuits BEFORE the trace
    // block (the trace check sits AFTER the noexec
    // continue), so under both flags an `echo hi` line
    // produces NEITHER the `+ echo hi` trace NOR the `hi`
    // stdout output. POSIX is undefined on the
    // interaction; v1 chooses "no command runs → no trace"
    // matching the existing trace-skip rule for blank
    // lines and expansion errors. Note: separate lines
    // because v1's `set` doesn't cluster flags.
    let (status, stdout, stderr, _env, _flags) = drive("set -n\nset -x\necho hi\n");
    assert_eq!(status, ExitStatus::Eof);
    assert!(
        !stdout.contains("hi"),
        "echo under set -nx must NOT emit output: {stdout:?}"
    );
    assert!(
        !stderr.contains("+ echo hi"),
        "set -nx must NOT trace skipped commands: {stderr:?}"
    );
}

#[test]
fn set_n_blank_line_does_not_error() {
    // Blank lines short-circuit BEFORE the noexec check
    // (the `parts.is_empty()` continue runs above the
    // noexec block). So a blank line under `set -n` is a
    // no-op and the REPL stays alive. Pins that blank
    // lines don't accidentally trigger any error path
    // when noexec is on. Sanity test — without this the
    // existing blank-line continue arm could regress.
    let (status, stdout, _stderr, _env, _flags) = drive("set -n\n\necho hi\n");
    assert_eq!(status, ExitStatus::Eof);
    assert!(
        !stdout.contains("hi"),
        "echo under set -n must NOT emit output: {stdout:?}"
    );
}

#[test]
fn set_n_unknown_command_does_not_warn() {
    // Pins that the "command not found" path is also
    // short-circuited under noexec. Without the
    // short-circuit, an unknown command would write
    // `sh: command not found: <name>\n` to stderr and
    // set last_status=127. Under `set -n` the dispatch
    // never runs, so neither the diagnostic nor the
    // status mutation happens. This is the
    // syntax-check-not-a-runner property: `set -n`
    // catches PARSE errors but does NOT catch
    // missing-command errors (those are a runtime
    // concern, not a syntax concern).
    let (status, _stdout, stderr, _env, _flags) = drive("set -n\nno_such_command_here\n");
    assert_eq!(status, ExitStatus::Eof);
    assert!(
        !stderr.contains("command not found"),
        "unknown command under set -n must NOT warn: {stderr:?}"
    );
}

#[test]
fn set_n_exit_is_silently_skipped() {
    // POSIX `set -n` mode terminates only on EOF, not on
    // `exit`. The `exit` command is silently a no-op
    // (because dispatch is skipped). So `set -n\nexit\n`
    // followed by EOF terminates via EOF (exit code 0),
    // NOT via the `exit` command. Pins that `exit` does
    // NOT get an exception alongside `set` — the only
    // exempt builtin is `set` itself.
    let (status, _stdout, _stderr, _env, _flags) = drive("set -n\nexit 5\n");
    assert_eq!(
        status,
        ExitStatus::Eof,
        "exit under set -n must be skipped; EOF terminates the script"
    );
}
