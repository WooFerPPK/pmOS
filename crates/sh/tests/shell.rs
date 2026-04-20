//! `Shell::eval` isolation tests.

use std::collections::BTreeMap;

use sh::{Shell, BUILTINS};

fn stdout_str(s: &sh::ShellOutput) -> &str {
    std::str::from_utf8(&s.stdout).unwrap()
}

fn stderr_str(s: &sh::ShellOutput) -> &str {
    std::str::from_utf8(&s.stderr).unwrap()
}

#[test]
fn new_shell_has_root_cwd_and_empty_env() {
    let s = Shell::new();
    assert_eq!(s.cwd(), "/");
    assert!(s.get_env("HOME").is_none());
    assert!(!s.has_exited());
}

#[test]
fn empty_line_is_a_no_op() {
    let mut s = Shell::new();
    let out = s.eval("");
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
    assert!(out.exit_code.is_none());
}

#[test]
fn whitespace_only_line_is_a_no_op() {
    let mut s = Shell::new();
    let out = s.eval("   \t ");
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
}

#[test]
fn echo_with_no_args_prints_just_a_newline() {
    let mut s = Shell::new();
    let out = s.eval("echo");
    assert_eq!(stdout_str(&out), "\n");
}

#[test]
fn echo_joins_args_with_spaces_and_appends_a_newline() {
    let mut s = Shell::new();
    let out = s.eval("echo hello world");
    assert_eq!(stdout_str(&out), "hello world\n");
}

#[test]
fn echo_honours_single_quoted_whitespace() {
    let mut s = Shell::new();
    let out = s.eval("echo 'hi  there'");
    assert_eq!(stdout_str(&out), "hi  there\n");
}

#[test]
fn pwd_prints_current_cwd_with_trailing_newline() {
    let mut s = Shell::new();
    let out = s.eval("pwd");
    assert_eq!(stdout_str(&out), "/\n");
}

#[test]
fn cd_absolute_updates_cwd() {
    let mut s = Shell::new();
    s.eval("cd /home/user");
    assert_eq!(s.cwd(), "/home/user");
    let out = s.eval("pwd");
    assert_eq!(stdout_str(&out), "/home/user\n");
}

#[test]
fn cd_relative_joins_against_cwd() {
    let mut s = Shell::with_state("/home/user", BTreeMap::new());
    s.eval("cd src");
    assert_eq!(s.cwd(), "/home/user/src");
}

#[test]
fn cd_dot_dot_pops_a_segment() {
    let mut s = Shell::with_state("/home/user/src", BTreeMap::new());
    s.eval("cd ..");
    assert_eq!(s.cwd(), "/home/user");
}

#[test]
fn cd_normalises_repeated_slashes_and_dots() {
    let mut s = Shell::new();
    s.eval("cd /home///./user/../admin");
    assert_eq!(s.cwd(), "/home/admin");
}

#[test]
fn cd_with_no_args_goes_to_root() {
    let mut s = Shell::with_state("/tmp", BTreeMap::new());
    s.eval("cd");
    assert_eq!(s.cwd(), "/");
}

#[test]
fn set_and_env_round_trip() {
    let mut s = Shell::new();
    s.eval("set FOO=bar");
    s.eval("set PATH=/bin");
    let out = s.eval("env");
    // BTreeMap sorts alphabetically, so FOO comes before PATH.
    assert_eq!(stdout_str(&out), "FOO=bar\nPATH=/bin\n");
}

#[test]
fn set_with_no_args_prints_env() {
    // POSIX `set` with no args should list every env var.
    let mut s = Shell::new();
    s.eval("set A=1");
    let out = s.eval("set");
    assert_eq!(stdout_str(&out), "A=1\n");
}

#[test]
fn set_missing_equals_prints_usage_on_stderr() {
    let mut s = Shell::new();
    let out = s.eval("set FOO");
    assert!(stderr_str(&out).contains("usage"));
    assert!(out.stdout.is_empty());
}

#[test]
fn unset_removes_the_variable() {
    let mut s = Shell::new();
    s.eval("set KEEP=y");
    s.eval("set DROP=y");
    s.eval("unset DROP");
    assert!(s.get_env("KEEP").is_some());
    assert!(s.get_env("DROP").is_none());
}

