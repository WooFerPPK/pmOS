//! Isolation tests for `toolkit::widget::WindowFrame`.
//!
//! These tests construct a `WindowFrame` against a native
//! `Canvas` with no display server in the loop, then assert
//! on geometry, rendered pixels, hit-testing, and the close
//! callback. A future slice will add widget tests for
//! `Label`, `Button`, and the layout primitives.

use std::cell::Cell;
use std::rc::Rc;

use toolkit::draw::font::CELL_WIDTH;
use toolkit::draw::{Canvas, Color, Rect};
use toolkit::theme::Theme;
use toolkit::widget::frame::{
    PointerOutcome, WindowFrame, BORDER_WIDTH, CLOSE_BUTTON_MARGIN, CLOSE_BUTTON_SIZE,
    TITLEBAR_HEIGHT, TITLE_TEXT_MARGIN_X, TITLE_TEXT_TRAILING_GAP, WINDOW_CONTROL_WIDTH,
};

fn rgba(color: Color) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

fn px(canvas: &Canvas, x: u32, y: u32) -> [u8; 4] {
    let slice = canvas.pixel(x, y).expect("pixel in bounds");
    [slice[0], slice[1], slice[2], slice[3]]
}

// ---- geometry ------------------------------------------------------

#[test]
fn frame_draws_border_at_correct_bounds() {
    let mut canvas = Canvas::new(200, 150);
    let frame = WindowFrame::new(Rect::new(10, 20, 100, 80), "pmos.term");
    frame.draw(&mut canvas);

    let border = rgba(Theme::LIGHT.border_active);

    // Four corners of the border.
    assert_eq!(px(&canvas, 10, 20), border, "top-left corner");
    assert_eq!(px(&canvas, 109, 20), border, "top-right corner");
    assert_eq!(px(&canvas, 10, 99), border, "bottom-left corner");
    assert_eq!(px(&canvas, 109, 99), border, "bottom-right corner");

    // Midpoints of each edge.
    assert_eq!(px(&canvas, 60, 20), border, "top edge midpoint");
    assert_eq!(px(&canvas, 60, 99), border, "bottom edge midpoint");
    assert_eq!(px(&canvas, 10, 60), border, "left edge midpoint");
    assert_eq!(px(&canvas, 109, 60), border, "right edge midpoint");

    // A pixel outside the frame is still zero.
    assert_eq!(px(&canvas, 5, 5), [0, 0, 0, 0]);
    // A pixel one row below the titlebar (inside the content area) is
    // neither the border colour nor the titlebar fill — it was never
    // touched.
    assert_eq!(px(&canvas, 60, 60), [0, 0, 0, 0]);
}

#[test]
fn content_rect_is_interior_minus_border_and_titlebar() {
    let frame = WindowFrame::new(Rect::new(10, 20, 100, 80), "x");
    let content = frame.content_rect();
    assert_eq!(content.x, 10 + BORDER_WIDTH as i32);
    assert_eq!(content.y, 20 + TITLEBAR_HEIGHT as i32);
    assert_eq!(content.width, 100 - 2 * BORDER_WIDTH);
    assert_eq!(content.height, 80 - TITLEBAR_HEIGHT - BORDER_WIDTH);
}

// ---- titlebar text -------------------------------------------------

#[test]
fn titlebar_renders_app_id_text() {
    let mut canvas = Canvas::new(300, 100);
    let frame = WindowFrame::new(Rect::new(0, 0, 300, 100), "pmos.term");
    frame.draw(&mut canvas);

    let text_color = rgba(Theme::LIGHT.titlebar_text_active);
    let titlebar_fill = rgba(Theme::LIGHT.titlebar_active);

    // Scan the left half of the titlebar area (below the top border,
    // above the titlebar bottom) and count pixels painted in the
    // title-text colour. A real bitmap font draws dozens of glyph
    // pixels for a 9-character string; any count > 10 rules out the
    // test matching a single stray pixel.
    let mut text_pixels = 0u32;
    let mut fill_pixels = 0u32;
    for y in BORDER_WIDTH..TITLEBAR_HEIGHT {
        for x in 0..120 {
            let p = px(&canvas, x, y);
            if p == text_color {
                text_pixels += 1;
            } else if p == titlebar_fill {
                fill_pixels += 1;
            }
        }
    }
    assert!(
        text_pixels > 10,
        "expected title-text pixels in titlebar, got {text_pixels}",
    );
    assert!(
        fill_pixels > text_pixels,
        "expected more titlebar fill than text pixels, got fill={fill_pixels} text={text_pixels}",
    );
}

