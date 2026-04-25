//! REPL driver for `/bin/sh`.
//!
//! [`run`] is the testable entry point the userland `sh`
//! binary wires into real stdin / stdout / stderr. It
//! implements the minimal T123 shell loop:
//!
//! * print a `"$ "` prompt and flush;
//! * read one newline-terminated line from `stdin`;
//! * on EOF, return [`ExitStatus::Eof`];
//! * on stdin I/O error, return [`ExitStatus::IoError`];
//! * tokenise the line by whitespace (no quoting, no
//!   escaping, no variable expansion — that's T142);
//! * dispatch the first token against the minimal
//!   builtin set (`echo`, `exit`, `cd`, `pwd`, `env`,
//!   `export`);
//! * on `exit [code]`, return [`ExitStatus::Exit(code)`];
//! * on unknown command, write `sh: command not found:
//!   <token>\n` to `stderr` and loop.
//!
//! The cwd is tracked in a local `PathBuf` rather than
//! via `std::env::set_current_dir`: WASI preview 1 does
//! not expose a process-cwd syscall, so any call to
//! `set_current_dir` is either a no-op or an error on the
//! wasip1 target. Tracking it locally keeps `cd` / `pwd`
//! round-tripping in isolation tests.
//!
//! The env map is tracked locally in a [`BTreeMap`] so
//! `env` / `export` output is deterministically sorted by
//! key — both for human readability and for tests that
//! assert on byte-exact stdout.

use core::str::FromStr;
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Outcome of the REPL loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// Stdin reached EOF cleanly.
    Eof,
    /// User ran `exit [code]`.
    Exit(i32),
    /// Fatal I/O error on stdin / stdout / stderr.
    IoError,
}

impl ExitStatus {
    /// Translate the outcome into the process-level exit
    /// code the userland `sh` binary should pass to
    /// `__wasi_proc_exit`.
    pub fn code(self) -> i32 {
        match self {
            ExitStatus::Eof => 0,
            ExitStatus::Exit(c) => c,
            ExitStatus::IoError => 1,
        }
    }
}

/// Run the minimal REPL loop with a fresh empty env map.
///
/// Prints `"$ "`, reads one line, dispatches the first
/// whitespace-separated token against the builtin set, and
/// loops until EOF or `exit`. Tests construct a
/// `Cursor<Vec<u8>>` for `stdin`, `Vec<u8>` buffers for
/// `stdout` / `stderr`, and assert on the bytes `run`
/// writes.
///
/// Thin wrapper over [`run_with_env`] — the userland `sh`
/// binary calls this; tests that need to pre-seed env
/// entries call [`run_with_env`] directly.
pub fn run<R: BufRead, W: Write, E: Write>(stdin: R, stdout: W, stderr: E) -> ExitStatus {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    run_with_env(stdin, stdout, stderr, &mut env)
}

/// Run the REPL loop against a caller-provided env map.
///
/// Test entry point: lets a test pre-seed entries (so
/// `env` / `export` output can be asserted against a
/// known-non-empty map without having to run `export`
/// commands first) and observe mutations after the loop
/// returns.
pub fn run_with_env<R: BufRead, W: Write, E: Write>(
    mut stdin: R,
    mut stdout: W,
    mut stderr: E,
    env: &mut BTreeMap<String, String>,
) -> ExitStatus {
    // Seed cwd from the real process cwd when std gives us
    // one — on wasip1 this may just be `/`. Fall back to
    // `/` when the call fails so tests stay deterministic.
    let mut cwd: PathBuf = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut line = String::new();

    loop {
        if write!(stdout, "$ ").is_err() {
            return ExitStatus::IoError;
        }
        if stdout.flush().is_err() {
            return ExitStatus::IoError;
        }

        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) => return ExitStatus::Eof,
            Ok(_) => {}
            Err(_) => return ExitStatus::IoError,
        }

        // Strip one trailing newline (and a trailing \r
        // for CRLF input) so the tokenizer sees the raw
        // arguments. Leave any embedded whitespace alone.
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');

        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }

        match dispatch_builtin(&tokens, &mut cwd, env, &mut stdout, &mut stderr) {
            BuiltinOutcome::Continue => {}
            BuiltinOutcome::Exit(code) => return ExitStatus::Exit(code),
            BuiltinOutcome::IoError => return ExitStatus::IoError,
            BuiltinOutcome::NotBuiltin => {
                // Unknown command → stderr, keep the REPL alive.
                if writeln!(stderr, "sh: command not found: {}", tokens[0]).is_err() {
                    return ExitStatus::IoError;
                }
                if stderr.flush().is_err() {
                    return ExitStatus::IoError;
                }
            }
        }
    }
}

