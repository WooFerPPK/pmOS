//! Text-measurement helpers on top of the toolkit bitmap
//! font.
//!
//! The toolkit ships a single monospaced 5×7 bitmap font
//! (see [`super::font`]), so measurement is always a
//! straight multiplication by [`super::font::CELL_WIDTH`].
//! This module exists so widgets that need to clip text to
//! a pixel budget (a titlebar, a label bound to a narrow
//! rect, a button caption) don't each reinvent the
//! character-boundary arithmetic.
//!
//! Helpers here are strictly about geometry — they answer
//! "how many characters fit?" or "how wide will this text
//! be?" They do **not** paint. Widgets call a helper to
//! decide what slice of text to draw and then call
//! [`super::Canvas::draw_text`] themselves.

use super::font::CELL_WIDTH;

/// Return the longest leading slice of `text` that fits
/// within `max_width_px` pixels when rendered with the
/// toolkit bitmap font.
///
/// If `max_width_px` is smaller than one glyph cell, the
/// result is `""`. If `text` fits entirely, the full input
/// is returned. Truncation always lands on a character
/// boundary so callers can hand the result directly to
/// [`super::Canvas::draw_text`] without worrying about
/// splitting a UTF-8 sequence.
pub fn fit_text_to_width(text: &str, max_width_px: u32) -> &str {
    let max_chars = (max_width_px / CELL_WIDTH) as usize;
    if max_chars == 0 {
        return "";
    }
    match text.char_indices().nth(max_chars) {
        Some((byte_pos, _)) => &text[..byte_pos],
        None => text,
    }
}

/// Pixel width of `text` if it were rendered directly via
/// [`super::Canvas::draw_text`]. Equal to
/// `text.chars().count() * CELL_WIDTH`.
pub fn text_width_px(text: &str) -> u32 {
    (text.chars().count() as u32) * CELL_WIDTH
}
