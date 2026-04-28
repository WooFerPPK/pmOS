//! `term::keymap` tests — scancode → ASCII translation.
//!
//! Pins the per-scancode translation table for the US-QWERTY
//! default layout. Modifier-state tracking is tested via
//! [`Modifiers::update`] so a Shift-A press / release round
//! trip flips Shift on, produces 'A', then flips Shift off
//! and produces 'a'.

use term::{translate_scancode, Key, Modifiers};

const KEY_A: u32 = 0x04;
const KEY_K: u32 = 0x0E;
const KEY_Z: u32 = 0x1D;
const DIGIT_1: u32 = 0x1E;
const DIGIT_5: u32 = 0x22;
const DIGIT_0: u32 = 0x27;
const ENTER: u32 = 0x28;
const BACKSPACE: u32 = 0x2A;
const TAB: u32 = 0x2B;
const SPACE: u32 = 0x2C;
const MINUS: u32 = 0x2D;
const EQUAL: u32 = 0x2E;
const BRACKET_LEFT: u32 = 0x2F;
const BRACKET_RIGHT: u32 = 0x30;
const BACKSLASH: u32 = 0x31;
const SEMICOLON: u32 = 0x33;
const QUOTE: u32 = 0x34;
const BACKQUOTE: u32 = 0x35;
const COMMA: u32 = 0x36;
const PERIOD: u32 = 0x37;
const SLASH: u32 = 0x38;
const SHIFT_LEFT: u32 = 0xE1;
const SHIFT_RIGHT: u32 = 0xE5;
const CONTROL_LEFT: u32 = 0xE0;
const CONTROL_RIGHT: u32 = 0xE4;

#[test]
fn unshifted_letter_a_translates_to_lowercase_a() {
    assert_eq!(translate_scancode(KEY_A, Modifiers::default()), Some(Key::Char('a')));
}

#[test]
fn unshifted_letter_z_translates_to_lowercase_z() {
    assert_eq!(translate_scancode(KEY_Z, Modifiers::default()), Some(Key::Char('z')));
}

#[test]
fn shifted_letter_a_translates_to_uppercase_a() {
    let mods = Modifiers { shift: true, ..Modifiers::default() };
    assert_eq!(translate_scancode(KEY_A, mods), Some(Key::Char('A')));
}

#[test]
fn shifted_letter_k_translates_to_uppercase_k() {
    let mods = Modifiers { shift: true, ..Modifiers::default() };
    assert_eq!(translate_scancode(KEY_K, mods), Some(Key::Char('K')));
}

#[test]
fn unshifted_digit_one_translates_to_one() {
    assert_eq!(translate_scancode(DIGIT_1, Modifiers::default()), Some(Key::Char('1')));
}

#[test]
fn shifted_digit_one_translates_to_bang() {
    let mods = Modifiers { shift: true, ..Modifiers::default() };
    assert_eq!(translate_scancode(DIGIT_1, mods), Some(Key::Char('!')));
}

#[test]
fn shifted_digit_five_translates_to_percent() {
    let mods = Modifiers { shift: true, ..Modifiers::default() };
    assert_eq!(translate_scancode(DIGIT_5, mods), Some(Key::Char('%')));
}

#[test]
fn unshifted_digit_zero_translates_to_zero() {
    assert_eq!(translate_scancode(DIGIT_0, Modifiers::default()), Some(Key::Char('0')));
}

#[test]
fn shifted_digit_zero_translates_to_close_paren() {
    let mods = Modifiers { shift: true, ..Modifiers::default() };
    assert_eq!(translate_scancode(DIGIT_0, mods), Some(Key::Char(')')));
}

#[test]
fn enter_translates_to_enter_key() {
    assert_eq!(translate_scancode(ENTER, Modifiers::default()), Some(Key::Enter));
}

#[test]
fn backspace_translates_to_backspace_key() {
    assert_eq!(translate_scancode(BACKSPACE, Modifiers::default()), Some(Key::Backspace));
}