// ---- close button --------------------------------------------------

#[test]
fn close_button_hit_test_inside_button_returns_true() {
    let frame = WindowFrame::new(Rect::new(0, 0, 200, 150), "x");
    let cb = frame.close_button_rect();
    let cx = cb.x + (cb.width as i32) / 2;
    let cy = cb.y + (cb.height as i32) / 2;
    assert!(frame.hit_test_close(cx, cy));
    // Top-left corner of the button is still inside.
    assert!(frame.hit_test_close(cb.x, cb.y));
    // One pixel past the bottom-right edge is outside (right/bottom
    // are exclusive).
    assert!(!frame.hit_test_close(cb.right(), cb.bottom()));
}

#[test]
fn close_button_hit_test_outside_button_returns_false() {
    let frame = WindowFrame::new(Rect::new(0, 0, 200, 150), "x");
    assert!(!frame.hit_test_close(-1, -1));
    assert!(!frame.hit_test_close(10, 75)); // content area
    assert!(!frame.hit_test_close(100, 10)); // titlebar, middle (not close button)
    assert!(!frame.hit_test_close(199, 149)); // bottom-right corner of window
    assert!(!frame.hit_test_close(0, 0)); // top-left corner of window
}

#[test]
fn close_callback_fires_on_button_press() {
    let fired = Rc::new(Cell::new(0u32));
    let fired_for_cb = Rc::clone(&fired);
    let mut frame = WindowFrame::new(Rect::new(0, 0, 200, 150), "x");
    frame.on_close(move || fired_for_cb.set(fired_for_cb.get() + 1));

    let cb = frame.close_button_rect();
    let cx = cb.x + (cb.width as i32) / 2;
    let cy = cb.y + (cb.height as i32) / 2;

    let outcome = frame.pointer_down(cx, cy);
    assert_eq!(outcome, PointerOutcome::Close);
    assert_eq!(fired.get(), 1);

    // A second press fires the callback again.
    let outcome = frame.pointer_down(cx, cy);
    assert_eq!(outcome, PointerOutcome::Close);
    assert_eq!(fired.get(), 2);
}

#[test]
fn close_callback_does_not_fire_on_titlebar_or_content_press() {
    let fired = Rc::new(Cell::new(false));
    let fired_for_cb = Rc::clone(&fired);
    let mut frame = WindowFrame::new(Rect::new(0, 0, 200, 150), "x");
    frame.on_close(move || fired_for_cb.set(true));

    // Titlebar (middle, not close button).
    assert_eq!(frame.pointer_down(40, 10), PointerOutcome::Titlebar);
    assert!(!fired.get());

    // Content area.
    assert_eq!(frame.pointer_down(50, 100), PointerOutcome::Content);
    assert!(!fired.get());

    // Outside the frame.
    assert_eq!(frame.pointer_down(400, 400), PointerOutcome::Outside);
    assert!(!fired.get());
}

#[test]
fn caption_controls_have_distinct_windows_style_hit_targets() {
    let mut frame = WindowFrame::new(Rect::new(0, 0, 300, 150), "x");
    let minimize = frame.minimize_button_rect();
    let maximize = frame.maximize_button_rect();
    let close = frame.close_button_rect();

    assert_eq!(minimize.width, WINDOW_CONTROL_WIDTH);
    assert_eq!(maximize.width, WINDOW_CONTROL_WIDTH);
    assert_eq!(close.width, WINDOW_CONTROL_WIDTH);
    assert_eq!(minimize.right(), maximize.x);
    assert_eq!(maximize.right(), close.x);

    assert_eq!(
        frame.pointer_down(minimize.x + 1, minimize.y + 1),
        PointerOutcome::Minimize
    );
    assert_eq!(
        frame.pointer_down(maximize.x + 1, maximize.y + 1),
        PointerOutcome::ToggleMaximize
    );
    assert_eq!(
        frame.pointer_down(close.x + 1, close.y + 1),
        PointerOutcome::Close
    );
}

