//! Client-side drawing primitive tests.

use toolkit::draw::{
    font::{glyph_for, CELL_WIDTH, GLYPH_HEIGHT, GLYPH_WIDTH},
    Canvas, Color, Rect, BYTES_PER_PIXEL,
};

// ---- Color ------------------------------------------------------

#[test]
fn color_argb_extracts_channels_in_the_right_positions() {
    let c = Color::argb(0xFFAA_BBCC);
    assert_eq!(c.a(), 0xFF);
    assert_eq!(c.r(), 0xAA);
    assert_eq!(c.g(), 0xBB);
    assert_eq!(c.b(), 0xCC);
    assert_eq!(c.to_argb(), 0xFFAA_BBCC);
}

#[test]
fn color_rgb_is_fully_opaque() {
    let c = Color::rgb(0x10, 0x20, 0x30);
    assert_eq!(c.a(), 0xFF);
    assert_eq!(c.r(), 0x10);
    assert_eq!(c.g(), 0x20);
    assert_eq!(c.b(), 0x30);
}

#[test]
fn color_rgba_preserves_alpha() {
    let c = Color::rgba(0x01, 0x02, 0x03, 0x80);
    assert_eq!(c.a(), 0x80);
    assert_eq!(c.r(), 0x01);
    assert_eq!(c.g(), 0x02);
    assert_eq!(c.b(), 0x03);
}

#[test]
fn transparent_is_all_zero() {
    let c = Color::TRANSPARENT;
    assert_eq!(c.a(), 0);
    assert_eq!(c.r(), 0);
    assert_eq!(c.g(), 0);
    assert_eq!(c.b(), 0);
}

// ---- Rect -------------------------------------------------------

#[test]
fn rect_exposes_right_and_bottom_as_exclusive_edges() {
    let r = Rect::new(10, 20, 30, 40);
    assert_eq!(r.right(), 40);
    assert_eq!(r.bottom(), 60);
}

#[test]
fn rect_is_empty_when_either_dimension_is_zero() {
    assert!(Rect::new(0, 0, 0, 10).is_empty());
    assert!(Rect::new(0, 0, 10, 0).is_empty());
    assert!(!Rect::new(0, 0, 10, 10).is_empty());
}

// ---- Canvas -----------------------------------------------------

#[test]
fn new_canvas_is_zero_filled_at_the_requested_size() {
    let c = Canvas::new(8, 4);
    assert_eq!(c.width(), 8);
    assert_eq!(c.height(), 4);
    assert_eq!(c.pixels().len(), 8 * 4 * BYTES_PER_PIXEL);
    assert!(c.pixels().iter().all(|b| *b == 0));
}

#[test]
fn clear_writes_color_bytes_in_rgba_memory_order() {
    let mut c = Canvas::new(2, 2);
    // 0xFFAARRGGBB → (R=A, G=R, B=G, ...) — use concrete
    // values to make the pin unambiguous.
    c.clear(Color::argb(0xFF12_3456));
    for px in c.pixels().chunks_exact(4) {
        assert_eq!(px, &[0x12, 0x34, 0x56, 0xFF]);
    }
}

#[test]
fn set_pixel_writes_the_expected_rgba_bytes() {
    let mut c = Canvas::new(4, 4);
    c.set_pixel(2, 1, Color::rgb(0x11, 0x22, 0x33));
    assert_eq!(c.pixel(2, 1).unwrap(), &[0x11, 0x22, 0x33, 0xFF]);
    // Other pixels untouched.
    assert_eq!(c.pixel(0, 0).unwrap(), &[0, 0, 0, 0]);
}

#[test]
fn set_pixel_clips_out_of_range_coordinates() {
    let mut c = Canvas::new(4, 4);
    c.set_pixel(-1, 0, Color::rgb(0xff, 0, 0));
    c.set_pixel(0, -1, Color::rgb(0, 0xff, 0));
    c.set_pixel(4, 0, Color::rgb(0, 0, 0xff));
    c.set_pixel(0, 4, Color::rgb(0xff, 0xff, 0));
    // Every pixel is still zero.
    assert!(c.pixels().iter().all(|b| *b == 0));
}

