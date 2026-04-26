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

pub mod launcher;
pub mod paint;
pub mod session;
pub mod taskbar;

pub use launcher::{
    DesktopEntry, DesktopEntryStore, Launcher, LauncherError, MemoryStore,
};
pub use paint::{run_shell, ShellExit, DEFAULT_HEIGHT, DEFAULT_WIDTH};
pub use session::{
    GlobalEntry, Session, SessionError, SessionStep, INTERESTING_INTERFACES,
};
pub use taskbar::{
    Taskbar, TaskbarClick, TaskbarEntry, TaskbarError, TASKBAR_ENTRY_GAP, TASKBAR_ENTRY_WIDTH,
    TASKBAR_HEIGHT, TASKBAR_LEFT_MARGIN,
};
