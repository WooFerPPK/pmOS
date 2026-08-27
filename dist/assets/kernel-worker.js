var __defProp = Object.defineProperty;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __esm = (fn, res, err) => function __init() {
  if (err) throw err[0];
  try {
    return fn && (res = (0, fn[__getOwnPropNames(fn)[0]])(fn = 0)), res;
  } catch (e) {
    throw err = [e], e;
  }
};
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, { get: all[name], enumerable: true });
};

// src/drivers/types.ts
var DriverErrorCode;
var init_types = __esm({
  "src/drivers/types.ts"() {
    "use strict";
    DriverErrorCode = {
      /** Driver isn't wired up yet or its backing resource is gone. */
      NotReady: 1,
      /** Transport error: bad payload, invalid opcode, etc. */
      Transport: 2,
      /** The driver reports a POSIX errno to the kernel. */
      Errno: 3
    };
  }
});

// src/shared/platform-constants.ts
var DriverId, Devnum;
var init_platform_constants = __esm({
  "src/shared/platform-constants.ts"() {
    "use strict";
    DriverId = {
      Framebuffer: 0,
      InputKbd: 1,
      InputMouse: 2,
      Block: 3,
      Net: 4,
      Console: 5
    };
    Devnum = {
      Null: 1,
      Zero: 2,
      Random: 3,
      Console: 4,
      Fb0: 10,
      InputKbd: 20,
      InputMouse: 21
    };
  }
});

// src/shared/sab-layout.ts
var SAB_SIZE, OFF_REQ_HEAD, OFF_REQ_TAIL, OFF_RES_HEAD, OFF_RES_TAIL, OFF_USER_WAIT_SLOT, OFF_REQ_RING, OFF_RES_RING, OFF_HEAP_SCRATCH, HEAP_SCRATCH_BYTES, REQ_SLOT_COUNT, RES_SLOT_COUNT, STATUS_READY;
var init_sab_layout = __esm({
  "src/shared/sab-layout.ts"() {
    "use strict";
    SAB_SIZE = 65536;
    OFF_REQ_HEAD = 0;
    OFF_REQ_TAIL = 4;
    OFF_RES_HEAD = 8;
    OFF_RES_TAIL = 12;
    OFF_USER_WAIT_SLOT = 16;
    OFF_REQ_RING = 64;
    OFF_RES_RING = 16384;
    OFF_HEAP_SCRATCH = 32768;
    HEAP_SCRATCH_BYTES = 32768;
    REQ_SLOT_COUNT = 510;
    RES_SLOT_COUNT = 510;
    STATUS_READY = 3;
  }
});

// src/drivers/fb.ts
var fb_exports = {};
__export(fb_exports, {
  DEV_FB0_NODE: () => DEV_FB0_NODE,
  FB_DRIVER_ID: () => FB_DRIVER_ID,
  FB_PATCH_BATCH_MAX_RECTS: () => FB_PATCH_BATCH_MAX_RECTS,
  FB_PATCH_HEADER_BYTES: () => FB_PATCH_HEADER_BYTES,
  FB_PATCH_MAX_PAYLOAD_BYTES: () => FB_PATCH_MAX_PAYLOAD_BYTES,
  FB_PATCH_MAX_RGBA_BYTES: () => FB_PATCH_MAX_RGBA_BYTES,
  FramebufferDriver: () => FramebufferDriver,
  OP_BLIT: () => OP_BLIT,
  OP_BLIT_BEGIN: () => OP_BLIT_BEGIN,
  OP_BLIT_CHUNK: () => OP_BLIT_CHUNK,
  OP_BLIT_END: () => OP_BLIT_END,
  OP_PATCH: () => OP_PATCH,
  OP_PATCH_PALETTE_RLE_BATCH: () => OP_PATCH_PALETTE_RLE_BATCH,
  OP_PATCH_RLE: () => OP_PATCH_RLE,
  OP_PRESENT_FENCE: () => OP_PRESENT_FENCE,
  OP_SET_MODE: () => OP_SET_MODE
});
function rgbaByteCount(width, height) {
  return width * height * 4;
}
function readU32LE(bytes, offset) {
  return ((bytes[offset] ?? 0) | (bytes[offset + 1] ?? 0) << 8 | (bytes[offset + 2] ?? 0) << 16 | (bytes[offset + 3] ?? 0) * 16777216) >>> 0;
}
function readU16LE(bytes, offset) {
  return (bytes[offset] ?? 0) | (bytes[offset + 1] ?? 0) << 8;
}
function patchRgbaByteCount(width, height) {
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
function patchGeometryDoesNotOverflow(x, y, width, height) {
  const maxU32 = 4294967295;
  return x + width <= maxU32 && y + height <= maxU32;
}
var FB_DRIVER_ID, DEV_FB0_NODE, OP_SET_MODE, OP_BLIT, OP_BLIT_BEGIN, OP_BLIT_CHUNK, OP_BLIT_END, OP_PATCH, OP_PATCH_RLE, OP_PATCH_PALETTE_RLE_BATCH, OP_PRESENT_FENCE, FB_PATCH_HEADER_BYTES, FB_PATCH_MAX_PAYLOAD_BYTES, FB_PATCH_MAX_RGBA_BYTES, FB_PATCH_BATCH_MAX_RECTS, FramebufferDriver;
var init_fb = __esm({
  "src/drivers/fb.ts"() {
    "use strict";
    init_types();
    init_platform_constants();
    init_sab_layout();
    FB_DRIVER_ID = DriverId.Framebuffer;
    DEV_FB0_NODE = Devnum.Fb0;
    OP_SET_MODE = 1;
    OP_BLIT = 2;
    OP_BLIT_BEGIN = 3;
    OP_BLIT_CHUNK = 4;
    OP_BLIT_END = 5;
    OP_PATCH = 6;
    OP_PATCH_RLE = 7;
    OP_PATCH_PALETTE_RLE_BATCH = 8;
    OP_PRESENT_FENCE = 9;
    FB_PATCH_HEADER_BYTES = 16;
    FB_PATCH_MAX_PAYLOAD_BYTES = HEAP_SCRATCH_BYTES - 1;
    FB_PATCH_MAX_RGBA_BYTES = FB_PATCH_MAX_PAYLOAD_BYTES - FB_PATCH_HEADER_BYTES;
    FB_PATCH_BATCH_MAX_RECTS = 8;
    FramebufferDriver = class {
      driverId = FB_DRIVER_ID;
      name = "framebuffer";
      host;
      // Chunked-blit accumulator: allocated by OP_BLIT_BEGIN,
      // filled by OP_BLIT_CHUNK, posted + cleared by OP_BLIT_END.
      blitBuffer = null;
      blitWidth = 0;
      blitHeight = 0;
      modeWidth = 0;
      modeHeight = 0;
      init(host) {
        this.host = host;
      }
      call(op, payload) {
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
      handlePresentFence(host, payload) {
        if (payload.byteLength !== 4) {
          return { ok: false, error: DriverErrorCode.Transport };
        }
        const serial = readU32LE(payload, 0);
        if (serial === 0) {
          return { ok: false, error: DriverErrorCode.Transport };
        }
        const message = {
          kind: "fb:present-fence",
          serial
        };
        host.postToMain(message);
        return { ok: true, value: payload.byteLength };
      }
      handleBlitBegin(payload) {
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
      handleBlitChunk(payload) {
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
      handleBlitEnd(host) {
        if (this.blitBuffer === null) {
          return { ok: false, error: DriverErrorCode.Transport };
        }
        const message = {
          kind: "fb:blit",
          width: this.blitWidth,
          height: this.blitHeight,
          rgba: this.blitBuffer
        };
        host.postToMain(message);
        this.blitBuffer = null;
        this.blitWidth = 0;
        this.blitHeight = 0;
        return { ok: true, value: 0 };
      }
      // Framebuffer is write-only; no input route.
      handleSetMode(host, payload) {
        if (payload.byteLength < 8) {
          return { ok: false, error: DriverErrorCode.Transport };
        }
        const width = readU32LE(payload, 0);
        const height = readU32LE(payload, 4);
        this.modeWidth = width;
        this.modeHeight = height;
        const message = { kind: "fb:set-mode", width, height };
        host.postToMain(message);
        return { ok: true, value: 8 };
      }
      handleBlit(host, payload) {
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
        const message = { kind: "fb:blit", width, height, rgba };
        host.postToMain(message);
        return { ok: true, value: payload.byteLength };
      }
      handlePatch(host, payload) {
        if (payload.byteLength < FB_PATCH_HEADER_BYTES || payload.byteLength > FB_PATCH_MAX_PAYLOAD_BYTES) {
          return { ok: false, error: DriverErrorCode.Transport };
        }
        const x = readU32LE(payload, 0);
        const y = readU32LE(payload, 4);
        const width = readU32LE(payload, 8);
        const height = readU32LE(payload, 12);
        const needed = patchRgbaByteCount(width, height);
        if (needed === null || !patchGeometryDoesNotOverflow(x, y, width, height) || payload.byteLength !== FB_PATCH_HEADER_BYTES + needed) {
          return { ok: false, error: DriverErrorCode.Transport };
        }
        const rgba = new Uint8Array(needed);
        rgba.set(payload.subarray(FB_PATCH_HEADER_BYTES));
        const message = {
          kind: "fb:patch",
          x,
          y,
          width,
          height,
          rgba
        };
        host.postToMain(message);
        return { ok: true, value: payload.byteLength };
      }
      handleRlePatch(host, payload) {
        if (payload.byteLength < FB_PATCH_HEADER_BYTES + 8 || payload.byteLength > FB_PATCH_MAX_PAYLOAD_BYTES || (payload.byteLength - FB_PATCH_HEADER_BYTES) % 8 !== 0) {
          return { ok: false, error: DriverErrorCode.Transport };
        }
        const x = readU32LE(payload, 0);
        const y = readU32LE(payload, 4);
        const width = readU32LE(payload, 8);
        const height = readU32LE(payload, 12);
        const patchPixels = width * height;
        const modePixels = this.modeWidth * this.modeHeight;
        if (width === 0 || height === 0 || !Number.isSafeInteger(patchPixels) || !Number.isSafeInteger(modePixels) || patchPixels > Math.floor(Number.MAX_SAFE_INTEGER / 4) || modePixels > Math.floor(Number.MAX_SAFE_INTEGER / 4) || x + width > this.modeWidth || y + height > this.modeHeight) {
          return { ok: false, error: DriverErrorCode.Transport };
        }
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
        const message = {
          kind: "fb:patch",
          x,
          y,
          width,
          height,
          rgba
        };
        host.postToMain(message);
        return { ok: true, value: payload.byteLength };
      }
      handlePaletteRlePatchBatch(host, payload) {
        const reject = () => ({
          ok: false,
          error: DriverErrorCode.Transport
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
        if (paletteEnd > payload.byteLength || this.modeWidth === 0 || this.modeHeight === 0 || !Number.isSafeInteger(modePixels) || modePixels > Math.floor(Number.MAX_SAFE_INTEGER / 4)) {
          return reject();
        }
        const encodedPatches = [];
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
          if (width === 0 || height === 0 || !Number.isSafeInteger(pixelCount) || pixelCount > Math.floor(Number.MAX_SAFE_INTEGER / 4) || x + width > this.modeWidth || y + height > this.modeHeight) {
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
            if (count === 0 || paletteIndex >= paletteCount || count > pixelCount - decodedPixels) {
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
            runsEnd: encodedOffset
          });
        }
        if (encodedOffset !== payload.byteLength) {
          return reject();
        }
        const patches = encodedPatches.map((encoded) => {
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
            rgba
          };
        });
        const message = { kind: "fb:patch-batch", patches };
        host.postToMain(message);
        return { ok: true, value: payload.byteLength };
      }
    };
  }
});

// src/drivers/block.ts
var block_exports = {};
__export(block_exports, {
  BLOCK_DRIVER_ID: () => BLOCK_DRIVER_ID,
  BLOCK_SIZE: () => BLOCK_SIZE,
  BlockDriver: () => BlockDriver,
  BlockImageState: () => BlockImageState,
  DEFAULT_BLOCK_COUNT: () => DEFAULT_BLOCK_COUNT,
  EINVAL: () => EINVAL,
  EIO: () => EIO,
  ENOSPC: () => ENOSPC,
  OP_BLOCK_COUNT: () => OP_BLOCK_COUNT,
  OP_FLUSH: () => OP_FLUSH,
  OP_IMAGE_STATE: () => OP_IMAGE_STATE,
  OP_READ: () => OP_READ,
  OP_WRITE: () => OP_WRITE,
  PMOS_IMG_FILENAME: () => PMOS_IMG_FILENAME
});
function readU64LE(bytes, offset) {
  const v = new DataView(bytes.buffer, bytes.byteOffset + offset, 8);
  const lo = v.getUint32(0, true);
  const hi = v.getUint32(4, true);
  if (hi > 2097151) {
    throw new Error(
      `BlockDriver: LBA ${hi}.${lo} exceeds JavaScript safe-integer range`
    );
  }
  return hi * 4294967296 + lo;
}
function isQuotaExceeded(e) {
  if (typeof e !== "object" || e === null) return false;
  const cand = e;
  return cand.name === "QuotaExceededError";
}
function isNotFound(e) {
  if (typeof e !== "object" || e === null) return false;
  const cand = e;
  return cand.name === "NotFoundError";
}
var BLOCK_DRIVER_ID, BLOCK_SIZE, ENOSPC, EIO, EINVAL, DEFAULT_BLOCK_COUNT, OP_BLOCK_COUNT, OP_READ, OP_WRITE, OP_FLUSH, OP_IMAGE_STATE, BlockImageState, PMOS_IMG_FILENAME, BlockDriver;
var init_block = __esm({
  "src/drivers/block.ts"() {
    "use strict";
    init_types();
    init_platform_constants();
    BLOCK_DRIVER_ID = DriverId.Block;
    BLOCK_SIZE = 4096;
    ENOSPC = 51;
    EIO = 5;
    EINVAL = 22;
    DEFAULT_BLOCK_COUNT = 4096;
    OP_BLOCK_COUNT = 1;
    OP_READ = 2;
    OP_WRITE = 3;
    OP_FLUSH = 4;
    OP_IMAGE_STATE = 5;
    BlockImageState = {
      Existing: 0,
      NewlyCreated: 1
    };
    PMOS_IMG_FILENAME = "pmos.img";
    BlockDriver = class _BlockDriver {
      constructor(handle, blockCount, imageState) {
        this.handle = handle;
        this.blockCount = blockCount;
        this.imageState = imageState;
      }
      handle;
      blockCount;
      imageState;
      driverId = BLOCK_DRIVER_ID;
      name = "block";
      /**
       * Open the block driver against the browser's OPFS root.
       * Creates `pmos.img` if it doesn't exist; pre-sizes it to
       * `blockCount * BLOCK_SIZE` bytes so the kernel sees a
       * fixed-size device. Idempotent: reopening an existing
       * `pmos.img` keeps its contents and reports `Existing`; only
       * creating the file in this call reports `NewlyCreated`.
       *
       * In tests, `rootOverride` lets a fake `OpfsRoot` stand in
       * for `navigator.storage.getDirectory()`.
       */
      static async openInOpfs(blockCount = DEFAULT_BLOCK_COUNT, rootOverride) {
        const root = rootOverride ?? await navigator.storage.getDirectory();
        let file;
        let imageState;
        try {
          file = await root.getFileHandle(PMOS_IMG_FILENAME);
          imageState = BlockImageState.Existing;
        } catch (e) {
          if (!isNotFound(e)) throw e;
          file = await root.getFileHandle(PMOS_IMG_FILENAME, { create: true });
          imageState = BlockImageState.NewlyCreated;
        }
        const handle = await file.createSyncAccessHandle();
        if (imageState === BlockImageState.NewlyCreated && handle.getSize() !== 0) {
          imageState = BlockImageState.Existing;
        }
        const expectedSize = blockCount * BLOCK_SIZE;
        if (handle.getSize() < expectedSize) {
          handle.truncate(expectedSize);
        }
        return new _BlockDriver(handle, blockCount, imageState);
      }
      /**
       * Test seam: construct a `BlockDriver` with a pre-built
       * `SyncAccessHandle`. Tests use this with the in-memory
       * `MemSyncAccessHandle` from `web/tests/unit/block.test.ts` so
       * the driver behaviour can be exercised without touching the
       * real OPFS surface (which jsdom doesn't expose). The safe
       * default is `Existing`; first-boot tests must opt into
       * `NewlyCreated` explicitly.
       */
      static withHandle(handle, blockCount = DEFAULT_BLOCK_COUNT, imageState = BlockImageState.Existing) {
        const expectedSize = blockCount * BLOCK_SIZE;
        if (handle.getSize() < expectedSize) {
          handle.truncate(expectedSize);
        }
        return new _BlockDriver(handle, blockCount, imageState);
      }
      init(_host) {
      }
      call(op, payload) {
        switch (op) {
          case OP_BLOCK_COUNT:
            return { ok: true, value: this.blockCount };
          case OP_READ:
            return this.read(payload);
          case OP_WRITE:
            return this.write(payload);
          case OP_FLUSH:
            return this.flushOp();
          case OP_IMAGE_STATE:
            return { ok: true, value: this.imageState };
          default:
            return { ok: false, error: DriverErrorCode.Transport };
        }
      }
      /** Permanently close the underlying handle. Tests + the
       *  panic-overlay teardown call this; production runs a single
       *  driver for the lifetime of the kernel Worker so this is
       *  rarely needed. */
      close() {
        this.handle.close();
      }
      read(payload) {
        if (payload.byteLength < 8 + BLOCK_SIZE) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
        }
        let lba;
        try {
          lba = readU64LE(payload, 0);
        } catch {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
        }
        if (lba >= this.blockCount) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
        }
        const offset = lba * BLOCK_SIZE;
        const dest = new Uint8Array(payload.buffer, payload.byteOffset + 8, BLOCK_SIZE);
        let n;
        try {
          n = this.handle.read(dest, { at: offset });
        } catch {
          return { ok: false, error: DriverErrorCode.Errno, errno: EIO };
        }
        if (n < BLOCK_SIZE) {
          dest.fill(0, n);
        }
        return { ok: true, value: BLOCK_SIZE };
      }
      write(payload) {
        if (payload.byteLength < 8 + BLOCK_SIZE) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
        }
        let lba;
        try {
          lba = readU64LE(payload, 0);
        } catch {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
        }
        if (lba >= this.blockCount) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
        }
        const offset = lba * BLOCK_SIZE;
        const src = new Uint8Array(payload.buffer, payload.byteOffset + 8, BLOCK_SIZE);
        try {
          const n = this.handle.write(src, { at: offset });
          if (n !== BLOCK_SIZE) {
            return { ok: false, error: DriverErrorCode.Errno, errno: EIO };
          }
          return { ok: true, value: BLOCK_SIZE };
        } catch (e) {
          const errno = isQuotaExceeded(e) ? ENOSPC : EIO;
          return { ok: false, error: DriverErrorCode.Errno, errno };
        }
      }
      flushOp() {
        try {
          this.handle.flush();
          return { ok: true, value: 0 };
        } catch {
          return { ok: false, error: DriverErrorCode.Errno, errno: EIO };
        }
      }
    };
  }
});

