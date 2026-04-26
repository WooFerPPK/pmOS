//! `sh::run` REPL driver tests for the POSIX `test` and `[`
//! conditional expression evaluator builtins (T144 follow-up).
//!
//! `test` and `[` are two dispatch arms over the SAME
//! evaluator. The `[` form requires its LAST arg to be `]`
//! (else `Status(2)` "missing ]"); `test` MUST NOT have a
//! trailing `]` (else `]` becomes a regular operand and the
//! expression usually fails operator lookup). After the `]`
//! handling, the evaluator runs the same arg-arity matrix
//! for both: 0 → false, 1 → non-empty test, 2 → unary or
//! `! EXPR`, 3 → binary or `! EXPR`, 4+ → usage error
//! (except a leading `!` peels one arg off the front).
//!
//! v1 ships the FOUNDATIONAL POSIX subset: string ops `-z`
//! / `-n` / `=` / `!=`, integer ops `-eq` / `-ne` / `-lt` /
//! `-le` / `-gt` / `-ge`, and prefix negation `!`. File-test
//! operators (`-e`, `-f`, `-d`, etc.), terminal-test (`-t`),
//! compound `-a` / `-o`, and bash-extended operators (`==`,
//! `=~`, `[[ ... ]]`) are all DEFERRED — but unrecognised
//! operators surface as `unknown unary operator` / `unknown
//! binary operator` rather than silently failing, so users
//! get a clear "not yet implemented" signal.
//!
//! Tests drive `run_with_env` end-to-end and cover the arity
//! matrix, both `[` and `test` invocations, every operator
//! category, the negation rule (boolean inverted, usage
//! error propagated), and the diagnostic shapes.

use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};

use sh::{run_with_env, ExitStatus, ShellFlags};

/// Drive `run_with_env` with a byte-string stdin and a
/// pre-seeded env map; return `(status, stdout, stderr,
/// env)` for assertion. Constructs a fresh default
/// `ShellFlags` per call (errexit off) — the `Status(2)`
/// usage errors in some of these tests would terminate the
/// REPL under errexit, so leaving it cleared keeps multi-line
/// scripts running through to the trailing `exit`.
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

// ---------- Bracket form basics ----------

#[test]
fn bracket_no_args_returns_one() {
    // `[ ]` is the POSIX-defined zero-arg empty test — it
    // evaluates to false (Status(1)). The bracket strip
    // peels off the trailing `]`, leaving zero inner args;
    // the arity matrix's 0-arg branch produces Status(1).
    // `echo $?` then prints `1`.
    let (status, stdout, stderr, _env) = drive("[ ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-empty-bracket status 1: {stdout:?}"
    );
}

#[test]
fn bracket_missing_close_bracket_returns_two_with_diagnostic() {
    // `[ -z foo` (no trailing `]`) is a syntax error: the
    // `[` form requires `]` as its last arg. Status(2) plus
    // `[: missing ]` to stderr. The REPL stays alive so the
    // trailing `exit` still runs.
    let (status, _stdout, stderr, _env) = drive("[ -z foo\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("missing ]"),
        "stderr missing missing-bracket diagnostic: {stderr:?}"
    );
}

#[test]
fn bracket_one_arg_nonempty_returns_zero() {
    // Single non-empty operand evaluates as the implicit
    // `-n STR` (POSIX-defined shorthand). Status(0) → `$?`
    // is `0`.
    let (status, stdout, stderr, _env) = drive("[ foo ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-nonempty status 0: {stdout:?}"
    );
}

#[test]
fn bracket_one_arg_empty_returns_one() {
    // Single empty operand (single-quoted empty string)
    // evaluates as `-n ''` → Status(1). Pins that the
    // single-arg branch correctly tests for non-emptiness
    // and not just "is there an arg" (the tokenizer
    // produces a zero-length token from `''`).
    let (status, stdout, stderr, _env) = drive("[ '' ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-empty status 1: {stdout:?}"
    );
}

// ---------- String operators ----------

