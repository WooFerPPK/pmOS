//! Bounded VFS wallpaper loading, PNG decoding, and fit-aware painting.
//!
//! Wallpapers are ordinary files in the shell's VFS namespace. The shell
//! decodes the selected image in its own isolated process and paints it through
//! the toolkit canvas; neither the browser substrate nor the display server
//! receives a wallpaper-specific shortcut.

use crate::desktop_preferences::{WallpaperChoice, WallpaperFit};
use std::fmt;
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use toolkit::draw::{Canvas, Color, BYTES_PER_PIXEL};

pub const WALLPAPER_DIRECTORY: &str = "/usr/share/wallpapers";

/// Encoded files are intentionally smaller than OPFS's single-indirect file
/// limit. The extra byte used by the filesystem reader detects truncation at
/// this boundary without allocating based on untrusted metadata.
pub const MAX_WALLPAPER_ENCODED_BYTES: usize = 2 * 1024 * 1024;

/// A decoded wallpaper may contain at most four million pixels (16 MiB when
/// represented as RGBA). The shipped 1448x1086 assets stay comfortably below
/// this limit.
pub const MAX_WALLPAPER_PIXELS: u64 = 4 * 1024 * 1024;
pub const MAX_WALLPAPER_DIMENSION: u32 = 2048;
pub const MAX_WALLPAPER_DECODED_BYTES: usize = MAX_WALLPAPER_PIXELS as usize * BYTES_PER_PIXEL;
pub const WALLPAPER_READ_BYTES_PER_STEP: usize = 64 * 1024;
pub const WALLPAPER_DECODE_ROWS_PER_STEP: usize = 8;
pub const WALLPAPER_DECODE_BYTES_PER_STEP: usize = 64 * 1024;
const MAX_PNG_HEADER_BYTES: usize = 64 * 1024;

/// Retry a missing or transiently unreadable immutable asset at most once per
/// second. The preference monitor calls refresh at 10 Hz.
const RETRY_AFTER_PREFERENCE_POLLS: u8 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallpaperDecodeError {
    EncodedTooLarge,
    InvalidPng,
    UnsupportedFormat,
    DimensionsOutOfRange,
    DecodedTooLarge,
}

impl fmt::Display for WallpaperDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EncodedTooLarge => "encoded wallpaper exceeds 2 MiB",
            Self::InvalidPng => "wallpaper is not a valid PNG",
            Self::UnsupportedFormat => {
                "wallpaper must be a static, non-interlaced RGB/RGBA 8-bit PNG"
            }
            Self::DimensionsOutOfRange => "wallpaper dimensions are outside the supported range",
            Self::DecodedTooLarge => "decoded wallpaper exceeds 16 MiB",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for WallpaperDecodeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallpaperImage {
    width: u32,
    height: u32,
    channels: usize,
    pixels: Vec<u8>,
}

