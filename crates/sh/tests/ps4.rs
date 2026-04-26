//! `sh::run` REPL driver tests for customisable `PS4`
//! trace prefix (T142 follow-up to `set -x`).
//!
//! POSIX `PS4` is the env variable holding the trace prefix
//! used by `set -x` to mark each command. The default is
//! `"+ "` (which is what the original `set -x` slice
//! hardcoded). This slice reads `PS4` from the env map AT
//! TRACE TIME (NOT cached at trace-enable time) so
//! `export PS4="++ "; set -x; ...` shows the new prefix on
//! the next command immediately.
//!
//! These tests drive `run_with_env` directly so they can
//! observe the post-loop env map AND pin the per-line
//! temporal ordering between PS4 mutations and trace fires.
//!
//! Subtle semantic pin: PS4 is written VERBATIM with NO
//! recursive expansion in v1. POSIX bash DOES expand `$VAR`
//! references inside PS4, but that requires a recursion
//! guard to prevent `PS4="$PS4 "` from looping infinitely.
//! v1 keeps it literal — `ps4_does_not_recursively_expand`
//! pins this so a future slice that adds expansion has an
//! existing test to update rather than blindly enabling
//! recursion.

use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};

use sh::{run_with_env, ExitStatus, ShellFlags};

/// Drive `run_with_env` with a byte-string stdin, a fresh
/// empty env map, and a fresh default `ShellFlags`; return
/// `(status, stdout, stderr, env, flags)` for assertion.
/// The returned env lets a test inspect post-loop PS4
/// mutations (e.g. did `export PS4=>>>` actually set the
/// entry).
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
fn ps4_default_is_plus_space() {
    // Sanity: with no PS4 entry in the env map, the trace
    // prefix is the POSIX default `"+ "` — the same
    // behaviour the original `set -x` slice (e83aa05)
    // hardcoded. Pins that this slice did not regress the
    // default. Without this pin a buggy implementation that
    // returned `"+"` (no trailing space) or `""` (nothing
    // at all) for the unset case could slip through.
    let (status, _stdout, stderr, _env, _flags) = drive("set -x\necho hi\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("+ echo hi"),
        "stderr missing default-PS4 trace: {stderr:?}"
    );
}

#[test]
fn ps4_custom_prefix_is_used() {
    // `export PS4=>>>` then `set -x` then `echo hi` →
    // stderr contains `>>>echo hi` (note: NO space between
    // `>>>` and `echo` — the user explicitly chose a
    // prefix without a trailing space, and PS4 is verbatim
    // so the shell must NOT add a space). Pins that the
    // env entry IS consulted at trace time.
    let (status, _stdout, stderr, _env, _flags) =
        drive("export PS4=>>>\nset -x\necho hi\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains(">>>echo hi"),
        "stderr missing custom PS4 trace: {stderr:?}"
    );
}

#[test]
fn ps4_custom_with_trailing_space() {
    // `export PS4='>>> '` (note the trailing space inside
    // the single quotes) → `>>> echo hi`. Pins that the
    // user controls EVERY BYTE of the prefix including its
    // trailing whitespace; the shell does NOT inject any
    // implicit separator between PS4 and the command. The
    // single-quote form is required to preserve the
    // trailing space (without quotes the tokeniser would
    // strip the trailing whitespace before assignment).
    let (status, _stdout, stderr, _env, _flags) =
        drive("export PS4='>>> '\nset -x\necho hi\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains(">>> echo hi"),
        "stderr missing custom PS4 with trailing space: {stderr:?}"
    );
}

#[test]
fn ps4_change_takes_effect_for_next_command() {
    // Temporal ordering pin: PS4 is read AT TRACE TIME,
    // not cached when xtrace was enabled. So the sequence
    // `set -x\nexport PS4=>>\necho hi\n` produces:
    //   - the `set -x` line itself does NOT trace (xtrace
    //     is still off at its trace point — same edge as
    //     `set_x_alone_does_not_trace_itself`)
    //   - the `export PS4=>>` line traces with the DEFAULT
    //     `"+ "` (PS4 was unset when its trace fired —
    //     this dispatch is what mutates the env entry)
    //   - the `echo hi` line traces with `>>` (PS4 was
    //     set by the prior export, and is read fresh at
    //     this trace point)
    // Pins that PS4 changes propagate immediately to the
    // NEXT command's trace, not the same command's trace.
    let (status, _stdout, stderr, _env, _flags) =
        drive("set -x\nexport PS4=>>\necho hi\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("+ export PS4=>>"),
        "export line should trace with DEFAULT prefix (PS4 not yet set at its trace point): {stderr:?}"
    );
    assert!(
        stderr.contains(">>echo hi"),
        "echo line should trace with NEW prefix (PS4 set by prior export): {stderr:?}"
    );
}

