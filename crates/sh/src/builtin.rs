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
//! `set`, `test` / `[`. Anything else returns
//! [`BuiltinOutcome::NotBuiltin`] so the REPL can fall through
//! to the "command not found" path until a future slice wires
//! `proc_spawn` into userland.

use core::str::FromStr;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Shell-wide mode flags toggled at runtime by `set`.
///
/// Surfaces the POSIX `errexit` (`set -e` / `set -o
/// errexit`), `nounset` (`set -u` / `set -o nounset`),
/// `xtrace` (`set -x` / `set -o xtrace`), and `noexec`
/// (`set -n` / `set -o noexec`) modes. Future flags slot in
/// as sibling fields on this struct without changing any
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
    /// `set -x` / `set -o xtrace`: write each command to
    /// stderr BEFORE executing it, prefixed by `+ ` (the
    /// default POSIX PS4 prompt). The trace shows the
    /// EXPANDED tokens joined by single spaces, NOT the
    /// original input bytes — so `echo $X` with `X=hello`
    /// traces as `+ echo hello`. The trace fires AFTER
    /// expansion succeeds (so var refs are resolved) but
    /// BEFORE dispatch (so it precedes execution). Blank
    /// lines, quote errors, and expansion errors all skip
    /// the trace because they short-circuit before reaching
    /// the trace point — POSIX-aligned because none of those
    /// cases produce an executed command. The first `set -x`
    /// line does NOT trace itself: at its trace point xtrace
    /// is still false (the dispatch hasn't run yet); only
    /// subsequent commands trace. Conversely `set +x` DOES
    /// trace itself because at ITS trace point xtrace is
    /// still true from the previous line; the clear happens
    /// during dispatch. Off by default — POSIX shells start
    /// with xtrace cleared.
    pub xtrace: bool,
    /// `set -n` / `set -o noexec`: parse and tokenise each
    /// line but do NOT dispatch any command — every command
    /// is a no-op syntax-check. Useful for "lint" mode: feed
    /// a script through `sh -n` to catch quote / expansion
    /// errors without executing anything. Variable expansion
    /// still runs (so `set -nu` still surfaces unset-var
    /// errors at expansion time before the dispatch
    /// short-circuit fires). The trace block ALSO skips
    /// under noexec because no command executes — under
    /// `set -nx` no `+ ` lines appear. Critical exemption:
    /// the `set` builtin itself ALWAYS runs even under
    /// noexec (otherwise `set +n` could never disable the
    /// flag once enabled — practical necessity). Every
    /// other builtin including `exit` is silently skipped;
    /// the script terminates only on EOF (the
    /// validated-successfully exit path) or on an
    /// expansion-layer error (the `set -u` short-circuit).
    /// Off by default — POSIX shells start with noexec
    /// cleared.
    pub noexec: bool,
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
        "test" => evaluate_test(&tokens[1..], false, stderr),
        "[" => evaluate_test(&tokens[1..], true, stderr),
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
/// v1 supports the `errexit`, `nounset`, `xtrace`, and
/// `noexec` flags in two equivalent shapes each:
///
/// * `set -e` / `set -o errexit` — turn errexit ON.
/// * `set +e` / `set +o errexit` — turn errexit OFF.
/// * `set -u` / `set -o nounset` — turn nounset ON.
/// * `set +u` / `set +o nounset` — turn nounset OFF.
/// * `set -x` / `set -o xtrace` — turn xtrace ON.
/// * `set +x` / `set +o xtrace` — turn xtrace OFF.
/// * `set -n` / `set -o noexec` — turn noexec ON.
/// * `set +n` / `set +o noexec` — turn noexec OFF.
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
/// check can never trigger on the `set` line. xtrace
/// follows a similar pattern: the trace point is in the
/// dispatch loop AFTER expansion and BEFORE dispatch, so
/// the `set -x` line itself prints under whatever value
/// xtrace had BEFORE the line was processed — the first
/// `set -x` doesn't trace itself; subsequent commands do.
/// noexec is the load-bearing case: under `set -n` every
/// dispatch is short-circuited EXCEPT the `set` builtin
/// itself — without that exemption `set +n` could never
/// clear the flag, leaving the user permanently stuck in
/// syntax-check mode.
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
            "-x" => flags.xtrace = true,
            "+x" => flags.xtrace = false,
            "-n" => flags.noexec = true,
            "+n" => flags.noexec = false,
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
                    "xtrace" => flags.xtrace = on,
                    "noexec" => flags.noexec = on,
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

