//! Turn a [`TerminalSnapshot`] into an ARGB8888 pixel buffer
//! suitable for a `pmd_buffer` upload.
//!
//! The rasterizer iterates the scrollback top-down, lays each
//! line out using the selected bitmap font's cell metrics, and
//! paints glyph bits from a bounded P1 PBM atlas. The active
//! input line is rendered at the bottom below the scrollback.
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

use std::fs::File;
use std::io::Read;
use std::sync::OnceLock;

use crate::terminal::{LineKind, TerminalSnapshot};

/// Directory containing the two v1 terminal font assets.
pub const FONT_DIR: &str = "/usr/share/fonts";
/// Safe font used when the preference or selected asset is unusable.
pub const DEFAULT_FONT_NAME: &str = preferences::TERMINAL_FONT_NAMES[0];
/// Alternate v1 terminal font.
pub const VGA_FONT_NAME: &str = preferences::TERMINAL_FONT_NAMES[1];
/// Maximum accepted preference file size.
pub const MAX_PREFERENCES_BYTES: usize = 64 * 1024;
/// Maximum accepted encoded PBM size.
pub const MAX_FONT_BYTES: usize = 64 * 1024;
/// Metrics of the safe default atlas. Use [`BitmapFont`] methods for a selected font.
pub const DEFAULT_CELL_WIDTH: u32 = 8;
pub const DEFAULT_CELL_HEIGHT: u32 = 14;
pub const DEFAULT_GLYPH_WIDTH: u32 = DEFAULT_CELL_WIDTH;
pub const DEFAULT_GLYPH_HEIGHT: u32 = DEFAULT_CELL_HEIGHT;

const ATLAS_COLUMNS: u32 = 16;
const ATLAS_ROWS: u32 = 16;
const ATLAS_WIDTH: u32 = ATLAS_COLUMNS * DEFAULT_CELL_WIDTH;
const COMPACT_ATLAS_HEIGHT: u32 = ATLAS_ROWS * DEFAULT_CELL_HEIGHT;
const VGA_ATLAS_HEIGHT: u32 = 256;
const EMBEDDED_DEFAULT_FONT: &[u8] = include_bytes!("../assets/fonts/unifont-mono-14.pbm");

/// Rejection reason for a terminal font atlas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontError {
    TooLarge,
    MissingHeader,
    InvalidMagic,
    InvalidNumber,
    InvalidDimensions,
    InvalidPixel,
    InvalidPixelCount,
}

/// A validated 16×16 glyph atlas with bit-packed pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitmapFont {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl BitmapFont {
    /// Parse the bounded ASCII PBM (P1) format used by PMos font assets.
    pub fn parse_p1(bytes: &[u8]) -> Result<Self, FontError> {
        if bytes.len() > MAX_FONT_BYTES {
            return Err(FontError::TooLarge);
        }

        let mut cursor = PbmCursor::new(bytes);
        let magic = cursor.next_token().ok_or(FontError::MissingHeader)?;
        if magic != b"P1" {
            return Err(FontError::InvalidMagic);
        }
        let width = parse_u32(cursor.next_token().ok_or(FontError::MissingHeader)?)?;
        let height = parse_u32(cursor.next_token().ok_or(FontError::MissingHeader)?)?;
        if width != ATLAS_WIDTH || (height != COMPACT_ATLAS_HEIGHT && height != VGA_ATLAS_HEIGHT) {
            return Err(FontError::InvalidDimensions);
        }

        let pixel_count = (width as usize) * (height as usize);
        let mut pixels = vec![0u8; pixel_count.div_ceil(8)];
        let mut parsed = 0usize;
        while let Some(pixel) = cursor.next_pixel()? {
            if parsed == pixel_count {
                return Err(FontError::InvalidPixelCount);
            }
            if pixel {
                pixels[parsed / 8] |= 1 << (7 - (parsed % 8));
            }
            parsed += 1;
        }
        if parsed != pixel_count {
            return Err(FontError::InvalidPixelCount);
        }

        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    pub const fn cell_width(&self) -> u32 {
        self.width / ATLAS_COLUMNS
    }

    pub const fn cell_height(&self) -> u32 {
        self.height / ATLAS_ROWS
    }

    pub const fn glyph_width(&self) -> u32 {
        self.cell_width()
    }

    pub const fn glyph_height(&self) -> u32 {
        self.cell_height()
    }

    /// Return a glyph pixel, mapping codepoints outside the 256-slot atlas to `?`.
    pub fn glyph_pixel(&self, ch: char, col: u32, row: u32) -> bool {
        if col >= self.cell_width() || row >= self.cell_height() {
            return false;
        }
        let (glyph_x, glyph_y) = self.glyph_origin(ch);
        self.pixel(glyph_x + col, glyph_y + row)
    }

    fn glyph_origin(&self, ch: char) -> (u32, u32) {
        let codepoint = if u32::from(ch) < ATLAS_COLUMNS * ATLAS_ROWS {
            u32::from(ch)
        } else {
            u32::from('?')
        };
        (
            (codepoint % ATLAS_COLUMNS) * self.cell_width(),
            (codepoint / ATLAS_COLUMNS) * self.cell_height(),
        )
    }

    fn pixel(&self, x: u32, y: u32) -> bool {
        let index = (y * self.width + x) as usize;
        self.pixels
            .get(index / 8)
            .is_some_and(|byte| byte & (1 << (7 - (index % 8))) != 0)
    }

    fn blank_safe_default() -> Self {
        let pixel_count = (ATLAS_WIDTH * COMPACT_ATLAS_HEIGHT) as usize;
        Self {
            width: ATLAS_WIDTH,
            height: COMPACT_ATLAS_HEIGHT,
            pixels: vec![0; pixel_count.div_ceil(8)],
        }
    }
}

