//! Scancode → character translation for the term app.
//!
//! The display server emits raw `pmd_keyboard.key(surface_id,
//! key, state)` events where `key` is the USB-HID-style
//! scancode produced by `display_server::Scancode`. The term
//! has to translate those scancodes back into the printable
//! characters they represent under the active US-QWERTY
//! layout, while honouring the Shift modifier.
//!
//! v1 wires only the US-QWERTY default layout. Layout
//! switching lands when the keymap-admin syscall + the layout
//! file format work in T-2xx.

use crate::terminal::Key;

/// USB HID scancodes the display server may emit. Mirrors
/// `display_server::Scancode` but kept as a private detail
/// here to avoid pulling the display-server crate into term's
/// runtime dependency graph (term ships in user space; the
/// display server runs in its own process).
mod sc {
    pub const KEY_A: u32 = 0x04;
    pub const KEY_Z: u32 = 0x1D;
    pub const DIGIT_1: u32 = 0x1E;
    pub const DIGIT_0: u32 = 0x27;
    pub const ENTER: u32 = 0x28;
    pub const BACKSPACE: u32 = 0x2A;
    pub const TAB: u32 = 0x2B;
    pub const SPACE: u32 = 0x2C;
    pub const MINUS: u32 = 0x2D;
    pub const EQUAL: u32 = 0x2E;
    pub const BRACKET_LEFT: u32 = 0x2F;
    pub const BRACKET_RIGHT: u32 = 0x30;
    pub const BACKSLASH: u32 = 0x31;
    pub const SEMICOLON: u32 = 0x33;
    pub const QUOTE: u32 = 0x34;
    pub const BACKQUOTE: u32 = 0x35;
    pub const COMMA: u32 = 0x36;
    pub const PERIOD: u32 = 0x37;
    pub const SLASH: u32 = 0x38;
    pub const SHIFT_LEFT: u32 = 0xE1;
    pub const SHIFT_RIGHT: u32 = 0xE5;
    pub const CONTROL_LEFT: u32 = 0xE0;
    pub const CONTROL_RIGHT: u32 = 0xE4;
}

/// Modifier state tracked across press / release events. The
/// term holds one of these alongside its [`Terminal`] and
/// updates it on every keyboard event before consulting the
/// keymap.
#[derive(Default, Debug, Clone, Copy)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
}

impl Modifiers {
    /// Update the modifier state in response to a keyboard
    /// event. Returns true iff the scancode is a modifier (so
    /// the caller knows not to also treat it as a printable
    /// keystroke).
    pub fn update(&mut self, scancode: u32, pressed: bool) -> bool {
        match scancode {
            sc::SHIFT_LEFT | sc::SHIFT_RIGHT => {
                self.shift = pressed;
                true
            }
            sc::CONTROL_LEFT | sc::CONTROL_RIGHT => {
                self.ctrl = pressed;
                true
            }
            _ => false,
        }
    }
}

/// Translate a keyboard press event (`scancode`, `mods`) into
/// a [`Key`] suitable for [`Terminal::feed_key`]. Returns
/// `None` when the scancode produces no terminal-visible
/// keystroke (for instance: modifier-key transitions, which
/// the caller handles via [`Modifiers::update`]).
///
/// This is the **press** path only — release events should be
/// fed through [`Modifiers::update`] but never produce a
/// `Key`. Repeating presses, key-repeat timers, and IME
/// composition are out of scope for v1.
pub fn translate(scancode: u32, mods: Modifiers) -> Option<Key> {
    match scancode {
        sc::ENTER => Some(Key::Enter),
        sc::BACKSPACE => Some(Key::Backspace),
        sc::TAB => Some(Key::Char('\t')),
        sc::SPACE => Some(Key::Char(' ')),
        sc::SHIFT_LEFT | sc::SHIFT_RIGHT | sc::CONTROL_LEFT | sc::CONTROL_RIGHT => None,
        c if (sc::KEY_A..=sc::KEY_Z).contains(&c) => {
            // 'a' = 0x04 base. Shift uppercases.
            let base = b'a' + (c - sc::KEY_A) as u8;
            let ch = if mods.shift {
                (base as char).to_ascii_uppercase()
            } else {
                base as char
            };
            Some(Key::Char(ch))
        }
        c if (sc::DIGIT_1..=sc::DIGIT_0).contains(&c) => {
            // Digits are 1..9 then 0 in scancode order; their
            // shifted forms are the symbols above the number
            // row on a US keyboard.
            let unshifted = match c {
                sc::DIGIT_1 => '1',
                0x1F => '2',
                0x20 => '3',
                0x21 => '4',
                0x22 => '5',
                0x23 => '6',
                0x24 => '7',
                0x25 => '8',
                0x26 => '9',
                sc::DIGIT_0 => '0',
                _ => return None,
            };
            let shifted = match unshifted {
                '1' => '!',
                '2' => '@',
                '3' => '#',
                '4' => '$',
                '5' => '%',
                '6' => '^',
                '7' => '&',
                '8' => '*',
                '9' => '(',
                '0' => ')',
                _ => unshifted,
            };
            Some(Key::Char(if mods.shift { shifted } else { unshifted }))
        }
        sc::MINUS => Some(Key::Char(if mods.shift { '_' } else { '-' })),
        sc::EQUAL => Some(Key::Char(if mods.shift { '+' } else { '=' })),
        sc::BRACKET_LEFT => Some(Key::Char(if mods.shift { '{' } else { '[' })),
        sc::BRACKET_RIGHT => Some(Key::Char(if mods.shift { '}' } else { ']' })),
        sc::BACKSLASH => Some(Key::Char(if mods.shift { '|' } else { '\\' })),
        sc::SEMICOLON => Some(Key::Char(if mods.shift { ':' } else { ';' })),
        sc::QUOTE => Some(Key::Char(if mods.shift { '"' } else { '\'' })),
        sc::BACKQUOTE => Some(Key::Char(if mods.shift { '~' } else { '`' })),
        sc::COMMA => Some(Key::Char(if mods.shift { '<' } else { ',' })),
        sc::PERIOD => Some(Key::Char(if mods.shift { '>' } else { '.' })),
        sc::SLASH => Some(Key::Char(if mods.shift { '?' } else { '/' })),
        _ => None,
    }
}
