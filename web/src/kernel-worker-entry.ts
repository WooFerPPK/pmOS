// Kernel Worker entry point.
//
// This is the code that runs inside `new Worker('kernel-worker.js',
// {type: 'module'})`. In production it binds to the Worker's
// global `self` via `installWorkerEntry(self)`; in Vitest tests
// it binds to a fake messaging interface so the boot / message
// flow can be exercised without spawning a real Worker.
//
// Responsibilities:
//
//   * Wait for a `boot` message from the main thread.
//   * Construct a `MockKernel` (until T085+ produces the Rust
//     WASM kernel), bind it to a freshly-booted
//     `KernelWorker` scaffold, and forward all subsequent
//     `MainToKernel` messages to the scaffold's
//     `handleMainMessage`.
//   * Post `KernelToMain` messages (driver output, panic, ready)
//     back to the main thread via the messaging interface.
//
// Separating the logic from `self` means two very different
// callers can use it:
//
//   1. `installWorkerEntry(self)` wires it to the real Worker
//      global at production boot time.
//   2. Vitest tests construct a `FakeWorkerMessaging` object,
//      pass it to `installWorkerEntry`, and drive the whole
//      loop deterministically.

import { bootKernelWorker } from "./kernel-worker";
import type { KernelWorker } from "./kernel-worker";
import { MockKernel } from "./mock-kernel";
import type { KernelToMain, MainToKernel } from "./shared/worker-proto";

/**
 * The minimum subset of the `DedicatedWorkerGlobalScope` API the
 * entry point needs. Implementing this with a plain object lets
 * tests drive the entry point without a real Worker.
 */
export interface WorkerMessaging {
  /**
   * Install a main-thread → Worker message handler. The entry
   * point installs exactly one handler; subsequent calls
   * replace it (matching real Worker semantics for
   * `onmessage`).
   */
  onmessage: ((ev: { data: MainToKernel }) => void) | null;
  /** Post a Worker → main-thread message. */
  postMessage(msg: KernelToMain): void;
}

/**
 * A handle to the installed entry. Returned by
 * `installWorkerEntry` for tests to introspect state, and
 * discarded at production boot time.
 */
export interface WorkerEntry {
  /** The scaffold, once boot has happened. `undefined` pre-boot. */
  readonly scaffold: KernelWorker | undefined;
}

/**
 * Install the kernel-worker entry-point on a messaging object.
 * The caller is responsible for either passing a real
 * `DedicatedWorkerGlobalScope` (via an adapter) or a test
 * fake. The entry point installs its own `onmessage` handler;
 * don't install another one.
 */
export function installWorkerEntry(messaging: WorkerMessaging): WorkerEntry {
  let scaffold: KernelWorker | undefined;

  messaging.onmessage = (ev: { data: MainToKernel }): void => {
    const msg = ev.data;
    if (scaffold === undefined) {
      // Pre-boot: the only message we accept is a boot.
      if (msg.kind !== "boot") {
        messaging.postMessage({
          kind: "panic",
          message: `kernel-worker: ${msg.kind} received before boot`,
        });
        return;
      }
      const mock = new MockKernel({
        policy: { kind: "faux-shell" },
        // When the main thread asked for a framebuffer, have
        // the mock emit a splash the first time it sees any
        // console input. The scaffold's fb driver routes it
        // to the main-thread FbHost.
        emitSplashOnFirstInput: msg.config.enableFramebuffer,
      });
      scaffold = bootKernelWorker({
        kernel: mock,
        config: msg.config,
        postToMain(out: KernelToMain): void {
          messaging.postMessage(out);
        },
      });
      mock.bindScaffold(scaffold);
      return;
    }
    // Post-boot: forward to the scaffold. The scaffold itself
    // posts a panic on a stray second boot, so we don't
    // short-circuit that case here.
    scaffold.handleMainMessage(msg);
  };

  return {
    get scaffold(): KernelWorker | undefined {
      return scaffold;
    },
  };
}
