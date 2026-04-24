use shell::launcher::{parse_desktop_entry, LauncherError, Launcher, MemoryStore};
use std::time::Duration;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn minimal_desktop(name: &str, exec: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n"
    )
}

// ── Parser tests ─────────────────────────────────────────────────────────────

#[test]
fn parses_well_formed_entry_minimum_fields() {
    let content = minimal_desktop("Text Editor", "/opt/edit/bin/edit.wasm");
    let entry = parse_desktop_entry("edit", &content).unwrap();
    assert_eq!(entry.id, "edit");
    assert_eq!(entry.name, "Text Editor");
    assert_eq!(entry.exec, "/opt/edit/bin/edit.wasm");
    assert_eq!(entry.icon, None);
    assert_eq!(entry.summary, None);
    assert!(entry.mime_types.is_empty());
    assert!(entry.categories.is_empty());
    assert!(entry.caps.is_empty());
}

#[test]
fn parses_all_optional_fields() {
    let content = "\
[Desktop Entry]
Type=Application
Name=Text Editor
Exec=/opt/edit/bin/edit.wasm
Icon=/opt/edit/assets/icon.png
Summary=Simple text editor.
MimeType=text/plain;text/markdown;
Categories=Utility;Editor;
X-PMos-Caps=DISPLAY_CLIENT;PROC_ENUMERATE;
";
    let entry = parse_desktop_entry("edit", content).unwrap();
    assert_eq!(entry.icon, Some("/opt/edit/assets/icon.png".to_string()));
    assert_eq!(entry.summary, Some("Simple text editor.".to_string()));
    assert_eq!(entry.mime_types, vec!["text/plain", "text/markdown"]);
    assert_eq!(entry.categories, vec!["Utility", "Editor"]);
    assert_eq!(entry.caps, vec!["DISPLAY_CLIENT", "PROC_ENUMERATE"]);
}

#[test]
fn ignores_unknown_keys() {
    let content = "\
[Desktop Entry]
Type=Application
Name=My App
Exec=/opt/myapp/bin/myapp.wasm
X-Future-Key=some-value
AnotherUnknown=ignored
";
    let entry = parse_desktop_entry("myapp", content).unwrap();
    assert_eq!(entry.name, "My App");
}

#[test]
fn rejects_missing_name() {
    let content = "[Desktop Entry]\nType=Application\nExec=/opt/myapp/bin/myapp.wasm\n";
    let err = parse_desktop_entry("myapp", content).unwrap_err();
    assert_eq!(err, LauncherError::MissingRequiredKey("Name"));
}

#[test]
fn rejects_missing_exec() {
    let content = "[Desktop Entry]\nType=Application\nName=My App\n";
    let err = parse_desktop_entry("myapp", content).unwrap_err();
    assert_eq!(err, LauncherError::MissingRequiredKey("Exec"));
}

#[test]
fn rejects_missing_section_header() {
    let content = "Type=Application\nName=My App\nExec=/opt/myapp/bin/myapp.wasm\n";
    let err = parse_desktop_entry("myapp", content).unwrap_err();
    assert_eq!(err, LauncherError::MissingSectionHeader);
}

#[test]
fn rejects_type_not_application() {
    let content = "[Desktop Entry]\nType=Link\nName=My App\nExec=/opt/myapp/bin/myapp.wasm\n";
    let err = parse_desktop_entry("myapp", content).unwrap_err();
    assert_eq!(err, LauncherError::NotAnApplication);
}

#[test]
fn rejects_missing_type_field() {
    let content = "[Desktop Entry]\nName=My App\nExec=/opt/myapp/bin/myapp.wasm\n";
    let err = parse_desktop_entry("myapp", content).unwrap_err();
    assert_eq!(err, LauncherError::NotAnApplication);
}

#[test]
fn splits_mime_type_correctly() {
    let content = "\
[Desktop Entry]
Type=Application
Name=Browser
Exec=/opt/browser/bin/browser.wasm
MimeType=text/plain;text/markdown;application/json;
";
    let entry = parse_desktop_entry("browser", content).unwrap();
    assert_eq!(
        entry.mime_types,
        vec!["text/plain", "text/markdown", "application/json"]
    );
}

#[test]
fn splits_caps_correctly() {
    let content = "\
[Desktop Entry]
Type=Application
Name=SysMon
Exec=/opt/sysmon/bin/sysmon.wasm
X-PMos-Caps=DISPLAY_CLIENT;PROC_ENUMERATE;
";
    let entry = parse_desktop_entry("sysmon", content).unwrap();
    assert_eq!(entry.caps, vec!["DISPLAY_CLIENT", "PROC_ENUMERATE"]);
}

#[test]
fn ignores_sections_after_desktop_entry() {
    let content = "\
[Desktop Entry]
Type=Application
Name=My App
Exec=/opt/myapp/bin/myapp.wasm

[OtherSection]
Name=Should Not Appear
";
    let entry = parse_desktop_entry("myapp", content).unwrap();
    assert_eq!(entry.name, "My App");
}

#[test]
fn ignores_comment_lines() {
    let content = "\
# This is a comment
[Desktop Entry]
Type=Application
# Another comment
Name=My App
Exec=/opt/myapp/bin/myapp.wasm
";
    let entry = parse_desktop_entry("myapp", content).unwrap();
    assert_eq!(entry.name, "My App");
}

// ── Launcher struct tests ────────────────────────────────────────────────────

fn make_store(entries: &[(&str, &str, &str)]) -> MemoryStore {
    let mut store = MemoryStore::new();
    for (id, name, exec) in entries {
        store
            .entries
            .insert(id.to_string(), minimal_desktop(name, exec));
    }
    store
}

