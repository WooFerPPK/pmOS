//! `TextInput` — single-line editable text widget.
//!
//! Paints a rectangle with a 1-pixel border, a background
//! fill that varies with [`TextInputState`] (`Idle` /
//! `Hover` / `Focused`), the current text content clipped
//! to the widget's interior, and — when focused — a cursor
//! indicator at the current caret position. When the text
//! is empty and the widget is not focused, a dimmed
//! placeholder string paints in the text area instead.
//!
//! Input model:
//!
//! * [`TextInput::pointer_down`] hit-tests; a press inside
//!   transitions the widget to [`TextInputState::Focused`]
//!   and moves the caret to the end of the text. Click-to-
//!   position within the visible text is deferred to a
//!   future slice.
//! * [`TextInput::handle_key`] mutates the text + caret
//!   according to a small set of supported keys: printable
//!   ASCII → insert at caret; [`Key::Backspace`] → delete
//!   the char to the left of the caret; [`Key::Left`] /
//!   [`Key::Right`] → move the caret one char boundary;
//!   [`Key::Home`] / [`Key::End`] → jump the caret to the
//!   start or end. Other keys are ignored. See
//!   [`KeyOutcome`] for the return-value vocabulary.
//! * [`TextInput::set_focus`] and [`TextInput::set_hover`]
//!   are app-level hooks for the future input router
//!   (T114's `App`) — same shape as
//!   [`super::button::Button::set_state`].
//!
//! No word wrapping, no scrolling when the text exceeds the
//! widget's interior — content wider than the interior is
//! clipped at a character boundary on the trailing edge.
//! v1 only — a future slice adds a horizontal scroll window
//! tied to the caret position.

use crate::draw::font::{CELL_WIDTH, GLYPH_HEIGHT};
use crate::draw::text::fit_text_to_width;
use crate::draw::{Canvas, Color, Rect};
use crate::theme::Theme;

/// Horizontal pad between the widget border and the text /
/// placeholder content inside it, in pixels. 4 px on the
/// left, same on the right — matches the look of
/// [`super::button::Button`]'s caption inset.
pub const TEXT_INPUT_PADDING_X: u32 = 4;

/// Visual state of a [`TextInput`]. Controls which fill
/// colour the widget paints with and whether the caret is
/// drawn.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TextInputState {
    /// Resting: pointer is not over the widget and the
    /// widget does not have keyboard focus.
    Idle,
    /// Pointer is hovering over the widget.
    Hover,
    /// Widget has keyboard focus. Cursor paints; key events
    /// mutate the text + caret.
    Focused,
}

impl Default for TextInputState {
    fn default() -> Self {
        TextInputState::Idle
    }
}

/// Outcome of a [`TextInput::handle_key`] call. Lets the
/// caller distinguish "text content changed" from "caret
/// moved" from "ignored keystroke" so apps can choose
/// whether to re-render or not.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Text content was mutated (char inserted or deleted).
    Changed,
    /// Caret moved without mutating text.
    CaretMoved,
    /// Key was not handled by this [`TextInput`].
    Ignored,
}

/// Minimal keyboard vocabulary consumed by
/// [`TextInput::handle_key`]. The toolkit does not yet have
/// a cross-widget keyboard event type; when T114's `App`
/// wires display-server keyboard events into widget calls
/// this enum will either grow or be replaced by a shared
/// vocabulary. For v1 it stays local to this module.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// A printable character to insert at the caret.
    /// Non-printable characters (control codes, DEL,
    /// non-ASCII beyond the bitmap font) are ignored.
    Char(char),
    /// Delete the char immediately left of the caret.
    /// Ignored when the caret is at the start of the text.
    Backspace,
    /// Move the caret one char boundary left. Ignored at
    /// position 0.
    Left,
    /// Move the caret one char boundary right. Ignored at
    /// the end of the text.
    Right,
    /// Jump the caret to position 0.
    Home,
    /// Jump the caret to the end of the text.
    End,
}

/// Single-line editable text widget.
pub struct TextInput {
    bounds: Rect,
    text: String,
    /// Byte index into [`Self::text`] — must always land on
    /// a UTF-8 char boundary. Invariant maintained by every
    /// method that mutates `text` or `caret`.
    caret: usize,
    state: TextInputState,
    placeholder: String,
}

impl TextInput {
    /// Construct an empty text input with the given bounds.
    /// Starts in [`TextInputState::Idle`]; no placeholder.
    pub fn new(bounds: Rect) -> Self {
        TextInput {
            bounds,
            text: String::new(),
            caret: 0,
            state: TextInputState::Idle,
            placeholder: String::new(),
        }
    }

