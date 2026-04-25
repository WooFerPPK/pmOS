//! POSIX-ish `cat`: concatenate files (or stdin when no paths given)
//! to stdout. A failed file prints a diagnostic to stderr, flips the
//! had-error flag, and does not abort the remaining files — matching
//! the behaviour users expect from `cat a missing b`.
//!
//! Flag parsing now mirrors grep's POSIX-style short-flag clustering
//! (commit `6f682af`): `-nE` parses as both `-n` and `-E`, so the
//! per-character match arm in the parser handles each letter
//! independently. `--` is still a hard separator. Flags accepted:
//! `-n` prefixes each output line with a 1-indexed line number
//! formatted in a right-justified 6-char field followed by a tab
//! (GNU coreutils shape, e.g. `     1\thello`); line numbers are
//! CONTINUOUS across multiple file args (so the second of two 3-line
//! files starts at 4); stdin mode in `-n` counts from 1 the same
//! way. `-E` appends a `$` to the end of each line, immediately
//! before the newline (GNU `cat -E` shape). Unknown flags write
//! `cat: unknown flag: <flag>` to stderr and exit 1 (cat's existing
//! exit-1 convention, not grep's exit-2).
//!
//! Implementation has two paths: when neither `-n` nor `-E` is set
//! we keep the existing `fs::read` + bulk `write_all` fast path
//! (raw bytes streamed verbatim — no UTF-8 inspection, no line
//! splitting); otherwise we switch to `BufReader::lines()` so we
//! can interleave the line-number prefix and/or the `$` suffix.
//! `lines()` strips the trailing `\n` (we re-emit one via
//! `writeln!`) and treats a missing-final-newline input as a single
//! trailing line, matching GNU `cat -n` / `cat -E`.

use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let raw: Vec<String> = env::args().skip(1).collect();
    let mut number_lines = false;
    let mut show_ends = false;
    let mut paths: Vec<String> = Vec::new();
    let mut sep_seen = false;
    for arg in raw {
        if !sep_seen && arg == "--" {
            sep_seen = true;
            continue;
        }
        if !sep_seen && arg.starts_with('-') && arg != "-" {
            for ch in arg[1..].chars() {
                match ch {
                    'n' => number_lines = true,
                    'E' => show_ends = true,
                    _ => {
                        let _ = writeln!(io::stderr(), "cat: unknown flag: {arg}");
                        return ExitCode::from(1);
                    }
                }
            }
        } else {
            paths.push(arg);
        }
    }

    let mut had_error = false;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut lineno: u64 = 1;
    let line_mode = number_lines || show_ends;

    if paths.is_empty() {
        if line_mode {
            let stdin = io::stdin();
            if let Err(e) = format_reader(
                stdin.lock(),
                &mut lineno,
                &mut out,
                "stdin",
                number_lines,
                show_ends,
            ) {
                let _ = writeln!(io::stderr(), "cat: stdin: {e}");
                had_error = true;
            }
        } else {
            let mut buf = Vec::new();
            if let Err(e) = io::stdin().lock().read_to_end(&mut buf) {
                let _ = writeln!(io::stderr(), "cat: stdin: {e}");
                return ExitCode::from(1);
            }
            if let Err(e) = out.write_all(&buf) {
                let _ = writeln!(io::stderr(), "cat: stdout: {e}");
                return ExitCode::from(1);
            }
        }
        return if had_error { ExitCode::from(1) } else { ExitCode::from(0) };
    }

    for path in &paths {
        if line_mode {
            match File::open(path) {
                Ok(f) => {
                    if let Err(e) = format_reader(
                        BufReader::new(f),
                        &mut lineno,
                        &mut out,
                        path,
                        number_lines,
                        show_ends,
                    ) {
                        let _ = writeln!(io::stderr(), "cat: {path}: {e}");
                        had_error = true;
                    }
                }
                Err(e) => {
                    let _ = writeln!(io::stderr(), "cat: {path}: {e}");
                    had_error = true;
                }
            }
        } else {
            match fs::read(path) {
                Ok(bytes) => {
                    if let Err(e) = out.write_all(&bytes) {
                        let _ = writeln!(io::stderr(), "cat: {path}: {e}");
                        had_error = true;
                    }
                }
                Err(e) => {
                    let _ = writeln!(io::stderr(), "cat: {path}: {e}");
                    had_error = true;
                }
            }
        }
    }

    if had_error { ExitCode::from(1) } else { ExitCode::from(0) }
}

fn format_reader<R: BufRead, W: Write>(
    r: R,
    lineno: &mut u64,
    out: &mut W,
    label: &str,
    number_lines: bool,
    show_ends: bool,
) -> io::Result<()> {
    for line in r.lines() {
        match line {
            Ok(s) => {
                let suffix = if show_ends { "$" } else { "" };
                if number_lines {
                    writeln!(out, "{:>6}\t{s}{suffix}", *lineno)?;
                } else {
                    writeln!(out, "{s}{suffix}")?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let _ = writeln!(io::stderr(), "cat: {label}: invalid utf-8 at line {}", *lineno);
            }
            Err(e) => return Err(e),
        }
        *lineno += 1;
    }
    Ok(())
}