// src/drivers/net.ts
var net_exports = {};
__export(net_exports, {
  EAGAIN: () => EAGAIN,
  EBADF: () => EBADF,
  ECONNRESET: () => ECONNRESET,
  EINVAL: () => EINVAL2,
  ENOSPC: () => ENOSPC2,
  ENOTREADY: () => ENOTREADY,
  NET_DRIVER_ID: () => NET_DRIVER_ID,
  NetDriver: () => NetDriver,
  OP_FETCH_BEGIN: () => OP_FETCH_BEGIN,
  OP_FETCH_POLL: () => OP_FETCH_POLL,
  OP_WS_CLOSE: () => OP_WS_CLOSE,
  OP_WS_OPEN: () => OP_WS_OPEN,
  OP_WS_RECV: () => OP_WS_RECV,
  OP_WS_SEND: () => OP_WS_SEND
});
function defaultFetcher(url, init) {
  const reqInit = {};
  if (init?.method !== void 0) reqInit.method = init.method;
  if (init?.headers !== void 0) reqInit.headers = init.headers;
  if (init?.body !== void 0) {
    const body = new Uint8Array(init.body.byteLength);
    body.set(init.body);
    reqInit.body = body.buffer;
  }
  return globalThis.fetch(url, reqInit).then(async (r) => {
    const headers = {};
    r.headers.forEach((v, k) => {
      headers[k] = v;
    });
    return {
      status: r.status,
      headers,
      arrayBuffer: () => r.arrayBuffer()
    };
  });
}
function defaultWsFactory(url) {
  const ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";
  return ws;
}
function isQuotaExceeded2(e) {
  if (typeof e !== "object" || e === null) return false;
  const cand = e;
  return cand.name === "QuotaExceededError";
}
var NET_DRIVER_ID, OP_FETCH_BEGIN, OP_FETCH_POLL, OP_WS_OPEN, OP_WS_SEND, OP_WS_RECV, OP_WS_CLOSE, EBADF, EAGAIN, EINVAL2, ENOSPC2, ECONNRESET, ENOTREADY, NetDriver;
var init_net = __esm({
  "src/drivers/net.ts"() {
    "use strict";
    init_types();
    init_platform_constants();
    NET_DRIVER_ID = DriverId.Net;
    OP_FETCH_BEGIN = 1;
    OP_FETCH_POLL = 2;
    OP_WS_OPEN = 3;
    OP_WS_SEND = 4;
    OP_WS_RECV = 5;
    OP_WS_CLOSE = 6;
    EBADF = 8;
    EAGAIN = 9;
    EINVAL2 = 22;
    ENOSPC2 = 51;
    ECONNRESET = 73;
    ENOTREADY = 76;
    NetDriver = class {
      constructor(fetcher = defaultFetcher, wsFactory = defaultWsFactory) {
        this.fetcher = fetcher;
        this.wsFactory = wsFactory;
      }
      fetcher;
      wsFactory;
      driverId = NET_DRIVER_ID;
      name = "net";
      fetches = /* @__PURE__ */ new Map();
      sockets = /* @__PURE__ */ new Map();
      nextFetchHandle = 1;
      nextSocketHandle = 1;
      init(_host) {
      }
      call(op, payload) {
        switch (op) {
          case OP_FETCH_BEGIN:
            return this.fetchBegin(payload);
          case OP_FETCH_POLL:
            return this.fetchPoll(payload);
          case OP_WS_OPEN:
            return this.wsOpen(payload);
          case OP_WS_SEND:
            return this.wsSend(payload);
          case OP_WS_RECV:
            return this.wsRecv(payload);
          case OP_WS_CLOSE:
            return this.wsClose(payload);
          default:
            return { ok: false, error: DriverErrorCode.Transport };
        }
      }
      // ---- fetch ---------------------------------------------------------
      fetchBegin(payload) {
        let cursor = 0;
        if (payload.byteLength < 1) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const methodLen = payload[cursor];
        cursor += 1;
        if (cursor + methodLen + 2 > payload.byteLength) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const method = new TextDecoder().decode(
          payload.subarray(cursor, cursor + methodLen)
        );
        cursor += methodLen;
        const view = new DataView(payload.buffer, payload.byteOffset);
        const urlLen = view.getUint16(cursor, true);
        cursor += 2;
        if (cursor + urlLen + 2 > payload.byteLength) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const url = new TextDecoder().decode(
          payload.subarray(cursor, cursor + urlLen)
        );
        cursor += urlLen;
        const headerCount = view.getUint16(cursor, true);
        cursor += 2;
        const headers = {};
        for (let i = 0; i < headerCount; i += 1) {
          if (cursor + 2 > payload.byteLength) {
            return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
          }
          const nameLen = view.getUint16(cursor, true);
          cursor += 2;
          if (cursor + nameLen + 2 > payload.byteLength) {
            return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
          }
          const name = new TextDecoder().decode(
            payload.subarray(cursor, cursor + nameLen)
          );
          cursor += nameLen;
          const valueLen = view.getUint16(cursor, true);
          cursor += 2;
          if (cursor + valueLen > payload.byteLength) {
            return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
          }
          const value = new TextDecoder().decode(
            payload.subarray(cursor, cursor + valueLen)
          );
          cursor += valueLen;
          headers[name] = value;
        }
        if (cursor + 4 > payload.byteLength) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const bodyLen = view.getUint32(cursor, true);
        cursor += 4;
        if (cursor + bodyLen > payload.byteLength) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const body = bodyLen > 0 ? new Uint8Array(payload.subarray(cursor, cursor + bodyLen)) : void 0;
        const handle = this.nextFetchHandle;
        this.nextFetchHandle += 1;
        const entry = { done: false };
        this.fetches.set(handle, entry);
        const init = {};
        if (method.length > 0) init.method = method;
        if (headerCount > 0) init.headers = headers;
        if (body !== void 0) init.body = body;
        void this.fetcher(url, init).then(
          async (resp) => {
            try {
              const buf = await resp.arrayBuffer();
              entry.status = resp.status;
              entry.headers = resp.headers;
              entry.body = new Uint8Array(buf);
              entry.done = true;
            } catch {
              entry.error = ECONNRESET;
              entry.done = true;
            }
          },
          () => {
            entry.error = ECONNRESET;
            entry.done = true;
          }
        );
        return { ok: true, value: handle };
      }
      fetchPoll(payload) {
        if (payload.byteLength < 4) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const view = new DataView(payload.buffer, payload.byteOffset);
        const handle = view.getUint32(0, true);
        const entry = this.fetches.get(handle);
        if (!entry) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EBADF };
        }
        if (!entry.done) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EAGAIN };
        }
        if (entry.error !== void 0) {
          this.fetches.delete(handle);
          return { ok: false, error: DriverErrorCode.Errno, errno: entry.error };
        }
        const status = entry.status ?? 0;
        const headers = entry.headers ?? {};
        const body = entry.body ?? new Uint8Array(0);
        const headerEntries = Object.entries(headers);
        let needed = 4;
        const headerBytes = [];
        for (const [name, value] of headerEntries) {
          const nameBytes = new TextEncoder().encode(name);
          const valueBytes = new TextEncoder().encode(value);
          headerBytes.push({ nameBytes, valueBytes });
          needed += 2 + nameBytes.length + 2 + valueBytes.length;
        }
        needed += 4 + body.length;
        if (4 + needed > payload.byteLength) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        let cursor = 4;
        view.setUint16(cursor, status, true);
        cursor += 2;
        view.setUint16(cursor, headerEntries.length, true);
        cursor += 2;
        for (let i = 0; i < headerEntries.length; i += 1) {
          const { nameBytes, valueBytes } = headerBytes[i];
          view.setUint16(cursor, nameBytes.length, true);
          cursor += 2;
          payload.set(nameBytes, cursor);
          cursor += nameBytes.length;
          view.setUint16(cursor, valueBytes.length, true);
          cursor += 2;
          payload.set(valueBytes, cursor);
          cursor += valueBytes.length;
        }
        view.setUint32(cursor, body.length, true);
        cursor += 4;
        payload.set(body, cursor);
        cursor += body.length;
        this.fetches.delete(handle);
        return { ok: true, value: cursor - 4 };
      }
      // ---- websocket -----------------------------------------------------
      wsOpen(payload) {
        if (payload.byteLength < 2) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const view = new DataView(payload.buffer, payload.byteOffset);
        const urlLen = view.getUint16(0, true);
        if (2 + urlLen > payload.byteLength) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const url = new TextDecoder().decode(payload.subarray(2, 2 + urlLen));
        const handle = this.nextSocketHandle;
        this.nextSocketHandle += 1;
        let socket;
        try {
          socket = this.wsFactory(url);
        } catch {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const entry = { socket, open: false, closed: false, recvQueue: [] };
        this.sockets.set(handle, entry);
        socket.onopen = () => {
          entry.open = true;
        };
        socket.onmessage = (ev) => {
          let bytes;
          if (typeof ev.data === "string") {
            bytes = new TextEncoder().encode(ev.data);
          } else {
            bytes = new Uint8Array(ev.data);
          }
          entry.recvQueue.push(bytes);
        };
        socket.onerror = () => {
          entry.closed = true;
        };
        socket.onclose = () => {
          entry.closed = true;
        };
        return { ok: true, value: handle };
      }
      wsSend(payload) {
        if (payload.byteLength < 4) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const view = new DataView(payload.buffer, payload.byteOffset);
        const handle = view.getUint32(0, true);
        const entry = this.sockets.get(handle);
        if (!entry) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EBADF };
        }
        if (entry.closed) {
          return { ok: false, error: DriverErrorCode.Errno, errno: ECONNRESET };
        }
        const data = new Uint8Array(payload.subarray(4));
        try {
          entry.socket.send(data);
          return { ok: true, value: data.length };
        } catch (e) {
          const errno = isQuotaExceeded2(e) ? ENOSPC2 : ECONNRESET;
          return { ok: false, error: DriverErrorCode.Errno, errno };
        }
      }
      wsRecv(payload) {
        if (payload.byteLength < 4) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const view = new DataView(payload.buffer, payload.byteOffset);
        const handle = view.getUint32(0, true);
        const entry = this.sockets.get(handle);
        if (!entry) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EBADF };
        }
        if (entry.recvQueue.length === 0) {
          if (entry.closed) {
            return { ok: false, error: DriverErrorCode.Errno, errno: ECONNRESET };
          }
          return { ok: true, value: 0 };
        }
        const frame = entry.recvQueue.shift();
        const cap = payload.byteLength - 4;
        if (frame.length > cap) {
          entry.recvQueue.unshift(frame);
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        payload.set(frame, 4);
        return { ok: true, value: frame.length };
      }
      wsClose(payload) {
        if (payload.byteLength < 4) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL2 };
        }
        const view = new DataView(payload.buffer, payload.byteOffset);
        const handle = view.getUint32(0, true);
        const entry = this.sockets.get(handle);
        if (!entry) {
          return { ok: false, error: DriverErrorCode.Errno, errno: EBADF };
        }
        try {
          entry.socket.close();
        } catch {
        }
        this.sockets.delete(handle);
        return { ok: true, value: 0 };
      }
    };
  }
});

// src/drivers/console.ts
init_types();
init_platform_constants();
var CONSOLE_DRIVER_ID = DriverId.Console;
var DEV_CONSOLE_NODE = Devnum.Console;
var OP_WRITE_LINE = 1;
function isConsoleInput(m) {
  if (typeof m !== "object" || m === null) {
    return false;
  }
  const cand = m;
  return cand.kind === "console:input" && cand.bytes instanceof Uint8Array;
}
var ConsoleDriver = class {
  driverId = CONSOLE_DRIVER_ID;
  name = "console";
  host;
  init(host) {
    this.host = host;
  }
  call(op, payload) {
    const host = this.host;
    if (!host) {
      return { ok: false, error: DriverErrorCode.NotReady };
    }
    if (op !== OP_WRITE_LINE) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const copy = new Uint8Array(payload.byteLength);
    copy.set(payload);
    const message = { kind: "console:write", bytes: copy };
    host.postToMain(message);
    return { ok: true, value: payload.byteLength };
  }
  onHostMessage(msg) {
    const host = this.host;
    if (!host) {
      return;
    }
    if (isConsoleInput(msg)) {
      const copy = new Uint8Array(msg.bytes.byteLength);
      copy.set(msg.bytes);
      host.pushInputToKernel(DEV_CONSOLE_NODE, copy);
    }
  }
};

// src/kernel-worker.ts
init_fb();

// src/drivers/input.ts
init_types();
init_platform_constants();
var INPUT_DRIVER_ID = DriverId.InputKbd;
var DEV_INPUT_KBD_NODE = Devnum.InputKbd;
var DEV_INPUT_MOUSE_NODE = Devnum.InputMouse;
function isInputKbd(m) {
  if (typeof m !== "object" || m === null) {
    return false;
  }
  const cand = m;
  return cand.kind === "input:kbd" && cand.bytes instanceof Uint8Array;
}
function isInputMouse(m) {
  if (typeof m !== "object" || m === null) {
    return false;
  }
  const cand = m;
  return cand.kind === "input:mouse" && cand.bytes instanceof Uint8Array;
}
var InputDriver = class {
  driverId = INPUT_DRIVER_ID;
  name = "input";
  host;
  init(host) {
    this.host = host;
  }
  /**
   * The input device nodes are read-only; every opcode is a
   * caller bug, reported as `Transport`. We DELIBERATELY do
   * not distinguish "driver not initialised" here because the
   * only valid response is "don't call me".
   */
  call(_op, _payload) {
    return { ok: false, error: DriverErrorCode.Transport };
  }
  onHostMessage(msg) {
    const host = this.host;
    if (!host) {
      return;
    }
    if (isInputKbd(msg)) {
      const copy = new Uint8Array(msg.bytes.byteLength);
      copy.set(msg.bytes);
      host.pushInputToKernel(DEV_INPUT_KBD_NODE, copy);
      return;
    }
    if (isInputMouse(msg)) {
      const copy = new Uint8Array(msg.bytes.byteLength);
      copy.set(msg.bytes);
      host.pushInputToKernel(DEV_INPUT_MOUSE_NODE, copy);
      return;
    }
  }
};

