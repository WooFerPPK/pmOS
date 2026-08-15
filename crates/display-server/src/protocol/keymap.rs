//! Keymap loader for PMos display server.
//!
//! Parses the compact PMKM binary keymap format and provides an
//! in-memory representation mapping [`Scancode`] values to Unicode
//! codepoints.  The default US-QWERTY keymap is embedded via
//! `include_bytes!` so it is available at compile time with no I/O.
//!
//! ## Binary format (version 1)
//!
//! ```text
//! offset  size  field
//!      0     4  magic: b"PMKM"
//!      4     2  version: u16 LE, currently 1
//!      6     2  entry_count: u16 LE
//!      8     N  entries (N = entry_count * 11 bytes each)
//!
//! Each entry (11 bytes):
//!      0     1  scancode: u8  (Scancode enum discriminant)
//!      1     4  codepoint_unshifted: u32 LE (UTF-32)
//!      5     4  codepoint_shifted:   u32 LE (UTF-32)
//!      9     2  modifier_mask: u16 LE (unused in v1, always 0)
//! ```

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use preferences::{KeyboardLayout, Preferences};

/// PMos keymap binary magic bytes.
pub const MAGIC: &[u8; 4] = b"PMKM";

/// Format version this module can parse.
pub const SUPPORTED_VERSION: u16 = 1;

/// Byte size of one keymap entry in the binary file.
const ENTRY_SIZE: usize = 11;

/// Errors returned by [`parse`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeymapError {
    /// The first four bytes are not `b"PMKM"`.
    WrongMagic,
    /// The version field names a version this module cannot handle.
    UnsupportedVersion,
    /// The byte slice ends before all declared entries have been read.
    Truncated,
    /// An entry carries a scancode byte with no corresponding
    /// [`Scancode`] variant.
    UnknownScancode(u8),
}

/// PMos scancode — a compact enum that names every DOM
/// `KeyboardEvent.code` value the display server needs to map.
///
/// Values are chosen to mirror USB HID usage IDs (the same space
/// that most browser implementations already translate their DOM
/// codes from), which keeps the mapping trivial.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Scancode {
    // Alphabet (USB HID 0x04–0x1D)
    KeyA = 0x04,
    KeyB = 0x05,
    KeyC = 0x06,
    KeyD = 0x07,
    KeyE = 0x08,
    KeyF = 0x09,
    KeyG = 0x0A,
    KeyH = 0x0B,
    KeyI = 0x0C,
    KeyJ = 0x0D,
    KeyK = 0x0E,
    KeyL = 0x0F,
    KeyM = 0x10,
    KeyN = 0x11,
    KeyO = 0x12,
    KeyP = 0x13,
    KeyQ = 0x14,
    KeyR = 0x15,
    KeyS = 0x16,
    KeyT = 0x17,
    KeyU = 0x18,
    KeyV = 0x19,
    KeyW = 0x1A,
    KeyX = 0x1B,
    KeyY = 0x1C,
    KeyZ = 0x1D,
    // Digits (USB HID 0x1E–0x27)
    Digit1 = 0x1E,
    Digit2 = 0x1F,
    Digit3 = 0x20,
    Digit4 = 0x21,
    Digit5 = 0x22,
    Digit6 = 0x23,
    Digit7 = 0x24,
    Digit8 = 0x25,
    Digit9 = 0x26,
    Digit0 = 0x27,
    // Control keys
    Enter = 0x28,
    Backspace = 0x2A,
    Tab = 0x2B,
    Space = 0x2C,
    // Punctuation (US-QWERTY positions)
    Minus = 0x2D,        // - / _
    Equal = 0x2E,        // = / +
    BracketLeft = 0x2F,  // [ / {
    BracketRight = 0x30, // ] / }
    Backslash = 0x31,    // \ / |
    Semicolon = 0x33,    // ; / :
    Quote = 0x34,        // ' / "
    Backquote = 0x35,    // ` / ~
    Comma = 0x36,        // , / <
    Period = 0x37,       // . / >
    Slash = 0x38,        // / / ?
    // Modifier keys (produce no codepoint; codepoints are 0)
    ShiftLeft = 0xE1,
    ShiftRight = 0xE5,
    ControlLeft = 0xE0,
    ControlRight = 0xE4,
}

