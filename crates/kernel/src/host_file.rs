//! Host-imported file table.
//!
//! Backs the `host_file_recv` extension opcode (`0x1500`,
//! `contracts/syscalls.md §3.6`). The bootstrap-side TS layer
//! detects user drag-drop / file-picker events on the browser tab,
//! stashes the host `File` object in a per-tab token table, and
//! posts a `host_file_dropped(token, name, size, mime)` notification
//! to the kernel. Userland (the file manager subscribed to
//! `/run/host-files`) calls `host_file_recv(token)` to obtain a
//! read-only fd whose reads stream the host file's bytes.
//!
//! In v1 the kernel-side test surface treats the bytes as already
//! resident — `Kernel::host_file_dropped` takes a `Vec<u8>` payload
//! directly. The TS-side bootstrap → kernel wiring (which would
//! stream chunks from a `File.stream()` reader rather than holding
//! the full bytes in kernel memory) is a future slice; today's
//! semantic-level tests pre-load the bytes into the table the same
//! way a future chunked reader would back-fill them.
//!
//! ## Token lifecycle
//!
//! 1. `host_file_dropped(token, file)` registers a fresh
//!    `(token → HostFile)` entry. The bootstrap is responsible for
//!    picking unique tokens; collisions overwrite the prior entry
//!    (the spec leaves collision policy to the bootstrap, since
//!    only the bootstrap mints tokens).
//! 2. `host_file_recv(pid, token)` consumes the entry from the
//!    table, allocates an `FdObject::HostFile { token }` in the
//!    caller's fd table, and stashes the bytes on the kernel-side
//!    fd-state map keyed by the token. Per the spec: "Exactly one
//!    `host_file_recv` call is permitted per token; a second call
//!    with the same token returns `EBADF`." The handler enforces
//!    this by removing the table entry on the first successful
//!    recv — a second recv finds nothing and returns `BadFd`.
//! 3. `fd_read` on the resulting fd advances an internal offset
//!    over the stashed bytes; reads past EOF return 0.
//! 4. `fd_close` drops the kernel-side bytes via the `release_object`
//!    arm. This satisfies the spec's "Closing the fd releases the
//!    browser-side `File` reference" — the kernel-side bytes are
//!    a stand-in for that reference today.
//!
//! ## Why it's not a "queue"
//!
//! An earlier slice draft modelled `host_file_recv` as a per-pid
//! queue with `EAGAIN`-on-empty semantics. The §3.6 spec is
//! token-based instead: the bootstrap mints a unique token per
//! drop event and posts it to the file-manager process via the
//! `/run/host-files` IPC notification, so the file manager always
//! knows a specific token to dequeue rather than polling a queue.
//! The token-table design also lets multiple files coexist (one
//! per token) without a per-pid queue ordering invariant.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// A single host-imported file. Owns the bytes the kernel will
/// stream to userland via `fd_read`. `name` and `mime` are
/// metadata reported by the bootstrap; v1 doesn't expose them
/// over the syscall surface (a future slice can add a
/// `host_file_stat(token)` opcode), but the kernel keeps them
/// around so a future `fd_filestat_get` extension can return
/// the original filename via a side channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFile {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

impl HostFile {
    /// Construct a host-file payload. Defensive constructor that
    /// keeps the field shape stable as future slices add metadata
    /// (mtime, source URL, etc.).
    pub fn new(name: impl Into<String>, mime: impl Into<String>, bytes: Vec<u8>) -> Self {
        HostFile {
            name: name.into(),
            mime: mime.into(),
            bytes,
        }
    }

    /// Total byte length of the file. Returned in `st_size` by
    /// `fd_filestat_get` on a HostFile fd.
    pub fn size(&self) -> u64 {
        self.bytes.len() as u64
    }
}

/// Per-fd streaming state for a HostFile fd.
///
/// Each `host_file_recv` allocates one of these and stashes it in
/// `Kernel::host_file_fds` keyed by the consumed token. `fd_read`
/// looks up the entry, copies bytes from `bytes[offset..]` into
/// the caller's buffer, and advances `offset`. `fd_close` drops
/// the entry, releasing the kernel-side bytes.
///
/// `offset` is per-fd, not per-token; if a future slice ever
/// allows multiple fds for the same token (the current spec
/// forbids it via the second-recv-EBADF rule), each fd would
/// need its own offset slot — the per-token keying here keeps
/// that future option open without a layout change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFileFd {
    pub file: HostFile,
    pub offset: u64,
}

impl HostFileFd {
    pub fn new(file: HostFile) -> Self {
        HostFileFd { file, offset: 0 }
    }

    /// Stream up to `buf.len()` bytes from `bytes[offset..]` into
    /// `buf`, advancing `offset` by the copied count. Returns the
    /// number of bytes copied. A read at or past EOF returns 0
    /// (POSIX-shaped end-of-file).
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let total = self.file.bytes.len() as u64;
        if self.offset >= total {
            return 0;
        }
        let remaining = (total - self.offset) as usize;
        let n = remaining.min(buf.len());
        let off = self.offset as usize;
        buf[..n].copy_from_slice(&self.file.bytes[off..off + n]);
        self.offset += n as u64;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_streams_all_bytes_then_eof() {
        let file = HostFile::new("a.txt", "text/plain", b"hello".to_vec());
        let mut fd = HostFileFd::new(file);
        let mut buf = [0u8; 8];
        let n = fd.read(&mut buf);
        assert_eq!(n, 5);
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(fd.offset, 5);
        // Second read at EOF returns 0 without touching the buffer.
        let n2 = fd.read(&mut buf);
        assert_eq!(n2, 0);
    }

    #[test]
    fn read_chunks_advances_offset_per_call() {
        let file = HostFile::new("a.bin", "application/octet-stream", vec![1, 2, 3, 4, 5]);
        let mut fd = HostFileFd::new(file);
        let mut buf = [0u8; 2];
        assert_eq!(fd.read(&mut buf), 2);
        assert_eq!(buf, [1, 2]);
        assert_eq!(fd.read(&mut buf), 2);
        assert_eq!(buf, [3, 4]);
        assert_eq!(fd.read(&mut buf), 1);
        assert_eq!(buf[0], 5);
        assert_eq!(fd.read(&mut buf), 0);
    }

    #[test]
    fn size_reports_byte_length() {
        let file = HostFile::new("a", "x", vec![0u8; 1024]);
        assert_eq!(file.size(), 1024);
    }
}
