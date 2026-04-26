//! OPFS-backed persistent root filesystem.
//!
//! `OpfsFs` implements the [`Filesystem`](crate::vfs::Filesystem)
//! trait over a [`BlockDevice`]. Layout lives in [`layout`];
//! block I/O in [`block`]; crash-safe metadata updates in
//! [`journal`]; first-boot initialisation in [`mkfs`].
//!
//! This is the filesystem mounted at `/` in a real PMos boot.
//! In tests it runs over a [`block::MockBlockDevice`]; in
//! production it runs over a `DriverBlockDevice` that proxies
//! to the TypeScript block driver via `Platform::driver_call`.
//!
//! ## Scope of the v1 slice
//!
//! This slice implements **every method of the `Filesystem`
//! trait** over real on-disk structures, including journaled
//! metadata updates. The feature set is:
//!
//! * `create`, `mkdir`, `unlink`, `rmdir`, `rename` (intra-fs)
//! * `lookup`, `readdir`, `stat`
//! * `read`, `write`, `truncate`
//! * `sync` (forces journal apply + block device flush)
//! * `kind_name` returns `"opfs"`
//!
//! Simplifications from data-model.md §3.3:
//!
//! * **Direct blocks only.** The inode holds 12 direct pointers
//!   and an indirect-block pointer, but in v1 we only use the
//!   direct table. Max file size: 12 * 4 KiB = 48 KiB. The
//!   bundled apps all write well under that; larger files are
//!   a follow-up when OPFS-hosted documents start growing.
//! * **Next-free cursor allocator.** Inodes and data blocks
//!   are allocated sequentially from a cursor in the superblock.
//!   Freed inodes and data blocks are NOT reused in v1. A
//!   proper bitmap or free-list allocator is a polish pass.
//! * **In-memory inode cache: no cache.** Every stat/read/write
//!   re-reads the inode from the inode table. Inode-table reads
//!   hit the block device's mock or real cache, so this is fast
//!   enough for v1.

use alloc::vec::Vec;

use crate::platform;
use crate::vfs::{
    DirEntry, FileStat, Filesystem, FsError, Ino, Mode, NanosSinceEpoch, StorageUsage,
};

pub mod block;
pub mod journal;
pub mod layout;
pub mod mkfs;

use block::{BlockDevice, DynBlockDevice};
use journal::{Journal, Transaction};
use layout::{
    DirEntryOnDisk, InodeKind, InodeOnDisk, Lba, Superblock, BLOCK_SIZE, INODES_PER_BLOCK,
    INODE_BYTES, INODE_DIRECT_BLOCKS, ROOT_INO,
};

/// The OPFS filesystem.
pub struct OpfsFs {
    device: DynBlockDevice,
    sb: Superblock,
    journal: Journal,
}

impl OpfsFs {
    /// Mount an existing OPFS image. Reads and validates the
    /// superblock, restores journal cursors, and replays any
    /// committed-but-not-applied transactions.
    pub fn mount(mut device: DynBlockDevice) -> Result<Self, FsError> {
        let mut header_buf = [0u8; BLOCK_SIZE];
        device.read(layout::LBA_SUPERBLOCK, &mut header_buf)?;
        let sb = Superblock::from_bytes(&header_buf)?;

        let mut journal = Journal::new(sb.journal_start, sb.journal_blocks);
        journal.load_cursors(sb.journal_head, sb.journal_tail, 1);

        let mut fs = OpfsFs {
            device,
            sb,
            journal,
        };

        // Replay any committed-but-not-applied transactions.
        let applied = fs.journal.apply_committed(fs.device.as_mut())?;
        if applied > 0 {
            // Persist the advanced tail.
            fs.sb.journal_tail = fs.journal.tail;
            fs.sb.mount_generation = fs.sb.mount_generation.saturating_add(1);
            fs.write_superblock()?;
            fs.device.flush()?;
        } else {
            // No replay work, but still bump the mount generation
            // so it's observable at runtime.
            fs.sb.mount_generation = fs.sb.mount_generation.saturating_add(1);
            fs.write_superblock()?;
            fs.device.flush()?;
        }

        Ok(fs)
    }

    /// Direct-construct from an already-initialised superblock.
    /// Used by `mkfs` after it has built a fresh image in-place.
    pub(crate) fn from_parts(device: DynBlockDevice, sb: Superblock, journal: Journal) -> Self {
        OpfsFs {
            device,
            sb,
            journal,
        }
    }

