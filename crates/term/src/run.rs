//! Terminal app event loop.
//!
//! [`run_term`] is the production driver: connect via
//! [`toolkit::App::connect_with_shell`] (which also binds
//! `pmd_seat` + `pmd_keyboard` + `pmd_pointer` when the
//! server advertises them), create a top-level surface, set
//! the title, drive the configure handshake, paint a
//! scrollback buffer per [`crate::rasterizer`], and route
//! `pmd_keyboard.key` events through [`crate::keymap`] into
//! the embedded [`Terminal`].
//!
//! The shell is embedded directly: each committed input line
//! is fed into the [`Terminal`]'s in-process [`sh::Shell`]
//! evaluator. v1 cannot spawn `/bin/sh` as a child because
//! the kernel's pipe `fd_read` returns `WouldBlock`/EAGAIN
//! immediately rather than parking the caller — `sh::run`'s
//! `BufRead::read_line` would error out on the first poll
//! against an empty pipe. Once the kernel grows blocking
//! pipe reads (the comment near `crates/kernel/src/syscall/
//! wasi.rs:2384` documents the planned slice) this driver
//! will switch to `proc_spawn /bin/sh` + ipc_pipe stdio.

use display_proto::events::{KeyboardKey, key_state};
use display_proto::Interface;
use toolkit::draw::{Canvas, Color};
use toolkit::{App, BufferPool, ClientError, Connection, Window};

use crate::keymap::{translate, Modifiers};
use crate::rasterizer::{rasterize_snapshot_with_palette, Palette, BYTES_PER_PIXEL};
use crate::terminal::{KeyFeedResult, Terminal, TerminalOptions};

/// Default window size when the server defers to the client
/// (`configure(0, 0)`).
pub const DEFAULT_WIDTH: u32 = 720;
/// Default window height in the same case.
pub const DEFAULT_HEIGHT: u32 = 480;

/// Result of running [`run_term`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermExit {
    /// The server requested the window close via
    /// `xdg_toplevel::close`.
    CloseRequested,
    /// The embedded shell's `exit` builtin fired.
    ShellExited,
    /// `max_dispatch_iterations` exhausted without exit.
    IterationLimit,
}

/// Connect to the display server, drive a terminal window
/// against it, and route keystrokes through the embedded
/// [`Terminal`] until the user closes the window or the
/// shell exits.
///
/// `max_dispatch_iterations` caps the loop so unit tests can
/// assert non-hanging behaviour. Production passes
/// `u32::MAX`.
pub fn run_term<C: Connection>(
    connection: C,
    max_dispatch_iterations: u32,
) -> Result<TermExit, ClientError> {
    run_term_with_options(
        connection,
        max_dispatch_iterations,
        TerminalOptions {
            max_lines: 1024,
            banner: vec![
                "PMos Terminal".to_string(),
                "Type a command and press Enter. `help` for builtins, `exit` to quit.".to_string(),
            ],
            prompt: "$ ".to_string(),
        },
    )
}

