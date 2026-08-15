//! Desktop-facing view of `/etc/preferences.toml`.
//!
//! Settings may replace the VFS file atomically or write it in place. Production
//! watches the stable parent plus the current inode, normalises values to its
//! supported choices, and keeps the last usable snapshot across transient I/O
//! failures. The explicit poll clock remains for deterministic native fixtures.

use preferences::Preferences;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use toolkit::draw::Color;
use toolkit::theme::Theme;

/// Upper bound between preference reads while the desktop loop is running.
pub const PREFERENCE_POLL_INTERVAL_MS: u64 = 100;

/// Avoid a clock syscall on every empty display dispatch. The production
/// connection is non-blocking after bootstrap, so sixteen turns remain far
/// below the wall-clock poll interval under normal desktop load.
pub const PREFERENCE_CLOCK_CHECK_EVERY_ITERATIONS: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeChoice {
    Light,
    Dark,
}

impl ThemeChoice {
    pub const fn palette(self) -> Theme {
        match self {
            ThemeChoice::Light => Theme::LIGHT,
            ThemeChoice::Dark => Theme::DARK,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallpaperChoice {
    Blue,
    Green,
    Dark,
}

impl WallpaperChoice {
    /// Stable fallback palette used only when the corresponding bundled PNG
    /// cannot be read or decoded. It keeps the desktop usable without letting
    /// a damaged image blank or crash the shell.
    pub const fn color(self) -> Color {
        match self {
            WallpaperChoice::Blue => Color::rgb(0x35, 0x6b, 0xa8),
            WallpaperChoice::Green => Color::rgb(0x3f, 0x78, 0x5a),
            WallpaperChoice::Dark => Color::rgb(0x20, 0x27, 0x33),
        }
    }

    pub const fn filename(self) -> &'static str {
        match self {
            WallpaperChoice::Blue => "blue.png",
            WallpaperChoice::Green => "green.png",
            WallpaperChoice::Dark => "dark.png",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallpaperFit {
    Stretch,
    Tile,
    Center,
    Fill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimezoneChoice {
    Utc,
    AmericaNewYork,
    EuropeLondon,
    AsiaTokyo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopPreferences {
    pub theme: ThemeChoice,
    pub wallpaper: WallpaperChoice,
    pub wallpaper_fit: WallpaperFit,
    pub timezone: TimezoneChoice,
}

impl DesktopPreferences {
    pub fn from_preferences(raw: &Preferences) -> Self {
        DesktopPreferences {
            theme: match raw.theme_name.as_deref() {
                Some("dark") => ThemeChoice::Dark,
                _ => ThemeChoice::Light,
            },
            wallpaper: match raw.wallpaper_name.as_deref() {
                Some("green.png") | Some("sunset.png") => WallpaperChoice::Green,
                Some("dark.png") | Some("abstract.png") => WallpaperChoice::Dark,
                _ => WallpaperChoice::Blue,
            },
            wallpaper_fit: match raw.theme_fit.as_deref() {
                Some("tile") => WallpaperFit::Tile,
                Some("center") => WallpaperFit::Center,
                Some("fill") => WallpaperFit::Fill,
                _ => WallpaperFit::Stretch,
            },
            timezone: match raw.timezone_iana.as_deref() {
                Some("America/New_York") => TimezoneChoice::AmericaNewYork,
                Some("Europe/London") => TimezoneChoice::EuropeLondon,
                Some("Asia/Tokyo") => TimezoneChoice::AsiaTokyo,
                _ => TimezoneChoice::Utc,
            },
        }
    }
}

impl Default for DesktopPreferences {
    fn default() -> Self {
        DesktopPreferences {
            theme: ThemeChoice::Light,
            wallpaper: WallpaperChoice::Blue,
            wallpaper_fit: WallpaperFit::Stretch,
            timezone: TimezoneChoice::Utc,
        }
    }
}

/// Read seam used by the production filesystem source and deterministic tests.
pub trait PreferenceSource {
    /// `Ok(None)` means the canonical preference file does not exist.
    fn read(&mut self) -> io::Result<Option<Vec<u8>>>;
}

pub struct FilesystemPreferenceSource {
    path: PathBuf,
}

impl FilesystemPreferenceSource {
    pub fn new(path: impl AsRef<Path>) -> Self {
        FilesystemPreferenceSource {
            path: path.as_ref().to_path_buf(),
        }
    }
}

impl PreferenceSource for FilesystemPreferenceSource {
    fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

pub struct PreferenceMonitor<S> {
    source: S,
    current: DesktopPreferences,
}

impl<S: PreferenceSource> PreferenceMonitor<S> {
    pub fn new(mut source: S) -> Self {
        let current = read_snapshot(&mut source).unwrap_or_default();
        PreferenceMonitor { source, current }
    }

    pub const fn current(&self) -> DesktopPreferences {
        self.current
    }

    /// Reload the snapshot. A transient read failure keeps the current state;
    /// a missing or malformed file intentionally restores safe defaults.
    pub fn poll(&mut self) -> bool {
        let Some(next) = read_snapshot(&mut self.source) else {
            return false;
        };
        if next == self.current {
            return false;
        }
        self.current = next;
        true
    }
}

fn read_snapshot(source: &mut impl PreferenceSource) -> Option<DesktopPreferences> {
    match source.read() {
        Ok(Some(bytes)) => Some(
            Preferences::parse(&bytes)
                .map(|raw| DesktopPreferences::from_preferences(&raw))
                .unwrap_or_default(),
        ),
        Ok(None) => Some(DesktopPreferences::default()),
        Err(_) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSnapshot {
    pub monotonic_ms: u64,
    pub unix_seconds: i64,
}

pub trait PreferenceClock {
    fn monotonic_ms(&mut self) -> u64;
    fn unix_seconds(&mut self) -> i64;
}

pub struct SystemPreferenceClock {
    started: Instant,
}

impl SystemPreferenceClock {
    pub fn new() -> Self {
        SystemPreferenceClock {
            started: Instant::now(),
        }
    }
}

impl Default for SystemPreferenceClock {
    fn default() -> Self {
        Self::new()
    }
}

impl PreferenceClock for SystemPreferenceClock {
    fn monotonic_ms(&mut self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    fn unix_seconds(&mut self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs()
            .min(i64::MAX as u64) as i64
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DesktopPreferenceUpdate {
    /// True when the 100 ms VFS preference-read boundary was reached, even if
    /// normalization produced the same snapshot. Wallpaper loading uses this
    /// signal for its separately throttled transient-I/O retry.
    pub preferences_checked: bool,
    pub preferences_changed: bool,
    pub clock_changed: bool,
}

impl DesktopPreferenceUpdate {
    pub const fn needs_repaint(self) -> bool {
        self.preferences_changed || self.clock_changed
    }
}

/// Time-gated preference monitor plus the taskbar's derived wall-clock label.
pub struct DesktopPreferenceRuntime<S, C> {
    monitor: PreferenceMonitor<S>,
    clock: C,
    last_preference_poll_ms: u64,
    clock_text: String,
    clock_check_every_iterations: u32,
    iterations_until_clock_check: u32,
}

impl<S: PreferenceSource, C: PreferenceClock> DesktopPreferenceRuntime<S, C> {
    pub fn new(source: S, mut clock: C) -> Self {
        let monitor = PreferenceMonitor::new(source);
        let monotonic_ms = clock.monotonic_ms();
        let clock_text = format_clock(clock.unix_seconds(), monitor.current().timezone);
        DesktopPreferenceRuntime {
            monitor,
            clock,
            last_preference_poll_ms: monotonic_ms,
            clock_text,
            clock_check_every_iterations: PREFERENCE_CLOCK_CHECK_EVERY_ITERATIONS,
            iterations_until_clock_check: 0,
        }
    }

    /// Override the cheap iteration gate. Primarily useful for deterministic
    /// isolation tests whose mock event loop advances time once per turn.
    pub fn with_clock_check_every_iterations(mut self, iterations: u32) -> Self {
        self.clock_check_every_iterations = iterations.max(1);
        self.iterations_until_clock_check = 0;
        self
    }

    pub const fn preferences(&self) -> DesktopPreferences {
        self.monitor.current()
    }

    pub fn clock_text(&self) -> &str {
        &self.clock_text
    }

    pub fn poll(&mut self) -> DesktopPreferenceUpdate {
        if self.iterations_until_clock_check > 0 {
            self.iterations_until_clock_check -= 1;
            return DesktopPreferenceUpdate::default();
        }
        self.iterations_until_clock_check = self.clock_check_every_iterations - 1;

        let monotonic_ms = self.clock.monotonic_ms();
        let elapsed = monotonic_ms.saturating_sub(self.last_preference_poll_ms);
        if elapsed < PREFERENCE_POLL_INTERVAL_MS {
            return DesktopPreferenceUpdate::default();
        }
        self.last_preference_poll_ms = monotonic_ms;
        let preferences_changed = self.monitor.poll();
        let clock_changed = self.refresh_clock_text();
        DesktopPreferenceUpdate {
            preferences_checked: true,
            preferences_changed,
            clock_changed,
        }
    }

    /// Re-read preferences immediately after the stable-parent/current-inode
    /// watch reports a change. This is the event-driven production path; the
    /// time-gated [`Self::poll`] remains for deterministic injected fixtures.
    pub fn refresh_preferences(&mut self) -> DesktopPreferenceUpdate {
        self.last_preference_poll_ms = self.clock.monotonic_ms();
        let preferences_changed = self.monitor.poll();
        let clock_changed = self.refresh_clock_text();
        DesktopPreferenceUpdate {
            preferences_checked: true,
            preferences_changed,
            clock_changed,
        }
    }

    /// Refresh only the wall-clock label after its real minute deadline.
    pub fn refresh_clock(&mut self) -> DesktopPreferenceUpdate {
        DesktopPreferenceUpdate {
            clock_changed: self.refresh_clock_text(),
            ..DesktopPreferenceUpdate::default()
        }
    }

    /// Relative delay until the next minute boundary. The desktop clock has
    /// minute precision, so no idle wake is needed before this deadline.
    pub fn next_clock_deadline(&mut self) -> Duration {
        let seconds = self.clock.unix_seconds().rem_euclid(60) as u64;
        Duration::from_secs(60 - seconds)
    }

    fn refresh_clock_text(&mut self) -> bool {
        let next_clock = format_clock(self.clock.unix_seconds(), self.monitor.current().timezone);
        if next_clock == self.clock_text {
            return false;
        }
        self.clock_text = next_clock;
        true
    }
}

/// Format the desktop clock for the supported offline IANA subset. This does
/// not mutate the shell process's `TZ`; it only controls the shell-owned clock.
pub fn format_clock(unix_seconds: i64, timezone: TimezoneChoice) -> String {
    let (offset_seconds, suffix) = timezone_offset(unix_seconds, timezone);
    let local = unix_seconds.saturating_add(offset_seconds as i64);
    let seconds_in_day = local.rem_euclid(86_400);
    let hour = seconds_in_day / 3_600;
    let minute = (seconds_in_day % 3_600) / 60;
    format!("{hour:02}:{minute:02} {suffix}")
}

fn timezone_offset(unix_seconds: i64, timezone: TimezoneChoice) -> (i32, &'static str) {
    match timezone {
        TimezoneChoice::Utc => (0, "UTC"),
        TimezoneChoice::AsiaTokyo => (9 * 3_600, "JST"),
        TimezoneChoice::AmericaNewYork => {
            let (year, _, _) = civil_from_days(unix_seconds.div_euclid(86_400));
            let start_day = nth_weekday_of_month(year, 3, 0, 2);
            let end_day = nth_weekday_of_month(year, 11, 0, 1);
            let start = days_from_civil(year, 3, start_day) * 86_400 + 7 * 3_600;
            let end = days_from_civil(year, 11, end_day) * 86_400 + 6 * 3_600;
            if unix_seconds >= start && unix_seconds < end {
                (-4 * 3_600, "EDT")
            } else {
                (-5 * 3_600, "EST")
            }
        }
        TimezoneChoice::EuropeLondon => {
            let (year, _, _) = civil_from_days(unix_seconds.div_euclid(86_400));
            let start_day = last_weekday_of_month(year, 3, 0);
            let end_day = last_weekday_of_month(year, 10, 0);
            let start = days_from_civil(year, 3, start_day) * 86_400 + 3_600;
            let end = days_from_civil(year, 10, end_day) * 86_400 + 3_600;
            if unix_seconds >= start && unix_seconds < end {
                (3_600, "BST")
            } else {
                (0, "GMT")
            }
        }
    }
}

fn nth_weekday_of_month(year: i32, month: u32, weekday: u32, nth: u32) -> u32 {
    let first_weekday = weekday_from_days(days_from_civil(year, month, 1));
    1 + (weekday + 7 - first_weekday) % 7 + (nth - 1) * 7
}

fn last_weekday_of_month(year: i32, month: u32, weekday: u32) -> u32 {
    let last = days_in_month(year, month);
    let last_weekday = weekday_from_days(days_from_civil(year, month, last));
    last - (last_weekday + 7 - weekday) % 7
}

fn weekday_from_days(days_since_epoch: i64) -> u32 {
    // 1970-01-01 was Thursday; Sunday is zero.
    (days_since_epoch + 4).rem_euclid(7) as u32
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 30,
    }
}

fn is_leap_year(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

// Howard Hinnant's civil-calendar conversion, with day zero at 1970-01-01.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = month as i32 + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day as i32 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    (era * 146_097 + day_of_era - 719_468) as i64
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era as i32 + era as i32 * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i32::from(month <= 2);
    (year, month as u32, day as u32)
}