// src/kernel-worker.ts
init_types();
function bootKernelWorker(options) {
  const drivers = /* @__PURE__ */ new Map();
  const host = {
    postToMain(msg) {
      options.postToMain(msg);
    },
    pushInputToKernel(devnum, bytes) {
      options.kernel.injectInput(devnum, bytes);
    }
  };
  if (options.config.enableConsole) {
    const console_ = new ConsoleDriver();
    console_.init(host);
    drivers.set(console_.driverId, console_);
  }
  if (options.config.enableInput) {
    const input = new InputDriver();
    input.init(host);
    drivers.set(input.driverId, input);
  }
  if (options.config.enableFramebuffer) {
    const fb = new FramebufferDriver();
    fb.init(host);
    drivers.set(fb.driverId, fb);
  }
  options.postToMain({ kind: "ready" });
  return {
    handleMainMessage(msg) {
      switch (msg.kind) {
        case "boot": {
          options.postToMain({
            kind: "panic",
            message: "kernel-worker: received boot message while already booted"
          });
          return;
        }
        case "shutdown": {
          drivers.clear();
          return;
        }
        case "console:input": {
          const d = drivers.get(CONSOLE_DRIVER_ID);
          d?.onHostMessage?.(msg);
          return;
        }
        case "input:kbd":
        case "input:mouse": {
          const d = drivers.get(INPUT_DRIVER_ID);
          d?.onHostMessage?.(msg);
          return;
        }
      }
    },
    callDriver(devId, op, payload) {
      const d = drivers.get(devId);
      if (!d) {
        return { ok: false, error: DriverErrorCode.NotReady };
      }
      return d.call(op, payload);
    },
    get driverCount() {
      return drivers.size;
    }
  };
}

// src/kernel-wasm-host.ts
init_platform_constants();
init_sab_layout();

// src/shared/worker-proto.ts
var HOST_FILE_IMPORT_MAX_BYTES = 16 * 1024 * 1024;
var HOST_FILE_IMPORT_MAX_TOTAL_BYTES = 32 * 1024 * 1024;

// src/shared/syscall.ts
var SLOT_SIZE = 32;
function encodeRequest(req) {
  const buf = new Uint8Array(SLOT_SIZE);
  const view = new DataView(buf.buffer);
  view.setUint16(0, req.opcode, true);
  view.setUint16(2, req.flags ?? 0, true);
  view.setUint32(4, req.requestId, true);
  if (req.args !== void 0) {
    if (req.args.length !== 16) {
      throw new Error(
        `syscall.encodeRequest: args must be 16 bytes, got ${req.args.length}`
      );
    }
    if (req.arg0 !== void 0) {
      throw new Error(
        "syscall.encodeRequest: pass either args or arg0, not both"
      );
    }
    buf.set(req.args, 8);
  } else if (req.arg0 !== void 0) {
    view.setUint32(8, req.arg0, true);
  }
  view.setUint32(24, req.heapPtr ?? 0, true);
  view.setUint32(28, req.heapLen ?? 0, true);
  return buf;
}
function decodeResponse(bytes) {
  if (bytes.length !== SLOT_SIZE) {
    throw new Error(
      `syscall.decodeResponse: expected ${SLOT_SIZE} bytes, got ${bytes.length}`
    );
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, SLOT_SIZE);
  return {
    requestId: view.getUint32(0, true),
    status: view.getInt32(4, true),
    value: view.getBigInt64(8, true),
    extraLen: view.getUint32(16, true)
  };
}
function encodeResponse(res) {
  const buf = new Uint8Array(SLOT_SIZE);
  const view = new DataView(buf.buffer);
  view.setUint32(0, res.requestId, true);
  view.setInt32(4, res.status, true);
  view.setBigInt64(8, res.value, true);
  view.setUint32(16, res.extraLen, true);
  return buf;
}
function decodeRequest(bytes) {
  if (bytes.length !== SLOT_SIZE) {
    throw new Error(
      `syscall.decodeRequest: expected ${SLOT_SIZE} bytes, got ${bytes.length}`
    );
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, SLOT_SIZE);
  return {
    opcode: view.getUint16(0, true),
    flags: view.getUint16(2, true),
    requestId: view.getUint32(4, true),
    args: bytes.slice(8, 24),
    heapPtr: view.getUint32(24, true),
    heapLen: view.getUint32(28, true)
  };
}
var OP_EXT = {
  IPC_SOCKET: 4096,
  IPC_BIND: 4097,
  IPC_LISTEN: 4098,
  IPC_CONNECT: 4099,
  IPC_ACCEPT: 4100,
  /** Send a bounded byte payload and, optionally, one duplicated fd. */
  IPC_SEND: 4101,
  /** Receive a bounded byte payload and, optionally, one installed fd. */
  IPC_RECV: 4102,
  /** Wire-format identity for `ipc_pipe`. Create a pipe pair: the
   * kernel allocates two fds on the caller — a PipeRead at
   * heap[0..4] and a PipeWrite at heap[4..8] — and returns success
   * with extraLen = 8. No inline args; heap_len must be >= 8 or the
   * dispatcher rejects with EINVAL before allocating any fds. On a
   * failed second-fd alloc mid-call, the kernel rolls back the
   * first install so the fd table never holds a half-installed
   * pair. After a successful call: bytes written to the write fd
   * are readable via the read fd through the existing fd_read /
   * fd_write arms (landed in fbddb91); closing either end
   * propagates to the other (reader closed → subsequent writes
   * EPIPE; writer closed → subsequent reads see (0, []) EOF). */
  IPC_PIPE: 4103,
  /** Kernel-authenticated capability snapshot for the process on the
   * other end of a connected IPC socket (`SO_PEERCRED` equivalent). */
  IPC_PEER_CAPS: 4104,
  /** Kernel-authenticated pid snapshot for the process on the other
   * end of a connected IPC socket (`SO_PEERCRED` equivalent). */
  IPC_PEER_PID: 4105,
  PROC_SPAWN: 4352,
  PROC_SELF: 4355,
  PROC_PARENT: 4356,
  PROC_WAIT: 4353,
  PROC_KILL: 4354,
  PROC_CAPS_GET: 4357,
  DISPLAY_CONNECT: 4608,
  DISPLAY_BIND: 4609,
  CAP_CHECK: 4864,
  CAP_LIST: 4865,
  MOUNT: 5120,
  UMOUNT: 5121,
  FS_WATCH: 5122,
  FS_CHMOD: 5123,
  HOST_FILE_RECV: 5376,
  HOST_FILE_PICK: 5377,
  HOST_FILE_SEND: 5378
};
var ERRNO = {
  /** Permission denied. */
  EACCES: 2,
  EAGAIN: 6,
  EBADF: 8,
  /** No child processes. Returned by `proc_wait` when the caller
   * has no children matching the wait target, or when the target
   * is the caller's own pid (POSIX: can't wait on self). Mirrors
   * `abi::errno::ECHILD`. */
  ECHILD: 9,
  ECONNREFUSED: 14,
  EEXIST: 20,
  /** Bad address: a user pointer range crosses linear-memory bounds. */
  EFAULT: 21,
  /** Interrupted function. Surfaced on a blocking syscall
   * (currently only `ipc_accept` with `flags=0`) when a signal
   * interrupts the park. Mirrors `abi::errno::EINTR`. */
  EINTR: 27,
  EINVAL: 28,
  /** I/O error. Used when a transport/backend violates an I/O byte-count
   * contract. Mirrors `abi::errno::EIO`. */
  EIO: 29,
  EISDIR: 31,
  /** Too many levels of symbolic links. Returned from path
   * resolution when a symlink chain exceeds SYMLOOP_MAX (40).
   * Mirrors `abi::errno::ELOOP`. */
  ELOOP: 32,
  ENOENT: 44,
  ENOSYS: 52,
  ENOTDIR: 54,
  ENOTEMPTY: 55,
  ENOTSUP: 58,
  /** Broken pipe / socket. Returned by send / write when the
   * peer has fully closed, the local write side has been shut
   * down via SOCK_SHUTDOWN, or the peer has shut down its read
   * side. Maps to `abi::errno::EPIPE`. */
  EPIPE: 64,
  EROFS: 69,
  /** No such process. Returned by process-management opcodes
   * (`proc_kill`, `proc_caps_get`) when the target pid doesn't
   * exist or has already been reaped. Mirrors
   * `abi::errno::ESRCH`. */
  ESRCH: 71,
  /** Caller's capability set does not permit this operation.
   * PMos-specific errno (not in the POSIX baseline); used by
   * `proc_kill` when the sender is not the target's parent, not
   * the target itself, and doesn't hold `Cap::ProcKillAny`.
   * Mirrors `abi::errno::ENOTCAPABLE`. */
  ENOTCAPABLE: 76
};
var DEV = {
  FRAMEBUFFER: 0,
  INPUT_KBD: 1,
  INPUT_MOUSE: 2,
  BLOCK: 3,
  NET: 4,
  CONSOLE: 5
};
var CAP = {
  DISPLAY_CLIENT: 1,
  DISPLAY_SERVER: 2,
  SHELL: 3,
  PROC_ENUMERATE: 4,
  PROC_KILL_ANY: 5,
  NET: 6,
  MOUNT: 7,
  CAP_GRANT: 8,
  DEV_BLOCK: 9,
  KEYMAP_ADMIN: 10,
  PROC_INSPECT: 11,
  HOST_TRANSFER: 12
};
function capBit(cap) {
  return 1n << BigInt(cap);
}
var SPAWN_V1_MAGIC = 827215955;
var SPAWN_V1_VERSION = 1;
var SPAWN_V1_HEADER_LEN = 48;
var SPAWN_V1_MAX_BYTES = 32768;
var SPAWN_FLAG_CWD = 1;
var SPAWN_FLAG_CAPS = 2;
var SPAWN_KNOWN_FLAGS = SPAWN_FLAG_CWD | SPAWN_FLAG_CAPS;
var SPAWN_INHERIT_FD = -1;
var SPAWN_FIRST_DYNAMIC_FD = 5;
var SPAWN_FD_SOFT_LIMIT = 1024;
function encodeSpawnManifest(manifest) {
  const enc = new TextEncoder();
  const encodeText = (label, value, allowEmpty) => {
    if (!allowEmpty && value.length === 0 || value.includes("\0")) {
      throw new RangeError(`encodeSpawnManifest: invalid ${label}`);
    }
    const bytes = enc.encode(value);
    if (bytes.length > 65535) {
      throw new RangeError(`encodeSpawnManifest: ${label} exceeds u16 length`);
    }
    return bytes;
  };
  const validateFd = (label, fd) => {
    if (fd === void 0) return SPAWN_INHERIT_FD;
    if (!Number.isSafeInteger(fd) || fd < 0 || fd > 2147483647) {
      throw new RangeError(`encodeSpawnManifest: invalid ${label} ${fd}`);
    }
    return fd;
  };
  if (!manifest.path.startsWith("/")) {
    throw new RangeError("encodeSpawnManifest: path must be absolute");
  }
  if (manifest.cwd !== void 0 && !manifest.cwd.startsWith("/")) {
    throw new RangeError("encodeSpawnManifest: cwd must be absolute");
  }
  const path = encodeText("path", manifest.path, false);
  const cwd = manifest.cwd === void 0 ? new Uint8Array(0) : encodeText("cwd", manifest.cwd, false);
  const argv = (manifest.argv ?? []).map(
    (arg, index) => encodeText(`argv[${index}]`, arg, true)
  );
  const envp = (manifest.envp ?? []).map(([key, value], index) => {
    if (key.includes("=")) {
      throw new RangeError(
        `encodeSpawnManifest: envp[${index}] key contains '='`
      );
    }
    return [
      encodeText(`envp[${index}] key`, key, false),
      encodeText(`envp[${index}] value`, value, true)
    ];
  });
  const extraFds = manifest.extraFds ?? [];
  if (argv.length > 65535 || envp.length > 65535 || extraFds.length > 65535) {
    throw new RangeError("encodeSpawnManifest: entry count exceeds u16");
  }
  const childFds = /* @__PURE__ */ new Set();
  for (const [parentFd, childFd] of extraFds) {
    validateFd("extra parent fd", parentFd);
    validateFd("extra child fd", childFd);
    if (childFd < SPAWN_FIRST_DYNAMIC_FD || childFd >= SPAWN_FD_SOFT_LIMIT) {
      throw new RangeError(
        `encodeSpawnManifest: reserved/out-of-range child fd ${childFd}`
      );
    }
    if (childFds.has(childFd)) {
      throw new RangeError(
        `encodeSpawnManifest: duplicate child fd ${childFd}`
      );
    }
    childFds.add(childFd);
  }
  const bodyLen = path.length + cwd.length + argv.reduce((sum, arg) => sum + 2 + arg.length, 0) + envp.reduce((sum, [key, value]) => sum + 4 + key.length + value.length, 0) + extraFds.length * 8;
  const totalLen = SPAWN_V1_HEADER_LEN + bodyLen;
  if (totalLen > SPAWN_V1_MAX_BYTES) {
    throw new RangeError(
      `encodeSpawnManifest: blob size ${totalLen} exceeds ${SPAWN_V1_MAX_BYTES}`
    );
  }
  const heap = new Uint8Array(totalLen);
  const header = new DataView(heap.buffer);
  const flags = (manifest.cwd === void 0 ? 0 : SPAWN_FLAG_CWD) | (manifest.caps === void 0 ? 0 : SPAWN_FLAG_CAPS);
  header.setUint32(0, SPAWN_V1_MAGIC, true);
  header.setUint16(4, SPAWN_V1_VERSION, true);
  header.setUint16(6, flags, true);
  header.setUint32(8, totalLen, true);
  header.setUint16(12, path.length, true);
  header.setUint16(14, cwd.length, true);
  header.setUint16(16, argv.length, true);
  header.setUint16(18, envp.length, true);
  header.setUint16(20, extraFds.length, true);
  header.setInt32(24, validateFd("stdin fd", manifest.stdinFd), true);
  header.setInt32(28, validateFd("stdout fd", manifest.stdoutFd), true);
  header.setInt32(32, validateFd("stderr fd", manifest.stderrFd), true);
  header.setBigUint64(40, manifest.caps ?? 0n, true);
  let offset = SPAWN_V1_HEADER_LEN;
  const put = (bytes) => {
    heap.set(bytes, offset);
    offset += bytes.length;
  };
  const putU16 = (value) => {
    new DataView(heap.buffer).setUint16(offset, value, true);
    offset += 2;
  };
  const putU32 = (value) => {
    new DataView(heap.buffer).setUint32(offset, value, true);
    offset += 4;
  };
  put(path);
  put(cwd);
  for (const arg of argv) {
    putU16(arg.length);
    put(arg);
  }
  for (const [key, value] of envp) {
    putU16(key.length);
    putU16(value.length);
    put(key);
    put(value);
  }
  for (const [parentFd, childFd] of extraFds) {
    putU32(parentFd);
    putU32(childFd);
  }
  if (offset !== totalLen) {
    throw new Error("encodeSpawnManifest: internal length mismatch");
  }
  return encodeSpawnManifestBlob(heap);
}
function encodeSpawnManifestBlob(blob) {
  if (!isValidSpawnManifestBlob(blob)) {
    throw new RangeError("encodeSpawnManifestBlob: malformed spawn manifest");
  }
  const args = new Uint8Array(16);
  const view = new DataView(args.buffer);
  view.setUint32(0, SPAWN_V1_MAGIC, true);
  view.setUint32(4, blob.length, true);
  view.setUint16(8, SPAWN_V1_VERSION, true);
  return { args, heap: blob.slice() };
}
function isValidSpawnManifestBlob(blob) {
  if (blob.length < SPAWN_V1_HEADER_LEN || blob.length > SPAWN_V1_MAX_BYTES)
    return false;
  const view = new DataView(blob.buffer, blob.byteOffset, blob.byteLength);
  if (view.getUint32(0, true) !== SPAWN_V1_MAGIC || view.getUint16(4, true) !== SPAWN_V1_VERSION || view.getUint32(8, true) !== blob.length || view.getUint16(22, true) !== 0 || view.getUint32(36, true) !== 0)
    return false;
  const flags = view.getUint16(6, true);
  if ((flags & ~SPAWN_KNOWN_FLAGS) !== 0) return false;
  const pathLen = view.getUint16(12, true);
  const cwdLen = view.getUint16(14, true);
  const argc = view.getUint16(16, true);
  const envc = view.getUint16(18, true);
  const extraCount = view.getUint16(20, true);
  if ((flags & SPAWN_FLAG_CWD) === 0 !== (cwdLen === 0)) return false;
  if ((flags & SPAWN_FLAG_CAPS) === 0 && view.getBigUint64(40, true) !== 0n)
    return false;
  for (const offset2 of [24, 28, 32]) {
    if (view.getInt32(offset2, true) < SPAWN_INHERIT_FD) return false;
  }
  const utf8 = new TextDecoder("utf-8", { fatal: true });
  let offset = SPAWN_V1_HEADER_LEN;
  const take = (length, allowEmpty) => {
    if (!allowEmpty && length === 0 || offset + length > blob.length)
      return null;
    const bytes = blob.subarray(offset, offset + length);
    offset += length;
    if (bytes.includes(0)) return null;
    try {
      utf8.decode(bytes);
    } catch {
      return null;
    }
    return bytes;
  };
  const readU16 = () => {
    if (offset + 2 > blob.length) return null;
    const value = view.getUint16(offset, true);
    offset += 2;
    return value;
  };
  const readU32 = () => {
    if (offset + 4 > blob.length) return null;
    const value = view.getUint32(offset, true);
    offset += 4;
    return value;
  };
  const path = take(pathLen, false);
  if (path === null || utf8.decode(path)[0] !== "/") return false;
  if (cwdLen > 0) {
    const cwd = take(cwdLen, false);
    if (cwd === null || utf8.decode(cwd)[0] !== "/") return false;
  }
  for (let i = 0; i < argc; i++) {
    const len = readU16();
    if (len === null || take(len, true) === null) return false;
  }
  const envKeys = /* @__PURE__ */ new Set();
  for (let i = 0; i < envc; i++) {
    const keyLen = readU16();
    const valueLen = readU16();
    if (keyLen === null || valueLen === null) return false;
    const key = take(keyLen, false);
    const value = take(valueLen, true);
    if (key === null || value === null) return false;
    const decoded = utf8.decode(key);
    if (decoded.includes("=") || envKeys.has(decoded)) return false;
    envKeys.add(decoded);
  }
  const childFds = /* @__PURE__ */ new Set();
  for (let i = 0; i < extraCount; i++) {
    const parentFd = readU32();
    const childFd = readU32();
    if (parentFd === null || childFd === null || childFd < SPAWN_FIRST_DYNAMIC_FD || childFd >= SPAWN_FD_SOFT_LIMIT || childFds.has(childFd))
      return false;
    childFds.add(childFd);
  }
  return offset === blob.length;
}
var CAPSET_ALL = 0xffffffffffffffffn;
var CAPSET_DESKTOP_SHELL = capBit(CAP.DISPLAY_CLIENT) | capBit(CAP.SHELL) | capBit(CAP.PROC_ENUMERATE) | capBit(CAP.PROC_KILL_ANY) | capBit(CAP.KEYMAP_ADMIN) | capBit(CAP.HOST_TRANSFER);
var CAPSET_ORDINARY_APP = capBit(CAP.DISPLAY_CLIENT);
var CAPSET_FILES = capBit(CAP.DISPLAY_CLIENT) | capBit(CAP.HOST_TRANSFER);

