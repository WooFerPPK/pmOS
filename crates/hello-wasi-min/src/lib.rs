//! Minimum-viable WASI preview 1 user binary.
//!
//! `no_std` cdylib + exactly two `wasi_snapshot_preview1` imports
//! (`fd_write` + `proc_exit`). The whole program fits in a handful
//! of WASM instructions and is the smallest wasm binary the PMos
//! user-runtime side can use to prove the pipeline works end-to-end:
//!
//!   * The runtime can load it.
//!   * The WASI shim can satisfy its imports.
//!   * The shim's `fd_write` translates to a PMos `FD_WRITE`
//!     syscall that lands on the kernel's `/dev/console` device.
//!   * The shim's `proc_exit` terminates the process cleanly.
//!
//! The binary writes `"hello from userland\n"` to fd 1 and exits
//! with status 0. That's it.
//!
//! Deliberately avoids Rust's `std` + WASI libc startup so:
//!
//!   * The compiled output has **only** the two imports we name
//!     (no hidden `fd_prestat_*`, `clock_time_get`, or
//!     `environ_*` dependencies pulled in by the Rust startup
//!     scaffold).
//!   * Any breakage in the user-runtime test surfaces at the
//!     layer that broke, not in Rust's std WASI integration.
//!   * The binary is small enough to bake into tests without
//!     worrying about build times or disk footprint.
//!
//! ## Why a `cdylib` target instead of `[[bin]]`
//!
//! A bin target for `wasm32-wasip1` pulls in `crt1-command.o`,
//! which defines its own `_start` and conflicts with ours. A
//! cdylib skips the crt entirely, producing a wasm module whose
//! only exported function is the `#[no_mangle] pub extern "C" fn
//! _start` below.
//!
//! ## Why everything below is `#[cfg(target_arch = "wasm32")]`
//!
//! `cargo test --workspace` walks every workspace member and
//! builds each for the host target (for its unit-test harness).
//! A `#![no_std]` crate with a `#[panic_handler]` conflicts with
//! `std`'s panic-handler lang item on native, producing a
//! duplicate-lang-item error. The fix is to make the crate a
//! no-op on non-wasm32 targets: `no_std` is applied conditionally,
//! every item is gated on `wasm32`, and on native there's nothing
//! to compile beyond the (default-std) crate shell. The Justfile's
//! build target still compiles the crate for `wasm32-wasip1`,
//! where all the gated items become live.

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    /// WASI preview 1 `fd_write`. Writes a scatter-gather list
    /// of buffers to `fd`. Returns an errno (0 = success).
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;

    /// WASI preview 1 `proc_exit`. Terminates the process with
    /// exit code `rval` and never returns.
    fn proc_exit(rval: i32) -> !;
}

/// Layout of WASI preview 1's `ciovec_t`: a pointer into the
/// caller's linear memory + a length. The runtime's WASI shim
/// reads this struct from the user wasm's own memory at the
/// `iovs_ptr` address `fd_write` was called with.
#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Ciovec {
    buf: *const u8,
    buf_len: u32,
}

/// WASI `_start` entry point. The runtime calls this directly
/// after instantiation.
#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn _start() {
    const MSG: &[u8] = b"hello from userland\n";
    let iov = Ciovec {
        buf: MSG.as_ptr(),
        buf_len: MSG.len() as u32,
    };
    let mut nwritten: u32 = 0;
    unsafe {
        // Fire-and-check: on any errno we still proc_exit, just
        // with a non-zero code so the test can tell the write
        // path failed vs. the binary never ran.
        let rc = fd_write(1, &iov, 1, &mut nwritten);
        if rc != 0 {
            proc_exit(rc);
        }
        proc_exit(0);
    }
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // No allocator, no formatting, no print — the panic handler
    // exists only to satisfy the `no_std` requirement. A real
    // panic during `_start` terminates the user process with a
    // distinctive non-zero exit code so the test can distinguish
    // "wrote the message and exited" (code 0) from "crashed"
    // (code 101).
    unsafe { proc_exit(101) }
}
