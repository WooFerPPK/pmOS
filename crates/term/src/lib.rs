//! `/usr/bin/term` — PMos graphical terminal emulator.
//!
//! The `term` crate is organised as a library (wrapped by a
//! thin bin driver) so the core state machine — a bounded
//! scrollback buffer plus an input editor plus an embedded
//! [`sh::Shell`] — is testable on the native host without
//! needing a display-protocol connection or a WASI sandbox.
//!
//! The design mirrors the TypeScript-side `web/src/terminal.ts`
//! used by the bootstrap demo: bounded scrollback, a `feed_key`
//! method for DOM-like keystrokes, and an `append_output`
//! method for streaming UTF-8 bytes. The crucial difference is
//! that the Rust terminal **owns** its own [`Shell`] instance,
//! so committed lines are evaluated in-process and their
//! stdout/stderr are appended to scrollback directly — there
//! is no kernel round-trip.
//!
//! A future slice (Phase 4 T132 wiring) will wrap this library
//! in a `toolkit::Client` so the terminal becomes a real
//! display-protocol surface hosted by the display server; at
//! that point the bin driver will grow from the stdin REPL
//! below to a full window-hosted renderer.

pub mod session;
pub mod terminal;

pub use sh::{Shell, ShellOutput};
pub use session::{
    GlobalEntry, ProtocolErrorNotice, Session, SessionError, SessionStep,
    INTERESTING_INTERFACES,
};
pub use terminal::{
    Key, KeyFeedResult, LineKind, Terminal, TerminalLine, TerminalOptions, TerminalSnapshot,
    DEFAULT_MAX_LINES,
};
