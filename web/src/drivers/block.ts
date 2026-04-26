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
// First-boot creation: the SyncAccessHandle is opened with
// `{ create: true }` so a fresh OPFS gets the file. The handle's
// `.truncate(blockCount * BLOCK_SIZE)` pre-sizes it; reads past
// the high-water mark return zero bytes (sparse-file semantics
// match `MockBlockDevice`'s "unwritten LBAs read as zeros"
// contract). When the kernel's `OpfsFs::mount` reads LBA 0
// (superblock), it sees zeros, returns `FsError::Io`, and
// `kernel_init` then calls `mkfs(device)` which writes a fresh
// superblock + zeroes the inode table + journal.
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
 * Default block count: 4096 blocks * 4 KiB = 16 MiB. Above
 * `MIN_BLOCK_COUNT` (~386) on the kernel side, which is the
 * minimum size for a formatted OPFS image.
 */
export const DEFAULT_BLOCK_COUNT = 4096;

/** Block driver opcodes — mirrored on the Rust side in
 *  `crates/kernel/src/fs/opfs/block.rs`. */
export const OP_BLOCK_COUNT = 0x01;
export const OP_READ = 0x02;
export const OP_WRITE = 0x03;
export const OP_FLUSH = 0x04;

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
  ) {}

  /**
   * Open the block driver against the browser's OPFS root.
   * Creates `pmos.img` if it doesn't exist; pre-sizes it to
   * `blockCount * BLOCK_SIZE` bytes so the kernel sees a
   * fixed-size device. Idempotent: reopening an existing
   * `pmos.img` keeps its contents (the kernel reads the
   * superblock and either mounts the existing FS or runs mkfs
   * if the image is freshly zeroed).
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
    const file = await root.getFileHandle(PMOS_IMG_FILENAME, { create: true });
    const handle = await file.createSyncAccessHandle();
    const expectedSize = blockCount * BLOCK_SIZE;
    if (handle.getSize() < expectedSize) {
      handle.truncate(expectedSize);
    }
    return new BlockDriver(handle, blockCount);
  }

  /**
   * Test seam: construct a `BlockDriver` with a pre-built
   * `SyncAccessHandle`. Tests use this with the in-memory
   * `MemSyncAccessHandle` from `web/tests/unit/block.test.ts` so
   * the driver behaviour can be exercised without touching the
   * real OPFS surface (which jsdom doesn't expose).
   */
  static withHandle(
    handle: SyncAccessHandle,
    blockCount: number = DEFAULT_BLOCK_COUNT,
  ): BlockDriver {
    const expectedSize = blockCount * BLOCK_SIZE;
    if (handle.getSize() < expectedSize) {
      handle.truncate(expectedSize);
    }
    return new BlockDriver(handle, blockCount);
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
      this.handle.write(src, { at: offset });
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
