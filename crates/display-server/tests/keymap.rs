//! Isolation tests for the keymap loader.
//!
//! Exercises both the happy path (parse + look-up) and the error
//! paths (truncated input, wrong magic, unsupported version).

use display_server::protocol::keymap::{
    load_default, parse, KeymapError, Scancode, MAGIC, SUPPORTED_VERSION,
};

// ---- Happy-path tests --------------------------------------------------

#[test]
fn parses_default_keymap_without_error() {
    let km = load_default();
    assert!(!km.is_empty());
}

#[test]
fn default_keymap_contains_letter_a_on_keya_scancode() {
    let km = load_default();
    let entry = km.get(Scancode::KeyA).expect("KeyA must be present");
    assert_eq!(entry.codepoint_unshifted, 'a' as u32);
}

#[test]
fn default_keymap_shifted_a_is_upper_case() {
    let km = load_default();
    let entry = km.get(Scancode::KeyA).expect("KeyA must be present");
    assert_eq!(entry.codepoint_shifted, 'A' as u32);
}

#[test]
fn default_keymap_contains_digit_1_on_digit1_scancode() {
    let km = load_default();
    let entry = km.get(Scancode::Digit1).expect("Digit1 must be present");
    assert_eq!(entry.codepoint_unshifted, '1' as u32);
}

#[test]
fn default_keymap_shifted_of_digit_1_is_exclamation_mark() {
    let km = load_default();
    let entry = km.get(Scancode::Digit1).expect("Digit1 must be present");
    assert_eq!(entry.codepoint_shifted, '!' as u32);
}

#[test]
fn default_keymap_space_has_space_codepoint() {
    let km = load_default();
    let entry = km.get(Scancode::Space).expect("Space must be present");
    assert_eq!(entry.codepoint_unshifted, ' ' as u32);
}

#[test]
fn default_keymap_enter_has_carriage_return() {
    let km = load_default();
    let entry = km.get(Scancode::Enter).expect("Enter must be present");
    assert_eq!(entry.codepoint_unshifted, '\r' as u32);
}

#[test]
fn default_keymap_all_entries_have_zero_modifier_mask() {
    // v1 spec: modifier_mask is always 0 in the default keymap.
    let km = load_default();
    for sc in [
        Scancode::KeyA,
        Scancode::Digit0,
        Scancode::Space,
        Scancode::ShiftLeft,
    ] {
        if let Some(entry) = km.get(sc) {
            assert_eq!(
                entry.modifier_mask, 0,
                "expected modifier_mask == 0 for {sc:?}"
            );
        }
    }
}

#[test]
fn default_keymap_modifier_keys_have_zero_codepoints() {
    let km = load_default();
    for sc in [
        Scancode::ShiftLeft,
        Scancode::ShiftRight,
        Scancode::ControlLeft,
        Scancode::ControlRight,
    ] {
        let entry = km.get(sc).unwrap_or_else(|| panic!("{sc:?} must be present"));
        assert_eq!(
            entry.codepoint_unshifted, 0,
            "{sc:?} unshifted should be 0"
        );
        assert_eq!(entry.codepoint_shifted, 0, "{sc:?} shifted should be 0");
    }
}

#[test]
fn roundtrip_manually_constructed_keymap() {
    // Build a minimal 1-entry keymap by hand and parse it.
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&SUPPORTED_VERSION.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // 1 entry
    bytes.push(0x04); // KeyA
    bytes.extend_from_slice(&('a' as u32).to_le_bytes());
    bytes.extend_from_slice(&('A' as u32).to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    let km = parse(&bytes).expect("parse should succeed");
    let entry = km.get(Scancode::KeyA).unwrap();
    assert_eq!(entry.codepoint_unshifted, 'a' as u32);
    assert_eq!(entry.codepoint_shifted, 'A' as u32);
    assert_eq!(km.len(), 1);
}

// ---- Error-path tests --------------------------------------------------

#[test]
fn keymap_parse_rejects_wrong_magic() {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"WXYZ"); // wrong magic
    bytes.extend_from_slice(&SUPPORTED_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    assert_eq!(parse(&bytes).unwrap_err(), KeymapError::WrongMagic);
}

#[test]
fn keymap_parse_rejects_empty_input() {
    assert_eq!(parse(&[]).unwrap_err(), KeymapError::WrongMagic);
}

#[test]
fn keymap_parse_rejects_truncated_input() {
    // Only the magic, no version bytes.
    assert_eq!(parse(b"PMKM").unwrap_err(), KeymapError::Truncated);
}

#[test]
fn keymap_parse_rejects_truncated_after_count() {
    // Magic + version + count declaring 2 entries, but no entry data at all.
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&SUPPORTED_VERSION.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes()); // claims 2 entries
    // no entry data
    assert_eq!(parse(&bytes).unwrap_err(), KeymapError::Truncated);
}

#[test]
fn keymap_parse_rejects_unknown_version() {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&99u16.to_le_bytes()); // unsupported version
    bytes.extend_from_slice(&0u16.to_le_bytes());
    assert_eq!(
        parse(&bytes).unwrap_err(),
        KeymapError::UnsupportedVersion
    );
}

#[test]
fn keymap_parse_rejects_unknown_scancode() {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&SUPPORTED_VERSION.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // 1 entry
    bytes.push(0xFF); // unknown scancode
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    assert_eq!(
        parse(&bytes).unwrap_err(),
        KeymapError::UnknownScancode(0xFF)
    );
}

#[test]
fn keymap_parse_zero_entries_is_ok() {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&SUPPORTED_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes()); // 0 entries
    let km = parse(&bytes).expect("empty keymap is valid");
    assert!(km.is_empty());
}
