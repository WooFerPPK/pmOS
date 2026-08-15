//! Edit isolation tests: bounded VFS I/O and data-loss-sensitive lifecycle.

use std::collections::BTreeMap;
use std::fs;
use std::io;

use edit::{
    read_file, write_file, DocumentError, DocumentJobSuccess, DocumentJobTurn, DocumentStore,
    EditBuffer, EditorEffect, EditorInput, EditorIoTurn, EditorMode, EditorSession,
    EditorStepwiseEffect, PathAction, PendingAction, DOCUMENT_IO_CHUNK_BYTES, MAX_DOCUMENT_BYTES,
};

fn temp_path(suffix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "pmos-edit-test-{}-{}-{suffix}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock must follow the Unix epoch")
            .as_nanos(),
    ))
}

#[derive(Default)]
struct FakeStore {
    files: BTreeMap<String, String>,
    fail_reads: bool,
    fail_writes: bool,
}

impl DocumentStore for FakeStore {
    fn read_document(&mut self, path: &str) -> Result<String, DocumentError> {
        if self.fail_reads {
            return Err(fake_io_error(path, "injected read failure"));
        }
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| DocumentError::Io {
                path: path.to_string(),
                source: io::Error::new(io::ErrorKind::NotFound, "missing fixture"),
            })
    }

    fn write_document(&mut self, path: &str, contents: &str) -> Result<(), DocumentError> {
        if self.fail_writes {
            return Err(fake_io_error(path, "injected write failure"));
        }
        self.files.insert(path.to_string(), contents.to_string());
        Ok(())
    }
}

fn fake_io_error(path: &str, message: &str) -> DocumentError {
    DocumentError::Io {
        path: path.to_string(),
        source: io::Error::other(message),
    }
}

fn type_text(session: &mut EditorSession, store: &mut impl DocumentStore, text: &str) {
    for character in text.chars() {
        let effect = session.handle(EditorInput::Character(character), store);
        assert_eq!(effect, EditorEffect::Continue);
    }
}

#[test]
fn read_existing_file_returns_bounded_utf8() {
    let path = temp_path("read.txt");
    fs::write(&path, b"hello\nworld\n").expect("write fixture");

    let contents = read_file(path.to_str().expect("UTF-8 temp path")).expect("read fixture");
    assert_eq!(contents, "hello\nworld\n");

    fs::remove_file(path).expect("remove fixture");
}

#[test]
fn read_missing_file_exposes_the_real_io_error() {
    let error = read_file("/no/such/file/anywhere").expect_err("missing path must fail");
    assert!(error.is_not_found());
    assert!(error.to_string().contains("No such file"));
}

#[test]
fn read_and_write_reject_oversized_documents() {
    let path = temp_path("large.txt");
    let oversized = vec![b'x'; MAX_DOCUMENT_BYTES + 1];
    fs::write(&path, &oversized).expect("write oversized fixture");

    assert!(matches!(
        read_file(path.to_str().expect("UTF-8 temp path")),
        Err(DocumentError::TooLarge { .. })
    ));
    assert!(matches!(
        write_file(
            path.to_str().expect("UTF-8 temp path"),
            &"y".repeat(MAX_DOCUMENT_BYTES + 1),
        ),
        Err(DocumentError::TooLarge { .. })
    ));
    assert_eq!(fs::read(&path).expect("old file survives"), oversized);

    fs::remove_file(path).expect("remove fixture");
}

#[test]
fn write_file_creates_parents_and_atomically_replaces_contents() {
    let base = temp_path("write");
    let nested = base.join("a").join("b").join("note.txt");
    let path = nested.to_str().expect("UTF-8 temp path");

    write_file(path, "first\n").expect("initial save");
    write_file(path, "second\n").expect("replacement save");
    assert_eq!(read_file(path).expect("read replacement"), "second\n");
    let names: Vec<_> = fs::read_dir(nested.parent().expect("file parent"))
        .expect("list parent")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from("note.txt")]);

    fs::remove_dir_all(base).expect("remove fixture tree");
}

