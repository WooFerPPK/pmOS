//! `pmd_shell_manager` (spec §15) typed request/event +
//! opcode-table tests.

use display_proto::events::shell_window_state_flags;
use display_proto::{
    Interface, ShellManagerBeginRestore, ShellManagerCloseWindow, ShellManagerDesktopReady,
    ShellManagerEndRestore, ShellManagerFocusWindow, ShellManagerMinimizeWindow,
    ShellManagerPlaceRestoredWindow, ShellManagerSetWorkAreaBottom,
    ShellManagerSubscribeWindowState, ShellManagerSubscribeWindows,
    ShellManagerToggleMaximizedWindow, ShellManagerUnminimizeWindow, ShellRestoreFinished,
    ShellWindowCreated, ShellWindowDestroyed, ShellWindowFocused, ShellWindowSnapshotDone,
    ShellWindowState, ShellWindowTitleChanged,
};

// ---- Opcode tables ------------------------------------------------

#[test]
fn shell_manager_request_opcodes_include_desktop_ready_fence() {
    let s = Interface::ShellManager;
    assert_eq!(s.lookup_request(1).unwrap().name, "subscribe_windows");
    assert_eq!(s.lookup_request(2).unwrap().name, "focus_window");
    assert_eq!(s.lookup_request(3).unwrap().name, "close_window");
    assert_eq!(s.lookup_request(4).unwrap().name, "minimize_window");
    assert_eq!(s.lookup_request(5).unwrap().name, "unminimize_window");
    assert_eq!(s.lookup_request(6).unwrap().name, "set_work_area_bottom");
    assert_eq!(s.lookup_request(7).unwrap().name, "toggle_maximized_window");
    assert_eq!(s.lookup_request(8).unwrap().name, "desktop_ready");
    assert_eq!(s.lookup_request(9).unwrap().name, "subscribe_window_state");
    assert_eq!(s.lookup_request(10).unwrap().name, "begin_restore");
    assert_eq!(s.lookup_request(11).unwrap().name, "place_restored_window");
    assert_eq!(s.lookup_request(12).unwrap().name, "end_restore");
    assert!(s.lookup_request(13).is_err());
}

#[test]
fn shell_manager_event_opcodes_are_window_created_destroyed_focused_title_changed() {
    let s = Interface::ShellManager;
    assert_eq!(s.lookup_event(1).unwrap().name, "window_created");
    assert_eq!(s.lookup_event(2).unwrap().name, "window_destroyed");
    assert_eq!(s.lookup_event(3).unwrap().name, "window_focused");
    assert_eq!(s.lookup_event(4).unwrap().name, "window_title_changed");
    assert_eq!(s.lookup_event(5).unwrap().name, "window_created_v2");
    assert_eq!(s.lookup_event(6).unwrap().name, "window_state_changed");
    assert_eq!(s.lookup_event(7).unwrap().name, "window_snapshot_done");
    assert_eq!(s.lookup_event(8).unwrap().name, "restore_finished");
    assert!(s.lookup_event(9).is_err());
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
fn desktop_ready_decodes_from_an_empty_payload() {
    assert_eq!(
        ShellManagerDesktopReady::decode(&[]).unwrap(),
        ShellManagerDesktopReady,
    );
    assert!(ShellManagerDesktopReady::decode(&[0]).is_err());
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
fn unminimize_window_decodes_a_single_u32_window_id() {
    let payload = 101u32.to_le_bytes();
    let req = ShellManagerUnminimizeWindow::decode(&payload).unwrap();
    assert_eq!(req.window_id, 101);
}

#[test]
fn set_work_area_bottom_decodes_pixel_height() {
    let req = ShellManagerSetWorkAreaBottom::decode(&32u32.to_le_bytes()).unwrap();
    assert_eq!(req.height_px, 32);
    assert!(ShellManagerSetWorkAreaBottom::decode(&[0u8; 3]).is_err());
}

#[test]
fn toggle_maximized_window_decodes_global_window_id() {
    let req = ShellManagerToggleMaximizedWindow::decode(&73u32.to_le_bytes()).unwrap();
    assert_eq!(req.window_id, 73);
    assert!(ShellManagerToggleMaximizedWindow::decode(&[0u8; 3]).is_err());
}

#[test]
fn focus_window_rejects_short_payload() {
    assert!(ShellManagerFocusWindow::decode(&[0u8; 3]).is_err());
}

#[test]
fn v2_subscription_and_restore_requests_have_exact_layouts() {
    assert_eq!(
        ShellManagerSubscribeWindowState::decode(&17u32.to_le_bytes())
            .unwrap()
            .snapshot_id,
        17,
    );
    assert!(ShellManagerSubscribeWindowState::decode(&[]).is_err());

    let mut begin = Vec::new();
    begin.extend_from_slice(&41u32.to_le_bytes());
    begin.extend_from_slice(&350u32.to_le_bytes());
    assert_eq!(
        ShellManagerBeginRestore::decode(&begin).unwrap(),
        ShellManagerBeginRestore {
            restore_id: 41,
            timeout_ms: 350,
        },
    );
    assert!(ShellManagerBeginRestore::decode(&begin[..4]).is_err());

    let values = [41u32, 7, (-12i32) as u32, 23, 640, 480, 9, 3];
    let place = values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    assert_eq!(
        ShellManagerPlaceRestoredWindow::decode(&place).unwrap(),
        ShellManagerPlaceRestoredWindow {
            restore_id: 41,
            window_id: 7,
            normal_x: -12,
            normal_y: 23,
            normal_width: 640,
            normal_height: 480,
            z_rank: 9,
            flags: 3,
        },
    );
    assert!(ShellManagerPlaceRestoredWindow::decode(&place[..28]).is_err());

    let mut end = Vec::new();
    end.extend_from_slice(&41u32.to_le_bytes());
    end.extend_from_slice(&7u32.to_le_bytes());
    assert_eq!(
        ShellManagerEndRestore::decode(&end).unwrap(),
        ShellManagerEndRestore {
            restore_id: 41,
            focus_window_id: 7,
        },
    );
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

#[test]
fn authoritative_window_state_round_trips_all_identity_and_geometry() {
    let original = ShellWindowState {
        snapshot_id: 91,
        window_id: 42,
        owner_pid: 301,
        ordinal: 2,
        current_x: 11,
        current_y: -7,
        current_width: 800,
        current_height: 600,
        normal_x: 32,
        normal_y: 48,
        normal_width: 640,
        normal_height: 480,
        flags: shell_window_state_flags::ALL,
        z_rank: 4,
        title: "Editor — notes".to_string(),
        app_id: "pmos.edit".to_string(),
    };
    let mut payload = Vec::new();
    original.encode(&mut payload);
    assert_eq!(ShellWindowState::decode(&payload).unwrap(), original);
    let truncated = &payload[..payload.len() - 1];
    assert!(ShellWindowState::decode(truncated).is_err());
    payload.push(0);
    assert!(ShellWindowState::decode(&payload).is_err());
}

#[test]
fn snapshot_and_restore_completion_ids_round_trip() {
    let done = ShellWindowSnapshotDone { snapshot_id: 91 };
    let mut payload = Vec::new();
    done.encode(&mut payload);
    assert_eq!(ShellWindowSnapshotDone::decode(&payload).unwrap(), done);

    let finished = ShellRestoreFinished {
        restore_id: 41,
        status: 2,
        placed: 6,
    };
    payload.clear();
    finished.encode(&mut payload);
    assert_eq!(ShellRestoreFinished::decode(&payload).unwrap(), finished);
}
