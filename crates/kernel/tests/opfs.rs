//! OPFS isolation tests (T061).
//!
//! Runs via `cargo test -p kernel`. Covers the on-disk layout,
//! the MockBlockDevice, the journal's commit + replay path, and
//! the OpfsFs Filesystem trait implementation as a whole — from
//! `mkfs` producing a fresh image all the way through round-
//! tripping the FR-013a starter kit.

#![cfg(feature = "native-platform")]

use std::sync::{Arc, Mutex};

use kernel::fs::opfs::block::{BlockDevice, BlockImageState, MockBlockDevice};
use kernel::fs::opfs::layout::{
    BLOCK_SIZE, DEFAULT_INODE_TABLE_BLOCKS, DEFAULT_JOURNAL_BLOCKS, MAX_FILE_BYTES,
    MIN_BLOCK_COUNT, ROOT_INO,
};
use kernel::fs::opfs::mkfs::{
    default_credits_txt, default_edit_desktop, default_files_desktop, default_init_conf,
    default_license_txt, default_pc_vga_16, default_settings_desktop, default_sysmon_desktop,
    default_terminal_desktop, default_unifont_mono_14, mkfs, starter_editing, starter_readme,
    starter_welcome,
};
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

#[derive(Clone)]
struct SharedBlockDevice(Arc<Mutex<MockBlockDevice>>);

impl BlockDevice for SharedBlockDevice {
    fn read(&mut self, lba: u64, out: &mut [u8; BLOCK_SIZE]) -> Result<(), FsError> {
        self.0.lock().unwrap().read(lba, out)
    }

    fn write(&mut self, lba: u64, buf: &[u8; BLOCK_SIZE]) -> Result<(), FsError> {
        self.0.lock().unwrap().write(lba, buf)
    }

    fn flush(&mut self) -> Result<(), FsError> {
        self.0.lock().unwrap().flush()
    }

    fn block_count(&self) -> u64 {
        self.0.lock().unwrap().block_count()
    }
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
    assert!(
        sb.data_block_free < sb.data_block_count,
        "mkfs consumed some data blocks"
    );
}

#[test]
fn storage_usage_matches_superblock_counters() {
    let fs = fresh_fs();
    let sb = fs.superblock();
    let usage = fs.storage_usage().expect("opfs reports storage usage");

    assert_eq!(usage.quota_bytes, TEST_BLOCKS * BLOCK_SIZE as u64);
    assert_eq!(
        usage.used_bytes,
        (sb.total_blocks - sb.data_block_free) * BLOCK_SIZE as u64,
    );
    assert_eq!(usage.file_count, sb.inode_count - sb.inode_free);
}

