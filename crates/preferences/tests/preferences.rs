//! Isolation tests for the `preferences` crate (T183).
//!
//! Pure parser — no mocks needed. These tests pin down the
//! schema, the forward-compat policy, and the error surface
//! used by init (spawn-time TZ), settings (writes), and
//! toolkit::watch_theme (re-parse on fs_watch event).

use preferences::{Preferences, PreferencesError};

#[test]
fn empty_bytes_parses_to_all_none() {
    let prefs = Preferences::parse(b"").expect("empty bytes should parse");
    assert_eq!(prefs, Preferences::empty());
    assert!(prefs.theme_name.is_none());
    assert!(prefs.theme_fit.is_none());
    assert!(prefs.wallpaper_name.is_none());
    assert!(prefs.keyboard_layout.is_none());
    assert!(prefs.timezone_iana.is_none());
    assert!(prefs.terminal_font.is_none());
}

#[test]
fn full_config_parses_every_schema_field() {
    let input = br#"[theme]
name = "dark"
fit  = "stretch"

[wallpaper]
name = "mountains.png"

[keyboard]
layout = "us-qwerty"

[timezone]
iana = "America/New_York"

[terminal]
font = "unifont-mono-14.pbm"
"#;
    let prefs = Preferences::parse(input).expect("full config should parse");
    assert_eq!(prefs.theme_name.as_deref(), Some("dark"));
    assert_eq!(prefs.theme_fit.as_deref(), Some("stretch"));
    assert_eq!(prefs.wallpaper_name.as_deref(), Some("mountains.png"));
    assert_eq!(prefs.keyboard_layout.as_deref(), Some("us-qwerty"));
    assert_eq!(prefs.timezone_iana.as_deref(), Some("America/New_York"));
    assert_eq!(prefs.terminal_font.as_deref(), Some("unifont-mono-14.pbm"));
}

#[test]
fn unknown_key_in_known_section_is_ignored() {
    // Forward-compat: future versions may add keys that older
    // parsers don't understand. Mirrors the .desktop-parser
    // convention used by `crates/shell/src/launcher.rs`.
    let input = br#"[theme]
name = "light"
future_field = "should be ignored"
"#;
    let prefs = Preferences::parse(input).expect("unknown key should be tolerated");
    assert_eq!(prefs.theme_name.as_deref(), Some("light"));
    assert!(prefs.theme_fit.is_none());
}

#[test]
fn unknown_section_is_ignored_not_errored() {
    // Unknown sections are skipped entirely — both the header
    // and every key inside it — so a newer version of the file
    // won't break an older reader.
    let input = br#"[future_feature]
some_key = "some_value"
other_key = "other_value"

[theme]
name = "dark"
"#;
    let prefs = Preferences::parse(input).expect("unknown section should be tolerated");
    assert_eq!(prefs.theme_name.as_deref(), Some("dark"));
    assert!(prefs.wallpaper_name.is_none());
}

#[test]
fn partial_config_parses_only_present_fields() {
    let input = br#"[timezone]
iana = "UTC"
"#;
    let prefs = Preferences::parse(input).expect("partial config should parse");
    assert_eq!(prefs.timezone_iana.as_deref(), Some("UTC"));
    assert!(prefs.theme_name.is_none());
    assert!(prefs.theme_fit.is_none());
    assert!(prefs.wallpaper_name.is_none());
    assert!(prefs.keyboard_layout.is_none());
    assert!(prefs.terminal_font.is_none());
}

#[test]
fn malformed_line_returns_error_with_line_number() {
    // Line 2 is malformed — no `= "value"` shape.
    let input = b"[theme]\nthis is not a key value line\n";
    match Preferences::parse(input) {
        Err(PreferencesError::MalformedLine(2)) => {}
        other => panic!("expected MalformedLine(2), got {:?}", other),
    }
}

#[test]
fn whitespace_and_comment_lines_are_ignored() {
    let input = br#"
# top-of-file comment
   # indented comment

[theme]
   # comment inside a section
name = "dark"

   # trailing comment
"#;
    let prefs = Preferences::parse(input).expect("whitespace + comments should be tolerated");
    assert_eq!(prefs.theme_name.as_deref(), Some("dark"));
}

#[test]
fn invalid_utf8_returns_invalid_utf8_error() {
    let bad = [0xff, 0xfe, 0xfd];
    assert_eq!(Preferences::parse(&bad), Err(PreferencesError::InvalidUtf8));
}

#[test]
fn malformed_section_header_returns_error() {
    // Unterminated bracket.
    let input = b"[theme\nname = \"dark\"\n";
    match Preferences::parse(input) {
        Err(PreferencesError::MalformedSection(1)) => {}
        other => panic!("expected MalformedSection(1), got {:?}", other),
    }
}

#[test]
fn key_outside_any_section_is_malformed() {
    // Schema has no root-level keys, so this is always wrong.
    let input = b"name = \"dark\"\n";
    match Preferences::parse(input) {
        Err(PreferencesError::MalformedLine(1)) => {}
        other => panic!("expected MalformedLine(1), got {:?}", other),
    }
}
