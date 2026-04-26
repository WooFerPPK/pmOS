//! OPFS on-disk layout.
//!
//! Every byte pattern PMos writes to the block device is defined
//! here. Mirrors `data-model.md §3.3`. The on-disk format is
//! little-endian throughout (wasm32 is unconditionally LE, and
//! every host target we care about is too).
//!
//! Simplifications from data-model.md §3.3 for the v1 slice:
//!
//! * No double-indirect block. The v1 inode holds
//!   `direct_blocks: [u64; 12]` + one indirect block pointer.
//!   With a 4 KiB block size this gives a per-file cap of
//!   12*4 KiB + 512*4 KiB = 2 MiB. That covers every file the
//!   bundled apps and the starter kit write. Larger files are
//!   a v2 amendment.
//! * Free data blocks are tracked by a simple next-free cursor
//!   in the superblock. Freed blocks are NOT reused in v1 —
//!   this means long-lived filesystems fragment over time, but
//!   the correctness story is simpler. A proper bitmap or
//!   free-list allocator lands in a later polish pass when
//!   measurement shows fragmentation is actually costing
//!   something.
//! * Inode allocation is also next-free-cursor. Same rationale.

use alloc::vec::Vec;

use crate::vfs::{FsError, NodeType};

/// Size of a block in bytes. Every disk I/O is a multiple of this.
pub const BLOCK_SIZE: usize = 4096;

/// Logical block address. LBA 0 is the superblock.
pub type Lba = u64;

/// Superblock LBA.
pub const LBA_SUPERBLOCK: Lba = 0;

/// Magic bytes that identify a PMos OPFS image.
pub const FS_MAGIC: [u8; 8] = *b"PMOSFS\x01\x00";

/// Current on-disk format version.
pub const FS_VERSION_MAJOR: u16 = 1;
pub const FS_VERSION_MINOR: u16 = 0;

/// Number of direct data-block pointers in an on-disk inode.
pub const INODE_DIRECT_BLOCKS: usize = 12;

/// Per-inode byte size. Chosen so that exactly 16 inodes fit in a
/// 4 KiB block (4096 / 256 = 16).
pub const INODE_BYTES: usize = 256;

/// Inodes per block.
pub const INODES_PER_BLOCK: u64 = (BLOCK_SIZE / INODE_BYTES) as u64;

/// Root inode number.
pub const ROOT_INO: u64 = 1;

// --- Partition layout within the block device ---------------------------
//
// The block device presented by the TypeScript block driver is a flat LBA
// space. The kernel partitions it as:
//
//   LBA 0                     : superblock (1 block)
//   LBA 1 .. 1+JOURNAL_BLOCKS : journal ring
//   LBA .. INODE_TABLE_END    : inode table (packed inodes)
//   LBA .. end                : data blocks
//
// The exact block counts are recorded in the superblock so different
// images can have different sizes; the constants below are defaults
// used by `mkfs` for a 16 MiB image.

/// Default journal size in blocks for `mkfs` (= 256 * 4 KiB = 1 MiB).
pub const DEFAULT_JOURNAL_BLOCKS: u64 = 256;

/// Default inode-table size in blocks for `mkfs` (= 128 * 16 inodes = 2048 inodes).
pub const DEFAULT_INODE_TABLE_BLOCKS: u64 = 128;

/// Minimum block count for a valid OPFS image: superblock + journal +
/// inode table + at least one data block.
pub const MIN_BLOCK_COUNT: u64 = 1 + DEFAULT_JOURNAL_BLOCKS + DEFAULT_INODE_TABLE_BLOCKS + 1;

// --- Superblock ---------------------------------------------------------

