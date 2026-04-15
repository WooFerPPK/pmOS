//! Isolation tests for `toolkit::widget::Button`.
//!
//! The tests construct `Button` instances directly against
//! a native `Canvas` with no display server in the loop.
//! `WindowFrame`'s close button is `Button`'s first real
//! consumer — the API here was designed alongside that
//! consumer — so a few of these tests intentionally mirror
//! scenarios that `widget_frame.rs` covers for the close
//! button as well.

use std::cell::Cell;
use std::rc::Rc;

use toolkit::draw::font::{glyph_for, CELL_WIDTH, GLYPH_HEIGHT, GLYPH_WIDTH};
use toolkit::draw::{Canvas, Color, Rect};
use toolkit::theme::Theme;
use toolkit::widget::button::{Button, ButtonState};

// ---- helpers -------------------------------------------------------

fn rgba(color: Color) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

fn px(canvas: &Canvas, x: u32, y: u32) -> [u8; 4] {
    let slice = canvas.pixel(x, y).expect("pixel in bounds");
    [slice[0], slice[1], slice[2], slice[3]]
}

// ---- hit testing --------------------------------------------------

#[test]
fn hit_test_inside_returns_true() {
    let button = Button::new(Rect::new(10, 20, 40, 18), "ok");
    let cx = 10 + 20;
    let cy = 20 + 9;
    assert!(button.hit_test(cx, cy), "middle of button");
    assert!(button.hit_test(10, 20), "top-left corner is inside");
    // Right/bottom are exclusive.
    assert!(!button.hit_test(10 + 40, 20 + 18));
    assert!(button.hit_test(10 + 39, 20 + 17));
}

#[test]
fn hit_test_outside_returns_false() {
    let button = Button::new(Rect::new(10, 20, 40, 18), "ok");
    assert!(!button.hit_test(9, 20), "one pixel left of bounds");
    assert!(!button.hit_test(10, 19), "one pixel above bounds");
    assert!(!button.hit_test(60, 30));
    assert!(!button.hit_test(-100, -100));
}

#[test]
fn hit_test_is_false_on_empty_bounds() {
    let button = Button::new(Rect::new(5, 5, 0, 0), "x");
    assert!(!button.hit_test(5, 5));
    assert!(!button.hit_test(0, 0));
}

// ---- click callback ----------------------------------------------

#[test]
fn click_callback_fires_on_pointer_down_inside() {
    let fired = Rc::new(Cell::new(0u32));
    let fired_for_cb = Rc::clone(&fired);
    let mut button = Button::new(Rect::new(0, 0, 40, 18), "ok");
    button.on_click(move || fired_for_cb.set(fired_for_cb.get() + 1));

    assert!(button.pointer_down(20, 9));
    assert_eq!(fired.get(), 1);

    // A second press fires the callback again.
    assert!(button.pointer_down(20, 9));
    assert_eq!(fired.get(), 2);
}

#[test]
fn click_callback_does_not_fire_on_pointer_down_outside() {
    let fired = Rc::new(Cell::new(false));
    let fired_for_cb = Rc::clone(&fired);
    let mut button = Button::new(Rect::new(10, 10, 40, 18), "ok");
    button.on_click(move || fired_for_cb.set(true));

    assert!(!button.pointer_down(0, 0));
    assert!(!button.pointer_down(60, 50));
    assert!(!fired.get());
}

#[test]
fn click_callback_replaces_previous_callback() {
    let first = Rc::new(Cell::new(0u32));
    let second = Rc::new(Cell::new(0u32));
    let first_cb = Rc::clone(&first);
    let second_cb = Rc::clone(&second);

    let mut button = Button::new(Rect::new(0, 0, 40, 18), "ok");
    button.on_click(move || first_cb.set(first_cb.get() + 1));
    button.on_click(move || second_cb.set(second_cb.get() + 1));
    button.pointer_down(20, 9);

    assert_eq!(first.get(), 0, "replaced callback must not fire");
    assert_eq!(second.get(), 1);
}

