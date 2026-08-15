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

use crate::desktop_preferences::{
    DesktopPreferenceRuntime, FilesystemPreferenceSource, PreferenceClock, PreferenceSource,
    SystemPreferenceClock,
};
use crate::launcher::{
    Launcher, LauncherClock, LauncherReloadStep, LauncherRuntime, SystemLauncherClock,
};
use crate::session_restore::{SessionAction, SessionRuntime, SESSION_SNAPSHOT_ID};
use crate::taskbar::Taskbar;
use crate::wallpaper::{
    FilesystemWallpaperSource, WallpaperRefreshStep, WallpaperRuntime, WallpaperSource,
};
use display_proto::events::{
    shell_window_state_flags, CallbackDone, PointerButton, PointerMotion, ShellRestoreFinished,
    ShellWindowCreated, ShellWindowDestroyed, ShellWindowFocused, ShellWindowSnapshotDone,
    ShellWindowState, ShellWindowTitleChanged,
};
use display_proto::Interface;
use std::time::{Duration, Instant};
use toolkit::draw::font::GLYPH_HEIGHT;
use toolkit::draw::{Canvas, Color, Rect, BYTES_PER_PIXEL};
use toolkit::theme::Theme;
use toolkit::{App, BufferPool, ClientError, Connection, WaitFd, Window};

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
    window.set_app_id("pmos.shell")?;
    window.commit()?;

    let theme = Theme::default();
    let wallpaper: Color = theme.window_background;
    let mut painted = false;
    let mut pool: Option<BufferPool> = None;

    for _ in 0..max_dispatch_iterations {
        // dispatch() internally acks any configure + records
        // close; passthrough events are ignored in this slice
        // (input/frame handling lands in a later slice).
        let _ = window.dispatch()?;

        if window.close_requested() {
            return Ok(ShellExit::CloseRequested);
        }

        if let Some(buffers) = pool.as_mut().filter(|buffers| buffers.commit_pending()) {
            if buffers.progress_commit(&mut window)? == toolkit::CommitProgress::Committed {
                painted = true;
                println!("shell: wallpaper painted");
            }
        }

        if !painted && window.is_configured() {
            let (cfg_w, cfg_h) = window.configured_size();
            let (w, h) = if cfg_w == 0 || cfg_h == 0 {
                (DEFAULT_WIDTH, DEFAULT_HEIGHT)
            } else {
                (cfg_w, cfg_h)
            };
            taskbar.set_framebuffer_size(w, h);

            if pool.is_none() {
                pool = Some(BufferPool::new(window.app_mut(), w, h)?);
            }
            let buffers = pool.as_mut().expect("pool created above");
            if let Some(mut canvas) = buffers.acquire_back_canvas() {
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
                if buffers.commit_in_place(&mut window)? == toolkit::CommitProgress::Committed {
                    painted = true;
                    println!("shell: wallpaper painted");
                }
            }
            // If acquire returned None (back buffer in-use),
            // leave `painted=false` and retry on the next
            // iteration — should not happen on the first
            // paint since both buffers start free.
        }
        window.flush_outbound()?;
        if pool.as_ref().is_some_and(BufferPool::commit_pending) && !window.outbound_pending() {
            continue;
        }
        window.wait(None)?;
    }

    Ok(ShellExit::IterationLimit)
}

/// One slot in the launcher's app catalog. Each slot is a
/// pair of (label, exec-path). Clicking a launcher item
/// invokes the caller-supplied [`SpawnFn`] with the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherSlot<'a> {
    pub label: &'a str,
    pub exec: &'a str,
}

/// Default catalog the production shell ships with.
/// Each entry is a `.wasm` binary the kernel will spawn when
/// the corresponding launcher row is clicked. The list is
/// hard-coded for v1; a future slice swaps it for a
/// `.desktop` file scan via [`crate::launcher::Launcher`].
pub const DEFAULT_LAUNCHER_SLOTS: &[LauncherSlot<'static>] = &[
    LauncherSlot {
        label: "Hello Window",
        exec: "/bin/hello-toplevel",
    },
    LauncherSlot {
        label: "Term",
        exec: "/bin/term",
    },
    LauncherSlot {
        label: "Files",
        exec: "/bin/files",
    },
    LauncherSlot {
        label: "Edit",
        exec: "/bin/edit",
    },
    LauncherSlot {
        label: "Settings",
        exec: "/bin/settings",
    },
    LauncherSlot {
        label: "Sysmon",
        exec: "/bin/sysmon",
    },
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

const LAUNCHER_FEEDBACK_HEIGHT: u32 = 24;
const LAUNCHER_FEEDBACK_WIDTH: u32 = 360;
const LAUNCHER_FEEDBACK_BG: Color = Color::rgb(0xa8, 0x2d, 0x2d);
const LAUNCHER_FEEDBACK_FG: Color = Color::rgb(0xff, 0xff, 0xff);

/// Persistent user-visible result of a failed launcher request. It remains on
/// screen until a later launch succeeds, so a missing binary is not reduced to
/// a developer-console-only error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LauncherFeedback {
    message: String,
}

impl LauncherFeedback {
    pub fn message(&self) -> &str {
        &self.message
    }

    fn failed(slot: &LauncherSlot<'_>, errno: i32) -> Self {
        Self {
            message: format!("Could not launch {} (errno {errno})", slot.label),
        }
    }
}

struct PendingLaunch {
    label: String,
    exec: String,
}

#[derive(Default)]
struct LauncherUiState {
    open: bool,
    hover: Option<usize>,
    feedback: Option<LauncherFeedback>,
    pending_launch: Option<PendingLaunch>,
}

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

/// Drain every zombie child the shell currently has. Called
/// periodically from the desktop event loop so spawned apps
/// that exit don't accumulate zombie proc-table entries
/// forever. Production main supplies a closure that calls
/// `pmos_ext.proc_wait(-1, WNOHANG)` in a loop; tests pass a
/// no-op.
pub trait Reaper {
    fn reap(&mut self);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DesktopWake {
    pub preferences: bool,
    pub launcher: bool,
    pub sigchld: bool,
    pub fatal_errno: Option<i32>,
}

/// Auxiliary readiness owned by the shell rather than the display protocol.
/// Production supplies signal and filesystem-watch descriptors; native
/// fixtures use the immediate no-op default and retain deterministic clocks.
pub trait DesktopEventSource {
    fn wait_fds(&self) -> Vec<WaitFd> {
        Vec::new()
    }

    fn drain(&mut self) -> DesktopWake {
        DesktopWake::default()
    }

    fn event_driven(&self) -> bool {
        false
    }

    /// Observe the first successfully published launcher catalog. Production
    /// sources emit a boot diagnostic; injected fixtures normally keep the
    /// default no-op implementation.
    fn catalog_published(&mut self, _entry_count: usize) {}

