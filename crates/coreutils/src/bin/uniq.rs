//! T146 follow-up — POSIX-ish `uniq`: filter ADJACENT duplicate lines
//! from input. Reads stdin (no file args) or a single file path,
//! groups consecutive runs of byte-equal lines, and emits one
//! representative per run with optional count / duplicate / unique
//! filtering.
//!
//! IMPORTANT distinction from `sort -u`: POSIX `uniq` only collapses
//! ADJACENT duplicates. For unsorted input only consecutive runs of
//! the same line collapse — non-adjacent repeats survive. Input
//! `a\na\nb\na\n` (3 runs: [a,a] [b] [a]) emits `a\nb\na\n` by
//! default, NOT `a\nb\n`. Callers needing global dedup should
//! pipe `sort | uniq` (or use `sort -u`, which fuses the two passes).
//!
//! `uniq [-c] [-d] [-u] [file]`:
//! - Default (no flags): emit one copy of each adjacent run.
//! - `-c`: prefix each output line with `<count>\t<line>` (run length).
//! - `-d`: emit ONLY duplicated lines (count > 1), one copy each.
//! - `-u`: emit ONLY unique lines (count == 1).
//! - `-d` + `-u`: empty output (no run is both duplicated AND unique).
//!
//! Argument count: POSIX `uniq` takes 0 or 1 file arg (NOT multi-file
//! like cat/grep — the historical second arg is an output file path
//! we don't implement in v1). Two args = first is input, second is
//! output, which v1 rejects with `usage: uniq [-cdu] [file]` exit 1.
//! Stdin mode is 0-arg invocation. A bare `-` is preserved as a path
//! arg per cat/grep convention but treated identically to stdin would
//! be (it just won't open as a path); v1 leaves that as an error
//! shape rather than special-casing — `uniq -` simply tries to open
//! a file named `-` and reports the open error.
//!
//! CLI parser shape: mirrors grep's POSIX-style short-flag clustering
//! (commit f667018) — `-cd` toggles both `-c` and `-d`. `--` is the
//! hard separator forcing the next arg into paths regardless of leading
//! `-`. Unknown flag → `uniq: unknown flag: <flag>` exit 1 (matches
//! the brief's spec; differs from grep/sort which exit 2 — uniq's
//! brief explicitly lists exit 1 for both unknown-flag AND usage
//! errors, so this slice keeps the failure-mode partition simple).
//!
//! Errors: missing file → `uniq: <path>: <err>` to stderr, exit 1.
//! Open errors abort (we only ever read 0 or 1 file, so there's no
//! partial-success continuation as in cat/grep/sort multi-file mode).
//!
//! Pattern precedent: `crates/coreutils/src/bin/{cat,grep,wc,head,sort}.rs`.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut count = false;
    let mut only_dup = false;
    let mut only_uniq = false;
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
                    'c' => count = true,
                    'd' => only_dup = true,
                    'u' => only_uniq = true,
                    _ => {
                        let _ = writeln!(io::stderr(), "uniq: unknown flag: {arg}");
                        return ExitCode::from(1);
                    }
                }
            }
        } else {
            paths.push(arg);
        }
    }

    if paths.len() > 1 {
        let _ = writeln!(io::stderr(), "usage: uniq [-cdu] [file]");
        return ExitCode::from(1);
    }

    let buf = if let Some(path) = paths.first() {
        match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                let _ = writeln!(io::stderr(), "uniq: {path}: {e}");
                return ExitCode::from(1);
            }
        }
    } else {
        let mut s = String::new();
        if let Err(e) = io::stdin().lock().read_to_string(&mut s) {
            let _ = writeln!(io::stderr(), "uniq: stdin: {e}");
            return ExitCode::from(1);
        }
        s
    };

    let lines = split_lines(&buf);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut had_error = false;

    let mut i = 0;
    while i < lines.len() {
        let mut j = i + 1;
        while j < lines.len() && lines[j] == lines[i] {
            j += 1;
        }
        let run_len = j - i;
        let is_dup = run_len > 1;
        let is_uniq = run_len == 1;
        let emit = match (only_dup, only_uniq) {
            (false, false) => true,
            (true, false) => is_dup,
            (false, true) => is_uniq,
            (true, true) => false,
        };
        if emit {
            let result = if count {
                writeln!(out, "{run_len}\t{}", lines[i])
            } else {
                writeln!(out, "{}", lines[i])
            };
            if result.is_err() {
                had_error = true;
                break;
            }
        }
        i = j;
    }

    if had_error { ExitCode::from(1) } else { ExitCode::from(0) }
}

fn split_lines(text: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = text.split('\n').collect();
    if matches!(parts.last(), Some(&"")) {
        parts.pop();
    }
    parts
}
