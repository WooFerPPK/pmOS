//! Isolation tests for the `preferences` crate (T183).
//!
//! Pure parser — no mocks needed. These tests pin down the
//! schema, the forward-compat policy, and the error surface
//! used by init (spawn-time TZ), settings (writes), and
//! toolkit::watch_theme (re-parse on fs_watch event).

use preferences::{
    normalize_timezone_name, spawn_environment_with_timezone, timezone_from_preferences_bytes,
    KeyboardLayout, Preferences, PreferencesError, DEFAULT_TIMEZONE_NAME, KEYBOARD_LAYOUT_NAMES,
    TERMINAL_FONT_NAMES, TIMEZONE_NAMES,
};

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
fn bundled_keyboard_layout_names_round_trip_without_drift() {
    let expected = [
        KeyboardLayout::UsQwerty,
        KeyboardLayout::UkQwerty,
        KeyboardLayout::Dvorak,
    ];
    assert_eq!(KEYBOARD_LAYOUT_NAMES.len(), expected.len());
    for (name, layout) in KEYBOARD_LAYOUT_NAMES.iter().zip(expected) {
        assert_eq!(KeyboardLayout::from_name(name), Some(layout));
        assert_eq!(layout.as_str(), *name);
    }
    assert_eq!(KeyboardLayout::from_name("de-qwertz"), None);
    assert_eq!(KeyboardLayout::default(), KeyboardLayout::UsQwerty);
}

#[test]
fn bundled_terminal_font_names_keep_the_safe_default_first() {
    assert_eq!(
        TERMINAL_FONT_NAMES,
        &["unifont-mono-14.pbm", "pc-vga-16.pbm"]
    );
}

#[test]
fn bundled_timezone_names_normalize_to_the_safe_default() {
    assert_eq!(
        TIMEZONE_NAMES,
        &["UTC", "America/New_York", "Europe/London", "Asia/Tokyo"]
    );
    assert_eq!(DEFAULT_TIMEZONE_NAME, TIMEZONE_NAMES[0]);
    for name in TIMEZONE_NAMES {
        assert_eq!(normalize_timezone_name(Some(name)), *name);
    }
    assert_eq!(normalize_timezone_name(None), DEFAULT_TIMEZONE_NAME);
    assert_eq!(
        normalize_timezone_name(Some("Europe/Paris")),
        DEFAULT_TIMEZONE_NAME
    );
}

#[test]
fn parsed_preferences_expose_a_normalized_timezone() {
    let supported = Preferences::parse(b"[timezone]\niana = \"Asia/Tokyo\"\n").unwrap();
    assert_eq!(supported.normalized_timezone(), "Asia/Tokyo");

    let unsupported = Preferences::parse(b"[timezone]\niana = \"Etc/GMT+3\"\n").unwrap();
    assert_eq!(unsupported.normalized_timezone(), DEFAULT_TIMEZONE_NAME);
    assert_eq!(
        Preferences::empty().normalized_timezone(),
        DEFAULT_TIMEZONE_NAME
    );
}

#[test]
fn spawn_timezone_defaults_on_missing_malformed_or_unsupported_preferences() {
    assert_eq!(timezone_from_preferences_bytes(None), "UTC");
    assert_eq!(timezone_from_preferences_bytes(Some(b"not toml")), "UTC");
    assert_eq!(
        timezone_from_preferences_bytes(Some(b"[timezone]\niana = \"Europe/Paris\"\n")),
        "UTC"
    );
    assert_eq!(
        timezone_from_preferences_bytes(Some(b"[timezone]\niana = \"Europe/London\"\n")),
        "Europe/London"
    );
}

#[test]
fn spawn_environment_overrides_static_timezone_and_preserves_other_entries() {
    let baseline = vec![
        ("PATH".to_string(), "/bin:/usr/bin".to_string()),
        ("TZ".to_string(), "Asia/Tokyo".to_string()),
        ("HOME".to_string(), "/home/user".to_string()),
    ];
    let environment = spawn_environment_with_timezone(
        &baseline,
        Some(b"[timezone]\niana = \"America/New_York\"\n"),
    );
    assert_eq!(
        environment,
        vec![
            ("PATH".to_string(), "/bin:/usr/bin".to_string()),
            ("HOME".to_string(), "/home/user".to_string()),
            ("TZ".to_string(), "America/New_York".to_string()),
        ]
    );
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

#[test]
fn canonical_serializer_round_trips_every_field_in_stable_order() {
    let prefs = Preferences {
        theme_name: Some("dark".to_string()),
        theme_fit: Some("fill".to_string()),
        wallpaper_name: Some("green.png".to_string()),
        keyboard_layout: Some("dvorak".to_string()),
        timezone_iana: Some("Asia/Tokyo".to_string()),
        terminal_font: Some("pc-vga-16.pbm".to_string()),
    };
    let encoded = prefs.to_toml().expect("serializable preferences");
    assert!(encoded.starts_with("[theme]\n"));
    assert!(encoded.find("[theme]").unwrap() < encoded.find("[wallpaper]").unwrap());
    assert!(encoded.find("[wallpaper]").unwrap() < encoded.find("[keyboard]").unwrap());
    assert!(encoded.find("[keyboard]").unwrap() < encoded.find("[timezone]").unwrap());
    assert!(encoded.find("[timezone]").unwrap() < encoded.find("[terminal]").unwrap());
    assert_eq!(Preferences::parse(encoded.as_bytes()).unwrap(), prefs);
}

#[test]
fn serializer_omits_empty_sections() {
    let prefs = Preferences {
        timezone_iana: Some("UTC".to_string()),
        ..Preferences::empty()
    };
    assert_eq!(prefs.to_toml().unwrap(), "[timezone]\niana = \"UTC\"\n");
}

#[test]
fn serializer_rejects_values_the_parser_cannot_round_trip() {
    let mut prefs = Preferences::empty();
    prefs.theme_name = Some("bad\"theme".to_string());
    assert_eq!(
        prefs.to_toml(),
        Err(PreferencesError::InvalidValue("theme.name"))
    );

    prefs.theme_name = Some("bad\rtheme".to_string());
    assert_eq!(
        prefs.to_toml(),
        Err(PreferencesError::InvalidValue("theme.name"))
    );
}
