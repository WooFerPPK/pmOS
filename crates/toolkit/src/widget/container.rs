//! `Container` — layout primitive with optional border +
//! uniform padding that delegates child painting to an
//! opaque closure.
//!
//! A `Container` is the generic wrapper widgets reach for
//! when they need to nest one paint region inside another.
//! It owns three things: an optional border stroke, a
//! uniform padding value, and a single child paint closure
//! that it hands an interior rect to.
//!
//! The child is held as `Box<dyn FnMut(&mut Canvas, Rect)>`
//! rather than a typed enum of known widgets. That keeps
//! Container orthogonal to the rest of the widget library —
//! the closure body is free to call `label.draw(canvas)`,
//! `button.draw(canvas)`, another `container.paint(...)`,
//! or raw `canvas.fill_rect(...)` primitives without
//! Container having to know about any of them. Nested
//! containers come for free: the outer closure just calls
//! `inner.paint(canvas, bounds)`.
//!
//! The lifetime parameter `'child` ties the closure's
//! borrows to the Container's lifetime so a Container can
//! close over `&mut` references in the caller's stack.
//!
//! Container does not re-use [`super::label::Label`] or
//! [`super::button::Button`]'s border/padding constants
//! because it isn't drawing chrome for a specific widget
//! kind — its border width and padding are data supplied
//! at construction time. Theme integration is deliberately
//! absent: a Container that wants a themed border asks the
//! caller to pass `Theme::LIGHT.button_border` (or any
//! other slot) into [`Container::with_border`]. This keeps
//! Container from hardcoding any one theme slot's semantic
//! meaning.

use crate::draw::{Canvas, Color, Rect};

/// A rectangular region that optionally strokes a border,
/// shrinks its bounds by a uniform padding, and paints a
/// single child inside the resulting interior rect.
pub struct Container<'child> {
    padding: u32,
    border: Option<(u32, Color)>,
    child: Option<Box<dyn FnMut(&mut Canvas<'_>, Rect) + 'child>>,
}

impl<'child> Container<'child> {
    /// Construct an empty container: no padding, no border,
    /// no child paint closure. The resulting Container
    /// paints nothing.
    pub fn new() -> Self {
        Container {
            padding: 0,
            border: None,
            child: None,
        }
    }

    /// Set uniform padding (in pixels) applied to all four
    /// sides of the container's bounds before the child is
    /// painted. Stacks additively with the border width.
    pub fn with_padding(mut self, px: u32) -> Self {
        self.padding = px;
        self
    }

    /// Stroke a 1-pixel-wide-per-unit border of the given
    /// pixel width around the container's bounds using
    /// `color`. A width of zero is recorded but produces
    /// nothing at paint time (the stroke primitive short-
    /// circuits on empty rects).
    pub fn with_border(mut self, px: u32, color: Color) -> Self {
        self.border = Some((px, color));
        self
    }

    /// Install the child paint closure. Called by
    /// [`Container::paint`] exactly once per paint, with
    /// the interior rect (bounds shrunk by border + padding
    /// on every side). Replaces any previously-installed
    /// closure.
    pub fn with_child<F: FnMut(&mut Canvas<'_>, Rect) + 'child>(mut self, f: F) -> Self {
        self.child = Some(Box::new(f));
        self
    }

    /// Paint the container into `canvas`: stroke the border
    /// (if any), compute the interior rect by shrinking
    /// `bounds` uniformly by `border_width + padding` on
    /// every side, and invoke the child closure on the
    /// interior rect. If the interior rect has non-positive
    /// width or height, the child closure is not called.
    pub fn paint(&mut self, canvas: &mut Canvas<'_>, bounds: Rect) {
        if let Some((width, color)) = self.border {
            if width > 0 {
                canvas.stroke_rect(bounds, color);
            }
        }

        let border_px = self.border.map(|(w, _)| w).unwrap_or(0);
        let shrink = border_px.saturating_add(self.padding);
        let total_shrink = shrink.saturating_mul(2);
        if bounds.width <= total_shrink || bounds.height <= total_shrink {
            return;
        }
        let interior = Rect::new(
            bounds.x + shrink as i32,
            bounds.y + shrink as i32,
            bounds.width - total_shrink,
            bounds.height - total_shrink,
        );

        if let Some(child) = self.child.as_mut() {
            child(canvas, interior);
        }
    }

    /// Hit-test a point against the container's full
    /// bounds, border included. Returns `true` iff `point`
    /// lies inside `bounds` (right/bottom edges are
    /// exclusive, matching the convention used by
    /// [`super::button::Button::hit_test`]). Children do
    /// their own hit-test; Container's job is just to say
    /// "the pointer landed inside me."
    pub fn hit_test(&self, point: (u32, u32), bounds: Rect) -> bool {
        if bounds.is_empty() {
            return false;
        }
        let (px, py) = point;
        let px = px as i32;
        let py = py as i32;
        px >= bounds.x && px < bounds.right() && py >= bounds.y && py < bounds.bottom()
    }
}

impl<'child> Default for Container<'child> {
    fn default() -> Self {
        Container::new()
    }
}
