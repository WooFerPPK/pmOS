// `KernelWasmHost` — production wrapper around the kernel.wasm cdylib.
//
// This is the TS-side seam that lets the kernel Worker run the real
// Rust kernel instead of the preview-slice `MockKernel`. It loads the
// compiled `kernel.wasm`, satisfies the five `pmos_host_*` imports the
// `WasmPlatform` pulls in, initialises the kernel, and exposes a
// narrow typed API for:
//
//   * process lifecycle (`registerProcess`, `installConsoleFd`,
//     `markRunning`)
//   * syscall dispatch (`dispatch`), which accepts a typed
//     [`SyscallRequest`] + optional heap input and returns a typed
//     [`SyscallResponse`] + heap output
//   * input injection (`injectInput`), which implements the tight
//     `Kernel` interface the existing driver scaffold already uses
//
// ## Scope today (T091 TS-side first landing)
//
// The class stands alone. It is NOT yet wired into
// `kernel-worker-entry.ts`: the boot path still constructs a
// `MockKernel` so every pre-existing test keeps passing. A subsequent
// slice flips the default (probably via a `useRealKernel` boot-config
// flag that gates which kernel the scaffold constructs), and a slice
// after that replaces the mock with the real kernel outright.
//
// What the class does NOT do yet:
//
//   * Multi-process SAB-based dispatch. The kernel's current exports
//     service one request at a time from a static scratch region
//     inside the kernel's own linear memory; there is no concept of
//     "poll every user process's ring." That lands when the Worker-
//     spawn slice adds a per-pid SAB bridge.
//   * Full driver-host integration. `onConsoleWrite` is the only
//     driver-callback wired today because `FD_WRITE` → `/dev/console`
//     is the only path the 9-opcode dispatcher exercises through
//     `pmos_host_driver_call`. Framebuffer + input hookups land with
//     their respective slices.
//   * Blocking / scheduler hooks. `dispatch` runs to completion
//     synchronously; any syscall that would block returns `EAGAIN`
//     via the dispatcher's `WouldBlock` mapping.
//
// ## Memory management
//
// Every `dispatch` / `injectInput` call re-fetches
// `exports.memory.buffer` immediately before each read or write
// because the kernel's bump allocator can trigger `memory.grow`
// between calls, and a `DataView` / `Uint8Array` constructed against
// a subsequently-detached buffer throws on first access. The pattern
// is: enter a method → do any kernel calls that might grow →
// re-construct the view → read / write → return. Never hold a view
// across a kernel export call.

import type { Driver, DriverHost } from "./drivers/types";
import type { Kernel } from "./kernel-worker";
import { Devnum } from "./shared/platform-constants";
import {
  HEAP_SCRATCH_BYTES,
  OFF_HEAP_SCRATCH,
  OFF_REQ_HEAD,
  OFF_REQ_RING,
  OFF_REQ_TAIL,
  OFF_RES_HEAD,
  OFF_RES_RING,
  OFF_RES_TAIL,
  OFF_USER_WAIT_SLOT,
  REQ_SLOT_COUNT,
  RES_SLOT_COUNT,
  SAB_SIZE,
  STATUS_READY,
} from "./shared/sab-layout";
import type { KernelToMain } from "./shared/worker-proto";
import {
  decodeRequest,
  decodeResponse,
  encodeRequest,
  encodeResponse,
  ERRNO,
  SLOT_SIZE,
  type SyscallRequest,
  type SyscallResponse,
  DEV,
} from "./shared/syscall";

// ---- Export surface --------------------------------------------------

/**
 * Shape of the exports `crates/kernel/src/wasm_entry.rs` produces.
 * Kept in sync manually with the `#[no_mangle] extern "C"` surface in
 * that file; a mismatch blows up at `KernelWasmHost.create` time with
 * a property-access error on the first unused method.
 */
interface KernelExports {
  readonly memory: WebAssembly.Memory;
  readonly kernel_init: () => number;
  readonly kernel_register_process: (caps: bigint) => number;
  readonly kernel_register_process_for_spawn: (
    parent: number,
    name_ptr: number,
    name_len: number,
  ) => number;
  readonly kernel_install_console_fd: (pid: number, fd: number) => number;
  readonly kernel_install_signal_channel_fd: (pid: number, fd: number) => number;
  readonly kernel_mark_running: (pid: number) => number;
  readonly kernel_record_process_memory: (
    pid: number,
    bytesLo: number,
    bytesHi: number,
  ) => number;
  readonly kernel_sync_all: () => number;
  readonly kernel_dispatch: (pid: number) => number;
  readonly kernel_take_next_wake_for_pid: (pid: number) => number;
  readonly kernel_resp_heap_ptr: () => number;
  readonly kernel_inject_console_input: (len: number) => number;
  readonly kernel_inject_input_kbd: (len: number) => number;
  readonly kernel_inject_input_mouse: (len: number) => number;
  readonly kernel_req_ptr: () => number;
  readonly kernel_resp_ptr: () => number;
  readonly kernel_heap_ptr: () => number;
  readonly kernel_heap_len: () => number;
}

/**
 * Outcome of a [`KernelWasmHostOptions.onSpawnProcess`] callback. The
 * TS host returns `{ ok: true }` when it has accepted responsibility
 * for instantiating a Worker for the new pid, or
 * `{ ok: false, errno }` when it cannot — in which case the kernel
 * rolls back the new process-table entry before returning `-EIO` from
 * the `PROC_SPAWN` syscall.
 *
 * `errno` is interpreted as the `DriverError::Errno` value for the
 * Rust-side [`Platform::spawn_process`]. The kernel opcode handler
 * currently translates ANY `spawn_process` failure to `-EIO`, so the
 * exact errno only affects Rust-level diagnostics; a future slice may
 * thread the precise errno through.
 */
export type SpawnOutcome =
  | { readonly ok: true }
  | { readonly ok: false; readonly errno: number };

// ---- Host-import callbacks ------------------------------------------

/**
 * Options passed to [`KernelWasmHost.create`]. Every field is
 * optional; defaults keep the host quiet and use standard browser APIs
 * where possible.
 */
