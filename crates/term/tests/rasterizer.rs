//! Unit tests for the `term::rasterizer` module.
//!
//! Covers the font lookup helpers, palette defaults, and
//! the pixel layout `rasterize_snapshot` produces for a
//! handful of representative terminal states.

use std::cell::RefCell;

use term::rasterizer::{
    colors, default_font, load_startup_font_with, rasterize_snapshot, rasterize_snapshot_with_font,
    BitmapFont, FontError, Palette, DEFAULT_FONT_NAME, FONT_DIR, MAX_FONT_BYTES,
    MAX_PREFERENCES_BYTES, PADDING, VGA_FONT_NAME,
};
use term::{Key, LineKind, Terminal, TerminalLine, TerminalOptions, TerminalSnapshot};
use toolkit::draw::font::{glyph_for, glyph_pixel};

const COMPACT_FONT: &[u8] = include_bytes!("../assets/fonts/unifont-mono-14.pbm");
const VGA_FONT: &[u8] = include_bytes!("../assets/fonts/pc-vga-16.pbm");

fn bg_pixel() -> [u8; 4] {
    // 0xFF0A0E14 → (B=14, G=0E, R=0A, A=FF).
    [0x14, 0x0E, 0x0A, 0xFF]
}

fn pixel(buffer: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * width + x) as usize) * 4;
    [
        buffer[idx],
        buffer[idx + 1],
        buffer[idx + 2],
        buffer[idx + 3],
    ]
}

#[test]
fn glyph_for_space_returns_all_zero_rows() {
    let g = glyph_for(' ');
    assert!(g.iter().all(|r| *r == 0));
}

#[test]
fn glyph_for_known_letters_have_non_zero_rows() {
    for ch in [
        'a', 'b', 'c', 'e', 'h', 'i', 'l', 'm', 'n', 'o', 'p', 'r', 's', 't', 'u', 'x', 'y',
    ] {
        let g = glyph_for(ch);
        assert!(
            g.iter().any(|r| *r != 0),
            "letter {ch} must have a non-empty glyph"
        );
    }
}

#[test]
fn glyph_for_unmapped_codepoint_falls_back_to_hollow_square() {
    // 0x60 '`' — deliberately left as zeros in FONT_DATA
    // so it promotes to UNKNOWN_GLYPH.
    let g = glyph_for('`');
    // UNKNOWN_GLYPH is a 5x7 hollow square — four corners
    // and a full top + bottom row.
    assert_eq!(g[0], 0b11111);
    assert_eq!(g[6], 0b11111);
    assert_eq!(g[3], 0b10001);
}

#[test]
fn glyph_for_non_ascii_codepoint_returns_unknown() {
    let g = glyph_for('π');
    assert_eq!(g[0], 0b11111);
}

#[test]
fn glyph_pixel_extracts_per_pixel_flags_correctly() {
    // Digit '1' has a distinctive left stem pattern.
    let g = glyph_for('1');
    // Row 0 is `0b00100` — only column 2 is set.
    assert!(!glyph_pixel(g, 0, 0));
    assert!(!glyph_pixel(g, 1, 0));
    assert!(glyph_pixel(g, 2, 0));
    assert!(!glyph_pixel(g, 3, 0));
    assert!(!glyph_pixel(g, 4, 0));
    // Out-of-range coords return false.
    assert!(!glyph_pixel(g, 5, 0));
    assert!(!glyph_pixel(g, 0, 7));
}

#[test]
fn rasterize_produces_buffer_of_requested_size() {
    let snap = TerminalSnapshot::default();
    let pixels = rasterize_snapshot(&snap, 64, 32);
    assert_eq!(pixels.len(), 64 * 32 * 4);
}

