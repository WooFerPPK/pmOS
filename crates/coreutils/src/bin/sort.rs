//! T146 follow-up — POSIX-ish `sort`: read each input (stdin when no
//! file args, otherwise every path in turn) into a `Vec<String>` of
//! lines, concatenate the results into a single bucket, sort with the
//! Rust default `Vec::sort` (lexicographic byte-order — matches POSIX
//! `sort` under the C / POSIX locale), and emit each line on its own
//! `\n`-terminated row. Open/read errors write `sort: <path>: <err>`
//! to stderr, set a had-error flag, and continue with remaining
//! files. Exit 0 on full success, 1 on any per-file failure.
//!
//! Line-splitting semantics: `bytes.split('\n')` over the file as
//! UTF-8; an exact final empty segment caused by a trailing `\n` is
//! dropped so a file ending in `c\nb\na\n` contributes three lines
//! `c` / `b` / `a` (not four with a phantom empty tail). A file
//! without a trailing newline keeps every segment, so `c\nb\na`
//! contributes the same three lines.
//!
//! Flags mirror grep's POSIX-style short-flag clustering (commit
//! `f667018`): `-r` reverses the sorted result via `Vec::reverse`;
//! `-u` collapses adjacent duplicates via `Vec::dedup` after the
//! sort (since the input is sorted, dedup gives unique entries
//! globally); `-n` switches the comparison key from byte order to
//! the leading signed integer parsed via `parse_leading_int` (lines
//! whose leading non-whitespace token isn't numeric compare as 0,
//! so they cluster among themselves in input order — POSIX leaves
//! that order unspecified for v1); `-f` folds ASCII lowercase to
//! uppercase for the comparison key only (output bytes are the
//! original input bytes — only the sort/dedup KEY is folded), via
//! `fold_to_upper_bytes`. Non-ASCII bytes pass through unchanged
//! (no Unicode case-folding in v1). When both `-n` and `-f` are
//! set, numeric dominates: case-folding is a no-op for the
//! integer-parsing key. `-c` switches sort into check-only mode:
//! emit no stdout, walk consecutive line pairs computing the same
//! comparison key the sort path would have used, and on the FIRST
//! out-of-order pair write `sort: -:<line-num>: disorder: <line>\n`
//! to stderr (POSIX-conformant diagnostic; the `-:` prefix is
//! POSIX's "stdin file name" since v1 sort doesn't accept file args
//! in check mode either) and exit 1; otherwise exit 0 with no
//! output. `-c` composes with the comparison flags: `-cn` checks
//! numeric ordering, `-cf` checks fold ordering, `-cr` checks
//! descending order, `-cu` ALSO checks uniqueness (equal adjacent
//! keys count as a violation, since under `-u` the input would have
//! been collapsed). `-ru` / `-ur` / `-nr` / `-nu` / `-nru` /
//! `-fr` / `-fu` / `-fnu` / `-cn` / `-cf` / `-cr` / `-cu` etc.
//! apply the chosen combination. Unknown flags write
//! `sort: unknown flag: <flag>` to stderr and exit 2 (matching
//! grep's open-error/usage exit code).
//!
//! Pattern precedent: `crates/coreutils/src/bin/{cat,grep,wc,head}.rs`.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut reverse = false;
    let mut unique = false;
    let mut numeric = false;
    let mut fold = false;
    let mut check_only = false;
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
                    'r' => reverse = true,
                    'u' => unique = true,
                    'n' => numeric = true,
                    'f' => fold = true,
                    'c' => check_only = true,
                    _ => {
                        let _ = writeln!(io::stderr(), "sort: unknown flag: {arg}");
                        return ExitCode::from(2);
                    }
                }
            }
        } else {
            paths.push(arg);
        }
    }

    let mut lines: Vec<String> = Vec::new();
    let mut had_error = false;

    if paths.is_empty() {
        let mut buf = String::new();
        match io::stdin().lock().read_to_string(&mut buf) {
            Ok(_) => append_lines(&buf, &mut lines),
            Err(e) => {
                let _ = writeln!(io::stderr(), "sort: stdin: {e}");
                had_error = true;
            }
        }
    } else {
        for path in &paths {
            match fs::read_to_string(path) {
                Ok(text) => append_lines(&text, &mut lines),
                Err(e) => {
                    let _ = writeln!(io::stderr(), "sort: {path}: {e}");
                    had_error = true;
                }
            }
        }
    }

    if check_only {
        return check_sorted(&lines, numeric, fold, reverse, unique);
    }

    if numeric {
        lines.sort_by_key(|line| parse_leading_int(line));
    } else if fold {
        lines.sort_by_key(|line| fold_to_upper_bytes(line));
    } else {
        lines.sort();
    }
    if reverse {
        lines.reverse();
    }
    if unique {
        if fold && !numeric {
            lines.dedup_by_key(|line| fold_to_upper_bytes(line));
        } else {
            lines.dedup();
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in &lines {
        if writeln!(out, "{line}").is_err() {
            had_error = true;
            break;
        }
    }

    if had_error { ExitCode::from(1) } else { ExitCode::from(0) }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Numeric(i64),
    Bytes(Vec<u8>),
}

fn key_for(line: &str, numeric: bool, fold: bool) -> Key {
    if numeric {
        Key::Numeric(parse_leading_int(line))
    } else if fold {
        Key::Bytes(fold_to_upper_bytes(line))
    } else {
        Key::Bytes(line.as_bytes().to_vec())
    }
}

fn check_sorted(
    lines: &[String],
    numeric: bool,
    fold: bool,
    reverse: bool,
    unique: bool,
) -> ExitCode {
    use std::cmp::Ordering;
    for i in 1..lines.len() {
        let prev = key_for(&lines[i - 1], numeric, fold);
        let curr = key_for(&lines[i], numeric, fold);
        let ord = prev.cmp(&curr);
        let violation = match (reverse, unique) {
            (false, false) => ord == Ordering::Greater,
            (false, true) => ord != Ordering::Less,
            (true, false) => ord == Ordering::Less,
            (true, true) => ord != Ordering::Greater,
        };
        if violation {
            let _ = writeln!(
                io::stderr(),
                "sort: -:{}: disorder: {}",
                i + 1,
                lines[i]
            );
            return ExitCode::from(1);
        }
    }
    ExitCode::from(0)
}

fn append_lines(text: &str, sink: &mut Vec<String>) {
    let mut parts: Vec<&str> = text.split('\n').collect();
    if matches!(parts.last(), Some(&"")) {
        parts.pop();
    }
    for p in parts {
        sink.push(p.to_string());
    }
}

fn fold_to_upper_bytes(s: &str) -> Vec<u8> {
    s.bytes()
        .map(|b| if b.is_ascii_lowercase() { b - 32 } else { b })
        .collect()
}

fn parse_leading_int(s: &str) -> i64 {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    let mut negative = false;
    if i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b'+') {
        negative = bytes[i] == b'-';
        i += 1;
    }
    let digits_start = i;
    let mut value: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        let digit = i64::from(bytes[i] - b'0');
        value = value
            .saturating_mul(10)
            .saturating_add(if negative { -digit } else { digit });
        i += 1;
    }
    if i == digits_start {
        return 0;
    }
    value
}
