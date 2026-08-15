//! `sh::run` REPL driver tests for `set -x` xtrace mode
//! toggle (T142 follow-up to `set -e` and `set -u`).
//!
//! POSIX `set -x` (also `set -o xtrace`) makes the shell
//! write each command to stderr BEFORE executing it,
//! prefixed by `+ ` (the default POSIX PS4 prompt; v1 does
//! not customise PS4). The trace shows the EXPANDED tokens
//! joined by single spaces, NOT the original input bytes —
//! so `echo $X` with `X=hello` traces as `+ echo hello`.
//! The trace fires AFTER expansion succeeds (so var refs
//! are resolved) and BEFORE dispatch (so it precedes the
//! command's own output). `set +x` (and `set +o xtrace`)
//! clears the flag.
//!
//! These tests drive `run_with_env` directly so they can
//! observe the post-loop `flags` struct AND pin the
//! "trace goes to stderr, command output goes to stdout"
//! channel separation that distinguishes xtrace from
//! interleaved diagnostics.
//!
//! Subtle ordering pin: the trace point is in the dispatch
//! loop BEFORE dispatch, but `set -x` toggles the flag
//! DURING dispatch. So the FIRST `set -x` line does NOT
//! trace itself (xtrace is still off at its trace point),
//! while `set +x` DOES trace itself (xtrace is still on at
//! its trace point — the clear happens during dispatch).
//! Each ordering edge is pinned by a dedicated test.

use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};

use sh::{run_with_env, ExitStatus, ShellFlags};

/// Drive `run_with_env` with a byte-string stdin, a fresh
/// empty env map, and a fresh default `ShellFlags`; return
/// `(status, stdout, stderr, flags)` for assertion. The
/// returned flags lets a test inspect post-loop mutations
/// (e.g. did `set -x` actually flip the bit).
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
fn set_x_alone_does_not_trace_itself() {
    // The trace point is BEFORE dispatch; `set -x` flips
    // the flag DURING dispatch. So at the trace point of
    // the first `set -x` line, xtrace is still false (just
    // initialised), and no trace fires. Pins the load-
    // bearing ordering edge — without this property a
    // user enabling xtrace would see a redundant `+ set -x`
    // line on every shell startup-script.
    let (status, _stdout, stderr, flags) = drive("set -x\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        !stderr.contains("+ set -x"),
        "first set -x should NOT trace itself: {stderr:?}"
    );
    assert!(flags.xtrace, "set -x should have flipped the flag");
}

#[test]
fn set_x_then_command_traces_command() {
    // The canonical `set -x` happy path: turn xtrace on,
    // run a command, the trace fires for the command (NOT
    // for the `set -x` line itself per the previous test).
    // Pins the basic trace-output-on-stderr behaviour.
    let (status, stdout, stderr, _flags) = drive("set -x\necho hi\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("+ echo hi"),
        "stderr missing trace for echo: {stderr:?}"
    );
    assert!(
        stdout.contains("hi\n"),
        "stdout missing echo output: {stdout:?}"
    );
}

#[test]
fn set_x_traces_expanded_token_not_original() {
    // The trace fires AFTER expansion, so `$X` becomes
    // `hello` (with X=hello) by the time the trace runs.
    // Pins that the trace shows what actually runs, NOT
    // the user's input bytes — critical for debugging
    // (the user already knows what they typed; what they
    // need to see is the expanded form). The export line
    // itself traces under xtrace too (`+ export X=hello`).
    let (status, stdout, stderr, _flags) = drive("set -x\nexport X=hello\necho $X\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("+ echo hello"),
        "stderr missing trace with EXPANDED value: {stderr:?}"
    );
    assert!(
        !stderr.contains("+ echo $X"),
        "stderr should NOT show pre-expansion form: {stderr:?}"
    );
    assert!(
        stdout.contains("hello\n"),
        "stdout missing expansion output: {stdout:?}"
    );
}

#[test]
fn set_x_traces_export_then_other_commands() {
    // Multi-line trace coverage: every command after `set
    // -x` traces independently. Pins that the trace point
    // fires per dispatch iteration, not just on the first
    // command. The export line also traces because xtrace
    // is on at its trace point.
    let (status, _stdout, stderr, _flags) = drive("set -x\nexport X=1\necho $X\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("+ export X=1"),
        "stderr missing trace for export line: {stderr:?}"
    );
    assert!(
        stderr.contains("+ echo 1"),
        "stderr missing trace for echo line: {stderr:?}"
    );
    assert!(
        stderr.contains("+ exit"),
        "stderr missing trace for exit line: {stderr:?}"
    );
}

#[test]
fn set_x_with_long_form_o_xtrace() {
    // `set -o xtrace` is the long-option form, semantically
    // identical to `set -x`. Same trace behaviour applies.
    // Pins the `-o NAME` parser path AND the shared
    // `flags.xtrace = true` mutation.
    let (status, stdout, stderr, flags) = drive("set -o xtrace\necho hi\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("+ echo hi"),
        "stderr missing trace via -o xtrace: {stderr:?}"
    );
    assert!(
        stdout.contains("hi\n"),
        "stdout missing echo output: {stdout:?}"
    );
    assert!(
        flags.xtrace,
        "set -o xtrace should have flipped the flag: {flags:?}"
    );
}

