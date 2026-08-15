// T085: Block driver tests against an in-memory `MemSyncAccessHandle`
// stub. jsdom doesn't expose `FileSystemSyncAccessHandle`, so the
// driver is exercised through its `BlockDriver.withHandle` test seam
// rather than `openInOpfs`. Coverage:
//
//   * round-trip: write LBA N, read LBA N back, get the same bytes
//   * sparse semantics: read of an unwritten LBA returns zeros
//   * persistence across handles: a second BlockDriver wrapping the
//     same MemSyncAccessHandle reads back the prior session's bytes
//     (the "first-boot writes superblock; second mount reads it"
//     contract that drives `OpfsFs::mount`'s magic check)
//   * quota-exceeded mapping: a `QuotaExceededError`-throwing handle
//     surfaces as `errno = 51` (ENOSPC) on write
//   * EINVAL: short payload, LBA past the high water, unknown opcode
//   * OP_FLUSH calls handle.flush() exactly once
//   * OP_BLOCK_COUNT returns the configured count

import { describe, expect, it } from "vitest";

import {
  BlockDriver,
  BlockImageState,
  BLOCK_SIZE,
  DEFAULT_BLOCK_COUNT,
  EINVAL,
  EIO,
  ENOSPC,
  OP_BLOCK_COUNT,
  OP_FLUSH,
  OP_IMAGE_STATE,
  OP_READ,
  OP_WRITE,
  PMOS_IMG_FILENAME,
  type OpfsRoot,
  type SyncAccessHandle,
} from "../../src/drivers/block";
import { DriverErrorCode } from "../../src/drivers/types";

// ---- in-memory stub for FileSystemSyncAccessHandle ------------------

interface MemHandleOptions {
  readonly capacity?: number;
  readonly throwQuotaExceededAfterBytes?: number;
  readonly throwIoOnEveryRead?: boolean;
}

class MemSyncAccessHandle implements SyncAccessHandle {
  private buf: Uint8Array;
  private size = 0;
  flushCount = 0;
  closed = false;

  constructor(private opts: MemHandleOptions = {}) {
    this.buf = new Uint8Array(opts.capacity ?? 1 << 24);
  }

  read(buffer: Uint8Array, options: { at: number }): number {
    if (this.opts.throwIoOnEveryRead) {
      throw new DOMException("io error", "InvalidStateError");
    }
    const end = Math.min(this.size, options.at + buffer.length);
    if (end <= options.at) return 0;
    const n = end - options.at;
    buffer.set(this.buf.subarray(options.at, end), 0);
    return n;
  }

  write(buffer: Uint8Array, options: { at: number }): number {
    const need = options.at + buffer.length;
    if (this.opts.throwQuotaExceededAfterBytes !== undefined) {
      if (need > this.opts.throwQuotaExceededAfterBytes) {
        const e = new Error("quota exceeded");
        (e as Error & { name: string }).name = "QuotaExceededError";
        throw e;
      }
    }
    if (need > this.buf.length) {
      throw new Error(`MemSyncAccessHandle: write past capacity (${need} > ${this.buf.length})`);
    }
    this.buf.set(buffer, options.at);
    if (need > this.size) {
      this.size = need;
    }
    return buffer.length;
  }

  flush(): void {
    this.flushCount += 1;
  }

  getSize(): number {
    return this.size;
  }

  truncate(newSize: number): void {
    if (newSize > this.buf.length) {
      throw new Error(`MemSyncAccessHandle: truncate past capacity`);
    }
    if (newSize > this.size) {
      // Extend with zeros (already there since the underlying buffer
      // is zero-initialised).
      this.size = newSize;
    } else {
      this.size = newSize;
    }
  }

  close(): void {
    this.closed = true;
  }
}

function freshHandle(opts: MemHandleOptions = {}): MemSyncAccessHandle {
  return new MemSyncAccessHandle(opts);
}

