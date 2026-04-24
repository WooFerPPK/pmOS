//! Launcher content source for the desktop shell.
//!
//! Parses `.desktop`-format files from `/usr/share/applications/`
//! into [`DesktopEntry`] structs and re-polls on a 5 s interval.
//!
//! File I/O is injected through [`DesktopEntryStore`] so production
//! code wires real VFS reads while tests use [`MemoryStore`].
//!
//! What is NOT in scope here: `proc_spawn` on selection (follow-up
//! slice), launcher popup UI (T121 follow-up), icon loading, and
//! inotify / file-watcher (v1 is polling per §4 of
//! `package-manifest.md`).

use std::collections::HashMap;
use std::time::Duration;

// ── Data model ──────────────────────────────────────────────────────────────

/// One installed application, parsed from a `.desktop` file.
///
/// `id` is the filename without the `.desktop` suffix so callers
/// can round-trip back to the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopEntry {
    pub id: String,
    pub name: String,
    pub exec: String,
    pub icon: Option<String>,
    pub summary: Option<String>,
    pub mime_types: Vec<String>,
    pub categories: Vec<String>,
    /// Stringly-typed capability names from `X-PMos-Caps=`.
    /// Policy enforcement happens at spawn time, not here.
    pub caps: Vec<String>,
}

// ── Error type ──────────────────────────────────────────────────────────────

/// Reasons a single `.desktop` file may be rejected.
///
/// Errors are not fatal to the whole launcher: one bad file is
/// dropped and the rest are still returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LauncherError {
    /// The `[Desktop Entry]` section header was missing.
    MissingSectionHeader,
    /// A required key (`Name` or `Exec`) was absent.
    MissingRequiredKey(&'static str),
    /// The entry's `Type=` value was not `Application`.
    NotAnApplication,
}

impl std::fmt::Display for LauncherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LauncherError::MissingSectionHeader => {
                write!(f, "missing [Desktop Entry] section header")
            }
            LauncherError::MissingRequiredKey(k) => write!(f, "missing required key: {k}"),
            LauncherError::NotAnApplication => write!(f, "Type is not Application"),
        }
    }
}

// ── Store trait ─────────────────────────────────────────────────────────────

/// Source of raw `.desktop` file content.
///
/// Each call to [`list_entries`] returns every currently-available
/// file as `(id, content)` pairs, where `id` is the filename stem
/// (no `.desktop` suffix).  The launcher calls this at startup and
/// again after each 5 s poll interval.
pub trait DesktopEntryStore {
    fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError>;
}

// ── In-memory store (for tests) ─────────────────────────────────────────────

/// An in-memory [`DesktopEntryStore`] backed by a `HashMap`.
///
/// Keys are the file id (stem without `.desktop`); values are the
/// raw file contents.  Tests insert/remove entries freely between
/// poll cycles to exercise the add/remove paths.
#[derive(Default)]
pub struct MemoryStore {
    pub entries: HashMap<String, String>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DesktopEntryStore for MemoryStore {
    fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError> {
        Ok(self
            .entries
            .iter()
            .map(|(id, content)| (id.clone(), content.clone()))
            .collect())
    }
}

// ── Parser ───────────────────────────────────────────────────────────────────

/// Parse the content of one `.desktop` file.
///
/// Unrecognised keys are silently ignored (forward compatibility per
/// §2.2 of `package-manifest.md`).  Returns `Err` when a required
/// field is absent or `Type` is not `Application`.
pub fn parse_desktop_entry(id: &str, content: &str) -> Result<DesktopEntry, LauncherError> {
    let mut in_section = false;
    let mut kv: HashMap<&str, &str> = HashMap::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[Desktop Entry]" {
            in_section = true;
            continue;
        }
        if line.starts_with('[') {
            in_section = false;
            continue;
        }
        if in_section {
            if let Some((k, v)) = line.split_once('=') {
                kv.insert(k.trim(), v.trim());
            }
        }
    }

    if !in_section && kv.is_empty() {
        return Err(LauncherError::MissingSectionHeader);
    }

    match kv.get("Type") {
        Some(&"Application") => {}
        None => return Err(LauncherError::NotAnApplication),
        _ => return Err(LauncherError::NotAnApplication),
    }

    let name = kv
        .get("Name")
        .ok_or(LauncherError::MissingRequiredKey("Name"))?
        .to_string();

    let exec = kv
        .get("Exec")
        .ok_or(LauncherError::MissingRequiredKey("Exec"))?
        .to_string();

    let icon = kv.get("Icon").map(|v| v.to_string());
    let summary = kv.get("Summary").map(|v| v.to_string());

    let mime_types = kv
        .get("MimeType")
        .map(|v| split_semicolons(v))
        .unwrap_or_default();

    let categories = kv
        .get("Categories")
        .map(|v| split_semicolons(v))
        .unwrap_or_default();

    let caps = kv
        .get("X-PMos-Caps")
        .map(|v| split_semicolons(v))
        .unwrap_or_default();

    Ok(DesktopEntry {
        id: id.to_string(),
        name,
        exec,
        icon,
        summary,
        mime_types,
        categories,
        caps,
    })
}

fn split_semicolons(s: &str) -> Vec<String> {
    s.split(';')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

// ── Launcher ─────────────────────────────────────────────────────────────────

const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Desktop-shell app launcher.
///
/// Owns a [`DesktopEntryStore`] + a cached entry list.  The caller
/// drives the poll clock by passing the current uptime to
/// [`advance_poll`]; the method returns `true` when 5 s have elapsed
/// and a re-read was triggered.  Keeping `now` explicit (rather than
/// calling `SystemTime::now()` internally) makes tests deterministic.
pub struct Launcher {
    store: Box<dyn DesktopEntryStore>,
    entries: Vec<DesktopEntry>,
    last_poll: Duration,
}

impl Launcher {
    /// Construct a launcher and immediately read the store.
    ///
    /// Malformed entries are logged (via `eprintln!`) and dropped;
    /// they do not prevent healthy entries from loading.
    pub fn new(mut store: Box<dyn DesktopEntryStore>) -> Self {
        let entries = load_entries(store.as_mut());
        Launcher {
            store,
            entries,
            last_poll: Duration::ZERO,
        }
    }

    /// The current cached entry list.
    pub fn entries(&self) -> &[DesktopEntry] {
        &self.entries
    }

    /// Advance the poll clock.
    ///
    /// Returns `true` if `now` is at least 5 s ahead of the last
    /// successful poll and a re-read was performed.  The `last_poll`
    /// timestamp is updated only when a poll fires.
    pub fn advance_poll(&mut self, now: Duration) -> bool {
        if now >= self.last_poll + POLL_INTERVAL {
            self.entries = load_entries(self.store.as_mut());
            self.last_poll = now;
            true
        } else {
            false
        }
    }
}

fn load_entries(store: &mut dyn DesktopEntryStore) -> Vec<DesktopEntry> {
    match store.list_entries() {
        Err(e) => {
            eprintln!("launcher: store error: {e}");
            Vec::new()
        }
        Ok(raw) => {
            let mut out = Vec::with_capacity(raw.len());
            for (id, content) in raw {
                match parse_desktop_entry(&id, &content) {
                    Ok(entry) => out.push(entry),
                    Err(e) => eprintln!("launcher: dropping {id}.desktop: {e}"),
                }
            }
            out
        }
    }
}
