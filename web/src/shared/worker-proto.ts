// Message protocol between the main thread and the kernel Worker.
//
// Both sides discriminate on the `kind` field. Every message is
// an interface with a readonly `kind` so TypeScript's narrowing
// through `switch` statements is exhaustive by construction.
//
// This protocol carries HIGH-LEVEL events — boot, shutdown,
// console I/O, driver events. The actual kernel<->user-process
// syscall traffic rides on a SharedArrayBuffer ring buffer
// defined by `../shared/sab-layout.ts`, not by these messages.

import type {
  ConsoleInputMessage,
  ConsoleWriteMessage,
} from "../drivers/console";
import type {
  FbBlitMessage,
  FbPatchBatchMessage,
  FbPatchMessage,
  FbPresentFenceMessage,
  FbSetModeMessage,
} from "../drivers/fb";
import type { InputKbdMessage, InputMouseMessage } from "../drivers/input";

/** Maximum bytes accepted for one browser-to-kernel host-file import. */
export const HOST_FILE_IMPORT_MAX_BYTES = 16 * 1024 * 1024;

/** Kernel-wide host-import limits mirrored for one browser selection batch. */
export const HOST_FILE_IMPORT_MAX_TOTAL_BYTES = 32 * 1024 * 1024;
export const HOST_FILE_IMPORT_MAX_FILES = 64;

/** Boot-time configuration forwarded from the main thread. */
export interface BootConfig {
  /** When true, the console driver is registered at boot. */
  readonly enableConsole: boolean;
  /** When true, the input (kbd + mouse) driver is registered at boot. */
  readonly enableInput: boolean;
  /** When true, the framebuffer driver is registered at boot. */
  readonly enableFramebuffer: boolean;
  /**
   * When true, the mock kernel runs in live-terminal mode:
   * it maintains its own scrollback + input buffer,
   * processes keystrokes one byte at a time, and
   * rasterizes + blits the full terminal snapshot on every
   * state change. Defaults to false; mutually exclusive
   * with the one-shot splash path.
   */
  readonly liveTerminal?: boolean;
  /**
   * Banner lines to pre-seed the live-terminal scrollback
   * with. Each string becomes a `banner`-kind line. Only
   * honoured when [`liveTerminal`] is true. Defaults to an
   * empty list.
   */
  readonly terminalBanner?: readonly string[];
  /**
   * When true, the kernel Worker constructs a real
   * [`KernelWasmHost`] from the compiled `kernel.wasm` cdylib
   * instead of the preview-slice [`MockKernel`]. Defaults to
   * false so every existing caller keeps the mock path.
   *
   * Production callers fetch `/assets/kernel.wasm` at Worker
   * scope; tests inject the bytes via the second argument to
   * `installWorkerEntry` and never touch the network.
   */
  readonly useRealKernel?: boolean;
  /**
   * Optional path of a wasm binary to spawn on boot via
   * `PROC_SPAWN`. Only honoured when [`useRealKernel`] is true
   * and the binary path resolves through the entry's
   * `binaryRegistry`. The boot binary runs to completion before
   * the entry resolves its `whenReady` promise; production code
   * should treat the boot binary as a transient sanity demo until
   * the real init binary supersedes it.
   */
  readonly bootBinary?: string;
}

/** Main-thread → kernel-worker. */
export type MainToKernel =
  | { readonly kind: "boot"; readonly config: BootConfig }
  | { readonly kind: "shutdown" }
  | ConsoleInputMessage
  | InputKbdMessage
  | InputMouseMessage
  | {
      // Main has spawned a user Worker for `pid` and is handing the
      // kernel its SAB ring view. The kernel's dispatch loop adds the
      // pid to its pidMap and starts polling the ring.
      readonly kind: "proc:sab";
      readonly pid: number;
      readonly sab: ArrayBufferLike;
    }
  | {
      // Main observed the user Worker for `pid` exit (clean
      // proc_exit, trap, or script error). The kernel reaps the pid
      // and removes it from its pidMap. `trap` is set for the abnormal
      // paths so the kernel can include it in any Diagnostic surface.
      readonly kind: "proc:exited";
      readonly pid: number;
      readonly code: number;
      readonly trap?: string;
      readonly memoryBytes?: number;
    }
  | {
      // Main observed a user Worker's current wasm linear-memory
      // size. The kernel records this as process VM accounting; the
      // user Worker remains the authority because each process owns a
      // separate wasm memory object.
      readonly kind: "proc:memory";
      readonly pid: number;
      readonly bytes: number;
    }
  | {
      // Best-effort persistence barrier. Main posts this on the
      // `pagehide` lifecycle event so OPFS-backed mutations survive
      // the user closing the tab; the kernel calls `vfs.sync_dirty()`
      // and never replies. Any flush failure is recorded as a panic
      // surface but does not block the page from going away.
      readonly kind: "sync:request";
    }
  | {
      // The user dropped a host file onto the canvas (drag-drop) or
      // picked one through the file-picker (Import menu). Main has
      // assigned `token` and read an owned byte array; the kernel
      // registers a `HostFile` entry under `token` so a userland
      // `host_file_recv(token)` (opcode 0x1500) can later stream the
      // bytes into a read-only fd. Browser structured clone carries
      // the owned bytes as one message; the kernel
      // Worker copies it into the kernel through bounded scratch-sized
      // chunks. Files are capped by HOST_FILE_IMPORT_MAX_BYTES.
      readonly kind: "host:dropped";
      readonly token: number;
      readonly name: string;
      readonly mime: string;
      readonly bytes: Uint8Array;
    };

