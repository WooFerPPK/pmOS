//! Toolkit theme palettes and the live VFS preference reader.
//!
//! Themes stay entirely on the application side of the display protocol.
//! [`watch_theme`] reads `/etc/preferences.toml` synchronously at startup and
//! then returns a bounded [`ThemeWatcher`]. Production consumers pair it with
//! toolkit `PathWatch` and call [`ThemeWatcher::refresh`] only after
//! stable-parent/current-inode readiness. A changed normalized theme produces
//! one repaint signal; unchanged snapshots do not. The explicit clock-gated
//! poll method remains only for deterministic native compatibility fixtures.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::draw::Color;

/// Maximum preference snapshot accepted by a theme-aware application.
pub const THEME_PREFERENCE_MAX_BYTES: usize = 64 * 1024;

/// Minimum interval between VFS reads, capping each watcher at ten per second.
pub const THEME_POLL_INTERVAL_MS: u64 = 100;

/// Avoid a monotonic-clock syscall on every empty display dispatch.
pub const THEME_CLOCK_CHECK_EVERY_ITERATIONS: u32 = 16;

/// A complete palette for window chrome and default widget
/// backgrounds.
///
/// Field names follow the convention *subject*_*state*, so
/// `titlebar_active` is the titlebar fill for a focused
/// window and `titlebar_inactive` is the titlebar fill for
/// an unfocused window. The toolkit treats "active" as a
/// synonym for "has keyboard focus."
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Short machine-readable identifier, e.g. `"light"` or
    /// `"dark"`. Used by the settings app when writing to
    /// `/etc/preferences.toml`.
    pub name: &'static str,

    /// Default window content background — the colour a
    /// freshly-created [`crate::draw::Canvas`] should be
    /// cleared to before widgets paint over it.
    pub window_background: Color,

    /// Titlebar fill for a focused window.
    pub titlebar_active: Color,
    /// Titlebar fill for an unfocused window.
    pub titlebar_inactive: Color,

    /// App-id text colour on an active titlebar.
    pub titlebar_text_active: Color,
    /// App-id text colour on an inactive titlebar.
    pub titlebar_text_inactive: Color,

    /// 1-pixel window border for a focused window.
    pub border_active: Color,
    /// 1-pixel window border for an unfocused window.
    pub border_inactive: Color,

    /// Resting fill available to titlebar close controls.
    pub close_button: Color,
    /// Close-button fill when the pointer is hovering over
    /// it. The hover-state setter on [`crate::widget::frame::WindowFrame`]
    /// swaps the fill between [`Self::close_button`] and
    /// this value. Input routing that drives the setter is
    /// the display server's job — today tests toggle it
    /// directly.
    pub close_button_hover: Color,
    /// Colour of the "X" glyph drawn on top of the close button.
    pub close_button_glyph: Color,

    /// Default text colour for [`crate::widget::label::Label`]
    /// and other widgets that render body-copy strings
    /// inside the window's content area.
    pub label_text: Color,

    /// Default fill for a [`crate::widget::button::Button`]
    /// in its resting state.
    pub button_fill: Color,
    /// Default fill when the pointer is hovering over a
    /// button.
    pub button_fill_hover: Color,
    /// Default fill while a button is being pressed.
    pub button_fill_pressed: Color,
    /// Default 1-pixel border drawn around a button.
    pub button_border: Color,
    /// Default caption text colour for a button.
    pub button_text: Color,

    /// Default fill for a [`crate::widget::text_input::TextInput`]
    /// in its resting state. Slightly lighter than the
    /// window background so an empty input is still visible
    /// against the chrome.
    pub text_input_bg: Color,
    /// Fill for a [`crate::widget::text_input::TextInput`]
    /// when the pointer is hovering over it.
    pub text_input_bg_hover: Color,
    /// Fill for a [`crate::widget::text_input::TextInput`]
    /// while it has keyboard focus.
    pub text_input_bg_focused: Color,
    /// Text colour inside a
    /// [`crate::widget::text_input::TextInput`]. Also used
    /// for the 1-pixel caret bar.
    pub text_input_fg: Color,
    /// Dimmed colour used to paint the placeholder string
    /// when the input is empty and unfocused.
    pub text_input_placeholder_fg: Color,
    /// 1-pixel border drawn around a
    /// [`crate::widget::text_input::TextInput`].
    pub text_input_border: Color,
}

