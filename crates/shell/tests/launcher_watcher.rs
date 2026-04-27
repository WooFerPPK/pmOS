//! T200 launcher_watcher tests — verify the 5 s poll detects new
//! `.desktop` entries and emits an `Added` event for each one.

use core::time::Duration;

use shell::launcher::{DesktopEntry, DesktopEntryStore, LauncherError};
use shell::launcher_watcher::{tick, WatchEvent};
use shell::Launcher;

fn entry(id: &str, name: &str) -> DesktopEntry {
    DesktopEntry {
        id: id.to_string(),
        name: name.to_string(),
        exec: format!("/bin/{id}"),
        icon: None,
        summary: None,
        mime_types: Vec::new(),
        categories: Vec::new(),
        caps: vec!["DISPLAY_CLIENT".to_string()],
    }
}

fn entry_content(id: &str, name: &str) -> (String, String) {
    (
        format!("{id}.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName={name}\nExec=/bin/{id}\nX-PMos-Caps=DISPLAY_CLIENT\n"
        ),
    )
}

/// Test store whose entries can mutate between polls.
struct MutableStore {
    inner: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

impl DesktopEntryStore for MutableStore {
    fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError> {
        Ok(self.inner.lock().unwrap().clone())
    }
}

#[test]
fn watcher_emits_added_event_when_new_entry_appears_after_5s() {
    let inner = std::sync::Arc::new(std::sync::Mutex::new(vec![entry_content("term", "Terminal")]));
    let store = MutableStore { inner: inner.clone() };
    let mut launcher = Launcher::new(Box::new(store));
    assert_eq!(launcher.entries().len(), 1);

    // First tick at t=0 fires the initial poll (last_poll==ZERO so
    // 0 >= 0 + 5s is false — no fire). t=5s does fire.
    let events = tick(&mut launcher, Duration::from_secs(4));
    assert!(events.is_empty(), "no events at t=4s");

    // Mutate the underlying store to add a new entry, then tick at t=5s.
    inner
        .lock()
        .unwrap()
        .push(entry_content("hello", "Hello"));
    let events = tick(&mut launcher, Duration::from_secs(5));
    assert_eq!(events.len(), 1, "one Added event after store mutates");
    match &events[0] {
        WatchEvent::Added(e) => assert_eq!(e.id, "hello.desktop"),
        _ => panic!("expected Added, got {events:?}"),
    }
}

#[test]
fn watcher_emits_removed_event_for_deleted_entry() {
    use shell::launcher_watcher::diff_entries;
    let before = vec![entry("term", "Terminal"), entry("old", "Old")];
    let after = vec![entry("term", "Terminal")];
    let diff = diff_entries(&before, &after);
    assert_eq!(diff.len(), 1);
    match &diff[0] {
        WatchEvent::Removed(id) => assert_eq!(id, "old"),
        _ => panic!("expected Removed"),
    }
}

#[test]
fn watcher_handles_simultaneous_add_and_remove() {
    use shell::launcher_watcher::diff_entries;
    let before = vec![entry("a", "A"), entry("b", "B")];
    let after = vec![entry("a", "A"), entry("c", "C")];
    let diff = diff_entries(&before, &after);
    // Removals first, then adds (deterministic for stable taskbar order).
    assert_eq!(diff.len(), 2);
    assert!(matches!(&diff[0], WatchEvent::Removed(id) if id == "b"));
    assert!(matches!(&diff[1], WatchEvent::Added(e) if e.id == "a" || e.id == "c"));
}
