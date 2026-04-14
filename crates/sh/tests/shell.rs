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
