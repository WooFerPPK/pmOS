// Block driver — the TS half of the OPFS-backed disk.
//
// The kernel's `OpfsFs` (`crates/kernel/src/fs/opfs/`) is written
// against a `BlockDevice` trait that does fixed-size 4 KiB block
// I/O. In the browser the trait is implemented by `WasmBlockDevice`
// which forwards every read/write/flush through `Platform::driver_call`
// (DevId::Block). This file is the other end of that call: a
// `Driver` implementation that owns a single
// `FileSystemSyncAccessHandle` to a sparse OPFS file
// `pmos.img`. Each LBA maps to byte offset `lba * 4096` inside
// that file; the kernel's superblock + journal + inode table +
// data blocks all live as ranges within the same file, mirroring
// the kernel's layout module.
//
// First-boot creation: the driver first opens `pmos.img` without
// `create`, and only creates it after a `NotFoundError`. It reports
// that provenance through OP_IMAGE_STATE. The kernel formats only a
// newly-created image; an existing empty or corrupt image is never
// reformatted after a mount error.
//
// Quota mapping: a `QuotaExceededError` from `.write()` is
// surfaced as `errno = ENOSPC = 51`, propagating through the
// kernel as `FsError::NoSpace` and ultimately as `-ENOSPC` from
// the WASI write syscall — the contract `WasmBlockDevice::write`
// pins.

import type { Driver, DriverHost, DriverResult } from "./types";
import { DriverErrorCode } from "./types";
import { DriverId } from "../shared/platform-constants";

/** Driver-class identifier for the block driver. */
export const BLOCK_DRIVER_ID = DriverId.Block;

/** OPFS block size — must match `BLOCK_SIZE` on the Rust side. */
export const BLOCK_SIZE = 4096;

/** OPFS errno used for quota-exceeded writes. Mirrors `abi::errno::ENOSPC`. */
export const ENOSPC = 51;
/** OPFS errno used for any other I/O failure. Mirrors `abi::errno::EIO`. */
export const EIO = 5;
/** OPFS errno for invalid arguments (e.g., short read/write payload). */
export const EINVAL = 22;

/**
 * Default block count: 4096 blocks * 4 KiB = 16 MiB. Above the kernel's
 * exact 1498-block minimum (complete bundled root plus layout metadata),
 * leaving about 10 MiB for user data on a new image.
 */
export const DEFAULT_BLOCK_COUNT = 4096;

/** Block driver opcodes — mirrored on the Rust side in
 *  `crates/kernel/src/fs/opfs/block.rs`. */
export const OP_BLOCK_COUNT = 0x01;
export const OP_READ = 0x02;
export const OP_WRITE = 0x03;
export const OP_FLUSH = 0x04;
export const OP_IMAGE_STATE = 0x05;

/** Image provenance returned by `OP_IMAGE_STATE`. */
export const BlockImageState = {
  Existing: 0,
  NewlyCreated: 1,
} as const;
export type BlockImageState =
  (typeof BlockImageState)[keyof typeof BlockImageState];

/** OPFS image filename in the OPFS root directory. */
export const PMOS_IMG_FILENAME = "pmos.img";

/**
 * Subset of `FileSystemSyncAccessHandle` the driver actually uses.
 * Lets vitest tests inject a stub backing store without needing a
 * real browser OPFS implementation.
 */
export interface SyncAccessHandle {
  read(buffer: Uint8Array, options: { at: number }): number;
  write(buffer: Uint8Array, options: { at: number }): number;
  flush(): void;
  getSize(): number;
  truncate(newSize: number): void;
  close(): void;
}

/**
 * Subset of the OPFS root directory + file handle surface the
 * driver uses. `navigator.storage.getDirectory()` returns a
 * `FileSystemDirectoryHandle`; we only need `getFileHandle` and
 * `createSyncAccessHandle`. Tests pass a stub root.
 */
export interface OpfsRoot {
  getFileHandle(
    name: string,
    options?: { create?: boolean },
  ): Promise<{
    createSyncAccessHandle(): Promise<SyncAccessHandle>;
  }>;
}

/**
 * Read a little-endian u64 from `bytes[offset..offset+8]` as a
 * JS number. The OPFS image won't exceed `Number.MAX_SAFE_INTEGER`
 * blocks (2^53 / 4096 = ~2.25 PiB blocks), so the precision loss
 * never matters in practice; we still assert the high 21 bits are
 * zero so a broken kernel can't sneak past the safe-integer
 * window unnoticed.
 */
function readU64LE(bytes: Uint8Array, offset: number): number {
  const v = new DataView(bytes.buffer, bytes.byteOffset + offset, 8);
  const lo = v.getUint32(0, true);
  const hi = v.getUint32(4, true);
  if (hi > 0x1f_ffff) {
    throw new Error(
      `BlockDriver: LBA ${hi}.${lo} exceeds JavaScript safe-integer range`,
    );
  }
  return hi * 0x1_0000_0000 + lo;
}

