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

import type { Kernel } from "./kernel-worker";
import {
  decodeResponse,
  encodeRequest,
  ERRNO,
  SLOT_SIZE,
  type SyscallRequest,
  type SyscallResponse,
  DEV,
} from "./shared/syscall";
import {
  KernelWasmHostBackend,
  UserWasmRuntime,
} from "./user-wasm-runtime";

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
  readonly kernel_install_console_fd: (pid: number, fd: number) => number;
  readonly kernel_mark_running: (pid: number) => number;
  readonly kernel_dispatch: (pid: number) => number;
  readonly kernel_inject_console_input: (len: number) => number;
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
   * Called when the kernel asks the host to spawn a user Worker for
   * a newly-created pid. The callback receives the pid the kernel
   * allocated and the UTF-8 binary path the caller passed to
   * `PROC_SPAWN`, and returns a [`SpawnOutcome`] describing whether
   * the host accepted the spawn request.
   *
   * When [`binaryRegistry`](#binaryregistry) is set and this
   * callback is NOT, a default implementation is installed that
   * looks the path up in the registry, queues the spawn for
   * [`KernelWasmHost.drainPendingSpawns`] to execute later, and
   * returns `{ ok: true }` on hit or `{ ok: false, errno: ENOENT }`
   * on miss. Callers that want fully custom spawn semantics
   * override this.
   *
   * When neither this callback nor a binary registry is set, the
   * host accepts every spawn with `{ ok: true }` but never
   * actually runs anything — the pid exists, it just has no
   * backing process. Useful for the earlier slices that tested
   * the seam but not the execution.
   *
   * Returning `{ ok: false, errno }` triggers the kernel's
   * roll-back path: the new pid is reaped before the `PROC_SPAWN`
   * syscall returns `-EIO` to the caller.
   */
  readonly onSpawnProcess?: (pid: number, path: string) => SpawnOutcome;
  /**
   * Map from binary path (e.g. `/usr/bin/hello`) to the wasm bytes
   * for that binary. Used by the default [`onSpawnProcess`]
   * implementation to look up a binary when the kernel asks for a
   * spawn. Caller can pass `BufferSource` of any kind (usually
   * `ArrayBuffer` or `Uint8Array`); the host just forwards the
   * bytes to `UserWasmRuntime` which in turn passes them to
   * `WebAssembly.instantiate`.
   *
   * When set, implies the default queuing `onSpawnProcess` unless
   * `onSpawnProcess` is explicitly provided (in which case that
   * callback wins and the registry is ignored — the caller is
   * responsible for their own lookup).
   */
  readonly binaryRegistry?: ReadonlyMap<string, BufferSource>;
}

/** One entry in the [`KernelWasmHost.drainPendingSpawns`] queue. */
interface PendingSpawn {
  readonly pid: number;
  readonly path: string;
  readonly bytes: BufferSource;
}

/** A finished spawn — records what ran and how it exited. */
export interface SpawnHistoryEntry {
  readonly pid: number;
  readonly path: string;
  readonly exitCode: number;
}

// ---- Dispatch result -------------------------------------------------

/** Decoded response + heap output from one `dispatch` call. */
export interface DispatchResult {
  readonly response: SyscallResponse;
  /**
   * Bytes the handler wrote to the heap scratch region. Length
   * matches `response.extraLen`. Empty for handlers that produce no
   * heap payload.
   */
  readonly heapOut: Uint8Array;
}

// ---- KernelWasmHost -------------------------------------------------

export class KernelWasmHost implements Kernel {
  // Note: the class deliberately does NOT retain the caller's
  // `KernelWasmHostOptions` past construction. Every field of that
  // options bag is captured by the host-import closures built in
  // `create()`. The only state the class itself owns is the WASM
  // exports record and — when a `binaryRegistry` is in play — the
  // pending-spawn queue that the default `onSpawnProcess` pushes
  // into. The queue lives on the class (instead of being closed
  // over) because `drainPendingSpawns` needs to pop from the same
  // array the closure pushes to.
  private constructor(
    private readonly exports: KernelExports,
    private readonly pendingSpawns: PendingSpawn[],
  ) {}

  /** Spawn history, appended to by `drainPendingSpawns`. */
  private readonly spawnHistory: SpawnHistoryEntry[] = [];

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

    // Pending-spawn queue: allocated up front so both the default
    // `onSpawnProcess` closure and the class-side `drainPendingSpawns`
    // method share the same array instance. A future cross-thread
    // slice will replace this in-memory queue with a real Worker-
    // registry; the shape (pid + path + bytes) is deliberately
    // small so that migration is mechanical.
    const pendingSpawns: PendingSpawn[] = [];

    // Resolve the `onSpawnProcess` callback. Priority:
    //   1. Explicit `options.onSpawnProcess` wins if provided.
    //   2. Otherwise, if `options.binaryRegistry` is set, install
    //      the default queuing callback that looks the path up in
    //      the registry.
    //   3. Otherwise, leave unset (the host import closure treats
    //      "no callback" as accept-and-do-nothing).
    const binaryRegistry = options.binaryRegistry;
    const resolvedOnSpawnProcess:
      | ((pid: number, path: string) => SpawnOutcome)
      | undefined =
      options.onSpawnProcess ??
      (binaryRegistry !== undefined
        ? (pid: number, path: string): SpawnOutcome => {
            const bytes = binaryRegistry.get(path);
            if (bytes === undefined) {
              return { ok: false, errno: ERRNO.ENOENT };
            }
            pendingSpawns.push({ pid, path, bytes });
            return { ok: true };
          }
        : undefined);

