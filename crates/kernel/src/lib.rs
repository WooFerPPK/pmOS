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

pub mod alloc_;
pub mod cap;
pub mod dev;
pub mod fd;
pub mod fs;
pub mod host_file;
pub mod ipc;
pub mod platform;
pub mod proc;
pub mod sys;
pub mod syscall;
pub mod vfs;

// wasm32-only entry points: the narrow extern "C" seam that lets
// the kernel Worker call into the dispatcher from the TS side.
// Gated on the same cfg as the panic handler below so native
// tests never see it — native coverage for the dispatcher lives
// in `tests/syscall.rs`, which calls `kernel::syscall::dispatch`
// directly.
#[cfg(all(not(feature = "native-platform"), target_arch = "wasm32"))]
pub mod wasm_entry;

// Panic handler for the wasm32-unknown-unknown cdylib build. Lives in
// lib.rs (not alloc_.rs) because the #[panic_handler] attribute has
// to be at crate-root scope, not nested inside a submodule. Routes
// through the Platform hook so the bootstrap kernel-panic overlay
// (FR-009a) receives the notification on the JS side.
#[cfg(all(not(feature = "native-platform"), target_arch = "wasm32"))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    crate::platform::current().on_panic(info);
    crate::platform::current().halt("kernel panic")
}