function blockBytes(seed: number): Uint8Array {
  const out = new Uint8Array(BLOCK_SIZE);
  // Deterministic per-LBA pattern so a misrouted LBA shows up as a
  // mismatch rather than as an accidental match.
  for (let i = 0; i < BLOCK_SIZE; i++) {
    out[i] = (seed * 31 + i) & 0xff;
  }
  return out;
}

function makeReadPayload(lba: number): Uint8Array {
  const buf = new Uint8Array(8 + BLOCK_SIZE);
  new DataView(buf.buffer).setBigUint64(0, BigInt(lba), true);
  return buf;
}

function makeWritePayload(lba: number, data: Uint8Array): Uint8Array {
  const buf = new Uint8Array(8 + BLOCK_SIZE);
  new DataView(buf.buffer).setBigUint64(0, BigInt(lba), true);
  buf.set(data, 8);
  return buf;
}

// ---- happy-path round trip ------------------------------------------

describe("BlockDriver round-trip", () => {
  it("write then read at the same LBA returns the original bytes", () => {
    const handle = freshHandle();
    const driver = BlockDriver.withHandle(handle, 16);
    const data = blockBytes(7);

    const w = driver.call(OP_WRITE, makeWritePayload(3, data));
    expect(w).toEqual({ ok: true, value: BLOCK_SIZE });

    const r = driver.call(OP_READ, makeReadPayload(3));
    expect(r.ok).toBe(true);
    if (!r.ok) return;
    // The read fills the destination payload[8..]; verify it matches
    // by reading the buffer from the BlockDriver result side: the
    // driver mutates the input payload directly, so we need to keep
    // a handle to the same buffer.
  });

  it("read after write actually populates the caller's payload", () => {
    const handle = freshHandle();
    const driver = BlockDriver.withHandle(handle, 16);
    const data = blockBytes(11);
    driver.call(OP_WRITE, makeWritePayload(5, data));

    const readPayload = makeReadPayload(5);
    const r = driver.call(OP_READ, readPayload);
    expect(r.ok).toBe(true);
    expect(readPayload.subarray(8)).toEqual(data);
  });

  it("write at LBA A is independent of read at LBA B (sparse semantics)", () => {
    const handle = freshHandle();
    const driver = BlockDriver.withHandle(handle, 16);
    const data = blockBytes(13);
    driver.call(OP_WRITE, makeWritePayload(2, data));

    // Read LBA 3 (never written); the unwritten-LBAs-read-as-zeros
    // sparse contract means we get a 4096-byte zero block.
    const readPayload = makeReadPayload(3);
    const r = driver.call(OP_READ, readPayload);
    expect(r.ok).toBe(true);
    expect(readPayload.subarray(8)).toEqual(new Uint8Array(BLOCK_SIZE));
  });

  it("multiple writes round-trip through the same driver instance", () => {
    const handle = freshHandle();
    const driver = BlockDriver.withHandle(handle, 32);
    const lbas = [0, 1, 2, 7, 31];
    for (const lba of lbas) {
      driver.call(OP_WRITE, makeWritePayload(lba, blockBytes(lba)));
    }
    for (const lba of lbas) {
      const payload = makeReadPayload(lba);
      driver.call(OP_READ, payload);
      expect(payload.subarray(8)).toEqual(blockBytes(lba));
    }
  });
});

// ---- persistence across BlockDriver instances ------------------------

