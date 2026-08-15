//! Isolation tests for `toolkit::widget::List`.
//!
//! Drive `List` directly against a native `Canvas` with no display
//! server in the loop (Principle X). The keyboard + pointer surface is
//! exercised via the public `handle_key` / `on_pointer_down` methods;
//! paint assertions sample pixels inside specific rows.

use toolkit::draw::font::GLYPH_HEIGHT;
use toolkit::draw::{Canvas, Color, Rect};
use toolkit::theme::Theme;
use toolkit::widget::list::{List, ListKey, ListKeyOutcome, LIST_HPAD, LIST_ROW_HEIGHT};

fn rgba(color: Color) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

fn px(canvas: &Canvas, x: u32, y: u32) -> [u8; 4] {
    let slice = canvas.pixel(x, y).expect("pixel in bounds");
    [slice[0], slice[1], slice[2], slice[3]]
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// ---- construction + accessors -------------------------------------

#[test]
fn list_new_has_no_selection_and_no_items() {
    let list = List::new();
    assert_eq!(list.selected(), None);
    assert!(list.items().is_empty());
    assert_eq!(list.scroll_offset(), 0);
    assert_eq!(list.selected_item(), None);
    assert_eq!(list.hover(), None);
}

#[test]
fn list_select_within_bounds() {
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    list.select(Some(1));
    assert_eq!(list.selected(), Some(1));
    assert_eq!(list.selected_item(), Some("b"));
}

#[test]
fn list_select_out_of_bounds_clamps() {
    // Documented behaviour: `select(Some(i))` clamps to
    // `items.len() - 1` (saturating). On an empty list, any Some
    // collapses to None.
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    list.select(Some(999));
    assert_eq!(list.selected(), Some(2), "clamped to last index");

    // Explicit None still sets to None.
    list.select(None);
    assert_eq!(list.selected(), None);

    // Empty list: Some → None.
    let mut empty = List::new();
    empty.select(Some(4));
    assert_eq!(empty.selected(), None);
}

#[test]
fn list_set_items_resets_scroll_and_selection() {
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    list.select(Some(2));
    list.set_items(strings(&["x", "y"]));
    assert_eq!(list.selected(), None);
    assert_eq!(list.scroll_offset(), 0);
}

// ---- pointer hit-test ---------------------------------------------

#[test]
fn list_pointer_hits_row() {
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    // Bounds 0,0,200,54 → rows at y 0..18, 18..36, 36..54.
    let bounds = Rect::new(0, 0, 200, 3 * LIST_ROW_HEIGHT);
    // Point at (50, 27) — inside row 1 (y in [18, 36)).
    assert_eq!(list.row_at((50, 27), bounds), Some(1));
    assert!(list.on_pointer_down((50, 27), bounds));
    assert_eq!(list.selected(), Some(1));
}

#[test]
fn list_pointer_outside_bounds_no_selection() {
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    let bounds = Rect::new(0, 0, 200, 3 * LIST_ROW_HEIGHT);
    assert!(!list.on_pointer_down((50, 200), bounds));
    assert_eq!(list.selected(), None);
}

#[test]
fn list_pointer_past_last_item_returns_none() {
    // Bounds hold 5 rows but only 3 items — clicking row 4 hits no
    // data row.
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    let bounds = Rect::new(0, 0, 200, 5 * LIST_ROW_HEIGHT);
    // y = 4 * 18 + 5 = 77 — row index 4 inside view, but items.len() is 3.
    assert_eq!(list.row_at((50, 77), bounds), None);
    assert!(!list.on_pointer_down((50, 77), bounds));
    assert_eq!(list.selected(), None);
}

// ---- keyboard ------------------------------------------------------

#[test]
fn list_key_down_moves_selection_forward() {
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    list.select(Some(0));
    let out = list.handle_key(ListKey::Down, 3);
    assert_eq!(list.selected(), Some(1));
    assert_eq!(out, ListKeyOutcome::SelectionChanged);
}

#[test]
fn list_key_down_at_end_is_ignored() {
    // Documented behaviour: Down at the last row is a no-op that
    // returns `Ignored`. Callers that want "wrap to top" compose
    // `select(Some(0))` themselves.
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    list.select(Some(2));
    let out = list.handle_key(ListKey::Down, 3);
    assert_eq!(list.selected(), Some(2));
    assert_eq!(out, ListKeyOutcome::Ignored);
}

#[test]
fn list_key_up_at_start_is_ignored() {
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    list.select(Some(0));
    assert_eq!(list.handle_key(ListKey::Up, 3), ListKeyOutcome::Ignored);
    assert_eq!(list.selected(), Some(0));
}

#[test]
fn list_key_home_end_jump_to_bounds() {
    let mut list = List::new().with_items(strings(&["a", "b", "c", "d"]));
    list.select(Some(1));

    assert_eq!(
        list.handle_key(ListKey::End, 4),
        ListKeyOutcome::SelectionChanged
    );
    assert_eq!(list.selected(), Some(3));

    assert_eq!(
        list.handle_key(ListKey::Home, 4),
        ListKeyOutcome::SelectionChanged
    );
    assert_eq!(list.selected(), Some(0));
}

#[test]
fn list_key_enter_returns_activated() {
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    list.select(Some(0));
    let out = list.handle_key(ListKey::Enter, 3);
    assert_eq!(out, ListKeyOutcome::Activated);
    assert_eq!(
        list.selected(),
        Some(0),
        "Enter does not move the selection"
    );
}

#[test]
fn list_key_enter_without_selection_is_ignored() {
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    let out = list.handle_key(ListKey::Enter, 3);
    assert_eq!(out, ListKeyOutcome::Ignored);
}

#[test]
fn list_scroll_follows_selection_out_of_visible_window() {
    let items: Vec<String> = (0..20).map(|i| format!("item{i}")).collect();
    let mut list = List::new().with_items(items);
    list.select(Some(0));
    assert_eq!(list.scroll_offset(), 0);

    // Move down 6 times with 5 visible rows: 0 → 1 → 2 → 3 → 4 → 5 → 6.
    // scroll_offset must advance to keep the selection in view.
    // After row 5 (0-indexed), visible window is [0,5). sel=5 must
    // push scroll_offset to 1 so window becomes [1,6) and contains 5.
    for _ in 0..6 {
        list.handle_key(ListKey::Down, 5);
    }
    assert_eq!(list.selected(), Some(6));
    let offset = list.scroll_offset();
    assert!(
        offset > 0,
        "scroll_offset must advance so the selection stays visible (got {offset})"
    );
    assert!(
        list.selected().unwrap() >= offset && list.selected().unwrap() < offset + 5,
        "selection must sit inside [scroll_offset, scroll_offset + visible_rows)"
    );
}

#[test]
fn list_pageup_pagedown_scroll_only() {
    let items: Vec<String> = (0..20).map(|i| format!("item{i}")).collect();
    let mut list = List::new().with_items(items);
    list.select(Some(0));

    // PageDown scrolls by visible_rows (5) without changing the selection.
    let out = list.handle_key(ListKey::PageDown, 5);
    assert_eq!(out, ListKeyOutcome::ScrolledOnly);
    assert_eq!(list.selected(), Some(0), "PageDown must not move selection");
    assert_eq!(list.scroll_offset(), 5);

    // PageUp scrolls back.
    let out = list.handle_key(ListKey::PageUp, 5);
    assert_eq!(out, ListKeyOutcome::ScrolledOnly);
    assert_eq!(list.scroll_offset(), 0);
}

#[test]
fn list_key_on_empty_list_is_ignored() {
    let mut list = List::new();
    for key in [
        ListKey::Up,
        ListKey::Down,
        ListKey::Home,
        ListKey::End,
        ListKey::PageUp,
        ListKey::PageDown,
        ListKey::Enter,
    ] {
        assert_eq!(list.handle_key(key, 3), ListKeyOutcome::Ignored);
    }
}

// ---- paint ---------------------------------------------------------

#[test]
fn list_paint_selected_row_uses_accent_color() {
    let theme = Theme::LIGHT;
    let bounds = Rect::new(0, 0, 200, 3 * LIST_ROW_HEIGHT);
    let mut canvas = Canvas::new(200, 3 * LIST_ROW_HEIGHT);
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    list.select(Some(1));
    list.paint(&mut canvas, bounds, &theme);

    // Row 1 is at y 18..36. Sample a pixel comfortably inside the
    // row but past the glyph column so we're looking at pure fill.
    // LIST_HPAD=6 and the "b" glyph occupies ~6 px; sample at x=100.
    let accent = rgba(theme.button_fill_pressed);
    assert_eq!(
        px(&canvas, 100, 27),
        accent,
        "selected row 1 must be accent colour"
    );

    // Row 0 has no fill — it should still be the zero-initialised
    // canvas background (transparent black).
    assert_eq!(px(&canvas, 100, 5), [0, 0, 0, 0], "row 0 is not selected");
}

#[test]
fn list_paint_hover_row_uses_hover_color() {
    let theme = Theme::LIGHT;
    let bounds = Rect::new(0, 0, 200, 3 * LIST_ROW_HEIGHT);
    let mut canvas = Canvas::new(200, 3 * LIST_ROW_HEIGHT);
    let mut list = List::new().with_items(strings(&["a", "b", "c"]));
    list.set_hover(Some(2));
    list.paint(&mut canvas, bounds, &theme);

    // Row 2 is at y 36..54.
    let hover = rgba(theme.button_fill_hover);
    assert_eq!(
        px(&canvas, 100, 45),
        hover,
        "hover row 2 must be hover colour"
    );
}

#[test]
fn list_paint_draws_row_text() {
    let theme = Theme::LIGHT;
    let bounds = Rect::new(0, 0, 200, 3 * LIST_ROW_HEIGHT);
    let mut canvas = Canvas::new(200, 3 * LIST_ROW_HEIGHT);
    let list = List::new().with_items(strings(&["a", "b", "c"]));
    list.paint(&mut canvas, bounds, &theme);

    // Find a lit pixel inside row 0's text strip and confirm it's
    // the label_text colour. Text top-y = (LIST_ROW_HEIGHT - GLYPH_HEIGHT) / 2
    // = (18 - 7) / 2 = 5. Text x starts at LIST_HPAD = 6.
    let fg = rgba(theme.label_text);
    let text_y_top: u32 = (LIST_ROW_HEIGHT - GLYPH_HEIGHT) / 2;
    let mut found_any_lit = false;
    // Glyph area for "a": x in [6, 11], y in [5, 12).
    'outer: for y in text_y_top..(text_y_top + GLYPH_HEIGHT) {
        for x in LIST_HPAD..(LIST_HPAD + 6) {
            if px(&canvas, x, y) == fg {
                found_any_lit = true;
                break 'outer;
            }
        }
    }
    assert!(found_any_lit, "'a' glyph must paint at least one lit pixel");
}

#[test]
fn list_paint_empty_items_paints_nothing() {
    let theme = Theme::LIGHT;
    let mut canvas = Canvas::new(200, 100);
    let list = List::new();
    list.paint(&mut canvas, Rect::new(0, 0, 200, 100), &theme);

    assert!(
        canvas.pixels().iter().all(|b| *b == 0),
        "an empty list must not touch any pixel"
    );
}

#[test]
fn list_paint_zero_bounds_does_not_panic() {
    let theme = Theme::LIGHT;
    let mut canvas = Canvas::new(40, 40);
    let list = List::new().with_items(strings(&["a"]));
    list.paint(&mut canvas, Rect::new(0, 0, 0, 0), &theme);
    assert!(canvas.pixels().iter().all(|b| *b == 0));
}