/// Built-in last-resort font. The checked-in asset is validated by release tests;
/// a blank 8×14 atlas remains available if it is ever damaged during development.
pub fn default_font() -> &'static BitmapFont {
    static DEFAULT: OnceLock<BitmapFont> = OnceLock::new();
    DEFAULT.get_or_init(|| {
        BitmapFont::parse_p1(EMBEDDED_DEFAULT_FONT)
            .unwrap_or_else(|_| BitmapFont::blank_safe_default())
    })
}

/// Read the startup preference and selected VFS asset once. Any missing,
/// oversized, malformed, or unsupported input returns the embedded safe font.
pub fn load_startup_font() -> BitmapFont {
    let preferences = read_bounded(preferences::DEFAULT_PATH, MAX_PREFERENCES_BYTES);
    load_startup_font_with(preferences.as_deref(), |path| {
        read_bounded(path, MAX_FONT_BYTES)
    })
}

/// Testable startup loader with an injected VFS read boundary.
pub fn load_startup_font_with<F>(preference_bytes: Option<&[u8]>, mut read_font: F) -> BitmapFont
where
    F: FnMut(&str) -> Option<Vec<u8>>,
{
    let name = normalized_font_name(preference_bytes);
    let path = format!("{FONT_DIR}/{name}");
    let Some(bytes) = read_font(&path) else {
        return default_font().clone();
    };
    BitmapFont::parse_p1(&bytes).unwrap_or_else(|_| default_font().clone())
}

fn normalized_font_name(preference_bytes: Option<&[u8]>) -> &'static str {
    let Some(bytes) = preference_bytes.filter(|bytes| bytes.len() <= MAX_PREFERENCES_BYTES) else {
        return DEFAULT_FONT_NAME;
    };
    let Ok(preferences) = preferences::Preferences::parse(bytes) else {
        return DEFAULT_FONT_NAME;
    };
    match preferences.terminal_font.as_deref() {
        Some(VGA_FONT_NAME) => VGA_FONT_NAME,
        Some(DEFAULT_FONT_NAME) | None => DEFAULT_FONT_NAME,
        Some(_) => DEFAULT_FONT_NAME,
    }
}

fn read_bounded(path: &str, max_bytes: usize) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= max_bytes).then_some(bytes)
}

fn parse_u32(token: &[u8]) -> Result<u32, FontError> {
    let text = core::str::from_utf8(token).map_err(|_| FontError::InvalidNumber)?;
    text.parse().map_err(|_| FontError::InvalidNumber)
}

