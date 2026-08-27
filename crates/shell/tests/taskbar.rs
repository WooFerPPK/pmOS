//! T130 — `Taskbar` isolation tests.
//!
//! Pin the data-model + click-routing surface against the
//! `pmd_shell_manager` event stream defined in
//! `display_proto`. The taskbar paints colored boxes via
//! its `draw` method but the visual layer is exercised
//! lightly here — the focus is on the *behavioral*
//! contract: how does the entry list evolve as events
//! arrive, and what does a click route to.

use display_proto::events::{
    ShellWindowCreated, ShellWindowDestroyed, ShellWindowFocused, ShellWindowTitleChanged,
};
use shell::{Taskbar, TaskbarClick, TaskbarError};
use toolkit::draw::{Canvas, Color};
use toolkit::theme::Theme;

fn rgba(color: Color) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

fn build_window_created(window_id: u32, title: &str, app_id: &str) -> Vec<u8> {
    let event = ShellWindowCreated {
        window_id,
        title: title.to_string(),
        app_id: app_id.to_string(),
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    payload
}

fn build_window_destroyed(window_id: u32) -> Vec<u8> {
    let event = ShellWindowDestroyed { window_id };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    payload
}

fn build_window_focused(window_id: u32) -> Vec<u8> {
    let event = ShellWindowFocused { window_id };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    payload
}

fn build_window_title_changed(window_id: u32, new_title: &str) -> Vec<u8> {
    let event = ShellWindowTitleChanged {
        window_id,
        new_title: new_title.to_string(),
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    payload
}

#[test]
fn taskbar_starts_with_no_entries() {
    let tb = Taskbar::new(800, 600);
    assert!(tb.entries().is_empty());
    assert_eq!(tb.clock_text(), "");
}

#[test]
fn clock_label_reports_visual_changes_only() {
    let mut tb = Taskbar::new(800, 600);
    assert!(tb.set_clock_text("14:05 UTC"));
    assert!(!tb.set_clock_text("14:05 UTC"));
    assert_eq!(tb.clock_text(), "14:05 UTC");
    assert!(tb.set_clock_text("09:05 EST"));
}

#[test]
fn handle_event_window_created_appends_entry() {
    let mut tb = Taskbar::new(800, 600);
    tb.handle_event_bytes(1, &build_window_created(7, "Term", "pmos.term"))
        .unwrap();
    assert_eq!(tb.entries().len(), 1);
    let entry = &tb.entries()[0];
    assert_eq!(entry.window_id, 7);
    assert_eq!(entry.title, "Term");
    assert_eq!(entry.app_id, "pmos.term");
    assert!(!entry.focused);
    assert!(!entry.minimized);
}

#[test]
fn add_window_is_idempotent_updates_title_and_app_id_in_place() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "Term", "pmos.term");
    tb.add_window(7, "Terminal v2", "pmos.term");
    assert_eq!(tb.entries().len(), 1);
    assert_eq!(tb.entries()[0].title, "Terminal v2");
}

#[test]
fn desktop_shell_is_retained_outside_the_visible_task_list() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(1, "PMos", "pmos.shell");
    tb.set_shell_window_id(1);
    assert!(tb.entries().is_empty());

    tb.add_window(2, "Terminal", "pmos.term");
    assert_eq!(tb.entries().len(), 1);
    assert_eq!(tb.entries()[0].window_id, 2);

    tb.remove_window(1);
    assert_eq!(tb.entries().len(), 1);
}

#[test]
fn untrusted_shell_app_id_remains_an_ordinary_visible_task() {
    let mut tb = Taskbar::new(800, 600);
    tb.set_shell_window_id(1);
    tb.add_window(7, "Spoof", "pmos.shell");

    assert_eq!(tb.entries().len(), 1);
    assert_eq!(tb.entries()[0].window_id, 7);
    assert_eq!(tb.entries()[0].app_id, "pmos.shell");
}

#[test]
fn handle_event_window_destroyed_removes_entry() {
    let mut tb = Taskbar::new(800, 600);
    tb.handle_event_bytes(1, &build_window_created(7, "Term", "pmos.term"))
        .unwrap();
    tb.handle_event_bytes(1, &build_window_created(8, "Files", "pmos.files"))
        .unwrap();
    assert_eq!(tb.entries().len(), 2);
    tb.handle_event_bytes(2, &build_window_destroyed(7))
        .unwrap();
    assert_eq!(tb.entries().len(), 1);
    assert_eq!(tb.entries()[0].window_id, 8);
}

#[test]
fn handle_event_window_focused_flips_focused_flag_on_one_entry_only() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "A", "pmos.a");
    tb.add_window(8, "B", "pmos.b");
    tb.handle_event_bytes(3, &build_window_focused(8)).unwrap();
    assert!(!tb.entries()[0].focused);
    assert!(tb.entries()[1].focused);
    tb.handle_event_bytes(3, &build_window_focused(7)).unwrap();
    assert!(tb.entries()[0].focused);
    assert!(!tb.entries()[1].focused);
}

