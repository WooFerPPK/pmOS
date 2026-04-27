//! Desktop-shell paint loop — wallpaper + taskbar.
//!
//! Exposes [`run_shell`] — the shell's main event loop
//! used by the `shell` binary and the
//! `tests/paint_wallpaper.rs` integration test. Separates
//! connection setup (done by `main`) from the long-running
//! loop so tests can drive the loop with a mock connection
//! without going through the real WASI IPC shim.
//!
//! Scope for this slice:
//!
//! * connect via [`toolkit::App::connect`]
//! * create a top-level window via [`toolkit::Window::new`]
//! * set title, commit to solicit the server's first
//!   `configure`
//! * loop up to `max_dispatch_iterations` times
//! * on the first configure event: allocate a
//!   [`toolkit::BufferPool`], paint the wallpaper colour
//!   into the back canvas, paint the [`crate::Taskbar`]
//!   strip on top, and commit exactly once
//! * return [`ShellExit::CloseRequested`] on
//!   `xdg_toplevel::close`, or [`ShellExit::IterationLimit`]
//!   on cap exhaustion
//!
//! Explicitly deferred: launcher, click handling on the
//! taskbar, `pmd_shell_manager` event-stream wiring (the
//! taskbar paints with whatever entries the caller has
//! pushed in via [`crate::Taskbar::add_window`]),
//! frame-callback-driven redraw, real IPC wiring. See T121
//! partial note in `tasks.md` for the running scope list.

use crate::taskbar::Taskbar;
use toolkit::draw::{Color, Rect};
use toolkit::theme::Theme;
use toolkit::{App, BufferPool, ClientError, Connection, Window};

/// Default window size used when the server has not
/// suggested a preferred size via `configure(w, h)`.
/// 640×480 is the legacy-safe fallback for a v1 desktop
/// wallpaper surface.
pub const DEFAULT_WIDTH: u32 = 640;

/// Default window height when the server defers to the
/// client for sizing.
pub const DEFAULT_HEIGHT: u32 = 480;

/// Result of running the event loop. Returned by
/// [`run_shell`] so tests can distinguish "server requested
/// close" from "hit the iteration cap".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellExit {
    /// The server requested the window to close via
    /// `xdg_toplevel::close`.
    CloseRequested,
    /// `max_dispatch_iterations` exhausted without a close.
    IterationLimit,
}

/// Run the shell against a connected display server.
///
/// The body is deliberately narrow: connect → create
/// top-level window → set title → commit to solicit the
/// server's first `configure` → loop up to
/// `max_dispatch_iterations` times; on the first
/// configure, allocate a `BufferPool`, paint the wallpaper
/// colour, attach + damage + commit, and then idle. The
/// paint-once guard avoids re-issuing a commit on every
/// loop iteration; a future slice replaces it with a
/// frame-callback-driven paint cycle.
///
/// # Errors
///
/// Propagates [`ClientError`] from every underlying
/// request — `MissingGlobal` if the server doesn't
/// advertise one of the three required globals, `Wire` on
/// encoding overflow, and the rest from request sends.
pub fn run_shell<C: Connection>(
    connection: C,
    max_dispatch_iterations: u32,
) -> Result<ShellExit, ClientError> {
    run_shell_with_taskbar(connection, max_dispatch_iterations, Taskbar::new(0, 0))
}

/// Variant of [`run_shell`] that accepts a pre-populated
/// [`Taskbar`] so callers (production shell main, test
/// fixtures) can stage entries before the paint loop fires.
/// The taskbar's framebuffer dimensions are fixed up to the
/// surface's configured size on the first paint iteration —
/// callers don't have to know the size up front.
pub fn run_shell_with_taskbar<C: Connection>(
    connection: C,
    max_dispatch_iterations: u32,
    mut taskbar: Taskbar,
) -> Result<ShellExit, ClientError> {
    let mut app = App::connect(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("PMos")?;
    window.commit()?;

    let theme = Theme::default();
    let wallpaper: Color = theme.window_background;
    let mut painted = false;

    for _ in 0..max_dispatch_iterations {
        // dispatch() internally acks any configure + records
        // close; passthrough events are ignored in this slice
        // (input/frame handling lands in a later slice).
        let _ = window.dispatch()?;

        if window.close_requested() {
            return Ok(ShellExit::CloseRequested);
        }

        if !painted && window.is_configured() {
            let (cfg_w, cfg_h) = window.configured_size();
            let (w, h) = if cfg_w == 0 || cfg_h == 0 {
                (DEFAULT_WIDTH, DEFAULT_HEIGHT)
            } else {
                (cfg_w, cfg_h)
            };
            taskbar.set_framebuffer_size(w, h);

            let mut pool = BufferPool::new(window.app_mut(), w, h)?;
            if let Some(mut canvas) = pool.acquire_back_canvas() {
                canvas.fill_rect(
                    Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    },
                    wallpaper,
                );
                taskbar.draw(&mut canvas);
                drop(canvas);
                pool.commit_and_swap(&mut window)?;
                painted = true;
                println!("shell: wallpaper painted");
            }
            // If acquire returned None (back buffer in-use),
            // leave `painted=false` and retry on the next
            // iteration — should not happen on the first
            // paint since both buffers start free.
        }
    }

    Ok(ShellExit::IterationLimit)
}