impl Theme {
    /// The bundled light theme. The neutral layered surfaces, restrained
    /// borders, and blue focus accent follow a modern desktop vocabulary while
    /// remaining PMos-owned artwork rather than copying platform assets.
    pub const LIGHT: Theme = Theme {
        name: "light",
        window_background: Color::rgb(0xf3, 0xf3, 0xf3),
        titlebar_active: Color::rgb(0xf9, 0xf9, 0xf9),
        titlebar_inactive: Color::rgb(0xed, 0xed, 0xed),
        titlebar_text_active: Color::rgb(0x1b, 0x1b, 0x1b),
        titlebar_text_inactive: Color::rgb(0x6d, 0x6d, 0x6d),
        border_active: Color::rgb(0x00, 0x67, 0xc0),
        border_inactive: Color::rgb(0xc6, 0xc6, 0xc6),
        close_button: Color::rgb(0xf9, 0xf9, 0xf9),
        close_button_hover: Color::rgb(0xc4, 0x2b, 0x1c),
        close_button_glyph: Color::rgb(0x1b, 0x1b, 0x1b),
        label_text: Color::rgb(0x1b, 0x1b, 0x1b),
        button_fill: Color::rgb(0xfb, 0xfb, 0xfb),
        button_fill_hover: Color::rgb(0xe9, 0xe9, 0xe9),
        button_fill_pressed: Color::rgb(0xda, 0xda, 0xda),
        button_border: Color::rgb(0xd0, 0xd0, 0xd0),
        button_text: Color::rgb(0x1b, 0x1b, 0x1b),
        text_input_bg: Color::rgb(0xff, 0xff, 0xff),
        text_input_bg_hover: Color::rgb(0xf9, 0xf9, 0xf9),
        text_input_bg_focused: Color::rgb(0xff, 0xff, 0xff),
        text_input_fg: Color::rgb(0x1b, 0x1b, 0x1b),
        text_input_placeholder_fg: Color::rgb(0x7a, 0x7a, 0x7a),
        text_input_border: Color::rgb(0x8a, 0x8a, 0x8a),
    };

    /// Bundled dark theme used by Settings-aware applications. The red close
    /// hover stays intentionally shared with light mode so destructive chrome
    /// retains one recognizable accent across palettes.
    pub const DARK: Theme = Theme {
        name: "dark",
        window_background: Color::rgb(0x20, 0x20, 0x20),
        titlebar_active: Color::rgb(0x20, 0x20, 0x20),
        titlebar_inactive: Color::rgb(0x2b, 0x2b, 0x2b),
        titlebar_text_active: Color::rgb(0xf5, 0xf5, 0xf5),
        titlebar_text_inactive: Color::rgb(0x9a, 0x9a, 0x9a),
        border_active: Color::rgb(0x60, 0xcd, 0xff),
        border_inactive: Color::rgb(0x45, 0x45, 0x45),
        close_button: Color::rgb(0x20, 0x20, 0x20),
        close_button_hover: Color::rgb(0xc4, 0x2b, 0x1c),
        close_button_glyph: Color::rgb(0xf5, 0xf5, 0xf5),
        label_text: Color::rgb(0xf5, 0xf5, 0xf5),
        button_fill: Color::rgb(0x2d, 0x2d, 0x2d),
        button_fill_hover: Color::rgb(0x3a, 0x3a, 0x3a),
        button_fill_pressed: Color::rgb(0x18, 0x18, 0x18),
        button_border: Color::rgb(0x50, 0x50, 0x50),
        button_text: Color::rgb(0xf5, 0xf5, 0xf5),
        text_input_bg: Color::rgb(0x1c, 0x1c, 0x1c),
        text_input_bg_hover: Color::rgb(0x24, 0x24, 0x24),
        text_input_bg_focused: Color::rgb(0x2a, 0x2a, 0x2a),
        text_input_fg: Color::rgb(0xf5, 0xf5, 0xf5),
        text_input_placeholder_fg: Color::rgb(0x9a, 0x9a, 0x9a),
        text_input_border: Color::rgb(0x5c, 0x5c, 0x5c),
    };

    /// Normalize a persisted v1 theme name. Unknown and absent names use the
    /// documented safe light palette.
    pub fn from_name(name: Option<&str>) -> Theme {
        match name {
            Some("dark") => Theme::DARK,
            Some("light") | None | Some(_) => Theme::LIGHT,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::LIGHT
    }
}

/// Read seam shared by the production VFS source and deterministic tests.
pub trait ThemeSource {
    /// `Ok(None)` means the canonical preference file does not exist.
    fn read(&mut self) -> io::Result<Option<Vec<u8>>>;
}

/// Bounded reader for the canonical preferences file.
pub struct FilesystemThemeSource {
    path: PathBuf,
}

impl FilesystemThemeSource {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }
}

