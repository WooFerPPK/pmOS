#![no_std]

//! Typed accessor library for `/etc/preferences.toml` (T183).
//!
//! Shared by init (preference read-at-spawn path for the `TZ`
//! env-var whitelist per `contracts/init-conf.md §3.6`), the
//! settings app, the desktop shell, the toolkit's `watch_theme`
//! helper, and the terminal. Because init is linked against the
//! kernel's `no_std + alloc` universe, this crate is
//! `#![no_std]`-compatible and pulls in no external dependencies.
//!
//! The parser is deliberately hand-rolled: the schema is a flat
//! set of `section.key = "value"` string pairs, and a full
//! TOML crate would drag in dependencies (indexmap, serde,
//! lexical-core) that outweigh the handful of lines below. The
//! trade-off is explicit — we reject anything that isn't a
//! plain `key = "quoted string"` line with a clear error.
//!
//! ### Forward-compat policy (mirrors the .desktop-parser
//! convention used elsewhere in this repo):
//! * Unknown keys inside a known section are silently ignored.
//! * Unknown section headers are silently ignored (their body
//!   is skipped line-by-line until the next header or EOF).
//! * Malformed syntax — a non-blank, non-comment, non-header
//!   line that doesn't match `key = "value"` — is an error.
//! * Invalid UTF-8 in the input bytes is an error.
//!
//! ### Schema (T183 / T187 / T188 / T190):
//! ```toml
//! [theme]
//! name = "dark"
//! fit  = "stretch"
//!
//! [wallpaper]
//! name = "mountains.png"
//!
//! [keyboard]
//! layout = "us-qwerty"
//!
//! [timezone]
//! iana = "America/New_York"
//!
//! [terminal]
//! font = "unifont-mono-14.pbm"
//! ```
//!
//! All fields are optional; `Preferences::empty()` yields the
//! all-`None` baseline that callers treat as "use the built-in
//! defaults".

extern crate alloc;

use alloc::{string::String, vec::Vec};
use core::str;

/// Canonical VFS location shared by every preference reader and writer.
pub const DEFAULT_PATH: &str = "/etc/preferences.toml";

/// Canonical names of the keyboard layouts bundled with PMos v1.
///
/// Settings uses this list for its finite picker and the display server uses
/// [`KeyboardLayout::from_name`] when normalising the persisted preference.
/// Keeping the names here prevents the writer and live reader from drifting.
pub const KEYBOARD_LAYOUT_NAMES: &[&str] = &["us-qwerty", "uk-qwerty", "dvorak"];

/// Canonical terminal font names bundled with PMos v1, safe default first.
pub const TERMINAL_FONT_NAMES: &[&str] = &["unifont-mono-14.pbm", "pc-vga-16.pbm"];

/// Safe timezone used when the preference is absent, malformed, or outside
/// the v1 bundle.
pub const DEFAULT_TIMEZONE_NAME: &str = "UTC";

/// Canonical IANA timezone names bundled with PMos v1, safe default first.
///
/// Consumers must normalize persisted values through
/// [`normalize_timezone_name`] before using them as an environment value or
/// constructing a zoneinfo asset path.
pub const TIMEZONE_NAMES: &[&str] = &[
    DEFAULT_TIMEZONE_NAME,
    "America/New_York",
    "Europe/London",
    "Asia/Tokyo",
];

/// Return the canonical bundled timezone matching `name`, or UTC when the
/// value is absent or unsupported.
pub fn normalize_timezone_name(name: Option<&str>) -> &'static str {
    match name {
        Some("America/New_York") => TIMEZONE_NAMES[1],
        Some("Europe/London") => TIMEZONE_NAMES[2],
        Some("Asia/Tokyo") => TIMEZONE_NAMES[3],
        Some("UTC") | None | Some(_) => DEFAULT_TIMEZONE_NAME,
    }
}

/// Parse one preference-file snapshot and return its normalized spawn-time
/// timezone. Missing or malformed bytes select UTC.
pub fn timezone_from_preferences_bytes(bytes: Option<&[u8]>) -> &'static str {
    bytes
        .and_then(|bytes| Preferences::parse(bytes).ok())
        .map_or(DEFAULT_TIMEZONE_NAME, |prefs| prefs.normalized_timezone())
}

