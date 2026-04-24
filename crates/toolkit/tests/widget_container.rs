//! Isolation tests for `toolkit::widget::Container`.
//!
//! Construct `Container` instances directly against a
//! native `Canvas` with no display server in the loop. The
//! child is modelled as an opaque `FnMut(&mut Canvas, Rect)`
//! closure; tests drive those closures against a
//! `Cell<Option<Rect>>` or `Cell<bool>` to observe what
//! interior rect the Container handed down and whether it
//! called the child at all.

use std::cell::Cell;

use toolkit::draw::{Canvas, Color, Rect};
use toolkit::widget::Container;

const RED: Color = Color::rgb(0xff, 0x00, 0x00);
const BLUE: Color = Color::rgb(0x00, 0x00, 0xff);

fn rgba(color: Color) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

fn px(canvas: &Canvas, x: u32, y: u32) -> [u8; 4] {
    let slice = canvas.pixel(x, y).expect("pixel in bounds");
    [slice[0], slice[1], slice[2], slice[3]]
}

#[test]
fn container_no_child_no_border_paints_nothing() {
    let mut canvas = Canvas::new(100, 100);
    let mut container = Container::new();
    container.paint(&mut canvas, Rect::new(0, 0, 100, 100));

    assert!(
        canvas.pixels().iter().all(|b| *b == 0),
        "a border-less child-less Container must not touch any pixel",
    );
}

#[test]
fn container_border_strokes_outline() {
    let mut canvas = Canvas::new(50, 50);
    let mut container = Container::new().with_border(2, RED);
    container.paint(&mut canvas, Rect::new(0, 0, 50, 50));

    let expected = rgba(RED);
    // All four corners of the bounds belong to the stroked outline.
    assert_eq!(px(&canvas, 0, 0), expected, "top-left corner");
    assert_eq!(px(&canvas, 49, 0), expected, "top-right corner");
    assert_eq!(px(&canvas, 0, 49), expected, "bottom-left corner");
    assert_eq!(px(&canvas, 49, 49), expected, "bottom-right corner");

    // Centre pixel is well inside the bounds and must stay zeroed
    // — the Container has no child and the 1-pixel stroke only
    // touches the perimeter.
    assert_eq!(
        px(&canvas, 25, 25),
        [0, 0, 0, 0],
        "centre pixel must be untouched",
    );
}

#[test]
fn container_padding_shrinks_child_rect() {
    let recorded: Cell<Option<Rect>> = Cell::new(None);
    let mut canvas = Canvas::new(60, 60);

    {
        let mut container = Container::new()
            .with_padding(5)
            .with_child(|_canvas, rect| {
                recorded.set(Some(rect));
            });
        container.paint(&mut canvas, Rect::new(0, 0, 50, 50));
    }

    let rect = recorded.get().expect("child closure was called");
    assert_eq!(rect, Rect::new(5, 5, 40, 40));
}

#[test]
fn container_border_plus_padding_compose() {
    let recorded: Cell<Option<Rect>> = Cell::new(None);
    let mut canvas = Canvas::new(60, 60);

    {
        let mut container = Container::new()
            .with_border(2, BLUE)
            .with_padding(3)
            .with_child(|_canvas, rect| {
                recorded.set(Some(rect));
            });
        container.paint(&mut canvas, Rect::new(0, 0, 50, 50));
    }

    // (2 + 3) = 5 px shrink on each side: 50 - 10 = 40 interior.
    let rect = recorded.get().expect("child closure was called");
    assert_eq!(rect, Rect::new(5, 5, 40, 40));
}

#[test]
fn container_empty_interior_skips_child() {
    let called = Cell::new(false);
    let mut canvas = Canvas::new(60, 60);

    {
        let mut container = Container::new()
            .with_padding(30)
            .with_child(|_canvas, _rect| {
                called.set(true);
            });
        // 50 - (2 * 30) = -10 → interior empty, child skipped.
        container.paint(&mut canvas, Rect::new(0, 0, 50, 50));
    }

    assert!(
        !called.get(),
        "child closure must not fire when interior collapses to empty",
    );
}

#[test]
fn container_hit_test_includes_border() {
    let container = Container::new().with_border(3, RED).with_padding(5);
    let bounds = Rect::new(10, 20, 40, 30);

    // Point two pixels in from the top-left corner — inside the
    // border region, *before* the padding starts (border is 3 px
    // thick, so (12, 22) is still within the stroke).
    assert!(
        container.hit_test((12, 22), bounds),
        "point inside the border region must hit",
    );
    // Top-left corner itself is inclusive.
    assert!(container.hit_test((10, 20), bounds));
    // Centre is obviously inside.
    assert!(container.hit_test((30, 35), bounds));
    // One pixel past the right edge is outside (exclusive).
    assert!(!container.hit_test((50, 35), bounds));
    // One pixel past the bottom edge is outside (exclusive).
    assert!(!container.hit_test((30, 50), bounds));
    // Well outside bounds.
    assert!(!container.hit_test((0, 0), bounds));
}
