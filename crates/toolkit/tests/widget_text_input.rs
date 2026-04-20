//! Isolation tests for `toolkit::widget::TextInput`.
//!
//! Drive `TextInput` directly against a native `Canvas`
//! with no display server or toolkit protocol client in the
//! loop (Principle X). Pixel-level paint assertions live at
//! the bottom of the file; the top covers the state machine
//! and the keyboard / pointer event surface.

use toolkit::draw::font::{glyph_for, CELL_WIDTH, GLYPH_HEIGHT, GLYPH_WIDTH};
use toolkit::draw::{Canvas, Color, Rect};
use toolkit::theme::Theme;
use toolkit::widget::text_input::{
    Key, KeyOutcome, TextInput, TextInputState, TEXT_INPUT_PADDING_X,
};

// ---- helpers -------------------------------------------------------

fn rgba(color: Color) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

fn px(canvas: &Canvas, x: u32, y: u32) -> [u8; 4] {
    let slice = canvas.pixel(x, y).expect("pixel in bounds");
    [slice[0], slice[1], slice[2], slice[3]]
}

// ---- construction + accessors -------------------------------------

#[test]
fn text_input_new_has_empty_text_idle_state() {
    let input = TextInput::new(Rect::new(10, 20, 120, 18));
    assert_eq!(input.text(), "");
    assert_eq!(input.caret(), 0);
    assert_eq!(input.state(), TextInputState::Idle);
    assert_eq!(input.bounds(), Rect::new(10, 20, 120, 18));
    assert_eq!(input.placeholder(), "");
}

#[test]
fn with_placeholder_stores_placeholder_without_touching_text() {
    let input = TextInput::new(Rect::new(0, 0, 120, 18))
        .with_placeholder("type here");
    assert_eq!(input.placeholder(), "type here");
    assert_eq!(input.text(), "");
    assert_eq!(input.state(), TextInputState::Idle);
}

#[test]
fn text_input_set_text_positions_caret_at_end() {
    let mut input = TextInput::new(Rect::new(0, 0, 120, 18));
    input.set_text("hello");
    assert_eq!(input.text(), "hello");
    assert_eq!(input.caret(), 5);
    // Overwriting short text re-pins the caret at the new end.
    input.set_text("hi");
    assert_eq!(input.text(), "hi");
    assert_eq!(input.caret(), 2);
}

#[test]
fn text_input_clear_empties_text_and_resets_caret() {
    let mut input = TextInput::new(Rect::new(0, 0, 120, 18));
    input.set_text("hello");
    input.clear();
    assert_eq!(input.text(), "");
    assert_eq!(input.caret(), 0);
}

// ---- hit-test + focus transition ----------------------------------

#[test]
fn text_input_pointer_down_inside_transitions_to_focused() {
    let mut input = TextInput::new(Rect::new(10, 20, 120, 18));
    input.set_text("hello");
    assert_eq!(input.state(), TextInputState::Idle);

    // Press in the middle of the widget.
    assert!(input.pointer_down(70, 29));
    assert_eq!(input.state(), TextInputState::Focused);
    // Caret snaps to the end of the text.
    assert_eq!(input.caret(), 5);
}

#[test]
fn text_input_pointer_down_outside_does_not_change_state() {
    let mut input = TextInput::new(Rect::new(10, 20, 120, 18));
    input.set_text("hello");

    assert!(!input.pointer_down(0, 0));
    assert_eq!(input.state(), TextInputState::Idle);
    // Caret was not at the end before; still unchanged after a miss.
    assert_eq!(input.caret(), 5, "set_text pins caret at end; miss does not touch it");

    // A miss while focused must still not transition back to Idle.
    input.set_focus(true);
    assert!(!input.pointer_down(200, 200));
    assert_eq!(input.state(), TextInputState::Focused);
}

#[test]
fn hit_test_returns_false_for_empty_bounds() {
    let input = TextInput::new(Rect::new(5, 5, 0, 0));
    assert!(!input.hit_test(5, 5));
    assert!(!input.hit_test(0, 0));
}

// ---- state setters -------------------------------------------------