#[test]
fn handle_event_window_title_changed_updates_title_only() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "Initial", "pmos.term");
    tb.handle_event_bytes(4, &build_window_title_changed(7, "Updated"))
        .unwrap();
    assert_eq!(tb.entries()[0].title, "Updated");
    assert_eq!(tb.entries()[0].app_id, "pmos.term"); // untouched
}

#[test]
fn handle_event_unknown_opcode_returns_unknown_opcode_error() {
    let mut tb = Taskbar::new(800, 600);
    let err = tb.handle_event_bytes(99, &[]).unwrap_err();
    match err {
        TaskbarError::UnknownOpcode { opcode } => assert_eq!(opcode, 99),
        other => panic!("expected UnknownOpcode, got {other:?}"),
    }
}

#[test]
fn handle_event_malformed_payload_returns_malformed_error() {
    let mut tb = Taskbar::new(800, 600);
    // window_created needs a u32 + two strings; one byte is way too short.
    let err = tb.handle_event_bytes(1, &[0u8]).unwrap_err();
    assert_eq!(err, TaskbarError::Malformed);
}

#[test]
fn bounds_anchored_to_bottom_of_framebuffer() {
    let tb = Taskbar::new(800, 600);
    let b = tb.bounds();
    assert_eq!(b.x, 0);
    assert_eq!(b.y, 600 - shell::TASKBAR_HEIGHT as i32);
    assert_eq!(b.width, 800);
    assert_eq!(b.height, shell::TASKBAR_HEIGHT);
}

#[test]
fn entry_rect_lays_entries_left_to_right_with_gap() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "A", "");
    tb.add_window(8, "B", "");
    let r0 = tb.entry_rect(0).unwrap();
    let r1 = tb.entry_rect(1).unwrap();
    assert_eq!(
        r0.x,
        (shell::TASKBAR_LEFT_MARGIN + shell::TASKBAR_LAUNCHER_RESERVED_WIDTH) as i32
    );
    assert_eq!(r0.width, shell::TASKBAR_ENTRY_WIDTH);
    let stride = (shell::TASKBAR_ENTRY_WIDTH + shell::TASKBAR_ENTRY_GAP) as i32;
    assert_eq!(r1.x, r0.x + stride);
}

#[test]
fn many_entries_page_between_launcher_and_clock_without_overlap() {
    let mut tb = Taskbar::new(800, 600);
    for id in 1..=10 {
        tb.add_window(id, format!("Window {id}"), format!("pmos.app{id}"));
    }

    let first = tb.entry_rect(0).expect("first visible entry");
    let first_page = tb.visible_range();
    let last = tb
        .entry_rect(first_page.end - 1)
        .expect("last visible entry");
    assert_eq!(
        first.x,
        (shell::TASKBAR_LEFT_MARGIN + shell::TASKBAR_LAUNCHER_RESERVED_WIDTH) as i32
    );
    assert!(
        first.x >= 90,
        "entries must not overlap the launcher button"
    );
    assert!(
        last.right() <= 800 - shell::TASKBAR_CLOCK_RESERVED_WIDTH as i32,
        "last entry must leave the clock reservation visible"
    );
    assert!(last.width >= shell::TASKBAR_MIN_ENTRY_WIDTH);
    assert!(tb.has_overflow());
    assert!(tb.entry_rect(first_page.end).is_none());

    let old_page = tb.visible_range();
    tb.cycle_overflow();
    assert_ne!(tb.visible_range(), old_page);
    assert!(tb.entry_rect(9).is_some());
}

#[test]
fn entry_rect_returns_none_for_out_of_range_index() {
    let tb = Taskbar::new(800, 600);
    assert_eq!(tb.entry_rect(0), None);
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "A", "");
    assert!(tb.entry_rect(0).is_some());
    assert_eq!(tb.entry_rect(1), None);
}

#[test]
fn hit_test_entry_picks_the_entry_at_the_pointer() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "A", "");
    tb.add_window(8, "B", "");
    let r1 = tb.entry_rect(1).unwrap();
    let cx = r1.x + (r1.width as i32) / 2;
    let cy = r1.y + (r1.height as i32) / 2;
    assert_eq!(tb.hit_test_entry(cx, cy), Some(1));
}

#[test]
fn hit_test_entry_returns_none_outside_any_entry() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "A", "");
    // Click far above the taskbar.
    assert_eq!(tb.hit_test_entry(50, 50), None);
    // Click in the gap between entries (or past the last entry).
    let r0 = tb.entry_rect(0).unwrap();
    assert_eq!(tb.hit_test_entry(r0.right() + 1, r0.y + 1), None);
}

#[test]
fn handle_pointer_down_focus_for_visible_entry() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "A", "pmos.a");
    let r = tb.entry_rect(0).unwrap();
    let outcome = tb.handle_pointer_down(r.x + 5, r.y + 5);
    assert_eq!(outcome, Some(TaskbarClick::Focus { window_id: 7 }));
}

#[test]
fn handle_pointer_down_restore_for_minimized_entry() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "A", "pmos.a");
    tb.set_window_minimized(7, true);
    let r = tb.entry_rect(0).unwrap();
    let outcome = tb.handle_pointer_down(r.x + 5, r.y + 5);
    assert_eq!(outcome, Some(TaskbarClick::Restore { window_id: 7 }));
}

