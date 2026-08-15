#![cfg(feature = "native-platform")]

use kernel::fs::opfs::mkfs::{
    default_blue_wallpaper, default_dark_wallpaper, default_green_wallpaper, default_pc_vga_16,
    default_unifont_mono_14, default_zoneinfo_america_new_york, default_zoneinfo_asia_tokyo,
    default_zoneinfo_europe_london, default_zoneinfo_utc,
};
use kernel::fs::seed::{seed_system_defaults, seed_volatile_user_home};
use kernel::fs::tmpfs::TmpFs;
use kernel::vfs::{
    DirEntry, FileStat, Filesystem, FsError, Ino, Mode, NodeType, StorageUsage, Vfs,
};

fn empty_root() -> Vfs {
    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(TmpFs::new())).unwrap();
    vfs
}

struct ControlledTmpFs {
    inner: TmpFs,
    free_bytes: u64,
    fail_wallpaper_writes: bool,
    fail_zoneinfo_writes: bool,
    fail_font_writes: bool,
    report_storage_usage: bool,
}

impl ControlledTmpFs {
    fn new(free_bytes: u64, fail_wallpaper_writes: bool) -> Self {
        Self {
            inner: TmpFs::new(),
            free_bytes,
            fail_wallpaper_writes,
            fail_zoneinfo_writes: false,
            fail_font_writes: false,
            report_storage_usage: true,
        }
    }
}

impl Filesystem for ControlledTmpFs {
    fn root(&self) -> Ino {
        self.inner.root()
    }

    fn lookup(&mut self, dir: Ino, name: &str) -> Result<Ino, FsError> {
        self.inner.lookup(dir, name)
    }

    fn read(&mut self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        self.inner.read(ino, offset, buf)
    }

    fn write(&mut self, ino: Ino, offset: u64, buf: &[u8]) -> Result<usize, FsError> {
        if self.fail_wallpaper_writes && buf.len() >= 512 * 1024 {
            return Err(FsError::NoSpace);
        }
        if self.fail_zoneinfo_writes && buf.starts_with(b"TZif") {
            return Err(FsError::NoSpace);
        }
        if self.fail_font_writes && buf.starts_with(b"P1\n") {
            return Err(FsError::NoSpace);
        }
        self.inner.write(ino, offset, buf)
    }

    fn readdir(&mut self, dir: Ino, out: &mut Vec<DirEntry>) -> Result<(), FsError> {
        self.inner.readdir(dir, out)
    }

    fn create(&mut self, dir: Ino, name: &str, mode: Mode) -> Result<Ino, FsError> {
        self.inner.create(dir, name, mode)
    }

    fn mkdir(&mut self, dir: Ino, name: &str, mode: Mode) -> Result<Ino, FsError> {
        self.inner.mkdir(dir, name, mode)
    }

    fn unlink(&mut self, dir: Ino, name: &str) -> Result<(), FsError> {
        self.inner.unlink(dir, name)
    }

    fn rmdir(&mut self, dir: Ino, name: &str) -> Result<(), FsError> {
        self.inner.rmdir(dir, name)
    }

    fn rename(
        &mut self,
        from_dir: Ino,
        from_name: &str,
        to_dir: Ino,
        to_name: &str,
    ) -> Result<(), FsError> {
        self.inner.rename(from_dir, from_name, to_dir, to_name)
    }

    fn stat(&mut self, ino: Ino) -> Result<FileStat, FsError> {
        self.inner.stat(ino)
    }

    fn truncate(&mut self, ino: Ino, new_size: u64) -> Result<(), FsError> {
        self.inner.truncate(ino, new_size)
    }

    fn sync(&mut self) -> Result<(), FsError> {
        self.inner.sync()
    }

    fn storage_usage(&self) -> Option<StorageUsage> {
        if !self.report_storage_usage {
            return None;
        }
        const QUOTA: u64 = 64 * 1024 * 1024;
        Some(StorageUsage {
            quota_bytes: QUOTA,
            used_bytes: QUOTA - self.free_bytes.min(QUOTA),
            file_count: 0,
        })
    }