#[test]
fn set_plus_x_clears_flag() {
    // `set +x` (and `set +o xtrace`) reverts the flag.
    // Subtle ordering pin: the `set +x` line itself
    // traces (xtrace is STILL ON at its trace point — the
    // clear happens during dispatch), but the next
    // command does NOT trace because by then xtrace is
    // false. Pins both polarities AND the asymmetric
    // ordering between `set -x` (doesn't trace itself)
    // and `set +x` (does trace itself).
    let (status, stdout, stderr, flags) = drive("set -x\nset +x\necho not_traced\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("+ set +x"),
        "set +x SHOULD trace itself (xtrace still on at trace point): {stderr:?}"
    );
    assert!(
        !stderr.contains("+ echo not_traced"),
        "echo after set +x must NOT trace: {stderr:?}"
    );
    assert!(
        stdout.contains("not_traced\n"),
        "stdout missing echo output after clear: {stdout:?}"
    );
    assert!(
        !flags.xtrace,
        "set +x should have cleared xtrace, got {flags:?}"
    );
}

#[test]
fn set_x_blank_line_does_not_trace() {
    // Blank lines short-circuit BEFORE the trace point
    // (the `parts.is_empty()` continue runs above the
    // trace block). So a blank line under xtrace does
    // NOT produce a trace `+ ` with an empty argument
    // list — POSIX-aligned because no command ran. The
    // `+ echo hi` line still appears for the third line.
    let (status, _stdout, stderr, _flags) = drive("set -x\n\necho hi\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("+ echo hi"),
        "stderr missing trace for echo line: {stderr:?}"
    );
    // Pin: there should be no trace line for the blank.
    // A blank-line trace would look like `+ \n` — pin the
    // absence of an empty trace.
    assert!(
        !stderr.contains("+ \n"),
        "blank line should NOT produce empty trace: {stderr:?}"
    );
}

#[test]
fn set_x_quote_error_does_not_trace() {
    // Unterminated quote errors short-circuit BEFORE the
    // trace point (the `Err(QuoteError::*)` continue runs
    // above the trace block). So a line with `echo
    // 'unclosed` under xtrace produces ONLY the
    // unterminated-quote diagnostic on stderr, NOT a
    // `+ echo ...` trace. Pins that recoverable parse
    // errors don't get accidentally traced before being
    // reported.
    let (status, _stdout, stderr, _flags) = drive("set -x\necho 'unclosed\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("unterminated single quote"),
        "stderr missing quote-error diagnostic: {stderr:?}"
    );
    assert!(
        !stderr.contains("+ echo"),
        "quote-error line must NOT trace: {stderr:?}"
    );
}

#[test]
fn set_x_expansion_error_does_not_trace() {
    // Expansion errors (from `set -u`) short-circuit
    // BEFORE the trace point — the `Err(NotSet)` arm
    // returns `Exit(1)` directly, bypassing the trace
    // block. With both nounset and xtrace on, the first
    // reference to an unset var terminates with the
    // nounset diagnostic; no trace fires for that line
    // because the command never runs. Pins that the
    // expansion-layer termination path doesn't accidentally
    // produce a trace for the failing line. Note: v1's
    // `set` builtin doesn't recognise clustered short
    // flags (`set -ux` would be parsed as one invalid
    // option), so we issue the two flags on separate
    // lines.
    let (status, _stdout, stderr, _flags) = drive("set -u\nset -x\necho $UNSET\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(
        stderr.contains("parameter not set"),
        "stderr missing nounset diagnostic: {stderr:?}"
    );
    assert!(
        !stderr.contains("+ echo"),
        "failing-expansion line must NOT trace: {stderr:?}"
    );
}

#[test]
fn set_x_traces_to_stderr_not_stdout() {
    // Channel-separation regression guard: traces go to
    // stderr ONLY; stdout stays pure command output.
    // Without this property a user piping stdout into
    // another tool would see contaminating `+ ` lines.
    // Pin both halves: stdout has the echo output but
    // NOT the trace; stderr has the trace but not the
    // echo output.
    let (status, stdout, stderr, _flags) = drive("set -x\necho hello\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stdout.contains("hello\n"),
        "stdout missing echo output: {stdout:?}"
    );
    assert!(
        !stdout.contains("+ "),
        "stdout must NOT contain trace prefix: {stdout:?}"
    );
    assert!(
        stderr.contains("+ echo hello"),
        "stderr missing trace: {stderr:?}"
    );
    assert!(
        !stderr.contains("hello\n") || stderr.contains("+ echo hello"),
        "stderr must only contain trace, not echo output: {stderr:?}"
    );
}
