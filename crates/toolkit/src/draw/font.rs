//! Minimal 5x7 bitmap font for printable ASCII.
//!
//! Each glyph is 5 pixels wide × 7 pixels tall, stored as
//! seven row bytes where bit 4 is the leftmost column and
//! bit 0 is the rightmost column. Cells are laid out at
//! [`CELL_WIDTH`] × [`CELL_HEIGHT`] so glyphs get a
//! 1-pixel right/bottom margin for legibility.
//!
//! The glyph set covers the printable ASCII range
//! `0x20..=0x7E` (95 characters). Any character outside
//! that range, or a codepoint whose glyph hasn't been
//! filled in yet, falls back to [`UNKNOWN_GLYPH`] — a
//! hollow square that is visually obvious without being
//! all pixels (which would swamp the layout).
//!
//! The font is hand-crafted so there are no build-time
//! dependencies and nothing needs to be extracted from an
//! external file. The shapes are crude but legible at
//! 1:1 and make the end-to-end pixel pipeline easy to
//! eyeball.

/// Width of the rendered glyph in pixels (not counting
/// right padding).
pub const GLYPH_WIDTH: u32 = 5;

/// Height of the rendered glyph in pixels (not counting
/// bottom padding).
pub const GLYPH_HEIGHT: u32 = 7;

/// Width of a cell (glyph + right padding).
pub const CELL_WIDTH: u32 = 6;

/// Height of a cell (glyph + bottom padding).
pub const CELL_HEIGHT: u32 = 8;

/// First codepoint the font covers.
pub const FIRST_CHAR: u32 = 0x20;
/// Last codepoint the font covers (inclusive).
pub const LAST_CHAR: u32 = 0x7E;

/// Number of glyphs in [`FONT_DATA`].
pub const GLYPH_COUNT: usize = (LAST_CHAR - FIRST_CHAR + 1) as usize;

/// A 5x7 glyph: 7 rows of 5-bit column masks. Bit 4 is
/// the leftmost column; bit 0 is the rightmost.
pub type Glyph = [u8; GLYPH_HEIGHT as usize];

/// Fallback glyph for any character the font doesn't
/// cover: a hollow 5×7 rectangle.
pub const UNKNOWN_GLYPH: Glyph = [
    0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111,
];

/// True iff the `(col, row)` pixel in `glyph` is set.
/// `col` is 0-indexed from the left, `row` is 0-indexed
/// from the top. Out-of-range indices return `false`.
pub fn glyph_pixel(glyph: &Glyph, col: u32, row: u32) -> bool {
    if col >= GLYPH_WIDTH || row >= GLYPH_HEIGHT {
        return false;
    }
    let row_bits = glyph[row as usize];
    let shift = GLYPH_WIDTH - 1 - col;
    (row_bits >> shift) & 1 != 0
}

/// Look up the glyph for a character. Returns
/// [`UNKNOWN_GLYPH`] for anything outside the font's
/// covered range OR for codepoints whose glyph hasn't
/// been hand-coded yet (zero rows would render as
/// blank — which isn't distinguishable from `' '` — so
/// zero rows are treated as "unmapped" and promoted to
/// the fallback).
pub fn glyph_for(c: char) -> &'static Glyph {
    let code = c as u32;
    if code == 0x20 {
        // Space is legitimately blank.
        return &FONT_DATA[0];
    }
    if !(FIRST_CHAR..=LAST_CHAR).contains(&code) {
        return &UNKNOWN_GLYPH;
    }
    let idx = (code - FIRST_CHAR) as usize;
    let glyph = &FONT_DATA[idx];
    if glyph.iter().all(|&r| r == 0) {
        return &UNKNOWN_GLYPH;
    }
    glyph
}

