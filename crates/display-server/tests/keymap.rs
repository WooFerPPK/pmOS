//! Isolation tests for the keymap loader.
//!
//! Exercises both the happy path (parse + look-up) and the error
//! paths (truncated input, wrong magic, unsupported version).

use display_server::protocol::keymap::{
    load_bundled, load_default, parse, KeymapError, KeymapPreferenceClock, KeymapPreferenceMonitor,
    KeymapPreferenceReadError, KeymapPreferenceRuntime, KeymapPreferenceSource, Scancode, MAGIC,
    PREFERENCE_CLOCK_CHECK_EVERY_ITERATIONS, PREFERENCE_POLL_INTERVAL_MS, SUPPORTED_VERSION,
};
use preferences::KeyboardLayout;
use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;

const UK_QWERTY: &[u8] = include_bytes!("../assets/keymaps/uk-qwerty.bin");
const DVORAK: &[u8] = include_bytes!("../assets/keymaps/dvorak.bin");

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
fn bundled_layouts_are_valid_and_materially_distinct() {
    let us = load_bundled(KeyboardLayout::UsQwerty).expect("bundled US keymap is valid");
    let uk = load_bundled(KeyboardLayout::UkQwerty).expect("bundled UK keymap is valid");
    let dvorak = load_bundled(KeyboardLayout::Dvorak).expect("bundled Dvorak keymap is valid");

    assert_eq!(
        us.len(),
        parse(include_bytes!("../assets/keymaps/us-qwerty.bin"))
            .unwrap()
            .len()
    );
    assert_eq!(uk.len(), parse(UK_QWERTY).unwrap().len());
    assert_eq!(dvorak.len(), parse(DVORAK).unwrap().len());

    assert_eq!(
        us.get(Scancode::Digit2).unwrap().codepoint_shifted,
        '@' as u32
    );
    assert_eq!(
        uk.get(Scancode::Digit2).unwrap().codepoint_shifted,
        '"' as u32
    );
    assert_eq!(
        uk.get(Scancode::Digit3).unwrap().codepoint_shifted,
        '£' as u32
    );
    assert_eq!(
        dvorak.get(Scancode::KeyB).unwrap().codepoint_unshifted,
        'x' as u32
    );
    assert_eq!(
        dvorak.get(Scancode::KeyS).unwrap().codepoint_unshifted,
        'o' as u32
    );
}

#[test]
fn bundled_layouts_map_physical_keys_into_existing_client_namespace() {
    let us = load_bundled(KeyboardLayout::UsQwerty).unwrap();
    let uk = load_bundled(KeyboardLayout::UkQwerty).unwrap();
    let dvorak = load_bundled(KeyboardLayout::Dvorak).unwrap();

    assert_eq!(
        dvorak.map_to_logical_scancode(Scancode::KeyS as u8 as u32, false, &us),
        Scancode::KeyO as u8 as u32,
        "physical KeyS is logical 'o' in Dvorak",
    );
    assert_eq!(
        uk.map_to_logical_scancode(Scancode::Digit2 as u8 as u32, true, &us),
        Scancode::Quote as u8 as u32,
        "UK Shift+2 is a quote and must reach fixed-map v1 clients as such",
    );
    assert_eq!(
        dvorak.map_to_logical_scancode(Scancode::ShiftLeft as u8 as u32, false, &us),
        Scancode::ShiftLeft as u8 as u32,
        "modifier transitions are never remapped",
    );
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
        let entry = km
            .get(sc)
            .unwrap_or_else(|| panic!("{sc:?} must be present"));
        assert_eq!(entry.codepoint_unshifted, 0, "{sc:?} unshifted should be 0");
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
    assert_eq!(parse(&bytes).unwrap_err(), KeymapError::UnsupportedVersion);
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

struct SequenceSource {
    reads: Rc<Cell<usize>>,
    values: VecDeque<Result<Option<Vec<u8>>, KeymapPreferenceReadError>>,
}

impl SequenceSource {
    fn new(
        values: impl IntoIterator<Item = Result<Option<Vec<u8>>, KeymapPreferenceReadError>>,
    ) -> (Self, Rc<Cell<usize>>) {
        let reads = Rc::new(Cell::new(0));
        (
            Self {
                reads: reads.clone(),
                values: values.into_iter().collect(),
            },
            reads,
        )
    }
}

impl KeymapPreferenceSource for SequenceSource {
    fn read(&mut self) -> Result<Option<Vec<u8>>, KeymapPreferenceReadError> {
        self.reads.set(self.reads.get() + 1);
        self.values.pop_front().unwrap_or(Ok(None))
    }
}

fn preference(layout: &str) -> Result<Option<Vec<u8>>, KeymapPreferenceReadError> {
    Ok(Some(
        format!("[keyboard]\nlayout = \"{layout}\"\n").into_bytes(),
    ))
}

#[test]
fn preference_monitor_preserves_last_good_on_malformed_unknown_and_io_error() {
    let (source, reads) = SequenceSource::new([
        preference("dvorak"),
        Ok(Some(b"[keyboard\nlayout = \"uk-qwerty\"\n".to_vec())),
        Err(KeymapPreferenceReadError::Unavailable),
        preference("de-qwertz"),
        preference("uk-qwerty"),
    ]);
    let mut monitor = KeymapPreferenceMonitor::new(source);
    assert_eq!(monitor.current(), KeyboardLayout::Dvorak);

    for _ in 0..3 {
        assert!(!monitor.poll());
        assert_eq!(monitor.current(), KeyboardLayout::Dvorak);
    }
    assert!(monitor.poll());
    assert_eq!(monitor.current(), KeyboardLayout::UkQwerty);
    assert_eq!(reads.get(), 5);
}

struct SequenceClock {
    values: VecDeque<u64>,
    last: u64,
}

impl SequenceClock {
    fn new(values: impl IntoIterator<Item = u64>) -> Self {
        let values: VecDeque<_> = values.into_iter().collect();
        let last = *values.front().expect("at least one clock sample");
        Self { values, last }
    }
}

impl KeymapPreferenceClock for SequenceClock {
    fn monotonic_ms(&mut self) -> u64 {
        if let Some(next) = self.values.pop_front() {
            self.last = next;
        }
        self.last
    }
}

#[test]
fn preference_runtime_bounds_clock_and_vfs_work_while_applying_within_100_ms() {
    let (source, reads) = SequenceSource::new([preference("us-qwerty"), preference("dvorak")]);
    let clock = SequenceClock::new([0, 0, PREFERENCE_POLL_INTERVAL_MS]);
    let mut runtime = KeymapPreferenceRuntime::new(source, clock);
    assert_eq!(runtime.current(), KeyboardLayout::UsQwerty);
    assert_eq!(reads.get(), 1);

    for _ in 0..PREFERENCE_CLOCK_CHECK_EVERY_ITERATIONS {
        assert!(!runtime.poll());
    }
    assert_eq!(reads.get(), 1, "idle turns must not poll the VFS");

    assert!(runtime.poll());
    assert_eq!(runtime.current(), KeyboardLayout::Dvorak);
    assert_eq!(reads.get(), 2);
}
