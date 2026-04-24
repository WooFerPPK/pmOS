//! T146 partial — POSIX-ish `mkdir`: create each named directory with
//! `std::fs::create_dir`. No `-p` in this slice, so a missing parent
//! is a failure and an existing target is a failure (both match POSIX
//! `mkdir` without `-p`). Multi-path invocations follow the same
//! partial-success shape as cat/grep/cp: each path is attempted, the
//! per-path error goes to stderr as `mkdir: <path>: <error>`, and the
//! exit code is 1 if any path failed, 0 otherwise. Recursive creation
//! (`-p`) is a follow-up slice.
//! Pattern precedent: `crates/coreutils/src/bin/{cat,grep,cp}.rs`.
//! Argv parse is hand-rolled (no `clap`), `std::fs` only (no WASI).

use std::env;
use std::fs;
use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        let _ = writeln!(io::stderr(), "mkdir: usage: mkdir <dir>...");
        return ExitCode::from(1);
    }

    let mut had_error = false;
    for path in &args {
        if let Err(e) = fs::create_dir(path) {
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
