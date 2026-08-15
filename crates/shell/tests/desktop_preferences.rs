use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;

use preferences::Preferences;
use shell::{
    format_clock, ClockSnapshot, DesktopPreferenceRuntime, DesktopPreferences, PreferenceClock,
    PreferenceMonitor, PreferenceSource, ThemeChoice, TimezoneChoice, WallpaperChoice,
    WallpaperFit, PREFERENCE_CLOCK_CHECK_EVERY_ITERATIONS, PREFERENCE_POLL_INTERVAL_MS,
};

struct SequenceSource {
    reads: Rc<Cell<usize>>,
    values: VecDeque<io::Result<Option<Vec<u8>>>>,
}

impl SequenceSource {
    fn new(
        values: impl IntoIterator<Item = io::Result<Option<Vec<u8>>>>,
    ) -> (Self, Rc<Cell<usize>>) {
        let reads = Rc::new(Cell::new(0));
        (
            SequenceSource {
                reads: reads.clone(),
                values: values.into_iter().collect(),
            },
            reads,
        )
    }
}

impl PreferenceSource for SequenceSource {
    fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.reads.set(self.reads.get() + 1);
        self.values.pop_front().unwrap_or(Ok(None))
    }
}

struct SequenceClock {
    values: VecDeque<ClockSnapshot>,
    last: ClockSnapshot,
}

impl SequenceClock {
    fn new(values: impl IntoIterator<Item = ClockSnapshot>) -> Self {
        let values: VecDeque<_> = values.into_iter().collect();
        let last = *values.front().expect("at least one clock value");
        SequenceClock { values, last }
    }
}

impl PreferenceClock for SequenceClock {
    fn monotonic_ms(&mut self) -> u64 {
        if let Some(next) = self.values.pop_front() {
            self.last = next;
        }
        self.last.monotonic_ms
    }

    fn unix_seconds(&mut self) -> i64 {
        self.last.unix_seconds
    }
}

fn bytes(text: &str) -> io::Result<Option<Vec<u8>>> {
    Ok(Some(text.as_bytes().to_vec()))
}

#[test]
fn supported_values_normalise_into_desktop_choices() {
    let raw = Preferences::parse(
        b"[theme]\nname = \"dark\"\nfit = \"tile\"\n\
          [wallpaper]\nname = \"green.png\"\n\
          [timezone]\niana = \"Asia/Tokyo\"\n",
    )
    .unwrap();

    assert_eq!(
        DesktopPreferences::from_preferences(&raw),
        DesktopPreferences {
            theme: ThemeChoice::Dark,
            wallpaper: WallpaperChoice::Green,
            wallpaper_fit: WallpaperFit::Tile,
            timezone: TimezoneChoice::AsiaTokyo,
        }
    );
    assert_eq!(WallpaperChoice::Blue.filename(), "blue.png");
    assert_eq!(WallpaperChoice::Green.filename(), "green.png");
    assert_eq!(WallpaperChoice::Dark.filename(), "dark.png");
}

#[test]
fn unsupported_values_fall_back_independently() {
    let raw = Preferences::parse(
        b"[theme]\nname = \"neon\"\nfit = \"crop\"\n\
          [wallpaper]\nname = \"outside.png\"\n\
          [timezone]\niana = \"Mars/Olympus\"\n",
    )
    .unwrap();
    assert_eq!(
        DesktopPreferences::from_preferences(&raw),
        DesktopPreferences::default()
    );
}

#[test]
fn transient_io_retains_last_good_but_malformed_file_uses_safe_defaults() {
    let (source, _) = SequenceSource::new([
        bytes("[theme]\nname = \"dark\"\n"),
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "busy")),
        bytes("[theme\nname = \"dark\"\n"),
    ]);
    let mut monitor = PreferenceMonitor::new(source);
    assert_eq!(monitor.current().theme, ThemeChoice::Dark);

    assert!(
        !monitor.poll(),
        "transient error must not discard live state"
    );
    assert_eq!(monitor.current().theme, ThemeChoice::Dark);
    assert!(monitor.poll(), "malformed content restores a safe snapshot");
    assert_eq!(monitor.current(), DesktopPreferences::default());
}

