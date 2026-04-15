//! Turn a [`TerminalSnapshot`] into an ARGB8888 pixel buffer
//! suitable for a `pmd_buffer` upload.
//!
//! The rasterizer is intentionally dumb: it iterates the
//! scrollback top-down, lays each line out at a fixed
//! [`CELL_HEIGHT`]-pixel stride, and for each character
//! consults the [`toolkit::draw::font`] glyph table to decide
//! which pixels are foreground and which stay at the
//! background colour. The active input line is rendered
//! at the bottom below the scrollback.
//!
//! The output is a `Vec<u8>` of `width * height * 4`
//! bytes in little-endian ARGB8888 byte order — the
//! same layout [`display_server::Framebuffer`] expects.
//! The caller writes it into an shm pool and commits.
//!
//! Things the rasterizer deliberately does **not** do in
//! v1:
//!
//! * **Scrolling:** if more lines fit in the target than
//!   the scrollback has, the blank rows at the top are
//!   left as background. Conversely, if the scrollback
//!   has more lines than fit, only the tail is rendered
//!   (newest-first from the bottom).
//! * **Wrap:** lines longer than the target width are
//!   clipped at the right edge. No soft-wrap.
//! * **Antialiasing / subpixel hinting:** it's a bitmap
//!   font, pixels are either foreground or background.
//! * **Cursor blink:** the input line gets a fixed solid
//!   cursor block at the end of the buffer.

use toolkit::draw::font::{
    glyph_for, glyph_pixel, CELL_HEIGHT, CELL_WIDTH, GLYPH_HEIGHT, GLYPH_WIDTH,
};

use crate::terminal::{LineKind, TerminalSnapshot};

/// A mutable view of the rasterizer output: the pixel
/// byte buffer plus its width and height. Bundled into a
/// single struct so the internal helpers take two
/// arguments instead of three (the fourth time clippy
/// flagged `fn draw_line(pixels, width, height, ...)` for
/// arg count).
struct Target<'a> {
    pixels: &'a mut [u8],
    width: u32,
    height: u32,
}

/// Border padding around the text grid, in pixels.
pub const PADDING: u32 = 4;

/// Bytes per pixel in the output.
pub const BYTES_PER_PIXEL: usize = 4;

/// ARGB8888 colours used by the default [`Palette`].
/// Each constant is `0xAARRGGBB` (the high byte is alpha),
/// matching the shape [`Framebuffer::clear`] expects.
pub mod colors {
    pub const BG: u32 = 0xFF0A_0E14;
    pub const FG_OUTPUT: u32 = 0xFFE6_E6E6;
    pub const FG_INPUT: u32 = 0xFF7C_B7FF;
    pub const FG_ERROR: u32 = 0xFFFF_7070;
    pub const FG_BANNER: u32 = 0xFF80_8591;
    pub const CURSOR: u32 = 0xFFFF_FFFF;
}

/// Foreground + background colour assignment for each
/// line kind. The default palette uses the colours in
/// [`colors`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: u32,
    pub banner: u32,
    pub input: u32,
    pub output: u32,
    pub error: u32,
    pub cursor: u32,
}

impl Palette {
    pub fn fg_for(&self, kind: LineKind) -> u32 {
        match kind {
            LineKind::Banner => self.banner,
            LineKind::Input => self.input,
            LineKind::Output => self.output,
            LineKind::Error => self.error,
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            bg: colors::BG,
            banner: colors::FG_BANNER,
            input: colors::FG_INPUT,
            output: colors::FG_OUTPUT,
            error: colors::FG_ERROR,
            cursor: colors::CURSOR,
        }
    }
}

/// Rasterize `snapshot` into a fresh ARGB8888 buffer of
/// `width × height` pixels, painted with the default
/// [`Palette`]. The buffer's stride is tight
/// (`width * 4` bytes).
pub fn rasterize_snapshot(
    snapshot: &TerminalSnapshot,
    width: u32,
    height: u32,
) -> Vec<u8> {
    rasterize_snapshot_with_palette(snapshot, width, height, Palette::default())
}

