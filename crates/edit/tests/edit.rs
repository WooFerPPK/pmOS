//! T160 — edit isolation tests against fixture files.

use std::fs;

#[test]
fn read_existing_file_returns_contents_and_true() {
    let path = std::env::temp_dir().join(format!(
        "pmos-edit-test-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::write(&path, b"hello\nworld\n").unwrap();

    let (contents, ok) = edit::read_file(path.to_str().unwrap());
    assert!(ok);
    assert_eq!(contents, "hello\nworld\n");

    fs::remove_file(&path).unwrap();
}

#[test]
fn read_missing_file_returns_error_string_and_false() {
    let (contents, ok) = edit::read_file("/no/such/file/anywhere");
    assert!(!ok);
    assert!(contents.contains("failed to open"));
}
