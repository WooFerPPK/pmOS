// Unit tests for the TS framebuffer driver. Covers every
// branch of `FramebufferDriver.call`:
//
//   * both opcodes (OP_SET_MODE, OP_BLIT)
//   * short-payload rejection
//   * pixel-count mismatch rejection
//   * defensive copy of RGBA bytes on blit
//   * unknown opcode → Transport
//   * call before init → NotReady
//
// The kernel-worker routing tests live in
// `kernel-worker.test.ts`.

import { describe, expect, it } from "vitest";
import {
  DEV_FB0_NODE,
  FB_DRIVER_ID,
  FramebufferDriver,
  FB_PATCH_MAX_RGBA_BYTES,
  OP_BLIT,
  OP_PATCH,
  OP_PATCH_PALETTE_RLE_BATCH,
  OP_PATCH_RLE,
  OP_PRESENT_FENCE,
  OP_SET_MODE,
} from "../../src/drivers/fb";
import { DriverErrorCode } from "../../src/drivers/types";
import type { DriverHost } from "../../src/drivers/types";
import { Devnum, DriverId } from "../../src/shared/platform-constants";

interface CapturingHost extends DriverHost {
  readonly posted: unknown[];
  readonly pushed: Array<{ devnum: number; bytes: Uint8Array }>;
}

function makeHost(): CapturingHost {
  const posted: unknown[] = [];
  const pushed: Array<{ devnum: number; bytes: Uint8Array }> = [];
  return {
    posted,
    pushed,
    postToMain(msg: unknown): void {
      posted.push(msg);
    },
    pushInputToKernel(devnum: number, bytes: Uint8Array): void {
      pushed.push({ devnum, bytes });
    },
  };
}

/** Pack `(width, height)` as an 8-byte little-endian u32 pair. */
function packGeometry(width: number, height: number): Uint8Array {
  const out = new Uint8Array(8);
  const view = new DataView(out.buffer);
  view.setUint32(0, width, true);
  view.setUint32(4, height, true);
  return out;
}

/** Pack an 8-byte header + RGBA pixel body. */
function packBlit(
  width: number,
  height: number,
  pixels: Uint8Array,
): Uint8Array {
  const out = new Uint8Array(8 + pixels.byteLength);
  const view = new DataView(out.buffer);
  view.setUint32(0, width, true);
  view.setUint32(4, height, true);
  out.set(pixels, 8);
  return out;
}

/** Pack the OP_PATCH payload after its one-byte opcode. */
function packPatch(
  x: number,
  y: number,
  width: number,
  height: number,
  pixels: Uint8Array,
): Uint8Array {
  const out = new Uint8Array(16 + pixels.byteLength);
  const view = new DataView(out.buffer);
  view.setUint32(0, x, true);
  view.setUint32(4, y, true);
  view.setUint32(8, width, true);
  view.setUint32(12, height, true);
  out.set(pixels, 16);
  return out;
}

function packRlePatch(
  x: number,
  y: number,
  width: number,
  height: number,
  runs: ReadonlyArray<{ count: number; rgba: readonly number[] }>,
): Uint8Array {
  const out = new Uint8Array(16 + runs.length * 8);
  const view = new DataView(out.buffer);
  view.setUint32(0, x, true);
  view.setUint32(4, y, true);
  view.setUint32(8, width, true);
  view.setUint32(12, height, true);
  runs.forEach((run, index) => {
    const offset = 16 + index * 8;
    view.setUint32(offset, run.count, true);
    out.set(run.rgba, offset + 4);
  });
  return out;
}