/// In-memory superblock representation.
///
/// Serialises to exactly `BLOCK_SIZE` bytes on disk. Fields are
/// laid out in declaration order with explicit offsets in
/// [`Superblock::to_bytes`] so the layout is unambiguous even
/// when the struct is reordered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Superblock {
    pub magic: [u8; 8],
    pub version_major: u16,
    pub version_minor: u16,
    pub block_size: u32,
    pub total_blocks: u64,
    pub journal_start: Lba,
    pub journal_blocks: u64,
    pub inode_table_start: Lba,
    pub inode_table_blocks: u64,
    pub inode_count: u64,
    pub inode_free: u64,
    /// Next-free-inode cursor. mkfs sets this to 2 (after root).
    pub next_free_inode: u64,
    pub data_start: Lba,
    pub data_block_count: u64,
    pub data_block_free: u64,
    /// Next-free-data-block cursor. mkfs sets this to the first
    /// data block; allocation bumps it, freed blocks are NOT
    /// reused in v1.
    pub next_free_data_block: Lba,
    pub root_ino: u64,
    pub journal_head: u64,
    pub journal_tail: u64,
    /// Monotonic mount generation. Bumped each time the image is
    /// mounted; journal replay uses it to skip entries from a
    /// previous mount that had a torn commit.
    pub mount_generation: u64,
}

impl Superblock {
    /// Serialise to a 4 KiB block. The trailing bytes (past the
    /// structure) are zeroed.
    pub fn to_bytes(&self) -> [u8; BLOCK_SIZE] {
        let mut b = [0u8; BLOCK_SIZE];
        b[0..8].copy_from_slice(&self.magic);
        b[8..10].copy_from_slice(&self.version_major.to_le_bytes());
        b[10..12].copy_from_slice(&self.version_minor.to_le_bytes());
        b[12..16].copy_from_slice(&self.block_size.to_le_bytes());
        b[16..24].copy_from_slice(&self.total_blocks.to_le_bytes());
        b[24..32].copy_from_slice(&self.journal_start.to_le_bytes());
        b[32..40].copy_from_slice(&self.journal_blocks.to_le_bytes());
        b[40..48].copy_from_slice(&self.inode_table_start.to_le_bytes());
        b[48..56].copy_from_slice(&self.inode_table_blocks.to_le_bytes());
        b[56..64].copy_from_slice(&self.inode_count.to_le_bytes());
        b[64..72].copy_from_slice(&self.inode_free.to_le_bytes());
        b[72..80].copy_from_slice(&self.next_free_inode.to_le_bytes());
        b[80..88].copy_from_slice(&self.data_start.to_le_bytes());
        b[88..96].copy_from_slice(&self.data_block_count.to_le_bytes());
        b[96..104].copy_from_slice(&self.data_block_free.to_le_bytes());
        b[104..112].copy_from_slice(&self.next_free_data_block.to_le_bytes());
        b[112..120].copy_from_slice(&self.root_ino.to_le_bytes());
        b[120..128].copy_from_slice(&self.journal_head.to_le_bytes());
        b[128..136].copy_from_slice(&self.journal_tail.to_le_bytes());
        b[136..144].copy_from_slice(&self.mount_generation.to_le_bytes());
        // Checksum covers bytes 0..BLOCK_SIZE-4; write it in the
        // last 4 bytes so a torn write past the struct body is
        // still caught.
        let csum = crc32(&b[0..BLOCK_SIZE - 4]);
        b[BLOCK_SIZE - 4..BLOCK_SIZE].copy_from_slice(&csum.to_le_bytes());
        b
    }

    /// Deserialise from a 4 KiB block. Validates magic, version,
    /// and checksum.
    pub fn from_bytes(b: &[u8; BLOCK_SIZE]) -> Result<Self, FsError> {
        let mut magic = [0u8; 8];
        magic.copy_from_slice(&b[0..8]);
        if magic != FS_MAGIC {
            return Err(FsError::Io);
        }
        // Checksum check.
        let expected = u32::from_le_bytes([
            b[BLOCK_SIZE - 4],
            b[BLOCK_SIZE - 3],
            b[BLOCK_SIZE - 2],
            b[BLOCK_SIZE - 1],
        ]);
        if crc32(&b[0..BLOCK_SIZE - 4]) != expected {
            return Err(FsError::Io);
        }
        let sb = Superblock {
            magic,
            version_major: u16::from_le_bytes([b[8], b[9]]),
            version_minor: u16::from_le_bytes([b[10], b[11]]),
            block_size: u32_at(b, 12),
            total_blocks: u64_at(b, 16),
            journal_start: u64_at(b, 24),
            journal_blocks: u64_at(b, 32),
            inode_table_start: u64_at(b, 40),
            inode_table_blocks: u64_at(b, 48),
            inode_count: u64_at(b, 56),
            inode_free: u64_at(b, 64),
            next_free_inode: u64_at(b, 72),
            data_start: u64_at(b, 80),
            data_block_count: u64_at(b, 88),
            data_block_free: u64_at(b, 96),
            next_free_data_block: u64_at(b, 104),
            root_ino: u64_at(b, 112),
            journal_head: u64_at(b, 120),
            journal_tail: u64_at(b, 128),
            mount_generation: u64_at(b, 136),
        };
        if sb.version_major != FS_VERSION_MAJOR {
            return Err(FsError::Io);
        }
        if sb.block_size as usize != BLOCK_SIZE {
            return Err(FsError::Io);
        }
        Ok(sb)
    }
}

