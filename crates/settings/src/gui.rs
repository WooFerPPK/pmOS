//! Tabbed graphical Settings UI (T184 + T185 + T187 + T188 +
//! T190 + T191 + T192).
//!
//! Six tabs across the top of the window — Wallpaper, Appearance
//! (theme + fit), Keyboard, Timezone, Terminal, About — and a
//! single content pane below. Pointer clicks switch tabs; the
//! current selection highlights its tab strip. Each pane reads
//! the live `/etc/preferences.toml` snapshot and shows the
//! current value for its section. Apply buttons cycle through
//! a small bundled allow-list (themes light/dark; fit modes
//! stretch/tile/center/fill; keyboard layouts us-qwerty/uk-qwerty/
//! dvorak; timezones America/New_York / Europe/London / Asia/Tokyo
//! / UTC; terminal fonts unifont-mono-14.pbm / pc-vga-16.pbm) and
//! write back via the same TOML serialiser the CLI subcommands
//! use, so the round-trip semantics stay identical.

#[cfg(target_arch = "wasm32")]
use display_proto::events::{key_state, KeyboardKey, PointerButton};
#[cfg(target_arch = "wasm32")]
use display_proto::Interface;
use preferences::Preferences;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use toolkit::draw::font::GLYPH_HEIGHT;
use toolkit::draw::{Canvas, Color, Rect};
#[cfg(target_arch = "wasm32")]
use toolkit::{App, BufferPool, ClientError, Connection, WaitFd, Window};

/// Path that the GUI reads/writes. Resolved once on entry; an
/// override exists primarily for tests.
pub const DEFAULT_CONFIG: &str = preferences::DEFAULT_PATH;

const PROC_VERSION_PATH: &str = "/proc/version";
const PROC_STORAGE_PATH: &str = "/proc/storage";
const LICENSE_PATH: &str = "/usr/share/doc/pmos/LICENSE.txt";
const CREDITS_PATH: &str = "/usr/share/doc/pmos/CREDITS.txt";
const PROC_READ_LIMIT: usize = 1_024;
const DOC_READ_LIMIT: usize = 4_096;
const ABOUT_LINE_CHARS: usize = 65;
const PREFERENCE_WRITE_CHUNK_BYTES: usize = 16 * 1024;
const TEMPORARY_NAME_ATTEMPTS: u32 = 32;

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
struct BoundedRead {
    bytes: Vec<u8>,
    total_bytes: u64,
    truncated: bool,
}

trait AboutSource {
    fn read_bounded(&self, path: &str, max_bytes: usize) -> Result<BoundedRead, String>;
}

struct FsAboutSource;