/// Variant of [`run_term`] that accepts pre-baked
/// [`TerminalOptions`] — used by tests to control the
/// banner / prompt without inheriting the production
/// defaults.
pub fn run_term_with_options<C: Connection>(
    connection: C,
    max_dispatch_iterations: u32,
    options: TerminalOptions,
) -> Result<TermExit, ClientError> {
    let mut app = App::connect_with_shell(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Terminal")?;
    window.set_app_id("pmos.term")?;
    window.commit()?;

    let mut terminal = Terminal::new(options);
    let mut mods = Modifiers::default();

    let mut last_size: (u32, u32) = (0, 0);
    let mut pool: Option<BufferPool> = None;
    let mut needs_paint = false;
    let mut force_first_paint_done = false;

    for _ in 0..max_dispatch_iterations {
        let events = window.dispatch()?;

        if window.close_requested() {
            return Ok(TermExit::CloseRequested);
        }

        for event in events {
            match (event.interface, event.opcode) {
                (Interface::Keyboard, 1 /* key */) => {
                    if let Ok(key) = KeyboardKey::decode(&event.payload) {
                        let pressed = key.state == key_state::PRESSED;
                        let was_modifier = mods.update(key.key, pressed);
                        if !pressed || was_modifier {
                            continue;
                        }
                        let Some(translated) = translate(key.key, mods) else {
                            continue;
                        };
                        match terminal.feed_key(translated) {
                            KeyFeedResult::Edited => needs_paint = true,
                            KeyFeedResult::Committed { exited, .. } => {
                                needs_paint = true;
                                if exited {
                                    // Paint the final state
                                    // before bailing so the
                                    // user sees the last
                                    // line of output.
                                    paint_terminal(
                                        &mut window,
                                        &mut pool,
                                        &mut last_size,
                                        &terminal,
                                    )?;
                                    return Ok(TermExit::ShellExited);
                                }
                            }
                            KeyFeedResult::Ignored => {}
                        }
                    }
                }
                (Interface::Buffer, 1 /* release */) => {
                    if let Some(p) = pool.as_mut() {
                        let _ = p.handle_release(event.object_id);
                    }
                }
                _ => {}
            }
        }

        if !force_first_paint_done && window.is_configured() {
            needs_paint = true;
            force_first_paint_done = true;
        }

        if needs_paint && window.is_configured() {
            if paint_terminal(&mut window, &mut pool, &mut last_size, &terminal)? {
                needs_paint = false;
            }
        }
    }

    Ok(TermExit::IterationLimit)
}

/// One paint pass. Returns `Ok(true)` iff a buffer was
/// successfully acquired and committed; `Ok(false)` when the
/// back buffer is awaiting `pmd_buffer.release`.
fn paint_terminal<C: Connection>(
    window: &mut Window<'_, C>,
    pool: &mut Option<BufferPool>,
    last_size: &mut (u32, u32),
    terminal: &Terminal,
) -> Result<bool, ClientError> {
    // The server's initial configure offers the full work area;
    // for a v1 terminal that's too large to be useful, and the
    // toolkit doesn't yet send a `set_min_size` / `set_max_size`
    // hint to nudge the server toward a smaller suggestion.
    // Clamp to DEFAULT_WIDTH × DEFAULT_HEIGHT so the window
    // opens at a comfortable default; once interactive resize
    // (T133) lands the user can drag it bigger.
    let (cfg_w, cfg_h) = window.configured_size();
    let (w, h) = if cfg_w == 0 || cfg_h == 0 {
        (DEFAULT_WIDTH, DEFAULT_HEIGHT)
    } else {
        (cfg_w.min(DEFAULT_WIDTH), cfg_h.min(DEFAULT_HEIGHT))
    };

    if *last_size != (w, h) {
        *pool = Some(BufferPool::new(window.app_mut(), w, h)?);
        *last_size = (w, h);
    }
    let p = pool.as_mut().expect("pool initialised when w/h are set");
    let Some(mut canvas) = p.acquire_back_canvas() else {
        return Ok(false);
    };

    paint_into_canvas(&mut canvas, w, h, terminal);
    drop(canvas);
    p.commit_and_swap(window)?;
    Ok(true)
}

/// Rasterise the terminal snapshot into the supplied
/// canvas. The rasterizer produces a tightly-packed ARGB8888
/// frame of `width × height` pixels; we copy it directly
/// onto the canvas. Both buffers share the same byte order,
/// so the copy is a single `pixels_mut().copy_from_slice`.
fn paint_into_canvas(canvas: &mut Canvas<'_>, width: u32, height: u32, terminal: &Terminal) {
    // Background fill is performed by the rasterizer itself;
    // the canvas wipe below is defensive — any pixel the
    // rasterizer skipped (out-of-bounds glyph row, padding
    // edge) lands at a known colour rather than garbage from
    // a previous frame still resident in the back buffer.
    canvas.clear(Color::rgb(0x0a, 0x0e, 0x14));
    let snapshot = terminal.snapshot();
    let bytes = rasterize_snapshot_with_palette(&snapshot, width, height, Palette::default());
    let pixels = canvas.pixels_mut();
    let copy_len = pixels.len().min(bytes.len());
    pixels[..copy_len].copy_from_slice(&bytes[..copy_len]);
    debug_assert_eq!(
        bytes.len(),
        (width as usize) * (height as usize) * BYTES_PER_PIXEL
    );
}
