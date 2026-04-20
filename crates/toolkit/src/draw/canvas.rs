//! Client-side drawing primitives.
//!
//! A [`Canvas`] is a plain RGBA8888 pixel buffer with a
//! small set of primitives — `fill_rect`, `stroke_rect`,
//! `draw_text`, `set_pixel`, `clear`. Apps build up a
//! frame by constructing a canvas sized to match their
//! `pmd_buffer`, drawing into it, and then copying the
//! bytes into the shm pool the buffer lives in.
//!
//! Pixel layout: `pixels[(y * width + x) * 4 + 0..4]` is
//! `[R, G, B, A]` — the memory order consumed directly by
//! the web platform's `ImageData`, so that by the time a
//! buffer reaches the main-thread canvas no byte swap is
//! needed. The u32 colour constants use `0xAARRGGBB`
//! notation (alpha in the high byte) and [`Color::a`] /
//! `r` / `g` / `b` extract each channel for direct
//! comparison with the pixel bytes in tests.
//!
//! Out-of-range coordinates are silently clipped by
//! [`Canvas::set_pixel`] so the higher-level primitives
//! don't have to re-check bounds on every call. All
//! primitives are clip-safe.

use super::font::{glyph_for, glyph_pixel, CELL_WIDTH, GLYPH_HEIGHT, GLYPH_WIDTH};

/// Bytes per pixel in the canvas — always 4 (RGBA8888).
pub const BYTES_PER_PIXEL: usize = 4;

/// An ARGB colour carried as a single `u32` in
/// `0xAARRGGBB` layout (alpha in the high byte).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Color(u32);

impl Color {
    /// Construct from a `0xAARRGGBB` u32.
    pub const fn argb(argb: u32) -> Self {
        Color(argb)
    }

    /// Construct an opaque colour from three channel bytes.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color(
            0xff00_0000
                | ((r as u32) << 16)
                | ((g as u32) << 8)
                | (b as u32),
        )
    }

    /// Construct from four channel bytes.
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color(
            ((a as u32) << 24)
                | ((r as u32) << 16)
                | ((g as u32) << 8)
                | (b as u32),
        )
    }

    /// Fully transparent. Every channel is zero.
    pub const TRANSPARENT: Color = Color(0);

    pub const fn to_argb(self) -> u32 {
        self.0
    }

    pub const fn r(self) -> u8 {
        ((self.0 >> 16) & 0xff) as u8
    }
    pub const fn g(self) -> u8 {
        ((self.0 >> 8) & 0xff) as u8
    }
    pub const fn b(self) -> u8 {
        (self.0 & 0xff) as u8
    }
    pub const fn a(self) -> u8 {
        ((self.0 >> 24) & 0xff) as u8
    }
}

/// An axis-aligned rectangle in integer pixel coordinates.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Rect { x, y, width, height }
    }

    /// Right edge (exclusive).
    pub const fn right(&self) -> i32 {
        self.x + self.width as i32
    }

    /// Bottom edge (exclusive).
    pub const fn bottom(&self) -> i32 {
        self.y + self.height as i32
    }

    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Internal pixel storage for [`Canvas`] — either an
/// owned `Vec<u8>` (produced by [`Canvas::new`]) or a
/// mutably borrowed slice (produced by
/// [`Canvas::from_slice`], used by
/// [`crate::draw::BufferPool`] to hand the back buffer
/// out to callers for painting without copying).
enum CanvasStorage<'a> {
    Owned(Vec<u8>),
    Borrowed(&'a mut [u8]),
}

impl<'a> CanvasStorage<'a> {
    fn as_slice(&self) -> &[u8] {
        match self {
            CanvasStorage::Owned(v) => v.as_slice(),
            CanvasStorage::Borrowed(s) => s,
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            CanvasStorage::Owned(v) => v.as_mut_slice(),
            CanvasStorage::Borrowed(s) => s,
        }
    }
}

/// A plain RGBA8888 pixel buffer.
///
/// The lifetime parameter tracks the borrow of an
/// externally-owned pixel slice when the canvas is built
/// through [`Canvas::from_slice`]; [`Canvas::new`] returns
/// a `Canvas<'static>` backed by its own `Vec<u8>` so
/// existing call sites keep compiling with inferred
/// lifetime.
pub struct Canvas<'a> {
    width: u32,
    height: u32,
    pixels: CanvasStorage<'a>,
}

impl Canvas<'static> {
    /// Allocate a fresh zero-filled canvas of `width ×
    /// height` pixels, backed by a fresh `Vec<u8>`.
    ///
    /// # Panics
    ///
    /// Panics if `width * height * 4` would overflow
    /// `usize`. In practice the caller picks a sane
    /// buffer size.
    pub fn new(width: u32, height: u32) -> Self {
        let total = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
            .expect("canvas dimensions overflow usize");
        Canvas {
            width,
            height,
            pixels: CanvasStorage::Owned(vec![0u8; total]),
        }
    }
}