#[test]
fn narrow_titlebar_keeps_close_and_drops_secondary_controls() {
    let frame = WindowFrame::new(Rect::new(0, 0, 80, 40), "Narrow");
    assert!(!frame.close_button_rect().is_empty());
    assert!(frame.maximize_button_rect().is_empty());
    assert!(frame.minimize_button_rect().is_empty());
    assert!(frame.visible_title_chars() > 0);
}

#[test]
fn app_mark_never_paints_under_the_narrow_close_target() {
    let mark_pixel = (6, (TITLEBAR_HEIGHT - 9) / 2);
    for (width, mark_visible) in [(31, true), (32, false), (45, false), (46, true)] {
        let mut canvas = Canvas::new(width, TITLEBAR_HEIGHT);
        let frame = WindowFrame::new(Rect::new(0, 0, width, TITLEBAR_HEIGHT), "x");
        frame.draw(&mut canvas);
        let expected = if mark_visible {
            rgba(Theme::LIGHT.border_active)
        } else {
            rgba(Theme::LIGHT.titlebar_active)
        };
        assert_eq!(
            px(&canvas, mark_pixel.0, mark_pixel.1),
            expected,
            "unexpected app-mark visibility at width {width}",
        );
    }
}

// ---- active vs inactive themes ------------------------------------

#[test]
fn active_and_inactive_themes_render_different_colors() {
    let mut c_active = Canvas::new(100, 50);
    let mut c_inactive = Canvas::new(100, 50);

    let mut active = WindowFrame::new(Rect::new(0, 0, 100, 50), "x");
    active.set_focused(true);
    let mut inactive = WindowFrame::new(Rect::new(0, 0, 100, 50), "x");
    inactive.set_focused(false);

    active.draw(&mut c_active);
    inactive.draw(&mut c_inactive);

    // A pixel inside the titlebar fill region, away from the title
    // text (x=60) and away from the close button (which starts at
    // 100 - 3 - 16 = 81). x=60 is safely in the middle of the
    // titlebar fill.
    let active_fill = px(&c_active, 60, 10);
    let inactive_fill = px(&c_inactive, 60, 10);
    assert_ne!(
        active_fill, inactive_fill,
        "titlebar fill should differ between focused and unfocused state",
    );
    assert_eq!(active_fill, rgba(Theme::LIGHT.titlebar_active));
    assert_eq!(inactive_fill, rgba(Theme::LIGHT.titlebar_inactive));

    // Border top edge should also differ.
    let active_border = px(&c_active, 50, 0);
    let inactive_border = px(&c_inactive, 50, 0);
    assert_ne!(active_border, inactive_border);
    assert_eq!(active_border, rgba(Theme::LIGHT.border_active));
    assert_eq!(inactive_border, rgba(Theme::LIGHT.border_inactive));
}

#[test]
fn close_button_hover_changes_fill_color() {
    let mut canvas = Canvas::new(200, 100);
    let mut frame = WindowFrame::new(Rect::new(0, 0, 200, 100), "x");

    frame.set_close_hover(false);
    frame.draw(&mut canvas);
    let cb = frame.close_button_rect();
    let inside_cx = cb.x + 2;
    let inside_cy = cb.y + 2;
    let resting = px(&canvas, inside_cx as u32, inside_cy as u32);
    assert_eq!(resting, rgba(Theme::LIGHT.close_button));

    let mut canvas = Canvas::new(200, 100);
    frame.set_close_hover(true);
    frame.draw(&mut canvas);
    let hovered = px(&canvas, inside_cx as u32, inside_cy as u32);
    assert_eq!(hovered, rgba(Theme::LIGHT.close_button_hover));
    assert_ne!(resting, hovered);
}

// ---- degenerate sizes ---------------------------------------------

