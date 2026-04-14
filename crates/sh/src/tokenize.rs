//! Shell tokenizer.
//!
//! Splits an input line into whitespace-separated tokens,
//! honouring single quotes (`'…'`, literal contents),
//! double quotes (`"…"`, backslash escapes honoured but no
//! variable expansion), and backslash escapes outside
//! quotes. Unterminated quoted strings are absorbed as-is
//! — the shell's `eval` layer reports them, not the
//! tokenizer.
//!
//! Examples:
//!
//! ```text
//! echo hello world           -> ["echo", "hello", "world"]
//! echo 'hello  world'        -> ["echo", "hello  world"]
//! echo "hello  world"        -> ["echo", "hello  world"]
//! echo hello\ world          -> ["echo", "hello world"]
//! echo 'it\'s fine'          -> ["echo", "it\\", "s fine"]
//!     (single-quote literals do NOT unescape)
//! ```

use alloc::string::String;
use alloc::vec::Vec;

/// Split `line` into shell-style tokens.
///
/// See module docs for the escape/quoting rules. The
/// output is always a `Vec<String>` — an empty input
/// yields an empty vector.
pub fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if in_single {
            if c == '\'' {
                in_single = false;
            } else {
                current.push(c);
            }
            continue;
        }
        if in_double {
            match c {
                '"' => {
                    in_double = false;
                }
                '\\' => {
                    // Inside double quotes, backslash
                    // escapes the next char (POSIX: only
                    // $, `, ", \, newline — we'll just
                    // pass the next char through).
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                other => current.push(other),
            }
            continue;
        }

        if c.is_whitespace() {
            if !current.is_empty() {
                tokens.push(core::mem::take(&mut current));
            }
            continue;
        }
        match c {
            '\'' => in_single = true,
            '"' => in_double = true,
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}