impl ThemeSource for FilesystemThemeSource {
    fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::with_capacity(1024);
        file.take((THEME_PREFERENCE_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > THEME_PREFERENCE_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "preferences exceed the 64 KiB theme-reader limit",
            ));
        }
        Ok(Some(bytes))
    }
}

/// Monotonic clock seam used to make the polling cadence testable.
pub trait ThemeClock {
    fn monotonic_ms(&mut self) -> u64;
}

pub struct SystemThemeClock {
    started: Instant,
}

impl SystemThemeClock {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Default for SystemThemeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl ThemeClock for SystemThemeClock {
    fn monotonic_ms(&mut self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
}

/// Application-owned live theme monitor.
///
/// A transient I/O failure retains the last good palette. A missing or
/// malformed snapshot restores the safe light palette, matching the canonical
/// preference contract. [`Self::poll`] returns `Some` only when the normalized
/// palette actually changes, so callers can use it directly as a repaint gate.
pub struct ThemeWatcher<S = FilesystemThemeSource, C = SystemThemeClock> {
    source: S,
    clock: C,
    current: Theme,
    last_poll_ms: u64,
    clock_check_every_iterations: u32,
    iterations_until_clock_check: u32,
}

impl ThemeWatcher<FilesystemThemeSource, SystemThemeClock> {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self::from_parts(FilesystemThemeSource::new(path), SystemThemeClock::new())
    }
}

impl<S: ThemeSource, C: ThemeClock> ThemeWatcher<S, C> {
    /// Construct a watcher and synchronously read its initial palette.
    pub fn from_parts(mut source: S, mut clock: C) -> Self {
        let current = read_theme_snapshot(&mut source).unwrap_or_default();
        let last_poll_ms = clock.monotonic_ms();
        Self {
            source,
            clock,
            current,
            last_poll_ms,
            clock_check_every_iterations: THEME_CLOCK_CHECK_EVERY_ITERATIONS,
            iterations_until_clock_check: 0,
        }
    }

    /// Override the cheap iteration gate for deterministic isolation tests.
    pub fn with_clock_check_every_iterations(mut self, iterations: u32) -> Self {
        self.clock_check_every_iterations = iterations.max(1);
        self.iterations_until_clock_check = 0;
        self
    }

    pub const fn current(&self) -> Theme {
        self.current
    }

    /// Re-read the canonical snapshot immediately after an external
    /// readiness source reports a preference-file mutation.
    pub fn refresh(&mut self) -> Option<Theme> {
        let now_ms = self.clock.monotonic_ms();
        self.refresh_at(now_ms)
    }

    fn refresh_at(&mut self, now_ms: u64) -> Option<Theme> {
        self.last_poll_ms = now_ms;
        let next = read_theme_snapshot(&mut self.source)?;
        if next == self.current {
            return None;
        }
        self.current = next;
        Some(next)
    }

    /// Clock-gated compatibility path for deterministic native fixtures.
    pub fn poll(&mut self) -> Option<Theme> {
        if self.iterations_until_clock_check > 0 {
            self.iterations_until_clock_check -= 1;
            return None;
        }
        self.iterations_until_clock_check = self.clock_check_every_iterations - 1;

        let now_ms = self.clock.monotonic_ms();
        if now_ms.saturating_sub(self.last_poll_ms) < THEME_POLL_INTERVAL_MS {
            return None;
        }
        self.refresh_at(now_ms)
    }
}

/// Start watching the canonical PMos preference file. The initial snapshot is
/// read before this function returns.
pub fn watch_theme() -> ThemeWatcher {
    ThemeWatcher::new(preferences::DEFAULT_PATH)
}

fn read_theme_snapshot(source: &mut impl ThemeSource) -> Option<Theme> {
    match source.read() {
        Ok(Some(bytes)) => Some(
            preferences::Preferences::parse(&bytes)
                .map(|prefs| Theme::from_name(prefs.theme_name.as_deref()))
                .unwrap_or_default(),
        ),
        Ok(None) => Some(Theme::LIGHT),
        Err(_) => None,
    }
}