// src/kernel-wasm-host.ts
var NO_POLL_TIMEOUT_NS = 0xffffffffffffffffn;
function pollTimeoutMs(timeoutNs) {
  if (timeoutNs === NO_POLL_TIMEOUT_NS) return void 0;
  return Math.max(0, Number(timeoutNs) / 1e6);
}
var WorkerTaskYielder = class {
  channel;
  pending;
  closed = false;
  constructor() {
    if (typeof globalThis.MessageChannel !== "function") {
      throw new Error(
        "KernelWasmHost: task-yielding dispatcher requires MessageChannel"
      );
    }
    const channel = new MessageChannel();
    try {
      channel.port1.onmessage = () => {
        const pending = this.pending;
        if (pending === void 0) return;
        this.pending = void 0;
        pending.resolve();
      };
      channel.port1.onmessageerror = () => {
        const pending = this.pending;
        if (pending === void 0) return;
        this.pending = void 0;
        pending.reject(
          new Error("KernelWasmHost: MessageChannel task yield failed")
        );
      };
      channel.port1.start();
    } catch (error) {
      channel.port1.close();
      channel.port2.close();
      throw error;
    }
    this.channel = channel;
  }
  nextTask() {
    if (this.closed) {
      return Promise.reject(
        new Error("KernelWasmHost: task yielder is already closed")
      );
    }
    if (this.pending !== void 0) {
      return Promise.reject(
        new Error("KernelWasmHost: task yield is already pending")
      );
    }
    return new Promise((resolve, reject) => {
      this.pending = { resolve, reject };
      try {
        this.channel.port2.postMessage(void 0);
      } catch (error) {
        this.pending = void 0;
        reject(error);
      }
    });
  }
  close() {
    if (this.closed) return;
    this.closed = true;
    const pending = this.pending;
    this.pending = void 0;
    this.channel.port1.onmessage = null;
    this.channel.port1.onmessageerror = null;
    this.channel.port1.close();
    this.channel.port2.close();
    pending?.reject(
      new Error("KernelWasmHost: task yielder closed before delivery")
    );
  }
};
var KernelWasmHost = class _KernelWasmHost {
  // Note: the class deliberately does NOT retain the caller's
  // `KernelWasmHostOptions` past construction. Every field of that
  // options bag is captured by the host-import closures built in
  // `create()`. The only state the class itself owns is the WASM
  // exports record and the shared 32-byte wake slot every user
  // Worker + the main thread bumps to wake the kernel's dispatch
  // loop.
  constructor(exports, wakeBuffer) {
    this.exports = exports;
    this.wakeBuffer = wakeBuffer;
    this.wakeView = new Int32Array(wakeBuffer, 0, 8);
  }
  exports;
  wakeBuffer;
  /** `Int32Array` view over [`wakeBuffer`]; index 0 is the wake slot. */
  wakeView;
  /**
   * Load `wasmBytes`, satisfy the host imports, and call
   * `kernel_init`. Returns a ready-to-use host.
   *
   * Throws if instantiation fails, if any import is missing from
   * `wasmBytes`, or if `kernel_init` returns non-zero.
   */
  static async create(wasmBytes, options = {}) {
    let memory;
    const binaryRegistry = options.binaryRegistry;
    const kernelWorkerChannel = options.kernelWorkerChannel;
    const resolvedOnSpawnProcess = options.onSpawnProcess ?? (binaryRegistry !== void 0 && kernelWorkerChannel !== void 0 ? (pid, path, executable) => {
      const bytes = executable ?? binaryRegistry.get(path);
      if (bytes === void 0) {
        return { ok: false, errno: ERRNO.ENOENT };
      }
      const wasmBytes2 = bytes instanceof ArrayBuffer ? bytes : ArrayBuffer.isView(bytes) ? bytes.buffer.slice(
        bytes.byteOffset,
        bytes.byteOffset + bytes.byteLength
      ) : bytes;
      kernelWorkerChannel.postMessage({
        kind: "proc:spawn",
        pid,
        path,
        wasmBytes: wasmBytes2
      });
      return { ok: true };
    } : void 0);
    const resolvedOnTerminateProcess = options.onTerminateProcess ?? (kernelWorkerChannel !== void 0 ? (pid) => {
      kernelWorkerChannel.postMessage({
        kind: "proc:terminate",
        pid,
        signal: 9
      });
    } : void 0);
    const resolvedOnHostFilePicker = options.onHostFilePicker ?? (kernelWorkerChannel !== void 0 ? () => {
      kernelWorkerChannel.postMessage({ kind: "host:pick" });
    } : void 0);
    const resolvedOnHostDownload = options.onHostDownload ?? (kernelWorkerChannel !== void 0 ? (name, mime, bytes) => {
      kernelWorkerChannel.postMessage({
        kind: "host:download",
        name,
        mime,
        bytes
      });
    } : void 0);
    const randomBytes = options.randomBytes ?? ((out) => {
      crypto.getRandomValues(out);
    });
    const nowNs = options.nowNs ?? (() => {
      return BigInt(Math.floor(performance.now() * 1e6));
    });
    const nowRealtimeNs = options.nowRealtimeNs ?? (() => {
      return BigInt(Date.now()) * 1000000n;
    });
    const onPanic = options.onPanic ?? ((message) => {
      throw new Error(`KernelWasmHost panic: ${message}`);
    });
    const framebufferDriver = options.framebufferDriver;
    if (framebufferDriver !== void 0) {
      const fbDriverHost = {
        postToMain: (msg) => {
          options.onFramebufferMessage?.(msg);
        },
        pushInputToKernel: () => {
        }
      };
      framebufferDriver.init(fbDriverHost);
    }
    const blockDriver = options.blockDriver;
    if (blockDriver !== void 0) {
      const blockDriverHost = {
        postToMain: () => {
        },
        pushInputToKernel: () => {
        }
      };
      blockDriver.init(blockDriverHost);
    }
    const netDriver = options.netDriver;
    if (netDriver !== void 0) {
      const netDriverHost = {
        postToMain: () => {
        },
        pushInputToKernel: () => {
        }
      };
      netDriver.init(netDriverHost);
    }
    const imports = {
      env: {
        pmos_host_now_ns: () => nowNs(),
        pmos_host_now_realtime_ns: () => nowRealtimeNs(),
        pmos_host_driver_call: (dev, op, argsPtr, argsLen, resultPtr) => {
          if (memory === void 0) return 0;
          if (dev === DEV.CONSOLE && options.onConsoleWrite !== void 0) {
            const src = new Uint8Array(memory.buffer, argsPtr, argsLen);
            options.onConsoleWrite(new Uint8Array(src));
          } else if (dev === DEV.FRAMEBUFFER) {
            const src = new Uint8Array(memory.buffer, argsPtr, argsLen);
            const copy = new Uint8Array(src);
            if (options.onFramebufferWrite !== void 0) {
              options.onFramebufferWrite(copy);
            }
            if (framebufferDriver !== void 0 && copy.length >= 1) {
              framebufferDriver.call(copy[0], copy.subarray(1));
            }
          } else if (dev === DEV.BLOCK) {
            if (blockDriver === void 0) {
              return 1;
            }
            const view = new Uint8Array(memory.buffer, argsPtr, argsLen);
            const result = blockDriver.call(op, view);
            if (result.ok) {
              if (resultPtr !== 0) {
                new DataView(memory.buffer).setUint32(
                  resultPtr,
                  result.value >>> 0,
                  true
                );
              }
              return 0;
            }
            if (result.error === 3) {
              return -(result.errno ?? 5);
            }
            return 1;
          } else if (dev === DEV.NET) {
            if (netDriver === void 0) {
              return 1;
            }
            const view = new Uint8Array(memory.buffer, argsPtr, argsLen);
            const result = netDriver.call(op, view);
            if (result.ok) {
              if (resultPtr !== 0) {
                new DataView(memory.buffer).setUint32(
                  resultPtr,
                  result.value >>> 0,
                  true
                );
              }
              return 0;
            }
            if (result.error === 3) {
              return -(result.errno ?? 5);
            }
            return 1;
          }
          return 0;
        },
        pmos_host_random_bytes: (ptr, len) => {
          if (memory === void 0) return;
          const dest = new Uint8Array(memory.buffer, ptr, len);
          randomBytes(dest);
        },
        pmos_host_halt: (ptr, len) => {
          let message = "kernel halted";
          if (memory !== void 0 && len > 0) {
            const bytes = new Uint8Array(memory.buffer, ptr, len);
            message = new TextDecoder().decode(bytes);
          }
          onPanic(message);
          throw new Error(`kernel halted: ${message}`);
        },
        pmos_host_panic: (ptr, len) => {
          if (memory === void 0) return;
          const bytes = new Uint8Array(memory.buffer, ptr, len);
          const message = new TextDecoder().decode(bytes);
          onPanic(message);
        },
        pmos_host_spawn_process: (pid, pathPtr, pathLen, executablePtr, executableLen) => {
          if (memory === void 0) return 0;
          if (pathPtr < 0 || pathLen < 0 || pathPtr + pathLen > memory.buffer.byteLength || executablePtr < 0 || executableLen < 0 || executablePtr + executableLen > memory.buffer.byteLength) {
            return 1;
          }
          const pathBytes = new Uint8Array(memory.buffer, pathPtr, pathLen);
          const path = new TextDecoder().decode(pathBytes);
          const executable = executablePtr === 0 ? void 0 : new Uint8Array(
            memory.buffer,
            executablePtr,
            executableLen
          ).slice();
          if (resolvedOnSpawnProcess === void 0) return 0;
          let outcome;
          try {
            outcome = resolvedOnSpawnProcess(pid, path, executable);
          } catch {
            return 1;
          }
          if (outcome.ok) return 0;
          return -outcome.errno;
        },
        pmos_host_terminate_process: (pid) => {
          if (resolvedOnTerminateProcess === void 0) return 1;
          try {
            resolvedOnTerminateProcess(pid);
            return 0;
          } catch {
            return 1;
          }
        },
        pmos_host_file_picker: () => {
          if (resolvedOnHostFilePicker === void 0) return 1;
          try {
            resolvedOnHostFilePicker();
            return 0;
          } catch {
            return 1;
          }
        },
        pmos_host_download_file: (namePtr, nameLen, mimePtr, mimeLen, bytesPtr, bytesLen) => {
          if (memory === void 0 || resolvedOnHostDownload === void 0)
            return 1;
          try {
            const decoder = new TextDecoder("utf-8", { fatal: true });
            const name = decoder.decode(
              new Uint8Array(memory.buffer, namePtr, nameLen)
            );
            const mime = decoder.decode(
              new Uint8Array(memory.buffer, mimePtr, mimeLen)
            );
            const bytes = new Uint8Array(
              new Uint8Array(memory.buffer, bytesPtr, bytesLen)
            );
            resolvedOnHostDownload(name, mime, bytes);
            return 0;
          } catch {
            return 1;
          }
        }
      }
    };
    const { instance } = await WebAssembly.instantiate(wasmBytes, imports);
    const exports = instance.exports;
    memory = exports.memory;
    const rc = exports.kernel_init();
    if (rc !== 0) {
      throw new Error(`KernelWasmHost: kernel_init returned ${rc}`);
    }
    let wakeBuffer;
    try {
      wakeBuffer = new SharedArrayBuffer(32);
    } catch {
      wakeBuffer = new ArrayBuffer(32);
    }
    return new _KernelWasmHost(exports, wakeBuffer);
  }
  // ---- process lifecycle --------------------------------------------
  /**
   * Register a process with the given cap bitset. Returns the newly
   * allocated pid. Throws if the kernel rejects the registration (the
   * current implementation always succeeds, so the throw path is
   * defensive).
   */
  registerProcess(caps) {
    const pid = this.exports.kernel_register_process(caps);
    if (pid < 0) {
      throw new Error(
        `KernelWasmHost.registerProcess: kernel_register_process returned ${pid}`
      );
    }
    return pid;
  }
  /**
   * Install `/dev/console` at `fd` in `pid`'s fd table. Convenience
   * wrapper over the kernel export of the same name.
   */
  installConsoleFd(pid, fd) {
    const rc = this.exports.kernel_install_console_fd(pid, fd);
    if (rc !== 0) {
      throw new Error(
        `KernelWasmHost.installConsoleFd(${pid}, ${fd}): rc=${rc}`
      );
    }
  }
  /** Install the WASI `/` directory preopen at `fd`. */
  installRootPreopenFd(pid, fd) {
    const rc = this.exports.kernel_install_root_preopen_fd(pid, fd);
    if (rc !== 0) {
      throw new Error(
        `KernelWasmHost.installRootPreopenFd(${pid}, ${fd}): rc=${rc}`
      );
    }
  }
  /**
   * Install `FdObject::SignalChannel` at `fd` in `pid`'s fd table.
   * Companion to {@link installConsoleFd}; gives host-side tests
   * a way to stage the per-process signal channel on a pid
   * that was created via `registerProcess` (which deliberately
   * does not auto-install, unlike proc_spawn'd children which
   * get fd 4 = SignalChannel for free).
   */
  installSignalChannelFd(pid, fd) {
    const rc = this.exports.kernel_install_signal_channel_fd(pid, fd);
    if (rc !== 0) {
      throw new Error(
        `KernelWasmHost.installSignalChannelFd(${pid}, ${fd}): rc=${rc}`
      );
    }
  }
  /**
   * Transition a newly-registered process from `Starting` through
   * `Ready` to `Running`. Required before the process can issue any
   * syscall that needs the caller to be in `Running` state (most
   * notably `PROC_EXIT`).
   */
  markRunning(pid) {
    const rc = this.exports.kernel_mark_running(pid);
    if (rc !== 0) {
      throw new Error(`KernelWasmHost.markRunning(${pid}): rc=${rc}`);
    }
  }
  /**
   * Make a host-observed Worker return/trap authoritative in the
   * kernel. Returns `true` when the pid was known (including an
   * idempotent acknowledgement of an already-terminal process) and
   * `false` for a stale/unknown pid.
   */
  reconcileProcessExit(pid, code, crashed) {
    const rc = this.exports.kernel_reconcile_process_exit(
      pid,
      code,
      crashed ? 1 : 0
    );
    if (rc < 0) {
      throw new Error(
        `KernelWasmHost.reconcileProcessExit(${pid}, ${code}): rc=${rc}`
      );
    }
    return rc === 0;
  }
  recordProcessMemory(pid, bytes) {
    if (!Number.isFinite(bytes) || bytes < 0 || !Number.isSafeInteger(bytes)) {
      throw new Error(
        `KernelWasmHost.recordProcessMemory: invalid byte count ${bytes}`
      );
    }
    const bytesLo = bytes >>> 0;
    const bytesHi = Math.floor(bytes / 4294967296) >>> 0;
    const rc = this.exports.kernel_record_process_memory(pid, bytesLo, bytesHi);
    if (rc !== 0) {
      throw new Error(
        `KernelWasmHost.recordProcessMemory(${pid}, ${bytes}): rc=${rc}`
      );
    }
  }
  /**
   * Best-effort flush of every dirty VFS mount through the kernel's
   * `vfs.sync_dirty()` path. Wired up to `pagehide` on the main
   * thread so OPFS-backed mutations survive the user closing the
   * tab while a long-running process is still mid-flight. Mounts
   * whose `sync` hook errors stay dirty for the next attempt; this
   * call returns without throwing in that case so the pagehide
   * handler can finish synchronously.
   *
   * Returns `true` if every dirty mount flushed cleanly, `false` if
   * any mount's `sync` hook errored.
   */
  syncAll() {
    return this.exports.kernel_sync_all() === 0;
  }
  /**
   * Register a host-imported file in the kernel's host-file table.
   * The bootstrap-side drag-drop / file-picker handler calls this
   * with the token it has assigned to the host `File`, the file's
   * name + mime, and the raw bytes the user dropped. A subsequent
   * userland `host_file_recv(token)` (extension opcode 0x1500)
   * consumes the entry and hands the bytes to the calling process
   * as a read-only fd.
   *
   * Metadata is copied once, then bytes are copied through repeated bounded
   * scratch windows. The kernel reserves the declared size before accepting
   * chunks, so files up to the shared 16 MiB import limit do not depend on
   * the 64 KiB syscall scratch size.
   */
  hostFileDropped(token, name, mime, bytes) {
    const enc = new TextEncoder();
    const nameBytes = enc.encode(name);
    const mimeBytes = enc.encode(mime);
    const heapLen = this.exports.kernel_heap_len();
    if (nameBytes.length + mimeBytes.length > heapLen || bytes.length > HOST_FILE_IMPORT_MAX_BYTES) {
      console.warn(
        `[pmos-kernel-worker] rejected host import token=${token}: metadata=${nameBytes.length + mimeBytes.length}, bytes=${bytes.length}`
      );
      return false;
    }
    const heapPtr = this.exports.kernel_heap_ptr();
    let view = new Uint8Array(
      this.exports.memory.buffer,
      heapPtr,
      nameBytes.length + mimeBytes.length
    );
    view.set(nameBytes, 0);
    view.set(mimeBytes, nameBytes.length);
    const begin = this.exports.kernel_host_file_drop_begin(
      token,
      nameBytes.length,
      mimeBytes.length,
      bytes.length
    );
    if (begin !== 0) {
      console.warn(
        `[pmos-kernel-worker] rejected host import token=${token} at begin: rc=${begin}`
      );
      return false;
    }
    for (let offset = 0; offset < bytes.length; offset += heapLen) {
      const chunk = bytes.subarray(
        offset,
        Math.min(offset + heapLen, bytes.length)
      );
      view = new Uint8Array(this.exports.memory.buffer, heapPtr, chunk.length);
      view.set(chunk);
      const chunkRc = this.exports.kernel_host_file_drop_chunk(
        token,
        chunk.length
      );
      if (chunkRc !== 0) {
        this.exports.kernel_host_file_drop_abort(token);
        console.warn(
          `[pmos-kernel-worker] rejected host import token=${token} at offset=${offset}: rc=${chunkRc}`
        );
        return false;
      }
    }
    const end = this.exports.kernel_host_file_drop_end(token);
    if (end !== 0) {
      this.exports.kernel_host_file_drop_abort(token);
      console.warn(
        `[pmos-kernel-worker] rejected host import token=${token} at end: rc=${end}`
      );
      return false;
    }
    console.info(
      `[pmos-kernel-worker] accepted host import token=${token} bytes=${bytes.length}`
    );
    this.notifyDispatchLoop();
    return true;
  }
  /**
   * Test-only: spawn a child process of `parent` with
   * ORDINARY_APP caps + console stdio, returning the child pid.
   * Mirror of the kernel-tests `spawn_ordinary_app` Rust helper.
   * Used by slice-2c.1 dispatcher tests to build parent/child
   * pairs for PROC_WAIT blocking scenarios.
   *
   * `parent` must already be registered + markedRunning; the
   * child is left in `Ready` state (the kernel's `proc_spawn`
   * auto-transitions children to `Ready`; the test harness may
   * bump to `Running` via `markRunning(child)` if the child
   * needs to dispatch its own syscalls).
   */
  spawnChildForTest(parent, name) {
    const nameLen = this.writeNameToHeapScratch(name);
    const rc = this.exports.kernel_register_process_for_spawn(
      parent,
      0,
      nameLen
    );
    if (rc < 0) {
      throw new Error(
        `KernelWasmHost.spawnChildForTest: kernel_register_process_for_spawn returned ${rc}`
      );
    }
    return rc;
  }
  // The kernel export reads the name from `HEAP_SCRATCH[0..name_len]`;
  // the ptr argument is unused but preserved for export-signature
  // stability. Returns UTF-8 byte length (not UTF-16 code-unit count)
  // so the kernel sees the exact byte window the JS side wrote.
  writeNameToHeapScratch(s) {
    const bytes = new TextEncoder().encode(s);
    const heapCap = this.exports.kernel_heap_len();
    if (bytes.length > heapCap) {
      throw new Error(
        `KernelWasmHost.writeNameToHeapScratch: ${bytes.length} > heap capacity ${heapCap}`
      );
    }
    const heapPtr = this.exports.kernel_heap_ptr();
    new Uint8Array(this.exports.memory.buffer, heapPtr, bytes.length).set(
      bytes
    );
    return bytes.length;
  }
  // ---- syscall dispatch ---------------------------------------------
  /**
   * Dispatch one syscall on behalf of `pid`. Encodes `request`,
   * writes `heapIn` to the kernel's heap scratch region if provided,
   * calls `kernel_dispatch`, and reads back the decoded response plus
   * any heap output the handler wrote.
   *
   * `request.heapPtr` is interpreted as an offset inside the heap
   * scratch region, not as a linear-memory pointer. The kernel's
   * handlers use the same convention — the heap scratch is a
   * contiguous buffer addressed starting at offset 0.
   */
  dispatch(pid, request, heapIn) {
    const reqBytes = encodeRequest(request);
    {
      const buf = this.exports.memory.buffer;
      const reqPtr = this.exports.kernel_req_ptr();
      new Uint8Array(buf, reqPtr, SLOT_SIZE).set(reqBytes);
      if (heapIn !== void 0 && heapIn.length > 0) {
        const heapPtr = this.exports.kernel_heap_ptr();
        const heapCap = this.exports.kernel_heap_len();
        const offset = request.heapPtr ?? 0;
        if (offset + heapIn.length > heapCap) {
          throw new Error(
            `KernelWasmHost.dispatch: heap payload ${offset}+${heapIn.length} > capacity ${heapCap}`
          );
        }
        new Uint8Array(buf, heapPtr + offset, heapIn.length).set(heapIn);
      }
    }
    const rc = this.exports.kernel_dispatch(pid);
    if (rc === 1) {
      return { heapOut: new Uint8Array(0), parked: true };
    }
    if (rc !== 0) {
      throw new Error(
        `KernelWasmHost.dispatch: kernel_dispatch returned ${rc}`
      );
    }
    const respBuf = this.exports.memory.buffer;
    const respPtr = this.exports.kernel_resp_ptr();
    const respBytes = new Uint8Array(
      new Uint8Array(respBuf, respPtr, SLOT_SIZE)
    );
    const response = decodeResponse(respBytes);
    let heapOut = new Uint8Array(0);
    if (response.extraLen > 0) {
      const heapBuf = this.exports.memory.buffer;
      const heapPtr = this.exports.kernel_heap_ptr();
      const offset = request.heapPtr ?? 0;
      const src = new Uint8Array(heapBuf, heapPtr + offset, response.extraLen);
      heapOut = new Uint8Array(src);
    }
    return { response, heapOut };
  }
  /**
   * Test-only: pop one wake for `pid` through the kernel's
   * `kernel_take_next_wake_for_pid` export and return the decoded
   * Response directly, without pushing onto any SAB. Used by unit
   * tests that want to assert wake-response shape without building
   * a full SAB transport.
   *
   * Production callers use `drainWakesForPid` (which pushes onto
   * the pid's SAB so the user Worker's Atomics.wait returns).
   */
  takeNextWakeForPid(pid) {
    if (this.exports.kernel_take_next_wake_for_pid(pid) !== 1) {
      return null;
    }
    const respPtr = this.exports.kernel_resp_ptr();
    const respBytes = new Uint8Array(
      new Uint8Array(this.exports.memory.buffer, respPtr, SLOT_SIZE)
    );
    return decodeResponse(respBytes);
  }
  /**
   * Test-only: pop one wake for `pid` and return the decoded
   * Response PLUS any heap bytes the wake carries. Heap bytes
   * are cloned from kernel HEAP_SCRATCH[0..extra_len]; `heapBytes
   * === null` when `response.extraLen === 0`.
   *
   * Mirrors `takeNextWakeForPid` but surfaces the `extra_len`
   * payload that `drainWakesForPid` would copy back into the SAB.
   * Used by dispatcher tests that assert the reaped-child-pid
   * readback shape without a full SAB round-trip.
   */
  takeNextWakeForPidWithHeap(pid) {
    if (this.exports.kernel_take_next_wake_for_pid(pid) !== 1) {
      return null;
    }
    const respPtr = this.exports.kernel_resp_ptr();
    const respBytes = new Uint8Array(
      new Uint8Array(this.exports.memory.buffer, respPtr, SLOT_SIZE)
    );
    const response = decodeResponse(respBytes);
    if (response.extraLen === 0) {
      return { response, heapBytes: null };
    }
    const heapPtrInKernel = this.exports.kernel_heap_ptr();
    const kernelHeap = new Uint8Array(
      this.exports.memory.buffer,
      heapPtrInKernel,
      response.extraLen
    );
    const heapBytes = new Uint8Array(kernelHeap);
    return { response, heapBytes };
  }
  /**
   * Drain any pending wakes queued for `pid` and push each response
   * onto `sab`'s response ring. Called by `startDispatchLoop` before
   * `serviceSab` on each pid so a previously-parked acceptor sees
   * its completed accept response on the next round-robin pass.
   *
   * Returns the number of wake responses pushed (0 if nothing was
   * queued). Uses the same push shape as `serviceSab` — decode via
   * RESP_SCRATCH, encode via `encodeResponse`, advance RES_HEAD.
   */
  drainWakesForPid(pid, sab) {
    if (sab.byteLength < SAB_SIZE) {
      throw new Error(
        `KernelWasmHost.drainWakesForPid: sab is ${sab.byteLength} bytes, need ${SAB_SIZE}`
      );
    }
    const buffer = sab.buffer;
    const baseOffset = sab.byteOffset;
    const header = new Int32Array(buffer, baseOffset, OFF_HEAP_SCRATCH / 4);
    let pushed = 0;
    while (this.exports.kernel_take_next_wake_for_pid(pid) === 1) {
      const respPtr = this.exports.kernel_resp_ptr();
      const respBytes = new Uint8Array(
        new Uint8Array(this.exports.memory.buffer, respPtr, SLOT_SIZE)
      );
      const response = decodeResponse(respBytes);
      const resHead = Atomics.load(header, OFF_RES_HEAD / 4);
      const resTail = Atomics.load(header, OFF_RES_TAIL / 4);
      const nextResHead = (resHead + 1 >>> 0) % RES_SLOT_COUNT;
      if (nextResHead === resTail) {
        throw new Error(
          `KernelWasmHost.drainWakesForPid: response ring full for pid ${pid}`
        );
      }
      const resSlotIx = (resHead >>> 0) % RES_SLOT_COUNT;
      const resSlotOffset = baseOffset + OFF_RES_RING + resSlotIx * SLOT_SIZE;
      const encoded = encodeResponse(response);
      new Uint8Array(buffer, resSlotOffset, SLOT_SIZE).set(encoded);
      if (response.extraLen > 0) {
        const heapPtrInSab = this.exports.kernel_resp_heap_ptr();
        const heapPtrInKernel = this.exports.kernel_heap_ptr();
        const kernelHeap = new Uint8Array(
          this.exports.memory.buffer,
          heapPtrInKernel,
          response.extraLen
        );
        const copy = new Uint8Array(kernelHeap);
        const sabHeapOffset = baseOffset + OFF_HEAP_SCRATCH + heapPtrInSab;
        new Uint8Array(buffer, sabHeapOffset, response.extraLen).set(copy);
      }
      Atomics.store(header, OFF_RES_HEAD / 4, nextResHead);
      pushed += 1;
    }
    return pushed;
  }
  /**
   * Service one pending request on the per-pid SAB ring.
   *
   * Pops one request out of the SAB's request ring, calls
   * [`dispatch`] on behalf of `pid`, pushes the response into the
   * SAB's response ring, and copies any heap output back into the
   * SAB's heap scratch region at the request's declared `heap_ptr`
   * offset.
   *
   * Return values:
   *
   *   * `0` — one request was serviced.
   *   * `1` — the request ring was empty; no work done.
   *
   * `sab` is a `Uint8Array` view over the full `SAB_SIZE` bytes of
   * the per-pid `SharedArrayBuffer`. Header atomics go through an
   * `Int32Array` view constructed over the same backing; slot bytes
   * are read and written directly.
   *
   * Wake-slot semaphores (`OFF_USER_WAIT_SLOT`,
   * `OFF_KERNEL_WAIT_SLOT`) are intentionally NOT touched by this
   * method — those are the kernel-Worker loop's concern. The caller
   * is responsible for notifying the user after the response lands.
   *
   * Design note — why this orchestration lives in TS rather than in
   * a kernel-side `kernel_service_sab` export (as
   * `multi-process-plan.md §2 Changing` speculated): the kernel's
   * WASM linear memory is a distinct address space from the SAB; a
   * `*mut u8` pointing into the SAB is not a valid pointer in the
   * kernel's memory, so the kernel cannot construct a
   * `ring::Sab::from_raw` over SAB bytes without a memcpy-each-way
   * through its own scratch region — and once the memcpy is on the
   * JS side, there is no remaining work for the kernel to do that
   * it does not already do inside the existing `kernel_dispatch`
   * export. The plan's §4 block is correct in substance; only the
   * language split moves.
   */
  serviceSab(pid, sab) {
    if (sab.byteLength < SAB_SIZE) {
      throw new Error(
        `KernelWasmHost.serviceSab: sab is ${sab.byteLength} bytes, need ${SAB_SIZE}`
      );
    }
    const buffer = sab.buffer;
    const baseOffset = sab.byteOffset;
    const header = new Int32Array(buffer, baseOffset, OFF_HEAP_SCRATCH / 4);
    const reqHead = Atomics.load(header, OFF_REQ_HEAD / 4);
    const reqTail = Atomics.load(header, OFF_REQ_TAIL / 4);
    if (reqHead === reqTail) {
      return 1;
    }
    const reqSlotIx = (reqTail >>> 0) % REQ_SLOT_COUNT;
    const reqSlotOffset = baseOffset + OFF_REQ_RING + reqSlotIx * SLOT_SIZE;
    const requestBytes = new Uint8Array(buffer, reqSlotOffset, SLOT_SIZE);
    const decoded = decodeRequest(requestBytes);
    let heapIn;
    if (decoded.heapLen > 0) {
      if (decoded.heapPtr + decoded.heapLen > HEAP_SCRATCH_BYTES || decoded.heapPtr > HEAP_SCRATCH_BYTES) {
        throw new Error(
          `KernelWasmHost.serviceSab: request heap ${decoded.heapPtr}+${decoded.heapLen} out of bounds (${HEAP_SCRATCH_BYTES})`
        );
      }
      const heapOffset = baseOffset + OFF_HEAP_SCRATCH + decoded.heapPtr;
      heapIn = new Uint8Array(
        new Uint8Array(buffer, heapOffset, decoded.heapLen)
      );
    }
    const dispatchResult = this.dispatch(
      pid,
      {
        opcode: decoded.opcode,
        flags: decoded.flags,
        requestId: decoded.requestId,
        args: decoded.args,
        heapPtr: decoded.heapPtr,
        heapLen: decoded.heapLen
      },
      heapIn
    );
    const nextTail = (reqTail + 1 >>> 0) % REQ_SLOT_COUNT;
    Atomics.store(header, OFF_REQ_TAIL / 4, nextTail);
    if (dispatchResult.parked === true) {
      return 0;
    }
    const response = dispatchResult.response;
    const heapOut = dispatchResult.heapOut;
    const resHead = Atomics.load(header, OFF_RES_HEAD / 4);
    const resTail = Atomics.load(header, OFF_RES_TAIL / 4);
    const nextResHead = (resHead + 1 >>> 0) % RES_SLOT_COUNT;
    if (nextResHead === resTail) {
      throw new Error(
        `KernelWasmHost.serviceSab: response ring full for pid ${pid}`
      );
    }
    const resSlotIx = (resHead >>> 0) % RES_SLOT_COUNT;
    const resSlotOffset = baseOffset + OFF_RES_RING + resSlotIx * SLOT_SIZE;
    const resBytes = encodeResponse(response);
    new Uint8Array(buffer, resSlotOffset, SLOT_SIZE).set(resBytes);
    if (response.extraLen > 0 && heapOut.length > 0) {
      const heapOffset = baseOffset + OFF_HEAP_SCRATCH + decoded.heapPtr;
      new Uint8Array(buffer, heapOffset, response.extraLen).set(heapOut);
    }
    Atomics.store(header, OFF_RES_HEAD / 4, nextResHead);
    return 0;
  }
  // ---- Kernel interface --------------------------------------------
  /**
   * Push bytes into a kernel device's input ring. Implements the
   * tight `Kernel` interface the existing driver scaffold uses.
   *
   * `devnum` is a [`Devnum`] value (`kernel::fs::devfs::DEV_*`) —
   * one per device NODE. This matches the convention the driver
   * scaffold's `pushInputToKernel` passes through and the
   * preview-slice `MockKernel.injectInput` also uses. The three
   * wired nodes are `/dev/console`, `/dev/input_kbd`, and
   * `/dev/input_mouse`; block/net input is deferred (those devices
   * are driven by the TS drivers from the other direction and don't
   * have a kernel-side input ring).
   */
  injectInput(devnum, bytes) {
    let injectFn;
    let fnName;
    if (devnum === Devnum.Console) {
      injectFn = this.exports.kernel_inject_console_input;
      fnName = "kernel_inject_console_input";
    } else if (devnum === Devnum.InputKbd) {
      injectFn = this.exports.kernel_inject_input_kbd;
      fnName = "kernel_inject_input_kbd";
    } else if (devnum === Devnum.InputMouse) {
      injectFn = this.exports.kernel_inject_input_mouse;
      fnName = "kernel_inject_input_mouse";
    } else {
      throw new Error(
        `KernelWasmHost.injectInput: devnum ${devnum} not supported; wired device nodes are Devnum.Console (${Devnum.Console}), Devnum.InputKbd (${Devnum.InputKbd}), Devnum.InputMouse (${Devnum.InputMouse})`
      );
    }
    const heapCap = this.exports.kernel_heap_len();
    if (bytes.length > heapCap) {
      throw new Error(
        `KernelWasmHost.injectInput: ${bytes.length} bytes > heap capacity ${heapCap}`
      );
    }
    if (bytes.length === 0) return;
    const buf = this.exports.memory.buffer;
    const heapPtr = this.exports.kernel_heap_ptr();
    new Uint8Array(buf, heapPtr, bytes.length).set(bytes);
    const rc = injectFn(bytes.length);
    if (rc !== 0) {
      throw new Error(`KernelWasmHost.injectInput: ${fnName} returned ${rc}`);
    }
    this.notifyDispatchLoop();
  }
  /**
   * Publish browser-substrate work to the kernel dispatch loop. Host input
   * and completed file imports both mutate kernel queues outside a user
   * process's SAB request path, so neither has a user Worker available to
   * bump the shared wake counter on its behalf.
   */
  /**
   * Notify the steady-state dispatcher after work arrives outside a user
   * process's syscall ring. Kernel-worker lifecycle messages use this for
   * newly published SABs and host-reconciled exits; both can otherwise leave
   * runnable work or a parked parent's completion behind an existing
   * `Atomics.waitAsync` epoch.
   */
  notifyDispatchLoop() {
    if (typeof SharedArrayBuffer !== "undefined" && this.wakeBuffer instanceof SharedArrayBuffer) {
      Atomics.add(this.wakeView, 0, 1);
      Atomics.notify(this.wakeView, 0);
    } else {
      this.wakeView[0] = this.wakeView[0] + 1 | 0;
    }
  }
  // ---- dispatch loop -------------------------------------------------
  /**
   * Shared kernel wake slot. 32 bytes backed by a `SharedArrayBuffer`
   * when the environment allows; a plain `ArrayBuffer` otherwise
   * (vitest under node). Every user Worker's `SabBackend` and the
   * browser-substrate event routing bumps `index 0` via `Atomics.add` +
   * `Atomics.notify` so the kernel's dispatch loop wakes from its
   * `Atomics.waitAsync` park.
   *
   * The slot is semantically "wake counter": notifiers increment it,
   * the parker reads it before waiting, and a spurious-wake-free
   * park returns as soon as the counter changes. Production code
   * should NEVER mutate the counter directly — use
   * [`notifyDispatchLoop`] for host-side work.
   */
  get wakeSlot() {
    return this.wakeView;
  }
  /** Re-check the kernel's bounded parked `poll_oneoff` sets. */
  servicePollWaiters() {
    return this.exports.kernel_service_poll_waiters();
  }
  /** Nanoseconds to the nearest poll clock, or `u64::MAX` for fd-only waits. */
  nextPollTimeoutNs() {
    return BigInt.asUintN(64, this.exports.kernel_next_poll_timeout_ns());
  }
  /**
   * Round-robin dispatch loop. Services every live pid's SAB ring up
   * to `budget` requests per pass; parks on `parkFn` when a pass
   * completes without work; exits when `halted()` returns true.
   *
   * The dispatch loop is the kernel Worker's steady-state after boot
   * (T233 / M1.4): the bootstrap pid (synthetic parent of init) runs
   * one in-process `dispatch(PROC_SPAWN init)` to kick the system
   * into motion, then the loop takes over. Spawned children arrive
   * via `proc:sab` messages from main (router in `bootstrap.ts`),
   * exits arrive via `proc:exited` — both bump the pidMap the caller
   * passes through `pidSource`, so the loop picks up every
   * lifecycle change at the start of the next pass.
   *
   * `parkFn` defaults to a `SharedArrayBuffer`-backed
   * `Atomics.waitAsync` on the shared wake slot. Fd-only waits are indefinite;
   * a poll clock supplies the nearest real deadline.
   * The loop snapshots the wake counter before scanning any rings and
   * passes that epoch to the parker. A notifier racing with the scan then
   * makes the wait return `not-equal` instead of being hidden by a fresh
   * post-scan load and sleeping until the timeout.
   * Under vitest (no cross-origin-isolated context), tests pass a
   * microtask-yield stub so the loop never actually blocks — the
   * test seeds the rings synchronously anyway.
   *
   * The loop is purely cooperative: a user Worker that never calls
   * a syscall ties up only its own Worker thread. That matches
   * `multi-process-plan.md §1` "Non-goals: pre-emption".
   */
  async startDispatchLoop(args) {
    const budget = args.budget ?? 8;
    const passesBeforeTaskYield = Math.max(
      1,
      Math.trunc(args.passesBeforeTaskYield ?? 4)
    );
    const parkFn = args.parkFn ?? ((observedWake, timeoutMs) => this.defaultPark(observedWake, timeoutMs));
    const taskYielder = args.taskYieldFn === void 0 ? new WorkerTaskYielder() : void 0;
    const taskYieldFn = args.taskYieldFn ?? (() => taskYielder.nextTask());
    const haveSharedArrayBuffer = typeof SharedArrayBuffer !== "undefined";
    let passesSinceTaskYield = 0;
    try {
      while (!args.halted()) {
        const observedWake = Atomics.load(this.wakeView, 0);
        let anyServiced = false;
        const pids = args.pidSource();
        for (const [pid, sab] of pids) {
          const view = new Uint8Array(sab);
          const header = new Int32Array(sab, 0, OFF_HEAP_SCRATCH / 4);
          const sabIsShared = haveSharedArrayBuffer && sab instanceof SharedArrayBuffer;
          for (let i = 0; i < budget; i++) {
            const resHeadBefore = Atomics.load(header, OFF_RES_HEAD / 4);
            const wakesPushed = this.drainWakesForPid(pid, view);
            if (wakesPushed > 0) {
              try {
                this.markRunning(pid);
              } catch {
              }
            }
            const rc = this.serviceSab(pid, view);
            if (rc === 0) {
              anyServiced = true;
            }
            const resHeadAfter = Atomics.load(header, OFF_RES_HEAD / 4);
            const responsePushed = resHeadAfter !== resHeadBefore;
            if (responsePushed) {
              Atomics.store(header, OFF_USER_WAIT_SLOT / 4, STATUS_READY);
              if (sabIsShared) {
                Atomics.notify(header, OFF_USER_WAIT_SLOT / 4);
              }
              anyServiced = true;
            }
            if (rc === 1) {
              break;
            }
          }
        }
        if (args.halted()) return;
        if (this.nextPollTimeoutNs() === 0n && this.servicePollWaiters() > 0) {
          anyServiced = true;
        }
        let parkedAsynchronously = false;
        if (!anyServiced) {
          if (this.servicePollWaiters() > 0) {
            anyServiced = true;
          } else {
            const timeoutNs = this.nextPollTimeoutNs();
            const timeoutMs = pollTimeoutMs(timeoutNs);
            parkedAsynchronously = await parkFn(observedWake, timeoutMs) === true;
          }
        }
        if (parkedAsynchronously) {
          passesSinceTaskYield = 0;
          continue;
        }
        passesSinceTaskYield += 1;
        if (passesSinceTaskYield >= passesBeforeTaskYield) {
          passesSinceTaskYield = 0;
          await taskYieldFn();
        }
      }
    } finally {
      taskYielder?.close();
    }
  }
  /**
   * Default [`startDispatchLoop`] park. Fd-only waits have no timeout;
   * clock-backed waits use exactly the nearest kernel deadline. Unsupported
   * runtimes fail explicitly instead of falling back to a polling timer.
   */
  async defaultPark(observedWake, timeoutMs) {
    const waitAsync = Atomics.waitAsync;
    if (typeof SharedArrayBuffer === "undefined" || !(this.wakeBuffer instanceof SharedArrayBuffer) || waitAsync === void 0) {
      throw new Error(
        "KernelWasmHost: blocking dispatcher requires SharedArrayBuffer and Atomics.waitAsync"
      );
    }
    const r = waitAsync(this.wakeView, 0, observedWake, timeoutMs);
    if (r.async) {
      await r.value;
      return true;
    }
    return false;
  }
};

