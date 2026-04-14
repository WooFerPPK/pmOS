//! OPFS isolation tests (T061).
//!
//! Runs via `cargo test -p kernel`. Covers the on-disk layout,
//! the MockBlockDevice, the journal's commit + replay path, and
//! the OpfsFs Filesystem trait implementation as a whole — from
//! `mkfs` producing a fresh image all the way through round-
//! tripping the FR-013a starter kit.

#![cfg(feature = "native-platform")]

use kernel::fs::opfs::block::{BlockDevice, MockBlockDevice};
use kernel::fs::opfs::layout::{
    BLOCK_SIZE, DEFAULT_INODE_TABLE_BLOCKS, DEFAULT_JOURNAL_BLOCKS, ROOT_INO,
};
use kernel::fs::opfs::mkfs::{mkfs, starter_editing, starter_readme, starter_welcome};
use kernel::fs::opfs::OpfsFs;
use kernel::vfs::{Filesystem, FsError, NodeType};

// ---- Test helpers ---------------------------------------------------

/// Total block count for the standard test device. Big enough to
/// hold the default inode table + journal + ~2000 data blocks.
const TEST_BLOCKS: u64 = 4096;

fn fresh_device() -> Box<MockBlockDevice> {
    Box::new(MockBlockDevice::new(TEST_BLOCKS))
}

fn fresh_fs() -> OpfsFs {
    mkfs(fresh_device()).expect("mkfs")
}

// ---- Layout + mkfs --------------------------------------------------

#[test]
fn mkfs_produces_a_readable_superblock() {
    let fs = fresh_fs();
    let sb = fs.superblock();
    assert_eq!(sb.block_size as usize, BLOCK_SIZE);
    assert_eq!(sb.journal_blocks, DEFAULT_JOURNAL_BLOCKS);
    assert_eq!(sb.inode_table_blocks, DEFAULT_INODE_TABLE_BLOCKS);
    assert_eq!(sb.root_ino, ROOT_INO);
    assert!(sb.inode_free < sb.inode_count, "mkfs consumed some inodes");
    assert!(sb.data_block_free < sb.data_block_count, "mkfs consumed some data blocks");
}

#[test]
fn mkfs_refuses_tiny_device() {
    let device = Box::new(MockBlockDevice::new(10));
    // `unwrap_err()` requires Debug on Ok; OpfsFs contains a
    // Box<dyn BlockDevice> which can't derive Debug, so we
    // match by hand.
    match mkfs(device) {
        Ok(_) => panic!("mkfs on a 10-block device should fail"),
        Err(e) => assert_eq!(e, FsError::NoSpace),
    }
}

#[test]
fn mkfs_system_tree_exists() {
    let mut fs = fresh_fs();
    for top in ["bin", "dev", "etc", "home", "opt", "proc", "run", "tmp", "usr"] {
        fs.lookup(ROOT_INO, top).unwrap_or_else(|e| panic!("/{top} missing: {e:?}"));
    }
    let usr_ino = fs.lookup(ROOT_INO, "usr").unwrap();
    fs.lookup(usr_ino, "bin").unwrap();
    let share_ino = fs.lookup(usr_ino, "share").unwrap();
    fs.lookup(share_ino, "applications").unwrap();
    let home_ino = fs.lookup(ROOT_INO, "home").unwrap();
    fs.lookup(home_ino, "user").unwrap();
}

#[test]
fn fr_013a_starter_kit_is_present_and_matches_source() {
    let mut fs = fresh_fs();

    // Walk to /home/user.
    let home_ino = fs.lookup(ROOT_INO, "home").unwrap();
    let user_ino = fs.lookup(home_ino, "user").unwrap();

    // README.md exists and has the expected content.
    let readme_ino = fs.lookup(user_ino, "README.md").unwrap();
    let mut buf = vec![0u8; starter_readme().len() + 32];
    let n = fs.read(readme_ino, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], starter_readme());
    assert_eq!(n, starter_readme().len());

    // Downloads/ exists and is empty.
    let downloads_ino = fs.lookup(user_ino, "Downloads").unwrap();
    let mut entries = Vec::new();
    fs.readdir(downloads_ino, &mut entries).unwrap();
    assert!(entries.is_empty());

    // Documents/ exists and contains two files.
    let docs_ino = fs.lookup(user_ino, "Documents").unwrap();
    let welcome_ino = fs.lookup(docs_ino, "welcome.txt").unwrap();
    let mut buf = vec![0u8; starter_welcome().len()];
    let n = fs.read(welcome_ino, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], starter_welcome());

    let editing_ino = fs.lookup(docs_ino, "editing.md").unwrap();
    let mut buf = vec![0u8; starter_editing().len()];
    let n = fs.read(editing_ino, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], starter_editing());

    // Pictures/ exists and is empty.
    let pictures_ino = fs.lookup(user_ino, "Pictures").unwrap();
    let mut entries = Vec::new();
    fs.readdir(pictures_ino, &mut entries).unwrap();
    assert!(entries.is_empty());
}

