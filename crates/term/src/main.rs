//! `/usr/bin/term` — terminal emulator.
//!
//! Until the toolkit + display-protocol wiring lands (Phase 4
//! T132), the `term` binary is a line-oriented stdin REPL that
//! exercises the [`term::Terminal`] library on the native host.
//! It reads whole lines from stdin, feeds them into a
//! `Terminal`, and prints the embedded shell's stdout/stderr
//! to the process's stdout. This lets `cargo run -p term`
//! smoke-test the library without a WASI build or a live
//! display server.
//!
//! Once the toolkit wiring lands, `main` will switch to
//! constructing a `toolkit::Client`, binding a surface, and
//! routing keystrokes through [`term::Terminal::feed_key`].

use std::io::{BufRead, Write};
use term::{Key, KeyFeedResult, Terminal, TerminalOptions};

fn main() {
    let mut terminal = Terminal::new(TerminalOptions {
        max_lines: 1024,
        banner: vec![
            "PMos term — Rust terminal emulator".to_string(),
            "type 'help' for a list of builtins, 'exit' to quit.".to_string(),
        ],
        prompt: "> ".to_string(),
    });

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in &terminal.snapshot().lines {
        let _ = writeln!(out, "{}", line.text);
    }
    let _ = write!(out, "{}", terminal.prompt());
    let _ = out.flush();

    let stdin = std::io::stdin();
    for input in stdin.lock().lines() {
        let Ok(input) = input else { break };
        for ch in input.chars() {
            terminal.feed_key(Key::Char(ch));
        }
        if let KeyFeedResult::Committed { output, exited, .. } = terminal.feed_key(Key::Enter) {
            let _ = out.write_all(&output.stdout);
            let _ = out.write_all(&output.stderr);
            if exited {
                return;
            }
        }
        let _ = write!(out, "{}", terminal.prompt());
        let _ = out.flush();
    }
}
