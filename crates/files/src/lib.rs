//! `files` library — pure file-manager state/actions plus native
//! directory, preview, and transfer helpers shared with isolation tests.
//!
//! The GUI uses the documented WASI filesystem surface exclusively.
//! Host import/export helpers remain available to native tests, but no
//! browser-to-process transfer bridge exists in this slice.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

mod desktop_dispatch;
mod host_notification;
mod state;

pub use desktop_dispatch::{
    parse_text_dispatch, resolve_text_dispatch, text_mime_for_path, DesktopDispatch, DispatchError,
    MAX_DESKTOP_ENTRY_BYTES, TEXT_EDITOR_DESKTOP,
};
pub use host_notification::{HostFileNotification, HostNotificationDecoder, NotificationError};
pub use state::{
    load_text_preview, read_directory, ActionOutcome, DialogKind, DirectoryScanStep,
    DirectoryScanner, DoubleActivation, FileAction, FileEntry, FileManagerState, FileSystem,
    PendingDirectoryAction, PointerTarget, StdFileSystem, StepwiseAction, TextPreview, UiKey,
    ViewMode, DIRECTORY_ENTRIES_PER_STEP, DIRECTORY_ENTRY_LIMIT, PREVIEW_LIMIT_BYTES,
};

/// Height of the client-painted draggable titlebar in the Files window.
pub const TITLEBAR_HEIGHT: u32 = 22;

/// Preferred normal-state Files geometry. A larger compositor work-area offer
/// is not an instruction to occupy the whole desktop unless MAXIMIZED is set.
pub const NORMAL_WINDOW_WIDTH: u32 = 640;
pub const NORMAL_WINDOW_HEIGHT: u32 = 420;

/// Select the client buffer size for an XDG configure. Maximized windows use
/// the exact work-area offer; normal windows keep their preferred size while
/// clamping to a smaller output so they cannot hide shell chrome immediately.
pub fn configured_window_size(maximized: bool, offered: (u32, u32)) -> (u32, u32) {
    match offered {
        (width, height) if maximized && width > 0 && height > 0 => (width, height),
        (width, height) if width > 0 && height > 0 => (
            NORMAL_WINDOW_WIDTH.min(width),
            NORMAL_WINDOW_HEIGHT.min(height),
        ),
        _ => (NORMAL_WINDOW_WIDTH, NORMAL_WINDOW_HEIGHT),
    }
}

/// Pure hit-test used before Files routes a press to toolbar/list actions. A
/// titlebar press is consumed by the XDG interactive-move request, so ordinary
/// file actions below the titlebar remain unchanged.
pub fn titlebar_drag_hit(x: i32, y: i32, width: u32) -> bool {
    x >= 0 && x < width as i32 && y >= 0 && y < TITLEBAR_HEIGHT as i32
}

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

/// Copy a host-imported byte buffer into a PMos directory.
///
/// `target_dir` must already exist. `name` is the file name
/// reported by the browser-side drag-drop handler. If `name`
/// already exists in `target_dir` we synthesise a unique
/// `name (n).ext` variant so the copy never silently overwrites
/// an existing file — the same convention every desktop file
/// manager uses for drag-drop import.
///
/// Returns the final absolute path the bytes landed at.
pub fn import_bytes(target_dir: &str, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let dir = Path::new(target_dir);
    if !dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("target {} is not a directory", target_dir),
        ));
    }
    let (final_path, mut file) = create_unique_file(dir, name)?;
    file.write_all(bytes)?;
    Ok(final_path)
}

/// Read a PMos file's bytes for export to the host. The
/// `Blob` construction itself happens in TS — the rust side
/// just hands over a `Vec<u8>`.
pub fn export_bytes(path: &str) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// Rename a PMos directory entry. Uses POSIX rename(2) so an
/// open fd held by another process keeps reading from the same
/// inode after the rename — the v1 spec's "rename-while-open"
/// invariant (T159).
pub fn rename(old_path: &str, new_path: &str) -> io::Result<()> {
    std::fs::rename(old_path, new_path)
}

/// Strip path separators and reduce a host-side name to its
/// final component. Defends against bootstrap-supplied names
/// that include slashes (e.g. WebKit on macOS reports relative
/// paths for folder drops).
pub fn sanitise_filename(name: &str) -> String {
    let last = Path::new(name)
        .file_name()
        .unwrap_or(OsStr::new("untitled"));
    let mut s = last.to_string_lossy().into_owned();
    if s.is_empty() || s == "." || s == ".." {
        s = "untitled".to_string();
    }
    s
}