impl TryFrom<u8> for Scancode {
    type Error = KeymapError;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            0x04 => Ok(Scancode::KeyA),
            0x05 => Ok(Scancode::KeyB),
            0x06 => Ok(Scancode::KeyC),
            0x07 => Ok(Scancode::KeyD),
            0x08 => Ok(Scancode::KeyE),
            0x09 => Ok(Scancode::KeyF),
            0x0A => Ok(Scancode::KeyG),
            0x0B => Ok(Scancode::KeyH),
            0x0C => Ok(Scancode::KeyI),
            0x0D => Ok(Scancode::KeyJ),
            0x0E => Ok(Scancode::KeyK),
            0x0F => Ok(Scancode::KeyL),
            0x10 => Ok(Scancode::KeyM),
            0x11 => Ok(Scancode::KeyN),
            0x12 => Ok(Scancode::KeyO),
            0x13 => Ok(Scancode::KeyP),
            0x14 => Ok(Scancode::KeyQ),
            0x15 => Ok(Scancode::KeyR),
            0x16 => Ok(Scancode::KeyS),
            0x17 => Ok(Scancode::KeyT),
            0x18 => Ok(Scancode::KeyU),
            0x19 => Ok(Scancode::KeyV),
            0x1A => Ok(Scancode::KeyW),
            0x1B => Ok(Scancode::KeyX),
            0x1C => Ok(Scancode::KeyY),
            0x1D => Ok(Scancode::KeyZ),
            0x1E => Ok(Scancode::Digit1),
            0x1F => Ok(Scancode::Digit2),
            0x20 => Ok(Scancode::Digit3),
            0x21 => Ok(Scancode::Digit4),
            0x22 => Ok(Scancode::Digit5),
            0x23 => Ok(Scancode::Digit6),
            0x24 => Ok(Scancode::Digit7),
            0x25 => Ok(Scancode::Digit8),
            0x26 => Ok(Scancode::Digit9),
            0x27 => Ok(Scancode::Digit0),
            0x28 => Ok(Scancode::Enter),
            0x2A => Ok(Scancode::Backspace),
            0x2B => Ok(Scancode::Tab),
            0x2C => Ok(Scancode::Space),
            0x2D => Ok(Scancode::Minus),
            0x2E => Ok(Scancode::Equal),
            0x2F => Ok(Scancode::BracketLeft),
            0x30 => Ok(Scancode::BracketRight),
            0x31 => Ok(Scancode::Backslash),
            0x33 => Ok(Scancode::Semicolon),
            0x34 => Ok(Scancode::Quote),
            0x35 => Ok(Scancode::Backquote),
            0x36 => Ok(Scancode::Comma),
            0x37 => Ok(Scancode::Period),
            0x38 => Ok(Scancode::Slash),
            0xE0 => Ok(Scancode::ControlLeft),
            0xE1 => Ok(Scancode::ShiftLeft),
            0xE4 => Ok(Scancode::ControlRight),
            0xE5 => Ok(Scancode::ShiftRight),
            other => Err(KeymapError::UnknownScancode(other)),
        }
    }
}

/// The codepoints associated with one physical key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeymapEntry {
    /// UTF-32 codepoint produced when no shift modifier is active.
    /// Zero for keys that produce no character (e.g. modifier keys).
    pub codepoint_unshifted: u32,
    /// UTF-32 codepoint produced when Shift is held.
    pub codepoint_shifted: u32,
    /// Modifier mask hint (v1: always 0; reserved for future layouts).
    pub modifier_mask: u16,
}

/// An in-memory keymap: a map from [`Scancode`] to [`KeymapEntry`].
#[derive(Debug)]
pub struct Keymap {
    entries: BTreeMap<Scancode, KeymapEntry>,
    unshifted_scancodes: BTreeMap<u32, Scancode>,
    shifted_scancodes: BTreeMap<u32, Scancode>,
}

impl Keymap {
    /// Look up the entry for `scancode`, returning `None` if the
    /// keymap contains no mapping for it.
    pub fn get(&self, scancode: Scancode) -> Option<&KeymapEntry> {
        self.entries.get(&scancode)
    }

    /// Number of entries in the keymap.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the keymap has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Translate a physical scancode through this layout into the equivalent
    /// scancode in `logical_layout` for clients that consume the v1 logical
    /// HID key field. Non-printing and unsupported keys remain unchanged.
    pub fn map_to_logical_scancode(
        &self,
        physical: u32,
        shifted: bool,
        logical_layout: &Keymap,
    ) -> u32 {
        let Ok(raw) = u8::try_from(physical) else {
            return physical;
        };
        let Ok(scancode) = Scancode::try_from(raw) else {
            return physical;
        };
        let Some(entry) = self.get(scancode) else {
            return physical;
        };
        let codepoint = if shifted {
            entry.codepoint_shifted
        } else {
            entry.codepoint_unshifted
        };
        if codepoint == 0 {
            return physical;
        }

        logical_layout
            .scancode_for_codepoint(codepoint, shifted)
            .map(|mapped| mapped as u8 as u32)
            .unwrap_or(physical)
    }

