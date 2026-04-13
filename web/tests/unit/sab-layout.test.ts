// T038 — sab-layout drift detection.
//
// This test asserts that the well-known constants in
// `src/shared/sab-layout.ts` match a hand-written expected table.
// If someone changes the `abi` crate's ring layout and regenerates
// sab-layout.ts without also updating this table, the drift gets
// caught here; if someone edits sab-layout.ts by hand in a way
// that would desync it from abi, this test flags it too.
//
// The "gold standard" drift check is `cargo run -p xtask --
// gen-sab-layout --check`, which rebuilds the TS from abi and
// asserts byte-for-byte equality. That runs in CI. This file is
// the client-side mirror test — the one that catches breakage
// inside the Vitest harness when the driver layer's TS starts
// misbehaving.

import { describe, expect, it } from "vitest";
import * as layout from "../../src/shared/sab-layout";

describe("sab-layout", () => {
  it("ABI_VERSION matches the v1.1 contract from contracts/syscalls.md §5", () => {
    expect(layout.ABI_MAJOR).toBe(1);
    expect(layout.ABI_MINOR).toBe(1);
    expect(layout.ABI_VERSION).toEqual([1, 1]);
  });

  it("SAB is 64 KiB and split into the documented regions", () => {
    expect(layout.SAB_SIZE).toBe(0x10000);
    expect(layout.OFF_HEAP_SCRATCH + layout.HEAP_SCRATCH_BYTES).toBe(layout.SAB_SIZE);
    expect(layout.OFF_REQ_RING + layout.REQ_RING_BYTES).toBe(layout.OFF_RES_RING);
    expect(layout.OFF_RES_RING + layout.RES_RING_BYTES).toBe(layout.OFF_HEAP_SCRATCH);
  });

  it("header offsets are at the documented positions from contracts/driver-kernel.md §1.1", () => {
    expect(layout.OFF_REQ_HEAD).toBe(0x00);
    expect(layout.OFF_REQ_TAIL).toBe(0x04);
    expect(layout.OFF_RES_HEAD).toBe(0x08);
    expect(layout.OFF_RES_TAIL).toBe(0x0C);
    expect(layout.OFF_USER_WAIT_SLOT).toBe(0x10);
    expect(layout.OFF_KERNEL_WAIT_SLOT).toBe(0x14);
    expect(layout.OFF_USER_BLOCK_COUNT).toBe(0x18);
    expect(layout.OFF_KERNEL_BLOCK_COUNT).toBe(0x1C);
    expect(layout.OFF_HEADER_FLAGS).toBe(0x20);
    expect(layout.OFF_REQ_RING).toBe(0x40);
    expect(layout.OFF_RES_RING).toBe(0x4000);
    expect(layout.OFF_HEAP_SCRATCH).toBe(0x8000);
  });

  it("slot geometry: 32-byte slots, 510 slots per ring", () => {
    expect(layout.SLOT_SIZE).toBe(32);
    // 0x3FC0 / 32 = 510
    expect(layout.REQ_SLOT_COUNT).toBe(510);
    expect(layout.RES_SLOT_COUNT).toBe(510);
    expect(layout.REQ_RING_BYTES).toBe(layout.REQ_SLOT_COUNT * layout.SLOT_SIZE);
    expect(layout.RES_RING_BYTES).toBe(layout.RES_SLOT_COUNT * layout.SLOT_SIZE);
  });

  it("magic status values are 0/1/2/3 and distinct", () => {
    expect(layout.STATUS_IDLE).toBe(0);
    expect(layout.STATUS_REQUESTED).toBe(1);
    expect(layout.STATUS_SERVICING).toBe(2);
    expect(layout.STATUS_READY).toBe(3);
    const all = new Set([
      layout.STATUS_IDLE,
      layout.STATUS_REQUESTED,
      layout.STATUS_SERVICING,
      layout.STATUS_READY,
    ]);
    expect(all.size).toBe(4);
  });

  it("header offsets are all 4-byte aligned (required for Atomics.wait)", () => {
    const offsets = [
      layout.OFF_REQ_HEAD,
      layout.OFF_REQ_TAIL,
      layout.OFF_RES_HEAD,
      layout.OFF_RES_TAIL,
      layout.OFF_USER_WAIT_SLOT,
      layout.OFF_KERNEL_WAIT_SLOT,
      layout.OFF_USER_BLOCK_COUNT,
      layout.OFF_KERNEL_BLOCK_COUNT,
      layout.OFF_HEADER_FLAGS,
      layout.OFF_REQ_RING,
      layout.OFF_RES_RING,
      layout.OFF_HEAP_SCRATCH,
    ];
    for (const o of offsets) {
      expect(o % 4).toBe(0);
    }
  });

  it("sabHeader and sabHeapScratch return correctly-sized TypedArrays", () => {
    // Note: SharedArrayBuffer only exists in cross-origin isolated
    // contexts, so in the Vitest test environment we fall back to a
    // plain ArrayBuffer that is type-compatible for this test's
    // purposes (the helpers only use offset/length arithmetic).
    const backing = new ArrayBuffer(layout.SAB_SIZE);
    const header = layout.sabHeader(backing as unknown as SharedArrayBuffer);
    expect(header.length).toBe(layout.OFF_HEAP_SCRATCH / 4);

    const heap = layout.sabHeapScratch(backing as unknown as SharedArrayBuffer);
    expect(heap.byteLength).toBe(layout.HEAP_SCRATCH_BYTES);
    expect(heap.byteOffset).toBe(layout.OFF_HEAP_SCRATCH);
  });
});
