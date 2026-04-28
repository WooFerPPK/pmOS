//! `/usr/bin/sysmon` — system monitor (T170).
//!
//! Native build: a `--proc-root <dir>`-friendly CLI that walks
//! `/proc`, reads each `/proc/<pid>/status`, and prints a
//! fixed-width process table. Used by sysmon::tests::cli for
//! the per-pid table assertions.
//!
//! WASI build (the shipped /usr/bin/sysmon binary): a real
//! toolkit GUI window painting the same process table. Refresh
//! is on first paint only for v1; the toolkit's frame-callback
//! drive lands in a follow-up. The Terminate button + 1 s
//! refresh tick from the original task spec depend on the
//! toolkit pointer-event surface (T120 hooked it up; integration
//! still pending) — defer to the next slice.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use sysmon::{read_status, truncate_name};

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    use super::*;
    use toolkit::draw::font::GLYPH_HEIGHT;
    use toolkit::draw::{Color, Rect};
    use toolkit::protocol::Connection;
    use toolkit::{App, BufferPool, Window};

    extern crate alloc;

    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn fd_read(fd: i32, iovs_ptr: *const Iovec, iovs_len: i32, nread_ptr: *mut u32) -> i32;
        fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32)
            -> i32;
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
    pub struct Ciovec { pub buf: *const u8, pub buf_len: u32 }
    #[repr(C)]
    pub struct Iovec { pub buf: *mut u8, pub buf_len: u32 }

    pub struct FdConnection { fd: i32, connected: core::cell::Cell<bool> }
    impl FdConnection {
        pub fn new(fd: i32) -> Self { FdConnection { fd, connected: core::cell::Cell::new(false) } }
    }
    impl Connection for FdConnection {
        fn send(&mut self, bytes: &[u8]) {
            if bytes.is_empty() { return; }
            let mut sent = 0usize;
            while sent < bytes.len() {
                let remaining = &bytes[sent..];
                let iov = Ciovec { buf: remaining.as_ptr(), buf_len: remaining.len() as u32 };
                let mut nwritten: u32 = 0;
                let rc = unsafe { fd_write(self.fd, &iov, 1, &mut nwritten) };
                if rc == 0 && nwritten > 0 { sent += nwritten as usize; continue; }
                if rc == 0 || rc == EAGAIN || rc == EINVAL { unsafe { let _ = sched_yield(); } continue; }
                return;
            }
        }
        fn drain_outbound(&mut self) -> alloc::vec::Vec<u8> { alloc::vec::Vec::new() }
        fn recv(&mut self) -> alloc::vec::Vec<u8> {
            let mut buf = [0u8; 4096];
            let iov = Iovec { buf: buf.as_mut_ptr(), buf_len: buf.len() as u32 };
            let mut nread: u32 = 0;
            if !self.connected.get() {
                for _ in 0..RECV_MAX_POLLS {
                    let rc = unsafe { fd_read(self.fd, &iov, 1, &mut nread) };
                    if rc == 0 && nread > 0 { self.connected.set(true); return buf[..nread as usize].to_vec(); }
                    if rc == 0 || rc == EAGAIN { nread = 0; unsafe { let _ = sched_yield(); } continue; }
                    return alloc::vec::Vec::new();
                }
                return alloc::vec::Vec::new();
            }
            let rc = unsafe { fd_read(self.fd, &iov, 1, &mut nread) };
            if rc == 0 && nread > 0 { return buf[..nread as usize].to_vec(); }
            alloc::vec::Vec::new()
        }
    }

    pub fn run() -> ! {
        println!("sysmon: starting");
        let mut fd: i32 = -1;
        for _ in 0..CONNECT_MAX_POLLS {
            let rc = unsafe { display_connect() };
            if rc >= 0 { fd = rc; break; }
            if rc == -ECONNREFUSED { unsafe { let _ = sched_yield(); } continue; }
            unsafe { proc_exit(-rc) };
        }
        if fd < 0 { unsafe { proc_exit(ECONNREFUSED) }; }
        let conn = FdConnection::new(fd);
        match run_window(conn) {
            Ok(_) => unsafe { proc_exit(0) },
            Err(_) => unsafe { proc_exit(1) },
        }
    }

    fn collect() -> Vec<String> {
        sysmon::collect_snapshot(&PathBuf::from("/proc"))
    }

    fn run_window<C: Connection>(connection: C) -> Result<(), toolkit::ClientError> {
        let mut app = App::connect(connection)?;
        let mut window = Window::new(&mut app)?;
        window.set_title("System Monitor")?;
        window.set_app_id("pmos.sysmon")?;
        window.commit()?;

        // Repaint cadence: collect every Nth tick + paint a
        // fresh snapshot. The shell's chunked write path
        // makes per-iter paints expensive (3 MiB / paint) so
        // we throttle rather than refresh per tick. The
        // refresh tick count is tuned so a steady-state
        // sysmon repaints roughly twice a second under the
        // tight inner loop.
        const REFRESH_EVERY: u32 = 5_000;
        let mut snapshot = collect();
        let mut tick: u32 = 0;
        let mut last_refresh: u32 = 0;
        let mut needs_paint = true;
        let mut configured_once = false;
        let (w, h) = (560u32, 360u32);
        let mut pool: Option<BufferPool> = None;
        loop {
            tick = tick.wrapping_add(1);
            let _ = window.dispatch()?;
            if window.close_requested() {
                return Ok(());
            }
            if !configured_once && window.is_configured() {
                configured_once = true;
                pool = Some(BufferPool::new(window.app_mut(), w, h)?);
                needs_paint = true;
            }
            if configured_once && tick.wrapping_sub(last_refresh) >= REFRESH_EVERY {
                snapshot = collect();
                last_refresh = tick;
                needs_paint = true;
            }
            if needs_paint && configured_once {
                let p = pool.as_mut().expect("pool initialised");
                if let Some(mut canvas) = p.acquire_back_canvas() {
                    let bg = Color::rgb(0xfa, 0xfa, 0xfa);
                    let titlebar = Color::rgb(0x60, 0x40, 0x40);
                    let header_bg = Color::rgb(0xe0, 0xe0, 0xe4);
                    let row_alt = Color::rgb(0xf2, 0xf2, 0xf6);
                    let text_fg = Color::rgb(0x10, 0x10, 0x10);
                    let muted_fg = Color::rgb(0x80, 0x80, 0x90);

                    canvas.fill_rect(Rect { x: 0, y: 0, width: w, height: h }, bg);
                    let titlebar_h = 22u32;
                    canvas.fill_rect(
                        Rect { x: 0, y: 0, width: w, height: titlebar_h },
                        titlebar,
                    );
                    canvas.draw_text(
                        8,
                        ((titlebar_h as i32 - GLYPH_HEIGHT as i32) / 2).max(0),
                        "System Monitor",
                        Color::rgb(0xff, 0xff, 0xff),
                    );

                    let row_h = 14_i32;
                    let header_y = titlebar_h as i32 + 4;
                    canvas.fill_rect(
                        Rect { x: 0, y: header_y, width: w, height: row_h as u32 },
                        header_bg,
                    );
                    canvas.draw_text(
                        8,
                        header_y + 2,
                        "PID    NAME              STATE       PPID",
                        text_fg,
                    );

                    let mut y = header_y + row_h + 2;
                    if snapshot.is_empty() {
                        canvas.draw_text(
                            8,
                            y + 2,
                            "(no processes visible — /proc may be empty)",
                            muted_fg,
                        );
                    } else {
                        for (i, line) in snapshot.iter().take(20).enumerate() {
                            if i % 2 == 1 {
                                canvas.fill_rect(
                                    Rect { x: 0, y, width: w, height: row_h as u32 },
                                    row_alt,
                                );
                            }
                            canvas.draw_text(8, y + 2, line, text_fg);
                            y += row_h;
                        }
                    }
                    let footer = format!(
                        "{} process{}   |   refresh ~ every {} ticks",
                        snapshot.len(),
                        if snapshot.len() == 1 { "" } else { "es" },
                        REFRESH_EVERY,
                    );
                    canvas.draw_text(8, h as i32 - GLYPH_HEIGHT as i32 - 6, &footer, muted_fg);

                    drop(canvas);
                    p.commit_and_swap(&mut window)?;
                    needs_paint = false;
                }
            }
        }
    }
}

