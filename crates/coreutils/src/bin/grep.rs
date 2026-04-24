//! POSIX-ish `grep`: fixed-string line matcher. Reads each input
//! (stdin when no file args, otherwise every path in turn) line by
//! line and emits matching lines to stdout. With more than one file
//! arg, each match is prefixed with `<path>:` — the standard POSIX
//! multi-file disambiguation. Open errors flip `had_open_error` and
//! are reported to stderr but do not abort the remaining files;
//! malformed UTF-8 per line is diagnosed and skipped. Exit codes:
//! 0 = matched & no open errors, 1 = no match & no open errors,
//! 2 = any open error or usage error.

use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some((pattern, files)) = args.split_first() else {
        let _ = writeln!(io::stderr(), "usage: grep <pattern> [file ...]");
        return ExitCode::from(2);
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut matched = false;
    let mut had_open_error = false;
    let multi = files.len() > 1;

    if files.is_empty() {
        let stdin = io::stdin();
        matched |= scan(stdin.lock(), pattern, None, &mut out);
    } else {
        for path in files {
            match File::open(path) {
                Ok(f) => {
                    let prefix = if multi { Some(path.as_str()) } else { None };
                    matched |= scan(BufReader::new(f), pattern, prefix, &mut out);
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

fn scan<R: BufRead, W: Write>(r: R, pat: &str, prefix: Option<&str>, out: &mut W) -> bool {
    let mut any = false;
    let label = prefix.unwrap_or("-");
    for (n, line) in r.split(b'\n').enumerate() {
        let bytes = match line {
            Ok(b) => b,
            Err(e) => {
                let _ = writeln!(io::stderr(), "grep: {label}: {e}");
                return any;
            }
        };
        match std::str::from_utf8(&bytes) {
            Ok(s) if s.contains(pat) => {
                any = true;
                if let Some(p) = prefix {
                    let _ = writeln!(out, "{p}:{s}");
                } else {
                    let _ = writeln!(out, "{s}");
                }
            }
            Ok(_) => {}
            Err(_) => {
                let _ = writeln!(io::stderr(), "grep: {label}: invalid utf-8 at line {}", n + 1);
            }
        }
    }
    any
}
