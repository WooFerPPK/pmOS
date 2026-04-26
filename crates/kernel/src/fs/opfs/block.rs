//! Block-device abstraction.
//!
//! The OPFS filesystem is written against the [`BlockDevice`]
//! trait. Two impls:
//!
//! * [`MockBlockDevice`] — in-memory, `BTreeMap`-backed, for
//!   native tests. Supports capacity limits, `flush()` counters,
//!   and deterministic fault injection so T061's journal-replay
//!   test can simulate a torn write.
//! * `DriverBlockDevice` — lives in the kernel's device-dispatch
//!   layer (T067+). Uses `Platform::driver_call` to read and
//!   write blocks via the TypeScript block driver. Not in this
//!   slice; the kernel integration step wires it up.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;

use crate::vfs::FsError;

use super::layout::{Lba, BLOCK_SIZE};

/// Every block I/O is exactly this many bytes.
pub type Block = [u8; BLOCK_SIZE];

/// The interface OPFS uses to talk to whatever backs the disk.
pub trait BlockDevice: Send {
    /// Read one block at `lba` into `out`.
    fn read(&mut self, lba: Lba, out: &mut Block) -> Result<(), FsError>;

    /// Write one block at `lba` from `buf`.
    fn write(&mut self, lba: Lba, buf: &Block) -> Result<(), FsError>;

    /// Flush any buffered writes. For the mock this is a
    /// counter bump; for the real driver this is an
    /// ioctl FLUSH.
    fn flush(&mut self) -> Result<(), FsError>;

    /// Total number of addressable blocks.
    fn block_count(&self) -> u64;
}

/// In-memory block device for tests.
///
/// Uses a `BTreeMap<Lba, Box<Block>>` under the hood so unwritten
/// LBAs consume no memory. Reads of never-written LBAs return a
/// zero block (Unix sparse-file semantics).
///
/// Includes fault-injection hooks used by T061's journal replay
/// tests to simulate a torn write.
pub struct MockBlockDevice {
    blocks: BTreeMap<Lba, Box<Block>>,
    capacity: u64,
    flush_count: usize,
    /// Number of blocks currently allocated (distinct from
    /// capacity, which is the advertised block count).
    allocated: usize,
    /// If `Some(n)`, the next `n` writes succeed normally; the
    /// `(n+1)`th write returns `Err(Io)` without touching the
    /// underlying storage. After triggering, this resets to
    /// `None`. Used to simulate a torn write mid-journal.
    crash_after_writes: Option<usize>,
    /// Total number of successful writes since creation (for
    /// journal-replay tests).
    total_writes: u64,
}

impl MockBlockDevice {
    /// Create a fresh mock device with the given block count.
    pub fn new(capacity: u64) -> Self {
        MockBlockDevice {
            blocks: BTreeMap::new(),
            capacity,
            flush_count: 0,
            allocated: 0,
            crash_after_writes: None,
            total_writes: 0,
        }
    }

    /// Test helper: schedule the next `after`-th write (0-indexed
    /// from now) to fail with `Err(Io)`, simulating a torn write.
    /// Any subsequent write after the fault behaves normally.
    pub fn crash_after(&mut self, after: usize) {
        self.crash_after_writes = Some(after);
    }

    /// How many blocks have actually been written so far.
    pub fn allocated_blocks(&self) -> usize {
        self.allocated
    }

    /// How many times `flush` has been called.
    pub fn flush_count(&self) -> usize {
        self.flush_count
    }

    /// Total successful writes since creation.
    pub fn total_writes(&self) -> u64 {
        self.total_writes
    }

    /// Directly inspect the backing map. Test-only.
    pub fn raw_block(&self, lba: Lba) -> Option<&Block> {
        self.blocks.get(&lba).map(|b| b.as_ref())
    }
}

impl BlockDevice for MockBlockDevice {
    fn read(&mut self, lba: Lba, out: &mut Block) -> Result<(), FsError> {
        if lba >= self.capacity {
            return Err(FsError::InvalidArgument);
        }
        match self.blocks.get(&lba) {
            Some(b) => {
                out.copy_from_slice(b.as_ref());
            }
            None => {
                // Sparse read: never-written LBA returns all zeros.
                out.fill(0);
            }
        }
        Ok(())
    }

