//! Isolation tests for `toolkit::widget::Label` and the
//! `toolkit::draw::text::fit_text_to_width` helper it was
//! extracted against.

use toolkit::draw::font::{glyph_for, CELL_WIDTH, GLYPH_HEIGHT, GLYPH_WIDTH};
use toolkit::draw::text::{fit_text_to_width, text_width_px};
use toolkit::draw::{Canvas, Color, Rect};
use toolkit::theme::Theme;
use toolkit::widget::{Alignment, Label, LABEL_HPAD, LABEL_VPAD};

// ---- helpers -------------------------------------------------------

fn rgba(color: Color) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

/// Assert that the pixels at `(origin_x, origin_y)` through
/// `(origin_x + GLYPH_WIDTH, origin_y + GLYPH_HEIGHT)` contain the
/// expected glyph for `ch` in colour `color`. Mirrors the assertion
/// style used by `tests/draw.rs::draw_text_paints_glyph_on_pixels_at_the_right_cell`.
fn assert_glyph_at(canvas: &Canvas, origin_x: i32, origin_y: i32, ch: char, color: Color) {
    let glyph = glyph_for(ch);
    let expected = rgba(color);
    for row in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            let on = (glyph[row as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 != 0;
            if !on {
                continue;
            }
            let x = origin_x + col as i32;
            let y = origin_y + row as i32;
            let px = canvas.pixel(x as u32, y as u32).unwrap();
            assert_eq!(
                px,
                expected,
                "glyph '{ch}' pixel at (+{col}, +{row}) from origin ({origin_x}, {origin_y})",
            );
        }
    }
}

// ---- geometry / alignment -----------------------------------------

#[test]
fn label_renders_text_at_bounds_origin_for_left_alignment() {
    // 100x16 bounds centres the 7-pixel glyph vertically at
    // y = 0 + (16 - 7) / 2 = 4.
    let bounds = Rect::new(10, 20, 100, 16);
    let mut canvas = Canvas::new(200, 80);
    let label = Label::new(bounds, "hi");
    label.draw(&mut canvas);

    let color = Theme::LIGHT.label_text;
    assert_glyph_at(&canvas, 10, 20 + 4, 'h', color);
    assert_glyph_at(&canvas, 10 + CELL_WIDTH as i32, 20 + 4, 'i', color);
}

#[test]
fn label_centers_text_horizontally_for_center_alignment() {
    let bounds = Rect::new(10, 20, 60, 16);
    let mut canvas = Canvas::new(200, 80);
    let mut label = Label::new(bounds, "ab");
    label.set_alignment(Alignment::Center);
    label.draw(&mut canvas);

    // Text width: 2 chars × CELL_WIDTH (6) = 12 px.
    // Horizontal offset within bounds: (60 - 12) / 2 = 24.
    let text_x = 10 + 24;
    let text_y = 20 + 4;
    let color = Theme::LIGHT.label_text;
    assert_glyph_at(&canvas, text_x, text_y, 'a', color);
    assert_glyph_at(&canvas, text_x + CELL_WIDTH as i32, text_y, 'b', color);
}

#[test]
fn label_right_aligns_text_for_right_alignment() {
    let bounds = Rect::new(10, 20, 60, 16);
    let mut canvas = Canvas::new(200, 80);
    let mut label = Label::new(bounds, "ab");
    label.set_alignment(Alignment::Right);
    label.draw(&mut canvas);

    // Text width: 12 px. Right-aligned x = 10 + 60 - 12 = 58.
    let text_x = 58;
    let text_y = 20 + 4;
    let color = Theme::LIGHT.label_text;
    assert_glyph_at(&canvas, text_x, text_y, 'a', color);
    assert_glyph_at(&canvas, text_x + CELL_WIDTH as i32, text_y, 'b', color);
}

// ---- clipping ------------------------------------------------------

#[test]
fn label_clips_text_too_wide_for_bounds() {
    // 20 px wide → 20 / 6 = 3 full glyph cells. Text has 7 chars.
    let bounds = Rect::new(0, 0, 20, 16);
    let mut canvas = Canvas::new(40, 16);
    let label = Label::new(bounds, "abcdefg");
    assert_eq!(label.visible_text(), "abc");
    label.draw(&mut canvas);

    let color = Theme::LIGHT.label_text;
    let text_y = 0 + (16 - GLYPH_HEIGHT as i32) / 2;
    assert_glyph_at(&canvas, 0, text_y, 'a', color);
    assert_glyph_at(&canvas, CELL_WIDTH as i32, text_y, 'b', color);
    assert_glyph_at(&canvas, (2 * CELL_WIDTH) as i32, text_y, 'c', color);

    // The 4th char 'd' would start at x = 18, which is still within
    // the 20-wide bounds column-wise, but the glyph runs 18..23 and
    // would paint pixels at x = 18 and x = 19 (then be clipped by
    // the canvas edge). `fit_text_to_width` drops the whole 'd'
    // because its cell (x = 18..24) doesn't fit in 20 px. Verify
    // no glyph pixels appear at x = 18 on the glyph rows.
    for row in 0..GLYPH_HEIGHT {
        let y = text_y + row as i32;
        let px = canvas.pixel(18, y as u32).unwrap();
        assert_eq!(
            px,
            &[0, 0, 0, 0],
            "expected no clipped 'd' pixel at (18, {y})",
        );
    }
}

