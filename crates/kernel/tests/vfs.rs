//! VFS isolation tests (T055).
//!
//! Covers the VFS core (mount table, path resolver, public API)
//! against real filesystem implementations (tmpfs, devfs,
//! procfs). No browser involved; this is the Principle X gate
//! for the VFS layer.

#![cfg(feature = "native-platform")]

use kernel::fs::devfs::{DevFs, DEV_CONSOLE, DEV_NULL};
use kernel::fs::opfs::block::MockBlockDevice;
use kernel::fs::opfs::layout::BLOCK_SIZE;
use kernel::fs::opfs::mkfs::mkfs;
use kernel::fs::procfs::ProcFs;
use kernel::fs::tmpfs::TmpFs;
use kernel::vfs::{DirEntry, FsError, NodeType, Vfs};

// ---- Helpers --------------------------------------------------------

fn fresh_vfs_with_root_tmpfs() -> Vfs {
    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(TmpFs::new())).unwrap();
    vfs
}

fn fresh_vfs_full() -> Vfs {
    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(TmpFs::new())).unwrap();
    vfs.mount("/tmp", Box::new(TmpFs::new())).unwrap();
    vfs.mount("/dev", Box::new(DevFs::new())).unwrap();
    vfs.mount("/proc", Box::new(ProcFs::with_static())).unwrap();
    vfs
}

// ---- Path normalisation + component iteration ----------------------
// These are in kernel::vfs::path::tests as in-module tests; here we
// only cover the integration through the Vfs public API.

// ---- Single-filesystem operations ----------------------------------

#[test]
fn create_write_read_round_trip() {
    let mut vfs = fresh_vfs_with_root_tmpfs();

    vfs.create("/hello.txt", 0o644).unwrap();
    assert_eq!(vfs.write("/hello.txt", 0, b"hello, world\n").unwrap(), 13);

    let mut buf = [0u8; 32];
    let n = vfs.read("/hello.txt", 0, &mut buf).unwrap();
    assert_eq!(n, 13);
    assert_eq!(&buf[..n], b"hello, world\n");
}

#[test]
fn successful_mutations_mark_mount_dirty_until_sync() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    assert_eq!(vfs.dirty_mount_count(), 0);

    vfs.create("/dirty.txt", 0o644).unwrap();
    assert_eq!(vfs.dirty_mount_count(), 1);

    vfs.sync_dirty().unwrap();
    assert_eq!(vfs.dirty_mount_count(), 0);

    vfs.write("/dirty.txt", 0, b"x").unwrap();
    assert_eq!(vfs.dirty_mount_count(), 1);

    let mut buf = [0u8; 1];
    vfs.read("/dirty.txt", 0, &mut buf).unwrap();
    assert_eq!(vfs.dirty_mount_count(), 1);

    vfs.sync_dirty().unwrap();
    assert_eq!(vfs.dirty_mount_count(), 0);
}

#[test]
fn storage_usage_projects_exact_mount_counters() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    assert_eq!(vfs.storage_usage("/").unwrap(), None);

    let blocks = 4096;
    let opfs = mkfs(Box::new(MockBlockDevice::new(blocks))).expect("mkfs");
    vfs.mount("/persist", Box::new(opfs)).unwrap();

    let usage = vfs
        .storage_usage("/persist")
        .unwrap()
        .expect("opfs reports storage usage");
    assert_eq!(usage.quota_bytes, blocks * BLOCK_SIZE as u64);
    assert!(usage.used_bytes > 0);
    assert!(usage.file_count > 0);
}

#[test]
fn read_past_end_returns_zero() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/a", 0o644).unwrap();
    vfs.write("/a", 0, b"abc").unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(vfs.read("/a", 3, &mut buf).unwrap(), 0);
    assert_eq!(vfs.read("/a", 10, &mut buf).unwrap(), 0);
}

#[test]
fn write_extends_file() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/a", 0o644).unwrap();
    vfs.write("/a", 0, b"abc").unwrap();
    vfs.write("/a", 3, b"def").unwrap();
    let mut buf = [0u8; 6];
    assert_eq!(vfs.read("/a", 0, &mut buf).unwrap(), 6);
    assert_eq!(&buf, b"abcdef");
    assert_eq!(vfs.stat("/a").unwrap().size, 6);
}

