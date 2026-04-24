//! `List` — scrollable single-column list of text rows.
//!
//! Each row is a constant-height ([`LIST_ROW_HEIGHT`]) strip of text
//! positioned inside a caller-supplied bounds rect. The list owns the
//! item strings, a scroll offset (the index of the first visible row),
//! a selected index, and a hovered index. Paint and hit-test consume
//! the bounds rect supplied by the caller at call time rather than
//! storing it on the struct — this matches the [`super::container::Container`]
//! shape, keeps the list reusable inside nested layouts, and lets the
//! future [`crate::app::App`] hand a different rect on resize without
//! re-building the widget.
//!
//! Selected rows paint with [`crate::theme::Theme::button_fill_pressed`],
//! hovered (non-selected) rows with [`crate::theme::Theme::button_fill_hover`].
//! Row text uses [`crate::theme::Theme::label_text`]. These slots were
//! already in the theme; no new slots are introduced.
//!
//! Out-of-bounds selection via [`List::select`] is clamped to
//! `items.len() - 1` (saturating). When the items vec is empty, any
//! selection request is coerced to `None` — there is no row to point
//! at.
//!
//! Scroll model: [`List::handle_key`] advances `scroll_offset` only
//! when the selected row would otherwise leave the visible window,
//! so Up/Down always keep the selection in view. [`ListKey::PageUp`] /
//! [`ListKey::PageDown`] scroll by `visible_rows` without touching the
//! selection (the selected row may scroll off-screen; callers that want
//! page-move-and-select compose Up/Down afterwards).
//!
//! v1 has no scrollbar paint — the scroll position is conceptual and
//! consumed only by [`List::paint`] to pick which rows to draw. A
//! future slice may add a gutter scrollbar; the plumbing is already in
//! place.

use crate::draw::font::GLYPH_HEIGHT;
use crate::draw::{Canvas, Rect};
use crate::theme::Theme;

/// Fixed pixel height of every row in a [`List`].
pub const LIST_ROW_HEIGHT: u32 = 18;

/// Horizontal padding between the row's left edge and its text. Mirrors
/// [`super::text_input::TEXT_INPUT_PADDING_X`] conceptually but uses a
/// slightly wider value to keep list items visually distinct from a
/// focusable input.
pub const LIST_HPAD: u32 = 6;

/// Geometry derived from a bounds rect at paint time. Today only
/// exposed for callers that want to compute `visible_rows` before
/// issuing a key — e.g. a scroll bar sibling in a future slice.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ListDimensions {
    pub row_height: u32,
    pub visible_rows: u32,
}

impl ListDimensions {
    /// Derive from a paint bounds rect: every row is
    /// [`LIST_ROW_HEIGHT`] tall and `visible_rows` is the number of
    /// whole rows that fit inside `bounds.height`.
    pub fn from_bounds(bounds: Rect) -> Self {
        ListDimensions {
            row_height: LIST_ROW_HEIGHT,
            visible_rows: bounds.height / LIST_ROW_HEIGHT,
        }
    }
}

/// Keyboard vocabulary consumed by [`List::handle_key`]. Separate from
/// [`super::text_input::Key`] because the two widgets share no
/// handlers — List has arrow / Home / End / PageUp / PageDown / Enter
/// and never sees `Char` or `Backspace`, and the TextInput key enum
/// has no Up / Down / PageUp / PageDown / Enter. Keeping them apart
/// avoids "handle_key ignores half its enum" on both widgets. When
/// T114's `App` wires display-server keyboard events into widget
/// calls, both enums will either grow or be replaced by a shared
/// vocabulary.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ListKey {
    /// Move selection one row up; clamped at 0.
    Up,
    /// Move selection one row down; clamped at `items.len() - 1`.
    Down,
    /// Jump selection to the first row.
    Home,
    /// Jump selection to the last row.
    End,
    /// Scroll `scroll_offset` up by `visible_rows` without touching
    /// the selection.
    PageUp,
    /// Scroll `scroll_offset` down by `visible_rows` without touching
    /// the selection.
    PageDown,
    /// Activate the current selection (e.g. open a file, pick an
    /// option).
    Enter,
}