    const randomBytes = options.randomBytes ?? ((out: Uint8Array): void => {
      crypto.getRandomValues(out);
    });
    const nowNs = options.nowNs ?? ((): bigint => {
      return BigInt(Math.floor(performance.now() * 1_000_000));
    });
    const onPanic = options.onPanic ?? ((message: string): void => {
      throw new Error(`KernelWasmHost panic: ${message}`);
    });

    const imports: WebAssembly.Imports = {
      env: {
        pmos_host_now_ns: (): bigint => nowNs(),

        pmos_host_driver_call: (
          dev: number,
          _op: number,
          argsPtr: number,
          argsLen: number,
          _resultPtr: number,
        ): number => {
          if (memory === undefined) return 0;
          // Copy out of kernel memory immediately — the buffer
          // can detach on a subsequent grow, and even within the
          // same microtask a stored view would become invalid.
          // Each per-device branch owns its own copy.
          if (dev === DEV.CONSOLE && options.onConsoleWrite !== undefined) {
            const src = new Uint8Array(memory.buffer, argsPtr, argsLen);
            options.onConsoleWrite(new Uint8Array(src));
          } else if (
            dev === DEV.FRAMEBUFFER &&
            options.onFramebufferWrite !== undefined
          ) {
            const src = new Uint8Array(memory.buffer, argsPtr, argsLen);
            options.onFramebufferWrite(new Uint8Array(src));
          }
          // Input / block / net: not wired yet. Return 0
          // ("success") for all devs so the kernel's side of
          // driver_call doesn't propagate a spurious error back
          // into the dispatch path for devices we just don't
          // route to a callback.
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

    return new KernelWasmHost(exports, pendingSpawns);
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

  // ---- Kernel interface --------------------------------------------

  // ---- pending-spawn drain -----------------------------------------

  /**
   * Run every queued spawn to completion. Pops one pending spawn
   * at a time, builds a [`UserWasmRuntime`] around its bytes +
   * a [`KernelWasmHostBackend`] bound to the spawn's pid, calls
   * `run()`, waits for it to return, then loops. If a running
   * child queues more spawns (by issuing its own `PROC_SPAWN`
   * syscalls), those are picked up on subsequent loop iterations.
   *
   * Sequential by design for the in-process slice: one runtime
   * runs at a time, the kernel's scratch region is never
   * contended, and `drainPendingSpawns` returns only when every
   * transitive child has exited. A future cross-thread slice
   * will replace this with a real multi-Worker scheduler that
   * runs children concurrently in their own dedicated Web
   * Workers; this method's return semantics become "all
   * currently-known children have reached a checkpoint", not
   * "all children have exited".
   *
   * Only meaningful when the host was constructed with a
   * `binaryRegistry` (or a caller-supplied `onSpawnProcess` that
   * also pushes into the queue — unusual but supported if the
   * caller wants a custom lookup path that still uses
   * `drainPendingSpawns` as the execution engine).
   *
   * Exit codes are ignored today. A follow-up slice will wire
   * `proc_wait` into the kernel so the parent can reap children
   * and observe their exit status; at that point this method
   * will need to drive the reap path.
   */
  async drainPendingSpawns(): Promise<void> {
    while (this.pendingSpawns.length > 0) {
      const spawn = this.pendingSpawns.shift()!;
      const backend = new KernelWasmHostBackend(this, spawn.pid);
      const runtime = new UserWasmRuntime(spawn.bytes, backend);
      const exitCode = await runtime.run();
      this.spawnHistory.push({ pid: spawn.pid, path: spawn.path, exitCode });
    }
  }

  /**
   * History of every spawn that `drainPendingSpawns` has run,
   * in drain order. Each entry records the spawn's pid, binary
   * path, and exit code. Lets tests assert on per-child success
   * without having to reach into the runtime directly — the
   * `drainPendingSpawns` method itself doesn't return per-child
   * codes because in the eventual cross-thread design, children
   * run concurrently and don't have a well-ordered "return"
   * point. This history is a test-only affordance that works
   * for the sequential in-process model.
   */
  get spawnHistoryEntries(): readonly SpawnHistoryEntry[] {
    return this.spawnHistory;
  }

  /** True iff at least one spawn is queued and not yet run. */
  get hasPendingSpawns(): boolean {
    return this.pendingSpawns.length > 0;
  }

  /**
   * Push bytes into a kernel device's input ring. Implements the
   * tight `Kernel` interface the existing driver scaffold uses.
   *
   * Today only `DEV.CONSOLE` is supported because the kernel only
   * exports `kernel_inject_console_input`. Keyboard / mouse paths
   * will add their own exports when the input-driver slice lands.
   */
  injectInput(devnum: number, bytes: Uint8Array): void {
    if (devnum !== DEV.CONSOLE) {
      throw new Error(
        `KernelWasmHost.injectInput: devnum ${devnum} not supported; only DEV.CONSOLE (${DEV.CONSOLE}) is wired`,
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

    const rc = this.exports.kernel_inject_console_input(bytes.length);
    if (rc !== 0) {
      throw new Error(`KernelWasmHost.injectInput: kernel_inject_console_input returned ${rc}`);
    }
  }
}