#[test]
fn storage_usage_updates_after_allocations() {
    let mut fs = fresh_fs();
    let before = fs.storage_usage().expect("opfs reports storage usage");

    let ino = fs.create(ROOT_INO, "usage.txt", 0o644).unwrap();
    fs.write(ino, 0, b"storage accounting").unwrap();

    let after = fs.storage_usage().expect("opfs reports storage usage");
    assert!(after.file_count > before.file_count);
    assert!(after.used_bytes > before.used_bytes);
    assert_eq!(after.quota_bytes, before.quota_bytes);
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
fn mkfs_minimum_matches_the_complete_seeded_root() {
    let fs = mkfs(Box::new(MockBlockDevice::new(MIN_BLOCK_COUNT)))
        .expect("declared minimum must hold every bundled root asset");
    assert_eq!(fs.superblock().total_blocks, MIN_BLOCK_COUNT);
    assert_eq!(
        fs.superblock().data_block_free,
        0,
        "the minimum is the measured exact fresh-root boundary"
    );

    match mkfs(Box::new(MockBlockDevice::new(MIN_BLOCK_COUNT - 1))) {
        Ok(_) => panic!("one block below the declared minimum must fail"),
        Err(error) => assert_eq!(error, FsError::NoSpace),
    }
}

#[test]
fn mkfs_system_tree_exists() {
    let mut fs = fresh_fs();
    for top in [
        "bin", "dev", "etc", "home", "opt", "proc", "run", "tmp", "usr",
    ] {
        fs.lookup(ROOT_INO, top)
            .unwrap_or_else(|e| panic!("/{top} missing: {e:?}"));
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

#[test]
fn mkfs_installs_default_license_and_credits() {
    let mut fs = fresh_fs();

    // Walk to /usr/share/doc/pmos.
    let usr_ino = fs.lookup(ROOT_INO, "usr").unwrap();
    let share_ino = fs.lookup(usr_ino, "share").unwrap();
    let doc_ino = fs.lookup(share_ino, "doc").unwrap();
    let pmos_ino = fs.lookup(doc_ino, "pmos").unwrap();

    // LICENSE.txt exists as a regular file at mode 0o644, byte-
    // equal to the bundled asset.
    let license_ino = fs.lookup(pmos_ino, "LICENSE.txt").unwrap();
    let license_stat = fs.stat(license_ino).unwrap();
    assert_eq!(license_stat.ty, NodeType::RegularFile);
    assert_eq!(license_stat.mode, 0o644);
    let mut buf = vec![0u8; default_license_txt().len()];
    let n = fs.read(license_ino, 0, &mut buf).unwrap();
    assert_eq!(n, default_license_txt().len());
    assert_eq!(&buf[..n], default_license_txt());

    // Light "not a placeholder" check: header mentions MIT License.
    let license_text = core::str::from_utf8(default_license_txt()).unwrap();
    assert!(
        license_text.contains("MIT License"),
        "LICENSE.txt does not advertise the MIT License header",
    );

    // CREDITS.txt exists as a regular file at mode 0o644, byte-
    // equal to the bundled asset.
    let credits_ino = fs.lookup(pmos_ino, "CREDITS.txt").unwrap();
    let credits_stat = fs.stat(credits_ino).unwrap();
    assert_eq!(credits_stat.ty, NodeType::RegularFile);
    assert_eq!(credits_stat.mode, 0o644);
    let mut buf = vec![0u8; default_credits_txt().len()];
    let n = fs.read(credits_ino, 0, &mut buf).unwrap();
    assert_eq!(n, default_credits_txt().len());
    assert_eq!(&buf[..n], default_credits_txt());

    // Light "not a placeholder" check: mentions PMos.
    let credits_text = core::str::from_utf8(default_credits_txt()).unwrap();
    assert!(
        credits_text.contains("PMos"),
        "CREDITS.txt does not mention PMos",
    );
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
fn single_indirect_file_round_trips_past_48_kib() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "big.bin", 0o644).unwrap();
    let mut data = vec![0u8; 96 * 1024 + 17];
    for (index, byte) in data.iter_mut().enumerate() {
        *byte = ((index * 31 + 7) & 0xff) as u8;
    }
    let n = fs.write(ino, 0, &data).unwrap();
    assert_eq!(n, data.len());

    let mut read_back = vec![0u8; data.len()];
    let n = fs.read(ino, 0, &mut read_back).unwrap();
    assert_eq!(n, data.len());
    assert_eq!(read_back, data);
}

#[test]
fn single_indirect_file_survives_remount() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "installed.wasm", 0o755).unwrap();
    let data: Vec<u8> = (0..80 * 1024 + 3)
        .map(|index| ((index * 17 + 11) & 0xff) as u8)
        .collect();
    fs.write(ino, 0, &data).unwrap();
    fs.sync().unwrap();

    let device = fs.into_device();
    let mut remounted = OpfsFs::mount(device).unwrap();
    let ino = remounted.lookup(ROOT_INO, "installed.wasm").unwrap();
    let mut read_back = vec![0u8; data.len()];
    let n = remounted.read(ino, 0, &mut read_back).unwrap();
    assert_eq!(n, data.len());
    assert_eq!(read_back, data);
}

#[test]
fn single_indirect_capacity_is_enforced_before_allocation() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "too-big.bin", 0o644).unwrap();
    let next_free_before = fs.superblock().next_free_data_block;

    let err = fs.write(ino, MAX_FILE_BYTES, &[1]).unwrap_err();
    assert_eq!(err, FsError::NoSpace);
    assert_eq!(fs.superblock().next_free_data_block, next_free_before);
}