#[test]
fn runtime_bounds_reads_and_repaints_for_preference_and_timezone_change() {
    let (source, reads) = SequenceSource::new([
        bytes("[theme]\nname = \"light\"\n[timezone]\niana = \"UTC\"\n"),
        bytes(
            "[theme]\nname = \"dark\"\n[wallpaper]\nname = \"green.png\"\n\
             [timezone]\niana = \"Asia/Tokyo\"\n",
        ),
    ]);
    let clock = SequenceClock::new([
        ClockSnapshot {
            monotonic_ms: 0,
            unix_seconds: 1_768_478_400,
        },
        ClockSnapshot {
            monotonic_ms: PREFERENCE_POLL_INTERVAL_MS - 1,
            unix_seconds: 1_768_478_400,
        },
        ClockSnapshot {
            monotonic_ms: PREFERENCE_POLL_INTERVAL_MS,
            unix_seconds: 1_768_478_400,
        },
    ]);
    let mut runtime =
        DesktopPreferenceRuntime::new(source, clock).with_clock_check_every_iterations(1);
    assert_eq!(reads.get(), 1);
    assert_eq!(runtime.clock_text(), "12:00 UTC");

    let before_boundary = runtime.poll();
    assert!(!before_boundary.preferences_checked);
    assert!(!before_boundary.needs_repaint());
    assert_eq!(reads.get(), 1, "no VFS read before the 100 ms boundary");

    let update = runtime.poll();
    assert!(update.preferences_checked);
    assert!(update.preferences_changed);
    assert!(update.clock_changed);
    assert!(update.needs_repaint());
    assert_eq!(reads.get(), 2);
    assert_eq!(runtime.preferences().theme, ThemeChoice::Dark);
    assert_eq!(runtime.preferences().wallpaper, WallpaperChoice::Green);
    assert_eq!(runtime.clock_text(), "21:00 JST");
}

#[test]
fn production_iteration_gate_avoids_a_clock_and_vfs_read_every_turn() {
    let (source, reads) = SequenceSource::new([
        bytes("[theme]\nname = \"light\"\n"),
        bytes("[theme]\nname = \"dark\"\n"),
    ]);
    let clock = SequenceClock::new([
        ClockSnapshot {
            monotonic_ms: 0,
            unix_seconds: 1_768_478_400,
        },
        ClockSnapshot {
            monotonic_ms: 0,
            unix_seconds: 1_768_478_400,
        },
        ClockSnapshot {
            monotonic_ms: PREFERENCE_POLL_INTERVAL_MS,
            unix_seconds: 1_768_478_400,
        },
    ]);
    let mut runtime = DesktopPreferenceRuntime::new(source, clock);

    for _ in 0..PREFERENCE_CLOCK_CHECK_EVERY_ITERATIONS {
        assert!(!runtime.poll().needs_repaint());
    }
    assert_eq!(reads.get(), 1, "empty display turns must not poll the VFS");

    assert!(runtime.poll().preferences_changed);
    assert_eq!(reads.get(), 2);
    assert_eq!(runtime.preferences().theme, ThemeChoice::Dark);
}

#[test]
fn clock_formats_supported_zones_and_dst_boundaries() {
    assert_eq!(
        format_clock(1_768_478_400, TimezoneChoice::Utc),
        "12:00 UTC"
    );
    assert_eq!(
        format_clock(1_768_478_400, TimezoneChoice::AmericaNewYork),
        "07:00 EST"
    );
    assert_eq!(
        format_clock(1_784_116_800, TimezoneChoice::AmericaNewYork),
        "08:00 EDT"
    );
    assert_eq!(
        format_clock(1_784_116_800, TimezoneChoice::EuropeLondon),
        "13:00 BST"
    );
    assert_eq!(
        format_clock(1_768_478_400, TimezoneChoice::AsiaTokyo),
        "21:00 JST"
    );

    assert_eq!(
        format_clock(1_772_953_140, TimezoneChoice::AmericaNewYork),
        "01:59 EST"
    );
    assert_eq!(
        format_clock(1_772_953_200, TimezoneChoice::AmericaNewYork),
        "03:00 EDT"
    );
    assert_eq!(
        format_clock(1_774_745_940, TimezoneChoice::EuropeLondon),
        "00:59 GMT"
    );
    assert_eq!(
        format_clock(1_774_746_000, TimezoneChoice::EuropeLondon),
        "02:00 BST"
    );
}