#[test]
fn unset_with_no_args_prints_usage_on_stderr() {
    let mut s = Shell::new();
    let out = s.eval("unset");
    assert!(stderr_str(&out).contains("usage"));
}

#[test]
fn true_is_a_successful_empty_command() {
    let mut s = Shell::new();
    let out = s.eval("true");
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
    assert!(out.exit_code.is_none());
}

#[test]
fn false_returns_exit_code_one_without_exiting_the_shell() {
    let mut s = Shell::new();
    let out = s.eval("false");
    assert_eq!(out.exit_code, Some(1));
    assert!(!s.has_exited(), "`false` reports exit code but does not terminate the shell");
}

#[test]
fn exit_with_no_code_exits_zero() {
    let mut s = Shell::new();
    let out = s.eval("exit");
    assert_eq!(out.exit_code, Some(0));
    assert!(s.has_exited());
    assert_eq!(s.exit_status(), Some(0));
}

#[test]
fn exit_with_a_numeric_code_preserves_it() {
    let mut s = Shell::new();
    let out = s.eval("exit 42");
    assert_eq!(out.exit_code, Some(42));
    assert_eq!(s.exit_status(), Some(42));
}

#[test]
fn exit_with_garbage_code_defaults_to_zero() {
    let mut s = Shell::new();
    let out = s.eval("exit abc");
    assert_eq!(out.exit_code, Some(0));
}

#[test]
fn help_lists_every_builtin() {
    let mut s = Shell::new();
    let out = s.eval("help");
    let text = stdout_str(&out);
    for name in BUILTINS {
        assert!(text.contains(name), "help missing {name}");
    }
}

#[test]
fn unknown_command_writes_command_not_found_to_stderr_with_no_exit_code() {
    let mut s = Shell::new();
    let out = s.eval("ls -la");
    assert!(out.stdout.is_empty());
    let err = stderr_str(&out);
    assert!(err.contains("command not found"));
    assert!(err.contains("ls"));
    assert!(out.exit_code.is_none(), "command-not-found should not terminate the shell");
}

#[test]
fn command_not_found_does_not_mutate_shell_state() {
    let mut s = Shell::new();
    s.eval("set PRESERVED=yes");
    s.eval("ls /nope");
    assert_eq!(s.get_env("PRESERVED"), Some("yes"));
    assert_eq!(s.cwd(), "/");
    assert!(!s.has_exited());
}

#[test]
fn repl_style_sequence_echo_help_exit() {
    // A realistic demo session: user runs a few commands,
    // then exits. Exercises the state machine across
    // multiple calls.
    let mut s = Shell::new();
    assert_eq!(stdout_str(&s.eval("echo hello")), "hello\n");
    let help = s.eval("help");
    assert!(stdout_str(&help).contains("echo"));
    assert!(stdout_str(&help).contains("exit"));
    let exit = s.eval("exit 7");
    assert_eq!(exit.exit_code, Some(7));
    assert!(s.has_exited());
}

// -----------------------------------------------------------------------------
// `sh::run` REPL driver tests (T123)
//
// These cover the userland-binary-facing entry point: a
// minimal REPL that reads stdin, tokenises by whitespace,
// dispatches against the four-builtin slice (echo / exit /
// cd / pwd), and reports an `ExitStatus` to the caller.
// Distinct from the `Shell::eval` tests above — those
// cover the library state machine embedded in other crates
// (term, integration-tests); these cover the binary loop.
// -----------------------------------------------------------------------------

use sh::{run, ExitStatus};
use std::io::{BufReader, Cursor};

/// Drive `run` with a byte-string stdin and return
/// `(status, stdout, stderr)` for assertion.
fn drive(input: &str) -> (ExitStatus, String, String) {
    let stdin = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let status = run(stdin, &mut stdout, &mut stderr);
    let out = String::from_utf8(stdout).expect("stdout must be utf-8");
    let err = String::from_utf8(stderr).expect("stderr must be utf-8");
    (status, out, err)
}

#[test]
fn run_eof_exits_clean() {
    let (status, stdout, stderr) = drive("");
    assert_eq!(status, ExitStatus::Eof);
    // With no input the prompt still prints once before
    // `read_line` hits EOF; nothing else may appear.
    assert_eq!(stdout, "$ ");
    assert!(stderr.is_empty());
}

