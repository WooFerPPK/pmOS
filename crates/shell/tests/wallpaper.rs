use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{self, Cursor, Read};
use std::rc::Rc;

use shell::{
    paint_wallpaper, WallpaperChoice, WallpaperDecodeError, WallpaperFit, WallpaperImage,
    WallpaperRefreshStep, WallpaperRuntime, WallpaperSource, MAX_WALLPAPER_ENCODED_BYTES,
    WALLPAPER_DECODE_ROWS_PER_STEP, WALLPAPER_READ_BYTES_PER_STEP,
};
use toolkit::draw::{Canvas, Color};

fn encode_png(
    width: u32,
    height: u32,
    color_type: png::ColorType,
    bit_depth: png::BitDepth,
    pixels: &[u8],
) -> Vec<u8> {
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(color_type);
        encoder.set_depth(bit_depth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(pixels).unwrap();
    }
    encoded
}

fn rgba_image(width: u32, height: u32, pixels: &[u8]) -> WallpaperImage {
    WallpaperImage::decode_png(&encode_png(
        width,
        height,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        pixels,
    ))
    .unwrap()
}

fn pixel(canvas: &Canvas<'_>, x: u32, y: u32) -> [u8; 4] {
    canvas.pixel(x, y).unwrap().try_into().unwrap()
}

#[test]
fn shipped_wallpapers_are_supported_bounded_pngs() {
    let assets: [&[u8]; 3] = [
        include_bytes!("../../kernel/assets/usr/share/wallpapers/blue.png"),
        include_bytes!("../../kernel/assets/usr/share/wallpapers/green.png"),
        include_bytes!("../../kernel/assets/usr/share/wallpapers/dark.png"),
    ];
    for encoded in assets {
        assert!(encoded.len() <= MAX_WALLPAPER_ENCODED_BYTES);
        let image = WallpaperImage::decode_png(encoded).expect("shipped wallpaper must decode");
        assert_eq!((image.width(), image.height()), (1448, 1086));
        assert_eq!(image.pixels().len(), 1448 * 1086 * 3);
    }
}

#[test]
fn decoder_accepts_only_bounded_rgb_or_rgba_eight_bit_png() {
    let rgb = encode_png(
        2,
        1,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &[1, 2, 3, 4, 5, 6],
    );
    let image = WallpaperImage::decode_png(&rgb).unwrap();
    assert_eq!((image.width(), image.height()), (2, 1));

    let grayscale = encode_png(
        1,
        1,
        png::ColorType::Grayscale,
        png::BitDepth::Eight,
        &[127],
    );
    assert_eq!(
        WallpaperImage::decode_png(&grayscale),
        Err(WallpaperDecodeError::UnsupportedFormat)
    );
    assert_eq!(
        WallpaperImage::decode_png(b"not a png"),
        Err(WallpaperDecodeError::InvalidPng)
    );
    assert_eq!(
        WallpaperImage::decode_png(&vec![0; MAX_WALLPAPER_ENCODED_BYTES + 1]),
        Err(WallpaperDecodeError::EncodedTooLarge)
    );
}

#[test]
fn stretch_scales_the_full_image() {
    let image = rgba_image(
        2,
        2,
        &[
            255, 0, 0, 255, 0, 255, 0, 255, // red, green
            0, 0, 255, 255, 255, 255, 255, 255, // blue, white
        ],
    );
    let mut canvas = Canvas::new(4, 4);
    paint_wallpaper(
        &mut canvas,
        Some(&image),
        WallpaperFit::Stretch,
        Color::rgb(0, 0, 0),
    );
    assert_eq!(pixel(&canvas, 0, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&canvas, 1, 1), [255, 0, 0, 255]);
    assert_eq!(pixel(&canvas, 2, 0), [0, 255, 0, 255]);
    assert_eq!(pixel(&canvas, 0, 3), [0, 0, 255, 255]);
    assert_eq!(pixel(&canvas, 3, 3), [255, 255, 255, 255]);
}

#[test]
fn tile_repeats_and_center_leaves_fallback_bars() {
    let image = rgba_image(
        2,
        2,
        &[
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ],
    );
    let mut tiled = Canvas::new(3, 3);
    paint_wallpaper(
        &mut tiled,
        Some(&image),
        WallpaperFit::Tile,
        Color::rgb(0, 0, 0),
    );
    assert_eq!(pixel(&tiled, 2, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&tiled, 1, 2), [0, 255, 0, 255]);
    assert_eq!(pixel(&tiled, 2, 2), [255, 0, 0, 255]);

    let mut centered = Canvas::new(4, 4);
    paint_wallpaper(
        &mut centered,
        Some(&image),
        WallpaperFit::Center,
        Color::rgb(9, 8, 7),
    );
    assert_eq!(pixel(&centered, 0, 0), [9, 8, 7, 255]);
    assert_eq!(pixel(&centered, 1, 1), [255, 0, 0, 255]);
    assert_eq!(pixel(&centered, 2, 2), [255, 255, 255, 255]);
    assert_eq!(pixel(&centered, 3, 3), [9, 8, 7, 255]);
}