#[test]
fn single_indirect_capacity_is_reachable_with_syscall_sized_writes() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "capacity.bin", 0o644).unwrap();
    let chunk = vec![0x6d; 32 * 1024];
    let mut offset = 0u64;
    while offset < MAX_FILE_BYTES {
        let remaining = (MAX_FILE_BYTES - offset) as usize;
        let write_len = remaining.min(chunk.len());
        let written = fs.write(ino, offset, &chunk[..write_len]).unwrap();
        assert_eq!(written, write_len);
        offset += written as u64;
    }

    assert_eq!(fs.stat(ino).unwrap().size, MAX_FILE_BYTES);
    let mut tail = [0u8; 17];
    let n = fs
        .read(ino, MAX_FILE_BYTES - tail.len() as u64, &mut tail)
        .unwrap();
    assert_eq!(n, tail.len());
    assert_eq!(tail, [0x6d; 17]);

    assert_eq!(
        fs.write(ino, MAX_FILE_BYTES, &[0x6d]).unwrap_err(),
        FsError::NoSpace,
    );
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
    assert_eq!(
        fs.lookup(ROOT_INO, "gone.txt").unwrap_err(),
        FsError::NotFound
    );
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
    assert_eq!(
        fs.lookup(ROOT_INO, "src.txt").unwrap_err(),
        FsError::NotFound
    );
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
fn executable_mode_change_survives_remount() {
    let mut fs = fresh_fs();
    let file = fs.create(ROOT_INO, "installed.wasm", 0o644).unwrap();
    fs.write(file, 0, b"\0asm\x01\0\0\0").unwrap();
    fs.set_mode(file, 0o755).unwrap();
    fs.sync().unwrap();

    let device = fs.into_device();
    let mut reopened = OpfsFs::mount(device).unwrap();
    let file = reopened.lookup(ROOT_INO, "installed.wasm").unwrap();
    assert_eq!(reopened.stat(file).unwrap().mode, 0o755);
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
fn truncate_across_indirect_boundary_discards_old_tail() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "indirect-truncate", 0o644).unwrap();
    let data: Vec<u8> = (0..72 * 1024)
        .map(|index| ((index * 13 + 5) & 0xff) as u8)
        .collect();
    fs.write(ino, 0, &data).unwrap();

    let retained = 48 * 1024 + 29;
    fs.truncate(ino, retained as u64).unwrap();
    fs.truncate(ino, data.len() as u64).unwrap();

    let mut read_back = vec![0xff; data.len()];
    let n = fs.read(ino, 0, &mut read_back).unwrap();
    assert_eq!(n, data.len());
    assert_eq!(&read_back[..retained], &data[..retained]);
    assert!(read_back[retained..].iter().all(|byte| *byte == 0));

    fs.truncate(ino, (BLOCK_SIZE + 7) as u64).unwrap();
    fs.truncate(ino, (64 * 1024) as u64).unwrap();
    let mut after_direct_shrink = vec![0xff; 64 * 1024];
    fs.read(ino, 0, &mut after_direct_shrink).unwrap();
    assert_eq!(
        &after_direct_shrink[..BLOCK_SIZE + 7],
        &data[..BLOCK_SIZE + 7]
    );
    assert!(after_direct_shrink[BLOCK_SIZE + 7..]
        .iter()
        .all(|byte| *byte == 0));
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
fn canonical_home_content_survives_filesystem_reinstantiation() {
    let mut fs = fresh_fs();
    let home = fs.lookup(ROOT_INO, "home").unwrap();
    let user = fs.lookup(home, "user").unwrap();
    let notes = fs.create(user, "persistent-notes.txt", 0o644).unwrap();
    fs.write(notes, 0, b"survives a fresh kernel filesystem")
        .unwrap();
    fs.sync().unwrap();

    let device = fs.into_device();
    let mut remounted = OpfsFs::mount(device).expect("remount persistent root");
    let home = remounted.lookup(ROOT_INO, "home").unwrap();
    let user = remounted.lookup(home, "user").unwrap();
    let notes = remounted.lookup(user, "persistent-notes.txt").unwrap();
    let mut buf = [0u8; 64];
    let n = remounted.read(notes, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"survives a fresh kernel filesystem");
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

#[test]
fn newly_created_image_state_is_the_only_state_that_formats() {
    let fs = OpfsFs::open_image(fresh_device(), BlockImageState::NewlyCreated)
        .expect("new image should be formatted");
    assert_eq!(fs.superblock().total_blocks, TEST_BLOCKS);
}

#[test]
fn existing_empty_image_fails_without_any_format_writes() {
    let storage = Arc::new(Mutex::new(MockBlockDevice::new(TEST_BLOCKS)));
    let device = SharedBlockDevice(Arc::clone(&storage));

    let error = match OpfsFs::open_image(Box::new(device), BlockImageState::Existing) {
        Ok(_) => panic!("existing empty image must not be formatted"),
        Err(error) => error,
    };

    assert_eq!(error, FsError::Io);
    let storage = storage.lock().unwrap();
    assert_eq!(storage.total_writes(), 0);
    assert_eq!(storage.allocated_blocks(), 0);
}

#[test]
fn corrupt_existing_image_fails_without_overwriting_any_blocks() {
    let storage = Arc::new(Mutex::new(MockBlockDevice::new(TEST_BLOCKS)));
    let mut corrupt_superblock = [0xA5; BLOCK_SIZE];
    corrupt_superblock[..8].copy_from_slice(b"CORRUPT!");
    let sentinel = [0x3C; BLOCK_SIZE];
    {
        let mut inner = storage.lock().unwrap();
        inner.write(0, &corrupt_superblock).unwrap();
        inner.write(73, &sentinel).unwrap();
    }
    let writes_before_mount = storage.lock().unwrap().total_writes();
    let device = SharedBlockDevice(Arc::clone(&storage));

    let error = match OpfsFs::open_image(Box::new(device), BlockImageState::Existing) {
        Ok(_) => panic!("corrupt existing image must not be formatted"),
        Err(error) => error,
    };

    assert_eq!(error, FsError::Io);
    let storage = storage.lock().unwrap();
    assert_eq!(storage.total_writes(), writes_before_mount);
    assert_eq!(storage.raw_block(0), Some(&corrupt_superblock));
    assert_eq!(storage.raw_block(73), Some(&sentinel));
}

// ---- Real vnode timestamps ----------------------------------------
//
// OPFS's on-disk inode already carries `atime_ns` / `mtime_ns` /
// `ctime_ns` — the disk format has always had slots for them.
// Pre-slice, create/mkdir hard-coded all three to 0 and write/
// truncate left them untouched, so stat always reported 0. With
// the touch-on-write slice wiring `Platform::now_realtime_ns()`
// into create/mkdir/write/truncate, the fields carry meaningful
// values and persist across the journal commit + replay path.

#[test]
fn opfs_create_sets_nonzero_timestamps() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "f", 0o644).unwrap();
    let st = fs.stat(ino).unwrap();
    assert!(st.atime_ns > 0, "atime");
    assert!(st.mtime_ns > 0, "mtime");
    assert!(st.ctime_ns > 0, "ctime");
    // Fresh file gets one now() applied to all three.
    assert_eq!(st.atime_ns, st.mtime_ns);
    assert_eq!(st.mtime_ns, st.ctime_ns);
}

