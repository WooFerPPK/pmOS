//! Widgets that compose on top of [`crate::draw::Canvas`].
//!
//! Each widget is a pure client-side paint + hit-test
//! helper. Widgets do not own any protocol state — the
//! future [`crate::app::App`] (T114) wires pointer/keyboard
//! events from the display server into widget hit-tests and
//! callbacks. Tests here drive the widgets directly against
//! a native [`crate::draw::Canvas`] with no display server
//! in the loop (Principle X).
//!
//! Populated incrementally. Currently:
//!
//! * [`frame::WindowFrame`] — the chrome drawn around every
//!   top-level window.
//! * [`label::Label`] — single-line text inside a rect.
//!
//! `Button`, `TextInput`, `List`, and `Container` (the rest
//! of T116) land in later slices.

pub mod frame;
pub mod label;

pub use frame::{
    PointerOutcome, WindowFrame, BORDER_WIDTH, CLOSE_BUTTON_MARGIN, CLOSE_BUTTON_SIZE,
    TITLE_TEXT_MARGIN_X, TITLE_TEXT_TRAILING_GAP, TITLEBAR_HEIGHT,
};
pub use label::{Alignment, Label};
