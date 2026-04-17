// Dedicated user-Worker entry bundle.
//
// This is the code that runs inside `new Worker('/assets/user-
// worker.js', {type:'module'})`. In production it auto-installs
// against the Worker's global `self`; in vitest tests it binds to
// a fake messaging interface so the boot → run → exit choreography
// is exercised without a real Worker.
//
// Per-process lifetime: receive exactly one `boot {pid, sab,
// wasmBytes}` message; instantiate the user wasm against a
// `SabBackend` over the SAB; run `_start`; post `{kind:"exited",
// pid, code}` (or `code:-1, trap:<msg>` on a non-`UserProcessExited`
// error); then the Worker exits. All subsequent main-thread
// messages are ignored — this Worker is single-shot.
//
// Wake protocol: T232 (this slice) drives the SAB ring through a
// `serviceHook` test affordance the same way `SabBackend`'s T231
// tests do. Production wake — `Atomics.store(user_wait_slot,
// REQUESTED) → Atomics.notify(kernel_wake_slot) →
// Atomics.wait(user_wait_slot, REQUESTED)` — lands in T233 alongside
// the kernel Worker dispatch loop. No `serviceHook` will be needed
// when that wiring is in place; the option goes away then.
//
// Why this is its own module separate from `user-wasm-runtime.ts`:
// `user-wasm-runtime.ts` is the WASI-shim + backend-dispatch logic
// shared by both the in-process drain (still in production while
// T232–T234 land side-by-side) and the new Worker path. The entry
// here is the Worker-scope adapter that wires a messaging channel
// into a runtime — the same separation `kernel-worker-entry.ts`
// makes between the kernel scaffold and the kernel Worker's
// onmessage handler.

import { SabBackend } from "./sab-backend";
import { UserWasmRuntime } from "./user-wasm-runtime";
import type { MainToUser, UserToMain } from "./shared/worker-proto";

/**
 * The minimum subset of the `DedicatedWorkerGlobalScope` API the
 * entry needs. Implementing this with a plain object lets tests
 * drive the entry without a real Worker.
 */
export interface UserWorkerMessaging {
  /**
   * Install a main-thread → Worker handler. The entry installs
   * exactly one; subsequent assignments replace it (matching
   * `Worker.onmessage` semantics).
   */
  onmessage: ((ev: { data: MainToUser }) => void) | null;
  /** Post a Worker → main-thread message. */
  postMessage(msg: UserToMain): void;
}

/**
 * Optional per-install configuration for [`installUserWorkerEntry`].
 *
 * `serviceHook` is the T232 stand-in for the production wake
 * protocol: it runs synchronously between every SAB request push and
 * response pop, and the test harness uses it to call
 * `KernelWasmHost.serviceSab(pid, sab)` in the same tick. Production
 * passes nothing; T233+ replaces this option with the real
 * `Atomics.wait` round-trip and the slot writes that frame it.
 */
export interface UserWorkerEntryOptions {
  readonly serviceHook?: () => void;
}

/** A handle to the installed entry. */
export interface UserWorkerEntry {
  /**
   * Resolves once the user wasm has run to completion (or trapped)
   * and the `exited` reply has been posted. Tests await this before
   * asserting on `messaging.posted`.
   */
  readonly whenExited: Promise<void>;
}

/**
 * Install the user-worker entry on a messaging object. The caller is
 * responsible for either passing a real `DedicatedWorkerGlobalScope`
 * (via an adapter) or a test fake. The entry installs its own
 * `onmessage` handler; don't install another one.
 */
export function installUserWorkerEntry(
  messaging: UserWorkerMessaging,
  options: UserWorkerEntryOptions = {},
): UserWorkerEntry {
  let resolveExited!: () => void;
  const whenExited = new Promise<void>((resolve) => {
    resolveExited = resolve;
  });

  let bootSeen = false;
  messaging.onmessage = (ev: { data: MainToUser }): void => {
    if (bootSeen) {
      // The user Worker is single-shot. Anything after the boot is
      // an upstream bug; silently ignore so the running wasm is not
      // disturbed. The kernel never sends post-boot traffic to user
      // Workers; user→user traffic goes through kernel-mediated IPC,
      // not main-mediated postMessage.
      return;
    }
    const msg = ev.data;
    if (msg.kind !== "boot") {
      messaging.postMessage({
        kind: "exited",
        pid: -1,
        code: -1,
        trap: `user-worker: ${msg.kind} received before boot`,
      });
      resolveExited();
      return;
    }
    bootSeen = true;
    void runOnce(messaging, msg, options).finally(() => resolveExited());
  };

  return { whenExited };
}

async function runOnce(
  messaging: UserWorkerMessaging,
  boot: Extract<MainToUser, { kind: "boot" }>,
  options: UserWorkerEntryOptions,
): Promise<void> {
  const sabView = new Uint8Array(boot.sab);
  // Production wake-slot view: a shared `Int32Array` over the kernel's
  // 32-byte wake buffer. The kernel allocates this in
  // `KernelWasmHost.create` and main forwards it via the boot message
  // (see `MainToUser.boot.kernelWakeSlot` in `shared/worker-proto.ts`).
  // Tests that drive the entry through a `serviceHook` keep `boot.
  // kernelWakeSlot` undefined; `SabBackend` then takes the legacy
  // synchronous path.
  const kernelWakeSlot =
    boot.kernelWakeSlot !== undefined
      ? new Int32Array(boot.kernelWakeSlot, 0, 8)
      : undefined;
  const backend = new SabBackend({
    sab: sabView,
    pid: boot.pid,
    ...(options.serviceHook ? { serviceHook: options.serviceHook } : {}),
    ...(kernelWakeSlot !== undefined ? { kernelWakeSlot } : {}),
  });
  const runtime = new UserWasmRuntime(boot.wasmBytes, backend);
  try {
    const code = await runtime.run();
    messaging.postMessage({ kind: "exited", pid: boot.pid, code });
  } catch (err) {
    const trap = err instanceof Error ? err.message : String(err);
    messaging.postMessage({ kind: "exited", pid: boot.pid, code: -1, trap });
  }
}

// ---- Worker auto-install --------------------------------------
//
// When this module is loaded inside a real dedicated Web Worker
// (production boot path: `new Worker('/assets/user-worker.js',
// {type:'module'})`), auto-install the entry against the Worker's
// global so it starts listening for the boot message without an
// explicit call. Gated on `DedicatedWorkerGlobalScope` so the
// vitest tests — which import this module in a node environment —
// do NOT auto-install: they call `installUserWorkerEntry` with
// their own `FakeWorkerMessaging` instead.
if (
  typeof DedicatedWorkerGlobalScope !== "undefined" &&
  typeof self !== "undefined" &&
  self instanceof DedicatedWorkerGlobalScope
) {
  installUserWorkerEntry(self as unknown as UserWorkerMessaging);
}