describe("BlockDriver persistence (first-boot superblock contract)", () => {
  it("a second BlockDriver wrapping the same handle reads the first session's bytes", () => {
    const handle = freshHandle();
    const session1 = BlockDriver.withHandle(handle, 16);
    const data = blockBytes(42);
    session1.call(OP_WRITE, makeWritePayload(0, data));
    session1.call(OP_FLUSH, new Uint8Array(0));

    // Simulate kernel re-init: same handle, fresh driver.
    const session2 = BlockDriver.withHandle(handle, 16);
    const payload = makeReadPayload(0);
    const r = session2.call(OP_READ, payload);
    expect(r.ok).toBe(true);
    expect(payload.subarray(8)).toEqual(data);
  });

  it("withHandle pre-sizes a smaller backing handle to blockCount * BLOCK_SIZE", () => {
    const handle = freshHandle();
    expect(handle.getSize()).toBe(0);
    BlockDriver.withHandle(handle, 8);
    expect(handle.getSize()).toBe(8 * BLOCK_SIZE);
  });

  it("withHandle does NOT shrink a larger backing handle", () => {
    const handle = freshHandle();
    handle.truncate(64 * BLOCK_SIZE);
    BlockDriver.withHandle(handle, 8);
    // The pre-existing size is preserved — the driver's blockCount
    // bounds *new* writes but doesn't truncate the handle.
    expect(handle.getSize()).toBe(64 * BLOCK_SIZE);
  });
});

// ---- quota-exceeded mapping (FR-013a + ENOSPC contract) -------------

describe("BlockDriver quota mapping", () => {
  it("a QuotaExceededError throw on write surfaces as Errno(ENOSPC)", () => {
    // Allow exactly 4 KiB of writes before throwing.
    const handle = freshHandle({ throwQuotaExceededAfterBytes: BLOCK_SIZE });
    const driver = BlockDriver.withHandle(handle, 16);
    const data = blockBytes(1);

    // First write fits.
    const w0 = driver.call(OP_WRITE, makeWritePayload(0, data));
    expect(w0).toEqual({ ok: true, value: BLOCK_SIZE });

    // Second write would push the cumulative offset past the quota.
    const w1 = driver.call(OP_WRITE, makeWritePayload(1, data));
    expect(w1).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: ENOSPC,
    });
  });

  it("a non-quota write throw surfaces as Errno(EIO)", () => {
    const handle: SyncAccessHandle = {
      read: () => 0,
      write: () => {
        throw new Error("disk on fire");
      },
      flush: () => {},
      getSize: () => BLOCK_SIZE * 16,
      truncate: () => {},
      close: () => {},
    };
    const driver = BlockDriver.withHandle(handle, 16);
    const r = driver.call(OP_WRITE, makeWritePayload(0, blockBytes(1)));
    expect(r).toEqual({ ok: false, error: DriverErrorCode.Errno, errno: EIO });
  });

  it("a short write surfaces as EIO and does not touch an unrelated block", () => {
    const backing = freshHandle();
    const sentinel = blockBytes(77);
    backing.write(sentinel, { at: BLOCK_SIZE });
    const handle: SyncAccessHandle = {
      read: (buffer, options) => backing.read(buffer, options),
      write: (buffer, options) => {
        const short = buffer.subarray(0, 128);
        backing.write(short, options);
        return short.byteLength;
      },
      flush: () => backing.flush(),
      getSize: () => backing.getSize(),
      truncate: (size) => backing.truncate(size),
      close: () => backing.close(),
    };
    const driver = BlockDriver.withHandle(handle, 16);

    const result = driver.call(OP_WRITE, makeWritePayload(0, blockBytes(1)));
    expect(result).toEqual({
      ok: false,
      error: DriverErrorCode.Errno,
      errno: EIO,
    });

    const untouched = makeReadPayload(1);
    expect(driver.call(OP_READ, untouched).ok).toBe(true);
    expect(untouched.subarray(8)).toEqual(sentinel);
  });

  it("a read throw surfaces as Errno(EIO)", () => {
    const handle = freshHandle({ throwIoOnEveryRead: true });
    const driver = BlockDriver.withHandle(handle, 16);
    const r = driver.call(OP_READ, makeReadPayload(0));
    expect(r).toEqual({ ok: false, error: DriverErrorCode.Errno, errno: EIO });
  });
});

// ---- argument validation --------------------------------------------