#[test]
fn run_echo_writes_args_with_spaces_and_newline() {
    let (status, stdout, stderr) = drive("echo hello world\n");
    // EOF after the echo line → clean.
    assert_eq!(status, ExitStatus::Eof);
    assert!(
        stdout.contains("hello world\n"),
        "stdout missing echo output: {stdout:?}"
    );
    assert!(stderr.is_empty());
}

#[test]
fn run_echo_with_no_args_emits_just_newline() {
    let (status, stdout, stderr) = drive("echo\n");
    assert_eq!(status, ExitStatus::Eof);
    // The prompt + a bare newline (echo's output) + the
    // second prompt before EOF. Guarantees the line
    // exists in the stream, exact-match on the two-prompt
    // shape.
    assert_eq!(stdout, "$ \n$ ");
    assert!(stderr.is_empty());
}

#[test]
fn run_exit_zero_returns_exit_status() {
    let (status, _stdout, stderr) = drive("exit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty());
}

#[test]
fn run_exit_with_code_returns_that_code() {
    let (status, _stdout, stderr) = drive("exit 42\n");
    assert_eq!(status, ExitStatus::Exit(42));
    assert!(stderr.is_empty());
}

#[test]
fn run_exit_with_invalid_code_returns_exit_two() {
    let (status, _stdout, stderr) = drive("exit notanumber\n");
    assert_eq!(status, ExitStatus::Exit(2));
    assert!(
        stderr.contains("numeric argument required"),
        "stderr missing error: {stderr:?}"
    );
    assert!(stderr.contains("notanumber"));
}

#[test]
fn run_unknown_command_writes_to_stderr_and_continues() {
    let (status, _stdout, stderr) = drive("foo\nexit\n");
    // The REPL survived the unknown command and ran
    // `exit` on the next line.
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.contains("sh: command not found: foo"),
        "stderr missing error: {stderr:?}"
    );
}

#[test]
fn run_pwd_writes_current_dir() {
    let (status, stdout, stderr) = drive("pwd\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    // Stream shape: `"$ <path>\n$ "`. Strip the leading
    // prompt and grab the pwd output up to its newline.
    let after_prompt = stdout
        .strip_prefix("$ ")
        .unwrap_or_else(|| panic!("stdout missing initial prompt: {stdout:?}"));
    let (pwd_output, _rest) = after_prompt
        .split_once('\n')
        .unwrap_or_else(|| panic!("stdout missing pwd newline: {stdout:?}"));
    assert!(
        pwd_output.starts_with('/'),
        "pwd output should be an absolute path: {pwd_output:?}"
    );
    assert!(stderr.is_empty());
}

#[test]
fn run_cd_changes_current_dir_and_affects_pwd() {
    // `cd /tmp` → `pwd` should report `/tmp`.
    // Tracked in a local PathBuf inside `run` so this works
    // even when the underlying std::env::set_current_dir is
    // a no-op (e.g. wasip1).
    let (status, stdout, _stderr) = drive("cd /tmp\npwd\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    // `pwd` writes `/tmp` + newline somewhere in the stream
    // between the prompts. Use a substring check against
    // the raw bytes rather than splitting on newline (the
    // prompt never emits one, so `stdout.lines()` produces
    // concatenated prompt+output segments).
    assert!(
        stdout.contains("/tmp\n"),
        "stdout missing `/tmp` pwd line: {stdout:?}"
    );
}

#[test]
fn run_blank_line_reprints_prompt() {
    // Blank line is a no-op that just re-prints `"$ "`.
    // The sequence is:
    //   prompt #1 — before we read the blank line.
    //   blank line is a no-op (no output).
    //   prompt #2 — before we read `"exit"`.
    //   exit runs, prompt is NOT re-printed.
    let (status, stdout, _stderr) = drive("\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert_eq!(stdout, "$ $ ");
}

#[test]
fn run_cd_with_no_args_returns_to_root_and_pwd_reflects_it() {
    // cd without args defaults to `/`. Preceded by a
    // non-trivial cd so the test actually exercises the
    // "reset" path.
    let (status, stdout, _stderr) = drive("cd /tmp\ncd\npwd\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stdout.contains("/\n"),
        "stdout missing `/` pwd line: {stdout:?}"
    );
}
