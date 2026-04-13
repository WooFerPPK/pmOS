//! Concrete filesystem implementations.
//!
//! Each submodule here is a `Filesystem` trait impl. The VFS
//! layer (`crate::vfs`) owns the abstractions and the mount
//! table; this module is just the inventory of real
//! filesystems the kernel ships with.
//!
//! * [`tmpfs`] — in-memory, used for `/tmp`, `/run`, and tests.
//! * [`devfs`] — static device-node directory, used for `/dev`.
//! * [`procfs`] — synthetic process-introspection filesystem,
//!   used for `/proc`.
//!
//! OPFS client lives under this same module (Phase 2 T056..T061).

pub mod tmpfs;
pub mod devfs;
pub mod procfs;
