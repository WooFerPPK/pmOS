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
   * The default is `{ ok: true }` — tests that don't care about the
   * spawn path get a no-op accept, and production code overrides
   * this to actually look the binary up and instantiate a new
   * Worker. Returning `{ ok: false, errno }` triggers the kernel's
   * roll-back path: the new pid is reaped before the `PROC_SPAWN`
   * syscall returns `-EIO` to the caller.
   */
  readonly onSpawnProcess?: (pid: number, path: string) => SpawnOutcome;
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
  // `create()`, so the class-side state is exactly the WASM exports
  // and nothing else.
  private constructor(private readonly exports: KernelExports) {}

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
          if (dev === DEV.CONSOLE && options.onConsoleWrite !== undefined) {
            // Copy out of kernel memory immediately — the buffer can
            // detach on a subsequent grow, and even within the same
            // microtask a stored view would become invalid.
            const src = new Uint8Array(memory.buffer, argsPtr, argsLen);
            options.onConsoleWrite(new Uint8Array(src));
          }
          // Framebuffer / input / block / net: not wired yet. Return
          // 0 ("success") so the kernel's side of driver_call doesn't
          // propagate a spurious error back into the dispatch path.
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
          const callback = options.onSpawnProcess;
          if (callback === undefined) return 0;
          const outcome = callback(pid, path);
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

    return new KernelWasmHost(exports);
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