/// Outcome of a [`List::handle_key`] call.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ListKeyOutcome {
    /// Up / Down / Home / End moved the selection (and possibly
    /// scrolled to keep it in view).
    SelectionChanged,
    /// PageUp / PageDown scrolled without changing the selection.
    ScrolledOnly,
    /// Enter pressed on a selected row.
    Activated,
    /// The key did nothing — no selection to move, navigation
    /// already at a boundary, or Enter with no selection.
    Ignored,
}

/// Scrollable single-column list widget.
pub struct List {
    items: Vec<String>,
    /// Index of the first visible row. Must satisfy
    /// `scroll_offset <= items.len().saturating_sub(1)` when `items`
    /// is non-empty; always 0 when `items` is empty.
    scroll_offset: usize,
    /// Selected row index into `items`, or `None` when nothing is
    /// selected (or the list is empty).
    selected: Option<usize>,
    /// Hovered non-selected row index, driven by the pointer router.
    /// When `hover == selected`, the selection paint wins.
    hover: Option<usize>,
}

impl List {
    /// Construct an empty list: no items, no selection, no hover.
    pub fn new() -> Self {
        List {
            items: Vec::new(),
            scroll_offset: 0,
            selected: None,
            hover: None,
        }
    }

    /// Builder-style setter for the initial item set. Selection and
    /// scroll are reset to their empty-list defaults.
    pub fn with_items(mut self, items: Vec<String>) -> Self {
        self.set_items(items);
        self
    }

    /// Replace the current item set. Selection, hover, and scroll are
    /// all reset — callers that want to preserve selection across a
    /// refresh call [`Self::select`] after `set_items`.
    pub fn set_items(&mut self, items: Vec<String>) {
        self.items = items;
        self.scroll_offset = 0;
        self.selected = None;
        self.hover = None;
    }

