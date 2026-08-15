//! `/opt/hello/bin/hello.wasm` — third-party hello-world app
//! (T202). Packaged into `hello-0.1.0.pmpkg.tar` by
//! `xtask package sample-app` for the US10 install-flow
//! integration test.
//!
//! Trivial toolkit window: solid background, titlebar strip with a label, and
//! body text. The window stays open until the server emits
//! `xdg_toplevel.close`.

#[cfg(target_arch = "wasm32")]
use toolkit::draw::font::GLYPH_HEIGHT;
#[cfg(target_arch = "wasm32")]
use toolkit::draw::{Color, Rect};
#[cfg(target_arch = "wasm32")]
use toolkit::{App, BufferPool, Window};

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn proc_exit(rval: i32) -> !;
    }

    pub fn run() {
        println!("hello: starting");
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

#[cfg(target_arch = "wasm32")]
fn run_window<C: toolkit::protocol::Connection>(connection: C) -> Result<(), toolkit::ClientError> {
    let mut app = App::connect(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Hello, PMos!")?;
    window.set_app_id("pmos.sample-app")?;
    window.commit()?;

    let bg = Color::rgb(0x2a, 0x6c, 0x8a);
    let titlebar = Color::rgb(0x1a, 0x4c, 0x6a);
    let mut painted = false;
    let mut pool: Option<BufferPool> = None;
    loop {
        let _ = window.dispatch()?;
        if window.close_requested() {
            return Ok(());
        }
        if let Some(buffers) = pool.as_mut().filter(|buffers| buffers.commit_pending()) {
            if buffers.progress_commit(&mut window)? == toolkit::CommitProgress::Committed {
                painted = true;
                println!("hello: ready");
            }
        }
        if !painted && window.is_configured() {
            let (width, height) = (300u32, 160u32);
            if pool.is_none() {
                pool = Some(BufferPool::new(window.app_mut(), width, height)?);
            }
            let buffers = pool.as_mut().expect("pool created above");
            if let Some(mut canvas) = buffers.acquire_back_canvas() {
                canvas.fill_rect(Rect::new(0, 0, width, height), bg);
                let titlebar_height = 22u32;
                canvas.fill_rect(Rect::new(0, 0, width, titlebar_height), titlebar);
                let title_y = ((titlebar_height as i32 - GLYPH_HEIGHT as i32) / 2).max(0);
                canvas.draw_text(8, title_y, "Hello, PMos!", Color::rgb(0xff, 0xff, 0xff));
                canvas.draw_text(
                    14,
                    titlebar_height as i32 + 24,
                    "Hello from a third-party app.",
                    Color::rgb(0xff, 0xff, 0xff),
                );
                canvas.draw_text(
                    14,
                    titlebar_height as i32 + 44,
                    "Installed via pkginstall + .pmpkg.tar.",
                    Color::rgb(0xcf, 0xe6, 0xf5),
                );
                drop(canvas);
                if buffers.commit_and_swap(&mut window)? == toolkit::CommitProgress::Committed {
                    painted = true;
                    println!("hello: ready");
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
    println!("sample-app (host build): use `cargo build --target wasm32-wasip1 -p sample-app`");
}
