// TypeScript port of the 5x7 bitmap font in
// `crates/term/src/font.rs`. Glyph data is kept byte-for-
// byte in sync with the Rust source; the two must render
// identically so the eventual WASM kernel can swap in the
// Rust rasterizer without the pixels shifting.
//
// Each glyph is 5 pixels wide × 7 pixels tall, stored as
// seven row bytes where bit 4 is the leftmost column and
// bit 0 is the rightmost. Cells are laid out at
// CELL_WIDTH × CELL_HEIGHT so glyphs get a 1-pixel right/
// bottom margin for legibility.
//
// The glyph set covers the printable ASCII range 0x20..=0x7E
// (95 characters). Any character outside that range, or a
// codepoint whose glyph hasn't been filled in, falls back
// to `UNKNOWN_GLYPH` — a hollow 5x7 rectangle that is
// visually obvious without being all pixels.

export const GLYPH_WIDTH = 5;
export const GLYPH_HEIGHT = 7;
export const CELL_WIDTH = 6;
export const CELL_HEIGHT = 8;
export const FIRST_CHAR = 0x20;
export const LAST_CHAR = 0x7e;
export const GLYPH_COUNT = LAST_CHAR - FIRST_CHAR + 1;

/** A 5x7 glyph: 7 rows of 5-bit column masks. */
export type Glyph = Uint8Array;

/** Fallback glyph used when a codepoint has no mapping. */
export const UNKNOWN_GLYPH: Glyph = new Uint8Array([
  0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111,
]);

// Flat glyph table: GLYPH_COUNT entries, each GLYPH_HEIGHT
// bytes. An all-zero slot means "not a space, not a real
// glyph" and gets promoted to UNKNOWN_GLYPH at lookup time.
const FONT_DATA: Uint8Array = new Uint8Array(GLYPH_COUNT * GLYPH_HEIGHT);

function setGlyph(code: number, rows: readonly number[]): void {
  const base = (code - FIRST_CHAR) * GLYPH_HEIGHT;
  for (let i = 0; i < GLYPH_HEIGHT; i += 1) {
    FONT_DATA[base + i] = rows[i] ?? 0;
  }
}

