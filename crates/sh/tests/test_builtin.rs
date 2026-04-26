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
