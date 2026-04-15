//! `WindowFrame` — chrome drawn around a top-level window's
//! content area.
//!
//! A `WindowFrame` is a pure client-side widget: it takes a
//! bounds rectangle, a theme, and an app-id, and paints a
//! 1-pixel border + a titlebar + a close button into any
//! [`Canvas`] the caller hands it. It has no protocol
//! affinity — it doesn't know a `pmd_xdg_toplevel` from a
//! hole in the ground — so it can be composed with the term
//! rasterizer long before [`crate::app::App`] (T114) exists
//! to wire it to real window events.
//!
//! Event wiring is shallow by design: the close callback is
//! a closure supplied at construction time, and
//! [`WindowFrame::pointer_down`] is the only input
//! entry-point. When [`crate::app::App::connect`] lands
//! (T114), an `App` will call `pointer_down` for every
//! pointer event that lands on this frame's bounds and
//! translate `PointerOutcome::Close` into a real
//! `xdg_toplevel.close` request — but that wiring does
//! **not** live in this slice.

use crate::draw::font::GLYPH_HEIGHT;
use crate::draw::text::fit_text_to_width;
use crate::draw::{Canvas, Color, Rect};
use crate::theme::Theme;

/// Height of the titlebar in pixels, including the 1-pixel
/// top window border. A content area of
/// `(bounds.height - TITLEBAR_HEIGHT - BORDER_WIDTH)` pixels
/// sits below.
pub const TITLEBAR_HEIGHT: u32 = 22;

/// Width of the window border, in pixels. Kept as a named
/// constant so geometry calculations don't sprinkle `1` all
/// over the place — if a future slice decides to thicken
/// the border for a particular theme, one edit lands it.
pub const BORDER_WIDTH: u32 = 1;

/// Close-button side length. The button is a square.
pub const CLOSE_BUTTON_SIZE: u32 = 16;

/// Margin between the close button and the right / top
/// edges of the window, in pixels.
pub const CLOSE_BUTTON_MARGIN: u32 = 3;

/// Horizontal inset of the titlebar title text from the
/// left window edge, in pixels.
pub const TITLE_TEXT_MARGIN_X: u32 = 6;

/// Gap between the end of the title text and the left edge
/// of the close button, in pixels. Used when clipping long
/// app-ids.
pub const TITLE_TEXT_TRAILING_GAP: u32 = 4;

/// Outcome of a pointer-down event routed through
/// [`WindowFrame::pointer_down`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PointerOutcome {
    /// The press landed on the close button. The frame's
    /// close callback (if any) has already fired by the
    /// time this variant is returned.
    Close,
    /// The press landed on the titlebar outside the close
    /// button. A future `App::connect` will turn this into
    /// an interactive move request.
    Titlebar,
    /// The press landed on the content area — the region
    /// inside the border and below the titlebar. Apps route
    /// this to their own widgets.
    Content,
    /// The press was outside the frame entirely. Returned
    /// when `pointer_down` is called with coordinates that
    /// don't intersect `bounds()`.
    Outside,
}

/// Chrome widget drawn around a window's content area.
///
/// See the module doc-comment for design notes. Construct
/// with [`WindowFrame::new`], set focus state with
/// [`WindowFrame::set_focused`], attach a close callback
/// with [`WindowFrame::on_close`], then call
/// [`WindowFrame::draw`] once per frame and route pointer
/// events through [`WindowFrame::pointer_down`].
pub struct WindowFrame {
    bounds: Rect,
    app_id: String,
    focused: bool,
    close_hover: bool,
    theme: Theme,
    close_callback: Option<Box<dyn FnMut()>>,
}

impl WindowFrame {
    /// Construct a frame with the given bounds and app-id.
    /// The frame starts focused (so the active-theme
    /// colours are in effect) and has no close callback.
    pub fn new(bounds: Rect, app_id: impl Into<String>) -> Self {
        WindowFrame {
            bounds,
            app_id: app_id.into(),
            focused: true,
            close_hover: false,
            theme: Theme::default(),
            close_callback: None,
        }
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// Switch between active (focused) and inactive chrome
    /// colours. The display server's input router will
    /// drive this when window focus changes; tests flip it
    /// directly.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn is_close_hover(&self) -> bool {
        self.close_hover
    }

