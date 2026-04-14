//! `pmd_shell_manager` (spec §15) typed request/event +
//! opcode-table tests.

use display_proto::{
    Interface, ShellManagerCloseWindow, ShellManagerFocusWindow,
    ShellManagerMinimizeWindow, ShellManagerSubscribeWindows, ShellWindowCreated,
    ShellWindowDestroyed, ShellWindowFocused, ShellWindowTitleChanged,
};

// ---- Opcode tables ------------------------------------------------

#[test]
fn shell_manager_request_opcodes_are_subscribe_focus_close_minimize() {
    let s = Interface::ShellManager;
    assert_eq!(
        s.lookup_request(1).unwrap().name,
        "subscribe_windows"
    );
    assert_eq!(s.lookup_request(2).unwrap().name, "focus_window");
    assert_eq!(s.lookup_request(3).unwrap().name, "close_window");
    assert_eq!(s.lookup_request(4).unwrap().name, "minimize_window");
    assert!(s.lookup_request(5).is_err());
}

#[test]
fn shell_manager_event_opcodes_are_window_created_destroyed_focused_title_changed() {
    let s = Interface::ShellManager;
    assert_eq!(s.lookup_event(1).unwrap().name, "window_created");
    assert_eq!(s.lookup_event(2).unwrap().name, "window_destroyed");
    assert_eq!(s.lookup_event(3).unwrap().name, "window_focused");
    assert_eq!(s.lookup_event(4).unwrap().name, "window_title_changed");
    assert!(s.lookup_event(5).is_err());
}

// ---- Request decoders -------------------------------------------

#[test]
fn subscribe_windows_decodes_from_an_empty_payload() {
    let req = ShellManagerSubscribeWindows::decode(&[]).unwrap();
    assert_eq!(req, ShellManagerSubscribeWindows);
    // Also accepts trailing junk (it ignores the payload).
    let req2 = ShellManagerSubscribeWindows::decode(&[1, 2, 3]).unwrap();
    assert_eq!(req2, ShellManagerSubscribeWindows);
}

#[test]
fn focus_window_decodes_a_single_u32_window_id() {
    let payload = 7u32.to_le_bytes();
    let req = ShellManagerFocusWindow::decode(&payload).unwrap();
    assert_eq!(req.window_id, 7);
}

#[test]
fn close_window_decodes_a_single_u32_window_id() {
    let payload = 13u32.to_le_bytes();
    let req = ShellManagerCloseWindow::decode(&payload).unwrap();
    assert_eq!(req.window_id, 13);
}

#[test]
fn minimize_window_decodes_a_single_u32_window_id() {
    let payload = 99u32.to_le_bytes();
    let req = ShellManagerMinimizeWindow::decode(&payload).unwrap();
    assert_eq!(req.window_id, 99);
}

#[test]
fn focus_window_rejects_short_payload() {
    assert!(ShellManagerFocusWindow::decode(&[0u8; 3]).is_err());
}

// ---- Event encode/decode round trips ----------------------------

#[test]
fn window_created_round_trips_with_title_and_app_id() {
    let original = ShellWindowCreated {
        window_id: 42,
        title: "Untitled — sh".to_string(),
        app_id: "pmos.sh".to_string(),
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    let decoded = ShellWindowCreated::decode(&buf).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn window_created_handles_empty_title_and_app_id() {
    let original = ShellWindowCreated {
        window_id: 1,
        title: String::new(),
        app_id: String::new(),
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    // u32 window_id (4) + u32 strlen=0 (4) + 0 content
    // + u32 strlen=0 (4) + 0 content = 12 bytes total.
    assert_eq!(buf.len(), 12);
    let decoded = ShellWindowCreated::decode(&buf).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn window_destroyed_round_trips() {
    let original = ShellWindowDestroyed { window_id: 7 };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    assert_eq!(buf, vec![7, 0, 0, 0]);
    assert_eq!(ShellWindowDestroyed::decode(&buf).unwrap(), original);
}

#[test]
fn window_focused_round_trips() {
    let original = ShellWindowFocused { window_id: 11 };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    assert_eq!(buf, vec![11, 0, 0, 0]);
    assert_eq!(ShellWindowFocused::decode(&buf).unwrap(), original);
}

#[test]
fn window_title_changed_round_trips() {
    let original = ShellWindowTitleChanged {
        window_id: 5,
        new_title: "edit: README.md".to_string(),
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    let decoded = ShellWindowTitleChanged::decode(&buf).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn window_created_rejects_truncated_payload() {
    // window_id only — missing both strings.
    let payload = 1u32.to_le_bytes();
    assert!(ShellWindowCreated::decode(&payload).is_err());
}
