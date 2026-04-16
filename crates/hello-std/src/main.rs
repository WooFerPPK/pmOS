//! The minimum-viable Rust `std` binary: `fn main()` + `println!`.
//!
//! Exercises the full Rust libc/WASI startup path against the PMos
//! kernel opcode handlers and the user-wasm-runtime shim. If this
//! binary runs to completion and writes the expected line to stdout,
//! every opcode touched during `std`'s `__wasi_proc_exit`-terminated
//! startup sequence is wired.
//!
//! Unlike every other `hello-*` crate in this workspace, this one is
//! NOT `#![no_std]`: it deliberately uses the full `std` crate so the
//! test surfaces any gap in the PMos layer that a conventional Rust
//! program would hit.

fn main() {
    println!("hello from std");
}
