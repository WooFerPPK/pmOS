//! `sh::run` REPL driver tests for quote-string handling
//! (T142 partial — quoting slice).
//!
//! These cover the userland-binary-facing entry point: the
//! REPL's tokeniser recognises `'...'` as a literal segment
//! that preserves whitespace and SUPPRESSES `$VAR` expansion
//! inside the quotes, and `"..."` as a non-splitting segment
//! that preserves whitespace but STILL EXPANDS `$VAR`/`${VAR}`
//! references inside it. Tests drive `run_with_env` so quoted
//! `$VAR` cases can pre-seed the env map and verify the
//! expand / no-expand split. Backslash escapes inside `"..."`
//! are NOT supported in this slice (a future micro-slice).

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

#[test]
fn double_quoted_preserves_whitespace() {
    // `echo "hello world"` → one arg `hello world` →
    // stdout contains `hello world\n`. Like the single-quote
    // case, the inner space is part of one token, NOT a token
    // splitter. The key invariant is that `"..."` suppresses
    // whitespace splitting just as `'...'` does.
    let (status, stdout, stderr, _env) =
        drive("echo \"hello world\"\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("hello world\n"),
        "stdout missing literal whitespace token: {stdout:?}"
    );
}

#[test]
fn double_quoted_expands_dollar_var() {
    // Seed `X=1`. `echo "$X"` → stdout contains `1\n`
    // (NOT `$X\n`) — double quotes preserve whitespace but
    // $VAR DOES expand inside them. This is the key
    // contrast with the single-quote case (where `'$X'`
    // stays literal as `$X`).
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "1".to_string());
    let (status, stdout, stderr, _env) = drive("echo \"$X\"\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing expanded value: {stdout:?}"
    );
    assert!(
        !stdout.contains("$X\n"),
        "stdout should NOT contain literal $X: {stdout:?}"
    );
}

#[test]
fn double_quoted_with_braced_var_expansion() {
    // Seed `X=hi`. `echo "${X}there"` → stdout contains
    // `hithere\n`. The braced form works inside double
    // quotes the same as outside — assemble_token runs
    // expand_vars over the whole Unquoted segment regardless
    // of how the segment was bounded.
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "hi".to_string());
    let (status, stdout, stderr, _env) = drive("echo \"${X}there\"\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("hithere\n"),
        "stdout missing braced expansion: {stdout:?}"
    );
}

#[test]
fn double_and_single_quote_concat_in_one_token() {
    // Seed `X=1`. `echo "$X"'$Y'` → tokens `["echo",
    // "1$Y"]` → stdout contains `1$Y\n`. The double-quoted
    // segment expands `$X` to `1`; the single-quoted
    // segment keeps `$Y` literal. With no whitespace
    // between the two quote groups they concat into one
    // token. `Y` is unset so even if it leaked through
    // expansion it would expand to empty — the literal
    // `$Y\n` in stdout is the only output that proves the
    // single quotes blocked expansion.
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "1".to_string());
    let (status, stdout, stderr, _env) = drive("echo \"$X\"'$Y'\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1$Y\n"),
        "stdout missing mixed-quote concat: {stdout:?}"
    );
}

#[test]
fn unterminated_double_quote_errors_to_stderr() {
    // `echo "unclosed\nexit\n` — the first line has an
    // open `"` with no matching close. The REPL must:
    //   1. Write `sh: unterminated double quote\n` to stderr.
    //   2. Skip dispatch for that line (so the bogus `echo`
    //      doesn't run).
    //   3. Stay alive — the next line's `exit` must run.
    let (status, _stdout, stderr, _env) =
        drive("echo \"unclosed\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("unterminated double quote"),
        "stderr missing error: {stderr:?}"
    );
}

#[test]
fn double_quoted_inside_concat_with_unquoted_text() {
    // Seed `X=mid`. `echo a"$X"b` → tokens `["echo",
    // "amidb"]` → stdout contains `amidb\n`. The bare
    // `a`, the double-quoted `$X` (expands to `mid`), and
    // the bare `b` concat into one token. Mirror of the
    // single-quoted `single_quoted_concat_with_unquoted_text`
    // test, but the middle segment EXPANDS instead of
    // staying literal.
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "mid".to_string());
    let (status, stdout, stderr, _env) = drive("echo a\"$X\"b\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("amidb\n"),
        "stdout missing concatenated token: {stdout:?}"
    );
}