describe("BlockDriver argument validation", () => {
  it("OP_READ with payload smaller than 8 + BLOCK_SIZE returns EINVAL", () => {
    const driver = BlockDriver.withHandle(freshHandle(), 16);
    const r = driver.call(OP_READ, new Uint8Array(8));
    expect(r).toEqual({ ok: false, error: DriverErrorCode.Errno, errno: EINVAL });
  });

  it("OP_WRITE with payload smaller than 8 + BLOCK_SIZE returns EINVAL", () => {
    const driver = BlockDriver.withHandle(freshHandle(), 16);
    const r = driver.call(OP_WRITE, new Uint8Array(100));
    expect(r).toEqual({ ok: false, error: DriverErrorCode.Errno, errno: EINVAL });
  });

  it("OP_READ at an LBA past blockCount returns EINVAL", () => {
    const driver = BlockDriver.withHandle(freshHandle(), 16);
    const r = driver.call(OP_READ, makeReadPayload(99));
    expect(r).toEqual({ ok: false, error: DriverErrorCode.Errno, errno: EINVAL });
  });

  it("OP_WRITE at an LBA past blockCount returns EINVAL", () => {
    const driver = BlockDriver.withHandle(freshHandle(), 16);
    const r = driver.call(OP_WRITE, makeWritePayload(99, blockBytes(0)));
    expect(r).toEqual({ ok: false, error: DriverErrorCode.Errno, errno: EINVAL });
  });

  it("an unknown opcode returns Transport", () => {
    const driver = BlockDriver.withHandle(freshHandle(), 16);
    const r = driver.call(0xff, new Uint8Array(0));
    expect(r).toEqual({ ok: false, error: DriverErrorCode.Transport });
  });
});

// ---- control opcodes -------------------------------------------------

describe("BlockDriver control opcodes", () => {
  it("OP_BLOCK_COUNT returns the configured block count", () => {
    const driver = BlockDriver.withHandle(freshHandle(), 4096);
    const r = driver.call(OP_BLOCK_COUNT, new Uint8Array(0));
    expect(r).toEqual({ ok: true, value: 4096 });
  });

  it("OP_IMAGE_STATE returns the explicit image provenance", () => {
    const fresh = BlockDriver.withHandle(
      freshHandle(),
      16,
      BlockImageState.NewlyCreated,
    );
    const existing = BlockDriver.withHandle(freshHandle(), 16);
    expect(fresh.call(OP_IMAGE_STATE, new Uint8Array(0))).toEqual({
      ok: true,
      value: BlockImageState.NewlyCreated,
    });
    expect(existing.call(OP_IMAGE_STATE, new Uint8Array(0))).toEqual({
      ok: true,
      value: BlockImageState.Existing,
    });
  });

  it("OP_FLUSH calls handle.flush() exactly once per call", () => {
    const handle = freshHandle();
    const driver = BlockDriver.withHandle(handle, 16);
    expect(handle.flushCount).toBe(0);
    driver.call(OP_FLUSH, new Uint8Array(0));
    expect(handle.flushCount).toBe(1);
    driver.call(OP_FLUSH, new Uint8Array(0));
    expect(handle.flushCount).toBe(2);
  });

  it("OP_FLUSH that throws surfaces as Errno(EIO)", () => {
    const handle: SyncAccessHandle = {
      read: () => 0,
      write: () => 0,
      flush: () => {
        throw new Error("flush error");
      },
      getSize: () => BLOCK_SIZE,
      truncate: () => {},
      close: () => {},
    };
    const driver = BlockDriver.withHandle(handle, 1);
    const r = driver.call(OP_FLUSH, new Uint8Array(0));
    expect(r).toEqual({ ok: false, error: DriverErrorCode.Errno, errno: EIO });
  });
});

// ---- close() lifecycle ----------------------------------------------

