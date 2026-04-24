//! procfs — synthetic process-introspection filesystem.
//!
//! Top-level directory skeleton — `/proc/version`, `/proc/uptime`,
//! `/proc/meminfo`, `/proc/loadavg`, `/proc/storage` — plus a
//! per-pid subtree under `/proc/<pid>/` that surfaces live
//! process-table fields. In this slice only `/proc/<pid>/status`
//! is populated (with Name / State / Pid / PPid); follow-up
//! slices add `cmdline`, `stat`, `maps`, etc. alongside the
//! memory-tracking fields (`VmSize`, `VmPeak`) once the process
//! table owns that accounting.
//!
//! Data sources are injected through the [`ProcFsSource`] trait.
//! The v1 slice provides a [`StaticProcFsSource`] that returns
//! canned values and an explicitly-populated per-pid status map —
//! it's what the kernel VFS tests use. A kernel boot path bridge
//! (a `KernelProcFsSource` reading the live `ProcessTable`) is
//! a follow-up; the trait shape here is what that bridge will
//! implement.
//!
//! # Inode layout
//!
//! * `1` — `/proc` root directory.
//! * `2..=6` — the fixed top-level files (version, uptime,
//!   meminfo, loadavg, storage).
//! * `PID_DIR_BASE + pid * PID_STRIDE + offset` — per-pid nodes.
//!   Stride is chosen large enough to admit future fields
//!   (`cmdline`, `stat`, `maps`, ...) without renumbering. A
//!   `pid_of(ino)` helper rounds back to the pid; an out-of-range
//!   offset yields `FsError::NotFound`.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use abi::ext::Pid;

use crate::platform;
use crate::vfs::{DirEntry, FileStat, Filesystem, FsError, Ino, Mode, NodeType};

/// Per-file snapshot as a plain string that procfs serves on `read`.
///
/// Top-level files return their content as a `String`. Per-pid
/// fields return structured snapshots that procfs formats into
/// `Key: Value` bytes — keeping the formatter inside `ProcFs`
/// means every backend produces byte-identical output.
pub trait ProcFsSource: Send + Sync {
    fn version(&self) -> String;
    fn uptime(&self) -> String;
    fn meminfo(&self) -> String;
    fn loadavg(&self) -> String;
    fn storage(&self) -> String;

    /// Return the process-table snapshot for `pid`, or `None` if
    /// no such live pid exists.
    ///
    /// Default: no live pids. Tests and the future
    /// `KernelProcFsSource` override this.
    fn pid_status(&self, _pid: Pid) -> Option<ProcStatusSnapshot> {
        None
    }

    /// Ascending list of live pids used by readdir on `/proc`.
    ///
    /// Default: empty. Overriding this + `pid_status` together is
    /// what makes per-pid procfs entries visible.
    fn live_pids(&self) -> Vec<Pid> {
        Vec::new()
    }
}

/// Canned values — used in tests and as the default before the
/// kernel's real data source is installed.
pub struct StaticProcFsSource {
    pub version_line: String,
    pub uptime_line: String,
    pub meminfo_line: String,
    pub loadavg_line: String,
    pub storage_line: String,
    pub pid_statuses: BTreeMap<Pid, ProcStatusSnapshot>,
}

impl Default for StaticProcFsSource {
    fn default() -> Self {
        StaticProcFsSource {
            version_line: String::from("PMos 0.1.0 (native-test)\n"),
            uptime_line: String::from("0 0\n"),
            meminfo_line: String::from("0 0 0\n"),
            loadavg_line: String::from("0.00 0.00 0.00 0/0 0\n"),
            storage_line: String::from("0 0 0\n"),
            pid_statuses: BTreeMap::new(),
        }
    }
}

impl StaticProcFsSource {
    /// Register or replace the snapshot for `pid`. Returns `self`
    /// by `&mut` so tests can chain several inserts on a builder
    /// expression.
    pub fn set_pid_status(&mut self, snap: ProcStatusSnapshot) -> &mut Self {
        self.pid_statuses.insert(snap.pid, snap);
        self
    }
}

