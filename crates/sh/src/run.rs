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
//! * tokenise the line — whitespace outside `'...'` splits
//!   tokens, single-quoted bytes pass through verbatim
//!   (no `$VAR` expansion, no whitespace splitting);
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

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::builtin::{dispatch_builtin, BuiltinOutcome, ShellFlags};

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

/// Run the minimal REPL loop with a fresh empty env map
/// and a fresh default-cleared mode-flag struct.
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
/// entries OR pre-set mode flags (e.g. start the REPL with
/// `errexit` already on, the way `sh -e script.sh` would)
/// call [`run_with_env`] directly.
pub fn run<R: BufRead, W: Write, E: Write>(stdin: R, stdout: W, stderr: E) -> ExitStatus {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut flags = ShellFlags::default();
    run_with_env(stdin, stdout, stderr, &mut env, &mut flags)
}

/// Run the REPL loop against a caller-provided env map and
/// mode-flag struct.
///
/// Test entry point: lets a test pre-seed entries (so
/// `env` / `export` output can be asserted against a
/// known-non-empty map without having to run `export`
/// commands first), pre-set mode flags (so a test can pin
/// "errexit terminates the REPL the moment a non-zero
/// status arrives" without first writing `set -e`), and
/// observe both after the loop returns.
///
/// `flags` is `&mut` rather than `mut` so the caller's
/// struct sees any mutations the in-loop `set` builtin
/// makes (e.g. a test that runs `set -e` then asserts the
/// post-loop flags has `errexit == true`). The same shape
/// `env` already uses.
pub fn run_with_env<R: BufRead, W: Write, E: Write>(
    mut stdin: R,
    mut stdout: W,
    mut stderr: E,
    env: &mut BTreeMap<String, String>,
    flags: &mut ShellFlags,
) -> ExitStatus {
    // Seed cwd from the real process cwd when std gives us
    // one — on wasip1 this may just be `/`. Fall back to
    // `/` when the call fails so tests stay deterministic.
    let mut cwd: PathBuf = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut line = String::new();
    // POSIX `$?` parameter: the exit status of the most
    // recently executed command. Starts at 0 before any
    // command runs; updated AFTER each dispatch from the
    // BuiltinOutcome (Continue → 0, Status(N) → N,
    // IoError → 1 matching POSIX I/O failure convention,
    // NotBuiltin / "command not found" → 127). Blank lines
    // and quote errors leave it untouched (no command ran).
    // Stored in a dedicated `i32` rather than the env map
    // so userland can't `export ?=foo` and break the
    // resolver — the `?` name is reserved by the shell.
    let mut last_status: i32 = 0;

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

        // Tokenise with quote awareness. `'...'` is a literal
        // segment (no whitespace splitting, no `$VAR`
        // expansion); `"..."` is an unquoted segment that
        // preserves whitespace but still runs `expand_vars` over
        // its contents (so `$VAR` / `${VAR}` references DO
        // expand inside double quotes); everything else is a
        // bare unquoted segment that expands and splits on
        // whitespace. Adjacent segments concat into one token
        // (`a'bc'd` → `abcd`; `a"$X"b` → `a` + value-of-X + `b`).
        // An unterminated `'` or `"` is a recoverable error:
        // write to stderr, skip dispatch for this iteration,
        // REPL stays alive.
        let parts = match tokenise_with_quotes(trimmed) {
            Ok(p) => p,
            Err(QuoteError::UnterminatedSingle) => {
                if write!(stderr, "sh: unterminated single quote\n").is_err() {
                    return ExitStatus::IoError;
                }
                if stderr.flush().is_err() {
                    return ExitStatus::IoError;
                }
                continue;
            }
            Err(QuoteError::UnterminatedDouble) => {
                if write!(stderr, "sh: unterminated double quote\n").is_err() {
                    return ExitStatus::IoError;
                }
                if stderr.flush().is_err() {
                    return ExitStatus::IoError;
                }
                continue;
            }
        };
        if parts.is_empty() {
            continue;
        }

        // Assemble each token by walking its parts: literal
        // parts pass through as-is; unquoted parts run through
        // `expand_vars` so `$NAME` / `${NAME}` references resolve
        // against the env map. Under `set -u` (nounset) an
        // unset bare or braced reference returns
        // `Err(ExpandError::NotSet(name))`; we surface that as
        // a POSIX-style stderr diagnostic and terminate the
        // REPL with status 1 — the failing-expansion command
        // never runs (the dispatch_builtin call below is
        // skipped). With nounset off (the default), unset
        // names expand to the empty string and the token is
        // preserved (`echo $UNSET` tokenises as `["echo", ""]`
        // post-expansion). `last_status` is threaded so `$?`
        // resolves to the most recent command's exit code as
        // a decimal string. `flags` is `&*flags` (re-borrowed
        // immutable) because `expand_vars` doesn't need mut.
        let expanded: Vec<String> = {
            let mut acc = Vec::with_capacity(parts.len());
            let mut bail: Option<ExpandError> = None;
            for token in &parts {
                match assemble_token(token, env, last_status, flags) {
                    Ok(s) => acc.push(s),
                    Err(e) => {
                        bail = Some(e);
                        break;
                    }
                }
            }
            match bail {
                Some(ExpandError::NotSet(name)) => {
                    // POSIX `set -u` diagnostic shape — bash
                    // and dash both write `<shellname>:
                    // <name>: parameter not set\n` and exit 1.
                    // Pin the exit-1 termination so userland
                    // can detect nounset failures distinctly
                    // from a generic command failure (which
                    // would surface the command's own exit
                    // status under errexit).
                    if writeln!(stderr, "sh: {name}: parameter not set").is_err() {
                        return ExitStatus::IoError;
                    }
                    if stderr.flush().is_err() {
                        return ExitStatus::IoError;
                    }
                    return ExitStatus::Exit(1);
                }
                Some(ExpandError::Required { name, message }) => {
                    // POSIX `${NAME:?error}` diagnostic shape
                    // — bash and dash both write `<shellname>:
                    // <name>: <message>\n` (or `<shellname>:
                    // <name>: parameter null or not set\n`
                    // when no message was provided) and exit
                    // 1. The exit-1 termination matches the
                    // `set -u` short-circuit shape because
                    // both are expansion-layer failures that
                    // happened before the failing-expansion
                    // command had a chance to run; the
                    // dispatch_builtin call is skipped.
                    let phrase = message
                        .as_deref()
                        .unwrap_or("parameter null or not set");
                    if writeln!(stderr, "sh: {name}: {phrase}").is_err() {
                        return ExitStatus::IoError;
                    }
                    if stderr.flush().is_err() {
                        return ExitStatus::IoError;
                    }
                    return ExitStatus::Exit(1);
                }
                None => {}
            }
            acc
        };
        let expanded_refs: Vec<&str> = expanded.iter().map(|s| s.as_str()).collect();

        // POSIX `set -n` / noexec: under syntax-check mode,
        // skip BOTH the trace and the dispatch — every
        // command is a no-op once expansion succeeds. The
        // expansion-layer errors (quote errors, set -u
        // unset-var failures) ALREADY surfaced above; this
        // arm only short-circuits the post-expansion trace
        // and dispatch path. Critical exemption: the `set`
        // builtin itself ALWAYS runs even under noexec —
        // without that escape hatch `set +n` could never
        // clear the flag once enabled, leaving the user
        // permanently stuck in syntax-check mode. Every
        // other builtin (including `exit`) is silently
        // skipped; the script terminates only on EOF (the
        // validated-successfully path) or on an
        // expansion-layer error (the `set -u` short-circuit
        // above). The check sits BEFORE the trace block so
        // `set -nx` produces zero `+ ` lines — POSIX is
        // undefined on the interaction but "no command runs
        // → no trace" matches the existing trace-skip rule
        // for blank lines and expansion errors.
        if flags.noexec && expanded_refs[0] != "set" {
            continue;
        }

        // POSIX `set -x` / xtrace: write each command to
        // stderr BEFORE executing it, prefixed by `+ ` (the
        // default POSIX PS4 prompt; v1 doesn't customise
        // PS4). The trace fires HERE — AFTER expansion
        // succeeds (so var refs are resolved and the trace
        // shows what actually runs, not the input bytes) and
        // BEFORE dispatch (so the trace precedes the
        // command's own output). Blank lines (the
        // `parts.is_empty()` continue above), quote errors
        // (the `Err(QuoteError::*)` continues above),
        // expansion errors (the `Err(NotSet)` Exit-1
        // short-circuit above), and noexec lines (the
        // `flags.noexec` continue above) all skip this
        // point — none of those cases produces an executed
        // command, so per POSIX none should trace. The
        // first `set -x` line also doesn't trace itself: at
        // this point xtrace is still false from the initial
        // state; the dispatch below flips it for subsequent
        // commands. Conversely `set +x` traces itself
        // because at its trace point xtrace is still true
        // from the previous line; the clear happens during
        // dispatch. Trace failures are intentionally
        // swallowed (`let _`) — a trace write failure
        // shouldn't terminate the REPL because the
        // diagnostic itself is best-effort; the IoError arm
        // below handles fatal stderr write failures from the
        // dispatch path.
        if flags.xtrace {
            let _ = writeln!(stderr, "+ {}", expanded_refs.join(" "));
            let _ = stderr.flush();
        }

        // Capture the dispatch outcome and translate to the
        // POSIX status byte BEFORE handling REPL-terminating
        // cases. `Exit(N)` and `IoError` short-circuit out of
        // the loop so the new `last_status` they would imply
        // is never observed; only the recoverable arms
        // (`Continue`, `Status(N)`, `NotBuiltin`) update the
        // stash for the next command's `$?`.
        match dispatch_builtin(&expanded_refs, &mut cwd, env, flags, &mut stdout, &mut stderr) {
            BuiltinOutcome::Continue => {
                last_status = 0;
            }
            BuiltinOutcome::Status(code) => {
                // Builtin reported a non-zero exit status but
                // the REPL continues (e.g. `false` always
                // returns Status(1)). Stash it for the next
                // command's `$?` expansion.
                last_status = code;
            }
            BuiltinOutcome::Exit(code) => return ExitStatus::Exit(code),
            BuiltinOutcome::IoError => return ExitStatus::IoError,
            BuiltinOutcome::NotBuiltin => {
                // Unknown command → stderr, keep the REPL alive.
                // Use the expanded-token slice so the message
                // reflects what the user actually invoked
                // post-expansion (e.g. an unset `$CMD` produces
                // `sh: command not found: ` with the empty
                // first token, mirroring bash / dash).
                if writeln!(stderr, "sh: command not found: {}", expanded_refs[0]).is_err() {
                    return ExitStatus::IoError;
                }
                if stderr.flush().is_err() {
                    return ExitStatus::IoError;
                }
                // POSIX-mandated: an attempt to run a command
                // that wasn't found produces status 127.
                last_status = 127;
            }
        }

        // POSIX `set -e` / errexit: if the most recent command
        // returned a non-zero status AND errexit is enabled,
        // terminate the REPL with the failing command's exit
        // status. The check fires AFTER `last_status` is
        // updated and BEFORE the next prompt — so any non-zero
        // post-dispatch state (Status(N) from `false`, 127 from
        // a NotBuiltin, etc.) triggers it. `set` itself can't
        // trigger it because the builtin always returns
        // `Continue` (status 0); `Exit(N)` and `IoError` arms
        // already short-circuited above so they bypass the
        // check (intentional: `exit 5` should report Exit(5)
        // unconditionally, not get reinterpreted as an errexit
        // termination). v1 has no `if` / `while` / `until` /
        // `&&` / `||` / `!` constructs, so there are no POSIX
        // exemption contexts to handle — every non-zero status
        // triggers termination when errexit is on.
        if flags.errexit && last_status != 0 {
            // i32 → u8 via the same rem_euclid clamp the `sh`
            // binary's main applies, so multi-byte status codes
            // wrap consistently. 0 was just ruled out by the
            // condition above, so the resulting byte is always
            // non-zero and userland can distinguish errexit
            // termination from a clean EOF.
            let byte = u8::try_from(last_status.rem_euclid(256)).unwrap_or(1);
            return ExitStatus::Exit(byte as i32);
        }
    }
}

