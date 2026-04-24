//! T146 partial — POSIX-ish `rm`: remove each named file with
//! `std::fs::remove_file`. No `-r` / `-R` in this slice, so a
//! directory target is a failure (POSIX `rm` without `-r` refuses
//! directories). A missing path is a failure. Multi-path invocations
//! follow the same partial-success shape as cat/grep/cp/mkdir: each
//! path is attempted, the per-path error goes to stderr as
//! `rm: <path>: <error>`, and the exit code is 1 if any path failed,
//! 0 otherwise. Recursive removal (`-r`), force (`-f`), interactive
//! (`-i`), and verbose (`-v`) are follow-up slices.
//! Pattern precedent: `crates/coreutils/src/bin/{cat,grep,cp,mkdir}.rs`.
//! Argv parse is hand-rolled (no `clap`), `std::fs` only (no WASI).

use std::env;
use std::fs;
use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        let _ = writeln!(io::stderr(), "rm: usage: rm <path>...");
        return ExitCode::from(1);
    }

    let mut had_error = false;
    for path in &args {
        if let Err(e) = fs::remove_file(path) {
            let _ = writeln!(io::stderr(), "rm: {path}: {e}");
            had_error = true;
        }
    }

    if had_error {
        ExitCode::from(1)
    } else {
        ExitCode::from(0)
    }
}