/// [`rasterize_snapshot`] with an explicit palette.
pub fn rasterize_snapshot_with_palette(
    snapshot: &TerminalSnapshot,
    width: u32,
    height: u32,
    palette: Palette,
) -> Vec<u8> {
    let pixels_total = (width as usize) * (height as usize) * BYTES_PER_PIXEL;
    let mut out = vec![0u8; pixels_total];
    let mut target = Target {
        pixels: &mut out,
        width,
        height,
    };
    fill_bg(&mut target, palette.bg);

    if width <= 2 * PADDING || height <= 2 * PADDING {
        return out;
    }

    let text_origin_x = PADDING;
    let text_origin_y = PADDING;
    let text_width = width - 2 * PADDING;
    let text_height = height - 2 * PADDING;
    let cols = text_width / CELL_WIDTH;
    let rows_total = text_height / CELL_HEIGHT;
    if cols == 0 || rows_total == 0 {
        return out;
    }

    // Reserve the bottom row for the active input line.
    // If `rows_total == 1` the input line takes the only
    // row and scrollback is hidden.
    let scrollback_rows = rows_total.saturating_sub(1);

    // Scrollback: render the most-recent `scrollback_rows`
    // lines of `snapshot.lines`.
    let lines = &snapshot.lines;
    let start = lines.len().saturating_sub(scrollback_rows as usize);
    for (row_idx, line) in lines[start..].iter().enumerate() {
        let pixel_y = text_origin_y + row_idx as u32 * CELL_HEIGHT;
        let fg = palette.fg_for(line.kind);
        draw_line(&mut target, text_origin_x, pixel_y, cols, &line.text, fg);
    }

    // Active input line at the bottom row, rendered as if
    // it were an Input line (prompt-coloured). The line
    // text is `snapshot.prompt + snapshot.input_buffer`.
    let input_row = scrollback_rows;
    let pixel_y = text_origin_y + input_row * CELL_HEIGHT;
    let mut combined = String::with_capacity(snapshot.prompt.len() + snapshot.input_buffer.len());
    combined.push_str(&snapshot.prompt);
    combined.push_str(&snapshot.input_buffer);
    draw_line(&mut target, text_origin_x, pixel_y, cols, &combined, palette.input);

    // Cursor block right after the input text (clipped at
    // the right edge of the text area).
    let cursor_col = combined.chars().count() as u32;
    if cursor_col < cols {
        let cursor_x = text_origin_x + cursor_col * CELL_WIDTH;
        fill_rect(
            &mut target,
            cursor_x,
            pixel_y,
            GLYPH_WIDTH,
            GLYPH_HEIGHT,
            palette.cursor,
        );
    }

    out
}

fn fill_bg(target: &mut Target, argb: u32) {
    let (b, g, r, a) = split_argb(argb);
    for chunk in target.pixels.chunks_exact_mut(BYTES_PER_PIXEL) {
        chunk[0] = b;
        chunk[1] = g;
        chunk[2] = r;
        chunk[3] = a;
    }
}

fn draw_line(target: &mut Target, origin_x: u32, origin_y: u32, cols: u32, text: &str, fg: u32) {
    for (i, ch) in text.chars().enumerate() {
        let col = i as u32;
        if col >= cols {
            break;
        }
        let glyph = glyph_for(ch);
        let x0 = origin_x + col * CELL_WIDTH;
        draw_glyph(target, glyph, x0, origin_y, fg);
    }
}

fn draw_glyph(target: &mut Target, glyph: &toolkit::draw::font::Glyph, x0: u32, y0: u32, fg: u32) {
    for row in 0..GLYPH_HEIGHT {
        for col in 0..GLYPH_WIDTH {
            if !glyph_pixel(glyph, col, row) {
                continue;
            }
            set_pixel(target, x0 + col, y0 + row, fg);
        }
    }
}

fn fill_rect(target: &mut Target, x0: u32, y0: u32, w: u32, h: u32, argb: u32) {
    for dy in 0..h {
        for dx in 0..w {
            set_pixel(target, x0 + dx, y0 + dy, argb);
        }
    }
}

fn set_pixel(target: &mut Target, x: u32, y: u32, argb: u32) {
    if x >= target.width || y >= target.height {
        return;
    }
    let idx = ((y * target.width + x) as usize) * BYTES_PER_PIXEL;
    if idx + BYTES_PER_PIXEL > target.pixels.len() {
        return;
    }
    let (b, g, r, a) = split_argb(argb);
    target.pixels[idx] = b;
    target.pixels[idx + 1] = g;
    target.pixels[idx + 2] = r;
    target.pixels[idx + 3] = a;
}

fn split_argb(argb: u32) -> (u8, u8, u8, u8) {
    (
        (argb & 0xff) as u8,
        ((argb >> 8) & 0xff) as u8,
        ((argb >> 16) & 0xff) as u8,
        ((argb >> 24) & 0xff) as u8,
    )
}