    /// Borrow the superblock for tests and diagnostics.
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// Return the underlying block device. Used by tests to
    /// inspect the raw storage.
    pub fn device(&self) -> &dyn BlockDevice {
        self.device.as_ref()
    }

    /// Mutable borrow of the underlying block device. Used by
    /// mkfs and by `sync`.
    pub(crate) fn device_mut(&mut self) -> &mut dyn BlockDevice {
        self.device.as_mut()
    }

    /// Consume self and return the block device. Used by tests
    /// that want to remount after unmounting.
    pub fn into_device(self) -> DynBlockDevice {
        self.device
    }

    /// Directly write an inode to its slot in the inode table,
    /// bypassing the journal. Only `mkfs` uses this — during
    /// initial image population there is no "previous state"
    /// to protect against, so the journal adds no value and
    /// creates a chicken-and-egg (the root inode must exist
    /// before the journal can replay onto anything).
    pub(crate) fn write_inode_direct(&mut self, ino: &InodeOnDisk) -> Result<(), FsError> {
        let (lba, block) = self.make_inode_block_update(ino)?;
        self.device.write(lba, &block)?;
        Ok(())
    }

    // --- Block / inode arithmetic --------------------------------------

    fn inode_lba(&self, ino: u64) -> Result<(Lba, usize), FsError> {
        if ino == 0 || ino > self.sb.inode_count {
            return Err(FsError::NotFound);
        }
        let ino_index = ino - 1;
        let block_offset = ino_index / INODES_PER_BLOCK;
        let within = (ino_index % INODES_PER_BLOCK) as usize;
        let lba = self.sb.inode_table_start + block_offset;
        Ok((lba, within))
    }

    pub(crate) fn read_inode(&mut self, ino: u64) -> Result<InodeOnDisk, FsError> {
        let (lba, within) = self.inode_lba(ino)?;
        let mut block = [0u8; BLOCK_SIZE];
        self.device.read(lba, &mut block)?;
        let slot_start = within * INODE_BYTES;
        let mut slot = [0u8; INODE_BYTES];
        slot.copy_from_slice(&block[slot_start..slot_start + INODE_BYTES]);
        InodeOnDisk::from_bytes(&slot)
    }

    /// Produce a block image in which the inode's slot has been
    /// replaced with `ino`'s new bytes. Leaves the rest of the
    /// block's inodes untouched. Used by `mkfs`'s direct write
    /// path (no journal); the journaled path uses
    /// [`stage_inode_write`] which routes through
    /// [`Transaction::add_or_merge_write`] so multiple inode
    /// slots in the same block merge correctly.
    fn make_inode_block_update(
        &mut self,
        ino: &InodeOnDisk,
    ) -> Result<(Lba, [u8; BLOCK_SIZE]), FsError> {
        let (lba, within) = self.inode_lba(ino.ino)?;
        let mut block = [0u8; BLOCK_SIZE];
        self.device.read(lba, &mut block)?;
        let slot_start = within * INODE_BYTES;
        let mut slot = [0u8; INODE_BYTES];
        ino.to_bytes(&mut slot);
        block[slot_start..slot_start + INODE_BYTES].copy_from_slice(&slot);
        Ok((lba, block))
    }

    /// Allocate a fresh inode number from the next-free cursor.
    /// Writes the allocated inode's slot to disk as
    /// [`InodeKind::Unused`] so the caller can then fill it in.
    /// Caller is responsible for `write_inode` + journal commit.
    pub(crate) fn alloc_inode(&mut self) -> Result<u64, FsError> {
        if self.sb.inode_free == 0 {
            return Err(FsError::NoSpace);
        }
        let ino = self.sb.next_free_inode;
        if ino > self.sb.inode_count {
            return Err(FsError::NoSpace);
        }
        self.sb.next_free_inode = self.sb.next_free_inode.saturating_add(1);
        self.sb.inode_free = self.sb.inode_free.saturating_sub(1);
        Ok(ino)
    }

