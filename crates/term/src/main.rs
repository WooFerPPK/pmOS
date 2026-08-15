//! `/usr/bin/term` — terminal emulator binary.
//!
//! On `wasm32-wasip1` (the production target) `main` opens
//! `/run/display` via the kernel's `pmos_ext.display_connect`
//! extension syscall, wraps the returned fd in an
//! [`FdConnection`] adapter, and hands it to
//! [`term::run_term`], which drives the toolkit window event
//! loop, paints scrollback + the input line via the
//! rasterizer, and routes keyboard events through the
//! scancode → ASCII translator.
//!
//! On the native host (used by `cargo run -p term` and by
//! `cargo test`) `main` falls back to a line-oriented stdin
//! REPL that exercises the [`Terminal`] library without
//! needing a WASI sandbox or a live display server.

#[cfg(not(target_arch = "wasm32"))]
use term::{Key, KeyFeedResult, Terminal, TerminalOptions};

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    use term::{
        load_startup_font, run_term_with_stepwise_runner_and_font, PmosShellSession, TermExit,
        TerminalOptions,
    };
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn proc_exit(rval: i32) -> !;
    }

    pub fn run() {
        let font = load_startup_font();
        println!(
            "term: starting font={}x{}",
            font.cell_width(),
            font.cell_height()
        );
        let conn = match toolkit::wasi::FdConnection::connect() {
            Ok(connection) => connection,
            Err(errno) => unsafe { proc_exit(errno) },
        };
        let syscalls = sh::WasmPmosSyscalls::default();
        let mut shell = match PmosShellSession::start_stepwise(syscalls) {
            Ok(shell) => shell,
            Err(errno) => unsafe { proc_exit(errno) },
        };
        let options = TerminalOptions {
            max_lines: 1024,
            banner: vec![
                "PMos Terminal".to_string(),
                "Type a command and press Enter. `help` for builtins, `exit` to quit.".to_string(),
            ],
            prompt: "$ ".to_string(),
        };
        match run_term_with_stepwise_runner_and_font(conn, u32::MAX, options, &mut shell, &font) {
            Ok(TermExit::CloseRequested) => unsafe { proc_exit(0) },
            Ok(TermExit::ShellExited) => unsafe { proc_exit(0) },
            Ok(TermExit::IterationLimit) => unsafe { proc_exit(0) },
            Err(_) => unsafe { proc_exit(1) },
        }
    }
}

#[cfg(target_arch = "wasm32")]
extern crate alloc;

// Anchor references to the library's run_term so `just build`
// doesn't DCE the symbol out of the WASM binary if `fn main`
// only uses it under cfg(target_arch = "wasm32"). The
// host-target build still gets a runnable host REPL below.
#[cfg(not(target_arch = "wasm32"))]
fn host_repl() {
    use std::io::{BufRead, Write};

    let mut terminal = Terminal::new(TerminalOptions {
        max_lines: 1024,
        banner: vec![
            "PMos term — Rust terminal emulator (host build)".to_string(),
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

fn main() {
    #[cfg(target_arch = "wasm32")]
    wasm_main::run();
    #[cfg(not(target_arch = "wasm32"))]
    host_repl();
}