    pub fn items(&self) -> &[String] {
        &self.items
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Set the selected row index. Out-of-bounds values are clamped to
    /// `items.len() - 1`; on an empty list any `Some` becomes `None`.
    pub fn select(&mut self, index: Option<usize>) {
        self.selected = match index {
            None => None,
            Some(_) if self.items.is_empty() => None,
            Some(i) => Some(i.min(self.items.len() - 1)),
        };
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// The currently selected item's text, or `None` if there is no
    /// selection.
    pub fn selected_item(&self) -> Option<&str> {
        self.selected.map(|i| self.items[i].as_str())
    }

    /// Set the hovered row. Same clamping rules as [`Self::select`].
    pub fn set_hover(&mut self, index: Option<usize>) {
        self.hover = match index {
            None => None,
            Some(_) if self.items.is_empty() => None,
            Some(i) => Some(i.min(self.items.len() - 1)),
        };
    }

    pub fn hover(&self) -> Option<usize> {
        self.hover
    }

    /// Map a pixel position to the item index under it, accounting for
    /// the current `scroll_offset` and the row height. Returns `None`
    /// when `point` is outside `bounds` or past the last row of data.
    pub fn row_at(&self, point: (u32, u32), bounds: Rect) -> Option<usize> {
        if bounds.is_empty() {
            return None;
        }
        let (px, py) = (point.0 as i32, point.1 as i32);
        if px < bounds.x || px >= bounds.right() || py < bounds.y || py >= bounds.bottom() {
            return None;
        }
        let row_in_view = ((py - bounds.y) as u32) / LIST_ROW_HEIGHT;
        let index = self.scroll_offset + row_in_view as usize;
        if index >= self.items.len() {
            return None;
        }
        Some(index)
    }

    /// Handle a pointer-down. If the press lands on a row, that row
    /// becomes the new selection and the call returns `true`. A miss
    /// leaves the selection unchanged and returns `false`.
    pub fn on_pointer_down(&mut self, point: (u32, u32), bounds: Rect) -> bool {
        match self.row_at(point, bounds) {
            Some(i) => {
                self.selected = Some(i);
                true
            }
            None => false,
        }
    }

    /// Apply a keystroke. `visible_rows` tells the list how many rows
    /// fit in the current viewport so it can keep a moved selection
    /// in view and so PageUp / PageDown know how far to scroll. When
    /// `visible_rows` is 0 the call still honours selection changes
    /// but skips the scroll-follow step.
    pub fn handle_key(&mut self, key: ListKey, visible_rows: u32) -> ListKeyOutcome {
        if self.items.is_empty() {
            return ListKeyOutcome::Ignored;
        }
        match key {
            ListKey::Up => {
                let current = self.selected.unwrap_or(0);
                if current == 0 {
                    return ListKeyOutcome::Ignored;
                }
                self.selected = Some(current - 1);
                self.follow_selection(visible_rows);
                ListKeyOutcome::SelectionChanged
            }
            ListKey::Down => {
                let current = self.selected.unwrap_or(0);
                let last = self.items.len() - 1;
                if current >= last {
                    return ListKeyOutcome::Ignored;
                }
                self.selected = Some(current + 1);
                self.follow_selection(visible_rows);
                ListKeyOutcome::SelectionChanged
            }
            ListKey::Home => {
                if self.selected == Some(0) {
                    return ListKeyOutcome::Ignored;
                }
                self.selected = Some(0);
                self.follow_selection(visible_rows);
                ListKeyOutcome::SelectionChanged
            }
            ListKey::End => {
                let last = self.items.len() - 1;
                if self.selected == Some(last) {
                    return ListKeyOutcome::Ignored;
                }
                self.selected = Some(last);
                self.follow_selection(visible_rows);
                ListKeyOutcome::SelectionChanged
            }
            ListKey::PageUp => {
                if self.scroll_offset == 0 {
                    return ListKeyOutcome::Ignored;
                }
                self.scroll_offset =
                    self.scroll_offset.saturating_sub(visible_rows as usize);
                ListKeyOutcome::ScrolledOnly
            }
            ListKey::PageDown => {
                let max_offset = self.max_scroll_offset(visible_rows);
                if self.scroll_offset >= max_offset {
                    return ListKeyOutcome::Ignored;
                }
                let next = self.scroll_offset + visible_rows as usize;
                self.scroll_offset = next.min(max_offset);
                ListKeyOutcome::ScrolledOnly
            }
            ListKey::Enter => {
                if self.selected.is_some() {
                    ListKeyOutcome::Activated
                } else {
                    ListKeyOutcome::Ignored
                }
            }
        }
    }

    /// Greatest legal `scroll_offset` for the given viewport: the
    /// smallest value that still keeps the viewport full (or, for
    /// short lists, zero). Saturates at zero when the list fits.
    fn max_scroll_offset(&self, visible_rows: u32) -> usize {
        let visible = visible_rows as usize;
        self.items.len().saturating_sub(visible)
    }

    /// After a selection change, adjust `scroll_offset` so the new
    /// selection is inside the visible window `[scroll_offset,
    /// scroll_offset + visible_rows)`. No-op when `visible_rows` is 0.
    fn follow_selection(&mut self, visible_rows: u32) {
        let Some(sel) = self.selected else {
            return;
        };
        if visible_rows == 0 {
            return;
        }
        let visible = visible_rows as usize;
        if sel < self.scroll_offset {
            self.scroll_offset = sel;
        } else if sel >= self.scroll_offset + visible {
            self.scroll_offset = sel + 1 - visible;
        }
    }

    /// Paint the visible slice of the list into `canvas`. Selected row
    /// fills with `theme.button_fill_pressed`, hover row with
    /// `theme.button_fill_hover`, every row's text with
    /// `theme.label_text`. No-op on empty bounds or when the bounds
    /// can't fit a single row.
    pub fn paint(&self, canvas: &mut Canvas<'_>, bounds: Rect, theme: &Theme) {
        if bounds.is_empty() || bounds.height < LIST_ROW_HEIGHT {
            return;
        }
        if self.items.is_empty() {
            return;
        }
        let dims = ListDimensions::from_bounds(bounds);
        let end = (self.scroll_offset + dims.visible_rows as usize).min(self.items.len());
        for i in self.scroll_offset..end {
            let row_idx = (i - self.scroll_offset) as u32;
            let row_y = bounds.y + (row_idx * LIST_ROW_HEIGHT) as i32;
            let row = Rect::new(bounds.x, row_y, bounds.width, LIST_ROW_HEIGHT);

            if Some(i) == self.selected {
                canvas.fill_rect(row, theme.button_fill_pressed);
            } else if Some(i) == self.hover {
                canvas.fill_rect(row, theme.button_fill_hover);
            }

            let text_x = row.x + LIST_HPAD as i32;
            let text_y = row.y + ((LIST_ROW_HEIGHT - GLYPH_HEIGHT) / 2) as i32;
            canvas.draw_text(text_x, text_y, &self.items[i], theme.label_text);
        }
    }
}

impl Default for List {
    fn default() -> Self {
        List::new()
    }
}