// 0x20 ' ' — intentionally all zeros.
// 0x21 '!'
setGlyph(0x21, [0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00000, 0b00100]);
// 0x22 '"'
setGlyph(0x22, [0b01010, 0b01010, 0b01010, 0b00000, 0b00000, 0b00000, 0b00000]);
// 0x23 '#'
setGlyph(0x23, [0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010]);
// 0x27 "'"
setGlyph(0x27, [0b00100, 0b00100, 0b00100, 0b00000, 0b00000, 0b00000, 0b00000]);
// 0x28 '('
setGlyph(0x28, [0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010]);
// 0x29 ')'
setGlyph(0x29, [0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000]);
// 0x2A '*'
setGlyph(0x2a, [0b00000, 0b01010, 0b00100, 0b11111, 0b00100, 0b01010, 0b00000]);
// 0x2B '+'
setGlyph(0x2b, [0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000]);
// 0x2C ','
setGlyph(0x2c, [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00100, 0b01000]);
// 0x2D '-'
setGlyph(0x2d, [0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000]);
// 0x2E '.'
setGlyph(0x2e, [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00100]);
// 0x2F '/'
setGlyph(0x2f, [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000]);
// 0x30 '0'
setGlyph(0x30, [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110]);
// 0x31 '1'
setGlyph(0x31, [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110]);
// 0x32 '2'
setGlyph(0x32, [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111]);
// 0x33 '3'
setGlyph(0x33, [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110]);
// 0x34 '4'
setGlyph(0x34, [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010]);
// 0x35 '5'
setGlyph(0x35, [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110]);
// 0x36 '6'
setGlyph(0x36, [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110]);
// 0x37 '7'
setGlyph(0x37, [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000]);
// 0x38 '8'
setGlyph(0x38, [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110]);
// 0x39 '9'
setGlyph(0x39, [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100]);
// 0x3A ':'
setGlyph(0x3a, [0b00000, 0b00100, 0b00000, 0b00000, 0b00000, 0b00100, 0b00000]);
// 0x3B ';'
setGlyph(0x3b, [0b00000, 0b00100, 0b00000, 0b00000, 0b00100, 0b00100, 0b01000]);
// 0x3C '<'
setGlyph(0x3c, [0b00010, 0b00100, 0b01000, 0b10000, 0b01000, 0b00100, 0b00010]);
// 0x3D '='
setGlyph(0x3d, [0b00000, 0b00000, 0b11111, 0b00000, 0b11111, 0b00000, 0b00000]);
// 0x3E '>'
setGlyph(0x3e, [0b01000, 0b00100, 0b00010, 0b00001, 0b00010, 0b00100, 0b01000]);
// 0x3F '?'
setGlyph(0x3f, [0b01110, 0b10001, 0b00010, 0b00100, 0b00100, 0b00000, 0b00100]);
// 0x41 'A'
setGlyph(0x41, [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001]);
// 0x42 'B'
setGlyph(0x42, [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110]);
// 0x43 'C'
setGlyph(0x43, [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110]);
// 0x44 'D'
setGlyph(0x44, [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110]);
// 0x45 'E'
setGlyph(0x45, [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111]);
// 0x46 'F'
setGlyph(0x46, [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000]);
// 0x47 'G'
setGlyph(0x47, [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110]);
// 0x48 'H'
setGlyph(0x48, [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001]);
// 0x49 'I'
setGlyph(0x49, [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110]);
// 0x4A 'J'
setGlyph(0x4a, [0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100]);
// 0x4B 'K'
setGlyph(0x4b, [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001]);
// 0x4C 'L'
setGlyph(0x4c, [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111]);
// 0x4D 'M'
setGlyph(0x4d, [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001]);
// 0x4E 'N'
setGlyph(0x4e, [0b10001, 0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001]);
// 0x4F 'O'
setGlyph(0x4f, [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110]);
// 0x50 'P'
setGlyph(0x50, [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000]);
// 0x51 'Q'
setGlyph(0x51, [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101]);
// 0x52 'R'
setGlyph(0x52, [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001]);
// 0x53 'S'
setGlyph(0x53, [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110]);
// 0x54 'T'
setGlyph(0x54, [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100]);
// 0x55 'U'
setGlyph(0x55, [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110]);
// 0x56 'V'
setGlyph(0x56, [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100]);
// 0x57 'W'
setGlyph(0x57, [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001]);
// 0x58 'X'
setGlyph(0x58, [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001]);
// 0x59 'Y'
setGlyph(0x59, [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100]);
// 0x5A 'Z'
setGlyph(0x5a, [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111]);
// 0x5B '['
setGlyph(0x5b, [0b01110, 0b01000, 0b01000, 0b01000, 0b01000, 0b01000, 0b01110]);
// 0x5D ']'
setGlyph(0x5d, [0b01110, 0b00010, 0b00010, 0b00010, 0b00010, 0b00010, 0b01110]);
// 0x5F '_'
setGlyph(0x5f, [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111]);
// 0x61 'a'
setGlyph(0x61, [0b00000, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111]);
// 0x62 'b'
setGlyph(0x62, [0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b11110]);
// 0x63 'c'
setGlyph(0x63, [0b00000, 0b00000, 0b01110, 0b10001, 0b10000, 0b10001, 0b01110]);
// 0x64 'd'
setGlyph(0x64, [0b00001, 0b00001, 0b01101, 0b10011, 0b10001, 0b10001, 0b01111]);
// 0x65 'e'
setGlyph(0x65, [0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110]);
// 0x66 'f'
setGlyph(0x66, [0b00110, 0b01001, 0b01000, 0b11110, 0b01000, 0b01000, 0b01000]);
// 0x67 'g'
setGlyph(0x67, [0b00000, 0b00000, 0b01111, 0b10001, 0b01111, 0b00001, 0b01110]);
// 0x68 'h'
setGlyph(0x68, [0b10000, 0b10000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001]);
// 0x69 'i'
setGlyph(0x69, [0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110]);
// 0x6A 'j'
setGlyph(0x6a, [0b00010, 0b00000, 0b00110, 0b00010, 0b00010, 0b10010, 0b01100]);
// 0x6B 'k'
setGlyph(0x6b, [0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010]);
// 0x6C 'l'
setGlyph(0x6c, [0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110]);
// 0x6D 'm'
setGlyph(0x6d, [0b00000, 0b00000, 0b11010, 0b10101, 0b10101, 0b10001, 0b10001]);
// 0x6E 'n'
setGlyph(0x6e, [0b00000, 0b00000, 0b10110, 0b11001, 0b10001, 0b10001, 0b10001]);
// 0x6F 'o'
setGlyph(0x6f, [0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110]);
// 0x70 'p'
setGlyph(0x70, [0b00000, 0b00000, 0b11110, 0b10001, 0b11110, 0b10000, 0b10000]);
// 0x71 'q'
setGlyph(0x71, [0b00000, 0b00000, 0b01111, 0b10001, 0b01111, 0b00001, 0b00001]);
// 0x72 'r'
setGlyph(0x72, [0b00000, 0b00000, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000]);
// 0x73 's'
setGlyph(0x73, [0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110]);
// 0x74 't'
setGlyph(0x74, [0b01000, 0b01000, 0b11110, 0b01000, 0b01000, 0b01001, 0b00110]);
// 0x75 'u'
setGlyph(0x75, [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b10011, 0b01101]);
// 0x76 'v'
setGlyph(0x76, [0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100]);
// 0x77 'w'
setGlyph(0x77, [0b00000, 0b00000, 0b10001, 0b10001, 0b10101, 0b10101, 0b01010]);
// 0x78 'x'
setGlyph(0x78, [0b00000, 0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001]);
// 0x79 'y'
setGlyph(0x79, [0b00000, 0b00000, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110]);
// 0x7A 'z'
setGlyph(0x7a, [0b00000, 0b00000, 0b11111, 0b00010, 0b00100, 0b01000, 0b11111]);