    fn write(&mut self, lba: Lba, buf: &Block) -> Result<(), FsError> {
        if lba >= self.capacity {
            return Err(FsError::NoSpace);
        }
        if let Some(remaining) = self.crash_after_writes {
            if remaining == 0 {
                // This write is the torn one.
                self.crash_after_writes = None;
                return Err(FsError::Io);
            }
            self.crash_after_writes = Some(remaining - 1);
        }
        let inserted = !self.blocks.contains_key(&lba);
        let mut block: Box<Block> = Box::new([0u8; BLOCK_SIZE]);
        block.copy_from_slice(buf);
        self.blocks.insert(lba, block);
        if inserted {
            self.allocated += 1;
        }
        self.total_writes += 1;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), FsError> {
        self.flush_count += 1;
        Ok(())
    }

    fn block_count(&self) -> u64 {
        self.capacity
    }
}

// --- Boxed trait-object convenience -----------------------------------

/// Shortcut for storing a `Box<dyn BlockDevice>` in struct fields
/// that want a concrete type name rather than a `dyn` cluttering
/// every signature.
pub type DynBlockDevice = Box<dyn BlockDevice>;

// --- Block driver protocol opcodes ------------------------------------
//
// Mirrored TS-side as constants in `web/src/drivers/block.ts`. The
// kernel issues these via `Platform::driver_call(DevId::Block, op,
// args)`; the TS block driver dispatches on `op`.
//
// READ and WRITE pass a 4104-byte buffer: bytes [0..8] hold the LBA
// as little-endian u64; bytes [8..4104] hold either the block being
// written (WRITE) or scratch space the driver fills with the read
// data (READ). The "TS writes back into a `&[u8]` argument buffer"
// pattern is consistent with how `pmos_host_driver_call` works for
// every other driver — `args_ptr` is a real wasm-memory address and
// the host can poke it.

/// Query the device's total block count. `args` is empty; the
/// driver returns the count via the `result_ptr` u32. Errors map
/// to `FsError::Io`.
pub const OP_BLOCK_COUNT: u32 = 0x01;
/// Read one block. `args` = [lba: u64 LE | scratch: [u8; 4096]];
/// the driver fills scratch with the block contents.
pub const OP_READ: u32 = 0x02;
/// Write one block. `args` = [lba: u64 LE | data: [u8; 4096]].
pub const OP_WRITE: u32 = 0x03;
/// Flush in-flight writes to the persistence backing store.
pub const OP_FLUSH: u32 = 0x04;

// --- WasmBlockDevice (browser-side OPFS) ------------------------------

#[cfg(target_arch = "wasm32")]
use crate::platform::{self, DevId, DriverError};
#[cfg(target_arch = "wasm32")]
use abi::errno::ENOSPC;
#[cfg(target_arch = "wasm32")]
use alloc::vec::Vec;

/// `BlockDevice` implementation that proxies every read/write/flush
/// through the host platform's `driver_call(DevId::Block, ...)`.
/// Active in the browser kernel where the TS-side `BlockDriver`
/// (`web/src/drivers/block.ts`) holds the `FileSystemSyncAccessHandle`
/// for `pmos.img` in the OPFS root.
///
/// `block_count` is captured at `open()` time via `OP_BLOCK_COUNT`
/// so subsequent reads of `block_count()` don't make a host
/// round-trip; the count is stable for the lifetime of the device
/// (the TS driver pre-sizes `pmos.img` to a fixed capacity at
/// init).
///
/// Only present on the `wasm32` target — the protocol shim has
/// no native correspondent; OPFS coverage on native runs through
/// `MockBlockDevice` (`crates/kernel/tests/opfs.rs`). Round-trip
/// verification of the wasm shim happens browser-side via the TS
/// `BlockDriver` tests (`web/tests/unit/block.test.ts`) against
/// the Vitest stub `FileSystemSyncAccessHandle`, and end-to-end in
/// the Playwright file-persistence flow (`real-kernel.spec.ts`).
#[cfg(target_arch = "wasm32")]
pub struct WasmBlockDevice {
    block_count: u64,
}