#[test]
fn normal_save_follows_the_open_inode_after_files_renames_its_path() {
    let base = temp_path("rename-open");
    fs::create_dir_all(&base).expect("create fixture directory");
    let old = base.join("before.txt");
    let renamed = base.join("after.txt");
    fs::write(&old, "inode body").expect("write fixture");

    let old_label = old.to_str().expect("UTF-8 temp path");
    let mut store = edit::StdDocumentStore::default();
    let opened = store.open_document(old_label).expect("open retained fd");
    let mut session = EditorSession::from_open_document(old_label, opened)
        .expect("fixture stays under document cap");
    assert!(session.has_open_handle());

    fs::rename(&old, &renamed).expect("Files-style POSIX rename");
    type_text(&mut session, &mut store, "saved ");
    assert_eq!(
        session.handle(EditorInput::Save, &mut store),
        EditorEffect::Continue
    );

    assert!(!old.exists(), "Save must not recreate the stale pathname");
    assert_eq!(
        fs::read_to_string(&renamed).expect("read renamed inode"),
        "saved inode body"
    );
    assert_eq!(session.path(), Some(old_label));
    assert!(!session.buffer().dirty());

    fs::remove_dir_all(base).expect("remove fixture tree");
}

#[test]
fn edit_buffer_preserves_unicode_cursor_and_byte_accounting() {
    let mut buffer = EditBuffer::try_from_text("aé\ncd").expect("small document");
    assert_eq!(buffer.byte_len(), 6);
    buffer.move_right();
    buffer.move_right();
    buffer.backspace();
    assert_eq!(buffer.document_text(), "a\ncd");
    assert_eq!(buffer.byte_len(), 4);
    assert!(buffer.dirty());
}

#[test]
fn edit_buffer_refuses_input_past_the_document_cap_without_mutation() {
    let full = "x".repeat(MAX_DOCUMENT_BYTES);
    let mut buffer = EditBuffer::try_from_text(&full).expect("document at exact cap");
    assert!(!buffer.insert_char('y'));
    assert!(!buffer.insert_newline());
    assert_eq!(buffer.byte_len(), MAX_DOCUMENT_BYTES);
    assert_eq!(buffer.document_text(), full);
    assert!(!buffer.dirty());
}

#[test]
fn new_document_save_opens_save_as_and_binds_only_after_success() {
    let mut store = FakeStore::default();
    let mut session = EditorSession::new();
    type_text(&mut session, &mut store, "daily note");

    assert_eq!(
        session.handle(EditorInput::Save, &mut store),
        EditorEffect::Continue
    );
    assert!(matches!(
        session.mode(),
        EditorMode::Path {
            action: PathAction::SaveAs,
            after_save: None,
            ..
        }
    ));
    assert_eq!(session.path(), None);

    type_text(&mut session, &mut store, "daily.txt");
    assert_eq!(
        session.handle(EditorInput::Enter, &mut store),
        EditorEffect::Continue
    );
    let path = "/home/user/Documents/daily.txt";
    assert_eq!(session.path(), Some(path));
    assert_eq!(
        store.files.get(path).map(String::as_str),
        Some("daily note")
    );
    assert!(!session.buffer().dirty());
    assert_eq!(
        session.status(),
        "saved /home/user/Documents/daily.txt bytes=10"
    );
}

#[test]
fn clean_open_replaces_the_document_and_binding() {
    let original = "/home/user/Documents/original.txt";
    let target = "/home/user/Documents/target.txt";
    let mut store = FakeStore::default();
    store
        .files
        .insert(target.to_string(), "target contents".to_string());
    let mut session =
        EditorSession::from_document(original, "original contents").expect("small document");

    session.handle(EditorInput::Open, &mut store);
    type_text(&mut session, &mut store, "target.txt");
    assert_eq!(
        session.handle(EditorInput::Enter, &mut store),
        EditorEffect::Continue
    );

    assert_eq!(session.path(), Some(target));
    assert_eq!(session.buffer().document_text(), "target contents");
    assert!(!session.buffer().dirty());
    assert_eq!(
        session.status(),
        "opened /home/user/Documents/target.txt bytes=15"
    );
}