#[test]
fn set_focus_and_set_hover_drive_state_correctly() {
    let mut input = TextInput::new(Rect::new(0, 0, 120, 18));
    input.set_focus(true);
    assert_eq!(input.state(), TextInputState::Focused);
    input.set_focus(false);
    assert_eq!(input.state(), TextInputState::Idle);

    input.set_hover(true);
    assert_eq!(input.state(), TextInputState::Hover);
    input.set_hover(false);
    assert_eq!(input.state(), TextInputState::Idle);

    // Hover must not clobber focus — focus has precedence.
    input.set_focus(true);
    input.set_hover(true);
    assert_eq!(input.state(), TextInputState::Focused);
    input.set_hover(false);
    assert_eq!(input.state(), TextInputState::Focused);
}

// ---- key handling: insertion / deletion / navigation --------------

#[test]
fn text_input_handle_key_inserts_printable_char() {
    let mut input = TextInput::new(Rect::new(0, 0, 120, 18));
    input.set_focus(true);

    assert_eq!(input.handle_key(Key::Char('h')), KeyOutcome::Changed);
    assert_eq!(input.handle_key(Key::Char('i')), KeyOutcome::Changed);
    assert_eq!(input.text(), "hi");
    assert_eq!(input.caret(), 2);

    // Insert in the middle after moving caret.
    input.handle_key(Key::Left);
    assert_eq!(input.handle_key(Key::Char('!')), KeyOutcome::Changed);
    assert_eq!(input.text(), "h!i");
    assert_eq!(input.caret(), 2);
}

#[test]
fn handle_key_ignores_non_printable_chars() {
    let mut input = TextInput::new(Rect::new(0, 0, 120, 18));
    input.set_focus(true);
    // NUL
    assert_eq!(input.handle_key(Key::Char('\u{0}')), KeyOutcome::Ignored);
    // Newline (0x0A, control code)
    assert_eq!(input.handle_key(Key::Char('\n')), KeyOutcome::Ignored);
    // Tab (0x09, control code — explicitly disallowed per the spec)
    assert_eq!(input.handle_key(Key::Char('\t')), KeyOutcome::Ignored);
    // DEL (0x7F)
    assert_eq!(input.handle_key(Key::Char('\u{7f}')), KeyOutcome::Ignored);
    // Non-ASCII (not in the bitmap font)
    assert_eq!(input.handle_key(Key::Char('é')), KeyOutcome::Ignored);
    assert_eq!(input.text(), "");
    assert_eq!(input.caret(), 0);
}

#[test]
fn handle_key_when_not_focused_is_ignored() {
    let mut input = TextInput::new(Rect::new(0, 0, 120, 18));
    assert_eq!(input.state(), TextInputState::Idle);
    assert_eq!(input.handle_key(Key::Char('x')), KeyOutcome::Ignored);
    assert_eq!(input.text(), "");
}

#[test]
fn text_input_handle_key_backspace_deletes_char_before_caret() {
    let mut input = TextInput::new(Rect::new(0, 0, 120, 18));
    input.set_focus(true);
    input.set_text("hello");
    assert_eq!(input.caret(), 5);

    assert_eq!(input.handle_key(Key::Backspace), KeyOutcome::Changed);
    assert_eq!(input.text(), "hell");
    assert_eq!(input.caret(), 4);

    // Move to start, backspace is a no-op there.
    input.handle_key(Key::Home);
    assert_eq!(input.handle_key(Key::Backspace), KeyOutcome::Ignored);
    assert_eq!(input.text(), "hell");
    assert_eq!(input.caret(), 0);

    // Backspace from the middle.
    input.handle_key(Key::Right);
    input.handle_key(Key::Right);
    assert_eq!(input.caret(), 2);
    assert_eq!(input.handle_key(Key::Backspace), KeyOutcome::Changed);
    assert_eq!(input.text(), "hll");
    assert_eq!(input.caret(), 1);
}

