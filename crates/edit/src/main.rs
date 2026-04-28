//! `/usr/bin/edit` — plain-text editor (T157).
//!
//! Toolkit window with a read-only preview of the file passed as
//! argv[1] (or `/etc/preferences.toml` if no arg). Edit-and-save
//! land in a follow-up slice; this slice ships the open-and-view
//! flow that file-manager → MIME dispatch (T158) needs as its
//! first observable.

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

    pub fn run() {
        println!("edit: starting");
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

use edit::read_file;

/// Pick a default file to display. Walks a list of likely
/// paths (bundled assets in /persist/etc, then a few common
/// docs) and returns the first one that exists. Falls back
/// to a baked-in welcome string if nothing is readable so
/// the editor never opens with just an error message.
fn pick_default_path() -> (String, String, bool) {
    let candidates = [
        "/persist/etc/init.conf",
        "/persist/usr/share/doc/welcome.md",
        "/etc/init.conf",
        "/etc/preferences.toml",
    ];
    for path in candidates {
        let (contents, ok) = read_file(path);
        if ok {
            return (path.to_string(), contents, true);
        }
    }
    let welcome = "\
Welcome to PMos Editor.

This is a read-only viewer in v1 — keyboard editing lands
in a follow-up slice. Pass any file path on the command line
to open it (the launcher's argv plumbing is also a v1
follow-up).

Try opening a terminal and pointing edit at:
  /persist/etc/init.conf      kernel boot config
  /proc/<pid>/status          live process info
  /persist/home/user/         your persistent home

Press the close button on the titlebar (when wired) to exit.\n";
    ("(welcome)".to_string(), welcome.to_string(), false)
}

fn run_window<C: toolkit::protocol::Connection>(
    connection: C,
) -> Result<(), toolkit::ClientError> {
    let mut app = App::connect(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Editor")?;
    window.set_app_id("pmos.edit")?;
    window.commit()?;

    let argv_path = std::env::args().nth(1);
    let (path, contents, ok) = match argv_path {
        Some(p) => {
            let (c, ok) = read_file(&p);
            if ok {
                (p, c, true)
            } else {
                // Fall back to default if argv path missing.
                pick_default_path()
            }
        }
        None => pick_default_path(),
    };

    let mut painted = false;
    let line_count = contents.lines().count();
    loop {
        let _ = window.dispatch()?;
        if window.close_requested() {
            return Ok(());
        }
        if !painted && window.is_configured() {
            let (w, h) = (640u32, 420u32);
            let mut pool: BufferPool = BufferPool::new(window.app_mut(), w, h)?;
            if let Some(mut canvas) = pool.acquire_back_canvas() {
                let bg = Color::rgb(0xfa, 0xfa, 0xfa);
                let titlebar = Color::rgb(0x6a, 0x4a, 0x8a);
                let menu_bar = Color::rgb(0xe0, 0xe0, 0xe4);
                let gutter_bg = Color::rgb(0xee, 0xee, 0xf2);
                let gutter_fg = Color::rgb(0x80, 0x80, 0x90);
                let status_bg = Color::rgb(0xe0, 0xe0, 0xe4);
                let text_fg = Color::rgb(0x10, 0x10, 0x10);
                let titlebar_h = 22u32;
                let menubar_h = 18u32;
                let status_h = 18u32;
                let gutter_w = 36u32;
                let line_h = 14_i32;

                canvas.fill_rect(Rect { x: 0, y: 0, width: w, height: h }, bg);
                canvas.fill_rect(
                    Rect { x: 0, y: 0, width: w, height: titlebar_h },
                    titlebar,
                );
                let title = if ok {
                    format!("Editor — {}", path)
                } else {
                    "Editor — welcome".to_string()
                };
                canvas.draw_text(
                    8,
                    ((titlebar_h as i32 - GLYPH_HEIGHT as i32) / 2).max(0),
                    &title,
                    Color::rgb(0xff, 0xff, 0xff),
                );

                canvas.fill_rect(
                    Rect { x: 0, y: titlebar_h as i32, width: w, height: menubar_h },
                    menu_bar,
                );
                canvas.draw_text(
                    8,
                    titlebar_h as i32 + 4,
                    "File   Edit   View   Help",
                    text_fg,
                );

                let content_y_top = (titlebar_h + menubar_h) as i32 + 6;
                let content_y_bot = h as i32 - status_h as i32;
                let content_h = (content_y_bot - content_y_top).max(0) as u32;
                let visible_lines = (content_h as i32 / line_h).max(0) as usize;

                // Gutter background (line numbers).
                canvas.fill_rect(
                    Rect {
                        x: 0,
                        y: content_y_top,
                        width: gutter_w,
                        height: content_h,
                    },
                    gutter_bg,
                );

                for (i, line) in contents.lines().take(visible_lines).enumerate() {
                    let y = content_y_top + (i as i32) * line_h;
                    let lineno = format!("{:>4}", i + 1);
                    canvas.draw_text(4, y, &lineno, gutter_fg);
                    let truncated: String = line.chars().take(96).collect();
                    canvas.draw_text(gutter_w as i32 + 6, y, &truncated, text_fg);
                }

                // Status bar.
                canvas.fill_rect(
                    Rect {
                        x: 0,
                        y: content_y_bot,
                        width: w,
                        height: status_h,
                    },
                    status_bg,
                );
                let displayed = visible_lines.min(line_count);
                let status = format!(
                    "{}   |   showing {} / {} line{}   |   read-only",
                    if ok { "loaded" } else { "default content" },
                    displayed,
                    line_count,
                    if line_count == 1 { "" } else { "s" },
                );
                canvas.draw_text(8, content_y_bot + 4, &status, text_fg);

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
        let path = std::env::args().nth(1).unwrap_or_else(|| "/etc/preferences.toml".to_string());
        let (contents, ok) = read_file(&path);
        println!("editor (host build) — {} ({})", path, if ok { "ok" } else { "missing" });
        for (i, line) in contents.lines().take(20).enumerate() {
            println!("{:4}: {}", i + 1, line);
        }
    }
}