// --- Inode --------------------------------------------------------------

/// On-disk inode, serialises to exactly `INODE_BYTES` bytes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InodeOnDisk {
    pub ino: u64,
    /// Node type. Encoded as a single byte at offset 8.
    pub kind: InodeKind,
    /// POSIX mode bits.
    pub mode: u32,
    pub nlink: u32,
    /// File size in bytes, or directory content length in bytes
    /// (directory content is a sequence of DirEntryOnDisk records).
    pub size: u64,
    pub atime_ns: u64,
    pub mtime_ns: u64,
    pub ctime_ns: u64,
    pub direct: [Lba; INODE_DIRECT_BLOCKS],
    pub indirect: Lba,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InodeKind {
    Unused = 0,
    RegularFile = 1,
    Directory = 2,
    SymLink = 3,
}

impl InodeKind {
    pub const fn to_node_type(self) -> Option<NodeType> {
        match self {
            InodeKind::RegularFile => Some(NodeType::RegularFile),
            InodeKind::Directory => Some(NodeType::Directory),
            InodeKind::SymLink => Some(NodeType::SymLink),
            InodeKind::Unused => None,
        }
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(InodeKind::Unused),
            1 => Some(InodeKind::RegularFile),
            2 => Some(InodeKind::Directory),
            3 => Some(InodeKind::SymLink),
            _ => None,
        }
    }
}

impl InodeOnDisk {
    /// Create an empty unused slot.
    pub const fn unused(ino: u64) -> Self {
        InodeOnDisk {
            ino,
            kind: InodeKind::Unused,
            mode: 0,
            nlink: 0,
            size: 0,
            atime_ns: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            direct: [0; INODE_DIRECT_BLOCKS],
            indirect: 0,
        }
    }

    /// Serialise into the inode-sized byte slot at `out[..INODE_BYTES]`.
    pub fn to_bytes(&self, out: &mut [u8; INODE_BYTES]) {
        out.fill(0);
        out[0..8].copy_from_slice(&self.ino.to_le_bytes());
        out[8] = self.kind as u8;
        // 3 bytes padding at 9..12
        out[12..16].copy_from_slice(&self.mode.to_le_bytes());
        out[16..20].copy_from_slice(&self.nlink.to_le_bytes());
        // 4 bytes padding at 20..24
        out[24..32].copy_from_slice(&self.size.to_le_bytes());
        out[32..40].copy_from_slice(&self.atime_ns.to_le_bytes());
        out[40..48].copy_from_slice(&self.mtime_ns.to_le_bytes());
        out[48..56].copy_from_slice(&self.ctime_ns.to_le_bytes());
        for i in 0..INODE_DIRECT_BLOCKS {
            let off = 56 + i * 8;
            out[off..off + 8].copy_from_slice(&self.direct[i].to_le_bytes());
        }
        // 56 + 12*8 = 152
        out[152..160].copy_from_slice(&self.indirect.to_le_bytes());
        // 160..256 reserved (zeroed)
    }

    /// Deserialise from an inode-sized byte slot.
    pub fn from_bytes(b: &[u8; INODE_BYTES]) -> Result<Self, FsError> {
        let kind = InodeKind::from_u8(b[8]).ok_or(FsError::Io)?;
        let mut direct = [0u64; INODE_DIRECT_BLOCKS];
        for i in 0..INODE_DIRECT_BLOCKS {
            direct[i] = u64_at_slice(b, 56 + i * 8);
        }
        Ok(InodeOnDisk {
            ino: u64_at_slice(b, 0),
            kind,
            mode: u32_at_slice(b, 12),
            nlink: u32_at_slice(b, 16),
            size: u64_at_slice(b, 24),
            atime_ns: u64_at_slice(b, 32),
            mtime_ns: u64_at_slice(b, 40),
            ctime_ns: u64_at_slice(b, 48),
            direct,
            indirect: u64_at_slice(b, 152),
        })
    }
}

