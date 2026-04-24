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
//! * [`alignment::Alignment`] — horizontal placement
//!   vocabulary shared by every widget that places
//!   content inside its own bounds.
//! * [`frame::WindowFrame`] — the chrome drawn around every
//!   top-level window.
//! * [`label::Label`] — single-line text inside a rect.
//! * [`button::Button`] — clickable rect with a centred
//!   caption. `WindowFrame`'s close button is a `Button`.
//! * [`text_input::TextInput`] — single-line editable text
//!   with cursor, visual states, pointer hit-test, and
//!   keyboard handlers.
//! * [`container::Container`] — layout primitive with
//!   optional border + uniform padding that delegates child
//!   painting to an opaque `FnMut` closure.
//! * [`list::List`] — scrollable single-column list with
//!   pointer + keyboard selection.
//!
//! With `List` and `Container` landed, the "Widget trait +
//! primitives" scope of T116 is complete. No `Widget` trait
//! was introduced — each widget stands alone with inherent
//! `paint` / `hit_test` methods, matching the style of
//! `Label`, `Button`, and `TextInput`.

pub mod alignment;
pub mod button;
pub mod container;
pub mod frame;
pub mod label;
pub mod list;
pub mod text_input;

pub use alignment::Alignment;
pub use button::{Button, ButtonState, BUTTON_HPAD, BUTTON_VPAD};
pub use container::Container;
pub use frame::{
    PointerOutcome, WindowFrame, BORDER_WIDTH, CLOSE_BUTTON_MARGIN, CLOSE_BUTTON_SIZE,
    TITLE_TEXT_MARGIN_X, TITLE_TEXT_TRAILING_GAP, TITLEBAR_HEIGHT,
};
pub use label::{Label, LABEL_HPAD, LABEL_VPAD};
pub use list::{List, ListDimensions, ListKey, ListKeyOutcome, LIST_HPAD, LIST_ROW_HEIGHT};
pub use text_input::{Key, KeyOutcome, TextInput, TextInputState, TEXT_INPUT_PADDING_X};
