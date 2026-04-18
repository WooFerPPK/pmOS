//! procfs — synthetic process-introspection filesystem.
//!
//! **Framework only in this slice.** The procfs exposed here has
//! a working directory skeleton — `/proc/version`,
//! `/proc/uptime`, `/proc/meminfo`, `/proc/loadavg`,
//! `/proc/storage` — and returns canned or self-reported values
//! for each. The per-process `/proc/<pid>/` trees (which need a
//! live borrow of the kernel's process table) are wired in at
//! kernel-integration time: T067 integrates procfs with the
//! actual process table via a trait object so procfs stays
//! testable in isolation.
//!
//! Data sources are injected through the [`ProcFsSource`]
//! trait. The v1 slice provides a [`StaticProcFsSource`] that
//! returns canned values — it's what the kernel VFS tests use.
//! The kernel's real boot path will substitute a
//! `KernelProcFsSource` that reads from the process table,
//! memory accounting, and block driver quota.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::platform;
use crate::vfs::{DirEntry, FileStat, Filesystem, FsError, Ino, Mode, NodeType};

/// Per-file snapshot as a plain string that procfs serves on `read`.
pub trait ProcFsSource: Send + Sync {
    fn version(&self) -> String;
    fn uptime(&self) -> String;
    fn meminfo(&self) -> String;
    fn loadavg(&self) -> String;
    fn storage(&self) -> String;
}

/// Canned values — used in tests and as the default before the
/// kernel's real data source is installed.
pub struct StaticProcFsSource {
    pub version_line: String,
    pub uptime_line: String,
    pub meminfo_line: String,
    pub loadavg_line: String,
    pub storage_line: String,
}

impl Default for StaticProcFsSource {
    fn default() -> Self {
        StaticProcFsSource {
            version_line: String::from("PMos 0.1.0 (native-test)\n"),
            uptime_line: String::from("0 0\n"),
            meminfo_line: String::from("0 0 0\n"),
            loadavg_line: String::from("0.00 0.00 0.00 0/0 0\n"),
            storage_line: String::from("0 0 0\n"),
        }
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
}

// Inode layout — ino 1 is the root directory; each top-level file
// has a fixed ino so tests can assert on them.
const INO_ROOT:    Ino = 1;
const INO_VERSION: Ino = 2;
const INO_UPTIME:  Ino = 3;
const INO_MEMINFO: Ino = 4;
const INO_LOADAVG: Ino = 5;
const INO_STORAGE: Ino = 6;

pub struct ProcFs {
    source: Box<dyn ProcFsSource>,
    /// File inode → canned-name lookup. Used by readdir and
    /// `stat` to know what kind of node we're talking about.
    entries: BTreeMap<Ino, &'static str>,
    /// Name → inode lookup for `lookup(dir, name)`.
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

    fn contents_for(&self, ino: Ino) -> Option<String> {
        match ino {
            INO_VERSION => Some(self.source.version()),
            INO_UPTIME => Some(self.source.uptime()),
            INO_MEMINFO => Some(self.source.meminfo()),
            INO_LOADAVG => Some(self.source.loadavg()),
            INO_STORAGE => Some(self.source.storage()),
            _ => None,
        }
    }
}

impl Filesystem for ProcFs {
    fn root(&self) -> Ino {
        INO_ROOT
    }

    fn lookup(&mut self, dir: Ino, name: &str) -> Result<Ino, FsError> {
        if dir != INO_ROOT {
            return Err(FsError::NotADirectory);
        }
        self.name_to_ino
            .get(name)
            .copied()
            .ok_or(FsError::NotFound)
    }

    fn read(&mut self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        if ino == INO_ROOT {
            return Err(FsError::IsADirectory);
        }
        let content = self.contents_for(ino).ok_or(FsError::NotFound)?;
        let bytes = content.as_bytes();
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
        if dir != INO_ROOT {
            return Err(FsError::NotADirectory);
        }
        for (name, ino) in &self.name_to_ino {
            out.push(DirEntry {
                name: (*name).to_string(),
                ino: *ino,
                ty: NodeType::RegularFile,
            });
        }
        Ok(())
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
        if !self.entries.contains_key(&ino) {
            return Err(FsError::NotFound);
        }
        let content = self.contents_for(ino).ok_or(FsError::NotFound)?;
        Ok(FileStat {
            ino,
            ty: NodeType::RegularFile,
            mode: 0o444,
            nlink: 1,
            size: content.len() as u64,
            atime_ns: now,
            mtime_ns: now,
            ctime_ns: now,
        })
    }

    fn truncate(&mut self, _ino: Ino, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    fn kind_name(&self) -> &'static str {
        "procfs"
    }
}