#[test]
fn mkdir_and_nested_create() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/home", 0o755).unwrap();
    vfs.mkdir("/home/user", 0o755).unwrap();
    vfs.mkdir("/home/user/notes", 0o755).unwrap();
    vfs.create("/home/user/notes/hi.txt", 0o644).unwrap();
    vfs.write("/home/user/notes/hi.txt", 0, b"hello\n").unwrap();

    let mut buf = [0u8; 16];
    let n = vfs.read("/home/user/notes/hi.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello\n");
}

#[test]
fn mkdir_existing_fails() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/a", 0o755).unwrap();
    let err = vfs.mkdir("/a", 0o755).unwrap_err();
    assert_eq!(err, FsError::AlreadyExists);
}

#[test]
fn create_in_missing_directory_is_not_found() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    let err = vfs.create("/nowhere/missing.txt", 0o644).unwrap_err();
    assert_eq!(err, FsError::NotFound);
}

#[test]
fn create_under_nondirectory_is_an_error() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/afile", 0o644).unwrap();
    let err = vfs.create("/afile/child", 0o644).unwrap_err();
    assert!(matches!(err, FsError::NotADirectory | FsError::NotFound));
}

#[test]
fn unlink_and_read_after_unlink_is_not_found() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/a", 0o644).unwrap();
    vfs.write("/a", 0, b"hi").unwrap();
    vfs.unlink("/a").unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(vfs.read("/a", 0, &mut buf).unwrap_err(), FsError::NotFound);
}

#[test]
fn unlink_directory_fails_with_is_a_directory() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/d", 0o755).unwrap();
    let err = vfs.unlink("/d").unwrap_err();
    assert_eq!(err, FsError::IsADirectory);
}

#[test]
fn rmdir_empty_succeeds_nonempty_fails() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/a", 0o755).unwrap();
    vfs.create("/a/f", 0o644).unwrap();
    assert_eq!(vfs.rmdir("/a").unwrap_err(), FsError::NotEmpty);
    vfs.unlink("/a/f").unwrap();
    vfs.rmdir("/a").unwrap();
}

#[test]
fn readdir_root_listing_is_deterministic() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/a", 0o644).unwrap();
    vfs.create("/c", 0o644).unwrap();
    vfs.create("/b", 0o644).unwrap();
    let entries = vfs.readdir("/").unwrap();
    // tmpfs's children are stored in a BTreeMap, so readdir
    // returns them in ascending name order.
    let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(
        names,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
    for e in &entries {
        assert_eq!(e.ty, NodeType::RegularFile);
    }
}

#[test]
fn rename_within_same_directory() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/foo.txt", 0o644).unwrap();
    vfs.write("/foo.txt", 0, b"content").unwrap();
    vfs.rename("/foo.txt", "/bar.txt").unwrap();

    let mut buf = [0u8; 8];
    let n = vfs.read("/bar.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"content");
    assert_eq!(
        vfs.read("/foo.txt", 0, &mut buf).unwrap_err(),
        FsError::NotFound
    );
}

#[test]
fn rename_across_directories_same_fs() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/a", 0o755).unwrap();
    vfs.mkdir("/b", 0o755).unwrap();
    vfs.create("/a/x", 0o644).unwrap();
    vfs.write("/a/x", 0, b"moved").unwrap();
    vfs.rename("/a/x", "/b/y").unwrap();

    let mut buf = [0u8; 8];
    let n = vfs.read("/b/y", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"moved");

    let a_entries = vfs.readdir("/a").unwrap();
    assert!(a_entries.is_empty());
}

#[test]
fn rename_replaces_existing_destination() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/src", 0o644).unwrap();
    vfs.write("/src", 0, b"SRC").unwrap();
    vfs.create("/dst", 0o644).unwrap();
    vfs.write("/dst", 0, b"DST").unwrap();
    vfs.rename("/src", "/dst").unwrap();

    let mut buf = [0u8; 8];
    let n = vfs.read("/dst", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"SRC");
    assert_eq!(
        vfs.read("/src", 0, &mut buf).unwrap_err(),
        FsError::NotFound
    );
}

