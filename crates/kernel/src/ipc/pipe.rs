//! Kernel pipes.
//!
//! Each pipe is a kernel-owned ring buffer with two endpoints:
//! a **read** side and a **write** side. Both sides are
//! reference-counted — the kernel's fd table holds a handle to
//! either endpoint, and when the last handle to one side drops,
//! the pipe transitions to a terminal state:
//!
//! * Last **writer** closed: subsequent reads drain the buffer
//!   and then return 0 (EOF).
//! * Last **reader** closed: subsequent writes return
//!   [`FsError::PipeBroken`] (conceptually SIGPIPE; the kernel
//!   translates this into the right errno at syscall-dispatch
//!   time).
//!
//! Non-blocking mode: the high-level syscall dispatcher checks
//! the fd's `FdFlags::NONBLOCK` and converts a would-block into
//! `FsError::WouldBlock`. This module exposes the distinction via
//! [`PipeReadResult`] and [`PipeWriteResult`] — the kernel
//! translates those into syscall returns.
//!
//! The buffer is a fixed-size `[u8; PIPE_BUF_CAP]`. Default cap
//! is 64 KiB, matching the plan's `data-model.md §4.1`.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use abi::ext::Pid;

use super::IpcError;

/// Default pipe buffer capacity.
pub const PIPE_BUF_CAP: usize = 64 * 1024;

/// Per-pipe identifier handed out by the kernel's IPC table.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PipeId(pub u32);

/// Result of a non-blocking read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PipeReadResult {
    /// `n` bytes were read into the caller's buffer.
    Read(usize),
    /// Read would block: no bytes available, but the writer side
    /// is still open.
    WouldBlock,
    /// Writer side is closed and the buffer is drained.
    Eof,
}

/// Result of a non-blocking write.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PipeWriteResult {
    /// `n` bytes were written from the caller's buffer.
    Wrote(usize),
    /// Write would block: buffer full, but the reader side is
    /// still open.
    WouldBlock,
    /// Reader side is closed. The kernel should convert this
    /// into SIGPIPE / EPIPE at syscall-dispatch time.
    Broken,
}

/// A single kernel pipe.
///
/// Ownership: the [`super::IpcTable`] owns `Pipe` values by
/// `PipeId`. The per-process fd table holds `PipeId` handles
/// + a `ReaderOrWriter` tag. When the process closes an fd
/// referring to a pipe end, the ipc table decrements the
/// matching `reader_refcount` / `writer_refcount`; when the
/// count reaches zero the end is marked closed.
pub struct Pipe {
    pub id: PipeId,
    buffer: VecDeque<u8>,
    cap: usize,
    reader_refcount: u32,
    writer_refcount: u32,
    /// Pids currently parked waiting for data (reader side).
    /// The scheduler wakes them when data arrives or the
    /// writer side closes.
    waiting_readers: Vec<Pid>,
    /// Pids currently parked waiting for space (writer side).
    waiting_writers: Vec<Pid>,
}

impl Pipe {
    /// Create a brand-new pipe with one reader and one writer
    /// reference.
    pub fn new(id: PipeId) -> Self {
        Pipe {
            id,
            buffer: VecDeque::new(),
            cap: PIPE_BUF_CAP,
            reader_refcount: 1,
            writer_refcount: 1,
            waiting_readers: Vec::new(),
            waiting_writers: Vec::new(),
        }
    }