    /// Emit an optional diagnostic after the loop queues the authenticated
    /// desktop-ready protocol request. This callback is not a readiness
    /// transport: the browser gates input on the display server's typed
    /// presentation fence.
    fn desktop_ready(&mut self) {}
}

#[derive(Default)]
struct NoopDesktopEventSource;

impl DesktopEventSource for NoopDesktopEventSource {}

impl<F> Reaper for F
where
    F: FnMut(),
{
    fn reap(&mut self) {
        (self)()
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
pub fn run_desktop_shell<C, S, R>(
    connection: C,
    max_dispatch_iterations: u32,
    taskbar: Taskbar,
    slots: &[LauncherSlot<'_>],
    spawner: S,
    reaper: R,
) -> Result<ShellExit, ClientError>
where
    C: Connection,
    S: Spawner,
    R: Reaper,
{
    let preferences = DesktopPreferenceRuntime::new(
        FilesystemPreferenceSource::new(preferences::DEFAULT_PATH),
        SystemPreferenceClock::new(),
    );
    run_desktop_shell_with_preferences(
        connection,
        max_dispatch_iterations,
        taskbar,
        slots,
        spawner,
        reaper,
        preferences,
    )
}

/// Production desktop loop backed by the live VFS launcher catalog.
pub fn run_desktop_shell_live<C, S, R>(
    connection: C,
    max_dispatch_iterations: u32,
    taskbar: Taskbar,
    launcher: Launcher,
    spawner: S,
    reaper: R,
) -> Result<ShellExit, ClientError>
where
    C: Connection,
    S: Spawner,
    R: Reaper,
{
    let preferences = DesktopPreferenceRuntime::new(
        FilesystemPreferenceSource::new(preferences::DEFAULT_PATH),
        SystemPreferenceClock::new(),
    );
    run_desktop_shell_with_runtimes(
        connection,
        max_dispatch_iterations,
        taskbar,
        LauncherRuntime::new(launcher, SystemLauncherClock::new()),
        spawner,
        reaper,
        preferences,
    )
}

/// Production desktop loop backed by readiness notifications instead of VFS
/// and child-reap polling intervals.
pub fn run_desktop_shell_live_with_events<C, S, R, E>(
    connection: C,
    max_dispatch_iterations: u32,
    taskbar: Taskbar,
    launcher: Launcher,
    spawner: S,
    reaper: R,
    events: E,
) -> Result<ShellExit, ClientError>
where
    C: Connection,
    S: Spawner,
    R: Reaper,
    E: DesktopEventSource,
{
    let preferences = DesktopPreferenceRuntime::new(
        FilesystemPreferenceSource::new(preferences::DEFAULT_PATH),
        SystemPreferenceClock::new(),
    );
    run_desktop_shell_with_runtimes_and_events(
        connection,
        max_dispatch_iterations,
        taskbar,
        LauncherRuntime::new(launcher, SystemLauncherClock::new()),
        spawner,
        reaper,
        preferences,
        events,
    )
}

/// Testable desktop loop variant with injected VFS and clock seams. Production
/// uses [`run_desktop_shell`], which reads the canonical preference path.
pub fn run_desktop_shell_with_preferences<C, S, R, P, T>(
    connection: C,
    max_dispatch_iterations: u32,
    taskbar: Taskbar,
    slots: &[LauncherSlot<'_>],
    spawner: S,
    reaper: R,
    preferences: DesktopPreferenceRuntime<P, T>,
) -> Result<ShellExit, ClientError>
where
    C: Connection,
    S: Spawner,
    R: Reaper,
    P: PreferenceSource,
    T: PreferenceClock,
{
    run_desktop_shell_inner(
        connection,
        max_dispatch_iterations,
        taskbar,
        FixedLauncherCatalog { slots },
        spawner,
        reaper,
        preferences,
        NoopDesktopEventSource,
        None,
        FilesystemWallpaperSource::default(),
    )
}

/// Fully injectable desktop loop used by launcher/preference isolation tests.
pub fn run_desktop_shell_with_runtimes<C, S, R, P, T, L>(
    connection: C,
    max_dispatch_iterations: u32,
    taskbar: Taskbar,
    launcher: LauncherRuntime<L>,
    spawner: S,
    reaper: R,
    preferences: DesktopPreferenceRuntime<P, T>,
) -> Result<ShellExit, ClientError>
where
    C: Connection,
    S: Spawner,
    R: Reaper,
    P: PreferenceSource,
    T: PreferenceClock,
    L: LauncherClock,
{
    run_desktop_shell_inner(
        connection,
        max_dispatch_iterations,
        taskbar,
        launcher,
        spawner,
        reaper,
        preferences,
        NoopDesktopEventSource,
        None,
        FilesystemWallpaperSource::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn run_desktop_shell_with_runtimes_and_events<C, S, R, P, T, L, E>(
    connection: C,
    max_dispatch_iterations: u32,
    taskbar: Taskbar,
    launcher: LauncherRuntime<L>,
    spawner: S,
    reaper: R,
    preferences: DesktopPreferenceRuntime<P, T>,
    events: E,
) -> Result<ShellExit, ClientError>
where
    C: Connection,
    S: Spawner,
    R: Reaper,
    P: PreferenceSource,
    T: PreferenceClock,
    L: LauncherClock,
    E: DesktopEventSource,
{
    run_desktop_shell_with_runtimes_events_and_wallpaper_source(
        connection,
        max_dispatch_iterations,
        taskbar,
        launcher,
        spawner,
        reaper,
        preferences,
        events,
        FilesystemWallpaperSource::default(),
    )
}

/// Fully injectable production-shaped desktop loop used by paint-scheduling
/// isolation tests. Production wrappers keep using the canonical filesystem
/// wallpaper source.
#[allow(clippy::too_many_arguments)]
pub fn run_desktop_shell_with_runtimes_events_and_wallpaper_source<C, S, R, P, T, L, E, W>(
    connection: C,
    max_dispatch_iterations: u32,
    taskbar: Taskbar,
    launcher: LauncherRuntime<L>,
    spawner: S,
    reaper: R,
    preferences: DesktopPreferenceRuntime<P, T>,
    events: E,
    wallpaper_source: W,
) -> Result<ShellExit, ClientError>
where
    C: Connection,
    S: Spawner,
    R: Reaper,
    P: PreferenceSource,
    T: PreferenceClock,
    L: LauncherClock,
    E: DesktopEventSource,
    W: WallpaperSource,
{
    run_desktop_shell_inner(
        connection,
        max_dispatch_iterations,
        taskbar,
        launcher,
        spawner,
        reaper,
        preferences,
        events,
        None,
        wallpaper_source,
    )
}

/// Production event-driven desktop loop with the shell-owned durable session
/// runtime enabled. The older entry points intentionally remain session-neutral
/// so protocol fixtures do not gain filesystem or restore side effects.
#[allow(clippy::too_many_arguments)]
pub fn run_desktop_shell_live_with_events_and_session<C, S, R, E>(
    connection: C,
    max_dispatch_iterations: u32,
    taskbar: Taskbar,
    launcher: Launcher,
    spawner: S,
    reaper: R,
    own_pid: u32,
    events: E,
) -> Result<ShellExit, ClientError>
where
    C: Connection,
    S: Spawner,
    R: Reaper,
    E: DesktopEventSource,
{
    let preferences = DesktopPreferenceRuntime::new(
        FilesystemPreferenceSource::new(preferences::DEFAULT_PATH),
        SystemPreferenceClock::new(),
    );
    run_desktop_shell_inner(
        connection,
        max_dispatch_iterations,
        taskbar,
        LauncherRuntime::new(launcher, SystemLauncherClock::new()),
        spawner,
        reaper,
        preferences,
        events,
        Some(SessionRuntime::production(own_pid)),
        FilesystemWallpaperSource::default(),
    )
}

/// Fully injectable production-shaped session loop used by shell isolation
/// tests. Callers own the filesystem seam inside `session`.
#[allow(clippy::too_many_arguments)]
pub fn run_desktop_shell_with_runtimes_events_and_session<C, S, R, P, T, L, E>(
    connection: C,
    max_dispatch_iterations: u32,
    taskbar: Taskbar,
    launcher: LauncherRuntime<L>,
    spawner: S,
    reaper: R,
    preferences: DesktopPreferenceRuntime<P, T>,
    events: E,
    session: SessionRuntime,
) -> Result<ShellExit, ClientError>
where
    C: Connection,
    S: Spawner,
    R: Reaper,
    P: PreferenceSource,
    T: PreferenceClock,
    L: LauncherClock,
    E: DesktopEventSource,
{
    run_desktop_shell_inner(
        connection,
        max_dispatch_iterations,
        taskbar,
        launcher,
        spawner,
        reaper,
        preferences,
        events,
        Some(session),
        FilesystemWallpaperSource::default(),
    )
}

trait LauncherCatalog {
    fn poll(&mut self) -> bool;
    fn request_refresh(&mut self);
    fn step_refresh(&mut self) -> LauncherReloadStep;
    fn refresh_pending(&self) -> bool;
    fn entry_count(&self) -> usize;
    fn slots(&self) -> Vec<LauncherSlot<'_>>;
    fn session_entries(&self) -> Option<&[crate::launcher::DesktopEntry]>;
}

struct FixedLauncherCatalog<'a> {
    slots: &'a [LauncherSlot<'a>],
}

impl LauncherCatalog for FixedLauncherCatalog<'_> {
    fn poll(&mut self) -> bool {
        false
    }

    fn request_refresh(&mut self) {}

    fn step_refresh(&mut self) -> LauncherReloadStep {
        LauncherReloadStep::Idle
    }

    fn refresh_pending(&self) -> bool {
        false
    }

    fn entry_count(&self) -> usize {
        self.slots.len()
    }

    fn slots(&self) -> Vec<LauncherSlot<'_>> {
        self.slots.to_vec()
    }

    fn session_entries(&self) -> Option<&[crate::launcher::DesktopEntry]> {
        None
    }
}

impl<L: LauncherClock> LauncherCatalog for LauncherRuntime<L> {
    fn poll(&mut self) -> bool {
        LauncherRuntime::poll(self)
    }

    fn request_refresh(&mut self) {
        LauncherRuntime::request_reload(self)
    }

    fn step_refresh(&mut self) -> LauncherReloadStep {
        LauncherRuntime::step_reload(self)
    }

    fn refresh_pending(&self) -> bool {
        LauncherRuntime::reload_pending(self)
    }

    fn entry_count(&self) -> usize {
        self.entries().len()
    }

    fn slots(&self) -> Vec<LauncherSlot<'_>> {
        current_launcher_slots(self)
    }

    fn session_entries(&self) -> Option<&[crate::launcher::DesktopEntry]> {
        Some(self.entries())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct ShellChromeState {
    taskbar_layout_generation: u64,
    focused_window: Option<u32>,
    launcher_generation: u64,
    overlay: Option<Rect>,
}

#[derive(Copy, Clone, Debug)]
struct PendingShellFrame {
    buffer_index: usize,
    chrome_state: ShellChromeState,
    full_generation: Option<u64>,
    paint_serial: u64,
}

#[allow(clippy::too_many_arguments)]
fn run_desktop_shell_inner<C, S, R, P, T, K, E, W>(
    connection: C,
    max_dispatch_iterations: u32,
    mut taskbar: Taskbar,
    mut launcher: K,
    mut spawner: S,
    mut reaper: R,
    mut preferences: DesktopPreferenceRuntime<P, T>,
    mut event_source: E,
    mut session: Option<SessionRuntime>,
    wallpaper_source: W,
) -> Result<ShellExit, ClientError>
where
    C: Connection,
    S: Spawner,
    R: Reaper,
    P: PreferenceSource,
    T: PreferenceClock,
    K: LauncherCatalog,
    E: DesktopEventSource,
    W: WallpaperSource,
{
    let mut app = App::connect_with_shell(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("PMos")?;
    window.set_app_id("pmos.shell")?;
    window.commit()?;

    if window.app_mut().shell_manager().is_some() {
        let _ = window
            .app_mut()
            .shell_manager_set_work_area_bottom(crate::taskbar::TASKBAR_HEIGHT);
        if session.is_some() {
            shell_manager_subscribe_window_state(window.app_mut(), SESSION_SNAPSHOT_ID)?;
        } else {
            let _ = window.app_mut().shell_manager_subscribe_windows();
        }
    }

    // Deterministic legacy fixtures keep iteration-gated reaping. Production
    // uses DesktopEventSource's signal-fd readiness and never wakes merely to
    // poll proc_wait.
    const REAP_EVERY: u32 = 5_000;
    let mut iter_since_reap: u32 = 0;

    let event_driven = event_source.event_driven();
    let session_started = session.as_ref().map(|_| Instant::now());
    let mut theme = preferences.preferences().theme.palette();
    let initial_preferences = preferences.preferences();
    let mut wallpaper = if event_driven {
        WallpaperRuntime::new_stepwise(wallpaper_source, initial_preferences.wallpaper)
    } else {
        WallpaperRuntime::new(wallpaper_source, initial_preferences.wallpaper)
    };
    let mut initial_wallpaper_ready = !event_driven;
    taskbar.set_theme(theme);
    taskbar.set_clock_text(preferences.clock_text());
    let mut last_size: (u32, u32) = (0, 0);
    let mut pool: Option<BufferPool> = None;
    let mut wallpaper_pixels = Vec::new();
    let mut buffer_chrome_states = [None; 2];
    let mut presented_chrome_state = None;
    let mut full_generation = 1u64;
    let mut taskbar_layout_generation = 1u64;
    let mut launcher_generation = 1u64;
    let mut paint_serial = 0u64;
    let mut buffer_full_generations = [0u64; 2];
    let mut pending_frame: Option<PendingShellFrame> = None;
    let mut launcher_ui = LauncherUiState::default();
    let mut initial_catalog_ready_reported = false;
    let mut initial_catalog_published = false;
    let mut desktop_ready_reported = false;

    let mut needs_paint = false;
    let mut force_first_paint_done = false;

    for _ in 0..max_dispatch_iterations {
        // Display close/input is always serviced before filesystem work.
        let events = match window.dispatch() {
            Ok(events) => events,
            Err(error) => {
                println!("shell: dispatch error: {:?}", error);
                return Err(error);
            }
        };
        if window.close_requested() {
            return Ok(ShellExit::CloseRequested);
        }

        let mut session_clock_sampled = false;
        let mut session_now = session
            .as_ref()
            .map(SessionRuntime::now)
            .unwrap_or(Duration::ZERO);
        if session
            .as_ref()
            .is_some_and(SessionRuntime::needs_clock_sample)
        {
            refresh_session_time(
                session_started.as_ref(),
                session.as_mut(),
                &mut session_now,
                &mut session_clock_sampled,
            );
        }

        let wake = event_source.drain();
        if let Some(errno) = wake.fatal_errno {
            return Err(ClientError::Transport(errno));
        }
        if event_driven && wake.sigchld {
            reaper.reap();
        }
        if session
            .as_ref()
            .is_none_or(SessionRuntime::allows_user_launch)
            && process_pending_launch(&mut launcher_ui, &mut spawner)
        {
            advance_generation(&mut launcher_generation);
            request_paint(&mut needs_paint, &mut paint_serial);
        }

        let launcher_changed = if event_driven {
            if wake.launcher {
                launcher.request_refresh();
            }
            let step = launcher.step_refresh();
            if matches!(step, LauncherReloadStep::Complete { .. }) {
                if let Some(session) = session.as_mut() {
                    if let Some(entries) = launcher.session_entries() {
                        session.set_catalog(entries);
                    } else {
                        session.mark_empty_catalog_ready();
                    }
                    refresh_session_time(
                        session_started.as_ref(),
                        Some(session),
                        &mut session_now,
                        &mut session_clock_sampled,
                    );
                }
            }
            if !initial_catalog_ready_reported
                && matches!(step, LauncherReloadStep::Complete { .. })
            {
                let entry_count = launcher.entry_count();
                event_source.catalog_published(entry_count);
                initial_catalog_published = true;
                initial_catalog_ready_reported = true;
            }
            matches!(step, LauncherReloadStep::Complete { changed: true })
        } else {
            launcher.poll()
        };
        if launcher_changed {
            launcher_ui.hover = None;
            if launcher_ui.open {
                advance_generation(&mut launcher_generation);
                request_paint(&mut needs_paint, &mut paint_serial);
            }
        }
        let slots = launcher.slots();

        let preference_update = if event_driven {
            if wake.preferences {
                preferences.refresh_preferences()
            } else {
                preferences.refresh_clock()
            }
        } else {
            preferences.poll()
        };
        let current_preferences = preferences.preferences();
        let wallpaper_changed = if event_driven {
            if preference_update.preferences_checked {
                wallpaper.request_refresh(current_preferences.wallpaper);
            }
            let step = wallpaper.step_refresh();
            if !initial_wallpaper_ready && matches!(step, WallpaperRefreshStep::Complete { .. }) {
                initial_wallpaper_ready = true;
            }
            matches!(step, WallpaperRefreshStep::Complete { changed: true })
        } else {
            preference_update.preferences_checked
                && wallpaper.refresh(current_preferences.wallpaper)
        };
        if wallpaper_changed {
            request_paint(&mut needs_paint, &mut paint_serial);
            advance_generation(&mut full_generation);
        }
        let source_pending_work =
            event_driven && (launcher.refresh_pending() || wallpaper.refresh_pending());
        if preference_update.preferences_changed {
            let current = current_preferences;
            theme = current.theme.palette();
            taskbar.set_theme(theme);
            advance_generation(&mut taskbar_layout_generation);
            advance_generation(&mut launcher_generation);
            advance_generation(&mut full_generation);
        }
        if preference_update.clock_changed {
            taskbar.set_clock_text(preferences.clock_text());
            advance_generation(&mut taskbar_layout_generation);
        }
        if preference_update.needs_repaint() {
            request_paint(&mut needs_paint, &mut paint_serial);
        }

        if !event_driven {
            iter_since_reap = iter_since_reap.wrapping_add(1);
            if iter_since_reap >= REAP_EVERY {
                reaper.reap();
                iter_since_reap = 0;
            }
        }
        for event in events {
            match (event.interface, event.opcode) {
                (Interface::ShellManager, 1 /* window_created */) => {
                    if let Ok(decoded) = ShellWindowCreated::decode(&event.payload) {
                        taskbar.add_window(decoded.window_id, decoded.title, decoded.app_id);
                        advance_generation(&mut taskbar_layout_generation);
                        request_paint(&mut needs_paint, &mut paint_serial);
                    }
                }
                (Interface::ShellManager, 2 /* window_destroyed */) => {
                    if let Ok(decoded) = ShellWindowDestroyed::decode(&event.payload) {
                        if let Some(session) = session.as_mut() {
                            session.observe_window_destroyed(decoded.window_id);
                            refresh_session_time(
                                session_started.as_ref(),
                                Some(session),
                                &mut session_now,
                                &mut session_clock_sampled,
                            );
                        }
                        taskbar.remove_window(decoded.window_id);
                        advance_generation(&mut taskbar_layout_generation);
                        request_paint(&mut needs_paint, &mut paint_serial);
                    }
                }
                (Interface::ShellManager, 3 /* window_focused */) => {
                    if let Ok(decoded) = ShellWindowFocused::decode(&event.payload) {
                        if let Some(session) = session.as_mut() {
                            session.observe_window_focused(decoded.window_id);
                            refresh_session_time(
                                session_started.as_ref(),
                                Some(session),
                                &mut session_now,
                                &mut session_clock_sampled,
                            );
                        }
                        // Only repaint if the focus actually
                        // moved between taskbar entries —
                        // every pointer click on the wallpaper
                        // also broadcasts a window_focused
                        // event for the shell's own toplevel,
                        // which would otherwise re-paint on
                        // every cursor click and saturate the
                        // 64 KiB rx_buf with chunked
                        // shm_pool.write traffic.
                        let focus_change = focus_taskbar_window(&mut taskbar, decoded.window_id);
                        if focus_change.visual_changed {
                            if focus_change.layout_changed {
                                advance_generation(&mut taskbar_layout_generation);
                            }
                            request_paint(&mut needs_paint, &mut paint_serial);
                        }
                    }
                }
                (Interface::ShellManager, 4 /* window_title_changed */) => {
                    if let Ok(decoded) = ShellWindowTitleChanged::decode(&event.payload) {
                        taskbar.set_window_title(decoded.window_id, decoded.new_title);
                        advance_generation(&mut taskbar_layout_generation);
                        request_paint(&mut needs_paint, &mut paint_serial);
                    }
                }
                (Interface::ShellManager, 5 | 6 /* full v2 state */) => {
                    if let Ok(decoded) = ShellWindowState::decode(&event.payload) {
                        taskbar.add_window(
                            decoded.window_id,
                            decoded.title.clone(),
                            decoded.app_id.clone(),
                        );
                        taskbar.set_window_minimized(
                            decoded.window_id,
                            decoded.flags & shell_window_state_flags::MINIMIZED != 0,
                        );
                        if decoded.flags & shell_window_state_flags::FOCUSED != 0 {
                            taskbar.set_focused_window(decoded.window_id);
                        }
                        if let Some(session) = session.as_mut() {
                            session.observe_window_state(decoded);
                            refresh_session_time(
                                session_started.as_ref(),
                                Some(session),
                                &mut session_now,
                                &mut session_clock_sampled,
                            );
                        }
                        advance_generation(&mut taskbar_layout_generation);
                        request_paint(&mut needs_paint, &mut paint_serial);
                    }
                }
                (Interface::ShellManager, 7 /* window_snapshot_done */) => {
                    if let Ok(decoded) = ShellWindowSnapshotDone::decode(&event.payload) {
                        if let Some(session) = session.as_mut() {
                            session.observe_snapshot_done(decoded.snapshot_id);
                            refresh_session_time(
                                session_started.as_ref(),
                                Some(session),
                                &mut session_now,
                                &mut session_clock_sampled,
                            );
                        }
                    }
                }
                (Interface::ShellManager, 8 /* restore_finished */) => {
                    if let Ok(decoded) = ShellRestoreFinished::decode(&event.payload) {
                        if let Some(session) = session.as_mut() {
                            session.observe_restore_finished(
                                decoded.restore_id,
                                decoded.status,
                                decoded.placed,
                            );
                            refresh_session_time(
                                session_started.as_ref(),
                                Some(session),
                                &mut session_now,
                                &mut session_clock_sampled,
                            );
                        }
                    }
                }
                (Interface::Callback, 1 /* done */) => {
                    if CallbackDone::decode(&event.payload).is_ok()
                        && session
                            .as_mut()
                            .is_some_and(|session| session.observe_callback(event.object_id.raw()))
                    {
                        window.app_mut().client_mut().drop_object(event.object_id);
                    }
                }
                (Interface::Buffer, 1 /* release */) => {
                    if let Some(p) = pool.as_mut() {
                        let _ = p.handle_release(event.object_id);
                    }
                }
                (Interface::Pointer, 1 /* motion */) => {
                    if let Ok(motion) = PointerMotion::decode(&event.payload) {
                        let new_hover = if launcher_ui.open {
                            launcher_menu_row_at(&taskbar, &slots, motion.x, motion.y)
                        } else {
                            None
                        };
                        if new_hover != launcher_ui.hover {
                            launcher_ui.hover = new_hover;
                            if launcher_ui.open {
                                advance_generation(&mut launcher_generation);
                                request_paint(&mut needs_paint, &mut paint_serial);
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
                            let outcome = handle_press(
                                button.x,
                                button.y,
                                &mut taskbar,
                                &slots,
                                &mut launcher_ui,
                                window.app_mut(),
                            );
                            if outcome.taskbar_layout_changed {
                                advance_generation(&mut taskbar_layout_generation);
                            }
                            if outcome.launcher_changed {
                                advance_generation(&mut launcher_generation);
                            }
                            if outcome.visual_changed {
                                request_paint(&mut needs_paint, &mut paint_serial);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        let mut session_local_work = false;
        if let Some(session) = session.as_mut() {
            session_local_work = session.step_io();
            refresh_session_time(
                session_started.as_ref(),
                Some(session),
                &mut session_now,
                &mut session_clock_sampled,
            );
            if let Some(action) = session.next_action(session_now) {
                session_local_work = true;
                match action {
                    SessionAction::BeginRestore {
                        restore_id,
                        timeout_ms,
                    } => {
                        shell_manager_begin_restore(window.app_mut(), restore_id, timeout_ms)?;
                        let callback = window.app_mut().client_mut().sync()?;
                        session.barrier_sent(callback.raw());
                    }
                    SessionAction::Spawn { instance_id, exec } => {
                        let result = spawner.spawn(&exec);
                        session.spawn_result(instance_id, result);
                    }
                    SessionAction::Place {
                        restore_id,
                        window_id,
                        normal_x,
                        normal_y,
                        normal_width,
                        normal_height,
                        z_rank,
                        flags,
                    } => shell_manager_place_restored_window(
                        window.app_mut(),
                        restore_id,
                        window_id,
                        normal_x,
                        normal_y,
                        normal_width,
                        normal_height,
                        z_rank,
                        flags,
                    )?,
                    SessionAction::EndRestore {
                        restore_id,
                        focus_window_id,
                    } => shell_manager_end_restore(window.app_mut(), restore_id, focus_window_id)?,
                }
            }
        }

        if let Some(p) = pool.as_mut().filter(|p| p.commit_pending()) {
            if p.progress_commit(&mut window)? == toolkit::CommitProgress::Committed {
                if let Some(frame) = pending_frame.take() {
                    buffer_chrome_states[frame.buffer_index] = Some(frame.chrome_state);
                    presented_chrome_state = Some(frame.chrome_state);
                    if let Some(generation) = frame.full_generation {
                        buffer_full_generations[frame.buffer_index] = generation;
                    }
                    // Input may have changed while the upload was staged. The
                    // completed frame records what reached the surface, but it
                    // must never clear a newer paint request.
                    if paint_serial != frame.paint_serial {
                        needs_paint = true;
                    }
                }
                if buffer_full_generations
                    .iter()
                    .any(|generation| *generation != full_generation)
                {
                    needs_paint = true;
                }
            }
        }

        // Configure and all other boot work continue while the selected
        // wallpaper advances in bounded quanta. Seed the two framebuffer
        // slots only after that initial load reaches either a validated image
        // or its terminal safe fallback; later replacements retain the
        // current image and never gate repaint scheduling.
        if !force_first_paint_done && window.is_configured() && initial_wallpaper_ready {
            request_paint(&mut needs_paint, &mut paint_serial);
            force_first_paint_done = true;
        }

        let paint_budget = if window
            .app_mut()
            .client_mut()
            .connection()
            .incremental_uploads()
        {
            1
        } else {
            2
        };
        for _ in 0..paint_budget {
            if !paint_is_actionable(
                needs_paint,
                window.is_configured(),
                initial_wallpaper_ready,
                pool.as_ref().is_some_and(BufferPool::commit_pending),
            ) {
                break;
            }
            let (cfg_w, cfg_h) = window.configured_size();
            let (w, h) = if cfg_w == 0 || cfg_h == 0 {
                (DEFAULT_WIDTH, DEFAULT_HEIGHT)
            } else {
                (cfg_w, cfg_h)
            };
            if let Some(session) = session.as_mut() {
                let output_changed = last_size != (w, h);
                session.set_output_size(w, h);
                if output_changed {
                    refresh_session_time(
                        session_started.as_ref(),
                        Some(session),
                        &mut session_now,
                        &mut session_clock_sampled,
                    );
                }
            }
            taskbar.set_framebuffer_size(w, h);

            // Re-allocate the pool only when geometry
            // changes; otherwise reuse so the back/front
            // swap doesn't churn allocations.
            if last_size != (w, h) {
                BufferPool::replace(&mut pool, window.app_mut(), w, h)?;
                last_size = (w, h);
                wallpaper_pixels.clear();
                buffer_chrome_states = [None; 2];
                presented_chrome_state = None;
                buffer_full_generations = [0; 2];
                advance_generation(&mut full_generation);
            }
            let p = pool.as_mut().expect("pool initialised when w/h are set");
            let expected_pixels = (w as usize)
                .checked_mul(h as usize)
                .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
                .expect("configured shell surface exceeds address space");
            let painted_buffer = p.back_index();
            let full_paint = buffer_full_generations[painted_buffer] != full_generation
                || wallpaper_pixels.len() != expected_pixels;
            let current_chrome_state = shell_chrome_state(
                &taskbar,
                &slots,
                &launcher_ui,
                taskbar_layout_generation,
                launcher_generation,
            );
            let submitted_serial = paint_serial;
            if full_paint {
                let mut canvas = p
                    .acquire_back_canvas()
                    .expect("pending commits are excluded above");
                wallpaper.paint(&mut canvas, current_preferences.wallpaper_fit);
                wallpaper_pixels.clear();
                wallpaper_pixels.extend_from_slice(canvas.pixels());
                draw_shell_chrome(&mut canvas, &taskbar, &theme, &slots, &launcher_ui);
                drop(canvas);
                let progress = p.commit_and_swap(&mut window)?;
                let frame = PendingShellFrame {
                    buffer_index: painted_buffer,
                    chrome_state: current_chrome_state,
                    full_generation: Some(full_generation),
                    paint_serial: submitted_serial,
                };
                if progress == toolkit::CommitProgress::Pending {
                    pending_frame = Some(frame);
                } else {
                    buffer_chrome_states[painted_buffer] = Some(current_chrome_state);
                    presented_chrome_state = Some(current_chrome_state);
                    buffer_full_generations[painted_buffer] = full_generation;
                }
            } else {
                let mut canvas = p
                    .acquire_back_canvas()
                    .expect("pending commits are excluded above");
                let damages = shell_chrome_damage_regions(
                    &taskbar,
                    buffer_chrome_states[painted_buffer],
                    presented_chrome_state,
                    current_chrome_state,
                );
                for damage in &damages {
                    let restored = restore_cached_region(&mut canvas, &wallpaper_pixels, *damage);
                    debug_assert!(restored, "validated wallpaper cache must restore");
                }
                draw_shell_chrome(&mut canvas, &taskbar, &theme, &slots, &launcher_ui);
                drop(canvas);
                let progress = p.commit_and_swap_damage_regions(&mut window, &damages)?;
                let frame = PendingShellFrame {
                    buffer_index: painted_buffer,
                    chrome_state: current_chrome_state,
                    full_generation: None,
                    paint_serial: submitted_serial,
                };
                if progress == toolkit::CommitProgress::Pending {
                    pending_frame = Some(frame);
                } else {
                    // An empty/clipped damage list is a no-op and deliberately
                    // does not swap, so only advance shell slot/front state
                    // when a real surface transaction was submitted.
                    if !damages.is_empty() {
                        buffer_chrome_states[painted_buffer] = Some(current_chrome_state);
                        presented_chrome_state = Some(current_chrome_state);
                    }
                }
            }
            finish_submitted_paint(&mut needs_paint, paint_serial, submitted_serial);
            if pending_frame.is_none()
                && buffer_full_generations
                    .iter()
                    .any(|generation| *generation != full_generation)
            {
                needs_paint = true;
            }
        }

        window.flush_outbound()?;
        let commit_pending = pool.as_ref().is_some_and(BufferPool::commit_pending);
        let actionable_paint = paint_is_actionable(
            needs_paint,
            window.is_configured(),
            initial_wallpaper_ready,
            commit_pending,
        );
        let all_buffers_current = pool.is_some()
            && buffer_full_generations
                .iter()
                .all(|generation| *generation == full_generation);
        let session_ready = session
            .as_ref()
            .is_none_or(SessionRuntime::ready_for_desktop);
        let desktop_settled = initial_catalog_published
            && !launcher.refresh_pending()
            && !wallpaper.refresh_pending()
            && session_ready
            && all_buffers_current
            && pending_frame.is_none()
            && !commit_pending
            && !needs_paint
            && !window.outbound_pending();
        if event_driven && !desktop_ready_reported && desktop_settled {
            window.app_mut().shell_manager_desktop_ready()?;
            desktop_ready_reported = true;
            event_source.desktop_ready();
        }
        if source_pending_work
            || session_local_work
            || session.as_ref().is_some_and(SessionRuntime::has_local_work)
            || (commit_pending && !window.outbound_pending())
            || actionable_paint
        {
            continue;
        }
        let mut sources = event_source.wait_fds();
        if let Some(session) = session.as_ref() {
            sources.extend(session.wait_fds().into_iter().map(|wait| match wait {
                crate::session_store::SessionWait::Read(fd) => WaitFd::readable(fd),
                crate::session_store::SessionWait::Write(fd) => WaitFd::writable(fd),
            }));
        }
        let preference_deadline = event_driven.then(|| preferences.next_clock_deadline());
        let session_deadline = session
            .as_ref()
            .and_then(|session| session.next_deadline(session_now));
        let deadline = match (preference_deadline, session_deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(timeout), None) | (None, Some(timeout)) => Some(timeout),
            (None, None) => None,
        };
        window.wait_with(&sources, deadline)?;
    }

    Ok(ShellExit::IterationLimit)
}

fn current_launcher_slots<L: LauncherClock>(
    launcher: &LauncherRuntime<L>,
) -> Vec<LauncherSlot<'_>> {
    if launcher.entries().is_empty() {
        return DEFAULT_LAUNCHER_SLOTS.to_vec();
    }
    launcher
        .entries()
        .iter()
        .map(|entry| LauncherSlot {
            label: entry.name.as_str(),
            exec: entry.exec.as_str(),
        })
        .collect()
}

fn advance_generation(generation: &mut u64) {
    *generation = generation.wrapping_add(1);
    if *generation == 0 {
        *generation = 1;
    }
}

fn request_paint(needs_paint: &mut bool, paint_serial: &mut u64) {
    *needs_paint = true;
    advance_generation(paint_serial);
}

fn paint_is_actionable(
    needs_paint: bool,
    configured: bool,
    initial_wallpaper_ready: bool,
    commit_pending: bool,
) -> bool {
    needs_paint && configured && initial_wallpaper_ready && !commit_pending
}

fn finish_submitted_paint(needs_paint: &mut bool, paint_serial: u64, submitted_serial: u64) {
    if paint_serial == submitted_serial {
        *needs_paint = false;
    }
}

fn process_pending_launch<S: Spawner>(ui: &mut LauncherUiState, spawner: &mut S) -> bool {
    let Some(pending) = ui.pending_launch.take() else {
        return false;
    };
    let rc = spawner.spawn(&pending.exec);
    if rc < 0 {
        println!("shell: launch {} failed errno={}", pending.exec, -rc);
        ui.feedback = Some(LauncherFeedback::failed(
            &LauncherSlot {
                label: &pending.label,
                exec: &pending.exec,
            },
            -rc,
        ));
        true
    } else {
        println!("shell: launched {} pid={rc}", pending.exec);
        ui.feedback.take().is_some()
    }
}

fn refresh_session_time(
    started: Option<&Instant>,
    session: Option<&mut SessionRuntime>,
    now: &mut Duration,
    sampled: &mut bool,
) {
    let (Some(started), Some(session)) = (started, session) else {
        return;
    };
    if !session.needs_clock_sample() {
        return;
    }
    if *sampled {
        session.set_now(*now);
        return;
    }
    *now = started.elapsed();
    session.set_now(*now);
    *sampled = true;
}

fn shell_manager_send<C: Connection>(
    app: &mut App<C>,
    opcode: u16,
    payload: &[u8],
) -> Result<(), ClientError> {
    let manager = app
        .shell_manager()
        .ok_or(ClientError::MissingGlobal("pmd_shell_manager"))?;
    app.client_mut().send_request(manager, opcode, payload)
}

fn shell_manager_subscribe_window_state<C: Connection>(
    app: &mut App<C>,
    snapshot_id: u32,
) -> Result<(), ClientError> {
    shell_manager_send(app, 9, &snapshot_id.to_le_bytes())
}

fn shell_manager_begin_restore<C: Connection>(
    app: &mut App<C>,
    restore_id: u32,
    timeout_ms: u32,
) -> Result<(), ClientError> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&restore_id.to_le_bytes());
    payload.extend_from_slice(&timeout_ms.to_le_bytes());
    shell_manager_send(app, 10, &payload)
}

#[allow(clippy::too_many_arguments)]
fn shell_manager_place_restored_window<C: Connection>(
    app: &mut App<C>,
    restore_id: u32,
    window_id: u32,
    normal_x: i32,
    normal_y: i32,
    normal_width: u32,
    normal_height: u32,
    z_rank: u32,
    flags: u32,
) -> Result<(), ClientError> {
    let mut payload = Vec::with_capacity(32);
    payload.extend_from_slice(&restore_id.to_le_bytes());
    payload.extend_from_slice(&window_id.to_le_bytes());
    payload.extend_from_slice(&normal_x.to_le_bytes());
    payload.extend_from_slice(&normal_y.to_le_bytes());
    payload.extend_from_slice(&normal_width.to_le_bytes());
    payload.extend_from_slice(&normal_height.to_le_bytes());
    payload.extend_from_slice(&z_rank.to_le_bytes());
    payload.extend_from_slice(&flags.to_le_bytes());
    shell_manager_send(app, 11, &payload)
}

fn shell_manager_end_restore<C: Connection>(
    app: &mut App<C>,
    restore_id: u32,
    focus_window_id: u32,
) -> Result<(), ClientError> {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&restore_id.to_le_bytes());
    payload.extend_from_slice(&focus_window_id.to_le_bytes());
    shell_manager_send(app, 12, &payload)
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
fn launcher_menu_bounds(taskbar: &Taskbar, slots: &[LauncherSlot<'_>]) -> Rect {
    let bar = taskbar.bounds();
    let height = LAUNCHER_MENU_PADDING * 2 + LAUNCHER_MENU_ROW_HEIGHT * slots.len() as u32;
    let x = bar.x + (crate::taskbar::TASKBAR_LEFT_MARGIN as i32);
    let y = bar.y - height as i32;
    Rect::new(x, y, LAUNCHER_MENU_WIDTH, height)
}

/// Bounds of menu row `idx` in framebuffer space.
fn launcher_menu_row_bounds(
    taskbar: &Taskbar,
    slots: &[LauncherSlot<'_>],
    idx: usize,
) -> Option<Rect> {
    if idx >= slots.len() {
        return None;
    }
    let menu = launcher_menu_bounds(taskbar, slots);
    let y = menu.y + LAUNCHER_MENU_PADDING as i32 + (idx as i32) * LAUNCHER_MENU_ROW_HEIGHT as i32;
    Some(Rect::new(
        menu.x,
        y,
        LAUNCHER_MENU_WIDTH,
        LAUNCHER_MENU_ROW_HEIGHT,
    ))
}

/// Hit-test a point against the menu rows; returns the
/// hovered row index if any.
fn launcher_menu_row_at(
    taskbar: &Taskbar,
    slots: &[LauncherSlot<'_>],
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
fn draw_launcher_button(canvas: &mut Canvas<'_>, taskbar: &Taskbar, theme: &Theme, open: bool) {
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

fn draw_shell_chrome(
    canvas: &mut Canvas<'_>,
    taskbar: &Taskbar,
    theme: &Theme,
    slots: &[LauncherSlot<'_>],
    launcher_ui: &LauncherUiState,
) {
    taskbar.draw(canvas);
    draw_launcher_button(canvas, taskbar, theme, launcher_ui.open);
    if launcher_ui.open {
        draw_launcher_menu(canvas, taskbar, theme, slots, launcher_ui.hover);
    }
    if let Some(feedback) = launcher_ui.feedback.as_ref() {
        draw_launcher_feedback(canvas, taskbar, feedback);
    }
}

/// Paint the launcher popup menu (only when open).
fn draw_launcher_menu(
    canvas: &mut Canvas<'_>,
    taskbar: &Taskbar,
    theme: &Theme,
    slots: &[LauncherSlot<'_>],
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
        let text_y = row.y + ((row.height as i32 - GLYPH_HEIGHT as i32) / 2).max(0);
        canvas.draw_text(text_x, text_y, slot.label, theme.label_text);
    }
}

fn launcher_feedback_bounds(taskbar: &Taskbar) -> Rect {
    let bar = taskbar.bounds();
    let width = LAUNCHER_FEEDBACK_WIDTH.min(bar.width.saturating_sub(8));
    Rect::new(
        bar.x + 4,
        (bar.y - LAUNCHER_FEEDBACK_HEIGHT as i32 - 4).max(0),
        width,
        LAUNCHER_FEEDBACK_HEIGHT,
    )
}

fn union_rect(left: Rect, right: Rect) -> Rect {
    let x = left.x.min(right.x);
    let y = left.y.min(right.y);
    let right_edge = left.right().max(right.right());
    let bottom = left.bottom().max(right.bottom());
    Rect::new(x, y, (right_edge - x) as u32, (bottom - y) as u32)
}

fn launcher_overlay_bounds(
    taskbar: &Taskbar,
    slots: &[LauncherSlot<'_>],
    launcher_ui: &LauncherUiState,
) -> Option<Rect> {
    let mut overlay = None;
    if launcher_ui.open {
        overlay = Some(launcher_menu_bounds(taskbar, slots));
    }
    if launcher_ui.feedback.is_some() {
        let feedback = launcher_feedback_bounds(taskbar);
        overlay = Some(match overlay {
            Some(existing) => union_rect(existing, feedback),
            None => feedback,
        });
    }
    overlay
}

fn shell_chrome_state(
    taskbar: &Taskbar,
    slots: &[LauncherSlot<'_>],
    launcher_ui: &LauncherUiState,
    taskbar_layout_generation: u64,
    launcher_generation: u64,
) -> ShellChromeState {
    ShellChromeState {
        taskbar_layout_generation,
        focused_window: taskbar
            .entries()
            .iter()
            .find(|entry| entry.focused)
            .map(|entry| entry.window_id),
        launcher_generation,
        overlay: launcher_overlay_bounds(taskbar, slots, launcher_ui),
    }
}

fn clip_above(rect: Rect, bottom_limit: i32) -> Option<Rect> {
    let bottom = rect.bottom().min(bottom_limit);
    if rect.y >= bottom || rect.is_empty() {
        None
    } else {
        Some(Rect::new(
            rect.x,
            rect.y,
            rect.width,
            (bottom - rect.y) as u32,
        ))
    }
}

fn shell_chrome_damage_regions(
    taskbar: &Taskbar,
    target_back: Option<ShellChromeState>,
    presented_front: Option<ShellChromeState>,
    desired: ShellChromeState,
) -> Vec<Rect> {
    let taskbar_layout_dirty = [target_back, presented_front].into_iter().any(|state| {
        state
            .map(|state| state.taskbar_layout_generation != desired.taskbar_layout_generation)
            .unwrap_or(true)
    });
    let launcher_dirty = [target_back, presented_front].into_iter().any(|state| {
        state
            .map(|state| state.launcher_generation != desired.launcher_generation)
            .unwrap_or(true)
    });

    // A structural taskbar mismatch always comes first. This keeps the
    // full-width 32px band intact and lets an overlay be clipped above it,
    // avoiding both overlap and a full-width popup-height bounding box.
    let mut damages = Vec::with_capacity(4);
    let taskbar_bounds = taskbar.bounds();
    let mut full_taskbar_dirty = taskbar_layout_dirty;
    if !full_taskbar_dirty {
        let desired_focus = desired.focused_window;
        for state in [target_back, presented_front].into_iter().flatten() {
            if state.focused_window == desired_focus {
                continue;
            }
            for window_id in [state.focused_window, desired_focus].into_iter().flatten() {
                let Some(rect) = taskbar
                    .entries()
                    .iter()
                    .position(|entry| entry.window_id == window_id)
                    .and_then(|index| taskbar.entry_rect(index))
                else {
                    full_taskbar_dirty = true;
                    break;
                };
                if !damages.contains(&rect) {
                    damages.push(rect);
                }
            }
            if full_taskbar_dirty {
                break;
            }
        }
    }
    if full_taskbar_dirty {
        damages.clear();
    }
    if full_taskbar_dirty && !taskbar_bounds.is_empty() {
        damages.push(taskbar_bounds);
    }
    if launcher_dirty {
        let mut launcher_damage = None;
        for overlay in [
            target_back.and_then(|state| state.overlay),
            presented_front.and_then(|state| state.overlay),
            desired.overlay,
        ]
        .into_iter()
        .flatten()
        {
            launcher_damage = Some(match launcher_damage {
                Some(existing) => union_rect(existing, overlay),
                None => overlay,
            });
        }
        if !full_taskbar_dirty {
            let button = launcher_button_bounds(taskbar);
            launcher_damage = Some(match launcher_damage {
                Some(existing) => union_rect(existing, button),
                None => button,
            });
        }
        let launcher_damage = if full_taskbar_dirty {
            launcher_damage.and_then(|damage| clip_above(damage, taskbar_bounds.y))
        } else {
            launcher_damage
        };
        if let Some(damage) = launcher_damage.filter(|damage| !damage.is_empty()) {
            damages.push(damage);
        }
    }
    debug_assert!(damages.len() <= 4);
    damages
}

fn restore_cached_region(canvas: &mut Canvas<'_>, cached: &[u8], region: Rect) -> bool {
    let width = canvas.width();
    let height = canvas.height();
    let Some(expected) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
    else {
        return false;
    };
    if cached.len() != expected {
        return false;
    }

    let width_i64 = i64::from(width);
    let height_i64 = i64::from(height);
    let x_start = i64::from(region.x).clamp(0, width_i64) as usize;
    let y_start = i64::from(region.y).clamp(0, height_i64) as usize;
    let x_end = (i64::from(region.x) + i64::from(region.width)).clamp(0, width_i64) as usize;
    let y_end = (i64::from(region.y) + i64::from(region.height)).clamp(0, height_i64) as usize;
    if x_start >= x_end || y_start >= y_end {
        return true;
    }

    let stride = width as usize * BYTES_PER_PIXEL;
    let row_start = x_start * BYTES_PER_PIXEL;
    let row_end = x_end * BYTES_PER_PIXEL;
    let destination = canvas.pixels_mut();
    for y in y_start..y_end {
        let start = y * stride + row_start;
        let end = y * stride + row_end;
        destination[start..end].copy_from_slice(&cached[start..end]);
    }
    true
}

fn draw_launcher_feedback(canvas: &mut Canvas<'_>, taskbar: &Taskbar, feedback: &LauncherFeedback) {
    let bounds = launcher_feedback_bounds(taskbar);
    canvas.fill_rect(bounds, LAUNCHER_FEEDBACK_BG);
    canvas.stroke_rect(bounds, LAUNCHER_FEEDBACK_FG);
    let text_y = bounds.y + ((bounds.height as i32 - GLYPH_HEIGHT as i32) / 2).max(0);
    canvas.draw_text(
        bounds.x + 6,
        text_y,
        feedback.message(),
        LAUNCHER_FEEDBACK_FG,
    );
}

/// Shared press routing — checks the launcher menu first
/// (so a click inside an open menu picks a row), then the
/// launcher button (toggle open/closed), then the taskbar
/// (focus / restore on entry click).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct PressOutcome {
    visual_changed: bool,
    taskbar_layout_changed: bool,
    taskbar_focus_changed: bool,
    launcher_changed: bool,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct TaskbarFocusChange {
    visual_changed: bool,
    layout_changed: bool,
}

fn focus_taskbar_window(taskbar: &mut Taskbar, window_id: u32) -> TaskbarFocusChange {
    let previous_focus = taskbar
        .entries()
        .iter()
        .find(|entry| entry.focused)
        .map(|entry| entry.window_id);
    if previous_focus == Some(window_id) {
        return TaskbarFocusChange::default();
    }
    let previous_page = taskbar.visible_range();
    let restored_from_minimized = taskbar
        .entries()
        .iter()
        .any(|entry| entry.window_id == window_id && entry.minimized);
    taskbar.set_focused_window(window_id);
    TaskbarFocusChange {
        visual_changed: true,
        layout_changed: restored_from_minimized || taskbar.visible_range() != previous_page,
    }
}

fn handle_press<C: Connection>(
    x: i32,
    y: i32,
    taskbar: &mut Taskbar,
    slots: &[LauncherSlot<'_>],
    launcher_ui: &mut LauncherUiState,
    app: &mut App<C>,
) -> PressOutcome {
    let mut outcome = PressOutcome::default();
    let launcher_button = launcher_button_bounds(taskbar);
    // Menu has highest priority while open.
    if launcher_ui.open {
        if let Some(idx) = launcher_menu_row_at(taskbar, slots, x, y) {
            launcher_ui.pending_launch = Some(PendingLaunch {
                label: slots[idx].label.to_string(),
                exec: slots[idx].exec.to_string(),
            });
            launcher_ui.open = false;
            launcher_ui.hover = None;
            return PressOutcome {
                visual_changed: true,
                launcher_changed: true,
                ..PressOutcome::default()
            };
        }
        // Click outside the menu closes it.
        launcher_ui.open = false;
        launcher_ui.hover = None;
        outcome.visual_changed = true;
        outcome.launcher_changed = true;
        if x >= launcher_button.x
            && x < launcher_button.right()
            && y >= launcher_button.y
            && y < launcher_button.bottom()
        {
            return outcome;
        }
        // Fall through to the taskbar / launcher button
        // tests below so a click that lands on a taskbar
        // entry while the menu is open still focuses that
        // window.
    }

    if x >= launcher_button.x
        && x < launcher_button.right()
        && y >= launcher_button.y
        && y < launcher_button.bottom()
    {
        launcher_ui.open = true;
        launcher_ui.hover = None;
        outcome.visual_changed = true;
        outcome.launcher_changed = true;
        if let Some(shell_window_id) = taskbar
            .entries()
            .iter()
            .find(|entry| entry.app_id == "pmos.shell")
            .map(|entry| entry.window_id)
        {
            if app.shell_manager_focus_window(shell_window_id).is_ok() {
                let focus_change = focus_taskbar_window(taskbar, shell_window_id);
                if focus_change.visual_changed {
                    outcome.taskbar_focus_changed = true;
                }
                outcome.taskbar_layout_changed = focus_change.layout_changed;
            }
        }
        return outcome;
    }

    if let Some(click) = taskbar.handle_pointer_down(x, y) {
        match click {
            crate::taskbar::TaskbarClick::Focus { window_id } => {
                if app.shell_manager_focus_window(window_id).is_ok() {
                    let focus_change = focus_taskbar_window(taskbar, window_id);
                    if focus_change.visual_changed {
                        // Reserved-strip focus is one compositor transaction:
                        // the server holds the target raise until this local
                        // highlight reaches the shell's next surface commit.
                        outcome.visual_changed = true;
                        outcome.taskbar_focus_changed = true;
                    }
                    println!("shell: taskbar focus requested window_id={window_id}");
                }
            }
            crate::taskbar::TaskbarClick::Restore { window_id } => {
                if app.shell_manager_unminimize_window(window_id).is_ok() {
                    taskbar.set_window_minimized(window_id, false);
                    taskbar.set_focused_window(window_id);
                    outcome.visual_changed = true;
                    outcome.taskbar_layout_changed = true;
                    outcome.taskbar_focus_changed = true;
                    println!("shell: taskbar restore requested window_id={window_id}");
                }
            }
            crate::taskbar::TaskbarClick::Minimize { window_id } => {
                if app.shell_manager_minimize_window(window_id).is_ok() {
                    taskbar.set_window_minimized(window_id, true);
                    outcome.visual_changed = true;
                    outcome.taskbar_layout_changed = true;
                    println!("shell: taskbar minimize requested window_id={window_id}");
                }
            }
            crate::taskbar::TaskbarClick::ToggleMaximize { window_id } => {
                if app.shell_manager_toggle_maximized_window(window_id).is_ok() {
                    outcome.visual_changed = true;
                    println!("shell: taskbar maximize toggled window_id={window_id}");
                }
            }
            crate::taskbar::TaskbarClick::Close { window_id } => {
                if app.shell_manager_close_window(window_id).is_ok() {
                    println!("shell: taskbar close requested window_id={window_id}");
                }
            }
            crate::taskbar::TaskbarClick::CycleOverflow => {
                taskbar.cycle_overflow();
                outcome.visual_changed = true;
                outcome.taskbar_layout_changed = true;
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use display_proto::events::RegistryGlobal;
    use toolkit::{MemoryConnection, MessageHeader, ObjectId, HEADER_SIZE};

    fn shell_app() -> App<MemoryConnection> {
        const REGISTRY_ID: ObjectId = ObjectId::new(3);
        let mut connection = MemoryConnection::new();
        let mut globals = Vec::new();
        for (name, interface) in [
            (1, "pmd_compositor"),
            (2, "pmd_shm"),
            (3, "pmd_xdg_shell"),
            (4, "pmd_shell_manager"),
        ] {
            let global = RegistryGlobal {
                name,
                interface: interface.to_string(),
                version: 1,
            };
            let mut payload = Vec::new();
            global.encode(&mut payload);
            let header = MessageHeader::try_new(REGISTRY_ID, 1, payload.len(), 0).unwrap();
            let start = globals.len();
            globals.resize(start + HEADER_SIZE + payload.len(), 0);
            header
                .encode(&mut globals[start..start + HEADER_SIZE])
                .unwrap();
            globals[start + HEADER_SIZE..].copy_from_slice(&payload);
        }
        connection.feed_inbound(&globals);
        App::connect_with_shell(connection).expect("shell manager fixture connects")
    }

    #[test]
    fn focus_press_updates_taskbar_before_server_broadcast_and_emits_request() {
        let mut app = shell_app();
        let _ = app.client_mut().connection_mut().drain_outbound();
        let shell_manager = app.shell_manager().expect("shell manager bound");
        let mut taskbar = Taskbar::new(1024, 768);
        taskbar.add_window(1, "PMos", "pmos.shell");
        taskbar.add_window(2, "Terminal", "pmos.term");
        taskbar.set_focused_window(1);
        let target = taskbar.entry_rect(1).expect("target taskbar entry");

        let outcome = handle_press(
            target.x + 8,
            target.y + 8,
            &mut taskbar,
            &[],
            &mut LauncherUiState::default(),
            &mut app,
        );
        assert_eq!(
            outcome,
            PressOutcome {
                visual_changed: true,
                taskbar_layout_changed: false,
                taskbar_focus_changed: true,
                launcher_changed: false,
            }
        );
        assert!(!taskbar.entries()[0].focused);
        assert!(taskbar.entries()[1].focused);

        let request = app.client_mut().connection_mut().drain_outbound();
        let header = MessageHeader::decode(&request).expect("focus request header");
        assert_eq!(header.object_id, shell_manager);
        assert_eq!(header.opcode, 2);
        assert_eq!(&request[HEADER_SIZE..], &2_u32.to_le_bytes());

        assert!(
            !handle_press(
                target.x + 8,
                target.y + 8,
                &mut taskbar,
                &[],
                &mut LauncherUiState::default(),
                &mut app,
            )
            .visual_changed
        );
        let repeat = app.client_mut().connection_mut().drain_outbound();
        assert_eq!(MessageHeader::decode(&repeat).unwrap().opcode, 2);
    }

    #[test]
    fn launcher_open_focuses_the_shell_window_before_painting_menu() {
        let mut app = shell_app();
        let _ = app.client_mut().connection_mut().drain_outbound();
        let shell_manager = app.shell_manager().expect("shell manager bound");
        let mut taskbar = Taskbar::new(1024, 768);
        taskbar.add_window(1, "PMos", "pmos.shell");
        taskbar.add_window(2, "Terminal", "pmos.term");
        taskbar.set_focused_window(2);
        let launcher = launcher_button_bounds(&taskbar);
        let mut launcher_ui = LauncherUiState::default();

        let outcome = handle_press(
            launcher.x + 4,
            launcher.y + 4,
            &mut taskbar,
            &[],
            &mut launcher_ui,
            &mut app,
        );
        assert!(outcome.visual_changed);
        assert!(!outcome.taskbar_layout_changed);
        assert!(outcome.taskbar_focus_changed);
        assert!(outcome.launcher_changed);
        assert!(launcher_ui.open);
        assert!(taskbar.entries()[0].focused);
        assert!(!taskbar.entries()[1].focused);

        let request = app.client_mut().connection_mut().drain_outbound();
        let header = MessageHeader::decode(&request).expect("shell focus request header");
        assert_eq!(header.object_id, shell_manager);
        assert_eq!(header.opcode, 2);
        assert_eq!(&request[HEADER_SIZE..], &1_u32.to_le_bytes());
    }

    #[test]
    fn cached_wallpaper_restore_is_clipped_and_leaves_other_rows_untouched() {
        let mut source = Canvas::new(4, 4);
        for y in 0..4 {
            for x in 0..4 {
                source.set_pixel(x, y, Color::rgb(x as u8, y as u8, 0x44));
            }
        }
        let cached = source.pixels().to_vec();
        let mut destination = Canvas::new(4, 4);
        destination.clear(Color::rgb(0xaa, 0xbb, 0xcc));

        assert!(restore_cached_region(
            &mut destination,
            &cached,
            Rect::new(-3, 1, 10, 2),
        ));
        for y in 0..4 {
            for x in 0..4 {
                let expected = if (1..3).contains(&y) {
                    [x as u8, y as u8, 0x44, 0xff]
                } else {
                    [0xaa, 0xbb, 0xcc, 0xff]
                };
                assert_eq!(destination.pixel(x, y), Some(&expected[..]));
            }
        }
    }

    #[test]
    fn launcher_damage_is_narrow_and_covers_front_and_back_history() {
        let taskbar = Taskbar::new(1024, 768);
        let slots = &DEFAULT_LAUNCHER_SLOTS[..5];
        let closed = shell_chrome_state(&taskbar, slots, &LauncherUiState::default(), 1, 1);
        let open = shell_chrome_state(
            &taskbar,
            slots,
            &LauncherUiState {
                open: true,
                ..LauncherUiState::default()
            },
            1,
            2,
        );

        let open_damage = shell_chrome_damage_regions(&taskbar, Some(closed), Some(closed), open);
        assert_eq!(open_damage, vec![Rect::new(4, 608, 200, 158)]);

        // The target slot already contains closed chrome, but the front still
        // presents the popup. Closing must damage the front popup footprint as
        // well as the button or compositor pixels would remain visible.
        let desired_closed = ShellChromeState {
            launcher_generation: 3,
            ..closed
        };
        let close_damage =
            shell_chrome_damage_regions(&taskbar, Some(closed), Some(open), desired_closed);
        assert_eq!(close_damage, vec![Rect::new(4, 608, 200, 158)]);

        // Alternating again selects a slot whose old contents include the
        // popup; the same union erases that back-slot history even though the
        // currently presented state is closed.
        let reopened = ShellChromeState {
            launcher_generation: 4,
            ..open
        };
        let alternating =
            shell_chrome_damage_regions(&taskbar, Some(open), Some(desired_closed), reopened);
        assert_eq!(alternating, vec![Rect::new(4, 608, 200, 158)]);
    }

    #[test]
    fn focus_only_damage_is_the_exact_two_entry_pair() {
        let mut taskbar = Taskbar::new(1024, 768);
        taskbar.add_window(1, "PMos", "pmos.shell");
        taskbar.add_window(2, "Terminal", "pmos.term");
        taskbar.set_focused_window(1);
        let old = shell_chrome_state(&taskbar, &[], &LauncherUiState::default(), 7, 3);
        taskbar.set_focused_window(2);
        let desired = shell_chrome_state(&taskbar, &[], &LauncherUiState::default(), 7, 3);

        assert_eq!(
            shell_chrome_damage_regions(&taskbar, Some(old), Some(old), desired),
            vec![Rect::new(90, 738, 160, 28), Rect::new(252, 738, 160, 28)]
        );
    }

    #[test]
    fn focus_damage_includes_stale_presented_history_when_target_is_already_desired() {
        let mut taskbar = Taskbar::new(1024, 768);
        taskbar.add_window(1, "PMos", "pmos.shell");
        taskbar.add_window(2, "Terminal", "pmos.term");
        taskbar.set_focused_window(1);
        let old_front = shell_chrome_state(&taskbar, &[], &LauncherUiState::default(), 11, 4);
        taskbar.set_focused_window(2);
        let desired = shell_chrome_state(&taskbar, &[], &LauncherUiState::default(), 11, 4);

        assert_eq!(
            shell_chrome_damage_regions(&taskbar, Some(desired), Some(old_front), desired),
            vec![Rect::new(90, 738, 160, 28), Rect::new(252, 738, 160, 28)]
        );
    }

    #[test]
    fn taskbar_layout_mismatch_falls_back_to_the_full_band() {
        let mut taskbar = Taskbar::new(1024, 768);
        taskbar.add_window(1, "PMos", "pmos.shell");
        taskbar.add_window(2, "Terminal", "pmos.term");
        taskbar.set_focused_window(2);
        let desired = shell_chrome_state(&taskbar, &[], &LauncherUiState::default(), 13, 5);
        let stale = ShellChromeState {
            taskbar_layout_generation: 12,
            ..desired
        };

        assert_eq!(
            shell_chrome_damage_regions(&taskbar, Some(stale), Some(desired), desired),
            vec![Rect::new(0, 736, 1024, 32)]
        );
    }

    #[test]
    fn off_page_focus_switch_is_structural_and_falls_back_to_the_full_band() {
        let mut taskbar = Taskbar::new(1024, 768);
        for window_id in 1..=8 {
            taskbar.add_window(window_id, format!("Window {window_id}"), "pmos.test");
        }
        taskbar.set_focused_window(1);
        assert_eq!(taskbar.visible_range(), 0..7);
        let old = shell_chrome_state(&taskbar, &[], &LauncherUiState::default(), 21, 6);

        let focus_change = focus_taskbar_window(&mut taskbar, 8);
        assert_eq!(
            focus_change,
            TaskbarFocusChange {
                visual_changed: true,
                layout_changed: true,
            }
        );
        assert_eq!(taskbar.visible_range(), 7..8);
        let desired = shell_chrome_state(&taskbar, &[], &LauncherUiState::default(), 22, 6);

        assert_eq!(
            shell_chrome_damage_regions(&taskbar, Some(old), Some(old), desired),
            vec![Rect::new(0, 736, 1024, 32)]
        );
    }

    #[test]
    fn launcher_close_and_focus_keep_their_sparse_regions_disjoint() {
        let mut app = shell_app();
        let mut taskbar = Taskbar::new(1024, 768);
        for (window_id, app_id) in [
            (1, "pmos.shell"),
            (2, "pmos.term"),
            (3, "pmos.files"),
            (4, "pmos.edit"),
            (5, "pmos.settings"),
            (6, "pmos.sysmon"),
            (7, "pmos.term"),
        ] {
            taskbar.add_window(window_id, app_id, app_id);
        }
        taskbar.set_focused_window(2);
        let slots = &DEFAULT_LAUNCHER_SLOTS[..5];
        let mut launcher_ui = LauncherUiState {
            open: true,
            ..LauncherUiState::default()
        };
        let old = shell_chrome_state(&taskbar, slots, &launcher_ui, 31, 9);
        let target = taskbar.entry_rect(6).expect("second Term is visible");

        let outcome = handle_press(
            target.x + 8,
            target.y + 8,
            &mut taskbar,
            slots,
            &mut launcher_ui,
            &mut app,
        );
        assert_eq!(
            outcome,
            PressOutcome {
                visual_changed: true,
                taskbar_layout_changed: false,
                taskbar_focus_changed: true,
                launcher_changed: true,
            }
        );
        let desired = shell_chrome_state(&taskbar, slots, &launcher_ui, 31, 10);

        assert_eq!(
            shell_chrome_damage_regions(&taskbar, Some(old), Some(old), desired),
            vec![
                Rect::new(213, 738, 121, 28),
                Rect::new(828, 738, 121, 28),
                Rect::new(4, 608, 200, 158),
            ]
        );
    }

    #[test]
    fn stale_taskbar_precedes_disjoint_popup_and_pins_six_or_eleven_turns() {
        fn upload_turns(rect: Rect) -> usize {
            let row_bytes = rect.width as usize * BYTES_PER_PIXEL;
            let rows_per_turn = (toolkit::SHM_WRITE_CHUNK_BYTES / row_bytes).max(1);
            (rect.height as usize).div_ceil(rows_per_turn)
        }

        let taskbar = Taskbar::new(1024, 768);
        let slots = &DEFAULT_LAUNCHER_SLOTS[..5];
        let closed = shell_chrome_state(&taskbar, slots, &LauncherUiState::default(), 1, 1);
        let open = shell_chrome_state(
            &taskbar,
            slots,
            &LauncherUiState {
                open: true,
                ..LauncherUiState::default()
            },
            1,
            2,
        );
        let narrow = shell_chrome_damage_regions(&taskbar, Some(closed), Some(closed), open);
        assert_eq!(narrow, vec![Rect::new(4, 608, 200, 158)]);
        assert_eq!(narrow.iter().copied().map(upload_turns).sum::<usize>(), 6);

        let stale_back = ShellChromeState {
            taskbar_layout_generation: 0,
            ..closed
        };
        let stale = shell_chrome_damage_regions(&taskbar, Some(stale_back), Some(closed), open);
        assert_eq!(
            stale,
            vec![Rect::new(0, 736, 1024, 32), Rect::new(4, 608, 200, 128)]
        );
        assert!(stale[0].y >= stale[1].bottom(), "regions must be disjoint");
        assert_eq!(stale.iter().copied().map(upload_turns).sum::<usize>(), 11);
    }

    #[test]
    fn newer_input_survives_completion_of_an_older_staged_paint() {
        let mut needs_paint = true;
        let mut paint_serial = 1;
        let submitted_serial = paint_serial;
        request_paint(&mut needs_paint, &mut paint_serial);
        finish_submitted_paint(&mut needs_paint, paint_serial, submitted_serial);
        assert!(needs_paint);
        assert_ne!(paint_serial, submitted_serial);

        let latest_serial = paint_serial;
        finish_submitted_paint(&mut needs_paint, paint_serial, latest_serial);
        assert!(!needs_paint);
    }

    #[test]
    fn failed_spawn_feedback_is_visible_and_names_the_app() {
        let slot = LauncherSlot {
            label: "Notes",
            exec: "/missing/notes",
        };
        let feedback = LauncherFeedback::failed(&slot, 2);
        assert_eq!(feedback.message(), "Could not launch Notes (errno 2)");

        let taskbar = Taskbar::new(640, 480);
        let bounds = launcher_feedback_bounds(&taskbar);
        let mut canvas = Canvas::new(640, 480);
        draw_launcher_feedback(&mut canvas, &taskbar, &feedback);
        assert_eq!(
            canvas.pixel((bounds.x + 2) as u32, (bounds.y + 2) as u32),
            Some(&[0xa8, 0x2d, 0x2d, 0xff][..]),
        );
    }

    #[test]
    fn queued_spawn_failure_becomes_persistent_feedback() {
        let mut ui = LauncherUiState {
            pending_launch: Some(PendingLaunch {
                label: "Broken".to_string(),
                exec: "/missing/broken".to_string(),
            }),
            ..LauncherUiState::default()
        };
        let mut spawner = |_path: &str| -2;
        assert!(process_pending_launch(&mut ui, &mut spawner));
        assert_eq!(
            ui.feedback.as_ref().map(LauncherFeedback::message),
            Some("Could not launch Broken (errno 2)"),
        );
        assert!(ui.pending_launch.is_none());
    }
}
