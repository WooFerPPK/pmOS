// Typed wire format for the PMos syscall transport.
//
// Hand-maintained TypeScript mirror of the Rust `abi` crate:
//
//   * `crates/abi/src/ring.rs` — `Request` / `Response` struct layout
//   * `crates/abi/src/wasi.rs` — WASI preview 1 opcode constants
//   * `crates/abi/src/ext.rs`  — PMos extension opcode constants
//   * `crates/abi/src/errno.rs` — errno constants (positive values)
//   * `crates/abi/src/cap.rs`  — capability enum
//   * `crates/kernel/src/platform/mod.rs` — `DevId` enum
//
// Kept hand-written for now because the opcode / errno / cap tables
// are small enough that drift is manageable and a Vitest round-trip
// test at `web/tests/unit/syscall.test.ts` would catch any mismatch
// mechanically. A future slice should promote this file to
// autogeneration by `cargo run -p xtask -- gen-sab-layout` the same
// way `./sab-layout.ts` works today.
//
// The Rust side stays normative: if these constants ever disagree
// with the abi crate, the abi crate wins and this file is wrong.

/* eslint-disable @typescript-eslint/naming-convention */

// ---- Request / Response byte layout ---------------------------------

/** Size of a request / response slot in bytes. */
export const SLOT_SIZE = 32;

/**
 * A syscall request as the caller wants the kernel to see it. Mirrors
 * `abi::ring::Request` but with ergonomic fields instead of the raw
 * 16-byte inline args window.
 *
 * For the common case where a handler reads a single `u32` at
 * `args[0..4]` (almost every WASI opcode we've implemented so far),
 * set `arg0` and leave `args` undefined. If a handler needs a more
 * complex args layout, pass a 16-byte `args` Uint8Array directly and
 * leave `arg0` undefined.
 */
export interface SyscallRequest {
  readonly opcode: number;
  readonly requestId: number;
  /** Reserved in v1; MUST be 0. */
  readonly flags?: number;
  /** Single u32 written at `args[0..4]`. Mutually exclusive with `args`. */
  readonly arg0?: number;
  /** Full 16-byte inline args. Mutually exclusive with `arg0`. */
  readonly args?: Uint8Array;
  /** Offset into the heap scratch region where a payload lives. */
  readonly heapPtr?: number;
  /** Length of the payload at `heapPtr`. */
  readonly heapLen?: number;
}

/** Decoded fields of a [`Response`] slot. */
export interface SyscallResponse {
  readonly requestId: number;
  /** 0 on success, negative errno on failure. */
  readonly status: number;
  /** Primary return value (widened to `bigint` because Rust-side is `i64`). */
  readonly value: bigint;
  /** Length of any payload the handler wrote to the heap scratch region. */
  readonly extraLen: number;
}

/**
 * Encode a [`SyscallRequest`] into the 32-byte little-endian layout
 * the kernel's dispatcher reads from its request slot.
 *
 * Inverse of [`decodeResponse`] / `Request::to_le_bytes` on the Rust
 * side.
 */
export function encodeRequest(req: SyscallRequest): Uint8Array {
  const buf = new Uint8Array(SLOT_SIZE);
  const view = new DataView(buf.buffer);
  view.setUint16(0, req.opcode, true);
  view.setUint16(2, req.flags ?? 0, true);
  view.setUint32(4, req.requestId, true);
  if (req.args !== undefined) {
    if (req.args.length !== 16) {
      throw new Error(`syscall.encodeRequest: args must be 16 bytes, got ${req.args.length}`);
    }
    if (req.arg0 !== undefined) {
      throw new Error("syscall.encodeRequest: pass either args or arg0, not both");
    }
    buf.set(req.args, 8);
  } else if (req.arg0 !== undefined) {
    view.setUint32(8, req.arg0, true);
  }
  view.setUint32(24, req.heapPtr ?? 0, true);
  view.setUint32(28, req.heapLen ?? 0, true);
  return buf;
}

/**
 * Decode a 32-byte [`Response`] slot into the semantically-interesting
 * fields. Padding is dropped because no handler uses it yet.
 *
 * Inverse of `Response::from_le_bytes` on the Rust side.
 */