/// Why an expansion failed. Surfaced to the dispatch loop
/// so the REPL can write a POSIX-style diagnostic and
/// terminate (or, in future variants, recover).
///
/// `NotSet` is surfaced when `set -u` (nounset) is on AND
/// the bare-`$NAME` or braced-`${NAME}` form references an
/// unset name (the `${NAME:-default}` form is exempt
/// because the default-value path provides the fallback;
/// `$?` / `${?}` is exempt because `last_status` is always
/// defined). `Required` is surfaced by the
/// `${NAME:?error}` form when NAME is unset OR empty —
/// this fires REGARDLESS of the nounset flag (the `:?`
/// form has its own diagnostic mechanism that does not
/// depend on `set -u`). Future expansion errors
/// (recursive-expansion overflow would surface a `Depth`
/// variant) slot in as sibling variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandError {
    /// A bare or braced reference targeted an unset name
    /// while `set -u` was on. Carries the offending name so
    /// the dispatch loop can write `sh: <name>: parameter
    /// not set\n` to stderr.
    NotSet(String),
    /// A `${NAME:?error}` (or `${NAME:?}`) reference
    /// targeted an unset OR empty name. Carries the
    /// offending name plus the optional custom message
    /// so the dispatch loop can write `sh: <name>:
    /// <message>\n` (or `sh: <name>: parameter null or not
    /// set\n` when no message was provided) to stderr.
    /// Fires regardless of `flags.nounset` — the `:?` form
    /// is its own diagnostic mechanism.
    Required {
        /// The variable name (the bytes between `${` and
        /// the `:?` modifier).
        name: String,
        /// The custom error message (the bytes between
        /// `:?` and the closing `}`), or `None` when the
        /// form was the bare `${NAME:?}` with an empty
        /// message — in which case the dispatch loop
        /// substitutes the POSIX default phrase.
        message: Option<String>,
    },
}

