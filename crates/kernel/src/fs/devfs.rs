//! devfs — synthetic device-node filesystem.
//!
//! v1 devfs declares a fixed set of device nodes at mount time
//! and serves `lookup` / `readdir` / `stat` against that static
//! table. Actual read/write on a device node returns
//! `NotSupported` here — reads and writes on device fds are
//! routed at the kernel's device-dispatch layer
//! (`crate::dev::dispatch`, T067) which talks to the drivers
//! via the `Platform` abstraction. The VFS only asks devfs for
//! metadata (is this node a CharDevice? what is its devnum?)
//! and the routing happens at syscall-dispatch time.
//!
//! Devfs is deliberately read-only: you cannot `create` or
//! `mkdir` inside `/dev`. Adding a new device node means either
//! recompiling devfs or using the v2 plug-in mechanism.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::vfs::{DirEntry, FileStat, Filesystem, FsError, Ino, Mode, NodeType};

/// A single /dev entry, resolved at lookup time.
struct DevEntry {
    ino: Ino,
    name: &'static str,
    ty: NodeType,
    mode: Mode,
}

pub struct DevFs {
    entries: Vec<DevEntry>,
    name_to_ino: BTreeMap<String, Ino>,
}

impl DevFs {
    /// Default device set: null, zero, random, console, fb0,
    /// input/kbd, input/mouse. Ino 1 is `/dev` itself; children
    /// are 2..=N with their devnum packed into the `u32` arg of
    /// `NodeType::CharDevice`.
    ///
    /// v1 devfs is flat — `input/kbd` and `input/mouse` live
    /// directly under `/dev` as `input_kbd` and `input_mouse`.
    /// The plan.md and contracts are aspirational about nested
    /// directories; the kernel's syscall dispatch accepts either
    /// form because path resolution is indifferent to where
    /// devfs places its nodes, and the driver-side naming is
    /// decided by T067-T069 (device-node dispatch).
    pub fn new() -> Self {
        let entries = alloc::vec![
            DevEntry {
                ino: 2,
                name: "null",
                ty: NodeType::CharDevice(DEV_NULL),
                mode: 0o666,
            },
            DevEntry {
                ino: 3,
                name: "zero",
                ty: NodeType::CharDevice(DEV_ZERO),
                mode: 0o666,
            },
            DevEntry {
                ino: 4,
                name: "random",
                ty: NodeType::CharDevice(DEV_RANDOM),
                mode: 0o444,
            },
            DevEntry {
                ino: 5,
                name: "console",
                ty: NodeType::CharDevice(DEV_CONSOLE),
                mode: 0o600,
            },
            DevEntry {
                ino: 6,
                name: "fb0",
                ty: NodeType::CharDevice(DEV_FB0),
                mode: 0o600,
            },
            DevEntry {
                ino: 7,
                name: "input_kbd",
                ty: NodeType::CharDevice(DEV_INPUT_KBD),
                mode: 0o600,
            },
            DevEntry {
                ino: 8,
                name: "input_mouse",
                ty: NodeType::CharDevice(DEV_INPUT_MOUSE),
                mode: 0o600,
            },
        ];
        let mut name_to_ino = BTreeMap::new();
        for e in &entries {
            name_to_ino.insert(String::from(e.name), e.ino);
        }
        DevFs {
            entries,
            name_to_ino,
        }
    }

    fn entry_by_ino(&self, ino: Ino) -> Option<&DevEntry> {
        self.entries.iter().find(|e| e.ino == ino)
    }
}

impl Default for DevFs {
    fn default() -> Self {
        DevFs::new()
    }
}

// Device numbers — returned inside `NodeType::CharDevice(u32)` and
// used by the device-dispatch layer (T067) to route fd_read/fd_write
// on an open device fd to the right driver.
pub const DEV_NULL:        u32 = 1;
pub const DEV_ZERO:        u32 = 2;
pub const DEV_RANDOM:      u32 = 3;
pub const DEV_CONSOLE:     u32 = 4;
pub const DEV_FB0:         u32 = 10;
pub const DEV_INPUT_KBD:   u32 = 20;
pub const DEV_INPUT_MOUSE: u32 = 21;

impl Filesystem for DevFs {
    fn root(&self) -> Ino {
        1
    }

    fn lookup(&mut self, dir: Ino, name: &str) -> Result<Ino, FsError> {
        if dir != 1 {
            return Err(FsError::NotADirectory);
        }
        self.name_to_ino.get(name).copied().ok_or(FsError::NotFound)
    }

    fn read(&mut self, _ino: Ino, _offset: u64, _buf: &mut [u8]) -> Result<usize, FsError> {
        // Reads on devfs nodes go through the device-dispatch layer
        // (T067), not through the VFS read path. At the VFS layer
        // this is a NotSupported.
        Err(FsError::NotSupported)
    }

    fn write(&mut self, _ino: Ino, _offset: u64, _buf: &[u8]) -> Result<usize, FsError> {
        Err(FsError::NotSupported)
    }

    fn readdir(&mut self, dir: Ino, out: &mut Vec<DirEntry>) -> Result<(), FsError> {
        if dir != 1 {
            return Err(FsError::NotADirectory);
        }
        for e in &self.entries {
            out.push(DirEntry {
                name: String::from(e.name),
                ino: e.ino,
                ty: e.ty,
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
        if ino == 1 {
            return Ok(FileStat::zeroed(1, NodeType::Directory, 0o755));
        }
        let e = self.entry_by_ino(ino).ok_or(FsError::NotFound)?;
        Ok(FileStat {
            ino: e.ino,
            ty: e.ty,
            mode: e.mode,
            nlink: 1,
            size: 0,
            atime_ns: 0,
            mtime_ns: 0,
            ctime_ns: 0,
        })
    }

    fn truncate(&mut self, _ino: Ino, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::NotSupported)
    }

    fn kind_name(&self) -> &'static str {
        "devfs"
    }
}
