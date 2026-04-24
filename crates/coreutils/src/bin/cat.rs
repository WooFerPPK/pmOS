//! POSIX-ish `cat`: concatenate files (or stdin when no paths given)
//! to stdout. A failed file prints a diagnostic to stderr, flips the
//! had-error flag, and does not abort the remaining files — matching
//! the behaviour users expect from `cat a missing b`.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut had_error = false;

    if args.is_empty() {
        let mut buf = Vec::new();
        if let Err(e) = io::stdin().lock().read_to_end(&mut buf) {
            let _ = writeln!(io::stderr(), "cat: stdin: {e}");
            return ExitCode::from(1);
        }
        if let Err(e) = io::stdout().lock().write_all(&buf) {
            let _ = writeln!(io::stderr(), "cat: stdout: {e}");
            return ExitCode::from(1);
        }
        return ExitCode::from(0);
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    for path in &args {
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

    if had_error { ExitCode::from(1) } else { ExitCode::from(0) }
}
