//! POSIX-ish `cp`: single-file copy. Reads `src` wholesale into memory
//! and writes it to `dst`, overwriting any existing file (POSIX
//! default). No `-r` / `-R` (directory recursion), `-n` (no-clobber),
//! `-i` (interactive), `-v` (verbose), or `-p` (preserve metadata) in
//! this slice — deferred to follow-up work. Exit 0 on success, 1 on
//! any error (usage, src read, dst write).

use std::env;
use std::fs;
use std::io::{self, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let [src, dst] = match args.as_slice() {
        [a, b] => [a, b],
        _ => {
            let _ = writeln!(io::stderr(), "cp: usage: cp <src> <dst>");
            return ExitCode::from(1);
        }
    };

    let bytes = match fs::read(src) {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(io::stderr(), "cp: {src}: {e}");
            return ExitCode::from(1);
        }
    };

    if let Err(e) = fs::write(dst, &bytes) {
        let _ = writeln!(io::stderr(), "cp: {dst}: {e}");
        return ExitCode::from(1);
    }

    ExitCode::from(0)
}