struct PbmCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> PbmCursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn next_token(&mut self) -> Option<&'a [u8]> {
        self.skip_trivia();
        let start = self.offset;
        while let Some(byte) = self.bytes.get(self.offset) {
            if byte.is_ascii_whitespace() || *byte == b'#' {
                break;
            }
            self.offset += 1;
        }
        (self.offset > start).then_some(&self.bytes[start..self.offset])
    }

    fn next_pixel(&mut self) -> Result<Option<bool>, FontError> {
        self.skip_trivia();
        let Some(byte) = self.bytes.get(self.offset).copied() else {
            return Ok(None);
        };
        self.offset += 1;
        match byte {
            b'0' => Ok(Some(false)),
            b'1' => Ok(Some(true)),
            _ => Err(FontError::InvalidPixel),
        }
    }

    fn skip_trivia(&mut self) {
        loop {
            while self
                .bytes
                .get(self.offset)
                .is_some_and(u8::is_ascii_whitespace)
            {
                self.offset += 1;
            }
            if self.bytes.get(self.offset) != Some(&b'#') {
                return;
            }
            while self
                .bytes
                .get(self.offset)
                .is_some_and(|byte| *byte != b'\n')
            {
                self.offset += 1;
            }
        }
    }
}

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
pub fn rasterize_snapshot(snapshot: &TerminalSnapshot, width: u32, height: u32) -> Vec<u8> {
    rasterize_snapshot_with_font(snapshot, width, height, default_font())
}

/// [`rasterize_snapshot`] with an explicit validated bitmap font.
pub fn rasterize_snapshot_with_font(
    snapshot: &TerminalSnapshot,
    width: u32,
    height: u32,
    font: &BitmapFont,
) -> Vec<u8> {
    rasterize_snapshot_with_palette_and_font(snapshot, width, height, Palette::default(), font)
}

/// [`rasterize_snapshot`] with an explicit palette.
pub fn rasterize_snapshot_with_palette(
    snapshot: &TerminalSnapshot,
    width: u32,
    height: u32,
    palette: Palette,
) -> Vec<u8> {
    rasterize_snapshot_with_palette_and_font(snapshot, width, height, palette, default_font())
}

/// Rasterize with an explicit palette and validated bitmap font.
pub fn rasterize_snapshot_with_palette_and_font(
    snapshot: &TerminalSnapshot,
    width: u32,
    height: u32,
    palette: Palette,
    font: &BitmapFont,
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
    let cols = text_width / font.cell_width();
    let rows_total = text_height / font.cell_height();
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
        let pixel_y = text_origin_y + row_idx as u32 * font.cell_height();
        let fg = palette.fg_for(line.kind);
        draw_line(
            &mut target,
            text_origin_x,
            pixel_y,
            cols,
            &line.text,
            fg,
            font,
        );
    }

    // Active input line at the bottom row, rendered as if
    // it were an Input line (prompt-coloured). The line
    // text is `snapshot.prompt + snapshot.input_buffer`.
    let input_row = scrollback_rows;
    let pixel_y = text_origin_y + input_row * font.cell_height();
    let mut combined = String::with_capacity(snapshot.prompt.len() + snapshot.input_buffer.len());
    combined.push_str(&snapshot.prompt);
    combined.push_str(&snapshot.input_buffer);
    draw_line(
        &mut target,
        text_origin_x,
        pixel_y,
        cols,
        &combined,
        palette.input,
        font,
    );

    // Cursor block right after the input text (clipped at
    // the right edge of the text area).
    let cursor_col = combined.chars().count() as u32;
    if cursor_col < cols {
        let cursor_x = text_origin_x + cursor_col * font.cell_width();
        fill_rect(
            &mut target,
            cursor_x,
            pixel_y,
            font.glyph_width(),
            font.glyph_height(),
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

fn draw_line(
    target: &mut Target,
    origin_x: u32,
    origin_y: u32,
    cols: u32,
    text: &str,
    fg: u32,
    font: &BitmapFont,
) {
    for (i, ch) in text.chars().enumerate() {
        let col = i as u32;
        if col >= cols {
            break;
        }
        let x0 = origin_x + col * font.cell_width();
        draw_glyph(target, font, ch, x0, origin_y, fg);
    }
}

fn draw_glyph(target: &mut Target, font: &BitmapFont, ch: char, x0: u32, y0: u32, fg: u32) {
    let (glyph_x, glyph_y) = font.glyph_origin(ch);
    for row in 0..font.glyph_height() {
        for col in 0..font.glyph_width() {
            if !font.pixel(glyph_x + col, glyph_y + row) {
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
