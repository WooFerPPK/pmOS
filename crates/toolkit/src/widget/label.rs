//! `Label` — single-line text inside a rect.
//!
//! The simplest widget in the toolkit. A `Label` owns a
//! bounding rect, a string, a colour, and a horizontal
//! alignment. Vertical centring within the bounds is
//! automatic because the font is monospace and every
//! glyph is the same height. Text that doesn't fit the
//! bounds is clipped at a character boundary via
//! [`crate::draw::text::fit_text_to_width`].
//!
//! A `Label` has no input surface — it never sees a
//! pointer event. Widgets that need click handling (the
//! near-term `Button`) compose a `Label` for their caption
//! and add hit-testing on top.

use super::alignment::Alignment;
use crate::draw::font::GLYPH_HEIGHT;
use crate::draw::text::{fit_text_to_width, text_width_px};
use crate::draw::{Canvas, Color, Rect};
use crate::theme::Theme;

/// Total horizontal pad added by [`Label::preferred_size`]
/// to the measured text width. 2 px on each side — enough
/// to keep glyphs from butting up against an edge rect
/// drawn around the label. Design default; widget setters
/// can override later if a real app demands a tighter or
/// looser fit.
pub const LABEL_HPAD: u32 = 4;

/// Total vertical pad added by [`Label::preferred_size`]
/// to [`GLYPH_HEIGHT`]. 3 px above and 3 px below the
/// glyph row. Design default; widget setters can override
/// later.
pub const LABEL_VPAD: u32 = 6;

/// Single-line text widget.
pub struct Label {
    bounds: Rect,
    text: String,
    color: Color,
    alignment: Alignment,
}

impl Label {
    /// Construct a new label with the given bounds and
    /// text. The colour defaults to the current light
    /// theme's [`Theme::label_text`] and alignment
    /// defaults to [`Alignment::Left`].
    pub fn new(bounds: Rect, text: impl Into<String>) -> Self {
        Label {
            bounds,
            text: text.into(),
            color: Theme::LIGHT.label_text,
            alignment: Alignment::default(),
        }
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }

    pub fn color(&self) -> Color {
        self.color
    }

    pub fn set_color(&mut self, color: Color) {
        self.color = color;
    }

    pub fn alignment(&self) -> Alignment {
        self.alignment
    }

    pub fn set_alignment(&mut self, alignment: Alignment) {
        self.alignment = alignment;
    }

    /// Leading slice of the label text that actually fits
    /// within the bounds. The rest is clipped at a
    /// character boundary.
    pub fn visible_text(&self) -> &str {
        fit_text_to_width(&self.text, self.bounds.width)
    }

    /// Minimum rect size that comfortably holds the label's
    /// current text with standard padding. Advisory: the
    /// caller either feeds the result into a layout
    /// (e.g. [`crate::layout::Row::next`]) or ignores it.
    /// No coupling between widget and layout.
    ///
    /// Width = text width + [`LABEL_HPAD`]. Empty text
    /// returns just the pad width so a label with no text
    /// still gets a non-zero horizontal slot.
    ///
    /// Height = [`GLYPH_HEIGHT`] + [`LABEL_VPAD`].
    /// Independent of the text since every glyph in the
    /// toolkit font is the same height.
    pub fn preferred_size(&self) -> (u32, u32) {
        (
            text_width_px(&self.text) + LABEL_HPAD,
            GLYPH_HEIGHT + LABEL_VPAD,
        )
    }

    /// Paint the label into `canvas`. Does nothing if the
    /// bounds are empty, the text is empty, or the bounds
    /// can't fit a single glyph row.
    pub fn draw(&self, canvas: &mut Canvas) {
        if self.bounds.is_empty() || self.bounds.height < GLYPH_HEIGHT {
            return;
        }
        let visible = self.visible_text();
        if visible.is_empty() {
            return;
        }

        let text_px = text_width_px(visible);
        let x_offset = match self.alignment {
            Alignment::Left => 0,
            Alignment::Center => (self.bounds.width.saturating_sub(text_px) / 2) as i32,
            Alignment::Right => self.bounds.width.saturating_sub(text_px) as i32,
        };
        let y_offset = ((self.bounds.height - GLYPH_HEIGHT) / 2) as i32;

        let text_x = self.bounds.x + x_offset;
        let text_y = self.bounds.y + y_offset;
        canvas.draw_text(text_x, text_y, visible, self.color);
    }
}
