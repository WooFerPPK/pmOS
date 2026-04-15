//! Sequential layout within a parent rect.
//!
//! Two structs, [`Row`] and [`Column`], provide the
//! minimum viable placement API: "put N child widgets in a
//! row (or column) inside this parent rect, with
//! consistent edge padding and inter-child spacing."
//!
//! No flex weights, no grow/shrink, no grid, no min/max
//! sizing. Callers decide how big each child is; the
//! layout decides where each child goes. Vertical
//! centring for [`Row`] and horizontal centring for
//! [`Column`] is automatic, so a caller can mix
//! differently-sized children in the same run and they
//! all centre on the cross axis.
//!
//! The design is deliberately "cursor + next()". State —
//! where the next child should go — is awkward to thread
//! through free-function calls, so a small mutable struct
//! owns it. Callers alternate between
//! [`Row::remaining`] / [`Column::remaining`] to decide
//! whether to place a child and [`Row::next`] /
//! [`Column::next`] to actually claim a rect. Overflow
//! does not panic: `next()` returns a clipped rect (or an
//! empty rect if nothing fits), and the cursor keeps
//! advancing as if the requested size had been honoured
//! so that subsequent calls also report zero remaining
//! rather than silently packing into the "gap" the
//! previous clipped child left behind.

use crate::draw::Rect;

/// Place children from left to right inside a parent rect.
/// The parent is inset by `padding` on every edge to form
/// the **interior**; children are placed from the
/// interior's left edge, separated by `spacing` pixels,
/// and vertically centred within the interior's height
/// independently per-call (so children of different
/// heights all centre around the same horizontal midline).
pub struct Row {
    /// Absolute x-coordinate of the interior's left edge
    /// on the target canvas.
    interior_x: i32,
    /// Absolute y-coordinate of the interior's top edge.
    interior_y: i32,
    /// Interior width (parent width minus 2 × padding, or
    /// 0 if padding is too large).
    interior_width: u32,
    /// Interior height (parent height minus 2 × padding).
    interior_height: u32,
    /// Inter-child gap in pixels. Not applied before the
    /// first child or after the last.
    spacing: u32,
    /// Cursor offset from the interior's left edge, in
    /// pixels. Starts at 0; each `next()` advances by the
    /// requested child width + spacing.
    cursor_x: u32,
}

impl Row {
    /// Construct a row layout with the given parent rect,
    /// symmetric edge padding, and inter-child spacing.
    pub fn new(parent: Rect, padding: u32, spacing: u32) -> Self {
        let interior_x = parent.x + padding as i32;
        let interior_y = parent.y + padding as i32;
        let interior_width = parent.width.saturating_sub(2 * padding);
        let interior_height = parent.height.saturating_sub(2 * padding);
        Row {
            interior_x,
            interior_y,
            interior_width,
            interior_height,
            spacing,
            cursor_x: 0,
        }
    }

    /// Interior width in pixels. The sum of every
    /// child's width plus the spacing between them must
    /// fit within this for a clean layout.
    pub fn interior_width(&self) -> u32 {
        self.interior_width
    }

    /// Interior height in pixels.
    pub fn interior_height(&self) -> u32 {
        self.interior_height
    }

    /// Pixels of interior width still available to the
    /// right of the cursor. Callers that don't want
    /// clipping check this before calling `next()`.
    pub fn remaining(&self) -> u32 {
        self.interior_width.saturating_sub(self.cursor_x)
    }

    /// Claim the next child rect. Returns a rect at
    /// `(cursor, vertically-centred y)` with the requested
    /// size, clipped so its right edge doesn't cross the
    /// interior's right edge. Advances the cursor by
    /// `width + spacing` regardless of clipping so a
    /// caller that keeps asking for too-wide children
    /// sees zero-width rects instead of surprising
    /// second-chance placement.
    pub fn next(&mut self, width: u32, height: u32) -> Rect {
        let child_x = self.interior_x + self.cursor_x as i32;
        let y_offset = self.interior_height.saturating_sub(height) / 2;
        let child_y = self.interior_y + y_offset as i32;

        let clipped_w = width.min(self.remaining());
        let clipped_h = height.min(self.interior_height);

        self.cursor_x = self
            .cursor_x
            .saturating_add(width)
            .saturating_add(self.spacing);

        Rect::new(child_x, child_y, clipped_w, clipped_h)
    }
}

/// Place children from top to bottom inside a parent rect.
/// Mirror of [`Row`] — children are horizontally centred
/// within the interior's width, independently per-call.
pub struct Column {
    interior_x: i32,
    interior_y: i32,
    interior_width: u32,
    interior_height: u32,
    spacing: u32,
    cursor_y: u32,
}

impl Column {
    pub fn new(parent: Rect, padding: u32, spacing: u32) -> Self {
        let interior_x = parent.x + padding as i32;
        let interior_y = parent.y + padding as i32;
        let interior_width = parent.width.saturating_sub(2 * padding);
        let interior_height = parent.height.saturating_sub(2 * padding);
        Column {
            interior_x,
            interior_y,
            interior_width,
            interior_height,
            spacing,
            cursor_y: 0,
        }
    }

    pub fn interior_width(&self) -> u32 {
        self.interior_width
    }

    pub fn interior_height(&self) -> u32 {
        self.interior_height
    }

    /// Pixels of interior height still available below the
    /// cursor.
    pub fn remaining(&self) -> u32 {
        self.interior_height.saturating_sub(self.cursor_y)
    }

    pub fn next(&mut self, width: u32, height: u32) -> Rect {
        let x_offset = self.interior_width.saturating_sub(width) / 2;
        let child_x = self.interior_x + x_offset as i32;
        let child_y = self.interior_y + self.cursor_y as i32;

        let clipped_w = width.min(self.interior_width);
        let clipped_h = height.min(self.remaining());

        self.cursor_y = self
            .cursor_y
            .saturating_add(height)
            .saturating_add(self.spacing);

        Rect::new(child_x, child_y, clipped_w, clipped_h)
    }
}
