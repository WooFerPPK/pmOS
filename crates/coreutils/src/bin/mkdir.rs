//! T146 follow-up — POSIX-ish `mkdir`: create each named directory with
//! `std::fs::create_dir`. With `-p`, intermediate parents are created
//! as needed via `std::fs::create_dir_all` and a pre-existing target
//! is not an error (POSIX `mkdir -p` semantics). Without `-p`, a
//! missing parent is a failure and an existing target is a failure
//! (both match POSIX `mkdir` without `-p`). Multi-path invocations
//! follow the same partial-success shape as cat/grep/cp: each path is
//! attempted, the per-path error goes to stderr as
//! `mkdir: <path>: <error>`, and the exit code is 1 if any path
//! failed, 0 otherwise.
//!
//! Flag parsing mirrors `ls -l` (commit `c3d1aa7`) and `cp -r`
//! (commit `b2623ff`): single-pass arg split; `-p` toggles
//! parents-as-needed; everything after `--` is forced into paths
//! regardless of leading `-`. Unknown flags write
//! `mkdir: unknown flag: <flag>` to stderr and exit 2 (distinct from
//! per-path exit 1).
//!
//! Pattern precedent: `crates/coreutils/src/bin/{cat,grep,cp}.rs`.
//! Argv parse is hand-rolled (no `clap`), `std::fs` only (no WASI).

use std::env;
use std::fs;
use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut parents = false;
    let mut paths: Vec<String> = Vec::new();
    let mut sep_seen = false;
    for arg in args {
        if !sep_seen && arg == "--" {
            sep_seen = true;
            continue;
        }
        if !sep_seen && arg.starts_with('-') && arg != "-" {
            if arg == "-p" {
                parents = true;
            } else {
                let _ = writeln!(io::stderr(), "mkdir: unknown flag: {arg}");
                return ExitCode::from(2);
            }
        } else {
            paths.push(arg);
        }
    }

    if paths.is_empty() {
        let _ = writeln!(io::stderr(), "mkdir: usage: mkdir [-p] <dir>...");
        return ExitCode::from(1);
    }

    let mut had_error = false;
    for path in &paths {
        let res = if parents {
            fs::create_dir_all(path)
        } else {
            fs::create_dir(path)
        };
        if let Err(e) = res {
            let _ = writeln!(io::stderr(), "mkdir: {path}: {e}");
            had_error = true;
        }
    }

    if had_error {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}
