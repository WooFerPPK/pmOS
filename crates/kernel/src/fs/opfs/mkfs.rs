//! First-boot OPFS initialiser.
//!
//! `mkfs` takes a freshly-allocated block device and writes a
//! valid OPFS image to it: superblock, zeroed inode table,
//! empty journal, root directory, the system directory tree,
//! and the **FR-013a starter kit** — every file and directory
//! a first-run visitor sees in `/home/user` the moment they
//! open the URL.
//!
//! The starter kit per `spec.md` FR-013a is:
//!
//! ```text
//! /home/user/README.md
//! /home/user/Downloads/
//! /home/user/Documents/welcome.txt
//! /home/user/Documents/editing.md
//! /home/user/Pictures/
//! ```
//!
//! Plus the system directories `/bin`, `/etc`, `/dev`, `/proc`,
//! `/run`, `/tmp`, `/home`, `/opt`, `/usr`, `/usr/bin`,
//! `/usr/share`, `/usr/share/applications`. These are created
//! at mkfs time because the VFS mounts tmpfs / devfs / procfs
//! over `/dev`, `/proc`, `/run`, `/tmp` at boot — having the
//! persistent mount points present means recovery-mode boots
//! (without those mounts) still find a coherent tree.

use crate::vfs::{Filesystem, FsError};

use super::block::DynBlockDevice;
use super::journal::Journal;
use super::layout::{
    InodeKind, InodeOnDisk, Superblock, BLOCK_SIZE, DEFAULT_INODE_TABLE_BLOCKS,
    DEFAULT_JOURNAL_BLOCKS, FS_MAGIC, FS_VERSION_MAJOR, FS_VERSION_MINOR, INODES_PER_BLOCK,
    INODE_DIRECT_BLOCKS, LBA_SUPERBLOCK, MIN_BLOCK_COUNT, ROOT_INO,
};
use super::OpfsFs;

/// Format an empty block device as a fresh OPFS image and
/// populate it with the FR-013a starter kit.
///
/// The device must have at least [`MIN_BLOCK_COUNT`] blocks.
/// The resulting `OpfsFs` is mounted and ready for use.
pub fn mkfs(mut device: DynBlockDevice) -> Result<OpfsFs, FsError> {
    let total_blocks = device.block_count();
    if total_blocks < MIN_BLOCK_COUNT {
        return Err(FsError::NoSpace);
    }

    // Partition layout.
    let journal_start = 1;
    let journal_blocks = DEFAULT_JOURNAL_BLOCKS;
    let inode_table_start = journal_start + journal_blocks;
    let inode_table_blocks = DEFAULT_INODE_TABLE_BLOCKS;
    let inode_count = inode_table_blocks * INODES_PER_BLOCK;
    let data_start = inode_table_start + inode_table_blocks;
    let data_block_count = total_blocks - data_start;

    // Step 1: zero the inode table. InodeOnDisk::from_bytes on
    // a zeroed slot returns kind=Unused, which is what we want.
    let zero_block = [0u8; BLOCK_SIZE];
    for i in 0..inode_table_blocks {
        device.write(inode_table_start + i, &zero_block)?;
    }

    // Step 2: zero the journal so a prior life's "JTXN" magic
    // isn't mistaken for a live commit.
    for i in 0..journal_blocks {
        device.write(journal_start + i, &zero_block)?;
    }

    // Step 3: construct the initial superblock.
    let sb = Superblock {
        magic: FS_MAGIC,
        version_major: FS_VERSION_MAJOR,
        version_minor: FS_VERSION_MINOR,
        block_size: BLOCK_SIZE as u32,
        total_blocks,
        journal_start,
        journal_blocks,
        inode_table_start,
        inode_table_blocks,
        inode_count,
        inode_free: inode_count,
        next_free_inode: ROOT_INO,
        data_start,
        data_block_count,
        data_block_free: data_block_count,
        next_free_data_block: data_start,
        root_ino: ROOT_INO,
        journal_head: 0,
        journal_tail: 0,
        mount_generation: 1,
    };

    // Step 4: write the initial superblock so the image is
    // recognisable even if mkfs aborts mid-populate.
    let sb_block = sb.to_bytes();
    device.write(LBA_SUPERBLOCK, &sb_block)?;
    device.flush()?;

    // Step 5: construct an in-memory OpfsFs around the freshly-
    // written device, bypassing the normal mount path (which
    // would pointlessly replay an empty journal).
    let journal = Journal::new(journal_start, journal_blocks);
    let mut fs = OpfsFs::from_parts(device, sb, journal);

    // Step 6: allocate root inode (ino 1) directly — no journal.
    //
    // Reason: everything in the normal Filesystem trait path
    // (mkdir, create, write) depends on an existing root inode
    // to hang entries off. That's a chicken-and-egg unless the
    // root is placed directly. Every subsequent write goes
    // through the normal journal-backed path.
    let alloc_ino = fs.alloc_inode()?;
    debug_assert_eq!(alloc_ino, ROOT_INO);
    let root = InodeOnDisk {
        ino: ROOT_INO,
        kind: InodeKind::Directory,
        mode: 0o755,
        nlink: 2,
        size: 0,
        atime_ns: 0,
        mtime_ns: 0,
        ctime_ns: 0,
        direct: [0; INODE_DIRECT_BLOCKS],
        indirect: 0,
    };
    fs.write_inode_direct(&root)?;
    fs.write_superblock()?;
    fs.device_mut().flush()?;

    // Sanity check: read the root inode back and make sure the
    // on-disk format round-trips via the current journaling path.
    // If this ever fails, it's a mkfs or inode-serialisation bug.
    let verify = fs.read_inode(ROOT_INO)?;
    debug_assert_eq!(
        verify.kind,
        InodeKind::Directory,
        "mkfs: root inode round-trip failed; got kind={:?}",
        verify.kind,
    );

    // Step 7: from here on, everything goes through the normal
    // journal-backed Filesystem trait methods. Each call is
    // atomic with respect to a crash.
    create_system_tree(&mut fs)?;
    create_starter_kit(&mut fs)?;

    // Step 8: final flush so everything is durable before we
    // return. `sync()` applies the journal and flushes.
    fs.sync()?;
    Ok(fs)
}

