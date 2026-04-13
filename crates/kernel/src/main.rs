// WASM entrypoint for the kernel.
//
// When compiled for wasm32-unknown-unknown without the
// `native-platform` feature, this binary is the kernel Worker's
// entry. The host JS (`web/src/kernel-worker.ts`) instantiates the
// WASM, calls `kernel_init`, and then pumps `kernel_step` from the
// Worker's microtask loop.
//
// Under the `native-platform` feature (used by `cargo test`), the
// crate builds as a library via lib.rs and this file is a no-op
// `fn main()` so `cargo build -p kernel --features native-platform`
// still produces a binary target if needed.
//
// Populated further in Phase 2 T073..T078.

#![cfg_attr(
    all(not(feature = "native-platform"), target_arch = "wasm32"),
    no_std,
    no_main
)]

#[cfg(all(not(feature = "native-platform"), target_arch = "wasm32"))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Route through the Platform's panic hook so the bootstrap
    // overlay + auto-reload path (FR-009a) receives the notification.
    kernel::platform::current().on_panic(info);
    kernel::platform::current().halt("kernel panic");
}

#[cfg(any(feature = "native-platform", not(target_arch = "wasm32")))]
fn main() {
    // Host-target main is unused; tests invoke the library directly
    // through the Platform abstraction. This main exists only so
    // `cargo build -p kernel` works on the host.
}
