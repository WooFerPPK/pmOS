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
/// Begin a chunked BLIT. Payload: `(width: u32, height: u32)`.
/// Allocates an internal `width*height*4` byte buffer; subsequent
/// `OP_BLIT_CHUNK` calls fill it; `OP_BLIT_END` posts the buffer
/// to main as a single `fb:blit` message. Used by the display
/// server's `present_framebuffer` because the SAB ring's
/// per-syscall heap window (`HEAP_SCRATCH_BYTES`, 32 KiB) is
/// smaller than a full 800x600+ frame.
export const OP_BLIT_BEGIN = 0x03;
/// Append pixels to the in-progress BLIT. Payload:
/// `(offset: u32) || pixel_bytes`. The driver writes
/// `pixel_bytes` into the accumulator buffer at `offset`. Sender
/// chunks the frame into SAB-ring-sized pieces.
export const OP_BLIT_CHUNK = 0x04;
/// Finalize the in-progress BLIT — post the accumulated buffer
/// to main as `fb:blit` and reset internal state. Payload empty.
export const OP_BLIT_END = 0x05;

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
  // Chunked-blit accumulator: allocated by OP_BLIT_BEGIN,
  // filled by OP_BLIT_CHUNK, posted + cleared by OP_BLIT_END.
  private blitBuffer: Uint8Array | null = null;
  private blitWidth = 0;
  private blitHeight = 0;

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
      case OP_BLIT_BEGIN:
        return this.handleBlitBegin(payload);
      case OP_BLIT_CHUNK:
        return this.handleBlitChunk(payload);
      case OP_BLIT_END:
        return this.handleBlitEnd(host);
      default:
        return { ok: false, error: DriverErrorCode.Transport };
    }
  }

  private handleBlitBegin(payload: Uint8Array): DriverResult {
    if (payload.byteLength < 8) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const width = readU32LE(payload, 0);
    const height = readU32LE(payload, 4);
    const needed = rgbaByteCount(width, height);
    this.blitBuffer = new Uint8Array(needed);
    this.blitWidth = width;
    this.blitHeight = height;
    return { ok: true, value: 8 };
  }

  private handleBlitChunk(payload: Uint8Array): DriverResult {
    if (payload.byteLength < 4 || this.blitBuffer === null) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const offset = readU32LE(payload, 0);
    const data = payload.subarray(4);
    if (offset + data.byteLength > this.blitBuffer.byteLength) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    this.blitBuffer.set(data, offset);
    return { ok: true, value: payload.byteLength };
  }

  private handleBlitEnd(host: DriverHost): DriverResult {
    if (this.blitBuffer === null) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const message: FbBlitMessage = {
      kind: "fb:blit",
      width: this.blitWidth,
      height: this.blitHeight,
      rgba: this.blitBuffer,
    };
    host.postToMain(message);
    this.blitBuffer = null;
    this.blitWidth = 0;
    this.blitHeight = 0;
    return { ok: true, value: 0 };
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
    const rgba = new Uint8Array(needed);
    rgba.set(payload.subarray(8));
    const message: FbBlitMessage = { kind: "fb:blit", width, height, rgba };
    host.postToMain(message);
    return { ok: true, value: payload.byteLength };
  }
}
