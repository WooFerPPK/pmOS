//! VFS isolation tests (T055).
//!
//! Covers the VFS core (mount table, path resolver, public API)
//! against real filesystem implementations (tmpfs, devfs,
//! procfs). No browser involved; this is the Principle X gate
//! for the VFS layer.

#![cfg(feature = "native-platform")]

use kernel::fs::devfs::{DevFs, DEV_CONSOLE, DEV_NULL};
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
    assert_eq!(names, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
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
    assert_eq!(vfs.read("/foo.txt", 0, &mut buf).unwrap_err(), FsError::NotFound);
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
    assert_eq!(vfs.read("/src", 0, &mut buf).unwrap_err(), FsError::NotFound);
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
    for path in &["/foo/bar.txt", "//foo//bar.txt", "/./foo/./bar.txt", "/foo/./bar.txt/"] {
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
    assert_eq!(vfs.create("/dev/foo", 0o644).unwrap_err(), FsError::ReadOnly);
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
    assert_eq!(vfs.create("/proc/new", 0o644).unwrap_err(), FsError::ReadOnly);
    assert_eq!(vfs.unlink("/proc/version").unwrap_err(), FsError::ReadOnly);
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
            DirEntry { ref name, ty: NodeType::Directory, .. } if name == "d" => saw_dir = true,
            DirEntry { ref name, ty: NodeType::RegularFile, .. } if name == "f" => saw_file = true,
            _ => {}
        }
    }
    assert!(saw_dir);
    assert!(saw_file);
}
