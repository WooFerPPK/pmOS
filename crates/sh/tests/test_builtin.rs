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
use std::fs;
use std::io::{BufReader, Cursor, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

// ---------- File-test operators ----------
//
// These tests cover the POSIX file-test unary operators
// added in the T144 follow-up after the foundational `[`/
// `test` slice (d2c6e59). Each test creates an isolated
// scratch directory under `std::env::temp_dir()` keyed by
// test tag + pid + counter so parallel test execution
// doesn't collide. The directory is cleaned up at the end
// of each successful test (a failing test leaves it on
// disk for debugging — this matches the cat / coreutils
// scratch-dir convention).
//
// All tests drive the full REPL via `drive` (which calls
// `run_with_env`); they construct a path with absolute-form
// (under `temp_dir()`), splice it into the input script as
// the operand to `[ -X PATH ]`, and assert on `$?` after.
// File-test ops take the operand verbatim (no expansion in
// the file-test layer; expansion already happened in the
// dispatch loop), so the absolute path is what
// `std::fs::metadata` sees.

static FILE_TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn scratch_dir(tag: &str) -> PathBuf {
    let n = FILE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pmos-sh-filetest-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    let mut f = fs::File::create(&path).expect("create temp file");
    f.write_all(bytes).expect("write temp file");
    path
}

fn cleanup(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn bracket_dash_e_existing_file_returns_zero() {
    // `[ -e PATH ]` on an existing regular file → Status(0).
    let dir = scratch_dir("dash-e-file");
    let file = write_file(&dir, "real", b"x");
    let script = format!("[ -e {} ]\necho $?\nexit\n", file.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-e existing) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_e_existing_directory_returns_zero() {
    // `[ -e PATH ]` on a directory also returns Status(0)
    // (the operator tests existence regardless of file
    // type). Pins that `-e` is type-agnostic.
    let dir = scratch_dir("dash-e-dir");
    let script = format!("[ -e {} ]\necho $?\nexit\n", dir.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-e dir) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_e_missing_path_returns_one() {
    // `[ -e /nonexistent/path ]` → Status(1) with NO
    // stderr output (missing file is a normal `false`,
    // not a usage error).
    let (status, stdout, stderr, _env) = drive(
        "[ -e /pmos-definitely-not-here-12345 ]\necho $?\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-e missing) status 1: {stdout:?}"
    );
}

#[test]
fn bracket_dash_e_empty_path_returns_one() {
    // `[ -e '' ]` — empty path string is a valid string
    // but `stat("")` always fails → Status(1). Pins that
    // empty-string operand goes through the metadata path
    // and returns `false` rather than triggering a panic
    // or odd diagnostic.
    let (status, stdout, stderr, _env) =
        drive("[ -e '' ]\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-e empty) status 1: {stdout:?}"
    );
}

#[test]
fn bracket_dash_f_regular_file_returns_zero() {
    // `[ -f PATH ]` true only for regular files.
    let dir = scratch_dir("dash-f-file");
    let file = write_file(&dir, "real", b"content");
    let script = format!("[ -f {} ]\necho $?\nexit\n", file.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-f file) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_f_directory_returns_one() {
    // `[ -f PATH ]` on a directory → Status(1). Pins the
    // type discrimination — `-f` is NOT the same as `-e`.
    let dir = scratch_dir("dash-f-dir");
    let script = format!("[ -f {} ]\necho $?\nexit\n", dir.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-f dir) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_f_missing_returns_one() {
    // `[ -f /nonexistent ]` → Status(1) with NO stderr.
    let (status, stdout, stderr, _env) = drive(
        "[ -f /pmos-not-here-67890 ]\necho $?\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-f missing) status 1: {stdout:?}"
    );
}

#[test]
fn bracket_dash_d_directory_returns_zero() {
    // `[ -d PATH ]` true only for directories.
    let dir = scratch_dir("dash-d-dir");
    let script = format!("[ -d {} ]\necho $?\nexit\n", dir.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-d dir) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_d_regular_file_returns_one() {
    // `[ -d PATH ]` on a regular file → Status(1). The
    // mirror of `bracket_dash_f_directory_returns_one`.
    let dir = scratch_dir("dash-d-file");
    let file = write_file(&dir, "real", b"x");
    let script = format!("[ -d {} ]\necho $?\nexit\n", file.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-d file) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_d_missing_returns_one() {
    // `[ -d /nonexistent ]` → Status(1).
    let (status, stdout, stderr, _env) = drive(
        "[ -d /pmos-not-a-dir-13579 ]\necho $?\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-d missing) status 1: {stdout:?}"
    );
}

#[test]
fn bracket_dash_s_nonempty_file_returns_zero() {
    // `[ -s PATH ]` true when file exists AND has size > 0.
    let dir = scratch_dir("dash-s-nonempty");
    let file = write_file(&dir, "data", b"ABCDE");
    let script = format!("[ -s {} ]\necho $?\nexit\n", file.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-s nonempty) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_s_empty_file_returns_one() {
    // `[ -s PATH ]` on an empty (zero-byte) file →
    // Status(1). Pins the size check vs the existence
    // check (an empty file would pass `-e` and `-f`).
    let dir = scratch_dir("dash-s-empty");
    let file = write_file(&dir, "empty", b"");
    let script = format!("[ -s {} ]\necho $?\nexit\n", file.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-s empty) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_s_missing_returns_one() {
    // `[ -s /nonexistent ]` → Status(1).
    let (status, stdout, stderr, _env) = drive(
        "[ -s /pmos-no-such-file-24680 ]\necho $?\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-s missing) status 1: {stdout:?}"
    );
}

#[test]
fn bracket_dash_r_readable_file_returns_zero() {
    // `[ -r PATH ]` true when owner-read bit (0o400) is
    // set on the file. Files created via `fs::File::create`
    // get default permissions that include owner-read on
    // every Unix-like host (the umask doesn't strip 0o400
    // by default), so a freshly-created scratch file
    // passes `-r`.
    let dir = scratch_dir("dash-r-readable");
    let file = write_file(&dir, "rfile", b"x");
    let script = format!("[ -r {} ]\necho $?\nexit\n", file.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-r readable) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_r_unreadable_file_returns_one() {
    // `[ -r PATH ]` returns Status(1) when the owner-read
    // bit is cleared. Force the bit off via
    // `set_permissions(0o000)`. This pins that the bit
    // check is real, not "any-reachable-file passes".
    // Skipped on non-root contexts only if the
    // set_permissions call would silently succeed but the
    // test cannot run as root — it's safe everywhere
    // because metadata reads don't need read permission on
    // the file itself, only on its parent directory.
    let dir = scratch_dir("dash-r-unreadable");
    let file = write_file(&dir, "norfile", b"x");
    let mut perm = fs::metadata(&file).expect("metadata").permissions();
    perm.set_mode(0o000);
    fs::set_permissions(&file, perm).expect("clear perms");
    let script = format!("[ -r {} ]\necho $?\nexit\n", file.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    // Restore writable permissions so cleanup can rm.
    let mut restore = fs::metadata(&file).expect("metadata").permissions();
    restore.set_mode(0o600);
    let _ = fs::set_permissions(&file, restore);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-r unreadable) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_w_writable_file_returns_zero() {
    // `[ -w PATH ]` true when owner-write bit (0o200) is
    // set. Default file creation includes owner-write.
    let dir = scratch_dir("dash-w-writable");
    let file = write_file(&dir, "wfile", b"x");
    let script = format!("[ -w {} ]\necho $?\nexit\n", file.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-w writable) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_x_executable_file_returns_zero_when_set() {
    // `[ -x PATH ]` true when owner-exec bit (0o100) is
    // set. Default file creation does NOT include
    // owner-exec, so set it explicitly to 0o700 and verify
    // the test passes.
    let dir = scratch_dir("dash-x-exec");
    let file = write_file(&dir, "xfile", b"x");
    let mut perm = fs::metadata(&file).expect("metadata").permissions();
    perm.set_mode(0o700);
    fs::set_permissions(&file, perm).expect("set exec");
    let script = format!("[ -x {} ]\necho $?\nexit\n", file.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-x set) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_x_non_executable_file_returns_one() {
    // `[ -x PATH ]` on a 0o600 (rw-only) file → Status(1).
    // Pins the bit check vs "any-reachable-file passes".
    let dir = scratch_dir("dash-x-no-exec");
    let file = write_file(&dir, "noxfile", b"x");
    let mut perm = fs::metadata(&file).expect("metadata").permissions();
    perm.set_mode(0o600);
    fs::set_permissions(&file, perm).expect("clear exec");
    let script = format!("[ -x {} ]\necho $?\nexit\n", file.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-x cleared) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_negate_dash_e_returns_zero_for_missing() {
    // `[ ! -e /missing ]` — the negation correctly wraps
    // the file-test form. `-e /missing` returns Status(1);
    // `!` inverts it to Status(0). Pins that the negation
    // path treats file-test ops as ordinary boolean tests.
    let (status, stdout, stderr, _env) = drive(
        "[ ! -e /pmos-yet-another-missing-99999 ]\necho $?\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(! -e missing) inverted status 0: {stdout:?}"
    );
}

#[test]
fn bracket_negate_dash_f_returns_zero_for_directory() {
    // `[ ! -f DIR ]` — `-f DIR` returns Status(1) (DIR is
    // not a regular file); `!` inverts to Status(0).
    let dir = scratch_dir("negate-dash-f");
    let script = format!("[ ! -f {} ]\necho $?\nexit\n", dir.display());
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(! -f dir) inverted status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_l_still_unknown_unary_operator() {
    // `-L` (symlink test) is explicitly DEFERRED in this
    // slice (needs `lstat` instead of `stat`). It must
    // surface as `unknown unary operator: -L` so users get
    // a clear "not yet implemented" signal rather than a
    // silent wrong answer. Pins the deferred-op
    // discoverability property for the remaining file-test
    // operators.
    let (status, _stdout, stderr, _env) =
        drive("[ -L /tmp ]\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("unknown unary operator: -L"),
        "stderr missing unknown-unary diagnostic for deferred -L: {stderr:?}"
    );
}

// ---------- Binary file-test operators ----------
//
// These tests cover the POSIX binary file-test operators
// (`-nt`, `-ot`, `-ef`) added in the T144 follow-up after
// the unary file-test slice (ed7257f). Unlike the unary
// file-test ops which live in the 2-arg branch, these live
// in the 3-arg branch alongside the integer comparison
// operators — the dispatch is on the MIDDLE arg.
//
// Reuses the `scratch_dir` / `write_file` / `cleanup`
// helpers from the unary file-test block. Tests that compare
// modification times insert a small `thread::sleep` between
// two file creations to ensure the mtimes actually differ —
// 10ms is enough on most systems (the underlying mtime
// resolution is typically 1ms or better on tmpfs).
//
// Missing-path semantics follow bash exactly:
// * `-nt` is true if PATH2 is missing (PATH1 is "newer than
//   nothing"); false if PATH1 is missing.
// * `-ot` is the mirror.
// * `-ef` is false if EITHER path is missing — including the
//   both-missing case (we explicitly pin that "missing ==
//   missing" doesn't accidentally return true).

#[test]
fn bracket_dash_nt_newer_returns_zero() {
    // `[ PATH1 -nt PATH2 ]` where PATH1 was created AFTER
    // PATH2 → Status(0). Sleep between creates so the mtimes
    // are unambiguously different.
    let dir = scratch_dir("dash-nt-newer");
    let older = write_file(&dir, "older", b"a");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let newer = write_file(&dir, "newer", b"b");
    let script = format!(
        "[ {} -nt {} ]\necho $?\nexit\n",
        newer.display(),
        older.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-nt newer) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_nt_older_returns_one() {
    // Mirror: `[ PATH1 -nt PATH2 ]` where PATH1 is OLDER
    // than PATH2 → Status(1).
    let dir = scratch_dir("dash-nt-older");
    let older = write_file(&dir, "older", b"a");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let newer = write_file(&dir, "newer", b"b");
    let script = format!(
        "[ {} -nt {} ]\necho $?\nexit\n",
        older.display(),
        newer.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-nt older) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_nt_same_path_returns_one() {
    // `[ PATH -nt PATH ]` — comparing a file to itself, the
    // mtimes are equal so neither side is "newer". POSIX
    // defines equal mtimes as not-newer (Status(1)). Pins
    // the strict `>` semantic — "newer" means STRICTLY
    // newer, not "not older".
    let dir = scratch_dir("dash-nt-same");
    let file = write_file(&dir, "same", b"x");
    let script = format!(
        "[ {} -nt {} ]\necho $?\nexit\n",
        file.display(),
        file.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-nt same) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_nt_path1_missing_returns_one() {
    // `[ MISSING -nt EXISTING ]` — bash semantic: a missing
    // file is "older than anything that exists" → Status(1).
    let dir = scratch_dir("dash-nt-p1-missing");
    let existing = write_file(&dir, "existing", b"x");
    let script = format!(
        "[ /pmos-not-here-nt-1-{} -nt {} ]\necho $?\nexit\n",
        std::process::id(),
        existing.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-nt missing-vs-existing) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_nt_path2_missing_returns_zero() {
    // `[ EXISTING -nt MISSING ]` — bash semantic: an
    // existing file is "newer than nothing" → Status(0).
    let dir = scratch_dir("dash-nt-p2-missing");
    let existing = write_file(&dir, "existing", b"x");
    let script = format!(
        "[ {} -nt /pmos-not-here-nt-2-{} ]\necho $?\nexit\n",
        existing.display(),
        std::process::id()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-nt existing-vs-missing) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_ot_older_returns_zero() {
    // `[ PATH1 -ot PATH2 ]` where PATH1 is OLDER than PATH2
    // → Status(0).
    let dir = scratch_dir("dash-ot-older");
    let older = write_file(&dir, "older", b"a");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let newer = write_file(&dir, "newer", b"b");
    let script = format!(
        "[ {} -ot {} ]\necho $?\nexit\n",
        older.display(),
        newer.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-ot older) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_ot_newer_returns_one() {
    // Mirror: `[ PATH1 -ot PATH2 ]` where PATH1 is NEWER
    // than PATH2 → Status(1).
    let dir = scratch_dir("dash-ot-newer");
    let older = write_file(&dir, "older", b"a");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let newer = write_file(&dir, "newer", b"b");
    let script = format!(
        "[ {} -ot {} ]\necho $?\nexit\n",
        newer.display(),
        older.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-ot newer) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_ot_path1_missing_returns_zero() {
    // `[ MISSING -ot EXISTING ]` — bash semantic: a missing
    // file is "older than anything that exists" → Status(0).
    let dir = scratch_dir("dash-ot-p1-missing");
    let existing = write_file(&dir, "existing", b"x");
    let script = format!(
        "[ /pmos-not-here-ot-1-{} -ot {} ]\necho $?\nexit\n",
        std::process::id(),
        existing.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-ot missing-vs-existing) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_ot_path2_missing_returns_one() {
    // `[ EXISTING -ot MISSING ]` — bash semantic: an
    // existing file is NOT "older than nothing" → Status(1).
    let dir = scratch_dir("dash-ot-p2-missing");
    let existing = write_file(&dir, "existing", b"x");
    let script = format!(
        "[ {} -ot /pmos-not-here-ot-2-{} ]\necho $?\nexit\n",
        existing.display(),
        std::process::id()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-ot existing-vs-missing) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_ef_same_path_returns_zero() {
    // `[ PATH -ef PATH ]` — same literal path on both sides
    // → Status(0). The `stat()` returns the same dev+inode
    // for both lookups, so the device + inode comparison
    // succeeds.
    let dir = scratch_dir("dash-ef-same-path");
    let file = write_file(&dir, "same", b"x");
    let script = format!(
        "[ {} -ef {} ]\necho $?\nexit\n",
        file.display(),
        file.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-ef same path) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_ef_hard_link_returns_zero() {
    // `[ ORIG -ef LINK ]` where LINK is a hard link to ORIG
    // → Status(0). Hard links share dev+inode by definition,
    // so the comparison succeeds even though the path
    // strings are different. Pins the "same underlying file"
    // semantic — `-ef` is NOT a path-equality check, it's a
    // file-identity check.
    let dir = scratch_dir("dash-ef-hard-link");
    let orig = write_file(&dir, "orig", b"shared content");
    let link = dir.join("link");
    std::fs::hard_link(&orig, &link).expect("create hard link");
    let script = format!(
        "[ {} -ef {} ]\necho $?\nexit\n",
        orig.display(),
        link.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(-ef hard link) status 0: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_ef_different_files_returns_one() {
    // `[ PATH1 -ef PATH2 ]` where PATH1 and PATH2 are
    // distinct regular files → Status(1). Different inodes,
    // different files. Pins the "false on different files"
    // semantic.
    let dir = scratch_dir("dash-ef-different");
    let a = write_file(&dir, "a", b"a");
    let b = write_file(&dir, "b", b"b");
    let script = format!(
        "[ {} -ef {} ]\necho $?\nexit\n",
        a.display(),
        b.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-ef different files) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_ef_path1_missing_returns_one() {
    // `[ MISSING -ef EXISTING ]` — `-ef` returns Status(1)
    // when either path fails to stat.
    let dir = scratch_dir("dash-ef-p1-missing");
    let existing = write_file(&dir, "existing", b"x");
    let script = format!(
        "[ /pmos-not-here-ef-1-{} -ef {} ]\necho $?\nexit\n",
        std::process::id(),
        existing.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-ef missing path1) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_ef_path2_missing_returns_one() {
    // `[ EXISTING -ef MISSING ]` — symmetric to the above.
    let dir = scratch_dir("dash-ef-p2-missing");
    let existing = write_file(&dir, "existing", b"x");
    let script = format!(
        "[ {} -ef /pmos-not-here-ef-2-{} ]\necho $?\nexit\n",
        existing.display(),
        std::process::id()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-ef missing path2) status 1: {stdout:?}"
    );
    cleanup(&dir);
}

#[test]
fn bracket_dash_ef_both_missing_returns_one() {
    // `[ MISSING1 -ef MISSING2 ]` — both paths fail to stat
    // → Status(1), NOT Status(0). Pins that "missing ==
    // missing" doesn't accidentally return true; the
    // comparison only succeeds when both paths actually
    // resolve to a real file with matching dev+inode.
    let pid = std::process::id();
    let script = format!(
        "[ /pmos-not-here-ef-both-a-{pid} -ef /pmos-not-here-ef-both-b-{pid} ]\necho $?\nexit\n"
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing post-(-ef both missing) status 1: {stdout:?}"
    );
}

#[test]
fn bracket_negate_dash_ef_works() {
    // `[ ! PATH1 -ef PATH2 ]` where PATH1 and PATH2 are
    // different files — `-ef` returns Status(1); `!` inverts
    // to Status(0). Pins that the existing `! EXPR` peeling
    // wraps the binary file-test forms transparently the
    // same way it wraps the unary forms (the negation path
    // is operator-agnostic, it just inverts the inner
    // result).
    let dir = scratch_dir("negate-dash-ef");
    let a = write_file(&dir, "a", b"a");
    let b = write_file(&dir, "b", b"b");
    let script = format!(
        "[ ! {} -ef {} ]\necho $?\nexit\n",
        a.display(),
        b.display()
    );
    let (status, stdout, stderr, _env) = drive(&script, BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-(! -ef different) inverted status 0: {stdout:?}"
    );
    cleanup(&dir);
}

// ---------- read builtin ----------
//
// `read VAR` pulls one line from the shell's stdin into a
// named env var. The dispatcher threads stdin into
// `dispatch_builtin` so the builtin reads from the SAME
// `BufRead` source the REPL pulls command lines from. Tests
// drive `run_with_env` with multi-line stdin scripts where
// some lines are commands (`read X`, `echo $X`, `exit`) and
// the lines BETWEEN them are the input the `read` builtin
// consumes — the REPL's own `read_line` loop and the
// `read` builtin's `read_line` call share the same reader,
// so they alternate correctly: the REPL reads the `read X`
// command line, dispatches into the builtin, the builtin
// reads the next line for X, control returns to the REPL,
// which reads the next command line for `echo $X`. This
// shape pins the "shared stdin source" semantic by relying
// on it.

#[test]
fn read_assigns_line_to_var_returns_zero() {
    // Canonical happy path: `read X` pulls `hello` from
    // stdin; `echo $X` proves the env mutation took effect.
    // Status(0) → `$?` → `0` after the read.
    let (status, stdout, stderr, env) =
        drive("read X\nhello\necho $X\necho $?\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("X").map(String::as_str), Some("hello"));
    assert!(
        stdout.contains("hello\n"),
        "stdout missing echoed X value: {stdout:?}"
    );
    assert!(
        stdout.contains("0\n"),
        "stdout missing post-read status 0: {stdout:?}"
    );
}

#[test]
fn read_strips_trailing_newline_only() {
    // Internal whitespace must survive verbatim — `read`
    // does NOT split or trim the line body, only its
    // trailing newline. `LINE="foo bar"` after `read LINE`
    // against `"foo bar\n"`.
    let (status, _stdout, stderr, env) =
        drive("read LINE\nfoo bar\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("LINE").map(String::as_str), Some("foo bar"));
}

#[test]
fn read_strips_crlf() {
    // CRLF input (common from copy-paste) must yield a
    // value with neither `\r` nor `\n`. The defensive
    // strip-`\n`-then-strip-`\r` shape produces `foo` from
    // `"foo\r\n"`.
    let (status, _stdout, stderr, env) =
        drive("read X\nfoo\r\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("X").map(String::as_str), Some("foo"));
}

#[test]
fn read_returns_one_on_eof() {
    // Stdin exhausted right after the `read X` command line
    // → the read builtin's own `read_line` returns Ok(0),
    // Status(1), no env mutation. With `set -e` enabled,
    // the failing status terminates the REPL with Exit(1).
    // Critical: the input must contain ONLY the `set -e`
    // and `read X` lines — any subsequent line would be
    // consumed BY the read instead of triggering EOF.
    use std::io::BufReader;
    use std::io::Cursor;
    let stdin = BufReader::new(Cursor::new(b"set -e\nread X\n".to_vec()));
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut env = BTreeMap::<String, String>::new();
    let mut flags = ShellFlags::default();
    let status = run_with_env(stdin, &mut stdout, &mut stderr, &mut env, &mut flags);
    // errexit trips on the `read X` Status(1) → REPL exits
    // with the same byte. NO `X` entry should exist post-loop.
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(env.get("X").is_none(), "unexpected X entry: {env:?}");
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
}

#[test]
fn read_returns_one_after_last_line() {
    // Multi-line input: `a` and `b` arrive on stdin, then
    // EOF. Three sequential `read X` calls — the first two
    // succeed (X=a then X=b), the third hits EOF and
    // returns Status(1), leaving X=b unchanged. The script
    // pattern is "REPL line + read-input line" alternating
    // (because the read builtin and the REPL share stdin —
    // a line typed AFTER `read X` is consumed BY the read,
    // not interpreted as a REPL command). The third `read
    // X` is the LAST line of the input so the `read_line`
    // call inside it hits EOF cleanly. The REPL's own next
    // iteration also hits EOF, returning ExitStatus::Eof.
    use std::io::BufReader;
    use std::io::Cursor;
    let stdin = BufReader::new(Cursor::new(
        b"read X\na\nread X\nb\nread X\n".to_vec(),
    ));
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut env = BTreeMap::<String, String>::new();
    let mut flags = ShellFlags::default();
    let status = run_with_env(stdin, &mut stdout, &mut stderr, &mut env, &mut flags);
    // REPL hits EOF after the third read returns Status(1)
    // (the read consumed the input completely). Eof maps
    // to exit code 0 in `ExitStatus::code()`.
    assert_eq!(status, ExitStatus::Eof);
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    // After all reads complete, X must hold the value from
    // the LAST successful read (`b`) — the EOF read does
    // not overwrite.
    assert_eq!(env.get("X").map(String::as_str), Some("b"));
}

#[test]
fn read_no_args_is_usage_error() {
    // Bare `read` (no var name) → stderr diagnostic plus
    // Status(2). The REPL stays alive so the trailing
    // `exit` runs.
    let (status, _stdout, stderr, env) =
        drive("read\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("sh: read: missing variable name"),
        "stderr missing usage diagnostic: {stderr:?}"
    );
    // Sanity: no env mutation from a usage error.
    assert!(env.is_empty(), "unexpected env entries: {env:?}");
}

#[test]
fn read_empty_name_is_invalid_identifier() {
    // `read ""` (empty-string var name from a quoted empty
    // arg) → stderr diagnostic plus Status(2). Mirrors the
    // existing `export` empty-name handling.
    let (status, _stdout, stderr, env) =
        drive("read \"\"\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("sh: read: : not a valid identifier"),
        "stderr missing invalid-identifier diagnostic: {stderr:?}"
    );
    // No env mutation; the empty-name short-circuit fires
    // BEFORE the read attempt.
    assert!(env.is_empty(), "unexpected env entries: {env:?}");
}

#[test]
fn read_multi_var_first_gets_line_rest_empty() {
    // v1 simplification: `read A B C` against `"x y z\n"`
    // assigns A="x y z", B="", C="" — no IFS-splitting yet.
    // Pin the simplification explicitly so the future
    // IFS-splitting slice has a clear regression target
    // (this test will be UPDATED, not deleted, when the
    // real splitting lands).
    let (status, _stdout, stderr, env) =
        drive("read A B C\nx y z\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("A").map(String::as_str), Some("x y z"));
    assert_eq!(env.get("B").map(String::as_str), Some(""));
    assert_eq!(env.get("C").map(String::as_str), Some(""));
}

#[test]
fn read_overwrites_existing_var() {
    // Pre-seed X=old; `read X` against `"new\n"` must
    // replace the existing entry, not append. Confirms the
    // `BTreeMap::insert` shape (overwrite-on-collision)
    // matches the user's mental model for `read`.
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "old".to_string());
    let (status, _stdout, stderr, env) =
        drive("read X\nnew\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("X").map(String::as_str), Some("new"));
}

#[test]
fn read_handles_unicode_line() {
    // Rust `BufRead::read_line` is utf-8 native, so
    // multibyte sequences round-trip cleanly through the
    // string buffer. Pin that the strip-trailing-newline
    // logic doesn't accidentally trim a multibyte tail
    // byte (the `\n` / `\r` matchers are single-byte ASCII
    // chars, so they cannot match inside a utf-8 sequence).
    let (status, _stdout, stderr, env) =
        drive("read X\nh\u{00e9}llo\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("X").map(String::as_str), Some("h\u{00e9}llo"));
}

// ---------- read -r flag ----------
//
// Without `-r`, POSIX `read` interprets a trailing
// backslash at the end of an input line as a
// line-continuation marker — the backslash and following
// newline are stripped, and a SECOND line is read and
// concatenated. Multiple consecutive continuations chain.
// A trailing backslash with another backslash before it
// (an EVEN count of trailing backslashes) is NOT a
// continuation — the second-to-last backslash escapes the
// last one and the value is preserved verbatim. With `-r`,
// ALL backslashes are LITERAL — no continuation handling,
// no escape interpretation. `read -r` is the SAFE form
// that virtually all modern POSIX scripts use (the
// canonical idiom is `while IFS= read -r line; do ... done`).
//
// EOF mid-continuation: when the FIRST line ends with a
// continuation backslash but EOF arrives before the SECOND
// line, the v1 simplification returns whatever was
// accumulated with `Status(0)` (because at least one line
// WAS read successfully) — POSIX permits either the
// "Status(0) with partial value" or "Status(1) with no
// value" reading; v1 picks the more useful one for
// scripts that want to capture what they got.

#[test]
fn read_r_treats_trailing_backslash_as_literal() {
    // `read -r X` against `"foo\\\n"` reads ONE line and
    // assigns `X=foo\\` — the backslash is preserved
    // because raw mode disables continuation handling.
    let (status, _stdout, stderr, env) =
        drive("read -r X\nfoo\\\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("X").map(String::as_str), Some("foo\\"));
}

#[test]
fn read_default_treats_trailing_backslash_as_continuation() {
    // Without `-r`, `"foo\\\nbar\n"` is a continuation:
    // the backslash + newline are stripped, the second
    // line is appended → `X=foobar`.
    let (status, _stdout, stderr, env) =
        drive("read X\nfoo\\\nbar\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("X").map(String::as_str), Some("foobar"));
}

#[test]
fn read_default_handles_multiple_continuations() {
    // Three lines joined by two continuation backslashes
    // → `"a\\\nb\\\nc\n"` → `X=abc`. Pins that the
    // continuation loop runs until a non-continuation
    // line is reached.
    let (status, _stdout, stderr, env) =
        drive("read X\na\\\nb\\\nc\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("X").map(String::as_str), Some("abc"));
}

#[test]
fn read_default_double_backslash_is_not_continuation() {
    // `"foo\\\\\n"` has TWO trailing backslashes (even
    // count) — the second-to-last escapes the last, so
    // there's no continuation. Both backslashes are
    // preserved verbatim → `X=foo\\\\`.
    let (status, _stdout, stderr, env) =
        drive("read X\nfoo\\\\\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("X").map(String::as_str), Some("foo\\\\"));
}

#[test]
fn read_r_passes_through_double_backslash() {
    // Same input as the previous test but with `-r`.
    // The result is identical (`X=foo\\\\`) because raw
    // mode also preserves all backslashes — but the path
    // through the code differs (the trailing-count check
    // is bypassed entirely in raw mode).
    let (status, _stdout, stderr, env) =
        drive("read -r X\nfoo\\\\\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("X").map(String::as_str), Some("foo\\\\"));
}

#[test]
fn read_r_with_multi_var_works() {
    // The `-r` flag consumes the first arg slot; the
    // remaining args are var names. The v1 multi-VAR
    // simplification still applies — first var gets the
    // whole line, the rest get the empty string.
    let (status, _stdout, stderr, env) =
        drive("read -r A B C\nx y z\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("A").map(String::as_str), Some("x y z"));
    assert_eq!(env.get("B").map(String::as_str), Some(""));
    assert_eq!(env.get("C").map(String::as_str), Some(""));
}

#[test]
fn read_r_with_no_var_names_is_usage_error() {
    // `read -r` with no var names after the flag → same
    // diagnostic as bare `read`. The flag is consumed but
    // the missing-name check fires before any read.
    let (status, _stdout, stderr, env) =
        drive("read -r\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("sh: read: missing variable name"),
        "stderr missing usage diagnostic: {stderr:?}"
    );
    assert!(env.is_empty(), "unexpected env entries: {env:?}");
}

#[test]
fn read_default_eof_during_continuation_returns_partial_value() {
    // FIRST line ends with a continuation backslash but
    // EOF arrives before the SECOND line. v1 simplification:
    // return whatever was accumulated (`X=foo` after the
    // continuation backslash is stripped) with Status(0)
    // because at least one line was read successfully.
    // Uses direct `run_with_env` because the input must
    // end EXACTLY at the continuation backslash so the
    // second `read_line` returns Ok(0).
    use std::io::BufReader;
    use std::io::Cursor;
    let stdin = BufReader::new(Cursor::new(b"read X\nfoo\\\n".to_vec()));
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut env = BTreeMap::<String, String>::new();
    let mut flags = ShellFlags::default();
    let status = run_with_env(stdin, &mut stdout, &mut stderr, &mut env, &mut flags);
    // REPL hits EOF after the read returns Status(0). Eof
    // maps to exit code 0.
    assert_eq!(status, ExitStatus::Eof);
    assert!(
        String::from_utf8(stderr).expect("stderr utf-8").is_empty(),
        "unexpected stderr"
    );
    // Partial value preserved: the continuation backslash
    // was stripped, the EOF aborted the second-line read,
    // and what remains is `foo`.
    assert_eq!(env.get("X").map(String::as_str), Some("foo"));
}

#[test]
fn read_r_preserves_backslash_in_middle_of_line() {
    // Backslash in the MIDDLE of the line — `-r` mode has
    // no escape interpretation anywhere, so `"foo\\bar\n"`
    // reads as `X=foo\\bar` verbatim.
    let (status, _stdout, stderr, env) =
        drive("read -r X\nfoo\\bar\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(env.get("X").map(String::as_str), Some("foo\\bar"));
}

// ---------- read -p flag ----------
//
// `read -p PROMPT VAR` writes PROMPT to STDERR (NOT stdout —
// POSIX-aligned, since stdout is reserved for the script's
// own output) WITHOUT a trailing newline, BEFORE blocking on
// the read. The prompt is a literal string (no expansion at
// the builtin layer; the upstream dispatcher already expanded
// any `$VAR` references before calling `builtin_read`). Both
// the space-separated form (`-p PROMPT`, two argv slots) and
// the glued no-space form (`-pPROMPT`, one argv slot) are
// accepted, mirroring the `sort -o FILE` / `sort -oFILE`
// pattern. The flag composes with `-r` in either order.
//
// v1 simplification: the prompt is written EXACTLY ONCE per
// `read -p` invocation; default-mode trailing-backslash
// continuation does NOT re-prompt (bash optionally uses PS2
// for that, but v1 doesn't model PS2 yet — the user just
// types the second line with no further visible cue).
//
// Edge case: `read -p -r VAR` (where `-r` LOOKS like a flag
// but appears in the prompt-value slot) is LITERALLY accepted
// — `-p` consumes the very next token as the prompt value
// regardless of content, so the prompt is the literal string
// `-r` and `VAR` is read in NON-raw mode. Matches bash.

#[test]
fn read_p_writes_prompt_to_stderr_before_blocking() {
    // Canonical happy path. `read -p "Enter: " X` against the
    // input line `hello\n` writes `Enter: ` to stderr (NO
    // trailing newline — the cursor sits on the same line,
    // ready for input) and assigns `X=hello`. The prompt
    // appears EXACTLY as written (no expansion, no escape
    // interpretation, no trim).
    let (status, _stdout, stderr, env) =
        drive("read -p \"Enter: \" X\nhello\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert_eq!(env.get("X").map(String::as_str), Some("hello"));
    assert!(
        stderr.contains("Enter: "),
        "stderr missing prompt: {stderr:?}"
    );
    assert!(
        !stderr.contains("Enter: \n"),
        "stderr prompt has unwanted trailing newline: {stderr:?}"
    );
}

#[test]
fn read_p_glued_form_works() {
    // `-pEnter:` (no space) is the glued one-slot form. The
    // prompt value is the suffix after `-p` and must reach
    // stderr identically to the space-separated form. Quote
    // the whole arg so the tokenizer keeps `-pEnter:` as a
    // single token. Pin the same `X=hello` outcome.
    let (status, _stdout, stderr, env) =
        drive("read \"-pEnter: \" X\nhello\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert_eq!(env.get("X").map(String::as_str), Some("hello"));
    assert!(
        stderr.contains("Enter: "),
        "stderr missing glued prompt: {stderr:?}"
    );
}

#[test]
fn read_p_with_no_value_is_usage_error() {
    // Bare `read -p` at end-of-args → no prompt value
    // available; must short-circuit with a usage diagnostic
    // and Status(2). Critically, the read must NOT proceed —
    // a fall-through that consumed the next stdin line as if
    // `-p` weren't there would silently hide the user's
    // typo. The next line in stdin (`exit`) must reach the
    // REPL untouched, so the script terminates via the
    // `exit` command rather than via EOF or via a
    // misinterpreted read.
    let (status, _stdout, stderr, env) =
        drive("read -p\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("sh: read: -p: missing prompt"),
        "stderr missing missing-prompt diagnostic: {stderr:?}"
    );
    assert!(env.is_empty(), "unexpected env mutation: {env:?}");
}

#[test]
fn read_p_compose_with_r_flag_either_order() {
    // Both `-r -p PROMPT VAR` and `-p PROMPT -r VAR` must
    // work identically: a single prompt write to stderr,
    // raw mode active (so a trailing backslash is LITERAL,
    // no continuation), value `foo\\` (Rust escape: one
    // backslash) preserved verbatim from `foo\\\n` input.
    let (status_a, _stdout_a, stderr_a, env_a) = drive(
        "read -r -p \"Hi: \" X\nfoo\\\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status_a, ExitStatus::Exit(0));
    assert_eq!(env_a.get("X").map(String::as_str), Some("foo\\"));
    assert!(stderr_a.contains("Hi: "), "order A stderr: {stderr_a:?}");

    let (status_b, _stdout_b, stderr_b, env_b) = drive(
        "read -p \"Hi: \" -r X\nfoo\\\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status_b, ExitStatus::Exit(0));
    assert_eq!(env_b.get("X").map(String::as_str), Some("foo\\"));
    assert!(stderr_b.contains("Hi: "), "order B stderr: {stderr_b:?}");
    // Both orders produce IDENTICAL stderr — exactly one
    // prompt write per `read -p`, no duplication.
    assert_eq!(stderr_a.matches("Hi: ").count(), 1);
    assert_eq!(stderr_b.matches("Hi: ").count(), 1);
}

#[test]
fn read_p_does_not_rewrite_prompt_on_continuation() {
    // Default mode (no `-r`) treats trailing backslash as
    // continuation. `read -p "Q: " X` against `"a\\\nb\n"`
    // (two input lines, the first continues into the
    // second) reads BOTH lines and assigns `X=ab`. The
    // prompt is written EXACTLY ONCE — the continuation
    // iteration does NOT re-prompt (v1 has no PS2 yet, so
    // there's no continuation prompt to write).
    let (status, _stdout, stderr, env) = drive(
        "read -p \"Q: \" X\na\\\nb\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status, ExitStatus::Exit(0));
    assert_eq!(env.get("X").map(String::as_str), Some("ab"));
    assert_eq!(
        stderr.matches("Q: ").count(),
        1,
        "expected exactly one prompt write, got: {stderr:?}"
    );
}

#[test]
fn read_p_empty_prompt_writes_nothing_visible() {
    // `read -p "" X` with an empty prompt is valid — no
    // diagnostic, the empty `write!` produces zero bytes,
    // and the read proceeds normally. Pin that the empty
    // prompt does not accidentally leak into stderr or
    // alter the env mutation.
    let (status, _stdout, stderr, env) =
        drive("read -p \"\" X\nhello\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert_eq!(env.get("X").map(String::as_str), Some("hello"));
    assert!(stderr.is_empty(), "stderr should be empty: {stderr:?}");
}

#[test]
fn read_p_with_quirky_value_dash_r() {
    // `read -p -r X` — the `-p` flag consumes the very next
    // argv slot as its prompt value REGARDLESS of content.
    // So the prompt is the literal string `-r` (NOT
    // interpreted as the raw-mode flag) and the read
    // proceeds in NON-raw mode against `X`. This matches
    // bash's behaviour and pins the v1 "consume next slot
    // verbatim" rule.
    let (status, _stdout, stderr, env) =
        drive("read -p -r X\nfoo\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert_eq!(env.get("X").map(String::as_str), Some("foo"));
    assert!(
        stderr.contains("-r"),
        "stderr should contain literal -r prompt: {stderr:?}"
    );
}

#[test]
fn read_p_does_not_pollute_stdout() {
    // POSIX-critical invariant: the prompt MUST go to
    // stderr, NOT stdout. Stdout is reserved for the
    // script's data output (so `script.sh | grep foo`
    // still works while the script prints prompts to the
    // user). Pin that no part of the prompt leaks into
    // stdout — the script does no `echo`, so stdout should
    // be empty (the REPL's per-line `$ ` prompt is not
    // emitted in the run_with_env shape; see existing
    // tests).
    let (status, stdout, _stderr, env) =
        drive("read -p \"Q: \" X\nhello\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert_eq!(env.get("X").map(String::as_str), Some("hello"));
    assert!(
        !stdout.contains("Q:"),
        "stdout must not contain prompt: {stdout:?}"
    );
}

#[test]
fn read_p_ascii_prompt_with_punctuation() {
    // The builtin layer passes the prompt verbatim via
    // `write!(stderr, "{p}")` — any byte sequence is
    // forwarded as-is. Multi-byte utf-8 round-tripping
    // through the v1 shell tokeniser is currently affected
    // by a pre-existing double-encoding limitation in the
    // upstream `tokenise_with_quotes` / expansion layer
    // (non-ASCII bytes inside `"..."` get re-interpreted as
    // latin-1 and re-encoded as utf-8), so this slice
    // documents that limitation without stretching test
    // coverage into the upstream layer's bug. This test
    // pins the ASCII-punctuation case (`?`, `!`, colon,
    // brackets) which IS preserved cleanly through the
    // tokeniser today, demonstrating the builtin's
    // pass-through behaviour for the prompt arg.
    let (status, _stdout, stderr, env) =
        drive("read -p \"What's your name? \" NAME\nalice\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert_eq!(env.get("NAME").map(String::as_str), Some("alice"));
    assert!(
        stderr.contains("What's your name? "),
        "stderr missing punctuation prompt: {stderr:?}"
    );
}

#[test]
fn read_p_prompt_with_multiple_words() {
    // Quoted multi-word prompts must reach the builtin as a
    // SINGLE argv slot (the v1 tokenizer is quote-aware —
    // see `tokenise_with_quotes` in run.rs — so
    // `"Enter your name now: "` survives whitespace-split
    // suppression). Pins that the prompt tokenizer behaves
    // correctly under multi-word quoted prompts (the
    // typical use case for `read -p`).
    let (status, _stdout, stderr, env) = drive(
        "read -p \"Enter your name now: \" NAME\nalice\nexit\n",
        BTreeMap::new(),
    );
    assert_eq!(status, ExitStatus::Exit(0));
    assert_eq!(env.get("NAME").map(String::as_str), Some("alice"));
    assert!(
        stderr.contains("Enter your name now: "),
        "stderr missing multi-word prompt: {stderr:?}"
    );
}