impl ProcFsSource for StaticProcFsSource {
    fn version(&self) -> String {
        self.version_line.clone()
    }
    fn uptime(&self) -> String {
        self.uptime_line.clone()
    }
    fn meminfo(&self) -> String {
        self.meminfo_line.clone()
    }
    fn loadavg(&self) -> String {
        self.loadavg_line.clone()
    }
    fn storage(&self) -> String {
        self.storage_line.clone()
    }
    fn pid_status(&self, pid: Pid) -> Option<ProcStatusSnapshot> {
        self.pid_statuses.get(&pid).cloned()
    }
    fn live_pids(&self) -> Vec<Pid> {
        self.pid_statuses.keys().copied().collect()
    }
}

/// Letter-coded process state that `/proc/<pid>/status` serves.
///
/// Mirrors the Linux `/proc/<pid>/status` convention: one letter
/// plus a parenthesised human-readable name. Maps from the
/// kernel's [`crate::proc::ProcState`] variants at snapshot time.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProcStatusState {
    /// Currently Running (scheduler has the cpu on it).
    Running,
    /// Any Blocked-on-* variant or Ready/Starting — i.e. "alive
    /// and waiting for an event", which is all one bucket at
    /// `/proc` resolution.
    Sleeping,
    /// Exited but not yet reaped.
    Zombie,
    /// Stopped (e.g. by SIGSTOP). Reserved for future slices —
    /// v1 has no SIGSTOP delivery, so the kernel never hands us
    /// this at snapshot time. Present so the formatter is total.
    Stopped,
}

impl ProcStatusState {
    /// Linux-style one-letter code.
    #[inline]
    pub const fn letter(self) -> char {
        match self {
            ProcStatusState::Running => 'R',
            ProcStatusState::Sleeping => 'S',
            ProcStatusState::Zombie => 'Z',
            ProcStatusState::Stopped => 'T',
        }
    }

    /// Human-readable name that follows the letter in parentheses.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            ProcStatusState::Running => "running",
            ProcStatusState::Sleeping => "sleeping",
            ProcStatusState::Zombie => "zombie",
            ProcStatusState::Stopped => "stopped",
        }
    }
}

/// A snapshot of one process's `/proc/<pid>/status` fields.
///
/// Owned values (not borrows) so that an emit-time read can
/// release the source borrow before formatting; simplifies the
/// kernel-bridge backend where the source borrow is the
/// `ProcessTable` itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcStatusSnapshot {
    pub pid: Pid,
    pub ppid: Pid,
    pub name: String,
    pub state: ProcStatusState,
}

// Inode layout — ino 1 is the root directory; each top-level file
// has a fixed ino so tests can assert on them.
const INO_ROOT:    Ino = 1;
const INO_VERSION: Ino = 2;
const INO_UPTIME:  Ino = 3;
const INO_MEMINFO: Ino = 4;
const INO_LOADAVG: Ino = 5;
const INO_STORAGE: Ino = 6;

/// Start of the per-pid inode range. Chosen well above the fixed
/// top-level inos so a stray `2..=6` never aliases a per-pid ino.
const PID_DIR_BASE: Ino = 1_000_000;
/// Bytes reserved per pid. Slot 0 is the pid's directory itself;
/// slot 1 is `status`; further slots are for future files.
const PID_STRIDE: Ino = 16;
/// Offset within a pid's stride for the `status` file.
const PID_OFFSET_STATUS: Ino = 1;

/// Map a pid to its directory ino (slot 0 within its stride).
#[inline]
const fn pid_dir_ino(pid: Pid) -> Ino {
    PID_DIR_BASE + (pid as u64) * PID_STRIDE
}

/// Map a pid to its `status` file ino.
#[inline]
const fn pid_status_ino(pid: Pid) -> Ino {
    pid_dir_ino(pid) + PID_OFFSET_STATUS
}

/// Inverse of `pid_dir_ino` + `pid_status_ino`. Returns the pid
/// and the slot within its stride, or `None` if `ino` is not in
/// the per-pid range.
fn decode_pid_ino(ino: Ino) -> Option<(Pid, Ino)> {
    if ino < PID_DIR_BASE {
        return None;
    }
    let rel = ino - PID_DIR_BASE;
    let pid = (rel / PID_STRIDE) as Pid;
    let slot = rel % PID_STRIDE;
    Some((pid, slot))
}