// ---- Filesystem trait coverage --------------------------------------

#[test]
fn create_write_read_round_trip() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "hello.txt", 0o644).unwrap();
    let n = fs.write(ino, 0, b"hello, world\n").unwrap();
    assert_eq!(n, 13);
    let mut buf = [0u8; 64];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello, world\n");
}

#[test]
fn write_then_extend_then_read_all() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "big.txt", 0o644).unwrap();
    fs.write(ino, 0, b"abc").unwrap();
    fs.write(ino, 3, b"def").unwrap();
    fs.write(ino, 6, b"ghi").unwrap();
    let mut buf = [0u8; 16];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"abcdefghi");
    assert_eq!(fs.stat(ino).unwrap().size, 9);
}

#[test]
fn write_at_offset_past_eof_zero_fills() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "sparse.bin", 0o644).unwrap();
    fs.write(ino, 8, b"xyz").unwrap();
    assert_eq!(fs.stat(ino).unwrap().size, 11);
    let mut buf = [0xAAu8; 16];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    assert_eq!(n, 11);
    assert_eq!(&buf[..8], &[0u8; 8]);
    assert_eq!(&buf[8..11], b"xyz");
}

#[test]
fn large_write_spans_multiple_blocks() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "multi.bin", 0o644).unwrap();
    // 10 KiB — spans 3 direct blocks (at 4 KiB each).
    let mut data = vec![0u8; 10 * 1024];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i & 0xFF) as u8;
    }
    let n = fs.write(ino, 0, &data).unwrap();
    assert_eq!(n, data.len());

    let mut read_back = vec![0u8; data.len()];
    let n = fs.read(ino, 0, &mut read_back).unwrap();
    assert_eq!(n, data.len());
    assert_eq!(read_back, data);
}

#[test]
fn max_file_size_is_48_kib_for_direct_blocks_only() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "big.bin", 0o644).unwrap();
    // 48 KiB = exactly 12 direct blocks. Should fit.
    let data = vec![0x55u8; 48 * 1024];
    let n = fs.write(ino, 0, &data).unwrap();
    assert_eq!(n, data.len());

    // 49 KiB would spill past the 12 direct slots → NoSpace in v1.
    let too_big = vec![0u8; 49 * 1024];
    let err = fs.write(ino, 0, &too_big).unwrap_err();
    assert_eq!(err, FsError::NoSpace);
}

#[test]
fn read_past_eof_returns_zero() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "short.txt", 0o644).unwrap();
    fs.write(ino, 0, b"hi").unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(fs.read(ino, 2, &mut buf).unwrap(), 0);
    assert_eq!(fs.read(ino, 100, &mut buf).unwrap(), 0);
}

#[test]
fn create_existing_name_fails() {
    let mut fs = fresh_fs();
    fs.create(ROOT_INO, "x", 0o644).unwrap();
    assert_eq!(
        fs.create(ROOT_INO, "x", 0o644).unwrap_err(),
        FsError::AlreadyExists
    );
}

#[test]
fn mkdir_then_nested_create() {
    let mut fs = fresh_fs();
    let sub = fs.mkdir(ROOT_INO, "sub", 0o755).unwrap();
    let file = fs.create(sub, "f.txt", 0o644).unwrap();
    fs.write(file, 0, b"nested").unwrap();
    let mut buf = [0u8; 16];
    let n = fs.read(file, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"nested");
}

#[test]
fn readdir_yields_all_entries() {
    let mut fs = fresh_fs();
    let sub = fs.mkdir(ROOT_INO, "zoo", 0o755).unwrap();
    fs.create(sub, "a", 0o644).unwrap();
    fs.create(sub, "b", 0o644).unwrap();
    fs.mkdir(sub, "c", 0o755).unwrap();

    let mut entries = Vec::new();
    fs.readdir(sub, &mut entries).unwrap();
    assert_eq!(entries.len(), 3);
    let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    assert!(names.contains(&"a".to_string()));
    assert!(names.contains(&"b".to_string()));
    assert!(names.contains(&"c".to_string()));

    // And each entry has the right type.
    for e in &entries {
        if e.name == "c" {
            assert_eq!(e.ty, NodeType::Directory);
        } else {
            assert_eq!(e.ty, NodeType::RegularFile);
        }
    }
}