#[test]
fn save_as_rebinds_without_overwriting_the_original() {
    let original = "/home/user/Documents/original.txt";
    let copy = "/home/user/Documents/copy.txt";
    let mut store = FakeStore::default();
    store
        .files
        .insert(original.to_string(), "original".to_string());
    let mut session = EditorSession::from_document(original, "original").expect("small document");
    type_text(&mut session, &mut store, "copy: ");

    session.handle(EditorInput::SaveAs, &mut store);
    type_text(&mut session, &mut store, "copy.txt");
    assert_eq!(
        session.handle(EditorInput::Enter, &mut store),
        EditorEffect::Continue
    );

    assert_eq!(session.path(), Some(copy));
    assert_eq!(
        store.files.get(original).map(String::as_str),
        Some("original")
    );
    assert_eq!(
        store.files.get(copy).map(String::as_str),
        Some("copy: original")
    );
    assert!(!session.buffer().dirty());
}

#[test]
fn failed_save_as_keeps_the_original_binding_and_dirty_buffer() {
    let original = "/home/user/Documents/original.txt";
    let mut store = FakeStore {
        fail_writes: true,
        ..FakeStore::default()
    };
    let mut session = EditorSession::from_document(original, "original").expect("small document");
    type_text(&mut session, &mut store, "draft ");

    session.handle(EditorInput::SaveAs, &mut store);
    type_text(&mut session, &mut store, "copy.txt");
    session.handle(EditorInput::Enter, &mut store);

    assert_eq!(session.path(), Some(original));
    assert_eq!(session.buffer().document_text(), "draft original");
    assert!(session.buffer().dirty());
    assert!(session.status().contains("injected write failure"));
    assert!(matches!(
        session.mode(),
        EditorMode::Path {
            action: PathAction::SaveAs,
            ..
        }
    ));
}

#[test]
fn dirty_new_and_open_require_an_explicit_decision() {
    let original = "/home/user/Documents/original.txt";
    let target = "/home/user/Documents/target.txt";
    let mut store = FakeStore::default();
    store
        .files
        .insert(original.to_string(), "on disk".to_string());
    store.files.insert(target.to_string(), "target".to_string());
    let mut session = EditorSession::from_document(original, "on disk").expect("small document");
    type_text(&mut session, &mut store, "draft ");

    session.handle(EditorInput::New, &mut store);
    assert_eq!(
        session.mode(),
        &EditorMode::ConfirmDiscard(PendingAction::New)
    );
    session.handle(EditorInput::Escape, &mut store);
    assert_eq!(session.path(), Some(original));
    assert_eq!(session.buffer().document_text(), "draft on disk");

    session.handle(EditorInput::Open, &mut store);
    assert_eq!(
        session.mode(),
        &EditorMode::ConfirmDiscard(PendingAction::Open)
    );
    session.handle(EditorInput::Character('d'), &mut store);
    assert!(matches!(
        session.mode(),
        EditorMode::Path {
            action: PathAction::Open,
            ..
        }
    ));
    type_text(&mut session, &mut store, "target.txt");
    session.handle(EditorInput::Enter, &mut store);

    assert_eq!(session.path(), Some(target));
    assert_eq!(session.buffer().document_text(), "target");
    assert_eq!(
        store.files.get(original).map(String::as_str),
        Some("on disk")
    );
}

#[test]
fn failed_save_keeps_the_original_file_and_dirty_buffer() {
    let path = "/home/user/Documents/safe.txt";
    let mut store = FakeStore::default();
    store.files.insert(path.to_string(), "old".to_string());
    let mut session = EditorSession::from_document(path, "old").expect("small document");
    type_text(&mut session, &mut store, "new");
    store.fail_writes = true;

    assert_eq!(
        session.handle(EditorInput::Save, &mut store),
        EditorEffect::Continue
    );
    assert!(session.buffer().dirty());
    assert_eq!(store.files.get(path).map(String::as_str), Some("old"));
    assert!(session.status().contains("injected write failure"));
}