export class BlockDriver implements Driver {
  readonly driverId = BLOCK_DRIVER_ID;
  readonly name = "block";

  private constructor(
    private handle: SyncAccessHandle,
    public readonly blockCount: number,
    public readonly imageState: BlockImageState,
  ) {}

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
  static async openInOpfs(
    blockCount: number = DEFAULT_BLOCK_COUNT,
    rootOverride?: OpfsRoot,
  ): Promise<BlockDriver> {
    const root: OpfsRoot =
      rootOverride ??
      (await navigator.storage.getDirectory() as unknown as OpfsRoot);
    let file: Awaited<ReturnType<OpfsRoot["getFileHandle"]>>;
    let imageState: BlockImageState;
    try {
      file = await root.getFileHandle(PMOS_IMG_FILENAME);
      imageState = BlockImageState.Existing;
    } catch (e: unknown) {
      if (!isNotFound(e)) throw e;
      file = await root.getFileHandle(PMOS_IMG_FILENAME, { create: true });
      imageState = BlockImageState.NewlyCreated;
    }
    const handle = await file.createSyncAccessHandle();
    // Another context may populate the file between the existence check and
    // access-handle creation. If that happened, preserve it as existing.
    if (imageState === BlockImageState.NewlyCreated && handle.getSize() !== 0) {
      imageState = BlockImageState.Existing;
    }
    const expectedSize = blockCount * BLOCK_SIZE;
    if (handle.getSize() < expectedSize) {
      handle.truncate(expectedSize);
    }
    return new BlockDriver(handle, blockCount, imageState);
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
  static withHandle(
    handle: SyncAccessHandle,
    blockCount: number = DEFAULT_BLOCK_COUNT,
    imageState: BlockImageState = BlockImageState.Existing,
  ): BlockDriver {
    const expectedSize = blockCount * BLOCK_SIZE;
    if (handle.getSize() < expectedSize) {
      handle.truncate(expectedSize);
    }
    return new BlockDriver(handle, blockCount, imageState);
  }

  init(_host: DriverHost): void {
    // The block driver is purely request/response — no main-thread
    // events to push. The DriverHost is unused.
  }

  call(op: number, payload: Uint8Array): DriverResult {
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
  close(): void {
    this.handle.close();
  }

  private read(payload: Uint8Array): DriverResult {
    if (payload.byteLength < 8 + BLOCK_SIZE) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    let lba: number;
    try {
      lba = readU64LE(payload, 0);
    } catch {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    if (lba >= this.blockCount) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    const offset = lba * BLOCK_SIZE;
    // The destination view points into the kernel's wasm linear
    // memory; the SyncAccessHandle reads directly into it without
    // an intermediate copy.
    const dest = new Uint8Array(payload.buffer, payload.byteOffset + 8, BLOCK_SIZE);
    let n: number;
    try {
      n = this.handle.read(dest, { at: offset });
    } catch {
      return { ok: false, error: DriverErrorCode.Errno, errno: EIO };
    }
    // Short read (LBA past EOF, or a sparse hole). Zero the
    // tail so the caller sees the "unwritten LBA reads as
    // zeros" sparse-file semantics that `MockBlockDevice`
    // implements. `truncate` at openInOpfs time pre-sizes the
    // file so this branch is mostly defence-in-depth, but
    // first-boot reads of pmos.img-just-created hit it.
    if (n < BLOCK_SIZE) {
      dest.fill(0, n);
    }
    return { ok: true, value: BLOCK_SIZE };
  }

  private write(payload: Uint8Array): DriverResult {
    if (payload.byteLength < 8 + BLOCK_SIZE) {
      return { ok: false, error: DriverErrorCode.Errno, errno: EINVAL };
    }
    let lba: number;
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
    } catch (e: unknown) {
      const errno = isQuotaExceeded(e) ? ENOSPC : EIO;
      return { ok: false, error: DriverErrorCode.Errno, errno };
    }
  }

  private flushOp(): DriverResult {
    try {
      this.handle.flush();
      return { ok: true, value: 0 };
    } catch {
      return { ok: false, error: DriverErrorCode.Errno, errno: EIO };
    }
  }
}

/**
 * `e instanceof DOMException && e.name === "QuotaExceededError"`
 * doesn't work cross-browser because some impls throw a plain
 * `Error` with `name === "QuotaExceededError"` instead of a real
 * `DOMException`. Match by name only.
 */
function isQuotaExceeded(e: unknown): boolean {
  if (typeof e !== "object" || e === null) return false;
  const cand = e as { name?: unknown };
  return cand.name === "QuotaExceededError";
}

function isNotFound(e: unknown): boolean {
  if (typeof e !== "object" || e === null) return false;
  const cand = e as { name?: unknown };
  return cand.name === "NotFoundError";
}