// --- Directory entries --------------------------------------------------

/// A single directory entry as stored on disk. Directories are
/// logical files whose contents are a packed sequence of these
/// records, one after another, 8-byte aligned.
///
/// Layout:
///
/// ```text
/// offset  size  field
/// ------  ----  -----
/// 0       8     ino
/// 8       2     record_len (total bytes including this header + name + padding)
/// 10      2     name_len
/// 12      name_len bytes of UTF-8 name (no nul terminator)
/// 12+nl   pad   zeros up to 8-byte alignment from the start of the record
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntryOnDisk {
    pub ino: u64,
    pub name: alloc::string::String,
}

impl DirEntryOnDisk {
    /// Bytes this entry will consume on disk when serialised.
    pub fn encoded_len(&self) -> usize {
        let raw = 12 + self.name.len();
        (raw + 7) & !7 // round up to 8
    }

    /// Encode the entry, returning exactly `encoded_len` bytes.
    pub fn encode(&self) -> Vec<u8> {
        let len = self.encoded_len();
        let mut out = alloc::vec![0u8; len];
        out[0..8].copy_from_slice(&self.ino.to_le_bytes());
        out[8..10].copy_from_slice(&(len as u16).to_le_bytes());
        out[10..12].copy_from_slice(&(self.name.len() as u16).to_le_bytes());
        out[12..12 + self.name.len()].copy_from_slice(self.name.as_bytes());
        out
    }

    /// Decode a single entry starting at `b[0..]`. Returns the
    /// entry plus the number of bytes consumed.
    pub fn decode(b: &[u8]) -> Result<(Self, usize), FsError> {
        if b.len() < 12 {
            return Err(FsError::Io);
        }
        let ino = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
        let record_len = u16::from_le_bytes([b[8], b[9]]) as usize;
        let name_len = u16::from_le_bytes([b[10], b[11]]) as usize;
        if record_len < 12 + name_len || b.len() < record_len {
            return Err(FsError::Io);
        }
        if record_len & 7 != 0 {
            return Err(FsError::Io);
        }
        let name_bytes = &b[12..12 + name_len];
        let name = core::str::from_utf8(name_bytes)
            .map_err(|_| FsError::Io)?
            .into();
        Ok((DirEntryOnDisk { ino, name }, record_len))
    }
}

// --- Helpers ------------------------------------------------------------

fn u64_at(b: &[u8; BLOCK_SIZE], at: usize) -> u64 {
    u64::from_le_bytes([
        b[at],
        b[at + 1],
        b[at + 2],
        b[at + 3],
        b[at + 4],
        b[at + 5],
        b[at + 6],
        b[at + 7],
    ])
}

fn u32_at(b: &[u8; BLOCK_SIZE], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at_slice(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        b[at],
        b[at + 1],
        b[at + 2],
        b[at + 3],
        b[at + 4],
        b[at + 5],
        b[at + 6],
        b[at + 7],
    ])
}