#[test]
fn truncate_extends_and_shrinks() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/a", 0o644).unwrap();
    vfs.write("/a", 0, b"abcdef").unwrap();
    vfs.truncate("/a", 3).unwrap();
    assert_eq!(vfs.stat("/a").unwrap().size, 3);

    let mut buf = [0u8; 8];
    let n = vfs.read("/a", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"abc");

    // Extend back.
    vfs.truncate("/a", 6).unwrap();
    assert_eq!(vfs.stat("/a").unwrap().size, 6);
    let n = vfs.read("/a", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"abc\0\0\0");
}

// ---- Path normalisation via public API -----------------------------

#[test]
fn redundant_slashes_and_dot_resolve_correctly() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/foo", 0o755).unwrap();
    vfs.create("/foo/bar.txt", 0o644).unwrap();
    vfs.write("/foo/bar.txt", 0, b"ok").unwrap();

    let mut buf = [0u8; 4];
    for path in &[
        "/foo/bar.txt",
        "//foo//bar.txt",
        "/./foo/./bar.txt",
        "/foo/./bar.txt/",
    ] {
        let n = vfs.read(path, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"ok", "path: {path}");
    }
}

#[test]
fn dotdot_resolves_across_parents() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/a", 0o755).unwrap();
    vfs.mkdir("/a/b", 0o755).unwrap();
    vfs.create("/a/target.txt", 0o644).unwrap();
    vfs.write("/a/target.txt", 0, b"yes").unwrap();

    let mut buf = [0u8; 4];
    let n = vfs.read("/a/b/../target.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"yes");
}

// ---- Stat ----------------------------------------------------------

#[test]
fn stat_reports_node_type_and_size() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/d", 0o755).unwrap();
    vfs.create("/d/f", 0o644).unwrap();
    vfs.write("/d/f", 0, b"12345").unwrap();

    let d = vfs.stat("/d").unwrap();
    assert_eq!(d.ty, NodeType::Directory);
    assert_eq!(d.mode, 0o755);

    let f = vfs.stat("/d/f").unwrap();
    assert_eq!(f.ty, NodeType::RegularFile);
    assert_eq!(f.size, 5);
    assert_eq!(f.mode, 0o644);
}

// ---- Multi-mount / longest-prefix matching -------------------------

#[test]
fn longest_prefix_routes_to_correct_mount() {
    let mut vfs = fresh_vfs_full();

    // Create a file on root tmpfs.
    vfs.create("/root-file", 0o644).unwrap();
    vfs.write("/root-file", 0, b"ROOT").unwrap();

    // Create a file under /tmp (separate tmpfs).
    vfs.create("/tmp/scratch", 0o644).unwrap();
    vfs.write("/tmp/scratch", 0, b"TMP").unwrap();

    let mut buf = [0u8; 8];
    let n1 = vfs.read("/root-file", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n1], b"ROOT");

    let n2 = vfs.read("/tmp/scratch", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n2], b"TMP");
}

#[test]
fn writing_inside_tmp_does_not_leak_to_root() {
    let mut vfs = fresh_vfs_full();
    // /tmp is its own filesystem. Creating /tmp/foo should NOT
    // create /foo on the root filesystem.
    vfs.create("/tmp/foo", 0o644).unwrap();
    vfs.write("/tmp/foo", 0, b"x").unwrap();

    // Root is still empty (no /foo, no /tmp entry — /tmp is a mount point).
    let root = vfs.readdir("/").unwrap();
    let names: Vec<&str> = root.iter().map(|e| e.name.as_str()).collect();
    assert!(!names.contains(&"foo"));
}

#[test]
fn rename_across_mounts_rejected() {
    let mut vfs = fresh_vfs_full();
    vfs.create("/src", 0o644).unwrap();
    vfs.write("/src", 0, b"x").unwrap();
    let err = vfs.rename("/src", "/tmp/dst").unwrap_err();
    assert_eq!(err, FsError::NotSupported);
}