#[test]
fn pixel_returns_none_out_of_bounds() {
    let c = Canvas::new(2, 2);
    assert!(c.pixel(2, 0).is_none());
    assert!(c.pixel(0, 2).is_none());
}

#[test]
fn fill_rect_writes_every_pixel_in_bounds() {
    let mut c = Canvas::new(4, 4);
    c.fill_rect(Rect::new(1, 1, 2, 2), Color::rgb(0xff, 0, 0));
    // Pixels inside the 2x2 box.
    for y in 1..3 {
        for x in 1..3 {
            assert_eq!(c.pixel(x, y).unwrap(), &[0xff, 0, 0, 0xff]);
        }
    }
    // Corners outside the box.
    assert_eq!(c.pixel(0, 0).unwrap(), &[0, 0, 0, 0]);
    assert_eq!(c.pixel(3, 3).unwrap(), &[0, 0, 0, 0]);
}

#[test]
fn fill_rect_clips_at_canvas_edges() {
    let mut c = Canvas::new(4, 4);
    // Rectangle extends past the right and bottom edges.
    c.fill_rect(Rect::new(2, 2, 10, 10), Color::rgb(0x10, 0x20, 0x30));
    // Only the 2x2 in-bounds region was painted.
    assert_eq!(c.pixel(2, 2).unwrap(), &[0x10, 0x20, 0x30, 0xff]);
    assert_eq!(c.pixel(3, 3).unwrap(), &[0x10, 0x20, 0x30, 0xff]);
    // Top-left corner still zero.
    assert_eq!(c.pixel(0, 0).unwrap(), &[0, 0, 0, 0]);
}

#[test]
fn fill_rect_with_negative_origin_clips_leading_rows_and_columns() {
    let mut c = Canvas::new(4, 4);
    c.fill_rect(Rect::new(-1, -1, 3, 3), Color::rgb(0xff, 0xff, 0xff));
    // Visible region is (0..2, 0..2).
    for y in 0..2 {
        for x in 0..2 {
            assert_eq!(c.pixel(x, y).unwrap(), &[0xff, 0xff, 0xff, 0xff]);
        }
    }
    // Outside the visible region.
    assert_eq!(c.pixel(2, 0).unwrap(), &[0, 0, 0, 0]);
    assert_eq!(c.pixel(0, 2).unwrap(), &[0, 0, 0, 0]);
}

#[test]
fn fill_rect_with_zero_size_is_a_noop() {
    let mut c = Canvas::new(4, 4);
    c.fill_rect(Rect::new(1, 1, 0, 2), Color::rgb(0xff, 0, 0));
    c.fill_rect(Rect::new(1, 1, 2, 0), Color::rgb(0xff, 0, 0));
    assert!(c.pixels().iter().all(|b| *b == 0));
}

#[test]
fn stroke_rect_draws_a_single_pixel_border() {
    let mut c = Canvas::new(6, 6);
    c.stroke_rect(Rect::new(1, 1, 4, 4), Color::rgb(0, 0xff, 0));
    // Four corner pixels are green.
    for &(x, y) in &[(1, 1), (4, 1), (1, 4), (4, 4)] {
        assert_eq!(
            c.pixel(x, y).unwrap(),
            &[0, 0xff, 0, 0xff],
            "corner ({x}, {y})"
        );
    }
    // A pixel strictly inside the border is still zero.
    assert_eq!(c.pixel(2, 2).unwrap(), &[0, 0, 0, 0]);
    assert_eq!(c.pixel(3, 3).unwrap(), &[0, 0, 0, 0]);
    // A pixel strictly outside the border is still zero.
    assert_eq!(c.pixel(0, 0).unwrap(), &[0, 0, 0, 0]);
}