#[test]
fn dirty_close_can_cancel_then_discard_without_writing() {
    let path = "/home/user/Documents/close.txt";
    let mut store = FakeStore::default();
    store.files.insert(path.to_string(), "disk".to_string());
    let mut session = EditorSession::from_document(path, "disk").expect("small document");
    type_text(&mut session, &mut store, "draft");

    assert_eq!(
        session.handle(EditorInput::RequestClose, &mut store),
        EditorEffect::Continue
    );
    assert_eq!(
        session.mode(),
        &EditorMode::ConfirmDiscard(PendingAction::Close)
    );
    assert_eq!(
        session.handle(EditorInput::Character('c'), &mut store),
        EditorEffect::Continue
    );
    assert_eq!(session.status(), "close cancelled");
    assert!(session.buffer().dirty());

    session.handle(EditorInput::RequestClose, &mut store);
    assert_eq!(
        session.handle(EditorInput::Character('d'), &mut store),
        EditorEffect::Close
    );
    assert_eq!(store.files.get(path).map(String::as_str), Some("disk"));
}

#[test]
fn dirty_close_save_writes_then_closes() {
    let path = "/home/user/Documents/save-close.txt";
    let mut store = FakeStore::default();
    store.files.insert(path.to_string(), "a".to_string());
    let mut session = EditorSession::from_document(path, "a").expect("small document");
    type_text(&mut session, &mut store, "b");

    session.handle(EditorInput::RequestClose, &mut store);
    assert_eq!(
        session.handle(EditorInput::Character('s'), &mut store),
        EditorEffect::Close
    );
    assert_eq!(store.files.get(path).map(String::as_str), Some("ba"));
    assert!(!session.buffer().dirty());
}

#[test]
fn dirty_close_failed_save_keeps_the_editor_open_for_another_choice() {
    let path = "/home/user/Documents/save-close.txt";
    let mut store = FakeStore {
        fail_writes: true,
        ..FakeStore::default()
    };
    store.files.insert(path.to_string(), "disk".to_string());
    let mut session = EditorSession::from_document(path, "disk").expect("small document");
    type_text(&mut session, &mut store, "draft ");

    session.handle(EditorInput::RequestClose, &mut store);
    assert_eq!(
        session.handle(EditorInput::Character('s'), &mut store),
        EditorEffect::Continue
    );
    assert_eq!(
        session.mode(),
        &EditorMode::ConfirmDiscard(PendingAction::Close)
    );
    assert!(session.buffer().dirty());
    assert!(session.status().contains("injected write failure"));
    assert_eq!(store.files.get(path).map(String::as_str), Some("disk"));

    assert_eq!(
        session.handle(EditorInput::Escape, &mut store),
        EditorEffect::Continue
    );
    assert_eq!(session.status(), "close cancelled");
}

#[test]
fn unbound_dirty_close_save_as_closes_only_after_success() {
    let mut store = FakeStore::default();
    let mut session = EditorSession::new();
    type_text(&mut session, &mut store, "keep me");
    session.handle(EditorInput::RequestClose, &mut store);
    assert_eq!(
        session.handle(EditorInput::Character('s'), &mut store),
        EditorEffect::Continue
    );
    assert!(matches!(
        session.mode(),
        EditorMode::Path {
            action: PathAction::SaveAs,
            after_save: Some(PendingAction::Close),
            ..
        }
    ));

    type_text(&mut session, &mut store, "kept.txt");
    assert_eq!(
        session.handle(EditorInput::Enter, &mut store),
        EditorEffect::Close
    );
    assert_eq!(
        store
            .files
            .get("/home/user/Documents/kept.txt")
            .map(String::as_str),
        Some("keep me")
    );
}

#[test]
fn open_failure_is_visible_and_does_not_replace_the_current_document() {
    let path = "/home/user/Documents/original.txt";
    let mut store = FakeStore {
        fail_reads: true,
        ..FakeStore::default()
    };
    let mut session = EditorSession::from_document(path, "original").expect("small document");

    session.handle(EditorInput::Open, &mut store);
    type_text(&mut session, &mut store, "missing.txt");
    session.handle(EditorInput::Enter, &mut store);

    assert_eq!(session.path(), Some(path));
    assert_eq!(session.buffer().document_text(), "original");
    assert!(session.status().contains("injected read failure"));
    assert!(matches!(
        session.mode(),
        EditorMode::Path {
            action: PathAction::Open,
            ..
        }
    ));
}

