//! A stateless, desktop-style tab strip.
//!
//! [`TabStrip`] owns only borrowed tab labels. The application remains the
//! source of truth for the selected tab and passes that selection into
//! [`TabStrip::paint`]. Pointer and keyboard helpers return an index for the
//! application to translate into its own content state.

use crate::draw::font::GLYPH_HEIGHT;
use crate::draw::text::{fit_text_to_width, text_width_px};
use crate::draw::{Canvas, Rect};
use crate::theme::Theme;

/// Default height used by PMos application tab strips.
pub const TAB_STRIP_HEIGHT: u32 = 24;

/// Height of the focused-colour accent at the top of the selected tab.
pub const TAB_STRIP_ACCENT_HEIGHT: u32 = 2;

/// Horizontal inset reserved on both sides of a tab caption.
pub const TAB_STRIP_TEXT_INSET: u32 = 6;

/// Keyboard navigation understood by a [`TabStrip`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TabKey {
    /// Select the following tab, wrapping after the last tab.
    Next,
    /// Select the preceding tab, wrapping before the first tab.
    Previous,
    /// Select the first tab.
    Home,
    /// Select the last tab.
    End,
}

/// Result of keyboard navigation through a [`TabStrip`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TabKeyOutcome {
    /// The application should select the contained tab index.
    SelectionChanged(usize),
    /// No tab exists, or the requested selection is already current.
    Ignored,
}

/// An equal-width row of tab labels with a caller-owned selection.
///
/// Any remainder after equal division is assigned to the final tab. This
/// preserves every pixel in the supplied bounds and keeps the first tab
/// boundaries stable as a strip resizes.
#[derive(Copy, Clone, Debug)]
pub struct TabStrip<'labels> {
    labels: &'labels [&'labels str],
}

impl<'labels> TabStrip<'labels> {
    /// Construct a tab strip borrowing `labels`.
    pub const fn new(labels: &'labels [&'labels str]) -> Self {
        Self { labels }
    }

    pub const fn labels(&self) -> &'labels [&'labels str] {
        self.labels
    }

    pub const fn len(&self) -> usize {
        self.labels.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }

    /// Return the portion of `bounds` occupied by `index`.
    pub fn tab_bounds(&self, index: usize, bounds: Rect) -> Option<Rect> {
        if index >= self.labels.len() || bounds.is_empty() {
            return None;
        }
        let count = u32::try_from(self.labels.len()).ok()?;
        let tab_width = bounds.width / count;
        let x_offset = tab_width.saturating_mul(u32::try_from(index).ok()?);
        let width = if index + 1 == self.labels.len() {
            bounds.width.saturating_sub(x_offset)
        } else {
            tab_width
        };
        Some(Rect::new(
            bounds.x.saturating_add(x_offset as i32),
            bounds.y,
            width,
            bounds.height,
        ))
    }

    /// Map a pointer position to a tab index.
    ///
    /// Right and bottom edges are exclusive. Positions outside the complete
    /// strip never clamp to the first or final tab.
    pub fn tab_at(&self, point: (i32, i32), bounds: Rect) -> Option<usize> {
        if !contains(bounds, point) {
            return None;
        }
        (0..self.labels.len()).find(|index| {
            self.tab_bounds(*index, bounds)
                .is_some_and(|tab| contains(tab, point))
        })
    }

    /// Pointer-down convenience wrapper around [`Self::tab_at`].
    pub fn on_pointer_down(&self, point: (i32, i32), bounds: Rect) -> Option<usize> {
        self.tab_at(point, bounds)
    }

    /// Resolve keyboard navigation without retaining selection state.
    ///
    /// A missing or out-of-range selection is treated as no selection: Next,
    /// Home, and End choose a deterministic edge; Previous chooses the last
    /// tab. Next and Previous wrap. Home and End report [`TabKeyOutcome::Ignored`]
    /// when the requested edge is already selected.
    pub fn handle_key(&self, selected: Option<usize>, key: TabKey) -> TabKeyOutcome {
        let count = self.labels.len();
        if count == 0 {
            return TabKeyOutcome::Ignored;
        }
        let current = selected.filter(|index| *index < count);
        let next = match key {
            TabKey::Next => current.map_or(0, |index| (index + 1) % count),
            TabKey::Previous => current.map_or(count - 1, |index| (index + count - 1) % count),
            TabKey::Home => 0,
            TabKey::End => count - 1,
        };
        if current == Some(next) {
            TabKeyOutcome::Ignored
        } else {
            TabKeyOutcome::SelectionChanged(next)
        }
    }

    /// Paint the strip with existing [`Theme`] palette slots.
    ///
    /// Inactive tabs use the ordinary button surface. The selected tab uses
    /// the window surface, a focused-colour top accent, and an open bottom edge
    /// so it reads as connected to the content pane below.
    pub fn paint(
        &self,
        canvas: &mut Canvas<'_>,
        bounds: Rect,
        selected: Option<usize>,
        theme: &Theme,
    ) {
        if bounds.is_empty() {
            return;
        }
        canvas.fill_rect(bounds, theme.titlebar_inactive);

        for (index, label) in self.labels.iter().enumerate() {
            let Some(tab) = self.tab_bounds(index, bounds) else {
                continue;
            };
            if tab.is_empty() {
                continue;
            }
            let active = selected == Some(index);
            canvas.fill_rect(
                tab,
                if active {
                    theme.window_background
                } else {
                    theme.button_fill
                },
            );
            canvas.stroke_rect(tab, theme.button_border);

            if active {
                let accent_height = TAB_STRIP_ACCENT_HEIGHT.min(tab.height);
                canvas.fill_rect(
                    Rect::new(tab.x, tab.y, tab.width, accent_height),
                    theme.border_active,
                );
                canvas.fill_rect(
                    Rect::new(tab.x, tab.bottom() - 1, tab.width, 1),
                    theme.window_background,
                );
            }

            paint_caption(canvas, tab, label, active, theme);
        }
    }
}

fn contains(bounds: Rect, point: (i32, i32)) -> bool {
    !bounds.is_empty()
        && point.0 >= bounds.x
        && point.0 < bounds.right()
        && point.1 >= bounds.y
        && point.1 < bounds.bottom()
}

fn paint_caption(canvas: &mut Canvas<'_>, tab: Rect, label: &str, active: bool, theme: &Theme) {
    if tab.height < GLYPH_HEIGHT {
        return;
    }
    let text_area_width = tab
        .width
        .saturating_sub(TAB_STRIP_TEXT_INSET.saturating_mul(2));
    let visible = fit_text_to_width(label, text_area_width);
    if visible.is_empty() {
        return;
    }
    let text_width = text_width_px(visible);
    let text_x = tab.x + (tab.width.saturating_sub(text_width) / 2) as i32;
    let text_y = tab.y + ((tab.height - GLYPH_HEIGHT) / 2) as i32;
    canvas.draw_text(
        text_x,
        text_y,
        visible,
        if active {
            theme.label_text
        } else {
            theme.button_text
        },
    );
}