    /// Mark the close button as hovered (true) or not
    /// (false). A hovered close button paints with
    /// [`Theme::close_button_hover`] instead of
    /// [`Theme::close_button`].
    pub fn set_close_hover(&mut self, hovered: bool) {
        self.close_hover = hovered;
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Install a close callback. Called exactly once per
    /// `pointer_down` that hits the close button. The
    /// previous callback (if any) is replaced.
    pub fn on_close<F: FnMut() + 'static>(&mut self, callback: F) {
        self.close_callback = Some(Box::new(callback));
    }

    /// The titlebar rectangle, including the top 1-pixel
    /// border row. Empty if the window bounds can't fit a
    /// titlebar at all.
    pub fn titlebar_rect(&self) -> Rect {
        if self.bounds.width == 0 || self.bounds.height == 0 {
            return Rect::new(self.bounds.x, self.bounds.y, 0, 0);
        }
        let h = TITLEBAR_HEIGHT.min(self.bounds.height);
        Rect::new(self.bounds.x, self.bounds.y, self.bounds.width, h)
    }

    /// The close-button rectangle. Empty if the window is
    /// too small to hold it.
    pub fn close_button_rect(&self) -> Rect {
        if self.bounds.width < CLOSE_BUTTON_SIZE + 2 * CLOSE_BUTTON_MARGIN
            || self.bounds.height < TITLEBAR_HEIGHT
        {
            return Rect::new(self.bounds.x, self.bounds.y, 0, 0);
        }
        let x = self.bounds.x
            + (self.bounds.width as i32)
            - (CLOSE_BUTTON_MARGIN as i32)
            - (CLOSE_BUTTON_SIZE as i32);
        let y = self.bounds.y + CLOSE_BUTTON_MARGIN as i32;
        Rect::new(x, y, CLOSE_BUTTON_SIZE, CLOSE_BUTTON_SIZE)
    }

    /// The content rectangle — the area inside the border
    /// and below the titlebar that the app should paint
    /// into. Empty if the window can't spare any rows.
    pub fn content_rect(&self) -> Rect {
        if self.bounds.width < 2 * BORDER_WIDTH
            || self.bounds.height < TITLEBAR_HEIGHT + BORDER_WIDTH
        {
            return Rect::new(self.bounds.x, self.bounds.y, 0, 0);
        }
        let x = self.bounds.x + BORDER_WIDTH as i32;
        let y = self.bounds.y + TITLEBAR_HEIGHT as i32;
        let w = self.bounds.width - 2 * BORDER_WIDTH;
        let h = self.bounds.height - TITLEBAR_HEIGHT - BORDER_WIDTH;
        Rect::new(x, y, w, h)
    }

    /// Pixel width available to the title text between the
    /// title's left margin and the close button (or the
    /// right edge, when the window is too small for a
    /// close button). Returns `0` if the window is too
    /// narrow to fit any title text at all.
    fn title_text_budget_px(&self) -> u32 {
        let text_start_x = self.bounds.x + TITLE_TEXT_MARGIN_X as i32;
        let close = self.close_button_rect();
        let text_end_limit = if close.is_empty() {
            self.bounds.right() - TITLE_TEXT_MARGIN_X as i32
        } else {
            close.x - TITLE_TEXT_TRAILING_GAP as i32
        };
        if text_end_limit <= text_start_x {
            return 0;
        }
        (text_end_limit - text_start_x) as u32
    }

    /// Leading slice of the app-id that actually gets
    /// drawn. The rest is clipped by the close button.
    pub fn visible_title(&self) -> &str {
        fit_text_to_width(&self.app_id, self.title_text_budget_px())
    }

    /// Number of characters of the app-id that will be
    /// drawn. Thin wrapper over [`Self::visible_title`]
    /// kept for callers that only care about the count.
    pub fn visible_title_chars(&self) -> usize {
        self.visible_title().chars().count()
    }

    /// True iff `(x, y)` falls inside the close button.
    pub fn hit_test_close(&self, x: i32, y: i32) -> bool {
        let cb = self.close_button_rect();
        if cb.is_empty() {
            return false;
        }
        x >= cb.x && x < cb.right() && y >= cb.y && y < cb.bottom()
    }

