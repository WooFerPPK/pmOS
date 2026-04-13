// WASM entrypoint for the kernel.
//
// When compiled for wasm32-unknown-unknown, this binary is the kernel
// Worker's entry. The host JS (`web/src/kernel-worker.ts`) instantiates
// the WASM, calls `kernel_init`, and then pumps `kernel_step` from the
// Worker's microtask loop.
//
// Populated in Phase 2 T034..T078.

#![cfg_attr(not(feature = "native-platform"), no_std)]
#![cfg_attr(not(feature = "native-platform"), no_main)]

#[cfg(not(feature = "native-platform"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[cfg(feature = "native-platform")]
fn main() {
    // Host-target main is unused; tests invoke the library directly
    // through the Platform abstraction. This main exists only so the
    // crate builds as `cargo build -p kernel` on the host.
}