/// Outcome of dispatching one builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinOutcome {
    /// Builtin ran; continue the REPL.
    Continue,
    /// `exit` was invoked; terminate with this code.
    Exit(i32),
    /// Stdout / stderr write failed.
    IoError,
    /// Token was not a known builtin.
    NotBuiltin,
}

/// Dispatch the first token against the minimal builtin
/// set. Returns [`BuiltinOutcome::NotBuiltin`] when the
/// caller should fall back to external-command logic (in
/// v1: print "command not found").
pub(crate) fn dispatch_builtin<W: Write, E: Write>(
    tokens: &[&str],
    cwd: &mut PathBuf,
    env: &mut BTreeMap<String, String>,
    stdout: &mut W,
    stderr: &mut E,
) -> BuiltinOutcome {
    match tokens[0] {
        "echo" => builtin_echo(&tokens[1..], stdout),
        "exit" => builtin_exit(&tokens[1..], stderr),
        "cd" => builtin_cd(&tokens[1..], cwd),
        "pwd" => builtin_pwd(cwd, stdout),
        "env" => builtin_env(&tokens[1..], env, stdout, stderr),
        "export" => builtin_export(&tokens[1..], env, stdout, stderr),
        _ => BuiltinOutcome::NotBuiltin,
    }
}

fn builtin_echo<W: Write>(args: &[&str], stdout: &mut W) -> BuiltinOutcome {
    // Join args with single spaces + trailing newline.
    // `echo` with no args emits just the newline.
    let joined = args.join(" ");
    if writeln!(stdout, "{joined}").is_err() {
        return BuiltinOutcome::IoError;
    }
    if stdout.flush().is_err() {
        return BuiltinOutcome::IoError;
    }
    BuiltinOutcome::Continue
}

fn builtin_exit<E: Write>(args: &[&str], stderr: &mut E) -> BuiltinOutcome {
    match args.first() {
        None => BuiltinOutcome::Exit(0),
        Some(arg) => match i32::from_str(arg) {
            Ok(code) => BuiltinOutcome::Exit(code),
            Err(_) => {
                // Match bash / dash: print an error and exit
                // with status 2 on non-numeric exit args.
                let _ = writeln!(stderr, "sh: exit: {arg}: numeric argument required");
                let _ = stderr.flush();
                BuiltinOutcome::Exit(2)
            }
        },
    }
}

fn builtin_cd(args: &[&str], cwd: &mut PathBuf) -> BuiltinOutcome {
    let target = args.first().copied().unwrap_or("/");
    let new_cwd = if target.starts_with('/') {
        PathBuf::from(target)
    } else {
        cwd.join(target)
    };
    let normalised = normalise(&new_cwd);
    // Best-effort: ask std to update the real process cwd
    // (wasip1 likely no-ops / errors; we don't surface that
    // to the caller because our local cwd is the source of
    // truth for `pwd`).
    let _ = std::env::set_current_dir(&normalised);
    *cwd = normalised;
    BuiltinOutcome::Continue
}