#[test]
fn devfs_lookup_and_readdir() {
    let mut vfs = fresh_vfs_full();

    let entries = vfs.readdir("/dev").unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"null"));
    assert!(names.contains(&"console"));
    assert!(names.contains(&"fb0"));

    let null_stat = vfs.stat("/dev/null").unwrap();
    assert!(matches!(null_stat.ty, NodeType::CharDevice(n) if n == DEV_NULL));

    let console_stat = vfs.stat("/dev/console").unwrap();
    assert!(matches!(console_stat.ty, NodeType::CharDevice(n) if n == DEV_CONSOLE));
}

#[test]
fn devfs_is_read_only() {
    let mut vfs = fresh_vfs_full();
    assert_eq!(
        vfs.create("/dev/foo", 0o644).unwrap_err(),
        FsError::ReadOnly
    );
    assert_eq!(vfs.mkdir("/dev/d", 0o755).unwrap_err(), FsError::ReadOnly);
    assert_eq!(vfs.unlink("/dev/null").unwrap_err(), FsError::ReadOnly);
}

#[test]
fn procfs_serves_static_content() {
    let mut vfs = fresh_vfs_full();

    let entries = vfs.readdir("/proc").unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"version"));
    assert!(names.contains(&"uptime"));
    assert!(names.contains(&"meminfo"));
    assert!(names.contains(&"loadavg"));
    assert!(names.contains(&"storage"));

    let mut buf = [0u8; 64];
    let n = vfs.read("/proc/version", 0, &mut buf).unwrap();
    assert!(core::str::from_utf8(&buf[..n]).unwrap().starts_with("PMos"));
}

#[test]
fn procfs_is_read_only() {
    let mut vfs = fresh_vfs_full();
    assert_eq!(
        vfs.create("/proc/new", 0o644).unwrap_err(),
        FsError::ReadOnly
    );
    assert_eq!(vfs.unlink("/proc/version").unwrap_err(), FsError::ReadOnly);
}

// ---- Real vnode timestamps -----------------------------------------
//
// Every filesystem in v1 now threads `Platform::now_realtime_ns()`
// into its vnode metadata. Semantics are "touch-on-write with
// noatime":
//
//   create / mkdir      set atime = mtime = ctime = now()
//   write / truncate    update mtime + ctime, leave atime untouched
//   read                does NOT touch atime in v1 (relaxed POSIX;
//                       avoids the write-amplification that strict
//                       atime would introduce on every read)
//
// Filesystems with no per-vnode mutation surface (devfs, procfs)
// still report meaningful timestamps: devfs snapshots the time at
// mount and reports it for every entry; procfs returns `now()` per
// stat call since its content is synthesised fresh each time.
//
// The tests below assert `> 0` rather than specific values because
// the underlying `Platform::now_realtime_ns` reflects the host
// wall clock, which is opaque in tests. A real kernel integration
// test that needed exact timestamps would inject a deterministic
// Platform impl; these tests only pin the "zeros are gone"
// contract the filestat-get shims depended on.

#[test]
fn tmpfs_stat_returns_nonzero_timestamps_for_created_file() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/f", 0o644).unwrap();
    let st = vfs.stat("/f").unwrap();
    assert!(st.atime_ns > 0, "atime");
    assert!(st.mtime_ns > 0, "mtime");
    assert!(st.ctime_ns > 0, "ctime");
    // On a freshly-created file, all three are set to the same
    // now(), so they agree byte-for-byte (one Platform call, one
    // tuple assignment).
    assert_eq!(st.atime_ns, st.mtime_ns);
    assert_eq!(st.mtime_ns, st.ctime_ns);
}

#[test]
fn tmpfs_stat_returns_nonzero_timestamps_for_created_directory() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/d", 0o755).unwrap();
    let st = vfs.stat("/d").unwrap();
    assert!(st.atime_ns > 0);
    assert!(st.mtime_ns > 0);
    assert!(st.ctime_ns > 0);
}