#[test]
fn fill_preserves_aspect_ratio_and_crops_from_the_center() {
    let image = rgba_image(
        4,
        2,
        &[
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255, // row 1
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255, // row 2
        ],
    );
    let mut canvas = Canvas::new(2, 2);
    paint_wallpaper(
        &mut canvas,
        Some(&image),
        WallpaperFit::Fill,
        Color::rgb(0, 0, 0),
    );
    assert_eq!(pixel(&canvas, 0, 0), [0, 255, 0, 255]);
    assert_eq!(pixel(&canvas, 1, 0), [0, 0, 255, 255]);
    assert_eq!(pixel(&canvas, 0, 1), [0, 255, 0, 255]);
    assert_eq!(pixel(&canvas, 1, 1), [0, 0, 255, 255]);
}

#[test]
fn rgba_pixels_blend_over_the_safe_fallback() {
    let image = rgba_image(1, 1, &[200, 100, 50, 128]);
    let mut canvas = Canvas::new(1, 1);
    paint_wallpaper(
        &mut canvas,
        Some(&image),
        WallpaperFit::Stretch,
        Color::rgb(20, 40, 60),
    );
    assert_eq!(pixel(&canvas, 0, 0), [110, 70, 55, 255]);
}

struct SequenceSource {
    reads: VecDeque<io::Result<Option<Vec<u8>>>>,
}

impl WallpaperSource for SequenceSource {
    fn read(&mut self, _choice: WallpaperChoice) -> io::Result<Option<Vec<u8>>> {
        self.reads.pop_front().expect("unexpected wallpaper read")
    }
}

struct TrackingReader {
    inner: Cursor<Vec<u8>>,
    max_requested: Rc<Cell<usize>>,
}

impl Read for TrackingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.max_requested
            .set(self.max_requested.get().max(buf.len()));
        self.inner.read(buf)
    }
}

struct StepSource {
    encoded: Option<Vec<u8>>,
    max_requested: Rc<Cell<usize>>,
}

impl WallpaperSource for StepSource {
    fn read(&mut self, _choice: WallpaperChoice) -> io::Result<Option<Vec<u8>>> {
        panic!("stepwise source must use open")
    }

    fn open(&mut self, _choice: WallpaperChoice) -> io::Result<Option<Box<dyn Read>>> {
        Ok(self.encoded.take().map(|encoded| {
            Box::new(TrackingReader {
                inner: Cursor::new(encoded),
                max_requested: self.max_requested.clone(),
            }) as Box<dyn Read>
        }))
    }
}

#[test]
fn stepwise_wallpaper_caps_reads_and_decodes_rows_before_atomic_publish() {
    let height = (WALLPAPER_DECODE_ROWS_PER_STEP * 2) as u32;
    let pixels = vec![0x5a; height as usize * 3];
    let encoded = encode_png(
        1,
        height,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &pixels,
    );
    let max_requested = Rc::new(Cell::new(0));
    let source = StepSource {
        encoded: Some(encoded),
        max_requested: max_requested.clone(),
    };
    let mut runtime = WallpaperRuntime::new_stepwise(source, WallpaperChoice::Blue);

    assert_eq!(runtime.step_refresh(), WallpaperRefreshStep::Pending); // open
    assert_eq!(runtime.step_refresh(), WallpaperRefreshStep::Pending); // read
    assert!(max_requested.get() <= WALLPAPER_READ_BYTES_PER_STEP);
    assert!(runtime.image().is_none());
    assert_eq!(runtime.step_refresh(), WallpaperRefreshStep::Pending); // EOF/header
    assert!(runtime.image().is_none());
    assert_eq!(runtime.step_refresh(), WallpaperRefreshStep::Pending); // first rows
    assert!(runtime.image().is_none());
    assert_eq!(runtime.step_refresh(), WallpaperRefreshStep::Pending); // final rows
    assert!(runtime.image().is_none());
    assert_eq!(
        runtime.step_refresh(),
        WallpaperRefreshStep::Complete { changed: true }
    );
    assert_eq!(runtime.image().unwrap().pixels(), pixels);
}

#[test]
fn malformed_replacement_keeps_the_last_good_image() {
    let red = encode_png(
        1,
        1,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &[255, 0, 0],
    );
    let source = SequenceSource {
        reads: VecDeque::from([Ok(Some(red)), Ok(Some(b"broken".to_vec()))]),
    };
    let mut runtime = WallpaperRuntime::new(source, WallpaperChoice::Blue);
    assert!(runtime.image().is_some());
    assert!(!runtime.refresh(WallpaperChoice::Green));
    assert_eq!(runtime.requested_choice(), WallpaperChoice::Green);
    assert_eq!(runtime.displayed_choice(), WallpaperChoice::Blue);

    let mut canvas = Canvas::new(1, 1);
    runtime.paint(&mut canvas, WallpaperFit::Stretch);
    assert_eq!(pixel(&canvas, 0, 0), [255, 0, 0, 255]);

    // Decode failures are permanent for an immutable selected asset and do
    // not trigger an expensive re-decode on every preference poll.
    for _ in 0..20 {
        assert!(!runtime.refresh(WallpaperChoice::Green));
    }
}

#[test]
fn transient_missing_asset_retries_at_a_bounded_cadence() {
    let green = encode_png(
        1,
        1,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &[0, 255, 0],
    );
    let source = SequenceSource {
        reads: VecDeque::from([Ok(None), Ok(Some(green))]),
    };
    let mut runtime = WallpaperRuntime::new(source, WallpaperChoice::Green);
    assert!(runtime.image().is_none());
    for _ in 0..9 {
        assert!(!runtime.refresh(WallpaperChoice::Green));
    }
    assert!(runtime.refresh(WallpaperChoice::Green));
    assert_eq!(runtime.displayed_choice(), WallpaperChoice::Green);
    assert!(runtime.image().is_some());
}