    /// Allocate a fresh data block (LBA) from the next-free
    /// cursor. Returns the LBA; the caller writes content to
    /// it and includes the write in its journal txn.
    pub(crate) fn alloc_data_block(&mut self) -> Result<Lba, FsError> {
        if self.sb.data_block_free == 0 {
            return Err(FsError::NoSpace);
        }
        let lba = self.sb.next_free_data_block;
        if lba >= self.sb.data_start + self.sb.data_block_count {
            return Err(FsError::NoSpace);
        }
        self.sb.next_free_data_block = self.sb.next_free_data_block.saturating_add(1);
        self.sb.data_block_free = self.sb.data_block_free.saturating_sub(1);
        Ok(lba)
    }

    /// Direct-write the superblock (used by mkfs). Not
    /// journaled — the superblock is the journal cursor holder
    /// and has its own CRC for torn-write detection.
    pub(crate) fn write_superblock(&mut self) -> Result<(), FsError> {
        self.sb.journal_head = self.journal.head;
        self.sb.journal_tail = self.journal.tail;
        let block = self.sb.to_bytes();
        self.device.write(layout::LBA_SUPERBLOCK, &block)?;
        Ok(())
    }

    /// Stage an inode write into a transaction.
    ///
    /// Uses [`Transaction::add_or_merge_write`] so that multiple
    /// inodes in the same inode-table block (e.g., a newly-
    /// created inode at slot N and an updated parent-directory
    /// inode at slot 0) merge into a single op on commit. This
    /// prevents a classic read-modify-write collision where the
    /// second stage_inode_write would otherwise read the pre-txn
    /// block from disk and overwrite the first stage's slot.
    fn stage_inode_write(
        &mut self,
        ino: &InodeOnDisk,
        txn: &mut Transaction,
    ) -> Result<(), FsError> {
        let (lba, within) = self.inode_lba(ino.ino)?;
        // If the txn has already staged a write to this block,
        // use that pending image; otherwise read from disk.
        let initial = if let Some(pending) = txn.pending_block(lba) {
            *pending
        } else {
            let mut b = [0u8; BLOCK_SIZE];
            self.device.read(lba, &mut b)?;
            b
        };
        let slot_start = within * INODE_BYTES;
        let mut slot = [0u8; INODE_BYTES];
        ino.to_bytes(&mut slot);
        txn.add_or_merge_write(lba, initial, |block| {
            block[slot_start..slot_start + INODE_BYTES].copy_from_slice(&slot);
        })
    }

    /// Commit + apply a transaction in one shot. Most callers
    /// use this; mkfs builds its own large txn by hand.
    fn commit_and_apply(&mut self, txn: &Transaction) -> Result<(), FsError> {
        if txn.is_empty() {
            return Ok(());
        }
        self.journal.commit(txn, self.device.as_mut())?;
        // Persist the journal cursor update before apply so a
        // crash between commit and apply is still recoverable
        // by replay on mount.
        self.write_superblock()?;
        self.device.flush()?;
        self.journal.apply_committed(self.device.as_mut())?;
        self.write_superblock()?;
        self.device.flush()?;
        Ok(())
    }

    // --- Directory content reading / writing --------------------------

    /// Load the entire directory content as a flat byte vector.
    /// Used by `lookup`, `readdir`, `create`, and friends.
    fn load_dir_bytes(&mut self, dir: &InodeOnDisk) -> Result<Vec<u8>, FsError> {
        if dir.kind != InodeKind::Directory {
            return Err(FsError::NotADirectory);
        }
        let mut out = Vec::with_capacity(dir.size as usize);
        let size = dir.size as usize;
        let mut remaining = size;
        let mut block_ix = 0usize;
        while remaining > 0 {
            if block_ix >= INODE_DIRECT_BLOCKS {
                return Err(FsError::Io); // indirect not supported in v1
            }
            let lba = dir.direct[block_ix];
            if lba == 0 {
                return Err(FsError::Io);
            }
            let mut block = [0u8; BLOCK_SIZE];
            self.device.read(lba, &mut block)?;
            let take = core::cmp::min(BLOCK_SIZE, remaining);
            out.extend_from_slice(&block[..take]);
            remaining -= take;
            block_ix += 1;
        }
        Ok(out)
    }