// src/mock-kernel.ts
init_fb();

// src/shared/font.ts
var GLYPH_WIDTH = 5;
var GLYPH_HEIGHT = 7;
var CELL_WIDTH = 6;
var CELL_HEIGHT = 8;
var FIRST_CHAR = 32;
var LAST_CHAR = 126;
var GLYPH_COUNT = LAST_CHAR - FIRST_CHAR + 1;
var UNKNOWN_GLYPH = new Uint8Array([
  31,
  17,
  17,
  17,
  17,
  17,
  31
]);
var FONT_DATA = new Uint8Array(GLYPH_COUNT * GLYPH_HEIGHT);
function setGlyph(code, rows) {
  const base = (code - FIRST_CHAR) * GLYPH_HEIGHT;
  for (let i = 0; i < GLYPH_HEIGHT; i += 1) {
    FONT_DATA[base + i] = rows[i] ?? 0;
  }
}
setGlyph(33, [4, 4, 4, 4, 4, 0, 4]);
setGlyph(34, [10, 10, 10, 0, 0, 0, 0]);
setGlyph(35, [10, 10, 31, 10, 31, 10, 10]);
setGlyph(39, [4, 4, 4, 0, 0, 0, 0]);
setGlyph(40, [2, 4, 8, 8, 8, 4, 2]);
setGlyph(41, [8, 4, 2, 2, 2, 4, 8]);
setGlyph(42, [0, 10, 4, 31, 4, 10, 0]);
setGlyph(43, [0, 4, 4, 31, 4, 4, 0]);
setGlyph(44, [0, 0, 0, 0, 0, 4, 8]);
setGlyph(45, [0, 0, 0, 31, 0, 0, 0]);
setGlyph(46, [0, 0, 0, 0, 0, 0, 4]);
setGlyph(47, [1, 2, 2, 4, 8, 8, 16]);
setGlyph(48, [14, 17, 19, 21, 25, 17, 14]);
setGlyph(49, [4, 12, 4, 4, 4, 4, 14]);
setGlyph(50, [14, 17, 1, 2, 4, 8, 31]);
setGlyph(51, [31, 2, 4, 2, 1, 17, 14]);
setGlyph(52, [2, 6, 10, 18, 31, 2, 2]);
setGlyph(53, [31, 16, 30, 1, 1, 17, 14]);
setGlyph(54, [6, 8, 16, 30, 17, 17, 14]);
setGlyph(55, [31, 1, 2, 4, 8, 8, 8]);
setGlyph(56, [14, 17, 17, 14, 17, 17, 14]);
setGlyph(57, [14, 17, 17, 15, 1, 2, 12]);
setGlyph(58, [0, 4, 0, 0, 0, 4, 0]);
setGlyph(59, [0, 4, 0, 0, 4, 4, 8]);
setGlyph(60, [2, 4, 8, 16, 8, 4, 2]);
setGlyph(61, [0, 0, 31, 0, 31, 0, 0]);
setGlyph(62, [8, 4, 2, 1, 2, 4, 8]);
setGlyph(63, [14, 17, 2, 4, 4, 0, 4]);
setGlyph(65, [14, 17, 17, 31, 17, 17, 17]);
setGlyph(66, [30, 17, 17, 30, 17, 17, 30]);
setGlyph(67, [14, 17, 16, 16, 16, 17, 14]);
setGlyph(68, [30, 17, 17, 17, 17, 17, 30]);
setGlyph(69, [31, 16, 16, 30, 16, 16, 31]);
setGlyph(70, [31, 16, 16, 30, 16, 16, 16]);
setGlyph(71, [14, 17, 16, 23, 17, 17, 14]);
setGlyph(72, [17, 17, 17, 31, 17, 17, 17]);
setGlyph(73, [14, 4, 4, 4, 4, 4, 14]);
setGlyph(74, [7, 2, 2, 2, 2, 18, 12]);
setGlyph(75, [17, 18, 20, 24, 20, 18, 17]);
setGlyph(76, [16, 16, 16, 16, 16, 16, 31]);
setGlyph(77, [17, 27, 21, 21, 17, 17, 17]);
setGlyph(78, [17, 17, 25, 21, 19, 17, 17]);
setGlyph(79, [14, 17, 17, 17, 17, 17, 14]);
setGlyph(80, [30, 17, 17, 30, 16, 16, 16]);
setGlyph(81, [14, 17, 17, 17, 21, 18, 13]);
setGlyph(82, [30, 17, 17, 30, 20, 18, 17]);
setGlyph(83, [15, 16, 16, 14, 1, 1, 30]);
setGlyph(84, [31, 4, 4, 4, 4, 4, 4]);
setGlyph(85, [17, 17, 17, 17, 17, 17, 14]);
setGlyph(86, [17, 17, 17, 17, 17, 10, 4]);
setGlyph(87, [17, 17, 17, 21, 21, 27, 17]);
setGlyph(88, [17, 17, 10, 4, 10, 17, 17]);
setGlyph(89, [17, 17, 10, 4, 4, 4, 4]);
setGlyph(90, [31, 1, 2, 4, 8, 16, 31]);
setGlyph(91, [14, 8, 8, 8, 8, 8, 14]);
setGlyph(93, [14, 2, 2, 2, 2, 2, 14]);
setGlyph(95, [0, 0, 0, 0, 0, 0, 31]);
setGlyph(97, [0, 0, 14, 1, 15, 17, 15]);
setGlyph(98, [16, 16, 22, 25, 17, 17, 30]);
setGlyph(99, [0, 0, 14, 17, 16, 17, 14]);
setGlyph(100, [1, 1, 13, 19, 17, 17, 15]);
setGlyph(101, [0, 0, 14, 17, 31, 16, 14]);
setGlyph(102, [6, 9, 8, 30, 8, 8, 8]);
setGlyph(103, [0, 0, 15, 17, 15, 1, 14]);
setGlyph(104, [16, 16, 22, 25, 17, 17, 17]);
setGlyph(105, [4, 0, 12, 4, 4, 4, 14]);
setGlyph(106, [2, 0, 6, 2, 2, 18, 12]);
setGlyph(107, [16, 16, 18, 20, 24, 20, 18]);
setGlyph(108, [12, 4, 4, 4, 4, 4, 14]);
setGlyph(109, [0, 0, 26, 21, 21, 17, 17]);
setGlyph(110, [0, 0, 22, 25, 17, 17, 17]);
setGlyph(111, [0, 0, 14, 17, 17, 17, 14]);
setGlyph(112, [0, 0, 30, 17, 30, 16, 16]);
setGlyph(113, [0, 0, 15, 17, 15, 1, 1]);
setGlyph(114, [0, 0, 22, 25, 16, 16, 16]);
setGlyph(115, [0, 0, 15, 16, 14, 1, 30]);
setGlyph(116, [8, 8, 30, 8, 8, 9, 6]);
setGlyph(117, [0, 0, 17, 17, 17, 19, 13]);
setGlyph(118, [0, 0, 17, 17, 17, 10, 4]);
setGlyph(119, [0, 0, 17, 17, 21, 21, 10]);
setGlyph(120, [0, 0, 17, 10, 4, 10, 17]);
setGlyph(121, [0, 0, 17, 17, 15, 1, 14]);
setGlyph(122, [0, 0, 31, 2, 4, 8, 31]);
function glyphFor(c) {
  if (c.length === 0) {
    return UNKNOWN_GLYPH;
  }
  const code = c.charCodeAt(0);
  if (code === 32) {
    return new Uint8Array(GLYPH_HEIGHT);
  }
  if (code < FIRST_CHAR || code > LAST_CHAR) {
    return UNKNOWN_GLYPH;
  }
  const base = (code - FIRST_CHAR) * GLYPH_HEIGHT;
  const view = FONT_DATA.subarray(base, base + GLYPH_HEIGHT);
  let allZero = true;
  for (let i = 0; i < GLYPH_HEIGHT; i += 1) {
    if (view[i] !== 0) {
      allZero = false;
      break;
    }
  }
  if (allZero) {
    return UNKNOWN_GLYPH;
  }
  return view;
}
function glyphPixel(glyph, col, row) {
  if (col < 0 || col >= GLYPH_WIDTH || row < 0 || row >= GLYPH_HEIGHT) {
    return false;
  }
  const rowBits = glyph[row] ?? 0;
  const shift = GLYPH_WIDTH - 1 - col;
  return (rowBits >> shift & 1) !== 0;
}

