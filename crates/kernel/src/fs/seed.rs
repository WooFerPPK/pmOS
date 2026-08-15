//! Idempotent root-filesystem seeding and migration.
//!
//! A freshly formatted OPFS image is populated by `opfs::mkfs`. This module
//! covers the two other boot cases: an older valid image that predates a
//! bundled system file, and the explicit volatile tmpfs fallback used when
//! persistent storage is unavailable. Existing files are never overwritten.

use crate::fs::opfs::layout::{BLOCK_SIZE, INODE_DIRECT_BLOCKS};
use crate::fs::opfs::mkfs;
use crate::vfs::{FsError, NodeType, Vfs};

const WALLPAPER_DIR: &str = "/usr/share/wallpapers";
const ZONEINFO_DIR: &str = "/etc/zoneinfo";
const FONT_DIR: &str = "/usr/share/fonts";
const ASSET_WRITE_CHUNK_BYTES: usize = 512 * 1024;

const SYSTEM_DIRS: &[&str] = &[
    "/bin",
    "/dev",
    "/etc",
    "/home",
    "/opt",
    "/proc",
    "/run",
    "/tmp",
    "/usr",
    "/usr/bin",
    "/usr/share",
    "/usr/share/applications",
    "/usr/share/doc",
    "/usr/share/doc/pmos",
];

const USER_DIRS: &[&str] = &[
    "/home/user",
    "/home/user/Downloads",
    "/home/user/Documents",
    "/home/user/Pictures",
];

/// Ensure immutable/bundled system defaults exist, preserving every existing
/// node and its contents. This doubles as the migration path for older OPFS
/// images when a new default desktop entry or system document is introduced.
pub fn seed_system_defaults(vfs: &mut Vfs) -> Result<(), FsError> {
    for path in SYSTEM_DIRS {
        ensure_dir(vfs, path)?;
    }

    for (path, content) in [
        ("/etc/init.conf", mkfs::default_init_conf()),
        (
            "/usr/share/applications/terminal.desktop",
            mkfs::default_terminal_desktop(),
        ),
        (
            "/usr/share/applications/files.desktop",
            mkfs::default_files_desktop(),
        ),
        (
            "/usr/share/applications/edit.desktop",
            mkfs::default_edit_desktop(),
        ),
        (
            "/usr/share/applications/settings.desktop",
            mkfs::default_settings_desktop(),
        ),
        (
            "/usr/share/applications/sysmon.desktop",
            mkfs::default_sysmon_desktop(),
        ),
        (
            "/usr/share/doc/pmos/LICENSE.txt",
            mkfs::default_license_txt(),
        ),
        (
            "/usr/share/doc/pmos/CREDITS.txt",
            mkfs::default_credits_txt(),
        ),
    ] {
        ensure_file(vfs, path, content)?;
    }

    // Existing persistent roots treat newly bundled zone data as a best-effort
    // migration: quota pressure or a damaged target must not prevent boot.
    // Tmpfs has no quota report and is the volatile-root path, where the
    // bundled runtime data remains a strict part of the coherent system tree.
    let persistent_root = vfs.storage_usage("/")?.is_some();
    if persistent_root {
        let _ = seed_font_defaults(vfs);
        let _ = seed_zoneinfo_defaults(vfs);
    } else {
        seed_font_defaults(vfs)?;
        seed_zoneinfo_defaults(vfs)?;
    }

    // Wallpapers are large optional presentation defaults. A near-full older
    // persistent image must still boot and retain all user data, so migration
    // preflights space and stages each missing file behind an atomic rename.
    // Fresh mkfs remains strict and always contains the complete bundle.
    let _ = seed_wallpaper_defaults(vfs);
    vfs.sync_dirty()
}

/// Populate the user-visible starter kit when booting a fresh volatile root.
/// This is intentionally not called for an existing persistent root: a user
/// who deleted or edited starter content must not have it restored at boot.
pub fn seed_volatile_user_home(vfs: &mut Vfs) -> Result<(), FsError> {
    for path in USER_DIRS {
        ensure_dir(vfs, path)?;
    }
    for (path, content) in [
        ("/home/user/README.md", mkfs::starter_readme()),
        ("/home/user/Documents/welcome.txt", mkfs::starter_welcome()),
        ("/home/user/Documents/editing.md", mkfs::starter_editing()),
    ] {
        ensure_file(vfs, path, content)?;
    }
    vfs.sync_dirty()
}