    /// Stage a write of `content` into `dir`'s data blocks,
    /// allocating new data blocks as needed. Updates
    /// `dir.direct`, `dir.size`, and any modified data blocks
    /// into `txn`.
    fn stage_write_dir_bytes(
        &mut self,
        dir: &mut InodeOnDisk,
        content: &[u8],
        txn: &mut Transaction,
    ) -> Result<(), FsError> {
        let blocks_needed = content.len().div_ceil(BLOCK_SIZE);
        if blocks_needed > INODE_DIRECT_BLOCKS {
            return Err(FsError::NoSpace);
        }

        // Make sure we have enough direct-block slots allocated.
        for i in 0..blocks_needed {
            if dir.direct[i] == 0 {
                dir.direct[i] = self.alloc_data_block()?;
            }
        }

        // Write each block (full or partial for the final one).
        for i in 0..blocks_needed {
            let mut block = [0u8; BLOCK_SIZE];
            let start = i * BLOCK_SIZE;
            let end = core::cmp::min(start + BLOCK_SIZE, content.len());
            block[..end - start].copy_from_slice(&content[start..end]);
            txn.add_write(dir.direct[i], block)?;
        }

        // Free any previously-allocated blocks past the new size.
        // In v1 we leak them (next-free-cursor allocator never
        // reuses); we just zero the direct slot so readdir doesn't
        // walk past content.len().
        for i in blocks_needed..INODE_DIRECT_BLOCKS {
            dir.direct[i] = 0;
        }

        dir.size = content.len() as u64;
        Ok(())
    }

    /// Decode a directory's bytes into a list of entries.
    fn decode_dir_entries(&mut self, dir: &InodeOnDisk) -> Result<Vec<DirEntryOnDisk>, FsError> {
        let bytes = self.load_dir_bytes(dir)?;
        let mut out = Vec::new();
        let mut off = 0;
        while off < bytes.len() {
            let (entry, consumed) = DirEntryOnDisk::decode(&bytes[off..])?;
            off += consumed;
            if entry.ino != 0 {
                out.push(entry);
            }
        }
        Ok(out)
    }

    /// Encode a list of entries into a flat byte vector.
    fn encode_dir_entries(entries: &[DirEntryOnDisk]) -> Vec<u8> {
        let mut out = Vec::new();
        for e in entries {
            out.extend(e.encode());
        }
        out
    }

    /// Check that `name` is a valid directory entry name.
    fn check_name(name: &str) -> Result<(), FsError> {
        if name.is_empty() || name == "." || name == ".." {
            return Err(FsError::InvalidArgument);
        }
        if name.contains('/') || name.contains('\0') {
            return Err(FsError::InvalidArgument);
        }
        if name.len() > 255 {
            return Err(FsError::InvalidArgument);
        }
        Ok(())
    }

    // --- File content reading / writing -------------------------------

    fn file_read(
        &mut self,
        file: &InodeOnDisk,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        if file.kind != InodeKind::RegularFile {
            return Err(FsError::IsADirectory);
        }
        let start = offset;
        if start >= file.size {
            return Ok(0);
        }
        let end = core::cmp::min(file.size, start + buf.len() as u64);
        let mut written = 0usize;
        let mut cursor = start;
        while cursor < end {
            let block_ix = (cursor / BLOCK_SIZE as u64) as usize;
            if block_ix >= INODE_DIRECT_BLOCKS {
                return Err(FsError::Io);
            }
            let lba = file.direct[block_ix];
            if lba == 0 {
                return Err(FsError::Io);
            }
            let offset_in_block = (cursor % BLOCK_SIZE as u64) as usize;
            let chunk =
                core::cmp::min((BLOCK_SIZE - offset_in_block) as u64, end - cursor) as usize;
            let mut block = [0u8; BLOCK_SIZE];
            self.device.read(lba, &mut block)?;
            buf[written..written + chunk]
                .copy_from_slice(&block[offset_in_block..offset_in_block + chunk]);
            written += chunk;
            cursor += chunk as u64;
        }
        Ok(written)
    }