// src/shared/rasterizer.ts
var PADDING = 4;
var BYTES_PER_PIXEL = 4;
var colors = {
  BG: 4278849044,
  FG_OUTPUT: 4293322470,
  FG_INPUT: 4286363647,
  FG_ERROR: 4294930544,
  FG_BANNER: 4286612881,
  CURSOR: 4294967295
};
var DEFAULT_PALETTE = {
  bg: colors.BG,
  banner: colors.FG_BANNER,
  input: colors.FG_INPUT,
  output: colors.FG_OUTPUT,
  error: colors.FG_ERROR,
  cursor: colors.CURSOR
};
function rasterizeSnapshot(snapshot, width, height, palette = DEFAULT_PALETTE) {
  const pixels = new Uint8Array(width * height * BYTES_PER_PIXEL);
  fillBg(pixels, palette.bg);
  if (width <= 2 * PADDING || height <= 2 * PADDING) {
    return pixels;
  }
  const textOriginX = PADDING;
  const textOriginY = PADDING;
  const textWidth = width - 2 * PADDING;
  const textHeight = height - 2 * PADDING;
  const cols = Math.floor(textWidth / CELL_WIDTH);
  const rowsTotal = Math.floor(textHeight / CELL_HEIGHT);
  if (cols === 0 || rowsTotal === 0) {
    return pixels;
  }
  const scrollbackRows = Math.max(0, rowsTotal - 1);
  const lines = snapshot.lines;
  const start = Math.max(0, lines.length - scrollbackRows);
  const visible = lines.slice(start);
  for (let rowIdx = 0; rowIdx < visible.length; rowIdx += 1) {
    const line = visible[rowIdx];
    if (!line) {
      continue;
    }
    const pixelY2 = textOriginY + rowIdx * CELL_HEIGHT;
    const fg = fgForKind(palette, line.kind);
    drawLine(pixels, width, height, textOriginX, pixelY2, cols, line.text, fg);
  }
  const inputRow = scrollbackRows;
  const pixelY = textOriginY + inputRow * CELL_HEIGHT;
  const combined = snapshot.prompt + snapshot.inputBuffer;
  drawLine(pixels, width, height, textOriginX, pixelY, cols, combined, palette.input);
  const cursorCol = combined.length;
  if (cursorCol < cols) {
    const cursorX = textOriginX + cursorCol * CELL_WIDTH;
    fillRect(
      pixels,
      width,
      height,
      cursorX,
      pixelY,
      GLYPH_WIDTH,
      GLYPH_HEIGHT,
      palette.cursor
    );
  }
  if (snapshot.cursor) {
    drawMouseCursor(
      pixels,
      width,
      height,
      snapshot.cursor.x,
      snapshot.cursor.y,
      palette.cursor
    );
  }
  return pixels;
}
var MOUSE_CURSOR_SPRITE = [
  // Horizontal bar.
  [-2, 0],
  [-1, 0],
  [0, 0],
  [1, 0],
  [2, 0],
  // Vertical bar (excluding center, already drawn).
  [0, -2],
  [0, -1],
  [0, 1],
  [0, 2]
];
function drawMouseCursor(pixels, fbWidth, fbHeight, x, y, argb) {
  for (const [dx, dy] of MOUSE_CURSOR_SPRITE) {
    setPixel(pixels, fbWidth, fbHeight, x + dx, y + dy, argb);
  }
}
function fgForKind(p, kind) {
  switch (kind) {
    case "banner":
      return p.banner;
    case "input":
      return p.input;
    case "output":
      return p.output;
    case "error":
      return p.error;
  }
}
function fillBg(pixels, argb) {
  const r = argb >>> 16 & 255;
  const g = argb >>> 8 & 255;
  const b = argb & 255;
  const a = argb >>> 24 & 255;
  for (let i = 0; i < pixels.length; i += BYTES_PER_PIXEL) {
    pixels[i] = r;
    pixels[i + 1] = g;
    pixels[i + 2] = b;
    pixels[i + 3] = a;
  }
}
function drawLine(pixels, fbWidth, fbHeight, originX, originY, cols, text, fg) {
  for (let i = 0; i < text.length; i += 1) {
    if (i >= cols) {
      break;
    }
    const ch = text.charAt(i);
    const glyph = glyphFor(ch);
    const x0 = originX + i * CELL_WIDTH;
    drawGlyph(pixels, fbWidth, fbHeight, glyph, x0, originY, fg);
  }
}
function drawGlyph(pixels, fbWidth, fbHeight, glyph, x0, y0, fg) {
  for (let row = 0; row < GLYPH_HEIGHT; row += 1) {
    for (let col = 0; col < GLYPH_WIDTH; col += 1) {
      if (!glyphPixel(glyph, col, row)) {
        continue;
      }
      setPixel(pixels, fbWidth, fbHeight, x0 + col, y0 + row, fg);
    }
  }
}
function fillRect(pixels, fbWidth, fbHeight, x0, y0, w, h, argb) {
  for (let dy = 0; dy < h; dy += 1) {
    for (let dx = 0; dx < w; dx += 1) {
      setPixel(pixels, fbWidth, fbHeight, x0 + dx, y0 + dy, argb);
    }
  }
}
function setPixel(pixels, fbWidth, fbHeight, x, y, argb) {
  if (x < 0 || x >= fbWidth || y < 0 || y >= fbHeight) {
    return;
  }
  const idx = (y * fbWidth + x) * BYTES_PER_PIXEL;
  if (idx + BYTES_PER_PIXEL > pixels.length) {
    return;
  }
  const r = argb >>> 16 & 255;
  const g = argb >>> 8 & 255;
  const b = argb & 255;
  const a = argb >>> 24 & 255;
  pixels[idx] = r;
  pixels[idx + 1] = g;
  pixels[idx + 2] = b;
  pixels[idx + 3] = a;
}