#[test]
fn rasterize_empty_snapshot_fills_background_then_draws_only_the_cursor_and_prompt() {
    // Empty snapshot means the input row has nothing but
    // the prompt + cursor. Everywhere else should be the
    // default background colour.
    let snap = TerminalSnapshot {
        lines: Vec::new(),
        input_buffer: String::new(),
        prompt: String::new(),
    };
    let width = 64u32;
    let height = 32u32;
    let pixels = rasterize_snapshot(&snap, width, height);

    // Corners are background.
    assert_eq!(pixel(&pixels, width, 0, 0), bg_pixel());
    assert_eq!(pixel(&pixels, width, width - 1, 0), bg_pixel());
    assert_eq!(pixel(&pixels, width, 0, height - 1), bg_pixel());
    assert_eq!(pixel(&pixels, width, width - 1, height - 1), bg_pixel());
}

#[test]
fn rasterize_with_prompt_draws_cursor_block_at_the_expected_origin() {
    let snap = TerminalSnapshot {
        lines: Vec::new(),
        input_buffer: String::new(),
        prompt: "> ".to_string(),
    };
    let width = 120u32;
    let height = 64u32;
    let pixels = rasterize_snapshot(&snap, width, height);

    // The cursor block is at column 2 (after "> "), on
    // the bottom text row. Figure out the expected origin:
    let text_origin_x = PADDING;
    let text_origin_y = PADDING;
    let text_height = height - 2 * PADDING;
    let font = default_font();
    let rows_total = text_height / font.cell_height();
    let scrollback_rows = rows_total - 1;
    let input_row_y = text_origin_y + scrollback_rows * font.cell_height();
    let cursor_x = text_origin_x + 2 * font.cell_width();

    // Check one pixel in the middle of the cursor block
    // is the cursor colour (0xFFFFFFFF → [FF,FF,FF,FF]).
    let cx = cursor_x + font.glyph_width() / 2;
    let cy = input_row_y + font.glyph_height() / 2;
    assert_eq!(pixel(&pixels, width, cx, cy), [0xFF, 0xFF, 0xFF, 0xFF]);
}

#[test]
fn rasterize_scrollback_line_paints_foreground_pixels_at_expected_cell() {
    // A single scrollback output line "o" — render and
    // check that at least one pixel inside the glyph's
    // cell carries the default output foreground colour.
    let snap = TerminalSnapshot {
        lines: vec![TerminalLine {
            text: "o".to_string(),
            kind: LineKind::Output,
        }],
        input_buffer: String::new(),
        prompt: String::new(),
    };
    let width = 120u32;
    let height = 64u32;
    let pixels = rasterize_snapshot(&snap, width, height);

    // Default output fg is 0xFFE6E6E6 → [E6, E6, E6, FF].
    let fg = [0xE6, 0xE6, 0xE6, 0xFF];
    let base_x = PADDING;
    let base_y = PADDING;

    // Scan the entire selected-font cell at (base_x, base_y). At
    // least one pixel should be the output foreground
    // colour because the 'o' glyph has non-zero rows.
    let mut found = false;
    for row in 0..default_font().glyph_height() {
        for col in 0..default_font().glyph_width() {
            let px = pixel(&pixels, width, base_x + col, base_y + row);
            if px == fg {
                found = true;
            }
        }
    }
    assert!(found, "output glyph did not paint any foreground pixels");
}

