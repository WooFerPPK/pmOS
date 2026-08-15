//! Framebuffer / compositor primitive tests.

use display_server::client::BufferInfo;
use display_server::{Framebuffer, ObjectId, BYTES_PER_PIXEL};

/// Build a `BufferInfo` with a tight stride (width * 4
/// bytes). Useful for constructing test source buffers.
fn tight_info(width: u32, height: u32) -> BufferInfo {
    BufferInfo {
        pool_id: ObjectId::new(1),
        offset: 0,
        width,
        height,
        stride: width * BYTES_PER_PIXEL as u32,
        format: 0, /* ARGB8888 */
    }
}

/// Fill a Vec with a repeating 4-byte ARGB8888 pattern so
/// the blit is easy to assert against.
fn solid(width: usize, height: usize, rgba: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * 4);
    for _ in 0..(width * height) {
        out.extend_from_slice(&rgba);
    }
    out
}

#[test]
fn new_framebuffer_is_zero_filled_at_the_requested_size() {
    let fb = Framebuffer::new(8, 4);
    assert_eq!(fb.width(), 8);
    assert_eq!(fb.height(), 4);
    assert_eq!(fb.pixels().len(), 8 * 4 * 4);
    assert!(fb.pixels().iter().all(|b| *b == 0));
}

#[test]
fn clear_writes_argb_values_in_bgra_byte_order() {
    let mut fb = Framebuffer::new(2, 2);
    // 0xAARRGGBB = 0xFF804020 → (B=20, G=40, R=80, A=FF).
    fb.clear(0xFF80_4020);
    for px in fb.pixels().chunks_exact(4) {
        assert_eq!(px, &[0x20, 0x40, 0x80, 0xFF]);
    }
}

#[test]
fn pixel_returns_none_out_of_bounds() {
    let fb = Framebuffer::new(2, 2);
    assert!(fb.pixel(0, 0).is_some());
    assert!(fb.pixel(1, 1).is_some());
    assert!(fb.pixel(2, 1).is_none());
    assert!(fb.pixel(1, 2).is_none());
    assert!(fb.pixel(100, 100).is_none());
}

#[test]
fn blit_full_buffer_at_origin_overwrites_exactly_the_source_rectangle() {
    let mut fb = Framebuffer::new(4, 4);
    fb.clear(0xFF00_0000); // pre-paint black
    let src = solid(2, 2, [0x10, 0x20, 0x30, 0xFF]);
    let info = tight_info(2, 2);

    let written = fb.blit_buffer(&info, &src, 0, 0);
    assert_eq!(written, 4);

    // (0,0) and (0,1) and (1,0) and (1,1) carry src.
    for (x, y) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
        assert_eq!(fb.pixel(x, y).unwrap(), &[0x10, 0x20, 0x30, 0xFF]);
    }
    // (2,0) is still black.
    assert_eq!(fb.pixel(2, 0).unwrap(), &[0x00, 0x00, 0x00, 0xFF]);
    assert_eq!(fb.pixel(3, 3).unwrap(), &[0x00, 0x00, 0x00, 0xFF]);
}

#[test]
fn blit_with_negative_dst_clips_leading_rows_and_columns() {
    let mut fb = Framebuffer::new(4, 4);
    let src = solid(4, 4, [0xAA, 0xBB, 0xCC, 0xFF]);
    let info = tight_info(4, 4);
    let written = fb.blit_buffer(&info, &src, -2, -1);
    // 2 cols × 3 rows = 6 pixels visible.
    assert_eq!(written, 2 * 3);
    // The visible rectangle is fb[0..2, 0..3].
    for x in 0..2u32 {
        for y in 0..3u32 {
            assert_eq!(fb.pixel(x, y).unwrap(), &[0xAA, 0xBB, 0xCC, 0xFF]);
        }
    }
    // Outside should still be zero.
    assert_eq!(fb.pixel(2, 0).unwrap(), &[0; 4]);
    assert_eq!(fb.pixel(0, 3).unwrap(), &[0; 4]);
}

#[test]
fn blit_past_right_and_bottom_edges_is_clipped() {
    let mut fb = Framebuffer::new(4, 4);
    let src = solid(4, 4, [0x11, 0x22, 0x33, 0xFF]);
    let info = tight_info(4, 4);
    // Origin at (2, 3) — only a 2×1 sub-rect fits.
    let written = fb.blit_buffer(&info, &src, 2, 3);
    assert_eq!(written, 2);
    assert_eq!(fb.pixel(2, 3).unwrap(), &[0x11, 0x22, 0x33, 0xFF]);
    assert_eq!(fb.pixel(3, 3).unwrap(), &[0x11, 0x22, 0x33, 0xFF]);
    // Nothing above row 3.
    assert_eq!(fb.pixel(2, 2).unwrap(), &[0; 4]);
}