    fn kind_name(&self) -> &'static str {
        "controlled-tmpfs"
    }
}

fn controlled_root(free_bytes: u64, fail_wallpaper_writes: bool) -> Vfs {
    let mut vfs = Vfs::new();
    vfs.mount(
        "/",
        Box::new(ControlledTmpFs::new(free_bytes, fail_wallpaper_writes)),
    )
    .unwrap();
    vfs
}

fn controlled_root_with_zone_failure(free_bytes: u64) -> Vfs {
    let mut filesystem = ControlledTmpFs::new(free_bytes, false);
    filesystem.fail_zoneinfo_writes = true;
    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(filesystem)).unwrap();
    vfs
}

fn controlled_volatile_root_with_zone_failure() -> Vfs {
    let mut filesystem = ControlledTmpFs::new(u64::MAX, false);
    filesystem.fail_zoneinfo_writes = true;
    filesystem.report_storage_usage = false;
    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(filesystem)).unwrap();
    vfs
}

fn controlled_root_with_font_failure(free_bytes: u64, persistent: bool) -> Vfs {
    let mut filesystem = ControlledTmpFs::new(free_bytes, false);
    filesystem.fail_font_writes = true;
    filesystem.report_storage_usage = persistent;
    let mut vfs = Vfs::new();
    vfs.mount("/", Box::new(filesystem)).unwrap();
    vfs
}

fn read_exact(vfs: &mut Vfs, path: &str, expected: &[u8]) {
    let mut actual = vec![0; expected.len()];
    let read = vfs.read(path, 0, &mut actual).unwrap();
    assert_eq!(read, expected.len());
    assert_eq!(actual, expected);
}

#[test]
fn volatile_seed_builds_coherent_system_and_home_tree() {
    let mut vfs = empty_root();
    seed_system_defaults(&mut vfs).unwrap();
    seed_volatile_user_home(&mut vfs).unwrap();

    assert_eq!(vfs.stat("/home/user").unwrap().ty, NodeType::Directory);
    assert_eq!(
        vfs.stat("/usr/share/applications/terminal.desktop")
            .unwrap()
            .ty,
        NodeType::RegularFile
    );
    let mut bytes = [0u8; 256];
    let n = vfs
        .read("/usr/share/applications/terminal.desktop", 0, &mut bytes)
        .unwrap();
    assert!(core::str::from_utf8(&bytes[..n])
        .unwrap()
        .contains("Exec=/bin/term"));

    for (path, expected) in [
        ("/usr/share/wallpapers/blue.png", default_blue_wallpaper()),
        ("/usr/share/wallpapers/green.png", default_green_wallpaper()),
        ("/usr/share/wallpapers/dark.png", default_dark_wallpaper()),
    ] {
        read_exact(&mut vfs, path, expected);
    }
    assert_eq!(
        vfs.stat("/usr/share/wallpapers/.pmos-seed-blue.png"),
        Err(FsError::NotFound),
        "successful staged rename must not expose its temporary file"
    );

    for path in [
        "/usr/share/fonts/unifont-mono-14.pbm",
        "/usr/share/fonts/pc-vga-16.pbm",
    ] {
        assert_eq!(vfs.stat(path).unwrap().ty, NodeType::RegularFile);
        let mut magic = [0u8; 2];
        let n = vfs.read(path, 0, &mut magic).unwrap();
        assert_eq!(&magic[..n], b"P1");
    }

    for (path, expected) in [
        ("/etc/zoneinfo/UTC", default_zoneinfo_utc()),
        (
            "/etc/zoneinfo/America_New_York",
            default_zoneinfo_america_new_york(),
        ),
        (
            "/etc/zoneinfo/Europe_London",
            default_zoneinfo_europe_london(),
        ),
        ("/etc/zoneinfo/Asia_Tokyo", default_zoneinfo_asia_tokyo()),
    ] {
        read_exact(&mut vfs, path, expected);
        assert_eq!(&expected[..4], b"TZif");
    }
    assert_eq!(
        vfs.stat("/etc/zoneinfo/.pmos-seed-UTC"),
        Err(FsError::NotFound),
        "successful staged rename must not expose a zoneinfo temporary file"
    );
}

