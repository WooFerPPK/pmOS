//! T146 follow-up — POSIX-ish `head`: print the first N lines of
//! each input. Default N is 10. With no file args, reads stdin. With
//! more than one file arg, each section is prefixed by a
//! `==> <path> <==\n` header (GNU multi-file convention); a single
//! file arg prints with no header. Failed open / read writes
//! `head: <path>: <err>` to stderr, sets a had-error flag, and
//! continues with remaining files. Exit 0 on full success, 1 if any
//! file failed.
//!
//! Counting semantics: a "line" is `\n`-terminated. A file with
//! `foo\nbar` (no trailing newline) has 1 complete line + a partial
//! tail; with N >= 1 we emit `foo\n` + `bar` verbatim — no extra
//! newline injected on a partial tail. N=0 prints nothing (the file
//! is opened so missing-file diagnostics still fire). N negative is
//! a usage error: `head: invalid count: -<N>` to stderr, exit 1.
//! GNU's `head -n -K` "all but last K" semantic is intentionally
//! deferred to a future slice.
//!
//! Byte-count mode: `-c N` switches the count selector from lines
//! to bytes — print the first N bytes of each input instead of the
//! first N lines. Same multi-file headers, same stdin fallback,
//! same partial-success diagnostic shape. `N=0` prints nothing
//! (still opens the file so missing-file diagnostics fire). When
//! both `-n N` and `-c N` are passed in the same argv, the latest
//! count-selection flag wins (matches GNU behavior — argv left-to-
//! right, last selector binds the mode + count). The forms
//! `-c -N` (GNU "all but last N bytes") and `-c N[KMG]` (suffix
//! multipliers) are intentionally deferred to future slices.
//!
//! Flag parsing extends the cluster pattern from cat/grep/cp/rm with
//! a value-consuming flag: `-n N` reads the next argv as a u64;
//! `-nN` (no space) and `-N` (bare digit, GNU-style `head -5 file`)
//! both parse as the count. Detection: if an arg matches `-?-?\d+`
//! exactly (one optional sign character then all digits), treat the
//! whole arg as a count specifier — that distinguishes the bare
//! `-5` form from a short-flag cluster like `-lc`. `-c N` mirrors
//! `-n N`'s value-follows-the-flag shape (also accepts `-cN` no-
//! space). Unknown flags write `head: unknown flag: <flag>` to
//! stderr and exit 1.
//!
//! Pattern precedent: `crates/coreutils/src/bin/{cat,grep,cp,mkdir,rm,mv,ls}.rs`.

use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::process::ExitCode;

#[derive(Clone, Copy)]
enum Mode {
    Lines,
    Bytes,
}

fn main() -> ExitCode {
    let raw: Vec<String> = env::args().skip(1).collect();
    let mut count: u64 = 10;
    let mut mode = Mode::Lines;
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
                    Ok(n) => {
                        count = n;
                        mode = Mode::Lines;
                    }
                    Err(bad) => {
                        let _ = writeln!(io::stderr(), "head: invalid count: {bad}");
                        return ExitCode::from(1);
                    }
                }
                continue;
            }
            if arg == "-n" {
                let Some(next) = iter.next() else {
                    let _ = writeln!(io::stderr(), "head: option requires an argument: -n");
                    return ExitCode::from(1);
                };
                match next.parse::<u64>() {
                    Ok(n) => {
                        count = n;
                        mode = Mode::Lines;
                    }
                    Err(_) => {
                        let _ = writeln!(io::stderr(), "head: invalid count: {next}");
                        return ExitCode::from(1);
                    }
                }
                continue;
            }
            if arg == "-c" {
                let Some(next) = iter.next() else {
                    let _ = writeln!(io::stderr(), "head: option requires an argument: -c");
                    return ExitCode::from(1);
                };
                match next.parse::<u64>() {
                    Ok(n) => {
                        count = n;
                        mode = Mode::Bytes;
                    }
                    Err(_) => {
                        let _ = writeln!(io::stderr(), "head: invalid count: {next}");
                        return ExitCode::from(1);
                    }
                }
                continue;
            }
            if let Some(rest) = arg.strip_prefix("-c") {
                match rest.parse::<u64>() {
                    Ok(n) => {
                        count = n;
                        mode = Mode::Bytes;
                    }
                    Err(_) => {
                        let _ = writeln!(io::stderr(), "head: invalid count: {rest}");
                        return ExitCode::from(1);
                    }
                }
                continue;
            }
            let _ = writeln!(io::stderr(), "head: unknown flag: {arg}");
            return ExitCode::from(1);
        }
        paths.push(arg);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut had_error = false;

    if paths.is_empty() {
        let stdin = io::stdin();
        if let Err(e) = emit_first(stdin.lock(), count, mode, &mut out) {
            let _ = writeln!(io::stderr(), "head: stdin: {e}");
            had_error = true;
        }
    } else {
        let multi = paths.len() > 1;
        for (i, path) in paths.iter().enumerate() {
            match File::open(path) {
                Ok(f) => {
                    if multi {
                        let leading = if i == 0 { "" } else { "\n" };
                        if let Err(e) = writeln!(out, "{leading}==> {path} <==") {
                            let _ = writeln!(io::stderr(), "head: {path}: {e}");
                            had_error = true;
                            continue;
                        }
                    }
                    if let Err(e) = emit_first(BufReader::new(f), count, mode, &mut out) {
                        let _ = writeln!(io::stderr(), "head: {path}: {e}");
                        had_error = true;
                    }
                }
                Err(e) => {
                    let _ = writeln!(io::stderr(), "head: {path}: {e}");
                    had_error = true;
                }
            }
        }
    }

    if had_error {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}

fn parse_count_arg(arg: &str) -> Option<Result<u64, String>> {
    let body = &arg[1..];
    if body.is_empty() {
        return None;
    }
    let after_sign = body.strip_prefix('-').unwrap_or(body);
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
        if rest.starts_with('-') && rest.len() > 1 && rest[1..].chars().all(|c| c.is_ascii_digit())
        {
            return Some(Err(arg.to_string()));
        }
    }
    None
}

fn emit_first<R: BufRead, W: Write>(r: R, count: u64, mode: Mode, out: &mut W) -> io::Result<()> {
    match mode {
        Mode::Lines => emit_first_n_lines(r, count, out),
        Mode::Bytes => emit_first_n_bytes(r, count, out),
    }
}

fn emit_first_n_lines<R: BufRead, W: Write>(mut r: R, count: u64, out: &mut W) -> io::Result<()> {
    if count == 0 {
        let mut sink = Vec::new();
        let _ = r.read_to_end(&mut sink);
        return Ok(());
    }
    let mut emitted: u64 = 0;
    let mut buf: Vec<u8> = Vec::new();
    while emitted < count {
        buf.clear();
        let n = r.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf)?;
        if buf.ends_with(b"\n") {
            emitted += 1;
        } else {
            break;
        }
    }
    Ok(())
}

fn emit_first_n_bytes<R: BufRead, W: Write>(r: R, count: u64, out: &mut W) -> io::Result<()> {
    let mut take = r.take(count);
    io::copy(&mut take, out)?;
    Ok(())
}