/// POSIX `test` / `[` conditional expression evaluator.
///
/// Shared between the `test` and `[` dispatch arms. The
/// `for_bracket` flag flips the trailing-`]` requirement:
///
/// * `for_bracket = true` (the `[` invocation): the LAST arg
///   MUST be `]`. If absent, the call returns
///   `BuiltinOutcome::Status(2)` with `[: missing ]` on
///   stderr. The trailing `]` is stripped before the
///   arg-arity matrix runs.
/// * `for_bracket = false` (the `test` invocation): the LAST
///   arg MUST NOT be `]`. The `]` token is treated as a
///   regular operand — and since `]` is not a valid operator
///   anywhere in POSIX `test` syntax, the resulting
///   expression usually fails the unary / binary operator
///   lookup and produces a usage error.
///
/// After the `]` handling, the arity matrix is:
///
/// * 0 args → `Status(1)` (POSIX-defined: empty `test`
///   evaluates to false).
/// * 1 arg → `Status(0)` if non-empty, else `Status(1)`
///   (single-arg shorthand for `[ -n STR ]`).
/// * 2 args → either `! EXPR` (negate the 1-arg form) or a
///   unary operator (`-z` / `-n`) plus operand.
/// * 3 args → either `! EXPR` (negate the 2-arg form) or a
///   binary form `STR1 OP STR2` (string `=` / `!=` or integer
///   `-eq` / `-ne` / `-lt` / `-le` / `-gt` / `-ge`).
/// * 4 args → must start with `!` (negate the 3-arg form);
///   any other 4-arg shape is `Status(2)` "too many
///   arguments".
/// * 5+ args → `Status(2)` "too many arguments".
///
/// Negation rules: `! EXPR` inverts a Status(0) → Status(1)
/// and a Status(1) → Status(0). A Status(2) usage error from
/// the inner expression is NOT inverted; it propagates as
/// Status(2) (POSIX-aligned: a syntax error in the inner
/// expression is still a syntax error of the whole).
///
/// Integer ops require BOTH operands to parse as `i64` via
/// `str::parse::<i64>`; non-integer operands produce a
/// `<command>: <op>: integer expression expected: <arg>`
/// diagnostic on stderr and return `Status(2)`.
///
/// File-test operators (`-e`, `-f`, `-d`, `-r`, `-w`, `-x`,
/// `-s`) live in the 2-arg branch as well — they query
/// `std::fs::metadata` for the operand path. `Err(_)` from
/// metadata (file missing, unreachable, permission denied at
/// the lookup itself) maps to `Status(1)` for ALL file-test
/// ops; `Ok(meta)` then gates the answer on the relevant
/// metadata field (`is_file()`, `is_dir()`, `len() > 0`, or
/// the owner-permission bit via `PermissionsExt::mode()`).
/// File-test ops produce NO stderr output for "missing" /
/// "unreachable" / "no permission" — those are normal `false`
/// results, not usage errors. Only structural mistakes
/// (wrong arg count, unknown operator) write to stderr.
///
/// Binary file-test operators (`-nt`, `-ot`, `-ef`) live in
/// the 3-arg branch alongside the integer ops. Each takes two
/// paths and queries `std::fs::metadata` for both, then
/// compares modification time (`-nt`, `-ot`) or device + inode
/// (`-ef`). Missing-path semantics follow bash: `PATH1 -nt
/// PATH2` is true if PATH1 is newer (or PATH2 is missing —
/// "newer than nothing"); false if PATH1 is missing. `-ot` is
/// the mirror. `-ef` is false if either path is missing.
///
/// Deferred (out of v1 scope): symlink-test operators (`-L`,
/// `-h` — need `lstat` instead of `stat`); FIFO / socket /
/// device / setuid-setgid-sticky tests (`-p`, `-S`, `-b`,
/// `-c`, `-g`, `-u`, `-k` — need filesystem features the v1
/// substrate doesn't model); terminal-test operator (`-t fd`
/// — needs FD tracking); ownership tests (`-N`, `-O`, `-G` —
/// need uid/gid surfacing); compound expressions with `-a` /
/// `-o` / `(` / `)` (POSIX-deprecated, scripts should use
/// `&&` / `||` at the shell level which v1 also lacks);
/// bash-extended operators (`==`, `=~`, `<` / `>` for string
/// ordering, the `[[` double-bracket form). Each unrecognised
/// operator surfaces as `unknown unary operator: <X>` or
/// `unknown binary operator: <X>` (NOT a silent failure) so
/// users get a clear "this slice didn't implement that yet"
/// signal rather than mysterious wrong answers.
fn evaluate_test<E: Write>(
    raw_args: &[&str],
    for_bracket: bool,
    stderr: &mut E,
) -> BuiltinOutcome {
    let command = if for_bracket { "[" } else { "test" };

    // Strip the trailing `]` for the `[` form; reject when
    // missing. The `test` form rejects nothing here — a
    // trailing `]` in `test` is just a regular operand.
    let args: &[&str] = if for_bracket {
        match raw_args.last() {
            Some(&"]") => &raw_args[..raw_args.len() - 1],
            _ => {
                let _ = writeln!(stderr, "{command}: missing ]");
                let _ = stderr.flush();
                return BuiltinOutcome::Status(2);
            }
        }
    } else {
        raw_args
    };

    // Negation: `! EXPR` strips the leading `!` and inverts
    // the result of the inner expression. Usage errors
    // (Status(2)) propagate without inversion.
    if let Some((&"!", rest)) = args.split_first() {
        return match evaluate_test_expr(rest, command, stderr) {
            BuiltinOutcome::Status(0) => BuiltinOutcome::Status(1),
            BuiltinOutcome::Status(1) => BuiltinOutcome::Status(0),
            other => other,
        };
    }

    evaluate_test_expr(args, command, stderr)
}

