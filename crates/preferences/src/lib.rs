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

use alloc::string::String;
use core::str;

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