// src/shared/input-proto.ts
var MOUSE_EVENT_SIZE = 20;
var MouseEventKind = {
  /** Pointer moved to (x, y) in screen space. */
  Motion: 0,
  /** A mouse button was pressed or released at (x, y). */
  Button: 1,
  /** Wheel scrolled by `(button, state)` reinterpreted as
   *  `(deltaX, deltaY)` — see `packMouseWheel`. v1 reserves
   *  this discriminant for the wheel-scroll path the
   *  display server's window manager will route to focus
   *  windows. */
  Wheel: 2
};
var MouseButtonState = {
  Released: 0,
  Pressed: 1
};
function unpackMouseEvent(bytes) {
  if (bytes.byteLength < MOUSE_EVENT_SIZE) {
    return null;
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const kind = view.getUint32(0, true);
  if (kind !== MouseEventKind.Motion && kind !== MouseEventKind.Button && kind !== MouseEventKind.Wheel) {
    return null;
  }
  if (kind === MouseEventKind.Wheel) {
    return {
      kind,
      x: view.getInt32(4, true),
      y: view.getInt32(8, true),
      // `button` reinterprets `deltaX` u32-bits as a u32; tests
      // that use unpackMouseEvent on a wheel will see the raw
      // bits, which is fine because they're expected to use
      // unpackMouseWheel anyway.
      button: view.getUint32(12, true),
      state: MouseButtonState.Released
    };
  }
  const x = view.getInt32(4, true);
  const y = view.getInt32(8, true);
  const button = view.getUint32(12, true);
  const stateRaw = view.getUint32(16, true);
  if (stateRaw !== MouseButtonState.Released && stateRaw !== MouseButtonState.Pressed) {
    return null;
  }
  return {
    kind,
    x,
    y,
    button,
    state: stateRaw
  };
}
var KBD_EVENT_SIZE = 8;
var KbdKeyState = {
  Released: 0,
  Pressed: 1
};
function unpackKbdEvent(bytes) {
  if (bytes.byteLength < KBD_EVENT_SIZE) {
    return null;
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const key = view.getUint32(0, true);
  const stateRaw = view.getUint32(4, true);
  if (stateRaw !== KbdKeyState.Released && stateRaw !== KbdKeyState.Pressed) {
    return null;
  }
  return { key, state: stateRaw };
}

// src/mock-kernel.ts
var MockKernel = class {
  scaffold;
  policy;
  emitSplashOnFirstInput;
  liveTerminal;
  panicEmit;
  splashEmitted = false;
  /** Per-devnum line buffers — default + splash modes only. */
  lineBuffers = /* @__PURE__ */ new Map();
  /** Live-terminal state. */
  scrollback = [];
  liveInputBuffer = "";
  prompt;
  fbWidth;
  fbHeight;
  fbModeEmitted = false;
  /**
   * Sticky "we tried to start the fb driver and it rejected
   * us" flag. Set to true after the first `SET_MODE` attempt
   * that returns `NotReady` so subsequent keystrokes don't
   * retry or attempt to blit.
   */
  fbDisabled = false;
  /** Most recent decoded pointer position. `null` until the
   * first mouse motion event is injected.
   */
  pointer = null;
  /** Most recent button event, press or release. `null`
   * until the first button event arrives.
   */
  lastButton = null;
  /** Total number of mouse events the kernel has consumed. */
  mouseEventCount = 0;
  /** Total number of keyboard events consumed via the
   * `/dev/input/kbd` path (distinct from the console input
   * path the live terminal uses for scrollback).
   */
  kbdEventCount = 0;
  constructor(options) {
    this.policy = options.policy;
    this.emitSplashOnFirstInput = options.emitSplashOnFirstInput ?? false;
    this.liveTerminal = options.liveTerminal ?? false;
    this.panicEmit = options.panicEmit;
    this.prompt = options.prompt ?? "> ";
    this.fbWidth = options.fbWidth ?? SPLASH_WIDTH;
    this.fbHeight = options.fbHeight ?? SPLASH_HEIGHT;
    if (options.initialScrollback) {
      for (const line of options.initialScrollback) {
        this.scrollback.push({ text: line.text, kind: line.kind });
      }
    }
  }
  /**
   * Bind the scaffold after boot. Called by
   * `kernel-worker-entry.ts` immediately after
   * `bootKernelWorker` returns. Idempotent.
   */
  bindScaffold(scaffold) {
    this.scaffold = scaffold;
    if (this.liveTerminal) {
      this.renderAndBlit();
    }
  }
  injectInput(devnum, bytes) {
    if (devnum === DEV_INPUT_MOUSE_NODE) {
      this.injectMouseEvent(bytes);
      return;
    }
    if (devnum === DEV_INPUT_KBD_NODE) {
      this.injectKbdEvent(bytes);
      return;
    }
    if (devnum !== DEV_CONSOLE_NODE) {
      return;
    }
    if (this.liveTerminal) {
      this.injectLiveInput(bytes);
      return;
    }
    if (this.emitSplashOnFirstInput) {
      this.maybeEmitSplash();
    }
    let buf = this.lineBuffers.get(devnum);
    if (!buf) {
      buf = [];
      this.lineBuffers.set(devnum, buf);
    }
    for (const b of bytes) {
      buf.push(b);
      if (b === 10) {
        this.flushLine(devnum, buf);
        buf = [];
        this.lineBuffers.set(devnum, buf);
      }
    }
  }
  /**
   * Decode a packed mouse event from the `/dev/input/mouse`
   * device ring and update the tracked pointer state. A
   * motion event updates `pointer`; a button event updates
   * both `pointer` and `lastButton`. Malformed bytes are
   * silently dropped (the packer + unpacker are symmetric,
   * so the only failure mode is a length mismatch caused
   * by a caller bug).
   *
   * When live-terminal mode is on and a fresh pointer
   * position changes anything visible, the kernel re-renders
   * so the future cursor-drawing slice can land without
   * re-plumbing the blit trigger.
   */
  injectMouseEvent(bytes) {
    const evt = unpackMouseEvent(bytes);
    if (!evt) {
      return;
    }
    this.mouseEventCount += 1;
    this.pointer = { x: evt.x, y: evt.y };
    if (evt.kind === MouseEventKind.Button) {
      this.lastButton = evt;
    }
    if (this.liveTerminal) {
      this.renderAndBlit();
    }
  }
  /**
   * Decode a packed keyboard event from the
   * `/dev/input/kbd` device ring. v1 only records the
   * event in a counter for tests; real consumption
   * (focused-window routing, scancode → ASCII) lands with
   * the next slice that wires this path into the live
   * terminal. The existing `console:input` bytes path
   * still delivers typed characters to the scrollback so
   * the browser demo's typing behaviour is unchanged.
   */
  injectKbdEvent(bytes) {
    const evt = unpackKbdEvent(bytes);
    if (!evt) {
      return;
    }
    this.kbdEventCount += 1;
  }
  /**
   * Live-terminal per-byte keystroke processor. See
   * [`MockKernelOptions.liveTerminal`] for the wire protocol.
   */
  injectLiveInput(bytes) {
    let changed = false;
    for (const b of bytes) {
      if (b === 10) {
        this.commitLiveInputLine();
        changed = true;
      } else if (b === 127 || b === 8) {
        if (this.liveInputBuffer.length > 0) {
          this.liveInputBuffer = this.liveInputBuffer.slice(0, -1);
          changed = true;
        }
      } else if (b >= 32 && b <= 126) {
        this.liveInputBuffer += String.fromCharCode(b);
        changed = true;
      }
    }
    if (changed) {
      this.renderAndBlit();
    }
  }
  /**
   * Commit the current live input line: append it to
   * scrollback as an `input` line, run it through the
   * policy, append the output as `output` / `error` lines,
   * and reset the input buffer.
   */
  commitLiveInputLine() {
    const input = this.liveInputBuffer;
    this.liveInputBuffer = "";
    this.scrollback.push({
      text: `${this.prompt}${input}`,
      kind: "input"
    });
    const inputBytesWithNewline = new TextEncoder().encode(`${input}
`);
    if (this.tryHandlePanicCommand(inputBytesWithNewline)) {
      return;
    }
    const output = this.applyPolicy(inputBytesWithNewline);
    if (output.byteLength > 0) {
      this.scaffold?.callDriver(CONSOLE_DRIVER_ID, OP_WRITE_LINE, output);
      const outputText = new TextDecoder().decode(output);
      const trimmed = outputText.endsWith("\n") ? outputText.slice(0, -1) : outputText;
      for (const outLine of trimmed.split("\n")) {
        this.scrollback.push({ text: outLine, kind: "output" });
      }
    }
    while (this.scrollback.length > 256) {
      this.scrollback.shift();
    }
  }
  /**
   * Rasterize the current live-terminal snapshot and blit
   * it through the framebuffer driver. On the first call
   * also emits `OP_SET_MODE`. No-op if the scaffold isn't
   * bound, the fb driver has been marked disabled after a
   * prior `NotReady`, or the current SET_MODE attempt
   * fails.
   */
  renderAndBlit() {
    const scaffold = this.scaffold;
    if (!scaffold) {
      return;
    }
    if (this.fbDisabled) {
      return;
    }
    if (!this.fbModeEmitted) {
      const setModeResult = scaffold.callDriver(
        FB_DRIVER_ID,
        OP_SET_MODE,
        packFbSetMode(this.fbWidth, this.fbHeight)
      );
      this.fbModeEmitted = true;
      if (!setModeResult.ok) {
        this.fbDisabled = true;
        return;
      }
    }
    const snapshot = {
      lines: this.scrollback,
      inputBuffer: this.liveInputBuffer,
      prompt: this.prompt,
      ...this.pointer ? { cursor: this.pointer } : {}
    };
    const pixels = rasterizeSnapshot(snapshot, this.fbWidth, this.fbHeight);
    scaffold.callDriver(
      FB_DRIVER_ID,
      OP_BLIT,
      packFbBlit(this.fbWidth, this.fbHeight, pixels)
    );
  }
  maybeEmitSplash() {
    if (this.splashEmitted) {
      return;
    }
    const scaffold = this.scaffold;
    if (!scaffold) {
      return;
    }
    this.splashEmitted = true;
    const setModeResult = scaffold.callDriver(
      FB_DRIVER_ID,
      OP_SET_MODE,
      packFbSetMode(SPLASH_WIDTH, SPLASH_HEIGHT)
    );
    if (!setModeResult.ok) {
      return;
    }
    const snapshot = {
      lines: [
        { text: "PMos 0.1.0-demo", kind: "banner" },
        { text: "kernel worker ready", kind: "banner" },
        { text: "type 'help' for commands", kind: "banner" },
        { text: "", kind: "output" }
      ],
      inputBuffer: "",
      prompt: "> "
    };
    const pixels = rasterizeSnapshot(snapshot, SPLASH_WIDTH, SPLASH_HEIGHT);
    scaffold.callDriver(
      FB_DRIVER_ID,
      OP_BLIT,
      packFbBlit(SPLASH_WIDTH, SPLASH_HEIGHT, pixels)
    );
  }
  flushLine(devnum, lineBytes) {
    const scaffold = this.scaffold;
    if (!scaffold) {
      return;
    }
    const line = Uint8Array.from(lineBytes);
    if (this.tryHandlePanicCommand(line)) {
      return;
    }
    const output = this.applyPolicy(line);
    if (output.byteLength === 0) {
      return;
    }
    void devnum;
    scaffold.callDriver(CONSOLE_DRIVER_ID, OP_WRITE_LINE, output);
  }
  /**
   * If `line` is a `panic <message>` command, forward
   * the message to `panicEmit` (if wired) and return
   * true to short-circuit the rest of line handling.
   * Returns false otherwise.
   */
  tryHandlePanicCommand(line) {
    let end = line.byteLength;
    if (end > 0 && line[end - 1] === 10) {
      end -= 1;
    }
    const body = line.subarray(0, end);
    const text = new TextDecoder().decode(body);
    if (text === "panic") {
      this.panicEmit?.("kernel: panic command received with no message");
      return true;
    }
    if (text.startsWith("panic ")) {
      const message = text.slice("panic ".length);
      this.panicEmit?.(`kernel: ${message}`);
      return true;
    }
    return false;
  }
  applyPolicy(line) {
    switch (this.policy.kind) {
      case "echo":
        return line;
      case "faux-shell":
        return fauxShellTransform(line);
    }
  }
  // ---- Test helpers -------------------------------------
  /**
   * Read-only view of the live-terminal scrollback. Returns
   * an empty array when `liveTerminal` is false. Exposed so
   * tests can assert on internal state without touching
   * private fields.
   */
  get liveScrollback() {
    return this.scrollback;
  }
  /** Read-only view of the live-terminal input buffer. */
  get liveInput() {
    return this.liveInputBuffer;
  }
  /** Most recent pointer position seen via
   * `/dev/input/mouse`, or `null` if no mouse event has
   * been injected yet. */
  get pointerPosition() {
    return this.pointer === null ? null : { ...this.pointer };
  }
  /** Most recent button event, or `null` if none has
   * been injected. */
  get lastMouseButton() {
    return this.lastButton;
  }
  /** Total mouse events consumed. */
  get mouseEventsObserved() {
    return this.mouseEventCount;
  }
  /** Total keyboard events consumed via `/dev/input/kbd`. */
  get kbdEventsObserved() {
    return this.kbdEventCount;
  }
};
var FAUX_SHELL_HELP = [
  "commands:",
  "  help     \u2014 this list",
  "  echo X   \u2014 print X",
  "  date     \u2014 print build date",
  "  whoami   \u2014 print current user",
  "  uname    \u2014 print system banner",
  "  panic X  \u2014 trigger a kernel panic with message X"
];
function fauxShellTransform(line) {
  let end = line.byteLength;
  if (end > 0 && line[end - 1] === 10) {
    end -= 1;
  }
  const body = line.subarray(0, end);
  const bodyText = new TextDecoder().decode(body);
  if (bodyText.length === 0) {
    return new Uint8Array(0);
  }
  if (bodyText.startsWith("echo ")) {
    const rest = bodyText.slice("echo ".length);
    return new TextEncoder().encode(`${rest}
`);
  }
  if (bodyText === "help") {
    return new TextEncoder().encode(`${FAUX_SHELL_HELP.join("\n")}
`);
  }
  if (bodyText === "date") {
    return new TextEncoder().encode("2026-04-14\n");
  }
  if (bodyText === "whoami") {
    return new TextEncoder().encode("pmos\n");
  }
  if (bodyText === "uname") {
    return new TextEncoder().encode("PMos 0.1.0-demo\n");
  }
  return new TextEncoder().encode("?\n");
}
var SPLASH_WIDTH = 320;
var SPLASH_HEIGHT = 240;
function packFbSetMode(width, height) {
  const out = new Uint8Array(8);
  const v = new DataView(out.buffer);
  v.setUint32(0, width, true);
  v.setUint32(4, height, true);
  return out;
}
function packFbBlit(width, height, pixels) {
  const out = new Uint8Array(8 + pixels.byteLength);
  const v = new DataView(out.buffer);
  v.setUint32(0, width, true);
  v.setUint32(4, height, true);
  out.set(pixels, 8);
  return out;
}

// src/kernel-worker-entry.ts
var STORAGE_DETAIL_MAX_CHARS = 512;
function boundedStorageDetail(detail) {
  return detail.length <= STORAGE_DETAIL_MAX_CHARS ? detail : `${detail.slice(0, STORAGE_DETAIL_MAX_CHARS - 1)}\u2026`;
}
function storageDegradedMessageFromConsoleLine(line) {
  if (line.includes(
    "[pmos] persistent root unavailable or invalid; storage left untouched; using volatile tmpfs root"
  )) {
    return {
      kind: "storage:degraded",
      reason: "persistent-root-invalid",
      detail: "The existing filesystem image failed validation or mount; volatile recovery was prepared without rewriting it.",
      existingImagePreserved: true
    };
  }
  if (line.includes(
    "[pmos] persistent root unavailable; storage left untouched; using volatile tmpfs root"
  )) {
    return {
      kind: "storage:degraded",
      reason: "persistent-root-unavailable",
      detail: "The persistent filesystem could not be installed at /; volatile recovery was prepared without rewriting storage.",
      existingImagePreserved: true
    };
  }
  return null;
}
var MAX_DEFERRED_INPUT_BYTES = 64 * 1024;
function isDeferredInputMessage(msg) {
  return msg.kind === "console:input" || msg.kind === "input:kbd" || msg.kind === "input:mouse";
}
function installWorkerEntry(messaging, options = {}) {
  let scaffold;
  let realKernel;
  let resolveReady;
  const whenReady = new Promise((resolve) => {
    resolveReady = resolve;
  });
  const pidMap = /* @__PURE__ */ new Map();
  const lifecycle = { hasEverSpawned: false };
  let bootRequested = false;
  const deferredInput = [];
  let deferredInputBytes = 0;
  const deferInput = (msg) => {
    if (msg.bytes.byteLength > MAX_DEFERRED_INPUT_BYTES) {
      return;
    }
    while (deferredInput.length > 0 && deferredInputBytes + msg.bytes.byteLength > MAX_DEFERRED_INPUT_BYTES) {
      const dropped = deferredInput.shift();
      deferredInputBytes -= dropped?.bytes.byteLength ?? 0;
    }
    const bytes = new Uint8Array(msg.bytes.byteLength);
    bytes.set(msg.bytes);
    deferredInput.push({ ...msg, bytes });
    deferredInputBytes += bytes.byteLength;
  };
  const replayDeferredInput = (target) => {
    for (const msg of deferredInput) {
      target.handleMainMessage(msg);
    }
    deferredInput.length = 0;
    deferredInputBytes = 0;
  };
  messaging.onmessage = (ev) => {
    const msg = ev.data;
    if (msg.kind === "proc:sab") {
      pidMap.set(msg.pid, msg.sab);
      lifecycle.hasEverSpawned = true;
      if (realKernel !== void 0) {
        try {
          realKernel.markRunning(msg.pid);
        } catch {
        }
        realKernel.notifyDispatchLoop();
      }
      return;
    }
    if (msg.kind === "proc:memory") {
      if (realKernel !== void 0) {
        try {
          realKernel.recordProcessMemory(msg.pid, msg.bytes);
        } catch {
        }
      }
      return;
    }
    if (msg.kind === "sync:request") {
      if (realKernel !== void 0) {
        try {
          realKernel.syncAll();
        } catch {
        }
      }
      return;
    }
    if (msg.kind === "proc:exited") {
      let reconciledKnownProcess = false;
      if (msg.memoryBytes !== void 0 && realKernel !== void 0) {
        try {
          realKernel.recordProcessMemory(msg.pid, msg.memoryBytes);
        } catch {
        }
      }
      if (realKernel !== void 0) {
        try {
          const known = realKernel.reconcileProcessExit(
            msg.pid,
            msg.code,
            msg.trap !== void 0
          );
          if (known) {
            lifecycle.hasEverSpawned = true;
            reconciledKnownProcess = true;
          }
        } catch (error) {
          messaging.postMessage({
            kind: "panic",
            message: `kernel-worker: failed to reconcile pid ${msg.pid} exit: ${String(error)}`
          });
        }
      }
      const removedSab = pidMap.delete(msg.pid);
      if (realKernel !== void 0 && (reconciledKnownProcess || removedSab)) {
        realKernel.notifyDispatchLoop();
      }
      return;
    }
    if (msg.kind === "host:dropped") {
      if (realKernel !== void 0) {
        try {
          realKernel.hostFileDropped(msg.token, msg.name, msg.mime, msg.bytes);
        } catch (e) {
          messaging.postMessage({
            kind: "panic",
            message: `kernel-worker: host:dropped failed: ${e.message}`
          });
        }
      }
      return;
    }
    if (scaffold === void 0) {
      if (bootRequested && isDeferredInputMessage(msg)) {
        deferInput(msg);
        return;
      }
      if (msg.kind !== "boot") {
        messaging.postMessage({
          kind: "panic",
          message: `kernel-worker: ${msg.kind} received before boot`
        });
        return;
      }
      if (bootRequested) {
        messaging.postMessage({
          kind: "panic",
          message: "kernel-worker: duplicate boot received before boot completed"
        });
        return;
      }
      bootRequested = true;
      if (msg.config.useRealKernel === true) {
        void bootRealKernel(
          messaging,
          msg.config,
          options,
          pidMap,
          lifecycle,
          (s, h) => {
            scaffold = s;
            realKernel = h;
            replayDeferredInput(s);
          }
        ).then(() => resolveReady());
        return;
      }
      scaffold = bootMockKernel(messaging, msg.config);
      replayDeferredInput(scaffold);
      resolveReady();
      return;
    }
    scaffold.handleMainMessage(msg);
  };
  return {
    get scaffold() {
      return scaffold;
    },
    get realKernel() {
      return realKernel;
    },
    whenReady
  };
}
function bootMockKernel(messaging, config) {
  const liveTerminal = config.liveTerminal === true && config.enableFramebuffer;
  const initialScrollback = liveTerminal ? (config.terminalBanner ?? []).map((text) => ({
    text,
    kind: "banner"
  })) : void 0;
  const mock = new MockKernel({
    policy: { kind: "faux-shell" },
    emitSplashOnFirstInput: config.enableFramebuffer && !liveTerminal,
    liveTerminal,
    ...initialScrollback ? { initialScrollback } : {},
    panicEmit: (message) => {
      messaging.postMessage({ kind: "panic", message });
    }
  });
  const scaffold = bootKernelWorker({
    kernel: mock,
    config,
    postToMain(out) {
      messaging.postMessage(out);
    }
  });
  mock.bindScaffold(scaffold);
  return scaffold;
}
async function bootRealKernel(messaging, config, options, pidMap, lifecycle, onScaffoldReady) {
  let storageDegradedPosted = false;
  const reportStorageDegraded = (message) => {
    if (storageDegradedPosted) return;
    storageDegradedPosted = true;
    messaging.postMessage(message);
  };
  const fetcher = options.fetcher ?? defaultFetcher2;
  let bytes;
  try {
    bytes = options.kernelWasmBytes ?? await fetcher("/assets/kernel.wasm");
  } catch (e) {
    const message = `kernel-worker: failed to load /assets/kernel.wasm: ${String(e)}`;
    messaging.postMessage({ kind: "panic", message });
    throw e;
  }
  let registry = options.binaryRegistry;
  if (registry === void 0 && config.bootBinary !== void 0) {
    try {
      registry = await fetchBinaryRegistry(fetcher);
    } catch (e) {
      const message = `kernel-worker: failed to populate binary registry: ${String(e)}`;
      messaging.postMessage({ kind: "panic", message });
      throw e;
    }
  }
  let blockDriver;
  if (options.blockDriver !== void 0) {
    blockDriver = options.blockDriver;
  } else {
    try {
      if (options.openBlockDriver !== void 0) {
        blockDriver = await options.openBlockDriver();
      } else {
        const { BlockDriver: BlockDriver2 } = await Promise.resolve().then(() => (init_block(), block_exports));
        blockDriver = await BlockDriver2.openInOpfs();
      }
    } catch (error) {
      const detail = boundedStorageDetail(String(error));
      console.warn(
        `[pmos] persistent storage unavailable; volatile recovery prepared: ${detail}`
      );
      reportStorageDegraded({
        kind: "storage:degraded",
        reason: "opfs-open-failed",
        detail: `OPFS block driver open failed: ${detail}`,
        existingImagePreserved: true
      });
      blockDriver = void 0;
    }
  }
  let netDriver;
  if (options.netDriver !== void 0) {
    netDriver = options.netDriver;
  } else {
    try {
      const { NetDriver: NetDriver2 } = await Promise.resolve().then(() => (init_net(), net_exports));
      netDriver = new NetDriver2();
    } catch {
      netDriver = void 0;
    }
  }
  let framebufferDriver;
  try {
    const { FramebufferDriver: FramebufferDriver2 } = await Promise.resolve().then(() => (init_fb(), fb_exports));
    framebufferDriver = new FramebufferDriver2();
  } catch {
    framebufferDriver = void 0;
  }
  const host = await KernelWasmHost.create(bytes, {
    // Bytes the kernel flushes from `/dev/console` ride the existing
    // ConsoleHost main-thread channel as `console:write` messages,
    // so the boot screen + live terminal don't need to know whether
    // the source was MockKernel or KernelWasmHost.
    onConsoleWrite: (bytes2) => {
      const degraded = storageDegradedMessageFromConsoleLine(
        new TextDecoder().decode(bytes2)
      );
      if (degraded !== null) reportStorageDegraded(degraded);
      messaging.postMessage({ kind: "console:write", bytes: bytes2 });
    },
    onPanic: (message) => {
      messaging.postMessage({ kind: "panic", message });
    },
    ...registry !== void 0 ? { binaryRegistry: registry } : {},
    ...blockDriver !== void 0 ? { blockDriver } : {},
    ...netDriver !== void 0 ? { netDriver } : {},
    ...framebufferDriver !== void 0 ? { framebufferDriver } : {},
    onFramebufferMessage: (msg) => {
      messaging.postMessage(msg);
    },
    kernelWorkerChannel: {
      postMessage: (msg) => {
        messaging.postMessage(msg);
      }
    }
  });
  const scaffold = bootKernelWorker({
    kernel: host,
    config,
    postToMain(out) {
      messaging.postMessage(out);
    }
  });
  onScaffoldReady(scaffold, host);
  messaging.postMessage({
    kind: "kernel:wake-slot",
    sab: host.wakeSlot.buffer
  });
  if (config.bootBinary !== void 0) {
    await runBootBinary(host, config.bootBinary, pidMap, lifecycle);
  }
}
async function defaultFetcher2(url) {
  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`HTTP ${res.status} fetching ${url}`);
  }
  return res.arrayBuffer();
}
async function fetchBinaryRegistry(fetcher) {
  const manifestBuf = await fetcher("/manifest.json");
  const manifestJson = new TextDecoder().decode(new Uint8Array(manifestBuf));
  const manifest = JSON.parse(manifestJson);
  const binAssets = manifest.assets.filter(
    (a) => a.startsWith("assets/bin/") && a.endsWith(".wasm")
  );
  const assets = await Promise.all(
    binAssets.map(async (asset) => {
      const stem = asset.slice("assets/bin/".length, -".wasm".length);
      const bytes = await fetcher(`/${asset}`);
      return [stem, bytes];
    })
  );
  const entries = [];
  for (const [stem, bytes] of assets) {
    entries.push([`/bin/${stem}`, bytes], [`/usr/bin/${stem}`, bytes]);
  }
  return new Map(entries);
}
async function runBootBinary(host, bootBinary, pidMap, lifecycle) {
  const bootstrapPid = host.registerProcess(CAPSET_ALL);
  host.installConsoleFd(bootstrapPid, 0);
  host.installConsoleFd(bootstrapPid, 1);
  host.installConsoleFd(bootstrapPid, 2);
  host.installRootPreopenFd(bootstrapPid, 3);
  host.markRunning(bootstrapPid);
  const manifest = encodeSpawnManifest({
    path: bootBinary,
    caps: CAPSET_ALL
  });
  const { response } = host.dispatch(
    bootstrapPid,
    {
      opcode: OP_EXT.PROC_SPAWN,
      requestId: 1,
      args: manifest.args,
      heapPtr: 0,
      heapLen: manifest.heap.length
    },
    manifest.heap
  );
  if (response === void 0) {
    throw new Error(
      `kernel-worker: PROC_SPAWN(${bootBinary}) returned no response (parked?)`
    );
  }
  if (response.status !== 0) {
    throw new Error(
      `kernel-worker: PROC_SPAWN(${bootBinary}) failed with status ${response.status}`
    );
  }
  await host.startDispatchLoop({
    pidSource: () => pidMap,
    halted: () => lifecycle.hasEverSpawned && pidMap.size === 0
  });
}
if (typeof DedicatedWorkerGlobalScope !== "undefined" && typeof self !== "undefined" && self instanceof DedicatedWorkerGlobalScope) {
  installWorkerEntry(self);
}
export {
  installWorkerEntry,
  storageDegradedMessageFromConsoleLine
};
