//! Mount table.
//!
//! A mount table maps absolute path prefixes to concrete
//! filesystem implementations. Path resolution against the VFS
//! finds the mount with the **longest** matching prefix and
//! strips it before handing the remainder to that filesystem.
//!
//! For v1 the table is a `Vec<MountEntry>` kept sorted by
//! descending path length, so `longest_prefix` is an O(n) scan
//! with n usually ≤ 5 (one per filesystem type). A sorted
//! structure is overkill until n grows, and a BTreeMap keyed by
//! prefix wouldn't naturally give us longest-match anyway.

use alloc::boxed::Box;
use alloc::string::String;

use super::{Filesystem, FsError};

/// Monotonic mount identifier. Mount IDs start at 1 so `0` can be
/// used as a "no mount" sentinel.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MountId(pub u32);

/// A single mount: path + filesystem + mount-flag bits.
///
/// `flags` is a `u32` bitset matching the `MOUNT_*` family in
/// [`abi::ext::mount_flags`]. Bootstrap mounts created via
/// [`MountTable::insert`] start with `flags = 0`; the syscall
/// surface populates non-zero values via [`MountTable::set_flags`]
/// (the in-place mutator that backs `MOUNT_REMOUNT`).
pub struct Mount {
    pub id: MountId,
    /// The absolute path at which this filesystem is mounted,
    /// already normalised ("/" or "/dev" or "/home/user/tmp",
    /// never with a trailing slash).
    pub mountpoint: String,
    pub fs: Box<dyn Filesystem>,
    /// Mount-flag bitset (`MOUNT_*` family). Mutated in place by
    /// [`MountTable::set_flags`] to implement REMOUNT semantics.
    pub flags: u32,
}

pub struct MountTable {
    next_id: u32,
    /// Sorted by descending mountpoint length so the first hit
    /// in `longest_prefix` is the winner.
    mounts: alloc::vec::Vec<Mount>,
}

impl MountTable {
    pub fn new() -> Self {
        MountTable {
            next_id: 1,
            mounts: alloc::vec::Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.mounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    /// Install a new filesystem at `mountpoint`. The mountpoint
    /// must have already been normalised by
    /// [`crate::vfs::path::normalize`].
    pub fn insert(
        &mut self,
        mountpoint: String,
        fs: Box<dyn Filesystem>,
    ) -> Result<MountId, FsError> {
        if self.mounts.iter().any(|m| m.mountpoint == mountpoint) {
            return Err(FsError::AlreadyExists);
        }
        let id = MountId(self.next_id);
        self.next_id = self.next_id.checked_add(1).expect("mount id overflow");
        self.mounts.push(Mount {
            id,
            mountpoint,
            fs,
            flags: 0,
        });
        self.mounts
            .sort_by(|a, b| b.mountpoint.len().cmp(&a.mountpoint.len()));
        Ok(id)
    }

    /// Mutate an existing mount's flag bitset in place. Returns the
    /// mount-id of the mutated entry, or `FsError::NotFound` if no
    /// mount matches `mountpoint`. Used by `Kernel::mount` when the
    /// `MOUNT_REMOUNT` bit is set on the call. The mountpoint
    /// comparison is exact (not longest-prefix) — `/dev/null` would
    /// NOT match a mount installed at `/dev`, mirroring the
    /// exact-match semantics of `MountTable::remove`.
    pub fn set_flags(&mut self, mountpoint: &str, flags: u32) -> Result<MountId, FsError> {
        for m in self.mounts.iter_mut() {
            if m.mountpoint == mountpoint {
                m.flags = flags;
                return Ok(m.id);
            }
        }
        Err(FsError::NotFound)
    }

    /// Read an installed mount's current flag bitset by mountpoint.
    /// Returns `None` if no mount matches `mountpoint`. Exact
    /// (non-prefix) match, paired with [`MountTable::set_flags`].
    pub fn flags_of(&self, mountpoint: &str) -> Option<u32> {
        self.mounts
            .iter()
            .find(|m| m.mountpoint == mountpoint)
            .map(|m| m.flags)
    }

    /// Return the mount id for an exact normalised mountpoint.
    pub fn id_of(&self, mountpoint: &str) -> Option<MountId> {
        self.mounts
            .iter()
            .find(|m| m.mountpoint == mountpoint)
            .map(|m| m.id)
    }

    /// Remove a mount by normalised path. Returns the freed
    /// filesystem trait object.
    pub fn remove(&mut self, mountpoint: &str) -> Result<Box<dyn Filesystem>, FsError> {
        let idx = self
            .mounts
            .iter()
            .position(|m| m.mountpoint == mountpoint)
            .ok_or(FsError::NotFound)?;
        let m = self.mounts.remove(idx);
        Ok(m.fs)
    }

    /// Find the mount whose mountpoint is the longest prefix of
    /// `path` and return its id plus the suffix (with the
    /// prefix stripped). `path` must already be normalised.
    ///
    /// Examples (with mounts at "/", "/dev", "/home"):
    ///
    /// * `"/"`            → ("/" mount, "")
    /// * `"/home/user"`   → ("/home" mount, "user")
    /// * `"/dev/null"`    → ("/dev" mount, "null")
    /// * `"/etc/passwd"`  → ("/" mount, "etc/passwd")
    pub fn longest_prefix(&self, path: &str) -> Option<(MountId, String)> {
        for m in &self.mounts {
            if prefix_matches(&m.mountpoint, path) {
                let rel = strip_prefix(&m.mountpoint, path);
                return Some((m.id, rel));
            }
        }
        None
    }

    /// Borrow a filesystem by mount id.
    pub fn fs_mut(&mut self, id: MountId) -> Option<&mut (dyn Filesystem + '_)> {
        for m in self.mounts.iter_mut() {
            if m.id == id {
                return Some(&mut *m.fs);
            }
        }
        None
    }

    /// Return the normalised mountpoint path for a mount id, or
    /// `None` if no such mount is installed.
    pub fn mountpoint_of(&self, id: MountId) -> Option<&str> {
        self.mounts
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.mountpoint.as_str())
    }

    /// Iterate mount metadata (id + mountpoint string) in the
    /// current sort order (longest first).
    pub fn iter(&self) -> impl Iterator<Item = (MountId, &str)> + '_ {
        self.mounts.iter().map(|m| (m.id, m.mountpoint.as_str()))
    }
}

