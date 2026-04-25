//! POSIX-ish `grep`: fixed-string line matcher. Reads each input
//! (stdin when no file args, otherwise every path in turn) line by
//! line and emits matching lines to stdout. With more than one file
//! arg, each match is prefixed with `<path>:` — the standard POSIX
//! multi-file disambiguation. Open errors flip `had_open_error` and
//! are reported to stderr but do not abort the remaining files;
//! malformed UTF-8 per line is diagnosed and skipped. Exit codes:
//! 0 = matched & no open errors, 1 = no match & no open errors,
//! 2 = any open error or usage error.
//!
//! Flag parsing mirrors `cp -r` (commit `b2623ff`): single-pass
//! arg split; `-i` toggles case-insensitive matching; `-n` prefixes
//! each match with its 1-indexed line number; `-v` inverts the
//! match (emit lines that do NOT contain the pattern); `-c`
//! suppresses per-line output and emits only the count of matches
//! per file (with multi-file invocations the path prefix still
//! applies); short flags may be clustered POSIX-style (`grep -inv
//! FOO` = `-i -n -v`); everything after `--` is forced into
//! pattern/file args regardless of leading `-`. Unknown flags
//! write `grep: unknown flag: <flag>` to stderr and exit 2.
//!
//! Explicitly deferred flag follow-ups: `-E` (POSIX-ERE regex),
//! `-F` (fixed-string is already the default), `-l` (filenames
//! only), Unicode-aware case-folding (current `-i` is
//! ASCII-via-`to_lowercase`).

use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut case_insensitive = false;
    let mut with_line_numbers = false;
    let mut invert = false;
    let mut count_only = false;
    let mut rest: Vec<String> = Vec::new();
    let mut sep_seen = false;
    for arg in args {
        if !sep_seen && arg == "--" {
            sep_seen = true;
            continue;
        }
        if !sep_seen && arg.starts_with('-') && arg != "-" {
            for ch in arg[1..].chars() {
                match ch {
                    'i' => case_insensitive = true,
                    'n' => with_line_numbers = true,
                    'v' => invert = true,
                    'c' => count_only = true,
                    _ => {
                        let _ = writeln!(io::stderr(), "grep: unknown flag: {arg}");
                        return ExitCode::from(2);
                    }
                }
            }
        } else {
            rest.push(arg);
        }
    }

    let Some((pattern, files)) = rest.split_first() else {
        let _ = writeln!(io::stderr(), "usage: grep [-i] [-n] [-v] [-c] <pattern> [file ...]");
        return ExitCode::from(2);
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut matched = false;
    let mut had_open_error = false;
    let multi = files.len() > 1;
    let needle = if case_insensitive { pattern.to_lowercase() } else { pattern.clone() };

    if files.is_empty() {
        let stdin = io::stdin();
        matched |= scan(stdin.lock(), &needle, case_insensitive, None, with_line_numbers, invert, count_only, &mut out);
    } else {
        for path in files {
            match File::open(path) {
                Ok(f) => {
                    let prefix = if multi { Some(path.as_str()) } else { None };
                    matched |= scan(BufReader::new(f), &needle, case_insensitive, prefix, with_line_numbers, invert, count_only, &mut out);
                }
                Err(e) => {
                    let _ = writeln!(io::stderr(), "grep: {path}: {e}");
                    had_open_error = true;
                }
            }
        }
    }

    if had_open_error {
        ExitCode::from(2)
    } else if matched {
        ExitCode::from(0)
    } else {
        ExitCode::from(1)
    }
}

fn scan<R: BufRead, W: Write>(
    r: R,
    needle: &str,
    case_insensitive: bool,
    prefix: Option<&str>,
    with_line_numbers: bool,
    invert: bool,
    count_only: bool,
    out: &mut W,
) -> bool {
    let mut any = false;
    let mut count: u64 = 0;
    let label = prefix.unwrap_or("-");
    for (n, line) in r.split(b'\n').enumerate() {
        let bytes = match line {
            Ok(b) => b,
            Err(e) => {
                let _ = writeln!(io::stderr(), "grep: {label}: {e}");
                if count_only {
                    emit_count(out, prefix, count);
                }
                return any;
            }
        };
        match std::str::from_utf8(&bytes) {
            Ok(s) => {
                let hit = if case_insensitive {
                    s.to_lowercase().contains(needle)
                } else {
                    s.contains(needle)
                };
                if hit != invert {
                    any = true;
                    if count_only {
                        count += 1;
                    } else {
                        let lineno = n + 1;
                        match (prefix, with_line_numbers) {
                            (Some(p), true) => {
                                let _ = writeln!(out, "{p}:{lineno}:{s}");
                            }
                            (Some(p), false) => {
                                let _ = writeln!(out, "{p}:{s}");
                            }
                            (None, true) => {
                                let _ = writeln!(out, "{lineno}:{s}");
                            }
                            (None, false) => {
                                let _ = writeln!(out, "{s}");
                            }
                        }
                    }
                }
            }
            Err(_) => {
                let _ = writeln!(io::stderr(), "grep: {label}: invalid utf-8 at line {}", n + 1);
            }
        }
    }
    if count_only {
        emit_count(out, prefix, count);
    }
    any
}

fn emit_count<W: Write>(out: &mut W, prefix: Option<&str>, count: u64) {
    match prefix {
        Some(p) => {
            let _ = writeln!(out, "{p}:{count}");
        }
        None => {
            let _ = writeln!(out, "{count}");
        }
    }
}
