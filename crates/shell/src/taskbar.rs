//! T130 — desktop shell's taskbar.
//!
//! `Taskbar` owns the open-window list (one entry per known
//! toplevel) and routes pointer clicks to focus, restore,
//! minimize, maximize/restore, or close the exact server-global window. The data
//! model is driven by the `pmd_shell_manager.window_*` event
//! stream emitted by the display server (spec §15) — the
//! shell binds `pmd_shell_manager` after `App::connect`,
//! sends `subscribe_windows`, and feeds each inbound event
//! through one of the `handle_*` methods on this struct.
//!
//! The visible strip is a fixed-height row anchored to the
//! bottom of the framebuffer. Each entry is a labeled box
//! showing the window's app_id (with the title as a fallback
//! when app_id is empty); the focused entry paints with a
//! highlight palette so the user can see which window has
//! keyboard focus. Entries are layed out left-to-right in
//! creation order with a small gap between them. A bounded page
//! of entries is shown when the row is full; the overflow button
//! cycles through the remaining pages without shrinking controls
//! into overlapping one-pixel slivers.
//!
//! Click routing: a press inside an entry returns
//! [`TaskbarClick`] from [`Taskbar::handle_pointer_down`];
//! the caller (typically the shell's main loop) decides what
//! to do with it. Window identity remains the opaque global id
//! supplied by the display server.
//!
//! * `Focus { window_id }` — the entry was clicked while
//!   the window was already in the focused state, OR the
//!   window was unfocused; either way the shell sends
//!   `pmd_shell_manager.focus_window(window_id)` to bring
//!   it forward.
//!
//! The main label area focuses/restores; the `_`, `[]`, and `x` controls
//! minimize, toggle maximize/restore, and request a graceful close.

use core::fmt;

use display_proto::events::{
    ShellWindowCreated, ShellWindowDestroyed, ShellWindowFocused, ShellWindowTitleChanged,
};
use toolkit::draw::{text_width_px, Canvas, Color, Rect};
use toolkit::theme::Theme;

/// Default height of the taskbar in pixels. Exposed as a
/// constant so the shell can pass it to
/// `Server::set_taskbar_height_px` to reserve the strip
/// from the work area used for `set_maximized` configures.
pub const TASKBAR_HEIGHT: u32 = 32;

/// Default width of a single taskbar entry, in pixels. The
/// taskbar v1 lays out entries left-to-right with a fixed
/// width per entry; the title text is clipped if it doesn't
/// fit.
pub const TASKBAR_ENTRY_WIDTH: u32 = 160;

/// Smallest task entry width. Keeping a real lower bound leaves enough room
/// for a clipped label plus three independently hit-testable controls.
pub const TASKBAR_MIN_ENTRY_WIDTH: u32 = 112;

/// Horizontal gap between adjacent taskbar entries, in pixels.
pub const TASKBAR_ENTRY_GAP: u32 = 2;

/// Margin between the taskbar's left edge and the first
/// entry, in pixels.
pub const TASKBAR_LEFT_MARGIN: u32 = 4;

/// Horizontal space occupied by the launcher button plus its
/// trailing gap. Window entries begin after this reservation.
pub const TASKBAR_LAUNCHER_RESERVED_WIDTH: u32 = 86;

/// Right-edge reservation for the shell-owned wall clock.
pub const TASKBAR_CLOCK_RESERVED_WIDTH: u32 = 68;

/// Gap between the clock text and the right framebuffer edge.
pub const TASKBAR_CLOCK_RIGHT_MARGIN: u32 = 6;

/// Margin between the entry's left edge and its label text,
/// in pixels.
pub const TASKBAR_ENTRY_TEXT_MARGIN: u32 = 6;

/// Width of each minimize/maximize/close control inside a task entry.
pub const TASKBAR_ENTRY_CONTROL_WIDTH: u32 = 20;

/// Gap between the label and controls and between adjacent controls.
pub const TASKBAR_ENTRY_CONTROL_GAP: u32 = 2;

/// Width reserved for the page-cycling overflow control.
pub const TASKBAR_OVERFLOW_WIDTH: u32 = 42;

const TASKBAR_RIGHT_MARGIN: u32 = 4;