#[test]
fn cli_save_subcommand_writes_file() {
    let path = temp_path("cli-save.txt");
    let path = path.to_str().expect("UTF-8 temp path");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_edit"))
        .args(["save", path, "hello from cli\n"])
        .output()
        .expect("spawn edit save");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        read_file(path).expect("read CLI output"),
        "hello from cli\n"
    );

    fs::remove_file(path).expect("remove CLI output");
}

#[test]
fn stepwise_open_returns_to_display_dispatch_between_bounded_reads() {
    let path = temp_path("stepwise-open.txt");
    let body = "x".repeat(DOCUMENT_IO_CHUNK_BYTES * 3 + 73);
    fs::write(&path, &body).expect("write large fixture");
    let path_text = path.to_str().expect("UTF-8 temp path");
    let mut store = edit::StdDocumentStore::default();
    let mut job = store.start_open(path_text).expect("plan open without I/O");
    let mut display_turns = 0_usize;
    let opened = loop {
        // This counter stands in for the production loop's display-first
        // dispatch/paint prefix. No job quantum may bypass a fresh turn.
        display_turns += 1;
        match job.step(&mut store) {
            DocumentJobTurn::Progress => {}
            DocumentJobTurn::Blocked(wait) => panic!("native regular file blocked: {wait:?}"),
            DocumentJobTurn::Complete(DocumentJobSuccess::Opened(opened)) => break opened,
            DocumentJobTurn::Complete(other) => panic!("unexpected completion: {other:?}"),
            DocumentJobTurn::Failed(error) => panic!("stepwise open failed: {error}"),
        }
    };
    assert_eq!(opened.contents, body);
    assert!(
        display_turns >= 3 + 3,
        "open/read/EOF/rewind must occupy distinct display turns"
    );
    fs::remove_file(path).expect("remove fixture");
}

#[test]
fn stepwise_save_defers_close_until_sync_and_then_closes_cleanly() {
    let path = temp_path("stepwise-save-close.txt");
    let original = "a".repeat(DOCUMENT_IO_CHUNK_BYTES * 2 + 11);
    fs::write(&path, &original).expect("write fixture");
    let path_text = path.to_str().expect("UTF-8 temp path");
    let mut store = edit::StdDocumentStore::default();
    let opened = store.open_document(path_text).expect("open retained fd");
    let mut session = EditorSession::from_open_document(path_text, opened).expect("small fixture");
    type_text(&mut session, &mut store, "z");

    let mut active = match session.handle_stepwise(EditorInput::Save, &mut store) {
        EditorStepwiseEffect::Started(job) => Some(*job),
        other => panic!("save did not start: {other:?}"),
    };
    assert_eq!(
        session.handle_during_io(EditorInput::RequestClose, &mut active, &mut store),
        EditorEffect::Continue
    );
    assert!(session.buffer().dirty(), "sync has not completed yet");

    let mut display_turns = 0_usize;
    let effect = loop {
        display_turns += 1;
        match active
            .as_mut()
            .expect("save remains active")
            .step(&mut session, &mut store)
        {
            EditorIoTurn::Progress => assert!(session.buffer().dirty()),
            EditorIoTurn::Blocked(wait) => panic!("native regular file blocked: {wait:?}"),
            EditorIoTurn::Complete(effect) => break effect,
        }
    };
    assert_eq!(effect, EditorEffect::Close);
    assert!(!session.buffer().dirty());
    assert_eq!(
        fs::read_to_string(&path).expect("read saved file"),
        format!("z{original}")
    );
    assert!(
        display_turns >= 7,
        "seek/write/truncate/sync/rewind are separate turns"
    );
    fs::remove_file(path).expect("remove fixture");
}