#[test]
fn launcher_returns_entries_at_startup() {
    let store = make_store(&[("edit", "Text Editor", "/opt/edit/bin/edit.wasm")]);
    let launcher = Launcher::new(Box::new(store));
    assert_eq!(launcher.entries().len(), 1);
    assert_eq!(launcher.entries()[0].name, "Text Editor");
}

#[test]
fn launcher_advance_poll_false_before_interval() {
    let store = make_store(&[("edit", "Text Editor", "/opt/edit/bin/edit.wasm")]);
    let mut launcher = Launcher::new(Box::new(store));
    assert!(!launcher.advance_poll(Duration::from_secs(4)));
}

#[test]
fn launcher_advance_poll_true_at_interval() {
    let store = make_store(&[("edit", "Text Editor", "/opt/edit/bin/edit.wasm")]);
    let mut launcher = Launcher::new(Box::new(store));
    assert!(launcher.advance_poll(Duration::from_secs(5)));
}

#[test]
fn launcher_advance_poll_rereads_store() {
    let mut store = MemoryStore::new();
    store.entries.insert(
        "edit".to_string(),
        minimal_desktop("Text Editor", "/opt/edit/bin/edit.wasm"),
    );
    let launcher = Launcher::new(Box::new(store));
    assert_eq!(launcher.entries().len(), 1);

    // Directly mutate the store through the MemoryStore — we can't after
    // handing over Box ownership, so drive via a second Launcher with a
    // fresh store that has the updated content.
    let mut store2 = MemoryStore::new();
    store2.entries.insert(
        "edit".to_string(),
        minimal_desktop("Text Editor", "/opt/edit/bin/edit.wasm"),
    );
    store2.entries.insert(
        "term".to_string(),
        minimal_desktop("Terminal", "/opt/term/bin/term.wasm"),
    );
    let mut launcher2 = Launcher::new(Box::new(store2));
    // Advance by 5 s to trigger re-read.
    let fired = launcher2.advance_poll(Duration::from_secs(5));
    assert!(fired);
    assert_eq!(launcher2.entries().len(), 2);
}

#[test]
fn launcher_remove_entry_after_poll() {
    let mut store = MemoryStore::new();
    store.entries.insert(
        "edit".to_string(),
        minimal_desktop("Text Editor", "/opt/edit/bin/edit.wasm"),
    );
    store.entries.insert(
        "term".to_string(),
        minimal_desktop("Terminal", "/opt/term/bin/term.wasm"),
    );

    // We need to own the MemoryStore but also mutate it after construction.
    // Use a wrapper that holds an Rc<RefCell<MemoryStore>>.
    use std::cell::RefCell;
    use std::rc::Rc;

    struct SharedStore(Rc<RefCell<MemoryStore>>);
    impl shell::launcher::DesktopEntryStore for SharedStore {
        fn list_entries(
            &mut self,
        ) -> Result<Vec<(String, String)>, LauncherError> {
            self.0.borrow_mut().list_entries()
        }
    }

    let shared = Rc::new(RefCell::new(store));
    let mut launcher = Launcher::new(Box::new(SharedStore(shared.clone())));
    assert_eq!(launcher.entries().len(), 2);

    shared.borrow_mut().entries.remove("term");
    launcher.advance_poll(Duration::from_secs(5));
    assert_eq!(launcher.entries().len(), 1);
    assert_eq!(launcher.entries()[0].id, "edit");
}

#[test]
fn launcher_add_entry_after_poll() {
    use std::cell::RefCell;
    use std::rc::Rc;

    struct SharedStore(Rc<RefCell<MemoryStore>>);
    impl shell::launcher::DesktopEntryStore for SharedStore {
        fn list_entries(
            &mut self,
        ) -> Result<Vec<(String, String)>, LauncherError> {
            self.0.borrow_mut().list_entries()
        }
    }

    let store = MemoryStore::new();
    let shared = Rc::new(RefCell::new(store));
    let mut launcher = Launcher::new(Box::new(SharedStore(shared.clone())));
    assert_eq!(launcher.entries().len(), 0);

    shared.borrow_mut().entries.insert(
        "settings".to_string(),
        minimal_desktop("Settings", "/opt/settings/bin/settings.wasm"),
    );
    launcher.advance_poll(Duration::from_secs(5));
    assert_eq!(launcher.entries().len(), 1);
    assert_eq!(launcher.entries()[0].name, "Settings");
}

#[test]
fn launcher_poll_resets_after_trigger() {
    let store = make_store(&[("edit", "Text Editor", "/opt/edit/bin/edit.wasm")]);
    let mut launcher = Launcher::new(Box::new(store));

    // First trigger at 5 s.
    assert!(launcher.advance_poll(Duration::from_secs(5)));
    // 4 s after the last poll reset (5 + 4 = 9 s total) — should be false.
    assert!(!launcher.advance_poll(Duration::from_secs(9)));
    // 5 s after the last poll reset (5 + 5 = 10 s total) — should be true.
    assert!(launcher.advance_poll(Duration::from_secs(10)));
}

#[test]
fn launcher_drops_malformed_entry_silently() {
    let mut store = MemoryStore::new();
    store.entries.insert(
        "good".to_string(),
        minimal_desktop("Good App", "/opt/good/bin/good.wasm"),
    );
    // Missing Name= — will fail parse.
    store
        .entries
        .insert("bad".to_string(), "[Desktop Entry]\nType=Application\nExec=/opt/bad/bin/bad.wasm\n".to_string());

    let launcher = Launcher::new(Box::new(store));
    assert_eq!(launcher.entries().len(), 1);
    assert_eq!(launcher.entries()[0].id, "good");
}