/// The glyph table, indexed by `(codepoint - FIRST_CHAR)`.
/// Glyphs left as all-zero fall back to [`UNKNOWN_GLYPH`]
/// in [`glyph_for`] so the font is easy to extend one
/// character at a time.
pub const FONT_DATA: [Glyph; GLYPH_COUNT] = {
    let mut table = [[0u8; GLYPH_HEIGHT as usize]; GLYPH_COUNT];
    // 0x20 ' ' — intentionally all zeros.
    // 0x21 '!'
    table[0x21 - 0x20] = [
        0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00000, 0b00100,
    ];
    // 0x22 '"'
    table[0x22 - 0x20] = [
        0b01010, 0b01010, 0b01010, 0b00000, 0b00000, 0b00000, 0b00000,
    ];
    // 0x23 '#'
    table[0x23 - 0x20] = [
        0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010,
    ];
    // 0x27 '\''
    table[0x27 - 0x20] = [
        0b00100, 0b00100, 0b00100, 0b00000, 0b00000, 0b00000, 0b00000,
    ];
    // 0x28 '('
    table[0x28 - 0x20] = [
        0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010,
    ];
    // 0x29 ')'
    table[0x29 - 0x20] = [
        0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000,
    ];
    // 0x2A '*'
    table[0x2A - 0x20] = [
        0b00000, 0b01010, 0b00100, 0b11111, 0b00100, 0b01010, 0b00000,
    ];
    // 0x2B '+'
    table[0x2B - 0x20] = [
        0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000,
    ];
    // 0x2C ','
    table[0x2C - 0x20] = [
        0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00100, 0b01000,
    ];
    // 0x2D '-'
    table[0x2D - 0x20] = [
        0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
    ];
    // 0x2E '.'
    table[0x2E - 0x20] = [
        0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00100,
    ];
    // 0x2F '/'
    table[0x2F - 0x20] = [
        0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
    ];
    // 0x30 '0'
    table[0x30 - 0x20] = [
        0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
    ];
    // 0x31 '1'
    table[0x31 - 0x20] = [
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ];
    // 0x32 '2'
    table[0x32 - 0x20] = [
        0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
    ];
    // 0x33 '3'
    table[0x33 - 0x20] = [
        0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
    ];
    // 0x34 '4'
    table[0x34 - 0x20] = [
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ];
    // 0x35 '5'
    table[0x35 - 0x20] = [
        0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
    ];
    // 0x36 '6'
    table[0x36 - 0x20] = [
        0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ];
    // 0x37 '7'
    table[0x37 - 0x20] = [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ];
    // 0x38 '8'
    table[0x38 - 0x20] = [
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ];
    // 0x39 '9'
    table[0x39 - 0x20] = [
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
    ];
    // 0x3A ':'
    table[0x3A - 0x20] = [
        0b00000, 0b00100, 0b00000, 0b00000, 0b00000, 0b00100, 0b00000,
    ];
    // 0x3B ';'
    table[0x3B - 0x20] = [
        0b00000, 0b00100, 0b00000, 0b00000, 0b00100, 0b00100, 0b01000,
    ];
    // 0x3C '<'
    table[0x3C - 0x20] = [
        0b00010, 0b00100, 0b01000, 0b10000, 0b01000, 0b00100, 0b00010,
    ];
    // 0x3D '='
    table[0x3D - 0x20] = [
        0b00000, 0b00000, 0b11111, 0b00000, 0b11111, 0b00000, 0b00000,
    ];
    // 0x3E '>'
    table[0x3E - 0x20] = [
        0b01000, 0b00100, 0b00010, 0b00001, 0b00010, 0b00100, 0b01000,
    ];
    // 0x3F '?'
    table[0x3F - 0x20] = [
        0b01110, 0b10001, 0b00010, 0b00100, 0b00100, 0b00000, 0b00100,
    ];
    // 0x41 'A'
    table[0x41 - 0x20] = [
        0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
    ];
    // 0x42 'B'
    table[0x42 - 0x20] = [
        0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
    ];
    // 0x43 'C'
    table[0x43 - 0x20] = [
        0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
    ];
    // 0x44 'D'
    table[0x44 - 0x20] = [
        0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
    ];
    // 0x45 'E'
    table[0x45 - 0x20] = [
        0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
    ];
    // 0x46 'F'
    table[0x46 - 0x20] = [
        0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
    ];
    // 0x47 'G'
    table[0x47 - 0x20] = [
        0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
    ];
    // 0x48 'H'
    table[0x48 - 0x20] = [
        0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
    ];
    // 0x49 'I'
    table[0x49 - 0x20] = [
        0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ];
    // 0x4A 'J'
    table[0x4A - 0x20] = [
        0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100,
    ];
    // 0x4B 'K'
    table[0x4B - 0x20] = [
        0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
    ];
    // 0x4C 'L'
    table[0x4C - 0x20] = [
        0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
    ];
    // 0x4D 'M'
    table[0x4D - 0x20] = [
        0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
    ];
    // 0x4E 'N'
    table[0x4E - 0x20] = [
        0b10001, 0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001,
    ];
    // 0x4F 'O'
    table[0x4F - 0x20] = [
        0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
    ];
    // 0x50 'P'
    table[0x50 - 0x20] = [
        0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
    ];
    // 0x51 'Q'
    table[0x51 - 0x20] = [
        0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
    ];
    // 0x52 'R'
    table[0x52 - 0x20] = [
        0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
    ];
    // 0x53 'S'
    table[0x53 - 0x20] = [
        0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
    ];
    // 0x54 'T'
    table[0x54 - 0x20] = [
        0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
    ];
    // 0x55 'U'
    table[0x55 - 0x20] = [
        0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
    ];
    // 0x56 'V'
    table[0x56 - 0x20] = [
        0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
    ];
    // 0x57 'W'
    table[0x57 - 0x20] = [
        0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
    ];
    // 0x58 'X'
    table[0x58 - 0x20] = [
        0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
    ];
    // 0x59 'Y'
    table[0x59 - 0x20] = [
        0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
    ];
    // 0x5A 'Z'
    table[0x5A - 0x20] = [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
    ];
    // 0x5B '['
    table[0x5B - 0x20] = [
        0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110,
    ];
    // 0x5D ']'
    table[0x5D - 0x20] = [
        0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110,
    ];
    // 0x5F '_'
    table[0x5F - 0x20] = [
        0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111,
    ];
    // 0x61 'a'
    table[0x61 - 0x20] = [
        0b00000, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111,
    ];
    // 0x62 'b'
    table[0x62 - 0x20] = [
        0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b11110,
    ];
    // 0x63 'c'
    table[0x63 - 0x20] = [
        0b00000, 0b00000, 0b01110, 0b10001, 0b10000, 0b10001, 0b01110,
    ];
    // 0x64 'd'
    table[0x64 - 0x20] = [
        0b00001, 0b00001, 0b01101, 0b10011, 0b10001, 0b10001, 0b01111,
    ];
    // 0x65 'e'
    table[0x65 - 0x20] = [
        0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110,
    ];
    // 0x66 'f'
    table[0x66 - 0x20] = [
        0b00110, 0b01001, 0b01000, 0b11110, 0b01000, 0b01000, 0b01000,
    ];
    // 0x67 'g'
    table[0x67 - 0x20] = [
        0b00000, 0b00000, 0b01111, 0b10001, 0b01111, 0b00001, 0b01110,
    ];
    // 0x68 'h'
    table[0x68 - 0x20] = [
        0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001,
    ];
    // 0x69 'i'
    table[0x69 - 0x20] = [
        0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110,
    ];
    // 0x6A 'j'
    table[0x6A - 0x20] = [
        0b00010, 0b00000, 0b00110, 0b00010, 0b00010, 0b10010, 0b01100,
    ];
    // 0x6B 'k'
    table[0x6B - 0x20] = [
        0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010,
    ];
    // 0x6C 'l'
    table[0x6C - 0x20] = [
        0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ];
    // 0x6D 'm'
    table[0x6D - 0x20] = [
        0b00000, 0b00000, 0b11010, 0b10101, 0b10101, 0b10001, 0b10001,
    ];
    // 0x6E 'n'
    table[0x6E - 0x20] = [
        0b00000, 0b00000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001,
    ];
    // 0x6F 'o'
    table[0x6F - 0x20] = [
        0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110,
    ];
    // 0x70 'p'
    table[0x70 - 0x20] = [
        0b00000, 0b00000, 0b11110, 0b10001, 0b11110, 0b10000, 0b10000,
    ];
    // 0x71 'q'
    table[0x71 - 0x20] = [
        0b00000, 0b00000, 0b01111, 0b10001, 0b01111, 0b00001, 0b00001,
    ];
    // 0x72 'r'
    table[0x72 - 0x20] = [
        0b00000, 0b00000, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000,
    ];
    // 0x73 's'
    table[0x73 - 0x20] = [
        0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110,
    ];
    // 0x74 't'
    table[0x74 - 0x20] = [
        0b01000, 0b01000, 0b11110, 0b01000, 0b01000, 0b01001, 0b00110,
    ];
    // 0x75 'u'
    table[0x75 - 0x20] = [
        0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b10011, 0b01101,
    ];
    // 0x76 'v'
    table[0x76 - 0x20] = [
        0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
    ];
    // 0x77 'w'
    table[0x77 - 0x20] = [
        0b00000, 0b00000, 0b10001, 0b10001, 0b10101, 0b10101, 0b01010,
    ];
    // 0x78 'x'
    table[0x78 - 0x20] = [
        0b00000, 0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001,
    ];
    // 0x79 'y'
    table[0x79 - 0x20] = [
        0b00000, 0b00000, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110,
    ];
    // 0x7A 'z'
    table[0x7A - 0x20] = [
        0b00000, 0b00000, 0b11111, 0b00010, 0b00100, 0b01000, 0b11111,
    ];
    table
};