#[test]
fn blit_entirely_off_screen_writes_nothing() {
    let mut fb = Framebuffer::new(4, 4);
    fb.clear(0xFF00_0000);
    let src = solid(2, 2, [0xFF, 0xFF, 0xFF, 0xFF]);
    let info = tight_info(2, 2);

    assert_eq!(fb.blit_buffer(&info, &src, -10, 0), 0);
    assert_eq!(fb.blit_buffer(&info, &src, 0, -10), 0);
    assert_eq!(fb.blit_buffer(&info, &src, 10, 0), 0);
    assert_eq!(fb.blit_buffer(&info, &src, 0, 10), 0);
    assert_eq!(fb.blit_buffer(&info, &src, 4, 4), 0);

    // Nothing changed.
    for px in fb.pixels().chunks_exact(4) {
        assert_eq!(px, &[0x00, 0x00, 0x00, 0xFF]);
    }
}

#[test]
fn blit_with_wide_stride_skips_padding_bytes() {
    // Source is 2x2 visible pixels but stride is 16 bytes
    // (double-wide): each row is 4 pixels wide in memory
    // and only the first 2 pixels are "real".
    let mut fb = Framebuffer::new(4, 2);
    let src = {
        let mut out = Vec::new();
        // row 0: red red _ _
        out.extend_from_slice(&[0, 0, 0xFF, 0xFF]);
        out.extend_from_slice(&[0, 0, 0xFF, 0xFF]);
        out.extend_from_slice(&[0xAA, 0xAA, 0xAA, 0xAA]); // padding
        out.extend_from_slice(&[0xAA, 0xAA, 0xAA, 0xAA]); // padding
                                                          // row 1: green green _ _
        out.extend_from_slice(&[0, 0xFF, 0, 0xFF]);
        out.extend_from_slice(&[0, 0xFF, 0, 0xFF]);
        out.extend_from_slice(&[0xAA, 0xAA, 0xAA, 0xAA]); // padding
        out.extend_from_slice(&[0xAA, 0xAA, 0xAA, 0xAA]); // padding
        out
    };
    let info = BufferInfo {
        pool_id: ObjectId::new(1),
        offset: 0,
        width: 2,
        height: 2,
        stride: 16, // 4 pixels × 4 bytes
        format: 0,
    };
    let written = fb.blit_buffer(&info, &src, 0, 0);
    assert_eq!(written, 4);

    // Destination has red at (0,0), (1,0) and green at
    // (0,1), (1,1). Padding bytes were NOT copied.
    assert_eq!(fb.pixel(0, 0).unwrap(), &[0, 0, 0xFF, 0xFF]);
    assert_eq!(fb.pixel(1, 0).unwrap(), &[0, 0, 0xFF, 0xFF]);
    assert_eq!(fb.pixel(0, 1).unwrap(), &[0, 0xFF, 0, 0xFF]);
    assert_eq!(fb.pixel(1, 1).unwrap(), &[0, 0xFF, 0, 0xFF]);
    // Columns 2-3 never got written — they should still
    // be zero.
    assert_eq!(fb.pixel(2, 0).unwrap(), &[0; 4]);
    assert_eq!(fb.pixel(3, 1).unwrap(), &[0; 4]);
}

#[test]
fn blit_with_partial_source_stops_at_the_last_full_row() {
    // `info` declares a 4×4 buffer (64 bytes) but the
    // caller only provides 32 bytes. We should blit the
    // 2 rows that DO fit and skip the rest rather than
    // panic on the slice index.
    let mut fb = Framebuffer::new(4, 4);
    let src = solid(4, 2, [0x99, 0x88, 0x77, 0xFF]); // 32 bytes
    let info = tight_info(4, 4);
    let written = fb.blit_buffer(&info, &src, 0, 0);
    assert_eq!(written, 4 * 2);
    assert_eq!(fb.pixel(0, 1).unwrap(), &[0x99, 0x88, 0x77, 0xFF]);
    // Row 2 onwards — never blitted.
    assert_eq!(fb.pixel(0, 2).unwrap(), &[0; 4]);
}

#[test]
fn blit_with_zero_sized_source_writes_nothing() {
    let mut fb = Framebuffer::new(4, 4);
    let info = BufferInfo {
        pool_id: ObjectId::new(1),
        offset: 0,
        width: 0,
        height: 0,
        stride: 0,
        format: 0,
    };
    assert_eq!(fb.blit_buffer(&info, &[], 0, 0), 0);
}

#[test]
fn default_framebuffer_uses_default_dimensions() {
    let fb = Framebuffer::default();
    assert_eq!(fb.width(), display_server::DEFAULT_WIDTH);
    assert_eq!(fb.height(), display_server::DEFAULT_HEIGHT);
}