    fn scancode_for_codepoint(&self, codepoint: u32, shifted: bool) -> Option<Scancode> {
        if shifted {
            self.shifted_scancodes.get(&codepoint).copied()
        } else {
            self.unshifted_scancodes.get(&codepoint).copied()
        }
    }
}

/// Parse a PMKM v1 binary keymap from raw bytes.
///
/// On success returns a [`Keymap`]; on failure returns the first
/// [`KeymapError`] encountered.
pub fn parse(bytes: &[u8]) -> Result<Keymap, KeymapError> {
    // Magic (4 bytes)
    if bytes.get(0..4) != Some(MAGIC.as_slice()) {
        return Err(KeymapError::WrongMagic);
    }
    // Version (2 bytes LE)
    let version = u16::from_le_bytes(
        bytes
            .get(4..6)
            .ok_or(KeymapError::Truncated)?
            .try_into()
            .unwrap(),
    );
    if version != SUPPORTED_VERSION {
        return Err(KeymapError::UnsupportedVersion);
    }
    // Entry count (2 bytes LE)
    let count = u16::from_le_bytes(
        bytes
            .get(6..8)
            .ok_or(KeymapError::Truncated)?
            .try_into()
            .unwrap(),
    ) as usize;

    let header_size = 8;
    let required = header_size + count * ENTRY_SIZE;
    if bytes.len() < required {
        return Err(KeymapError::Truncated);
    }

    let mut entries = BTreeMap::new();
    let mut unshifted_scancodes = BTreeMap::new();
    let mut shifted_scancodes = BTreeMap::new();
    for i in 0..count {
        let base = header_size + i * ENTRY_SIZE;
        let entry_bytes = &bytes[base..base + ENTRY_SIZE];
        let scancode = Scancode::try_from(entry_bytes[0])?;
        let cp_unshifted = u32::from_le_bytes(entry_bytes[1..5].try_into().unwrap());
        let cp_shifted = u32::from_le_bytes(entry_bytes[5..9].try_into().unwrap());
        let modifier_mask = u16::from_le_bytes(entry_bytes[9..11].try_into().unwrap());
        entries.insert(
            scancode,
            KeymapEntry {
                codepoint_unshifted: cp_unshifted,
                codepoint_shifted: cp_shifted,
                modifier_mask,
            },
        );
        if cp_unshifted != 0 {
            unshifted_scancodes.entry(cp_unshifted).or_insert(scancode);
        }
        if cp_shifted != 0 {
            shifted_scancodes.entry(cp_shifted).or_insert(scancode);
        }
    }

    Ok(Keymap {
        entries,
        unshifted_scancodes,
        shifted_scancodes,
    })
}

/// Bundled keymaps embedded at compile time. Selection comes from the
/// canonical VFS preference; no runtime network or browser fetch is involved.
static US_QWERTY_BYTES: &[u8] = include_bytes!("../../assets/keymaps/us-qwerty.bin");
static UK_QWERTY_BYTES: &[u8] = include_bytes!("../../assets/keymaps/uk-qwerty.bin");
static DVORAK_BYTES: &[u8] = include_bytes!("../../assets/keymaps/dvorak.bin");

/// Parse one of the three keymaps bundled with PMos v1.
pub fn load_bundled(layout: KeyboardLayout) -> Result<Keymap, KeymapError> {
    let bytes = match layout {
        KeyboardLayout::UsQwerty => US_QWERTY_BYTES,
        KeyboardLayout::UkQwerty => UK_QWERTY_BYTES,
        KeyboardLayout::Dvorak => DVORAK_BYTES,
    };
    parse(bytes)
}

/// Return the default US-QWERTY keymap.
///
/// Panics only if the embedded asset is corrupt — which is caught
/// by the compile-time `include_bytes!` + the keymap isolation
/// tests that run in CI.
pub fn load_default() -> Keymap {
    load_bundled(KeyboardLayout::UsQwerty).expect("embedded us-qwerty keymap is valid")
}

