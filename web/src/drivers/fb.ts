// Framebuffer driver — TS half of /dev/fb0.
//
// The Rust kernel's `DeviceDispatcher` forwards writes to
// `/dev/fb0` through the `Platform::driver_call(Framebuffer,
// ...)` path with an opaque byte payload; this module
// decodes the payload, repackages it as a structured main-
// thread message, and posts it via `DriverHost::postToMain`.
//
// A later main-thread `FbHost` module owns an
// `OffscreenCanvas` (or, on browsers without it, a plain
// `<canvas>` + `putImageData`) and consumes these messages to
// repaint the visible surface. That module is scheduled for a
// follow-up slice; this driver is the boundary between kernel
// and main-thread and is designed so the payload format is
// the only thing the two sides must agree on.
//
// Two opcodes in v1:
//
//   * `OP_SET_MODE` — 8-byte payload: `(width: u32, height:
//     u32)` both little-endian. Sets the logical framebuffer
//     geometry. The main thread typically resizes its canvas
//     backing store on receipt.
//   * `OP_BLIT` — header+pixels payload: 8-byte header
//     `(width: u32, height: u32)`, then `width * height * 4`
//     bytes of RGBA8. The main thread draws the pixels at
//     the origin `(0, 0)` of the current surface.
//
// Partial-region blits (an offset + source rect) are out of
// scope for v1; the display server's commit pipeline will
// introduce them when it needs sub-surface damage tracking.
//
// The driver has no input path: fb0 is write-only. Unknown
// opcodes and malformed payloads both return `Transport`
// (matching the kernel-side rejection in
// `DeviceDispatcher::framebuffer_write`).

import type { Driver, DriverHost, DriverResult } from "./types";
import { DriverErrorCode } from "./types";
import { Devnum, DriverId } from "../shared/platform-constants";

/** Driver-class id for the framebuffer. */
export const FB_DRIVER_ID = DriverId.Framebuffer;

/** Device-node number for /dev/fb0. */
export const DEV_FB0_NODE = Devnum.Fb0;

export const OP_SET_MODE = 0x01;
export const OP_BLIT = 0x02;

/** Main-thread-bound: "resize the logical framebuffer". */
export interface FbSetModeMessage {
  readonly kind: "fb:set-mode";
  readonly width: number;
  readonly height: number;
}

/** Main-thread-bound: "here are `width * height * 4` bytes of RGBA8". */
export interface FbBlitMessage {
  readonly kind: "fb:blit";
  readonly width: number;
  readonly height: number;
  readonly rgba: Uint8Array;
}

/** Expected pixel bytes for a given geometry at RGBA8. */
function rgbaByteCount(width: number, height: number): number {
  return width * height * 4;
}

/** Read a little-endian u32 from `bytes` starting at `offset`. */
function readU32LE(bytes: Uint8Array, offset: number): number {
  // We use Number() math so this stays a regular JS number
  // (safe up to 2^53). Width/height will never approach
  // 2^32 in practice.
  return (
    (bytes[offset] ?? 0) |
    ((bytes[offset + 1] ?? 0) << 8) |
    ((bytes[offset + 2] ?? 0) << 16) |
    ((bytes[offset + 3] ?? 0) * 0x0100_0000)
  );
}

export class FramebufferDriver implements Driver {
  readonly driverId = FB_DRIVER_ID;
  readonly name = "framebuffer";
  private host: DriverHost | undefined;

  init(host: DriverHost): void {
    this.host = host;
  }

  call(op: number, payload: Uint8Array): DriverResult {
    const host = this.host;
    if (!host) {
      return { ok: false, error: DriverErrorCode.NotReady };
    }
    switch (op) {
      case OP_SET_MODE:
        return this.handleSetMode(host, payload);
      case OP_BLIT:
        return this.handleBlit(host, payload);
      default:
        return { ok: false, error: DriverErrorCode.Transport };
    }
  }

  // Framebuffer is write-only; no input route.

  private handleSetMode(host: DriverHost, payload: Uint8Array): DriverResult {
    if (payload.byteLength < 8) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const width = readU32LE(payload, 0);
    const height = readU32LE(payload, 4);
    const message: FbSetModeMessage = { kind: "fb:set-mode", width, height };
    host.postToMain(message);
    return { ok: true, value: 8 };
  }

  private handleBlit(host: DriverHost, payload: Uint8Array): DriverResult {
    if (payload.byteLength < 8) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const width = readU32LE(payload, 0);
    const height = readU32LE(payload, 4);
    const needed = rgbaByteCount(width, height);
    const pixelBytes = payload.byteLength - 8;
    if (pixelBytes !== needed) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    // Defensive copy: the kernel may recycle its payload
    // immediately after the driver returns.
    const rgba = new Uint8Array(needed);
    rgba.set(payload.subarray(8));
    const message: FbBlitMessage = { kind: "fb:blit", width, height, rgba };
    host.postToMain(message);
    return { ok: true, value: payload.byteLength };
  }
}
