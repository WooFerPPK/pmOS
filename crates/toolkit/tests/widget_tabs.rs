//! Isolation tests for `toolkit::widget::TabStrip`.
//!
//! These tests exercise layout, pointer/keyboard selection, and paint directly
//! against a native Canvas with no display server in the loop (Principle X).

use toolkit::draw::{Canvas, Color, Rect};
use toolkit::theme::Theme;
use toolkit::widget::{TabKey, TabKeyOutcome, TabStrip, TAB_STRIP_ACCENT_HEIGHT, TAB_STRIP_HEIGHT};

const LABELS: [&str; 3] = ["General", "Display", "About"];

fn strip() -> TabStrip<'static> {
    TabStrip::new(&LABELS)
}

fn rgba(color: Color) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

fn px(canvas: &Canvas<'_>, x: u32, y: u32) -> [u8; 4] {
    let pixel = canvas.pixel(x, y).expect("pixel in bounds");
    [pixel[0], pixel[1], pixel[2], pixel[3]]
}

#[test]
fn labels_and_empty_state_are_exposed_without_ownership() {
    let tabs = strip();
    assert_eq!(tabs.labels(), LABELS.as_slice());
    assert_eq!(tabs.len(), 3);
    assert!(!tabs.is_empty());

    let empty = TabStrip::new(&[]);
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
}

#[test]
fn tab_bounds_are_equal_width_and_final_tab_absorbs_remainder() {
    let bounds = Rect::new(5, 7, 101, TAB_STRIP_HEIGHT);
    assert_eq!(strip().tab_bounds(0, bounds), Some(Rect::new(5, 7, 33, 24)));
    assert_eq!(
        strip().tab_bounds(1, bounds),
        Some(Rect::new(38, 7, 33, 24))
    );
    assert_eq!(
        strip().tab_bounds(2, bounds),
        Some(Rect::new(71, 7, 35, 24))
    );
    assert_eq!(strip().tab_bounds(3, bounds), None);
}

#[test]
fn pointer_hit_test_uses_exclusive_edges_and_rejects_outside_points() {
    let bounds = Rect::new(5, 7, 101, TAB_STRIP_HEIGHT);
    let tabs = strip();

    assert_eq!(tabs.tab_at((5, 7), bounds), Some(0));
    assert_eq!(tabs.tab_at((37, 30), bounds), Some(0));
    assert_eq!(tabs.tab_at((38, 8), bounds), Some(1));
    assert_eq!(tabs.on_pointer_down((105, 30), bounds), Some(2));

    assert_eq!(tabs.tab_at((4, 8), bounds), None, "left of strip");
    assert_eq!(tabs.tab_at((106, 8), bounds), None, "right edge");
    assert_eq!(tabs.tab_at((10, 6), bounds), None, "above strip");
    assert_eq!(tabs.tab_at((10, 31), bounds), None, "bottom edge");
}

#[test]
fn empty_geometry_or_empty_labels_never_hit() {
    let tabs = strip();
    assert_eq!(tabs.tab_at((0, 0), Rect::new(0, 0, 0, 24)), None);
    assert_eq!(tabs.tab_bounds(0, Rect::new(0, 0, 0, 24)), None);

    let empty = TabStrip::new(&[]);
    assert_eq!(empty.tab_at((1, 1), Rect::new(0, 0, 50, 24)), None);
    assert_eq!(empty.tab_bounds(0, Rect::new(0, 0, 50, 24)), None);
}

#[test]
fn next_and_previous_keyboard_navigation_wrap() {
    let tabs = strip();
    assert_eq!(
        tabs.handle_key(Some(1), TabKey::Next),
        TabKeyOutcome::SelectionChanged(2)
    );
    assert_eq!(
        tabs.handle_key(Some(2), TabKey::Next),
        TabKeyOutcome::SelectionChanged(0)
    );
    assert_eq!(
        tabs.handle_key(Some(0), TabKey::Previous),
        TabKeyOutcome::SelectionChanged(2)
    );
}

#[test]
fn edge_keys_and_missing_selection_are_deterministic() {
    let tabs = strip();
    assert_eq!(
        tabs.handle_key(Some(2), TabKey::Home),
        TabKeyOutcome::SelectionChanged(0)
    );
    assert_eq!(
        tabs.handle_key(Some(0), TabKey::Home),
        TabKeyOutcome::Ignored
    );
    assert_eq!(
        tabs.handle_key(Some(0), TabKey::End),
        TabKeyOutcome::SelectionChanged(2)
    );
    assert_eq!(
        tabs.handle_key(None, TabKey::Next),
        TabKeyOutcome::SelectionChanged(0)
    );
    assert_eq!(
        tabs.handle_key(Some(99), TabKey::Previous),
        TabKeyOutcome::SelectionChanged(2)
    );
    assert_eq!(
        TabStrip::new(&[]).handle_key(None, TabKey::Next),
        TabKeyOutcome::Ignored
    );
}

#[test]
fn paint_uses_theme_surfaces_border_and_selected_accent() {
    let mut canvas = Canvas::new(90, TAB_STRIP_HEIGHT);
    strip().paint(
        &mut canvas,
        Rect::new(0, 0, 90, TAB_STRIP_HEIGHT),
        Some(1),
        &Theme::LIGHT,
    );

    assert_eq!(px(&canvas, 2, 10), rgba(Theme::LIGHT.button_fill));
    assert_eq!(px(&canvas, 32, 10), rgba(Theme::LIGHT.window_background));
    assert_eq!(
        px(&canvas, 45, TAB_STRIP_ACCENT_HEIGHT - 1),
        rgba(Theme::LIGHT.border_active)
    );
    assert_eq!(
        px(&canvas, 45, TAB_STRIP_HEIGHT - 1),
        rgba(Theme::LIGHT.window_background),
        "selected tab opens into the content surface"
    );
    assert_eq!(
        px(&canvas, 15, 0),
        rgba(Theme::LIGHT.button_border),
        "inactive tab retains its border"
    );
}

#[test]
fn paint_clips_long_captions_and_handles_invalid_selection() {
    let labels = ["A caption much wider than its tab", "B"];
    let tabs = TabStrip::new(&labels);
    let mut canvas = Canvas::new(40, TAB_STRIP_HEIGHT);
    tabs.paint(
        &mut canvas,
        Rect::new(0, 0, 40, TAB_STRIP_HEIGHT),
        Some(99),
        &Theme::DARK,
    );

    assert_eq!(px(&canvas, 2, 10), rgba(Theme::DARK.button_fill));
    assert_eq!(px(&canvas, 22, 10), rgba(Theme::DARK.button_fill));
}
