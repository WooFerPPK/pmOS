//! Horizontal alignment for widget content.
//!
//! `Alignment` is a widget-layer vocabulary word, not a
//! coordinate primitive: it answers "where in a rect does
//! the content go?" and that question only makes sense if
//! you know both the container bounds and the content
//! size. The draw layer ([`crate::draw::Canvas`]) takes
//! explicit `(x, y)` coordinates and doesn't need this
//! enum; every widget that has to place a child inside
//! its own bounds does.
//!
//! Living here at the top of the widget module means
//! [`crate::widget::label::Label`],
//! [`crate::widget::button::Button`], future text inputs,
//! menu items, status-bar segments, and the upcoming
//! layout primitive all import one enum rather than each
//! rolling their own.

/// Horizontal content placement within a container rect.
/// Vertical placement is not spelled out here — the
/// toolkit's monospace bitmap font is fixed-height, so
/// "centre vertically" is the only sensible choice and
/// widgets do it automatically.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Alignment {
    /// Flush with the container's left edge.
    #[default]
    Left,
    /// Centred horizontally within the container.
    Center,
    /// Flush with the container's right edge.
    Right,
}