export interface KernelWasmHostOptions {
  /**
   * Called when the kernel writes a line to `/dev/console`. The bytes
   * are a freshly-owned `Uint8Array` — the class copies them out of
   * the kernel's linear memory before handing them over, so the
   * callback can retain the buffer without worrying about detachment
   * on the next `memory.grow`.
   */
  readonly onConsoleWrite?: (bytes: Uint8Array) => void;
  /**
   * Called when the kernel writes bytes to `/dev/fb0` via the
   * framebuffer driver. The bytes arrive as the kernel received
   * them — typically a chunk of raw pixel data in whatever format
   * the caller wrote. The class copies out of the kernel's linear
   * memory before invoking the callback, same as `onConsoleWrite`.
   *
   * The slow-path `driver_call`-based transport this callback
   * serves is explicitly documented in the kernel's
   * `framebuffer_write` as the cold path for low-frequency ioctls
   * (SET_MODE, initial pixel dumps). The hot path for display-
   * server commits is planned to go via shared-memory rings; that
   * wire-up lands with a future slice and won't come through this
   * callback. Routing the cold path through a typed callback
   * today is enough to prove the kernel-to-host framebuffer
   * pipeline works end-to-end.
   */
  readonly onFramebufferWrite?: (bytes: Uint8Array) => void;
  /**
   * Called when the kernel triggers a panic. The default is to
   * rethrow as an `Error`; production code will post a
   * `KernelToMain.panic` message instead so the bootstrap overlay
   * (FR-009a) picks it up.
   */
  readonly onPanic?: (message: string) => void;
  /**
   * Overridable random-byte source. Default: `crypto.getRandomValues`.
   * Tests that need deterministic bytes replace this with a stub.
   */
  readonly randomBytes?: (out: Uint8Array) => void;
  /**
   * Overridable monotonic clock. Default:
   * `BigInt(Math.floor(performance.now() * 1_000_000))`. Tests that
   * want deterministic time replace this with a counter.
   */
  readonly nowNs?: () => bigint;
  /**
   * Overridable wall-clock (`CLOCK_REALTIME`) source. Default:
   * `BigInt(Date.now()) * 1_000_000n` (Date.now is ms since Unix
   * epoch; multiply to ns). Tests that want deterministic wall time
   * — most notably `CLOCK_TIME_GET(REALTIME)` coverage — replace
   * this with a fixed value.
   *
   * Not required to be monotonic; the WASI `CLOCK_REALTIME` contract
   * permits the clock to step. Userland that needs monotonicity
   * calls `CLOCK_TIME_GET(MONOTONIC)` which hits [`nowNs`] instead.
   */
  readonly nowRealtimeNs?: () => bigint;
  /**
   * Called when the kernel asks the host to spawn a user Worker for
   * a newly-created pid. The callback receives the pid the kernel
   * allocated and the UTF-8 binary path the caller passed to
   * `PROC_SPAWN`, and returns a [`SpawnOutcome`] describing whether
   * the host accepted the spawn request.
   *
   * When [`binaryRegistry`](#binaryregistry) AND
   * [`kernelWorkerChannel`](#kernelworkerchannel) are BOTH set and
   * this callback is NOT, a default implementation is installed
   * that looks the path up in the registry, posts
   * `{kind:"proc:spawn", pid, path, wasmBytes}` on the channel for
   * the main-thread spawn router to handle, and returns
   * `{ ok: true }` on hit or `{ ok: false, errno: ENOENT }` on
   * miss. Callers that want fully custom spawn semantics override
   * this.
   *
   * When neither this callback nor the `binaryRegistry +
   * kernelWorkerChannel` pair is set, the host accepts every spawn
   * with `{ ok: true }` but never actually runs anything — the pid
   * exists, it just has no backing process. Useful for tests that
   * exercise only the seam.
   *
   * Returning `{ ok: false, errno }` triggers the kernel's
   * roll-back path: the new pid is reaped before the `PROC_SPAWN`
   * syscall returns `-EIO` to the caller.
   */
  readonly onSpawnProcess?: (pid: number, path: string) => SpawnOutcome;
  /**
   * Map from binary path (e.g. `/usr/bin/hello`) to the wasm bytes
   * for that binary. Used by the default [`onSpawnProcess`]
   * implementation (which requires [`kernelWorkerChannel`] to also
   * be set) to look up a binary when the kernel asks for a spawn.
   * Caller can pass `BufferSource` of any kind (usually
   * `ArrayBuffer` or `Uint8Array`); the host forwards the bytes
   * verbatim through the `proc:spawn` message to the main-thread
   * spawn router.
   *
   * When [`onSpawnProcess`] is explicitly provided, the registry is
   * ignored — the caller is responsible for their own lookup.
   */
  readonly binaryRegistry?: ReadonlyMap<string, BufferSource>;
  /**
   * Channel back to the main thread. When present alongside
   * [`binaryRegistry`] and no explicit [`onSpawnProcess`], the
   * default spawn handler posts `{kind:"proc:spawn", pid, path,
   * wasmBytes}` on this channel and returns `{ ok: true }`
   * synchronously to the kernel.
   *
   * The receiver (the main-thread spawn router from
   * [`bootstrap.ts`]) allocates a fresh SAB, instantiates a user
   * Worker against it, and posts `{kind:"proc:sab", pid, sab}` back
   * to the kernel Worker — which the kernel-worker-entry adds to
   * the dispatch loop's pidMap.
   *
   * Production's `/assets/kernel-worker.js` sets this to a thin
   * wrapper over `self.postMessage`; tests pass a fake that
   * captures every post.
   */
  readonly kernelWorkerChannel?: { postMessage(msg: KernelToMain): void };
  /**
   * Optional [`Driver`] (typically a [`FramebufferDriver`]) the host
   * calls whenever a `driver_call(Framebuffer, ...)` lands. The
   * payload the kernel forwarded is interpreted as a driver-framed
   * message: byte 0 is the device-specific op (e.g.
   * `fb.OP_SET_MODE = 0x01`, `fb.OP_BLIT = 0x02`), the rest is the
   * driver's own payload. The host invokes `driver.init(...)` once
   * at construction (with a minimal [`DriverHost`] that forwards
   * every `postToMain` to [`onFramebufferMessage`]) and then
   * `driver.call(op, payload)` on every framebuffer write.
   *
   * Independent of [`onFramebufferWrite`]: when both are set, the
   * raw-bytes callback fires first and the framed driver call fires
   * after. Callers that only want the decoded `fb:set-mode` /
   * `fb:blit` messages set just `framebufferDriver` +
   * `onFramebufferMessage`; callers that want the untouched bytes
   * (tests, tracing) use `onFramebufferWrite`.
   *
   * User wasm binaries that write raw RGBA (hello-framebuffer,
   * display-server-lite) bypass this path — they only satisfy the
   * `onFramebufferWrite` contract. Binaries that write framed
   * payloads (hello-fb-blit, any future display server) feed this
   * driver.
   */
  readonly framebufferDriver?: Driver;
  /**
   * Called when [`framebufferDriver`]'s `init(host)` handler uses
   * `host.postToMain(msg)` — i.e. when the driver has decoded a
   * `driver_call` payload into a typed main-thread message like
   * `fb:set-mode` or `fb:blit`. No-op when unset.
   */
  readonly onFramebufferMessage?: (msg: unknown) => void;
  /**
   * Optional [`Driver`] (a `BlockDriver`) the host calls whenever
   * a `driver_call(Block, ...)` lands. The call is dispatched in
   * place: the driver's `call(op, payload)` may read AND write
   * the payload buffer (the read path fills payload[8..] with
   * 4096 block bytes), and the result `value` is written into
   * the kernel's `result_ptr`. Errno errors propagate as a
   * negative rc; transport errors as a positive one.
   *
   * Production wiring: `BlockDriver.openInOpfs()` returns a ready
   * driver after the `FileSystemSyncAccessHandle` for `pmos.img`
   * is open. The `kernel_init` Rust path mounts an OPFS at
   * `/persist` only if `driver_call(Block, OP_BLOCK_COUNT)`
   * succeeds, so omitting this field cleanly skips the OPFS
   * mount (handy for tests and for environments where OPFS
   * isn't available).
   */
  readonly blockDriver?: Driver;
}