#[test]
fn space_translates_to_space_char() {
    assert_eq!(translate_scancode(SPACE, Modifiers::default()), Some(Key::Char(' ')));
}

#[test]
fn tab_translates_to_tab_char() {
    assert_eq!(translate_scancode(TAB, Modifiers::default()), Some(Key::Char('\t')));
}

#[test]
fn shifted_minus_translates_to_underscore() {
    let mods = Modifiers { shift: true, ..Modifiers::default() };
    assert_eq!(translate_scancode(MINUS, mods), Some(Key::Char('_')));
    assert_eq!(translate_scancode(MINUS, Modifiers::default()), Some(Key::Char('-')));
}

#[test]
fn punctuation_unshifted_and_shifted_pairs() {
    let plain = Modifiers::default();
    let shifted = Modifiers { shift: true, ..Modifiers::default() };
    let cases = [
        (EQUAL, '=', '+'),
        (BRACKET_LEFT, '[', '{'),
        (BRACKET_RIGHT, ']', '}'),
        (BACKSLASH, '\\', '|'),
        (SEMICOLON, ';', ':'),
        (QUOTE, '\'', '"'),
        (BACKQUOTE, '`', '~'),
        (COMMA, ',', '<'),
        (PERIOD, '.', '>'),
        (SLASH, '/', '?'),
    ];
    for (sc, unshifted_ch, shifted_ch) in cases {
        assert_eq!(translate_scancode(sc, plain), Some(Key::Char(unshifted_ch)));
        assert_eq!(translate_scancode(sc, shifted), Some(Key::Char(shifted_ch)));
    }
}

#[test]
fn modifier_keys_translate_to_none() {
    assert_eq!(translate_scancode(SHIFT_LEFT, Modifiers::default()), None);
    assert_eq!(translate_scancode(SHIFT_RIGHT, Modifiers::default()), None);
    assert_eq!(translate_scancode(CONTROL_LEFT, Modifiers::default()), None);
    assert_eq!(translate_scancode(CONTROL_RIGHT, Modifiers::default()), None);
}

#[test]
fn unknown_scancode_translates_to_none() {
    // 0x90 is not in our table.
    assert_eq!(translate_scancode(0x90, Modifiers::default()), None);
}

#[test]
fn shift_press_then_release_round_trips_state() {
    let mut mods = Modifiers::default();
    let was_modifier = mods.update(SHIFT_LEFT, true);
    assert!(was_modifier, "shift_left press must be flagged as a modifier");
    assert!(mods.shift, "shift state must be set after press");

    let was_modifier = mods.update(SHIFT_LEFT, false);
    assert!(was_modifier, "shift_left release must be flagged as a modifier");
    assert!(!mods.shift, "shift state must be cleared after release");
}

#[test]
fn shift_then_letter_yields_uppercase_then_lowercase_after_release() {
    let mut mods = Modifiers::default();
    mods.update(SHIFT_LEFT, true);
    assert_eq!(translate_scancode(KEY_A, mods), Some(Key::Char('A')));
    mods.update(SHIFT_LEFT, false);
    assert_eq!(translate_scancode(KEY_A, mods), Some(Key::Char('a')));
}

#[test]
fn ctrl_press_release_tracks_state_without_clearing_shift() {
    let mut mods = Modifiers::default();
    mods.update(SHIFT_LEFT, true);
    let was_modifier = mods.update(CONTROL_LEFT, true);
    assert!(was_modifier);
    assert!(mods.shift, "shift must still be held after ctrl press");
    assert!(mods.ctrl);
    mods.update(CONTROL_LEFT, false);
    assert!(mods.shift);
    assert!(!mods.ctrl);
}

#[test]
fn non_modifier_scancode_update_returns_false_and_does_not_change_modifiers() {
    let mut mods = Modifiers { shift: true, ctrl: true };
    let was_modifier = mods.update(KEY_A, true);
    assert!(!was_modifier);
    assert!(mods.shift);
    assert!(mods.ctrl);
}
