//! Client-side top-level window chrome.
//!
//! `WindowFrame` is deliberately protocol-free: it paints a PMos titlebar,
//! border, app mark, and the familiar minimize / maximize / close caption
//! controls into a caller-owned [`Canvas`]. Applications translate the
//! returned [`PointerOutcome`] through their own [`crate::Window`], preserving
//! the display protocol as the only route to the compositor.

use crate::draw::font::GLYPH_HEIGHT;
use crate::draw::text::fit_text_to_width;
use crate::draw::{Canvas, Color, Rect};
use crate::theme::Theme;

/// Existing v1 chrome footprint. Keeping this stable avoids moving every
/// application's content and preserves the display-server work-area contract.
pub const TITLEBAR_HEIGHT: u32 = 22;
pub const BORDER_WIDTH: u32 = 1;
pub const RESIZE_HIT_MARGIN: u32 = 4;

/// Width of each Windows-style caption control.
pub const WINDOW_CONTROL_WIDTH: u32 = 30;

/// Compatibility aliases retained for callers that previously reasoned about
/// the single square close button. The caption buttons now occupy the full
/// titlebar interior and therefore have no outer margin.
pub const CLOSE_BUTTON_SIZE: u32 = WINDOW_CONTROL_WIDTH;
pub const CLOSE_BUTTON_MARGIN: u32 = 0;