#[test]
fn system_seed_is_idempotent_and_preserves_existing_files() {
    let mut vfs = empty_root();
    seed_system_defaults(&mut vfs).unwrap();
    vfs.truncate("/etc/init.conf", 0).unwrap();
    vfs.write("/etc/init.conf", 0, b"user-owned\n").unwrap();
    vfs.truncate("/usr/share/fonts/pc-vga-16.pbm", 0).unwrap();
    vfs.write("/usr/share/fonts/pc-vga-16.pbm", 0, b"custom")
        .unwrap();
    vfs.truncate("/usr/share/wallpapers/blue.png", 0).unwrap();
    vfs.write("/usr/share/wallpapers/blue.png", 0, b"custom wallpaper")
        .unwrap();
    vfs.truncate("/etc/zoneinfo/UTC", 0).unwrap();
    vfs.write("/etc/zoneinfo/UTC", 0, b"custom zone").unwrap();

    seed_system_defaults(&mut vfs).unwrap();
    let mut bytes = [0u8; 32];
    let n = vfs.read("/etc/init.conf", 0, &mut bytes).unwrap();
    assert_eq!(&bytes[..n], b"user-owned\n");
    let n = vfs
        .read("/usr/share/fonts/pc-vga-16.pbm", 0, &mut bytes)
        .unwrap();
    assert_eq!(&bytes[..n], b"custom");
    let n = vfs
        .read("/usr/share/wallpapers/blue.png", 0, &mut bytes)
        .unwrap();
    assert_eq!(&bytes[..n], b"custom wallpaper");
    let n = vfs.read("/etc/zoneinfo/UTC", 0, &mut bytes).unwrap();
    assert_eq!(&bytes[..n], b"custom zone");
    read_exact(
        &mut vfs,
        "/etc/zoneinfo/Europe_London",
        default_zoneinfo_europe_london(),
    );
}

#[test]
fn insufficient_space_skips_optional_bundles_without_partial_targets() {
    let mut vfs = controlled_root(4096, false);
    seed_system_defaults(&mut vfs).expect("small essential defaults must still seed");

    assert_eq!(vfs.stat("/usr/share/wallpapers"), Err(FsError::NotFound));
    assert_eq!(vfs.stat("/etc/zoneinfo"), Err(FsError::NotFound));
    assert_eq!(vfs.stat("/usr/share/fonts"), Err(FsError::NotFound));
    assert_eq!(
        vfs.stat("/usr/share/wallpapers/blue.png"),
        Err(FsError::NotFound)
    );
}

#[test]
fn failed_font_stage_is_cleaned_up_and_does_not_block_existing_root() {
    let mut vfs = controlled_root_with_font_failure(16 * 1024 * 1024, true);
    seed_system_defaults(&mut vfs).expect("optional font failure must not block boot");

    assert_eq!(
        vfs.stat("/usr/share/fonts/unifont-mono-14.pbm"),
        Err(FsError::NotFound)
    );
    assert_eq!(
        vfs.stat("/usr/share/fonts/.pmos-seed-unifont-mono-14.pbm"),
        Err(FsError::NotFound),
        "failed font staging must leave neither a target nor a temp file"
    );
}

#[test]
fn failed_zoneinfo_stage_is_cleaned_up_and_does_not_block_existing_root() {
    let mut vfs = controlled_root_with_zone_failure(16 * 1024 * 1024);
    seed_system_defaults(&mut vfs).expect("optional zoneinfo failure must not block boot");

    assert_eq!(vfs.stat("/etc/zoneinfo/UTC"), Err(FsError::NotFound));
    assert_eq!(
        vfs.stat("/etc/zoneinfo/.pmos-seed-UTC"),
        Err(FsError::NotFound),
        "failed zoneinfo staging must leave neither a target nor a temp file"
    );
}