    /// Current number of buffered bytes.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn reader_count(&self) -> u32 {
        self.reader_refcount
    }

    pub fn writer_count(&self) -> u32 {
        self.writer_refcount
    }

    /// Reader is closed iff no reader references remain.
    pub fn reader_closed(&self) -> bool {
        self.reader_refcount == 0
    }

    /// Writer is closed iff no writer references remain.
    pub fn writer_closed(&self) -> bool {
        self.writer_refcount == 0
    }

    /// True iff the pipe is fully dead — both ends closed AND
    /// the buffer has been drained. The IPC table drops fully-
    /// dead pipes from its map.
    pub fn is_dead(&self) -> bool {
        self.reader_closed() && self.writer_closed() && self.buffer.is_empty()
    }

    /// Dup a reader reference (for [`dup`]-style fd copying or
    /// proc_spawn inheritance).
    pub fn dup_reader(&mut self) {
        self.reader_refcount = self.reader_refcount.saturating_add(1);
    }

    /// Dup a writer reference.
    pub fn dup_writer(&mut self) {
        self.writer_refcount = self.writer_refcount.saturating_add(1);
    }

    /// Drop one reader reference. Returns any pids that should
    /// be woken (i.e., writers parked on a full buffer) —
    /// because the pipe is now reader-closed, those writers
    /// will immediately see `Broken` on retry and the scheduler
    /// needs to unblock them so they get the error.
    pub fn drop_reader(&mut self) -> Vec<Pid> {
        self.reader_refcount = self.reader_refcount.saturating_sub(1);
        if self.reader_refcount == 0 {
            core::mem::take(&mut self.waiting_writers)
        } else {
            Vec::new()
        }
    }

    /// Drop one writer reference. Returns any pids that should
    /// be woken (readers parked on an empty buffer) so they
    /// observe EOF.
    pub fn drop_writer(&mut self) -> Vec<Pid> {
        self.writer_refcount = self.writer_refcount.saturating_sub(1);
        if self.writer_refcount == 0 {
            core::mem::take(&mut self.waiting_readers)
        } else {
            Vec::new()
        }
    }

    /// Non-blocking read into `buf`. Returns how many bytes
    /// were copied + the state of the pipe at call end.
    pub fn try_read(&mut self, buf: &mut [u8]) -> PipeReadResult {
        if self.buffer.is_empty() {
            if self.writer_closed() {
                return PipeReadResult::Eof;
            }
            return PipeReadResult::WouldBlock;
        }
        let take = core::cmp::min(buf.len(), self.buffer.len());
        for i in 0..take {
            // VecDeque::pop_front is O(1) amortised.
            buf[i] = self.buffer.pop_front().unwrap();
        }
        PipeReadResult::Read(take)
    }

    /// Non-blocking write of `buf`. Returns how many bytes were
    /// copied + the state of the pipe at call end.
    pub fn try_write(&mut self, buf: &[u8]) -> PipeWriteResult {
        if self.reader_closed() {
            return PipeWriteResult::Broken;
        }
        if buf.is_empty() {
            return PipeWriteResult::Wrote(0);
        }
        let free = self.cap.saturating_sub(self.buffer.len());
        if free == 0 {
            return PipeWriteResult::WouldBlock;
        }
        let take = core::cmp::min(free, buf.len());
        for &b in &buf[..take] {
            self.buffer.push_back(b);
        }
        PipeWriteResult::Wrote(take)
    }

    /// Park `pid` on the reader-wait list. The scheduler uses
    /// this when a process calls `read` on an empty pipe and
    /// the fd is blocking.
    pub fn park_reader(&mut self, pid: Pid) {
        self.waiting_readers.push(pid);
    }

    /// Park `pid` on the writer-wait list.
    pub fn park_writer(&mut self, pid: Pid) {
        self.waiting_writers.push(pid);
    }

    /// Drain and return any pids waiting for data. Called by
    /// the caller of `try_write` when at least one byte was
    /// written — those pids might be able to make progress.
    pub fn drain_waiting_readers(&mut self) -> Vec<Pid> {
        core::mem::take(&mut self.waiting_readers)
    }

    /// Drain and return any pids waiting for space. Called by
    /// the caller of `try_read` when at least one byte was
    /// read.
    pub fn drain_waiting_writers(&mut self) -> Vec<Pid> {
        core::mem::take(&mut self.waiting_writers)
    }
}

// --- Pipe allocator (owned by the IPC table) --------------------

/// Next-free-id allocator for pipe identifiers. Lives on the
/// [`super::IpcTable`]; the free list for v1 is just a
/// monotonic counter.
#[derive(Debug)]
pub struct PipeIdAllocator {
    next: u32,
}

impl PipeIdAllocator {
    pub const fn new() -> Self {
        PipeIdAllocator { next: 1 }
    }

    pub fn allocate(&mut self) -> PipeId {
        let id = PipeId(self.next);
        self.next = self.next.checked_add(1).expect("pipe id overflow");
        id
    }
}

impl Default for PipeIdAllocator {
    fn default() -> Self {
        PipeIdAllocator::new()
    }
}

// --- Convenience: read all at once -------------------------------

/// A convenience wrapper around [`Pipe::try_read`] that reads up
/// to `max` bytes into a `Vec<u8>`, allocating as needed. Used
/// by the fd-level syscall dispatcher when the user passes a
/// buffer larger than whatever's available in the pipe.
pub fn try_read_vec(pipe: &mut Pipe, max: usize) -> Result<(Vec<u8>, PipeReadResult), IpcError> {
    let mut scratch = alloc::vec![0u8; core::cmp::min(max, pipe.len())];
    let res = pipe.try_read(&mut scratch);
    if let PipeReadResult::Read(n) = res {
        scratch.truncate(n);
        Ok((scratch, res))
    } else {
        Ok((Vec::new(), res))
    }
}
