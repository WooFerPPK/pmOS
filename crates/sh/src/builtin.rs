//! Builtin command implementations for the REPL driver.
//!
//! Lifted out of [`crate::run`] so the file layout matches
//! the T144 spec hint (`crates/sh/src/builtin.rs`). No
//! behaviour change — every function and signature here is
//! identical to its previous home in `run.rs`. The REPL in
//! `run.rs` consumes [`BuiltinOutcome`] and
//! [`dispatch_builtin`] from this module the same way it
//! used to consume them as same-file items.
//!
//! The set is the minimal v1 dispatch: `:`, `true`, `false`,
//! `echo`, `exit`, `cd`, `pwd`, `env`, `export`, `unset`.
//! Anything else returns [`BuiltinOutcome::NotBuiltin`] so
//! the REPL can fall through to the "command not found" path
//! until a future slice wires `proc_spawn` into userland.

use core::str::FromStr;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Outcome of dispatching one builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinOutcome {
    /// Builtin ran; continue the REPL.
    Continue,
    /// Builtin ran with this non-zero exit status; the REPL
    /// continues. Distinct from [`BuiltinOutcome::Exit`],
    /// which terminates the whole shell. Used by `false` so
    /// `cmd || false` semantics are recoverable. A future
    /// slice will surface this as `$?` for the next command.
    Status(i32),
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
        ":" => BuiltinOutcome::Continue,
        "true" => BuiltinOutcome::Continue,
        "false" => BuiltinOutcome::Status(1),
        "echo" => builtin_echo(&tokens[1..], stdout),
        "exit" => builtin_exit(&tokens[1..], stderr),
        "cd" => builtin_cd(&tokens[1..], cwd),
        "pwd" => builtin_pwd(cwd, stdout),
        "env" => builtin_env(&tokens[1..], env, stdout, stderr),
        "export" => builtin_export(&tokens[1..], env, stdout, stderr),
        "unset" => builtin_unset(&tokens[1..], env, stderr),
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

fn builtin_unset<E: Write>(
    args: &[&str],
    env: &mut BTreeMap<String, String>,
    stderr: &mut E,
) -> BuiltinOutcome {
    if args.is_empty() {
        if writeln!(stderr, "sh: unset: usage: unset NAME...").is_err() {
            return BuiltinOutcome::IoError;
        }
        if stderr.flush().is_err() {
            return BuiltinOutcome::IoError;
        }
        return BuiltinOutcome::Continue;
    }
    for arg in args {
        if arg.is_empty() || arg.contains('=') {
            if writeln!(stderr, "sh: unset: {arg}: not a valid identifier").is_err() {
                return BuiltinOutcome::IoError;
            }
            if stderr.flush().is_err() {
                return BuiltinOutcome::IoError;
            }
            continue;
        }
        env.remove(*arg);
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