fn builtin_pwd<W: Write>(cwd: &Path, stdout: &mut W) -> BuiltinOutcome {
    let display = cwd.to_string_lossy();
    if writeln!(stdout, "{display}").is_err() {
        return BuiltinOutcome::IoError;
    }
    if stdout.flush().is_err() {
        return BuiltinOutcome::IoError;
    }
    BuiltinOutcome::Continue
}

fn builtin_env<W: Write, E: Write>(
    args: &[&str],
    env: &BTreeMap<String, String>,
    stdout: &mut W,
    stderr: &mut E,
) -> BuiltinOutcome {
    if !args.is_empty() {
        // Minimal v1: no `env [-i] [NAME=VALUE]... [command]`
        // form yet. Reject any positional args so a future
        // slice can implement the full POSIX shape without
        // breaking anyone who relied on the no-arg path.
        if write!(stderr, "sh: env: too many arguments\n").is_err() {
            return BuiltinOutcome::IoError;
        }
        if stderr.flush().is_err() {
            return BuiltinOutcome::IoError;
        }
        return BuiltinOutcome::Continue;
    }
    for (k, v) in env.iter() {
        if writeln!(stdout, "{k}={v}").is_err() {
            return BuiltinOutcome::IoError;
        }
    }
    if stdout.flush().is_err() {
        return BuiltinOutcome::IoError;
    }
    BuiltinOutcome::Continue
}

fn builtin_export<W: Write, E: Write>(
    args: &[&str],
    env: &mut BTreeMap<String, String>,
    stdout: &mut W,
    stderr: &mut E,
) -> BuiltinOutcome {
    if args.is_empty() {
        // bash convention: `export` with no args prints every
        // entry as `export NAME=VALUE` lines, sorted.
        for (k, v) in env.iter() {
            if writeln!(stdout, "export {k}={v}").is_err() {
                return BuiltinOutcome::IoError;
            }
        }
        if stdout.flush().is_err() {
            return BuiltinOutcome::IoError;
        }
        return BuiltinOutcome::Continue;
    }
    for arg in args {
        match arg.find('=') {
            Some(0) => {
                // Empty NAME (the arg starts with `=`).
                if write!(stderr, "sh: export: {arg}: not a valid identifier\n").is_err() {
                    return BuiltinOutcome::IoError;
                }
                if stderr.flush().is_err() {
                    return BuiltinOutcome::IoError;
                }
            }
            Some(idx) => {
                let (name, rest) = arg.split_at(idx);
                // rest still has the leading '='; strip it.
                let value = &rest[1..];
                env.insert(name.to_string(), value.to_string());
            }
            None => {
                // `export NAME` without `=`. POSIX sets the
                // exported bit; the v1 minimal shell has no
                // exported/unexported distinction, so leave
                // an existing entry alone and seed an empty
                // string for an absent one. Either way,
                // post-call `env` will list NAME.
                if arg.is_empty() {
                    if write!(stderr, "sh: export: {arg}: not a valid identifier\n").is_err() {
                        return BuiltinOutcome::IoError;
                    }
                    if stderr.flush().is_err() {
                        return BuiltinOutcome::IoError;
                    }
                } else if !env.contains_key(*arg) {
                    env.insert((*arg).to_string(), String::new());
                }
            }
        }
    }
    BuiltinOutcome::Continue
}

/// Collapse `.` / `..` / repeated `/` segments in `path`,
/// producing an absolute-ish normalised path. Anchored at
/// `/` when the accumulator empties out.
fn normalise(path: &Path) -> PathBuf {
    let mut stack: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::RootDir => {
                stack.clear();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                stack.pop();
            }
            Component::Normal(segment) => {
                stack.push(segment);
            }
            Component::Prefix(_) => {}
        }
    }
    if stack.is_empty() {
        PathBuf::from("/")
    } else {
        let mut out = PathBuf::from("/");
        for seg in stack {
            out.push(seg);
        }
        out
    }
}