function packPaletteRleBatch(
  palette: ReadonlyArray<readonly [number, number, number, number]>,
  rects: ReadonlyArray<{
    readonly x: number;
    readonly y: number;
    readonly width: number;
    readonly height: number;
    readonly runs: ReadonlyArray<{ readonly count: number; readonly index: number }>;
  }>,
): Uint8Array {
  const byteLength =
    2 +
    palette.length * 4 +
    rects.reduce((total, rect) => total + 16 + rect.runs.length * 3, 0);
  const out = new Uint8Array(byteLength);
  const view = new DataView(out.buffer);
  out[0] = rects.length;
  out[1] = palette.length - 1;
  let offset = 2;
  for (const color of palette) {
    out.set(color, offset);
    offset += 4;
  }
  for (const rect of rects) {
    view.setUint32(offset, rect.x, true);
    view.setUint32(offset + 4, rect.y, true);
    view.setUint32(offset + 8, rect.width, true);
    view.setUint32(offset + 12, rect.height, true);
    offset += 16;
    for (const run of rect.runs) {
      view.setUint16(offset, run.count, true);
      out[offset + 2] = run.index;
      offset += 3;
    }
  }
  return out;
}

describe("FramebufferDriver", () => {
  it("registers under the Framebuffer driver-class id", () => {
    const d = new FramebufferDriver();
    expect(d.driverId).toBe(FB_DRIVER_ID);
    expect(d.driverId).toBe(DriverId.Framebuffer);
    expect(d.name).toBe("framebuffer");
  });

  it("DEV_FB0_NODE matches the devnum namespace", () => {
    expect(DEV_FB0_NODE).toBe(Devnum.Fb0);
  });

  it("call() before init() returns NotReady", () => {
    const d = new FramebufferDriver();
    const result = d.call(OP_SET_MODE, packGeometry(640, 480));
    expect(result).toEqual({ ok: false, error: DriverErrorCode.NotReady });
  });

  it("OP_SET_MODE posts an fb:set-mode message with the little-endian u32 geometry", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);

    const result = d.call(OP_SET_MODE, packGeometry(1024, 768));
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.value).toBe(8);
    }
    expect(host.posted).toHaveLength(1);
    const msg = host.posted[0] as {
      kind: string;
      width: number;
      height: number;
    };
    expect(msg.kind).toBe("fb:set-mode");
    expect(msg.width).toBe(1024);
    expect(msg.height).toBe(768);
  });

  it("OP_SET_MODE with a too-short payload returns Transport", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);

    const result = d.call(OP_SET_MODE, new Uint8Array(4));
    expect(result).toEqual({ ok: false, error: DriverErrorCode.Transport });
    expect(host.posted).toHaveLength(0);
  });

  it("OP_PRESENT_FENCE posts the non-zero little-endian serial as a typed event", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);
    const payload = new Uint8Array(4);
    new DataView(payload.buffer).setUint32(0, 0x89ab_cdef, true);

    expect(d.call(OP_PRESENT_FENCE, payload)).toEqual({
      ok: true,
      value: 4,
    });
    expect(host.posted).toEqual([
      { kind: "fb:present-fence", serial: 0x89ab_cdef },
    ]);
  });

  it("OP_PRESENT_FENCE rejects zero and non-exact payloads before posting", () => {
    for (const payload of [
      new Uint8Array(0),
      new Uint8Array(3),
      new Uint8Array(4),
      new Uint8Array(5),
    ]) {
      const host = makeHost();
      const d = new FramebufferDriver();
      d.init(host);

      expect(d.call(OP_PRESENT_FENCE, payload)).toEqual({
        ok: false,
        error: DriverErrorCode.Transport,
      });
      expect(host.posted).toHaveLength(0);
    }
  });

  it("OP_BLIT posts an fb:blit message with width, height, and the rgba payload", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);

    // 2x2 RGBA8: four 4-byte pixels.
    const pixels = new Uint8Array([
      0xff, 0x00, 0x00, 0xff, // red
      0x00, 0xff, 0x00, 0xff, // green
      0x00, 0x00, 0xff, 0xff, // blue
      0xff, 0xff, 0xff, 0xff, // white
    ]);
    const payload = packBlit(2, 2, pixels);
    const result = d.call(OP_BLIT, payload);
    expect(result.ok).toBe(true);

    const msg = host.posted[0] as {
      kind: string;
      width: number;
      height: number;
      rgba: Uint8Array;
    };
    expect(msg.kind).toBe("fb:blit");
    expect(msg.width).toBe(2);
    expect(msg.height).toBe(2);
    expect(msg.rgba.byteLength).toBe(16);
    expect(Array.from(msg.rgba)).toEqual(Array.from(pixels));
  });

  it("OP_BLIT copies the rgba bytes defensively so the kernel can reuse its buffer", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);

    const pixels = new Uint8Array([0xaa, 0xbb, 0xcc, 0xdd]);
    const payload = packBlit(1, 1, pixels);
    d.call(OP_BLIT, payload);

    // Mutate the kernel-side payload buffer afterwards — the
    // driver's stored message must not be affected.
    payload[8] = 0x00;
    const msg = host.posted[0] as { rgba: Uint8Array };
    expect(msg.rgba[0]).toBe(0xaa);
  });

  it("OP_BLIT with a short payload (no pixels at all) returns Transport", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);

    const result = d.call(OP_BLIT, new Uint8Array(4));
    expect(result).toEqual({ ok: false, error: DriverErrorCode.Transport });
    expect(host.posted).toHaveLength(0);
  });

  it("OP_BLIT with a geometry that doesn't match the pixel count returns Transport", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);

    // Claim 2x2 but provide only one pixel worth of RGBA (4 bytes, not 16).
    const payload = packBlit(2, 2, new Uint8Array([0xff, 0xff, 0xff, 0xff]));
    const result = d.call(OP_BLIT, payload);
    expect(result).toEqual({ ok: false, error: DriverErrorCode.Transport });
    expect(host.posted).toHaveLength(0);
  });

  it("unknown opcode returns Transport", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);

    const result = d.call(0xff, new Uint8Array(16));
    expect(result).toEqual({ ok: false, error: DriverErrorCode.Transport });
    expect(host.posted).toHaveLength(0);
  });

  it("OP_SET_MODE with a zero geometry is accepted (caller responsibility)", () => {
    // The driver is structurally correct here — a 0x0 canvas
    // might be valid for "blank me". Higher layers may still
    // reject it; not the driver's job.
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);
    const result = d.call(OP_SET_MODE, packGeometry(0, 0));
    expect(result.ok).toBe(true);
    const msg = host.posted[0] as { width: number; height: number };
    expect(msg.width).toBe(0);
    expect(msg.height).toBe(0);
  });

  it("OP_BLIT with a zero-pixel 0x0 blit posts an empty fb:blit message", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);
    // 0 * 0 * 4 == 0 pixel bytes; payload is just the 8-byte header.
    const payload = packBlit(0, 0, new Uint8Array(0));
    const result = d.call(OP_BLIT, payload);
    expect(result.ok).toBe(true);
    const msg = host.posted[0] as {
      width: number;
      height: number;
      rgba: Uint8Array;
    };
    expect(msg.width).toBe(0);
    expect(msg.height).toBe(0);
    expect(msg.rgba.byteLength).toBe(0);
  });

  it("OP_PATCH posts an owned fb:patch with little-endian geometry", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);
    const pixels = new Uint8Array([
      1, 2, 3, 4,
      5, 6, 7, 8,
    ]);
    const payload = packPatch(11, 13, 2, 1, pixels);

    expect(d.call(OP_PATCH, payload)).toEqual({
      ok: true,
      value: payload.byteLength,
    });
    expect(host.posted).toHaveLength(1);
    const message = host.posted[0] as {
      kind: string;
      x: number;
      y: number;
      width: number;
      height: number;
      rgba: Uint8Array;
    };
    expect(message).toMatchObject({
      kind: "fb:patch",
      x: 11,
      y: 13,
      width: 2,
      height: 1,
    });
    expect(Array.from(message.rgba)).toEqual(Array.from(pixels));

    payload[16] = 0xff;
    expect(message.rgba[0]).toBe(1);
  });

  it("OP_PATCH accepts the largest whole-pixel body under the syscall cap", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);
    const pixels = Math.floor(FB_PATCH_MAX_RGBA_BYTES / 4);
    const payload = packPatch(0, 0, pixels, 1, new Uint8Array(pixels * 4));

    expect(d.call(OP_PATCH, payload).ok).toBe(true);
    expect(host.posted).toHaveLength(1);
  });

  it("OP_PATCH rejects malformed, empty, oversized, and overflowing rectangles", () => {
    const invalidPayloads = [
      new Uint8Array(15),
      packPatch(0, 0, 1, 1, new Uint8Array(3)),
      packPatch(0, 0, 0, 1, new Uint8Array(0)),
      packPatch(0xffff_ffff, 0, 1, 1, new Uint8Array(4)),
      packPatch(
        0,
        0,
        Math.floor(FB_PATCH_MAX_RGBA_BYTES / 4) + 1,
        1,
        new Uint8Array(
          (Math.floor(FB_PATCH_MAX_RGBA_BYTES / 4) + 1) * 4,
        ),
      ),
    ];

    for (const payload of invalidPayloads) {
      const host = makeHost();
      const d = new FramebufferDriver();
      d.init(host);
      expect(d.call(OP_PATCH, payload)).toEqual({
        ok: false,
        error: DriverErrorCode.Transport,
      });
      expect(host.posted).toHaveLength(0);
    }
  });

  it("OP_PATCH_RLE preflights and expands one bounded patch presentation", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);
    expect(d.call(OP_SET_MODE, packGeometry(4, 3)).ok).toBe(true);
    host.posted.length = 0;
    const payload = packRlePatch(1, 1, 2, 2, [
      { count: 3, rgba: [10, 20, 30, 255] },
      { count: 1, rgba: [40, 50, 60, 255] },
    ]);

    expect(d.call(OP_PATCH_RLE, payload)).toEqual({
      ok: true,
      value: payload.byteLength,
    });
    expect(host.posted).toHaveLength(1);
    const message = host.posted[0] as {
      kind: string;
      x: number;
      y: number;
      width: number;
      height: number;
      rgba: Uint8Array;
    };
    expect(message).toMatchObject({
      kind: "fb:patch",
      x: 1,
      y: 1,
      width: 2,
      height: 2,
    });
    expect(Array.from(message.rgba)).toEqual([
      10, 20, 30, 255,
      10, 20, 30, 255,
      10, 20, 30, 255,
      40, 50, 60, 255,
    ]);
  });

  it("OP_PATCH_RLE rejects malformed streams and mode overflow before posting", () => {
    const invalidPayloads = [
      new Uint8Array(16),
      packRlePatch(0, 0, 1, 1, [{ count: 0, rgba: [1, 2, 3, 4] }]),
      packRlePatch(0, 0, 2, 1, [{ count: 1, rgba: [1, 2, 3, 4] }]),
      packRlePatch(0, 0, 1, 1, [{ count: 2, rgba: [1, 2, 3, 4] }]),
      packRlePatch(3, 2, 2, 1, [{ count: 2, rgba: [1, 2, 3, 4] }]),
      Uint8Array.from([
        ...packRlePatch(0, 0, 1, 1, [{ count: 1, rgba: [1, 2, 3, 4] }]),
        0xff,
      ]),
    ];

    for (const payload of invalidPayloads) {
      const host = makeHost();
      const d = new FramebufferDriver();
      d.init(host);
      expect(d.call(OP_SET_MODE, packGeometry(4, 3)).ok).toBe(true);
      host.posted.length = 0;
      expect(d.call(OP_PATCH_RLE, payload)).toEqual({
        ok: false,
        error: DriverErrorCode.Transport,
      });
      expect(host.posted).toHaveLength(0);
    }

    const noModeHost = makeHost();
    const withoutMode = new FramebufferDriver();
    withoutMode.init(noModeHost);
    const valid = packRlePatch(0, 0, 1, 1, [
      { count: 1, rgba: [1, 2, 3, 4] },
    ]);
    expect(withoutMode.call(OP_PATCH_RLE, valid)).toEqual({
      ok: false,
      error: DriverErrorCode.Transport,
    });
    expect(noModeHost.posted).toHaveLength(0);
  });

  it("OP_PATCH_PALETTE_RLE_BATCH expands disjoint rectangles into one atomic message", () => {
    const host = makeHost();
    const d = new FramebufferDriver();
    d.init(host);
    expect(d.call(OP_SET_MODE, packGeometry(4, 3)).ok).toBe(true);
    host.posted.length = 0;
    const payload = packPaletteRleBatch(
      [
        [10, 20, 30, 255],
        [40, 50, 60, 255],
      ],
      [
        { x: 0, y: 0, width: 2, height: 1, runs: [{ count: 2, index: 0 }] },
        {
          x: 3,
          y: 1,
          width: 1,
          height: 2,
          runs: [
            { count: 1, index: 1 },
            { count: 1, index: 0 },
          ],
        },
      ],
    );

    expect(d.call(OP_PATCH_PALETTE_RLE_BATCH, payload)).toEqual({
      ok: true,
      value: payload.byteLength,
    });
    expect(host.posted).toHaveLength(1);
    const message = host.posted[0] as {
      kind: string;
      patches: Array<{ x: number; y: number; width: number; height: number; rgba: Uint8Array }>;
    };
    expect(message.kind).toBe("fb:patch-batch");
    expect(message.patches).toHaveLength(2);
    expect(message.patches[0]).toMatchObject({ x: 0, y: 0, width: 2, height: 1 });
    expect(Array.from(message.patches[0]!.rgba)).toEqual([
      10, 20, 30, 255, 10, 20, 30, 255,
    ]);
    expect(message.patches[1]).toMatchObject({ x: 3, y: 1, width: 1, height: 2 });
    expect(Array.from(message.patches[1]!.rgba)).toEqual([
      40, 50, 60, 255, 10, 20, 30, 255,
    ]);
  });

  it("OP_PATCH_PALETTE_RLE_BATCH rejects malformed or unbounded batches before posting", () => {
    const color = [[1, 2, 3, 255]] as const;
    const oneRect = packPaletteRleBatch(color, [
      { x: 0, y: 0, width: 1, height: 1, runs: [{ count: 1, index: 0 }] },
    ]);
    const valid = packPaletteRleBatch(color, [
      { x: 0, y: 0, width: 1, height: 1, runs: [{ count: 1, index: 0 }] },
      { x: 1, y: 0, width: 1, height: 1, runs: [{ count: 1, index: 0 }] },
    ]);
    const zeroRun = valid.slice();
    new DataView(zeroRun.buffer).setUint16(22, 0, true);
    const badIndex = valid.slice();
    badIndex[24] = 1;
    const tooMuchDecoded = packPaletteRleBatch(color, [
      { x: 0, y: 0, width: 4, height: 3, runs: [{ count: 12, index: 0 }] },
      { x: 0, y: 0, width: 4, height: 3, runs: [{ count: 12, index: 0 }] },
    ]);
    const invalidPayloads = [
      oneRect,
      zeroRun,
      badIndex,
      valid.slice(0, -1),
      Uint8Array.from([...valid, 0xff]),
      tooMuchDecoded,
    ];

    for (const payload of invalidPayloads) {
      const host = makeHost();
      const d = new FramebufferDriver();
      d.init(host);
      expect(d.call(OP_SET_MODE, packGeometry(4, 3)).ok).toBe(true);
      host.posted.length = 0;
      expect(d.call(OP_PATCH_PALETTE_RLE_BATCH, payload)).toEqual({
        ok: false,
        error: DriverErrorCode.Transport,
      });
      expect(host.posted).toHaveLength(0);
    }
  });
});
