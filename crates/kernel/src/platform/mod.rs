//! Platform abstraction.
//!
//! The kernel is built against a single trait, [`Platform`], which
//! abstracts every browser-specific capability the kernel cannot
//! implement in pure Rust:
//!
//! * **Clock**: monotonic nanoseconds since kernel boot
//!   (`clock_time_get(MONOTONIC)` implementation).
//! * **Driver transport**: how the kernel sends messages to, and
//!   receives asynchronous events from, the TypeScript driver layer
//!   — framebuffer, input, block, net, console.
//! * **Halt**: how the kernel stops the browser tab on an
//!   unrecoverable condition (FR-009a). The `bootstrap.ts` panic
//!   overlay + auto-reload timer lives on the other side of this
//!   hook.
//! * **Panic hook**: how a Rust `panic!()` inside the kernel
//!   Worker propagates to the bootstrap as a kernel panic.
//! * **Random bytes**: for `/dev/random`, `random_get`, and PID
//!   randomisation seeds. Backed by `crypto.getRandomValues` in
//!   the browser and by `getrandom`-equivalent on native.
//!
//! Two concrete impls:
//!
//! * [`native::NativePlatform`] — active when the crate is built
//!   with the `native-platform` feature. Used by `cargo test`
//!   on the host target so kernel isolation tests run without a
//!   browser (Principle X + the Principle VIII headless-shell
//!   gate T077).
//! * [`wasm::WasmPlatform`] — active when the crate is built for
//!   `wasm32-unknown-unknown` without the native-platform feature.
//!   Used at runtime in the browser. Backed by `extern "C"` host
//!   imports wired up by `web/src/kernel-worker.ts`.
//!
//! The active implementation is exposed as a static reference via
//! [`current`]. Kernel code that needs platform services calls
//! `platform::current().now_ns()` etc., never creating `Platform`
//! trait objects by hand.

use core::panic::PanicInfo;

// --- Driver identifiers -------------------------------------------------
//
// These match the DevId namespace in contracts/driver-kernel.md §2 and
// are used by the kernel to route driver_call() messages.

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DevId {
    Framebuffer = 0,
    InputKbd    = 1,
    InputMouse  = 2,
    Block       = 3,
    Net         = 4,
    Console     = 5,
}

/// Driver error codes the Platform may return from a control-channel call.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DriverError {
    /// Driver module is not initialised.
    NotReady,
    /// Driver returned an error; `errno` is the reported errno (positive).
    Errno(i32),
    /// The driver-kernel transport itself failed (usually a bug).
    Transport,
}

/// Result of a Platform::driver_call.
pub type DriverResult<T> = Result<T, DriverError>;

/// A Platform service interface. See module docs for the contract.
pub trait Platform: Sync + 'static {
    /// Monotonic nanoseconds since the Platform started. Must be
    /// strictly increasing on each call (i.e. `now_ns()` called twice
    /// in succession MUST NOT return equal values even if the
    /// underlying clock ticked less than 1 ns between calls — the
    /// Platform may add a `+1` fudge).
    fn now_ns(&self) -> u64;

    /// Non-blocking control-channel send to a driver. Used for
    /// device-open, ioctl, and similar cold-path operations. Hot
    /// paths (framebuffer submit, input event delivery) use SAB
    /// rings installed by `attach_data_ring` and do NOT go through
    /// this call.
    ///
    /// `op` is the ioctl opcode (driver-specific). `args` is the
    /// request payload. The returned `u32` is the driver's reply
    /// result code (meaning is driver-specific).
    fn driver_call(&self, dev: DevId, op: u32, args: &[u8]) -> DriverResult<u32>;

    /// Fill `out` with cryptographically-random bytes.
    fn random_bytes(&self, out: &mut [u8]);

    /// Signal to the bootstrap that the kernel has hit an
    /// unrecoverable condition. On `WasmPlatform` this posts a
    /// `kernel_panic` message to the main thread and then halts
    /// the worker; on `NativePlatform` it records the panic for
    /// later assertion in the test harness and returns `!` via
    /// `panic!()`.
    ///
    /// This call never returns.
    fn halt(&self, reason: &str) -> !;

    /// Panic hook, installed via `#[panic_handler]` on wasm32 and
    /// via `std::panic::set_hook` on native. Both impls route
    /// through here so the bootstrap sees a consistent panic
    /// notification regardless of whether the panic originated in
    /// a `panic!()`, an `unreachable!()`, or an allocator OOM.
    fn on_panic(&self, info: &PanicInfo);
}

// --- Active-implementation selector ------------------------------------

#[cfg(feature = "native-platform")]
pub mod native;

#[cfg(not(feature = "native-platform"))]
pub mod wasm;

/// Return a reference to the active [`Platform`] implementation.
///
/// Under `cargo test` with the `native-platform` feature, this returns
/// the [`native::NativePlatform`] singleton. Under
/// `wasm32-unknown-unknown` (the real kernel runtime target), it
/// returns [`wasm::WasmPlatform`].
#[cfg(feature = "native-platform")]
#[inline]
pub fn current() -> &'static dyn Platform {
    native::NATIVE_PLATFORM.get_or_install()
}

#[cfg(not(feature = "native-platform"))]
#[inline]
pub fn current() -> &'static dyn Platform {
    &wasm::WasmPlatform
}
