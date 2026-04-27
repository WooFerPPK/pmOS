//! `/bin/hello-toplevel` — minimal app that opens a real
//! pmd_xdg_toplevel and paints a solid color.
//!
//! Useful as the launcher's first "this actually works"
//! demo: clicking the launcher → kernel spawns this binary
//! → it connects to the display server → shell-manager
//! broadcasts a window_created → the desktop shell paints
//! a taskbar entry → the toplevel surface composites into
//! the framebuffer alongside the wallpaper.
//!
//! No interactivity beyond responding to the server's
//! configure handshake. Exits cleanly on
//! `xdg_toplevel.close` so the user can dismiss the window
//! via the close button (when the toolkit decoration lands)
//! or by closing it from the shell-manager (focus_window
//! click on the taskbar; close request via
//! shell_manager.close_window).

use toolkit::draw::{Color, Rect};
use toolkit::{App, BufferPool, Window};

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    use super::*;
    use toolkit::protocol::Connection;

    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn fd_read(
            fd: i32,
            iovs_ptr: *const Iovec,
            iovs_len: i32,
            nread_ptr: *mut u32,
        ) -> i32;
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
    const SEND_MAX_POLLS: u32 = 50_000;
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
    }

    impl FdConnection {
        pub fn new(fd: i32) -> Self {
            FdConnection { fd }
        }
    }

    impl Connection for FdConnection {
        fn send(&mut self, bytes: &[u8]) {
            if bytes.is_empty() {
                return;
            }
            let mut sent = 0usize;
            let mut spins: u32 = 0;
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
                    spins = 0;
                    continue;
                }
                if rc == 0 || rc == EAGAIN || rc == EINVAL {
                    spins = spins.saturating_add(1);
                    if spins > SEND_MAX_POLLS {
                        return;
                    }
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
            for _ in 0..RECV_MAX_POLLS {
                let rc = unsafe { fd_read(self.fd, &iov, 1, &mut nread) };
                if rc == 0 && nread > 0 {
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
            alloc::vec::Vec::new()
        }
    }

    pub fn run() {
        println!("hello-toplevel: starting");
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
    window.set_title("Hello Window")?;
    window.set_app_id("pmos.hello")?;
    window.commit()?;

    // Solid pinkish-purple fill so the demo window stands
    // out against the desktop wallpaper grey.
    let fill = Color::rgb(0xc8, 0x60, 0xa8);
    let mut painted = false;
    let mut iters: u32 = 0;
    let max_iters: u32 = 200_000;
    while iters < max_iters {
        iters += 1;
        let _ = window.dispatch()?;
        if window.close_requested() {
            return Ok(());
        }
        if !painted && window.is_configured() {
            // Ignore the server's configured size — this demo
            // is deliberately a small fixed-size window so it
            // doesn't fill the whole screen and the taskbar /
            // wallpaper stay visible behind it. The server's
            // initial-configure value is the work area, which
            // for a single-output v1 fb is full-screen.
            let (w, h) = (320u32, 200u32);
            let mut pool: BufferPool = BufferPool::new(window.app_mut(), w, h)?;
            if let Some(mut canvas) = pool.acquire_back_canvas() {
                canvas.fill_rect(
                    Rect { x: 0, y: 0, width: w, height: h },
                    fill,
                );
                drop(canvas);
                pool.commit_and_swap(&mut window)?;
                painted = true;
            }
        }
    }
    Ok(())
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    wasm_main::run();
    #[cfg(not(target_arch = "wasm32"))]
    println!("hello-toplevel (host build): use `cargo build --target wasm32-wasip1 -p hello-toplevel`");
}