#[cfg(target_arch = "wasm32")]
impl WasmBlockDevice {
    /// Open the block device: query the TS driver for its block
    /// count and cache it.
    ///
    /// Returns `FsError::Io` if the host driver isn't ready, or if
    /// the reported block count is below the minimum required for a
    /// formatted OPFS image (the caller will then call `mkfs` only
    /// if it's at least the minimum, and bail out otherwise).
    pub fn open() -> Result<Self, FsError> {
        let count = match platform::current().driver_call(DevId::Block, OP_BLOCK_COUNT, &[]) {
            Ok(c) => c as u64,
            Err(_) => return Err(FsError::Io),
        };
        Ok(Self { block_count: count })
    }
}

#[cfg(target_arch = "wasm32")]
impl BlockDevice for WasmBlockDevice {
    fn read(&mut self, lba: Lba, out: &mut Block) -> Result<(), FsError> {
        let mut buf: Vec<u8> = alloc::vec![0u8; 8 + BLOCK_SIZE];
        buf[0..8].copy_from_slice(&lba.to_le_bytes());
        match platform::current().driver_call(DevId::Block, OP_READ, &buf) {
            Ok(_) => {
                out.copy_from_slice(&buf[8..]);
                Ok(())
            }
            Err(_) => Err(FsError::Io),
        }
    }

    fn write(&mut self, lba: Lba, data: &Block) -> Result<(), FsError> {
        let mut buf: Vec<u8> = alloc::vec::Vec::with_capacity(8 + BLOCK_SIZE);
        buf.extend_from_slice(&lba.to_le_bytes());
        buf.extend_from_slice(data);
        match platform::current().driver_call(DevId::Block, OP_WRITE, &buf) {
            Ok(_) => Ok(()),
            Err(DriverError::Errno(e)) if e == ENOSPC => Err(FsError::NoSpace),
            Err(_) => Err(FsError::Io),
        }
    }

    fn flush(&mut self) -> Result<(), FsError> {
        match platform::current().driver_call(DevId::Block, OP_FLUSH, &[]) {
            Ok(_) => Ok(()),
            Err(_) => Err(FsError::Io),
        }
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(blocks: u64) -> MockBlockDevice {
        MockBlockDevice::new(blocks)
    }

    #[test]
    fn read_unwritten_returns_zeros() {
        let mut dev = fresh(16);
        let mut buf = [0xAAu8; BLOCK_SIZE];
        dev.read(0, &mut buf).unwrap();
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn write_then_read_round_trip() {
        let mut dev = fresh(16);
        let mut a = [0u8; BLOCK_SIZE];
        for (i, b) in a.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        dev.write(5, &a).unwrap();
        let mut b = [0u8; BLOCK_SIZE];
        dev.read(5, &mut b).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn lba_out_of_range_errors() {
        let mut dev = fresh(4);
        let mut buf = [0u8; BLOCK_SIZE];
        assert_eq!(dev.read(4, &mut buf), Err(FsError::InvalidArgument));
        assert_eq!(dev.write(10, &buf), Err(FsError::NoSpace));
    }

    #[test]
    fn allocated_count_tracks_unique_writes() {
        let mut dev = fresh(16);
        let zero = [0u8; BLOCK_SIZE];
        dev.write(0, &zero).unwrap();
        dev.write(1, &zero).unwrap();
        dev.write(0, &zero).unwrap(); // overwrite, not a new block
        assert_eq!(dev.allocated_blocks(), 2);
    }

    #[test]
    fn flush_is_counted() {
        let mut dev = fresh(16);
        assert_eq!(dev.flush_count(), 0);
        dev.flush().unwrap();
        dev.flush().unwrap();
        assert_eq!(dev.flush_count(), 2);
    }

    #[test]
    fn crash_after_triggers_at_the_right_write() {
        let mut dev = fresh(16);
        let zero = [0u8; BLOCK_SIZE];

        dev.crash_after(2);
        dev.write(0, &zero).unwrap(); // write #1, ok
        dev.write(1, &zero).unwrap(); // write #2, ok
        let err = dev.write(2, &zero).unwrap_err(); // write #3, torn
        assert_eq!(err, FsError::Io);

        // After the torn write, subsequent writes succeed.
        dev.write(3, &zero).unwrap();
        assert_eq!(dev.total_writes(), 3); // the three successful ones
    }
}