fn main() -> ExitCode {
    #[cfg(target_arch = "wasm32")]
    {
        wasm_main::run();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        run_cli()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn run_cli() -> ExitCode {
    let proc_root = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sysmon: {}", e);
            return ExitCode::from(1);
        }
    };

    let entries = match fs::read_dir(&proc_root) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("sysmon: failed to open {}: {}", proc_root.display(), e);
            return ExitCode::from(1);
        }
    };

    let mut pids: Vec<u32> = Vec::new();
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if let Ok(pid) = name.parse::<u32>() {
                pids.push(pid);
            }
        }
    }
    pids.sort_unstable();

    println!("{:<7} {:<16}  {:<11} {}", "PID", "NAME", "STATE", "PPID");
    println!("{:<7} {:<16}  {:<11} {}", "-----", "----------------", "----------", "-----");

    for pid in pids {
        let status_path = proc_root.join(pid.to_string()).join("status");
        match read_status(&status_path) {
            Ok(snap) => {
                let name = truncate_name(&snap.name);
                println!("{:<7} {:<16}  {:<11} {}", pid, name, snap.state, snap.ppid);
            }
            Err(reason) => {
                eprintln!("sysmon: pid {}: failed to parse status: {}", pid, reason);
            }
        }
    }

    ExitCode::from(0)
}

fn parse_args() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    let mut proc_root: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--proc-root" => {
                let val = args
                    .next()
                    .ok_or_else(|| String::from("--proc-root requires a value"))?;
                proc_root = Some(PathBuf::from(val));
            }
            other => {
                return Err(format!("unrecognised argument: {}", other));
            }
        }
    }
    Ok(proc_root.unwrap_or_else(|| PathBuf::from("/proc")))
}