/// Expand `$NAME` and `${NAME}` references inside one
/// whitespace-tokenised word against the caller-provided
/// env map. `last_status` is the integer to substitute for
/// `$?` / `${?}` (POSIX last-exit-status parameter).
/// `flags` is the shell's mode-flag state — only the
/// `nounset` field is consulted here.
///
/// Returns `Ok(String)` with the expanded bytes on success,
/// or `Err(ExpandError::NotSet(name))` when `flags.nounset`
/// is true AND a non-default-form reference targets an unset
/// name. The dispatch loop translates the error into a
/// POSIX-style stderr write and a REPL termination.
///
/// Rules (T142 partial — variable substitution slice):
///
/// * `$?` and `${?}` expand to the most recent command's
///   exit status as a decimal string (`0` initially, `0`
///   after success, `N` after `Status(N)`, `127` after a
///   "command not found"). The `?` character is NOT a
///   name-start char (so the existing name-scanner can't
///   match it) — the special case lives BEFORE the name
///   scanner. As with `$1` / positional args in real
///   shells, `?` is the WHOLE expansion (a single byte),
///   so `$?bar` expands to `<status>bar` (not
///   `${?bar}`). `set -u` does NOT affect `$?` because
///   `last_status` is always defined.
/// * `$NAME` where NAME starts with `[A-Za-z_]` and
///   continues with `[A-Za-z0-9_]*` is a variable
///   reference. The match is greedy: `$Xb` reads as
///   `${Xb}`, not `${X}b`. To insert a literal char after
///   an expansion you must use the braced form: `${X}b`.
/// * `${NAME}` is the explicit braced form, semantically
///   identical to the greedy bare form once the name is
///   isolated by `{` / `}`.
/// * `${NAME:-default}` is the POSIX "use default value"
///   parameter expansion. If NAME is unset OR set to the
///   empty string, the expansion is the literal `default`
///   bytes (everything between `:-` and the first `}`). If
///   NAME is set to a non-empty value, the expansion is
///   that value (the default is discarded). The default
///   part is a literal string — `$VAR` references inside
///   the default are NOT recursively expanded in this
///   slice (so `${UNSET:-$X}` produces the literal `$X`,
///   not the value of `X`). Recursive expansion is
///   deferred to a future T142 partial. The `:-` form is
///   exempt from `set -u` because the whole purpose of
///   the default-value form is to provide a fallback for
///   unset vars.
/// * `${NAME:?error}` is the POSIX "error if unset"
///   parameter expansion. If NAME is set AND non-empty,
///   the expansion is the var's value (the error message
///   is discarded). If NAME is unset OR set to the empty
///   string, the function returns
///   `Err(ExpandError::Required { name, message })` —
///   carrying the offending name plus the message bytes
///   (everything between `:?` and the first `}`). The
///   message is wrapped in `Some(_)` when non-empty and
///   `None` for the bare `${NAME:?}` form so the dispatch
///   loop can substitute the POSIX default phrase
///   `parameter null or not set`. Like `:-`, the message
///   is a literal string — `$VAR` references inside are
///   NOT recursively expanded in this slice. Unlike `:-`,
///   the `:?` form fires REGARDLESS of `flags.nounset` —
///   the form has its own diagnostic mechanism that does
///   not depend on `set -u`. The colon prefix means
///   empty-string-set is treated as unset (the no-colon
///   `${NAME?error}` form would distinguish empty-vs-
///   unset; not in v1).
/// * Unset names expand to the empty string when
///   `flags.nounset` is false (the default). When
///   `flags.nounset` is true, an unset name in a bare or
///   braced (non-`:-`) form returns `Err(NotSet(name))`
///   instead — the dispatch loop translates this into a
///   stderr diagnostic and terminates the REPL.
/// * `$$`, `$@`, `$0`, `$1`, etc. are NOT supported in
///   this slice. A `$` followed by anything other than a
///   name-start char (`[A-Za-z_]`), `{`, or `?` is
///   preserved as a literal `$` and the scanner advances
///   one byte.
/// * A trailing `$` at end-of-token is a literal `$`.
/// * Backslash escapes (`\$X`) are NOT handled here —
///   those land in the quoting / escaping slice. A `\$X`
///   in the input becomes literal `\` + the result of
///   expanding `$X`. The leading `\` is preserved.
/// * `${NAME` (open brace, no matching close) is treated
///   as a malformed ref: the literal `${NAME` is
///   preserved up to end-of-token. POSIX errors here; v1
///   prefers leniency over surfacing an error mid-line.
///   `set -u` does NOT trigger on the unterminated case
///   because the malformed-literal preservation IS the
///   recovery path — there's no "unset name" to report.
/// * Other parameter-expansion modifiers are NOT
///   implemented in this slice and are deferred to future
///   T142 partials: `${VAR-default}` (no colon — only-if-
///   unset, treats empty-string as set); `${VAR?error}`
///   (no-colon error-if-unset, treats empty-string as
///   set); `${VAR:=default}` (default-and-assign — would
///   mutate env); `${VAR:+alt}` (alternate-if-set);
///   `${VAR:offset:length}` (substring). Each of these
///   would extend the `:-` / `:?` parser path with a new
///   modifier-byte branch.
pub(crate) fn expand_vars(
    token: &str,
    env: &BTreeMap<String, String>,
    last_status: i32,
    flags: &ShellFlags,
) -> Result<String, ExpandError> {
    let bytes = token.as_bytes();
    let mut out = String::with_capacity(token.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'$' {
            // Multi-byte UTF-8 sequences pass through
            // verbatim — only ASCII `$` introduces an
            // expansion, and the name-charset is ASCII-only,
            // so byte-by-byte scanning is safe for any UTF-8
            // input as long as we copy non-`$` bytes through.
            out.push(b as char);
            i += 1;
            continue;
        }
        // Saw `$`. Look at the next byte to decide.
        let next = bytes.get(i + 1).copied();
        match next {
            Some(b'?') => {
                // POSIX `$?` — last-exit-status. Single-char
                // parameter (like `$1` would be in a shell
                // with positional args), so any byte after
                // the `?` resumes literal copying. `set -u`
                // is intentionally NOT consulted: $? is
                // always defined (last_status is an i32 with
                // a known initial value of 0), so there is
                // no "unset" state to report.
                out.push_str(&last_status.to_string());
                i += 2;
            }
            Some(b'{') => {
                // Braced form: scan to a matching `}`.
                // Nested braces are NOT handled — the close
                // is the FIRST `}` after `${`, matching the
                // simple-brace existing behavior. A literal
                // `}` inside the default would need escaping
                // in a future slice.
                let name_start = i + 2;
                let mut close = name_start;
                while close < bytes.len() && bytes[close] != b'}' {
                    close += 1;
                }
                if close >= bytes.len() {
                    // Unterminated `${...` — preserve literal.
                    // Covers both `${X` and `${X:-default`.
                    // `set -u` does NOT trigger here: the
                    // malformed-literal preservation IS the
                    // recovery, and there's no "unset name"
                    // to report (the parse never resolved a
                    // name in the first place).
                    out.push_str(&token[i..]);
                    return Ok(out);
                }
                // Recognise `${?}` as the explicit braced
                // form of `$?` BEFORE the name / modifier
                // logic — `?` isn't a name char, and there
                // are no parameter-expansion modifiers
                // defined for it. A `${?:-default}` shape
                // is not POSIX-defined, so v1 doesn't
                // bother to support it; only the bare
                // `${?}` falls into this arm. `set -u`
                // skips $? for the same reason as the bare
                // form: last_status is always defined.
                let region = &token[name_start..close];
                if region == "?" {
                    out.push_str(&last_status.to_string());
                    i = close + 1;
                    continue;
                }
                // Look for the `:-` or `:?` modifier inside the
                // brace region. The name part is everything up
                // to the modifier; the modifier-specific tail
                // (default value or error message) is everything
                // after, up to the closing `}`. Without any
                // modifier the whole region is the name (the
                // existing simple-brace behavior). `:-` is
                // checked first because it predates `:?` in the
                // implementation; either modifier consumes the
                // first occurrence in the region (so a name
                // containing a literal `:` would be
                // misinterpreted, but POSIX names cannot contain
                // `:` so this is a non-issue).
                if let Some(sep) = find_colon_dash(region.as_bytes()) {
                    let name = &region[..sep];
                    let default = &region[sep + 2..];
                    let use_default = match env.get(name) {
                        None => true,
                        Some(v) => v.is_empty(),
                    };
                    // `:-` form is EXEMPT from `set -u` —
                    // POSIX-required because the default-value
                    // form's whole purpose is to provide a
                    // fallback for unset vars. So no nounset
                    // check on this branch.
                    if use_default {
                        out.push_str(default);
                    } else {
                        // Safe: matched `Some(v)` above and
                        // the empty branch ruled out the
                        // empty-value case.
                        out.push_str(env.get(name).expect("checked above"));
                    }
                } else if let Some(sep) = find_colon_question(region.as_bytes()) {
                    // POSIX `${NAME:?error}` "error if unset"
                    // form. Like `:-`, the colon prefix means
                    // empty-string-set is treated as unset
                    // (the no-colon `${NAME?error}` form would
                    // distinguish empty-vs-unset; not in v1).
                    // This fires REGARDLESS of `flags.nounset`
                    // — the `:?` form has its own diagnostic
                    // mechanism, so `set -u` is moot here.
                    // When the var IS set and non-empty, the
                    // expansion is the var's value (the
                    // message is discarded). The message is a
                    // literal string — `$VAR` references
                    // inside the message are NOT recursively
                    // expanded in this slice (mirrors the
                    // `:-` default not recursively expanding).
                    let name = &region[..sep];
                    let message = &region[sep + 2..];
                    let fire = match env.get(name) {
                        None => true,
                        Some(v) => v.is_empty(),
                    };
                    if fire {
                        return Err(ExpandError::Required {
                            name: name.to_string(),
                            message: if message.is_empty() {
                                None
                            } else {
                                Some(message.to_string())
                            },
                        });
                    }
                    // Safe: matched `Some(v)` above and the
                    // empty branch ruled out the empty-value
                    // case (otherwise `fire` would be true).
                    out.push_str(env.get(name).expect("checked above"));
                } else if let Some(value) = env.get(region) {
                    out.push_str(value);
                } else if flags.nounset {
                    // Plain `${NAME}` for an unset name +
                    // nounset on → error. The dispatch loop
                    // surfaces this as a stderr diagnostic
                    // and terminates the REPL.
                    return Err(ExpandError::NotSet(region.to_string()));
                }
                // Else: unset name + nounset off → empty
                // string (no append).
                i = close + 1;
            }
            Some(c) if is_name_start(c) => {
                // Bare greedy form: scan name chars.
                let name_start = i + 1;
                let mut name_end = name_start;
                while name_end < bytes.len() && is_name_continue(bytes[name_end]) {
                    name_end += 1;
                }
                let name = &token[name_start..name_end];
                if let Some(value) = env.get(name) {
                    out.push_str(value);
                } else if flags.nounset {
                    // Plain `$NAME` for an unset name +
                    // nounset on → error, mirroring the
                    // braced-form arm above.
                    return Err(ExpandError::NotSet(name.to_string()));
                }
                // Else: unset name + nounset off → empty
                // string (no append).
                i = name_end;
            }
            _ => {
                // `$` followed by a non-name-start char (or
                // end-of-token). Preserve the literal `$`
                // and advance one byte.
                out.push('$');
                i += 1;
            }
        }
    }
    Ok(out)
}