/// Clone a baseline environment and apply the spawn-time timezone whitelist.
/// Existing `TZ` entries are removed before the validated value is appended,
/// so the result contains exactly one authoritative timezone entry.
pub fn spawn_environment_with_timezone(
    baseline: &[(String, String)],
    preference_bytes: Option<&[u8]>,
) -> Vec<(String, String)> {
    let timezone = timezone_from_preferences_bytes(preference_bytes);
    let mut environment = baseline
        .iter()
        .filter(|(key, _)| key != "TZ")
        .cloned()
        .collect::<Vec<_>>();
    environment.push((String::from("TZ"), String::from(timezone)));
    environment
}

/// A keyboard layout whose binary map is bundled with PMos v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyboardLayout {
    #[default]
    UsQwerty,
    UkQwerty,
    Dvorak,
}

impl KeyboardLayout {
    pub const fn as_str(self) -> &'static str {
        match self {
            KeyboardLayout::UsQwerty => "us-qwerty",
            KeyboardLayout::UkQwerty => "uk-qwerty",
            KeyboardLayout::Dvorak => "dvorak",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "us-qwerty" => Some(KeyboardLayout::UsQwerty),
            "uk-qwerty" => Some(KeyboardLayout::UkQwerty),
            "dvorak" => Some(KeyboardLayout::Dvorak),
            _ => None,
        }
    }
}

/// Parsed `/etc/preferences.toml` snapshot.
///
/// Every field is optional — missing sections or missing keys
/// are indistinguishable from "the user never set this yet".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Preferences {
    pub theme_name: Option<String>,
    pub theme_fit: Option<String>,
    pub wallpaper_name: Option<String>,
    pub keyboard_layout: Option<String>,
    pub timezone_iana: Option<String>,
    pub terminal_font: Option<String>,
}

/// Error returned by `Preferences::parse`.
///
/// Line numbers are 1-based to match editor tooling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreferencesError {
    /// The raw bytes were not valid UTF-8.
    InvalidUtf8,
    /// A non-blank, non-comment line (outside a section header)
    /// was not of the form `key = "value"`.
    MalformedLine(u32),
    /// A `[section]` header was malformed (unterminated bracket
    /// or empty name). Kept distinct from `MalformedLine` so
    /// callers that care can report it specifically.
    MalformedSection(u32),
    /// A value cannot be represented by the deliberately narrow
    /// quoted-string format. The field name identifies which value
    /// contained a quote or line break.
    InvalidValue(&'static str),
}

impl Preferences {
    /// All-None baseline — the value callers should treat as
    /// "file missing / file empty / user hasn't configured
    /// anything yet". Const so init can stash it in a static.
    pub const fn empty() -> Preferences {
        Preferences {
            theme_name: None,
            theme_fit: None,
            wallpaper_name: None,
            keyboard_layout: None,
            timezone_iana: None,
            terminal_font: None,
        }
    }

