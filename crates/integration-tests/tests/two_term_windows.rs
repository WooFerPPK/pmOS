//! Visual integration: two `term::Terminal` instances, each
//! rasterized into a sub-region of a shared framebuffer-sized
//! [`toolkit::draw::Canvas`], each wrapped in a
//! [`toolkit::widget::WindowFrame`] showing the `pmos.term`
//! app-id.
//!
//! This is the first end-to-end composition test that
//! proves `WindowFrame` + `Canvas` + the existing term
//! rasterizer compose cleanly. No display server, no
//! compositor, no IPC — the output is a single `Vec<u8>` of
//! framebuffer pixels that we assert on pixel-wise.
//!
//! The two terminals are positioned in a staircase so that
//! the **second window overlaps the first along the bottom
//! border and part of the content area**, which is the
//! minimal visual setup that will stress the real
//! compositor's z-order + damage clipping once T107's
//! compositor takes over. Here it just proves two windows
//! can coexist on the same canvas without corrupting each
//! other's chrome.
//!
//! When T107 lands for real, this test gets repointed at
//! the display server's compositor loopback and keeps its
//! assertions — the rasterizer + WindowFrame pair will
//! stay exactly where they are.

use term::{
    rasterizer::{rasterize_snapshot, BYTES_PER_PIXEL as TERM_BPP},
    Terminal, TerminalOptions,
};
use toolkit::draw::{Canvas, Color, Rect};
use toolkit::theme::Theme;
use toolkit::widget::frame::{WindowFrame, BORDER_WIDTH, TITLEBAR_HEIGHT};

/// Framebuffer dimensions for the composition. Small enough
/// to keep test runtime and byte budget trivial; large enough
/// to fit two 280x160 windows in a staircase layout.
const FB_WIDTH: u32 = 640;
const FB_HEIGHT: u32 = 400;

/// Window dimensions (identical for both terms).
const WIN_WIDTH: u32 = 280;
const WIN_HEIGHT: u32 = 160;

/// Blit a tight-stride RGBA8888 `src` of size `src_w × src_h`
/// into `canvas` at `(dest_x, dest_y)`. Out-of-range rows
/// and columns are clipped. Lives here (not in
/// `toolkit::draw::Canvas`) because image blit is explicitly
/// deferred until T118 follow-up slices decide on a proper
/// bitmap source type — for the test we only need a plain
/// byte-slice copy.
fn blit_rgba(canvas: &mut Canvas, dest_x: i32, dest_y: i32, src: &[u8], src_w: u32, src_h: u32) {
    assert_eq!(
        src.len(),
        (src_w as usize) * (src_h as usize) * TERM_BPP,
        "blit source byte length must match src_w * src_h * 4",
    );
    if src_w == 0 || src_h == 0 {
        return;
    }
    let canvas_w = canvas.width() as i32;
    let canvas_h = canvas.height() as i32;
    let dest_bytes = canvas.pixels_mut();
    for row in 0..src_h as i32 {
        let dy = dest_y + row;
        if dy < 0 || dy >= canvas_h {
            continue;
        }
        for col in 0..src_w as i32 {
            let dx = dest_x + col;
            if dx < 0 || dx >= canvas_w {
                continue;
            }
            let src_idx = ((row as usize) * (src_w as usize) + col as usize) * TERM_BPP;
            let dst_idx = ((dy as usize) * (canvas_w as usize) + dx as usize) * TERM_BPP;
            dest_bytes[dst_idx] = src[src_idx];
            dest_bytes[dst_idx + 1] = src[src_idx + 1];
            dest_bytes[dst_idx + 2] = src[src_idx + 2];
            dest_bytes[dst_idx + 3] = src[src_idx + 3];
        }
    }
}

/// Create a terminal pre-loaded with a banner + some
/// streamed output so the rasterized interior has visible
/// pixels the assertions can find.
fn make_term(banner: &[&str], streamed: &[u8]) -> Terminal {
    let options = TerminalOptions {
        banner: banner.iter().map(|s| (*s).to_string()).collect(),
        ..TerminalOptions::default()
    };
    let mut terminal = Terminal::new(options);
    terminal.append_output(streamed);
    terminal
}

