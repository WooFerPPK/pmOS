//! `edit` library — file-read helpers shared between the GUI
//! binary and the isolation tests (T160).

/// Read a file as a UTF-8 string. Returns the contents (or a
/// placeholder error string) plus an `ok` flag.
pub fn read_file(path: &str) -> (String, bool) {
    match std::fs::read_to_string(path) {
        Ok(s) => (s, true),
        Err(e) => (format!("(failed to open {path}: {e})"), false),
    }
}
