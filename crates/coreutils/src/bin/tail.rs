//! T146 follow-up — POSIX-ish `tail`: print the last N lines of each
//! input. Default N is 10. With no file args, reads stdin. With more
//! than one file arg, each section is prefixed by a
//! `==> <path> <==\n` header (GNU multi-file convention); a single
//! file arg prints with no header. Failed open / read writes
//! `tail: <path>: <err>` to stderr, sets a had-error flag, and
//! continues with remaining files. Exit 0 on full success, 1 if any
//! file failed.
//!
//! Counting semantics: a "line" is `\n`-terminated. The last N
//! `\n`-terminated lines are emitted in source order; if the file
//! ends without a trailing newline, that partial tail counts as one
//! of the lines and is emitted verbatim (no extra `\n` injected).
//! N=0 prints nothing (the file is opened so missing-file diagnostics
//! still fire). N negative is a usage error: `tail: invalid count:
//! -<N>` to stderr, exit 1. GNU's `tail -n -K` "all but first K"
//! semantic is intentionally deferred to a future slice.
//!
//! Implementation: read the file fully (v1 has small files; the
//! streaming O(N)-window form is a future slice if/when it matters),
//! split on `\n`, take the last N. Mirror cat/grep/cp/mkdir/rm/mv/ls/
//! head's flag-parsing convention — `-n N` consumes the next argv as
//! a u64; `-nN` (no space) and `-N` (bare digit) both parse as the
//! count. Detection: if an arg matches `-?-?\d+` exactly (one
//! optional sign character then all digits), treat the whole arg as
//! a count specifier — that distinguishes the bare `-5` form from a
//! short-flag cluster like `-lc`. Unknown flags write
//! `tail: unknown flag: <flag>` to stderr and exit 1.
//!
//! Pattern precedent: `crates/coreutils/src/bin/{cat,grep,cp,mkdir,rm,mv,ls,head}.rs`.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let raw: Vec<String> = env::args().skip(1).collect();
    let mut count: u64 = 10;
    let mut paths: Vec<String> = Vec::new();
    let mut sep_seen = false;
    let mut iter = raw.into_iter();
    while let Some(arg) = iter.next() {
        if !sep_seen && arg == "--" {
            sep_seen = true;
            continue;
        }
        if !sep_seen && arg.starts_with('-') && arg != "-" {
            if let Some(parsed) = parse_count_arg(&arg) {
                match parsed {
                    Ok(n) => count = n,
                    Err(bad) => {
                        let _ = writeln!(io::stderr(), "tail: invalid count: {bad}");
                        return ExitCode::from(1);
                    }
                }
                continue;
            }
            if arg == "-n" {
                let Some(next) = iter.next() else {
                    let _ = writeln!(io::stderr(), "tail: option requires an argument: -n");
                    return ExitCode::from(1);
                };
                match next.parse::<u64>() {
                    Ok(n) => count = n,
                    Err(_) => {
                        let _ = writeln!(io::stderr(), "tail: invalid count: {next}");
                        return ExitCode::from(1);
                    }
                }
                continue;
            }
            let _ = writeln!(io::stderr(), "tail: unknown flag: {arg}");
            return ExitCode::from(1);
        }
        paths.push(arg);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut had_error = false;

    if paths.is_empty() {
        let mut buf = Vec::new();
        match io::stdin().lock().read_to_end(&mut buf) {
            Ok(_) => {
                if let Err(e) = emit_last_n(&buf, count, &mut out) {
                    let _ = writeln!(io::stderr(), "tail: stdin: {e}");
                    had_error = true;
                }
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "tail: stdin: {e}");
                had_error = true;
            }
        }
    } else {
        let multi = paths.len() > 1;
        for (i, path) in paths.iter().enumerate() {
            match fs::read(path) {
                Ok(bytes) => {
                    if multi {
                        let leading = if i == 0 { "" } else { "\n" };
                        if let Err(e) = write!(out, "{leading}==> {path} <==\n") {
                            let _ = writeln!(io::stderr(), "tail: {path}: {e}");
                            had_error = true;
                            continue;
                        }
                    }
                    if let Err(e) = emit_last_n(&bytes, count, &mut out) {
                        let _ = writeln!(io::stderr(), "tail: {path}: {e}");
                        had_error = true;
                    }
                }
                Err(e) => {
                    let _ = writeln!(io::stderr(), "tail: {path}: {e}");
                    had_error = true;
                }
            }
        }
    }

    if had_error { ExitCode::from(1) } else { ExitCode::from(0) }
}

fn parse_count_arg(arg: &str) -> Option<Result<u64, String>> {
    let body = &arg[1..];
    if body.is_empty() {
        return None;
    }
    let after_sign = if body.starts_with('-') { &body[1..] } else { body };
    if !after_sign.is_empty() && after_sign.chars().all(|c| c.is_ascii_digit()) {
        if body.starts_with('-') {
            return Some(Err(arg.to_string()));
        }
        return match after_sign.parse::<u64>() {
            Ok(n) => Some(Ok(n)),
            Err(_) => Some(Err(arg.to_string())),
        };
    }
    if let Some(rest) = body.strip_prefix('n') {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
            return match rest.parse::<u64>() {
                Ok(n) => Some(Ok(n)),
                Err(_) => Some(Err(arg.to_string())),
            };
        }
        if rest.starts_with('-')
            && rest.len() > 1
            && rest[1..].chars().all(|c| c.is_ascii_digit())
        {
            return Some(Err(arg.to_string()));
        }
    }
    None
}

fn emit_last_n<W: Write>(bytes: &[u8], count: u64, out: &mut W) -> io::Result<()> {
    if count == 0 || bytes.is_empty() {
        return Ok(());
    }
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    let lines = split_lines(bytes);
    let start = lines.len().saturating_sub(count);
    for slice in &lines[start..] {
        out.write_all(slice)?;
    }
    Ok(())
}

fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            out.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}