/// Serialise a [`ProcStatusSnapshot`] into the exact byte layout
/// `/proc/<pid>/status` serves — one `Key:\tValue\n` per line, in
/// a fixed order, with a trailing newline. Callers receive a
/// `Vec<u8>` sized for the snapshot (no heap allocation beyond
/// the one `format!` that backs it).
fn format_pid_status(snap: &ProcStatusSnapshot) -> Vec<u8> {
    let text = format!(
        "Name:\t{}\nState:\t{} ({})\nPid:\t{}\nPPid:\t{}\n",
        snap.name,
        snap.state.letter(),
        snap.state.name(),
        snap.pid,
        snap.ppid,
    );
    text.into_bytes()
}

pub struct ProcFs {
    source: Box<dyn ProcFsSource>,
    /// File inode → canned-name lookup for top-level files. Used
    /// by readdir and stat; the per-pid subtree is resolved
    /// dynamically against `source.live_pids()` + `source.pid_status(pid)`.
    entries: BTreeMap<Ino, &'static str>,
    /// Name → inode lookup for top-level `lookup(dir, name)`.
    name_to_ino: BTreeMap<&'static str, Ino>,
}

impl ProcFs {
    pub fn new(source: Box<dyn ProcFsSource>) -> Self {
        let pairs: &[(Ino, &'static str)] = &[
            (INO_VERSION, "version"),
            (INO_UPTIME, "uptime"),
            (INO_MEMINFO, "meminfo"),
            (INO_LOADAVG, "loadavg"),
            (INO_STORAGE, "storage"),
        ];
        let mut entries = BTreeMap::new();
        let mut name_to_ino = BTreeMap::new();
        for (ino, name) in pairs {
            entries.insert(*ino, *name);
            name_to_ino.insert(*name, *ino);
        }
        ProcFs {
            source,
            entries,
            name_to_ino,
        }
    }

    /// Convenience constructor with canned test values.
    pub fn with_static() -> Self {
        ProcFs::new(Box::new(StaticProcFsSource::default()))
    }

    fn contents_for(&self, ino: Ino) -> Option<Vec<u8>> {
        // Top-level canned files.
        let static_text: Option<String> = match ino {
            INO_VERSION => Some(self.source.version()),
            INO_UPTIME => Some(self.source.uptime()),
            INO_MEMINFO => Some(self.source.meminfo()),
            INO_LOADAVG => Some(self.source.loadavg()),
            INO_STORAGE => Some(self.source.storage()),
            _ => None,
        };
        if let Some(s) = static_text {
            return Some(s.into_bytes());
        }
        // Per-pid files.
        let (pid, slot) = decode_pid_ino(ino)?;
        if slot == PID_OFFSET_STATUS {
            let snap = self.source.pid_status(pid)?;
            return Some(format_pid_status(&snap));
        }
        None
    }

    /// True iff `ino` is a per-pid directory for a currently-live pid.
    fn is_live_pid_dir(&self, ino: Ino) -> bool {
        let Some((pid, slot)) = decode_pid_ino(ino) else {
            return false;
        };
        slot == 0 && self.source.pid_status(pid).is_some()
    }
}

impl Filesystem for ProcFs {
    fn root(&self) -> Ino {
        INO_ROOT
    }

    fn lookup(&mut self, dir: Ino, name: &str) -> Result<Ino, FsError> {
        if dir == INO_ROOT {
            // Top-level: either a canned name or a numeric pid.
            if let Some(ino) = self.name_to_ino.get(name).copied() {
                return Ok(ino);
            }
            if let Ok(pid) = name.parse::<Pid>() {
                if self.source.pid_status(pid).is_some() {
                    return Ok(pid_dir_ino(pid));
                }
            }
            return Err(FsError::NotFound);
        }
        // Per-pid directory: only `status` is served in this slice.
        if self.is_live_pid_dir(dir) {
            let (pid, _) = decode_pid_ino(dir).expect("is_live_pid_dir implies decode ok");
            return match name {
                "status" => Ok(pid_status_ino(pid)),
                _ => Err(FsError::NotFound),
            };
        }
        Err(FsError::NotADirectory)
    }

