//! `sh::run` REPL driver tests for single-quote string
//! handling (T142 partial — quoting slice).
//!
//! These cover the userland-binary-facing entry point: the
//! REPL's tokeniser now recognises `'...'` as a literal
//! segment that preserves whitespace and SUPPRESSES `$VAR`
//! expansion inside the quotes. Tests drive `run_with_env`
//! so single-quoted `$VAR` cases can pre-seed the env map
//! and verify it does NOT expand. Double-quote handling is
//! a separate, deferred slice.

use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};

use sh::{run_with_env, ExitStatus};

/// Drive `run_with_env` with a byte-string stdin and a
/// pre-seeded env map; return `(status, stdout, stderr,
/// env)` for assertion.
fn drive(
    input: &str,
    mut env: BTreeMap<String, String>,
) -> (ExitStatus, String, String, BTreeMap<String, String>) {
    let stdin = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let status = run_with_env(stdin, &mut stdout, &mut stderr, &mut env);
    let out = String::from_utf8(stdout).expect("stdout must be utf-8");
    let err = String::from_utf8(stderr).expect("stderr must be utf-8");
    (status, out, err, env)
}

#[test]
fn single_quoted_preserves_whitespace() {
    // `echo 'hello world'` → one arg `hello world` →
    // stdout contains `hello world\n` (single space, NOT
    // double — the inner space is literal, and `echo`'s
    // join-with-single-space happens to render the same byte
    // for a 1-arg call). The key invariant is the
    // whitespace inside the quotes is preserved as part of a
    // single token, NOT split into two.
    let (status, stdout, stderr, _env) =
        drive("echo 'hello world'\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("hello world\n"),
        "stdout missing literal whitespace token: {stdout:?}"
    );
}

#[test]
fn single_quoted_preserves_dollar_var_literal() {
    // Seed `X=1`. `echo '$X'` must print `$X\n` literally
    // (NOT `1\n`) — single quotes suppress `$VAR` expansion.
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "1".to_string());
    let (status, stdout, stderr, _env) = drive("echo '$X'\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("$X\n"),
        "stdout missing literal $X: {stdout:?}"
    );
    assert!(
        !stdout.contains("1\n"),
        "stdout should NOT contain expanded `1\\n`: {stdout:?}"
    );
}

#[test]
fn single_quoted_concat_with_unquoted_text() {
    // `echo a'bc'd` → tokens `["echo", "abcd"]` → stdout
    // contains `abcd\n`. Adjacency without intervening
    // whitespace concatenates the parts into a single token.
    let (status, stdout, stderr, _env) =
        drive("echo a'bc'd\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("abcd\n"),
        "stdout missing concatenated token: {stdout:?}"
    );
}

#[test]
fn two_adjacent_single_quotes_concat_into_one_token() {
    // `echo 'a''b'` → tokens `["echo", "ab"]` → stdout
    // contains `ab\n`. Two literal segments with no
    // whitespace between them stick together as one token.
    let (status, stdout, stderr, _env) =
        drive("echo 'a''b'\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("ab\n"),
        "stdout missing concatenated literal pair: {stdout:?}"
    );
}

#[test]
fn unterminated_single_quote_errors_to_stderr() {
    // `echo 'unclosed\nexit\n` — the first line has an open
    // `'` with no matching close. The REPL must:
    //   1. Write `sh: unterminated single quote\n` to stderr.
    //   2. Skip dispatch for that line (so the bogus `echo`
    //      doesn't run).
    //   3. Stay alive — the next line's `exit` must run.
    let (status, _stdout, stderr, _env) =
        drive("echo 'unclosed\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("unterminated single quote"),
        "stderr missing error: {stderr:?}"
    );
}

#[test]
fn unquoted_text_outside_quotes_still_expands_dollar_var() {
    // Seed `X=hi`. `echo $X 'literal'` → tokens
    // `["echo", "hi", "literal"]` → stdout contains
    // `hi literal\n`. Unquoted segments DO expand;
    // single-quoted segments do NOT. The two coexist
    // within the same line.
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "hi".to_string());
    let (status, stdout, stderr, _env) = drive("echo $X 'literal'\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("hi literal\n"),
        "stdout missing mixed expand/literal output: {stdout:?}"
    );
}