/** Kernel-worker → main-thread. */
export type KernelToMain =
  | { readonly kind: "ready" }
  | { readonly kind: "panic"; readonly message: string }
  | {
      /**
       * Persistent storage could not be opened or installed as `/`. The
       * kernel may finish constructing its explicitly volatile recovery root,
       * but main MUST block normal user interaction until the user retries or
       * deliberately accepts a temporary session.
       */
      readonly kind: "storage:degraded";
      readonly reason:
        | "opfs-open-failed"
        | "persistent-root-unavailable"
        | "persistent-root-invalid";
      readonly detail: string;
      /** Existing image contents were not reformatted or overwritten. */
      readonly existingImagePreserved: true;
    }
  | ConsoleWriteMessage
  | FbSetModeMessage
  | FbBlitMessage
  | FbPatchMessage
  | FbPatchBatchMessage
  | FbPresentFenceMessage
  | {
      // A capability-authorised user request reached the kernel's
      // host-file bridge. Main may now expose the one-shot browser
      // confirmation whose trusted click opens the native picker; selected
      // files return through `host:dropped` and never bypass the kernel.
      readonly kind: "host:pick";
    }
  | {
      // A capability-authorised host-download fd was closed. The kernel
      // owns this message and has already copied the user process's bytes;
      // main may now turn them into a Blob-backed browser download.
      readonly kind: "host:download";
      readonly name: string;
      readonly mime: string;
      readonly bytes: Uint8Array;
    }
  | {
      // Kernel has allocated `pid` and needs main to instantiate the
      // user Worker. Main allocates the SAB, posts `boot` to the new
      // Worker, then publishes `proc:sab` only after the Worker
      // acknowledges that it accepted the boot message. The kernel pre-fetched
      // `wasmBytes` from its `binaryRegistry` so main doesn't need
      // its own copy of the binary table.
      readonly kind: "proc:spawn";
      readonly pid: number;
      readonly path: string;
      readonly wasmBytes: ArrayBufferLike;
    }
  | {
      // A non-catchable kernel exit (currently SIGKILL) has made
      // `pid` terminal. Main must terminate the corresponding user
      // Worker and remove its routing entry. The subsequent
      // `proc:exited` acknowledgement is idempotent against the
      // kernel's already-terminal process state.
      readonly kind: "proc:terminate";
      readonly pid: number;
      readonly signal: number;
    }
  | {
      // Kernel hands main its 32-byte wake-slot buffer so main can
      // forward it to every user Worker it spawns. Each user Worker's
      // `SabBackend` bumps `index 0` of this slot via `Atomics.add` +
      // `Atomics.notify` to wake the kernel's dispatch loop from its
      // park. Posted exactly once per kernel boot, immediately after
      // `KernelWasmHost.create` and BEFORE the first `proc:spawn` so
      // main never spawns a user Worker without the wake slot in
      // hand. The `sab` is the underlying buffer (typically a
      // `SharedArrayBuffer`; falls back to a plain `ArrayBuffer` in
      // non-cross-origin-isolated environments — `SabBackend`'s
      // production wake path no-ops on the latter, mirroring the
      // kernel's `defaultPark` fallback).
      readonly kind: "kernel:wake-slot";
      readonly sab: ArrayBufferLike;
    };

/**
 * Main-thread → dedicated user Worker. Each user Worker receives
 * exactly one `boot` message in its lifetime; the Worker exits after
 * the wasm `_start` returns or traps and the message channel is then
 * dead.
 */
export type MainToUser = {
  readonly kind: "boot";
  readonly pid: number;
  readonly sab: ArrayBufferLike;
  readonly wasmBytes: ArrayBufferLike;
  /**
   * The kernel's shared 32-byte wake-slot buffer. `SabBackend`
   * constructs an `Int32Array` view over this and bumps index 0
   * via `Atomics.add` + `Atomics.notify` to wake the kernel's
   * dispatch loop from its park (the production wake protocol).
   * Optional so vitest tests that drive the user-worker entry
   * with a `serviceHook` (the T231/T232 stand-in) can omit it;
   * production always sets it.
   */
  readonly kernelWakeSlot?: ArrayBufferLike;
};

/**
 * Dedicated user Worker → main-thread. The Worker may post memory
 * samples as its wasm memory becomes observable, then posts exactly
 * one `exited` message before closing. `trap` is present when the
 * user wasm trapped or the WASI shim threw a non-`UserProcessExited`
 * error; for clean `proc_exit(code)` paths only `code` is set.
 */
export type UserToMain =
  | {
      // The Worker accepted its one boot message and installed the
      // per-process SAB backend. Main may now publish `proc:sab` to
      // the kernel. This acknowledgement prevents a failed Worker
      // construction/boot from leaving an unreachable live pid.
      readonly kind: "booted";
      readonly pid: number;
    }
  | {
      readonly kind: "memory";
      readonly pid: number;
      readonly bytes: number;
    }
  | {
      readonly kind: "exited";
      readonly pid: number;
      readonly code: number;
      readonly trap?: string;
      readonly memoryBytes?: number;
    };
