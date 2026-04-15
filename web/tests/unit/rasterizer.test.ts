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

const BG_PIXEL: readonly [number, number, number, number] = [
  0x14, 0x0e, 0x0a, 0xff,
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

    // Default output fg is 0xFFE6E6E6 → [E6, E6, E6, FF].
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

    const expected: ReadonlyArray<[number, [number, number, number, number]]> =
      [
        [
          0,
          [
            DEFAULT_PALETTE.banner & 0xff,
            (DEFAULT_PALETTE.banner >>> 8) & 0xff,
            (DEFAULT_PALETTE.banner >>> 16) & 0xff,
            (DEFAULT_PALETTE.banner >>> 24) & 0xff,
          ],
        ],
        [
          1,
          [
            DEFAULT_PALETTE.input & 0xff,
            (DEFAULT_PALETTE.input >>> 8) & 0xff,
            (DEFAULT_PALETTE.input >>> 16) & 0xff,
            (DEFAULT_PALETTE.input >>> 24) & 0xff,
          ],
        ],
        [
          2,
          [
            DEFAULT_PALETTE.output & 0xff,
            (DEFAULT_PALETTE.output >>> 8) & 0xff,
            (DEFAULT_PALETTE.output >>> 16) & 0xff,
            (DEFAULT_PALETTE.output >>> 24) & 0xff,
          ],
        ],
        [
          3,
          [
            DEFAULT_PALETTE.error & 0xff,
            (DEFAULT_PALETTE.error >>> 8) & 0xff,
            (DEFAULT_PALETTE.error >>> 16) & 0xff,
            (DEFAULT_PALETTE.error >>> 24) & 0xff,
          ],
        ],
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