/// Inner evaluator without the `]`-stripping or top-level
/// negation handling — those live in [`evaluate_test`] so
/// negation can wrap any of the 0/1/2/3-arg forms uniformly.
fn evaluate_test_expr<E: Write>(
    args: &[&str],
    command: &str,
    stderr: &mut E,
) -> BuiltinOutcome {
    match args.len() {
        0 => BuiltinOutcome::Status(1),
        1 => {
            if args[0].is_empty() {
                BuiltinOutcome::Status(1)
            } else {
                BuiltinOutcome::Status(0)
            }
        }
        2 => {
            // Unary operator + operand. `-z STR` (true if
            // empty), `-n STR` (true if non-empty), plus the
            // POSIX file-test ops (`-e`, `-f`, `-d`, `-r`,
            // `-w`, `-x`, `-s`) which delegate to
            // `evaluate_file_test`. Anything else is "unknown
            // unary operator".
            let (op, operand) = (args[0], args[1]);
            match op {
                "-z" => bool_to_status(operand.is_empty()),
                "-n" => bool_to_status(!operand.is_empty()),
                "-e" | "-f" | "-d" | "-r" | "-w" | "-x" | "-s" => {
                    evaluate_file_test(op, operand)
                }
                other => {
                    let _ = writeln!(stderr, "{command}: unknown unary operator: {other}");
                    let _ = stderr.flush();
                    BuiltinOutcome::Status(2)
                }
            }
        }
        3 => {
            // Binary operator. String ops `=` / `!=` first
            // because they take any operands; integer ops
            // require both args parse as `i64`. Binary
            // file-test ops (`-nt`, `-ot`, `-ef`) compare two
            // paths' filesystem metadata via
            // `evaluate_binary_file_test`.
            let (lhs, op, rhs) = (args[0], args[1], args[2]);
            match op {
                "=" => bool_to_status(lhs == rhs),
                "!=" => bool_to_status(lhs != rhs),
                "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
                    let lhs_n = match lhs.parse::<i64>() {
                        Ok(n) => n,
                        Err(_) => return integer_expected(command, op, lhs, stderr),
                    };
                    let rhs_n = match rhs.parse::<i64>() {
                        Ok(n) => n,
                        Err(_) => return integer_expected(command, op, rhs, stderr),
                    };
                    let result = match op {
                        "-eq" => lhs_n == rhs_n,
                        "-ne" => lhs_n != rhs_n,
                        "-lt" => lhs_n < rhs_n,
                        "-le" => lhs_n <= rhs_n,
                        "-gt" => lhs_n > rhs_n,
                        "-ge" => lhs_n >= rhs_n,
                        _ => unreachable!("op verified above"),
                    };
                    bool_to_status(result)
                }
                "-nt" | "-ot" | "-ef" => evaluate_binary_file_test(op, lhs, rhs),
                other => {
                    let _ = writeln!(stderr, "{command}: unknown binary operator: {other}");
                    let _ = stderr.flush();
                    BuiltinOutcome::Status(2)
                }
            }
        }
        _ => {
            let _ = writeln!(stderr, "{command}: too many arguments");
            let _ = stderr.flush();
            BuiltinOutcome::Status(2)
        }
    }
}