/// Render one window (chrome + terminal interior) into
/// `canvas` at the given frame bounds. Returns the
/// constructed `WindowFrame` so the caller can inspect
/// geometry.
fn render_term_window(
    canvas: &mut Canvas,
    bounds: Rect,
    terminal: &Terminal,
    focused: bool,
) -> WindowFrame {
    let mut frame = WindowFrame::new(bounds, "pmos.term");
    frame.set_focused(focused);

    // Chrome first so the interior blit can overwrite the
    // content rectangle (chrome never paints inside
    // content_rect, so this is just defensive ordering).
    frame.draw(canvas);

    let content = frame.content_rect();
    if !content.is_empty() {
        let snapshot = terminal.snapshot();
        let term_pixels = rasterize_snapshot(&snapshot, content.width, content.height);
        blit_rgba(
            canvas,
            content.x,
            content.y,
            &term_pixels,
            content.width,
            content.height,
        );
    }
    frame
}

fn rgba(color: Color) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

fn px(canvas: &Canvas, x: u32, y: u32) -> [u8; 4] {
    let slice = canvas.pixel(x, y).expect("pixel in bounds");
    [slice[0], slice[1], slice[2], slice[3]]
}

#[test]
fn two_term_windows_compose_on_a_single_canvas() {
    let mut canvas = Canvas::new(FB_WIDTH, FB_HEIGHT);
    canvas.clear(Color::rgb(0x24, 0x26, 0x2b)); // wallpaper grey

    let term_a = make_term(&["PMos term (a)"], b"hello from a\n");
    let term_b = make_term(&["PMos term (b)"], b"hello from b\n");

    // Staircase: window A is up-and-left, window B is offset
    // 80 right and 60 down so it overlaps A's lower-right corner
    // but leaves A's titlebar + left border visible.
    let a_bounds = Rect::new(40, 30, WIN_WIDTH, WIN_HEIGHT);
    let b_bounds = Rect::new(120, 90, WIN_WIDTH, WIN_HEIGHT);

    // Paint A (unfocused), then B (focused) on top — mirrors
    // how a z-order compositor would paint back-to-front.
    let frame_a = render_term_window(&mut canvas, a_bounds, &term_a, false);
    let frame_b = render_term_window(&mut canvas, b_bounds, &term_b, true);

    // ---- frame A survives at its visible edges ------------

    // A's top-left border corner is still its unfocused-border colour
    // (nothing has painted over it).
    let inactive_border = rgba(Theme::LIGHT.border_inactive);
    assert_eq!(
        px(&canvas, a_bounds.x as u32, a_bounds.y as u32),
        inactive_border,
        "window A top-left border should survive",
    );
    // Mid-left border of A, above where B overlaps it, is still A's
    // inactive border.
    assert_eq!(
        px(&canvas, a_bounds.x as u32, (a_bounds.y + 20) as u32),
        inactive_border,
        "window A left border above overlap",
    );

    // A's titlebar fill on the unoverlapped left side uses the
    // inactive titlebar colour.
    let inactive_title = rgba(Theme::LIGHT.titlebar_inactive);
    assert_eq!(
        px(
            &canvas,
            (a_bounds.x + 60) as u32,
            (a_bounds.y + BORDER_WIDTH as i32 + 5) as u32,
        ),
        inactive_title,
        "window A titlebar fill on the left, inside the titlebar interior",
    );

    // ---- frame B replaces frame A where it overlaps -------

    // B's top-left border corner overwrote A's content area.
    let active_border = rgba(Theme::LIGHT.border_active);
    assert_eq!(
        px(&canvas, b_bounds.x as u32, b_bounds.y as u32),
        active_border,
        "window B top-left border overrides A's content",
    );
    // B's titlebar fill (active) at a pixel clearly inside its
    // titlebar interior and away from title text.
    let active_title = rgba(Theme::LIGHT.titlebar_active);
    assert_eq!(
        px(
            &canvas,
            (b_bounds.x + 140) as u32,
            (b_bounds.y + BORDER_WIDTH as i32 + 5) as u32,
        ),
        active_title,
        "window B titlebar fill, middle of titlebar interior",
    );

    // ---- wallpaper shows through the uncovered corners ----

    let wallpaper = [0x24, 0x26, 0x2b, 0xFF];
    assert_eq!(
        px(&canvas, 0, 0),
        wallpaper,
        "top-left of canvas is wallpaper"
    );
    assert_eq!(
        px(&canvas, FB_WIDTH - 1, FB_HEIGHT - 1),
        wallpaper,
        "bottom-right of canvas is wallpaper",
    );

    // ---- terminal interiors have non-chrome pixels -------

    // The `term::rasterizer` module currently writes pixel bytes in
    // **BGRA** order even though `toolkit::draw::Canvas` writes RGBA —
    // see `rasterizer.rs::fill_bg` vs `canvas.rs::Canvas::clear`. The
    // composition test pins this byte-order mismatch so that fixing
    // it in a future slice (planned as part of the term::Session →
    // Canvas migration) lights up this test as a reminder. The
    // rasterizer's default background is `colors::BG` (`0xFF0A0E14`):
    // BGRA bytes are [0x14, 0x0E, 0x0A, 0xFF].
    const TERM_BG_BGRA: [u8; 4] = [0x14, 0x0E, 0x0A, 0xFF];

    // Sample a pixel inside A's content rect AT a guaranteed-background
    // point: (content_a.x + 1, content_a.y + 1) is inside the
    // rasterizer's 4-pixel PADDING border where no glyph pixel ever
    // lands, regardless of what the scrollback contains.
    let content_a = frame_a.content_rect();
    let sample_a = px(&canvas, (content_a.x + 1) as u32, (content_a.y + 1) as u32);
    assert_eq!(
        sample_a, TERM_BG_BGRA,
        "window A terminal background bleeds through in the PADDING corner",
    );

    // Same check on B's content rect, near its upper-left interior.
    let content_b = frame_b.content_rect();
    let sample_b = px(&canvas, (content_b.x + 1) as u32, (content_b.y + 1) as u32);
    assert_eq!(
        sample_b, TERM_BG_BGRA,
        "window B terminal background bleeds through in the PADDING corner",
    );

    // ---- geometry sanity: the windows actually overlap ---

    assert!(
        b_bounds.x < a_bounds.right() && b_bounds.y < a_bounds.bottom(),
        "windows must actually overlap for this test to be meaningful",
    );
    // And B does not cover A entirely — otherwise the A-survival
    // assertions above wouldn't be testing anything.
    assert!(b_bounds.x > a_bounds.x);
    assert!(b_bounds.y > a_bounds.y);
}

#[test]
fn terminal_content_area_matches_window_frame_content_rect() {
    // Pin the invariant the composition test depends on: the
    // rasterizer output is exactly the size of the frame's
    // content_rect, so a byte-for-byte blit lines up. Regressions
    // here would silently corrupt the composition test.
    let bounds = Rect::new(0, 0, WIN_WIDTH, WIN_HEIGHT);
    let frame = WindowFrame::new(bounds, "pmos.term");
    let content = frame.content_rect();
    assert_eq!(content.width, WIN_WIDTH - 2 * BORDER_WIDTH);
    assert_eq!(content.height, WIN_HEIGHT - TITLEBAR_HEIGHT - BORDER_WIDTH);
    assert_eq!(content.x, BORDER_WIDTH as i32);
    assert_eq!(content.y, TITLEBAR_HEIGHT as i32);

    let term = make_term(&[], b"pin\n");
    let snap = term.snapshot();
    let px_out = rasterize_snapshot(&snap, content.width, content.height);
    assert_eq!(
        px_out.len(),
        (content.width as usize) * (content.height as usize) * 4,
    );
}