    fn stage_file_write(
        &mut self,
        file: &mut InodeOnDisk,
        offset: u64,
        data: &[u8],
        txn: &mut Transaction,
    ) -> Result<usize, FsError> {
        if file.kind != InodeKind::RegularFile {
            return Err(FsError::IsADirectory);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let end = offset + data.len() as u64;
        let blocks_needed = end.div_ceil(BLOCK_SIZE as u64) as usize;
        if blocks_needed > INODE_DIRECT_BLOCKS {
            return Err(FsError::NoSpace);
        }
        // Ensure direct blocks are allocated for the whole range.
        for i in 0..blocks_needed {
            if file.direct[i] == 0 {
                file.direct[i] = self.alloc_data_block()?;
            }
        }
        // Write affected blocks. For each block we read the old
        // content, patch the relevant range, and write it back.
        let mut written = 0usize;
        let mut cursor = offset;
        while cursor < end {
            let block_ix = (cursor / BLOCK_SIZE as u64) as usize;
            let lba = file.direct[block_ix];
            let offset_in_block = (cursor % BLOCK_SIZE as u64) as usize;
            let chunk =
                core::cmp::min((BLOCK_SIZE - offset_in_block) as u64, end - cursor) as usize;
            let mut block = [0u8; BLOCK_SIZE];
            // Read-modify-write so we don't clobber bytes outside
            // the write range (matters for sub-block writes past
            // the beginning of a file).
            self.device.read(lba, &mut block)?;
            block[offset_in_block..offset_in_block + chunk]
                .copy_from_slice(&data[written..written + chunk]);
            txn.add_write(lba, block)?;
            written += chunk;
            cursor += chunk as u64;
        }
        if end > file.size {
            file.size = end;
        }
        Ok(written)
    }

    fn stage_file_truncate(
        &mut self,
        file: &mut InodeOnDisk,
        new_size: u64,
        txn: &mut Transaction,
    ) -> Result<(), FsError> {
        if file.kind != InodeKind::RegularFile {
            return Err(FsError::IsADirectory);
        }
        let blocks_needed = new_size.div_ceil(BLOCK_SIZE as u64) as usize;
        if blocks_needed > INODE_DIRECT_BLOCKS {
            return Err(FsError::NoSpace);
        }

        if new_size < file.size {
            // Shrinking. If `new_size` falls inside a retained
            // block, zero the tail of that block from
            // `new_size mod BLOCK_SIZE` to the end so a
            // subsequent extend reads zeros rather than stale
            // post-truncate data. POSIX: shrinking discards
            // the truncated content.
            if new_size > 0 && blocks_needed > 0 {
                let last_ix = blocks_needed - 1;
                let offset_in_last = (new_size % BLOCK_SIZE as u64) as usize;
                if offset_in_last != 0 && file.direct[last_ix] != 0 {
                    let lba = file.direct[last_ix];
                    let initial = if let Some(pending) = txn.pending_block(lba) {
                        *pending
                    } else {
                        let mut b = [0u8; BLOCK_SIZE];
                        self.device.read(lba, &mut b)?;
                        b
                    };
                    txn.add_or_merge_write(lba, initial, |block| {
                        for byte in &mut block[offset_in_last..] {
                            *byte = 0;
                        }
                    })?;
                }
            }
            // Release direct slots past the new end (leaked in
            // v1 — see module docs on the next-free-cursor
            // allocator).
            for i in blocks_needed..INODE_DIRECT_BLOCKS {
                file.direct[i] = 0;
            }
        } else if new_size > file.size {
            // Extending. Allocate new blocks; they come from
            // the MockBlockDevice's sparse-read-returns-zeros
            // path already zeroed, but we explicitly stage a
            // zero write so the real block driver doesn't get
            // surprised.
            let zero_block = [0u8; BLOCK_SIZE];
            for i in 0..blocks_needed {
                if file.direct[i] == 0 {
                    file.direct[i] = self.alloc_data_block()?;
                    txn.add_write(file.direct[i], zero_block)?;
                }
            }
        }

        file.size = new_size;
        Ok(())
    }
}

// --- Filesystem trait impl --------------------------------------------

impl Filesystem for OpfsFs {
    fn root(&self) -> Ino {
        ROOT_INO
    }

    fn lookup(&mut self, dir: Ino, name: &str) -> Result<Ino, FsError> {
        let dir_ino = self.read_inode(dir)?;
        if dir_ino.kind != InodeKind::Directory {
            return Err(FsError::NotADirectory);
        }
        let entries = self.decode_dir_entries(&dir_ino)?;
        entries
            .into_iter()
            .find(|e| e.name == name)
            .map(|e| e.ino)
            .ok_or(FsError::NotFound)
    }

    fn read(&mut self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        let file_ino = self.read_inode(ino)?;
        self.file_read(&file_ino, offset, buf)
    }

    fn write(&mut self, ino: Ino, offset: u64, buf: &[u8]) -> Result<usize, FsError> {
        let now = now_ns();
        let mut file_ino = self.read_inode(ino)?;
        let mut txn = Transaction::new();
        let n = self.stage_file_write(&mut file_ino, offset, buf, &mut txn)?;
        file_ino.mtime_ns = now;
        file_ino.ctime_ns = now;
        self.stage_inode_write(&file_ino, &mut txn)?;
        self.commit_and_apply(&txn)?;
        Ok(n)
    }

