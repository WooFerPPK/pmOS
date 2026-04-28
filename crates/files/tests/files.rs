//! T160 — files isolation tests against a fixture directory.
//! No display server, no toolkit — just exercise the directory
//! reader the GUI binds to its window.

use std::fs;

#[test]
fn list_dir_returns_dirs_first_then_files_alphabetically() {
    let tmp = tempdir("files-list");
    fs::create_dir(tmp.join("sub")).unwrap();
    fs::write(tmp.join("zfile.txt"), b"z").unwrap();
    fs::write(tmp.join("afile.txt"), b"a").unwrap();
    fs::create_dir(tmp.join("alpha")).unwrap();

    let (entries, dirs, file_count) = files::list_dir(tmp.to_str().unwrap());
    assert_eq!(dirs, 2);
    assert_eq!(file_count, 2);
    // Dirs first (alphabetical), then files (alphabetical).
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["alpha", "sub", "afile.txt", "zfile.txt"]);

    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn list_missing_dir_returns_empty() {
    let (entries, dirs, files) =
        files::list_dir("/this/path/should/not/exist/at/all");
    assert!(entries.is_empty());
    assert_eq!(dirs, 0);
    assert_eq!(files, 0);
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
