#![no_std]

//! PMos OS ABI.
//!
//! Single source of truth for syscall numbering, request/response
//! layouts, capability identifiers, and the SAB ring-buffer protocol
//! contract shared between the kernel and every userland program.
//!
//! The contents of this crate are normative: anything defined here
//! mirrors `specs/001-browser-os-v1/contracts/syscalls.md` and
//! `contracts/driver-kernel.md`. When those documents change, this
//! crate changes in the same commit.

pub mod version;
pub mod wasi;
pub mod ext;
pub mod cap;
pub mod ring;
pub mod errno;