#[test]
fn test_dash_z_empty_returns_zero() {
    // `-z` is "true if STR is empty". `-z ''` → Status(0).
    let (status, stdout, stderr, _env) = drive("test -z ''\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-z empty) status 0: {stdout:?}"
    );
}

#[test]
fn test_dash_z_nonempty_returns_one() {
    // `-z foo` → Status(1).
    let (status, stdout, stderr, _env) = drive("test -z foo\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-z nonempty) status 1: {stdout:?}"
    );
}

#[test]
fn test_dash_n_nonempty_returns_zero() {
    // `-n` is "true if STR is non-empty". `-n foo` →
    // Status(0).
    let (status, stdout, stderr, _env) = drive("test -n foo\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-n nonempty) status 0: {stdout:?}"
    );
}

#[test]
fn test_dash_n_empty_returns_one() {
    // `-n ''` → Status(1). The pair `-z` / `-n` are
    // boolean-inverses of each other; this test plus
    // `test_dash_z_empty_returns_zero` pins that.
    let (status, stdout, stderr, _env) = drive("test -n ''\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-n empty) status 1: {stdout:?}"
    );
}

#[test]
fn bracket_string_equal_returns_zero() {
    // `STR1 = STR2` is true when byte-equal. POSIX `=`,
    // NOT bash's `==` (which is deferred).
    let (status, stdout, stderr, _env) = drive("[ foo = foo ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(=) equal status 0: {stdout:?}"
    );
}

#[test]
fn bracket_string_equal_unequal_returns_one() {
    // `[ foo = bar ]` → Status(1) (strings differ).
    let (status, stdout, stderr, _env) = drive("[ foo = bar ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(=) unequal status 1: {stdout:?}"
    );
}

#[test]
fn bracket_string_not_equal_returns_zero() {
    // `STR1 != STR2` is true when strings differ.
    let (status, stdout, stderr, _env) =
        drive("[ foo != bar ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(!=) status 0: {stdout:?}"
    );
}

// ---------- Integer operators ----------

#[test]
fn bracket_eq_equal_returns_zero() {
    // `5 -eq 5` → Status(0). Pins the integer-parse + eq
    // branch.
    let (status, stdout, stderr, _env) = drive("[ 5 -eq 5 ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-eq) equal status 0: {stdout:?}"
    );
}

#[test]
fn bracket_eq_unequal_returns_one() {
    // `5 -eq 6` → Status(1).
    let (status, stdout, stderr, _env) = drive("[ 5 -eq 6 ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-eq) unequal status 1: {stdout:?}"
    );
}