    fn readdir(&mut self, dir: Ino, out: &mut Vec<DirEntry>) -> Result<(), FsError> {
        let dir_ino = self.read_inode(dir)?;
        if dir_ino.kind != InodeKind::Directory {
            return Err(FsError::NotADirectory);
        }
        let entries = self.decode_dir_entries(&dir_ino)?;
        for e in entries {
            // Look up each child to find its type.
            let child = self.read_inode(e.ino)?;
            let ty = child.kind.to_node_type().ok_or(FsError::Io)?;
            out.push(DirEntry {
                name: e.name,
                ino: e.ino,
                ty,
            });
        }
        Ok(())
    }

    fn create(&mut self, dir: Ino, name: &str, mode: Mode) -> Result<Ino, FsError> {
        Self::check_name(name)?;
        let mut dir_ino = self.read_inode(dir)?;
        if dir_ino.kind != InodeKind::Directory {
            return Err(FsError::NotADirectory);
        }
        let mut entries = self.decode_dir_entries(&dir_ino)?;
        if entries.iter().any(|e| e.name == name) {
            return Err(FsError::AlreadyExists);
        }
        let new_ino = self.alloc_inode()?;
        let now = now_ns();
        let new_inode = InodeOnDisk {
            ino: new_ino,
            kind: InodeKind::RegularFile,
            mode,
            nlink: 1,
            size: 0,
            atime_ns: now,
            mtime_ns: now,
            ctime_ns: now,
            direct: [0; INODE_DIRECT_BLOCKS],
            indirect: 0,
        };
        entries.push(DirEntryOnDisk {
            ino: new_ino,
            name: name.into(),
        });
        let new_bytes = Self::encode_dir_entries(&entries);

        let mut txn = Transaction::new();
        self.stage_inode_write(&new_inode, &mut txn)?;
        self.stage_write_dir_bytes(&mut dir_ino, &new_bytes, &mut txn)?;
        self.stage_inode_write(&dir_ino, &mut txn)?;
        self.commit_and_apply(&txn)?;
        Ok(new_ino)
    }

    fn mkdir(&mut self, dir: Ino, name: &str, mode: Mode) -> Result<Ino, FsError> {
        Self::check_name(name)?;
        let mut dir_ino = self.read_inode(dir)?;
        if dir_ino.kind != InodeKind::Directory {
            return Err(FsError::NotADirectory);
        }
        let mut entries = self.decode_dir_entries(&dir_ino)?;
        if entries.iter().any(|e| e.name == name) {
            return Err(FsError::AlreadyExists);
        }
        let new_ino = self.alloc_inode()?;
        let now = now_ns();
        let new_inode = InodeOnDisk {
            ino: new_ino,
            kind: InodeKind::Directory,
            mode,
            nlink: 2, // self link + "." — we don't store "." / ".." explicitly in v1
            size: 0,
            atime_ns: now,
            mtime_ns: now,
            ctime_ns: now,
            direct: [0; INODE_DIRECT_BLOCKS],
            indirect: 0,
        };
        entries.push(DirEntryOnDisk {
            ino: new_ino,
            name: name.into(),
        });
        let new_bytes = Self::encode_dir_entries(&entries);

        let mut txn = Transaction::new();
        self.stage_inode_write(&new_inode, &mut txn)?;
        self.stage_write_dir_bytes(&mut dir_ino, &new_bytes, &mut txn)?;
        self.stage_inode_write(&dir_ino, &mut txn)?;
        self.commit_and_apply(&txn)?;
        Ok(new_ino)
    }