impl Default for MountTable {
    fn default() -> Self {
        MountTable::new()
    }
}

/// Does `mountpoint` prefix `path`? Both are already normalised,
/// so "/dev" prefix-matches "/dev", "/dev/null", "/dev/input/kbd"
/// but NOT "/devfs" or "/devil".
fn prefix_matches(mountpoint: &str, path: &str) -> bool {
    if mountpoint == "/" {
        return true; // root always matches
    }
    if path == mountpoint {
        return true;
    }
    // Require a trailing slash boundary to avoid matching "/dev" against "/devfs".
    path.starts_with(mountpoint) && path.as_bytes().get(mountpoint.len()) == Some(&b'/')
}

/// Strip `mountpoint` from the start of `path` and return the
/// suffix with no leading slash. For root, returns everything
/// after the leading `/`.
fn strip_prefix(mountpoint: &str, path: &str) -> String {
    if mountpoint == "/" {
        // "/foo/bar" -> "foo/bar"; "/" -> ""
        if path == "/" {
            return String::new();
        }
        return path.trim_start_matches('/').into();
    }
    if path == mountpoint {
        return String::new();
    }
    // path starts with mountpoint + "/"
    path[mountpoint.len() + 1..].into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_root_always_matches() {
        assert!(prefix_matches("/", "/"));
        assert!(prefix_matches("/", "/foo"));
        assert!(prefix_matches("/", "/foo/bar"));
    }

    #[test]
    fn prefix_requires_slash_boundary() {
        assert!(prefix_matches("/dev", "/dev"));
        assert!(prefix_matches("/dev", "/dev/null"));
        assert!(prefix_matches("/dev", "/dev/input/kbd"));
        assert!(!prefix_matches("/dev", "/devfs"));
        assert!(!prefix_matches("/dev", "/devil"));
        assert!(!prefix_matches("/dev", "/"));
    }

    #[test]
    fn strip_prefix_root() {
        assert_eq!(strip_prefix("/", "/"), "");
        assert_eq!(strip_prefix("/", "/foo"), "foo");
        assert_eq!(strip_prefix("/", "/foo/bar"), "foo/bar");
    }

    #[test]
    fn strip_prefix_subdir() {
        assert_eq!(strip_prefix("/dev", "/dev"), "");
        assert_eq!(strip_prefix("/dev", "/dev/null"), "null");
        assert_eq!(strip_prefix("/dev", "/dev/input/kbd"), "input/kbd");
    }
}