/// Map a Rust `bool` to the POSIX `test` outcome:
/// `true` → `Status(0)`, `false` → `Status(1)`.
fn bool_to_status(value: bool) -> BuiltinOutcome {
    if value {
        BuiltinOutcome::Status(0)
    } else {
        BuiltinOutcome::Status(1)
    }
}

/// Emit the POSIX-flavoured "integer expression expected"
/// usage error and return `Status(2)`. Shared between the
/// `lhs` / `rhs` parse-failure paths.
fn integer_expected<E: Write>(
    command: &str,
    op: &str,
    arg: &str,
    stderr: &mut E,
) -> BuiltinOutcome {
    let _ = writeln!(stderr, "{command}: {op}: integer expression expected: {arg}");
    let _ = stderr.flush();
    BuiltinOutcome::Status(2)
}

/// Evaluate a POSIX file-test unary operator (`-e`, `-f`,
/// `-d`, `-r`, `-w`, `-x`, `-s`) against `path`. ALL ops
/// return `Status(1)` when `std::fs::metadata(path)` errors
/// — the path is missing, unreachable, or otherwise
/// unstattable, which POSIX defines as a `false` outcome
/// rather than a usage error. NO stderr output for the
/// metadata-error path; the caller's `Status(1)` is the
/// signal. The single helper handles the common metadata
/// fetch + error-mapping once and dispatches on the
/// operator inside the inner match.
///
/// Path handling: the operand is passed verbatim to
/// `std::fs::metadata`. Variable expansion, glob expansion,
/// quote stripping have all already happened in the upstream
/// dispatcher, so the operand here is the literal byte
/// string the user typed (post-expansion). An empty operand
/// is a valid path string but always fails metadata lookup
/// (POSIX/Linux refuse `stat("")`), giving `Status(1)`,
/// which matches bash / dash behaviour for `[ -e "" ]`.
///
/// Permissions semantics for `-r` / `-w` / `-x`: this slice
/// uses `std::os::unix::fs::PermissionsExt::mode()` to read
/// the owner-permission bits (mode & 0o400 / 0o200 / 0o100).
/// On the native test target (`x86_64-unknown-linux-gnu`) the
/// trait is freely available; on `wasm32-wasip1` the trait
/// is also surfaced because wasip1's `std` shim re-exports
/// the Unix permission model. The owner-bit check is
/// deliberately simpler than the full `access(2)` "would
/// this user actually be able to open it" semantic — POSIX
/// `test -r` / `-w` / `-x` is allowed to be conservative
/// here and the v1 substrate doesn't model multiple users
/// anyway. The check matches the typical POSIX-utility
/// shell-test approximation of "does the owner bit allow
/// it" rather than the full effective-uid `access(2)` call.
fn evaluate_file_test(op: &str, path: &str) -> BuiltinOutcome {
    use std::os::unix::fs::PermissionsExt;

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return BuiltinOutcome::Status(1),
    };
    let pass = match op {
        "-e" => true,
        "-f" => meta.is_file(),
        "-d" => meta.is_dir(),
        "-s" => meta.len() > 0,
        "-r" => meta.permissions().mode() & 0o400 != 0,
        "-w" => meta.permissions().mode() & 0o200 != 0,
        "-x" => meta.permissions().mode() & 0o100 != 0,
        _ => unreachable!("evaluate_file_test called with non-file-test op {op}"),
    };
    bool_to_status(pass)
}

