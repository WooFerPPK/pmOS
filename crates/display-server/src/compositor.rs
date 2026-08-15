//! Minimal software compositor.
//!
//! The v1 compositor owns a single [`Framebuffer`] — an
//! ARGB8888 pixel buffer of fixed dimensions — and exposes a
//! single primitive: blit a rectangular region from a source
//! buffer (a `pmd_buffer` as recorded in
//! [`crate::client::BufferInfo`]) into the framebuffer at a
//! caller-specified destination origin.
//!
//! This module is the pixel primitive rather than the scene graph:
//! [`crate::server::Server`] owns surfaces, global z-order, focus,
//! and full-scene recomposition, and calls this framebuffer's clipped
//! blit operation for each surface in the chosen order.
//!
//! All coordinates are signed 32-bit — the same type
//! `pmd_surface.attach` and `pmd_surface.damage` carry on the
//! wire — and the blit is clipped at framebuffer edges so a
//! misbehaving client cannot scribble outside the output.

use alloc::vec;
use alloc::vec::Vec;

use crate::client::BufferInfo;

/// Bytes per pixel for every format in v1. Both `ARGB8888`
/// and `XRGB8888` are 4 bytes; the distinction only
/// matters when a higher layer composes alpha-blending,
/// which the v1 compositor does not.
pub const BYTES_PER_PIXEL: usize = 4;

/// Default framebuffer width if [`Framebuffer::new`] is
/// called via the `Default` impl. Matches the v1 demo
/// display mode in the spec. Full frames present via the
/// chunked-blit op sequence (`OP_BLIT_BEGIN` + N ×
/// `OP_BLIT_CHUNK` + `OP_BLIT_END`) so the SAB ring's
/// 32 KiB heap window doesn't cap the frame size.
pub const DEFAULT_WIDTH: u32 = 1024;

/// Default framebuffer height if [`Framebuffer::new`] is
/// called via the `Default` impl.
pub const DEFAULT_HEIGHT: u32 = 768;

/// A rectangular ARGB8888 pixel buffer owned by the server.
/// Byte layout: `pixels[(y * width + x) * 4 .. + 4]` is the
/// `[B, G, R, A]` of the pixel at `(x, y)` in little-endian
/// ARGB8888 (the native shape produced by the web platform's
/// `ImageData`).
pub struct Framebuffer {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    clear_argb: u32,
}

impl Framebuffer {
    /// Allocate a zero-filled framebuffer of `width × height`
    /// pixels. Panics if `width * height * 4` would overflow
    /// `usize`; for v1 this is effectively a can't-happen
    /// since the caller picks a sane screen size.
    pub fn new(width: u32, height: u32) -> Self {
        let total = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(BYTES_PER_PIXEL))
            .expect("framebuffer dimensions overflow usize");
        Framebuffer {
            width,
            height,
            pixels: vec![0u8; total],
            clear_argb: 0,
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
        &self.pixels
    }

    /// Mutable access to the raw pixel bytes. Exposed so
    /// tests (and the kernel-side `Fb` driver bridge) can
    /// read the composed frame and, in tests, pre-populate
    /// the backbuffer with a known colour before asserting
    /// that a blit overwrote the right pixels.
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }

    /// Borrow one pixel's bytes: `[B, G, R, A]`. Returns
    /// `None` if `(x, y)` is out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> Option<&[u8]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let idx = ((y * self.width + x) as usize) * BYTES_PER_PIXEL;
        self.pixels.get(idx..idx + BYTES_PER_PIXEL)
    }

    /// Fill the entire framebuffer with a single ARGB
    /// value. Interpreted as little-endian: the low byte is
    /// `B`, then `G`, `R`, `A`. Used by tests to pre-paint a
    /// distinctive background so blits can be detected.
    pub fn clear(&mut self, argb: u32) {
        self.clear_argb = argb;
        self.fill(argb);
    }

    /// Clear the backbuffer to the configured scene background
    /// without changing that background. The server calls this
    /// before every full-scene composition pass.
    pub(crate) fn clear_for_composition(&mut self) {
        self.fill(self.clear_argb);
    }

    fn fill(&mut self, argb: u32) {
        let b = (argb & 0xff) as u8;
        let g = ((argb >> 8) & 0xff) as u8;
        let r = ((argb >> 16) & 0xff) as u8;
        let a = ((argb >> 24) & 0xff) as u8;
        for chunk in self.pixels.chunks_exact_mut(BYTES_PER_PIXEL) {
            chunk[0] = b;
            chunk[1] = g;
            chunk[2] = r;
            chunk[3] = a;
        }
    }

    /// Blit `src_bytes` (the storage of a `pmd_buffer`) into
    /// this framebuffer at destination origin `(dst_x,
    /// dst_y)`. `info` carries the source rectangle's
    /// geometry (`width`, `height`, `stride`).
    ///
    /// The copy is clipped at both the source's and
    /// destination's extents, so:
    ///
    /// * `dst_x`/`dst_y` can be negative — rows/columns
    ///   before `0` are skipped.
    /// * Pixels past `framebuffer.width()` or
    ///   `framebuffer.height()` are silently dropped.
    /// * A partial source (e.g. `src_bytes` is shorter than
    ///   `info.byte_end()`) is clipped to whatever bytes
    ///   the caller actually supplied.
    ///
    /// Returns the number of pixels written.
    pub fn blit_buffer(
        &mut self,
        info: &BufferInfo,
        src_bytes: &[u8],
        dst_x: i32,
        dst_y: i32,
    ) -> usize {
        let src_w = info.width as i32;
        let src_h = info.height as i32;
        if src_w <= 0 || src_h <= 0 {
            return 0;
        }
        let fb_w = self.width as i32;
        let fb_h = self.height as i32;
        // Row-major clipping. Treat dst origin as the
        // top-left of the blit in framebuffer space.
        let x0 = dst_x.max(0);
        let y0 = dst_y.max(0);
        let x1 = (dst_x + src_w).min(fb_w);
        let y1 = (dst_y + src_h).min(fb_h);
        if x1 <= x0 || y1 <= y0 {
            return 0;
        }
        let src_x_off = x0 - dst_x;
        let src_y_off = y0 - dst_y;
        let copy_w = (x1 - x0) as usize;
        let copy_h = (y1 - y0) as usize;
        let src_stride = info.stride as usize;
        let row_bytes = copy_w * BYTES_PER_PIXEL;
        let fb_stride = self.width as usize * BYTES_PER_PIXEL;
        let mut pixels_written = 0usize;
        for row in 0..copy_h {
            let src_row_start =
                src_stride * (src_y_off as usize + row) + (src_x_off as usize) * BYTES_PER_PIXEL;
            let src_row_end = src_row_start + row_bytes;
            if src_row_end > src_bytes.len() {
                // Partial source: stop at the last full row
                // the caller actually provided. Protects
                // against a buffer whose storage is smaller
                // than its declared geometry.
                break;
            }
            let dst_row_start = fb_stride * (y0 as usize + row) + (x0 as usize) * BYTES_PER_PIXEL;
            let dst_row_end = dst_row_start + row_bytes;
            self.pixels[dst_row_start..dst_row_end]
                .copy_from_slice(&src_bytes[src_row_start..src_row_end]);
            pixels_written += copy_w;
        }
        pixels_written
    }
}

impl Default for Framebuffer {
    fn default() -> Self {
        Framebuffer::new(DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }
}