    /// Normalize the persisted timezone against the finite v1 bundle.
    pub fn normalized_timezone(&self) -> &'static str {
        normalize_timezone_name(self.timezone_iana.as_deref())
    }

    /// Parse the contents of `/etc/preferences.toml`.
    ///
    /// See the crate-level docs for the forward-compat policy.
    pub fn parse(bytes: &[u8]) -> Result<Preferences, PreferencesError> {
        let text = str::from_utf8(bytes).map_err(|_| PreferencesError::InvalidUtf8)?;
        let mut out = Preferences::empty();
        let mut section = Section::None;

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = (idx as u32) + 1;
            let line = raw_line.trim();

            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some(rest) = line.strip_prefix('[') {
                let name = rest
                    .strip_suffix(']')
                    .ok_or(PreferencesError::MalformedSection(line_no))?
                    .trim();
                if name.is_empty() {
                    return Err(PreferencesError::MalformedSection(line_no));
                }
                section = Section::from_name(name);
                continue;
            }

            let (key, value) = split_key_value(line, line_no)?;

            match section {
                Section::None => {
                    // Key before any section header — treat as
                    // malformed. The TOML spec allows a "root"
                    // table, but our schema has no root-level
                    // keys, so this is always a mistake.
                    return Err(PreferencesError::MalformedLine(line_no));
                }
                Section::Unknown => {
                    // Inside an unknown section: silently skip.
                }
                Section::Theme => match key {
                    "name" => out.theme_name = Some(value),
                    "fit" => out.theme_fit = Some(value),
                    _ => {}
                },
                Section::Wallpaper => {
                    if key == "name" {
                        out.wallpaper_name = Some(value);
                    }
                }
                Section::Keyboard => {
                    if key == "layout" {
                        out.keyboard_layout = Some(value);
                    }
                }
                Section::Timezone => {
                    if key == "iana" {
                        out.timezone_iana = Some(value);
                    }
                }
                Section::Terminal => {
                    if key == "font" {
                        out.terminal_font = Some(value);
                    }
                }
            }
        }

        Ok(out)
    }

    /// Serialize this snapshot into the canonical preference format.
    ///
    /// Sections are emitted in stable schema order and absent sections
    /// are omitted. The parser and serializer intentionally support plain
    /// quoted strings rather than TOML escape sequences, so values with a
    /// quote or line break are rejected instead of producing a file that
    /// cannot be read back.
    pub fn to_toml(&self) -> Result<String, PreferencesError> {
        let mut out = String::new();

        emit_section(
            &mut out,
            "theme",
            &[
                ("name", "theme.name", self.theme_name.as_deref()),
                ("fit", "theme.fit", self.theme_fit.as_deref()),
            ],
        )?;
        emit_section(
            &mut out,
            "wallpaper",
            &[("name", "wallpaper.name", self.wallpaper_name.as_deref())],
        )?;
        emit_section(
            &mut out,
            "keyboard",
            &[("layout", "keyboard.layout", self.keyboard_layout.as_deref())],
        )?;
        emit_section(
            &mut out,
            "timezone",
            &[("iana", "timezone.iana", self.timezone_iana.as_deref())],
        )?;
        emit_section(
            &mut out,
            "terminal",
            &[("font", "terminal.font", self.terminal_font.as_deref())],
        )?;

        Ok(out)
    }
}

fn emit_section(
    out: &mut String,
    section: &str,
    values: &[(&str, &'static str, Option<&str>)],
) -> Result<(), PreferencesError> {
    if values.iter().all(|(_, _, value)| value.is_none()) {
        return Ok(());
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push('[');
    out.push_str(section);
    out.push_str("]\n");
    for (key, field, value) in values {
        let Some(value) = value else {
            continue;
        };
        if value.contains(['"', '\n', '\r']) {
            return Err(PreferencesError::InvalidValue(field));
        }
        out.push_str(key);
        out.push_str(" = \"");
        out.push_str(value);
        out.push_str("\"\n");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Section {
    None,
    Unknown,
    Theme,
    Wallpaper,
    Keyboard,
    Timezone,
    Terminal,
}

impl Section {
    fn from_name(name: &str) -> Section {
        match name {
            "theme" => Section::Theme,
            "wallpaper" => Section::Wallpaper,
            "keyboard" => Section::Keyboard,
            "timezone" => Section::Timezone,
            "terminal" => Section::Terminal,
            _ => Section::Unknown,
        }
    }
}

/// Split `key = "value"` into `(key, value)`.
///
/// Requires exactly one `=`, a key of at least one non-whitespace
/// char, and a value wrapped in ASCII double quotes. Everything
/// else yields `MalformedLine`.
fn split_key_value(line: &str, line_no: u32) -> Result<(&str, String), PreferencesError> {
    let eq = line
        .find('=')
        .ok_or(PreferencesError::MalformedLine(line_no))?;
    let key = line[..eq].trim();
    let value_slice = line[eq + 1..].trim();

    if key.is_empty() {
        return Err(PreferencesError::MalformedLine(line_no));
    }

    let inner = value_slice
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or(PreferencesError::MalformedLine(line_no))?;

    // Embedded double quotes would require escape handling we
    // don't implement — our schema is all plain strings, so
    // reject them explicitly rather than silently truncating.
    if inner.contains('"') {
        return Err(PreferencesError::MalformedLine(line_no));
    }

    Ok((key, String::from(inner)))
}