// ---- Dispatch result -------------------------------------------------

/** Decoded response + heap output from one `dispatch` call. */
export interface DispatchResult {
  /**
   * Decoded syscall response. Undefined when the handler parked the
   * caller (see [`parked`]) — the kernel did not write a response
   * and the caller stays on `Atomics.wait` until a future
   * `drainWakesForPid` pushes the delayed response.
   */
  readonly response?: SyscallResponse;
  /**
   * Bytes the handler wrote to the heap scratch region. Length
   * matches `response.extraLen`. Empty for handlers that produce no
   * heap payload.
   */
  readonly heapOut: Uint8Array;
  /**
   * `true` iff the handler parked the caller (e.g. `IPC_ACCEPT`
   * with flags=0 on an empty backlog). When set, `response` is
   * undefined and the JS dispatch loop MUST NOT push a response
   * onto the caller's SAB.
   */
  readonly parked?: boolean;
}

// ---- KernelWasmHost -------------------------------------------------

export class KernelWasmHost implements Kernel {
  // Note: the class deliberately does NOT retain the caller's
  // `KernelWasmHostOptions` past construction. Every field of that
  // options bag is captured by the host-import closures built in
  // `create()`. The only state the class itself owns is the WASM
  // exports record and the shared 32-byte wake slot every user
  // Worker + the main thread bumps to wake the kernel's dispatch
  // loop.
  private constructor(
    private readonly exports: KernelExports,
    private readonly wakeBuffer: ArrayBufferLike,
  ) {
    this.wakeView = new Int32Array(wakeBuffer, 0, 8);
  }

  /** `Int32Array` view over [`wakeBuffer`]; index 0 is the wake slot. */
  private readonly wakeView: Int32Array;

