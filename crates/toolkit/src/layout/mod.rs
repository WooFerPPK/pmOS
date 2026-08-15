//! Sequential layout within a parent rect.
//!
//! Two structs, [`Row`] and [`Column`], provide the
//! minimum viable placement API: "put N child widgets in a
//! row (or column) inside this parent rect, with
//! consistent edge padding and inter-child spacing."
//!
//! The cursor + next() API handles fixed-size children. For
//! flex-weighted (grow) children use the batch functions
//! [`row_with_grow`] and [`col_with_grow`], which accept a
//! slice of [`Item`] values and return a `Vec<Rect>` in one
//! call. Fixed items take their stated size; Grow items
//! split the leftover space proportionally by weight.
//! The two APIs can coexist — a caller can use `Row` for a
//! non-grow strip and `row_with_grow` when weights are needed.
//!
//! No min/max clamps on Grow items (v1 — callers pre-compute
//! fixed sizes if they need a lower bound). No shrink (only
//! Grow). No nested grids. Cross-axis centring is automatic,
//! identical to the cursor API.
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

/// A child descriptor for the batch grow-layout functions
/// [`row_with_grow`] and [`col_with_grow`].
///
/// `Fixed(w)` — the child gets exactly `w` pixels on the
/// main axis (same as calling `Row::next(w, ...)` in the
/// cursor API, subject to the same overflow clipping).
///
/// `Grow(weight)` — the child claims a share of the
/// leftover space proportional to `weight`. A weight of 0
/// is treated as 1 (every Grow child gets at least an equal
/// share of whatever space remains). If there is no leftover
/// space all Grow children receive a zero-size rect.
///
/// No `min_width`/`min_height` field in v1; callers that
/// need a lower bound should use `Fixed` with a pre-computed
/// size instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// Fixed size on the main axis (pixels).
    Fixed(u32),
    /// Flex weight — shares leftover main-axis space with other
    /// Grow items proportionally.
    Grow(u32),
}

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

// ---- Batch grow-layout helpers ------------------------------------

/// Lay out `items` left-to-right inside `parent`, applying `padding`
/// and `spacing`.  Each child's height on the cross axis is `height`;
/// children are vertically centred within the interior, identical to
/// the [`Row`] cursor API.
///
/// Returns one [`Rect`] per item in the same order as `items`.
///
/// Fixed items are placed at their stated widths (clipped to
/// remaining space on overflow, same as the cursor API). Grow items
/// split whatever main-axis space is left over after all Fixed items
/// and all inter-child gaps are accounted for.
///
/// Spacing gaps are inserted between *all* adjacent items (Fixed or
/// Grow) — N items produce N-1 gaps, identical to the cursor API.
///
/// All arithmetic is saturating so zero-leftover and
/// zero-interior parents produce zero-size rects without panicking.
pub fn row_with_grow(
    parent: Rect,
    padding: u32,
    spacing: u32,
    items: &[Item],
    height: u32,
) -> Vec<Rect> {
    if items.is_empty() {
        return Vec::new();
    }
    let interior_x = parent.x + padding as i32;
    let interior_y = parent.y + padding as i32;
    let interior_w = parent.width.saturating_sub(2 * padding);
    let interior_h = parent.height.saturating_sub(2 * padding);

    let n = items.len() as u32;
    // Total spacing consumed between N children (N-1 gaps).
    let total_spacing = spacing.saturating_mul(n.saturating_sub(1));
    // Space available after all fixed widths and gaps.
    let fixed_sum: u32 = items
        .iter()
        .map(|it| if let Item::Fixed(w) = it { *w } else { 0 })
        .fold(0u32, |acc, w| acc.saturating_add(w));
    let leftover = interior_w
        .saturating_sub(fixed_sum)
        .saturating_sub(total_spacing);

    // Total grow weight (treat weight=0 as 1 per item).
    let total_weight: u32 = items
        .iter()
        .map(|it| {
            if let Item::Grow(w) = it {
                (*w).max(1)
            } else {
                0
            }
        })
        .fold(0u32, |acc, w| acc.saturating_add(w));

    let clipped_h = height.min(interior_h);
    let y_offset = interior_h.saturating_sub(height) / 2;
    let child_y = interior_y + y_offset as i32;

    let mut rects = Vec::with_capacity(items.len());
    let mut cursor_x: u32 = 0;

    for (i, item) in items.iter().enumerate() {
        let child_w = match item {
            Item::Fixed(w) => *w,
            Item::Grow(w) => {
                let weight = (*w).max(1);
                // weight / total_weight * leftover, saturating.
                leftover.saturating_mul(weight) / total_weight.max(1)
            }
        };
        let remaining = interior_w.saturating_sub(cursor_x);
        let clipped_w = child_w.min(remaining);
        let child_x = interior_x + cursor_x as i32;

        rects.push(Rect::new(child_x, child_y, clipped_w, clipped_h));

        cursor_x = cursor_x.saturating_add(child_w);
        // Add spacing after every item except the last.
        if i + 1 < items.len() {
            cursor_x = cursor_x.saturating_add(spacing);
        }
    }

    rects
}

/// Lay out `items` top-to-bottom inside `parent`, applying `padding`
/// and `spacing`.  Each child's width on the cross axis is `width`;
/// children are horizontally centred within the interior, identical
/// to the [`Column`] cursor API.
///
/// Returns one [`Rect`] per item in the same order as `items`.
///
/// Fixed items are placed at their stated heights (clipped on
/// overflow). Grow items split the leftover main-axis space after
/// all Fixed heights and gaps are accounted for.
pub fn col_with_grow(
    parent: Rect,
    padding: u32,
    spacing: u32,
    items: &[Item],
    width: u32,
) -> Vec<Rect> {
    if items.is_empty() {
        return Vec::new();
    }
    let interior_x = parent.x + padding as i32;
    let interior_y = parent.y + padding as i32;
    let interior_w = parent.width.saturating_sub(2 * padding);
    let interior_h = parent.height.saturating_sub(2 * padding);

    let n = items.len() as u32;
    let total_spacing = spacing.saturating_mul(n.saturating_sub(1));
    let fixed_sum: u32 = items
        .iter()
        .map(|it| if let Item::Fixed(h) = it { *h } else { 0 })
        .fold(0u32, |acc, h| acc.saturating_add(h));
    let leftover = interior_h
        .saturating_sub(fixed_sum)
        .saturating_sub(total_spacing);

    let total_weight: u32 = items
        .iter()
        .map(|it| {
            if let Item::Grow(w) = it {
                (*w).max(1)
            } else {
                0
            }
        })
        .fold(0u32, |acc, w| acc.saturating_add(w));

    let clipped_w = width.min(interior_w);
    let x_offset = interior_w.saturating_sub(width) / 2;
    let child_x = interior_x + x_offset as i32;

    let mut rects = Vec::with_capacity(items.len());
    let mut cursor_y: u32 = 0;

    for (i, item) in items.iter().enumerate() {
        let child_h = match item {
            Item::Fixed(h) => *h,
            Item::Grow(w) => {
                let weight = (*w).max(1);
                leftover.saturating_mul(weight) / total_weight.max(1)
            }
        };
        let remaining = interior_h.saturating_sub(cursor_y);
        let clipped_h = child_h.min(remaining);
        let child_y = interior_y + cursor_y as i32;

        rects.push(Rect::new(child_x, child_y, clipped_w, clipped_h));

        cursor_y = cursor_y.saturating_add(child_h);
        if i + 1 < items.len() {
            cursor_y = cursor_y.saturating_add(spacing);
        }
    }

    rects
}
