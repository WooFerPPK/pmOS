//! T146 follow-up — POSIX-ish `tee`: read stdin, write the bytes to
//! stdout AND to each named file (overwriting by default; appending
//! with `-a`). Zero file args is valid: stdin → stdout only (acts
//! like `cat`). Per-file open errors flip a had-error flag, write
//! `tee: <path>: <err>` to stderr, and continue with the remaining
//! files; stdin/stdout still receives the bytes regardless. Exit 0
//! on full success, 1 if any file failed (or unknown flag).
//!
//! Pattern precedent: `crates/coreutils/src/bin/{cat,grep,sort,tr}.rs`.
//! CLI parser mirrors grep's POSIX-style short-flag clustering
//! (commit f667018) — single-pass arg split into flags + paths,
//! `-a` toggles `append`, `--` is the hard separator, `-` (bare)
//! is preserved as a path arg. Unknown flag → `tee: unknown flag:
//! <flag>` to stderr and exit 1.
//!
//! Implementation: read stdin to a `Vec<u8>` (small files OK in v1),
//! then for each named file open with `OpenOptions` configured for
//! truncate-overwrite (default) or append (`-a`), write the bytes,
//! and finally write the bytes to stdout. Buffered stdin form
//! matches cat / sort / tr memory shape — streaming form is
//! deferred. Per-file write errors after a successful open flip
//! the had-error flag the same way open errors do.
//!
//! Explicitly deferred (out of slice scope, future single-flag
//! follow-up slices): `-i` (ignore SIGINT during write), `--`
//! long-form flags, the streaming write path that splits stdin
//! into chunks instead of buffering whole.

use std::env;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut append = false;
    let mut paths: Vec<String> = Vec::new();
    let mut sep_seen = false;
    for arg in args {
        if !sep_seen && arg == "--" {
            sep_seen = true;
            continue;
        }
        if !sep_seen && arg.starts_with('-') && arg != "-" && !arg.is_empty() {
            for ch in arg[1..].chars() {
                match ch {
                    'a' => append = true,
                    _ => {
                        let _ = writeln!(io::stderr(), "tee: unknown flag: {arg}");
                        return ExitCode::from(1);
                    }
                }
            }
        } else {
            paths.push(arg);
        }
    }

    let mut input: Vec<u8> = Vec::new();
    if let Err(e) = io::stdin().lock().read_to_end(&mut input) {
        let _ = writeln!(io::stderr(), "tee: stdin: {e}");
        return ExitCode::from(1);
    }

    let mut had_error = false;

    for path in &paths {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true);
        if append {
            opts.append(true);
        } else {
            opts.truncate(true);
        }
        match opts.open(path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(&input) {
                    let _ = writeln!(io::stderr(), "tee: {path}: {e}");
                    had_error = true;
                }
            }
            Err(e) => {
                let _ = writeln!(io::stderr(), "tee: {path}: {e}");
                had_error = true;
            }
        }
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    if out.write_all(&input).is_err() {
        had_error = true;
    }

    if had_error {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}
