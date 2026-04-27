//! `/opt/hello/hello.wasm` — third-party hello-world app
//! (T202). Packaged into `hello-0.1.0.pmpkg.tar` by
//! `xtask package sample-app` for the US10 install-flow
//! integration test.
//!
//! Trivial toolkit window: solid background, titlebar
//! strip with a label, body text. The window stays open
//! until the server emits `xdg_toplevel.close` — i.e. the
//! user closes it from the taskbar / shell-manager.

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
                    unsafe { let _ = sched_yield(); }
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
                        unsafe { let _ = sched_yield(); }
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
        println!("hello: starting");
        let mut fd: i32 = -1;
        for _ in 0..CONNECT_MAX_POLLS {
            let rc = unsafe { display_connect() };
            if rc >= 0 {
                fd = rc;
                break;
            }
            if rc == -ECONNREFUSED {
                unsafe { let _ = sched_yield(); }
                continue;
            }
            unsafe { proc_exit(-rc) };
        }
        if fd < 0 {
            unsafe { proc_exit(ECONNREFUSED) };
        }
        let conn = FdConnection::new(fd);
        match super::run_window(conn) {
            Ok(_) => unsafe { proc_exit(0) },
            Err(_) => unsafe { proc_exit(1) },
        }
    }
}

#[cfg(target_arch = "wasm32")]
extern crate alloc;

fn run_window<C: toolkit::protocol::Connection>(
    connection: C,
) -> Result<(), toolkit::ClientError> {
    let mut app = App::connect(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Hello, PMos!")?;
    window.set_app_id("pmos.sample-app")?;
    window.commit()?;

    let bg = Color::rgb(0x2a, 0x6c, 0x8a);
    let titlebar = Color::rgb(0x1a, 0x4c, 0x6a);
    let mut painted = false;
    loop {
        let _ = window.dispatch()?;
        if window.close_requested() {
            return Ok(());
        }
        if !painted && window.is_configured() {
            let (w, h) = (300u32, 160u32);
            let mut pool: BufferPool = BufferPool::new(window.app_mut(), w, h)?;
            if let Some(mut canvas) = pool.acquire_back_canvas() {
                canvas.fill_rect(Rect { x: 0, y: 0, width: w, height: h }, bg);
                let titlebar_h = 22u32;
                canvas.fill_rect(
                    Rect { x: 0, y: 0, width: w, height: titlebar_h },
                    titlebar,
                );
                let title = "Hello, PMos!";
                let tx = 8;
                let ty = ((titlebar_h as i32 - GLYPH_HEIGHT as i32) / 2).max(0);
                canvas.draw_text(tx, ty, title, Color::rgb(0xff, 0xff, 0xff));
                canvas.draw_text(
                    14,
                    titlebar_h as i32 + 24,
                    "Hello from a third-party app.",
                    Color::rgb(0xff, 0xff, 0xff),
                );
                canvas.draw_text(
                    14,
                    titlebar_h as i32 + 44,
                    "Installed via pkginstall + .pmpkg.tar.",
                    Color::rgb(0xcf, 0xe6, 0xf5),
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
    println!("sample-app (host build): use `cargo build --target wasm32-wasip1 -p sample-app`");
}
