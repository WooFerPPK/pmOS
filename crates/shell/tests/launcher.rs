use shell::launcher::{
    parse_desktop_entry, DesktopEntryScan, DesktopEntryScanBatch, DesktopEntryStore,
    FilesystemStore, Launcher, LauncherClock, LauncherError, LauncherReloadStep, LauncherRuntime,
    MemoryStore, CATALOG_ENTRIES_PER_STEP, MAX_CATALOG_ENTRIES,
};
use std::time::Duration;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn minimal_desktop(name: &str, exec: &str) -> String {
    format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n")
}

struct TestCatalog(std::path::PathBuf);

impl TestCatalog {
    fn new() -> Self {
        let unique = format!(
            "pmos-shell-catalog-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos(),
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir(&path).expect("create isolated catalog");
        Self(path)
    }
}

impl Drop for TestCatalog {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn filesystem_store_reads_only_desktop_files() {
    let catalog = TestCatalog::new();
    std::fs::write(
        catalog.0.join("terminal.desktop"),
        minimal_desktop("Terminal", "/bin/term"),
    )
    .expect("write desktop entry");
    std::fs::write(catalog.0.join("notes.txt"), "not an application")
        .expect("write unrelated file");

    let mut store = FilesystemStore::new(catalog.0.clone());
    let entries = store.list_entries().expect("read catalog");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "terminal");
    assert_eq!(store.directory(), catalog.0.as_path());
}

#[test]
fn filesystem_store_reports_missing_catalog() {
    let catalog = TestCatalog::new();
    let missing = catalog.0.join("missing");
    let mut store = FilesystemStore::new(missing);
    assert!(matches!(
        store.list_entries(),
        Err(LauncherError::StoreIo(_))
    ));
}

#[test]
fn stepwise_empty_catalog_is_a_successful_unchanged_publication() {
    let mut launcher = Launcher::new_stepwise(Box::new(MemoryStore::new()));
    assert_eq!(
        launcher.step_reload(),
        LauncherReloadStep::Complete { changed: false }
    );
    assert!(!launcher.reload_pending());
    assert!(launcher.entries().is_empty());
}

#[test]
fn stepwise_begin_and_scan_failures_are_not_reported_as_completions() {
    let catalog = TestCatalog::new();
    let missing = catalog.0.join("missing");
    let mut begin_failure = Launcher::new_stepwise(Box::new(FilesystemStore::new(missing)));
    assert_eq!(begin_failure.step_reload(), LauncherReloadStep::Failed);
    assert!(!begin_failure.reload_pending());

    struct FailingStore;
    struct FailingScan;

    impl DesktopEntryStore for FailingStore {
        fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError> {
            Ok(Vec::new())
        }

        fn begin_scan(&mut self) -> Result<Option<Box<dyn DesktopEntryScan>>, LauncherError> {
            Ok(Some(Box::new(FailingScan)))
        }
    }

    impl DesktopEntryScan for FailingScan {
        fn step(&mut self) -> Result<DesktopEntryScanBatch, LauncherError> {
            Err(LauncherError::StoreIo("forced scan failure".to_string()))
        }
    }

    let mut scan_failure = Launcher::new_stepwise(Box::new(FailingStore));
    assert_eq!(scan_failure.step_reload(), LauncherReloadStep::Failed);
    assert!(!scan_failure.reload_pending());
}

#[test]
fn filesystem_catalog_scan_reads_at_most_one_desktop_file_per_step() {
    let catalog = TestCatalog::new();
    for index in 0..(CATALOG_ENTRIES_PER_STEP + 3) {
        std::fs::write(
            catalog.0.join(format!("app-{index:02}.desktop")),
            minimal_desktop(&format!("App {index}"), "/bin/app"),
        )
        .expect("write desktop entry");
    }
    let mut store = FilesystemStore::new(catalog.0.clone());
    let mut scan = store.begin_scan().unwrap().expect("filesystem scan");
    let mut total = 0usize;
    loop {
        let batch = scan.step().expect("bounded scan step");
        assert!(batch.entries.len() <= 1, "one file-read quantum per turn");
        total += batch.entries.len();
        if batch.complete {
            break;
        }
    }
    assert_eq!(total, CATALOG_ENTRIES_PER_STEP + 3);
}

#[test]
fn stepwise_launcher_keeps_stable_cache_until_complete_snapshot() {
    let catalog = TestCatalog::new();
    for index in 0..3 {
        std::fs::write(
            catalog.0.join(format!("app-{index}.desktop")),
            minimal_desktop(&format!("App {index}"), "/bin/app"),
        )
        .expect("write desktop entry");
    }
    let mut launcher = Launcher::new_stepwise(Box::new(FilesystemStore::new(catalog.0.clone())));
    assert!(launcher.entries().is_empty());
    let mut pending_steps = 0;
    loop {
        match launcher.step_reload() {
            LauncherReloadStep::Pending => {
                pending_steps += 1;
                assert!(launcher.entries().is_empty());
            }
            LauncherReloadStep::Complete { changed } => {
                assert!(changed);
                break;
            }
            LauncherReloadStep::Failed => panic!("scan failed before completion"),
            LauncherReloadStep::Idle => panic!("scan became idle before completion"),
        }
    }
    assert!(pending_steps >= 3);
    assert_eq!(launcher.entries().len(), 3);
}

#[test]
fn stepwise_launcher_parses_exactly_one_manifest_per_turn() {
    let catalog = TestCatalog::new();
    for index in 0..3 {
        std::fs::write(
            catalog.0.join(format!("parse-{index}.desktop")),
            minimal_desktop(&format!("Parse {index}"), "/bin/app"),
        )
        .expect("write desktop entry");
    }
    let mut launcher = Launcher::new_stepwise(Box::new(FilesystemStore::new(catalog.0.clone())));
    while launcher.pending_parse_count() < 3 {
        assert_eq!(launcher.step_reload(), LauncherReloadStep::Pending);
        assert!(launcher.entries().is_empty());
    }
    assert_eq!(launcher.pending_parse_count(), 3);
    assert_eq!(launcher.parsed_entry_count(), 0);
    assert_eq!(
        launcher.step_reload(),
        LauncherReloadStep::Pending,
        "EOF detection begins the separate parse phase"
    );
    assert_eq!(launcher.pending_parse_count(), 3);
    assert_eq!(launcher.parsed_entry_count(), 0);

    assert_eq!(launcher.step_reload(), LauncherReloadStep::Pending);
    assert_eq!(launcher.pending_parse_count(), 2);
    assert_eq!(launcher.parsed_entry_count(), 1);
    assert!(launcher.entries().is_empty());

    assert_eq!(launcher.step_reload(), LauncherReloadStep::Pending);
    assert_eq!(launcher.pending_parse_count(), 1);
    assert_eq!(launcher.parsed_entry_count(), 2);
    assert!(launcher.entries().is_empty());

    assert_eq!(
        launcher.step_reload(),
        LauncherReloadStep::Complete { changed: true }
    );
    assert_eq!(launcher.pending_parse_count(), 0);
    assert_eq!(launcher.parsed_entry_count(), 0);
    assert_eq!(launcher.entries().len(), 3);
}

#[test]
fn filesystem_catalog_admits_exact_entry_cap_and_rejects_next() {
    let catalog = TestCatalog::new();
    for index in 0..MAX_CATALOG_ENTRIES {
        std::fs::write(
            catalog.0.join(format!("bounded-{index:03}.desktop")),
            minimal_desktop(&format!("Bounded {index}"), "/bin/app"),
        )
        .expect("write desktop entry");
    }
    let mut store = FilesystemStore::new(catalog.0.clone());
    let mut scan = store.begin_scan().unwrap().expect("filesystem scan");
    let mut admitted = 0usize;
    loop {
        let batch = scan.step().expect("catalog at exact cap is admitted");
        admitted += batch.entries.len();
        if batch.complete {
            break;
        }
    }
    assert_eq!(admitted, MAX_CATALOG_ENTRIES);

    std::fs::write(
        catalog.0.join("overflow.desktop"),
        minimal_desktop("Overflow", "/bin/app"),
    )
    .expect("write overflow entry");
    let mut scan = store.begin_scan().unwrap().expect("filesystem scan");
    let error = loop {
        match scan.step() {
            Ok(batch) if batch.complete => panic!("N+1 catalog unexpectedly completed"),
            Ok(_) => {}
            Err(error) => break error,
        }
    };
    assert!(matches!(error, LauncherError::StoreIo(message) if message.contains("exceeds")));
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
fn launcher_orders_core_apps_before_installed_extras() {
    let store = make_store(&[
        ("zebra", "Zebra", "/bin/zebra"),
        ("files", "Files", "/bin/files"),
        ("terminal", "Terminal", "/bin/term"),
        ("alpha", "Alpha", "/bin/alpha"),
    ]);
    let launcher = Launcher::new(Box::new(store));
    let ids: Vec<&str> = launcher
        .entries()
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    assert_eq!(ids, vec!["terminal", "files", "alpha", "zebra"]);
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
        fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError> {
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
        fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError> {
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
    store.entries.insert(
        "bad".to_string(),
        "[Desktop Entry]\nType=Application\nExec=/opt/bad/bin/bad.wasm\n".to_string(),
    );

    let launcher = Launcher::new(Box::new(store));
    assert_eq!(launcher.entries().len(), 1);
    assert_eq!(launcher.entries()[0].id, "good");
}

#[test]
fn live_runtime_refreshes_changed_catalog_at_five_seconds_only() {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    struct SharedStore(Rc<RefCell<MemoryStore>>);
    impl DesktopEntryStore for SharedStore {
        fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError> {
            self.0.borrow_mut().list_entries()
        }
    }

    struct StepClock(VecDeque<Duration>);
    impl LauncherClock for StepClock {
        fn elapsed(&mut self) -> Duration {
            self.0.pop_front().expect("clock sample")
        }
    }

    let mut store = MemoryStore::new();
    store.entries.insert(
        "terminal".to_string(),
        minimal_desktop("Terminal", "/bin/term"),
    );
    let shared = Rc::new(RefCell::new(store));
    let launcher = Launcher::new(Box::new(SharedStore(shared.clone())));
    let clock = StepClock(VecDeque::from([
        Duration::from_secs(4),
        Duration::from_secs(5),
        Duration::from_secs(10),
    ]));
    let mut runtime = LauncherRuntime::new(launcher, clock).with_clock_check_every_iterations(1);

    shared.borrow_mut().entries.insert(
        "notes".to_string(),
        minimal_desktop("Notes", "/opt/notes/bin/notes"),
    );
    assert!(
        !runtime.poll(),
        "catalog must not refresh before five seconds"
    );
    assert_eq!(runtime.entries().len(), 1);
    assert!(
        runtime.poll(),
        "changed catalog must surface at five seconds"
    );
    assert_eq!(runtime.entries().len(), 2);
    assert!(runtime.entries().iter().any(|entry| entry.id == "notes"));
    assert!(
        !runtime.poll(),
        "an unchanged scheduled rescan must not request a desktop repaint",
    );
}