/// What the shell should do in response to a pointer-down
/// inside a taskbar entry. The taskbar doesn't issue
/// protocol requests itself — that's the caller's job — it
/// just classifies the click.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TaskbarClick {
    /// The clicked window is currently visible. The shell
    /// should send `pmd_shell_manager.focus_window(window_id)`
    /// to bring it to the front.
    Focus { window_id: u32 },
    /// The clicked window is currently minimized. The shell
    /// should call `Server::restore_toplevel(window_id)` (or
    /// a future `pmd_shell_manager.restore_window` request)
    /// to re-map it.
    Restore { window_id: u32 },
    /// Explicit minimize control.
    Minimize { window_id: u32 },
    /// Toggle between maximized work-area geometry and restored geometry.
    ToggleMaximize { window_id: u32 },
    /// Explicit graceful-close control.
    Close { window_id: u32 },
    /// Cycle to the next bounded page of task entries.
    CycleOverflow,
}

/// One entry in the taskbar's open-window list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskbarEntry {
    pub window_id: u32,
    pub title: String,
    pub app_id: String,
    pub focused: bool,
    pub minimized: bool,
}

impl TaskbarEntry {
    /// The label the entry paints in the taskbar — `app_id`
    /// when set, falling back to `title` when not. Returns
    /// `"(untitled)"` if both are empty so the entry never
    /// paints with zero visible text.
    pub fn label(&self) -> &str {
        if !self.app_id.is_empty() {
            &self.app_id
        } else if !self.title.is_empty() {
            &self.title
        } else {
            "(untitled)"
        }
    }
}

/// The taskbar widget. Anchored to the bottom of the
/// framebuffer with [`TASKBAR_HEIGHT`] pixels of vertical
/// reservation; lays out entries left-to-right.
pub struct Taskbar {
    /// Width of the framebuffer the taskbar lives in. Used
    /// to compute the bottom-anchored bounds at paint time.
    fb_width: u32,
    /// Height of the framebuffer.
    fb_height: u32,
    /// Open-window list, in creation order. The taskbar
    /// doesn't reorder entries on focus changes — only
    /// `focused` flips on the existing entries.
    entries: Vec<TaskbarEntry>,
    /// Theme palette used for paint. Defaults to the
    /// bundled light theme; callers can swap via
    /// [`Taskbar::set_theme`].
    theme: Theme,
    /// Preformatted local time (for example `14:05 UTC`).
    clock_text: String,
    /// First entry on the current bounded overflow page.
    page_start: usize,
}

/// Errors returned by [`Taskbar::handle_event_bytes`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TaskbarError {
    /// The event payload didn't decode against the expected
    /// `pmd_shell_manager.window_*` event shape.
    Malformed,
    /// The opcode wasn't one of the four shell_manager
    /// events the taskbar handles (1-4 in spec §15).
    UnknownOpcode { opcode: u16 },
}

impl fmt::Display for TaskbarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskbarError::Malformed => write!(f, "malformed shell_manager event payload"),
            TaskbarError::UnknownOpcode { opcode } => {
                write!(f, "unknown shell_manager opcode {opcode}")
            }
        }
    }
}

impl Taskbar {
    /// Build a taskbar sized for a `(fb_width, fb_height)`
    /// framebuffer with the bundled light theme.
    pub fn new(fb_width: u32, fb_height: u32) -> Self {
        Taskbar {
            fb_width,
            fb_height,
            entries: Vec::new(),
            theme: Theme::LIGHT,
            clock_text: String::new(),
            page_start: 0,
        }
    }

    /// Replace the colour palette. Defaults to
    /// `Theme::LIGHT`; the dark palette is `Theme::DARK`.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Set the right-aligned wall-clock label. Returns whether the visible
    /// value changed so callers can avoid unnecessary desktop repaints.
    pub fn set_clock_text(&mut self, text: impl Into<String>) -> bool {
        let text = text.into();
        if text == self.clock_text {
            return false;
        }
        self.clock_text = text;
        true
    }

    pub fn clock_text(&self) -> &str {
        &self.clock_text
    }

    /// Update the framebuffer dimensions. Called when the
    /// display reports a new output size — the taskbar
    /// re-anchors to the bottom strip without losing entries.
    pub fn set_framebuffer_size(&mut self, width: u32, height: u32) {
        self.fb_width = width;
        self.fb_height = height;
        self.clamp_page();
    }