fn is_name_start(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'_')
}

fn is_name_continue(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_')
}

/// Find the byte offset of the first `:-` two-byte sequence
/// inside the given slice, or `None` if absent. Used by the
/// braced-form expander to split `${NAME:-default}` into the
/// name and default halves. Returns the offset of the `:`
/// (so the caller can take `&region[..sep]` for the name and
/// `&region[sep + 2..]` for the default).
fn find_colon_dash(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b':' && bytes[i + 1] == b'-' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Find the byte offset of the first `:?` two-byte sequence
/// inside the given slice, or `None` if absent. Sibling of
/// `find_colon_dash`, used by the braced-form expander to
/// split `${NAME:?error}` into the name and message halves.
/// Returns the offset of the `:` (so the caller can take
/// `&region[..sep]` for the name and `&region[sep + 2..]`
/// for the error message).
fn find_colon_question(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b':' && bytes[i + 1] == b'?' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// One segment of a tokenised word.
///
/// A token is a sequence of these — adjacent segments concat
/// into one token. The distinction between `Literal` and
/// `Unquoted` controls whether `expand_vars` runs over the
/// segment's content during [`assemble_token`]: `Literal`
/// segments come from inside `'...'` and pass through verbatim;
/// `Unquoted` segments come from outside any quoting and DO
/// expand `$NAME` / `${NAME}` references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenPart {
    /// Came from inside `'...'` — pass through, no expansion.
    Literal(String),
    /// Outside any quoting — `expand_vars` runs over the content.
    Unquoted(String),
}

/// Reason an unterminated quote was detected — distinguishes
/// `'...` from `"...` so the caller can surface a kind-aware
/// error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuoteError {
    /// A `'` opened a literal segment that never closed.
    UnterminatedSingle,
    /// A `"` opened an unquoted segment that never closed.
    UnterminatedDouble,
}

