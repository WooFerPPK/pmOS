//! tmpfs — in-memory filesystem.
//!
//! Used for `/tmp` and `/run`. Also used by kernel tests as a
//! stand-in for any real filesystem, because it is fully
//! functional: create files, write, read, mkdir, readdir, rename,
//! truncate, unlink, rmdir.
//!
//! Storage model:
//!
//! * Each inode lives in `BTreeMap<Ino, TmpNode>` owned by the
//!   `TmpFs`. Ino `1` is the root directory.
//! * Directories store a `BTreeMap<String, Ino>` child map.
//!   Ordered by name so `readdir` output is deterministic.
//! * Regular files store a `Vec<u8>` for contents.
//!
//! No capacity limit is enforced in v1. A future variant can
//! cap per-tmpfs bytes and return `FsError::NoSpace` on overflow.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::vfs::{DirEntry, FileStat, Filesystem, FsError, Ino, Mode, NodeType};

/// A single tmpfs node.
enum TmpNode {
    File(Vec<u8>),
    Directory(BTreeMap<String, Ino>),
}

impl TmpNode {
    fn is_dir(&self) -> bool {
        matches!(self, TmpNode::Directory(_))
    }

    fn node_type(&self) -> NodeType {
        match self {
            TmpNode::File(_) => NodeType::RegularFile,
            TmpNode::Directory(_) => NodeType::Directory,
        }
    }

    fn size(&self) -> u64 {
        match self {
            TmpNode::File(bytes) => bytes.len() as u64,
            TmpNode::Directory(children) => children.len() as u64,
        }
    }
}

/// Metadata wrapper: the type-specific content plus the mode.
struct TmpEntry {
    node: TmpNode,
    mode: Mode,
}

/// tmpfs filesystem.
pub struct TmpFs {
    nodes: BTreeMap<Ino, TmpEntry>,
    next_ino: Ino,
}

impl TmpFs {
    /// Create a fresh tmpfs with an empty root directory.
    pub fn new() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            1,
            TmpEntry {
                node: TmpNode::Directory(BTreeMap::new()),
                mode: 0o755,
            },
        );
        TmpFs {
            nodes,
            next_ino: 2,
        }
    }

    fn alloc_ino(&mut self) -> Ino {
        let ino = self.next_ino;
        self.next_ino = self.next_ino.checked_add(1).expect("tmpfs ino overflow");
        ino
    }

    fn entry(&self, ino: Ino) -> Result<&TmpEntry, FsError> {
        self.nodes.get(&ino).ok_or(FsError::NotFound)
    }

    fn entry_mut(&mut self, ino: Ino) -> Result<&mut TmpEntry, FsError> {
        self.nodes.get_mut(&ino).ok_or(FsError::NotFound)
    }

    fn dir_children(&self, ino: Ino) -> Result<&BTreeMap<String, Ino>, FsError> {
        match &self.entry(ino)?.node {
            TmpNode::Directory(c) => Ok(c),
            _ => Err(FsError::NotADirectory),
        }
    }

    fn dir_children_mut(&mut self, ino: Ino) -> Result<&mut BTreeMap<String, Ino>, FsError> {
        match &mut self.entry_mut(ino)?.node {
            TmpNode::Directory(c) => Ok(c),
            _ => Err(FsError::NotADirectory),
        }
    }
}

impl Default for TmpFs {
    fn default() -> Self {
        TmpFs::new()
    }
}

impl Filesystem for TmpFs {
    fn root(&self) -> Ino {
        1
    }

    fn lookup(&mut self, dir: Ino, name: &str) -> Result<Ino, FsError> {
        let children = self.dir_children(dir)?;
        children.get(name).copied().ok_or(FsError::NotFound)
    }

    fn read(&mut self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        let entry = self.entry(ino)?;
        let bytes = match &entry.node {
            TmpNode::File(b) => b,
            TmpNode::Directory(_) => return Err(FsError::IsADirectory),
        };
        let start = offset as usize;
        if start >= bytes.len() {
            return Ok(0);
        }
        let end = core::cmp::min(bytes.len(), start + buf.len());
        let n = end - start;
        buf[..n].copy_from_slice(&bytes[start..end]);
        Ok(n)
    }

