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
  OP_BLIT,
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
});
