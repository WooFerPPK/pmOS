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
        // against the env map. Unset names expand to the empty
        // string but the token is preserved (matches POSIX
        // `set -u`-off semantics; we never silently drop an arg).
        let expanded: Vec<String> = parts
            .iter()
            .map(|token| assemble_token(token, env))
            .collect();
        let expanded_refs: Vec<&str> = expanded.iter().map(|s| s.as_str()).collect();

        match dispatch_builtin(&expanded_refs, &mut cwd, env, &mut stdout, &mut stderr) {
            BuiltinOutcome::Continue => {}
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

/// Expand `$NAME` and `${NAME}` references inside one
/// whitespace-tokenised word against the caller-provided
/// env map.
///
/// Rules (T142 partial — variable substitution slice):
///
/// * `$NAME` where NAME starts with `[A-Za-z_]` and
///   continues with `[A-Za-z0-9_]*` is a variable
///   reference. The match is greedy: `$Xb` reads as
///   `${Xb}`, not `${X}b`. To insert a literal char after
///   an expansion you must use the braced form: `${X}b`.
/// * `${NAME}` is the explicit braced form, semantically
///   identical to the greedy bare form once the name is
///   isolated by `{` / `}`.
/// * Unset names expand to the empty string. We do NOT
///   error and we do NOT remove the token — `echo $UNSET`
///   tokenises as `["echo", ""]` post-expansion. This
///   mirrors POSIX `set -u`-off behaviour, which is the
///   default this slice ships.
/// * `$$`, `$@`, `$0`, `$1`, etc. are NOT supported in
///   this slice. A `$` followed by anything other than a
///   name-start char (`[A-Za-z_]`) or `{` is preserved as
///   a literal `$` and the scanner advances one byte.
/// * A trailing `$` at end-of-token is a literal `$`.
/// * Backslash escapes (`\$X`) are NOT handled here —
///   those land in the quoting / escaping slice. A `\$X`
///   in the input becomes literal `\` + the result of
///   expanding `$X`. The leading `\` is preserved.
/// * `${NAME` (open brace, no matching close) is treated
///   as a malformed ref: the literal `${NAME` is
///   preserved up to end-of-token. POSIX errors here; v1
///   prefers leniency over surfacing an error mid-line.
pub(crate) fn expand_vars(token: &str, env: &BTreeMap<String, String>) -> String {
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
            Some(b'{') => {
                // Braced form: scan to a matching `}`.
                let name_start = i + 2;
                let mut name_end = name_start;
                while name_end < bytes.len() && bytes[name_end] != b'}' {
                    name_end += 1;
                }
                if name_end >= bytes.len() {
                    // Unterminated `${...` — preserve literal.
                    out.push_str(&token[i..]);
                    return out;
                }
                let name = &token[name_start..name_end];
                if let Some(value) = env.get(name) {
                    out.push_str(value);
                }
                // Else: unset name → empty string (no append).
                i = name_end + 1;
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
                }
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
    out
}

fn is_name_start(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'_')
}