#[test]
fn frame_with_zero_size_does_not_panic() {
    let mut canvas = Canvas::new(50, 50);

    // Zero-sized bounds: draw is a no-op.
    let zero = WindowFrame::new(Rect::new(5, 5, 0, 0), "x");
    zero.draw(&mut canvas);
    assert!(zero.titlebar_rect().is_empty());
    assert!(zero.close_button_rect().is_empty());
    assert!(zero.content_rect().is_empty());
    // The whole canvas is still zero.
    assert!(canvas.pixels().iter().all(|b| *b == 0));

    // 1x1 bounds: titlebar is 1 row tall (clamped from 22) but the
    // close button doesn't fit, content doesn't fit, and draw must
    // not panic.
    let tiny = WindowFrame::new(Rect::new(10, 10, 1, 1), "x");
    tiny.draw(&mut canvas);
    assert_eq!(tiny.titlebar_rect().height, 1);
    assert!(tiny.close_button_rect().is_empty());
    assert!(tiny.content_rect().is_empty());

    // A width narrower than the close-button footprint: still draws
    // the border without panicking, and hit_test_close returns
    // false for every point in bounds.
    let narrow = WindowFrame::new(Rect::new(0, 0, 10, 40), "x");
    narrow.draw(&mut canvas);
    assert!(narrow.close_button_rect().is_empty());
    assert!(!narrow.hit_test_close(5, 10));
}

// ---- title text clipping ------------------------------------------

#[test]
fn titlebar_text_clipped_when_app_id_too_long_for_titlebar() {
    let long = "pmos.terminal.with.a.very.long.app.identifier";
    let narrow = WindowFrame::new(Rect::new(0, 0, 80, 40), long);

    let total = long.chars().count();
    let visible = narrow.visible_title_chars();
    assert!(visible > 0, "expected at least some visible chars");
    assert!(
        visible < total,
        "expected clipping: visible={visible} total={total}",
    );

    // Geometry check: the drawn title must end before the close
    // button's left edge (with the required trailing gap).
    let close = narrow.close_button_rect();
    assert!(!close.is_empty());
    let title_start_x = narrow.bounds().x + TITLE_TEXT_MARGIN_X as i32;
    let title_end_x = title_start_x + (visible as u32 * CELL_WIDTH) as i32;
    assert!(
        title_end_x <= close.x - TITLE_TEXT_TRAILING_GAP as i32,
        "title end {title_end_x} must leave a {TITLE_TEXT_TRAILING_GAP}px gap before close button at {}",
        close.x,
    );
}

#[test]
fn titlebar_text_fully_fits_when_app_id_is_short() {
    let frame = WindowFrame::new(Rect::new(0, 0, 400, 40), "ab");
    assert_eq!(frame.visible_title_chars(), 2);
}

// ---- sanity: the constants are self-consistent --------------------

#[test]
fn close_button_fits_inside_titlebar() {
    // Caption buttons use the full titlebar interior rather than the old
    // inset square geometry.
    const {
        assert!(CLOSE_BUTTON_SIZE == WINDOW_CONTROL_WIDTH);
        assert!(CLOSE_BUTTON_MARGIN == 0);
        assert!(TITLEBAR_HEIGHT > BORDER_WIDTH);
    }
}

#[test]
fn focus_damage_regions_are_non_overlapping_chrome_only() {
    let frame = WindowFrame::new(Rect::new(10, 20, 100, 80), "Files");
    assert_eq!(
        frame.focus_damage_regions(),
        vec![
            Rect::new(10, 20, 100, 22),
            Rect::new(10, 42, 1, 57),
            Rect::new(109, 42, 1, 57),
            Rect::new(10, 99, 100, 1),
        ]
    );
}

#[test]
fn rasterized_frame_regions_match_full_canvas_crops() {
    let mut frame = WindowFrame::new(Rect::new(10, 20, 100, 80), "Settings");
    frame.set_theme(Theme::DARK);
    frame.set_focused(false);
    frame.set_maximized(true);
    let mut full = Canvas::new(130, 120);
    frame.draw(&mut full);

    for region in frame.focus_damage_regions() {
        let packed = frame
            .rasterize_region(region)
            .expect("frame-owned region rasterizes");
        let row_bytes = region.width as usize * 4;
        for row in 0..region.height as usize {
            for column in 0..region.width as usize {
                let packed_offset = row * row_bytes + column * 4;
                assert_eq!(
                    &packed[packed_offset..packed_offset + 4],
                    full.pixel(
                        (region.x as usize + column) as u32,
                        (region.y as usize + row) as u32,
                    )
                    .expect("full crop pixel"),
                );
            }
        }
    }
}
