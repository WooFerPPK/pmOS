// TypeScript port of `crates/term/src/rasterizer.rs`.
//
// Takes a terminal snapshot (scrollback lines + current
// input buffer + prompt) and produces a flat ARGB8888 pixel
// buffer ready for a `pmd_buffer` upload or — in the v1
// demo path — direct consumption by the TS-side
// `FramebufferDriver` via `OP_BLIT`.
//
// The output layout and palette must match the Rust
// rasterizer byte-for-byte: the eventual WASM kernel will
// swap in the Rust version and no pixels should shift.

import {
  CELL_HEIGHT,
  CELL_WIDTH,
  GLYPH_HEIGHT,
  GLYPH_WIDTH,
  glyphFor,
  glyphPixel,
  type Glyph,
} from "./font";

/** Semantic class of a scrollback line. Mirrors Rust `LineKind`. */
export type LineKind = "banner" | "input" | "output" | "error";

/** One line in a rasterizable terminal snapshot. */
export interface RasterizerLine {
  readonly text: string;
  readonly kind: LineKind;
}

/** Everything the rasterizer needs to draw one frame. */
export interface RasterizerSnapshot {
  readonly lines: readonly RasterizerLine[];
  readonly inputBuffer: string;
  readonly prompt: string;
}

/** Foreground + background colour assignment for each line kind. */
export interface Palette {
  readonly bg: number;
  readonly banner: number;
  readonly input: number;
  readonly output: number;
  readonly error: number;
  readonly cursor: number;
}

/** Border padding around the text grid, in pixels. */
export const PADDING = 4;

/** Bytes per pixel in the output. */
export const BYTES_PER_PIXEL = 4;

/**
 * ARGB8888 colours — each constant is `0xAARRGGBB`. Must
 * match `crates/term/src/rasterizer.rs::colors`.
 */
export const colors = {
  BG: 0xff0a0e14,
  FG_OUTPUT: 0xffe6e6e6,
  FG_INPUT: 0xff7cb7ff,
  FG_ERROR: 0xffff7070,
  FG_BANNER: 0xff808591,
  CURSOR: 0xffffffff,
} as const;

/** Default palette — mirrors Rust `Palette::default()`. */
export const DEFAULT_PALETTE: Palette = {
  bg: colors.BG,
  banner: colors.FG_BANNER,
  input: colors.FG_INPUT,
  output: colors.FG_OUTPUT,
  error: colors.FG_ERROR,
  cursor: colors.CURSOR,
};

/**
 * Rasterize `snapshot` into a fresh ARGB8888 buffer of
 * `width × height` pixels, painted with `palette` (default:
 * [`DEFAULT_PALETTE`]). The returned `Uint8Array` has
 * length `width * height * 4` and tight stride.
 */
export function rasterizeSnapshot(
  snapshot: RasterizerSnapshot,
  width: number,
  height: number,
  palette: Palette = DEFAULT_PALETTE,
): Uint8Array {
  const pixels = new Uint8Array(width * height * BYTES_PER_PIXEL);
  fillBg(pixels, palette.bg);

  if (width <= 2 * PADDING || height <= 2 * PADDING) {
    return pixels;
  }

  const textOriginX = PADDING;
  const textOriginY = PADDING;
  const textWidth = width - 2 * PADDING;
  const textHeight = height - 2 * PADDING;
  const cols = Math.floor(textWidth / CELL_WIDTH);
  const rowsTotal = Math.floor(textHeight / CELL_HEIGHT);
  if (cols === 0 || rowsTotal === 0) {
    return pixels;
  }

  const scrollbackRows = Math.max(0, rowsTotal - 1);

  // Scrollback: render the most-recent `scrollbackRows` lines.
  const lines = snapshot.lines;
  const start = Math.max(0, lines.length - scrollbackRows);
  const visible = lines.slice(start);
  for (let rowIdx = 0; rowIdx < visible.length; rowIdx += 1) {
    const line = visible[rowIdx];
    if (!line) {
      continue;
    }
    const pixelY = textOriginY + rowIdx * CELL_HEIGHT;
    const fg = fgForKind(palette, line.kind);
    drawLine(pixels, width, height, textOriginX, pixelY, cols, line.text, fg);
  }

  // Active input line at the bottom — prompt + input buffer.
  const inputRow = scrollbackRows;
  const pixelY = textOriginY + inputRow * CELL_HEIGHT;
  const combined = snapshot.prompt + snapshot.inputBuffer;
  drawLine(pixels, width, height, textOriginX, pixelY, cols, combined, palette.input);

  // Cursor block right after the input text (clipped at
  // the right edge of the text area).
  const cursorCol = combined.length;
  if (cursorCol < cols) {
    const cursorX = textOriginX + cursorCol * CELL_WIDTH;
    fillRect(
      pixels,
      width,
      height,
      cursorX,
      pixelY,
      GLYPH_WIDTH,
      GLYPH_HEIGHT,
      palette.cursor,
    );
  }

  return pixels;
}