    /// Builder-style setter for the placeholder string
    /// shown when the text is empty and the widget is not
    /// focused. Returns `self` so the caller can chain it
    /// with [`Self::new`].
    pub fn with_placeholder(mut self, placeholder: &str) -> Self {
        self.placeholder = placeholder.to_string();
        self
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn state(&self) -> TextInputState {
        self.state
    }

    pub fn placeholder(&self) -> &str {
        &self.placeholder
    }

    /// Replace the text content. The caret is repositioned
    /// to the end of the new text so subsequent typing
    /// appends.
    pub fn set_text(&mut self, text: &str) {
        self.text.clear();
        self.text.push_str(text);
        self.caret = self.text.len();
    }

    /// Clear the text. Repositions the caret to 0.
    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    /// Set the visual state directly. Used by the future
    /// input router to drive
    /// [`TextInputState::Idle`]/[`Hover`]/[`Focused`]
    /// transitions from display-server keyboard + pointer
    /// events; tests flip it directly.
    ///
    /// [`Hover`]: TextInputState::Hover
    /// [`Focused`]: TextInputState::Focused
    pub fn set_state(&mut self, state: TextInputState) {
        self.state = state;
    }

    /// Convenience setter that flips between
    /// [`TextInputState::Focused`] and the resting state —
    /// [`TextInputState::Idle`] (no pointer) or
    /// [`TextInputState::Hover`] (pointer over) depending on
    /// `hovering`. Matches the `set_focus` shape requested
    /// by the T116 brief.
    pub fn set_focus(&mut self, focused: bool) {
        self.state = if focused {
            TextInputState::Focused
        } else {
            TextInputState::Idle
        };
    }

    /// Convenience setter that flips between
    /// [`TextInputState::Hover`] and [`TextInputState::Idle`].
    /// A widget that currently has focus is left alone —
    /// the focused state takes precedence over hover state,
    /// matching the stack `WindowFrame` uses for its close
    /// button.
    pub fn set_hover(&mut self, hovering: bool) {
        if self.state == TextInputState::Focused {
            return;
        }
        self.state = if hovering {
            TextInputState::Hover
        } else {
            TextInputState::Idle
        };
    }

    /// True iff `(x, y)` falls inside the widget's bounds.
    /// An empty-bounds widget never hits.
    pub fn hit_test(&self, x: i32, y: i32) -> bool {
        if self.bounds.is_empty() {
            return false;
        }
        x >= self.bounds.x
            && x < self.bounds.right()
            && y >= self.bounds.y
            && y < self.bounds.bottom()
    }

    /// Handle a pointer-down event. Transitions the widget
    /// to [`TextInputState::Focused`] if the press landed
    /// inside the widget's bounds and moves the caret to the
    /// end of the current text (click-to-position is a
    /// future slice). Returns `true` iff the press was
    /// inside (so the caller can short-circuit any further
    /// routing).
    pub fn pointer_down(&mut self, x: i32, y: i32) -> bool {
        if !self.hit_test(x, y) {
            return false;
        }
        self.state = TextInputState::Focused;
        self.caret = self.text.len();
        true
    }

    /// Apply a keystroke. See [`KeyOutcome`] for return-
    /// value semantics. Does nothing when the widget is not
    /// focused.
    pub fn handle_key(&mut self, key: Key) -> KeyOutcome {
        if self.state != TextInputState::Focused {
            return KeyOutcome::Ignored;
        }
        match key {
            Key::Char(c) => {
                if !is_printable_ascii(c) {
                    return KeyOutcome::Ignored;
                }
                self.text.insert(self.caret, c);
                self.caret += c.len_utf8();
                KeyOutcome::Changed
            }
            Key::Backspace => {
                if self.caret == 0 {
                    return KeyOutcome::Ignored;
                }
                let prev = prev_char_boundary(&self.text, self.caret);
                self.text.replace_range(prev..self.caret, "");
                self.caret = prev;
                KeyOutcome::Changed
            }
            Key::Left => {
                if self.caret == 0 {
                    return KeyOutcome::Ignored;
                }
                self.caret = prev_char_boundary(&self.text, self.caret);
                KeyOutcome::CaretMoved
            }
            Key::Right => {
                if self.caret >= self.text.len() {
                    return KeyOutcome::Ignored;
                }
                self.caret = next_char_boundary(&self.text, self.caret);
                KeyOutcome::CaretMoved
            }
            Key::Home => {
                if self.caret == 0 {
                    return KeyOutcome::Ignored;
                }
                self.caret = 0;
                KeyOutcome::CaretMoved
            }
            Key::End => {
                if self.caret == self.text.len() {
                    return KeyOutcome::Ignored;
                }
                self.caret = self.text.len();
                KeyOutcome::CaretMoved
            }
        }
    }

    /// Colour used to paint the widget's interior fill for
    /// the current [`TextInputState`].
    fn current_fill(&self, theme: &Theme) -> Color {
        match self.state {
            TextInputState::Idle => theme.text_input_bg,
            TextInputState::Hover => theme.text_input_bg_hover,
            TextInputState::Focused => theme.text_input_bg_focused,
        }
    }

    /// Pixel width available to the text content after the
    /// 1-pixel border + [`TEXT_INPUT_PADDING_X`] inset on
    /// both sides. Zero if the widget is too narrow to hold
    /// any glyph.
    fn text_budget_px(&self) -> u32 {
        let chrome = 2 * (1 + TEXT_INPUT_PADDING_X);
        self.bounds.width.saturating_sub(chrome)
    }

    /// Leading slice of the text that actually fits inside
    /// the widget interior after padding. Trailing edge
    /// clipped at a character boundary.
    pub fn visible_text(&self) -> &str {
        fit_text_to_width(&self.text, self.text_budget_px())
    }

    /// Paint the widget into `canvas` using `theme`
    /// colours. Does nothing on empty bounds or when the
    /// bounds can't fit one glyph row plus the 1-pixel
    /// border.
    pub fn paint(&self, canvas: &mut Canvas, theme: &Theme) {
        if self.bounds.is_empty() {
            return;
        }
        canvas.fill_rect(self.bounds, self.current_fill(theme));
        canvas.stroke_rect(self.bounds, theme.text_input_border);

        // Vertical centring inside the interior.
        if self.bounds.height < GLYPH_HEIGHT + 2 {
            return;
        }
        let interior_h = self.bounds.height.saturating_sub(2);
        let text_y = self.bounds.y
            + 1
            + ((interior_h.saturating_sub(GLYPH_HEIGHT)) / 2) as i32;
        let text_x = self.bounds.x + 1 + TEXT_INPUT_PADDING_X as i32;

        let use_placeholder = self.text.is_empty() && self.state != TextInputState::Focused;
        if use_placeholder {
            let visible = fit_text_to_width(&self.placeholder, self.text_budget_px());
            if !visible.is_empty() {
                canvas.draw_text(text_x, text_y, visible, theme.text_input_placeholder_fg);
            }
        } else {
            let visible = self.visible_text();
            if !visible.is_empty() {
                canvas.draw_text(text_x, text_y, visible, theme.text_input_fg);
            }
        }

        if self.state == TextInputState::Focused {
            // Render a 1-pixel vertical bar at the caret's
            // x-position. Caret x = text_x + (chars_before_caret * CELL_WIDTH).
            // If the caret is past the visible text budget we still paint
            // at the right edge of the interior — v1 has no horizontal
            // scroll so the caret can visually pin against the trailing
            // clip point.
            let chars_before_caret = self.text[..self.caret].chars().count() as u32;
            let raw_caret_x = text_x + (chars_before_caret * CELL_WIDTH) as i32;
            let interior_right = self.bounds.right() - 1 - TEXT_INPUT_PADDING_X as i32;
            let caret_x = raw_caret_x.min(interior_right);
            for row in 0..GLYPH_HEIGHT {
                canvas.set_pixel(caret_x, text_y + row as i32, theme.text_input_fg);
            }
        }
    }
}

/// True iff `c` is a printable ASCII character the bitmap
/// font can render (0x20–0x7E inclusive). Other code points
/// — control codes, DEL, anything above 0x7F — are rejected
/// by [`TextInput::handle_key`] when they arrive as
/// [`Key::Char`].
fn is_printable_ascii(c: char) -> bool {
    let code = c as u32;
    (0x20..=0x7E).contains(&code)
}

/// Return the byte index of the previous char boundary in
/// `s` before `byte_idx`. `byte_idx` must itself be a valid
/// char boundary. Returns 0 when `byte_idx` is 0.
fn prev_char_boundary(s: &str, byte_idx: usize) -> usize {
    if byte_idx == 0 {
        return 0;
    }
    s[..byte_idx]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Return the byte index of the next char boundary in
/// `s` after `byte_idx`. `byte_idx` must itself be a valid
/// char boundary. Returns `s.len()` when `byte_idx` is at
/// or past the end of the string.
fn next_char_boundary(s: &str, byte_idx: usize) -> usize {
    if byte_idx >= s.len() {
        return s.len();
    }
    s[byte_idx..]
        .char_indices()
        .nth(1)
        .map(|(i, _)| byte_idx + i)
        .unwrap_or(s.len())
}