    /// True iff `(x, y)` falls inside the titlebar
    /// (regardless of whether the close button claims it).
    pub fn hit_test_titlebar(&self, x: i32, y: i32) -> bool {
        let tb = self.titlebar_rect();
        if tb.is_empty() {
            return false;
        }
        x >= tb.x && x < tb.right() && y >= tb.y && y < tb.bottom()
    }

    /// True iff `(x, y)` falls inside the window's overall
    /// bounds.
    pub fn hit_test_window(&self, x: i32, y: i32) -> bool {
        let b = self.bounds;
        if b.is_empty() {
            return false;
        }
        x >= b.x && x < b.right() && y >= b.y && y < b.bottom()
    }

    /// Route a pointer-down event through the frame. Fires
    /// the close callback if the press lands on the close
    /// button. Returns a [`PointerOutcome`] describing
    /// where the press landed so the caller can route
    /// content-area presses into their own widgets.
    pub fn pointer_down(&mut self, x: i32, y: i32) -> PointerOutcome {
        if !self.hit_test_window(x, y) {
            return PointerOutcome::Outside;
        }
        if self.hit_test_close(x, y) {
            if let Some(callback) = self.close_callback.as_mut() {
                callback();
            }
            return PointerOutcome::Close;
        }
        if self.hit_test_titlebar(x, y) {
            return PointerOutcome::Titlebar;
        }
        PointerOutcome::Content
    }

    fn border_color(&self) -> Color {
        if self.focused {
            self.theme.border_active
        } else {
            self.theme.border_inactive
        }
    }

    fn titlebar_fill_color(&self) -> Color {
        if self.focused {
            self.theme.titlebar_active
        } else {
            self.theme.titlebar_inactive
        }
    }

    fn title_text_color(&self) -> Color {
        if self.focused {
            self.theme.titlebar_text_active
        } else {
            self.theme.titlebar_text_inactive
        }
    }

    fn close_button_fill_color(&self) -> Color {
        if self.close_hover {
            self.theme.close_button_hover
        } else {
            self.theme.close_button
        }
    }

    /// Paint the frame's chrome into `canvas`. Does not
    /// touch the content rectangle — that's the app's
    /// responsibility.
    pub fn draw(&self, canvas: &mut Canvas) {
        if self.bounds.is_empty() {
            return;
        }

        let titlebar = self.titlebar_rect();
        if !titlebar.is_empty() {
            let fill = Rect::new(
                titlebar.x + BORDER_WIDTH as i32,
                titlebar.y + BORDER_WIDTH as i32,
                titlebar.width.saturating_sub(2 * BORDER_WIDTH),
                titlebar.height.saturating_sub(BORDER_WIDTH),
            );
            canvas.fill_rect(fill, self.titlebar_fill_color());
        }

        let text = self.visible_title();
        if !text.is_empty() && self.bounds.height >= GLYPH_HEIGHT + 2 * BORDER_WIDTH {
            let text_x = self.bounds.x + TITLE_TEXT_MARGIN_X as i32;
            let titlebar_interior_h = titlebar.height.saturating_sub(BORDER_WIDTH);
            let text_y = self.bounds.y
                + BORDER_WIDTH as i32
                + ((titlebar_interior_h.saturating_sub(GLYPH_HEIGHT)) / 2) as i32;
            canvas.draw_text(text_x, text_y, text, self.title_text_color());
        }

        let close = self.close_button_rect();
        if !close.is_empty() {
            canvas.fill_rect(close, self.close_button_fill_color());
            canvas.stroke_rect(close, self.border_color());
            let glyph_x = close.x + ((CLOSE_BUTTON_SIZE - crate::draw::font::GLYPH_WIDTH) / 2) as i32;
            let glyph_y = close.y + ((CLOSE_BUTTON_SIZE - GLYPH_HEIGHT) / 2) as i32;
            canvas.draw_text(glyph_x, glyph_y, "x", self.theme.close_button_glyph);
        }

        canvas.stroke_rect(self.bounds, self.border_color());
    }
}