#[test]
fn editing_during_stepwise_save_keeps_newer_revision_dirty() {
    let path = temp_path("stepwise-save-revision.txt");
    let original = "b".repeat(DOCUMENT_IO_CHUNK_BYTES + 9);
    fs::write(&path, &original).expect("write fixture");
    let path_text = path.to_str().expect("UTF-8 temp path");
    let mut store = edit::StdDocumentStore::default();
    let opened = store.open_document(path_text).expect("open retained fd");
    let mut session = EditorSession::from_open_document(path_text, opened).expect("small fixture");
    type_text(&mut session, &mut store, "x");
    let saved_snapshot = session.buffer().document_text();
    let mut active = match session.handle_stepwise(EditorInput::Save, &mut store) {
        EditorStepwiseEffect::Started(job) => Some(*job),
        other => panic!("save did not start: {other:?}"),
    };
    session.handle_during_io(EditorInput::Character('y'), &mut active, &mut store);

    loop {
        match active
            .as_mut()
            .expect("save remains active")
            .step(&mut session, &mut store)
        {
            EditorIoTurn::Progress => {}
            EditorIoTurn::Blocked(wait) => panic!("native regular file blocked: {wait:?}"),
            EditorIoTurn::Complete(effect) => {
                assert_eq!(effect, EditorEffect::Continue);
                break;
            }
        }
    }
    assert_eq!(
        fs::read_to_string(&path).expect("read saved snapshot"),
        saved_snapshot
    );
    assert!(session.buffer().dirty());
    assert!(session.status().contains("newer changes pending"));
    fs::remove_file(path).expect("remove fixture");
}

#[test]
fn stepwise_save_as_rebinds_only_after_synced_atomic_rename() {
    let base = temp_path("stepwise-save-as");
    fs::create_dir_all(&base).expect("create fixture directory");
    let original_path = base.join("original.txt");
    let copy_path = base.join("copy.txt");
    fs::write(&original_path, "original").expect("write original");
    let original_text = original_path.to_str().expect("UTF-8 original path");
    let mut store = edit::StdDocumentStore::default();
    let opened = store
        .open_document(original_text)
        .expect("open retained original");
    let mut session = EditorSession::from_open_document(original_text, opened).expect("small file");
    type_text(&mut session, &mut store, "copy: ");
    session.handle(EditorInput::SaveAs, &mut store);
    type_text(&mut session, &mut store, "copy.txt");
    let mut active = match session.handle_stepwise(EditorInput::Enter, &mut store) {
        EditorStepwiseEffect::Started(job) => *job,
        other => panic!("Save As did not start: {other:?}"),
    };
    assert_eq!(session.path(), Some(original_text));
    assert!(session.buffer().dirty());

    loop {
        match active.step(&mut session, &mut store) {
            EditorIoTurn::Progress => {
                assert_eq!(session.path(), Some(original_text));
                assert!(session.buffer().dirty());
            }
            EditorIoTurn::Blocked(wait) => panic!("native regular file blocked: {wait:?}"),
            EditorIoTurn::Complete(effect) => {
                assert_eq!(effect, EditorEffect::Continue);
                break;
            }
        }
    }
    assert_eq!(fs::read_to_string(&original_path).unwrap(), "original");
    assert_eq!(fs::read_to_string(&copy_path).unwrap(), "copy: original");
    assert_eq!(session.path(), copy_path.to_str());
    assert!(!session.buffer().dirty());
    assert!(
        fs::read_dir(&base)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().contains("pmos-save")),
        "atomic Save As left a temporary sibling"
    );
    fs::remove_dir_all(base).expect("remove fixture tree");
}

#[test]
fn failed_stepwise_atomic_replace_preserves_target_and_cleans_temporary() {
    let base = temp_path("stepwise-atomic-failure");
    let destination = base.join("occupied");
    fs::create_dir_all(&destination).expect("create non-replaceable target directory");
    fs::write(destination.join("marker"), "old target").expect("write target marker");
    let mut store = edit::StdDocumentStore::default();
    let mut job = store
        .start_atomic_save(
            destination.to_str().expect("UTF-8 destination"),
            "new document".to_string(),
        )
        .expect("plan atomic save");

    let error = loop {
        match job.step(&mut store) {
            DocumentJobTurn::Progress => {}
            DocumentJobTurn::Blocked(wait) => panic!("native regular file blocked: {wait:?}"),
            DocumentJobTurn::Complete(success) => {
                panic!("replace unexpectedly succeeded: {success:?}")
            }
            DocumentJobTurn::Failed(error) => break error,
        }
    };
    assert!(error.to_string().contains("occupied"));
    assert_eq!(
        fs::read_to_string(destination.join("marker")).unwrap(),
        "old target"
    );
    assert!(
        fs::read_dir(&base)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().contains("pmos-save")),
        "failed atomic save left a temporary sibling"
    );
    fs::remove_dir_all(base).expect("remove fixture tree");
}
