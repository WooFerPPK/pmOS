// Vitest coverage for the TS-side text rasterizer. Mirrors
// the Rust `term::rasterizer` tests as closely as the
// cross-lang boundary allows so the two outputs can be
// compared by eye.

import { describe, expect, it } from "vitest";
import {
  BYTES_PER_PIXEL,
  DEFAULT_PALETTE,
  PADDING,
  colors,
  rasterizeSnapshot,
  type RasterizerSnapshot,
} from "../../src/shared/rasterizer";
import {
  CELL_HEIGHT,
  CELL_WIDTH,
  GLYPH_HEIGHT,
  GLYPH_WIDTH,
} from "../../src/shared/font";

// Background is 0xFF0A0E14 → (R=0A, G=0E, B=14, A=FF)
// written in RGBA memory order for canvas ImageData.
const BG_PIXEL: readonly [number, number, number, number] = [
  0x0a, 0x0e, 0x14, 0xff,
];

function readPixel(
  buf: Uint8Array,
  width: number,
  x: number,
  y: number,
): [number, number, number, number] {
  const idx = (y * width + x) * BYTES_PER_PIXEL;
  return [
    buf[idx] ?? 0,
    buf[idx + 1] ?? 0,
    buf[idx + 2] ?? 0,
    buf[idx + 3] ?? 0,
  ];
}

function emptySnapshot(): RasterizerSnapshot {
  return { lines: [], inputBuffer: "", prompt: "" };
}