/**
 * Look up the glyph for a character. Returns `UNKNOWN_GLYPH`
 * for anything outside the font's covered range OR for
 * codepoints whose glyph hasn't been filled in yet.
 */
export function glyphFor(c: string): Glyph {
  if (c.length === 0) {
    return UNKNOWN_GLYPH;
  }
  const code = c.charCodeAt(0);
  if (code === 0x20) {
    // Space is legitimately blank.
    return new Uint8Array(GLYPH_HEIGHT);
  }
  if (code < FIRST_CHAR || code > LAST_CHAR) {
    return UNKNOWN_GLYPH;
  }
  const base = (code - FIRST_CHAR) * GLYPH_HEIGHT;
  const view = FONT_DATA.subarray(base, base + GLYPH_HEIGHT);
  let allZero = true;
  for (let i = 0; i < GLYPH_HEIGHT; i += 1) {
    if (view[i] !== 0) {
      allZero = false;
      break;
    }
  }
  if (allZero) {
    return UNKNOWN_GLYPH;
  }
  return view;
}

/**
 * True iff the `(col, row)` pixel in `glyph` is set.
 * Out-of-range indices return false.
 */
export function glyphPixel(glyph: Glyph, col: number, row: number): boolean {
  if (col < 0 || col >= GLYPH_WIDTH || row < 0 || row >= GLYPH_HEIGHT) {
    return false;
  }
  const rowBits = glyph[row] ?? 0;
  const shift = GLYPH_WIDTH - 1 - col;
  return ((rowBits >> shift) & 1) !== 0;
}
