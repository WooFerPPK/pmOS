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
use display_proto::events::{
    PointerButton, PointerMotion, ShellWindowCreated, ShellWindowDestroyed,
    ShellWindowFocused, ShellWindowTitleChanged,
};
use display_proto::Interface;
use toolkit::draw::font::GLYPH_HEIGHT;
use toolkit::draw::{Canvas, Color, Rect};
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

/// One slot in the launcher's app catalog. Each slot is a
/// pair of (label, exec-path). Clicking a launcher item
/// invokes the caller-supplied [`SpawnFn`] with the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherSlot {
    pub label: &'static str,
    pub exec: &'static str,
}

/// Default catalog the production shell ships with.
/// Each entry is a `.wasm` binary the kernel will spawn when
/// the corresponding launcher row is clicked. The list is
/// hard-coded for v1; a future slice swaps it for a
/// `.desktop` file scan via [`crate::launcher::Launcher`].
pub const DEFAULT_LAUNCHER_SLOTS: &[LauncherSlot] = &[
    LauncherSlot { label: "Hello Window", exec: "/bin/hello-toplevel" },
    LauncherSlot { label: "Term",         exec: "/bin/term" },
    LauncherSlot { label: "Files",        exec: "/bin/files" },
    LauncherSlot { label: "Edit",         exec: "/bin/edit" },
    LauncherSlot { label: "Settings",     exec: "/bin/settings" },
    LauncherSlot { label: "Sysmon",       exec: "/bin/sysmon" },
];

/// Width of the launcher button in pixels — the leftmost
/// item on the taskbar strip. Reads "Launch" by default.
pub const LAUNCHER_BUTTON_WIDTH: u32 = 80;
/// Margin between the launcher button and the leftmost
/// taskbar entry.
pub const LAUNCHER_BUTTON_RIGHT_MARGIN: u32 = 6;
/// Width of the popup menu when the launcher is open.
pub const LAUNCHER_MENU_WIDTH: u32 = 200;
/// Per-row height inside the launcher menu.
pub const LAUNCHER_MENU_ROW_HEIGHT: u32 = 24;
/// Inset the menu's text from the row's left edge.
pub const LAUNCHER_MENU_TEXT_MARGIN: u32 = 8;
/// Pixels of padding inside the menu (top + bottom).
pub const LAUNCHER_MENU_PADDING: u32 = 4;

/// Function the shell calls when the user requests an app
/// be launched. Production main supplies a function that
/// invokes `pmos_ext.proc_spawn`; tests pass a closure that
/// records the spawn request without crossing a syscall.
/// Returns a non-negative pid on success, negative errno on
/// failure (matches the wasi shim's signed-i32 convention).
pub type SpawnFn = fn(path: &str) -> i32;

/// Trait alternative to [`SpawnFn`] so callers can use closures
/// or method handles. Implemented for any `FnMut(&str) -> i32`.
pub trait Spawner {
    fn spawn(&mut self, path: &str) -> i32;
}

impl<F> Spawner for F
where
    F: FnMut(&str) -> i32,
{
    fn spawn(&mut self, path: &str) -> i32 {
        (self)(path)
    }
}

