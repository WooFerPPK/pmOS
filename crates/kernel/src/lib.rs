#![cfg_attr(not(feature = "native-platform"), no_std)]

//! PMos kernel library.
//!
//! The kernel is the root of every syscall path. It owns the
//! process table, the VFS, the IPC subsystem, the capability set,
//! and the device-node dispatch layer.
//!
//! This crate has **two build targets**:
//!
//! * **wasm32-unknown-unknown**: the kernel runs inside a
//!   dedicated Web Worker in the browser. `no_std` + `alloc`,
//!   custom bump allocator (see `alloc_` module).
//! * **native (host target) with the `native-platform` feature**:
//!   the kernel links as a regular Rust library so `cargo test`
//!   can exercise it end-to-end without a browser. This is how
//!   Principle VIII's headless shell gate (T077) and the
//!   per-layer isolation tests (T046 / T048 / T055 / T061 / T064
//!   / T066 / T077 / T078) run.
//!
//! Every browser-specific behaviour is routed through the
//! [`Platform`](platform::Platform) trait. Tests use
//! `NativePlatform`; runtime uses `WasmPlatform`.
//!
//! `extern crate alloc;` is unconditional so that kernel code can
//! refer to `alloc::vec::Vec` and `alloc::collections::BTreeMap`
//! uniformly regardless of build mode.

extern crate alloc;

pub mod platform;
pub mod alloc_;
pub mod proc;
pub mod fd;
pub mod vfs;
pub mod fs;
pub mod ipc;
pub mod syscall;
pub mod cap;
pub mod dev;