    fn write(&mut self, ino: Ino, offset: u64, buf: &[u8]) -> Result<usize, FsError> {
        let entry = self.entry_mut(ino)?;
        let bytes = match &mut entry.node {
            TmpNode::File(b) => b,
            TmpNode::Directory(_) => return Err(FsError::IsADirectory),
        };
        let start = offset as usize;
        let needed = start + buf.len();
        if bytes.len() < needed {
            bytes.resize(needed, 0);
        }
        bytes[start..needed].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn readdir(&mut self, dir: Ino, out: &mut Vec<DirEntry>) -> Result<(), FsError> {
        let children_snapshot: Vec<(String, Ino)> = {
            let children = self.dir_children(dir)?;
            children.iter().map(|(k, v)| (k.clone(), *v)).collect()
        };
        for (name, child_ino) in children_snapshot {
            let child = self.entry(child_ino)?;
            out.push(DirEntry {
                name,
                ino: child_ino,
                ty: child.node.node_type(),
            });
        }
        Ok(())
    }

    fn create(&mut self, dir: Ino, name: &str, mode: Mode) -> Result<Ino, FsError> {
        // Validate name up front.
        check_name(name)?;
        // Check that the parent is a directory and does NOT already
        // contain `name`.
        {
            let children = self.dir_children(dir)?;
            if children.contains_key(name) {
                return Err(FsError::AlreadyExists);
            }
        }
        let ino = self.alloc_ino();
        self.nodes.insert(
            ino,
            TmpEntry {
                node: TmpNode::File(Vec::new()),
                mode,
            },
        );
        self.dir_children_mut(dir)?.insert(name.to_string(), ino);
        Ok(ino)
    }

    fn mkdir(&mut self, dir: Ino, name: &str, mode: Mode) -> Result<Ino, FsError> {
        check_name(name)?;
        {
            let children = self.dir_children(dir)?;
            if children.contains_key(name) {
                return Err(FsError::AlreadyExists);
            }
        }
        let ino = self.alloc_ino();
        self.nodes.insert(
            ino,
            TmpEntry {
                node: TmpNode::Directory(BTreeMap::new()),
                mode,
            },
        );
        self.dir_children_mut(dir)?.insert(name.to_string(), ino);
        Ok(ino)
    }

    fn unlink(&mut self, dir: Ino, name: &str) -> Result<(), FsError> {
        let child_ino = {
            let children = self.dir_children(dir)?;
            *children.get(name).ok_or(FsError::NotFound)?
        };
        let child = self.entry(child_ino)?;
        if child.node.is_dir() {
            return Err(FsError::IsADirectory);
        }
        // Safe to remove from parent and drop the node.
        self.dir_children_mut(dir)?.remove(name);
        self.nodes.remove(&child_ino);
        Ok(())
    }

    fn rmdir(&mut self, dir: Ino, name: &str) -> Result<(), FsError> {
        let child_ino = {
            let children = self.dir_children(dir)?;
            *children.get(name).ok_or(FsError::NotFound)?
        };
        let child = self.entry(child_ino)?;
        match &child.node {
            TmpNode::Directory(c) => {
                if !c.is_empty() {
                    return Err(FsError::NotEmpty);
                }
            }
            _ => return Err(FsError::NotADirectory),
        }
        self.dir_children_mut(dir)?.remove(name);
        self.nodes.remove(&child_ino);
        Ok(())
    }

    fn rename(
        &mut self,
        from_dir: Ino,
        from_name: &str,
        to_dir: Ino,
        to_name: &str,
    ) -> Result<(), FsError> {
        check_name(to_name)?;
        // Locate the source.
        let child_ino = {
            let children = self.dir_children(from_dir)?;
            *children.get(from_name).ok_or(FsError::NotFound)?
        };
        // If destination exists, replace it (POSIX rename).
        let replaced = {
            let dst_children = self.dir_children(to_dir)?;
            dst_children.get(to_name).copied()
        };
        // Remove from source first.
        self.dir_children_mut(from_dir)?.remove(from_name);
        // If we had a replacement, drop its node.
        if let Some(replaced_ino) = replaced {
            self.dir_children_mut(to_dir)?.remove(to_name);
            self.nodes.remove(&replaced_ino);
        }
        // Install at destination.
        self.dir_children_mut(to_dir)?
            .insert(to_name.to_string(), child_ino);
        Ok(())
    }

    fn stat(&mut self, ino: Ino) -> Result<FileStat, FsError> {
        let entry = self.entry(ino)?;
        Ok(FileStat {
            ino,
            ty: entry.node.node_type(),
            mode: entry.mode,
            nlink: 1,
            size: entry.node.size(),
            atime_ns: 0,
            mtime_ns: 0,
            ctime_ns: 0,
        })
    }

    fn truncate(&mut self, ino: Ino, new_size: u64) -> Result<(), FsError> {
        let entry = self.entry_mut(ino)?;
        let bytes = match &mut entry.node {
            TmpNode::File(b) => b,
            TmpNode::Directory(_) => return Err(FsError::IsADirectory),
        };
        bytes.resize(new_size as usize, 0);
        Ok(())
    }

    fn kind_name(&self) -> &'static str {
        "tmpfs"
    }
}

/// Reject empty names and names containing `/` or a NUL byte.
fn check_name(name: &str) -> Result<(), FsError> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(FsError::InvalidArgument);
    }
    if name.contains('/') || name.contains('\0') {
        return Err(FsError::InvalidArgument);
    }
    Ok(())
}
