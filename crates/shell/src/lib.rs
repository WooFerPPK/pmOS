//! PMos desktop shell — library layer.
//!
//! The shell is the process that holds the `Shell` +
//! `DisplayClient` + `ProcEnumerate` + `KeymapAdmin`
//! capabilities (see `abi::cap::initial::DESKTOP_SHELL`).
//! In production it draws the wallpaper, the taskbar, the
//! launcher, and the window chrome for every open app; it
//! is the "root" userland UI process and it is the SINGLE
//! most replaceable layer in the system (Principle II).
//!
//! This crate is a hybrid lib + bin. The library layer
//! below is the canonical protocol state machine: a
//! `Session` wraps a `toolkit::Client`, drives the
//! `display → registry → bind` handshake, and tracks which
//! globals the display server has advertised. The binary at
//! `src/main.rs` is the thin runtime driver — eventually it
//! will open `/run/display` via the kernel's
//! `display_connect` extension syscall, feed bytes through
//! a Session, and render windows; for now it is a stub that
//! prints the session state as a dry run.
//!
//! What's NOT in scope for the v1 skeleton:
//!
//! * Widgets / rendering. The wallpaper and taskbar get
//!   drawn only once a SHM-backed buffer path exists.
//! * `pmd_shell_manager` extension. The window-list /
//!   focus-window / close-window API needs its own
//!   display-proto interface table entries and lands in a
//!   follow-up slice.
//! * Proc spawn. Launching apps requires the kernel's
//!   `proc_spawn` to be bridged to userland, which is T074
//!   follow-up work.
//!
//! The slice below covers the foundation: the Session
//! state machine + the bind-reactive handler. Every
//! higher feature extends this core.

#![forbid(unsafe_code)]

pub mod desktop_preferences;
pub mod launcher;
pub mod launcher_watcher;
pub mod paint;
pub mod session;
pub mod session_restore;
pub mod session_store;
pub mod spawn;
pub mod taskbar;
pub mod wallpaper;

pub use desktop_preferences::{
    format_clock, ClockSnapshot, DesktopPreferenceRuntime, DesktopPreferenceUpdate,
    DesktopPreferences, FilesystemPreferenceSource, PreferenceClock, PreferenceMonitor,
    PreferenceSource, SystemPreferenceClock, ThemeChoice, TimezoneChoice, WallpaperChoice,
    WallpaperFit, PREFERENCE_CLOCK_CHECK_EVERY_ITERATIONS, PREFERENCE_POLL_INTERVAL_MS,
};
pub use launcher::{
    DesktopEntry, DesktopEntryStore, FilesystemStore, Launcher, LauncherClock, LauncherError,
    LauncherRuntime, MemoryStore, SystemLauncherClock, LAUNCHER_CLOCK_CHECK_EVERY_ITERATIONS,
};
pub use paint::{
    run_desktop_shell, run_desktop_shell_live, run_desktop_shell_live_with_events,
    run_desktop_shell_live_with_events_and_session, run_desktop_shell_with_preferences,
    run_desktop_shell_with_runtimes, run_desktop_shell_with_runtimes_and_events,
    run_desktop_shell_with_runtimes_events_and_session, run_shell, run_shell_with_taskbar,
    DesktopEventSource, DesktopWake, LauncherSlot, ShellExit, Spawner, DEFAULT_HEIGHT,
    DEFAULT_LAUNCHER_SLOTS, DEFAULT_WIDTH, LAUNCHER_BUTTON_RIGHT_MARGIN, LAUNCHER_BUTTON_WIDTH,
    LAUNCHER_MENU_PADDING, LAUNCHER_MENU_ROW_HEIGHT, LAUNCHER_MENU_TEXT_MARGIN,
    LAUNCHER_MENU_WIDTH,
};
pub use session::{GlobalEntry, Session, SessionError, SessionStep, INTERESTING_INTERFACES};
pub use session_restore::{
    CatalogIdentity, SessionAction, SessionRuntime, SESSION_RESTORE_ID,
    SESSION_RESTORE_SOFT_DEADLINE, SESSION_SNAPSHOT_ID,
};
pub use session_store::{
    AtomicSessionWriter, SessionFile, SessionFilesystem, SessionFormatError, SessionLoadStep,
    SessionLoader, SessionWait, SessionWriteStep, StdSessionFilesystem, StoredInstance,
    StoredSession, StoredWindow, MAX_SESSION_BYTES, MAX_SESSION_IDENTIFIER_BYTES,
    MAX_SESSION_INSTANCES, MAX_SESSION_WINDOWS, SESSION_FLAG_MAXIMIZED, SESSION_FLAG_MINIMIZED,
    SESSION_IO_CHUNK_BYTES, SESSION_PATH,
};
pub use spawn::encode_with_spawn_timezone;
pub use taskbar::{
    Taskbar, TaskbarClick, TaskbarEntry, TaskbarError, TASKBAR_CLOCK_RESERVED_WIDTH,
    TASKBAR_CLOCK_RIGHT_MARGIN, TASKBAR_ENTRY_CONTROL_GAP, TASKBAR_ENTRY_CONTROL_WIDTH,
    TASKBAR_ENTRY_GAP, TASKBAR_ENTRY_WIDTH, TASKBAR_HEIGHT, TASKBAR_LAUNCHER_RESERVED_WIDTH,
    TASKBAR_LEFT_MARGIN, TASKBAR_MIN_ENTRY_WIDTH, TASKBAR_OVERFLOW_WIDTH,
};
pub use wallpaper::{
    paint_wallpaper, FilesystemWallpaperSource, WallpaperDecodeError, WallpaperImage,
    WallpaperRefreshStep, WallpaperRuntime, WallpaperSource, MAX_WALLPAPER_DECODED_BYTES,
    MAX_WALLPAPER_DIMENSION, MAX_WALLPAPER_ENCODED_BYTES, MAX_WALLPAPER_PIXELS,
    WALLPAPER_DECODE_BYTES_PER_STEP, WALLPAPER_DECODE_ROWS_PER_STEP, WALLPAPER_DIRECTORY,
    WALLPAPER_READ_BYTES_PER_STEP,
};
