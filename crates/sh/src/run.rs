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

use crate::builtin::{dispatch_builtin, BuiltinOutcome};

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
///   deferred to a future T142 partial.
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
/// * Other parameter-expansion modifiers are NOT
///   implemented in this slice and are deferred to future
///   T142 partials: `${VAR-default}` (no colon — only-if-
///   unset, treats empty-string as set); `${VAR:=default}`
///   (default-and-assign — would mutate env);
///   `${VAR:?error}` (error-if-unset); `${VAR:+alt}`
///   (alternate-if-set); `${VAR:offset:length}` (substring).
///   Each of these would extend the `:-` parser path with a
///   new modifier-byte branch.
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
                    out.push_str(&token[i..]);
                    return out;
                }
                // Look for the `:-` modifier inside the brace
                // region. The name part is everything up to
                // the modifier; the default part is everything
                // after, up to the closing `}`. Without the
                // modifier the whole region is the name (the
                // existing simple-brace behavior).
                let region = &token[name_start..close];
                if let Some(sep) = find_colon_dash(region.as_bytes()) {
                    let name = &region[..sep];
                    let default = &region[sep + 2..];
                    let use_default = match env.get(name) {
                        None => true,
                        Some(v) => v.is_empty(),
                    };
                    if use_default {
                        out.push_str(default);
                    } else {
                        // Safe: matched `Some(v)` above and
                        // the empty branch ruled out the
                        // empty-value case.
                        out.push_str(env.get(name).expect("checked above"));
                    }
                } else {
                    if let Some(value) = env.get(region) {
                        out.push_str(value);
                    }
                    // Else: unset name → empty string (no append).
                }
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

    #[test]
    fn default_value_used_when_var_is_unset() {
        // `${UNSET:-fallback}` with no env entry → `fallback`.
        // This is the canonical use-case for `:-`: a guard
        // against missing env vars.
        let env = env_with(&[]);
        assert_eq!(expand_vars("${UNSET:-fallback}", &env), "fallback");
    }

    #[test]
    fn default_value_used_when_var_is_empty() {
        // `${X:-fallback}` with `X=""` → `fallback`. The colon
        // in `:-` is what makes the empty-string case use the
        // default; the no-colon `${X-fallback}` form (NOT
        // implemented this slice) would treat empty-string as
        // "set" and skip the default.
        let env = env_with(&[("X", "")]);
        assert_eq!(expand_vars("${X:-fallback}", &env), "fallback");
    }

    #[test]
    fn actual_value_used_when_var_is_set_to_nonempty() {
        // `${X:-fallback}` with `X=hello` → `hello`. The
        // default is discarded — only consulted when the var
        // is unset or empty.
        let env = env_with(&[("X", "hello")]);
        assert_eq!(expand_vars("${X:-fallback}", &env), "hello");
    }

    #[test]
    fn default_value_can_be_empty() {
        // `${UNSET:-}` is a valid POSIX form — explicit
        // "expand to empty if unset", which is exactly what
        // the bare `${UNSET}` already does. Test exists to
        // guard against the parser rejecting an empty default
        // (it should accept the zero-byte tail after `:-`).
        let env = env_with(&[]);
        assert_eq!(expand_vars("${UNSET:-}", &env), "");
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
            expand_vars("${UNSET:-some default}", &env),
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
        assert_eq!(expand_vars("${UNSET:-$X}", &env), "$X");
    }

    #[test]
    fn unterminated_brace_in_default_form_preserves_literal() {
        // `${X:-no_close` with no closing `}` → preserved
        // as-is, mirroring the existing `${X` unterminated
        // behavior. POSIX errors here; v1 prefers leniency
        // over surfacing a mid-line error.
        let env = env_with(&[("X", "hello")]);
        assert_eq!(
            expand_vars("${X:-no_close", &env),
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
            expand_vars("${X:-this is junk}", &env),
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
            expand_vars("a${UNSET:-mid}b", &env),
            "amidb"
        );
    }
}