impl WallpaperImage {
    pub fn decode_png(encoded: &[u8]) -> Result<Self, WallpaperDecodeError> {
        if encoded.len() > MAX_WALLPAPER_ENCODED_BYTES {
            return Err(WallpaperDecodeError::EncodedTooLarge);
        }

        let limits = png::Limits {
            bytes: MAX_WALLPAPER_DECODED_BYTES,
        };
        let mut decoder = png::Decoder::new_with_limits(Cursor::new(encoded), limits);
        decoder.set_ignore_text_chunk(true);
        decoder.set_ignore_iccp_chunk(true);
        decoder.set_transformations(png::Transformations::IDENTITY);
        let mut reader = decoder
            .read_info()
            .map_err(|_| WallpaperDecodeError::InvalidPng)?;

        let header = reader.info();
        let width = header.width;
        let height = header.height;
        let bit_depth = header.bit_depth;
        let color_type = header.color_type;
        let unsupported_metadata =
            header.interlaced || header.animation_control.is_some() || header.trns.is_some();
        if width == 0
            || height == 0
            || width > MAX_WALLPAPER_DIMENSION
            || height > MAX_WALLPAPER_DIMENSION
        {
            return Err(WallpaperDecodeError::DimensionsOutOfRange);
        }
        let pixel_count = u64::from(width) * u64::from(height);
        if pixel_count > MAX_WALLPAPER_PIXELS {
            return Err(WallpaperDecodeError::DecodedTooLarge);
        }
        if bit_depth != png::BitDepth::Eight
            || !matches!(color_type, png::ColorType::Rgb | png::ColorType::Rgba)
            || unsupported_metadata
        {
            return Err(WallpaperDecodeError::UnsupportedFormat);
        }

        let channels = color_type.samples();
        let expected = (pixel_count as usize)
            .checked_mul(channels)
            .ok_or(WallpaperDecodeError::DecodedTooLarge)?;
        if expected > MAX_WALLPAPER_DECODED_BYTES || reader.output_buffer_size() != Some(expected) {
            return Err(WallpaperDecodeError::DecodedTooLarge);
        }

        let mut pixels = vec![0; expected];
        let output = reader
            .next_frame(&mut pixels)
            .map_err(|_| WallpaperDecodeError::InvalidPng)?;
        if output.width != width
            || output.height != height
            || output.color_type != color_type
            || output.bit_depth != bit_depth
            || output.buffer_size() != expected
        {
            return Err(WallpaperDecodeError::UnsupportedFormat);
        }
        reader
            .finish()
            .map_err(|_| WallpaperDecodeError::InvalidPng)?;

        Ok(Self {
            width,
            height,
            channels,
            pixels,
        })
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
}

pub trait WallpaperSource {
    /// `Ok(None)` means the selected bundled asset does not exist.
    fn read(&mut self, choice: WallpaperChoice) -> io::Result<Option<Vec<u8>>>;

    /// Open a streaming reader for bounded production loading. Existing
    /// injected sources inherit the compatibility adapter over [`Self::read`].
    fn open(&mut self, choice: WallpaperChoice) -> io::Result<Option<Box<dyn Read>>> {
        self.read(choice)
            .map(|bytes| bytes.map(|bytes| Box::new(Cursor::new(bytes)) as Box<dyn Read>))
    }
}

pub struct FilesystemWallpaperSource {
    root: PathBuf,
}

impl FilesystemWallpaperSource {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }
}

impl Default for FilesystemWallpaperSource {
    fn default() -> Self {
        Self::new(WALLPAPER_DIRECTORY)
    }
}

impl WallpaperSource for FilesystemWallpaperSource {
    fn read(&mut self, choice: WallpaperChoice) -> io::Result<Option<Vec<u8>>> {
        let path = self.root.join(choice.filename());
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if file.metadata()?.len() > MAX_WALLPAPER_ENCODED_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "wallpaper exceeds encoded byte limit",
            ));
        }
        let mut bytes = Vec::new();
        file.take((MAX_WALLPAPER_ENCODED_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_WALLPAPER_ENCODED_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "wallpaper exceeds encoded byte limit",
            ));
        }
        Ok(Some(bytes))
    }

    fn open(&mut self, choice: WallpaperChoice) -> io::Result<Option<Box<dyn Read>>> {
        let path = self.root.join(choice.filename());
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if file.metadata()?.len() > MAX_WALLPAPER_ENCODED_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "wallpaper exceeds encoded byte limit",
            ));
        }
        Ok(Some(Box::new(file)))
    }
}

enum LoadFailure {
    Transient,
    Permanent,
}