impl<'a> Canvas<'a> {
    /// Wrap an externally-owned pixel slice in a
    /// [`Canvas`] without copying. The slice's length must
    /// exactly equal `width * height * 4`; callers pass in
    /// the back-buffer view returned by
    /// [`crate::draw::BufferPool::acquire_back_canvas`].
    ///
    /// # Panics
    ///
    /// Panics if `pixels.len()` does not equal
    /// `width * height * 4`.
    pub fn from_slice(pixels: &'a mut [u8], width: u32, height: u32) -> Self {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
            .expect("canvas dimensions overflow usize");
        assert_eq!(
            pixels.len(),
            expected,
            "slice length {} does not match {}x{}x{}",
            pixels.len(),
            width,
            height,
            BYTES_PER_PIXEL
        );
        Canvas {
            width,
            height,
            pixels: CanvasStorage::Borrowed(pixels),
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Borrow the raw pixel bytes. Length is
    /// `width * height * 4`.
    pub fn pixels(&self) -> &[u8] {
        self.pixels.as_slice()
    }

    /// Mutable raw bytes. Useful when an external caller
    /// (e.g. a font rasterizer or a blit source) writes
    /// directly into the canvas backbuffer.
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        self.pixels.as_mut_slice()
    }
}

impl Canvas<'static> {
    /// Take ownership of the pixel bytes, consuming the
    /// canvas. The caller typically ships the bytes into
    /// an shm pool via `toolkit::Client::shm_pool_create_buffer`.
    ///
    /// Only available on `Canvas<'static>` (the owning
    /// variant from [`Canvas::new`]); a borrowed canvas
    /// has no `Vec<u8>` to give up.
    pub fn into_pixels(self) -> Vec<u8> {
        match self.pixels {
            CanvasStorage::Owned(v) => v,
            CanvasStorage::Borrowed(_) => {
                unreachable!("Canvas<'static>::into_pixels always has owned storage")
            }
        }
    }
}

impl<'a> Canvas<'a> {
    /// Borrow one pixel as `[R, G, B, A]`. Returns `None`
    /// if `(x, y)` is out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> Option<&[u8]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let idx = ((y * self.width + x) as usize) * BYTES_PER_PIXEL;
        self.pixels.as_slice().get(idx..idx + BYTES_PER_PIXEL)
    }

    /// Paint every pixel with `color`.
    pub fn clear(&mut self, color: Color) {
        let (r, g, b, a) = (color.r(), color.g(), color.b(), color.a());
        for chunk in self.pixels.as_mut_slice().chunks_exact_mut(BYTES_PER_PIXEL) {
            chunk[0] = r;
            chunk[1] = g;
            chunk[2] = b;
            chunk[3] = a;
        }
    }

    /// Set one pixel to `color`. Out-of-range coordinates
    /// are silently clipped.
    pub fn set_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return;
        }
        let idx = ((y as u32 * self.width + x as u32) as usize) * BYTES_PER_PIXEL;
        let pixels = self.pixels.as_mut_slice();
        if idx + BYTES_PER_PIXEL > pixels.len() {
            return;
        }
        pixels[idx] = color.r();
        pixels[idx + 1] = color.g();
        pixels[idx + 2] = color.b();
        pixels[idx + 3] = color.a();
    }

    /// Fill every pixel inside `rect` with `color`. Clipped
    /// at the canvas edges.
    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        if rect.is_empty() {
            return;
        }
        let x0 = rect.x.max(0);
        let y0 = rect.y.max(0);
        let x1 = rect.right().min(self.width as i32);
        let y1 = rect.bottom().min(self.height as i32);
        for y in y0..y1 {
            for x in x0..x1 {
                self.set_pixel(x, y, color);
            }
        }
    }

    /// Draw a one-pixel outline of `rect` with `color`.
    /// Clipped at the canvas edges. A rectangle with
    /// `width == 0 || height == 0` draws nothing.
    pub fn stroke_rect(&mut self, rect: Rect, color: Color) {
        if rect.is_empty() {
            return;
        }
        let x0 = rect.x;
        let y0 = rect.y;
        let x1 = rect.right() - 1;
        let y1 = rect.bottom() - 1;
        // Top + bottom edges.
        for x in x0..=x1 {
            self.set_pixel(x, y0, color);
            if y1 != y0 {
                self.set_pixel(x, y1, color);
            }
        }
        // Left + right edges (skip corners already drawn).
        if y1 > y0 + 1 {
            for y in (y0 + 1)..y1 {
                self.set_pixel(x0, y, color);
                if x1 != x0 {
                    self.set_pixel(x1, y, color);
                }
            }
        }
    }

    /// Draw `text` starting at `(x, y)` in `color`. Each
    /// glyph is looked up in the shared bitmap font and
    /// its "on" pixels become `color`; "off" pixels are
    /// left alone. Advances by [`CELL_WIDTH`] per character.
    /// Returns the total width of the drawn text in pixels.
    pub fn draw_text(&mut self, x: i32, y: i32, text: &str, color: Color) -> u32 {
        let mut pen_x = x;
        let mut drawn = 0u32;
        for ch in text.chars() {
            let glyph = glyph_for(ch);
            for row in 0..GLYPH_HEIGHT {
                for col in 0..GLYPH_WIDTH {
                    if glyph_pixel(glyph, col, row) {
                        self.set_pixel(pen_x + col as i32, y + row as i32, color);
                    }
                }
            }
            pen_x += CELL_WIDTH as i32;
            drawn += CELL_WIDTH;
        }
        drawn
    }
}
