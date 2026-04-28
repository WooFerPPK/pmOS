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
//! [`run::run_term`] is the production-facing entry point: it
//! connects to the display server through the supplied
//! [`toolkit::Connection`], drives the toolkit window event
//! loop, and routes `pmd_keyboard.key` events through the
//! per-key scancode table in [`keymap`].

pub mod keymap;
pub mod rasterizer;
pub mod run;
pub mod session;
pub mod terminal;

pub use sh::{Shell, ShellOutput};
// Font constants live in `toolkit::draw::font` so every
// bundled app (term, files, edit, …) shares the same
// bitmap glyphs. Re-exported here for backwards compat
// with the existing `term::CELL_HEIGHT` / `GLYPH_WIDTH`
// usage sites.
pub use toolkit::draw::font::{CELL_HEIGHT, CELL_WIDTH, GLYPH_HEIGHT, GLYPH_WIDTH};
pub use keymap::{translate as translate_scancode, Modifiers};
pub use rasterizer::{
    colors, rasterize_snapshot, rasterize_snapshot_with_palette, Palette, BYTES_PER_PIXEL,
    PADDING,
};
pub use run::{run_term, run_term_with_options, TermExit, DEFAULT_HEIGHT, DEFAULT_WIDTH};
pub use session::{
    GlobalEntry, ProtocolErrorNotice, Session, SessionError, SessionStep,
    INTERESTING_INTERFACES,
};
pub use terminal::{
    Key, KeyFeedResult, LineKind, Terminal, TerminalLine, TerminalOptions, TerminalSnapshot,
    DEFAULT_MAX_LINES,
};