function fgForKind(p: Palette, kind: LineKind): number {
  switch (kind) {
    case "banner":
      return p.banner;
    case "input":
      return p.input;
    case "output":
      return p.output;
    case "error":
      return p.error;
  }
}

function fillBg(pixels: Uint8Array, argb: number): void {
  const [b, g, r, a] = splitArgb(argb);
  for (let i = 0; i < pixels.length; i += BYTES_PER_PIXEL) {
    pixels[i] = b;
    pixels[i + 1] = g;
    pixels[i + 2] = r;
    pixels[i + 3] = a;
  }
}

function drawLine(
  pixels: Uint8Array,
  fbWidth: number,
  fbHeight: number,
  originX: number,
  originY: number,
  cols: number,
  text: string,
  fg: number,
): void {
  for (let i = 0; i < text.length; i += 1) {
    if (i >= cols) {
      break;
    }
    const ch = text.charAt(i);
    const glyph = glyphFor(ch);
    const x0 = originX + i * CELL_WIDTH;
    drawGlyph(pixels, fbWidth, fbHeight, glyph, x0, originY, fg);
  }
}

function drawGlyph(
  pixels: Uint8Array,
  fbWidth: number,
  fbHeight: number,
  glyph: Glyph,
  x0: number,
  y0: number,
  fg: number,
): void {
  for (let row = 0; row < GLYPH_HEIGHT; row += 1) {
    for (let col = 0; col < GLYPH_WIDTH; col += 1) {
      if (!glyphPixel(glyph, col, row)) {
        continue;
      }
      setPixel(pixels, fbWidth, fbHeight, x0 + col, y0 + row, fg);
    }
  }
}

function fillRect(
  pixels: Uint8Array,
  fbWidth: number,
  fbHeight: number,
  x0: number,
  y0: number,
  w: number,
  h: number,
  argb: number,
): void {
  for (let dy = 0; dy < h; dy += 1) {
    for (let dx = 0; dx < w; dx += 1) {
      setPixel(pixels, fbWidth, fbHeight, x0 + dx, y0 + dy, argb);
    }
  }
}

function setPixel(
  pixels: Uint8Array,
  fbWidth: number,
  fbHeight: number,
  x: number,
  y: number,
  argb: number,
): void {
  if (x < 0 || x >= fbWidth || y < 0 || y >= fbHeight) {
    return;
  }
  const idx = (y * fbWidth + x) * BYTES_PER_PIXEL;
  if (idx + BYTES_PER_PIXEL > pixels.length) {
    return;
  }
  const [b, g, r, a] = splitArgb(argb);
  pixels[idx] = b;
  pixels[idx + 1] = g;
  pixels[idx + 2] = r;
  pixels[idx + 3] = a;
}

function splitArgb(argb: number): [number, number, number, number] {
  // Use unsigned shifts so the alpha byte decodes
  // correctly when argb's high bit is set.
  return [
    argb & 0xff,
    (argb >>> 8) & 0xff,
    (argb >>> 16) & 0xff,
    (argb >>> 24) & 0xff,
  ];
}