#[test]
fn clicking_the_focused_task_minimizes_it_without_embedded_controls() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "A", "pmos.a");
    tb.set_focused_window(7);
    let task = tb.entry_rect(0).expect("task button");
    assert_eq!(
        tb.handle_pointer_down(task.right() - 2, task.y + 1),
        Some(TaskbarClick::Minimize { window_id: 7 }),
    );
}

#[test]
fn overflow_control_cycles_pages_and_never_overlaps_entries() {
    let mut tb = Taskbar::new(420, 300);
    for id in 1..=8 {
        tb.add_window(id, format!("Window {id}"), format!("pmos.app{id}"));
    }
    let overflow = tb.overflow_rect().expect("overflow control");
    for idx in tb.visible_range() {
        assert!(tb.entry_rect(idx).unwrap().right() <= overflow.x);
    }
    assert_eq!(
        tb.handle_pointer_down(overflow.x + 1, overflow.y + 1),
        Some(TaskbarClick::CycleOverflow),
    );
    let first_page = tb.visible_range();
    tb.cycle_overflow();
    assert_ne!(tb.visible_range(), first_page);
}

#[test]
fn focus_event_restores_and_reveals_window_on_an_overflow_page() {
    let mut tb = Taskbar::new(420, 300);
    for id in 1..=8 {
        tb.add_window(id, format!("Window {id}"), format!("pmos.app{id}"));
    }
    tb.set_window_minimized(8, true);
    assert!(tb.entry_rect(7).is_none());
    tb.set_focused_window(8);
    assert!(tb.entry_rect(7).is_some());
    assert!(tb.entries()[7].focused);
    assert!(!tb.entries()[7].minimized);
}

#[test]
fn handle_pointer_down_returns_none_when_no_entry_hit() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "A", "");
    assert_eq!(tb.handle_pointer_down(50, 50), None);
}

#[test]
fn contains_point_recognizes_taskbar_strip_including_gaps() {
    let tb = Taskbar::new(800, 600);
    let b = tb.bounds();
    assert!(tb.contains_point(b.x + 1, b.y + 1));
    assert!(tb.contains_point(b.right() - 1, b.bottom() - 1));
    assert!(!tb.contains_point(b.x - 1, b.y + 1));
    assert!(!tb.contains_point(b.x + 1, b.y - 1));
}

#[test]
fn entry_label_falls_back_to_title_when_app_id_empty() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "Term", "");
    assert_eq!(tb.entries()[0].label(), "Term");
}

#[test]
fn entry_label_prefers_human_readable_title_over_app_id() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "Terminal — project", "pmos.term");
    assert_eq!(tb.entries()[0].label(), "Terminal — project");
}

#[test]
fn entry_label_falls_back_to_untitled_when_both_empty() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "", "");
    assert_eq!(tb.entries()[0].label(), "(untitled)");
}

#[test]
fn set_framebuffer_size_re_anchors_taskbar() {
    let mut tb = Taskbar::new(800, 600);
    tb.add_window(7, "A", "");
    let b1 = tb.bounds();
    tb.set_framebuffer_size(1024, 768);
    let b2 = tb.bounds();
    assert_eq!(b1.y, 600 - shell::TASKBAR_HEIGHT as i32);
    assert_eq!(b2.y, 768 - shell::TASKBAR_HEIGHT as i32);
    assert_eq!(b2.width, 1024);
    // Entries survive the re-anchor.
    assert_eq!(tb.entries().len(), 1);
}

#[test]
fn draw_uses_app_mark_focus_underline_and_minimized_palette() {
    let mut tb = Taskbar::new(480, 64);
    tb.set_theme(Theme::LIGHT);
    tb.add_window(7, "Terminal", "pmos.term");
    tb.add_window(8, "Files", "pmos.files");
    tb.set_focused_window(7);
    tb.set_window_minimized(8, true);

    let first = tb.entry_rect(0).expect("focused task");
    let second = tb.entry_rect(1).expect("minimized task");
    let mut canvas = Canvas::new(480, 64);
    tb.draw(&mut canvas);

    let mark_x = first.x as u32 + shell::TASKBAR_ENTRY_TEXT_MARGIN;
    let mark_y = first.y as u32 + (first.height.saturating_sub(shell::TASKBAR_APP_MARK_SIZE) / 2);
    assert_eq!(
        canvas.pixel(mark_x, mark_y),
        Some(&rgba(Theme::LIGHT.border_active)[..]),
    );

    let underline_x = first.x as u32
        + (first
            .width
            .saturating_sub(shell::TASKBAR_FOCUS_INDICATOR_WIDTH)
            / 2);
    assert_eq!(
        canvas.pixel(underline_x, first.bottom() as u32 - 1),
        Some(&rgba(Theme::LIGHT.border_active)[..]),
    );
    assert_eq!(
        canvas.pixel(second.x as u32 + 1, second.y as u32 + 1),
        Some(&rgba(Theme::LIGHT.titlebar_inactive)[..]),
    );
}
