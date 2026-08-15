// Framebuffer driver — TS half of /dev/fb0.
//
// The Rust kernel's `DeviceDispatcher` forwards writes to
// `/dev/fb0` through the `Platform::driver_call(Framebuffer,
// ...)` path with an opaque byte payload; this module
// decodes the payload, repackages it as a structured main-
// thread message, and posts it via `DriverHost::postToMain`.
//
// The main-thread `FbHost` + `FbRenderer` pair consumes these
// messages and repaints the visible canvas. This driver is the
// boundary between kernel and main thread, so the byte payload
// below is the only format the two sides must agree on.
//
// Framebuffer opcodes in v1:
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
//   * `OP_PATCH` — 16-byte header `(x: u32, y: u32, width:
//     u32, height: u32)`, then tightly packed row-major
//     `width * height * 4` RGBA8 bytes. A patch is bounded by
//     the user-process syscall heap: the opcode byte plus this
//     payload must fit in 32 KiB.
//
//   * `OP_PATCH_RLE` — the same 16-byte geometry followed by
//     `(count: u32, rgba: 4 bytes)` runs. The driver validates the complete
//     stream against the current mode before allocating or presenting.
//   * `OP_PATCH_PALETTE_RLE_BATCH` — multiple rectangles sharing one RGBA8
//     palette, each followed by `(count: u16, palette_index: u8)` runs. The
//     complete batch validates and reaches main as one atomic presentation.
//   * `OP_PRESENT_FENCE` — exact 4-byte payload containing a non-zero little-
//     endian serial. The display server emits it only after the corresponding
//     presentation boundary has settled; main receives it as a typed event.
//
// The driver has no input path: fb0 is write-only. Unknown
// opcodes and malformed payloads both return `Transport`
// (matching the kernel-side rejection in
// `DeviceDispatcher::framebuffer_write`).

import type { Driver, DriverHost, DriverResult } from "./types";
import { DriverErrorCode } from "./types";
import { Devnum, DriverId } from "../shared/platform-constants";
import { HEAP_SCRATCH_BYTES } from "../shared/sab-layout";

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
/** Paint one independently presentable rectangular RGBA8 patch. */
export const OP_PATCH = 0x06;
/** Paint one independently presentable run-length encoded RGBA8 patch. */
export const OP_PATCH_RLE = 0x07;
export const OP_PATCH_PALETTE_RLE_BATCH = 0x08;
export const OP_PRESENT_FENCE = 0x09;

export const FB_PATCH_HEADER_BYTES = 16;
/**
 * `fd_write` includes the one-byte opcode in its 32 KiB syscall payload.
 * `FramebufferDriver.call` receives the bytes after that opcode.
 */
export const FB_PATCH_MAX_PAYLOAD_BYTES = HEAP_SCRATCH_BYTES - 1;
export const FB_PATCH_MAX_RGBA_BYTES =
  FB_PATCH_MAX_PAYLOAD_BYTES - FB_PATCH_HEADER_BYTES;
export const FB_PATCH_BATCH_MAX_RECTS = 8;

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

/** Main-thread-bound rectangular RGBA8 update. */
export interface FbPatchMessage {
  readonly kind: "fb:patch";
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
  readonly rgba: Uint8Array;
}

/** One rectangle carried inside an atomic framebuffer batch. */
export interface FbPatchData {
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
  readonly rgba: Uint8Array;
}

/** Main-thread-bound collection painted as one presentation. */
export interface FbPatchBatchMessage {
  readonly kind: "fb:patch-batch";
  readonly patches: readonly FbPatchData[];
}

/** Main-thread-bound presentation boundary acknowledged by the display server. */
export interface FbPresentFenceMessage {
  readonly kind: "fb:present-fence";
  readonly serial: number;
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
  ) >>> 0;
}

function readU16LE(bytes: Uint8Array, offset: number): number {
  return (bytes[offset] ?? 0) | ((bytes[offset + 1] ?? 0) << 8);
}

function patchRgbaByteCount(width: number, height: number): number | null {
  if (width === 0 || height === 0) {
    return null;
  }
  const pixels = width * height;
  if (!Number.isSafeInteger(pixels)) {
    return null;
  }
  const bytes = pixels * 4;
  return bytes <= FB_PATCH_MAX_RGBA_BYTES ? bytes : null;
}