// ---- degenerate inputs --------------------------------------------

#[test]
fn label_with_empty_text_does_not_panic() {
    let mut canvas = Canvas::new(40, 40);
    let label = Label::new(Rect::new(5, 5, 30, 20), "");
    label.draw(&mut canvas);
    assert!(canvas.pixels().iter().all(|b| *b == 0));
}

#[test]
fn label_with_zero_width_bounds_does_not_panic() {
    let mut canvas = Canvas::new(40, 40);
    let label = Label::new(Rect::new(5, 5, 0, 20), "hi");
    label.draw(&mut canvas);
    assert_eq!(label.visible_text(), "");
    assert!(canvas.pixels().iter().all(|b| *b == 0));

    // Zero height too.
    let label = Label::new(Rect::new(5, 5, 30, 0), "hi");
    label.draw(&mut canvas);
    assert!(canvas.pixels().iter().all(|b| *b == 0));

    // Height less than one glyph row.
    let label = Label::new(Rect::new(5, 5, 30, GLYPH_HEIGHT - 1), "hi");
    label.draw(&mut canvas);
    assert!(canvas.pixels().iter().all(|b| *b == 0));
}

// ---- state -------------------------------------------------------

#[test]
fn label_set_text_updates_rendered_output() {
    let bounds = Rect::new(0, 0, 100, 16);
    let mut canvas_a = Canvas::new(100, 16);
    let mut canvas_b = Canvas::new(100, 16);
    let mut label = Label::new(bounds, "ab");
    label.draw(&mut canvas_a);
    label.set_text("xy");
    label.draw(&mut canvas_b);
    assert_ne!(canvas_a.pixels(), canvas_b.pixels());
    assert_eq!(label.text(), "xy");
}

#[test]
fn label_uses_label_text_theme_color() {
    let bounds = Rect::new(0, 0, 100, 16);
    let mut canvas = Canvas::new(100, 16);
    let label = Label::new(bounds, "a");
    label.draw(&mut canvas);

    let expected = rgba(Theme::LIGHT.label_text);
    let text_y = (16 - GLYPH_HEIGHT as i32) / 2;
    let glyph = glyph_for('a');
    // Find any "on" pixel in the 'a' glyph and verify it matches the
    // theme label_text colour.
    let mut found_on_pixel = false;
    for row in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            let on = (glyph[row as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 != 0;
            if !on {
                continue;
            }
            found_on_pixel = true;
            let px = canvas
                .pixel(col, (text_y + row as i32) as u32)
                .unwrap();
            assert_eq!(px, expected);
        }
    }
    assert!(found_on_pixel, "glyph 'a' has at least one lit pixel");
}

#[test]
fn label_set_color_overrides_theme_default() {
    let bounds = Rect::new(0, 0, 100, 16);
    let mut canvas = Canvas::new(100, 16);
    let custom = Color::rgb(0xff, 0x00, 0x80);
    let mut label = Label::new(bounds, "a");
    label.set_color(custom);
    label.draw(&mut canvas);

    let expected = rgba(custom);
    let text_y = (16 - GLYPH_HEIGHT as i32) / 2;
    // Glyph 'a' has a lit pixel at row 2, col 1 (middle of the top
    // curve) for the 5×7 bitmap used by the toolkit — but the safe
    // cross-check is to find any lit pixel and assert the colour.
    let glyph = glyph_for('a');
    for row in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            let on = (glyph[row as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 != 0;
            if !on {
                continue;
            }
            let px = canvas
                .pixel(col, (text_y + row as i32) as u32)
                .unwrap();
            assert_eq!(px, expected);
            return;
        }
    }
    panic!("glyph 'a' has no lit pixels");
}

// ---- vertical centering -------------------------------------------

#[test]
fn label_vertical_centering_within_bounds() {
    // 40-tall bounds → y offset = (40 - 7) / 2 = 16.
    let bounds = Rect::new(0, 0, 40, 40);
    let mut canvas = Canvas::new(40, 40);
    let label = Label::new(bounds, "a");
    label.draw(&mut canvas);

    let text_y = 16;
    let color = Theme::LIGHT.label_text;
    assert_glyph_at(&canvas, 0, text_y, 'a', color);

    // Rows above and below the glyph must be untouched.
    for row in 0..text_y {
        for col in 0..GLYPH_WIDTH {
            let px = canvas.pixel(col, row as u32).unwrap();
            assert_eq!(px, &[0, 0, 0, 0], "row {row} above glyph");
        }
    }
    for row in (text_y + GLYPH_HEIGHT as i32)..(bounds.height as i32) {
        for col in 0..GLYPH_WIDTH {
            let px = canvas.pixel(col, row as u32).unwrap();
            assert_eq!(px, &[0, 0, 0, 0], "row {row} below glyph");
        }
    }
}

