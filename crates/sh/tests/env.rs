//! `sh::run` REPL driver tests for the `env` and `export`
//! builtins (T144 partial).
//!
//! These cover the userland-binary-facing entry point: the
//! REPL extends T123's four-builtin set (echo / exit / cd /
//! pwd) with `env` (no-arg list, errors on positional args)
//! and `export` (NAME=VALUE / NAME / no-args list). Tests
//! drive `sh::run_with_env` so they can pre-seed the env
//! map and observe mutations after the loop returns.

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
fn env_no_args_prints_empty_when_map_empty() {
    // Empty seed, run `env` then exit. The only stdout
    // bytes between the two prompts should be the env
    // builtin's output — which is empty.
    let (status, stdout, stderr, _env) = drive("env\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty());
    // No env entries to print — stdout is just two prompts
    // (one before `env`, one before `exit`) with nothing
    // between them.
    assert_eq!(stdout, "$ $ ");
}

#[test]
fn env_no_args_prints_sorted_entries() {
    // BTreeMap iteration is sorted, so assert byte-exact
    // `KEY=VALUE\n` lines in alphabetical order regardless
    // of insertion order.
    let mut seed = BTreeMap::new();
    seed.insert("PATH".to_string(), "/bin".to_string());
    seed.insert("HOME".to_string(), "/home/user".to_string());
    seed.insert("USER".to_string(), "pmos".to_string());

    let (status, stdout, stderr, _env) = drive("env\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty());
    assert!(
        stdout.contains("HOME=/home/user\nPATH=/bin\nUSER=pmos\n"),
        "stdout missing sorted env block: {stdout:?}"
    );
}

#[test]
fn env_with_arg_errors_but_repl_continues() {
    // `env FOO` is not a valid v1 invocation — write to
    // stderr and keep the REPL alive (next prompt fires).
    let (status, stdout, stderr, _env) = drive("env FOO\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("sh: env: too many arguments"),
        "stderr missing error: {stderr:?}"
    );
    // Two prompts before the env error and a third before
    // exit — three prompts total.
    let prompt_count = stdout.matches("$ ").count();
    assert!(
        prompt_count >= 2,
        "expected at least two prompts (REPL stayed alive after env error): {stdout:?}"
    );
}

#[test]
fn export_sets_named_value() {
    // `export X=1` then `env` — the env block must contain
    // `X=1\n`.
    let (status, stdout, stderr, env) = drive("export X=1\nenv\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("X=1\n"),
        "stdout missing X=1: {stdout:?}"
    );
    // The mutation is also visible on the post-loop env map.
    assert_eq!(env.get("X"), Some(&"1".to_string()));
}

#[test]
fn export_no_equals_creates_empty_binding() {
    // `export X` without `=` creates an entry with the
    // empty string — `env` must show `X=\n`.
    let (status, stdout, stderr, env) = drive("export X\nenv\nexit\n", BTreeMap::new());
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("X=\n"),
        "stdout missing X= line: {stdout:?}"
    );
    assert_eq!(env.get("X"), Some(&String::new()));
}

#[test]
fn export_no_args_prints_exported_form() {
    // Pre-seed `X=1`, run `export` with no args. bash
    // convention: print every entry as `export NAME=VALUE`
    // lines, sorted.
    let mut seed = BTreeMap::new();
    seed.insert("X".to_string(), "1".to_string());

    let (status, stdout, stderr, _env) = drive("export\nexit\n", seed);
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("export X=1\n"),
        "stdout missing `export X=1` line: {stdout:?}"
    );
}
