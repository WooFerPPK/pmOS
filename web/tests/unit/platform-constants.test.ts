// Drift detection for the hand-mirrored constants in
// `src/shared/platform-constants.ts` against the Rust source of
// truth.
//
// Two Rust namespaces are mirrored here:
//
//   * `DriverId.*` ↔ `kernel::platform::DevId` in
//     crates/kernel/src/platform/mod.rs (#[repr(u8)])
//   * `Devnum.*`   ↔ `kernel::fs::devfs::DEV_*` in
//     crates/kernel/src/fs/devfs.rs
//
// The "gold standard" drift check is a future xtask
// `gen-platform-constants` (companion to `gen-sab-layout`) that
// regenerates the TS file and fails CI if the contents differ
// byte-for-byte from the Rust-side values. Until that exists,
// this file is the client-side mirror test.

import { describe, expect, it } from "vitest";
import { DriverId, Devnum } from "../../src/shared/platform-constants";

describe("platform-constants DriverId", () => {
  it("matches kernel::platform::DevId #[repr(u8)] values", () => {
    expect(DriverId.Framebuffer).toBe(0);
    expect(DriverId.InputKbd).toBe(1);
    expect(DriverId.InputMouse).toBe(2);
    expect(DriverId.Block).toBe(3);
    expect(DriverId.Net).toBe(4);
    expect(DriverId.Console).toBe(5);
  });

  it("DriverId values are dense and unique", () => {
    const values = Object.values(DriverId);
    const uniq = new Set(values);
    expect(uniq.size).toBe(values.length);
    // Max value + 1 == count, i.e. dense from 0.
    const max = Math.max(...values);
    expect(max + 1).toBe(values.length);
  });
});

describe("platform-constants Devnum", () => {
  it("matches kernel::fs::devfs::DEV_* constants", () => {
    expect(Devnum.Null).toBe(1);
    expect(Devnum.Zero).toBe(2);
    expect(Devnum.Random).toBe(3);
    expect(Devnum.Console).toBe(4);
    expect(Devnum.Fb0).toBe(10);
    expect(Devnum.InputKbd).toBe(20);
    expect(Devnum.InputMouse).toBe(21);
  });

  it("Devnum values are unique (dense-ness not required)", () => {
    const values = Object.values(Devnum);
    const uniq = new Set(values);
    expect(uniq.size).toBe(values.length);
  });

  it("Devnum and DriverId are distinct namespaces", () => {
    // The two share a numeric range but the semantics differ.
    // In particular, DriverId.Console (5) and Devnum.Console
    // (4) must NOT collide — if they did, a driver lookup by
    // devnum would accidentally route through the wrong driver.
    expect(Devnum.Console).not.toBe(DriverId.Console);
  });
});
