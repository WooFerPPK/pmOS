//! `sh::run` REPL driver tests for `$VAR` substitution
//! (T142 partial — variable-substitution slice).
//!
//! These cover the userland-binary-facing entry point:
//! after whitespace tokenisation, the REPL walks each
//! token and expands `$NAME` / `${NAME}` references against
//! the env map T144 wired through `run_with_env`. Tests
//! drive `run_with_env` so they can pre-seed env entries
//! and observe the substituted output via the `echo`
//! builtin's stdout.

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
fn echo_expands_set_var_to_value() {
    // Pre-seed `X=hello`; `echo $X` should print `hello\n`.
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "hello".to_string());
    let (status, stdout, stderr, _env) = drive("echo $X\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("hello\n"),
        "stdout missing expanded value: {stdout:?}"
    );
}

#[test]
fn echo_after_export_round_trips_value() {
    // `export X=1` then `echo $X` — the just-set value is
    // immediately visible to the next command's expansion.
    let (status, stdout, stderr, env) =
        drive("export X=1\necho $X\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("1\n"),
        "stdout missing expanded value: {stdout:?}"
    );
    assert_eq!(env.get("X"), Some(&"1".to_string()));
}

#[test]
fn echo_unset_var_emits_just_newline() {
    // `echo $UNSET` → empty token in the args. `echo` joins
    // with single spaces + trailing newline, so the line is
    // exactly `\n`. (`echo` can't tell the empty token from
    // a no-arg call from its own stdout — both shapes emit
    // just `\n`. The token IS preserved, though, so the
    // `args.join(" ")` walks one element.)
    let (status, stdout, stderr, _env) =
        drive("echo $UNSET\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    // The prompt + expanded line + exit prompt:
    // `"$ \n$ "`.
    assert_eq!(stdout, "$ \n$ ");
}

#[test]
fn echo_concat_two_vars_in_one_token() {
    // `echo $X$Y` with `X=foo`, `Y=bar` → tokens
    // `["echo", "foobar"]` → stdout contains `foobar\n`.
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "foo".to_string());
    seed.insert("Y".to_string(), "bar".to_string());
    let (status, stdout, stderr, _env) = drive("echo $X$Y\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("foobar\n"),
        "stdout missing concatenated value: {stdout:?}"
    );
}

#[test]
fn echo_braced_form_lets_literal_chars_follow() {
    // `echo ${X}b` → `hellob\n`. The bare form `$Xb` would
    // look up `Xb` (greedy) and expand to empty, so this is
    // a regression guard on the brace path.
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "hello".to_string());
    let (status, stdout, stderr, _env) = drive("echo ${X}b\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("hellob\n"),
        "stdout missing braced expansion: {stdout:?}"
    );
}