fn ensure_dir(vfs: &mut Vfs, path: &str) -> Result<(), FsError> {
    match vfs.stat(path) {
        Ok(stat) if stat.ty == NodeType::Directory => Ok(()),
        Ok(_) => Err(FsError::NotADirectory),
        Err(FsError::NotFound) => vfs.mkdir(path, 0o755).map(|_| ()),
        Err(error) => Err(error),
    }
}

fn ensure_file(vfs: &mut Vfs, path: &str, content: &[u8]) -> Result<(), FsError> {
    match vfs.stat(path) {
        Ok(stat) if stat.ty == NodeType::RegularFile => return Ok(()),
        Ok(_) => return Err(FsError::IsADirectory),
        Err(FsError::NotFound) => {}
        Err(error) => return Err(error),
    }
    vfs.create(path, 0o644)?;
    let written = vfs.write(path, 0, content)?;
    if written != content.len() {
        return Err(FsError::Io);
    }
    Ok(())
}

fn seed_font_defaults(vfs: &mut Vfs) -> Result<(), FsError> {
    let assets = [
        (
            "/usr/share/fonts/unifont-mono-14.pbm",
            "/usr/share/fonts/.pmos-seed-unifont-mono-14.pbm",
            mkfs::default_unifont_mono_14(),
        ),
        (
            "/usr/share/fonts/pc-vga-16.pbm",
            "/usr/share/fonts/.pmos-seed-pc-vga-16.pbm",
            mkfs::default_pc_vga_16(),
        ),
    ];

    let directory_missing = match vfs.stat(FONT_DIR) {
        Ok(stat) if stat.ty == NodeType::Directory => false,
        Ok(_) => return Err(FsError::NotADirectory),
        Err(FsError::NotFound) => true,
        Err(error) => return Err(error),
    };

    let mut missing = [false; 2];
    let mut required_bytes = if directory_missing {
        BLOCK_SIZE as u64
    } else {
        0
    };
    for (index, (path, _, content)) in assets.iter().enumerate() {
        if directory_missing {
            missing[index] = true;
        } else {
            match vfs.stat(path) {
                Ok(_) => continue,
                Err(FsError::NotFound) => missing[index] = true,
                Err(error) => return Err(error),
            }
        }
        required_bytes = required_bytes.saturating_add(asset_allocated_bytes(content.len()));
    }

    if !missing.iter().any(|value| *value) {
        return Ok(());
    }
    if let Some(usage) = vfs.storage_usage("/")? {
        let available = usage.quota_bytes.saturating_sub(usage.used_bytes);
        if available < required_bytes {
            return Ok(());
        }
    }

    if directory_missing {
        vfs.mkdir(FONT_DIR, 0o755)?;
    }
    for (index, (path, temporary, content)) in assets.iter().enumerate() {
        if missing[index] {
            stage_file(vfs, path, temporary, content)?;
        }
    }
    Ok(())
}

fn seed_zoneinfo_defaults(vfs: &mut Vfs) -> Result<(), FsError> {
    let assets = [
        (
            "/etc/zoneinfo/UTC",
            "/etc/zoneinfo/.pmos-seed-UTC",
            mkfs::default_zoneinfo_utc(),
        ),
        (
            "/etc/zoneinfo/America_New_York",
            "/etc/zoneinfo/.pmos-seed-America_New_York",
            mkfs::default_zoneinfo_america_new_york(),
        ),
        (
            "/etc/zoneinfo/Europe_London",
            "/etc/zoneinfo/.pmos-seed-Europe_London",
            mkfs::default_zoneinfo_europe_london(),
        ),
        (
            "/etc/zoneinfo/Asia_Tokyo",
            "/etc/zoneinfo/.pmos-seed-Asia_Tokyo",
            mkfs::default_zoneinfo_asia_tokyo(),
        ),
    ];

    let directory_missing = match vfs.stat(ZONEINFO_DIR) {
        Ok(stat) if stat.ty == NodeType::Directory => false,
        Ok(_) => return Err(FsError::NotADirectory),
        Err(FsError::NotFound) => true,
        Err(error) => return Err(error),
    };

    let mut missing = [false; 4];
    let mut required_bytes = if directory_missing {
        BLOCK_SIZE as u64
    } else {
        0
    };
    for (index, (path, _, content)) in assets.iter().enumerate() {
        if directory_missing {
            missing[index] = true;
        } else {
            match vfs.stat(path) {
                Ok(_) => continue,
                Err(FsError::NotFound) => missing[index] = true,
                Err(error) => return Err(error),
            }
        }
        required_bytes = required_bytes.saturating_add(asset_allocated_bytes(content.len()));
    }

    if !missing.iter().any(|value| *value) {
        return Ok(());
    }
    if let Some(usage) = vfs.storage_usage("/")? {
        let available = usage.quota_bytes.saturating_sub(usage.used_bytes);
        if available < required_bytes {
            return Ok(());
        }
    }

    if directory_missing {
        vfs.mkdir(ZONEINFO_DIR, 0o755)?;
    }
    for (index, (path, temporary, content)) in assets.iter().enumerate() {
        if missing[index] {
            stage_file(vfs, path, temporary, content)?;
        }
    }
    Ok(())
}

