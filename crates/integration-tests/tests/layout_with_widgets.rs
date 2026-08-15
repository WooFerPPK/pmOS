//! Integration: `Row` places a `Label + Button + Label` run
//! inside a parent rect, each widget is drawn to a shared
//! [`Canvas`], and the rendered pixels are cross-checked
//! against the rects `Row::next` produced.
//!
//! This is the "Layout composes with real widgets" smoke
//! test — equivalent to `two_term_windows.rs` for
//! `WindowFrame`. No display server, no compositor; just
//! three widgets laid out in a row on a single canvas.

use toolkit::draw::font::{glyph_for, GLYPH_HEIGHT, GLYPH_WIDTH};
use toolkit::draw::{Canvas, Color, Rect};
use toolkit::layout::Row;
use toolkit::theme::Theme;
use toolkit::widget::alignment::Alignment;
use toolkit::widget::{Button, Label, LABEL_HPAD};

fn rgba(color: Color) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

fn px(canvas: &Canvas, x: i32, y: i32) -> [u8; 4] {
    let slice = canvas
        .pixel(x as u32, y as u32)
        .expect("sample pixel in bounds");
    [slice[0], slice[1], slice[2], slice[3]]
}

#[test]
fn row_layout_places_label_button_label_at_expected_positions() {
    // 400 × 40 parent with 5-pixel symmetric padding and 8-pixel
    // inter-child spacing. Interior is (5, 5, 390, 30).
    const PARENT: Rect = Rect::new(0, 0, 400, 40);
    const PADDING: u32 = 5;
    const SPACING: u32 = 8;
    const CHILD_H: u32 = 20;

    // Widths come from each widget's preferred_size() — the
    // whole point of this test since the Layout slice. Build
    // probe copies with throwaway bounds to query the sizes,
    // then rebuild with the row-assigned rects below.
    let mut probe_label_left = Label::new(Rect::new(0, 0, 0, 0), "left  ");
    probe_label_left.set_alignment(Alignment::Left);
    let probe_button = Button::new(Rect::new(0, 0, 0, 0), "ok");
    let mut probe_label_right = Label::new(Rect::new(0, 0, 0, 0), "right   ");
    probe_label_right.set_alignment(Alignment::Right);

    let (label_left_w, _) = probe_label_left.preferred_size();
    let (button_w, _) = probe_button.preferred_size();
    let (label_right_w, _) = probe_label_right.preferred_size();

    let mut row = Row::new(PARENT, PADDING, SPACING);
    let label_left_rect = row.next(label_left_w, CHILD_H);
    let button_rect = row.next(button_w, CHILD_H);
    let label_right_rect = row.next(label_right_w, CHILD_H);

    // ---- rect positions match Row's math --------------------

    // label_left_w = text("left  ")(36) + LABEL_HPAD(4) = 40.
    // button_w     = text("ok")(12)      + BUTTON_HPAD(16) = 28.
    // label_right_w = text("right   ")(48) + LABEL_HPAD(4) = 52.
    // Interior left = 5, interior y = 5, interior height = 30.
    // Vertical centring: (30 - 20)/2 = 5 → child_y = 5 + 5 = 10.
    assert_eq!(label_left_rect, Rect::new(5, 10, 40, 20));
    assert_eq!(
        button_rect,
        Rect::new(5 + 40 + 8, 10, 28, 20),
        "button placed after label_left + spacing",
    );
    assert_eq!(
        label_right_rect,
        Rect::new(5 + 40 + 8 + 28 + 8, 10, 52, 20),
        "label_right placed after button + spacing",
    );

    // ---- widgets drawn at those positions ----

    let mut canvas = Canvas::new(400, 40);
    canvas.clear(Color::rgb(0xf2, 0xf2, 0xf2)); // matches Theme::LIGHT.window_background

    let label_left = {
        let mut l = Label::new(label_left_rect, "left  ");
        l.set_alignment(Alignment::Left);
        l
    };
    let button = Button::new(button_rect, "ok");
    let label_right = {
        let mut l = Label::new(label_right_rect, "right   ");
        l.set_alignment(Alignment::Right);
        l
    };

    label_left.draw(&mut canvas);
    button.draw(&mut canvas);
    label_right.draw(&mut canvas);

    // ---- pixel-level spot-checks ------------------------------

    // Left-aligned label draws its first glyph at (rect.x, y-center).
    // 'l' is the first char of "left"; verify at least one of its
    // lit glyph pixels appears at the rect's left edge.
    let label_text = rgba(Theme::LIGHT.label_text);
    let left_text_y = label_left_rect.y + ((label_left_rect.height - GLYPH_HEIGHT) / 2) as i32;
    let glyph_l = glyph_for('l');
    let mut found_left_glyph = false;
    for row_i in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            if (glyph_l[row_i as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 == 0 {
                continue;
            }
            let x = label_left_rect.x + col as i32;
            let y = left_text_y + row_i as i32;
            assert_eq!(
                px(&canvas, x, y),
                label_text,
                "label_left 'l' glyph pixel at ({x}, {y})",
            );
            found_left_glyph = true;
        }
    }
    assert!(
        found_left_glyph,
        "label_left must draw at least one 'l' glyph pixel"
    );

    // Button's border corners should be at the button rect's corners
    // in the button_border theme colour.
    let button_border = rgba(Theme::LIGHT.button_border);
    assert_eq!(
        px(&canvas, button_rect.x, button_rect.y),
        button_border,
        "button top-left border corner",
    );
    assert_eq!(
        px(&canvas, button_rect.right() - 1, button_rect.y),
        button_border,
        "button top-right border corner",
    );
    assert_eq!(
        px(&canvas, button_rect.x, button_rect.bottom() - 1),
        button_border,
        "button bottom-left border corner",
    );
    assert_eq!(
        px(&canvas, button_rect.right() - 1, button_rect.bottom() - 1),
        button_border,
        "button bottom-right border corner",
    );

    // Button's centred caption "ok" — find a lit 'o' glyph pixel.
    // "ok" text width = 2 × CELL_WIDTH = 12. Button width = 28
    // (12 text + BUTTON_HPAD(16)). Centre x offset =
    // (28 - 12)/2 = 8 → origin at button_rect.x + 8.
    let button_text = rgba(Theme::LIGHT.button_text);
    let button_text_origin_x = button_rect.x + 8;
    let button_text_origin_y = button_rect.y + ((button_rect.height - GLYPH_HEIGHT) / 2) as i32;
    let glyph_o = glyph_for('o');
    let mut found_button_glyph = false;
    for row_i in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            if (glyph_o[row_i as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 == 0 {
                continue;
            }
            let x = button_text_origin_x + col as i32;
            let y = button_text_origin_y + row_i as i32;
            assert_eq!(px(&canvas, x, y), button_text);
            found_button_glyph = true;
        }
    }
    assert!(
        found_button_glyph,
        "button must draw at least one 'o' glyph pixel"
    );

    // Right-aligned label: "right   " is 48 px of text in a
    // 52-wide rect (label_right_w = 48 + LABEL_HPAD(4)). The
    // right-aligned x_offset is rect.width - text_width = 4,
    // which is exactly LABEL_HPAD — the pad becomes a gutter
    // on the *left* for a right-aligned preferred-sized label.
    let right_text_origin_x = label_right_rect.x + LABEL_HPAD as i32;
    let right_text_y = label_right_rect.y + ((label_right_rect.height - GLYPH_HEIGHT) / 2) as i32;
    let glyph_r = glyph_for('r');
    let mut found_right_glyph = false;
    for row_i in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            if (glyph_r[row_i as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 == 0 {
                continue;
            }
            let x = right_text_origin_x + col as i32;
            let y = right_text_y + row_i as i32;
            assert_eq!(
                px(&canvas, x, y),
                label_text,
                "label_right 'r' glyph pixel at ({x}, {y})",
            );
            found_right_glyph = true;
        }
    }
    assert!(
        found_right_glyph,
        "label_right must draw at least one 'r' glyph pixel"
    );

    // ---- nothing has been drawn in the padding gutter --------

    // The space between label_left and button is spacing wide,
    // filled only by the background colour.
    let bg = [0xf2, 0xf2, 0xf2, 0xff];
    let gap_x = label_left_rect.right() + (SPACING as i32) / 2;
    let gap_y = label_left_rect.y + 5;
    assert_eq!(
        px(&canvas, gap_x, gap_y),
        bg,
        "inter-child gap must be background"
    );

    // The top padding strip (y = 0) is also still background.
    assert_eq!(px(&canvas, 200, 0), bg);
}

#[test]
fn row_with_all_three_widgets_at_preferred_widths_still_fits_and_reports_remaining() {
    // Same layout as above; check that Row correctly reports
    // remaining after three preferred_size-driven placements so
    // a caller could reason about adding a fourth widget.
    let label_left = Label::new(Rect::new(0, 0, 0, 0), "left  ");
    let button = Button::new(Rect::new(0, 0, 0, 0), "ok");
    let label_right = Label::new(Rect::new(0, 0, 0, 0), "right   ");
    let (label_left_w, _) = label_left.preferred_size();
    let (button_w, _) = button.preferred_size();
    let (label_right_w, _) = label_right.preferred_size();

    let mut row = Row::new(Rect::new(0, 0, 400, 40), 5, 8);
    row.next(label_left_w, 20);
    row.next(button_w, 20);
    row.next(label_right_w, 20);

    // Consumed: 40 + 8 + 28 + 8 + 52 + 8 = 144 px (including a
    // trailing spacing the cursor always appends).
    assert_eq!(
        row.remaining(),
        390 - (label_left_w + 8 + button_w + 8 + label_right_w + 8),
    );
}