#[test]
fn ps4_empty_string_produces_bare_command() {
    // `export PS4=` (empty value) → trace lines show ONLY
    // the command, no prefix at all. Edge case: an empty
    // PS4 IS a meaningful config, not a fallback-to-default
    // trigger. Pins that the resolver checks for
    // entry-existence (env.get returns Some("")), NOT for
    // entry-non-empty. Without this distinction a buggy
    // implementation that defaulted on empty string would
    // make `export PS4=` indistinguishable from "PS4 unset"
    // and the user would be unable to suppress the prefix.
    let (status, _stdout, stderr, _env, _flags) =
        drive("export PS4=\nset -x\necho hi\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    // The trace line should be exactly `echo hi\n` — the
    // empty PS4 produces no prefix bytes, so the command
    // appears bare on stderr.
    assert!(
        stderr.contains("echo hi\n"),
        "stderr missing bare-command trace: {stderr:?}"
    );
    // Negative pin: the default `+ ` MUST NOT appear (the
    // empty-string case must not fall through to default).
    assert!(
        !stderr.contains("+ echo hi"),
        "empty PS4 should NOT fall back to default prefix: {stderr:?}"
    );
}

#[test]
fn ps4_with_special_chars_is_verbatim() {
    // `export PS4='[trace] '` → `[trace] echo hi`. Pins
    // that PS4 is byte-verbatim with NO escape
    // interpretation: brackets, backslashes, and other
    // special chars all pass through. Distinguishes this
    // implementation from one that might (incorrectly) try
    // to interpret PS4 as a format string with escape
    // sequences like `\n` or `\t`.
    let (status, _stdout, stderr, _env, _flags) =
        drive("export PS4='[trace] '\nset -x\necho hi\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("[trace] echo hi"),
        "stderr missing bracketed PS4 trace: {stderr:?}"
    );
}

#[test]
fn ps4_unset_falls_back_to_default() {
    // Sanity duplicate of `ps4_default_is_plus_space` but
    // with a different command sequence to prove the
    // default-fallback fires for ANY command, not just
    // `echo hi`. Without this redundancy a regression that
    // accidentally echo-special-cased the default could
    // pass the first test.
    let (status, _stdout, stderr, _env, _flags) =
        drive("set -x\nexport X=val\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("+ export X=val"),
        "stderr missing default-PS4 trace for export: {stderr:?}"
    );
    assert!(
        stderr.contains("+ exit"),
        "stderr missing default-PS4 trace for exit: {stderr:?}"
    );
}

#[test]
fn ps4_does_not_recursively_expand() {
    // `export PS4='$X '` then `export X=hello` then
    // `set -x` then `echo hi` → trace contains `$X echo hi`
    // (LITERAL `$X`, NOT `hello echo hi`). Pins the
    // deferred-expansion semantic: PS4 is NOT recursively
    // expanded in v1. POSIX bash DOES expand `$VAR`
    // references inside PS4 — a future slice could match
    // that, but it would require a recursion guard to
    // prevent `PS4="$PS4 "` from looping infinitely.
    // Without this test a future slice that adds expansion
    // could land silently and break scripts that relied on
    // the literal-PS4 behaviour.
    let (status, _stdout, stderr, _env, _flags) =
        drive("export PS4='$X '\nexport X=hello\nset -x\necho hi\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("$X echo hi"),
        "stderr missing literal-$X PS4 trace: {stderr:?}"
    );
    assert!(
        !stderr.contains("hello echo hi"),
        "PS4 must NOT recursively expand $X in v1: {stderr:?}"
    );
}
