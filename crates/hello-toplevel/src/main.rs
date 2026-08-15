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

use toolkit::draw::font::GLYPH_HEIGHT;
use toolkit::draw::{Color, Rect};
use toolkit::{App, BufferPool, Window};

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn proc_exit(rval: i32) -> !;
    }

    pub fn run() {
        println!("hello-toplevel: starting");
        let conn = match toolkit::wasi::FdConnection::connect() {
            Ok(connection) => connection,
            Err(errno) => unsafe { proc_exit(errno) },
        };
        match super::run_window(conn) {
            Ok(_) => unsafe { proc_exit(0) },
            Err(_) => unsafe { proc_exit(1) },
        }
    }
}

#[cfg(target_arch = "wasm32")]
extern crate alloc;

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn run_window<C: toolkit::protocol::Connection>(connection: C) -> Result<(), toolkit::ClientError> {
    let mut app = App::connect(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Hello Window")?;
    window.set_app_id("pmos.hello")?;
    window.commit()?;

    // Solid pinkish-purple fill so the demo window stands
    // out against the desktop wallpaper grey.
    let fill = Color::rgb(0xc8, 0x60, 0xa8);
    let mut painted = false;
    let mut pool: Option<BufferPool> = None;
    // Loop forever — the only legitimate exit is the
    // server emitting `xdg_toplevel.close` (e.g. user
    // clicked the close button or shell-manager.close_window).
    // A bounded iteration count caused the demo to paint
    // once + spin its way to exit in seconds, which the
    // user observed as a "purple flash that disappeared".
    loop {
        let _ = window.dispatch()?;
        if window.close_requested() {
            return Ok(());
        }
        if let Some(buffers) = pool.as_mut().filter(|buffers| buffers.commit_pending()) {
            if buffers.progress_commit(&mut window)? == toolkit::CommitProgress::Committed {
                painted = true;
            }
        }
        if !painted && window.is_configured() {
            // Ignore the server's configured size — this demo
            // is deliberately a small fixed-size window so it
            // doesn't fill the whole screen and the taskbar /
            // wallpaper stay visible behind it. The server's
            // initial-configure value is the work area, which
            // for a single-output v1 fb is full-screen.
            let (w, h) = (320u32, 200u32);
            if pool.is_none() {
                pool = Some(BufferPool::new(window.app_mut(), w, h)?);
            }
            let buffers = pool.as_mut().expect("pool created above");
            if let Some(mut canvas) = buffers.acquire_back_canvas() {
                // Solid fill, then a thin border, then a
                // titlebar-like strip with a label so the
                // demo is clearly recognisable as a window
                // (not just an unframed colour blob).
                canvas.fill_rect(
                    Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    },
                    fill,
                );
                let titlebar_h = 22u32;
                let titlebar = Color::rgb(0x88, 0x40, 0x70);
                canvas.fill_rect(
                    Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: titlebar_h,
                    },
                    titlebar,
                );
                let title = "Hello Window";
                let tx = 8;
                let ty = ((titlebar_h as i32 - GLYPH_HEIGHT as i32) / 2).max(0);
                canvas.draw_text(tx, ty, title, Color::rgb(0xff, 0xff, 0xff));
                let body = "Spawned via the desktop launcher.";
                canvas.draw_text(
                    14,
                    titlebar_h as i32 + 24,
                    body,
                    Color::rgb(0xff, 0xff, 0xff),
                );
                drop(canvas);
                if buffers.commit_and_swap(&mut window)? == toolkit::CommitProgress::Committed {
                    painted = true;
                }
            }
        }
        window.flush_outbound()?;
        if pool.as_ref().is_some_and(BufferPool::commit_pending) && !window.outbound_pending() {
            continue;
        }
        window.wait(None)?;
    }
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    wasm_main::run();
    #[cfg(not(target_arch = "wasm32"))]
    println!(
        "hello-toplevel (host build): use `cargo build --target wasm32-wasip1 -p hello-toplevel`"
    );
}