    /// Read-only view of the entry list, in creation order.
    pub fn entries(&self) -> &[TaskbarEntry] {
        &self.entries
    }

    /// Add a window to the taskbar. Idempotent: if a window
    /// with `window_id` already exists, the title + app_id
    /// are updated in-place and the focus + minimized flags
    /// are preserved.
    pub fn add_window(
        &mut self,
        window_id: u32,
        title: impl Into<String>,
        app_id: impl Into<String>,
    ) {
        let title = title.into();
        let app_id = app_id.into();
        if let Some(existing) = self.entries.iter_mut().find(|e| e.window_id == window_id) {
            existing.title = title;
            existing.app_id = app_id;
            return;
        }
        self.entries.push(TaskbarEntry {
            window_id,
            title,
            app_id,
            focused: false,
            minimized: false,
        });
        self.clamp_page();
    }

    /// Remove a window from the taskbar. No-op if no entry
    /// matches `window_id`.
    pub fn remove_window(&mut self, window_id: u32) {
        self.entries.retain(|e| e.window_id != window_id);
        self.clamp_page();
    }

    /// Update the title of an existing entry. No-op if no
    /// entry matches.
    pub fn set_window_title(&mut self, window_id: u32, new_title: impl Into<String>) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.window_id == window_id) {
            entry.title = new_title.into();
        }
    }

    /// Mark `window_id` as the currently-focused window;
    /// every other entry has its focused flag cleared. v1
    /// taskbars surface focus via a highlight palette on the
    /// focused entry.
    pub fn set_focused_window(&mut self, window_id: u32) {
        let mut focused_idx = None;
        for (idx, entry) in self.entries.iter_mut().enumerate() {
            entry.focused = entry.window_id == window_id;
            if entry.focused {
                entry.minimized = false;
                focused_idx = Some(idx);
            }
        }
        if let Some(idx) = focused_idx {
            self.ensure_visible(idx);
        }
    }

    /// Mark `window_id` as minimized (`minimized = true`)
    /// or restored (`minimized = false`). Entries paint with
    /// a "minimized" palette when `minimized = true` so the
    /// user can distinguish them from visible windows.
    pub fn set_window_minimized(&mut self, window_id: u32, minimized: bool) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.window_id == window_id) {
            entry.minimized = minimized;
            if minimized {
                entry.focused = false;
            }
        }
    }

    /// Decode a `pmd_shell_manager.window_*` event payload
    /// and apply it to the model. The opcode disambiguates
    /// which event was sent (1=created, 2=destroyed,
    /// 3=focused, 4=title_changed) — same numbering as in
    /// `display_proto::objects::SHELL_MANAGER_EVENTS`.
    pub fn handle_event_bytes(&mut self, opcode: u16, payload: &[u8]) -> Result<(), TaskbarError> {
        match opcode {
            1 => {
                let event =
                    ShellWindowCreated::decode(payload).map_err(|_| TaskbarError::Malformed)?;
                self.add_window(event.window_id, event.title, event.app_id);
            }
            2 => {
                let event =
                    ShellWindowDestroyed::decode(payload).map_err(|_| TaskbarError::Malformed)?;
                self.remove_window(event.window_id);
            }
            3 => {
                let event =
                    ShellWindowFocused::decode(payload).map_err(|_| TaskbarError::Malformed)?;
                self.set_focused_window(event.window_id);
            }
            4 => {
                let event = ShellWindowTitleChanged::decode(payload)
                    .map_err(|_| TaskbarError::Malformed)?;
                self.set_window_title(event.window_id, event.new_title);
            }
            other => return Err(TaskbarError::UnknownOpcode { opcode: other }),
        }
        Ok(())
    }

    /// Bottom-anchored bounds of the taskbar strip in
    /// framebuffer space.
    pub fn bounds(&self) -> Rect {
        Rect::new(
            0,
            (self.fb_height as i32).saturating_sub(TASKBAR_HEIGHT as i32),
            self.fb_width,
            TASKBAR_HEIGHT.min(self.fb_height),
        )
    }

    /// Bounds of entry `idx` in framebuffer space, or `None`
    /// if `idx` is out of range.
    pub fn entry_rect(&self, idx: usize) -> Option<Rect> {
        let visible = self.visible_range();
        if !visible.contains(&idx) {
            return None;
        }
        let bounds = self.bounds();
        let pad = 2_i32; // top/bottom inset of each entry
        let entry_h = (TASKBAR_HEIGHT as i32 - pad * 2).max(1) as u32;
        let entry_width = self.fitted_entry_width();
        let stride = (entry_width + TASKBAR_ENTRY_GAP) as i32;
        let visible_idx = idx - visible.start;
        let x = bounds.x
            + (TASKBAR_LEFT_MARGIN + TASKBAR_LAUNCHER_RESERVED_WIDTH) as i32
            + (visible_idx as i32) * stride;
        let y = bounds.y + pad;
        Some(Rect::new(x, y, entry_width, entry_h))
    }

    /// Hit-test a screen-space point against the taskbar's
    /// entries. Returns the index of the entry containing
    /// `(x, y)`, or `None` if the point isn't inside any
    /// entry. Clicks inside the taskbar strip but outside
    /// any entry also return `None`.
    pub fn hit_test_entry(&self, x: i32, y: i32) -> Option<usize> {
        for idx in self.visible_range() {
            let rect = self.entry_rect(idx)?;
            if x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom() {
                return Some(idx);
            }
        }
        None
    }

    /// True iff `(x, y)` falls inside the taskbar strip
    /// (including the gaps between entries). Used by the
    /// shell's pointer-routing path to decide whether to
    /// route a click to the taskbar before falling through
    /// to a window.
    pub fn contains_point(&self, x: i32, y: i32) -> bool {
        let b = self.bounds();
        x >= b.x && x < b.right() && y >= b.y && y < b.bottom()
    }

    /// Route a pointer-down event through the taskbar.
    /// Returns the click classification, or `None` if the
    /// click missed every entry.
    pub fn handle_pointer_down(&self, x: i32, y: i32) -> Option<TaskbarClick> {
        if self
            .overflow_rect()
            .is_some_and(|rect| rect_contains(rect, x, y))
        {
            return Some(TaskbarClick::CycleOverflow);
        }
        let idx = self.hit_test_entry(x, y)?;
        let entry = &self.entries[idx];
        if self
            .close_rect(idx)
            .is_some_and(|rect| rect_contains(rect, x, y))
        {
            return Some(TaskbarClick::Close {
                window_id: entry.window_id,
            });
        }
        if self
            .minimize_rect(idx)
            .is_some_and(|rect| rect_contains(rect, x, y))
        {
            return Some(TaskbarClick::Minimize {
                window_id: entry.window_id,
            });
        }
        if self
            .maximize_rect(idx)
            .is_some_and(|rect| rect_contains(rect, x, y))
        {
            return Some(TaskbarClick::ToggleMaximize {
                window_id: entry.window_id,
            });
        }
        if entry.minimized {
            Some(TaskbarClick::Restore {
                window_id: entry.window_id,
            })
        } else {
            Some(TaskbarClick::Focus {
                window_id: entry.window_id,
            })
        }
    }

    /// Paint the taskbar into `canvas`. Background fill +
    /// each entry as a labeled box; the focused entry uses
    /// a highlight palette and minimized entries use a
    /// dimmed palette.
    pub fn draw(&self, canvas: &mut Canvas<'_>) {
        let bounds = self.bounds();
        if bounds.is_empty() {
            return;
        }
        // Taskbar uses the titlebar palette so its strip
        // stands out against the (lighter) wallpaper.
        canvas.fill_rect(bounds, self.theme.titlebar_active);
        canvas.stroke_rect(bounds, self.theme.border_active);
        for idx in self.visible_range() {
            let Some(rect) = self.entry_rect(idx) else {
                continue;
            };
            let entry = &self.entries[idx];
            let (fill, fg) = self.entry_palette(entry);
            canvas.fill_rect(rect, fill);
            canvas.stroke_rect(rect, self.theme.border_active);
            let minimize = self.minimize_rect(idx).expect("visible entry control");
            let maximize = self.maximize_rect(idx).expect("visible entry control");
            let close = self.close_rect(idx).expect("visible entry control");
            let label_width = minimize
                .x
                .saturating_sub(rect.x)
                .saturating_sub((TASKBAR_ENTRY_TEXT_MARGIN + TASKBAR_ENTRY_CONTROL_GAP) as i32)
                .max(0) as u32;
            let label = fitted_label(entry.label(), label_width);
            let text_x = rect.x + TASKBAR_ENTRY_TEXT_MARGIN as i32;
            let text_y = rect.y
                + ((rect.height as i32 - toolkit::draw::font::GLYPH_HEIGHT as i32) / 2).max(0);
            canvas.draw_text(text_x, text_y, &label, fg);

            canvas.fill_rect(minimize, self.theme.button_fill);
            canvas.stroke_rect(minimize, self.theme.button_border);
            canvas.draw_text(minimize.x + 6, text_y, "_", self.theme.button_text);

            canvas.fill_rect(maximize, self.theme.button_fill);
            canvas.stroke_rect(maximize, self.theme.button_border);
            canvas.draw_text(maximize.x + 2, text_y, "[]", self.theme.button_text);

            canvas.fill_rect(close, self.theme.close_button);
            canvas.stroke_rect(close, self.theme.button_border);
            canvas.draw_text(close.x + 6, text_y, "x", self.theme.close_button_glyph);
        }
        if let Some(overflow) = self.overflow_rect() {
            canvas.fill_rect(overflow, self.theme.button_fill);
            canvas.stroke_rect(overflow, self.theme.button_border);
            let capacity = self.visible_capacity().max(1);
            let pages = self.entries.len().div_ceil(capacity);
            let page = self.visible_range().start / capacity + 1;
            let label = format!("{page}/{pages}");
            let text_y = overflow.y
                + ((overflow.height as i32 - toolkit::draw::font::GLYPH_HEIGHT as i32) / 2).max(0);
            canvas.draw_text(overflow.x + 5, text_y, &label, self.theme.button_text);
        }
        if !self.clock_text.is_empty() {
            let text_width = text_width_px(&self.clock_text);
            let text_x = bounds
                .right()
                .saturating_sub(TASKBAR_CLOCK_RIGHT_MARGIN as i32)
                .saturating_sub(text_width as i32);
            let text_y = bounds.y
                + ((bounds.height as i32 - toolkit::draw::font::GLYPH_HEIGHT as i32) / 2).max(0);
            canvas.draw_text(
                text_x,
                text_y,
                &self.clock_text,
                self.theme.titlebar_text_active,
            );
        }
    }

    /// Pick the (fill, fg) colour pair for an entry based
    /// on its focused / minimized state.
    fn entry_palette(&self, entry: &TaskbarEntry) -> (Color, Color) {
        if entry.minimized {
            (self.theme.button_fill, self.theme.button_text)
        } else if entry.focused {
            (self.theme.button_fill_pressed, self.theme.button_text)
        } else {
            (self.theme.button_fill_hover, self.theme.button_text)
        }
    }

    /// Width available to each visible open-window entry.
    fn fitted_entry_width(&self) -> u32 {
        let count = self.visible_capacity().min(self.entries.len()) as u32;
        if count == 0 {
            return TASKBAR_ENTRY_WIDTH;
        }
        let available = self.entries_width(self.has_overflow());
        let gaps = TASKBAR_ENTRY_GAP.saturating_mul(count.saturating_sub(1));
        TASKBAR_ENTRY_WIDTH
            .min(available.saturating_sub(gaps) / count)
            .max(TASKBAR_MIN_ENTRY_WIDTH.min(available))
    }

    /// Number of entries that have full, non-overlapping controls on one page.
    pub fn visible_capacity(&self) -> usize {
        if self.entries.is_empty() {
            return 0;
        }
        let without_overflow = capacity_for_width(self.entries_width(false));
        if self.entries.len() <= without_overflow {
            return self.entries.len();
        }
        capacity_for_width(self.entries_width(true))
    }

    pub fn visible_range(&self) -> core::ops::Range<usize> {
        let capacity = self.visible_capacity();
        if capacity == 0 {
            return 0..0;
        }
        let max_start = (self.entries.len().saturating_sub(1) / capacity) * capacity;
        let start = self.page_start.min(max_start);
        start..(start + capacity).min(self.entries.len())
    }

    pub fn has_overflow(&self) -> bool {
        self.visible_capacity() < self.entries.len()
    }

    pub fn overflow_rect(&self) -> Option<Rect> {
        if !self.has_overflow() {
            return None;
        }
        let bar = self.bounds();
        let pad = 2_i32;
        let right = self
            .fb_width
            .saturating_sub(TASKBAR_CLOCK_RESERVED_WIDTH + TASKBAR_RIGHT_MARGIN);
        Some(Rect::new(
            right.saturating_sub(TASKBAR_OVERFLOW_WIDTH) as i32,
            bar.y + pad,
            TASKBAR_OVERFLOW_WIDTH,
            (TASKBAR_HEIGHT as i32 - pad * 2).max(1) as u32,
        ))
    }

    pub fn minimize_rect(&self, idx: usize) -> Option<Rect> {
        let entry = self.entry_rect(idx)?;
        let maximize = self.maximize_rect(idx)?;
        Some(Rect::new(
            maximize.x - TASKBAR_ENTRY_CONTROL_GAP as i32 - TASKBAR_ENTRY_CONTROL_WIDTH as i32,
            entry.y,
            TASKBAR_ENTRY_CONTROL_WIDTH,
            entry.height,
        ))
    }

    pub fn maximize_rect(&self, idx: usize) -> Option<Rect> {
        let entry = self.entry_rect(idx)?;
        let close = self.close_rect(idx)?;
        Some(Rect::new(
            close.x - TASKBAR_ENTRY_CONTROL_GAP as i32 - TASKBAR_ENTRY_CONTROL_WIDTH as i32,
            entry.y,
            TASKBAR_ENTRY_CONTROL_WIDTH,
            entry.height,
        ))
    }

    pub fn close_rect(&self, idx: usize) -> Option<Rect> {
        let entry = self.entry_rect(idx)?;
        Some(Rect::new(
            entry.right() - TASKBAR_ENTRY_CONTROL_WIDTH as i32,
            entry.y,
            TASKBAR_ENTRY_CONTROL_WIDTH,
            entry.height,
        ))
    }

    /// Advance one overflow page, wrapping to the first page.
    pub fn cycle_overflow(&mut self) {
        let capacity = self.visible_capacity();
        if capacity == 0 || !self.has_overflow() {
            self.page_start = 0;
            return;
        }
        let next = self.visible_range().start.saturating_add(capacity);
        self.page_start = if next >= self.entries.len() { 0 } else { next };
    }

    fn entries_width(&self, reserve_overflow: bool) -> u32 {
        let base = self
            .fb_width
            .saturating_sub(TASKBAR_LEFT_MARGIN + TASKBAR_LAUNCHER_RESERVED_WIDTH)
            .saturating_sub(TASKBAR_CLOCK_RESERVED_WIDTH + TASKBAR_RIGHT_MARGIN);
        if reserve_overflow {
            base.saturating_sub(TASKBAR_OVERFLOW_WIDTH + TASKBAR_ENTRY_GAP)
        } else {
            base
        }
    }

    fn clamp_page(&mut self) {
        let capacity = self.visible_capacity();
        if capacity == 0 {
            self.page_start = 0;
            return;
        }
        let max_start = (self.entries.len().saturating_sub(1) / capacity) * capacity;
        self.page_start = self.page_start.min(max_start);
    }

    fn ensure_visible(&mut self, idx: usize) {
        let capacity = self.visible_capacity();
        if capacity > 0 && !self.visible_range().contains(&idx) {
            self.page_start = (idx / capacity) * capacity;
        }
    }
}

fn capacity_for_width(width: u32) -> usize {
    ((width + TASKBAR_ENTRY_GAP) / (TASKBAR_MIN_ENTRY_WIDTH + TASKBAR_ENTRY_GAP)) as usize
}

fn rect_contains(rect: Rect, x: i32, y: i32) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

fn fitted_label(label: &str, max_width: u32) -> String {
    let mut fitted = String::new();
    for ch in label.chars() {
        let next_width = text_width_px(&format!("{fitted}{ch}"));
        if next_width > max_width {
            break;
        }
        fitted.push(ch);
    }
    fitted
}