#[test]
fn opfs_mkdir_sets_nonzero_timestamps() {
    let mut fs = fresh_fs();
    let ino = fs.mkdir(ROOT_INO, "d", 0o755).unwrap();
    let st = fs.stat(ino).unwrap();
    assert!(st.atime_ns > 0);
    assert!(st.mtime_ns > 0);
    assert!(st.ctime_ns > 0);
}

#[test]
fn opfs_write_advances_mtime_and_ctime_and_leaves_atime() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "f", 0o644).unwrap();
    let before = fs.stat(ino).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(1));
    fs.write(ino, 0, b"hello").unwrap();

    let after = fs.stat(ino).unwrap();
    assert!(after.mtime_ns > before.mtime_ns, "mtime advanced");
    assert!(after.ctime_ns > before.ctime_ns, "ctime advanced");
    // noatime in v1.
    assert_eq!(after.atime_ns, before.atime_ns);
}

#[test]
fn opfs_truncate_advances_mtime_and_ctime() {
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "f", 0o644).unwrap();
    fs.write(ino, 0, b"abcdef").unwrap();
    let before = fs.stat(ino).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(1));
    fs.truncate(ino, 3).unwrap();

    let after = fs.stat(ino).unwrap();
    assert!(after.mtime_ns > before.mtime_ns);
    assert!(after.ctime_ns > before.ctime_ns);
}

#[test]
fn opfs_timestamps_survive_remount() {
    // The journal + inode-on-disk persistence contract: mtime set
    // before unmount is readable byte-for-byte after remount. This
    // is the pin that catches a slice that forgets to route the
    // timestamp update through `stage_inode_write`.
    let mut fs = fresh_fs();
    let ino = fs.create(ROOT_INO, "f", 0o644).unwrap();
    fs.write(ino, 0, b"persisted").unwrap();
    let before = fs.stat(ino).unwrap();
    fs.sync().unwrap();

    let device = fs.into_device();
    let mut fs2 = OpfsFs::mount(device).expect("remount");
    let after = fs2.stat(ino).unwrap();
    assert_eq!(after.mtime_ns, before.mtime_ns);
    assert_eq!(after.ctime_ns, before.ctime_ns);
    assert_eq!(after.atime_ns, before.atime_ns);
}

