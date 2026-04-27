//! T130 — desktop shell's taskbar.
//!
//! `Taskbar` owns the open-window list (one entry per known
//! toplevel) and routes pointer clicks to either focus the
//! window or restore it from a minimized state. The data
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
//! creation order with a small gap between them.
//!
//! Click routing: a press inside an entry returns
//! [`TaskbarClick`] from [`Taskbar::handle_pointer_down`];
//! the caller (typically the shell's main loop) decides what
//! to do with it. Two actions are surfaced:
//!
//! * `Focus { window_id }` — the entry was clicked while
//!   the window was already in the focused state, OR the
//!   window was unfocused; either way the shell sends
//!   `pmd_shell_manager.focus_window(window_id)` to bring
//!   it forward.
//! * `Restore { window_id }` — the entry was clicked while
//!   the window was minimized. The shell calls
//!   `Server::restore_toplevel(window_id)` (or, in a future
//!   slice, sends a `pmd_shell_manager.restore_window`
//!   request).

use core::fmt;

use display_proto::events::{
    ShellWindowCreated, ShellWindowDestroyed, ShellWindowFocused, ShellWindowTitleChanged,
};
use toolkit::draw::{Canvas, Color, Rect};
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

/// Horizontal gap between adjacent taskbar entries, in pixels.
pub const TASKBAR_ENTRY_GAP: u32 = 2;

/// Margin between the taskbar's left edge and the first
/// entry, in pixels.
pub const TASKBAR_LEFT_MARGIN: u32 = 4;

/// Margin between the entry's left edge and its label text,
/// in pixels.
pub const TASKBAR_ENTRY_TEXT_MARGIN: u32 = 6;

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
        }
    }

    /// Replace the colour palette. Defaults to
    /// `Theme::LIGHT`; the dark palette is `Theme::DARK`.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Update the framebuffer dimensions. Called when the
    /// display reports a new output size — the taskbar
    /// re-anchors to the bottom strip without losing entries.
    pub fn set_framebuffer_size(&mut self, width: u32, height: u32) {
        self.fb_width = width;
        self.fb_height = height;
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
    }

    /// Remove a window from the taskbar. No-op if no entry
    /// matches `window_id`.
    pub fn remove_window(&mut self, window_id: u32) {
        self.entries.retain(|e| e.window_id != window_id);
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
        for entry in self.entries.iter_mut() {
            entry.focused = entry.window_id == window_id;
        }
    }

    /// Mark `window_id` as minimized (`minimized = true`)
    /// or restored (`minimized = false`). Entries paint with
    /// a "minimized" palette when `minimized = true` so the
    /// user can distinguish them from visible windows.
    pub fn set_window_minimized(&mut self, window_id: u32, minimized: bool) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.window_id == window_id) {
            entry.minimized = minimized;
        }
    }

    /// Decode a `pmd_shell_manager.window_*` event payload
    /// and apply it to the model. The opcode disambiguates
    /// which event was sent (1=created, 2=destroyed,
    /// 3=focused, 4=title_changed) — same numbering as in
    /// `display_proto::objects::SHELL_MANAGER_EVENTS`.
    pub fn handle_event_bytes(
        &mut self,
        opcode: u16,
        payload: &[u8],
    ) -> Result<(), TaskbarError> {
        match opcode {
            1 => {
                let event = ShellWindowCreated::decode(payload)
                    .map_err(|_| TaskbarError::Malformed)?;
                self.add_window(event.window_id, event.title, event.app_id);
            }
            2 => {
                let event = ShellWindowDestroyed::decode(payload)
                    .map_err(|_| TaskbarError::Malformed)?;
                self.remove_window(event.window_id);
            }
            3 => {
                let event = ShellWindowFocused::decode(payload)
                    .map_err(|_| TaskbarError::Malformed)?;
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
        if idx >= self.entries.len() {
            return None;
        }
        let bounds = self.bounds();
        let pad = 2_i32; // top/bottom inset of each entry
        let entry_h = (TASKBAR_HEIGHT as i32 - pad * 2).max(1) as u32;
        let stride = (TASKBAR_ENTRY_WIDTH + TASKBAR_ENTRY_GAP) as i32;
        let x = bounds.x + (TASKBAR_LEFT_MARGIN as i32) + (idx as i32) * stride;
        let y = bounds.y + pad;
        Some(Rect::new(x, y, TASKBAR_ENTRY_WIDTH, entry_h))
    }

    /// Hit-test a screen-space point against the taskbar's
    /// entries. Returns the index of the entry containing
    /// `(x, y)`, or `None` if the point isn't inside any
    /// entry. Clicks inside the taskbar strip but outside
    /// any entry also return `None`.
    pub fn hit_test_entry(&self, x: i32, y: i32) -> Option<usize> {
        for idx in 0..self.entries.len() {
            let rect = self.entry_rect(idx)?;
            if x >= rect.x
                && x < rect.right()
                && y >= rect.y
                && y < rect.bottom()
            {
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
        let idx = self.hit_test_entry(x, y)?;
        let entry = &self.entries[idx];
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
        for idx in 0..self.entries.len() {
            let Some(rect) = self.entry_rect(idx) else {
                continue;
            };
            let entry = &self.entries[idx];
            let (fill, fg) = self.entry_palette(entry);
            canvas.fill_rect(rect, fill);
            canvas.stroke_rect(rect, self.theme.border_active);
            let label = entry.label();
            let text_x = rect.x + TASKBAR_ENTRY_TEXT_MARGIN as i32;
            let text_y =
                rect.y + ((rect.height as i32 - toolkit::draw::font::GLYPH_HEIGHT as i32) / 2).max(0);
            canvas.draw_text(text_x, text_y, label, fg);
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
}