export function decodeResponse(bytes: Uint8Array): SyscallResponse {
  if (bytes.length !== SLOT_SIZE) {
    throw new Error(`syscall.decodeResponse: expected ${SLOT_SIZE} bytes, got ${bytes.length}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, SLOT_SIZE);
  return {
    requestId: view.getUint32(0, true),
    status: view.getInt32(4, true),
    value: view.getBigInt64(8, true),
    extraLen: view.getUint32(16, true),
  };
}

/**
 * Encode a [`SyscallResponse`] into the 32-byte little-endian layout
 * the kernel produces. Padding bytes are zeroed. Inverse of
 * [`decodeResponse`] and of `Response::to_le_bytes` on the Rust side.
 *
 * Used by the SAB-ring servicing path, which reads a decoded response
 * out of `KernelWasmHost.dispatch` and needs to write it back to the
 * per-pid SAB response ring in the exact byte layout the user-side
 * `Sab::try_pop_response` expects.
 */
export function encodeResponse(res: SyscallResponse): Uint8Array {
  const buf = new Uint8Array(SLOT_SIZE);
  const view = new DataView(buf.buffer);
  view.setUint32(0, res.requestId, true);
  view.setInt32(4, res.status, true);
  view.setBigInt64(8, res.value, true);
  view.setUint32(16, res.extraLen, true);
  return buf;
}

/** Decoded fields of a [`Request`] slot; mirror of [`SyscallRequest`]
 * but with fully-populated `args` (16 bytes, owned) and explicit
 * `flags`/`heapPtr`/`heapLen`. Used by the SAB-ring servicing path. */
export interface DecodedRequest {
  readonly opcode: number;
  readonly flags: number;
  readonly requestId: number;
  /** The full 16-byte inline args window, copied out of the slot. */
  readonly args: Uint8Array;
  readonly heapPtr: number;
  readonly heapLen: number;
}

/**
 * Decode a 32-byte request slot as the inverse of [`encodeRequest`]
 * and of `Request::from_le_bytes` on the Rust side. The returned
 * `args` field is a fresh owned `Uint8Array` — it does not alias the
 * input bytes.
 */
export function decodeRequest(bytes: Uint8Array): DecodedRequest {
  if (bytes.length !== SLOT_SIZE) {
    throw new Error(`syscall.decodeRequest: expected ${SLOT_SIZE} bytes, got ${bytes.length}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, SLOT_SIZE);
  return {
    opcode: view.getUint16(0, true),
    flags: view.getUint16(2, true),
    requestId: view.getUint32(4, true),
    args: bytes.slice(8, 24),
    heapPtr: view.getUint32(24, true),
    heapLen: view.getUint32(28, true),
  };
}

// ---- Opcode constants -----------------------------------------------
//
// Mirror of `abi::wasi` + `abi::ext`. Only the opcodes the dispatcher
// currently implements have named constants; adding a new one here is
// mechanical and happens when the Rust-side handler lands.

/** WASI preview 1 opcodes (0x0001..0x0080). */
export const OP_WASI = {
  ARGS_GET: 0x0001,
  ARGS_SIZES_GET: 0x0002,
  ENVIRON_GET: 0x0003,
  ENVIRON_SIZES_GET: 0x0004,
  FD_CLOSE: 0x0022,
  FD_FDSTAT_GET: 0x0024,
  FD_PRESTAT_GET: 0x002b,
  FD_READ: 0x002e,
  FD_WRITE: 0x0034,
  PATH_OPEN: 0x0044,
  PROC_EXIT: 0x0060,
  CLOCK_TIME_GET: 0x0011,
  RANDOM_GET: 0x0051,
  SCHED_YIELD: 0x0052,
  /** Unused by the WASI shim today; the tests probe it to verify
   * the dispatcher's `ENOSYS` path still fires for opcodes the
   * kernel doesn't yet handle. Swap to whichever WASI opcode is
   * still unhandled as the implementation catches up. */
  FD_SEEK: 0x0031,
} as const;

/** PMos extension opcodes (0x1000..0x1501). */
export const OP_EXT = {
  IPC_SOCKET: 0x1000,
  IPC_BIND: 0x1001,
  IPC_LISTEN: 0x1002,
  IPC_CONNECT: 0x1003,
  IPC_ACCEPT: 0x1004,
  PROC_SPAWN: 0x1100,
  PROC_SELF: 0x1103,
  PROC_PARENT: 0x1104,
  PROC_WAIT: 0x1101,
  DISPLAY_CONNECT: 0x1200,
  DISPLAY_BIND: 0x1201,
  CAP_CHECK: 0x1300,
  CAP_LIST: 0x1301,
} as const;

// ---- Errno constants -------------------------------------------------
//
// Positive values. A response's `status` field carries the *negated*
// form (`-errno`), so a test asserts `response.status === -ERRNO.EBADF`.

export const ERRNO = {
  EBADF: 8,
  EINVAL: 28,
  ENOENT: 44,
  ENOSYS: 52,
} as const;

// ---- Device identifiers ---------------------------------------------
//
// Mirror of `kernel::platform::DevId`. Used in the `pmos_host_driver_call`
// host import to route by device.

export const DEV = {
  FRAMEBUFFER: 0,
  INPUT_KBD: 1,
  INPUT_MOUSE: 2,
  BLOCK: 3,
  NET: 4,
  CONSOLE: 5,
} as const;

// ---- Capability constants -------------------------------------------
//
// Mirror of `abi::cap::Cap`. The u64 bit for a cap is `1 << (cap as u32)`.

export const CAP = {
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
} as const;

/** Bit for a cap, as a bigint (matches `CapSet::0` layout). */
export function capBit(cap: number): bigint {
  return 1n << BigInt(cap);
}

// ---- proc_spawn manifest encoding -----------------------------------
//
// Mirror of the [`abi::ext::SpawnManifest`] Rust type + the args /
// heap layout `crates/kernel/src/syscall/ext.rs`'s `handle_proc_spawn`
// parses. The current wire format is minimal: path string + caps
// bitset, with stdio inherited implicitly from the parent's fd table.
// Richer fields (argv, envp, cwd, extra fd dups) will be appended to
// the heap payload in future slices; this helper will grow
// correspondingly.

/** Arguments to a `PROC_SPAWN` syscall (first-landing shape). */
export interface SpawnManifest {
  /** Absolute path of the binary to spawn. */
  readonly path: string;
  /** Capability bitset the child should hold. Must be a subset of the caller's own caps. */
  readonly caps: bigint;
}

/**
 * Build the `(args, heap)` pair for a [`PROC_SPAWN`] syscall from a
 * typed manifest. `args` goes in `SyscallRequest.args` (or pack
 * path_len into `arg0` if preferred), and `heap` goes at the offset
 * `SyscallRequest.heapPtr` points at.
 */
export function encodeSpawnManifest(
  manifest: SpawnManifest,
): { args: Uint8Array; heap: Uint8Array } {
  const path = new TextEncoder().encode(manifest.path);
  const args = new Uint8Array(16);
  const view = new DataView(args.buffer);
  // args[0..4] = path_len
  view.setUint32(0, path.length, true);
  // args[4..12] = caps bitset (u64 LE)
  view.setBigUint64(4, manifest.caps, true);
  // args[12..16] = reserved (zero)
  return { args, heap: path };
}

/** `abi::cap::CapSet::ALL` — every bit set. */
export const CAPSET_ALL = 0xffff_ffff_ffff_ffffn;

/** `abi::cap::initial::DESKTOP_SHELL` — DisplayClient + Shell + ProcEnumerate + KeymapAdmin. */
export const CAPSET_DESKTOP_SHELL =
  capBit(CAP.DISPLAY_CLIENT) |
  capBit(CAP.SHELL) |
  capBit(CAP.PROC_ENUMERATE) |
  capBit(CAP.KEYMAP_ADMIN);

/** `abi::cap::initial::ORDINARY_APP` — just DisplayClient. */
export const CAPSET_ORDINARY_APP = capBit(CAP.DISPLAY_CLIENT);
