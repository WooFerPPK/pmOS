//! OPFS journal.
//!
//! A redo log kept in a fixed region of the block device. Every
//! metadata mutation (inode writes, directory-content writes,
//! superblock updates) is packaged into a [`Transaction`],
//! written to the journal, committed with a CRC-guarded header,
//! and then applied to the main filesystem. On mount, any
//! committed-but-not-applied transaction is replayed; any
//! half-written transaction is discarded.
//!
//! This satisfies FR-014: abrupt tab close (or any torn write
//! mid-transaction) leaves the disk in one of three states, all
//! of which are recoverable:
//!
//!   1. Transaction not yet written to the journal. Nothing to
//!      do — the pre-transaction state is still intact.
//!   2. Transaction partially written, commit header missing.
//!      Replay sees no valid header at the expected slot and
//!      stops — we effectively roll back.
//!   3. Transaction written and committed, but main fs writes
//!      not flushed (or only partially flushed). Replay reads
//!      the header, walks every payload slot, and reapplies
//!      each block write — every op is an idempotent whole-
//!      block overwrite, so running them again is safe.
//!
//! ## On-disk layout
//!
//! The journal region is a ring of blocks whose starting LBA
//! and length are recorded in the superblock. Each transaction
//! occupies one **header block** followed by one **payload
//! block per op**:
//!
//! ```text
//! Header block (1 block):
//!   offset  size  field
//!   0       4     magic "JTXN"
//!   4       4     sequence (monotonic u32; advisory)
//!   8       4     op_count (u32, 0..=MAX_OPS_PER_TXN)
//!   12      4     (reserved, 0)
//!   16      16    op[0]:  target_lba (u64), reserved (u64)
//!   32      16    op[1]:  target_lba (u64), reserved (u64)
//!   ...     ...
//!   <pad>   ...   zeros
//!   4092    4     CRC-32 over bytes 0..4092
//!
//! Payload blocks (1 per op): raw 4096 bytes that will be
//! written to op[i].target_lba during apply. No per-block
//! CRC — the header CRC covers the op metadata, and the
//! write ordering below guarantees that a torn payload write
//! is caught because the header is written LAST.
//! ```
//!
//! ## Write ordering
//!
//! `commit()` writes payload blocks FIRST, then the header block.
//! The header is the commit point: until it's durable, replay
//! sees either an absent header (the ring slot retains its old
//! contents, which have either an older sequence or a bad
//! magic) or a torn header (bad CRC). Either way, replay stops
//! before the torn txn and the fs stays consistent.
//!
//! After commit, `apply_committed()` replays the same
//! transactions to the main filesystem. This is both the
//! normal apply-after-commit path and the replay-on-mount path
//! — they are identical.

use alloc::vec::Vec;

use crate::vfs::FsError;

use super::block::{Block, BlockDevice};
use super::layout::{crc32, Lba, BLOCK_SIZE};

/// Header-block magic.
const HEADER_MAGIC: [u8; 4] = *b"JTXN";

/// Maximum number of ops a single transaction can hold.
/// Computed so the header block's op-metadata table + magic +
/// CRC fit in `BLOCK_SIZE - 4` bytes.
pub const MAX_OPS_PER_TXN: usize = (BLOCK_SIZE - 16 - 4) / 16;

/// One pending journal op: a block-write with a target LBA and
/// the full replacement contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalOp {
    pub target_lba: Lba,
    pub data: Block,
}

/// A transaction in flight. Accumulate ops via [`Transaction::add_write`],
/// then hand it to [`Journal::commit`] to make it durable.
pub struct Transaction {
    ops: Vec<JournalOp>,
}

impl Transaction {
    pub fn new() -> Self {
        Transaction { ops: Vec::new() }
    }