#[test]
fn bracket_lt_works() {
    // `3 -lt 5` → Status(0); `5 -lt 3` → Status(1). Two
    // shells in one stdin, two `echo $?` to pin both.
    let (status, stdout, stderr, _env) = drive(
        "[ 3 -lt 5 ]\necho $?\n[ 5 -lt 3 ]\necho $?\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    let zero_pos = stdout.find("0\n").expect("missing first 0 status");
    let one_pos = stdout.find("1\n").expect("missing second 1 status");
    assert!(
        zero_pos < one_pos,
        "expected `0` before `1` in {stdout:?}"
    );
}

#[test]
fn bracket_gt_works() {
    // `5 -gt 3` → Status(0).
    let (status, stdout, stderr, _env) = drive("[ 5 -gt 3 ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-gt) status 0: {stdout:?}"
    );
}

#[test]
fn bracket_le_works() {
    // `5 -le 5` → Status(0) (equal counts as <=).
    let (status, stdout, stderr, _env) = drive("[ 5 -le 5 ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-le) status 0: {stdout:?}"
    );
}

#[test]
fn bracket_ge_works() {
    // `5 -ge 5` → Status(0).
    let (status, stdout, stderr, _env) = drive("[ 5 -ge 5 ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-ge) status 0: {stdout:?}"
    );
}

#[test]
fn bracket_ne_works() {
    // `5 -ne 6` → Status(0).
    let (status, stdout, stderr, _env) = drive("[ 5 -ne 6 ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-ne) status 0: {stdout:?}"
    );
}

#[test]
fn bracket_eq_with_non_integer_returns_two_with_diagnostic() {
    // `[ foo -eq 5 ]` → Status(2) usage error because `foo`
    // doesn't parse as `i64`. Diagnostic shape: `[: -eq:
    // integer expression expected: foo`.
    let (status, _stdout, stderr, _env) =
        drive("[ foo -eq 5 ]\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("integer expression expected: foo"),
        "stderr missing integer-expected diagnostic: {stderr:?}"
    );
}

// ---------- Negation ----------

#[test]
fn bracket_negate_true_returns_one() {
    // `! foo` is `! (single-arg non-empty test)` → invert
    // Status(0) into Status(1).
    let (status, stdout, stderr, _env) = drive("[ ! foo ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(! foo) inverted status 1: {stdout:?}"
    );
}

#[test]
fn bracket_negate_false_returns_zero() {
    // `! ''` is `! (single-arg empty test)` → invert
    // Status(1) into Status(0).
    let (status, stdout, stderr, _env) = drive("[ ! '' ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(! '') inverted status 0: {stdout:?}"
    );
}

#[test]
fn bracket_negate_propagates_usage_error_status() {
    // `[ ! foo -eq bar ]` is `! (foo -eq bar)`. The inner
    // `foo -eq bar` produces Status(2) (foo not integer);
    // negation does NOT invert Status(2) into anything else.
    // The outer expression remains Status(2). Pins POSIX
    // semantics where syntax errors are not boolean values.
    let (status, _stdout, stderr, _env) =
        drive("[ ! foo -eq bar ]\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("integer expression expected"),
        "stderr missing inner integer-expected diagnostic: {stderr:?}"
    );
}

// ---------- `test` (no bracket) variants ----------

#[test]
fn test_command_works_without_close_bracket() {
    // `test foo = foo` — no trailing `]` because this is
    // the `test` form, not `[`. Should evaluate exactly the
    // same as `[ foo = foo ]` → Status(0).
    let (status, stdout, stderr, _env) =
        drive("test foo = foo\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(test ... =) status 0: {stdout:?}"
    );
}

#[test]
fn test_command_with_close_bracket_treated_as_operand() {
    // `test foo ]` — the `]` is NOT stripped by the test
    // form, so the evaluator sees 2 args (`foo`, `]`).
    // That's the unary form, but `foo` isn't a known unary
    // operator → `unknown unary operator: foo`. Status(2).
    let (status, _stdout, stderr, _env) = drive("test foo ]\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("unknown unary operator: foo"),
        "stderr missing unknown-unary diagnostic: {stderr:?}"
    );
}

// ---------- Error paths ----------

#[test]
fn bracket_too_many_arguments() {
    // `[ a b c d ]` strips `]`, leaves 4 args. The 4-arg
    // branch only matches a leading `!`; without it, this
    // is "too many arguments" → Status(2) with the
    // `[: too many arguments` diagnostic.
    let (status, _stdout, stderr, _env) =
        drive("[ a b c d ]\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("too many arguments"),
        "stderr missing too-many-arguments diagnostic: {stderr:?}"
    );
}

#[test]
fn bracket_unknown_unary_operator() {
    // `[ -X foo ]` strips `]`, leaves 2 args (`-X`, `foo`).
    // `-X` isn't `-z` or `-n` → Status(2) with the
    // `unknown unary operator: -X` diagnostic. Pins that
    // unrecognised operators (deferred file-test ops, etc.)
    // surface a clear "not implemented" message rather than
    // silently failing.
    let (status, _stdout, stderr, _env) =
        drive("[ -X foo ]\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("unknown unary operator: -X"),
        "stderr missing unknown-unary diagnostic: {stderr:?}"
    );
}

#[test]
fn bracket_unknown_binary_operator() {
    // `[ foo -X bar ]` strips `]`, leaves 3 args. `-X`
    // isn't a known binary op → Status(2) with the
    // `unknown binary operator: -X` diagnostic. Same
    // "deferred-op signal" property as the unary case.
    let (status, _stdout, stderr, _env) =
        drive("[ foo -X bar ]\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("unknown binary operator: -X"),
        "stderr missing unknown-binary diagnostic: {stderr:?}"
    );
}
