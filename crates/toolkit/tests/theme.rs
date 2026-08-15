use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;

use toolkit::theme::{
    FilesystemThemeSource, Theme, ThemeClock, ThemeSource, ThemeWatcher,
    THEME_CLOCK_CHECK_EVERY_ITERATIONS, THEME_POLL_INTERVAL_MS, THEME_PREFERENCE_MAX_BYTES,
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
            Self {
                reads: reads.clone(),
                values: values.into_iter().collect(),
            },
            reads,
        )
    }
}

impl ThemeSource for SequenceSource {
    fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.reads.set(self.reads.get() + 1);
        self.values.pop_front().unwrap_or(Ok(None))
    }
}

struct SequenceClock {
    values: VecDeque<u64>,
    last: u64,
}

impl SequenceClock {
    fn new(values: impl IntoIterator<Item = u64>) -> Self {
        let values: VecDeque<_> = values.into_iter().collect();
        let last = *values.front().expect("at least one clock value");
        Self { values, last }
    }
}

impl ThemeClock for SequenceClock {
    fn monotonic_ms(&mut self) -> u64 {
        if let Some(next) = self.values.pop_front() {
            self.last = next;
        }
        self.last
    }
}

fn snapshot(text: &str) -> io::Result<Option<Vec<u8>>> {
    Ok(Some(text.as_bytes().to_vec()))
}

#[test]
fn supported_theme_names_normalize_and_unknown_is_safe_light() {
    assert_eq!(Theme::from_name(Some("dark")), Theme::DARK);
    assert_eq!(Theme::from_name(Some("light")), Theme::LIGHT);
    assert_eq!(Theme::from_name(Some("neon")), Theme::LIGHT);
    assert_eq!(Theme::from_name(None), Theme::LIGHT);
}

#[test]
fn watcher_loads_initial_theme_and_emits_only_real_changes() {
    let (source, reads) = SequenceSource::new([
        snapshot("[theme]\nname = \"dark\"\n"),
        snapshot("[theme]\nname = \"dark\"\n"),
        snapshot("[theme]\nname = \"light\"\n"),
    ]);
    let clock = SequenceClock::new([0, THEME_POLL_INTERVAL_MS, 2 * THEME_POLL_INTERVAL_MS]);
    let mut watcher = ThemeWatcher::from_parts(source, clock).with_clock_check_every_iterations(1);

    assert_eq!(watcher.current(), Theme::DARK);
    assert_eq!(reads.get(), 1, "startup performs one synchronous VFS read");
    assert_eq!(watcher.poll(), None, "unchanged snapshots do not repaint");
    assert_eq!(watcher.poll(), Some(Theme::LIGHT));
    assert_eq!(watcher.current(), Theme::LIGHT);
    assert_eq!(reads.get(), 3);
}

#[test]
fn transient_io_retains_last_good_and_malformed_content_uses_safe_light() {
    let (source, _) = SequenceSource::new([
        snapshot("[theme]\nname = \"dark\"\n"),
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "busy")),
        snapshot("[theme\nname = \"dark\"\n"),
    ]);
    let clock = SequenceClock::new([0, 100, 200]);
    let mut watcher = ThemeWatcher::from_parts(source, clock).with_clock_check_every_iterations(1);

    assert_eq!(watcher.poll(), None);
    assert_eq!(watcher.current(), Theme::DARK);
    assert_eq!(watcher.poll(), Some(Theme::LIGHT));
    assert_eq!(watcher.current(), Theme::LIGHT);
}

#[test]
fn production_gate_bounds_clock_checks_and_vfs_reads() {
    let (source, reads) = SequenceSource::new([
        snapshot("[theme]\nname = \"light\"\n"),
        snapshot("[theme]\nname = \"dark\"\n"),
    ]);
    let clock = SequenceClock::new([0, 0, THEME_POLL_INTERVAL_MS]);
    let mut watcher = ThemeWatcher::from_parts(source, clock);

    for _ in 0..THEME_CLOCK_CHECK_EVERY_ITERATIONS {
        assert_eq!(watcher.poll(), None);
    }
    assert_eq!(reads.get(), 1, "empty turns must not poll the VFS");

    assert_eq!(watcher.poll(), Some(Theme::DARK));
    assert_eq!(reads.get(), 2);
}

#[test]
fn filesystem_source_rejects_an_oversized_snapshot() {
    let path = std::env::temp_dir().join(format!(
        "pmos-toolkit-theme-{}-{}.toml",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, vec![b'x'; THEME_PREFERENCE_MAX_BYTES + 1]).unwrap();

    let error = FilesystemThemeSource::new(&path).read().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn filesystem_source_reports_a_missing_snapshot_without_error() {
    let path = std::env::temp_dir().join(format!(
        "pmos-toolkit-theme-missing-{}-{}.toml",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_file(&path);

    assert_eq!(FilesystemThemeSource::new(path).read().unwrap(), None);
}
