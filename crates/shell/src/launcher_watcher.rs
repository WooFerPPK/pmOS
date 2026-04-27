//! `launcher_watcher` — T200.
//!
//! Detects new `.desktop` files under `/usr/share/applications/`
//! within 5 seconds (poll-based for v1, per
//! `contracts/package-manifest.md §4`).
//!
//! The actual poll loop lives on [`crate::launcher::Launcher`]
//! (it owns the entry list and the 5 s clock). This module is
//! the thin entry point + the helper that diffs two snapshots
//! into `Added`/`Removed` events for shell taskbars and any
//! UI that wants to react to install/uninstall.

use core::time::Duration;

use crate::launcher::{DesktopEntry, Launcher, POLL_INTERVAL};

/// Result of advancing the poll clock by one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    Added(DesktopEntry),
    Removed(String),
}

/// Compute the diff between an old entry list and a new one.
/// Order: removals first, then additions, sorted by id for
/// deterministic output (the shell taskbar wants stable order).
pub fn diff_entries(old: &[DesktopEntry], new: &[DesktopEntry]) -> Vec<WatchEvent> {
    let mut events = Vec::new();
    let old_ids: Vec<&str> = old.iter().map(|e| e.id.as_str()).collect();
    let new_ids: Vec<&str> = new.iter().map(|e| e.id.as_str()).collect();
    let mut removed: Vec<&str> = old_ids
        .iter()
        .filter(|id| !new_ids.contains(id))
        .copied()
        .collect();
    removed.sort();
    for id in removed {
        events.push(WatchEvent::Removed(id.to_string()));
    }
    let mut added: Vec<&DesktopEntry> = new
        .iter()
        .filter(|e| !old_ids.contains(&e.id.as_str()))
        .collect();
    added.sort_by(|a, b| a.id.cmp(&b.id));
    for entry in added {
        events.push(WatchEvent::Added(entry.clone()));
    }
    events
}

/// Advance a [`Launcher`] by one clock tick and return the
/// add/remove diff as a list of [`WatchEvent`]s.
///
/// The shell calls this every event-loop iteration with the
/// current monotonic time. Internal: snapshots the entries
/// before `advance_poll`, snapshots after, diffs the two.
pub fn tick(launcher: &mut Launcher, now: Duration) -> Vec<WatchEvent> {
    if now < launcher.last_poll() + POLL_INTERVAL && now != Duration::ZERO {
        return Vec::new();
    }
    let before: Vec<DesktopEntry> = launcher.entries().to_vec();
    let _ = launcher.advance_poll(now);
    let after: Vec<DesktopEntry> = launcher.entries().to_vec();
    diff_entries(&before, &after)
}