/// Upper bound between VFS reads while the display loop is scheduled.
pub const PREFERENCE_POLL_INTERVAL_MS: u64 = 100;

/// Avoid a clock syscall on every empty display-server turn.
pub const PREFERENCE_CLOCK_CHECK_EVERY_ITERATIONS: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeymapPreferenceReadError {
    Unavailable,
}

/// Read seam shared by the production VFS source and deterministic tests.
pub trait KeymapPreferenceSource {
    /// `Ok(None)` means the canonical preference file does not exist.
    fn read(&mut self) -> Result<Option<Vec<u8>>, KeymapPreferenceReadError>;
}

/// Monotonic-clock seam for the bounded production poller.
pub trait KeymapPreferenceClock {
    fn monotonic_ms(&mut self) -> u64;
}

/// Last-known-good keyboard selection backed by `/etc/preferences.toml`.
pub struct KeymapPreferenceMonitor<S> {
    source: S,
    current: KeyboardLayout,
}

impl<S: KeymapPreferenceSource> KeymapPreferenceMonitor<S> {
    pub fn new(mut source: S) -> Self {
        let current = read_preferred_layout(&mut source).unwrap_or_default();
        Self { source, current }
    }

    pub const fn current(&self) -> KeyboardLayout {
        self.current
    }

    /// Reload once, preserving the current layout on malformed content,
    /// unsupported names, transient I/O, or an invalid bundled asset.
    pub fn poll(&mut self) -> bool {
        let Some(next) = read_preferred_layout(&mut self.source) else {
            return false;
        };
        if next == self.current {
            return false;
        }
        self.current = next;
        true
    }
}

fn read_preferred_layout(source: &mut impl KeymapPreferenceSource) -> Option<KeyboardLayout> {
    let layout = match source.read() {
        Ok(Some(bytes)) => {
            let preferences = Preferences::parse(&bytes).ok()?;
            match preferences.keyboard_layout.as_deref() {
                Some(name) => KeyboardLayout::from_name(name)?,
                None => KeyboardLayout::default(),
            }
        }
        Ok(None) => KeyboardLayout::default(),
        Err(_) => return None,
    };

    // Validate the selected embedded bytes before publishing the selection.
    load_bundled(layout).ok()?;
    Some(layout)
}

/// Preference monitor whose clock-gated path is retained for native fixtures.
/// Production calls [`Self::refresh`] after filesystem-watch readiness.
pub struct KeymapPreferenceRuntime<S, C> {
    monitor: KeymapPreferenceMonitor<S>,
    clock: C,
    last_poll_ms: u64,
    clock_check_every_iterations: u32,
    iterations_until_clock_check: u32,
}

impl<S: KeymapPreferenceSource, C: KeymapPreferenceClock> KeymapPreferenceRuntime<S, C> {
    pub fn new(source: S, mut clock: C) -> Self {
        let monitor = KeymapPreferenceMonitor::new(source);
        let last_poll_ms = clock.monotonic_ms();
        Self {
            monitor,
            clock,
            last_poll_ms,
            clock_check_every_iterations: PREFERENCE_CLOCK_CHECK_EVERY_ITERATIONS,
            iterations_until_clock_check: 0,
        }
    }

    pub const fn current(&self) -> KeyboardLayout {
        self.monitor.current()
    }

    /// Override the cheap iteration gate for deterministic isolation tests.
    pub fn with_clock_check_every_iterations(mut self, iterations: u32) -> Self {
        self.clock_check_every_iterations = iterations.max(1);
        self.iterations_until_clock_check = 0;
        self
    }

    /// Poll only when the 100 ms boundary is due. Errors consume the current
    /// poll slot, bounding repeated VFS failures to ten reads per second.
    pub fn poll(&mut self) -> bool {
        if self.iterations_until_clock_check > 0 {
            self.iterations_until_clock_check -= 1;
            return false;
        }
        self.iterations_until_clock_check = self.clock_check_every_iterations - 1;

        let now = self.clock.monotonic_ms();
        if now.saturating_sub(self.last_poll_ms) < PREFERENCE_POLL_INTERVAL_MS {
            return false;
        }
        self.last_poll_ms = now;
        self.monitor.poll()
    }

    /// Re-read immediately after the stable-parent/current-inode filesystem
    /// watch reports a change. Production uses this event-driven path; the
    /// bounded clock poll remains only for injected deterministic fixtures.
    pub fn refresh(&mut self) -> bool {
        self.last_poll_ms = self.clock.monotonic_ms();
        self.monitor.poll()
    }
}
