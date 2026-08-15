//! `Button` — clickable rect with a centred caption.
//!
//! Composition: owns a [`Label`] for the caption so the
//! centre-alignment + clip-to-bounds path is shared with
//! every other text widget. The button draws its own fill
//! (with hover + pressed variants) and a 1-pixel border,
//! then paints the child label on top.
//!
//! The first real consumer is the close button on
//! [`super::frame::WindowFrame`] — the API here was
//! designed alongside that consumer, not against imagined
//! future buttons. WindowFrame stores a `Button` as a
//! field, configures its colours from the window-chrome
//! theme slots instead of the standalone `button_*` slots,
//! and delegates `on_close`, `set_close_hover`, `hit_test`,
//! and the `PointerOutcome::Close` arm of `pointer_down`
//! to the stored button.
//!
//! Event model is deliberately thin for v1: `pointer_down`
//! does a hit-test and fires the `on_click` callback if
//! the press lands inside. The button does not track
//! pointer state across events, so "press-and-release
//! outside cancels the click" is not yet a thing. The
//! three-state `ButtonState` enum exists for the *visual*
//! state (which fill colour to paint); callers — or the
//! future [`crate::app::App`] — drive transitions via
//! [`Button::set_state`]. When the real pointer-event
//! router lands it can fold press-state tracking into
//! `pointer_down` / `pointer_up` without changing the
//! callback contract.

use super::alignment::Alignment;
use super::label::Label;
use crate::draw::font::GLYPH_HEIGHT;
use crate::draw::text::text_width_px;
use crate::draw::{Canvas, Color, Rect};
use crate::theme::Theme;

/// Total horizontal pad added by [`Button::preferred_size`]
/// to the measured caption width. 8 px on each side — enough
/// breathing room for a button to feel clickable without
/// looking comically wide around short captions. Design
/// default; widget setters can override later if a real
/// app demands different sizing.
pub const BUTTON_HPAD: u32 = 16;

/// Total vertical pad added by [`Button::preferred_size`]
/// to [`GLYPH_HEIGHT`]. 4 px above and 4 px below the
/// caption row. Design default; widget setters can
/// override later.
pub const BUTTON_VPAD: u32 = 8;

/// Visual state of a [`Button`]. Controls which fill
/// colour the button paints with when [`Button::draw`] is
/// called.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ButtonState {
    /// Resting: pointer is not over the button.
    #[default]
    Resting,
    /// Pointer is hovering over the button.
    Hover,
    /// Pointer is down on the button.
    Pressed,
}

/// A rectangular button with a fill, a 1-pixel border, a
/// centred caption, and an on-click callback.
pub struct Button {
    bounds: Rect,
    caption: Label,
    fill: Color,
    fill_hover: Color,
    fill_pressed: Color,
    border: Color,
    state: ButtonState,
    on_click: Option<Box<dyn FnMut()>>,
}

impl Button {
    /// Construct a button with the given bounds and
    /// caption. Colours default to the bundled light
    /// theme's `button_*` slots; the caption is centred
    /// and coloured with `Theme::LIGHT.button_text`;
    /// state is [`ButtonState::Resting`]; no click
    /// callback is installed.
    ///
    /// Callers that want different colours (e.g.
    /// `WindowFrame`'s close button using `close_button*`
    /// slots instead of the standalone `button_*` slots)
    /// call the `set_*` methods after construction.
    pub fn new(bounds: Rect, caption: impl Into<String>) -> Self {
        let mut label = Label::new(bounds, caption);
        label.set_alignment(Alignment::Center);
        label.set_color(Theme::LIGHT.button_text);
        Button {
            bounds,
            caption: label,
            fill: Theme::LIGHT.button_fill,
            fill_hover: Theme::LIGHT.button_fill_hover,
            fill_pressed: Theme::LIGHT.button_fill_pressed,
            border: Theme::LIGHT.button_border,
            state: ButtonState::Resting,
            on_click: None,
        }
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// Replace the button's bounds. The caption resizes via
    /// internal fit-to-width logic on the next draw.
    pub fn set_bounds(&mut self, bounds: Rect) {
        self.bounds = bounds;
    }

    pub fn caption(&self) -> &str {
        self.caption.text()
    }

    /// Minimum rect size that comfortably holds the
    /// button's current caption with standard padding.
    /// Advisory: the caller either feeds the result into a
    /// layout (e.g. [`crate::layout::Row::next`]) or
    /// ignores it. No coupling between widget and layout.
    ///
    /// Width = caption width + [`BUTTON_HPAD`]. Empty
    /// caption returns just the pad width so a button with
    /// no caption still paints a clickable rect.
    ///
    /// Height = [`GLYPH_HEIGHT`] + [`BUTTON_VPAD`].
    /// Independent of the caption.
    pub fn preferred_size(&self) -> (u32, u32) {
        (
            text_width_px(self.caption.text()) + BUTTON_HPAD,
            GLYPH_HEIGHT + BUTTON_VPAD,
        )
    }

    pub fn set_caption(&mut self, caption: impl Into<String>) {
        self.caption.set_text(caption);
    }

    pub fn state(&self) -> ButtonState {
        self.state
    }

    pub fn set_state(&mut self, state: ButtonState) {
        self.state = state;
    }

    pub fn set_fill(&mut self, color: Color) {
        self.fill = color;
    }

    pub fn set_fill_hover(&mut self, color: Color) {
        self.fill_hover = color;
    }

    pub fn set_fill_pressed(&mut self, color: Color) {
        self.fill_pressed = color;
    }

    pub fn set_border(&mut self, color: Color) {
        self.border = color;
    }

    pub fn set_caption_color(&mut self, color: Color) {
        self.caption.set_color(color);
    }

    /// Install a click callback. Fires exactly once per
    /// `pointer_down` that hits the button. The previous
    /// callback, if any, is replaced.
    pub fn on_click<F: FnMut() + 'static>(&mut self, callback: F) {
        self.on_click = Some(Box::new(callback));
    }

    /// True iff `(x, y)` falls inside the button's
    /// bounds. An empty-bounds button never hits.
    pub fn hit_test(&self, x: i32, y: i32) -> bool {
        if self.bounds.is_empty() {
            return false;
        }
        x >= self.bounds.x
            && x < self.bounds.right()
            && y >= self.bounds.y
            && y < self.bounds.bottom()
    }

    /// Handle a pointer-down event. Fires `on_click` if
    /// the press lands inside the button's bounds.
    /// Returns `true` iff the press was inside (so the
    /// caller can short-circuit any further routing). No
    /// state transition happens here — visual pressed
    /// state is tracked separately via [`Self::set_state`]
    /// for now.
    pub fn pointer_down(&mut self, x: i32, y: i32) -> bool {
        if !self.hit_test(x, y) {
            return false;
        }
        if let Some(cb) = self.on_click.as_mut() {
            cb();
        }
        true
    }

    fn current_fill(&self) -> Color {
        match self.state {
            ButtonState::Resting => self.fill,
            ButtonState::Hover => self.fill_hover,
            ButtonState::Pressed => self.fill_pressed,
        }
    }

    /// Paint the button into `canvas`. Fills the bounds,
    /// strokes a 1-pixel border, and draws the centred
    /// caption on top. No-op on empty bounds.
    pub fn draw(&self, canvas: &mut Canvas) {
        if self.bounds.is_empty() {
            return;
        }
        canvas.fill_rect(self.bounds, self.current_fill());
        canvas.stroke_rect(self.bounds, self.border);
        self.caption.draw(canvas);
    }
}