describe("rasterizeSnapshot", () => {
  it("produces a buffer of width*height*4 bytes", () => {
    const pixels = rasterizeSnapshot(emptySnapshot(), 64, 32);
    expect(pixels.byteLength).toBe(64 * 32 * 4);
  });

  it("fills the whole buffer with background on an empty snapshot", () => {
    const pixels = rasterizeSnapshot(emptySnapshot(), 64, 32);
    // The cursor block is painted even on an empty
    // snapshot (the prompt is "" so the cursor sits at
    // col 0), so we check the four corners instead.
    expect(readPixel(pixels, 64, 0, 0)).toEqual([...BG_PIXEL]);
    expect(readPixel(pixels, 64, 63, 0)).toEqual([...BG_PIXEL]);
    expect(readPixel(pixels, 64, 0, 31)).toEqual([...BG_PIXEL]);
    expect(readPixel(pixels, 64, 63, 31)).toEqual([...BG_PIXEL]);
  });

  it("paints a cursor block after the prompt on the input row", () => {
    const snap: RasterizerSnapshot = {
      lines: [],
      inputBuffer: "",
      prompt: "> ",
    };
    const width = 120;
    const height = 64;
    const pixels = rasterizeSnapshot(snap, width, height);

    const textOriginX = PADDING;
    const textOriginY = PADDING;
    const textHeight = height - 2 * PADDING;
    const rowsTotal = Math.floor(textHeight / CELL_HEIGHT);
    const scrollbackRows = rowsTotal - 1;
    const inputRowY = textOriginY + scrollbackRows * CELL_HEIGHT;
    const cursorX = textOriginX + 2 * CELL_WIDTH;

    const cx = cursorX + Math.floor(GLYPH_WIDTH / 2);
    const cy = inputRowY + Math.floor(GLYPH_HEIGHT / 2);
    // Cursor colour is 0xFFFFFFFF → [FF, FF, FF, FF].
    expect(readPixel(pixels, width, cx, cy)).toEqual([0xff, 0xff, 0xff, 0xff]);
  });

  it("paints output lines with the output foreground colour", () => {
    const snap: RasterizerSnapshot = {
      lines: [{ text: "o", kind: "output" }],
      inputBuffer: "",
      prompt: "",
    };
    const width = 120;
    const height = 64;
    const pixels = rasterizeSnapshot(snap, width, height);

    // Default output fg is 0xFFE6E6E6 → R=E6, G=E6, B=E6,
    // A=FF (palindromic, so the byte order swap is
    // invisible here, but the test still pins down that
    // the channel reads from the correct shifts).
    const fg: [number, number, number, number] = [0xe6, 0xe6, 0xe6, 0xff];
    const baseX = PADDING;
    const baseY = PADDING;

    let found = false;
    for (let row = 0; row < GLYPH_HEIGHT; row += 1) {
      for (let col = 0; col < GLYPH_WIDTH; col += 1) {
        const px = readPixel(pixels, width, baseX + col, baseY + row);
        if (
          px[0] === fg[0] &&
          px[1] === fg[1] &&
          px[2] === fg[2] &&
          px[3] === fg[3]
        ) {
          found = true;
        }
      }
    }
    expect(
      found,
      "output glyph did not paint any foreground pixels",
    ).toBe(true);
  });

  it("paints different line kinds with different foreground colours", () => {
    const snap: RasterizerSnapshot = {
      lines: [
        { text: "B", kind: "banner" },
        { text: "I", kind: "input" },
        { text: "O", kind: "output" },
        { text: "E", kind: "error" },
      ],
      inputBuffer: "",
      prompt: "",
    };
    const width = 120;
    const height = 80;
    const pixels = rasterizeSnapshot(snap, width, height);

    // Palette constants are `0xAARRGGBB`; the rasterizer
    // writes R,G,B,A to memory, so expected pixels are
    // `[R, G, B, A]`.
    const toRgba = (argb: number): [number, number, number, number] => [
      (argb >>> 16) & 0xff,
      (argb >>> 8) & 0xff,
      argb & 0xff,
      (argb >>> 24) & 0xff,
    ];
    const expected: ReadonlyArray<[number, [number, number, number, number]]> =
      [
        [0, toRgba(DEFAULT_PALETTE.banner)],
        [1, toRgba(DEFAULT_PALETTE.input)],
        [2, toRgba(DEFAULT_PALETTE.output)],
        [3, toRgba(DEFAULT_PALETTE.error)],
      ];

    for (const [rowIdx, fg] of expected) {
      const baseX = PADDING;
      const baseY = PADDING + rowIdx * CELL_HEIGHT;
      let found = false;
      for (let row = 0; row < GLYPH_HEIGHT; row += 1) {
        for (let col = 0; col < GLYPH_WIDTH; col += 1) {
          const px = readPixel(pixels, width, baseX + col, baseY + row);
          if (
            px[0] === fg[0] &&
            px[1] === fg[1] &&
            px[2] === fg[2] &&
            px[3] === fg[3]
          ) {
            found = true;
          }
        }
      }
      expect(
        found,
        `row ${rowIdx} did not produce expected foreground pixel`,
      ).toBe(true);
    }
  });

  it("short-circuits tiny framebuffers to all background", () => {
    // Below 2*PADDING on either axis — the rasterizer
    // skips the text grid entirely and the whole buffer
    // stays at the clear colour.
    const pixels = rasterizeSnapshot(emptySnapshot(), 4, 4);
    for (let i = 0; i < pixels.length; i += 4) {
      expect([pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]).toEqual([
        ...BG_PIXEL,
      ]);
    }
  });

  it("clips lines longer than the column count without throwing", () => {
    // text area width = 24 - 2*PADDING = 16 → cols = 2.
    const snap: RasterizerSnapshot = {
      lines: [{ text: "aaaa", kind: "output" }],
      inputBuffer: "",
      prompt: "",
    };
    const pixels = rasterizeSnapshot(snap, 24, 32);
    expect(pixels.byteLength).toBe(24 * 32 * 4);
  });

  it("renders at least one non-background pixel for typical input", () => {
    const snap: RasterizerSnapshot = {
      lines: [
        { text: "PMos 0.1.0", kind: "banner" },
        { text: "hi", kind: "output" },
      ],
      inputBuffer: "ec",
      prompt: "> ",
    };
    const pixels = rasterizeSnapshot(snap, 160, 64);
    let nonBg = 0;
    for (let i = 0; i < pixels.length; i += 4) {
      const px: [number, number, number, number] = [
        pixels[i] ?? 0,
        pixels[i + 1] ?? 0,
        pixels[i + 2] ?? 0,
        pixels[i + 3] ?? 0,
      ];
      if (
        px[0] !== BG_PIXEL[0] ||
        px[1] !== BG_PIXEL[1] ||
        px[2] !== BG_PIXEL[2] ||
        px[3] !== BG_PIXEL[3]
      ) {
        nonBg += 1;
      }
    }
    expect(nonBg).toBeGreaterThan(0);
  });
});

