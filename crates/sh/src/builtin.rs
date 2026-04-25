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
//! `echo`, `exit`, `cd`, `pwd`, `env`, `export`, `unset`,
//! `set`. Anything else returns [`BuiltinOutcome::NotBuiltin`]
//! so the REPL can fall through to the "command not found"
//! path until a future slice wires `proc_spawn` into userland.

use core::str::FromStr;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Shell-wide mode flags toggled at runtime by `set`.
///
/// Surfaces the POSIX `errexit` (`set -e` / `set -o
/// errexit`) and `nounset` (`set -u` / `set -o nounset`)
/// modes. Future flags (`set -x` for "trace each
/// command", `set -n` for "syntax-check only") slot in as
/// sibling fields on this struct without changing any
/// existing call site — `dispatch_builtin` already takes
/// `flags: &mut ShellFlags`, so a new mode is one extra
/// field plus one extra `set` arm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShellFlags {
    /// `set -e` / `set -o errexit`: terminate the REPL on
    /// the first non-zero status. Off by default — POSIX
    /// shells start with errexit cleared.
    pub errexit: bool,
    /// `set -u` / `set -o nounset`: treat references to
    /// unset variables as errors. When true, expanding `$X`
    /// or `${X}` for an unset name writes `sh: <name>:
    /// parameter not set\n` to stderr and terminates the
    /// REPL with status 1. The `${X:-default}` default-value
    /// form is exempt (POSIX-required — its whole purpose is
    /// to provide a fallback for unset vars). `$?` is also
    /// never affected because it has its own resolver and is
    /// always defined. Off by default — POSIX shells start
    /// with nounset cleared.
    pub nounset: bool,
}

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
///
/// `flags` is the shell's mode-flag state (see
/// [`ShellFlags`]). The `set` builtin mutates it; every
/// other builtin ignores it. Threading `&mut ShellFlags`
/// through every dispatch lets a future flag (`set -u` /
/// `set -x` / `set -n`) be observed inside any builtin
/// without revisiting this signature.
pub(crate) fn dispatch_builtin<W: Write, E: Write>(
    tokens: &[&str],
    cwd: &mut PathBuf,
    env: &mut BTreeMap<String, String>,
    flags: &mut ShellFlags,
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
        "set" => builtin_set(&tokens[1..], flags, stderr),
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

/// POSIX `set` builtin — mode-flag toggle.
///
/// v1 supports the `errexit` and `nounset` flags in two
/// equivalent shapes each:
///
/// * `set -e` / `set -o errexit` — turn errexit ON.
/// * `set +e` / `set +o errexit` — turn errexit OFF.
/// * `set -u` / `set -o nounset` — turn nounset ON.
/// * `set +u` / `set +o nounset` — turn nounset OFF.
///
/// `set` with no args is a no-op in v1 (POSIX defines this
/// as "print every shell variable" but the v1 `env` builtin
/// already covers the variable-listing use case, so the
/// no-arg `set` path is reserved for future expansion
/// without breaking compatibility). Unknown shapes write
/// `sh: set: <arg>: invalid option\n` to stderr and return
/// `Continue` so the REPL stays alive.
///
/// Always returns `BuiltinOutcome::Continue` — `set` itself
/// must never have a non-zero exit status, otherwise an
/// active errexit flag would terminate the REPL the moment
/// the user typed `set -e`. The `Continue` arm in the REPL
/// loop sets `last_status = 0`, so the post-dispatch
/// errexit check naturally skips `set` invocations. The
/// same property holds for nounset: `set -u` doesn't itself
/// reference any variable, so the expansion-layer nounset
/// check can never trigger on the `set` line.
fn builtin_set<E: Write>(
    args: &[&str],
    flags: &mut ShellFlags,
    stderr: &mut E,
) -> BuiltinOutcome {
    // No args: POSIX prints every shell variable. v1 defers
    // this — `env` already lists exported entries and there's
    // no separate "shell variable" namespace yet. Return
    // Continue so the no-arg path is a no-op rather than an
    // error; future slices can wire variable-listing here
    // without breaking existing scripts.
    if args.is_empty() {
        return BuiltinOutcome::Continue;
    }
    // Walk args left-to-right. Each recognised toggle
    // updates the flags struct; the first unrecognised arg
    // emits a diagnostic and short-circuits (POSIX `set`
    // doesn't process subsequent args after a bad option).
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "-e" => flags.errexit = true,
            "+e" => flags.errexit = false,
            "-u" => flags.nounset = true,
            "+u" => flags.nounset = false,
            "-o" | "+o" => {
                // `set -o NAME` / `set +o NAME` — the
                // long-option form. The polarity (`-` vs
                // `+`) and the NAME live in adjacent args.
                let on = args[i] == "-o";
                let Some(name) = args.get(i + 1) else {
                    // Bare `set -o` without a name. POSIX
                    // would list every option's state; v1
                    // defers (mirroring the bare-`set`
                    // no-op convention).
                    return BuiltinOutcome::Continue;
                };
                match *name {
                    "errexit" => flags.errexit = on,
                    "nounset" => flags.nounset = on,
                    other => {
                        let _ = writeln!(
                            stderr,
                            "sh: set: {other}: invalid option name"
                        );
                        let _ = stderr.flush();
                        return BuiltinOutcome::Continue;
                    }
                }
                i += 1;
            }
            other => {
                let _ = writeln!(stderr, "sh: set: {other}: invalid option");
                let _ = stderr.flush();
                return BuiltinOutcome::Continue;
            }
        }
        i += 1;
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