#[test]
fn successful_existing_root_zoneinfo_migration_is_exact_and_atomic() {
    let mut vfs = controlled_root(16 * 1024 * 1024, false);
    seed_system_defaults(&mut vfs).expect("zoneinfo migration");

    for (path, expected) in [
        ("/etc/zoneinfo/UTC", default_zoneinfo_utc()),
        (
            "/etc/zoneinfo/America_New_York",
            default_zoneinfo_america_new_york(),
        ),
        (
            "/etc/zoneinfo/Europe_London",
            default_zoneinfo_europe_london(),
        ),
        ("/etc/zoneinfo/Asia_Tokyo", default_zoneinfo_asia_tokyo()),
    ] {
        read_exact(&mut vfs, path, expected);
    }
    assert_eq!(
        vfs.stat("/etc/zoneinfo/.pmos-seed-Asia_Tokyo"),
        Err(FsError::NotFound)
    );
    for (path, expected) in [
        (
            "/usr/share/fonts/unifont-mono-14.pbm",
            default_unifont_mono_14(),
        ),
        ("/usr/share/fonts/pc-vga-16.pbm", default_pc_vga_16()),
    ] {
        read_exact(&mut vfs, path, expected);
    }
    assert_eq!(
        vfs.stat("/usr/share/fonts/.pmos-seed-pc-vga-16.pbm"),
        Err(FsError::NotFound)
    );
}

#[test]
fn volatile_root_zoneinfo_failure_is_strict_and_atomic() {
    let mut vfs = controlled_volatile_root_with_zone_failure();
    assert_eq!(seed_system_defaults(&mut vfs), Err(FsError::NoSpace));
    assert_eq!(vfs.stat("/etc/zoneinfo/UTC"), Err(FsError::NotFound));
    assert_eq!(
        vfs.stat("/etc/zoneinfo/.pmos-seed-UTC"),
        Err(FsError::NotFound)
    );
}

#[test]
fn volatile_root_font_failure_is_strict_and_atomic() {
    let mut vfs = controlled_root_with_font_failure(u64::MAX, false);
    assert_eq!(seed_system_defaults(&mut vfs), Err(FsError::NoSpace));
    assert_eq!(
        vfs.stat("/usr/share/fonts/unifont-mono-14.pbm"),
        Err(FsError::NotFound)
    );
    assert_eq!(
        vfs.stat("/usr/share/fonts/.pmos-seed-unifont-mono-14.pbm"),
        Err(FsError::NotFound)
    );
}

#[test]
fn failed_staged_write_is_cleaned_up_and_does_not_block_boot() {
    let mut vfs = controlled_root(16 * 1024 * 1024, true);
    seed_system_defaults(&mut vfs).expect("optional wallpaper failure must not block boot");

    assert_eq!(
        vfs.stat("/usr/share/wallpapers/blue.png"),
        Err(FsError::NotFound)
    );
    assert_eq!(
        vfs.stat("/usr/share/wallpapers/.pmos-seed-blue.png"),
        Err(FsError::NotFound),
        "failed staging must never leave a visible partial target or temp"
    );
}

#[test]
fn volatile_home_seed_preserves_existing_documents() {
    let mut vfs = empty_root();
    seed_system_defaults(&mut vfs).unwrap();
    seed_volatile_user_home(&mut vfs).unwrap();
    vfs.truncate("/home/user/README.md", 0).unwrap();
    vfs.write("/home/user/README.md", 0, b"mine").unwrap();

    seed_volatile_user_home(&mut vfs).unwrap();
    let mut bytes = [0u8; 16];
    let n = vfs.read("/home/user/README.md", 0, &mut bytes).unwrap();
    assert_eq!(&bytes[..n], b"mine");
}