impl AboutSource for FsAboutSource {
    fn read_bounded(&self, path: &str, max_bytes: usize) -> Result<BoundedRead, String> {
        let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        let metadata_bytes = file.metadata().ok().map(|metadata| metadata.len());
        let read_limit = u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::with_capacity(max_bytes.min(4_096).saturating_add(1));
        file.take(read_limit)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;

        let observed_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let total_bytes = metadata_bytes.unwrap_or(observed_bytes).max(observed_bytes);
        let truncated = bytes.len() > max_bytes || total_bytes > max_bytes as u64;
        bytes.truncate(max_bytes);
        Ok(BoundedRead {
            bytes,
            total_bytes,
            truncated,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StorageInfo {
    quota_bytes: u64,
    used_bytes: u64,
    file_count: u64,
}

impl StorageInfo {
    fn summary(&self) -> String {
        if self.quota_bytes == 0 {
            format!("volatile root, {} files", self.file_count)
        } else {
            format!(
                "{} of {} used, {} files",
                crate::format_bytes_human(self.used_bytes),
                crate::format_bytes_human(self.quota_bytes),
                self.file_count,
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DocumentInfo {
    title: String,
    total_bytes: u64,
    truncated: bool,
}

impl DocumentInfo {
    fn summary(&self) -> String {
        let suffix = if self.truncated {
            ", bounded preview"
        } else {
            ""
        };
        format!(
            "{} ({}{})",
            self.title,
            crate::format_bytes_human(self.total_bytes),
            suffix,
        )
    }
}

#[derive(Debug, Clone)]
struct LiveField<T> {
    value: Option<T>,
    error: Option<String>,
}

impl<T> Default for LiveField<T> {
    fn default() -> Self {
        Self {
            value: None,
            error: None,
        }
    }
}

impl<T> LiveField<T> {
    fn update(&mut self, result: Result<T, String>) {
        match result {
            Ok(value) => {
                self.value = Some(value);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct AboutPane {
    version: LiveField<String>,
    storage: LiveField<StorageInfo>,
    license: LiveField<DocumentInfo>,
    credits: LiveField<DocumentInfo>,
    status: String,
}

impl AboutPane {
    fn refresh<S: AboutSource + ?Sized>(&mut self, source: &S) {
        self.version
            .update(load_version(source).map_err(|error| format!("{PROC_VERSION_PATH}: {error}")));
        self.storage
            .update(load_storage(source).map_err(|error| format!("{PROC_STORAGE_PATH}: {error}")));
        self.license.update(
            load_document(source, LICENSE_PATH).map_err(|error| format!("{LICENSE_PATH}: {error}")),
        );
        self.credits.update(
            load_document(source, CREDITS_PATH).map_err(|error| format!("{CREDITS_PATH}: {error}")),
        );

        let failed = [
            ("version", self.version.error.is_some()),
            ("storage", self.storage.error.is_some()),
            ("license", self.license.error.is_some()),
            ("credits", self.credits.error.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, failed)| failed.then_some(name))
        .collect::<Vec<_>>();
        self.status = if failed.is_empty() {
            "Live system details refreshed".to_string()
        } else {
            format!("Refresh error: {} unavailable", failed.join(", "))
        };
    }

    fn has_errors(&self) -> bool {
        self.version.error.is_some()
            || self.storage.error.is_some()
            || self.license.error.is_some()
            || self.credits.error.is_some()
    }

    fn lines(&self) -> Vec<String> {
        vec![
            "PMos system".to_string(),
            field_line("Version", &self.version, Clone::clone),
            format!(
                "Kernel ABI: {}.{}",
                abi::version::ABI_MAJOR,
                abi::version::ABI_MINOR,
            ),
            field_line("Storage", &self.storage, StorageInfo::summary),
            field_line("License", &self.license, DocumentInfo::summary),
            field_line("Credits", &self.credits, DocumentInfo::summary),
            "Live from PMos VFS. Press Enter or click Refresh.".to_string(),
        ]
        .into_iter()
        .map(|line| elide(&line, ABOUT_LINE_CHARS))
        .collect()
    }
}

fn field_line<T>(label: &str, field: &LiveField<T>, render: impl FnOnce(&T) -> String) -> String {
    match (&field.value, &field.error) {
        (Some(value), Some(_)) => format!("{label} (stale): {}", render(value)),
        (Some(value), None) => format!("{label}: {}", render(value)),
        (None, Some(error)) => format!("{label}: unavailable ({})", compact_error(error)),
        (None, None) => format!("{label}: not loaded"),
    }
}

fn compact_error(error: &str) -> String {
    let compact = error.split_whitespace().collect::<Vec<_>>().join(" ");
    elide(&compact, 38)
}

fn elide(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let mut out = text.chars().take(max_chars - 3).collect::<String>();
    out.push_str("...");
    out
}

fn load_version<S: AboutSource + ?Sized>(source: &S) -> Result<String, String> {
    let read = source.read_bounded(PROC_VERSION_PATH, PROC_READ_LIMIT)?;
    if read.truncated {
        return Err(format!("exceeds {PROC_READ_LIMIT}-byte limit"));
    }
    first_nonempty_line(&read.bytes)
}

fn load_storage<S: AboutSource + ?Sized>(source: &S) -> Result<StorageInfo, String> {
    let read = source.read_bounded(PROC_STORAGE_PATH, PROC_READ_LIMIT)?;
    if read.truncated {
        return Err(format!("exceeds {PROC_READ_LIMIT}-byte limit"));
    }
    let text = std::str::from_utf8(&read.bytes).map_err(|_| "is not UTF-8".to_string())?;
    let mut fields = text.split_ascii_whitespace();
    let quota_bytes = parse_storage_field(fields.next(), "quota")?;
    let used_bytes = parse_storage_field(fields.next(), "used")?;
    let file_count = parse_storage_field(fields.next(), "files")?;
    if fields.next().is_some() {
        return Err("expected exactly three decimal fields".to_string());
    }
    Ok(StorageInfo {
        quota_bytes,
        used_bytes,
        file_count,
    })
}

fn parse_storage_field(field: Option<&str>, name: &str) -> Result<u64, String> {
    field
        .ok_or_else(|| format!("missing {name} field"))?
        .parse::<u64>()
        .map_err(|_| format!("invalid {name} field"))
}

fn load_document<S: AboutSource + ?Sized>(source: &S, path: &str) -> Result<DocumentInfo, String> {
    let read = source.read_bounded(path, DOC_READ_LIMIT)?;
    let title = first_nonempty_line(&read.bytes)?;
    Ok(DocumentInfo {
        title,
        total_bytes: read.total_bytes,
        truncated: read.truncated,
    })
}

fn first_nonempty_line(bytes: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "is not UTF-8".to_string())?;
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|line| !line.is_empty())
        .ok_or_else(|| "is empty".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Wallpaper,
    Appearance,
    Keyboard,
    Timezone,
    Terminal,
    About,
}

impl Tab {
    const ALL: [Tab; 6] = [
        Tab::Wallpaper,
        Tab::Appearance,
        Tab::Keyboard,
        Tab::Timezone,
        Tab::Terminal,
        Tab::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Tab::Wallpaper => "Wallpaper",
            Tab::Appearance => "Appearance",
            Tab::Keyboard => "Keyboard",
            Tab::Timezone => "Timezone",
            Tab::Terminal => "Terminal",
            Tab::About => "About",
        }
    }
}

/// Bundled allow-lists. Kept module-private — the CLI-side
/// subcommands accept any string for forward-compat, but the GUI
/// cycles through a fixed list because the visual surface needs
/// a finite menu.
const THEMES: &[&str] = &["light", "dark"];
const FITS: &[&str] = &["stretch", "tile", "center", "fill"];
const WALLPAPERS: &[&str] = &["blue.png", "green.png", "dark.png"];
const LAYOUTS: &[&str] = preferences::KEYBOARD_LAYOUT_NAMES;
const TIMEZONES: &[&str] = &["UTC", "America/New_York", "Europe/London", "Asia/Tokyo"];
const FONTS: &[&str] = preferences::TERMINAL_FONT_NAMES;

/// Width / height the GUI requests when the server defers
/// (`configure(0, 0)`).
pub const DEFAULT_WIDTH: u32 = 560;
pub const DEFAULT_HEIGHT: u32 = 360;

/// Run the settings GUI against `connection`. Reads + writes
/// `config_path` directly. Returns when the window closes.
#[cfg(target_arch = "wasm32")]
pub fn run<C: Connection>(connection: C, config_path: &str) -> Result<(), ClientError> {
    let mut app = App::connect_with_shell(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Settings")?;
    window.set_app_id("pmos.settings")?;
    window.commit()?;

    let source = FsAboutSource;
    let mut state = State::new(config_path);
    let mut needs_paint = true;
    let mut configured_once = false;
    let mut pool: Option<BufferPool> = None;
    let mut active_save: Option<ActivePreferenceSave> = None;
    let mut close_pending = false;

    loop {
        let events = window.dispatch()?;
        close_pending |= window.close_requested();

        for event in events {
            match (event.interface, event.opcode) {
                (Interface::Pointer, 2 /* button */) => {
                    if let Ok(btn) = PointerButton::decode(&event.payload) {
                        let save_in_progress = active_save.is_some() || state.save_requested();
                        if btn.state == 1
                            && state.handle_pointer_press(btn.x, btn.y, &source, save_in_progress)
                        {
                            needs_paint = true;
                        }
                    }
                }
                (Interface::Pointer, 0 /* enter */) | (Interface::Pointer, 1 /* motion */) => {
                    // Hover-state isn't tracked in v1; the apply / cycle
                    // buttons highlight on press only.
                }
                (Interface::Keyboard, 1 /* key */) => {
                    if let Ok(key) = KeyboardKey::decode(&event.payload) {
                        if key.state == key_state::PRESSED {
                            // Tab key cycles tabs; arrow keys cycle a
                            // preference; Enter applies or refreshes About.
                            let save_in_progress = active_save.is_some() || state.save_requested();
                            if state.handle_key(key.key, &source, save_in_progress) {
                                needs_paint = true;
                            }
                        }
                    }
                }
                (Interface::Buffer, 1 /* release */) => {
                    if let Some(p) = pool.as_mut() {
                        let _ = p.handle_release(event.object_id);
                    }
                }
                _ => {}
            }
        }

        // A close with no in-flight write can finish immediately after every
        // queued display event (including buffer releases) has been handled.
        if close_pending && active_save.is_none() {
            state.discard_save_request();
            return Ok(());
        }
        if close_pending {
            let save = active_save.as_mut().expect("active save checked above");
            if save.job.committed {
                state.status = "finishing durable save before closing...".to_string();
            } else {
                let _ = save.job.request_cancel();
                state.status = "closing after save cleanup...".to_string();
            }
            needs_paint = true;
        }

        if let Some(buffers) = pool.as_mut().filter(|buffers| buffers.commit_pending()) {
            let _ = buffers.progress_commit(&mut window)?;
        }

        if !configured_once && window.is_configured() {
            configured_once = true;
            BufferPool::replace(&mut pool, window.app_mut(), DEFAULT_WIDTH, DEFAULT_HEIGHT)?;
            needs_paint = true;
        }

        if needs_paint && configured_once {
            let p = pool.as_mut().expect("pool initialised");
            if let Some(mut canvas) = p.acquire_back_canvas() {
                paint(&mut canvas, &state);
                drop(canvas);
                let _ = p.commit_and_swap(&mut window)?;
                needs_paint = false;
            }
        }
        window.flush_outbound()?;

        if active_save.is_none() {
            if let Some(revision) = state.take_save_request() {
                match PreferenceWriteJob::new(
                    &state.config_path,
                    state.prefs.clone(),
                    Box::new(StdPreferenceFs),
                ) {
                    Ok(job) => active_save = Some(ActivePreferenceSave { revision, job }),
                    Err(error) => {
                        state.finish_save(revision, Err(error));
                        needs_paint = true;
                    }
                }
            }
        }

        // Preference persistence runs after display dispatch, release
        // handling, paint, and protocol flush. One step performs no more than
        // one filesystem operation and no more than one 16 KiB write.
        let mut save_wait = None;
        let mut save_progress = false;
        if let Some(save) = active_save.as_mut() {
            match save.job.step() {
                PreferenceWriteTurn::Progress => save_progress = true,
                PreferenceWriteTurn::Blocked(fd) => save_wait = Some(WaitFd::writable(fd)),
                PreferenceWriteTurn::Complete => {
                    let revision = save.revision;
                    active_save = None;
                    state.finish_save(revision, Ok(()));
                    needs_paint = true;
                    save_progress = true;
                    if close_pending {
                        return Ok(());
                    }
                }
                PreferenceWriteTurn::Cancelled => {
                    active_save = None;
                    if close_pending {
                        return Ok(());
                    }
                    state.status = "save cancelled".to_string();
                    needs_paint = true;
                    save_progress = true;
                }
                PreferenceWriteTurn::Failed(error) => {
                    let revision = save.revision;
                    active_save = None;
                    state.finish_save(revision, Err(error));
                    needs_paint = true;
                    save_progress = true;
                    if close_pending {
                        return Ok(());
                    }
                }
            }
        }
        if save_progress {
            continue;
        }
        if pool.as_ref().is_some_and(BufferPool::commit_pending) && !window.outbound_pending() {
            continue;
        }
        if let Some(wait) = save_wait {
            window.wait_with(&[wait], None)?;
        } else {
            window.wait(None)?;
        }
    }
}

/// Test-only: run a single paint pass into a host-allocated
/// canvas. Used by the unit tests to verify each tab paints
/// without touching the network or a display server.
#[cfg(test)]
pub fn paint_for_test(canvas: &mut Canvas<'_>, tab: Tab) {
    let state = State {
        tab,
        ..State::default()
    };
    paint(canvas, &state);
}

#[derive(Debug, Clone)]
struct State {
    tab: Tab,
    config_path: String,
    prefs: Preferences,
    revision: u64,
    pending_save_revision: Option<u64>,
    /// Status line (e.g. "saved", "save failed: <reason>").
    status: String,
    about: AboutPane,
}

impl Default for State {
    fn default() -> Self {
        State {
            tab: Tab::Wallpaper,
            config_path: DEFAULT_CONFIG.to_string(),
            prefs: Preferences::empty(),
            revision: 0,
            pending_save_revision: None,
            status: String::new(),
            about: AboutPane::default(),
        }
    }
}

impl State {
    fn new(config_path: &str) -> Self {
        let prefs = read_prefs(config_path);
        State {
            tab: Tab::Wallpaper,
            config_path: config_path.to_string(),
            prefs,
            revision: 0,
            pending_save_revision: None,
            status: String::new(),
            about: AboutPane::default(),
        }
    }

    fn handle_pointer_press<S: AboutSource + ?Sized>(
        &mut self,
        x: i32,
        y: i32,
        source: &S,
        save_in_progress: bool,
    ) -> bool {
        if (TAB_BAR_TOP..TAB_BAR_TOP + TAB_BAR_HEIGHT).contains(&y) {
            let tab_w = (DEFAULT_WIDTH as i32) / Tab::ALL.len() as i32;
            let idx = (x / tab_w).clamp(0, (Tab::ALL.len() as i32) - 1) as usize;
            self.select_tab(Tab::ALL[idx], source, save_in_progress);
            return true;
        }
        // The shared action area applies a preference or refreshes
        // the live About snapshot, depending on the selected tab.
        if (ACTION_BUTTON_X..ACTION_BUTTON_X + ACTION_BUTTON_W as i32).contains(&x)
            && (ACTION_BUTTON_Y..ACTION_BUTTON_Y + ACTION_BUTTON_H as i32).contains(&y)
        {
            self.activate(source, save_in_progress);
            return true;
        }
        false
    }

    fn handle_key<S: AboutSource + ?Sized>(
        &mut self,
        scancode: u32,
        source: &S,
        save_in_progress: bool,
    ) -> bool {
        match scancode {
            0x2B /* tab */ => {
                let idx = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
                self.select_tab(
                    Tab::ALL[(idx + 1) % Tab::ALL.len()],
                    source,
                    save_in_progress,
                );
                true
            }
            0x28 /* enter */ => {
                self.activate(source, save_in_progress);
                true
            }
            0x15 /* r */ if self.tab == Tab::About => {
                self.about.refresh(source);
                true
            }
            0x4F /* arrow right */ | 0x51 /* arrow down */ if self.tab != Tab::About => {
                self.cycle(false);
                true
            }
            0x50 /* arrow left */ | 0x52 /* arrow up */ if self.tab != Tab::About => {
                self.cycle(true);
                true
            }
            _ => false,
        }
    }

    fn select_tab<S: AboutSource + ?Sized>(
        &mut self,
        tab: Tab,
        source: &S,
        save_in_progress: bool,
    ) {
        self.tab = tab;
        if !save_in_progress {
            self.status.clear();
        }
        if tab == Tab::About {
            self.about.refresh(source);
        }
    }

    fn activate<S: AboutSource + ?Sized>(&mut self, source: &S, save_in_progress: bool) {
        if self.tab == Tab::About {
            self.about.refresh(source);
        } else if save_in_progress {
            self.status = "save already in progress".to_string();
        } else {
            self.cycle(false);
            self.pending_save_revision = Some(self.revision);
            self.status = "saving...".to_string();
        }
    }

    fn save_requested(&self) -> bool {
        self.pending_save_revision.is_some()
    }

    fn take_save_request(&mut self) -> Option<u64> {
        self.pending_save_revision.take()
    }

    fn discard_save_request(&mut self) {
        self.pending_save_revision = None;
    }

    fn finish_save(&mut self, revision: u64, result: io::Result<()>) {
        self.status = match result {
            Ok(()) if self.revision == revision => "saved".to_string(),
            Ok(()) => "saved earlier changes; Apply to save current values".to_string(),
            Err(error) => format!("save failed: {error}"),
        };
    }

    fn cycle(&mut self, reverse: bool) {
        match self.tab {
            Tab::Wallpaper => {
                self.prefs.wallpaper_name = Some(cycle_str(
                    self.prefs.wallpaper_name.as_deref(),
                    WALLPAPERS,
                    reverse,
                ));
            }
            Tab::Appearance => {
                self.prefs.theme_name =
                    Some(cycle_str(self.prefs.theme_name.as_deref(), THEMES, reverse));
                self.prefs.theme_fit =
                    Some(cycle_str(self.prefs.theme_fit.as_deref(), FITS, reverse));
            }
            Tab::Keyboard => {
                self.prefs.keyboard_layout = Some(cycle_str(
                    self.prefs.keyboard_layout.as_deref(),
                    LAYOUTS,
                    reverse,
                ));
            }
            Tab::Timezone => {
                self.prefs.timezone_iana = Some(cycle_str(
                    self.prefs.timezone_iana.as_deref(),
                    TIMEZONES,
                    reverse,
                ));
            }
            Tab::Terminal => {
                self.prefs.terminal_font = Some(cycle_str(
                    self.prefs.terminal_font.as_deref(),
                    FONTS,
                    reverse,
                ));
            }
            Tab::About => {}
        }
        if self.tab != Tab::About {
            self.revision = self.revision.wrapping_add(1);
        }
    }
}

fn cycle_str(current: Option<&str>, choices: &[&str], reverse: bool) -> String {
    let idx = current
        .and_then(|c| choices.iter().position(|opt| *opt == c))
        .unwrap_or(0);
    let next_idx = if reverse {
        (idx + choices.len() - 1) % choices.len()
    } else {
        (idx + 1) % choices.len()
    };
    choices[next_idx].to_string()
}

const TAB_BAR_TOP: i32 = 22;
const TAB_BAR_HEIGHT: i32 = 24;
const PANE_TOP: i32 = TAB_BAR_TOP + TAB_BAR_HEIGHT + 6;
const ACTION_BUTTON_X: i32 = (DEFAULT_WIDTH as i32) - 110;
const ACTION_BUTTON_Y: i32 = (DEFAULT_HEIGHT as i32) - 40;
const ACTION_BUTTON_W: u32 = 96;
const ACTION_BUTTON_H: u32 = 22;

fn paint(canvas: &mut Canvas<'_>, state: &State) {
    let bg = Color::rgb(0xfa, 0xfa, 0xfa);
    let titlebar = Color::rgb(0x40, 0x60, 0x70);
    let tab_bar_bg = Color::rgb(0xe0, 0xe0, 0xe6);
    let tab_active = Color::rgb(0xfa, 0xfa, 0xfa);
    let tab_inactive = Color::rgb(0xd0, 0xd0, 0xd6);
    let text_fg = Color::rgb(0x10, 0x10, 0x10);
    let muted_fg = Color::rgb(0x60, 0x60, 0x70);
    let button_bg = Color::rgb(0x6a, 0x82, 0x9a);
    let button_fg = Color::rgb(0xff, 0xff, 0xff);

    canvas.fill_rect(
        Rect {
            x: 0,
            y: 0,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
        },
        bg,
    );
    let titlebar_h = TAB_BAR_TOP as u32;
    canvas.fill_rect(
        Rect {
            x: 0,
            y: 0,
            width: DEFAULT_WIDTH,
            height: titlebar_h,
        },
        titlebar,
    );
    canvas.draw_text(
        8,
        ((titlebar_h as i32 - GLYPH_HEIGHT as i32) / 2).max(0),
        "Settings",
        Color::rgb(0xff, 0xff, 0xff),
    );

    canvas.fill_rect(
        Rect {
            x: 0,
            y: TAB_BAR_TOP,
            width: DEFAULT_WIDTH,
            height: TAB_BAR_HEIGHT as u32,
        },
        tab_bar_bg,
    );
    let tab_w = (DEFAULT_WIDTH as i32) / Tab::ALL.len() as i32;
    for (i, tab) in Tab::ALL.iter().enumerate() {
        let x = (i as i32) * tab_w;
        let w = if i == Tab::ALL.len() - 1 {
            DEFAULT_WIDTH as i32 - x
        } else {
            tab_w
        };
        let active = *tab == state.tab;
        canvas.fill_rect(
            Rect {
                x,
                y: TAB_BAR_TOP,
                width: w as u32,
                height: TAB_BAR_HEIGHT as u32,
            },
            if active { tab_active } else { tab_inactive },
        );
        let text = tab.label();
        let text_w = (text.chars().count() as i32) * 8;
        canvas.draw_text(
            x + (w - text_w) / 2,
            TAB_BAR_TOP + (TAB_BAR_HEIGHT - GLYPH_HEIGHT as i32) / 2,
            text,
            text_fg,
        );
    }

    let mut y = PANE_TOP;
    let line_h = 16;
    match state.tab {
        Tab::Wallpaper => {
            canvas.draw_text(16, y, "Wallpaper", text_fg);
            y += line_h;
            canvas.draw_text(
                16,
                y,
                &format!(
                    "Current: {}",
                    state.prefs.wallpaper_name.as_deref().unwrap_or("(none)")
                ),
                muted_fg,
            );
            y += line_h;
            canvas.draw_text(
                16,
                y,
                "Click Apply to cycle through bundled wallpapers.",
                muted_fg,
            );
            y += line_h;
            canvas.draw_text(
                16,
                y,
                &format!("Choices: {}", WALLPAPERS.join(", ")),
                muted_fg,
            );
        }
        Tab::Appearance => {
            canvas.draw_text(16, y, "Theme", text_fg);
            y += line_h;
            canvas.draw_text(
                16,
                y,
                &format!(
                    "Current theme: {}",
                    state.prefs.theme_name.as_deref().unwrap_or("(default)")
                ),
                muted_fg,
            );
            y += line_h;
            canvas.draw_text(
                16,
                y,
                &format!(
                    "Wallpaper fit: {}",
                    state.prefs.theme_fit.as_deref().unwrap_or("(stretch)")
                ),
                muted_fg,
            );
            y += line_h;
            canvas.draw_text(
                16,
                y,
                &format!("Themes: {}.  Fits: {}.", THEMES.join(", "), FITS.join(", "),),
                muted_fg,
            );
        }
        Tab::Keyboard => {
            canvas.draw_text(16, y, "Keyboard layout", text_fg);
            y += line_h;
            canvas.draw_text(
                16,
                y,
                &format!(
                    "Current: {}",
                    state
                        .prefs
                        .keyboard_layout
                        .as_deref()
                        .unwrap_or("(us-qwerty)")
                ),
                muted_fg,
            );
            y += line_h;
            canvas.draw_text(16, y, &format!("Choices: {}", LAYOUTS.join(", ")), muted_fg);
        }
        Tab::Timezone => {
            canvas.draw_text(16, y, "Timezone", text_fg);
            y += line_h;
            canvas.draw_text(
                16,
                y,
                &format!(
                    "Current: {}",
                    state.prefs.timezone_iana.as_deref().unwrap_or("(UTC)")
                ),
                muted_fg,
            );
            y += line_h;
            canvas.draw_text(
                16,
                y,
                "Existing terminals keep the spawn-time zone until they exit.",
                muted_fg,
            );
            y += line_h;
            canvas.draw_text(
                16,
                y,
                &format!("Choices: {}", TIMEZONES.join(", ")),
                muted_fg,
            );
        }
        Tab::Terminal => {
            canvas.draw_text(16, y, "Terminal font", text_fg);
            y += line_h;
            canvas.draw_text(
                16,
                y,
                &format!(
                    "Current: {}",
                    state
                        .prefs
                        .terminal_font
                        .as_deref()
                        .unwrap_or("(unifont-mono-14.pbm)")
                ),
                muted_fg,
            );
            y += line_h;
            canvas.draw_text(16, y, &format!("Choices: {}", FONTS.join(", ")), muted_fg);
        }
        Tab::About => {
            for (index, line) in state.about.lines().iter().enumerate() {
                canvas.draw_text(16, y, line, if index == 0 { text_fg } else { muted_fg });
                y += line_h;
            }
        }
    }

    canvas.fill_rect(
        Rect {
            x: ACTION_BUTTON_X,
            y: ACTION_BUTTON_Y,
            width: ACTION_BUTTON_W,
            height: ACTION_BUTTON_H,
        },
        button_bg,
    );
    let (action_label, action_offset) = if state.tab == Tab::About {
        ("Refresh", 20)
    } else {
        ("Apply >", 24)
    };
    canvas.draw_text(
        ACTION_BUTTON_X + action_offset,
        ACTION_BUTTON_Y + (ACTION_BUTTON_H as i32 - GLYPH_HEIGHT as i32) / 2,
        action_label,
        button_fg,
    );

    let (status, status_color) = if !state.status.is_empty() {
        (
            state.status.as_str(),
            if state.status.starts_with("save failed:") {
                Color::rgb(0x9a, 0x20, 0x20)
            } else {
                text_fg
            },
        )
    } else if state.tab == Tab::About {
        (
            state.about.status.as_str(),
            if state.about.has_errors() {
                Color::rgb(0x9a, 0x20, 0x20)
            } else {
                text_fg
            },
        )
    } else {
        (state.status.as_str(), text_fg)
    };
    if !status.is_empty() {
        canvas.draw_text(
            16,
            DEFAULT_HEIGHT as i32 - 24,
            &elide(status, ABOUT_LINE_CHARS),
            status_color,
        );
    }
}

trait PreferenceFile {
    fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<usize>;
    fn sync_all(&mut self) -> io::Result<()>;
    fn raw_fd(&self) -> RawFd;
}

struct StdPreferenceFile(std::fs::File);

impl PreferenceFile for StdPreferenceFile {
    fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn sync_all(&mut self) -> io::Result<()> {
        self.0.sync_all()
    }

    fn raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

trait PreferenceFs {
    fn create_new(&mut self, path: &Path) -> io::Result<Box<dyn PreferenceFile>>;
    fn open_target(&mut self, path: &Path) -> io::Result<Box<dyn PreferenceFile>>;
    fn close(&mut self, file: Box<dyn PreferenceFile>);
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&mut self, path: &Path) -> io::Result<()>;
}

struct StdPreferenceFs;

impl PreferenceFs for StdPreferenceFs {
    fn create_new(&mut self, path: &Path) -> io::Result<Box<dyn PreferenceFile>> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(|file| Box::new(StdPreferenceFile(file)) as Box<dyn PreferenceFile>)
    }

    fn open_target(&mut self, path: &Path) -> io::Result<Box<dyn PreferenceFile>> {
        std::fs::File::open(path)
            .map(|file| Box::new(StdPreferenceFile(file)) as Box<dyn PreferenceFile>)
    }

    fn close(&mut self, file: Box<dyn PreferenceFile>) {
        drop(file);
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreferenceWriteStage {
    Serialize,
    CreateTemporary,
    Write,
    SyncTemporary,
    CloseTemporary,
    Rename,
    OpenTarget,
    SyncTarget,
    CloseTarget,
    FinishSuccess,
    CleanupCloseTemporary,
    CleanupRemoveTemporary,
    FinishFailure,
    FinishCancelled,
    Finished,
}

enum PendingPreferenceFinish {
    Failure(io::Error),
    Cancelled,
}

enum PreferenceWriteTurn {
    Progress,
    Blocked(RawFd),
    Complete,
    Cancelled,
    Failed(io::Error),
}

struct PreferenceWriteJob {
    fs: Box<dyn PreferenceFs>,
    target: PathBuf,
    file_name: String,
    nonce: u128,
    temporary_attempt: u32,
    temporary_path: Option<PathBuf>,
    temporary: Option<Box<dyn PreferenceFile>>,
    target_file: Option<Box<dyn PreferenceFile>>,
    snapshot: Option<Preferences>,
    bytes: Vec<u8>,
    offset: usize,
    stage: PreferenceWriteStage,
    committed: bool,
    pending_finish: Option<PendingPreferenceFinish>,
}

impl PreferenceWriteJob {
    fn new(path: &str, prefs: Preferences, fs: Box<dyn PreferenceFs>) -> io::Result<Self> {
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self::new_with_nonce(path, prefs, fs, clock ^ u128::from(sequence))
    }

    fn new_with_nonce(
        path: &str,
        prefs: Preferences,
        fs: Box<dyn PreferenceFs>,
        nonce: u128,
    ) -> io::Result<Self> {
        let target = PathBuf::from(path);
        let file_name = target
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "preference path has no file name",
                )
            })?
            .to_string();
        Ok(Self {
            fs,
            target,
            file_name,
            nonce,
            temporary_attempt: 0,
            temporary_path: None,
            temporary: None,
            target_file: None,
            snapshot: Some(prefs),
            bytes: Vec::new(),
            offset: 0,
            stage: PreferenceWriteStage::Serialize,
            committed: false,
            pending_finish: None,
        })
    }

    fn temporary_candidate(&self, attempt: u32) -> PathBuf {
        let name = format!(
            ".{}.pmos-settings-{:x}-{attempt}.tmp",
            self.file_name, self.nonce
        );
        self.target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from(&name), |parent| parent.join(&name))
    }

    /// Cancel before the atomic rename. Once the target is visible, close is
    /// deliberately deferred until the renamed target has been opened and
    /// synced.
    fn request_cancel(&mut self) -> bool {
        if self.committed
            || matches!(
                self.stage,
                PreferenceWriteStage::FinishSuccess
                    | PreferenceWriteStage::FinishFailure
                    | PreferenceWriteStage::FinishCancelled
                    | PreferenceWriteStage::Finished
            )
        {
            return false;
        }
        if !matches!(
            self.stage,
            PreferenceWriteStage::CleanupCloseTemporary
                | PreferenceWriteStage::CleanupRemoveTemporary
        ) {
            self.pending_finish = Some(PendingPreferenceFinish::Cancelled);
            self.begin_precommit_cleanup();
        }
        true
    }

    fn step(&mut self) -> PreferenceWriteTurn {
        match self.stage {
            PreferenceWriteStage::Serialize => {
                let prefs = self.snapshot.take().expect("serialize owns snapshot");
                match prefs.to_toml() {
                    Ok(serialized) => {
                        self.bytes = serialized.into_bytes();
                        self.stage = PreferenceWriteStage::CreateTemporary;
                        PreferenceWriteTurn::Progress
                    }
                    Err(error) => self.fail_precommit(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{error:?}"),
                    )),
                }
            }
            PreferenceWriteStage::CreateTemporary => {
                if self.temporary_attempt >= TEMPORARY_NAME_ATTEMPTS {
                    return self.fail_precommit(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "could not allocate a unique preferences temporary file",
                    ));
                }
                let candidate = self.temporary_candidate(self.temporary_attempt);
                self.temporary_attempt += 1;
                match self.fs.create_new(&candidate) {
                    Ok(file) => {
                        self.temporary_path = Some(candidate);
                        self.temporary = Some(file);
                        self.stage = PreferenceWriteStage::Write;
                        PreferenceWriteTurn::Progress
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        PreferenceWriteTurn::Progress
                    }
                    Err(error) => self.fail_precommit(error),
                }
            }
            PreferenceWriteStage::Write => {
                if self.offset == self.bytes.len() {
                    self.stage = PreferenceWriteStage::SyncTemporary;
                    return PreferenceWriteTurn::Progress;
                }
                let end = self
                    .offset
                    .saturating_add(PREFERENCE_WRITE_CHUNK_BYTES)
                    .min(self.bytes.len());
                let file = self.temporary.as_mut().expect("write owns temporary");
                match file.write_chunk(&self.bytes[self.offset..end]) {
                    Ok(0) => self.fail_precommit(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "preference temporary write returned zero bytes",
                    )),
                    Ok(written) if written <= end - self.offset => {
                        self.offset += written;
                        PreferenceWriteTurn::Progress
                    }
                    Ok(_) => self.fail_precommit(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "preference write reported more bytes than requested",
                    )),
                    Err(error) if io_would_block(&error) => {
                        PreferenceWriteTurn::Blocked(file.raw_fd())
                    }
                    Err(error) => self.fail_precommit(error),
                }
            }
            PreferenceWriteStage::SyncTemporary => {
                let file = self.temporary.as_mut().expect("temporary sync owns file");
                match file.sync_all() {
                    Ok(()) => {
                        self.stage = PreferenceWriteStage::CloseTemporary;
                        PreferenceWriteTurn::Progress
                    }
                    Err(error) if io_would_block(&error) => {
                        PreferenceWriteTurn::Blocked(file.raw_fd())
                    }
                    Err(error) => self.fail_precommit(error),
                }
            }
            PreferenceWriteStage::CloseTemporary => {
                let file = self.temporary.take().expect("close owns temporary");
                self.fs.close(file);
                self.stage = PreferenceWriteStage::Rename;
                PreferenceWriteTurn::Progress
            }
            PreferenceWriteStage::Rename => {
                let temporary = self
                    .temporary_path
                    .as_ref()
                    .expect("rename owns temporary path")
                    .clone();
                match self.fs.rename(&temporary, &self.target) {
                    Ok(()) => {
                        self.temporary_path = None;
                        self.committed = true;
                        self.stage = PreferenceWriteStage::OpenTarget;
                        PreferenceWriteTurn::Progress
                    }
                    Err(error) => self.fail_precommit(error),
                }
            }
            PreferenceWriteStage::OpenTarget => match self.fs.open_target(&self.target) {
                Ok(file) => {
                    self.target_file = Some(file);
                    self.stage = PreferenceWriteStage::SyncTarget;
                    PreferenceWriteTurn::Progress
                }
                Err(error) => self.fail_postcommit(error),
            },
            PreferenceWriteStage::SyncTarget => {
                let file = self.target_file.as_mut().expect("target sync owns file");
                match file.sync_all() {
                    Ok(()) => {
                        self.stage = PreferenceWriteStage::CloseTarget;
                        PreferenceWriteTurn::Progress
                    }
                    Err(error) if io_would_block(&error) => {
                        PreferenceWriteTurn::Blocked(file.raw_fd())
                    }
                    Err(error) => {
                        self.pending_finish = Some(PendingPreferenceFinish::Failure(error));
                        self.stage = PreferenceWriteStage::CloseTarget;
                        PreferenceWriteTurn::Progress
                    }
                }
            }
            PreferenceWriteStage::CloseTarget => {
                let file = self.target_file.take().expect("close owns target");
                self.fs.close(file);
                self.stage = if matches!(
                    self.pending_finish,
                    Some(PendingPreferenceFinish::Failure(_))
                ) {
                    PreferenceWriteStage::FinishFailure
                } else {
                    PreferenceWriteStage::FinishSuccess
                };
                PreferenceWriteTurn::Progress
            }
            PreferenceWriteStage::FinishSuccess => {
                self.stage = PreferenceWriteStage::Finished;
                PreferenceWriteTurn::Complete
            }
            PreferenceWriteStage::CleanupCloseTemporary => {
                if let Some(file) = self.temporary.take() {
                    self.fs.close(file);
                }
                self.stage = if self.temporary_path.is_some() {
                    PreferenceWriteStage::CleanupRemoveTemporary
                } else {
                    self.finish_stage()
                };
                PreferenceWriteTurn::Progress
            }
            PreferenceWriteStage::CleanupRemoveTemporary => {
                let path = self
                    .temporary_path
                    .take()
                    .expect("cleanup owns temporary path");
                match self.fs.remove_file(&path) {
                    Ok(()) => self.stage = self.finish_stage(),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        self.stage = self.finish_stage();
                    }
                    Err(cleanup_error) => {
                        let error = match self.pending_finish.take() {
                            Some(PendingPreferenceFinish::Failure(original)) => io::Error::new(
                                cleanup_error.kind(),
                                format!("{original}; temporary cleanup failed: {cleanup_error}"),
                            ),
                            Some(PendingPreferenceFinish::Cancelled) | None => cleanup_error,
                        };
                        self.pending_finish = Some(PendingPreferenceFinish::Failure(error));
                        self.stage = PreferenceWriteStage::FinishFailure;
                    }
                }
                PreferenceWriteTurn::Progress
            }
            PreferenceWriteStage::FinishFailure => {
                self.stage = PreferenceWriteStage::Finished;
                match self.pending_finish.take() {
                    Some(PendingPreferenceFinish::Failure(error)) => {
                        PreferenceWriteTurn::Failed(error)
                    }
                    _ => PreferenceWriteTurn::Failed(io::Error::other(
                        "preference save failed without an error",
                    )),
                }
            }
            PreferenceWriteStage::FinishCancelled => {
                self.stage = PreferenceWriteStage::Finished;
                self.pending_finish = None;
                PreferenceWriteTurn::Cancelled
            }
            PreferenceWriteStage::Finished => PreferenceWriteTurn::Failed(io::Error::other(
                "preference save job was already completed",
            )),
        }
    }

    fn fail_precommit(&mut self, error: io::Error) -> PreferenceWriteTurn {
        self.pending_finish = Some(PendingPreferenceFinish::Failure(error));
        self.begin_precommit_cleanup();
        PreferenceWriteTurn::Progress
    }

    fn fail_postcommit(&mut self, error: io::Error) -> PreferenceWriteTurn {
        self.pending_finish = Some(PendingPreferenceFinish::Failure(error));
        self.stage = if self.target_file.is_some() {
            PreferenceWriteStage::CloseTarget
        } else {
            PreferenceWriteStage::FinishFailure
        };
        PreferenceWriteTurn::Progress
    }

    fn begin_precommit_cleanup(&mut self) {
        self.stage = if self.temporary.is_some() {
            PreferenceWriteStage::CleanupCloseTemporary
        } else if self.temporary_path.is_some() {
            PreferenceWriteStage::CleanupRemoveTemporary
        } else {
            self.finish_stage()
        };
    }

    fn finish_stage(&self) -> PreferenceWriteStage {
        match self.pending_finish {
            Some(PendingPreferenceFinish::Cancelled) => PreferenceWriteStage::FinishCancelled,
            Some(PendingPreferenceFinish::Failure(_)) | None => PreferenceWriteStage::FinishFailure,
        }
    }
}

impl Drop for PreferenceWriteJob {
    fn drop(&mut self) {
        if let Some(file) = self.temporary.take() {
            self.fs.close(file);
        }
        if let Some(file) = self.target_file.take() {
            self.fs.close(file);
        }
        if !self.committed {
            if let Some(path) = self.temporary_path.take() {
                let _ = self.fs.remove_file(&path);
            }
        }
    }
}

fn io_would_block(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock || error.raw_os_error() == Some(abi::errno::EAGAIN)
}

#[cfg(target_arch = "wasm32")]
struct ActivePreferenceSave {
    revision: u64,
    job: PreferenceWriteJob,
}

fn read_prefs(path: &str) -> Preferences {
    match std::fs::read(path) {
        Ok(b) => Preferences::parse(&b).unwrap_or_else(|_| Preferences::empty()),
        Err(_) => Preferences::empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};
    use std::fs;
    use std::rc::Rc;

    #[derive(Default)]
    struct FakeSource {
        files: RefCell<BTreeMap<String, Result<Vec<u8>, String>>>,
        reads: RefCell<Vec<(String, usize)>>,
    }

    impl FakeSource {
        fn valid() -> Self {
            let source = Self::default();
            source.set_text(PROC_VERSION_PATH, "PMos 0.1.0-alpha (test)\n");
            source.set_text(PROC_STORAGE_PATH, "16777216 3145728 42\n");
            source.set_text(LICENSE_PATH, "\nMIT License\nFull terms follow.\n");
            source.set_text(CREDITS_PATH, "PMos contributors\nAlice\nBob\n");
            source
        }

        fn set_text(&self, path: &str, text: impl AsRef<[u8]>) {
            self.files
                .borrow_mut()
                .insert(path.to_string(), Ok(text.as_ref().to_vec()));
        }

        fn set_error(&self, path: &str, error: &str) {
            self.files
                .borrow_mut()
                .insert(path.to_string(), Err(error.to_string()));
        }
    }

    impl AboutSource for FakeSource {
        fn read_bounded(&self, path: &str, max_bytes: usize) -> Result<BoundedRead, String> {
            self.reads.borrow_mut().push((path.to_string(), max_bytes));
            let entry = self
                .files
                .borrow()
                .get(path)
                .cloned()
                .unwrap_or_else(|| Err("not found".to_string()))?;
            let total_bytes = entry.len() as u64;
            let truncated = entry.len() > max_bytes;
            Ok(BoundedRead {
                bytes: entry.into_iter().take(max_bytes).collect(),
                total_bytes,
                truncated,
            })
        }
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "pmos-settings-gui-{}-{}-{}.toml",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum FsOperation {
        Create(PathBuf),
        Write { path: PathBuf, requested: usize },
        Sync(PathBuf),
        Close,
        Rename { from: PathBuf, to: PathBuf },
        Open(PathBuf),
        Remove(PathBuf),
    }

    #[derive(Debug, Clone, Copy)]
    enum FakeWriteAction {
        Limit(usize),
        Block,
    }

    struct FakePreferenceState {
        target: PathBuf,
        files: BTreeMap<PathBuf, Vec<u8>>,
        operations: Vec<FsOperation>,
        writes: VecDeque<FakeWriteAction>,
        fail_temporary_sync: bool,
        block_target_sync_once: bool,
    }

    impl FakePreferenceState {
        fn new(target: PathBuf, original: &[u8]) -> Self {
            let mut files = BTreeMap::new();
            files.insert(target.clone(), original.to_vec());
            Self {
                target,
                files,
                operations: Vec::new(),
                writes: VecDeque::new(),
                fail_temporary_sync: false,
                block_target_sync_once: false,
            }
        }
    }

    struct FakePreferenceFile {
        state: Rc<RefCell<FakePreferenceState>>,
        path: PathBuf,
        fd: RawFd,
    }

    impl PreferenceFile for FakePreferenceFile {
        fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let action = {
                let mut state = self.state.borrow_mut();
                state.operations.push(FsOperation::Write {
                    path: self.path.clone(),
                    requested: bytes.len(),
                });
                state.writes.pop_front()
            };
            match action {
                Some(FakeWriteAction::Block) => {
                    Err(io::Error::from_raw_os_error(abi::errno::EAGAIN))
                }
                Some(FakeWriteAction::Limit(limit)) => {
                    let written = limit.min(bytes.len());
                    self.state
                        .borrow_mut()
                        .files
                        .get_mut(&self.path)
                        .expect("fake file remains present")
                        .extend_from_slice(&bytes[..written]);
                    Ok(written)
                }
                None => {
                    self.state
                        .borrow_mut()
                        .files
                        .get_mut(&self.path)
                        .expect("fake file remains present")
                        .extend_from_slice(bytes);
                    Ok(bytes.len())
                }
            }
        }

        fn sync_all(&mut self) -> io::Result<()> {
            let mut state = self.state.borrow_mut();
            state.operations.push(FsOperation::Sync(self.path.clone()));
            if self.path != state.target && state.fail_temporary_sync {
                return Err(io::Error::other("injected temporary sync failure"));
            }
            if self.path == state.target && state.block_target_sync_once {
                state.block_target_sync_once = false;
                return Err(io::Error::from_raw_os_error(abi::errno::EAGAIN));
            }
            Ok(())
        }

        fn raw_fd(&self) -> RawFd {
            self.fd
        }
    }

    struct FakePreferenceFs {
        state: Rc<RefCell<FakePreferenceState>>,
    }

    impl PreferenceFs for FakePreferenceFs {
        fn create_new(&mut self, path: &Path) -> io::Result<Box<dyn PreferenceFile>> {
            let mut state = self.state.borrow_mut();
            state
                .operations
                .push(FsOperation::Create(path.to_path_buf()));
            if state.files.contains_key(path) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "injected collision",
                ));
            }
            state.files.insert(path.to_path_buf(), Vec::new());
            Ok(Box::new(FakePreferenceFile {
                state: Rc::clone(&self.state),
                path: path.to_path_buf(),
                fd: 73,
            }))
        }

        fn open_target(&mut self, path: &Path) -> io::Result<Box<dyn PreferenceFile>> {
            let mut state = self.state.borrow_mut();
            state.operations.push(FsOperation::Open(path.to_path_buf()));
            if !state.files.contains_key(path) {
                return Err(io::Error::new(io::ErrorKind::NotFound, "missing target"));
            }
            Ok(Box::new(FakePreferenceFile {
                state: Rc::clone(&self.state),
                path: path.to_path_buf(),
                fd: 74,
            }))
        }

        fn close(&mut self, file: Box<dyn PreferenceFile>) {
            self.state.borrow_mut().operations.push(FsOperation::Close);
            drop(file);
        }

        fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
            let mut state = self.state.borrow_mut();
            state.operations.push(FsOperation::Rename {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
            });
            let bytes = state
                .files
                .remove(from)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing temporary"))?;
            state.files.insert(to.to_path_buf(), bytes);
            Ok(())
        }