/// Create every top-level system directory in a deterministic
/// order.
fn create_system_tree(fs: &mut OpfsFs) -> Result<(), FsError> {
    // First create the direct children of /.
    for name in ["bin", "dev", "etc", "home", "opt", "proc", "run", "tmp", "usr"] {
        let mode = if name == "proc" { 0o555 } else { 0o755 };
        fs.mkdir(ROOT_INO, name, mode)?;
    }

    // Nested: /usr/bin, /usr/share, /usr/share/applications.
    let usr_ino = fs.lookup(ROOT_INO, "usr")?;
    fs.mkdir(usr_ino, "bin", 0o755)?;
    let share_ino = fs.mkdir(usr_ino, "share", 0o755)?;
    fs.mkdir(share_ino, "applications", 0o755)?;

    // Nested: /home/user.
    let home_ino = fs.lookup(ROOT_INO, "home")?;
    fs.mkdir(home_ino, "user", 0o755)?;
    Ok(())
}

/// Populate `/home/user` with the FR-013a starter kit.
fn create_starter_kit(fs: &mut OpfsFs) -> Result<(), FsError> {
    let home_ino = fs.lookup(ROOT_INO, "home")?;
    let user_ino = fs.lookup(home_ino, "user")?;

    // README.md — a short pointer to the rest of the system.
    let readme_ino = fs.create(user_ino, "README.md", 0o644)?;
    fs.write(readme_ino, 0, README_CONTENT)?;

    // Empty Downloads/.
    fs.mkdir(user_ino, "Downloads", 0o755)?;

    // Documents/ with two sample files.
    let docs_ino = fs.mkdir(user_ino, "Documents", 0o755)?;
    let welcome_ino = fs.create(docs_ino, "welcome.txt", 0o644)?;
    fs.write(welcome_ino, 0, WELCOME_TXT)?;
    let editing_ino = fs.create(docs_ino, "editing.md", 0o644)?;
    fs.write(editing_ino, 0, EDITING_MD)?;

    // Empty Pictures/.
    fs.mkdir(user_ino, "Pictures", 0o755)?;

    Ok(())
}

// --- Starter-kit file contents ----------------------------------------

const README_CONTENT: &[u8] = b"# Welcome to PMos

This is your private filesystem. Everything you create here
persists across tab closes and browser restarts -- it lives in
browser-local storage (OPFS) that nobody else can see, including
the people who operate this site.

## What you have

* `Documents/` -- a place for plain text and markdown. Open the
  text editor from the launcher and start typing.
* `Downloads/` -- drop files into the file manager from your
  host OS and they land here.
* `Pictures/` -- reserved for images you drop in.

## Getting around

Launch apps from the taskbar. Open a terminal and run `ls`.
Open the file manager to browse this directory visually. Open
the system monitor to see what's running.

Feel free to delete this README.
";

const WELCOME_TXT: &[u8] = b"welcome to pmos.

this is a plain text file. the bundled text editor opens any
file whose name ends in .txt or .md. save to keep your edits;
they survive a tab reload because the filesystem is persistent.

try:
  - open this file, make an edit, save, reload the tab.
  - create a new file in the file manager.
  - open a terminal and run `ls /home/user/Documents/`.
";

const EDITING_MD: &[u8] = b"# Editing on PMos

The bundled text editor (`/usr/bin/edit`) supports:

* Plain text files
* Simple markdown (this file!)
* Unsaved-changes prompt on close
* Save and Save As

## Keyboard shortcuts

| key       | action                      |
|-----------|-----------------------------|
| Ctrl+S    | save                        |
| Ctrl+O    | open                        |
| Ctrl+W    | close the current buffer    |
| Ctrl+Q    | quit the editor             |

## Where your files live

Everything under `/home/user/` persists to your browser's
private OPFS storage. Everything under `/tmp/` is wiped on
reboot. Nothing leaves your machine.
";

// --- Starter-kit content accessors (used by T061 tests) --------------

pub fn starter_readme() -> &'static [u8] {
    README_CONTENT
}

pub fn starter_welcome() -> &'static [u8] {
    WELCOME_TXT
}

pub fn starter_editing() -> &'static [u8] {
    EDITING_MD
}