#[test]
fn stroke_rect_with_height_one_draws_a_single_horizontal_line() {
    let mut c = Canvas::new(6, 6);
    c.stroke_rect(Rect::new(1, 2, 4, 1), Color::rgb(0xff, 0, 0));
    // Row 2, columns 1..5 are red.
    for x in 1..5 {
        assert_eq!(c.pixel(x, 2).unwrap(), &[0xff, 0, 0, 0xff]);
    }
    // Row 1 and row 3 untouched.
    assert_eq!(c.pixel(2, 1).unwrap(), &[0, 0, 0, 0]);
    assert_eq!(c.pixel(2, 3).unwrap(), &[0, 0, 0, 0]);
}

#[test]
fn stroke_rect_with_width_one_draws_a_single_vertical_line() {
    let mut c = Canvas::new(6, 6);
    c.stroke_rect(Rect::new(2, 1, 1, 4), Color::rgb(0, 0, 0xff));
    for y in 1..5 {
        assert_eq!(c.pixel(2, y).unwrap(), &[0, 0, 0xff, 0xff]);
    }
    assert_eq!(c.pixel(1, 2).unwrap(), &[0, 0, 0, 0]);
    assert_eq!(c.pixel(3, 2).unwrap(), &[0, 0, 0, 0]);
}

// ---- draw_text --------------------------------------------------

#[test]
fn draw_text_advances_by_cell_width_per_char() {
    let mut c = Canvas::new(64, 16);
    let drawn = c.draw_text(0, 0, "hi", Color::rgb(0xff, 0xff, 0xff));
    assert_eq!(drawn, 2 * CELL_WIDTH);
}

#[test]
fn draw_text_paints_glyph_on_pixels_at_the_right_cell() {
    // Render "o" at (0, 0). The 'o' glyph has rows 2..6
    // with `.###.` and `#...#` shapes. At row 2
    // (0b01110) the center three columns are on. We
    // expect the canvas pixel at (1, 2), (2, 2), (3, 2)
    // to be foreground.
    let mut c = Canvas::new(16, 16);
    let fg = Color::rgb(0xde, 0xad, 0xbe);
    c.draw_text(0, 0, "o", fg);
    // Verify against the glyph row bytes directly so the
    // test doesn't drift if the glyph changes.
    let glyph = glyph_for('o');
    for row in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            let on = (glyph[row as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 != 0;
            let px = c.pixel(col, row).unwrap();
            if on {
                assert_eq!(px, &[0xde, 0xad, 0xbe, 0xff], "row {row} col {col}");
            } else {
                assert_eq!(px, &[0, 0, 0, 0], "row {row} col {col}");
            }
        }
    }
}

#[test]
fn draw_text_clips_past_the_right_edge() {
    // Canvas is 8 pixels wide; a 3-char string would need
    // 3 * CELL_WIDTH = 18 pixels. Only the first glyph
    // fully fits; the second spills past the edge and
    // the third is entirely off-canvas. `draw_text`
    // shouldn't panic — it relies on `set_pixel` clipping.
    let mut c = Canvas::new(8, 16);
    let drawn = c.draw_text(0, 0, "abc", Color::rgb(0xff, 0xff, 0xff));
    // It still reports the logical pen advance —
    // clipping is a canvas-side concern, not a text
    // measurement concern.
    assert_eq!(drawn, 3 * CELL_WIDTH);
}

#[test]
fn draw_text_handles_empty_string_as_a_noop() {
    let mut c = Canvas::new(8, 8);
    let drawn = c.draw_text(0, 0, "", Color::rgb(0xff, 0, 0));
    assert_eq!(drawn, 0);
    assert!(c.pixels().iter().all(|b| *b == 0));
}

#[test]
fn into_pixels_hands_back_ownership_of_the_byte_buffer() {
    let mut c = Canvas::new(2, 2);
    c.clear(Color::rgb(1, 2, 3));
    let bytes = c.into_pixels();
    assert_eq!(bytes.len(), 2 * 2 * 4);
    assert_eq!(bytes[0..4], [1, 2, 3, 0xff]);
}