#[test]
fn text_input_handle_key_arrows_move_caret() {
    let mut input = TextInput::new(Rect::new(0, 0, 120, 18));
    input.set_focus(true);
    input.set_text("abc");
    assert_eq!(input.caret(), 3);

    // Right at end is ignored.
    assert_eq!(input.handle_key(Key::Right), KeyOutcome::Ignored);
    assert_eq!(input.caret(), 3);

    // Left walks back one char at a time.
    assert_eq!(input.handle_key(Key::Left), KeyOutcome::CaretMoved);
    assert_eq!(input.caret(), 2);
    assert_eq!(input.handle_key(Key::Left), KeyOutcome::CaretMoved);
    assert_eq!(input.caret(), 1);
    assert_eq!(input.handle_key(Key::Left), KeyOutcome::CaretMoved);
    assert_eq!(input.caret(), 0);
    // Left at 0 is ignored.
    assert_eq!(input.handle_key(Key::Left), KeyOutcome::Ignored);
    assert_eq!(input.caret(), 0);

    // Right walks forward.
    assert_eq!(input.handle_key(Key::Right), KeyOutcome::CaretMoved);
    assert_eq!(input.caret(), 1);

    // Home jumps to 0.
    assert_eq!(input.handle_key(Key::Home), KeyOutcome::CaretMoved);
    assert_eq!(input.caret(), 0);
    // Home at 0 is ignored.
    assert_eq!(input.handle_key(Key::Home), KeyOutcome::Ignored);

    // End jumps to len.
    assert_eq!(input.handle_key(Key::End), KeyOutcome::CaretMoved);
    assert_eq!(input.caret(), 3);
    // End at end is ignored.
    assert_eq!(input.handle_key(Key::End), KeyOutcome::Ignored);
}

// ---- paint ---------------------------------------------------------

#[test]
fn text_input_paint_produces_deterministic_pixels_for_known_text() {
    // 60-wide × 18-tall bounds. Vertical centre of the 7-pixel
    // glyph inside the 16-pixel interior (height - 2 borders)
    // puts text_y at bounds.y + 1 + (16 - 7) / 2 = bounds.y + 5.
    // Text x starts at bounds.x + 1 (border) + TEXT_INPUT_PADDING_X (4).
    let theme = Theme::LIGHT;
    let bounds = Rect::new(10, 20, 60, 18);
    let mut canvas = Canvas::new(100, 60);
    let mut input = TextInput::new(bounds);
    input.set_focus(true);
    input.set_text("ab");
    input.paint(&mut canvas, &theme);

    // Border corners must be the border colour.
    let border = rgba(theme.text_input_border);
    assert_eq!(px(&canvas, 10, 20), border, "top-left border corner");
    assert_eq!(px(&canvas, 69, 20), border, "top-right border corner");
    assert_eq!(px(&canvas, 10, 37), border, "bottom-left border corner");
    assert_eq!(px(&canvas, 69, 37), border, "bottom-right border corner");

    // Fill (focused) is sampled in the middle of the interior,
    // away from text.
    let fill = rgba(theme.text_input_bg_focused);
    assert_eq!(px(&canvas, 50, 28), fill, "interior fill is focused colour");

    // The 'a' glyph should paint at (text_x, text_y) in
    // text_input_fg. text_x = 10 + 1 + 4 = 15; text_y = 20 + 5 = 25.
    let text_x: i32 = 10 + 1 + TEXT_INPUT_PADDING_X as i32;
    let text_y: i32 = 25;
    let fg = rgba(theme.text_input_fg);
    let glyph_a = glyph_for('a');
    let mut found_lit_a = false;
    for row in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            let on = (glyph_a[row as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 != 0;
            if !on {
                continue;
            }
            found_lit_a = true;
            let x = text_x + col as i32;
            let y = text_y + row as i32;
            assert_eq!(px(&canvas, x as u32, y as u32), fg, "'a' glyph ({col},{row})");
        }
    }
    assert!(found_lit_a, "'a' has lit pixels");

    // Caret: 2 chars before caret → caret_x = text_x + 2 * CELL_WIDTH = 15 + 12 = 27.
    // 1-pixel vertical bar spans rows text_y..text_y + GLYPH_HEIGHT = 25..32.
    let caret_x = text_x + (2 * CELL_WIDTH) as i32;
    for row in 0..GLYPH_HEIGHT {
        assert_eq!(
            px(&canvas, caret_x as u32, (text_y + row as i32) as u32),
            fg,
            "caret bar pixel at row {row}",
        );
    }

    // A pixel one column left of the caret, on the same rows,
    // should still be the fill (not touched by caret or glyph).
    // caret_x - 1 = 26; check at y = 26 where 'b' glyph has no
    // lit pixel.
    // 'b' glyph row 0 is 0b00000, so (caret_x - 1, text_y) is fill.
    assert_eq!(px(&canvas, (caret_x - 1) as u32, text_y as u32), fill);
}