fn u32_at_slice(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// A small CRC-32 (IEEE polynomial). Used for the superblock
/// checksum and for journal commit markers. Table-free — we
/// compute on the fly because these are not on the hot path.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn superblock_round_trip() {
        let sb = Superblock {
            magic: FS_MAGIC,
            version_major: FS_VERSION_MAJOR,
            version_minor: FS_VERSION_MINOR,
            block_size: BLOCK_SIZE as u32,
            total_blocks: 4096,
            journal_start: 1,
            journal_blocks: DEFAULT_JOURNAL_BLOCKS,
            inode_table_start: 1 + DEFAULT_JOURNAL_BLOCKS,
            inode_table_blocks: DEFAULT_INODE_TABLE_BLOCKS,
            inode_count: 2048,
            inode_free: 2047,
            next_free_inode: 2,
            data_start: 1 + DEFAULT_JOURNAL_BLOCKS + DEFAULT_INODE_TABLE_BLOCKS,
            data_block_count: 4096 - (1 + DEFAULT_JOURNAL_BLOCKS + DEFAULT_INODE_TABLE_BLOCKS),
            data_block_free: 4096 - (1 + DEFAULT_JOURNAL_BLOCKS + DEFAULT_INODE_TABLE_BLOCKS) - 1,
            next_free_data_block: 1 + DEFAULT_JOURNAL_BLOCKS + DEFAULT_INODE_TABLE_BLOCKS + 1,
            root_ino: ROOT_INO,
            journal_head: 0,
            journal_tail: 0,
            mount_generation: 1,
        };
        let bytes = sb.to_bytes();
        let parsed = Superblock::from_bytes(&bytes).expect("parse");
        assert_eq!(sb, parsed);
    }

    #[test]
    fn superblock_rejects_wrong_magic() {
        let sb = Superblock {
            magic: *b"NOTFS\x00\x00\x00",
            ..dummy_sb()
        };
        let bytes = sb.to_bytes();
        assert!(matches!(Superblock::from_bytes(&bytes), Err(FsError::Io)));
    }

    #[test]
    fn superblock_rejects_bad_checksum() {
        let sb = dummy_sb();
        let mut bytes = sb.to_bytes();
        bytes[100] ^= 0xFF; // flip a bit
        assert!(matches!(Superblock::from_bytes(&bytes), Err(FsError::Io)));
    }

    #[test]
    fn inode_round_trip() {
        let ino = InodeOnDisk {
            ino: 42,
            kind: InodeKind::RegularFile,
            mode: 0o644,
            nlink: 1,
            size: 1234,
            atime_ns: 10,
            mtime_ns: 20,
            ctime_ns: 30,
            direct: [100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111],
            indirect: 999,
        };
        let mut buf = [0u8; INODE_BYTES];
        ino.to_bytes(&mut buf);
        let parsed = InodeOnDisk::from_bytes(&buf).expect("parse");
        assert_eq!(ino, parsed);
    }

    #[test]
    fn inode_unused_round_trip() {
        let ino = InodeOnDisk::unused(7);
        let mut buf = [0u8; INODE_BYTES];
        ino.to_bytes(&mut buf);
        let parsed = InodeOnDisk::from_bytes(&buf).expect("parse");
        assert_eq!(parsed.kind, InodeKind::Unused);
        assert_eq!(parsed.ino, 7);
    }

    #[test]
    fn dir_entry_round_trip() {
        let e = DirEntryOnDisk {
            ino: 42,
            name: "hello.txt".to_string(),
        };
        let encoded = e.encode();
        assert_eq!(encoded.len(), e.encoded_len());
        assert_eq!(encoded.len() % 8, 0);
        let (parsed, consumed) = DirEntryOnDisk::decode(&encoded).unwrap();
        assert_eq!(parsed, e);
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn dir_entry_chain_decode() {
        let a = DirEntryOnDisk {
            ino: 1,
            name: "a".to_string(),
        };
        let b = DirEntryOnDisk {
            ino: 2,
            name: "longer".to_string(),
        };
        let mut buf = a.encode();
        buf.extend(b.encode());

        let (first, n) = DirEntryOnDisk::decode(&buf).unwrap();
        assert_eq!(first, a);
        let (second, _) = DirEntryOnDisk::decode(&buf[n..]).unwrap();
        assert_eq!(second, b);
    }

    #[test]
    fn crc32_known_values() {
        // "123456789" → 0xCBF43926 (standard CRC-32 test vector)
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    fn dummy_sb() -> Superblock {
        Superblock {
            magic: FS_MAGIC,
            version_major: FS_VERSION_MAJOR,
            version_minor: FS_VERSION_MINOR,
            block_size: BLOCK_SIZE as u32,
            total_blocks: 1024,
            journal_start: 1,
            journal_blocks: 16,
            inode_table_start: 17,
            inode_table_blocks: 16,
            inode_count: 256,
            inode_free: 255,
            next_free_inode: 2,
            data_start: 33,
            data_block_count: 991,
            data_block_free: 990,
            next_free_data_block: 34,
            root_ino: 1,
            journal_head: 0,
            journal_tail: 0,
            mount_generation: 1,
        }
    }
}