/// The PMos four-pane mark occupies the leading titlebar slot; text begins
/// after it rather than flush against the window edge.
pub const TITLE_TEXT_MARGIN_X: u32 = 22;
pub const TITLE_TEXT_TRAILING_GAP: u32 = 4;
const WINDOW_MARK_SIZE: u32 = 9;
const WINDOW_MARK_X: u32 = 6;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WindowControl {
    Minimize,
    Maximize,
    Close,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PointerOutcome {
    Minimize,
    ToggleMaximize,
    Close,
    Titlebar,
    Content,
    Outside,
}

pub struct WindowFrame {
    bounds: Rect,
    app_id: String,
    focused: bool,
    maximized: bool,
    theme: Theme,
    hovered_control: Option<WindowControl>,
    on_minimize: Option<Box<dyn FnMut()>>,
    on_toggle_maximize: Option<Box<dyn FnMut()>>,
    on_close: Option<Box<dyn FnMut()>>,
}

impl WindowFrame {
    pub fn new(bounds: Rect, app_id: impl Into<String>) -> Self {
        Self {
            bounds,
            app_id: app_id.into(),
            focused: true,
            maximized: false,
            theme: Theme::default(),
            hovered_control: None,
            on_minimize: None,
            on_toggle_maximize: None,
            on_close: None,
        }
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    pub fn set_app_id(&mut self, app_id: impl Into<String>) {
        self.app_id = app_id.into();
    }

    pub fn set_bounds(&mut self, bounds: Rect) {
        if self.bounds != bounds {
            self.hovered_control = None;
        }
        self.bounds = bounds;
    }

    pub fn pointer_up(&mut self, x: i32, y: i32) {
        self.hovered_control = self.control_at(x, y);
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn is_maximized(&self) -> bool {
        self.maximized
    }

    pub fn set_maximized(&mut self, maximized: bool) {
        self.maximized = maximized;
    }

    pub fn hovered_control(&self) -> Option<WindowControl> {
        self.hovered_control
    }

    /// Update caption-control hover from a surface-local pointer coordinate.
    /// Returns true only when the visible state changed.
    pub fn update_hover(&mut self, x: i32, y: i32) -> bool {
        let next = self.control_at(x, y);
        if next == self.hovered_control {
            return false;
        }
        self.hovered_control = next;
        true
    }

    pub fn clear_hover(&mut self) -> bool {
        if self.hovered_control.take().is_some() {
            return true;
        }
        false
    }

    pub fn is_close_hover(&self) -> bool {
        self.hovered_control == Some(WindowControl::Close)
    }

    /// Compatibility setter used by existing callers and isolation tests.
    pub fn set_close_hover(&mut self, hovered: bool) {
        self.hovered_control = hovered.then_some(WindowControl::Close);
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    pub fn on_minimize<F: FnMut() + 'static>(&mut self, callback: F) {
        self.on_minimize = Some(Box::new(callback));
    }

    pub fn on_toggle_maximize<F: FnMut() + 'static>(&mut self, callback: F) {
        self.on_toggle_maximize = Some(Box::new(callback));
    }

    pub fn on_close<F: FnMut() + 'static>(&mut self, callback: F) {
        self.on_close = Some(Box::new(callback));
    }

    pub fn titlebar_rect(&self) -> Rect {
        if self.bounds.is_empty() {
            return Rect::new(self.bounds.x, self.bounds.y, 0, 0);
        }
        Rect::new(
            self.bounds.x,
            self.bounds.y,
            self.bounds.width,
            TITLEBAR_HEIGHT.min(self.bounds.height),
        )
    }

    fn caption_rect_from_right(&self, position: u32) -> Rect {
        let titlebar = self.titlebar_rect();
        let height = titlebar.height.saturating_sub(BORDER_WIDTH);
        let required = position
            .saturating_add(1)
            .saturating_mul(WINDOW_CONTROL_WIDTH)
            .saturating_add(2 * BORDER_WIDTH);
        if height == 0 || titlebar.width < required {
            return Rect::new(titlebar.right(), titlebar.y, 0, 0);
        }
        Rect::new(
            titlebar.right() - BORDER_WIDTH as i32 - ((position + 1) * WINDOW_CONTROL_WIDTH) as i32,
            titlebar.y + BORDER_WIDTH as i32,
            WINDOW_CONTROL_WIDTH,
            height,
        )
    }

    pub fn close_button_rect(&self) -> Rect {
        self.caption_rect_from_right(0)
    }

    pub fn maximize_button_rect(&self) -> Rect {
        // Preserve a useful title drag target on very narrow windows.
        if self.bounds.width < 2 * WINDOW_CONTROL_WIDTH + 48 {
            return Rect::new(self.bounds.right(), self.bounds.y, 0, 0);
        }
        self.caption_rect_from_right(1)
    }

    pub fn minimize_button_rect(&self) -> Rect {
        if self.bounds.width < 3 * WINDOW_CONTROL_WIDTH + 48 {
            return Rect::new(self.bounds.right(), self.bounds.y, 0, 0);
        }
        self.caption_rect_from_right(2)
    }

    pub fn control_rect(&self, control: WindowControl) -> Rect {
        match control {
            WindowControl::Minimize => self.minimize_button_rect(),
            WindowControl::Maximize => self.maximize_button_rect(),
            WindowControl::Close => self.close_button_rect(),
        }
    }

    pub fn content_rect(&self) -> Rect {
        if self.bounds.width < 2 * BORDER_WIDTH
            || self.bounds.height < TITLEBAR_HEIGHT + BORDER_WIDTH
        {
            return Rect::new(self.bounds.x, self.bounds.y, 0, 0);
        }
        Rect::new(
            self.bounds.x + BORDER_WIDTH as i32,
            self.bounds.y + TITLEBAR_HEIGHT as i32,
            self.bounds.width - 2 * BORDER_WIDTH,
            self.bounds.height - TITLEBAR_HEIGHT - BORDER_WIDTH,
        )
    }

    /// Non-overlapping regions whose pixels can change when only the frame's
    /// focused state changes. The titlebar is one filled region; the remaining
    /// three rectangles cover the side and bottom borders without touching
    /// application content.
    ///
    /// This is a paint-only description. Protocol-aware code may tile these
    /// rectangles to its own transport limit.
    pub fn focus_damage_regions(&self) -> Vec<Rect> {
        let titlebar = self.titlebar_rect();
        if titlebar.is_empty() {
            return Vec::new();
        }

        let mut regions = vec![titlebar];
        if self.bounds.height <= titlebar.height {
            return regions;
        }

        let bottom_y = self.bounds.bottom() - 1;
        let sides_y = titlebar.bottom();
        let sides_height = bottom_y.saturating_sub(sides_y) as u32;
        if sides_height > 0 {
            regions.push(Rect::new(self.bounds.x, sides_y, 1, sides_height));
            if self.bounds.width > 1 {
                regions.push(Rect::new(self.bounds.right() - 1, sides_y, 1, sides_height));
            }
        }
        regions.push(Rect::new(self.bounds.x, bottom_y, self.bounds.width, 1));
        regions
    }

    /// Rasterize one frame-owned region into a densely packed RGBA buffer.
    /// Returns `None` unless the rectangle is non-empty and wholly contained
    /// by this frame's bounds.
    ///
    /// The temporary frame is translated so the returned pixels are exactly
    /// the same crop that [`Self::draw`] would place in a full window canvas.
    pub fn rasterize_region(&self, region: Rect) -> Option<Vec<u8>> {
        if region.is_empty()
            || region.x < self.bounds.x
            || region.y < self.bounds.y
            || region.right() > self.bounds.right()
            || region.bottom() > self.bounds.bottom()
        {
            return None;
        }

        let mut canvas = Canvas::new(region.width, region.height);
        let mut translated = WindowFrame::new(
            Rect::new(
                self.bounds.x - region.x,
                self.bounds.y - region.y,
                self.bounds.width,
                self.bounds.height,
            ),
            self.app_id.clone(),
        );
        translated.focused = self.focused;
        translated.maximized = self.maximized;
        translated.theme = self.theme;
        translated.hovered_control = self.hovered_control;
        translated.draw(&mut canvas);
        Some(canvas.into_pixels())
    }

    /// Copy only immutable paint state for a deferred chrome update. Pointer
    /// callbacks stay with the live widget and are never retained by a patch.
    pub(crate) fn paint_snapshot(&self) -> Self {
        Self {
            bounds: self.bounds,
            app_id: self.app_id.clone(),
            focused: self.focused,
            maximized: self.maximized,
            theme: self.theme,
            hovered_control: self.hovered_control,
            on_minimize: None,
            on_toggle_maximize: None,
            on_close: None,
        }
    }

    fn first_caption_x(&self) -> i32 {
        [
            self.minimize_button_rect(),
            self.maximize_button_rect(),
            self.close_button_rect(),
        ]
        .into_iter()
        .find(|rect| !rect.is_empty())
        .map(|rect| rect.x)
        .unwrap_or(self.bounds.right() - BORDER_WIDTH as i32)
    }

    fn title_text_budget_px(&self) -> u32 {
        let text_start_x = self.bounds.x + TITLE_TEXT_MARGIN_X as i32;
        let text_end = self.first_caption_x() - TITLE_TEXT_TRAILING_GAP as i32;
        text_end.saturating_sub(text_start_x).max(0) as u32
    }

    pub fn visible_title(&self) -> &str {
        fit_text_to_width(&self.app_id, self.title_text_budget_px())
    }

    pub fn visible_title_chars(&self) -> usize {
        self.visible_title().chars().count()
    }

    pub fn hit_test_close(&self, x: i32, y: i32) -> bool {
        rect_contains(self.close_button_rect(), x, y)
    }

    pub fn hit_test_maximize(&self, x: i32, y: i32) -> bool {
        rect_contains(self.maximize_button_rect(), x, y)
    }

    pub fn hit_test_minimize(&self, x: i32, y: i32) -> bool {
        rect_contains(self.minimize_button_rect(), x, y)
    }

    pub fn control_at(&self, x: i32, y: i32) -> Option<WindowControl> {
        if self.hit_test_close(x, y) {
            Some(WindowControl::Close)
        } else if self.hit_test_maximize(x, y) {
            Some(WindowControl::Maximize)
        } else if self.hit_test_minimize(x, y) {
            Some(WindowControl::Minimize)
        } else {
            None
        }
    }

    pub fn hit_test_titlebar(&self, x: i32, y: i32) -> bool {
        rect_contains(self.titlebar_rect(), x, y)
    }

    pub fn hit_test_window(&self, x: i32, y: i32) -> bool {
        rect_contains(self.bounds, x, y)
    }

    pub fn pointer_down(&mut self, x: i32, y: i32) -> PointerOutcome {
        if !self.hit_test_window(x, y) {
            return PointerOutcome::Outside;
        }
        match self.control_at(x, y) {
            Some(WindowControl::Close) => {
                if let Some(callback) = self.on_close.as_mut() {
                    callback();
                }
                PointerOutcome::Close
            }
            Some(WindowControl::Maximize) => {
                if let Some(callback) = self.on_toggle_maximize.as_mut() {
                    callback();
                }
                PointerOutcome::ToggleMaximize
            }
            Some(WindowControl::Minimize) => {
                if let Some(callback) = self.on_minimize.as_mut() {
                    callback();
                }
                PointerOutcome::Minimize
            }
            None if self.hit_test_titlebar(x, y) => PointerOutcome::Titlebar,
            None => PointerOutcome::Content,
        }
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

    pub fn draw(&self, canvas: &mut Canvas<'_>) {
        if self.bounds.is_empty() {
            return;
        }
        let titlebar = self.titlebar_rect();
        let fill = Rect::new(
            titlebar.x + BORDER_WIDTH as i32,
            titlebar.y + BORDER_WIDTH as i32,
            titlebar.width.saturating_sub(2 * BORDER_WIDTH),
            titlebar.height.saturating_sub(BORDER_WIDTH),
        );
        canvas.fill_rect(fill, self.titlebar_fill_color());

        self.draw_mark(canvas);
        let text = self.visible_title();
        if !text.is_empty() && titlebar.height >= GLYPH_HEIGHT + BORDER_WIDTH {
            let text_y = titlebar.y
                + BORDER_WIDTH as i32
                + ((titlebar.height - BORDER_WIDTH).saturating_sub(GLYPH_HEIGHT) / 2) as i32;
            canvas.draw_text(
                self.bounds.x + TITLE_TEXT_MARGIN_X as i32,
                text_y,
                text,
                self.title_text_color(),
            );
        }

        for control in [
            WindowControl::Minimize,
            WindowControl::Maximize,
            WindowControl::Close,
        ] {
            self.draw_control(canvas, control);
        }
        canvas.stroke_rect(self.bounds, self.border_color());
    }

    fn draw_mark(&self, canvas: &mut Canvas<'_>) {
        if self.bounds.width < TITLE_TEXT_MARGIN_X || self.titlebar_rect().height < WINDOW_MARK_SIZE
        {
            return;
        }
        let x = self.bounds.x + WINDOW_MARK_X as i32;
        // Caption controls win on narrow windows. Do not paint the leading app
        // mark underneath the close target; the first width where the two
        // regions merely meet is safe because Rect right edges are exclusive.
        if x + WINDOW_MARK_SIZE as i32 > self.first_caption_x() {
            return;
        }
        let y = self.bounds.y + ((TITLEBAR_HEIGHT - WINDOW_MARK_SIZE) / 2) as i32;
        let color = if self.focused {
            self.theme.border_active
        } else {
            self.theme.titlebar_text_inactive
        };
        for (dx, dy) in [(0, 0), (5, 0), (0, 5), (5, 5)] {
            canvas.fill_rect(Rect::new(x + dx, y + dy, 4, 4), color);
        }
    }

    fn draw_control(&self, canvas: &mut Canvas<'_>, control: WindowControl) {
        let rect = self.control_rect(control);
        if rect.is_empty() {
            return;
        }
        let hovered = self.hovered_control == Some(control);
        if hovered {
            let fill = if control == WindowControl::Close {
                self.theme.close_button_hover
            } else {
                self.theme.button_fill_hover
            };
            canvas.fill_rect(rect, fill);
        }
        let glyph = if hovered && control == WindowControl::Close {
            Color::rgb(0xff, 0xff, 0xff)
        } else {
            self.title_text_color()
        };
        let cx = rect.x + rect.width as i32 / 2;
        let cy = rect.y + rect.height as i32 / 2;
        match control {
            WindowControl::Minimize => {
                canvas.fill_rect(Rect::new(cx - 4, cy + 3, 9, 1), glyph);
            }
            WindowControl::Maximize if self.maximized => {
                canvas.stroke_rect(Rect::new(cx - 3, cy - 4, 7, 7), glyph);
                canvas.stroke_rect(Rect::new(cx - 5, cy - 2, 7, 7), glyph);
            }
            WindowControl::Maximize => {
                canvas.stroke_rect(Rect::new(cx - 4, cy - 4, 9, 8), glyph);
            }
            WindowControl::Close => {
                for offset in 0..=7 {
                    canvas.set_pixel(cx - 4 + offset, cy - 4 + offset, glyph);
                    canvas.set_pixel(cx + 3 - offset, cy - 4 + offset, glyph);
                }
            }
        }
    }
}

fn rect_contains(rect: Rect, x: i32, y: i32) -> bool {
    !rect.is_empty() && x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}
