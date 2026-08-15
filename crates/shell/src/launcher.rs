//! Launcher content source for the desktop shell.
//!
//! Parses `.desktop`-format files from `/usr/share/applications/`
//! into [`DesktopEntry`] structs and reloads after filesystem-watch events.
//!
//! File I/O is injected through [`DesktopEntryStore`] so production
//! code wires real VFS reads while tests use [`MemoryStore`].
//!
//! The shell paint loop consumes this cache for popup rows and dispatches
//! selection through its capability-aware spawner. Production scanning and
//! parsing are bounded so catalog churn cannot monopolize the shell turn.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, ReadDir};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    /// The backing application directory could not be read.
    StoreIo(String),
}

impl std::fmt::Display for LauncherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LauncherError::MissingSectionHeader => {
                write!(f, "missing [Desktop Entry] section header")
            }
            LauncherError::MissingRequiredKey(k) => write!(f, "missing required key: {k}"),
            LauncherError::NotAnApplication => write!(f, "Type is not Application"),
            LauncherError::StoreIo(message) => write!(f, "application catalog I/O: {message}"),
        }
    }
}

// ── Store trait ─────────────────────────────────────────────────────────────

/// Source of raw `.desktop` file content.
///
/// Each call to [`list_entries`] returns every currently-available
/// file as `(id, content)` pairs, where `id` is the filename stem
/// (no `.desktop` suffix). This synchronous method is retained for
/// injected/native fixtures; production uses [`DesktopEntryStore::begin_scan`].
pub trait DesktopEntryStore {
    fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError>;

    /// Start a bounded catalog scan when this store can provide one.
    /// Injected/native stores retain the synchronous compatibility path;
    /// production [`FilesystemStore`] overrides this method.
    fn begin_scan(&mut self) -> Result<Option<Box<dyn DesktopEntryScan>>, LauncherError> {
        Ok(None)
    }
}

/// At most this many directory entries are examined in one shell turn.
pub const CATALOG_ENTRIES_PER_STEP: usize = 16;
/// A single `.desktop` file is bounded independently of directory size.
pub const MAX_DESKTOP_ENTRY_BYTES: usize = 64 * 1024;
/// Maximum number of application manifests admitted to one catalog snapshot.
pub const MAX_CATALOG_ENTRIES: usize = 256;
/// Maximum aggregate manifest bytes retained while one snapshot is built.
pub const MAX_CATALOG_TOTAL_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub struct DesktopEntryScanBatch {
    pub entries: Vec<(String, String)>,
    pub complete: bool,
}

/// Incremental store scan used by the production event loop.
pub trait DesktopEntryScan {
    fn step(&mut self) -> Result<DesktopEntryScanBatch, LauncherError>;
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

/// Production application catalog backed by a directory in the
/// PMos VFS. Only regular `*.desktop` files are considered; an
/// unreadable individual entry is skipped so one broken package
/// cannot hide the rest of the launcher.
pub struct FilesystemStore {
    directory: PathBuf,
}

impl FilesystemStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

impl DesktopEntryStore for FilesystemStore {
    fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError> {
        let entries = std::fs::read_dir(&self.directory)
            .map_err(|error| LauncherError::StoreIo(error.to_string()))?;
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            found.push((id.to_string(), content));
        }
        Ok(found)
    }

    fn begin_scan(&mut self) -> Result<Option<Box<dyn DesktopEntryScan>>, LauncherError> {
        let entries = std::fs::read_dir(&self.directory)
            .map_err(|error| LauncherError::StoreIo(error.to_string()))?;
        Ok(Some(Box::new(FilesystemCatalogScan {
            entries,
            admitted_entries: 0,
            admitted_bytes: 0,
        })))
    }
}

struct FilesystemCatalogScan {
    entries: ReadDir,
    admitted_entries: usize,
    admitted_bytes: usize,
}

