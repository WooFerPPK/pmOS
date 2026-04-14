// Kernel Worker scaffold.
//
// This module is the entry point for the kernel Worker in
// production (loaded via `new Worker('kernel-worker.js', {type:
// 'module'})`) and for the Vitest harness in development (imported
// directly as a module, with a mock kernel and a captured
// postToMain). It does NOT unconditionally reference `self` so
// that test environments without a real Worker global still work.
//
// Responsibilities:
//
//   1. Own an implementation of the [`Kernel`] interface — the
//      real Rust WASM kernel in production (once T085+ wires it
//      up), or a TS MockKernel for tests.
//   2. Own a small set of registered drivers (ConsoleDriver in v1).
//   3. Route `driver_call` requests from the kernel to the right
//      driver via `KernelWorker.callDriver`.
//   4. Route main-thread messages to the right driver's
//      `onHostMessage` handler.
//   5. Post kernel-originated events (driver output, panics) to
//      the main thread via the injected `postToMain`.

import { ConsoleDriver, CONSOLE_DRIVER_ID } from "./drivers/console";
import { FramebufferDriver } from "./drivers/fb";
import { InputDriver, INPUT_DRIVER_ID } from "./drivers/input";
import { DriverErrorCode } from "./drivers/types";
import type { Driver, DriverHost, DriverResult } from "./drivers/types";
import type { BootConfig, KernelToMain, MainToKernel } from "./shared/worker-proto";

/**
 * Minimum TS-side kernel interface the scaffold needs. The real
 * WASM kernel exports a much bigger surface (via Rust's
 * `Kernel::path_open`, `fd_read`, `fd_write`, `proc_spawn`, …);
 * this TS interface is just the subset the drivers reach into.
 *
 * Tests implement this with an in-memory MockKernel; the real
 * kernel-worker.ts entry point will bridge it to the WASM
 * exports.
 */
export interface Kernel {
  /**
   * Push bytes into the kernel's input buffer for the given
   * device-NODE number (not DriverId). A process reading from
   * the corresponding fd drains them via
   * `DeviceDispatcher::read`. Mirrors
   * `kernel::dev::DeviceDispatcher::inject_console_input` (and
   * the keyboard / mouse equivalents) on the Rust side.
   */
  injectInput(devnum: number, bytes: Uint8Array): void;
}

/** Options for [`bootKernelWorker`]. */
export interface BootOptions {
  readonly kernel: Kernel;
  readonly config: BootConfig;
  /** Function the scaffold calls to post messages to the main thread. */
  readonly postToMain: (msg: KernelToMain) => void;
}

/**
 * A booted kernel Worker scaffold. Returned from
 * [`bootKernelWorker`] so tests and the in-Worker entry point
 * can drive it directly.
 */
export interface KernelWorker {
  /** Dispatch a message that came in from the main thread. */
  handleMainMessage(msg: MainToKernel): void;

  /**
   * Invoke a driver by its devId. Used by the `Platform`
   * abstraction when the Rust kernel calls `driver_call`. In
   * tests the kernel can be driven synthetically through this
   * method.
   */
  callDriver(devId: number, op: number, payload: Uint8Array): DriverResult;

  /** Number of registered drivers. Diagnostic. */
  readonly driverCount: number;
}

export function bootKernelWorker(options: BootOptions): KernelWorker {
  const drivers = new Map<number, Driver>();

  const host: DriverHost = {
    postToMain(msg: unknown): void {
      // Drivers only construct messages from the `KernelToMain`
      // union; the cast here reflects that contract.
      options.postToMain(msg as KernelToMain);
    },
    pushInputToKernel(devnum: number, bytes: Uint8Array): void {
      options.kernel.injectInput(devnum, bytes);
    },
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
    handleMainMessage(msg: MainToKernel): void {
      switch (msg.kind) {
        case "boot": {
          // A second boot while already booted is a caller bug.
          // Report but don't throw — the production kernel
          // Worker logs and continues rather than crashing the
          // whole session.
          options.postToMain({
            kind: "panic",
            message: "kernel-worker: received boot message while already booted",
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

    callDriver(devId: number, op: number, payload: Uint8Array): DriverResult {
      const d = drivers.get(devId);
      if (!d) {
        return { ok: false, error: DriverErrorCode.NotReady };
      }
      return d.call(op, payload);
    },

    get driverCount(): number {
      return drivers.size;
    },
  };
}