#[test]
fn tmpfs_write_advances_mtime_and_ctime_and_leaves_atime() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/f", 0o644).unwrap();
    let before = vfs.stat("/f").unwrap();

    // Sleep a nanosecond's worth so the realtime clock advances
    // past the create-time timestamp.
    std::thread::sleep(std::time::Duration::from_millis(1));
    vfs.write("/f", 0, b"hi").unwrap();

    let after = vfs.stat("/f").unwrap();
    assert!(after.mtime_ns > before.mtime_ns, "mtime advanced");
    assert!(after.ctime_ns > before.ctime_ns, "ctime advanced");
    // noatime in v1: atime stays at create-time.
    assert_eq!(after.atime_ns, before.atime_ns);
}

#[test]
fn tmpfs_truncate_advances_mtime_and_ctime() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/f", 0o644).unwrap();
    vfs.write("/f", 0, b"abcdef").unwrap();
    let before = vfs.stat("/f").unwrap();

    std::thread::sleep(std::time::Duration::from_millis(1));
    vfs.truncate("/f", 3).unwrap();

    let after = vfs.stat("/f").unwrap();
    assert!(after.mtime_ns > before.mtime_ns);
    assert!(after.ctime_ns > before.ctime_ns);
    assert_eq!(after.atime_ns, before.atime_ns);
}

#[test]
fn devfs_stat_returns_nonzero_timestamps() {
    let mut vfs = fresh_vfs_full();
    let st = vfs.stat("/dev/console").unwrap();
    assert!(st.atime_ns > 0);
    assert!(st.mtime_ns > 0);
    assert!(st.ctime_ns > 0);
    // Every devfs entry shares the mount-time snapshot, so the
    // three fields agree (single Platform call at mount).
    assert_eq!(st.atime_ns, st.mtime_ns);
    assert_eq!(st.mtime_ns, st.ctime_ns);
}

#[test]
fn devfs_stat_is_stable_across_calls() {
    // devfs timestamps are frozen at mount time — a second stat()
    // of the same node returns the same triple.
    let mut vfs = fresh_vfs_full();
    let first = vfs.stat("/dev/null").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1));
    let second = vfs.stat("/dev/null").unwrap();
    assert_eq!(first.mtime_ns, second.mtime_ns);
}

#[test]
fn procfs_stat_returns_nonzero_timestamps() {
    let mut vfs = fresh_vfs_full();
    let st = vfs.stat("/proc/version").unwrap();
    assert!(st.atime_ns > 0);
    assert!(st.mtime_ns > 0);
    assert!(st.ctime_ns > 0);
}

// ---- Mount table bookkeeping ---------------------------------------

#[test]
fn mount_count_matches_inserted_filesystems() {
    let mut vfs = Vfs::new();
    assert_eq!(vfs.mount_count(), 0);
    vfs.mount("/", Box::new(TmpFs::new())).unwrap();
    assert_eq!(vfs.mount_count(), 1);
    vfs.mount("/tmp", Box::new(TmpFs::new())).unwrap();
    assert_eq!(vfs.mount_count(), 2);
    vfs.mount("/dev", Box::new(DevFs::new())).unwrap();
    assert_eq!(vfs.mount_count(), 3);
}

#[test]
fn mounting_same_point_twice_is_an_error() {
    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(TmpFs::new())).unwrap();
    let err = vfs.mount("/", Box::new(TmpFs::new())).unwrap_err();
    assert_eq!(err, FsError::AlreadyExists);
}

#[test]
fn mounting_with_trailing_slash_is_equivalent() {
    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(TmpFs::new())).unwrap();
    vfs.mount("/tmp/", Box::new(TmpFs::new())).unwrap();
    // Creating under /tmp should hit the /tmp mount.
    vfs.create("/tmp/f", 0o644).unwrap();
    let entries = vfs.readdir("/tmp").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "f");
}

#[test]
fn umount_removes_the_mount() {
    let mut vfs = fresh_vfs_full();
    vfs.create("/tmp/a", 0o644).unwrap();
    vfs.umount("/tmp").unwrap();
    // After umount, /tmp is now just a path on the root filesystem —
    // which has no /tmp directory, so lookup fails.
    let err = vfs.readdir("/tmp").unwrap_err();
    assert_eq!(err, FsError::NotFound);
}