    fn read(&mut self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        if ino == INO_ROOT {
            return Err(FsError::IsADirectory);
        }
        if self.is_live_pid_dir(ino) {
            return Err(FsError::IsADirectory);
        }
        let bytes = self.contents_for(ino).ok_or(FsError::NotFound)?;
        let start = offset as usize;
        if start >= bytes.len() {
            return Ok(0);
        }
        let end = core::cmp::min(bytes.len(), start + buf.len());
        let n = end - start;
        buf[..n].copy_from_slice(&bytes[start..end]);
        Ok(n)
    }

    fn write(&mut self, _ino: Ino, _offset: u64, _buf: &[u8]) -> Result<usize, FsError> {
        Err(FsError::ReadOnly)
    }

    fn readdir(&mut self, dir: Ino, out: &mut Vec<DirEntry>) -> Result<(), FsError> {
        if dir == INO_ROOT {
            for (name, ino) in &self.name_to_ino {
                out.push(DirEntry {
                    name: (*name).to_string(),
                    ino: *ino,
                    ty: NodeType::RegularFile,
                });
            }
            for pid in self.source.live_pids() {
                out.push(DirEntry {
                    name: pid.to_string(),
                    ino: pid_dir_ino(pid),
                    ty: NodeType::Directory,
                });
            }
            return Ok(());
        }
        if self.is_live_pid_dir(dir) {
            let (pid, _) = decode_pid_ino(dir).expect("is_live_pid_dir implies decode ok");
            out.push(DirEntry {
                name: "status".to_string(),
                ino: pid_status_ino(pid),
                ty: NodeType::RegularFile,
            });
            return Ok(());
        }
        Err(FsError::NotADirectory)
    }

    fn create(&mut self, _dir: Ino, _name: &str, _mode: Mode) -> Result<Ino, FsError> {
        Err(FsError::ReadOnly)
    }

    fn mkdir(&mut self, _dir: Ino, _name: &str, _mode: Mode) -> Result<Ino, FsError> {
        Err(FsError::ReadOnly)
    }

    fn unlink(&mut self, _dir: Ino, _name: &str) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn rmdir(&mut self, _dir: Ino, _name: &str) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn rename(
        &mut self,
        _from_dir: Ino,
        _from_name: &str,
        _to_dir: Ino,
        _to_name: &str,
    ) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn stat(&mut self, ino: Ino) -> Result<FileStat, FsError> {
        // Procfs content is synthesised per-call — the "file" is
        // generated fresh from the ProcFsSource each read — so the
        // semantic truth for every timestamp is "now". Per-call
        // evaluation means successive stats on the same ino may
        // report different values; that's consistent with how the
        // content itself can change across calls (uptime, loadavg,
        // meminfo all update every tick).
        let now = platform::current().now_realtime_ns();
        if ino == INO_ROOT {
            return Ok(FileStat {
                ino: INO_ROOT,
                ty: NodeType::Directory,
                mode: 0o555,
                nlink: 1,
                size: 0,
                atime_ns: now,
                mtime_ns: now,
                ctime_ns: now,
            });
        }
        if self.is_live_pid_dir(ino) {
            return Ok(FileStat {
                ino,
                ty: NodeType::Directory,
                mode: 0o555,
                nlink: 1,
                size: 0,
                atime_ns: now,
                mtime_ns: now,
                ctime_ns: now,
            });
        }
        if self.entries.contains_key(&ino) {
            let content = self.contents_for(ino).ok_or(FsError::NotFound)?;
            return Ok(FileStat {
                ino,
                ty: NodeType::RegularFile,
                mode: 0o444,
                nlink: 1,
                size: content.len() as u64,
                atime_ns: now,
                mtime_ns: now,
                ctime_ns: now,
            });
        }
        // Per-pid regular files: recognise the stride offset.
        if let Some((_pid, slot)) = decode_pid_ino(ino) {
            if slot == PID_OFFSET_STATUS {
                let content = self.contents_for(ino).ok_or(FsError::NotFound)?;
                return Ok(FileStat {
                    ino,
                    ty: NodeType::RegularFile,
                    mode: 0o444,
                    nlink: 1,
                    size: content.len() as u64,
                    atime_ns: now,
                    mtime_ns: now,
                    ctime_ns: now,
                });
            }
        }
        Err(FsError::NotFound)
    }

    fn truncate(&mut self, _ino: Ino, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn kind_name(&self) -> &'static str {
        "procfs"
    }
}