    fn unlink(&mut self, dir: Ino, name: &str) -> Result<(), FsError> {
        let mut dir_ino = self.read_inode(dir)?;
        if dir_ino.kind != InodeKind::Directory {
            return Err(FsError::NotADirectory);
        }
        let mut entries = self.decode_dir_entries(&dir_ino)?;
        let pos = entries
            .iter()
            .position(|e| e.name == name)
            .ok_or(FsError::NotFound)?;
        let target_ino = entries[pos].ino;
        let target = self.read_inode(target_ino)?;
        if target.kind == InodeKind::Directory {
            return Err(FsError::IsADirectory);
        }

        entries.remove(pos);
        let new_bytes = Self::encode_dir_entries(&entries);

        let mut txn = Transaction::new();
        // Zero the target inode slot (marks it unused). Its data
        // blocks are leaked in v1 (next-free-cursor allocator).
        let dead = InodeOnDisk::unused(target_ino);
        self.stage_inode_write(&dead, &mut txn)?;
        self.stage_write_dir_bytes(&mut dir_ino, &new_bytes, &mut txn)?;
        self.stage_inode_write(&dir_ino, &mut txn)?;
        self.commit_and_apply(&txn)?;
        Ok(())
    }

    fn rmdir(&mut self, dir: Ino, name: &str) -> Result<(), FsError> {
        let mut dir_ino = self.read_inode(dir)?;
        if dir_ino.kind != InodeKind::Directory {
            return Err(FsError::NotADirectory);
        }
        let mut entries = self.decode_dir_entries(&dir_ino)?;
        let pos = entries
            .iter()
            .position(|e| e.name == name)
            .ok_or(FsError::NotFound)?;
        let target_ino = entries[pos].ino;
        let target = self.read_inode(target_ino)?;
        if target.kind != InodeKind::Directory {
            return Err(FsError::NotADirectory);
        }
        let target_entries = self.decode_dir_entries(&target)?;
        if !target_entries.is_empty() {
            return Err(FsError::NotEmpty);
        }

        entries.remove(pos);
        let new_bytes = Self::encode_dir_entries(&entries);

        let mut txn = Transaction::new();
        let dead = InodeOnDisk::unused(target_ino);
        self.stage_inode_write(&dead, &mut txn)?;
        self.stage_write_dir_bytes(&mut dir_ino, &new_bytes, &mut txn)?;
        self.stage_inode_write(&dir_ino, &mut txn)?;
        self.commit_and_apply(&txn)?;
        Ok(())
    }

    fn rename(
        &mut self,
        from_dir: Ino,
        from_name: &str,
        to_dir: Ino,
        to_name: &str,
    ) -> Result<(), FsError> {
        Self::check_name(to_name)?;
        // The simple case: same directory.
        if from_dir == to_dir {
            let mut dir_ino = self.read_inode(from_dir)?;
            if dir_ino.kind != InodeKind::Directory {
                return Err(FsError::NotADirectory);
            }
            let mut entries = self.decode_dir_entries(&dir_ino)?;
            let pos = entries
                .iter()
                .position(|e| e.name == from_name)
                .ok_or(FsError::NotFound)?;
            let src_ino = entries[pos].ino;

            // If the destination exists, unlink it first.
            if let Some(dst_pos) = entries.iter().position(|e| e.name == to_name) {
                let dst_ino = entries[dst_pos].ino;
                let dst = self.read_inode(dst_ino)?;
                if dst.kind == InodeKind::Directory {
                    // POSIX rejects rename-over-directory unless
                    // src is also a directory and dst is empty.
                    // v1 keeps the strict case: error.
                    return Err(FsError::AlreadyExists);
                }
                entries.remove(dst_pos);
                // Zero the evicted dst inode. Its data blocks leak.
                let mut dead_txn_only = Transaction::new();
                self.stage_inode_write(&InodeOnDisk::unused(dst_ino), &mut dead_txn_only)?;
                // We'll merge this into the main txn below.
                entries.iter_mut().for_each(|e| {
                    if e.ino == src_ino {
                        e.name = to_name.into();
                    }
                });
                // Recompute pos since we removed dst_pos which may
                // have been before or after pos.
                let new_bytes = Self::encode_dir_entries(&entries);
                let mut txn = dead_txn_only;
                self.stage_write_dir_bytes(&mut dir_ino, &new_bytes, &mut txn)?;
                self.stage_inode_write(&dir_ino, &mut txn)?;
                self.commit_and_apply(&txn)?;
                return Ok(());
            }

            // No destination collision: rename in place.
            entries[pos].name = to_name.into();
            let new_bytes = Self::encode_dir_entries(&entries);

            let mut txn = Transaction::new();
            self.stage_write_dir_bytes(&mut dir_ino, &new_bytes, &mut txn)?;
            self.stage_inode_write(&dir_ino, &mut txn)?;
            self.commit_and_apply(&txn)?;
            return Ok(());
        }

        // Cross-directory rename. Same fs, so both inodes are ours.
        let mut from_ino = self.read_inode(from_dir)?;
        let mut to_ino = self.read_inode(to_dir)?;
        if from_ino.kind != InodeKind::Directory || to_ino.kind != InodeKind::Directory {
            return Err(FsError::NotADirectory);
        }

        let mut from_entries = self.decode_dir_entries(&from_ino)?;
        let pos = from_entries
            .iter()
            .position(|e| e.name == from_name)
            .ok_or(FsError::NotFound)?;
        let moved_ino = from_entries[pos].ino;
        from_entries.remove(pos);

        let mut to_entries = self.decode_dir_entries(&to_ino)?;
        if to_entries.iter().any(|e| e.name == to_name) {
            return Err(FsError::AlreadyExists);
        }
        to_entries.push(DirEntryOnDisk {
            ino: moved_ino,
            name: to_name.into(),
        });

        let from_bytes = Self::encode_dir_entries(&from_entries);
        let to_bytes = Self::encode_dir_entries(&to_entries);

        let mut txn = Transaction::new();
        self.stage_write_dir_bytes(&mut from_ino, &from_bytes, &mut txn)?;
        self.stage_write_dir_bytes(&mut to_ino, &to_bytes, &mut txn)?;
        self.stage_inode_write(&from_ino, &mut txn)?;
        self.stage_inode_write(&to_ino, &mut txn)?;
        self.commit_and_apply(&txn)?;
        Ok(())
    }

