//! Filesystem-watch event-queue plumbing.
//!
//! Mirrors `contracts/syscalls.md §3.7`. A `Watch` subscribes to
//! the `(MountId, Inode)` pair returned by resolving a single
//! absolute path at register time; subsequent VFS mutations on
//! that pair (or, for directory watches, mutations on direct
//! children of that inode) are queued as [`WatchEvent`]s in the
//! per-watch ring and drained by userland via `fd_read` on the
//! `FdObject::Watch` fd that `fs_watch` returns.
//!
//! ## Notification model
//!
//! v1 watches are keyed by `(MountId, Inode)`:
//! * a watch on a *file* fires when that file's contents change
//!   (`WATCH_MODIFY`).
//! * a watch on a *directory* fires when a direct child is created
//!   (`WATCH_CREATE`) or removed (`WATCH_DELETE`).
//!
//! The mask passed to `fs_watch` filters which event kinds are
//! queued; events whose `mask` bit is missing from the watch's
//! subscription are dropped at notify time so the queue only
//! grows with events the caller asked for.
//!
//! Recursive directory watches and rename events are deferred to
//! a future slice — the v1 wire spec includes a `RECURSIVE` flag
//! bit and `RenamedFrom`/`RenamedTo` event kinds, but the kernel
//! handler rejects `RECURSIVE` (any non-zero `flags` argument)
//! and never queues rename events.

use alloc::collections::BTreeMap;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::{Ino, MountId};

/// Stable identifier for an installed watch. Allocated monotonically
/// by [`WatchTable::next_id`] from 1 (zero is reserved as a "no
/// watch" sentinel matching [`MountId`]'s convention).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WatchId(pub u32);

/// One queued event. The wire format is fixed-size (8 bytes) so
/// `fd_read` can return a whole number of records: 4 bytes of mask
/// (`WATCH_*` bit corresponding to the event kind) + 4 bytes of the
/// affected inode (the watched inode itself for `WATCH_MODIFY`, the
/// direct-child inode for `WATCH_CREATE` / `WATCH_DELETE`). The
/// inode field gives userland enough information to disambiguate
/// concurrent events on different children of the same watched
/// directory without forcing the kernel to inline the full path
/// (which would explode the per-event size and break the fixed-
/// record-size invariant).
///
/// We use `u32` for the inode to keep the event size minimal even
/// though the underlying [`Ino`] type is `u64`; v1 filesystems
/// allocate inodes from a small monotonic counter so the truncation
/// is harmless. A future slice that exhausts 32 bits of inode space
/// can widen this without breaking existing callers (the wire
/// surface is a kernel-internal queue, not an ABI promise — the
/// fixed-size invariant is the ABI promise, not the field widths).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WatchEvent {
    pub mask: u32,
    pub inode: u32,
}

impl WatchEvent {
    /// Wire-format byte length. Used by [`Watch::drain_into`] and
    /// the `fd_read` Watch arm to compute how many whole events
    /// fit in the caller's heap_out window.
    pub const SIZE: usize = 8;

    /// Encode a single event into 8 little-endian bytes:
    /// `[mask_le_u32, inode_le_u32]`. The order matches the
    /// FsWatchEvent struct in `contracts/syscalls.md §3.7`'s
    /// abbreviated wire form (mask first, payload second).
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.mask.to_le_bytes());
        buf[4..8].copy_from_slice(&self.inode.to_le_bytes());
        buf
    }
}

/// One installed watch. Owns its own event queue; events that
/// don't match `mask` are discarded at notify time so the queue
/// never grows with traffic the caller didn't subscribe to.
#[derive(Debug)]
pub struct Watch {
    pub id: WatchId,
    pub mount_id: MountId,
    pub inode: Ino,
    pub mask: u32,
    pub events: VecDeque<WatchEvent>,
}

impl Watch {
    /// Push `event` onto this watch's queue iff the event's mask
    /// bit is part of the watch's subscription. No-op otherwise.
    pub fn enqueue_if_subscribed(&mut self, event: WatchEvent) {
        if event.mask & self.mask != 0 {
            self.events.push_back(event);
        }
    }

    /// Drain as many events as fit in `out`, writing each as 8 LE
    /// bytes. Returns the number of bytes written; always a
    /// multiple of [`WatchEvent::SIZE`]. An empty queue or
    /// `out.len() < SIZE` returns 0.
    pub fn drain_into(&mut self, out: &mut [u8]) -> usize {
        let cap = out.len() / WatchEvent::SIZE;
        let mut written = 0;
        for _ in 0..cap {
            let Some(ev) = self.events.pop_front() else {
                break;
            };
            let bytes = ev.to_bytes();
            let off = written;
            out[off..off + WatchEvent::SIZE].copy_from_slice(&bytes);
            written += WatchEvent::SIZE;
        }
        written
    }
}

/// Per-VFS watch registry. Owns every installed watch + a reverse
/// index from `(MountId, Inode)` to the list of `WatchId`s
/// subscribed to that pair so the notify path is O(log n) on the
/// number of watches per inode (typically zero or one) rather
/// than O(total watches).
pub struct WatchTable {
    next_id: u32,
    pub watches: BTreeMap<WatchId, Watch>,
    pub by_target: BTreeMap<(MountId, Ino), Vec<WatchId>>,
}

impl WatchTable {
    pub fn new() -> Self {
        WatchTable {
            next_id: 1,
            watches: BTreeMap::new(),
            by_target: BTreeMap::new(),
        }
    }

    /// Install a fresh watch. Returns the assigned id.
    pub fn register(&mut self, mount_id: MountId, inode: Ino, mask: u32) -> WatchId {
        let id = WatchId(self.next_id);
        self.next_id = self.next_id.checked_add(1).expect("watch id overflow");
        self.watches.insert(
            id,
            Watch {
                id,
                mount_id,
                inode,
                mask,
                events: VecDeque::new(),
            },
        );
        self.by_target
            .entry((mount_id, inode))
            .or_insert_with(Vec::new)
            .push(id);
        id
    }

    /// Remove the watch named by `id`. Returns `true` iff the
    /// watch existed.
    pub fn unregister(&mut self, id: WatchId) -> bool {
        let Some(watch) = self.watches.remove(&id) else {
            return false;
        };
        let key = (watch.mount_id, watch.inode);
        if let Some(ids) = self.by_target.get_mut(&key) {
            ids.retain(|w| *w != id);
            if ids.is_empty() {
                self.by_target.remove(&key);
            }
        }
        true
    }

    /// Push `event` onto every watch subscribed to `(mount_id,
    /// inode)`. Watches whose `mask` doesn't include the event's
    /// `mask` bit silently drop the event; watches with no
    /// matching subscription get nothing.
    pub fn notify(&mut self, mount_id: MountId, inode: Ino, event: WatchEvent) {
        let Some(ids) = self.by_target.get(&(mount_id, inode)) else {
            return;
        };
        let id_list: Vec<WatchId> = ids.clone();
        for id in id_list {
            if let Some(watch) = self.watches.get_mut(&id) {
                watch.enqueue_if_subscribed(event);
            }
        }
    }

    /// Mutably borrow a watch by id.
    pub fn get_mut(&mut self, id: WatchId) -> Option<&mut Watch> {
        self.watches.get_mut(&id)
    }

    /// Number of installed watches. Used by tests + future
    /// observability surfaces.
    pub fn len(&self) -> usize {
        self.watches.len()
    }

    /// True iff no watches are installed.
    pub fn is_empty(&self) -> bool {
        self.watches.is_empty()
    }
}

impl Default for WatchTable {
    fn default() -> Self {
        WatchTable::new()
    }
}
