// Vitest coverage for the TS-side 5x7 bitmap font. Mirrors
// the Rust `term::font` tests as closely as the cross-lang
// boundary allows so the two implementations can be compared
// by eye.

import { describe, expect, it } from "vitest";
import {
  CELL_HEIGHT,
  CELL_WIDTH,
  FIRST_CHAR,
  GLYPH_HEIGHT,
  GLYPH_WIDTH,
  LAST_CHAR,
  UNKNOWN_GLYPH,
  glyphFor,
  glyphPixel,
} from "../../src/shared/font";

describe("font dimensions", () => {
  it("uses 5x7 glyphs with a 6x8 cell", () => {
    expect(GLYPH_WIDTH).toBe(5);
    expect(GLYPH_HEIGHT).toBe(7);
    expect(CELL_WIDTH).toBe(6);
    expect(CELL_HEIGHT).toBe(8);
  });

  it("covers the printable ASCII range", () => {
    expect(FIRST_CHAR).toBe(0x20);
    expect(LAST_CHAR).toBe(0x7e);
  });
});

describe("glyphFor", () => {
  it("returns all-zero rows for space", () => {
    const g = glyphFor(" ");
    for (let i = 0; i < GLYPH_HEIGHT; i += 1) {
      expect(g[i]).toBe(0);
    }
  });

  it("returns non-zero rows for known letters", () => {
    const letters = [
      "a",
      "b",
      "c",
      "e",
      "h",
      "i",
      "l",
      "m",
      "n",
      "o",
      "p",
      "r",
      "s",
      "t",
      "u",
      "x",
      "y",
    ];
    for (const letter of letters) {
      const g = glyphFor(letter);
      const hasPixel = Array.from(g).some((r) => r !== 0);
      expect(hasPixel, `letter '${letter}' must have a non-empty glyph`).toBe(
        true,
      );
    }
  });

  it("returns non-zero rows for digits", () => {
    for (const d of ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]) {
      const g = glyphFor(d);
      const hasPixel = Array.from(g).some((r) => r !== 0);
      expect(hasPixel, `digit '${d}' must have a non-empty glyph`).toBe(true);
    }
  });

  it("returns UNKNOWN_GLYPH for an unmapped ASCII codepoint", () => {
    // '`' (0x60) is deliberately left unset in the font
    // table, so lookup promotes it to UNKNOWN_GLYPH.
    const g = glyphFor("`");
    expect(Array.from(g)).toEqual(Array.from(UNKNOWN_GLYPH));
  });

  it("returns UNKNOWN_GLYPH for non-ASCII codepoints", () => {
    const g = glyphFor("π");
    expect(Array.from(g)).toEqual(Array.from(UNKNOWN_GLYPH));
  });

  it("returns UNKNOWN_GLYPH for an empty string", () => {
    const g = glyphFor("");
    expect(Array.from(g)).toEqual(Array.from(UNKNOWN_GLYPH));
  });

  it("matches the Rust-side row bytes for digit '1'", () => {
    // Hand-coded parity check: the TS port must match the
    // Rust source byte-for-byte for this one glyph. If
    // this fails the two codebases have drifted.
    const g = glyphFor("1");
    expect(Array.from(g)).toEqual([
      0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ]);
  });

  it("matches the Rust-side row bytes for letter 'H'", () => {
    const g = glyphFor("H");
    expect(Array.from(g)).toEqual([
      0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
    ]);
  });

  it("matches the Rust-side row bytes for letter 'o'", () => {
    const g = glyphFor("o");
    expect(Array.from(g)).toEqual([
      0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110,
    ]);
  });
});

describe("glyphPixel", () => {
  it("extracts per-pixel flags for digit '1'", () => {
    const g = glyphFor("1");
    // Row 0 is 0b00100 — only column 2 is set.
    expect(glyphPixel(g, 0, 0)).toBe(false);
    expect(glyphPixel(g, 1, 0)).toBe(false);
    expect(glyphPixel(g, 2, 0)).toBe(true);
    expect(glyphPixel(g, 3, 0)).toBe(false);
    expect(glyphPixel(g, 4, 0)).toBe(false);
  });

  it("returns false for out-of-range coords", () => {
    const g = glyphFor("1");
    expect(glyphPixel(g, 5, 0)).toBe(false);
    expect(glyphPixel(g, 0, 7)).toBe(false);
    expect(glyphPixel(g, -1, 0)).toBe(false);
    expect(glyphPixel(g, 0, -1)).toBe(false);
  });
});

describe("UNKNOWN_GLYPH", () => {
  it("is a hollow 5x7 rectangle", () => {
    // Top + bottom rows are all 1s, middle rows have left
    // and right edges set.
    expect(UNKNOWN_GLYPH[0]).toBe(0b11111);
    expect(UNKNOWN_GLYPH[6]).toBe(0b11111);
    for (let r = 1; r < 6; r += 1) {
      expect(UNKNOWN_GLYPH[r]).toBe(0b10001);
    }
  });
});