    fn stat(&mut self, ino: Ino) -> Result<FileStat, FsError> {
        let i = self.read_inode(ino)?;
        let ty = i.kind.to_node_type().ok_or(FsError::NotFound)?;
        Ok(FileStat {
            ino: i.ino,
            ty,
            mode: i.mode,
            nlink: i.nlink,
            size: i.size,
            atime_ns: i.atime_ns,
            mtime_ns: i.mtime_ns,
            ctime_ns: i.ctime_ns,
        })
    }

    fn truncate(&mut self, ino: Ino, new_size: u64) -> Result<(), FsError> {
        let now = now_ns();
        let mut file = self.read_inode(ino)?;
        let mut txn = Transaction::new();
        self.stage_file_truncate(&mut file, new_size, &mut txn)?;
        file.mtime_ns = now;
        file.ctime_ns = now;
        self.stage_inode_write(&file, &mut txn)?;
        self.commit_and_apply(&txn)?;
        Ok(())
    }

    fn set_times(
        &mut self,
        ino: Ino,
        atime_ns: Option<NanosSinceEpoch>,
        mtime_ns: Option<NanosSinceEpoch>,
    ) -> Result<(), FsError> {
        let now = now_ns();
        let mut inode = self.read_inode(ino)?;
        if let Some(a) = atime_ns {
            inode.atime_ns = a;
        }
        if let Some(m) = mtime_ns {
            inode.mtime_ns = m;
        }
        // Metadata change bumps ctime — identical to how write/
        // truncate stamp it — so a stat after set_times sees a
        // non-zero ctime even if the caller only asked for
        // SET_ATIM.
        inode.ctime_ns = now;
        let mut txn = Transaction::new();
        self.stage_inode_write(&inode, &mut txn)?;
        self.commit_and_apply(&txn)?;
        Ok(())
    }

    fn sync(&mut self) -> Result<(), FsError> {
        self.journal.apply_committed(self.device.as_mut())?;
        self.write_superblock()?;
        self.device.flush()?;
        Ok(())
    }

    fn storage_usage(&self) -> Option<StorageUsage> {
        let block_size = BLOCK_SIZE as u64;
        let allocated_blocks = self.sb.total_blocks.saturating_sub(self.sb.data_block_free);
        Some(StorageUsage {
            quota_bytes: self.sb.total_blocks.saturating_mul(block_size),
            used_bytes: allocated_blocks.saturating_mul(block_size),
            file_count: self.sb.inode_count.saturating_sub(self.sb.inode_free),
        })
    }

    fn kind_name(&self) -> &'static str {
        "opfs"
    }
}

/// Wall-clock ns via the active Platform impl, pulled into a local
/// helper so the per-method call sites stay compact. Writes go
/// through the journal + `stage_inode_write`, so the new timestamp
/// lands atomically alongside the payload it stamps.
fn now_ns() -> NanosSinceEpoch {
    platform::current().now_realtime_ns()
}
