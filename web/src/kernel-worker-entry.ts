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
      // Decide which framebuffer mode the mock kernel runs:
      //
      //   * `liveTerminal` takes precedence when enabled:
      //     the mock owns scrollback + input buffer state
      //     and re-rasterizes on every keystroke.
      //   * `enableFramebuffer` without `liveTerminal` gives
      //     the one-shot splash path (useful for the boot
      //     screen's proof-of-fb flash).
      //   * Neither → no framebuffer traffic at all.
      //
      // Banner lines arrive as plain strings on the boot
      // config; we map them 1:1 to `banner`-kind scrollback
      // entries so the first rendered frame already has
      // text.
      const liveTerminal =
        msg.config.liveTerminal === true && msg.config.enableFramebuffer;
      const initialScrollback = liveTerminal
        ? (msg.config.terminalBanner ?? []).map(
            (text) => ({ text, kind: "banner" as const }),
          )
        : undefined;
      const mock = new MockKernel({
        policy: { kind: "faux-shell" },
        // When the main thread asked for a framebuffer AND
        // didn't pick live-terminal mode, fall back to the
        // one-shot splash. The scaffold's fb driver routes
        // the blit to the main-thread FbHost.
        emitSplashOnFirstInput:
          msg.config.enableFramebuffer && !liveTerminal,
        liveTerminal,
        ...(initialScrollback ? { initialScrollback } : {}),
        // Wire the kernel panic sink to a postMessage on
        // the main-thread channel. This is what
        // bootstrap.ts's panic overlay listens on.
        panicEmit: (message: string) => {
          messaging.postMessage({ kind: "panic", message });
        },
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

// ---- Worker auto-install --------------------------------------
//
// When this module is loaded inside a real dedicated Web
// Worker (production boot path: `new Worker('/assets/kernel-
// worker.js', {type: 'module'})`), auto-install the entry
// point against the Worker's global so the scaffold starts
// listening for main-thread messages without an explicit
// call. Gated on `DedicatedWorkerGlobalScope` so the Vitest
// tests — which import this module in a node environment —
// do NOT auto-install: they call `installWorkerEntry` with
// their own `FakeWorkerMessaging` instead.
if (
  typeof DedicatedWorkerGlobalScope !== "undefined" &&
  typeof self !== "undefined" &&
  self instanceof DedicatedWorkerGlobalScope
) {
  installWorkerEntry(self as unknown as WorkerMessaging);
}
