//! `files` library — shared helpers between the GUI binary and
//! the isolation tests (T160).

/// Read directory entries (name + is_dir). Returns the name list,
/// dir count, and file count. Dirs sorted alphabetically first,
/// then files sorted alphabetically.
pub fn list_dir(path: &str) -> (Vec<(String, bool)>, usize, usize) {
    let mut entries = Vec::new();
    let mut dirs = 0;
    let mut files = 0;
    if let Ok(rd) = std::fs::read_dir(path) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                dirs += 1;
            } else {
                files += 1;
            }
            entries.push((name, is_dir));
        }
    }
    entries.sort_by(|a, b| match (a.1, b.1) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.cmp(&b.0),
    });
    (entries, dirs, files)
}