/// Internal state of the quote-aware tokeniser: are we currently
/// inside `'...'`, inside `"..."`, or outside any quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteState {
    Outside,
    InSingle,
    InDouble,
}

/// Tokenise `line` into a list of words, where each word is a
/// list of [`TokenPart`] segments.
///
/// Splits on whitespace OUTSIDE quotes; preserves all bytes
/// (including whitespace) inside `'...'` or `"..."` as a
/// single segment. Single-quoted bytes become a `Literal`
/// segment (no `$VAR` expansion at assemble time);
/// double-quoted bytes become an `Unquoted` segment
/// (`$VAR` / `${VAR}` references DO expand at assemble time —
/// only the whitespace-splitting and quote-recognition is
/// suppressed). Adjacency without intervening whitespace
/// concatenates into one word: `a'bc'd` produces
/// `[Unquoted("a"), Literal("bc"), Unquoted("d")]`, and
/// `a"$X"b` produces `[Unquoted("a"), Unquoted("$X"),
/// Unquoted("b")]` — both single words with three parts.
///
/// An unterminated `'` or `"` returns the matching
/// [`QuoteError`] variant; the caller is expected to surface
/// the error and skip dispatch for the current line.
///
/// Backslash escapes inside `"..."` recognise three sequences:
/// `\$` → literal `$` (suppresses `$VAR` expansion at that
/// position), `\"` → literal `"` (does NOT close the double
/// quote), `\\` → literal `\`. Any other `\<char>` is
/// preserved as the two-byte sequence `\<char>` (so `\n`
/// inside `"..."` stays the two bytes `\n`, NOT a newline).
/// To suppress expansion the escaped char is emitted as a
/// fresh `Literal` segment so `expand_vars` (which only sees
/// `Unquoted` segments) cannot reach the bare `$` byte.
/// Outside `"..."` (in unquoted text or inside `'...'`)
/// backslash is preserved as a literal byte — no escape
/// processing.
pub(crate) fn tokenise_with_quotes(line: &str) -> Result<Vec<Vec<TokenPart>>, QuoteError> {
    let mut tokens: Vec<Vec<TokenPart>> = Vec::new();
    let mut current: Vec<TokenPart> = Vec::new();
    let mut buf = String::new();
    let mut state = QuoteState::Outside;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match state {
            QuoteState::InSingle => {
                if c == '\'' {
                    state = QuoteState::Outside;
                    current.push(TokenPart::Literal(core::mem::take(&mut buf)));
                } else {
                    buf.push(c);
                }
            }
            QuoteState::InDouble => {
                if c == '\\' {
                    // Peek the next char to decide if this is a
                    // recognised escape. `\$` / `\"` / `\\` each
                    // emit the escaped char as a `Literal`
                    // segment so `expand_vars` (which runs only
                    // over `Unquoted` segments) cannot see the
                    // bare `$` / `"` / `\` byte. Other `\<char>`
                    // sequences keep both bytes verbatim in the
                    // current `Unquoted` buffer.
                    match chars.peek().copied() {
                        Some(next @ ('$' | '"' | '\\')) => {
                            chars.next();
                            if !buf.is_empty() {
                                current.push(TokenPart::Unquoted(
                                    core::mem::take(&mut buf),
                                ));
                            }
                            current.push(TokenPart::Literal(next.to_string()));
                        }
                        _ => {
                            buf.push('\\');
                        }
                    }
                } else if c == '"' {
                    state = QuoteState::Outside;
                    // Double-quoted bytes go into an Unquoted
                    // segment so assemble_token runs expand_vars
                    // over them. The whitespace-split suppression
                    // already happened (we accumulated the bytes
                    // verbatim through the InDouble branch).
                    current.push(TokenPart::Unquoted(core::mem::take(&mut buf)));
                } else {
                    buf.push(c);
                }
            }
            QuoteState::Outside => {
                if c == '\'' {
                    // Flush any pending unquoted bytes into the
                    // current token; the literal segment opens
                    // fresh. Adjacency with bare text is handled
                    // by NOT closing the current token here.
                    if !buf.is_empty() {
                        current.push(TokenPart::Unquoted(core::mem::take(&mut buf)));
                    }
                    state = QuoteState::InSingle;
                } else if c == '"' {
                    // Same flush-and-open shape as the single
                    // case, but the new segment is also Unquoted
                    // (so `$VAR` inside `"..."` expands). Adjacency
                    // with bare text concatenates the same way:
                    // `a"$X"b` → three Unquoted segments, one word.
                    if !buf.is_empty() {
                        current.push(TokenPart::Unquoted(core::mem::take(&mut buf)));
                    }
                    state = QuoteState::InDouble;
                } else if c.is_whitespace() {
                    // Whitespace closes the current token. Flush
                    // any pending bytes first; only push the token
                    // if it has at least one part.
                    if !buf.is_empty() {
                        current.push(TokenPart::Unquoted(core::mem::take(&mut buf)));
                    }
                    if !current.is_empty() {
                        tokens.push(core::mem::take(&mut current));
                    }
                } else {
                    buf.push(c);
                }
            }
        }
    }

    match state {
        QuoteState::InSingle => return Err(QuoteError::UnterminatedSingle),
        QuoteState::InDouble => return Err(QuoteError::UnterminatedDouble),
        QuoteState::Outside => {}
    }
    if !buf.is_empty() {
        current.push(TokenPart::Unquoted(buf));
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

/// Assemble one token's parts into a final `String`,
/// running `expand_vars` over `Unquoted` segments (with
/// `last_status` and `flags` threaded for `$?` and `set -u`
/// resolution) and passing `Literal` segments through
/// verbatim.
///
/// Returns `Err(ExpandError::NotSet(name))` when any
/// `Unquoted` segment hits an unset name under `set -u`.
/// `Literal` segments never trigger the check (they're the
/// literal-bytes-from-single-quotes path; `expand_vars`
/// never sees them).
pub(crate) fn assemble_token(
    parts: &[TokenPart],
    env: &BTreeMap<String, String>,
    last_status: i32,
    flags: &ShellFlags,
) -> Result<String, ExpandError> {
    let mut out = String::new();
    for part in parts {
        match part {
            TokenPart::Literal(s) => out.push_str(s),
            TokenPart::Unquoted(s) => {
                out.push_str(&expand_vars(s, env, last_status, flags)?);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod expand_tests {
    use super::{expand_vars, ExpandError};
    use crate::builtin::ShellFlags;
    use std::collections::BTreeMap;

    fn env_with(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), (*v).to_string());
        }
        m
    }

    /// Thin wrapper around `expand_vars` that constructs a
    /// fresh default `ShellFlags` (errexit / nounset both
    /// off) and unwraps the `Ok` arm. The tests in this
    /// module that pre-date `set -u` exercise the
    /// nounset-off path; the tests added in the `set -u`
    /// slice (in `tests/set_u.rs`) drive the nounset-on
    /// path through `run_with_env` so the dispatch loop's
    /// stderr / status response is also covered.
    fn ev(token: &str, env: &BTreeMap<String, String>, last_status: i32) -> String {
        expand_vars(token, env, last_status, &ShellFlags::default())
            .expect("expand_vars must not error with nounset off")
    }

    #[test]
    fn unset_var_expands_to_empty() {
        let env = env_with(&[]);
        assert_eq!(ev("$UNSET", &env, 0), "");
    }

    #[test]
    fn set_var_expands_to_value() {
        let env = env_with(&[("X", "hello")]);
        assert_eq!(ev("$X", &env, 0), "hello");
    }

    #[test]
    fn multiple_vars_in_token_concat() {
        // `$X$Y` with `X=hello`, `Y=world` → `helloworld`.
        let env = env_with(&[("X", "hello"), ("Y", "world")]);
        assert_eq!(ev("$X$Y", &env, 0), "helloworld");
    }

    #[test]
    fn braced_form_works() {
        // `${X}b` with `X=hello` → `hellob`. The bare form
        // `$Xb` would look up `Xb` (greedy) and return empty,
        // so this is the only way to insert a literal letter
        // immediately after an expansion.
        let env = env_with(&[("X", "hello")]);
        assert_eq!(ev("${X}b", &env, 0), "hellob");
    }

    #[test]
    fn partial_match_continues_to_name_end() {
        // `a$Xb` is `a` + `$Xb` (greedy). `Xb` is unset, so
        // the whole tail expands to empty: result is `a`.
        let env = env_with(&[("X", "hello")]);
        assert_eq!(ev("a$Xb", &env, 0), "a");
    }

    #[test]
    fn dollar_followed_by_invalid_char_is_literal() {
        // `$1` → `1` is not a name-start char, so the `$` is
        // preserved literal and the `1` is preserved literal.
        let env = env_with(&[]);
        assert_eq!(ev("$1", &env, 0), "$1");
    }

    #[test]
    fn lone_dollar_at_end_is_literal() {
        let env = env_with(&[]);
        assert_eq!(ev("foo$", &env, 0), "foo$");
    }

    #[test]
    fn var_with_underscore_works() {
        // Both `_LEAD` and `MID_DLE` and `TRAIL_` should all
        // be valid identifiers per the `[A-Za-z_][A-Za-z0-9_]*`
        // rule. Test all three shapes in one go.
        let env = env_with(&[("_X", "u"), ("FOO_BAR", "fb"), ("Z_", "zt")]);
        assert_eq!(ev("$_X", &env, 0), "u");
        assert_eq!(ev("$FOO_BAR", &env, 0), "fb");
        assert_eq!(ev("$Z_", &env, 0), "zt");
    }

    #[test]
    fn no_dollar_passes_through_verbatim() {
        let env = env_with(&[("X", "hello")]);
        assert_eq!(ev("plain text", &env, 0), "plain text");
        assert_eq!(ev("", &env, 0), "");
    }

    #[test]
    fn braced_form_with_unset_name_is_empty() {
        let env = env_with(&[]);
        assert_eq!(ev("${MISSING}", &env, 0), "");
        // Surrounding text preserved.
        assert_eq!(ev("a${MISSING}b", &env, 0), "ab");
    }

    #[test]
    fn unterminated_brace_preserves_literal() {
        // `${X` with no closing `}` → preserved as-is.
        let env = env_with(&[("X", "hello")]);
        assert_eq!(ev("${X", &env, 0), "${X");
        assert_eq!(ev("a${X", &env, 0), "a${X");
    }

    #[test]
    fn dollar_dollar_is_literal_in_v1() {
        // `$$` (PID) is NOT supported in this slice — the
        // second `$` is a non-name-start char so the first
        // `$` stays literal and the scanner advances. The
        // second `$` then sees end-of-token, also literal.
        let env = env_with(&[]);
        assert_eq!(ev("$$", &env, 0), "$$");
    }

    #[test]
    fn backslash_dollar_preserved_unprocessed() {
        // The escaping slice will handle `\$X`. For now a
        // literal `\` is preserved, then `$X` expands.
        let env = env_with(&[("X", "v")]);
        assert_eq!(ev("\\$X", &env, 0), "\\v");
    }

    #[test]
    fn default_value_used_when_var_is_unset() {
        // `${UNSET:-fallback}` with no env entry → `fallback`.
        // This is the canonical use-case for `:-`: a guard
        // against missing env vars.
        let env = env_with(&[]);
        assert_eq!(ev("${UNSET:-fallback}", &env, 0), "fallback");
    }

    #[test]
    fn default_value_used_when_var_is_empty() {
        // `${X:-fallback}` with `X=""` → `fallback`. The colon
        // in `:-` is what makes the empty-string case use the
        // default; the no-colon `${X-fallback}` form (NOT
        // implemented this slice) would treat empty-string as
        // "set" and skip the default.
        let env = env_with(&[("X", "")]);
        assert_eq!(ev("${X:-fallback}", &env, 0), "fallback");
    }

    #[test]
    fn actual_value_used_when_var_is_set_to_nonempty() {
        // `${X:-fallback}` with `X=hello` → `hello`. The
        // default is discarded — only consulted when the var
        // is unset or empty.
        let env = env_with(&[("X", "hello")]);
        assert_eq!(ev("${X:-fallback}", &env, 0), "hello");
    }

    #[test]
    fn default_value_can_be_empty() {
        // `${UNSET:-}` is a valid POSIX form — explicit
        // "expand to empty if unset", which is exactly what
        // the bare `${UNSET}` already does. Test exists to
        // guard against the parser rejecting an empty default
        // (it should accept the zero-byte tail after `:-`).
        let env = env_with(&[]);
        assert_eq!(ev("${UNSET:-}", &env, 0), "");
    }

    #[test]
    fn default_value_can_contain_spaces() {
        // `${UNSET:-some default}` → `some default`. Spaces
        // inside the brace region survive because the brace
        // parser scans the whole region as one chunk; this
        // test drives `expand_vars` directly because the
        // unquoted shell tokeniser would split on the space
        // BEFORE expansion runs (so `echo ${UNSET:-some
        // default}` would tokenise as `["echo",
        // "${UNSET:-some", "default}"]`). The work-around at
        // the user level is to wrap the expansion in double
        // quotes, which the existing T142 partial supports.
        let env = env_with(&[]);
        assert_eq!(
            ev("${UNSET:-some default}", &env, 0),
            "some default"
        );
    }

    #[test]
    fn default_value_does_not_recursively_expand_vars() {
        // `${UNSET:-$X}` with `X=hello` → literal `$X`, NOT
        // `hello`. Recursive expansion of the default part is
        // explicitly deferred — keeping the default as a
        // literal string keeps the parser shape simple
        // (one-pass, no recursion) and matches the slice-scope
        // boundary documented in the doc-comment.
        let env = env_with(&[("X", "hello")]);
        assert_eq!(ev("${UNSET:-$X}", &env, 0), "$X");
    }

    #[test]
    fn unterminated_brace_in_default_form_preserves_literal() {
        // `${X:-no_close` with no closing `}` → preserved
        // as-is, mirroring the existing `${X` unterminated
        // behavior. POSIX errors here; v1 prefers leniency
        // over surfacing a mid-line error.
        let env = env_with(&[("X", "hello")]);
        assert_eq!(
            ev("${X:-no_close", &env, 0),
            "${X:-no_close"
        );
    }

    #[test]
    fn default_value_with_set_var_ignores_default() {
        // Regression guard combining the set-var path with a
        // default that contains noise: the default must NOT
        // appear in the output when the var is set.
        let env = env_with(&[("X", "actual")]);
        assert_eq!(
            ev("${X:-this is junk}", &env, 0),
            "actual"
        );
    }

    #[test]
    fn default_value_in_middle_of_token_concatenates() {
        // Surrounding text on either side of the brace region
        // is preserved — the scanner advances past the closing
        // `}` and resumes literal copying.
        let env = env_with(&[]);
        assert_eq!(
            ev("a${UNSET:-mid}b", &env, 0),
            "amidb"
        );
    }

    #[test]
    fn dollar_question_expands_to_status_zero() {
        // `$?` with the canonical initial state (no command
        // run yet) → `"0"`. Pin against the initial-state
        // expectation that `run_with_env` seeds the same way.
        let env = env_with(&[]);
        assert_eq!(ev("$?", &env, 0), "0");
    }

    #[test]
    fn dollar_question_expands_to_arbitrary_status() {
        // `$?` with `last_status=42` → `"42"`. Confirms the
        // decimal-string conversion works for non-zero,
        // multi-digit codes — the most common shape after a
        // `Status(N)` builtin or a `command not found` (127).
        let env = env_with(&[]);
        assert_eq!(ev("$?", &env, 1), "1");
        assert_eq!(ev("$?", &env, 2), "2");
        assert_eq!(ev("$?", &env, 42), "42");
        assert_eq!(ev("$?", &env, 127), "127");
    }

    #[test]
    fn dollar_question_braced_form_works() {
        // `${?}` is the explicit braced form of `$?` —
        // semantically identical. The brace arm short-circuits
        // on `region == "?"` BEFORE the modifier scan so the
        // POSIX-undefined `${?:-default}` shape is not
        // accidentally accepted via the existing `:-` path.
        let env = env_with(&[]);
        assert_eq!(ev("${?}", &env, 0), "0");
        assert_eq!(ev("${?}", &env, 7), "7");
    }

    #[test]
    fn dollar_question_followed_by_literal_chars() {
        // `$?bar` → `<status>bar`. The `?` is the WHOLE
        // expansion (single byte after `$`), so the scanner
        // advances 2 bytes and resumes literal copying — no
        // need for a braced form to insert literal chars
        // after `$?`. This mirrors how `$1` works in real
        // shells with positional args.
        let env = env_with(&[]);
        assert_eq!(ev("$?bar", &env, 0), "0bar");
        assert_eq!(ev("$?bar", &env, 1), "1bar");
    }

    #[test]
    fn dollar_question_in_middle_of_token() {
        // Surrounding text on either side of `$?` is
        // preserved — the scanner emits the literal prefix,
        // then the status digits, then the literal suffix.
        let env = env_with(&[]);
        assert_eq!(ev("[$?]", &env, 42), "[42]");
    }

    #[test]
    fn dollar_question_does_not_consume_env_lookup() {
        // Critical regression guard: even if userland tries
        // to `export ?=foo`, `$?` MUST resolve to the
        // dedicated last_status, NOT the env entry — because
        // last_status is the only argument the resolver
        // consults for `?` / `${?}`. The env map is never
        // checked for the `?` name.
        let env = env_with(&[("?", "foo")]);
        assert_eq!(ev("$?", &env, 5), "5");
        assert_eq!(ev("${?}", &env, 5), "5");
    }

    // --- set -u nounset unit tests (function-level, not
    // dispatch-loop-level — those live in tests/set_u.rs).

    #[test]
    fn nounset_off_unset_var_returns_ok_empty() {
        // Sanity: with the default flags, `$UNSET` returns
        // Ok("") — no error path is taken when nounset is
        // off, even for unset names. Guards against an
        // accidental "always error" regression that would
        // break the seven test files relying on the default
        // empty-string expansion.
        let env = env_with(&[]);
        let flags = ShellFlags::default();
        assert_eq!(expand_vars("$UNSET", &env, 0, &flags).unwrap(), "");
    }

    #[test]
    fn nounset_on_bare_unset_var_returns_not_set_error() {
        // The canonical `set -u` failure: a bare `$NAME` for
        // an unset name returns `Err(NotSet("NAME"))`. The
        // dispatch loop translates this into the stderr
        // diagnostic and the Exit(1) termination.
        let env = env_with(&[]);
        let flags = ShellFlags { errexit: false, nounset: true, xtrace: false, noexec: false };
        let err = expand_vars("$UNSET", &env, 0, &flags).unwrap_err();
        assert_eq!(err, ExpandError::NotSet("UNSET".to_string()));
    }

    #[test]
    fn nounset_on_braced_unset_var_returns_not_set_error() {
        // The braced form `${NAME}` mirrors the bare form's
        // nounset behavior — no `:-` modifier means the
        // braced arm goes through the same error path.
        let env = env_with(&[]);
        let flags = ShellFlags { errexit: false, nounset: true, xtrace: false, noexec: false };
        let err = expand_vars("${UNSET}", &env, 0, &flags).unwrap_err();
        assert_eq!(err, ExpandError::NotSet("UNSET".to_string()));
    }

    #[test]
    fn nounset_on_default_value_form_does_not_error_when_var_unset() {
        // POSIX-required exemption: `${X:-default}` is the
        // fallback-for-unset-vars form, so it MUST NOT trip
        // nounset even when X is unset. Pin this directly
        // because it's the load-bearing semantic that lets
        // the `:-` form remain useful under `set -u`.
        let env = env_with(&[]);
        let flags = ShellFlags { errexit: false, nounset: true, xtrace: false, noexec: false };
        assert_eq!(
            expand_vars("${UNSET:-fallback}", &env, 0, &flags).unwrap(),
            "fallback"
        );
    }

    #[test]
    fn nounset_on_default_value_form_uses_value_when_var_set() {
        // `${X:-fallback}` with X=hello, nounset on → "hello".
        // The set-var branch wins; the default is discarded.
        // Same shape as the nounset-off path, just confirming
        // nounset doesn't accidentally interfere when the var
        // IS set.
        let env = env_with(&[("X", "hello")]);
        let flags = ShellFlags { errexit: false, nounset: true, xtrace: false, noexec: false };
        assert_eq!(
            expand_vars("${X:-fallback}", &env, 0, &flags).unwrap(),
            "hello"
        );
    }

    #[test]
    fn nounset_on_dollar_question_does_not_error() {
        // `$?` is always defined (last_status is an i32 with
        // a known initial value of 0) — `set -u` MUST NOT
        // fire on it. Same for the braced form `${?}`.
        let env = env_with(&[]);
        let flags = ShellFlags { errexit: false, nounset: true, xtrace: false, noexec: false };
        assert_eq!(expand_vars("$?", &env, 0, &flags).unwrap(), "0");
        assert_eq!(expand_vars("${?}", &env, 7, &flags).unwrap(), "7");
    }

    #[test]
    fn nounset_on_set_var_returns_value_via_ok() {
        // Sanity: with nounset on, a SET var still resolves
        // normally — no false-positive error. Pin both the
        // bare and braced forms.
        let env = env_with(&[("X", "hello")]);
        let flags = ShellFlags { errexit: false, nounset: true, xtrace: false, noexec: false };
        assert_eq!(expand_vars("$X", &env, 0, &flags).unwrap(), "hello");
        assert_eq!(expand_vars("${X}", &env, 0, &flags).unwrap(), "hello");
    }

    #[test]
    fn nounset_on_unterminated_brace_does_not_error() {
        // `${X` with no closing `}` is the lenient-literal
        // recovery path — there's no resolved name to report
        // as "unset", so nounset MUST NOT trip. The literal
        // `${X` is returned as-is.
        let env = env_with(&[]);
        let flags = ShellFlags { errexit: false, nounset: true, xtrace: false, noexec: false };
        assert_eq!(expand_vars("${X", &env, 0, &flags).unwrap(), "${X");
    }
}
