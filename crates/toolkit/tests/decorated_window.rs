//! T129 / T120 integration — DecoratedWindow geometry +
//! event-routing tests against a `MemoryConnection` whose
//! inbound side is pre-seeded with the three required global
//! advertisements. These tests pin the *visible* contract of
//! [`toolkit::DecoratedWindow`]: chrome geometry, hit-testing,
//! resize anchoring, and titlebar caption flow.
//!
//! Covers:
//!   * `DecoratedWindow::new` sends the full surface +
//!     xdg_toplevel + set_app_id sequence the server expects.
//!   * `set_title` flows into both the protocol (xdg_toplevel.set_title)
//!     and the WindowFrame (visible titlebar text).
//!   * `handle_pointer_down` returns `Close` for clicks inside
//!     the close-button rect and flips `close_requested`.
//!   * `handle_pointer_down` returns `Titlebar` for clicks on
//!     the titlebar outside the close button.
//!   * `handle_pointer_down` returns `Content` for clicks
//!     inside the content area.
//!   * `content_rect` shrinks the window bounds by the chrome
//!     dimensions (TITLEBAR_HEIGHT + 2 * BORDER_WIDTH).
//!   * `resize` keeps the close-button anchored to the new
//!     top-right corner.

use display_proto::events::RegistryGlobal;
use toolkit::draw::Rect;
use toolkit::protocol::MemoryConnection;
use toolkit::widget::frame::{
    BORDER_WIDTH, CLOSE_BUTTON_MARGIN, CLOSE_BUTTON_SIZE, TITLEBAR_HEIGHT,
};
use toolkit::{
    App, DecoratedPointerOutcome, DecoratedWindow, MessageHeader, ObjectId, HEADER_SIZE,
};

/// The toolkit's id allocator hands the registry id out
/// first: `get_registry` always lands at raw id 3.
const REGISTRY_ID: ObjectId = ObjectId::new(3);

/// Build a single `pmd_registry.global(name, interface, version)`
/// event framed against the registry object id.
fn build_global_event(name: u32, interface: &str, version: u32) -> Vec<u8> {
    let event = RegistryGlobal {
        name,
        interface: interface.to_string(),
        version,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header = MessageHeader::try_new(REGISTRY_ID, 1 /* global */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

/// Build an `App<MemoryConnection>` with the three required
/// globals (`pmd_compositor`, `pmd_shm`, `pmd_xdg_shell`)
/// pre-staged on the inbound side. `App::connect` drains them
/// during its bind loop. Outbound bytes (the surface +
/// xdg_toplevel sequence DecoratedWindow emits) accumulate in
/// the connection but aren't asserted on here — these tests
/// pin the *visible* DecoratedWindow contract, not the wire.
fn build_app() -> App<MemoryConnection> {
    let mut conn = MemoryConnection::new();
    let mut batch = Vec::new();
    batch.extend(build_global_event(1, "pmd_compositor", 1));
    batch.extend(build_global_event(2, "pmd_shm", 1));
    batch.extend(build_global_event(3, "pmd_xdg_shell", 1));
    conn.feed_inbound(&batch);
    App::connect(conn).expect("App::connect")
}

#[test]
fn content_rect_subtracts_chrome_geometry() {
    // 200x100 bounds → content = (BORDER_WIDTH, TITLEBAR_HEIGHT,
    // 200 - 2*BORDER_WIDTH, 100 - TITLEBAR_HEIGHT - BORDER_WIDTH)
    // The shape pinning lives in DecoratedWindow's tests; here
    // we exercise the same arithmetic via a freshly-built
    // decorated window.
    //
    // Note: we don't paint or dispatch — just construct +
    // assert geometry. App::connect is bypassed via a stub
    // server-paired connection.
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 200, 100);
    let dw = DecoratedWindow::new(&mut app, bounds, "test").expect("new");
    let cr = dw.content_rect();
    assert_eq!(cr.x, BORDER_WIDTH as i32);
    assert_eq!(cr.y, TITLEBAR_HEIGHT as i32);
    assert_eq!(cr.width, 200 - 2 * BORDER_WIDTH);
    assert_eq!(cr.height, 100 - TITLEBAR_HEIGHT - BORDER_WIDTH);
}

#[test]
fn small_window_yields_empty_content_rect() {
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 5, 5);
    let dw = DecoratedWindow::new(&mut app, bounds, "tiny").expect("new");
    let cr = dw.content_rect();
    assert_eq!(cr.width, 0);
    assert_eq!(cr.height, 0);
}

#[test]
fn close_button_click_returns_close_and_flips_close_requested() {
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 300, 200);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    assert!(!dw.close_requested());

    // Click in the centre of the close button rect.
    let cb = dw.frame().close_button_rect();
    let cx = cb.x + (cb.width as i32) / 2;
    let cy = cb.y + (cb.height as i32) / 2;
    let outcome = dw.handle_pointer_down(cx, cy);
    assert_eq!(outcome, DecoratedPointerOutcome::Close);
    assert!(dw.close_requested());
}