// ---- T096: /etc/init.conf default asset ----------------------------

#[test]
fn mkfs_installs_default_init_conf() {
    let mut fs = fresh_fs();
    let etc_ino = fs.lookup(ROOT_INO, "etc").unwrap();
    let conf_ino = fs.lookup(etc_ino, "init.conf").unwrap();

    let st = fs.stat(conf_ino).unwrap();
    assert_eq!(st.ty, NodeType::RegularFile);
    assert_eq!(st.mode, 0o644);
    assert_eq!(st.size as usize, default_init_conf().len());

    let mut buf = vec![0u8; default_init_conf().len()];
    let n = fs.read(conf_ino, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], default_init_conf());
}

// ---- T125: /usr/share/applications default desktop entries ----------

#[test]
fn mkfs_installs_default_desktop_entries() {
    let mut fs = fresh_fs();
    let usr_ino = fs.lookup(ROOT_INO, "usr").unwrap();
    let share_ino = fs.lookup(usr_ino, "share").unwrap();
    let apps_ino = fs.lookup(share_ino, "applications").unwrap();

    let cases: &[(&str, &[u8])] = &[
        ("terminal.desktop", default_terminal_desktop()),
        ("files.desktop", default_files_desktop()),
        ("edit.desktop", default_edit_desktop()),
        ("settings.desktop", default_settings_desktop()),
        ("sysmon.desktop", default_sysmon_desktop()),
    ];

    for (name, expected) in cases {
        let ino = fs
            .lookup(apps_ino, name)
            .unwrap_or_else(|_| panic!("mkfs missing /usr/share/applications/{name}"));
        let st = fs.stat(ino).unwrap();
        assert_eq!(st.ty, NodeType::RegularFile, "{name} not a regular file");
        assert_eq!(st.mode, 0o644, "{name} wrong mode");
        assert_eq!(st.size as usize, expected.len(), "{name} wrong size");

        let mut buf = vec![0u8; expected.len()];
        let n = fs.read(ino, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], *expected, "{name} content mismatch");

        // Spot-check: every entry declares at least DISPLAY_CLIENT.
        let content = core::str::from_utf8(expected).unwrap();
        assert!(
            content.contains("X-PMos-Caps=") && content.contains("DISPLAY_CLIENT"),
            "{name} missing X-PMos-Caps=...DISPLAY_CLIENT"
        );
    }

    // Check cap hints specific to settings and sysmon.
    let settings_content = core::str::from_utf8(default_settings_desktop()).unwrap();
    assert!(
        settings_content.contains("KEYMAP_ADMIN"),
        "settings.desktop missing KEYMAP_ADMIN"
    );

    let sysmon_content = core::str::from_utf8(default_sysmon_desktop()).unwrap();
    assert!(
        sysmon_content.contains("PROC_ENUMERATE"),
        "sysmon.desktop missing PROC_ENUMERATE"
    );
    assert!(
        sysmon_content.contains("PROC_KILL_ANY"),
        "sysmon.desktop missing PROC_KILL_ANY"
    );
}

#[test]
fn mkfs_installs_both_terminal_font_atlases() {
    let mut fs = fresh_fs();
    let usr_ino = fs.lookup(ROOT_INO, "usr").unwrap();
    let share_ino = fs.lookup(usr_ino, "share").unwrap();
    let fonts_ino = fs.lookup(share_ino, "fonts").unwrap();

    for (name, expected) in [
        ("unifont-mono-14.pbm", default_unifont_mono_14()),
        ("pc-vga-16.pbm", default_pc_vga_16()),
    ] {
        let ino = fs.lookup(fonts_ino, name).unwrap();
        let stat = fs.stat(ino).unwrap();
        assert_eq!(stat.ty, NodeType::RegularFile);
        assert_eq!(stat.mode, 0o644);
        assert_eq!(stat.size as usize, expected.len());

        let mut bytes = vec![0; expected.len()];
        let read = fs.read(ino, 0, &mut bytes).unwrap();
        assert_eq!(&bytes[..read], expected);
        assert!(bytes.starts_with(b"P1\n"));
    }
}