#[test]
fn rasterize_paints_different_line_kinds_with_different_colours() {
    let snap = TerminalSnapshot {
        lines: vec![
            TerminalLine {
                text: "B".to_string(),
                kind: LineKind::Banner,
            },
            TerminalLine {
                text: "I".to_string(),
                kind: LineKind::Input,
            },
            TerminalLine {
                text: "O".to_string(),
                kind: LineKind::Output,
            },
            TerminalLine {
                text: "E".to_string(),
                kind: LineKind::Error,
            },
        ],
        input_buffer: String::new(),
        prompt: String::new(),
    };
    let width = 120u32;
    let height = 80u32;
    let pixels = rasterize_snapshot(&snap, width, height);

    // For each of the 4 lines, find the first foreground
    // pixel within that row's glyph cell and verify it
    // matches the palette entry.
    let palette = Palette::default();
    let expected = [
        (LineKind::Banner, palette.banner),
        (LineKind::Input, palette.input),
        (LineKind::Output, palette.output),
        (LineKind::Error, palette.error),
    ];

    for (row_idx, (_kind, argb)) in expected.iter().enumerate() {
        let base_x = PADDING;
        let base_y = PADDING + (row_idx as u32) * default_font().cell_height();
        let (b, g, r, a) = (
            (*argb & 0xff) as u8,
            ((*argb >> 8) & 0xff) as u8,
            ((*argb >> 16) & 0xff) as u8,
            ((*argb >> 24) & 0xff) as u8,
        );
        let expected_px = [b, g, r, a];
        let mut found = false;
        for row in 0..default_font().glyph_height() {
            for col in 0..default_font().glyph_width() {
                let px = pixel(&pixels, width, base_x + col, base_y + row);
                if px == expected_px {
                    found = true;
                }
            }
        }
        assert!(
            found,
            "row {row_idx} did not produce the expected foreground pixel {expected_px:?}"
        );
    }
}

#[test]
fn rasterize_clips_line_longer_than_the_column_count() {
    // With a 16-pixel-wide text area and PADDING=4, we
    // have exactly two 8-pixel cells
    // visible columns. A line of "aaaa" should clip to
    // 2 characters without panicking.
    let snap = TerminalSnapshot {
        lines: vec![TerminalLine {
            text: "aaaa".to_string(),
            kind: LineKind::Output,
        }],
        input_buffer: String::new(),
        prompt: String::new(),
    };
    // width = 16 + 2*PADDING = 24.
    let width = 24u32;
    let height = 32u32;
    let pixels = rasterize_snapshot(&snap, width, height);
    assert_eq!(pixels.len() as u32, width * height * 4);
}

#[test]
fn rasterize_tiny_framebuffer_is_all_background() {
    // Below 2*PADDING on either axis → short-circuits.
    let snap = TerminalSnapshot::default();
    let pixels = rasterize_snapshot(&snap, 4, 4);
    for px in pixels.chunks_exact(4) {
        assert_eq!(px, &bg_pixel());
    }
}

#[test]
fn rasterize_integrates_with_terminal_feed_key_plus_snapshot() {
    // Drive a terminal with feed_key then rasterize —
    // this is the end-to-end path the session uses.
    let mut term = Terminal::new(TerminalOptions {
        max_lines: 16,
        banner: vec!["pmos".to_string()],
        prompt: "> ".to_string(),
    });
    for ch in "echo hi".chars() {
        term.feed_key(Key::Char(ch));
    }
    let _ = term.feed_key(Key::Enter);
    let snap = term.snapshot();
    let pixels = rasterize_snapshot(&snap, 160, 64);

    // Something was drawn — total non-bg pixel count > 0.
    let bg = bg_pixel();
    let non_bg: usize = pixels
        .chunks_exact(4)
        .filter(|px| {
            let arr: [u8; 4] = [px[0], px[1], px[2], px[3]];
            arr != bg
        })
        .count();
    assert!(non_bg > 0, "expected rasterizer to paint foreground pixels");
}

#[test]
fn default_palette_matches_exported_colour_constants() {
    let p = Palette::default();
    assert_eq!(p.bg, colors::BG);
    assert_eq!(p.banner, colors::FG_BANNER);
    assert_eq!(p.input, colors::FG_INPUT);
    assert_eq!(p.output, colors::FG_OUTPUT);
    assert_eq!(p.error, colors::FG_ERROR);
    assert_eq!(p.cursor, colors::CURSOR);
}

#[test]
fn palette_fg_for_returns_different_colours_per_line_kind() {
    let p = Palette::default();
    let b = p.fg_for(LineKind::Banner);
    let i = p.fg_for(LineKind::Input);
    let o = p.fg_for(LineKind::Output);
    let e = p.fg_for(LineKind::Error);
    assert_ne!(b, i);
    assert_ne!(b, o);
    assert_ne!(i, o);
    assert_ne!(o, e);
}