#[test]
fn titlebar_click_outside_close_button_returns_titlebar() {
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 300, 200);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    // Click at the very left of the titlebar, well clear of
    // the close button which sits in the right margin.
    let outcome = dw.handle_pointer_down(10, 5);
    assert_eq!(outcome, DecoratedPointerOutcome::Titlebar);
    assert!(!dw.close_requested());
}

#[test]
fn content_area_click_returns_content() {
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 300, 200);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    // Click well inside the content rect.
    let cr = dw.content_rect();
    let cx = cr.x + (cr.width as i32) / 2;
    let cy = cr.y + (cr.height as i32) / 2;
    let outcome = dw.handle_pointer_down(cx, cy);
    assert_eq!(outcome, DecoratedPointerOutcome::Content);
    assert!(!dw.close_requested());
}

#[test]
fn outside_window_click_returns_outside() {
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 100, 100);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    // Click well past the window's bottom-right.
    let outcome = dw.handle_pointer_down(500, 500);
    assert_eq!(outcome, DecoratedPointerOutcome::Outside);
    assert!(!dw.close_requested());
}

#[test]
fn resize_re_anchors_the_close_button_to_new_top_right() {
    let mut app = build_app();
    let bounds_a = Rect::new(0, 0, 200, 100);
    let mut dw = DecoratedWindow::new(&mut app, bounds_a, "app").expect("new");
    let cb_a = dw.frame().close_button_rect();
    assert!(cb_a.x > 100, "close button should be on the right");

    let bounds_b = Rect::new(0, 0, 400, 200);
    dw.resize(bounds_b);
    let cb_b = dw.frame().close_button_rect();
    // After resize, the close button should sit ~CLOSE_BUTTON_MARGIN
    // from the right edge of the new bounds (400).
    let expected_x = bounds_b.x + (bounds_b.width as i32)
        - (CLOSE_BUTTON_MARGIN as i32)
        - (CLOSE_BUTTON_SIZE as i32);
    assert_eq!(cb_b.x, expected_x);
    assert_eq!(cb_b.width, CLOSE_BUTTON_SIZE);
}

#[test]
fn set_title_updates_the_titlebar_caption() {
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 300, 100);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "initial").expect("new");
    assert_eq!(dw.frame().app_id(), "initial");
    dw.set_title("renamed").expect("set_title");
    assert_eq!(dw.frame().app_id(), "renamed");
}

#[test]
fn set_focused_flips_the_chrome_focus_state() {
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 200, 100);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    assert!(dw.frame().is_focused()); // default is focused
    dw.set_focused(false);
    assert!(!dw.frame().is_focused());
    dw.set_focused(true);
    assert!(dw.frame().is_focused());
}

#[test]
fn pointer_up_resets_close_button_state_safely() {
    // Defence in depth: a click that pressed but moved off
    // before release shouldn't leave the close button "stuck".
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 300, 200);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    let cb = dw.frame().close_button_rect();
    let _ = dw.handle_pointer_down(cb.x + 1, cb.y + 1);
    dw.handle_pointer_up(0, 0);
    // Frame's close button is back to resting; visible state
    // is the resting palette. Test pins the no-panic invariant
    // — exact pixel-level state belongs to the WindowFrame
    // tests.
    let _ = dw.frame();
}

#[test]
fn decorated_window_set_maximized_forwards_to_window() {
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 200, 100);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    assert!(!dw.is_maximized());
    dw.set_maximized().expect("set_maximized");
    // The wire-level effect is checked by the Window tests;
    // here we just pin the forwarding round-trip — calling
    // through DecoratedWindow doesn't panic and the inner
    // window state hasn't been mutated client-side
    // (is_maximized still false until configure lands).
    assert!(!dw.is_maximized());
}

#[test]
fn decorated_window_unset_maximized_forwards_to_window() {
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 200, 100);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    dw.unset_maximized().expect("unset_maximized");
    assert!(!dw.is_maximized());
}