function patchGeometryDoesNotOverflow(
  x: number,
  y: number,
  width: number,
  height: number,
): boolean {
  const maxU32 = 0xffff_ffff;
  return x + width <= maxU32 && y + height <= maxU32;
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
  private modeWidth = 0;
  private modeHeight = 0;

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
      case OP_PATCH:
        return this.handlePatch(host, payload);
      case OP_PATCH_RLE:
        return this.handleRlePatch(host, payload);
      case OP_PATCH_PALETTE_RLE_BATCH:
        return this.handlePaletteRlePatchBatch(host, payload);
      case OP_PRESENT_FENCE:
        return this.handlePresentFence(host, payload);
      default:
        return { ok: false, error: DriverErrorCode.Transport };
    }
  }

  private handlePresentFence(
    host: DriverHost,
    payload: Uint8Array,
  ): DriverResult {
    if (payload.byteLength !== 4) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const serial = readU32LE(payload, 0);
    if (serial === 0) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const message: FbPresentFenceMessage = {
      kind: "fb:present-fence",
      serial,
    };
    host.postToMain(message);
    return { ok: true, value: payload.byteLength };
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
    this.modeWidth = width;
    this.modeHeight = height;
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

  private handlePatch(host: DriverHost, payload: Uint8Array): DriverResult {
    if (
      payload.byteLength < FB_PATCH_HEADER_BYTES ||
      payload.byteLength > FB_PATCH_MAX_PAYLOAD_BYTES
    ) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const x = readU32LE(payload, 0);
    const y = readU32LE(payload, 4);
    const width = readU32LE(payload, 8);
    const height = readU32LE(payload, 12);
    const needed = patchRgbaByteCount(width, height);
    if (
      needed === null ||
      !patchGeometryDoesNotOverflow(x, y, width, height) ||
      payload.byteLength !== FB_PATCH_HEADER_BYTES + needed
    ) {
      return { ok: false, error: DriverErrorCode.Transport };
    }

    const rgba = new Uint8Array(needed);
    rgba.set(payload.subarray(FB_PATCH_HEADER_BYTES));
    const message: FbPatchMessage = {
      kind: "fb:patch",
      x,
      y,
      width,
      height,
      rgba,
    };
    host.postToMain(message);
    return { ok: true, value: payload.byteLength };
  }

  private handleRlePatch(host: DriverHost, payload: Uint8Array): DriverResult {
    if (
      payload.byteLength < FB_PATCH_HEADER_BYTES + 8 ||
      payload.byteLength > FB_PATCH_MAX_PAYLOAD_BYTES ||
      (payload.byteLength - FB_PATCH_HEADER_BYTES) % 8 !== 0
    ) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const x = readU32LE(payload, 0);
    const y = readU32LE(payload, 4);
    const width = readU32LE(payload, 8);
    const height = readU32LE(payload, 12);
    const patchPixels = width * height;
    const modePixels = this.modeWidth * this.modeHeight;
    if (
      width === 0 ||
      height === 0 ||
      !Number.isSafeInteger(patchPixels) ||
      !Number.isSafeInteger(modePixels) ||
      patchPixels > Math.floor(Number.MAX_SAFE_INTEGER / 4) ||
      modePixels > Math.floor(Number.MAX_SAFE_INTEGER / 4) ||
      x + width > this.modeWidth ||
      y + height > this.modeHeight
    ) {
      return { ok: false, error: DriverErrorCode.Transport };
    }

    // Validate every run and the exact decoded pixel sum before allocating or
    // posting. A malformed suffix can never partially update the framebuffer.
    let encodedOffset = FB_PATCH_HEADER_BYTES;
    let decodedPixels = 0;
    while (encodedOffset < payload.byteLength) {
      const count = readU32LE(payload, encodedOffset);
      if (count === 0 || count > patchPixels - decodedPixels) {
        return { ok: false, error: DriverErrorCode.Transport };
      }
      decodedPixels += count;
      encodedOffset += 8;
    }
    if (decodedPixels !== patchPixels) {
      return { ok: false, error: DriverErrorCode.Transport };
    }

    const rgba = new Uint8Array(patchPixels * 4);
    encodedOffset = FB_PATCH_HEADER_BYTES;
    let destination = 0;
    while (encodedOffset < payload.byteLength) {
      const count = readU32LE(payload, encodedOffset);
      const red = payload[encodedOffset + 4] ?? 0;
      const green = payload[encodedOffset + 5] ?? 0;
      const blue = payload[encodedOffset + 6] ?? 0;
      const alpha = payload[encodedOffset + 7] ?? 0;
      for (let pixel = 0; pixel < count; pixel += 1) {
        rgba[destination] = red;
        rgba[destination + 1] = green;
        rgba[destination + 2] = blue;
        rgba[destination + 3] = alpha;
        destination += 4;
      }
      encodedOffset += 8;
    }
    const message: FbPatchMessage = {
      kind: "fb:patch",
      x,
      y,
      width,
      height,
      rgba,
    };
    host.postToMain(message);
    return { ok: true, value: payload.byteLength };
  }

  private handlePaletteRlePatchBatch(
    host: DriverHost,
    payload: Uint8Array,
  ): DriverResult {
    const reject = (): DriverResult => ({
      ok: false,
      error: DriverErrorCode.Transport,
    });
    if (payload.byteLength < 2 || payload.byteLength > FB_PATCH_MAX_PAYLOAD_BYTES) {
      return reject();
    }
    const rectCount = payload[0] ?? 0;
    const paletteCount = (payload[1] ?? 0) + 1;
    if (rectCount < 2 || rectCount > FB_PATCH_BATCH_MAX_RECTS) {
      return reject();
    }
    const paletteStart = 2;
    const paletteEnd = paletteStart + paletteCount * 4;
    const modePixels = this.modeWidth * this.modeHeight;
    if (
      paletteEnd > payload.byteLength ||
      this.modeWidth === 0 ||
      this.modeHeight === 0 ||
      !Number.isSafeInteger(modePixels) ||
      modePixels > Math.floor(Number.MAX_SAFE_INTEGER / 4)
    ) {
      return reject();
    }

    const encodedPatches: Array<{
      readonly x: number;
      readonly y: number;
      readonly width: number;
      readonly height: number;
      readonly pixelCount: number;
      readonly runsStart: number;
      readonly runsEnd: number;
    }> = [];
    let encodedOffset = paletteEnd;
    let totalPixels = 0;
    for (let rect = 0; rect < rectCount; rect += 1) {
      if (encodedOffset + FB_PATCH_HEADER_BYTES > payload.byteLength) {
        return reject();
      }
      const x = readU32LE(payload, encodedOffset);
      const y = readU32LE(payload, encodedOffset + 4);
      const width = readU32LE(payload, encodedOffset + 8);
      const height = readU32LE(payload, encodedOffset + 12);
      encodedOffset += FB_PATCH_HEADER_BYTES;
      const pixelCount = width * height;
      if (
        width === 0 ||
        height === 0 ||
        !Number.isSafeInteger(pixelCount) ||
        pixelCount > Math.floor(Number.MAX_SAFE_INTEGER / 4) ||
        x + width > this.modeWidth ||
        y + height > this.modeHeight
      ) {
        return reject();
      }
      totalPixels += pixelCount;
      if (!Number.isSafeInteger(totalPixels) || totalPixels > modePixels) {
        return reject();
      }

      const runsStart = encodedOffset;
      let decodedPixels = 0;
      while (decodedPixels < pixelCount) {
        if (encodedOffset + 3 > payload.byteLength) {
          return reject();
        }
        const count = readU16LE(payload, encodedOffset);
        const paletteIndex = payload[encodedOffset + 2] ?? paletteCount;
        if (
          count === 0 ||
          paletteIndex >= paletteCount ||
          count > pixelCount - decodedPixels
        ) {
          return reject();
        }
        decodedPixels += count;
        encodedOffset += 3;
      }
      encodedPatches.push({
        x,
        y,
        width,
        height,
        pixelCount,
        runsStart,
        runsEnd: encodedOffset,
      });
    }
    if (encodedOffset !== payload.byteLength) {
      return reject();
    }

    const patches: FbPatchData[] = encodedPatches.map((encoded) => {
      const rgba = new Uint8Array(encoded.pixelCount * 4);
      let runOffset = encoded.runsStart;
      let destination = 0;
      while (runOffset < encoded.runsEnd) {
        const count = readU16LE(payload, runOffset);
        const paletteIndex = payload[runOffset + 2] ?? 0;
        const colorOffset = paletteStart + paletteIndex * 4;
        const red = payload[colorOffset] ?? 0;
        const green = payload[colorOffset + 1] ?? 0;
        const blue = payload[colorOffset + 2] ?? 0;
        const alpha = payload[colorOffset + 3] ?? 0;
        for (let pixel = 0; pixel < count; pixel += 1) {
          rgba[destination] = red;
          rgba[destination + 1] = green;
          rgba[destination + 2] = blue;
          rgba[destination + 3] = alpha;
          destination += 4;
        }
        runOffset += 3;
      }
      return {
        x: encoded.x,
        y: encoded.y,
        width: encoded.width,
        height: encoded.height,
        rgba,
      };
    });
    const message: FbPatchBatchMessage = { kind: "fb:patch-batch", patches };
    host.postToMain(message);
    return { ok: true, value: payload.byteLength };
  }
}