/// Run the desktop shell's full event-driven loop.
///
/// Differences from [`run_shell_with_taskbar`]:
///
/// * Uses [`App::connect_with_shell`] so `pmd_seat`,
///   `pmd_pointer`, and `pmd_shell_manager` are bound up
///   front (when the server advertises them).
/// * Subscribes to `pmd_shell_manager.window_*` so the
///   taskbar repopulates from server-broadcast events
///   (its OWN toplevel + every other client's toplevels
///   added/removed/focused at runtime).
/// * Routes `pmd_pointer.button` events through the
///   launcher and the taskbar — clicking a taskbar entry
///   sends `pmd_shell_manager.focus_window`, clicking the
///   launcher button opens a popup menu, clicking a popup
///   row dispatches the configured exec path through
///   the supplied [`Spawner`].
/// * Repaints the wallpaper / taskbar / launcher only when
///   state actually changed, so the loop yields back to
///   the worker between frames.
pub fn run_desktop_shell<C: Connection, S: Spawner>(
    connection: C,
    max_dispatch_iterations: u32,
    mut taskbar: Taskbar,
    slots: &[LauncherSlot],
    mut spawner: S,
) -> Result<ShellExit, ClientError> {
    println!("shell: run_desktop_shell entered");
    let mut app = match App::connect_with_shell(connection) {
        Ok(a) => {
            println!(
                "shell: App::connect_with_shell ok (seat={:?} pointer={:?} shell_manager={:?})",
                a.seat(), a.pointer(), a.shell_manager(),
            );
            a
        }
        Err(e) => {
            println!("shell: App::connect_with_shell failed: {:?}", e);
            return Err(e);
        }
    };
    let mut window = match Window::new(&mut app) {
        Ok(w) => w,
        Err(e) => {
            println!("shell: Window::new failed: {:?}", e);
            return Err(e);
        }
    };
    if let Err(e) = window.set_title("PMos") {
        println!("shell: set_title failed: {:?}", e);
        return Err(e);
    }
    if let Err(e) = window.commit() {
        println!("shell: commit failed: {:?}", e);
        return Err(e);
    }
    println!("shell: window created + committed");

    if window.app_mut().shell_manager().is_some() {
        match window.app_mut().shell_manager_subscribe_windows() {
            Ok(_) => println!("shell: subscribed to shell_manager"),
            Err(e) => println!("shell: subscribe failed: {:?}", e),
        }
    } else {
        println!("shell: no shell_manager bound");
    }

    let theme = Theme::default();
    let wallpaper = theme.window_background;
    let mut last_size: (u32, u32) = (0, 0);
    let mut pool: Option<BufferPool> = None;
    let mut launcher_open = false;
    let mut menu_hover: Option<usize> = None;

    let mut needs_paint = false;
    let mut force_first_paint_done = false;

    let mut iter_count: u32 = 0;
    let mut total_events: u64 = 0;
    let mut total_motion: u64 = 0;
    let mut total_buttons: u64 = 0;
    let mut total_paints: u64 = 0;
    let mut stuck_acquire_count: u64 = 0;
    let mut last_heartbeat_iter: u32 = 0;
    // Heartbeat cadence — every N iterations. A no-events
    // loop runs many thousands of iters/sec because each
    // FdConnection.recv is bounded; under pure idle the
    // heartbeat should fire in well under a second. If the
    // watchdog (8s of zero console output) trips before any
    // heartbeat lands, the shell is wedged inside one of
    // the syscalls (recv, fd_write, sched_yield), not just
    // event-starved.
    const HEARTBEAT_EVERY: u32 = 1_000;
    for _ in 0..max_dispatch_iterations {
        iter_count = iter_count.wrapping_add(1);
        let events = match window.dispatch() {
            Ok(e) => e,
            Err(e) => {
                println!("shell: dispatch error at iter {}: {:?}", iter_count, e);
                return Err(e);
            }
        };

        if iter_count.wrapping_sub(last_heartbeat_iter) >= HEARTBEAT_EVERY {
            println!(
                "shell: heartbeat iter={} total_events={} motion={} buttons={} paints={} stuck_acquire={} needs_paint={} close_req={} configured={}",
                iter_count,
                total_events,
                total_motion,
                total_buttons,
                total_paints,
                stuck_acquire_count,
                needs_paint,
                window.close_requested(),
                window.is_configured(),
            );
            last_heartbeat_iter = iter_count;
        }

        if window.close_requested() {
            println!("shell: close_requested at iter {}, exiting", iter_count);
            return Ok(ShellExit::CloseRequested);
        }

        if !events.is_empty() {
            total_events += events.len() as u64;
            for ev in &events {
                if ev.interface == Interface::Pointer && ev.opcode == 1 {
                    total_motion += 1;
                } else if ev.interface == Interface::Pointer && ev.opcode == 2 {
                    total_buttons += 1;
                }
            }
        }

        // Only log "interesting" events — pointer motion can
        // arrive in floods (one per pixel of mouse drag) and
        // crowds out everything useful.
        let interesting = events
            .iter()
            .any(|e| !(e.interface == Interface::Pointer && e.opcode == 1));
        if interesting {
            println!(
                "shell: iter {} got {} events, configured={} size={:?}",
                iter_count,
                events.len(),
                window.is_configured(),
                window.configured_size(),
            );
        }

        for event in events {
            let is_motion = event.interface == Interface::Pointer && event.opcode == 1;
            if !is_motion {
                println!(
                    "shell:   event interface={:?} opcode={} object_id={:?} payload_len={}",
                    event.interface, event.opcode, event.object_id, event.payload.len(),
                );
            }
            match (event.interface, event.opcode) {
                (Interface::ShellManager, 1 /* window_created */) => {
                    if let Ok(decoded) = ShellWindowCreated::decode(&event.payload) {
                        taskbar.add_window(decoded.window_id, decoded.title, decoded.app_id);
                        needs_paint = true;
                    }
                }
                (Interface::ShellManager, 2 /* window_destroyed */) => {
                    if let Ok(decoded) = ShellWindowDestroyed::decode(&event.payload) {
                        taskbar.remove_window(decoded.window_id);
                        needs_paint = true;
                    }
                }
                (Interface::ShellManager, 3 /* window_focused */) => {
                    if let Ok(decoded) = ShellWindowFocused::decode(&event.payload) {
                        // Only repaint if the focus actually
                        // moved between taskbar entries —
                        // every pointer click on the wallpaper
                        // also broadcasts a window_focused
                        // event for the shell's own toplevel,
                        // which would otherwise re-paint on
                        // every cursor click and saturate the
                        // 64 KiB rx_buf with chunked
                        // shm_pool.write traffic.
                        let prev_focus = taskbar
                            .entries()
                            .iter()
                            .find(|e| e.focused)
                            .map(|e| e.window_id);
                        if prev_focus != Some(decoded.window_id) {
                            taskbar.set_focused_window(decoded.window_id);
                            needs_paint = true;
                        }
                    }
                }
                (Interface::ShellManager, 4 /* window_title_changed */) => {
                    if let Ok(decoded) = ShellWindowTitleChanged::decode(&event.payload) {
                        taskbar.set_window_title(decoded.window_id, decoded.new_title);
                        needs_paint = true;
                    }
                }
                (Interface::Buffer, 1 /* release */) => {
                    if let Some(p) = pool.as_mut() {
                        let _ = p.handle_release(event.object_id);
                    }
                }
                (Interface::Pointer, 1 /* motion */) => {
                    if let Ok(motion) = PointerMotion::decode(&event.payload) {
                        let new_hover = if launcher_open {
                            launcher_menu_row_at(&taskbar, slots, motion.x, motion.y)
                        } else {
                            None
                        };
                        if new_hover != menu_hover {
                            menu_hover = new_hover;
                            if launcher_open {
                                needs_paint = true;
                            }
                        }
                    }
                }
                (Interface::Pointer, 2 /* button */) => {
                    if let Ok(button) = PointerButton::decode(&event.payload) {
                        if button.state == display_proto::events::pointer_button_state::PRESSED {
                            // Convert from surface-local
                            // coordinates the server emitted
                            // back to framebuffer-space. The
                            // shell's only window is full-
                            // screen at (0,0), so local ==
                            // screen for our hit tests. If
                            // future slices position the
                            // wallpaper surface differently
                            // we'll compose with the toplevel
                            // origin here.
                            //
                            // Only mark needs_paint when the
                            // press actually changed shell
                            // visual state — opening or
                            // closing the launcher menu,
                            // toggling a hover, etc. A click
                            // that "missed everything" doesn't
                            // change pixels and shouldn't
                            // trigger the 3 MiB chunked paint
                            // which saturates the rx_buf.
                            let launcher_was_open = launcher_open;
                            let menu_hover_before = menu_hover;
                            handle_press(
                                button.x,
                                button.y,
                                &taskbar,
                                slots,
                                &mut launcher_open,
                                &mut menu_hover,
                                &mut spawner,
                                window.app_mut(),
                            );
                            let visual_change = launcher_open != launcher_was_open
                                || menu_hover != menu_hover_before;
                            if visual_change {
                                needs_paint = true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // First paint always fires once configure lands.
        if !force_first_paint_done && window.is_configured() {
            needs_paint = true;
            force_first_paint_done = true;
        }

        if needs_paint && window.is_configured() {
            let (cfg_w, cfg_h) = window.configured_size();
            let (w, h) = if cfg_w == 0 || cfg_h == 0 {
                (DEFAULT_WIDTH, DEFAULT_HEIGHT)
            } else {
                (cfg_w, cfg_h)
            };
            taskbar.set_framebuffer_size(w, h);

            // Re-allocate the pool only when geometry
            // changes; otherwise reuse so the back/front
            // swap doesn't churn allocations.
            if last_size != (w, h) {
                println!("shell: allocating BufferPool {}x{}", w, h);
                pool = Some(match BufferPool::new(window.app_mut(), w, h) {
                    Ok(p) => p,
                    Err(e) => {
                        println!("shell: BufferPool::new failed: {:?}", e);
                        return Err(e);
                    }
                });
                last_size = (w, h);
            }
            let p = pool.as_mut().expect("pool initialised when w/h are set");
            // Detect prolonged back-pressure: if every
            // acquire returns None for many iterations the
            // server has stopped emitting buffer-release
            // events and we'll never paint again. Log every
            // 100 stuck iterations so we know to investigate.
            let acquire_attempt = p.acquire_back_canvas();
            if acquire_attempt.is_none() {
                stuck_acquire_count += 1;
                if stuck_acquire_count == 1 || stuck_acquire_count % 100 == 0 {
                    println!(
                        "shell: acquire_back_canvas returned None at iter {} (stuck for {} attempts)",
                        iter_count, stuck_acquire_count,
                    );
                }
            } else {
                if stuck_acquire_count > 0 {
                    println!(
                        "shell: acquire recovered after {} stuck attempts at iter {}",
                        stuck_acquire_count, iter_count,
                    );
                    stuck_acquire_count = 0;
                }
            }
            if let Some(mut canvas) = acquire_attempt {
                canvas.fill_rect(
                    Rect { x: 0, y: 0, width: w, height: h },
                    wallpaper,
                );
                taskbar.draw(&mut canvas);
                draw_launcher_button(&mut canvas, &taskbar, &theme, launcher_open);
                if launcher_open {
                    draw_launcher_menu(&mut canvas, &taskbar, &theme, slots, menu_hover);
                }
                drop(canvas);
                if let Err(e) = p.commit_and_swap(&mut window) {
                    println!("shell: commit_and_swap failed at iter {}: {:?}", iter_count, e);
                    return Err(e);
                }
                total_paints += 1;
                // Only spam the per-paint line every 20th
                // paint so the long-run heartbeat stays
                // readable. Boot's first few paints still
                // print individually.
                if total_paints <= 5 || total_paints % 20 == 0 {
                    println!(
                        "shell: painted frame #{} at iter {} (taskbar={}, launcher_open={})",
                        total_paints, iter_count, taskbar.entries().len(), launcher_open,
                    );
                }
                needs_paint = false;
            }
        }
    }

    println!("shell: iteration limit hit ({} iters)", iter_count);
    Ok(ShellExit::IterationLimit)
}

/// Bounds of the launcher button in framebuffer space —
/// leftmost slot of the taskbar strip.
fn launcher_button_bounds(taskbar: &Taskbar) -> Rect {
    let bar = taskbar.bounds();
    let pad = 2_i32;
    let h = (crate::taskbar::TASKBAR_HEIGHT as i32 - pad * 2).max(1) as u32;
    Rect::new(
        bar.x + (crate::taskbar::TASKBAR_LEFT_MARGIN as i32),
        bar.y + pad,
        LAUNCHER_BUTTON_WIDTH,
        h,
    )
}

/// Bounds of the launcher menu when open. Anchored above
/// the launcher button, growing upward.
fn launcher_menu_bounds(taskbar: &Taskbar, slots: &[LauncherSlot]) -> Rect {
    let bar = taskbar.bounds();
    let height =
        LAUNCHER_MENU_PADDING * 2 + LAUNCHER_MENU_ROW_HEIGHT * slots.len() as u32;
    let x = bar.x + (crate::taskbar::TASKBAR_LEFT_MARGIN as i32);
    let y = bar.y - height as i32;
    Rect::new(x, y, LAUNCHER_MENU_WIDTH, height)
}

/// Bounds of menu row `idx` in framebuffer space.
fn launcher_menu_row_bounds(
    taskbar: &Taskbar,
    slots: &[LauncherSlot],
    idx: usize,
) -> Option<Rect> {
    if idx >= slots.len() {
        return None;
    }
    let menu = launcher_menu_bounds(taskbar, slots);
    let y = menu.y
        + LAUNCHER_MENU_PADDING as i32
        + (idx as i32) * LAUNCHER_MENU_ROW_HEIGHT as i32;
    Some(Rect::new(menu.x, y, LAUNCHER_MENU_WIDTH, LAUNCHER_MENU_ROW_HEIGHT))
}

/// Hit-test a point against the menu rows; returns the
/// hovered row index if any.
fn launcher_menu_row_at(
    taskbar: &Taskbar,
    slots: &[LauncherSlot],
    x: i32,
    y: i32,
) -> Option<usize> {
    let menu = launcher_menu_bounds(taskbar, slots);
    if x < menu.x || x >= menu.right() || y < menu.y || y >= menu.bottom() {
        return None;
    }
    for idx in 0..slots.len() {
        let row = launcher_menu_row_bounds(taskbar, slots, idx)?;
        if y >= row.y && y < row.bottom() {
            return Some(idx);
        }
    }
    None
}

/// Paint the "Launch" button onto the canvas. Distinguishes
/// open / closed states with the active-titlebar / hover
/// palette swap.
fn draw_launcher_button(
    canvas: &mut Canvas<'_>,
    taskbar: &Taskbar,
    theme: &Theme,
    open: bool,
) {
    let bounds = launcher_button_bounds(taskbar);
    let fill = if open {
        theme.button_fill_pressed
    } else {
        theme.button_fill
    };
    canvas.fill_rect(bounds, fill);
    canvas.stroke_rect(bounds, theme.border_active);
    let label = "Launch";
    let text_x = bounds.x + 8;
    let text_y = bounds.y + ((bounds.height as i32 - GLYPH_HEIGHT as i32) / 2).max(0);
    canvas.draw_text(text_x, text_y, label, theme.button_text);
}

/// Paint the launcher popup menu (only when open).
fn draw_launcher_menu(
    canvas: &mut Canvas<'_>,
    taskbar: &Taskbar,
    theme: &Theme,
    slots: &[LauncherSlot],
    hover: Option<usize>,
) {
    let menu = launcher_menu_bounds(taskbar, slots);
    canvas.fill_rect(menu, theme.window_background);
    canvas.stroke_rect(menu, theme.border_active);
    for (idx, slot) in slots.iter().enumerate() {
        let Some(row) = launcher_menu_row_bounds(taskbar, slots, idx) else {
            continue;
        };
        if Some(idx) == hover {
            canvas.fill_rect(row, theme.button_fill_hover);
        }
        let text_x = row.x + LAUNCHER_MENU_TEXT_MARGIN as i32;
        let text_y =
            row.y + ((row.height as i32 - GLYPH_HEIGHT as i32) / 2).max(0);
        canvas.draw_text(text_x, text_y, slot.label, theme.label_text);
    }
}

/// Shared press routing — checks the launcher menu first
/// (so a click inside an open menu picks a row), then the
/// launcher button (toggle open/closed), then the taskbar
/// (focus / restore on entry click).
fn handle_press<C: Connection, S: Spawner>(
    x: i32,
    y: i32,
    taskbar: &Taskbar,
    slots: &[LauncherSlot],
    launcher_open: &mut bool,
    menu_hover: &mut Option<usize>,
    spawner: &mut S,
    app: &mut App<C>,
) {
    println!(
        "shell: handle_press at ({}, {}) launcher_open={} taskbar_bounds={:?}",
        x, y, *launcher_open, taskbar.bounds(),
    );
    // Menu has highest priority while open.
    if *launcher_open {
        if let Some(idx) = launcher_menu_row_at(taskbar, slots, x, y) {
            let path = slots[idx].exec;
            println!("shell: launcher row {} ({}) clicked, spawning", idx, path);
            let rc = spawner.spawn(path);
            println!("shell: spawn rc={}", rc);
            // We deliberately don't surface the spawn rc
            // up — a failed spawn is logged but the user's
            // click is still consumed (the menu closes).
            let _ = rc;
            *launcher_open = false;
            *menu_hover = None;
            return;
        }
        // Click outside the menu closes it.
        *launcher_open = false;
        *menu_hover = None;
        // Fall through to the taskbar / launcher button
        // tests below so a click that lands on a taskbar
        // entry while the menu is open still focuses that
        // window.
    }

    let lb = launcher_button_bounds(taskbar);
    println!("shell: launcher button bounds: {:?}", lb);
    if x >= lb.x && x < lb.right() && y >= lb.y && y < lb.bottom() {
        *launcher_open = !*launcher_open;
        *menu_hover = None;
        println!("shell: launcher toggled, now open={}", *launcher_open);
        return;
    }

    if let Some(click) = taskbar.handle_pointer_down(x, y) {
        match click {
            crate::taskbar::TaskbarClick::Focus { window_id }
            | crate::taskbar::TaskbarClick::Restore { window_id } => {
                println!("shell: focus_window({})", window_id);
                let _ = app.shell_manager_focus_window(window_id);
            }
        }
    } else {
        println!("shell: click missed everything");
    }
}