// ---- fit_text_to_width helper regressions -------------------------

#[test]
fn fit_text_to_width_returns_full_string_when_it_fits() {
    // 3 chars × 6 px = 18 px. 30 px budget.
    assert_eq!(fit_text_to_width("abc", 30), "abc");
    // Exact fit.
    assert_eq!(fit_text_to_width("abc", 3 * CELL_WIDTH), "abc");
    // Empty text fits any budget.
    assert_eq!(fit_text_to_width("", 100), "");
    assert_eq!(fit_text_to_width("", 0), "");
}

#[test]
fn fit_text_to_width_truncates_at_character_boundary_when_too_wide() {
    // 20 / 6 = 3 full cells.
    assert_eq!(fit_text_to_width("abcdefg", 20), "abc");
    // 18 / 6 = 3 exact.
    assert_eq!(fit_text_to_width("abcdefg", 18), "abc");
    // One pixel short of fitting 3 cells.
    assert_eq!(fit_text_to_width("abcdefg", 17), "ab");
    // Multi-byte char is not split: "aé" is a + e-acute. e-acute is
    // 2 bytes in UTF-8. If the budget fits 1 char, return "a"; if
    // it fits 2 chars, return the full "aé".
    assert_eq!(fit_text_to_width("aé", CELL_WIDTH), "a");
    assert_eq!(fit_text_to_width("aé", 2 * CELL_WIDTH), "aé");
}

#[test]
fn fit_text_to_width_returns_empty_when_max_width_smaller_than_one_glyph() {
    assert_eq!(fit_text_to_width("abc", CELL_WIDTH - 1), "");
    assert_eq!(fit_text_to_width("abc", 0), "");
    assert_eq!(fit_text_to_width("abc", 1), "");
}

#[test]
fn text_width_px_is_char_count_times_cell_width() {
    assert_eq!(text_width_px(""), 0);
    assert_eq!(text_width_px("a"), CELL_WIDTH);
    assert_eq!(text_width_px("abc"), 3 * CELL_WIDTH);
    // Multi-byte char counts once (not by byte length).
    assert_eq!(text_width_px("aé"), 2 * CELL_WIDTH);
}

// ---- preferred_size ------------------------------------------------

#[test]
fn preferred_size_for_short_text_returns_text_width_plus_pad() {
    // "abc" → 3 × CELL_WIDTH(6) = 18 px of text.
    // Width = 18 + LABEL_HPAD(4) = 22.
    // Height = GLYPH_HEIGHT(7) + LABEL_VPAD(6) = 13.
    let label = Label::new(Rect::new(0, 0, 100, 30), "abc");
    assert_eq!(label.preferred_size(), (22, 13));
}

#[test]
fn preferred_size_for_empty_text_returns_just_pad_width_and_full_height() {
    let label = Label::new(Rect::new(0, 0, 100, 30), "");
    assert_eq!(
        label.preferred_size(),
        (LABEL_HPAD, GLYPH_HEIGHT + LABEL_VPAD),
    );
    // Pad values pinned explicitly so a future edit that
    // silently halves them trips this test.
    assert_eq!(label.preferred_size(), (4, 13));
}

#[test]
fn preferred_size_height_is_glyph_height_plus_vpad_regardless_of_text_length() {
    let short = Label::new(Rect::new(0, 0, 100, 30), "a");
    let medium = Label::new(Rect::new(0, 0, 100, 30), "hello");
    let long = Label::new(
        Rect::new(0, 0, 100, 30),
        "the quick brown fox jumps over the lazy dog",
    );
    let empty = Label::new(Rect::new(0, 0, 100, 30), "");
    let expected_h = GLYPH_HEIGHT + LABEL_VPAD;
    assert_eq!(short.preferred_size().1, expected_h);
    assert_eq!(medium.preferred_size().1, expected_h);
    assert_eq!(long.preferred_size().1, expected_h);
    assert_eq!(empty.preferred_size().1, expected_h);
}

#[test]
fn preferred_size_for_long_text_scales_width_proportionally() {
    // Every additional char adds exactly CELL_WIDTH px.
    let three = Label::new(Rect::new(0, 0, 100, 30), "abc");
    let six = Label::new(Rect::new(0, 0, 100, 30), "abcdef");
    let (w_three, _) = three.preferred_size();
    let (w_six, _) = six.preferred_size();
    // Difference = 3 chars × CELL_WIDTH(6) = 18.
    assert_eq!(w_six - w_three, 3 * CELL_WIDTH);
    // Absolute values spell out the formula.
    assert_eq!(w_three, 3 * CELL_WIDTH + LABEL_HPAD);
    assert_eq!(w_six, 6 * CELL_WIDTH + LABEL_HPAD);
}