/// Evaluate a POSIX binary file-test operator (`-nt`, `-ot`,
/// `-ef`) against two paths. Each operator queries
/// `std::fs::metadata` for BOTH paths and compares the
/// resulting metadata.
///
/// `-nt` (newer-than): true if PATH1's mtime > PATH2's mtime.
/// Missing-path semantics follow bash: false if PATH1 is
/// missing (a non-existent file is "older than" anything);
/// true if PATH2 is missing but PATH1 exists ("newer than
/// nothing"); false if both are missing (no comparison
/// possible — pin "missing != newer" so a typo'd path doesn't
/// silently succeed).
///
/// `-ot` (older-than): the mirror of `-nt`. True if PATH1's
/// mtime < PATH2's mtime. False if PATH2 is missing but PATH1
/// exists (existing files are not "older than nothing"); true
/// if PATH1 is missing but PATH2 exists; false if both are
/// missing.
///
/// `-ef` (equal-files): true if both paths refer to the same
/// underlying file (same device + inode). The hard-link /
/// same-path identity check. False if EITHER path is missing
/// (including the both-missing case — pin "missing == missing"
/// doesn't accidentally return true). Uses
/// `std::os::unix::fs::MetadataExt::dev()` and `ino()` which
/// are surfaced on both `x86_64-unknown-linux-gnu` (the native
/// test target) and `wasm32-wasip1` (the wasip1 std shim
/// emulates the Unix metadata model). No fallback to
/// path-canonicalisation is needed because the unix metadata
/// shape is universally available across our supported
/// targets.
///
/// Modification-time comparison uses
/// `std::fs::Metadata::modified()` which returns
/// `Result<SystemTime, io::Error>`. `SystemTime` implements
/// `Ord` on Unix targets so direct `>` / `<` comparison of
/// the `Result` values via `Option`-coerced `.ok()` produces
/// the expected ordering — equal mtimes count as
/// not-newer-and-not-older (POSIX-aligned), so two files
/// touched at the same instant are neither newer nor older
/// than each other and `-nt` / `-ot` both return false.
///
/// Diagnostic output: NONE. As with the unary file-test ops,
/// any "missing" / "unreachable" / "permission denied at the
/// metadata lookup" outcome is a normal `false` result, not a
/// usage error.
fn evaluate_binary_file_test(op: &str, path1: &str, path2: &str) -> BuiltinOutcome {
    let meta1 = std::fs::metadata(path1);
    let meta2 = std::fs::metadata(path2);
    match op {
        "-nt" => match (&meta1, &meta2) {
            (Ok(m1), Ok(m2)) => bool_to_status(m1.modified().ok() > m2.modified().ok()),
            (Ok(_), Err(_)) => bool_to_status(true),
            (Err(_), _) => bool_to_status(false),
        },
        "-ot" => match (&meta1, &meta2) {
            (Ok(m1), Ok(m2)) => bool_to_status(m1.modified().ok() < m2.modified().ok()),
            (Err(_), Ok(_)) => bool_to_status(true),
            (_, Err(_)) => bool_to_status(false),
        },
        "-ef" => match (&meta1, &meta2) {
            (Ok(m1), Ok(m2)) => {
                use std::os::unix::fs::MetadataExt;
                bool_to_status(m1.dev() == m2.dev() && m1.ino() == m2.ino())
            }
            _ => bool_to_status(false),
        },
        _ => unreachable!("evaluate_binary_file_test called with non-file-test op {op}"),
    }
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