#[test]
fn text_input_paint_on_idle_shows_placeholder() {
    let theme = Theme::LIGHT;
    let bounds = Rect::new(0, 0, 80, 18);
    let mut canvas = Canvas::new(80, 18);
    let input = TextInput::new(bounds).with_placeholder("go");
    input.paint(&mut canvas, &theme);

    // Fill is idle colour.
    let fill = rgba(theme.text_input_bg);
    assert_eq!(px(&canvas, 40, 9), fill);

    // Placeholder "go" paints at text_x = 1 + 4 = 5, text_y = 1 + (16-7)/2 = 5.
    let placeholder_fg = rgba(theme.text_input_placeholder_fg);
    let glyph_g = glyph_for('g');
    let text_x: i32 = 1 + TEXT_INPUT_PADDING_X as i32;
    let text_y: i32 = 5;
    for row in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            if (glyph_g[row as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 != 0 {
                let x = text_x + col as i32;
                let y = text_y + row as i32;
                assert_eq!(
                    px(&canvas, x as u32, y as u32),
                    placeholder_fg,
                    "'g' placeholder glyph ({col},{row})",
                );
                return;
            }
        }
    }
    panic!("'g' glyph has no lit pixels");
}

#[test]
fn text_input_paint_idle_vs_hover_differ_in_fill_color() {
    let theme = Theme::LIGHT;
    let bounds = Rect::new(0, 0, 40, 18);

    let mut canvas_idle = Canvas::new(40, 18);
    let input_idle = TextInput::new(bounds);
    input_idle.paint(&mut canvas_idle, &theme);

    let mut canvas_hover = Canvas::new(40, 18);
    let mut input_hover = TextInput::new(bounds);
    input_hover.set_hover(true);
    input_hover.paint(&mut canvas_hover, &theme);

    // Sample a pixel inside the interior fill.
    let sample_idle = px(&canvas_idle, 20, 9);
    let sample_hover = px(&canvas_hover, 20, 9);
    assert_eq!(sample_idle, rgba(theme.text_input_bg));
    assert_eq!(sample_hover, rgba(theme.text_input_bg_hover));
}

#[test]
fn text_input_paint_no_caret_when_not_focused() {
    let theme = Theme::LIGHT;
    let bounds = Rect::new(0, 0, 80, 18);
    let mut canvas = Canvas::new(80, 18);
    let mut input = TextInput::new(bounds);
    input.set_text("ab");
    // State is Idle — caret should not appear. Caret would be at
    // text_x + 2 * CELL_WIDTH = 5 + 12 = 17.
    input.paint(&mut canvas, &theme);

    let fg = rgba(theme.text_input_fg);
    // Find a row in the glyph band where neither 'a' nor 'b' nor
    // caret would paint — use (17, 5) which is column 17 at row 5
    // (the topmost glyph row). 'a' row 0 is 0b00000 and 'b' row 0
    // is 0b10000. Column 17 is two cells over from text_x=5; in the
    // 'b' cell at local col 17-5-6 = 6... wait, 'b' is in the 2nd
    // cell starting at x=5+6=11; so x=17 is the 3rd cell (local
    // col 0). That cell is empty. On any row, (17, y) is fill.
    let fill = rgba(theme.text_input_bg);
    for row in 0..GLYPH_HEIGHT {
        assert_eq!(
            px(&canvas, 17, (5 + row) as u32),
            fill,
            "no caret when not focused (row {row})",
        );
    }
    // Sanity: the 'a' glyph still paints in text_input_fg.
    let glyph_a = glyph_for('a');
    for row in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            if (glyph_a[row as usize] >> (GLYPH_WIDTH - 1 - col)) & 1 != 0 {
                let x = 5 + col as i32;
                let y = 5 + row as i32;
                assert_eq!(px(&canvas, x as u32, y as u32), fg);
                return;
            }
        }
    }
    panic!("'a' has no lit pixels");
}

#[test]
fn text_input_paint_zero_bounds_does_not_panic() {
    let theme = Theme::LIGHT;
    let mut canvas = Canvas::new(40, 40);
    let input = TextInput::new(Rect::new(5, 5, 0, 0));
    input.paint(&mut canvas, &theme);
    assert!(canvas.pixels().iter().all(|b| *b == 0));
}
