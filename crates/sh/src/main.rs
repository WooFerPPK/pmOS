//! `/usr/bin/sh` — thin driver that wires real stdin /
//! stdout / stderr into the [`sh::run`] REPL loop.
//!
//! Every interesting decision lives in [`sh::run`]; this
//! file just locks the real fds, hands them in as
//! `BufRead` / `Write` impls, and translates the returned
//! [`sh::ExitStatus`] into a process exit code.
//!
//! Distinct from `/usr/bin/shell` (the desktop shell).
//! Populated in Phase 3 T123 (this slice — `echo`, `exit`,
//! `cd`, `pwd`). Phase 6 T142..T145 expands the binary with
//! pipes / redirection / job control; until then the
//! REPL's tokenizer is a whitespace split and external
//! commands fall through to "command not found".

use std::io::{self, BufReader};
use std::process::ExitCode;

fn main() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();

    let reader = BufReader::new(stdin.lock());
    let status = sh::run(reader, stdout.lock(), stderr.lock());

    let code = status.code();
    // `ExitCode::from(u8)` clamps: treat negative /
    // wrapping shell codes as zero. In practice `exit`
    // accepts any i32 but the process-level surface is u8.
    let byte = u8::try_from(code.rem_euclid(256)).unwrap_or(0);
    ExitCode::from(byte)
}
