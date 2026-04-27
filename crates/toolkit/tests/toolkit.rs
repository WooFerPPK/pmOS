//! T120: Toolkit isolation umbrella tests against a mock display server.
//!
//! Three tests exercise the contract surface in one place:
//! 1. Window creation dispatches the right sequence of protocol messages.
//! 2. Layout produces the expected child rects.
//! 3. Keyboard focus routes to the focused TextInput.
//!
//! Detailed per-area coverage lives in `tests/{window,layout,widget_text_input}.rs`;
//! this file is the cross-cutting smoke test cited in T120.

use std::collections::VecDeque;

use display_proto::events::RegistryGlobal;

use toolkit::draw::{Canvas, Rect};
use toolkit::layout::Row;
use toolkit::protocol::Connection;
use toolkit::widget::text_input::{Key, KeyOutcome, TextInput};
use toolkit::{App, HEADER_SIZE, MessageHeader, ObjectId, Window};

#[derive(Default)]
struct LoopbackConnection {
    outbound: Vec<u8>,
    inbound: VecDeque<Vec<u8>>,
}

impl LoopbackConnection {
    fn new() -> Self {
        Self::default()
    }
    fn push_inbound(&mut self, bytes: Vec<u8>) {
        self.inbound.push_back(bytes);
    }
}

impl Connection for LoopbackConnection {
    fn send(&mut self, bytes: &[u8]) {
        self.outbound.extend_from_slice(bytes);
    }
    fn drain_outbound(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbound)
    }
    fn recv(&mut self) -> Vec<u8> {
        self.inbound.pop_front().unwrap_or_default()
    }
}

const REGISTRY_ID: ObjectId = ObjectId::new(3);

fn build_global_event(name: u32, interface: &str, version: u32) -> Vec<u8> {
    let event = RegistryGlobal {
        name,
        interface: interface.to_string(),
        version,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header =
        MessageHeader::try_new(REGISTRY_ID, 1, payload.len(), 0).expect("valid header");
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

fn seed_registry(conn: &mut LoopbackConnection) {
    let mut batch = Vec::new();
    batch.extend(build_global_event(1, "pmd_compositor", 1));
    batch.extend(build_global_event(2, "pmd_shm", 1));
    batch.extend(build_global_event(3, "pmd_xdg_shell", 1));
    conn.push_inbound(batch);
}

/// Decode framed (object_id, opcode) tuples for outbound assertions.
fn parsed_opcodes(bytes: &[u8]) -> Vec<(ObjectId, u16)> {
    let mut bytes = bytes;
    let mut out = Vec::new();
    while bytes.len() >= HEADER_SIZE {
        let header = MessageHeader::decode(bytes).expect("framed");
        out.push((header.object_id, header.opcode));
        bytes = &bytes[header.length as usize..];
    }
    out
}

#[test]
fn window_creation_dispatches_create_surface_then_get_toplevel() {
    let mut conn = LoopbackConnection::new();
    seed_registry(&mut conn);
    let mut app = App::connect(conn).expect("bootstrap");
    let _ = app.client_mut().connection_mut().drain_outbound();

    let compositor_id = app.compositor();
    let xdg_shell_id = app.xdg_shell();
    let _window = Window::new(&mut app).expect("window");
    let outbound = app.client_mut().connection_mut().drain_outbound();
    let ops = parsed_opcodes(&outbound);

    // Expect at least: compositor.create_surface + xdg_shell.get_toplevel.
    let create_surface = ops
        .iter()
        .find(|(obj, op)| *obj == compositor_id && *op == 1);
    let get_toplevel = ops
        .iter()
        .find(|(obj, op)| *obj == xdg_shell_id && *op == 1);
    assert!(create_surface.is_some(), "create_surface dispatched");
    assert!(get_toplevel.is_some(), "get_toplevel dispatched");
    // get_toplevel must come AFTER create_surface (the surface id it references must exist).
    let cs_idx = ops.iter().position(|(o, p)| *o == compositor_id && *p == 1).unwrap();
    let gt_idx = ops.iter().position(|(o, p)| *o == xdg_shell_id && *p == 1).unwrap();
    assert!(cs_idx < gt_idx, "create_surface precedes get_toplevel");
}

#[test]
fn layout_row_produces_expected_child_rects() {
    let parent = Rect::new(0, 0, 300, 100);
    let mut row = Row::new(parent, 0, 10);
    let r0 = row.next(40, 30);
    let r1 = row.next(60, 30);
    let r2 = row.next(80, 30);
    // x positions accumulate widths plus spacing (10px).
    assert_eq!(r0.x, 0);
    assert_eq!(r0.width, 40);
    assert_eq!(r1.x, 50); // 0 + 40 + 10
    assert_eq!(r1.width, 60);
    assert_eq!(r2.x, 120); // 50 + 60 + 10
    assert_eq!(r2.width, 80);
}

#[test]
fn keyboard_focus_routes_to_focused_text_input() {
    let mut focused = TextInput::new(Rect::new(0, 0, 200, 20));
    let mut unfocused_typed = TextInput::new(Rect::new(0, 0, 200, 20));
    focused.set_focus(true);

    // Routing keys to a focused input mutates its text and
    // returns Changed; the unfocused input rejects the same key
    // and returns Ignored (the focus check is enforced inside
    // the widget — the app router does not need to gate by
    // focus state).
    let outcome = focused.handle_key(Key::Char('a'));
    assert!(matches!(outcome, KeyOutcome::Changed));
    assert_eq!(focused.text(), "a");
    let unfocused_outcome = unfocused_typed.handle_key(Key::Char('a'));
    assert!(matches!(unfocused_outcome, KeyOutcome::Ignored));
    assert_eq!(unfocused_typed.text(), "");
    // Force-set the unfocused one to "a" so the paint comparison
    // below isolates the caret bar (which only paints on focused).
    unfocused_typed.set_text("a");

    // Verify focused widget paints a caret bar that the unfocused
    // one does not — i.e. focus is observable in render output.
    let mut focused_canvas = Canvas::new(200, 20);
    let mut unfocused_canvas = Canvas::new(200, 20);
    focused.paint(&mut focused_canvas, &toolkit::theme::Theme::LIGHT);
    unfocused_typed.paint(&mut unfocused_canvas, &toolkit::theme::Theme::LIGHT);

    // Caret bar lands at text_x + 1 * CELL_WIDTH after typing one char.
    // text_x = bounds.x + border(1) + padding(4) = 5; CELL_WIDTH = 6.
    // Bar at column 11, scan a representative interior row (text_y+1).
    let focused_caret_col = px_at(&focused_canvas, 11, 6);
    let unfocused_same_col = px_at(&unfocused_canvas, 11, 6);
    assert_ne!(
        focused_caret_col, unfocused_same_col,
        "focused input paints caret bar; unfocused does not"
    );
}

fn px_at(canvas: &Canvas, x: u32, y: u32) -> [u8; 4] {
    let s = canvas.pixel(x, y).expect("in bounds");
    [s[0], s[1], s[2], s[3]]
}