#[test]
fn unlink_removes_file() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "gone.txt", 0o644).unwrap();
    fs.write(ino, 0, b"bye").unwrap();
    fs.unlink(ROOT_INO, "gone.txt").unwrap();
    assert_eq!(fs.lookup(ROOT_INO, "gone.txt").unwrap_err(), FsError::NotFound);
}

#[test]
fn unlink_of_directory_is_an_error() {
    let mut fs = fresh_fs();
    fs.mkdir(ROOT_INO, "d", 0o755).unwrap();
    assert_eq!(fs.unlink(ROOT_INO, "d").unwrap_err(), FsError::IsADirectory);
}

#[test]
fn rmdir_empty_and_nonempty() {
    let mut fs = fresh_fs();
    let a = fs.mkdir(ROOT_INO, "a", 0o755).unwrap();
    fs.create(a, "f", 0o644).unwrap();
    assert_eq!(fs.rmdir(ROOT_INO, "a").unwrap_err(), FsError::NotEmpty);
    fs.unlink(a, "f").unwrap();
    fs.rmdir(ROOT_INO, "a").unwrap();
    assert_eq!(fs.lookup(ROOT_INO, "a").unwrap_err(), FsError::NotFound);
}

#[test]
fn rename_within_directory() {
    let mut fs = fresh_fs();
    let src = fs.create(ROOT_INO, "src.txt", 0o644).unwrap();
    fs.write(src, 0, b"content").unwrap();
    fs.rename(ROOT_INO, "src.txt", ROOT_INO, "dst.txt").unwrap();
    assert_eq!(fs.lookup(ROOT_INO, "src.txt").unwrap_err(), FsError::NotFound);
    let dst = fs.lookup(ROOT_INO, "dst.txt").unwrap();
    assert_eq!(dst, src);
    let mut buf = [0u8; 16];
    let n = fs.read(dst, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"content");
}

#[test]
fn rename_across_directories() {
    let mut fs = fresh_fs();
    let a = fs.mkdir(ROOT_INO, "a", 0o755).unwrap();
    let b = fs.mkdir(ROOT_INO, "b", 0o755).unwrap();
    let x = fs.create(a, "x", 0o644).unwrap();
    fs.write(x, 0, b"moved").unwrap();
    fs.rename(a, "x", b, "y").unwrap();

    let y = fs.lookup(b, "y").unwrap();
    assert_eq!(y, x);
    let mut buf = [0u8; 16];
    let n = fs.read(y, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"moved");

    let mut entries = Vec::new();
    fs.readdir(a, &mut entries).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn rename_replaces_existing_destination_file() {
    let mut fs = fresh_fs();
    let src = fs.create(ROOT_INO, "src", 0o644).unwrap();
    fs.write(src, 0, b"SRC").unwrap();
    let _dst = fs.create(ROOT_INO, "dst", 0o644).unwrap();
    fs.write(_dst, 0, b"DST").unwrap();
    fs.rename(ROOT_INO, "src", ROOT_INO, "dst").unwrap();

    assert_eq!(fs.lookup(ROOT_INO, "src").unwrap_err(), FsError::NotFound);
    let dst_ino = fs.lookup(ROOT_INO, "dst").unwrap();
    assert_eq!(dst_ino, src);
    let mut buf = [0u8; 16];
    let n = fs.read(dst_ino, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"SRC");
}

#[test]
fn stat_reports_type_and_size() {
    let mut fs = fresh_fs();
    let dir = fs.mkdir(ROOT_INO, "d", 0o755).unwrap();
    let file = fs.create(dir, "f", 0o644).unwrap();
    fs.write(file, 0, b"12345").unwrap();

    let d_stat = fs.stat(dir).unwrap();
    assert_eq!(d_stat.ty, NodeType::Directory);
    assert_eq!(d_stat.mode, 0o755);

    let f_stat = fs.stat(file).unwrap();
    assert_eq!(f_stat.ty, NodeType::RegularFile);
    assert_eq!(f_stat.size, 5);
    assert_eq!(f_stat.mode, 0o644);
}

#[test]
fn truncate_shrinks_and_extends() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "t", 0o644).unwrap();
    fs.write(ino, 0, b"abcdef").unwrap();

    fs.truncate(ino, 3).unwrap();
    assert_eq!(fs.stat(ino).unwrap().size, 3);
    let mut buf = [0u8; 8];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"abc");

    fs.truncate(ino, 6).unwrap();
    assert_eq!(fs.stat(ino).unwrap().size, 6);
    let n = fs.read(ino, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"abc\0\0\0");
}