// ---- DirEntry typing -------------------------------------------------

#[test]
fn readdir_reports_correct_types() {
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/d", 0o755).unwrap();
    vfs.create("/f", 0o644).unwrap();

    let entries = vfs.readdir("/").unwrap();
    let mut saw_dir = false;
    let mut saw_file = false;
    for e in entries {
        match e {
            DirEntry {
                ref name,
                ty: NodeType::Directory,
                ..
            } if name == "d" => saw_dir = true,
            DirEntry {
                ref name,
                ty: NodeType::RegularFile,
                ..
            } if name == "f" => saw_file = true,
            _ => {}
        }
    }
    assert!(saw_dir);
    assert!(saw_file);
}

// ---- Symlink-aware path resolution --------------------------------
//
// Pre-slice `Vfs::resolve` stopped at whatever inode the filesystem
// yielded for each path component, so a SymLink component short-
// circuited the walk. Post-slice `resolve` follows symlinks (both
// intermediate and final components) with a SYMLOOP_MAX-bounded
// chain and delegates loop detection to `FsError::SymLoop → ELOOP`.
// `resolve_nofollow` preserves the pre-slice behaviour so
// `stat` / `readlink` keep their lstat-like semantics on a path
// whose final component is itself a symlink.

#[test]
fn resolve_follows_single_absolute_symlink_to_regular_file() {
    // /target is a regular file; /link is a symlink pointing at it.
    // Post-slice, resolve("/link") returns /target's ino, not /link's.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/target", 0o644).unwrap();
    vfs.write("/target", 0, b"hello").unwrap();
    vfs.symlink("/target", "/link").unwrap();

    let target_ino = vfs.resolve("/target").unwrap().1;
    let link_ino = vfs.resolve("/link").unwrap().1;
    assert_eq!(link_ino, target_ino, "resolve follows symlink to target");
}

#[test]
fn resolve_nofollow_returns_symlink_ino() {
    // resolve_nofollow preserves the pre-slice short-circuit: the
    // symlink's own ino is returned, not the target's.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/target", 0o644).unwrap();
    vfs.symlink("/target", "/link").unwrap();

    let target_ino = vfs.resolve("/target").unwrap().1;
    let link_ino = vfs.resolve_nofollow("/link").unwrap().1;
    assert_ne!(
        link_ino, target_ino,
        "resolve_nofollow returns symlink's own ino"
    );
    // And its node type is SymLink.
    let (mid, ino) = vfs.resolve_nofollow("/link").unwrap();
    let st = vfs.stat_ino(mid, ino).unwrap();
    assert!(st.ty.is_symlink());
}

#[test]
fn resolve_follows_three_link_chain() {
    // /a -> /b -> /c -> /target. resolve("/a") reaches /target in
    // three hops without tripping SYMLOOP_MAX.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/target", 0o644).unwrap();
    vfs.symlink("/target", "/c").unwrap();
    vfs.symlink("/c", "/b").unwrap();
    vfs.symlink("/b", "/a").unwrap();

    let target_ino = vfs.resolve("/target").unwrap().1;
    let a_ino = vfs.resolve("/a").unwrap().1;
    assert_eq!(a_ino, target_ino);
}

#[test]
fn resolve_follows_symlink_in_intermediate_component() {
    // /realdir is a directory with a file; /linkdir is a symlink to
    // /realdir. resolve("/linkdir/file") walks /linkdir as an
    // intermediate component, follows to /realdir, then looks up
    // "file" → /realdir/file.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/realdir", 0o755).unwrap();
    vfs.create("/realdir/file", 0o644).unwrap();
    vfs.symlink("/realdir", "/linkdir").unwrap();

    let real_ino = vfs.resolve("/realdir/file").unwrap().1;
    let via_link = vfs.resolve("/linkdir/file").unwrap().1;
    assert_eq!(via_link, real_ino);
}