describe("mouse cursor overlay", () => {
  it("draws a 5-pixel plus at the cursor position", () => {
    const width = 64;
    const height = 32;
    const cursorX = 20;
    const cursorY = 16;
    const snap: RasterizerSnapshot = {
      lines: [],
      inputBuffer: "",
      prompt: "",
      cursor: { x: cursorX, y: cursorY },
    };
    const pixels = rasterizeSnapshot(snap, width, height);

    // Cursor colour is 0xFFFFFFFF → [FF, FF, FF, FF].
    const white: [number, number, number, number] = [0xff, 0xff, 0xff, 0xff];
    // The 5 horizontal pixels (cursorX-2 .. cursorX+2, row cursorY).
    for (let dx = -2; dx <= 2; dx += 1) {
      expect(
        readPixel(pixels, width, cursorX + dx, cursorY),
        `horizontal dx=${dx}`,
      ).toEqual(white);
    }
    // The 4 vertical pixels (the center is shared with
    // the horizontal).
    for (const dy of [-2, -1, 1, 2]) {
      expect(
        readPixel(pixels, width, cursorX, cursorY + dy),
        `vertical dy=${dy}`,
      ).toEqual(white);
    }
    // A pixel not on the cross should still be background.
    expect(readPixel(pixels, width, cursorX + 2, cursorY + 2)).toEqual([
      ...BG_PIXEL,
    ]);
  });

  it("does not draw a cursor when snapshot.cursor is omitted", () => {
    const snap: RasterizerSnapshot = {
      lines: [],
      inputBuffer: "",
      prompt: "",
    };
    const width = 64;
    const height = 32;
    const pixels = rasterizeSnapshot(snap, width, height);
    // Count white pixels — without a cursor there should
    // be none at (any, any) outside the text cursor
    // block. The text cursor block is drawn at the
    // input line position, so we sample a clearly-
    // background region near the top-center.
    const px = readPixel(pixels, width, 32, 4);
    expect(px).toEqual([...BG_PIXEL]);
  });

  it("clips the cursor sprite at framebuffer edges", () => {
    const snap: RasterizerSnapshot = {
      lines: [],
      inputBuffer: "",
      prompt: "",
      cursor: { x: 0, y: 0 },
    };
    const width = 32;
    const height = 32;
    // Should not panic / write out of bounds.
    const pixels = rasterizeSnapshot(snap, width, height);
    expect(pixels.byteLength).toBe(width * height * 4);
    // The center pixel of the cursor lands at (0, 0) and
    // is white. Pixels at (-1, 0), (-2, 0), (0, -1),
    // (0, -2) are clipped away.
    expect(readPixel(pixels, width, 0, 0)).toEqual([0xff, 0xff, 0xff, 0xff]);
  });

  it("cursor is painted ON TOP of existing content", () => {
    // A snapshot with an output line at (0, 0) — the
    // letter "o". The cursor at (2, 3) should overpaint
    // any foreground pixel of the glyph at that spot.
    const snap: RasterizerSnapshot = {
      lines: [{ text: "o", kind: "output" }],
      inputBuffer: "",
      prompt: "",
      cursor: { x: PADDING + 2, y: PADDING + 3 },
    };
    const width = 64;
    const height = 32;
    const pixels = rasterizeSnapshot(snap, width, height);
    // That exact pixel is the center of the cursor → white.
    expect(readPixel(pixels, width, PADDING + 2, PADDING + 3)).toEqual([
      0xff, 0xff, 0xff, 0xff,
    ]);
  });
});

describe("DEFAULT_PALETTE", () => {
  it("exposes each colour via the colors namespace", () => {
    expect(DEFAULT_PALETTE.bg).toBe(colors.BG);
    expect(DEFAULT_PALETTE.banner).toBe(colors.FG_BANNER);
    expect(DEFAULT_PALETTE.input).toBe(colors.FG_INPUT);
    expect(DEFAULT_PALETTE.output).toBe(colors.FG_OUTPUT);
    expect(DEFAULT_PALETTE.error).toBe(colors.FG_ERROR);
    expect(DEFAULT_PALETTE.cursor).toBe(colors.CURSOR);
  });
});