impl DesktopEntryScan for FilesystemCatalogScan {
    fn step(&mut self) -> Result<DesktopEntryScanBatch, LauncherError> {
        let mut found = Vec::new();
        let mut examined = 0usize;
        let mut desktop_file_read = false;

        while examined < CATALOG_ENTRIES_PER_STEP && !desktop_file_read {
            let Some(next) = self.entries.next() else {
                return Ok(DesktopEntryScanBatch {
                    entries: found,
                    complete: true,
                });
            };
            examined += 1;
            let Ok(entry) = next else {
                continue;
            };
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            // One desktop-file read attempt per turn, successful or not.
            // A directory full of oversized or unreadable entries must not
            // bypass the byte-work quantum.
            desktop_file_read = true;
            let Ok(file) = File::open(&path) else {
                continue;
            };
            let mut bytes = Vec::new();
            if file
                .take((MAX_DESKTOP_ENTRY_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .is_err()
                || bytes.len() > MAX_DESKTOP_ENTRY_BYTES
            {
                continue;
            }
            let Ok(content) = String::from_utf8(bytes) else {
                continue;
            };
            let retained_bytes = id.len().saturating_add(content.len());
            if self.admitted_entries >= MAX_CATALOG_ENTRIES
                || self.admitted_bytes.saturating_add(retained_bytes) > MAX_CATALOG_TOTAL_BYTES
            {
                return Err(LauncherError::StoreIo(format!(
                    "application catalog exceeds {} entries or {} bytes",
                    MAX_CATALOG_ENTRIES, MAX_CATALOG_TOTAL_BYTES
                )));
            }
            self.admitted_entries += 1;
            self.admitted_bytes += retained_bytes;
            found.push((id.to_string(), content));
        }

        Ok(DesktopEntryScanBatch {
            entries: found,
            complete: false,
        })
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

pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Avoid a clock syscall on every empty display dispatch. The shell's
/// connection is non-blocking after bootstrap, so sixteen turns remain far
/// below the five-second catalog freshness contract under normal load.
pub const LAUNCHER_CLOCK_CHECK_EVERY_ITERATIONS: u32 = 16;

/// Monotonic clock seam for the live launcher catalog.
pub trait LauncherClock {
    fn elapsed(&mut self) -> Duration;
}

/// Production monotonic clock, measured from shell startup.
pub struct SystemLauncherClock {
    started: Instant,
}

impl SystemLauncherClock {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemLauncherClock {
    fn default() -> Self {
        Self::new()
    }
}

impl LauncherClock for SystemLauncherClock {
    fn elapsed(&mut self) -> Duration {
        self.started.elapsed()
    }
}

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
    scan: Option<Box<dyn DesktopEntryScan>>,
    scan_entries: VecDeque<(String, String)>,
    parsed_entries: Vec<DesktopEntry>,
    parsing: bool,
    rescan_requested: bool,
    completed_reload: Option<LauncherReloadCompletion>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LauncherReloadStep {
    Idle,
    Pending,
    Complete { changed: bool },
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LauncherReloadCompletion {
    Published { changed: bool },
    Failed,
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
            scan: None,
            scan_entries: VecDeque::new(),
            parsed_entries: Vec::new(),
            parsing: false,
            rescan_requested: false,
            completed_reload: None,
        }
    }

    /// Production constructor: publish an empty stable catalog immediately and
    /// populate it through bounded [`Self::step_reload`] calls.
    pub fn new_stepwise(store: Box<dyn DesktopEntryStore>) -> Self {
        let mut launcher = Launcher {
            store,
            entries: Vec::new(),
            last_poll: Duration::ZERO,
            scan: None,
            scan_entries: VecDeque::new(),
            parsed_entries: Vec::new(),
            parsing: false,
            rescan_requested: false,
            completed_reload: None,
        };
        launcher.request_reload();
        launcher
    }

    /// The current cached entry list.
    pub fn entries(&self) -> &[DesktopEntry] {
        &self.entries
    }

    /// Timestamp of the most recent successful poll. Used by
    /// [`launcher_watcher::tick`] to decide whether the next
    /// caller-tick should fire a poll.
    pub fn last_poll(&self) -> Duration {
        self.last_poll
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

    /// Re-read the catalog immediately after a filesystem-watch event.
    /// Returns true only when the visible ordered entries changed.
    pub fn reload(&mut self) -> bool {
        let before = self.entries.clone();
        self.entries = load_entries(self.store.as_mut());
        before != self.entries
    }

    /// Request a catalog refresh. Multiple notifications during an active
    /// scan coalesce into one follow-up scan so no mutation is lost and an
    /// attacker cannot grow an unbounded work queue.
    pub fn request_reload(&mut self) {
        if self.scan.is_some() || self.parsing || self.completed_reload.is_some() {
            self.rescan_requested = true;
            return;
        }
        self.begin_reload();
    }

    /// Advance at most one store quantum. The visible catalog remains stable
    /// until a complete snapshot has been parsed and sorted.
    pub fn step_reload(&mut self) -> LauncherReloadStep {
        if let Some(completion) = self.completed_reload.take() {
            self.finish_or_restart_scan();
            return match completion {
                LauncherReloadCompletion::Published { changed } => {
                    LauncherReloadStep::Complete { changed }
                }
                LauncherReloadCompletion::Failed => LauncherReloadStep::Failed,
            };
        }
        if self.parsing {
            return self.step_parse();
        }
        let Some(mut scan) = self.scan.take() else {
            return LauncherReloadStep::Idle;
        };
        let batch = match scan.step() {
            Ok(batch) => batch,
            Err(error) => {
                eprintln!("launcher: store error: {error}");
                self.scan_entries.clear();
                self.parsed_entries.clear();
                self.finish_or_restart_scan();
                return LauncherReloadStep::Failed;
            }
        };
        self.scan_entries.extend(batch.entries);
        if !batch.complete {
            self.scan = Some(scan);
            return LauncherReloadStep::Pending;
        }

        self.parsing = true;
        LauncherReloadStep::Pending
    }

    pub fn reload_pending(&self) -> bool {
        self.scan.is_some() || self.parsing || self.completed_reload.is_some()
    }

    /// Raw manifests still awaiting their one-per-turn parse quantum.
    pub fn pending_parse_count(&self) -> usize {
        self.scan_entries.len()
    }

    /// Successfully parsed manifests retained for the unpublished snapshot.
    pub fn parsed_entry_count(&self) -> usize {
        self.parsed_entries.len()
    }

    fn begin_reload(&mut self) {
        self.scan_entries.clear();
        self.parsed_entries.clear();
        self.parsing = false;
        match self.store.begin_scan() {
            Ok(Some(scan)) => self.scan = Some(scan),
            Ok(None) => {
                let before = self.entries.clone();
                self.entries = load_entries(self.store.as_mut());
                self.completed_reload = Some(LauncherReloadCompletion::Published {
                    changed: before != self.entries,
                });
            }
            Err(error) => {
                eprintln!("launcher: store error: {error}");
                self.completed_reload = Some(LauncherReloadCompletion::Failed);
            }
        }
    }

    fn finish_or_restart_scan(&mut self) {
        if core::mem::take(&mut self.rescan_requested) {
            self.begin_reload();
        }
    }

    fn step_parse(&mut self) -> LauncherReloadStep {
        if let Some((id, content)) = self.scan_entries.pop_front() {
            match parse_desktop_entry(&id, &content) {
                Ok(entry) => self.parsed_entries.push(entry),
                Err(error) => eprintln!("launcher: dropping {id}.desktop: {error}"),
            }
            if !self.scan_entries.is_empty() {
                return LauncherReloadStep::Pending;
            }
        }

        self.parsing = false;
        let mut replacement = core::mem::take(&mut self.parsed_entries);
        sort_entries(&mut replacement);
        let changed = replacement != self.entries;
        self.entries = replacement;
        self.finish_or_restart_scan();
        LauncherReloadStep::Complete { changed }
    }
}

/// Time-gated launcher cache retained for injected/native desktop fixtures.
///
/// Production calls [`Self::reload`] after filesystem-watch readiness and does
/// not wake every five seconds. [`Launcher`] keeps time injectable through
/// [`Launcher::advance_poll`] so legacy isolation fixtures remain deterministic.
pub struct LauncherRuntime<C> {
    launcher: Launcher,
    clock: C,
    clock_check_every_iterations: u32,
    iterations_until_clock_check: u32,
}

impl<C: LauncherClock> LauncherRuntime<C> {
    pub fn new(launcher: Launcher, clock: C) -> Self {
        Self {
            launcher,
            clock,
            clock_check_every_iterations: LAUNCHER_CLOCK_CHECK_EVERY_ITERATIONS,
            iterations_until_clock_check: 0,
        }
    }

    /// Override the cheap iteration gate for deterministic isolation tests.
    pub fn with_clock_check_every_iterations(mut self, iterations: u32) -> Self {
        self.clock_check_every_iterations = iterations.max(1);
        self.iterations_until_clock_check = 0;
        self
    }

    pub fn entries(&self) -> &[DesktopEntry] {
        self.launcher.entries()
    }

    /// Poll when the five-second boundary is due. Returns `true` only when the
    /// resulting ordered entry list differs from the prior visible catalog.
    pub fn poll(&mut self) -> bool {
        if self.iterations_until_clock_check > 0 {
            self.iterations_until_clock_check -= 1;
            return false;
        }
        self.iterations_until_clock_check = self.clock_check_every_iterations - 1;

        let now = self.clock.elapsed();
        if now < self.launcher.last_poll() + POLL_INTERVAL {
            return false;
        }

        // Clone only on an actual five-second rescan.  The shell loop is
        // intentionally non-blocking, so allocating on every clock check
        // would turn an idle desktop into a steady allocator workload.
        let before = self.launcher.entries().to_vec();
        let polled = self.launcher.advance_poll(now);
        debug_assert!(polled);
        before != self.launcher.entries()
    }

    pub fn reload(&mut self) -> bool {
        self.launcher.reload()
    }

    pub fn request_reload(&mut self) {
        self.launcher.request_reload();
    }

    pub fn step_reload(&mut self) -> LauncherReloadStep {
        self.launcher.step_reload()
    }

    pub fn reload_pending(&self) -> bool {
        self.launcher.reload_pending()
    }
}

fn load_entries(store: &mut dyn DesktopEntryStore) -> Vec<DesktopEntry> {
    match store.list_entries() {
        Err(e) => {
            eprintln!("launcher: store error: {e}");
            Vec::new()
        }
        Ok(raw) => parse_entries(raw),
    }
}

fn parse_entries(raw: Vec<(String, String)>) -> Vec<DesktopEntry> {
    let mut out = Vec::with_capacity(raw.len());
    for (id, content) in raw {
        match parse_desktop_entry(&id, &content) {
            Ok(entry) => out.push(entry),
            Err(e) => eprintln!("launcher: dropping {id}.desktop: {e}"),
        }
    }
    sort_entries(&mut out);
    out
}

fn sort_entries(out: &mut [DesktopEntry]) {
    // Keep the bundled core workflow stable while still admitting
    // package-installed applications. Additional entries follow
    // alphabetically after the core set.
    const CORE_ORDER: &[&str] = &["terminal", "files", "edit", "settings", "sysmon"];
    out.sort_by(|a, b| {
        let a_rank = CORE_ORDER.iter().position(|id| *id == a.id);
        let b_rank = CORE_ORDER.iter().position(|id| *id == b.id);
        match (a_rank, b_rank) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)),
        }
    });
}
