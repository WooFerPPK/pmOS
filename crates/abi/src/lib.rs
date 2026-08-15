#![cfg_attr(not(test), no_std)]

//! PMos OS ABI.
//!
//! Single source of truth for syscall numbering, request/response
//! layouts, capability identifiers, errno values, and the SAB
//! ring-buffer protocol contract shared between the kernel and
//! every userland program.
//!
//! The contents of this crate are normative: anything defined here
//! mirrors `specs/001-browser-os-v1/contracts/syscalls.md` and
//! `contracts/driver-kernel.md`. When those documents change, this
//! crate changes in the same commit.
//!
//! `#![cfg_attr(not(test), no_std)]` is deliberate: production
//! builds for `wasm32-unknown-unknown` (kernel) and `wasm32-wasip1`
//! (userland) are `no_std`; `cargo test --host` pulls `std` so the
//! default `#[test]` harness works.

pub mod cap;
pub mod errno;
pub mod ext;
pub mod fd;
pub mod ring;
pub mod version;
pub mod wasi;