// ---- caption rendering -------------------------------------------

#[test]
fn caption_renders_centered_in_bounds() {
    // 40-wide × 18-tall bounds. "ab" is 2 * CELL_WIDTH = 12 px wide.
    // Center x = (40 - 12) / 2 = 14. Center y = (18 - 7) / 2 = 5.
    // So 'a' glyph starts at (bounds.x + 14, bounds.y + 5) and 'b'
    // glyph starts at (bounds.x + 14 + 6, bounds.y + 5).
    let bounds = Rect::new(100, 100, 40, 18);
    let mut canvas = Canvas::new(200, 200);
    let button = Button::new(bounds, "ab");
    button.draw(&mut canvas);

    let glyph_a = glyph_for('a');
    let expected_text = rgba(Theme::LIGHT.button_text);
    let origin_x = 100 + 14;
    let origin_y = 100 + 5;
    let mut found_lit = false;
    for row in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            let on = (glyph_a[row as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 != 0;
            if !on {
                continue;
            }
            found_lit = true;
            let x = origin_x + col as i32;
            let y = origin_y + row as i32;
            assert_eq!(px(&canvas, x as u32, y as u32), expected_text);
        }
    }
    assert!(found_lit, "'a' glyph has at least one lit pixel");

    // And there should be no glyph-text pixels to the LEFT of the
    // 'a' glyph's cell (i.e. between the button's left fill and
    // the start of the centered caption).
    for y in 100..(100 + 18) {
        for x in 101..origin_x {
            let p = px(&canvas, x as u32, y as u32);
            assert_ne!(
                p, expected_text,
                "caption pixel leaked left of centered region at ({x}, {y})",
            );
        }
    }
}

// ---- state and fill colours --------------------------------------

#[test]
fn state_default_is_resting() {
    let button = Button::new(Rect::new(0, 0, 40, 18), "ok");
    assert_eq!(button.state(), ButtonState::Resting);
}

#[test]
fn hover_state_changes_fill_color() {
    let bounds = Rect::new(0, 0, 40, 18);
    let mut canvas_rest = Canvas::new(40, 18);
    let mut canvas_hover = Canvas::new(40, 18);
    let mut button = Button::new(bounds, "");

    button.set_state(ButtonState::Resting);
    button.draw(&mut canvas_rest);
    button.set_state(ButtonState::Hover);
    button.draw(&mut canvas_hover);

    // Sample a pixel inside the fill area (not on the stroke border).
    let sample = px(&canvas_rest, 20, 9);
    let sample_hover = px(&canvas_hover, 20, 9);
    assert_eq!(sample, rgba(Theme::LIGHT.button_fill));
    assert_eq!(sample_hover, rgba(Theme::LIGHT.button_fill_hover));
    assert_ne!(sample, sample_hover);
}

#[test]
fn pressed_state_changes_fill_color() {
    let bounds = Rect::new(0, 0, 40, 18);
    let mut canvas = Canvas::new(40, 18);
    let mut button = Button::new(bounds, "");
    button.set_state(ButtonState::Pressed);
    button.draw(&mut canvas);

    assert_eq!(px(&canvas, 20, 9), rgba(Theme::LIGHT.button_fill_pressed));
}

// ---- theme colours ------------------------------------------------

#[test]
fn caption_uses_button_text_theme_color_by_default() {
    let bounds = Rect::new(0, 0, 40, 18);
    let mut canvas = Canvas::new(40, 18);
    let button = Button::new(bounds, "a");
    button.draw(&mut canvas);

    // Find any lit pixel from the 'a' glyph and verify its colour.
    let glyph = glyph_for('a');
    let expected = rgba(Theme::LIGHT.button_text);
    let center_x = (40 - CELL_WIDTH) / 2;
    let center_y = (18 - GLYPH_HEIGHT) / 2;
    for row in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            if (glyph[row as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 != 0 {
                let x = center_x + col;
                let y = center_y + row;
                assert_eq!(px(&canvas, x, y), expected);
                return;
            }
        }
    }
    panic!("glyph 'a' has no lit pixels");
}