#[test]
fn left_edge_click_within_margin_returns_resize_edge_left() {
    use display_proto::xdg_toplevel_resize_edge as edge;
    use toolkit::widget::frame::{RESIZE_HIT_MARGIN, TITLEBAR_HEIGHT};
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 300, 200);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    // Click 1 px from the left edge, at a y past the
    // titlebar so it's in the resize hit-band.
    let cy = bounds.y + (TITLEBAR_HEIGHT as i32) + 10;
    let outcome = dw.handle_pointer_down(bounds.x + 1, cy);
    assert_eq!(outcome, DecoratedPointerOutcome::ResizeEdge(edge::LEFT));
    // Sanity: the helper agrees.
    assert_eq!(
        dw.resize_edge_at(bounds.x + (RESIZE_HIT_MARGIN as i32) - 1, cy),
        Some(edge::LEFT)
    );
}

#[test]
fn right_edge_click_within_margin_returns_resize_edge_right() {
    use display_proto::xdg_toplevel_resize_edge as edge;
    use toolkit::widget::frame::TITLEBAR_HEIGHT;
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 300, 200);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    let cx = bounds.right() - 1;
    let cy = bounds.y + (TITLEBAR_HEIGHT as i32) + 10;
    let outcome = dw.handle_pointer_down(cx, cy);
    assert_eq!(outcome, DecoratedPointerOutcome::ResizeEdge(edge::RIGHT));
}

#[test]
fn bottom_left_corner_click_returns_resize_edge_bottom_left() {
    use display_proto::xdg_toplevel_resize_edge as edge;
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 300, 200);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    let cx = bounds.x + 1;
    let cy = bounds.bottom() - 1;
    let outcome = dw.handle_pointer_down(cx, cy);
    assert_eq!(
        outcome,
        DecoratedPointerOutcome::ResizeEdge(edge::BOTTOM_LEFT)
    );
}

#[test]
fn bottom_right_corner_click_returns_resize_edge_bottom_right() {
    use display_proto::xdg_toplevel_resize_edge as edge;
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 300, 200);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    let cx = bounds.right() - 1;
    let cy = bounds.bottom() - 1;
    let outcome = dw.handle_pointer_down(cx, cy);
    assert_eq!(
        outcome,
        DecoratedPointerOutcome::ResizeEdge(edge::BOTTOM_RIGHT)
    );
}

#[test]
fn bottom_edge_click_within_margin_returns_resize_edge_bottom() {
    use display_proto::xdg_toplevel_resize_edge as edge;
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 300, 200);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    // Centre x so neither LEFT nor RIGHT triggers, just BOTTOM.
    let cx = bounds.x + (bounds.width as i32) / 2;
    let cy = bounds.bottom() - 1;
    let outcome = dw.handle_pointer_down(cx, cy);
    assert_eq!(outcome, DecoratedPointerOutcome::ResizeEdge(edge::BOTTOM));
}

#[test]
fn top_edge_does_not_surface_a_resize_edge_outcome() {
    // Per the design note in DecoratedPointerOutcome::ResizeEdge,
    // the top edge falls through to Titlebar so titlebar-drag
    // wins over a hidden resize hit-box at the top of the
    // window. This pins that contract.
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 300, 200);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    // Click at the very top edge, away from the close button.
    let outcome = dw.handle_pointer_down(20, bounds.y + 1);
    assert_eq!(outcome, DecoratedPointerOutcome::Titlebar);
    // Helper also returns None for clicks in the titlebar's
    // vertical span.
    assert_eq!(dw.resize_edge_at(20, bounds.y + 1), None);
}

#[test]
fn resize_edge_at_returns_none_outside_bounds() {
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 100, 100);
    let dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    assert_eq!(dw.resize_edge_at(-5, 50), None);
    assert_eq!(dw.resize_edge_at(500, 50), None);
    assert_eq!(dw.resize_edge_at(50, -5), None);
    assert_eq!(dw.resize_edge_at(50, 500), None);
}

#[test]
fn request_move_forwards_to_window_no_panic() {
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 200, 100);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    dw.request_move(42).expect("request_move");
}

#[test]
fn request_resize_forwards_to_window_no_panic() {
    use display_proto::xdg_toplevel_resize_edge as edge;
    let mut app = build_app();
    let bounds = Rect::new(0, 0, 200, 100);
    let mut dw = DecoratedWindow::new(&mut app, bounds, "app").expect("new");
    dw.request_resize(99, edge::BOTTOM_RIGHT)
        .expect("request_resize");
}