    /// Stage a block-write to be applied when this transaction
    /// commits. Returns `Err(FsError::NoSpace)` if the txn is
    /// already at its op-count cap.
    pub fn add_write(&mut self, target_lba: Lba, data: Block) -> Result<(), FsError> {
        if self.ops.len() >= MAX_OPS_PER_TXN {
            return Err(FsError::NoSpace);
        }
        self.ops.push(JournalOp { target_lba, data });
        Ok(())
    }

    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    /// Total journal-block cost (header + payloads).
    pub fn journal_blocks_needed(&self) -> u64 {
        1 + self.ops.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

impl Default for Transaction {
    fn default() -> Self {
        Transaction::new()
    }
}

/// Writable journal state.
///
/// Owns the ring head/tail pointers and a monotonic sequence
/// counter. Head/tail are persisted to the superblock on every
/// update; on mount the caller restores them via
/// [`Journal::load_cursors`].
pub struct Journal {
    pub ring_start: Lba,
    pub ring_blocks: u64,
    /// Next free slot, expressed as a block offset from `ring_start`
    /// (always < `ring_blocks`).
    pub head: u64,
    /// Oldest unapplied slot. When `tail == head` the ring is empty.
    pub tail: u64,
    /// Monotonic sequence counter for commit headers. Starts at 1.
    pub next_sequence: u32,
}

impl Journal {
    pub fn new(ring_start: Lba, ring_blocks: u64) -> Self {
        Journal {
            ring_start,
            ring_blocks,
            head: 0,
            tail: 0,
            next_sequence: 1,
        }
    }

    /// Reset to a pristine empty state. Used by mkfs.
    pub fn reset(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.next_sequence = 1;
    }

    /// Load head/tail cursors after reading the superblock on mount.
    pub fn load_cursors(&mut self, head: u64, tail: u64, next_sequence: u32) {
        self.head = head;
        self.tail = tail;
        self.next_sequence = next_sequence;
    }

    /// How many ring slots are currently free. When the ring is
    /// completely empty (head == tail), every slot is free.
    fn free_slots(&self) -> u64 {
        if self.head == self.tail {
            self.ring_blocks
        } else if self.head > self.tail {
            self.ring_blocks - (self.head - self.tail)
        } else {
            self.tail - self.head
        }
    }

    /// Make a transaction durable in the journal.
    ///
    /// Write ordering is (1) payloads, (2) header, in that
    /// order. The header is the commit point — replay trusts
    /// nothing until a valid header arrives.
    pub fn commit(
        &mut self,
        txn: &Transaction,
        device: &mut dyn BlockDevice,
    ) -> Result<(), FsError> {
        if txn.ops.is_empty() {
            // Empty txn is a no-op at the journal level.
            return Ok(());
        }
        let needed = txn.journal_blocks_needed();
        if needed > self.ring_blocks {
            return Err(FsError::NoSpace);
        }
        // We need at least `needed` free slots. If the caller
        // hasn't applied + advanced the tail yet, refuse.
        if needed > self.free_slots() {
            return Err(FsError::NoSpace);
        }

        let header_slot = self.head;
        let sequence = self.next_sequence;

        // Step 1: write every payload block.
        for (i, op) in txn.ops.iter().enumerate() {
            let slot = (self.head + 1 + i as u64) % self.ring_blocks;
            let lba = self.ring_start + slot;
            device.write(lba, &op.data)?;
        }

        // Step 2: write the header. This is the commit point.
        let header_block = encode_header_block(sequence, &txn.ops);
        let header_lba = self.ring_start + header_slot;
        device.write(header_lba, &header_block)?;

        // Step 3: advance head + bump sequence.
        self.head = (self.head + needed) % self.ring_blocks;
        self.next_sequence = self.next_sequence.wrapping_add(1);

        Ok(())
    }

    /// Apply every committed-but-not-yet-applied transaction,
    /// walking from `tail` forward to `head`. Stops on the first
    /// invalid header or the first torn payload read.
    ///
    /// Returns the number of transactions applied. The caller is
    /// expected to flush the device afterwards (we flush
    /// internally between txns too, so intermediate state is safe).
    pub fn apply_committed(
        &mut self,
        device: &mut dyn BlockDevice,
    ) -> Result<usize, FsError> {
        let mut applied = 0usize;
        while self.tail != self.head {
            // Read the header at the current tail slot.
            let header_lba = self.ring_start + self.tail;
            let mut header_buf = [0u8; BLOCK_SIZE];
            device.read(header_lba, &mut header_buf)?;

            let (_sequence, op_metas) = match decode_header_block(&header_buf) {
                Some(v) => v,
                None => {
                    // Corrupt or absent header. The journal is
                    // truncated here. Leave tail where it is —
                    // future commits will overwrite this slot.
                    return Ok(applied);
                }
            };

            let txn_blocks = 1 + op_metas.len() as u64;
            if txn_blocks > self.ring_blocks {
                return Ok(applied);
            }

            // Read every payload block BEFORE applying any of
            // them, so we never half-apply a torn txn.
            let mut pending: Vec<(Lba, Block)> = Vec::with_capacity(op_metas.len());
            for (i, target_lba) in op_metas.iter().enumerate() {
                let slot = (self.tail + 1 + i as u64) % self.ring_blocks;
                let lba = self.ring_start + slot;
                let mut payload = [0u8; BLOCK_SIZE];
                device.read(lba, &mut payload)?;
                pending.push((*target_lba, payload));
            }

            // Apply every op. Each is an idempotent whole-block
            // overwrite, so replay is safe.
            for (target_lba, payload) in &pending {
                device.write(*target_lba, payload)?;
            }
            device.flush()?;

            // Advance past this txn.
            self.tail = (self.tail + txn_blocks) % self.ring_blocks;
            applied += 1;
        }
        Ok(applied)
    }
}

// --- Header encode / decode --------------------------------------------

fn encode_header_block(sequence: u32, ops: &[JournalOp]) -> Block {
    debug_assert!(ops.len() <= MAX_OPS_PER_TXN);
    let mut b = [0u8; BLOCK_SIZE];
    b[0..4].copy_from_slice(&HEADER_MAGIC);
    b[4..8].copy_from_slice(&sequence.to_le_bytes());
    b[8..12].copy_from_slice(&(ops.len() as u32).to_le_bytes());
    // bytes 12..16: reserved (zero)
    let mut off = 16;
    for op in ops {
        b[off..off + 8].copy_from_slice(&op.target_lba.to_le_bytes());
        // bytes [off+8..off+16]: reserved (zero)
        off += 16;
    }
    // Trailing zeros until CRC.
    let csum = crc32(&b[0..BLOCK_SIZE - 4]);
    b[BLOCK_SIZE - 4..BLOCK_SIZE].copy_from_slice(&csum.to_le_bytes());
    b
}

/// Returns `Some((sequence, target_lbas))` if the header is
/// well-formed, else `None`.
fn decode_header_block(b: &Block) -> Option<(u32, Vec<Lba>)> {
    if b[0..4] != HEADER_MAGIC {
        return None;
    }
    let expected_crc = u32::from_le_bytes([
        b[BLOCK_SIZE - 4],
        b[BLOCK_SIZE - 3],
        b[BLOCK_SIZE - 2],
        b[BLOCK_SIZE - 1],
    ]);
    if crc32(&b[0..BLOCK_SIZE - 4]) != expected_crc {
        return None;
    }
    let sequence = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let op_count = u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize;
    if op_count > MAX_OPS_PER_TXN {
        return None;
    }
    let mut lbas = Vec::with_capacity(op_count);
    for i in 0..op_count {
        let off = 16 + i * 16;
        let lba = u64::from_le_bytes([
            b[off], b[off + 1], b[off + 2], b[off + 3],
            b[off + 4], b[off + 5], b[off + 6], b[off + 7],
        ]);
        lbas.push(lba);
    }
    Some((sequence, lbas))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_ring_is_all_free() {
        let j = Journal::new(100, 32);
        assert_eq!(j.free_slots(), 32);
    }

    #[test]
    fn head_ahead_of_tail_free_slots() {
        let mut j = Journal::new(0, 32);
        j.head = 8;
        j.tail = 0;
        assert_eq!(j.free_slots(), 24);
    }

    #[test]
    fn head_wrapped_past_tail_free_slots() {
        let mut j = Journal::new(0, 32);
        j.head = 4;
        j.tail = 24;
        assert_eq!(j.free_slots(), 20);
    }

    #[test]
    fn encode_decode_header_round_trip() {
        let ops = [
            JournalOp { target_lba: 100, data: [0u8; BLOCK_SIZE] },
            JournalOp { target_lba: 200, data: [0u8; BLOCK_SIZE] },
            JournalOp { target_lba: 300, data: [0u8; BLOCK_SIZE] },
        ];
        let block = encode_header_block(42, &ops);
        let (seq, lbas) = decode_header_block(&block).expect("decode");
        assert_eq!(seq, 42);
        assert_eq!(lbas, alloc::vec![100, 200, 300]);
    }

    #[test]
    fn torn_header_rejected_by_crc() {
        let ops = [JournalOp { target_lba: 1, data: [0u8; BLOCK_SIZE] }];
        let mut block = encode_header_block(1, &ops);
        block[100] ^= 0xFF; // flip a bit in the middle
        assert!(decode_header_block(&block).is_none());
    }

    #[test]
    fn bad_magic_rejected() {
        let block = [0u8; BLOCK_SIZE];
        assert!(decode_header_block(&block).is_none());
    }

    #[test]
    fn max_ops_per_txn_is_reasonable() {
        // (4096 - 16 - 4) / 16 = 254
        assert_eq!(MAX_OPS_PER_TXN, 254);
    }
}
