//! T160 — files isolation tests against a fixture directory.
//! No display server, no toolkit — just exercise the directory
//! reader, importer, exporter, and rename helpers the GUI binds
//! to its window.

use std::fs;
use std::time::Duration;

use files::{
    default_app_for, export_bytes, import_and_dispatch, import_bytes, list_dir,
    parse_text_dispatch, rename, sanitise_filename, unique_path, DialogKind, DirectoryScanStep,
    DirectoryScanner, DispatchError, DoubleActivation, FileAction, FileEntry, FileManagerState,
    PointerTarget, StdFileSystem, StepwiseAction, UiKey, ViewMode, DIRECTORY_ENTRIES_PER_STEP,
    MAX_DESKTOP_ENTRY_BYTES, PREVIEW_LIMIT_BYTES,
};

#[test]
fn directory_scan_is_bounded_and_publishes_one_stable_snapshot() {
    let tmp = tempdir("files-stepwise-directory");
    for index in 0..DIRECTORY_ENTRIES_PER_STEP * 2 + 1 {
        fs::write(tmp.join(format!("file-{index:03}.txt")), b"x").unwrap();
    }
    let mut scanner = DirectoryScanner::start(&tmp).expect("open directory");
    assert!(matches!(scanner.step(), DirectoryScanStep::Pending));
    assert_eq!(scanner.collected_len(), DIRECTORY_ENTRIES_PER_STEP);
    assert!(matches!(scanner.step(), DirectoryScanStep::Pending));
    assert_eq!(scanner.collected_len(), DIRECTORY_ENTRIES_PER_STEP * 2);
    let entries = match scanner.step() {
        DirectoryScanStep::Complete(Ok(entries)) => entries,
        _ => panic!("third bounded turn must finish the 33-entry directory"),
    };
    assert_eq!(entries.len(), DIRECTORY_ENTRIES_PER_STEP * 2 + 1);
    assert_eq!(entries[0].name, "file-000.txt");
    assert_eq!(entries.last().unwrap().name, "file-032.txt");

    let mut state = FileManagerState::from_entries(
        "/stable",
        vec![FileEntry {
            name: "old.txt".to_string(),
            is_dir: false,
        }],
    );
    let mut pending = match state.begin_stepwise_action(
        FileAction::Navigate {
            path: tmp.clone(),
            select_name: Some("file-020.txt".to_string()),
        },
        &StdFileSystem,
    ) {
        StepwiseAction::Pending(pending) => pending,
        StepwiseAction::Complete(_) => panic!("production filesystem must scan incrementally"),
    };
    assert_eq!(state.cwd(), std::path::Path::new("/stable"));
    assert_eq!(state.entries()[0].name, "old.txt");
    assert!(pending.step(&mut state).is_none());
    assert_eq!(state.cwd(), std::path::Path::new("/stable"));
    while pending.step(&mut state).is_none() {}
    assert_eq!(state.cwd(), tmp.as_path());
    assert_eq!(state.selected_entry().unwrap().name, "file-020.txt");
    fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn list_dir_returns_dirs_first_then_files_alphabetically() {
    let tmp = tempdir("files-list");
    fs::create_dir(tmp.join("sub")).unwrap();
    fs::write(tmp.join("zfile.txt"), b"z").unwrap();
    fs::write(tmp.join("afile.txt"), b"a").unwrap();
    fs::create_dir(tmp.join("alpha")).unwrap();

    let (entries, dirs, file_count) = list_dir(tmp.to_str().unwrap());
    assert_eq!(dirs, 2);
    assert_eq!(file_count, 2);
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["alpha", "sub", "afile.txt", "zfile.txt"]);

    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn list_missing_dir_returns_empty() {
    let (entries, dirs, files) = files::list_dir("/this/path/should/not/exist/at/all");
    assert!(entries.is_empty());
    assert_eq!(dirs, 0);
    assert_eq!(files, 0);
}

#[test]
fn import_bytes_writes_file_and_returns_final_path() {
    let tmp = tempdir("files-import");
    let final_path = import_bytes(tmp.to_str().unwrap(), "hello.txt", b"hi from host\n").unwrap();
    assert_eq!(final_path, tmp.join("hello.txt"));
    assert_eq!(fs::read(&final_path).unwrap(), b"hi from host\n");
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn import_bytes_avoids_overwrite_via_unique_suffix() {
    let tmp = tempdir("files-import-dup");
    fs::write(tmp.join("note.txt"), b"existing").unwrap();
    let final_path = import_bytes(tmp.to_str().unwrap(), "note.txt", b"fresh").unwrap();
    assert_eq!(
        final_path.file_name().and_then(|s| s.to_str()),
        Some("note (1).txt")
    );
    assert_eq!(fs::read_to_string(&final_path).unwrap(), "fresh");
    assert_eq!(
        fs::read_to_string(tmp.join("note.txt")).unwrap(),
        "existing"
    );
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn import_bytes_rejects_non_directory_target() {
    let tmp = tempdir("files-import-bad");
    let path = tmp.join("a-file");
    fs::write(&path, b"not a dir").unwrap();
    let err = import_bytes(path.to_str().unwrap(), "x.txt", b"y").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotADirectory);
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn sanitise_strips_path_separators_and_dotnames() {
    assert_eq!(sanitise_filename("foo.txt"), "foo.txt");
    assert_eq!(sanitise_filename("/etc/passwd"), "passwd");
    assert_eq!(sanitise_filename("a/b/c/d.png"), "d.png");
    assert_eq!(sanitise_filename(""), "untitled");
    assert_eq!(sanitise_filename("."), "untitled");
    assert_eq!(sanitise_filename(".."), "untitled");
}

#[test]
fn unique_path_picks_first_free_numbered_variant() {
    let tmp = tempdir("files-unique");
    fs::write(tmp.join("a.txt"), b"x").unwrap();
    fs::write(tmp.join("a (1).txt"), b"x").unwrap();
    let p = unique_path(&tmp, "a.txt");
    assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("a (2).txt"));

    let p = unique_path(&tmp, "b.txt");
    assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("b.txt"));

    let p = unique_path(&tmp, "noext");
    assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("noext"));
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn import_collision_keeps_a_255_byte_utf8_name_within_vfs_limit() {
    let tmp = tempdir("files-max-name-collision");
    let name = format!("{}a.txt", "é".repeat(125));
    assert_eq!(name.len(), 255);
    fs::write(tmp.join(&name), b"original").unwrap();

    let imported = import_bytes(tmp.to_str().unwrap(), &name, b"imported").unwrap();
    let imported_name = imported.file_name().unwrap().to_str().unwrap();
    assert_ne!(imported_name, name);
    assert!(imported_name.len() <= 255);
    assert!(imported_name.ends_with(".txt"));
    assert_eq!(fs::read(tmp.join(&name)).unwrap(), b"original");
    assert_eq!(fs::read(imported).unwrap(), b"imported");
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn export_bytes_round_trips_a_file() {
    let tmp = tempdir("files-export");
    let path = tmp.join("data.bin");
    let body = b"\x00\x01\x02\x03\x04\xfe\xff";
    fs::write(&path, body).unwrap();
    let bytes = export_bytes(path.to_str().unwrap()).unwrap();
    assert_eq!(bytes, body);
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn rename_preserves_inode_so_open_fd_keeps_reading() {
    let tmp = tempdir("files-rename");
    let old = tmp.join("old.txt");
    let new = tmp.join("new.txt");
    fs::write(&old, b"line1\nline2\n").unwrap();

    use std::io::Read;
    let mut fd = fs::File::open(&old).unwrap();

    rename(old.to_str().unwrap(), new.to_str().unwrap()).unwrap();
    assert!(!old.exists());
    assert!(new.exists());

    let mut buf = String::new();
    fd.read_to_string(&mut buf).unwrap();
    assert_eq!(buf, "line1\nline2\n");

    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn default_app_for_text_returns_edit_desktop() {
    assert_eq!(
        default_app_for("readme.md", None),
        Some("/usr/share/applications/edit.desktop"),
    );
    assert_eq!(
        default_app_for("notes.txt", None),
        Some("/usr/share/applications/edit.desktop"),
    );
    assert_eq!(
        default_app_for("init.toml", None),
        Some("/usr/share/applications/edit.desktop"),
    );
    assert_eq!(
        default_app_for("unknown", Some("text/plain")),
        Some("/usr/share/applications/edit.desktop"),
    );
}

#[test]
fn default_app_for_binary_returns_none() {
    assert_eq!(default_app_for("photo.bin", None), None);
    assert_eq!(default_app_for("blob.png", Some("image/png")), None);
}

#[test]
fn installed_text_entry_becomes_a_direct_spawn_plan_with_narrow_caps() {
    let selected = std::path::Path::new("/home/user/Documents/two words.txt");
    let entry = b"[Desktop Entry]\nType=Application\nName=Edit\nExec=/bin/edit\nMimeType=text/plain;text/markdown;\nX-PMos-Caps=DISPLAY_CLIENT\n";
    let dispatch = parse_text_dispatch(
        "/usr/share/applications/edit.desktop",
        entry,
        selected,
        "text/plain",
        abi::cap::initial::FILES,
    )
    .unwrap();

    assert_eq!(dispatch.executable, "/bin/edit");
    assert_eq!(
        dispatch.argv,
        ["/bin/edit", "/home/user/Documents/two words.txt"]
    );
    assert_eq!(dispatch.caps, abi::cap::initial::ORDINARY_APP);
    assert!(dispatch.caps.is_subset_of(abi::cap::initial::FILES));
}

#[test]
fn exec_tokenizer_replaces_exact_file_field_without_a_shell() {
    let entry = b"[Desktop Entry]\nType=Application\nExec=/bin/edit --mode 'plain text' %f\nMimeType=text/plain;\nX-PMos-Caps=DISPLAY_CLIENT\n";
    let dispatch = parse_text_dispatch(
        "/usr/share/applications/edit.desktop",
        entry,
        std::path::Path::new("/home/user/a b.txt"),
        "text/plain",
        abi::cap::initial::FILES,
    )
    .unwrap();
    assert_eq!(
        dispatch.argv,
        ["/bin/edit", "--mode", "plain text", "/home/user/a b.txt"]
    );
}

#[test]
fn malformed_or_privilege_widening_desktop_entries_are_rejected() {
    let path = std::path::Path::new("/home/user/note.txt");
    let parse = |entry: &str, held| {
        parse_text_dispatch(
            "/usr/share/applications/edit.desktop",
            entry.as_bytes(),
            path,
            "text/plain",
            held,
        )
    };

    assert!(matches!(
        parse(
            "[Desktop Entry]\nType=Application\nExec=/bin/edit\nExec=/bin/other\nMimeType=text/plain;\n",
            abi::cap::initial::FILES,
        ),
        Err(DispatchError::DuplicateKey(key)) if key == "Exec"
    ));
    assert!(matches!(
        parse(
            "[Desktop Entry]\nType=Application\nExec=bin/edit\nMimeType=text/plain;\n",
            abi::cap::initial::FILES,
        ),
        Err(DispatchError::InvalidExec)
    ));
    assert!(matches!(
        parse(
            "[Desktop Entry]\nType=Application\nExec=/bin/edit %u\nMimeType=text/plain;\n",
            abi::cap::initial::FILES,
        ),
        Err(DispatchError::UnsupportedExecFieldCode(code)) if code == "%u"
    ));
    assert!(matches!(
        parse(
            "[Desktop Entry]\nType=Application\nExec=/bin/edit\nMimeType=text/plain;\nX-PMos-Caps=PROC_KILL_ANY\n",
            abi::cap::initial::FILES,
        ),
        Err(DispatchError::CapabilityNotDelegable(name)) if name == "PROC_KILL_ANY"
    ));
    assert!(matches!(
        parse(
            "[Desktop Entry]\nType=Application\nExec=/bin/edit\nMimeType=text/plain;\nX-PMos-Caps=FUTURE_ADMIN\n",
            abi::cap::initial::FILES,
        ),
        Err(DispatchError::UnknownCapability(name)) if name == "FUTURE_ADMIN"
    ));
    assert!(matches!(
        parse(
            "[Desktop Entry]\nType=Application\nExec=/bin/edit\nMimeType=image/png;\n",
            abi::cap::initial::FILES,
        ),
        Err(DispatchError::MimeMismatch)
    ));
    assert!(matches!(
        parse_text_dispatch(
            "/usr/share/applications/edit.desktop",
            &vec![b'x'; MAX_DESKTOP_ENTRY_BYTES + 1],
            path,
            "text/plain",
            abi::cap::initial::FILES,
        ),
        Err(DispatchError::TooLarge)
    ));
}

#[test]
fn declared_caps_must_also_be_held_by_the_live_files_process() {
    let entry = b"[Desktop Entry]\nType=Application\nExec=/bin/edit\nMimeType=text/plain;\nX-PMos-Caps=DISPLAY_CLIENT\n";
    let result = parse_text_dispatch(
        "/usr/share/applications/edit.desktop",
        entry,
        std::path::Path::new("/home/user/note.txt"),
        "text/plain",
        abi::cap::CapSet::from_caps(&[abi::cap::Cap::HostTransfer]),
    );
    assert!(matches!(
        result,
        Err(DispatchError::CapabilityNotDelegable(name)) if name == "DISPLAY_CLIENT"
    ));
}

#[test]
fn import_and_dispatch_returns_path_and_text_handler() {
    let tmp = tempdir("files-dispatch");
    let (path, dispatch) =
        import_and_dispatch(tmp.to_str().unwrap(), "imported.md", None, b"hello\n").unwrap();
    assert!(path.exists());
    assert_eq!(dispatch, Some("/usr/share/applications/edit.desktop"));
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn cli_import_export_round_trip_through_binary() {
    let tmp = tempdir("files-cli-rt");
    let bin = env!("CARGO_BIN_EXE_files");

    // Import ASCII "abc" => 0x61 0x62 0x63 hex
    let import = std::process::Command::new(bin)
        .args(["import", tmp.to_str().unwrap(), "abc.txt", "616263"])
        .output()
        .unwrap();
    assert!(
        import.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&import.stderr)
    );
    let stdout = String::from_utf8_lossy(&import.stdout);
    assert!(stdout.contains("imported"));
    assert!(stdout.contains("dispatch /usr/share/applications/edit.desktop"));

    let imported = tmp.join("abc.txt");
    let export = std::process::Command::new(bin)
        .args(["export", imported.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(export.status.success());
    assert_eq!(export.stdout, b"abc");

    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn state_keyboard_selection_scrolls_and_clamps() {
    let entries = (0..12)
        .map(|index| FileEntry {
            name: format!("entry-{index:02}"),
            is_dir: false,
        })
        .collect();
    let mut state = FileManagerState::from_entries("/home/user", entries);

    assert_eq!(state.selected_index(), Some(0));
    assert_eq!(state.handle_key(UiKey::PageDown, 4), None);
    assert_eq!(state.selected_index(), Some(4));
    assert_eq!(state.scroll(), 1);

    state.handle_key(UiKey::End, 4);
    assert_eq!(state.selected_index(), Some(11));
    assert_eq!(state.scroll(), 8);
    state.handle_key(UiKey::Home, 4);
    assert_eq!(state.selected_index(), Some(0));
    assert_eq!(state.scroll(), 0);

    state.handle_key(UiKey::Escape, 4);
    assert_eq!(state.selected_index(), None);
    state.handle_key(UiKey::Up, 4);
    assert_eq!(state.selected_index(), Some(11));
}

#[test]
fn pointer_selects_rows_and_scroll_buttons_move_viewport() {
    let entries = (0..8)
        .map(|index| FileEntry {
            name: format!("dir-{index}"),
            is_dir: true,
        })
        .collect();
    let mut state = FileManagerState::from_entries("/home/user", entries);
    state.handle_key(UiKey::Escape, 3);

    state.handle_pointer(PointerTarget::ScrollDown, 3);
    state.handle_pointer(PointerTarget::ScrollDown, 3);
    assert_eq!(state.scroll(), 2);
    assert_eq!(state.handle_pointer(PointerTarget::Entry(4), 3), None);
    assert_eq!(state.selected_index(), Some(4));
    assert!(state.status().contains("dir-4"));
    assert_eq!(
        state.handle_pointer(PointerTarget::Open, 3),
        Some(FileAction::Navigate {
            path: "/home/user/dir-4".into(),
            select_name: None,
        })
    );
}

#[test]
fn pointer_toolbar_controls_emit_the_same_confined_actions_as_keyboard() {
    let mut state = FileManagerState::from_entries(
        "/home/user",
        vec![FileEntry {
            name: "notes".to_string(),
            is_dir: true,
        }],
    );

    assert_eq!(
        state.handle_pointer(PointerTarget::Parent, 10),
        Some(FileAction::Navigate {
            path: "/home".into(),
            select_name: Some("user".to_string()),
        })
    );
    assert_eq!(state.handle_pointer(PointerTarget::NewFolder, 10), None);
    assert!(matches!(
        state.mode(),
        ViewMode::Input {
            kind: DialogKind::CreateFolder,
            ..
        }
    ));
    state.handle_key(UiKey::Escape, 10);

    assert_eq!(state.handle_pointer(PointerTarget::Rename, 10), None);
    assert!(matches!(
        state.mode(),
        ViewMode::Input {
            kind: DialogKind::Rename,
            ..
        }
    ));
    state.handle_key(UiKey::Escape, 10);

    assert_eq!(state.handle_pointer(PointerTarget::Delete, 10), None);
    assert!(matches!(state.mode(), ViewMode::ConfirmDelete { .. }));
    state.handle_key(UiKey::Escape, 10);
    assert_eq!(
        state.handle_pointer(PointerTarget::Refresh, 10),
        Some(FileAction::Refresh)
    );
    assert_eq!(
        state.handle_pointer(PointerTarget::Import, 10),
        Some(FileAction::RequestHostImport)
    );
    assert_eq!(
        state.handle_pointer(PointerTarget::Export, 10),
        None,
        "directories cannot be exported as a truncated archive"
    );
    assert!(state.status().contains("folders are not supported"));
    assert_eq!(
        state.handle_pointer(PointerTarget::Close, 10),
        Some(FileAction::Close)
    );
}

#[test]
fn host_transfer_actions_are_confined_to_files_and_report_completion() {
    let tmp = tempdir("files-host-actions");
    fs::write(tmp.join("notes.txt"), b"hello").unwrap();
    let mut state = FileManagerState::from_directory(&tmp).unwrap();

    assert_eq!(
        state.handle_key(UiKey::Char('i'), 10),
        Some(FileAction::RequestHostImport)
    );
    state.host_import_pending();
    assert!(state.status().contains("Waiting for host file"));
    state.host_transfer_progress("Importing", "from-host.txt", 4, 8);
    assert_eq!(state.status(), "Importing from-host.txt: 4 / 8 bytes");

    let imported = tmp.join("from-host.txt");
    fs::write(&imported, b"imported").unwrap();
    let outcome = state.complete_host_import(&imported, &files::StdFileSystem);
    assert!(outcome.changed);
    assert_eq!(
        state.selected_entry().map(|entry| entry.name.as_str()),
        Some("from-host.txt")
    );
    assert!(state.status().starts_with("Imported "));

    assert_eq!(
        state.handle_pointer(PointerTarget::Export, 10),
        Some(FileAction::RequestHostExport(imported.clone()))
    );
    let outcome = state.host_export_complete(&imported);
    assert!(outcome.changed);
    assert!(state.status().starts_with("Exported "));

    state.host_transfer_failed("import", "permission denied");
    assert_eq!(state.status(), "Error: import: permission denied");
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn post_import_refresh_is_deferred_and_keeps_the_old_snapshot_until_complete() {
    let tmp = tempdir("files-post-import-refresh");
    for index in 0..(DIRECTORY_ENTRIES_PER_STEP + 1) {
        fs::write(tmp.join(format!("existing-{index:02}.txt")), b"old").unwrap();
    }
    let imported = tmp.join("from-host.txt");
    fs::write(&imported, b"imported").unwrap();
    let stable = vec![FileEntry {
        name: "previous-snapshot.txt".to_string(),
        is_dir: false,
    }];
    let mut state = FileManagerState::from_entries(tmp.clone(), stable.clone());

    let mut pending = match state.begin_complete_host_import(&imported, &StdFileSystem) {
        StepwiseAction::Pending(pending) => pending,
        StepwiseAction::Complete(_) => panic!("production filesystem must defer the refresh"),
    };
    assert_eq!(state.entries(), stable);
    assert!(pending.step(&mut state).is_none());
    assert_eq!(state.entries(), stable);

    let outcome = loop {
        if let Some(outcome) = pending.step(&mut state) {
            break outcome;
        }
        assert_eq!(state.entries(), stable);
    };
    assert!(outcome.changed);
    assert_eq!(
        state.selected_entry().map(|entry| entry.name.as_str()),
        Some("from-host.txt")
    );
    assert!(state.status().starts_with("Imported "));
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn backspace_requests_parent_and_reselects_departed_directory() {
    let mut state = FileManagerState::from_entries("/home/user/notes", Vec::new());
    assert_eq!(
        state.handle_key(UiKey::Backspace, 10),
        Some(FileAction::Navigate {
            path: "/home/user".into(),
            select_name: Some("notes".to_string()),
        })
    );
}

#[test]
fn create_and_rename_dialogs_emit_confined_child_actions() {
    let mut state = FileManagerState::from_entries(
        "/home/user",
        vec![FileEntry {
            name: "old-name".to_string(),
            is_dir: true,
        }],
    );

    state.handle_key(UiKey::Char('n'), 10);
    assert!(matches!(
        state.mode(),
        ViewMode::Input {
            kind: DialogKind::CreateFolder,
            ..
        }
    ));
    for ch in "notes".chars() {
        state.handle_key(UiKey::Char(ch), 10);
    }
    assert_eq!(
        state.handle_key(UiKey::Enter, 10),
        Some(FileAction::CreateFolder("/home/user/notes".into()))
    );

    state.handle_key(UiKey::Char('r'), 10);
    for ch in "renamed".chars() {
        state.handle_key(UiKey::Char(ch), 10);
    }
    assert_eq!(
        state.handle_key(UiKey::Enter, 10),
        Some(FileAction::Rename {
            old_path: "/home/user/old-name".into(),
            new_path: "/home/user/renamed".into(),
        })
    );
}

#[test]
fn invalid_dialog_name_stays_open_with_error_feedback() {
    let mut state = FileManagerState::from_entries("/home/user", Vec::new());
    state.handle_key(UiKey::Char('n'), 10);
    for ch in "../escape".chars() {
        state.handle_key(UiKey::Char(ch), 10);
    }
    assert_eq!(state.handle_key(UiKey::Enter, 10), None);
    assert!(matches!(state.mode(), ViewMode::Input { .. }));
    assert!(state.status().starts_with("Error:"));
}

#[test]
fn delete_requires_confirmation_and_escape_cancels() {
    let mut state = FileManagerState::from_entries(
        "/home/user",
        vec![FileEntry {
            name: "notes".to_string(),
            is_dir: true,
        }],
    );
    state.handle_key(UiKey::Delete, 10);
    assert!(matches!(state.mode(), ViewMode::ConfirmDelete { .. }));
    assert_eq!(state.handle_key(UiKey::Escape, 10), None);
    assert_eq!(state.mode(), &ViewMode::Browse);

    state.handle_key(UiKey::Char('d'), 10);
    assert_eq!(
        state.handle_key(UiKey::Enter, 10),
        Some(FileAction::Delete {
            path: "/home/user/notes".into(),
            is_dir: true,
        })
    );
}

#[test]
fn text_file_open_uses_default_app_while_preview_remains_explicit() {
    let tmp = tempdir("files-preview");
    let path = tmp.join("readme.txt");
    fs::write(&path, b"alpha\nbeta\ngamma\n").unwrap();
    let before = fs::read(&path).unwrap();
    let mut state = FileManagerState::from_directory(&tmp).unwrap();

    assert_eq!(
        state.handle_key(UiKey::Enter, 2),
        Some(FileAction::OpenDefault(path.clone()))
    );
    let action = state.handle_key(UiKey::Char('p'), 2).unwrap();
    let outcome = state.execute(action);
    assert!(outcome.changed);
    match state.mode() {
        ViewMode::Preview(preview) => {
            assert_eq!(preview.lines, vec!["alpha", "beta", "gamma"]);
            assert_eq!(preview.scroll, 0);
        }
        other => panic!("expected preview, got {other:?}"),
    }
    state.handle_key(UiKey::PageDown, 2);
    assert!(matches!(state.mode(), ViewMode::Preview(preview) if preview.scroll == 2));
    state.handle_pointer(PointerTarget::ScrollUp, 2);
    assert!(matches!(state.mode(), ViewMode::Preview(preview) if preview.scroll == 1));
    state.handle_key(UiKey::Backspace, 2);
    assert_eq!(state.mode(), &ViewMode::Browse);
    assert_eq!(fs::read(&path).unwrap(), before);
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn two_presses_on_the_same_row_within_the_bound_double_activate() {
    let mut tracker = DoubleActivation::default();
    assert!(!tracker.press(3, Duration::from_millis(100)));
    assert!(tracker.press(3, Duration::from_millis(599)));

    assert!(!tracker.press(3, Duration::from_secs(1)));
    tracker.cancel();
    assert!(!tracker.press(3, Duration::from_millis(1_100)));
    assert!(!tracker.press(4, Duration::from_millis(1_100)));
    assert!(!tracker.press(4, Duration::from_millis(1_601)));
    assert!(tracker.press(4, Duration::from_millis(1_700)));
}

#[test]
fn binary_preview_fails_without_mutating_the_file() {
    let tmp = tempdir("files-preview-binary");
    let path = tmp.join("blob.bin");
    let bytes = b"abc\0def";
    fs::write(&path, bytes).unwrap();
    let mut state = FileManagerState::from_directory(&tmp).unwrap();

    let action = state.handle_key(UiKey::Char('p'), 10).unwrap();
    let outcome = state.execute(action);
    assert!(outcome.log.unwrap().contains("error preview"));
    assert_eq!(state.mode(), &ViewMode::Browse);
    assert!(state.status().contains("binary"));
    assert_eq!(fs::read(&path).unwrap(), bytes);
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn text_preview_is_bounded_and_reports_truncation() {
    let tmp = tempdir("files-preview-limit");
    let path = tmp.join("large.txt");
    fs::write(&path, vec![b'x'; PREVIEW_LIMIT_BYTES as usize + 100]).unwrap();
    let mut state = FileManagerState::from_directory(&tmp).unwrap();

    let action = state.handle_key(UiKey::Char('p'), 10).unwrap();
    state.execute(action);
    assert!(matches!(
        state.mode(),
        ViewMode::Preview(preview) if preview.truncated
    ));
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn refresh_reloads_external_vfs_changes_and_updates_status() {
    let tmp = tempdir("files-refresh");
    let mut state = FileManagerState::from_directory(&tmp).unwrap();
    assert!(state.entries().is_empty());
    fs::write(tmp.join("appeared.txt"), b"new").unwrap();

    let action = state.handle_key(UiKey::Char('g'), 10).unwrap();
    let outcome = state.execute(action);
    assert!(outcome.changed);
    assert_eq!(state.entries()[0].name, "appeared.txt");
    assert!(state.status().starts_with("Refreshed "));
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn real_actions_create_navigate_rename_and_delete_inside_vfs() {
    let tmp = tempdir("files-state-workflow");
    let mut state = FileManagerState::from_directory(&tmp).unwrap();

    state.handle_key(UiKey::Char('n'), 10);
    for ch in "workflow".chars() {
        state.handle_key(UiKey::Char(ch), 10);
    }
    let create = state.handle_key(UiKey::Enter, 10).unwrap();
    assert!(state
        .execute(create)
        .log
        .unwrap()
        .contains("created folder"));
    assert!(tmp.join("workflow").is_dir());

    let enter = state.handle_key(UiKey::Enter, 10).unwrap();
    assert!(state.execute(enter).log.unwrap().contains("cwd"));
    assert_eq!(state.cwd(), tmp.join("workflow"));

    state.handle_key(UiKey::Char('n'), 10);
    for ch in "drafts".chars() {
        state.handle_key(UiKey::Char(ch), 10);
    }
    let create_child = state.handle_key(UiKey::Enter, 10).unwrap();
    state.execute(create_child);
    state.handle_key(UiKey::Char('r'), 10);
    for ch in "archive".chars() {
        state.handle_key(UiKey::Char(ch), 10);
    }
    let rename_child = state.handle_key(UiKey::Enter, 10).unwrap();
    assert!(state.execute(rename_child).log.unwrap().contains("renamed"));
    assert!(!tmp.join("workflow/drafts").exists());
    assert!(tmp.join("workflow/archive").is_dir());

    state.handle_key(UiKey::Char('d'), 10);
    let delete_child = state.handle_key(UiKey::Enter, 10).unwrap();
    assert!(state.execute(delete_child).log.unwrap().contains("deleted"));
    assert!(!tmp.join("workflow/archive").exists());

    let parent = state.handle_key(UiKey::Backspace, 10).unwrap();
    state.execute(parent);
    assert_eq!(state.cwd(), tmp.as_path());
    assert_eq!(
        state.selected_entry().map(|entry| entry.name.as_str()),
        Some("workflow")
    );
    state.handle_key(UiKey::Char('d'), 10);
    let delete_parent = state.handle_key(UiKey::Enter, 10).unwrap();
    state.execute(delete_parent);
    assert!(!tmp.join("workflow").exists());
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn nonempty_directory_delete_reports_error_and_preserves_content() {
    let tmp = tempdir("files-delete-nonempty");
    fs::create_dir(tmp.join("notes")).unwrap();
    fs::write(tmp.join("notes/keep.txt"), b"keep").unwrap();
    let mut state = FileManagerState::from_directory(&tmp).unwrap();

    state.handle_key(UiKey::Char('d'), 10);
    let delete = state.handle_key(UiKey::Enter, 10).unwrap();
    let outcome = state.execute(delete);
    assert!(outcome.log.unwrap().contains("error delete"));
    assert!(state.status().starts_with("Error:"));
    assert_eq!(fs::read(tmp.join("notes/keep.txt")).unwrap(), b"keep");
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn close_shortcut_emits_close_action_without_touching_vfs() {
    let mut state = FileManagerState::from_entries("/home/user", Vec::new());
    assert_eq!(state.handle_key(UiKey::Close, 10), Some(FileAction::Close));
    let outcome = state.execute(FileAction::Close);
    assert!(outcome.close);
    assert!(!outcome.changed);
}

#[test]
fn painted_titlebar_is_the_only_files_drag_initiation_region() {
    let width = 640;
    assert!(files::titlebar_drag_hit(0, 0, width));
    assert!(files::titlebar_drag_hit(
        width as i32 - 1,
        files::TITLEBAR_HEIGHT as i32 - 1,
        width
    ));
    assert!(!files::titlebar_drag_hit(-1, 4, width));
    assert!(!files::titlebar_drag_hit(width as i32, 4, width));
    assert!(!files::titlebar_drag_hit(
        8,
        files::TITLEBAR_HEIGHT as i32,
        width
    ));
}

#[test]
fn normal_files_geometry_does_not_consume_a_large_work_area() {
    assert_eq!(
        files::configured_window_size(false, (1024, 736)),
        (files::NORMAL_WINDOW_WIDTH, files::NORMAL_WINDOW_HEIGHT)
    );
    assert_eq!(files::configured_window_size(false, (480, 300)), (480, 300));
}

#[test]
fn maximized_files_geometry_uses_the_exact_work_area_offer() {
    assert_eq!(
        files::configured_window_size(true, (1024, 736)),
        (1024, 736)
    );
    assert_eq!(
        files::configured_window_size(true, (0, 0)),
        (files::NORMAL_WINDOW_WIDTH, files::NORMAL_WINDOW_HEIGHT)
    );
}

fn tempdir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pmos-{}-{}-{}",
        prefix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}