/// Resolve a unique file path inside `dir` for `name`. If
/// `name` is free we return `dir/name`. Otherwise we walk
/// `dir/name (1)`, `dir/name (2)`, … until we find a free slot.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let safe = sanitise_filename(name);
    for attempt in 0..1000 {
        let candidate = dir.join(collision_filename(&safe, attempt));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(collision_filename(&safe, 1000))
}

/// Atomically create a non-colliding import destination. Every candidate stays
/// within the VFS's 255-byte component ceiling, and an `AlreadyExists` race is
/// retried rather than overwriting or aborting the import.
pub fn create_unique_file(dir: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    if !dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("target {} is not a directory", dir.display()),
        ));
    }
    let safe = sanitise_filename(name);
    create_unique_with(dir, &safe, |path| {
        OpenOptions::new().write(true).create_new(true).open(path)
    })
}

const MAX_FILENAME_BYTES: usize = 255;
const MAX_COLLISION_ATTEMPTS: u32 = 64;

fn create_unique_with<T, F>(dir: &Path, name: &str, mut create: F) -> io::Result<(PathBuf, T)>
where
    F: FnMut(&Path) -> io::Result<T>,
{
    for attempt in 0..MAX_COLLISION_ATTEMPTS {
        let path = dir.join(collision_filename(name, attempt));
        match create(&path) {
            Ok(value) => return Ok((path, value)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no collision-free import filename available",
    ))
}

fn collision_filename(name: &str, attempt: u32) -> String {
    if attempt == 0 {
        return truncate_utf8(name, MAX_FILENAME_BYTES).to_owned();
    }
    let (stem, ext) = split_stem_ext(name);
    let suffix = format!(" ({attempt})");
    let extension = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    let extension_budget = MAX_FILENAME_BYTES.saturating_sub(suffix.len());
    let extension = truncate_utf8(&extension, extension_budget);
    let stem_budget = extension_budget.saturating_sub(extension.len());
    let stem = truncate_utf8(stem, stem_budget);
    format!("{stem}{suffix}{extension}")
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn split_stem_ext(name: &str) -> (&str, &str) {
    if let Some(pos) = name.rfind('.') {
        if pos > 0 {
            return (&name[..pos], &name[pos + 1..]);
        }
    }
    (name, "")
}

/// MIME-based default-app dispatch (T158).
///
/// The file manager calls this with a path/MIME to decide which
/// bundled app to spawn. Returns the absolute path of the
/// `.desktop` entry that should be launched, or `None` if no
/// default is registered for the type.
pub fn default_app_for(name: &str, mime: Option<&str>) -> Option<&'static str> {
    if let Some(m) = mime {
        if m.starts_with("text/") {
            return Some("/usr/share/applications/edit.desktop");
        }
    }
    let lower = name.to_ascii_lowercase();
    for ext in [
        ".txt", ".md", ".rs", ".toml", ".json", ".log", ".conf", ".ini",
    ] {
        if lower.ends_with(ext) {
            return Some("/usr/share/applications/edit.desktop");
        }
    }
    None
}

/// Combined import-and-launch helper used by the drag-drop and
/// Import-menu paths. Copies `bytes` into `target_dir` under
/// the sanitised `name`, then returns both the on-disk path and
/// the `.desktop` entry that the launcher should spawn (`None`
/// when no app handles the type — the file is still imported).
pub fn import_and_dispatch(
    target_dir: &str,
    name: &str,
    mime: Option<&str>,
    bytes: &[u8],
) -> io::Result<(PathBuf, Option<&'static str>)> {
    let path = import_bytes(target_dir, name, bytes)?;
    let display_name = path.file_name().and_then(|s| s.to_str()).unwrap_or(name);
    let desktop = default_app_for(display_name, mime);
    Ok((path, desktop))
}

#[cfg(test)]
mod collision_tests {
    use super::*;

    #[test]
    fn atomic_create_retries_a_concurrent_winner_without_overwrite() {
        let dir =
            std::env::temp_dir().join(format!("pmos-files-create-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let mut calls = 0;
        let (path, mut file) = create_unique_with(&dir, "race.txt", |candidate| {
            calls += 1;
            if calls == 1 {
                std::fs::write(candidate, b"concurrent winner")?;
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "raced"));
            }
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(candidate)
        })
        .unwrap();
        file.write_all(b"import").unwrap();
        drop(file);

        assert_eq!(calls, 2);
        assert_eq!(path.file_name().unwrap(), "race (1).txt");
        assert_eq!(
            std::fs::read(dir.join("race.txt")).unwrap(),
            b"concurrent winner"
        );
        assert_eq!(std::fs::read(path).unwrap(), b"import");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
