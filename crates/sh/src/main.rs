// /usr/bin/sh — the CLI POSIX-ish shell.
//
// The library layer in `lib.rs` owns the state machine
// (tokenizer, builtins, environment); this binary is the
// thin byte-stream driver that reads lines from stdin,
// feeds them through `Shell::eval`, and writes the
// resulting stdout/stderr bytes to the matching fds.
//
// Distinct from /usr/bin/shell (the desktop shell).
//
// Populated in Phase 3 T123 (minimal) and Phase 6
// T142..T145 (pipes, redirection, job control).

use std::io::{self, BufRead, Write};

use sh::Shell;

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut shell = Shell::new();

    // Greet.
    let mut out = stdout.lock();
    writeln!(out, "PMos sh — type 'help' for builtins, 'exit' to quit").ok();
    write!(out, "$ ").ok();
    out.flush().ok();

    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let result = shell.eval(&line);
        if !result.stdout.is_empty() {
            out.write_all(&result.stdout).ok();
            out.flush().ok();
        }
        if !result.stderr.is_empty() {
            let mut err = stderr.lock();
            err.write_all(&result.stderr).ok();
            err.flush().ok();
        }
        if shell.has_exited() {
            let code = shell.exit_status().unwrap_or(0);
            std::process::exit(code);
        }
        write!(out, "$ ").ok();
        out.flush().ok();
    }
}
