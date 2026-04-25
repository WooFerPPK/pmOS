//! POSIX-ish `wc`: word, line, and byte counter. Reads each input
//! (stdin when no file args, otherwise every path in turn) into a
//! buffer, counts `\n` bytes for the line total, runs
//! `str::split_whitespace` for the word total when the bytes parse
//! as UTF-8, and uses `bytes.len()` for the byte total. Per-file
//! output is `<lines>\t<words>\t<bytes>\t<filename>\n`; with multiple
//! file args a final `<lines>\t<words>\t<bytes>\ttotal\n` row sums
//! the columns. Stdin mode (no file args) omits the filename column
//! and emits just the counts. Open/read errors write
//! `wc: <path>: <err>` to stderr, set the had-error flag, and
//! continue with remaining files. Exit 0 on full success, 1 on any
//! per-file failure.
//!
//! Flag parsing mirrors grep's POSIX-style short-flag clustering
//! (commit `f667018`): `-l` toggles lines-only, `-w` words-only,
//! `-c` bytes-only, and combinations select the union of columns
//! (e.g. `-lc` shows lines + bytes, omits words). Output column
//! ordering is fixed at lines → words → bytes regardless of flag
//! order (`-cwl` and `-lwc` produce the same shape). When no
//! count-selection flag is set the default is all three columns.
//! `--` is a hard separator. Unknown flags write
//! `wc: unknown flag: <flag>` to stderr and exit 1.
//!
//! Counting semantics: lines = count of `\n` bytes (a final partial
//! line without a trailing `\n` is NOT counted, matching POSIX wc);
//! words = `split_whitespace().count()` over the file as UTF-8
//! (ASCII whitespace tokenisation — spaces, tabs, newlines, etc.);
//! bytes = `bytes.len()`. Invalid UTF-8 short-circuits the word
//! count to 0 for that file, surfaces a stderr diagnostic, and
//! flips the had-error flag.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut want_lines = false;
    let mut want_words = false;
    let mut want_bytes = false;
    let mut paths: Vec<String> = Vec::new();
    let mut sep_seen = false;
    for arg in args {
        if !sep_seen && arg == "--" {
            sep_seen = true;
            continue;
        }
        if !sep_seen && arg.starts_with('-') && arg != "-" {
            for ch in arg[1..].chars() {
                match ch {
                    'l' => want_lines = true,
                    'w' => want_words = true,
                    'c' => want_bytes = true,
                    _ => {
                        let _ = writeln!(io::stderr(), "wc: unknown flag: {arg}");
                        return ExitCode::from(1);
                    }
                }
            }
        } else {
            paths.push(arg);
        }
    }

    let any_flag = want_lines || want_words || want_bytes;
    let show_lines = if any_flag { want_lines } else { true };
    let show_words = if any_flag { want_words } else { true };
    let show_bytes = if any_flag { want_bytes } else { true };

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut had_error = false;

    if paths.is_empty() {
        let mut buf = Vec::new();
        match io::stdin().lock().read_to_end(&mut buf) {
            Ok(_) => {
                let (lines, words, bytes) = count(&buf, "stdin", &mut had_error);
                emit(&mut out, lines, words, bytes, None, show_lines, show_words, show_bytes);
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "wc: stdin: {e}");
                had_error = true;
            }
        }
        return if had_error { ExitCode::from(1) } else { ExitCode::from(0) };
    }

    let multi = paths.len() > 1;
    let mut total_lines: u64 = 0;
    let mut total_words: u64 = 0;
    let mut total_bytes: u64 = 0;

    for path in &paths {
        match fs::read(path) {
            Ok(bytes) => {
                let (l, w, b) = count(&bytes, path, &mut had_error);
                emit(&mut out, l, w, b, Some(path.as_str()), show_lines, show_words, show_bytes);
                total_lines += l;
                total_words += w;
                total_bytes += b;
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "wc: {path}: {e}");
                had_error = true;
            }
        }
    }

    if multi {
        emit(&mut out, total_lines, total_words, total_bytes, Some("total"), show_lines, show_words, show_bytes);
    }

    if had_error { ExitCode::from(1) } else { ExitCode::from(0) }
}

fn count(bytes: &[u8], label: &str, had_error: &mut bool) -> (u64, u64, u64) {
    let lines = bytes.iter().filter(|b| **b == b'\n').count() as u64;
    let byte_count = bytes.len() as u64;
    let words = match std::str::from_utf8(bytes) {
        Ok(s) => s.split_whitespace().count() as u64,
        Err(_) => {
            let _ = writeln!(io::stderr(), "wc: {label}: invalid utf-8");
            *had_error = true;
            0
        }
    };
    (lines, words, byte_count)
}

fn emit<W: Write>(
    out: &mut W,
    lines: u64,
    words: u64,
    bytes: u64,
    label: Option<&str>,
    show_lines: bool,
    show_words: bool,
    show_bytes: bool,
) {
    let mut cols: Vec<String> = Vec::with_capacity(4);
    if show_lines {
        cols.push(lines.to_string());
    }
    if show_words {
        cols.push(words.to_string());
    }
    if show_bytes {
        cols.push(bytes.to_string());
    }
    if let Some(name) = label {
        cols.push(name.to_string());
    }
    let _ = writeln!(out, "{}", cols.join("\t"));
}
