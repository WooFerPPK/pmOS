//! Drawing primitives for client-side rendering.
//!
//! Apps use [`Canvas`] to paint into an ARGB/RGBA pixel
//! buffer that they then upload into a `pmd_buffer` via
//! [`crate::protocol::Client::shm_pool_create_buffer`] +
//! `surface.attach` + `surface.commit`. The primitives
//! intentionally stay low-level — rectangles, text, single
//! pixels — so widgets ([`crate::widget`]) can compose
//! whatever chrome they need on top without reimplementing
//! pixel plotting.
//!
//! The [`font`] module is a tiny 5x7 bitmap font used by
//! [`Canvas::draw_text`]; it lives here instead of in a
//! per-app crate so the font is shared across every
//! toolkit client that needs text — term, files, edit,
//! settings, etc. See `font.rs` for the glyph table.

pub mod buffer;
pub mod canvas;
pub mod font;
pub mod text;

pub use buffer::BufferPool;
pub use canvas::{Canvas, Color, Rect, BYTES_PER_PIXEL};
pub use font::{
    glyph_for, glyph_pixel, Glyph, CELL_HEIGHT, CELL_WIDTH, FIRST_CHAR, GLYPH_COUNT,
    GLYPH_HEIGHT, GLYPH_WIDTH, LAST_CHAR, UNKNOWN_GLYPH,
};
pub use text::{fit_text_to_width, text_width_px};