#[test]
fn shipped_p1_atlases_have_distinct_dynamic_metrics_and_frames() {
    let compact = BitmapFont::parse_p1(COMPACT_FONT).expect("compact font parses");
    let vga = BitmapFont::parse_p1(VGA_FONT).expect("VGA font parses");
    assert_eq!((compact.cell_width(), compact.cell_height()), (8, 14));
    assert_eq!((vga.cell_width(), vga.cell_height()), (8, 16));
    assert_eq!((term::CELL_WIDTH, term::CELL_HEIGHT), (8, 14));
    assert_eq!((term::GLYPH_WIDTH, term::GLYPH_HEIGHT), (8, 14));

    let snapshot = TerminalSnapshot {
        lines: vec![TerminalLine {
            text: "PMos Aa0?".to_string(),
            kind: LineKind::Output,
        }],
        input_buffer: "font".to_string(),
        prompt: "$ ".to_string(),
    };
    let compact_frame = rasterize_snapshot_with_font(&snapshot, 160, 80, &compact);
    let vga_frame = rasterize_snapshot_with_font(&snapshot, 160, 80, &vga);
    assert_ne!(compact_frame, vga_frame);
}

#[test]
fn p1_parser_rejects_malformed_and_oversized_assets() {
    assert_eq!(
        BitmapFont::parse_p1(b"P4\n128 224\n0"),
        Err(FontError::InvalidMagic)
    );
    assert_eq!(
        BitmapFont::parse_p1(b"P1\n128 224\n0"),
        Err(FontError::InvalidPixelCount)
    );
    assert_eq!(
        BitmapFont::parse_p1(&vec![b'0'; MAX_FONT_BYTES + 1]),
        Err(FontError::TooLarge)
    );
}

#[test]
fn startup_loader_selects_vga_from_the_shared_preference() {
    let preferences = b"[terminal]\nfont = \"pc-vga-16.pbm\"\n";
    let requested = RefCell::new(Vec::new());
    let font = load_startup_font_with(Some(preferences), |path| {
        requested.borrow_mut().push(path.to_string());
        (path == format!("{FONT_DIR}/{VGA_FONT_NAME}")).then(|| VGA_FONT.to_vec())
    });
    assert_eq!(font.cell_width(), 8);
    assert_eq!(font.cell_height(), 16);
    assert_eq!(
        requested.into_inner(),
        vec![format!("{FONT_DIR}/{VGA_FONT_NAME}")]
    );
}

#[test]
fn malformed_or_oversized_startup_inputs_fall_back_safely() {
    let malformed_preferences = b"[terminal\nfont = \"pc-vga-16.pbm\"\n";
    let malformed = load_startup_font_with(Some(malformed_preferences), |_| {
        Some(b"P1\n128 224\nnot-pixels".to_vec())
    });
    assert_eq!(malformed, default_font().clone());

    let oversized_preferences = vec![b'x'; MAX_PREFERENCES_BYTES + 1];
    let oversized_font = vec![b'0'; MAX_FONT_BYTES + 1];
    let oversized = load_startup_font_with(Some(&oversized_preferences), |_| {
        Some(oversized_font.clone())
    });
    assert_eq!(oversized, default_font().clone());
}

#[test]
fn unsupported_font_name_cannot_escape_the_bundled_allow_list() {
    let preferences = b"[terminal]\nfont = \"../../host-file.pbm\"\n";
    let requested = RefCell::new(Vec::new());
    let font = load_startup_font_with(Some(preferences), |path| {
        requested.borrow_mut().push(path.to_string());
        Some(COMPACT_FONT.to_vec())
    });
    assert_eq!(font.cell_height(), 14);
    assert_eq!(
        requested.into_inner(),
        vec![format!("{FONT_DIR}/{DEFAULT_FONT_NAME}")]
    );
}