enum WallpaperLoad {
    Open(WallpaperChoice),
    Read {
        choice: WallpaperChoice,
        reader: Box<dyn Read>,
        encoded: Vec<u8>,
    },
    Decode {
        choice: WallpaperChoice,
        reader: Box<png::Reader<Cursor<Vec<u8>>>>,
        width: u32,
        height: u32,
        channels: usize,
        expected: usize,
        pixels: Vec<u8>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WallpaperRefreshStep {
    Idle,
    Pending,
    Complete { changed: bool },
}

/// Holds one decoded image and preserves it when a replacement is missing or
/// malformed. Only the selected image is resident, keeping steady-state memory
/// bounded independently of the number of bundled wallpapers.
pub struct WallpaperRuntime<S> {
    source: S,
    requested: WallpaperChoice,
    displayed: WallpaperChoice,
    image: Option<WallpaperImage>,
    retry_after_polls: Option<u8>,
    load: Option<WallpaperLoad>,
}

impl<S: WallpaperSource> WallpaperRuntime<S> {
    pub fn new(source: S, choice: WallpaperChoice) -> Self {
        let mut runtime = Self {
            source,
            requested: choice,
            displayed: choice,
            image: None,
            retry_after_polls: None,
            load: None,
        };
        let _ = runtime.load_requested();
        runtime
    }

    /// Production constructor that returns before opening, reading, or
    /// decoding the selected image. Call [`Self::step_refresh`] once per turn.
    pub fn new_stepwise(source: S, choice: WallpaperChoice) -> Self {
        Self {
            source,
            requested: choice,
            displayed: choice,
            image: None,
            retry_after_polls: None,
            load: Some(WallpaperLoad::Open(choice)),
        }
    }

    pub const fn requested_choice(&self) -> WallpaperChoice {
        self.requested
    }

    pub const fn displayed_choice(&self) -> WallpaperChoice {
        self.displayed
    }

    pub fn image(&self) -> Option<&WallpaperImage> {
        self.image.as_ref()
    }

    /// Observe a preference poll. A new selection loads immediately. Missing
    /// or transiently unreadable files are retried at a bounded cadence;
    /// malformed images are not repeatedly decoded until the selection changes.
    /// Returns true only when the visible wallpaper changed.
    pub fn refresh(&mut self, choice: WallpaperChoice) -> bool {
        if choice != self.requested {
            self.requested = choice;
            self.retry_after_polls = None;
            return self.load_requested();
        }

        let Some(remaining) = self.retry_after_polls else {
            return false;
        };
        if remaining > 1 {
            self.retry_after_polls = Some(remaining - 1);
            return false;
        }
        self.retry_after_polls = None;
        self.load_requested()
    }

    /// Request an asynchronous replacement. An active load is replaced by the
    /// newest selection; stale intermediate images are never published.
    pub fn request_refresh(&mut self, choice: WallpaperChoice) {
        if choice != self.requested {
            self.requested = choice;
            self.retry_after_polls = None;
            self.load = Some(WallpaperLoad::Open(choice));
        } else if self.load.is_none() && self.retry_after_polls.is_some() {
            self.load = Some(WallpaperLoad::Open(choice));
        }
    }

    pub fn refresh_pending(&self) -> bool {
        self.load.is_some()
    }

    /// Advance one bounded read/decode quantum. The old image stays visible
    /// until all rows of the replacement validate successfully.
    pub fn step_refresh(&mut self) -> WallpaperRefreshStep {
        let Some(load) = self.load.take() else {
            return WallpaperRefreshStep::Idle;
        };
        match load {
            WallpaperLoad::Open(choice) => match self.source.open(choice) {
                Ok(Some(reader)) => {
                    self.load = Some(WallpaperLoad::Read {
                        choice,
                        reader,
                        encoded: Vec::new(),
                    });
                    WallpaperRefreshStep::Pending
                }
                Ok(None) => self.finish_stepwise_failure(choice, LoadFailure::Transient),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    self.finish_stepwise_failure(choice, LoadFailure::Permanent)
                }
                Err(_) => self.finish_stepwise_failure(choice, LoadFailure::Transient),
            },
            WallpaperLoad::Read {
                choice,
                mut reader,
                mut encoded,
            } => {
                let remaining = (MAX_WALLPAPER_ENCODED_BYTES + 1).saturating_sub(encoded.len());
                if remaining == 0 {
                    return self.finish_stepwise_failure(choice, LoadFailure::Permanent);
                }
                let mut chunk = [0u8; WALLPAPER_READ_BYTES_PER_STEP];
                let limit = remaining.min(chunk.len());
                match reader.read(&mut chunk[..limit]) {
                    Ok(0) => match begin_stepwise_decode(encoded) {
                        Ok((png_reader, width, height, channels, expected)) => {
                            self.load = Some(WallpaperLoad::Decode {
                                choice,
                                reader: png_reader,
                                width,
                                height,
                                channels,
                                expected,
                                pixels: Vec::with_capacity(expected),
                            });
                            WallpaperRefreshStep::Pending
                        }
                        Err(_) => self.finish_stepwise_failure(choice, LoadFailure::Permanent),
                    },
                    Ok(n) => {
                        encoded.extend_from_slice(&chunk[..n]);
                        if encoded.len() > MAX_WALLPAPER_ENCODED_BYTES {
                            return self.finish_stepwise_failure(choice, LoadFailure::Permanent);
                        }
                        self.load = Some(WallpaperLoad::Read {
                            choice,
                            reader,
                            encoded,
                        });
                        WallpaperRefreshStep::Pending
                    }
                    Err(_) => self.finish_stepwise_failure(choice, LoadFailure::Transient),
                }
            }
            WallpaperLoad::Decode {
                choice,
                mut reader,
                width,
                height,
                channels,
                expected,
                mut pixels,
            } => {
                let mut rows = 0usize;
                let mut decoded = 0usize;
                loop {
                    if rows >= WALLPAPER_DECODE_ROWS_PER_STEP
                        || decoded >= WALLPAPER_DECODE_BYTES_PER_STEP
                    {
                        self.load = Some(WallpaperLoad::Decode {
                            choice,
                            reader,
                            width,
                            height,
                            channels,
                            expected,
                            pixels,
                        });
                        return WallpaperRefreshStep::Pending;
                    }
                    match reader.next_row() {
                        Ok(Some(row)) => {
                            if row.data().len() != width as usize * channels
                                || pixels.len().saturating_add(row.data().len()) > expected
                            {
                                return self
                                    .finish_stepwise_failure(choice, LoadFailure::Permanent);
                            }
                            pixels.extend_from_slice(row.data());
                            rows += 1;
                            decoded += row.data().len();
                        }
                        Ok(None) if pixels.len() == expected => {
                            let changed = self.image.as_ref().map(|image| {
                                image.width != width
                                    || image.height != height
                                    || image.channels != channels
                                    || image.pixels != pixels
                            }) != Some(false)
                                || self.displayed != choice;
                            self.displayed = choice;
                            self.image = Some(WallpaperImage {
                                width,
                                height,
                                channels,
                                pixels,
                            });
                            self.retry_after_polls = None;
                            return WallpaperRefreshStep::Complete { changed };
                        }
                        Ok(None) | Err(_) => {
                            return self.finish_stepwise_failure(choice, LoadFailure::Permanent)
                        }
                    }
                }
            }
        }
    }

    fn finish_stepwise_failure(
        &mut self,
        choice: WallpaperChoice,
        kind: LoadFailure,
    ) -> WallpaperRefreshStep {
        let fallback_changed = self.image.is_none() && self.displayed != choice;
        if self.image.is_none() {
            self.displayed = choice;
        }
        self.retry_after_polls = match kind {
            LoadFailure::Transient => Some(RETRY_AFTER_PREFERENCE_POLLS),
            LoadFailure::Permanent => None,
        };
        WallpaperRefreshStep::Complete {
            changed: fallback_changed,
        }
    }

    pub fn paint(&self, canvas: &mut Canvas<'_>, fit: WallpaperFit) {
        paint_wallpaper(canvas, self.image.as_ref(), fit, self.displayed.color());
    }

    fn load_requested(&mut self) -> bool {
        let result = match self.source.read(self.requested) {
            Ok(Some(bytes)) => {
                WallpaperImage::decode_png(&bytes).map_err(|_| LoadFailure::Permanent)
            }
            Ok(None) => Err(LoadFailure::Transient),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => Err(LoadFailure::Permanent),
            Err(_) => Err(LoadFailure::Transient),
        };

        match result {
            Ok(image) => {
                self.displayed = self.requested;
                self.image = Some(image);
                self.retry_after_polls = None;
                true
            }
            Err(kind) => {
                let fallback_changed = self.image.is_none() && self.displayed != self.requested;
                if self.image.is_none() {
                    self.displayed = self.requested;
                }
                self.retry_after_polls = match kind {
                    LoadFailure::Transient => Some(RETRY_AFTER_PREFERENCE_POLLS),
                    LoadFailure::Permanent => None,
                };
                fallback_changed
            }
        }
    }
}

type StepwiseDecoder = (Box<png::Reader<Cursor<Vec<u8>>>>, u32, u32, usize, usize);

fn begin_stepwise_decode(encoded: Vec<u8>) -> Result<StepwiseDecoder, WallpaperDecodeError> {
    if encoded.len() > MAX_WALLPAPER_ENCODED_BYTES || !bounded_png_header(&encoded) {
        return Err(WallpaperDecodeError::EncodedTooLarge);
    }
    let limits = png::Limits {
        bytes: MAX_WALLPAPER_DECODED_BYTES,
    };
    let mut decoder = png::Decoder::new_with_limits(Cursor::new(encoded), limits);
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_transformations(png::Transformations::IDENTITY);
    let reader = decoder
        .read_info()
        .map_err(|_| WallpaperDecodeError::InvalidPng)?;
    let header = reader.info();
    let width = header.width;
    let height = header.height;
    if width == 0
        || height == 0
        || width > MAX_WALLPAPER_DIMENSION
        || height > MAX_WALLPAPER_DIMENSION
    {
        return Err(WallpaperDecodeError::DimensionsOutOfRange);
    }
    let pixel_count = u64::from(width) * u64::from(height);
    if pixel_count > MAX_WALLPAPER_PIXELS {
        return Err(WallpaperDecodeError::DecodedTooLarge);
    }
    if header.bit_depth != png::BitDepth::Eight
        || !matches!(
            header.color_type,
            png::ColorType::Rgb | png::ColorType::Rgba
        )
        || header.interlaced
        || header.animation_control.is_some()
        || header.trns.is_some()
    {
        return Err(WallpaperDecodeError::UnsupportedFormat);
    }
    let channels = header.color_type.samples();
    let expected = (pixel_count as usize)
        .checked_mul(channels)
        .ok_or(WallpaperDecodeError::DecodedTooLarge)?;
    if expected > MAX_WALLPAPER_DECODED_BYTES || reader.output_buffer_size() != Some(expected) {
        return Err(WallpaperDecodeError::DecodedTooLarge);
    }
    Ok((Box::new(reader), width, height, channels, expected))
}

/// Keep `png::Decoder::read_info` itself bounded: production assets must reach
/// the first IDAT chunk within one 64 KiB metadata quantum.
fn bounded_png_header(encoded: &[u8]) -> bool {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if encoded.get(..8) != Some(SIGNATURE.as_slice()) {
        return false;
    }
    let mut offset = 8usize;
    let mut first = true;
    while offset <= MAX_PNG_HEADER_BYTES {
        let Some(header) = encoded.get(offset..offset.saturating_add(8)) else {
            return false;
        };
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let kind = &header[4..8];
        if first && kind != b"IHDR" {
            return false;
        }
        first = false;
        let Some(end) = offset
            .checked_add(12)
            .and_then(|base| base.checked_add(length))
        else {
            return false;
        };
        if end > encoded.len() {
            return false;
        }
        if kind == b"IDAT" {
            return true;
        }
        offset = end;
    }
    false
}

/// Paint an image with nearest-neighbour sampling. The canvas is first cleared
/// to `fallback`, so center mode has deterministic bars and translucent RGBA
/// pixels blend over a fully opaque color.
pub fn paint_wallpaper(
    canvas: &mut Canvas<'_>,
    image: Option<&WallpaperImage>,
    fit: WallpaperFit,
    fallback: Color,
) {
    canvas.clear(fallback);
    let Some(image) = image else {
        return;
    };

    let destination_width = canvas.width();
    let destination_height = canvas.height();
    if destination_width == 0 || destination_height == 0 {
        return;
    }

    let destination = canvas.pixels_mut();
    for y in 0..destination_height {
        for x in 0..destination_width {
            let Some((source_x, source_y)) = sample_coordinates(
                fit,
                x,
                y,
                destination_width,
                destination_height,
                image.width,
                image.height,
            ) else {
                continue;
            };
            let source_index = ((source_y * image.width + source_x) as usize) * image.channels;
            let destination_index = ((y * destination_width + x) as usize) * BYTES_PER_PIXEL;
            let source = &image.pixels[source_index..source_index + image.channels];
            let target = &mut destination[destination_index..destination_index + BYTES_PER_PIXEL];
            if image.channels == 3 || source[3] == u8::MAX {
                target[..3].copy_from_slice(&source[..3]);
            } else if source[3] != 0 {
                let alpha = u16::from(source[3]);
                let inverse = 255 - alpha;
                for channel in 0..3 {
                    target[channel] = ((u16::from(source[channel]) * alpha
                        + u16::from(target[channel]) * inverse
                        + 127)
                        / 255) as u8;
                }
            }
            target[3] = u8::MAX;
        }
    }
}

fn sample_coordinates(
    fit: WallpaperFit,
    x: u32,
    y: u32,
    destination_width: u32,
    destination_height: u32,
    source_width: u32,
    source_height: u32,
) -> Option<(u32, u32)> {
    match fit {
        WallpaperFit::Stretch => Some((
            scale_coordinate(x, source_width, destination_width),
            scale_coordinate(y, source_height, destination_height),
        )),
        WallpaperFit::Tile => Some((x % source_width, y % source_height)),
        WallpaperFit::Center => {
            let source_x = centered_coordinate(x, destination_width, source_width)?;
            let source_y = centered_coordinate(y, destination_height, source_height)?;
            Some((source_x, source_y))
        }
        WallpaperFit::Fill => {
            let source_aspect = u64::from(source_width) * u64::from(destination_height);
            let destination_aspect = u64::from(destination_width) * u64::from(source_height);
            if source_aspect > destination_aspect {
                let view_width = ((u64::from(destination_width) * u64::from(source_height))
                    / u64::from(destination_height))
                .max(1)
                .min(u64::from(source_width)) as u32;
                let crop_x = (source_width - view_width) / 2;
                Some((
                    crop_x + scale_coordinate(x, view_width, destination_width),
                    scale_coordinate(y, source_height, destination_height),
                ))
            } else if source_aspect < destination_aspect {
                let view_height = ((u64::from(destination_height) * u64::from(source_width))
                    / u64::from(destination_width))
                .max(1)
                .min(u64::from(source_height)) as u32;
                let crop_y = (source_height - view_height) / 2;
                Some((
                    scale_coordinate(x, source_width, destination_width),
                    crop_y + scale_coordinate(y, view_height, destination_height),
                ))
            } else {
                Some((
                    scale_coordinate(x, source_width, destination_width),
                    scale_coordinate(y, source_height, destination_height),
                ))
            }
        }
    }
}

fn scale_coordinate(coordinate: u32, source_extent: u32, destination_extent: u32) -> u32 {
    ((u64::from(coordinate) * u64::from(source_extent)) / u64::from(destination_extent))
        .min(u64::from(source_extent - 1)) as u32
}

fn centered_coordinate(
    coordinate: u32,
    destination_extent: u32,
    source_extent: u32,
) -> Option<u32> {
    if destination_extent >= source_extent {
        let offset = (destination_extent - source_extent) / 2;
        if coordinate >= offset && coordinate < offset + source_extent {
            Some(coordinate - offset)
        } else {
            None
        }
    } else {
        Some(coordinate + (source_extent - destination_extent) / 2)
    }
}