  /**
   * Load `wasmBytes`, satisfy the host imports, and call
   * `kernel_init`. Returns a ready-to-use host.
   *
   * Throws if instantiation fails, if any import is missing from
   * `wasmBytes`, or if `kernel_init` returns non-zero.
   */
  static async create(
    wasmBytes: BufferSource,
    options: KernelWasmHostOptions = {},
  ): Promise<KernelWasmHost> {
    // `memory` is captured after instantiation and closed over by
    // the host imports. Every import that needs to read from the
    // kernel's linear memory re-reads `memory.buffer` on every call
    // because `memory.grow` detaches the previous buffer.
    let memory: WebAssembly.Memory | undefined;

    // Resolve the `onSpawnProcess` callback. Priority:
    //   1. Explicit `options.onSpawnProcess` wins if provided.
    //   2. Otherwise, if `options.binaryRegistry` AND
    //      `options.kernelWorkerChannel` are BOTH set, install the
    //      proc:spawn-posting callback: look up the bytes, post
    //      `{kind:"proc:spawn", pid, path, wasmBytes}` to main, and
    //      return `{ ok: true }` synchronously. The caller
    //      (kernel-worker-entry) then picks up a `proc:sab` response
    //      from main and adds the pid to its dispatch loop.
    //   3. Otherwise, leave unset (the host import closure treats "no
    //      callback" as accept-and-do-nothing).
    const binaryRegistry = options.binaryRegistry;
    const kernelWorkerChannel = options.kernelWorkerChannel;
    const resolvedOnSpawnProcess:
      | ((pid: number, path: string) => SpawnOutcome)
      | undefined =
      options.onSpawnProcess ??
      (binaryRegistry !== undefined && kernelWorkerChannel !== undefined
        ? (pid: number, path: string): SpawnOutcome => {
            const bytes = binaryRegistry.get(path);
            if (bytes === undefined) {
              return { ok: false, errno: ERRNO.ENOENT };
            }
            // The proc:spawn message must carry a plain
            // `ArrayBufferLike` so the receiver can wrap a
            // `Uint8Array` / pass to `postMessage` without copying.
            const wasmBytes =
              bytes instanceof ArrayBuffer
                ? bytes
                : ArrayBuffer.isView(bytes)
                  ? bytes.buffer.slice(
                      bytes.byteOffset,
                      bytes.byteOffset + bytes.byteLength,
                    )
                  : bytes;
            kernelWorkerChannel.postMessage({
              kind: "proc:spawn",
              pid,
              path,
              wasmBytes,
            });
            return { ok: true };
          }
        : undefined);

    const randomBytes = options.randomBytes ?? ((out: Uint8Array): void => {
      crypto.getRandomValues(out);
    });
    const nowNs = options.nowNs ?? ((): bigint => {
      return BigInt(Math.floor(performance.now() * 1_000_000));
    });
    const nowRealtimeNs = options.nowRealtimeNs ?? ((): bigint => {
      // `Date.now()` is ms since the Unix epoch; multiply by 1e6 to
      // reach ns. The product fits in i64 — the year-2262 overflow
      // is far enough out that we don't guard for it here.
      return BigInt(Date.now()) * 1_000_000n;
    });
    const onPanic = options.onPanic ?? ((message: string): void => {
      throw new Error(`KernelWasmHost panic: ${message}`);
    });

    // Initialise the framebuffer driver (if provided) with a
    // `DriverHost` whose `postToMain` forwards to the caller's
    // `onFramebufferMessage` callback. The driver runs synchronously
    // during `driver_call`, so this single host instance is safe to
    // reuse across calls — there is no concurrent reentry in the
    // current sequential dispatch model. `pushInputToKernel` is a
    // no-op because the framebuffer is write-only in v1.
    const framebufferDriver = options.framebufferDriver;
    if (framebufferDriver !== undefined) {
      const fbDriverHost: DriverHost = {
        postToMain: (msg: unknown): void => {
          options.onFramebufferMessage?.(msg);
        },
        pushInputToKernel: (): void => {},
      };
      framebufferDriver.init(fbDriverHost);
    }

    // Initialise the block driver (T084). Like the framebuffer
    // path, the block driver runs synchronously inside
    // `driver_call`, so we hand it a one-shot DriverHost whose
    // `postToMain` is a no-op (the block driver doesn't post
    // events) and `pushInputToKernel` is a no-op (it's
    // request/response, not event-driven).
    const blockDriver = options.blockDriver;
    if (blockDriver !== undefined) {
      const blockDriverHost: DriverHost = {
        postToMain: (): void => {},
        pushInputToKernel: (): void => {},
      };
      blockDriver.init(blockDriverHost);
    }

    const imports: WebAssembly.Imports = {
      env: {
        pmos_host_now_ns: (): bigint => nowNs(),

        pmos_host_now_realtime_ns: (): bigint => nowRealtimeNs(),

        pmos_host_driver_call: (
          dev: number,
          op: number,
          argsPtr: number,
          argsLen: number,
          resultPtr: number,
        ): number => {
          if (memory === undefined) return 0;
          // Copy out of kernel memory immediately — the buffer
          // can detach on a subsequent grow, and even within the
          // same microtask a stored view would become invalid.
          // Each per-device branch owns its own copy.
          if (dev === DEV.CONSOLE && options.onConsoleWrite !== undefined) {
            const src = new Uint8Array(memory.buffer, argsPtr, argsLen);
            options.onConsoleWrite(new Uint8Array(src));
          } else if (dev === DEV.FRAMEBUFFER) {
            // Single copy out of kernel memory; both callback paths
            // below share it.
            const src = new Uint8Array(memory.buffer, argsPtr, argsLen);
            const copy = new Uint8Array(src);
            if (options.onFramebufferWrite !== undefined) {
              options.onFramebufferWrite(copy);
            }
            // Framed-driver path: byte 0 is the driver op, rest is
            // the driver's own payload. Zero-length writes are
            // dropped (a binary that wrote 0 bytes can't be framing
            // anything meaningful; this matches the kernel's own
            // tolerance of empty writes).
            if (framebufferDriver !== undefined && copy.length >= 1) {
              framebufferDriver.call(copy[0]!, copy.subarray(1));
            }
          } else if (dev === DEV.BLOCK) {
            if (blockDriver === undefined) {
              // No block driver attached → signal transport
              // error so the kernel-side `WasmBlockDevice::open`
              // sees `Err(NotReady)` and `kernel_init` cleanly
              // skips the OPFS mount.
              return 1;
            }
            // Block driver: read/write/flush. The driver may
            // write back into the args buffer (OP_READ fills
            // bytes 8..4104 with the read block), so we hand it
            // a *direct* view into kernel memory rather than a
            // copy. Result `value` goes into `resultPtr` as a
            // little-endian u32 so the kernel's
            // `WasmPlatform::driver_call` reads it back.
            const view = new Uint8Array(memory.buffer, argsPtr, argsLen);
            const result = blockDriver.call(op, view);
            if (result.ok) {
              if (resultPtr !== 0) {
                new DataView(memory.buffer).setUint32(
                  resultPtr,
                  result.value >>> 0,
                  true,
                );
              }
              return 0;
            }
            // Errno → negative rc; transport / not-ready →
            // positive rc per the WasmPlatform mapping.
            if (result.error === 3 /* DriverErrorCode.Errno */) {
              return -(result.errno ?? 5 /* EIO */);
            }
            return 1;
          }
          // Input / net: not wired yet. Return 0 ("success") for
          // all devs so the kernel's side of driver_call doesn't
          // propagate a spurious error back into the dispatch
          // path for devices we just don't route to a callback.
          return 0;
        },

        pmos_host_random_bytes: (ptr: number, len: number): void => {
          if (memory === undefined) return;
          const dest = new Uint8Array(memory.buffer, ptr, len);
          randomBytes(dest);
        },

        pmos_host_halt: (ptr: number, len: number): never => {
          let message = "kernel halted";
          if (memory !== undefined && len > 0) {
            const bytes = new Uint8Array(memory.buffer, ptr, len);
            message = new TextDecoder().decode(bytes);
          }
          onPanic(message);
          // Unreachable once onPanic rethrows, but the Rust signature
          // is `-> !` so we must throw here too.
          throw new Error(`kernel halted: ${message}`);
        },

        pmos_host_panic: (ptr: number, len: number): void => {
          if (memory === undefined) return;
          const bytes = new Uint8Array(memory.buffer, ptr, len);
          const message = new TextDecoder().decode(bytes);
          onPanic(message);
        },

        pmos_host_spawn_process: (
          pid: number,
          pathPtr: number,
          pathLen: number,
        ): number => {
          if (memory === undefined) return 0;
          const pathBytes = new Uint8Array(memory.buffer, pathPtr, pathLen);
          const path = new TextDecoder().decode(pathBytes);
          if (resolvedOnSpawnProcess === undefined) return 0;
          const outcome = resolvedOnSpawnProcess(pid, path);
          // Rust-side contract: 0 = success, <0 = -errno, >0 = transport error.
          if (outcome.ok) return 0;
          // The kernel-side `WasmPlatform::spawn_process` expects a
          // negative return value for DriverError::Errno; positive
          // values signal transport failures. Testers that want the
          // rollback path always hit DriverError::Errno — pass a
          // positive errno and it gets negated here.
          return -outcome.errno;
        },
      },
    };

    const { instance } = await WebAssembly.instantiate(wasmBytes, imports);
    const exports = instance.exports as unknown as KernelExports;
    memory = exports.memory;

    const rc = exports.kernel_init();
    if (rc !== 0) {
      throw new Error(`KernelWasmHost: kernel_init returned ${rc}`);
    }

    // Allocate the shared kernel wake slot. Production Worker scope
    // has `SharedArrayBuffer` available (COOP/COEP); vitest under node
    // without cross-origin-isolation falls back to a plain
    // `ArrayBuffer`, which `Atomics.load`/`store` both accept (but
    // `Atomics.wait`/`waitAsync` do not — the default park path
    // detects this and falls back to a microtask yield).
    let wakeBuffer: ArrayBufferLike;
    try {
      wakeBuffer = new SharedArrayBuffer(32);
    } catch {
      wakeBuffer = new ArrayBuffer(32);
    }

    return new KernelWasmHost(exports, wakeBuffer);
  }

