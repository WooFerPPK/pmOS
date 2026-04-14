// Hand-mirrored constants from the Rust platform + devfs layers.
//
// Two namespaces live here. They are distinct — see the header
// comment in `../drivers/types.ts` for the full explanation.
//
//   * `DriverId.*` mirrors `kernel::platform::DevId`
//     (crates/kernel/src/platform/mod.rs). One value per driver
//     class. The TS scaffold registers one driver per DriverId
//     and routes `Platform::driver_call` lookups by it.
//
//   * `Devnum.*` mirrors `kernel::fs::devfs::DEV_*`
//     (crates/kernel/src/fs/devfs.rs). One value per device
//     NODE. Drivers push input bytes into the kernel's
//     per-devnum input ring via `DriverHost::pushInputToKernel`.
//
// If either Rust-side namespace changes, this file and the
// Vitest tests that assert on it must be updated. A later xtask
// slice (companion to gen-sab-layout) will regenerate this file
// from the abi crate so drift becomes a build-break.

/** Mirrors `kernel::platform::DevId`. */
export const DriverId = {
  Framebuffer: 0,
  InputKbd: 1,
  InputMouse: 2,
  Block: 3,
  Net: 4,
  Console: 5,
} as const;

export type DriverIdValue = (typeof DriverId)[keyof typeof DriverId];

/** Mirrors `kernel::fs::devfs::DEV_*`. */
export const Devnum = {
  Null: 1,
  Zero: 2,
  Random: 3,
  Console: 4,
  Fb0: 10,
  InputKbd: 20,
  InputMouse: 21,
} as const;

export type DevnumValue = (typeof Devnum)[keyof typeof Devnum];