#[test]
fn sync_is_idempotent() {
    let mut fs = fresh_fs();
    fs.sync().unwrap();
    fs.sync().unwrap();
    fs.sync().unwrap();
}

// ---- Remount + persistence -----------------------------------------

#[test]
fn remount_sees_all_starter_kit_content() {
    // mkfs, write a file, remount, re-read.
    let device = fresh_device();
    let mut fs = mkfs(device).expect("mkfs");

    // Write something new on top of the starter kit.
    let ino = fs.create(ROOT_INO, "custom.txt", 0o644).unwrap();
    fs.write(ino, 0, b"user data that must survive").unwrap();
    fs.sync().unwrap();

    // Unmount → remount.
    let device = fs.into_device();
    let mut fs = OpfsFs::mount(device).expect("remount");

    // Starter kit is still there.
    let home = fs.lookup(ROOT_INO, "home").unwrap();
    let user = fs.lookup(home, "user").unwrap();
    let readme = fs.lookup(user, "README.md").unwrap();
    let mut buf = vec![0u8; starter_readme().len()];
    let n = fs.read(readme, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], starter_readme());

    // Our custom file is still there.
    let custom = fs.lookup(ROOT_INO, "custom.txt").unwrap();
    let mut buf = [0u8; 64];
    let n = fs.read(custom, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"user data that must survive");
}

#[test]
fn remount_after_multiple_mutations() {
    let device = fresh_device();
    let mut fs = mkfs(device).expect("mkfs");

    // Do a bunch of things.
    for i in 0..10 {
        let name = format!("f{i}.txt");
        let ino = fs.create(ROOT_INO, &name, 0o644).unwrap();
        fs.write(ino, 0, format!("content {i}").as_bytes()).unwrap();
    }
    fs.sync().unwrap();

    let device = fs.into_device();
    let mut fs = OpfsFs::mount(device).expect("remount");

    for i in 0..10 {
        let name = format!("f{i}.txt");
        let ino = fs.lookup(ROOT_INO, &name).unwrap();
        let mut buf = [0u8; 32];
        let n = fs.read(ino, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], format!("content {i}").as_bytes());
    }
}

#[test]
fn mount_generation_increments_on_remount() {
    let device = fresh_device();
    let fs = mkfs(device).expect("mkfs");
    let first_gen = fs.superblock().mount_generation;
    fs.superblock(); // silence warning
    let device = fs.into_device();
    let fs = OpfsFs::mount(device).expect("remount");
    let second_gen = fs.superblock().mount_generation;
    assert!(second_gen > first_gen, "{second_gen} > {first_gen}");
}

// ---- Journal commit / replay direct tests --------------------------

#[test]
fn journal_commit_makes_ops_durable_across_remount() {
    // Simulates the FR-014 abrupt-close path: commit several txns,
    // remount, verify the committed content is there.
    let device = fresh_device();
    let mut fs = mkfs(device).expect("mkfs");

    // Create a file, write to it, sync.
    let ino = fs.create(ROOT_INO, "journal.txt", 0o644).unwrap();
    fs.write(ino, 0, b"first write").unwrap();
    fs.sync().unwrap();

    // Second round without explicit sync — let the normal
    // commit_and_apply path in write() handle durability.
    fs.write(ino, 11, b"; second write").unwrap();

    let device = fs.into_device();
    let mut fs = OpfsFs::mount(device).expect("remount");
    let ino = fs.lookup(ROOT_INO, "journal.txt").unwrap();
    let mut buf = vec![0u8; 64];
    let n = fs.read(ino, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"first write; second write");
}

#[test]
fn invalid_superblock_magic_fails_mount() {
    // Allocate a device, write zeros to block 0, try to mount.
    let mut device = MockBlockDevice::new(TEST_BLOCKS);
    let zero = [0u8; BLOCK_SIZE];
    device.write(0, &zero).unwrap();
    match OpfsFs::mount(Box::new(device)) {
        Ok(_) => panic!("mount with zero magic should fail"),
        Err(e) => assert_eq!(e, FsError::Io),
    }
}

#[test]
fn corrupt_superblock_checksum_fails_mount() {
    // mkfs, then corrupt one byte in the superblock, then try
    // to remount.
    let device = fresh_device();
    let fs = mkfs(device).expect("mkfs");
    let mut device = fs.into_device();
    let mut block = [0u8; BLOCK_SIZE];
    device.read(0, &mut block).unwrap();
    block[200] ^= 0xFF; // flip a bit inside the protected region
    device.write(0, &block).unwrap();
    match OpfsFs::mount(device) {
        Ok(_) => panic!("mount with corrupted superblock should fail"),
        Err(e) => assert_eq!(e, FsError::Io),
    }
}