#[test]
fn resolve_follows_relative_symlink_target() {
    // /d/s is a symlink whose target is the relative string "t".
    // POSIX says this resolves to /d/t (relative to the symlink's
    // parent directory, not to cwd or /).
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/d", 0o755).unwrap();
    vfs.create("/d/t", 0o644).unwrap();
    vfs.symlink("t", "/d/s").unwrap();

    let target = vfs.resolve("/d/t").unwrap().1;
    let via_link = vfs.resolve("/d/s").unwrap().1;
    assert_eq!(via_link, target);
}

#[test]
fn resolve_follows_relative_symlink_with_dotdot() {
    // /d1/s is a symlink whose target is "../d2/t". Post-slice this
    // normalises to /d2/t relative to /d1's parent (which is /).
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/d1", 0o755).unwrap();
    vfs.mkdir("/d2", 0o755).unwrap();
    vfs.create("/d2/t", 0o644).unwrap();
    vfs.symlink("../d2/t", "/d1/s").unwrap();

    let target = vfs.resolve("/d2/t").unwrap().1;
    let via_link = vfs.resolve("/d1/s").unwrap().1;
    assert_eq!(via_link, target);
}

#[test]
fn resolve_symlink_self_loop_returns_eloop() {
    // /a -> /a. Following hits SYMLOOP_MAX immediately.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.symlink("/a", "/a").unwrap();

    let err = vfs.resolve("/a").unwrap_err();
    assert_eq!(err, FsError::SymLoop);
}

#[test]
fn resolve_symlink_chain_loop_returns_eloop() {
    // /a -> /b, /b -> /a. Hopping back and forth burns the budget.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.symlink("/b", "/a").unwrap();
    vfs.symlink("/a", "/b").unwrap();

    let err = vfs.resolve("/a").unwrap_err();
    assert_eq!(err, FsError::SymLoop);
}

#[test]
fn resolve_symlink_to_nonexistent_returns_not_found() {
    // A dangling symlink: /link -> /nope. Following reaches lookup
    // of "nope" in / which returns NotFound.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.symlink("/nope", "/link").unwrap();

    let err = vfs.resolve("/link").unwrap_err();
    assert_eq!(err, FsError::NotFound);
}

#[test]
fn resolve_follows_symlink_across_mounts() {
    // /tmp is a separate mount. /link on the root fs targets
    // /tmp/target. Resolving /link must restart resolution from
    // the VFS root so the mount table routes to /tmp's fs.
    let mut vfs = fresh_vfs_full();
    vfs.create("/tmp/target", 0o644).unwrap();
    vfs.symlink("/tmp/target", "/link").unwrap();

    let (tmp_mount, target_ino) = vfs.resolve("/tmp/target").unwrap();
    let (via_link_mount, via_link_ino) = vfs.resolve("/link").unwrap();
    assert_eq!(via_link_mount, tmp_mount);
    assert_eq!(via_link_ino, target_ino);
}

#[test]
fn stat_still_uses_lstat_semantics_for_final_symlink() {
    // stat() on a path whose final component is a symlink reports
    // the symlink's own stat (ty = SymLink), mirroring lstat().
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/target", 0o644).unwrap();
    vfs.symlink("/target", "/link").unwrap();

    let st = vfs.stat("/link").unwrap();
    assert!(st.ty.is_symlink(), "stat reports SymLink on final symlink");
}

#[test]
fn readlink_still_uses_nofollow() {
    // readlink() on a symlink path returns the target bytes rather
    // than following the link. The resolve_nofollow route is what
    // lets readlink see the symlink's own ino.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.symlink("/some/target", "/mylink").unwrap();

    let mut buf = [0u8; 32];
    let n = vfs.readlink("/mylink", &mut buf).unwrap();
    assert_eq!(&buf[..n], b"/some/target");
}