#[test]
fn setters_override_theme_defaults() {
    let custom_fill = Color::rgb(0xff, 0x55, 0x22);
    let custom_border = Color::rgb(0x11, 0x22, 0x33);
    let custom_caption = Color::rgb(0xaa, 0xbb, 0xcc);

    let mut canvas = Canvas::new(40, 18);
    let mut button = Button::new(Rect::new(0, 0, 40, 18), "a");
    button.set_fill(custom_fill);
    button.set_border(custom_border);
    button.set_caption_color(custom_caption);
    button.draw(&mut canvas);

    // Sample fill away from both the border stroke and the
    // centred glyph: (5, 9) is inside bounds, not on the
    // perimeter, and the 'a' glyph cell starts at x = 17.
    assert_eq!(px(&canvas, 5, 9), rgba(custom_fill));
    // Top-edge stroke pixel uses custom border.
    assert_eq!(px(&canvas, 20, 0), rgba(custom_border));
    // Any lit 'a' glyph pixel uses custom caption colour.
    let glyph = glyph_for('a');
    let center_x = (40 - CELL_WIDTH) / 2;
    let center_y = (18 - GLYPH_HEIGHT) / 2;
    for row in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            if (glyph[row as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 != 0 {
                let x = center_x + col;
                let y = center_y + row;
                assert_eq!(px(&canvas, x, y), rgba(custom_caption));
                return;
            }
        }
    }
    panic!("glyph 'a' has no lit pixels");
}

// ---- border renders at bounds --------------------------------------

#[test]
fn border_renders_at_bounds_perimeter() {
    let bounds = Rect::new(10, 20, 40, 18);
    let mut canvas = Canvas::new(100, 100);
    let button = Button::new(bounds, "");
    button.draw(&mut canvas);

    let border = rgba(Theme::LIGHT.button_border);
    // Four corners.
    assert_eq!(px(&canvas, 10, 20), border);
    assert_eq!(px(&canvas, 49, 20), border);
    assert_eq!(px(&canvas, 10, 37), border);
    assert_eq!(px(&canvas, 49, 37), border);
    // Edge midpoints.
    assert_eq!(px(&canvas, 30, 20), border);
    assert_eq!(px(&canvas, 30, 37), border);
    // Outside the border is zero.
    assert_eq!(px(&canvas, 9, 20), [0, 0, 0, 0]);
}

// ---- degenerate bounds --------------------------------------------

#[test]
fn zero_sized_bounds_does_not_panic() {
    let mut canvas = Canvas::new(40, 18);
    let button = Button::new(Rect::new(5, 5, 0, 0), "anything");
    button.draw(&mut canvas);
    assert!(canvas.pixels().iter().all(|b| *b == 0));

    let button = Button::new(Rect::new(5, 5, 10, 0), "anything");
    button.draw(&mut canvas);
    assert!(canvas.pixels().iter().all(|b| *b == 0));
}

#[test]
fn pointer_down_on_zero_sized_bounds_does_not_fire_callback() {
    let fired = Rc::new(Cell::new(false));
    let fired_for_cb = Rc::clone(&fired);
    let mut button = Button::new(Rect::new(0, 0, 0, 0), "x");
    button.on_click(move || fired_for_cb.set(true));
    assert!(!button.pointer_down(0, 0));
    assert!(!fired.get());
}

// ---- setters ------------------------------------------------------

#[test]
fn set_caption_changes_rendered_caption() {
    let bounds = Rect::new(0, 0, 40, 18);
    let mut canvas_first = Canvas::new(40, 18);
    let mut canvas_second = Canvas::new(40, 18);
    let mut button = Button::new(bounds, "a");
    button.draw(&mut canvas_first);
    button.set_caption("b");
    button.draw(&mut canvas_second);
    assert_eq!(button.caption(), "b");
    assert_ne!(canvas_first.pixels(), canvas_second.pixels());
}