        fn remove_file(&mut self, path: &Path) -> io::Result<()> {
            let mut state = self.state.borrow_mut();
            state
                .operations
                .push(FsOperation::Remove(path.to_path_buf()));
            state
                .files
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing temporary"))
        }
    }

    fn fake_job(
        target: &Path,
        prefs: Preferences,
        nonce: u128,
        original: &[u8],
    ) -> (PreferenceWriteJob, Rc<RefCell<FakePreferenceState>>) {
        let state = Rc::new(RefCell::new(FakePreferenceState::new(
            target.to_path_buf(),
            original,
        )));
        let fs = Box::new(FakePreferenceFs {
            state: Rc::clone(&state),
        });
        let job =
            PreferenceWriteJob::new_with_nonce(target.to_str().unwrap(), prefs, fs, nonce).unwrap();
        (job, state)
    }

    #[test]
    fn cycle_str_advances_and_wraps() {
        // None lands on idx 0, then advances by one — so the
        // result is the second entry under forward cycling.
        assert_eq!(cycle_str(None, THEMES, false), "dark");
        assert_eq!(cycle_str(Some("light"), THEMES, false), "dark");
        assert_eq!(cycle_str(Some("dark"), THEMES, false), "light");
        // Reverse cycling from the first entry wraps to the last.
        assert_eq!(cycle_str(Some("light"), THEMES, true), "dark");
        assert_eq!(cycle_str(Some("dark"), THEMES, true), "light");
    }

    #[test]
    fn staged_writer_bounds_turns_handles_partial_eagain_and_syncs_before_success() {
        let target = PathBuf::from("/etc/preferences.toml");
        let mut prefs = Preferences::empty();
        prefs.wallpaper_name = Some("x".repeat(PREFERENCE_WRITE_CHUNK_BYTES * 2 + 137));
        let expected = prefs.to_toml().unwrap().into_bytes();
        let nonce = 0xabc_u128;
        let (mut job, state) = fake_job(&target, prefs, nonce, b"old preferences");
        let first_candidate = job.temporary_candidate(0);
        {
            let mut fake = state.borrow_mut();
            fake.files
                .insert(first_candidate.clone(), b"collision".to_vec());
            fake.writes.extend([
                FakeWriteAction::Limit(7),
                FakeWriteAction::Block,
                FakeWriteAction::Limit(3_001),
            ]);
            fake.block_target_sync_once = true;
        }

        let mut saw_blocked_write = false;
        let mut saw_blocked_sync = false;
        let mut complete = false;
        for _ in 0..96 {
            let before = state.borrow().operations.len();
            let turn = job.step();
            let fake = state.borrow();
            let after = fake.operations.len();
            assert!(
                after.saturating_sub(before) <= 1,
                "one GUI turn performed multiple filesystem operations: {:?}",
                &fake.operations[before..after]
            );
            if let Some(FsOperation::Write { requested, .. }) = fake.operations.last() {
                assert!(*requested <= PREFERENCE_WRITE_CHUNK_BYTES);
            }
            let target_synced = fake
                .operations
                .iter()
                .any(|op| op == &FsOperation::Sync(target.clone()));
            drop(fake);

            match turn {
                PreferenceWriteTurn::Progress => {}
                PreferenceWriteTurn::Blocked(73) => saw_blocked_write = true,
                PreferenceWriteTurn::Blocked(74) => saw_blocked_sync = true,
                PreferenceWriteTurn::Blocked(fd) => panic!("unexpected wait fd {fd}"),
                PreferenceWriteTurn::Complete => {
                    assert!(target_synced, "success preceded renamed-target sync");
                    complete = true;
                    break;
                }
                PreferenceWriteTurn::Cancelled => panic!("save unexpectedly cancelled"),
                PreferenceWriteTurn::Failed(error) => panic!("save failed: {error}"),
            }
            assert!(!matches!(job.stage, PreferenceWriteStage::Finished));
        }

        assert!(complete);
        assert!(saw_blocked_write);
        assert!(saw_blocked_sync);
        let fake = state.borrow();
        assert_eq!(fake.files.get(&target), Some(&expected));
        assert_eq!(
            fake.files.get(&first_candidate),
            Some(&b"collision".to_vec())
        );
        let creates = fake
            .operations
            .iter()
            .filter(|operation| matches!(operation, FsOperation::Create(_)))
            .count();
        assert_eq!(creates, 2, "create_new must retry a colliding sibling");
        let rename_index = fake
            .operations
            .iter()
            .position(|operation| matches!(operation, FsOperation::Rename { .. }))
            .unwrap();
        let open_index = fake
            .operations
            .iter()
            .position(|operation| operation == &FsOperation::Open(target.clone()))
            .unwrap();
        let target_sync_index = fake
            .operations
            .iter()
            .rposition(|operation| operation == &FsOperation::Sync(target.clone()))
            .unwrap();
        assert!(rename_index < open_index && open_index < target_sync_index);
        assert!(matches!(fake.operations.last(), Some(FsOperation::Close)));
    }

    #[test]
    fn precommit_error_preserves_old_target_cleans_temp_and_updates_error_ui() {
        let target = PathBuf::from("/etc/preferences.toml");
        let old = b"[theme]\nname = \"light\"\n";
        let mut prefs = Preferences::empty();
        prefs.theme_name = Some("dark".to_string());
        let (mut job, fake) = fake_job(&target, prefs, 7, old);
        fake.borrow_mut().fail_temporary_sync = true;

        let failure = loop {
            match job.step() {
                PreferenceWriteTurn::Progress | PreferenceWriteTurn::Blocked(_) => {}
                PreferenceWriteTurn::Failed(error) => break error,
                PreferenceWriteTurn::Complete => panic!("failed save reported success"),
                PreferenceWriteTurn::Cancelled => panic!("failed save reported cancellation"),
            }
        };
        let snapshot = fake.borrow();
        assert_eq!(
            snapshot.files.get(&target).map(Vec::as_slice),
            Some(old.as_slice())
        );
        assert_eq!(
            snapshot.files.len(),
            1,
            "failed save leaked a temporary sibling"
        );
        assert!(snapshot
            .operations
            .iter()
            .any(|operation| matches!(operation, FsOperation::Remove(_))));
        drop(snapshot);

        let mut state = State::default();
        state.finish_save(0, Err(failure));
        assert!(state.status.starts_with("save failed:"));
    }

    #[test]
    fn invalid_serialization_never_touches_filesystem_or_replaces_target() {
        let target = PathBuf::from("/etc/preferences.toml");
        let old = b"[theme]\nname = \"light\"\n";
        let mut prefs = Preferences::empty();
        prefs.theme_name = Some("broken\nname".to_string());
        let (mut job, fake) = fake_job(&target, prefs, 11, old);

        assert!(matches!(job.step(), PreferenceWriteTurn::Progress));
        let error = match job.step() {
            PreferenceWriteTurn::Failed(error) => error,
            _ => panic!("invalid snapshot did not fail after serialization"),
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let snapshot = fake.borrow();
        assert!(snapshot.operations.is_empty());
        assert_eq!(
            snapshot.files.get(&target).map(Vec::as_slice),
            Some(old.as_slice())
        );
    }

    #[test]
    fn close_cancels_precommit_but_defers_after_rename_and_input_still_advances() {
        let target = PathBuf::from("/etc/preferences.toml");
        let old = b"old";
        let mut prefs = Preferences::empty();
        prefs.theme_name = Some("dark".to_string());
        let (mut cancelled, cancelled_fs) = fake_job(&target, prefs.clone(), 21, old);
        assert!(matches!(cancelled.step(), PreferenceWriteTurn::Progress));
        assert!(matches!(cancelled.step(), PreferenceWriteTurn::Progress));
        assert!(cancelled.request_cancel());
        loop {
            match cancelled.step() {
                PreferenceWriteTurn::Progress => {}
                PreferenceWriteTurn::Cancelled => break,
                PreferenceWriteTurn::Blocked(_) => panic!("cancel cleanup blocked"),
                PreferenceWriteTurn::Complete => panic!("cancelled save completed"),
                PreferenceWriteTurn::Failed(error) => panic!("cancel cleanup failed: {error}"),
            }
        }
        let cancelled_snapshot = cancelled_fs.borrow();
        assert_eq!(
            cancelled_snapshot.files.get(&target).map(Vec::as_slice),
            Some(old.as_slice())
        );
        assert_eq!(cancelled_snapshot.files.len(), 1);
        drop(cancelled_snapshot);

        let source = FakeSource::valid();
        let mut state = State::default();
        assert!(state.handle_key(0x2B, &source, true));
        assert_eq!(state.tab, Tab::Appearance, "input must advance during save");

        let (mut committed, committed_fs) = fake_job(&target, prefs, 22, old);
        while !committed.committed {
            assert!(matches!(committed.step(), PreferenceWriteTurn::Progress));
        }
        assert!(
            !committed.request_cancel(),
            "rename makes close non-cancellable"
        );
        loop {
            match committed.step() {
                PreferenceWriteTurn::Progress | PreferenceWriteTurn::Blocked(_) => {}
                PreferenceWriteTurn::Complete => break,
                PreferenceWriteTurn::Cancelled => panic!("committed save was cancelled"),
                PreferenceWriteTurn::Failed(error) => panic!("committed save failed: {error}"),
            }
        }
        let committed_snapshot = committed_fs.borrow();
        assert!(committed_snapshot
            .operations
            .iter()
            .any(|operation| operation == &FsOperation::Sync(target.clone())));
    }

    #[test]
    fn apply_reports_saving_until_the_matching_revision_finishes() {
        let source = FakeSource::valid();
        let mut state = State::default();
        assert_eq!(state.config_path, DEFAULT_CONFIG);
        state.activate(&source, false);
        assert!(state.save_requested());
        let revision = state.take_save_request().expect("Apply starts save");
        assert_eq!(state.status, "saving...");
        state.cycle(false);
        state.finish_save(revision, Ok(()));
        assert_eq!(
            state.status,
            "saved earlier changes; Apply to save current values"
        );

        state.activate(&source, false);
        state.discard_save_request();
        assert!(!state.save_requested());
    }

    #[test]
    fn staged_writer_round_trips_on_the_native_filesystem_without_temp_leaks() {
        let path = temp_path("staged-save");
        let mut prefs = Preferences::empty();
        prefs.wallpaper_name = Some("green.png".to_string());
        let mut job = PreferenceWriteJob::new(
            path.to_str().unwrap(),
            prefs.clone(),
            Box::new(StdPreferenceFs),
        )
        .unwrap();
        loop {
            match job.step() {
                PreferenceWriteTurn::Progress => {}
                PreferenceWriteTurn::Complete => break,
                PreferenceWriteTurn::Blocked(fd) => {
                    panic!("native file unexpectedly blocked: {fd}")
                }
                PreferenceWriteTurn::Cancelled => panic!("native save cancelled"),
                PreferenceWriteTurn::Failed(error) => panic!("native save failed: {error}"),
            }
        }
        assert_eq!(
            fs::read(&path).unwrap(),
            prefs.to_toml().unwrap().as_bytes()
        );
        let temporary_prefix = format!(
            ".{}.pmos-settings-",
            path.file_name().unwrap().to_string_lossy()
        );
        assert!(fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(&temporary_prefix)));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn handle_pointer_press_in_tab_strip_changes_tab() {
        let mut state = State::default();
        let source = FakeSource::valid();
        let tab_w = (DEFAULT_WIDTH as i32) / Tab::ALL.len() as i32;
        let third_tab_x = tab_w * 2 + 4;
        let changed = state.handle_pointer_press(third_tab_x, TAB_BAR_TOP + 5, &source, false);
        assert!(changed);
        assert_eq!(state.tab, Tab::Keyboard);
    }

    #[test]
    fn tab_key_advances_tab_and_wraps() {
        let mut state = State::default();
        let source = FakeSource::valid();
        for expected in [
            Tab::Appearance,
            Tab::Keyboard,
            Tab::Timezone,
            Tab::Terminal,
            Tab::About,
            Tab::Wallpaper,
        ] {
            assert!(state.handle_key(0x2B, &source, false));
            assert_eq!(state.tab, expected);
        }
    }

    #[test]
    fn about_refresh_reads_live_fields_with_explicit_byte_caps() {
        let source = FakeSource::valid();
        let mut about = AboutPane::default();

        about.refresh(&source);

        assert_eq!(
            about.version.value.as_deref(),
            Some("PMos 0.1.0-alpha (test)")
        );
        assert_eq!(
            about.storage.value,
            Some(StorageInfo {
                quota_bytes: 16 * 1024 * 1024,
                used_bytes: 3 * 1024 * 1024,
                file_count: 42,
            })
        );
        assert_eq!(
            about.license.value.as_ref().map(|doc| doc.title.as_str()),
            Some("MIT License")
        );
        assert_eq!(
            about.credits.value.as_ref().map(|doc| doc.title.as_str()),
            Some("PMos contributors")
        );
        assert_eq!(about.status, "Live system details refreshed");
        assert_eq!(
            source.reads.borrow().as_slice(),
            [
                (PROC_VERSION_PATH.to_string(), PROC_READ_LIMIT),
                (PROC_STORAGE_PATH.to_string(), PROC_READ_LIMIT),
                (LICENSE_PATH.to_string(), DOC_READ_LIMIT),
                (CREDITS_PATH.to_string(), DOC_READ_LIMIT),
            ]
        );
        let lines = about.lines();
        let abi_line = format!(
            "Kernel ABI: {}.{}",
            abi::version::ABI_MAJOR,
            abi::version::ABI_MINOR,
        );
        assert!(lines.contains(&abi_line));
        assert!(lines.iter().any(|line| line.contains("3.0 MB of 16.0 MB")));
        assert!(lines
            .iter()
            .all(|line| line.chars().count() <= ABOUT_LINE_CHARS));
    }

    #[test]
    fn refresh_preserves_last_good_value_and_marks_it_stale_on_error() {
        let source = FakeSource::valid();
        let mut about = AboutPane::default();
        about.refresh(&source);
        let previous = about.storage.value.clone();
        source.set_text(PROC_VERSION_PATH, "PMos 0.2.0 (test)\n");
        source.set_error(PROC_STORAGE_PATH, "device unavailable");

        about.refresh(&source);

        assert_eq!(about.version.value.as_deref(), Some("PMos 0.2.0 (test)"));
        assert_eq!(about.storage.value, previous);
        assert_eq!(
            about.storage.error.as_deref(),
            Some("/proc/storage: device unavailable")
        );
        assert_eq!(about.status, "Refresh error: storage unavailable");
        assert!(about
            .lines()
            .iter()
            .any(|line| line.starts_with("Storage (stale):")));
    }

    #[test]
    fn malformed_or_oversized_proc_data_is_visible_as_unavailable() {
        let source = FakeSource::valid();
        source.set_text(PROC_VERSION_PATH, "x".repeat(PROC_READ_LIMIT + 1));
        source.set_text(PROC_STORAGE_PATH, "10 invalid 3 extra\n");
        let mut about = AboutPane::default();

        about.refresh(&source);

        assert!(about.version.value.is_none());
        assert!(about
            .version
            .error
            .as_deref()
            .is_some_and(|error| error.contains("exceeds 1024-byte limit")));
        assert!(about.storage.value.is_none());
        assert!(about
            .storage
            .error
            .as_deref()
            .is_some_and(|error| error.contains("invalid used field")));
        assert_eq!(about.status, "Refresh error: version, storage unavailable");
    }

    #[test]
    fn filesystem_source_caps_returned_payload_and_reports_truncation() {
        let path = temp_path("bounded-about-read");
        fs::write(&path, vec![b'x'; 512]).unwrap();

        let read = FsAboutSource
            .read_bounded(path.to_str().unwrap(), 32)
            .unwrap();

        assert_eq!(read.bytes.len(), 32);
        assert_eq!(read.total_bytes, 512);
        assert!(read.truncated);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn about_enter_and_action_button_refresh_without_writing_preferences() {
        let path = temp_path("about-no-preference-write");
        let _ = fs::remove_file(&path);
        let source = FakeSource::valid();
        let mut state = State::new(path.to_str().unwrap());
        state.tab = Tab::About;

        assert!(state.handle_key(0x28, &source, false));
        assert_eq!(state.about.status, "Live system details refreshed");
        assert!(!path.exists());
        assert!(state.handle_pointer_press(
            ACTION_BUTTON_X + 2,
            ACTION_BUTTON_Y + 2,
            &source,
            false,
        ));
        assert_eq!(source.reads.borrow().len(), 8);
        assert!(!path.exists());
    }

    #[test]
    fn selecting_about_refreshes_and_every_tab_paints() {
        let source = FakeSource::valid();
        let mut state = State::default();
        let tab_width = (DEFAULT_WIDTH as i32) / Tab::ALL.len() as i32;

        assert!(state.handle_pointer_press(
            tab_width * (Tab::ALL.len() as i32 - 1) + 2,
            TAB_BAR_TOP + 2,
            &source,
            false,
        ));
        assert_eq!(state.tab, Tab::About);
        assert_eq!(source.reads.borrow().len(), 4);

        for tab in Tab::ALL {
            let mut canvas = Canvas::new(DEFAULT_WIDTH, DEFAULT_HEIGHT);
            paint_for_test(&mut canvas, tab);
        }
    }
}