#[test]
fn unlink_of_symlink_removes_link_not_target() {
    // unlink on a symlink path removes the symlink; the target file
    // is untouched. resolve_parent's intermediate follow doesn't
    // affect this because the final component of the link path is
    // a symlink and resolve_parent treats it as a basename rather
    // than resolving it.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/target", 0o644).unwrap();
    vfs.write("/target", 0, b"data").unwrap();
    vfs.symlink("/target", "/link").unwrap();

    vfs.unlink("/link").unwrap();

    // Target still exists.
    assert!(vfs.stat("/target").is_ok());
    // Link is gone.
    let err = vfs.resolve_nofollow("/link").unwrap_err();
    assert_eq!(err, FsError::NotFound);
}

#[test]
fn read_via_symlink_returns_target_bytes() {
    // Reading through a symlink path uses resolve (follow), so the
    // read lands on the target file's bytes rather than failing
    // with InvalidArgument on the symlink itself.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/target", 0o644).unwrap();
    vfs.write("/target", 0, b"bytes").unwrap();
    vfs.symlink("/target", "/link").unwrap();

    let mut buf = [0u8; 8];
    let n = vfs.read("/link", 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"bytes");
}

#[test]
fn open_follows_symlink_reports_target_filetype() {
    // Vfs::open returns (mount, ino, NodeType). For /link → /target
    // (regular file), open reports the target's type, not SymLink.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.create("/target", 0o644).unwrap();
    vfs.symlink("/target", "/link").unwrap();

    let (_m, _i, ty) = vfs.open("/link").unwrap();
    assert_eq!(ty, NodeType::RegularFile);
}

// ---- resolve_nofollow: POSIX-perfect intermediate follow -----------
//
// Pre-slice-7 resolve_nofollow was a literal "don't follow anything"
// walker — even an intermediate symlink short-circuited with
// NotADirectory. Post-slice-7 it follows intermediate symlinks
// (because real POSIX lstat semantics say only the FINAL component
// should stay at the symlink) while leaving the final component at
// its own ino when it's a symlink. Matches SYMLOOP_MAX-bounded chain
// walking with ELOOP detection.

#[test]
fn resolve_nofollow_follows_intermediate_symlink() {
    // /linkdir → /realdir (intermediate symlink). /realdir/file
    // exists. resolve_nofollow("/linkdir/file") should dereference
    // /linkdir, then look up "file" in /realdir, landing on the
    // regular file's ino.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/realdir", 0o755).unwrap();
    vfs.create("/realdir/file", 0o644).unwrap();
    vfs.symlink("/realdir", "/linkdir").unwrap();

    let real_file_ino = vfs.resolve("/realdir/file").unwrap().1;
    let via_link = vfs.resolve_nofollow("/linkdir/file").unwrap().1;
    assert_eq!(via_link, real_file_ino);
}

#[test]
fn resolve_nofollow_intermediate_symlink_loop_returns_eloop() {
    // /a → /b/sibling (intermediate ref), /b → /a (creates a loop
    // when resolving /a/anything since /a's target is /b/sibling
    // which references /a). SYMLOOP_MAX budget is shared with
    // resolve() via the inner helper so the bound applies here too.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.symlink("/b", "/a").unwrap();
    vfs.symlink("/a", "/b").unwrap();

    let err = vfs.resolve_nofollow("/a/leaf").unwrap_err();
    assert_eq!(err, FsError::SymLoop);
}

#[test]
fn stat_follows_intermediate_symlink_to_target() {
    // /linkdir → /realdir; /realdir/file exists. Pre-slice-7 stat
    // on /linkdir/file errored with NotADirectory because Vfs::stat
    // routes through resolve_nofollow and that didn't follow
    // intermediates. Post-slice-7 stat returns the target file's
    // metadata (RegularFile), matching POSIX lstat semantics where
    // intermediate symlinks are always dereferenced.
    let mut vfs = fresh_vfs_with_root_tmpfs();
    vfs.mkdir("/realdir", 0o755).unwrap();
    vfs.create("/realdir/file", 0o644).unwrap();
    vfs.write("/realdir/file", 0, b"hello").unwrap();
    vfs.symlink("/realdir", "/linkdir").unwrap();

    let st = vfs.stat("/linkdir/file").unwrap();
    assert_eq!(st.ty, NodeType::RegularFile);
    assert_eq!(st.size, 5);
}