  // ---- process lifecycle --------------------------------------------

  /**
   * Register a process with the given cap bitset. Returns the newly
   * allocated pid. Throws if the kernel rejects the registration (the
   * current implementation always succeeds, so the throw path is
   * defensive).
   */
  registerProcess(caps: bigint): number {
    const pid = this.exports.kernel_register_process(caps);
    if (pid < 0) {
      throw new Error(`KernelWasmHost.registerProcess: kernel_register_process returned ${pid}`);
    }
    return pid;
  }

  /**
   * Install `/dev/console` at `fd` in `pid`'s fd table. Convenience
   * wrapper over the kernel export of the same name.
   */
  installConsoleFd(pid: number, fd: number): void {
    const rc = this.exports.kernel_install_console_fd(pid, fd);
    if (rc !== 0) {
      throw new Error(`KernelWasmHost.installConsoleFd(${pid}, ${fd}): rc=${rc}`);
    }
  }

  /**
   * Install `FdObject::SignalChannel` at `fd` in `pid`'s fd table.
   * Companion to {@link installConsoleFd}; gives host-side tests
   * a way to stage the per-process signal channel on a pid
   * that was created via `registerProcess` (which deliberately
   * does not auto-install, unlike proc_spawn'd children which
   * get fd 3 = SignalChannel for free).
   */
  installSignalChannelFd(pid: number, fd: number): void {
    const rc = this.exports.kernel_install_signal_channel_fd(pid, fd);
    if (rc !== 0) {
      throw new Error(
        `KernelWasmHost.installSignalChannelFd(${pid}, ${fd}): rc=${rc}`,
      );
    }
  }

  /**
   * Transition a newly-registered process from `Starting` through
   * `Ready` to `Running`. Required before the process can issue any
   * syscall that needs the caller to be in `Running` state (most
   * notably `PROC_EXIT`).
   */
  markRunning(pid: number): void {
    const rc = this.exports.kernel_mark_running(pid);
    if (rc !== 0) {
      throw new Error(`KernelWasmHost.markRunning(${pid}): rc=${rc}`);
    }
  }