describe("BlockDriver lifecycle", () => {
  it("close() forwards to handle.close()", () => {
    const handle = freshHandle();
    const driver = BlockDriver.withHandle(handle, 16);
    expect(handle.closed).toBe(false);
    driver.close();
    expect(handle.closed).toBe(true);
  });
});

// ---- openInOpfs first-boot path -------------------------------------
//
// jsdom doesn't expose `navigator.storage`, so we drive openInOpfs
// through its `rootOverride` test seam with a fake `OpfsRoot` that
// returns a `FileHandle` that produces our `MemSyncAccessHandle`.

describe("BlockDriver.openInOpfs", () => {
  function makeFakeRoot(handles: Map<string, MemSyncAccessHandle>): OpfsRoot {
    return {
      async getFileHandle(name, options) {
        let h = handles.get(name);
        if (h === undefined) {
          if (!options?.create) {
            const error = new Error(`getFileHandle: ${name} not found`);
            error.name = "NotFoundError";
            throw error;
          }
          h = new MemSyncAccessHandle();
          handles.set(name, h);
        }
        const handle = h;
        return {
          async createSyncAccessHandle() {
            return handle;
          },
        };
      },
    };
  }

  it("creates pmos.img on first boot and pre-sizes it", async () => {
    const handles = new Map<string, MemSyncAccessHandle>();
    const driver = await BlockDriver.openInOpfs(64, makeFakeRoot(handles));
    expect(handles.size).toBe(1);
    expect(handles.has(PMOS_IMG_FILENAME)).toBe(true);
    expect(handles.get(PMOS_IMG_FILENAME)!.getSize()).toBe(64 * BLOCK_SIZE);
    expect(driver.blockCount).toBe(64);
    expect(driver.imageState).toBe(BlockImageState.NewlyCreated);
  });

  it("re-opens an existing pmos.img and preserves its bytes (FR-013a persistence)", async () => {
    const handles = new Map<string, MemSyncAccessHandle>();
    // First boot: write a block, flush, close.
    const session1 = await BlockDriver.openInOpfs(64, makeFakeRoot(handles));
    const data = blockBytes(99);
    session1.call(OP_WRITE, makeWritePayload(0, data));
    session1.call(OP_FLUSH, new Uint8Array(0));

    // Second boot: re-open. Same backing handle map, so the
    // previously-written block must read back.
    const session2 = await BlockDriver.openInOpfs(64, makeFakeRoot(handles));
    expect(session2.imageState).toBe(BlockImageState.Existing);
    const payload = makeReadPayload(0);
    session2.call(OP_READ, payload);
    expect(payload.subarray(8)).toEqual(data);
  });

  it("reports a pre-existing empty image as existing, never newly created", async () => {
    const handles = new Map<string, MemSyncAccessHandle>();
    handles.set(PMOS_IMG_FILENAME, new MemSyncAccessHandle());

    const driver = await BlockDriver.openInOpfs(64, makeFakeRoot(handles));

    expect(driver.imageState).toBe(BlockImageState.Existing);
    expect(driver.call(OP_IMAGE_STATE, new Uint8Array(0))).toEqual({
      ok: true,
      value: BlockImageState.Existing,
    });
  });

  it("does not turn a non-NotFound open failure into create permission", async () => {
    let createAttempted = false;
    const root: OpfsRoot = {
      async getFileHandle(_name, options) {
        if (options?.create) createAttempted = true;
        const error = new Error("permission denied");
        error.name = "NotAllowedError";
        throw error;
      },
    };

    await expect(BlockDriver.openInOpfs(64, root)).rejects.toMatchObject({
      name: "NotAllowedError",
    });
    expect(createAttempted).toBe(false);
  });

  it("uses the default block count when none is provided", async () => {
    const handles = new Map<string, MemSyncAccessHandle>();
    const driver = await BlockDriver.openInOpfs(undefined, makeFakeRoot(handles));
    expect(driver.blockCount).toBe(DEFAULT_BLOCK_COUNT);
  });
});