fn is_name_continue(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_')
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
/// Backslash escapes inside `"..."` are NOT supported in this
/// slice (a future micro-slice) — a literal `\$X` inside `"..."`
/// will expand `$X` after assemble (the leading `\` is
/// preserved verbatim by the tokeniser, then `expand_vars`
/// processes the rest).
pub(crate) fn tokenise_with_quotes(line: &str) -> Result<Vec<Vec<TokenPart>>, QuoteError> {
    let mut tokens: Vec<Vec<TokenPart>> = Vec::new();
    let mut current: Vec<TokenPart> = Vec::new();
    let mut buf = String::new();
    let mut state = QuoteState::Outside;

    for c in line.chars() {
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
                if c == '"' {
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
/// running `expand_vars` over `Unquoted` segments and
/// passing `Literal` segments through verbatim.
pub(crate) fn assemble_token(parts: &[TokenPart], env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for part in parts {
        match part {
            TokenPart::Literal(s) => out.push_str(s),
            TokenPart::Unquoted(s) => out.push_str(&expand_vars(s, env)),
        }
    }
    out
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

#[cfg(test)]
mod expand_tests {
    use super::expand_vars;
    use std::collections::BTreeMap;

    fn env_with(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), (*v).to_string());
        }
        m
    }

    #[test]
    fn unset_var_expands_to_empty() {
        let env = env_with(&[]);
        assert_eq!(expand_vars("$UNSET", &env), "");
    }

    #[test]
    fn set_var_expands_to_value() {
        let env = env_with(&[("X", "hello")]);
        assert_eq!(expand_vars("$X", &env), "hello");
    }

    #[test]
    fn multiple_vars_in_token_concat() {
        // `$X$Y` with `X=hello`, `Y=world` → `helloworld`.
        let env = env_with(&[("X", "hello"), ("Y", "world")]);
        assert_eq!(expand_vars("$X$Y", &env), "helloworld");
    }

    #[test]
    fn braced_form_works() {
        // `${X}b` with `X=hello` → `hellob`. The bare form
        // `$Xb` would look up `Xb` (greedy) and return empty,
        // so this is the only way to insert a literal letter
        // immediately after an expansion.
        let env = env_with(&[("X", "hello")]);
        assert_eq!(expand_vars("${X}b", &env), "hellob");
    }

    #[test]
    fn partial_match_continues_to_name_end() {
        // `a$Xb` is `a` + `$Xb` (greedy). `Xb` is unset, so
        // the whole tail expands to empty: result is `a`.
        let env = env_with(&[("X", "hello")]);
        assert_eq!(expand_vars("a$Xb", &env), "a");
    }

    #[test]
    fn dollar_followed_by_invalid_char_is_literal() {
        // `$1` → `1` is not a name-start char, so the `$` is
        // preserved literal and the `1` is preserved literal.
        let env = env_with(&[]);
        assert_eq!(expand_vars("$1", &env), "$1");
    }

    #[test]
    fn lone_dollar_at_end_is_literal() {
        let env = env_with(&[]);
        assert_eq!(expand_vars("foo$", &env), "foo$");
    }

    #[test]
    fn var_with_underscore_works() {
        // Both `_LEAD` and `MID_DLE` and `TRAIL_` should all
        // be valid identifiers per the `[A-Za-z_][A-Za-z0-9_]*`
        // rule. Test all three shapes in one go.
        let env = env_with(&[("_X", "u"), ("FOO_BAR", "fb"), ("Z_", "zt")]);
        assert_eq!(expand_vars("$_X", &env), "u");
        assert_eq!(expand_vars("$FOO_BAR", &env), "fb");
        assert_eq!(expand_vars("$Z_", &env), "zt");
    }

    #[test]
    fn no_dollar_passes_through_verbatim() {
        let env = env_with(&[("X", "hello")]);
        assert_eq!(expand_vars("plain text", &env), "plain text");
        assert_eq!(expand_vars("", &env), "");
    }

    #[test]
    fn braced_form_with_unset_name_is_empty() {
        let env = env_with(&[]);
        assert_eq!(expand_vars("${MISSING}", &env), "");
        // Surrounding text preserved.
        assert_eq!(expand_vars("a${MISSING}b", &env), "ab");
    }

    #[test]
    fn unterminated_brace_preserves_literal() {
        // `${X` with no closing `}` → preserved as-is.
        let env = env_with(&[("X", "hello")]);
        assert_eq!(expand_vars("${X", &env), "${X");
        assert_eq!(expand_vars("a${X", &env), "a${X");
    }

    #[test]
    fn dollar_dollar_is_literal_in_v1() {
        // `$$` (PID) is NOT supported in this slice — the
        // second `$` is a non-name-start char so the first
        // `$` stays literal and the scanner advances. The
        // second `$` then sees end-of-token, also literal.
        let env = env_with(&[]);
        assert_eq!(expand_vars("$$", &env), "$$");
    }

    #[test]
    fn backslash_dollar_preserved_unprocessed() {
        // The escaping slice will handle `\$X`. For now a
        // literal `\` is preserved, then `$X` expands.
        let env = env_with(&[("X", "v")]);
        assert_eq!(expand_vars("\\$X", &env), "\\v");
    }
}
