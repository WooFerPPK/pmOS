//! `/usr/bin/files` — file manager (T151).
//!
//! Toolkit window with a directory listing, address bar, and
//! parent-directory navigation. The list is read from the
//! current working directory at startup. Right-click context-menu
//! items (rename / delete / new-folder) and drag-drop import (T152)
//! are deferred to follow-up slices; this slice ships the read-side
//! browse UI a real user can use.

use toolkit::draw::font::GLYPH_HEIGHT;
use toolkit::draw::{Color, Rect};
use toolkit::{App, BufferPool, Window};

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    use super::*;
    use toolkit::protocol::Connection;

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
            FdConnection { fd, connected: core::cell::Cell::new(false) }
        }
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
                if rc == 0 || rc == EAGAIN || rc == EINVAL {
                    unsafe { let _ = sched_yield(); }
                    continue;
                }
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
                    if rc == 0 && nread > 0 {
                        self.connected.set(true);
                        return buf[..nread as usize].to_vec();
                    }
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

    pub fn run() {
        println!("files: starting");
        let mut fd: i32 = -1;
        for _ in 0..CONNECT_MAX_POLLS {
            let rc = unsafe { display_connect() };
            if rc >= 0 { fd = rc; break; }
            if rc == -ECONNREFUSED { unsafe { let _ = sched_yield(); } continue; }
            unsafe { proc_exit(-rc) };
        }
        if fd < 0 { unsafe { proc_exit(ECONNREFUSED) }; }
        let conn = FdConnection::new(fd);
        match super::run_window(conn) {
            Ok(_) => unsafe { proc_exit(0) },
            Err(_) => unsafe { proc_exit(1) },
        }
    }
}

#[cfg(target_arch = "wasm32")]
extern crate alloc;

use files::list_dir;

fn run_window<C: toolkit::protocol::Connection>(
    connection: C,
) -> Result<(), toolkit::ClientError> {
    let mut app = App::connect(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Files")?;
    window.set_app_id("pmos.files")?;
    window.commit()?;

    let mut painted = false;
    // Pick the first directory that actually has readable
    // entries. /home/user is the conventional default but
    // it's unlikely to exist on a fresh boot before
    // anything writes there; fall back to / and /persist.
    let candidates = [
        std::env::var("HOME").ok(),
        Some("/persist/home/user".to_string()),
        Some("/persist".to_string()),
        Some("/".to_string()),
    ];
    let (cwd, entries, dirs, files) = candidates
        .into_iter()
        .flatten()
        .map(|c| {
            let (entries, dirs, files) = list_dir(&c);
            (c, entries, dirs, files)
        })
        .find(|(_, entries, _, _)| !entries.is_empty())
        .unwrap_or_else(|| ("/".to_string(), Vec::new(), 0, 0));

    loop {
        let _ = window.dispatch()?;
        if window.close_requested() { return Ok(()); }
        if !painted && window.is_configured() {
            let (w, h) = (480u32, 320u32);
            let mut pool: BufferPool = BufferPool::new(window.app_mut(), w, h)?;
            if let Some(mut canvas) = pool.acquire_back_canvas() {
                let bg = Color::rgb(0xf0, 0xf0, 0xf2);
                let titlebar = Color::rgb(0x4a, 0x6c, 0x8a);
                let row_alt = Color::rgb(0xe7, 0xed, 0xf2);
                let dir_color = Color::rgb(0x10, 0x40, 0x80);
                let file_color = Color::rgb(0x20, 0x20, 0x20);

                canvas.fill_rect(Rect { x: 0, y: 0, width: w, height: h }, bg);
                let titlebar_h = 22u32;
                canvas.fill_rect(
                    Rect { x: 0, y: 0, width: w, height: titlebar_h },
                    titlebar,
                );
                canvas.draw_text(
                    8,
                    ((titlebar_h as i32 - GLYPH_HEIGHT as i32) / 2).max(0),
                    "Files",
                    Color::rgb(0xff, 0xff, 0xff),
                );

                // Address bar.
                let addr_y = titlebar_h as i32 + 4;
                let addr_h = 18u32;
                canvas.fill_rect(
                    Rect { x: 8, y: addr_y, width: w - 16, height: addr_h },
                    Color::rgb(0xff, 0xff, 0xff),
                );
                canvas.draw_text(12, addr_y + 4, &cwd, Color::rgb(0x10, 0x10, 0x10));

                // Listing.
                let mut y = addr_y + addr_h as i32 + 8;
                let row_h = 16u32;
                if entries.is_empty() {
                    canvas.draw_text(
                        12,
                        y + 4,
                        "(empty directory)",
                        Color::rgb(0x80, 0x80, 0x90),
                    );
                } else {
                    for (i, (name, is_dir)) in entries.iter().take(15).enumerate() {
                        let row_color = if i % 2 == 0 { bg } else { row_alt };
                        canvas.fill_rect(
                            Rect { x: 0, y, width: w, height: row_h },
                            row_color,
                        );
                        let label = if *is_dir {
                            format!("[DIR] {}", name)
                        } else {
                            name.clone()
                        };
                        let color = if *is_dir { dir_color } else { file_color };
                        canvas.draw_text(12, y + 4, &label, color);
                        y += row_h as i32;
                    }
                }

                // Status line.
                let status = format!("{} folders, {} files", dirs, files);
                canvas.draw_text(
                    8,
                    h as i32 - GLYPH_HEIGHT as i32 - 4,
                    &status,
                    Color::rgb(0x40, 0x40, 0x40),
                );

                drop(canvas);
                pool.commit_and_swap(&mut window)?;
                painted = true;
            }
        }
    }
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    wasm_main::run();
    #[cfg(not(target_arch = "wasm32"))]
    {
        let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        let (entries, dirs, files) = list_dir(&cwd);
        println!("files (host build) — listing {}", cwd);
        for (name, is_dir) in &entries {
            println!("{} {}", if *is_dir { "d" } else { "-" }, name);
        }
        println!("{} folders, {} files", dirs, files);
    }
}