  recordProcessMemory(pid: number, bytes: number): void {
    if (!Number.isFinite(bytes) || bytes < 0 || !Number.isSafeInteger(bytes)) {
      throw new Error(`KernelWasmHost.recordProcessMemory: invalid byte count ${bytes}`);
    }
    const bytesLo = bytes >>> 0;
    const bytesHi = Math.floor(bytes / 0x1_0000_0000) >>> 0;
    const rc = this.exports.kernel_record_process_memory(pid, bytesLo, bytesHi);
    if (rc !== 0) {
      throw new Error(
        `KernelWasmHost.recordProcessMemory(${pid}, ${bytes}): rc=${rc}`,
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
  syncAll(): boolean {
    return this.exports.kernel_sync_all() === 0;
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
  spawnChildForTest(parent: number, name: string): number {
    const nameLen = this.writeNameToHeapScratch(name);
    const rc = this.exports.kernel_register_process_for_spawn(
      parent,
      0,
      nameLen,
    );
    if (rc < 0) {
      throw new Error(
        `KernelWasmHost.spawnChildForTest: kernel_register_process_for_spawn returned ${rc}`,
      );
    }
    return rc;
  }

  // The kernel export reads the name from `HEAP_SCRATCH[0..name_len]`;
  // the ptr argument is unused but preserved for export-signature
  // stability. Returns UTF-8 byte length (not UTF-16 code-unit count)
  // so the kernel sees the exact byte window the JS side wrote.
  private writeNameToHeapScratch(s: string): number {
    const bytes = new TextEncoder().encode(s);
    const heapCap = this.exports.kernel_heap_len();
    if (bytes.length > heapCap) {
      throw new Error(
        `KernelWasmHost.writeNameToHeapScratch: ${bytes.length} > heap capacity ${heapCap}`,
      );
    }
    const heapPtr = this.exports.kernel_heap_ptr();
    new Uint8Array(this.exports.memory.buffer, heapPtr, bytes.length).set(bytes);
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
  dispatch(pid: number, request: SyscallRequest, heapIn?: Uint8Array): DispatchResult {
    const reqBytes = encodeRequest(request);

    // Stage the request + heap input into the kernel's scratch
    // regions. Each access uses a fresh buffer view.
    {
      const buf = this.exports.memory.buffer;
      const reqPtr = this.exports.kernel_req_ptr();
      new Uint8Array(buf, reqPtr, SLOT_SIZE).set(reqBytes);
      if (heapIn !== undefined && heapIn.length > 0) {
        const heapPtr = this.exports.kernel_heap_ptr();
        const heapCap = this.exports.kernel_heap_len();
        const offset = request.heapPtr ?? 0;
        if (offset + heapIn.length > heapCap) {
          throw new Error(
            `KernelWasmHost.dispatch: heap payload ${offset}+${heapIn.length} > capacity ${heapCap}`,
          );
        }
        new Uint8Array(buf, heapPtr + offset, heapIn.length).set(heapIn);
      }
    }

    const rc = this.exports.kernel_dispatch(pid);
    if (rc === 1) {
      // Parked: no response written. Caller stays on Atomics.wait
      // until a future drainWakesForPid pushes the delayed response.
      return { heapOut: new Uint8Array(0), parked: true };
    }
    if (rc !== 0) {
      throw new Error(`KernelWasmHost.dispatch: kernel_dispatch returned ${rc}`);
    }

    // Read the response back. memory.buffer may have changed if the
    // dispatcher allocated; re-fetch.
    const respBuf = this.exports.memory.buffer;
    const respPtr = this.exports.kernel_resp_ptr();
    const respBytes = new Uint8Array(new Uint8Array(respBuf, respPtr, SLOT_SIZE));
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
  takeNextWakeForPid(pid: number): SyscallResponse | null {
    if (this.exports.kernel_take_next_wake_for_pid(pid) !== 1) {
      return null;
    }
    const respPtr = this.exports.kernel_resp_ptr();
    const respBytes = new Uint8Array(
      new Uint8Array(this.exports.memory.buffer, respPtr, SLOT_SIZE),
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
  takeNextWakeForPidWithHeap(pid: number): {
    response: SyscallResponse;
    heapBytes: Uint8Array | null;
  } | null {
    if (this.exports.kernel_take_next_wake_for_pid(pid) !== 1) {
      return null;
    }
    const respPtr = this.exports.kernel_resp_ptr();
    const respBytes = new Uint8Array(
      new Uint8Array(this.exports.memory.buffer, respPtr, SLOT_SIZE),
    );
    const response = decodeResponse(respBytes);
    if (response.extraLen === 0) {
      return { response, heapBytes: null };
    }
    const heapPtrInKernel = this.exports.kernel_heap_ptr();
    const kernelHeap = new Uint8Array(
      this.exports.memory.buffer,
      heapPtrInKernel,
      response.extraLen,
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
  drainWakesForPid(pid: number, sab: Uint8Array): number {
    if (sab.byteLength < SAB_SIZE) {
      throw new Error(
        `KernelWasmHost.drainWakesForPid: sab is ${sab.byteLength} bytes, need ${SAB_SIZE}`,
      );
    }
    const buffer = sab.buffer;
    const baseOffset = sab.byteOffset;
    const header = new Int32Array(buffer, baseOffset, OFF_HEAP_SCRATCH / 4);
    let pushed = 0;
    while (this.exports.kernel_take_next_wake_for_pid(pid) === 1) {
      const respPtr = this.exports.kernel_resp_ptr();
      const respBytes = new Uint8Array(
        new Uint8Array(this.exports.memory.buffer, respPtr, SLOT_SIZE),
      );
      const response = decodeResponse(respBytes);
      const resHead = Atomics.load(header, OFF_RES_HEAD / 4);
      const resTail = Atomics.load(header, OFF_RES_TAIL / 4);
      const nextResHead = ((resHead + 1) >>> 0) % RES_SLOT_COUNT;
      if (nextResHead === resTail) {
        throw new Error(
          `KernelWasmHost.drainWakesForPid: response ring full for pid ${pid}`,
        );
      }
      const resSlotIx = (resHead >>> 0) % RES_SLOT_COUNT;
      const resSlotOffset = baseOffset + OFF_RES_RING + resSlotIx * SLOT_SIZE;
      const encoded = encodeResponse(response);
      new Uint8Array(buffer, resSlotOffset, SLOT_SIZE).set(encoded);

      // Slice 2c.1: if the wake carries heap bytes (extra_len > 0),
      // read them from kernel HEAP_SCRATCH[0..extra_len] and copy
      // to the SAB's heap scratch at baseOffset + OFF_HEAP_SCRATCH
      // + heap_ptr. heap_ptr is surfaced by `kernel_resp_heap_ptr`.
      if (response.extraLen > 0) {
        const heapPtrInSab = this.exports.kernel_resp_heap_ptr();
        const heapPtrInKernel = this.exports.kernel_heap_ptr();
        const kernelHeap = new Uint8Array(
          this.exports.memory.buffer,
          heapPtrInKernel,
          response.extraLen,
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
  serviceSab(pid: number, sab: Uint8Array): 0 | 1 {
    if (sab.byteLength < SAB_SIZE) {
      throw new Error(
        `KernelWasmHost.serviceSab: sab is ${sab.byteLength} bytes, need ${SAB_SIZE}`,
      );
    }
    const buffer = sab.buffer;
    const baseOffset = sab.byteOffset;
    const header = new Int32Array(buffer, baseOffset, OFF_HEAP_SCRATCH / 4);

    // Pop from the request ring. Producer (user) writes HEAD; consumer
    // (kernel) reads TAIL. Empty when head == tail. Slot at tail % N.
    const reqHead = Atomics.load(header, OFF_REQ_HEAD / 4);
    const reqTail = Atomics.load(header, OFF_REQ_TAIL / 4);
    if (reqHead === reqTail) {
      return 1;
    }
    const reqSlotIx = (reqTail >>> 0) % REQ_SLOT_COUNT;
    const reqSlotOffset = baseOffset + OFF_REQ_RING + reqSlotIx * SLOT_SIZE;
    const requestBytes = new Uint8Array(buffer, reqSlotOffset, SLOT_SIZE);
    const decoded = decodeRequest(requestBytes);

    // Read the request's heap payload (if any) out of the SAB's heap
    // scratch at the user-chosen offset. Copy to a fresh owned buffer
    // so subsequent Atomics operations or dispatch-side memcpys don't
    // see tearing from a concurrent writer (shouldn't happen in v1,
    // but an owned buffer matches the existing dispatch() input-side
    // pattern and is clearer under review).
    let heapIn: Uint8Array | undefined;
    if (decoded.heapLen > 0) {
      if (
        decoded.heapPtr + decoded.heapLen > HEAP_SCRATCH_BYTES ||
        decoded.heapPtr > HEAP_SCRATCH_BYTES
      ) {
        throw new Error(
          `KernelWasmHost.serviceSab: request heap ${decoded.heapPtr}+${decoded.heapLen} out of bounds (${HEAP_SCRATCH_BYTES})`,
        );
      }
      const heapOffset = baseOffset + OFF_HEAP_SCRATCH + decoded.heapPtr;
      heapIn = new Uint8Array(
        new Uint8Array(buffer, heapOffset, decoded.heapLen),
      );
    }

    // Dispatch. Remap `heapPtr` to 0 because the kernel's scratch
    // region is not the SAB's — the user's SAB-side offset is not
    // meaningful to the kernel, which always writes at offset 0 in
    // its own scratch.
    const dispatchResult = this.dispatch(
      pid,
      {
        opcode: decoded.opcode,
        flags: decoded.flags,
        requestId: decoded.requestId,
        args: decoded.args,
        heapPtr: 0,
        heapLen: decoded.heapLen,
      },
      heapIn,
    );

    // Advance the request ring's tail — we've taken ownership of
    // the request regardless of whether the handler parked.
    const nextTail = ((reqTail + 1) >>> 0) % REQ_SLOT_COUNT;
    Atomics.store(header, OFF_REQ_TAIL / 4, nextTail);

    if (dispatchResult.parked === true) {
      // Parked: no response push. Caller stays on Atomics.wait
      // until a future drainWakesForPid pushes the delayed response.
      return 0;
    }

    const response = dispatchResult.response!;
    const heapOut = dispatchResult.heapOut;

    // Push the response. Producer (kernel) writes HEAD; consumer (user)
    // reads TAIL. Full when (head + 1) % N == tail.
    const resHead = Atomics.load(header, OFF_RES_HEAD / 4);
    const resTail = Atomics.load(header, OFF_RES_TAIL / 4);
    const nextResHead = ((resHead + 1) >>> 0) % RES_SLOT_COUNT;
    if (nextResHead === resTail) {
      throw new Error(
        `KernelWasmHost.serviceSab: response ring full for pid ${pid}`,
      );
    }
    const resSlotIx = (resHead >>> 0) % RES_SLOT_COUNT;
    const resSlotOffset = baseOffset + OFF_RES_RING + resSlotIx * SLOT_SIZE;
    const resBytes = encodeResponse(response);
    new Uint8Array(buffer, resSlotOffset, SLOT_SIZE).set(resBytes);

    // Copy heap output (if any) back to the SAB's heap scratch at
    // the user-chosen offset so the user reads it from the same
    // place it wrote its input.
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
  injectInput(devnum: number, bytes: Uint8Array): void {
    let injectFn: ((len: number) => number) | undefined;
    let fnName: string;
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
        `KernelWasmHost.injectInput: devnum ${devnum} not supported; wired device nodes are Devnum.Console (${Devnum.Console}), Devnum.InputKbd (${Devnum.InputKbd}), Devnum.InputMouse (${Devnum.InputMouse})`,
      );
    }

    const heapCap = this.exports.kernel_heap_len();
    if (bytes.length > heapCap) {
      throw new Error(
        `KernelWasmHost.injectInput: ${bytes.length} bytes > heap capacity ${heapCap}`,
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
  }

  // ---- dispatch loop -------------------------------------------------

  /**
   * Shared kernel wake slot. 32 bytes backed by a `SharedArrayBuffer`
   * when the environment allows; a plain `ArrayBuffer` otherwise
   * (vitest under node). Every user Worker's `SabBackend` and the
   * main thread's `injectInput` routing bumps `index 0` via
   * `Atomics.add` + `Atomics.notify` so the kernel's dispatch loop
   * wakes from its `Atomics.waitAsync` park.
   *
   * The slot is semantically "wake counter": notifiers increment it,
   * the parker reads it before waiting, and a spurious-wake-free
   * park returns as soon as the counter changes. Production code
   * should NEVER mutate the counter directly — use `Atomics.add` +
   * `Atomics.notify` via a helper when that helper lands in T234.
   */
  get wakeSlot(): Int32Array {
    return this.wakeView;
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
   * `Atomics.waitAsync` on the shared wake slot with a 50 ms timeout.
   * Under vitest (no cross-origin-isolated context), tests pass a
   * microtask-yield stub so the loop never actually blocks — the
   * test seeds the rings synchronously anyway.
   *
   * The loop is purely cooperative: a user Worker that never calls
   * a syscall ties up only its own Worker thread. That matches
   * `multi-process-plan.md §1` "Non-goals: pre-emption".
   */
  async startDispatchLoop(args: StartDispatchLoopArgs): Promise<void> {
    const budget = args.budget ?? 8;
    const parkFn = args.parkFn ?? ((): Promise<void> => this.defaultPark());
    const haveSharedArrayBuffer = typeof SharedArrayBuffer !== "undefined";
    while (!args.halted()) {
      let anyServiced = false;
      const pids = args.pidSource();
      for (const [pid, sab] of pids) {
        const view = new Uint8Array(sab);
        // Shared header view for the user-wake protocol step (T234).
        // Constructed once per pid per pass; cheap, but pulled out of
        // the inner loop to avoid an extra allocation per syscall.
        const header = new Int32Array(sab, 0, OFF_HEAP_SCRATCH / 4);
        const sabIsShared =
          haveSharedArrayBuffer && sab instanceof SharedArrayBuffer;
        for (let i = 0; i < budget; i++) {
          // T095/T110 (slice 2c.2): a delayed wake (parked
          // proc_wait's reap, parked ipc_accept's connect wake,
          // EINTR interrupt) may land via drainWakesForPid even
          // when the request ring is empty. The RES_HEAD delta
          // across drainWakesForPid + serviceSab is the
          // authoritative "did anything reach the user's response
          // ring?" signal, since serviceSab returns 0 for BOTH a
          // normal-service response push AND a park (no push).
          const resHeadBefore = Atomics.load(header, OFF_RES_HEAD / 4);
          const wakesPushed = this.drainWakesForPid(pid, view);
          if (wakesPushed > 0) {
            // The kernel's wake paths (`wake_parked_waiter_for_child`,
            // `interrupt_parked_*`, `ipc_connect`) transition the
            // woken pid from Blocked* back to Ready — but the pid's
            // next syscall, which for a loop like init's proc_wait
            // supervisor is often another blocking call, needs the
            // pid to be Running so park_on_wait's Running→Blocked*
            // transition succeeds. Bump it here. Errors swallowed —
            // already-Running or reaped are both safe no-ops.
            try {
              this.markRunning(pid);
            } catch {
              // pass
            }
          }
          const rc = this.serviceSab(pid, view);
          const resHeadAfter = Atomics.load(header, OFF_RES_HEAD / 4);
          const responsePushed = resHeadAfter !== resHeadBefore;
          if (responsePushed) {
            // T234 production wake protocol step 5: now that a
            // response (service push or drained wake) has landed
            // in the ring, wake the user Worker's
            // `Atomics.wait(header, OFF_USER_WAIT_SLOT/4,
            // STATUS_REQUESTED)`. Storing any value other than
            // `STATUS_REQUESTED` makes a pre-existing waiter
            // return "ok" on the subsequent notify, AND makes a
            // not-yet-parked waiter return "not-equal" immediately
            // when it does call `wait`. Both paths land in the
            // same place: the user pops the response.
            // `Atomics.notify` only accepts `SharedArrayBuffer`-
            // backed views, so guard it; the `Atomics.store`
            // itself is legal on either backing and is harmless
            // when the user Worker is the legacy synchronous
            // `serviceHook` path.
            Atomics.store(header, OFF_USER_WAIT_SLOT / 4, STATUS_READY);
            if (sabIsShared) {
              Atomics.notify(header, OFF_USER_WAIT_SLOT / 4);
            }
            anyServiced = true;
          }
          if (rc === 1) {
            // Ring empty (or handler parked without push). Any
            // wakes drained above have been notified. Stop
            // spinning so the next pass can visit other pids.
            break;
          }
        }
      }
      // Check again after the pass so a caller whose halt condition
      // becomes true during servicing (e.g. init's `proc:exited`
      // arrived mid-pass and emptied the pidMap) exits without a
      // wasted park.
      if (args.halted()) return;
      if (!anyServiced) {
        await parkFn();
      }
    }
  }

  /**
   * Default [`startDispatchLoop`] park. `Atomics.waitAsync` on the
   * shared wake slot with a 50 ms timeout when the wake buffer is a
   * real `SharedArrayBuffer` and `Atomics.waitAsync` is available;
   * otherwise a microtask yield (`setTimeout(0)`) so the event loop
   * still gets a chance to drain messages between busy-spin passes.
   *
   * The 50 ms timeout exists as a belt-and-suspenders against lost
   * notifications; `Atomics.notify` never drops under contention, but
   * a caller that writes the wake slot before the kernel parks would
   * be a lost wake if the kernel waited forever. The plan §10 notes
   * 50 ms keeps well below the Principle IX 100 ms input budget.
   */
  private async defaultPark(): Promise<void> {
    if (
      typeof SharedArrayBuffer !== "undefined" &&
      this.wakeBuffer instanceof SharedArrayBuffer &&
      typeof Atomics.waitAsync === "function"
    ) {
      const last = Atomics.load(this.wakeView, 0);
      const r = Atomics.waitAsync(this.wakeView, 0, last, 50);
      if (r.async) {
        await r.value;
      }
      return;
    }
    await new Promise<void>((resolve) => {
      setTimeout(resolve, 0);
    });
  }
}

/** Arguments to [`KernelWasmHost.startDispatchLoop`]. */
export interface StartDispatchLoopArgs {
  /**
   * Snapshot of currently-live pids and their per-pid SAB views. The
   * loop re-reads this on every pass, so the caller's map mutations
   * (from `proc:sab` / `proc:exited`) are picked up without
   * restarting the loop.
   */
  readonly pidSource: () => ReadonlyMap<number, ArrayBufferLike>;
  /**
   * Caller's halt condition. Checked at the start AND end of every
   * pass; returning `true` exits the loop. Production boot path
   * typically passes `() => pidsHaveSpawned && pidMap.size === 0`.
   */
  readonly halted: () => boolean;
  /**
   * Invoked when a pass completed without servicing any request.
   * Defaults to a shared-wake-slot `Atomics.waitAsync` with a 50 ms
   * timeout (see [`KernelWasmHost.wakeSlot`]); tests pass a
   * microtask-yield stub so the loop never actually blocks.
   */
  readonly parkFn?: () => Promise<void>;
  /**
   * Maximum requests serviced per pid per pass. Defaults to 8; the
   * value keeps one chatty process from starving the others. Tuned
   * by the perf harness (T220). Tests use a smaller value to keep
   * the round-robin visible.
   */
  readonly budget?: number;
}