fn seed_wallpaper_defaults(vfs: &mut Vfs) -> Result<(), FsError> {
    let assets = [
        (
            "/usr/share/wallpapers/blue.png",
            "/usr/share/wallpapers/.pmos-seed-blue.png",
            mkfs::default_blue_wallpaper(),
        ),
        (
            "/usr/share/wallpapers/green.png",
            "/usr/share/wallpapers/.pmos-seed-green.png",
            mkfs::default_green_wallpaper(),
        ),
        (
            "/usr/share/wallpapers/dark.png",
            "/usr/share/wallpapers/.pmos-seed-dark.png",
            mkfs::default_dark_wallpaper(),
        ),
    ];

    let directory_missing = match vfs.stat(WALLPAPER_DIR) {
        Ok(stat) if stat.ty == NodeType::Directory => false,
        Ok(_) => return Ok(()),
        Err(FsError::NotFound) => true,
        Err(error) => return Err(error),
    };

    let mut missing = [false; 3];
    let mut required_bytes = BLOCK_SIZE as u64;
    for (index, (path, _, content)) in assets.iter().enumerate() {
        if directory_missing {
            missing[index] = true;
        } else {
            match vfs.stat(path) {
                Ok(_) => continue,
                Err(FsError::NotFound) => missing[index] = true,
                Err(error) => return Err(error),
            }
        }
        required_bytes = required_bytes.saturating_add(asset_allocated_bytes(content.len()));
    }

    if !missing.iter().any(|value| *value) {
        return Ok(());
    }
    if let Some(usage) = vfs.storage_usage("/")? {
        let available = usage.quota_bytes.saturating_sub(usage.used_bytes);
        if available < required_bytes {
            return Ok(());
        }
    }

    if directory_missing {
        vfs.mkdir(WALLPAPER_DIR, 0o755)?;
    }
    for (index, (path, temporary, content)) in assets.iter().enumerate() {
        if missing[index] {
            stage_file(vfs, path, temporary, content)?;
        }
    }
    Ok(())
}

fn asset_allocated_bytes(content_len: usize) -> u64 {
    let blocks = content_len.div_ceil(BLOCK_SIZE) as u64;
    let indirect = u64::from(blocks > INODE_DIRECT_BLOCKS as u64);
    (blocks + indirect) * BLOCK_SIZE as u64
}

fn stage_file(vfs: &mut Vfs, path: &str, temporary: &str, content: &[u8]) -> Result<(), FsError> {
    match vfs.stat(temporary) {
        Ok(stat) if stat.ty == NodeType::Directory => return Err(FsError::IsADirectory),
        Ok(_) => vfs.unlink(temporary)?,
        Err(FsError::NotFound) => {}
        Err(error) => return Err(error),
    }

    vfs.create(temporary, 0o644)?;
    let result = (|| {
        for (index, chunk) in content.chunks(ASSET_WRITE_CHUNK_BYTES).enumerate() {
            let offset = (index * ASSET_WRITE_CHUNK_BYTES) as u64;
            if vfs.write(temporary, offset, chunk)? != chunk.len() {
                return Err(FsError::Io);
            }
        }
        vfs.rename(temporary, path)
    })();
    if result.is_err() {
        let _ = vfs.unlink(temporary);
    }
    result
}
