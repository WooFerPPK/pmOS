//! `/usr/bin/term` — PMos graphical terminal emulator.
//!
//! The `term` crate is organised as a library (wrapped by a
//! thin bin driver) so the core state machine — a bounded
//! scrollback buffer plus an input editor plus an embedded
//! [`sh::Shell`] — is testable on the native host without
//! needing a display-protocol connection or a WASI sandbox.
//!
//! The state machine retains an embedded [`Shell`] for deterministic native
//! tests. Production uses [`PmosShellSession`], a persistent isolated
//! `/bin/sh` Worker connected by kernel pipes, and appends the returned byte
//! stream to the same bounded scrollback model.
//!
//! [`run::run_term`] is the production-facing entry point: it
//! connects to the display server through the supplied
//! [`toolkit::Connection`], drives the toolkit window event
//! loop, and routes `pmd_keyboard.key` events through the
//! per-key scancode table in [`keymap`].

pub mod keymap;
pub mod pmos_shell;
pub mod rasterizer;
pub mod run;
pub mod session;
pub mod terminal;

pub use sh::{Shell, ShellOutput};
// These legacy names describe the safe default font. Font-aware callers should
// use `BitmapFont` methods because the selected atlas may instead be 8×16.
pub use keymap::{translate as translate_scancode, Modifiers};
pub use pmos_shell::{PmosShellSession, StepwiseCommandRunner, StepwiseShellUpdate};
pub use rasterizer::{
    colors, default_font, load_startup_font, load_startup_font_with, rasterize_snapshot,
    rasterize_snapshot_region_with_palette_and_font, rasterize_snapshot_with_font,
    rasterize_snapshot_with_palette, rasterize_snapshot_with_palette_and_font, BitmapFont,
    FontError, Palette, RasterRegion, BYTES_PER_PIXEL, DEFAULT_CELL_HEIGHT as CELL_HEIGHT,
    DEFAULT_CELL_WIDTH as CELL_WIDTH, DEFAULT_FONT_NAME, DEFAULT_GLYPH_HEIGHT as GLYPH_HEIGHT,
    DEFAULT_GLYPH_WIDTH as GLYPH_WIDTH, FONT_DIR, MAX_FONT_BYTES, MAX_PREFERENCES_BYTES, PADDING,
    VGA_FONT_NAME,
};
pub use run::{
    run_term, run_term_with_font, run_term_with_options, run_term_with_runner,
    run_term_with_runner_and_font, run_term_with_stepwise_runner_and_font, TermExit,
    DEFAULT_HEIGHT, DEFAULT_WIDTH,
};
pub use session::{
    GlobalEntry, ProtocolErrorNotice, Session, SessionError, SessionStep, INTERESTING_INTERFACES,
};
pub use terminal::{
    CommandRunResult, CommandRunner, Key, KeyFeedResult, LineKind, Terminal, TerminalLine,
    TerminalOptions, TerminalSnapshot, DEFAULT_MAX_LINES, MAX_PENDING_OUTPUT_BYTES,
};
