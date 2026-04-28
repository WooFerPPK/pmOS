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
    use term::{run_term, TermExit};
    use toolkit::protocol::Connection;

    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn fd_read(fd: i32, iovs_ptr: *const Iovec, iovs_len: i32, nread_ptr: *mut u32) -> i32;
        fn fd_write(
            fd: i32,
            iovs_ptr: *const Ciovec,
            iovs_len: i32,
            nwritten_ptr: *mut u32,
        ) -> i32;
        fn sched_yield() -> i32;
        fn proc_exit(rval: i32) -> !;
    }

    #[link(wasm_import_module = "pmos_ext")]
    extern "C" {
        fn display_connect() -> i32;
    }

    const EAGAIN: i32 = 6;
    const EINVAL: i32 = 28;
    const ECONNREFUSED: i32 = 14;
    const RECV_MAX_POLLS: u32 = 50_000;
    const CONNECT_MAX_POLLS: u32 = 50_000;

    #[repr(C)]
    pub struct Ciovec {
        pub buf: *const u8,
        pub buf_len: u32,
    }
    #[repr(C)]
    pub struct Iovec {
        pub buf: *mut u8,
        pub buf_len: u32,
    }

    pub struct FdConnection {
        fd: i32,
        connected: core::cell::Cell<bool>,
    }

    impl FdConnection {
        pub fn new(fd: i32) -> Self {
            FdConnection {
                fd,
                connected: core::cell::Cell::new(false),
            }
        }
    }

    impl Connection for FdConnection {
        fn send(&mut self, bytes: &[u8]) {
            if bytes.is_empty() {
                return;
            }
            let mut sent = 0usize;
            while sent < bytes.len() {
                let remaining = &bytes[sent..];
                let iov = Ciovec {
                    buf: remaining.as_ptr(),
                    buf_len: remaining.len() as u32,
                };
                let mut nwritten: u32 = 0;
                let rc = unsafe { fd_write(self.fd, &iov, 1, &mut nwritten) };
                if rc == 0 && nwritten > 0 {
                    sent += nwritten as usize;
                    continue;
                }
                if rc == 0 || rc == EAGAIN || rc == EINVAL {
                    unsafe {
                        let _ = sched_yield();
                    }
                    continue;
                }
                return;
            }
        }

        fn drain_outbound(&mut self) -> alloc::vec::Vec<u8> {
            alloc::vec::Vec::new()
        }

        fn recv(&mut self) -> alloc::vec::Vec<u8> {
            let mut buf = [0u8; 4096];
            let iov = Iovec {
                buf: buf.as_mut_ptr(),
                buf_len: buf.len() as u32,
            };
            let mut nread: u32 = 0;
            if !self.connected.get() {
                for _ in 0..RECV_MAX_POLLS {
                    let rc = unsafe { fd_read(self.fd, &iov, 1, &mut nread) };
                    if rc == 0 && nread > 0 {
                        self.connected.set(true);
                        return buf[..nread as usize].to_vec();
                    }
                    if rc == 0 || rc == EAGAIN {
                        nread = 0;
                        unsafe {
                            let _ = sched_yield();
                        }
                        continue;
                    }
                    return alloc::vec::Vec::new();
                }
                return alloc::vec::Vec::new();
            }
            let rc = unsafe { fd_read(self.fd, &iov, 1, &mut nread) };
            if rc == 0 && nread > 0 {
                return buf[..nread as usize].to_vec();
            }
            alloc::vec::Vec::new()
        }
    }

    pub fn run() {
        println!("term: starting");
        let mut fd: i32 = -1;
        for _ in 0..CONNECT_MAX_POLLS {
            let rc = unsafe { display_connect() };
            if rc >= 0 {
                fd = rc;
                break;
            }
            if rc == -ECONNREFUSED {
                unsafe {
                    let _ = sched_yield();
                }
                continue;
            }
            unsafe { proc_exit(-rc) };
        }
        if fd < 0 {
            unsafe { proc_exit(ECONNREFUSED) };
        }
        let conn = FdConnection::new(fd);
        match run_term(conn, u32::MAX) {
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
