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
import { KernelWasmHost } from "./kernel-wasm-host";
import { MockKernel } from "./mock-kernel";
import {
  CAPSET_ALL,
  encodeSpawnManifest,
  OP_EXT,
} from "./shared/syscall";
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
  /**
   * The [`KernelWasmHost`] backing the scaffold, set only when
   * boot ran the `useRealKernel` path. `undefined` for the
   * MockKernel path and pre-boot. Tests use this to dispatch
   * syscalls directly against the real kernel; production code
   * does not touch it.
   */
  readonly realKernel: KernelWasmHost | undefined;
  /**
   * Resolves once boot has completed (kernel constructed and
   * scaffold bound). For the sync MockKernel path this resolves
   * during the `boot` message handler; for the async KernelWasmHost
   * path it resolves after `KernelWasmHost.create` settles.
   *
   * Callers that need to interact with the entry post-boot from a
   * test context should `await whenReady` first. Production code
   * doesn't need this — it sees `{kind: "ready"}` arrive on the
   * main-thread channel and proceeds from there.
   */
  readonly whenReady: Promise<void>;
}

/**
 * Optional per-install configuration for [`installWorkerEntry`].
 * Production callers (the auto-install branch at the bottom of this
 * module) pass nothing; tests inject the kernel wasm bytes here so
 * the boot path can build a `KernelWasmHost` without a real
 * `fetch('/assets/kernel.wasm')`.
 */
export interface WorkerEntryOptions {
  /**
   * Pre-fetched bytes for `kernel.wasm`. When the boot config has
   * `useRealKernel: true` and this option is set, the entry uses
   * these bytes verbatim. When the option is absent and
   * `useRealKernel: true`, the entry falls back to fetching
   * `/assets/kernel.wasm` from Worker scope (lands in a follow-up
   * slice — for now, missing bytes + `useRealKernel: true` is a
   * panic).
   */
  readonly kernelWasmBytes?: BufferSource;
  /**
   * Map from binary path to wasm bytes. Forwarded into
   * [`KernelWasmHost.create`] so the kernel's default
   * `onSpawnProcess` can look up paths the boot binary (or any
   * descendant) requests via `PROC_SPAWN`. Tests inject this
   * directly; production builds it at Worker scope from the
   * `assets/bin/*.wasm` listed in the deploy manifest.
   */
  readonly binaryRegistry?: ReadonlyMap<string, BufferSource>;
  /**
   * URL fetcher used by the Worker-scope fallback path when
   * [`kernelWasmBytes`] or [`binaryRegistry`] is absent. Defaults
   * to a wrapper around `globalThis.fetch` that returns the
   * response body as `ArrayBuffer`. Tests inject a mock so the
   * fallback path is exercised without hitting the network; the
   * production auto-install branch leaves it unset and the
   * default kicks in.
   */
  readonly fetcher?: (url: string) => Promise<ArrayBuffer>;
}

/**
 * Install the kernel-worker entry-point on a messaging object.
 * The caller is responsible for either passing a real
 * `DedicatedWorkerGlobalScope` (via an adapter) or a test
 * fake. The entry point installs its own `onmessage` handler;
 * don't install another one.
 */
export function installWorkerEntry(
  messaging: WorkerMessaging,
  options: WorkerEntryOptions = {},
): WorkerEntry {
  let scaffold: KernelWorker | undefined;
  let realKernel: KernelWasmHost | undefined;
  let resolveReady!: () => void;
  const whenReady = new Promise<void>((resolve) => {
    resolveReady = resolve;
  });

  // T233 (M1.4): per-pid SAB views the kernel's dispatch loop iterates.
  // `proc:sab` from main adds an entry; `proc:exited` removes one.
  // The dispatch loop re-reads this map on every pass so lifecycle
  // changes are picked up without restarting the loop.
  //
  // `lifecycle.hasEverSpawned` flips to true on the first `proc:sab`
  // — the dispatch loop's halt predicate uses it to distinguish "no
  // pid has landed yet" (wait longer) from "every pid has exited"
  // (halt). Tracking the flip at message-receipt time (rather than
  // inside the halt check) closes a race where a pid arrives and
  // exits between halt probes.
  const pidMap = new Map<number, ArrayBufferLike>();
  const lifecycle = { hasEverSpawned: false };

  messaging.onmessage = (ev: { data: MainToKernel }): void => {
    const msg = ev.data;
    // Multi-process lifecycle messages are handled directly by the
    // entry — they target the dispatch loop's pidMap, not the
    // scaffold. Arrive-out-of-order is fine: the dispatch loop picks
    // up entries on its next pass.
    if (msg.kind === "proc:sab") {
      pidMap.set(msg.pid, msg.sab);
      lifecycle.hasEverSpawned = true;
      // T095/T110: the user Worker owning this SAB is about to run
      // `_start` and issue syscalls. Transition the pid from Ready
      // to Running so blocking syscalls (`proc_wait`, `ipc_accept`
      // with `flags=0`) can take the kernel's Running→Blocked*
      // transitions rather than failing with ESRCH. Guarded on
      // `realKernel` because proc:sab may arrive before the boot
      // binary finishes instantiating under tight races — the
      // kernel-worker scaffold is built and real kernel held here.
      // Errors are swallowed (the kernel asserts on inconsistent
      // states; any error here is a harness race that a future
      // proc:sab duplicate will also hit).
      if (realKernel !== undefined) {
        try {
          realKernel.markRunning(msg.pid);
        } catch {
          // Already Running (harness replay), or pid was reaped
          // between PROC_SPAWN + this message — both are safe to
          // ignore. The kernel's transition check is the source of
          // truth.
        }
      }
      return;
    }
    if (msg.kind === "proc:memory") {
      if (realKernel !== undefined) {
        try {
          realKernel.recordProcessMemory(msg.pid, msg.bytes);
        } catch {
          // Unknown pid or stale lifecycle message; the process table
          // remains the source of truth.
        }
      }
      return;
    }
    if (msg.kind === "sync:request") {
      if (realKernel !== undefined) {
        try {
          realKernel.syncAll();
        } catch {
          // sync_dirty errors are non-fatal: the dirty bit stays set
          // for the next sync attempt. The pagehide handler can't
          // wait for a reply anyway, so swallowing here is correct.
        }
      }
      return;
    }
    if (msg.kind === "proc:exited") {
      if (msg.memoryBytes !== undefined && realKernel !== undefined) {
        try {
          realKernel.recordProcessMemory(msg.pid, msg.memoryBytes);
        } catch {
          // Stale exit for a pid the kernel no longer knows about.
        }
      }
      pidMap.delete(msg.pid);
      return;
    }
    if (scaffold === undefined) {
      // Pre-boot: the only message we accept is a boot.
      if (msg.kind !== "boot") {
        messaging.postMessage({
          kind: "panic",
          message: `kernel-worker: ${msg.kind} received before boot`,
        });
        return;
      }
      if (msg.config.useRealKernel === true) {
        void bootRealKernel(
          messaging,
          msg.config,
          options,
          pidMap,
          lifecycle,
          (s, h) => {
            // Publish the scaffold + host eagerly — before `runBootBinary`
            // enters its long-running dispatch loop — so `input:kbd` and
            // other driver messages posted during the boot binary's
            // execution find the scaffold and route to the driver layer
            // instead of hitting the pre-boot panic branch.
            scaffold = s;
            realKernel = h;
          },
        ).then(() => resolveReady());
        return;
      }
      scaffold = bootMockKernel(messaging, msg.config);
      resolveReady();
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
    get realKernel(): KernelWasmHost | undefined {
      return realKernel;
    },
    whenReady,
  };
}

/**
 * The original synchronous boot path: construct a `MockKernel`
 * with the chosen framebuffer mode, bind it into a fresh scaffold,
 * and return the scaffold.
 */
function bootMockKernel(
  messaging: WorkerMessaging,
  config: import("./shared/worker-proto").BootConfig,
): KernelWorker {
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
    config.liveTerminal === true && config.enableFramebuffer;
  const initialScrollback = liveTerminal
    ? (config.terminalBanner ?? []).map(
        (text) => ({ text, kind: "banner" as const }),
      )
    : undefined;
  const mock = new MockKernel({
    policy: { kind: "faux-shell" },
    emitSplashOnFirstInput:
      config.enableFramebuffer && !liveTerminal,
    liveTerminal,
    ...(initialScrollback ? { initialScrollback } : {}),
    panicEmit: (message: string) => {
      messaging.postMessage({ kind: "panic", message });
    },
  });
  const scaffold = bootKernelWorker({
    kernel: mock,
    config,
    postToMain(out: KernelToMain): void {
      messaging.postMessage(out);
    },
  });
  mock.bindScaffold(scaffold);
  return scaffold;
}

/**
 * The async boot path used when `boot.config.useRealKernel === true`.
 * Loads `kernel.wasm` (from `options.kernelWasmBytes` for tests, or
 * `fetch('/assets/kernel.wasm')` in production), constructs a
 * [`KernelWasmHost`], and binds it into a fresh scaffold.
 *
 * Posts a panic on the messaging channel and rethrows when the wasm
 * is unavailable or instantiation fails. The caller awaits the
 * returned promise; on rejection there is nothing left to do — the
 * scaffold stays unset and any subsequent main-thread message lands
 * back in the pre-boot panic branch.
 */
async function bootRealKernel(
  messaging: WorkerMessaging,
  config: import("./shared/worker-proto").BootConfig,
  options: WorkerEntryOptions,
  pidMap: Map<number, ArrayBufferLike>,
  lifecycle: { hasEverSpawned: boolean },
  onScaffoldReady: (scaffold: KernelWorker, host: KernelWasmHost) => void,
): Promise<void> {
  const fetcher = options.fetcher ?? defaultFetcher;
  let bytes: BufferSource;
  try {
    bytes = options.kernelWasmBytes ?? (await fetcher("/assets/kernel.wasm"));
  } catch (e) {
    const message = `kernel-worker: failed to load /assets/kernel.wasm: ${String(e)}`;
    messaging.postMessage({ kind: "panic", message });
    throw e;
  }
  let registry = options.binaryRegistry;
  if (registry === undefined && config.bootBinary !== undefined) {
    try {
      registry = await fetchBinaryRegistry(fetcher);
    } catch (e) {
      const message = `kernel-worker: failed to populate binary registry: ${String(e)}`;
      messaging.postMessage({ kind: "panic", message });
      throw e;
    }
  }
  const host = await KernelWasmHost.create(bytes, {
    // Bytes the kernel flushes from `/dev/console` ride the existing
    // ConsoleHost main-thread channel as `console:write` messages,
    // so the boot screen + live terminal don't need to know whether
    // the source was MockKernel or KernelWasmHost.
    onConsoleWrite: (bytes: Uint8Array) => {
      messaging.postMessage({ kind: "console:write", bytes });
    },
    onPanic: (message: string) => {
      messaging.postMessage({ kind: "panic", message });
    },
    ...(registry !== undefined ? { binaryRegistry: registry } : {}),
    kernelWorkerChannel: {
      postMessage: (msg: KernelToMain): void => {
        messaging.postMessage(msg);
      },
    },
  });
  const scaffold = bootKernelWorker({
    kernel: host,
    config,
    postToMain(out: KernelToMain): void {
      messaging.postMessage(out);
    },
  });
  // Publish the scaffold + host to the caller NOW, before
  // `runBootBinary` enters the long-running dispatch loop. The caller's
  // `onmessage` handler uses these to route driver messages (e.g.
  // `input:kbd` from the keydown listener) through to the scaffold's
  // `handleMainMessage` instead of letting them fall into the pre-boot
  // panic branch.
  onScaffoldReady(scaffold, host);
  // Hand main the kernel's wake slot so every user Worker main spawns
  // from now on can bump it via Atomics.notify to wake the kernel's
  // dispatch loop. Posted exactly once per kernel boot, immediately
  // after the host is constructed and the scaffold is ready, so it
  // lands at main BEFORE the first `proc:spawn` the kernel will emit
  // during `runBootBinary`'s PROC_SPAWN dispatch.
  messaging.postMessage({
    kind: "kernel:wake-slot",
    sab: host.wakeSlot.buffer,
  });
  if (config.bootBinary !== undefined) {
    await runBootBinary(host, config.bootBinary, pidMap, lifecycle);
  }
}

/**
 * Default URL fetcher: wraps `globalThis.fetch` and returns the
 * response body as `ArrayBuffer`. Throws when the response is not
 * 2xx so the caller's error path surfaces a useful message.
 */
async function defaultFetcher(url: string): Promise<ArrayBuffer> {
  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`HTTP ${res.status} fetching ${url}`);
  }
  return res.arrayBuffer();
}

/**
 * Fetch `/manifest.json`, walk every `assets/bin/*.wasm` entry, and
 * return a registry mapping `/bin/<stem>` → fetched bytes. Each
 * binary is fetched in parallel; the registry is built only after
 * all finish so a missing binary fails cleanly with the URL in the
 * error.
 *
 * The path key uses the bare basename (with extension stripped) and
 * a `/bin/` prefix — matching the convention the kernel side
 * already uses for spawn paths (e.g. `/bin/hello-std`,
 * `/bin/hello_wasi_min`). The Rust side does not yet care about
 * leading-slash normalization; the convention is kept consistent
 * here so callers can predict the lookup key from the on-disk
 * filename.
 */
async function fetchBinaryRegistry(
  fetcher: (url: string) => Promise<ArrayBuffer>,
): Promise<ReadonlyMap<string, BufferSource>> {
  const manifestBuf = await fetcher("/manifest.json");
  const manifestJson = new TextDecoder().decode(new Uint8Array(manifestBuf));
  const manifest = JSON.parse(manifestJson) as { assets: string[] };
  const binAssets = manifest.assets.filter(
    (a) => a.startsWith("assets/bin/") && a.endsWith(".wasm"),
  );
  const entries = await Promise.all(
    binAssets.map(async (asset): Promise<[string, ArrayBuffer]> => {
      const stem = asset.slice("assets/bin/".length, -".wasm".length);
      const bytes = await fetcher(`/${asset}`);
      return [`/bin/${stem}`, bytes];
    }),
  );
  return new Map(entries);
}

/**
 * Spawn the configured boot binary as a child of a freshly-allocated
 * bootstrap pid, then hand control to the kernel-Worker dispatch
 * loop. Mirrors the manual choreography that
 * `kernel-wasm-host.test.ts` and the user-wasm-runtime composition
 * tests perform: register a parent process holding `CAPSET_ALL` so
 * the spawn is permitted under the cap-subset rule, install the
 * three console fds the kernel demands of any spawn parent, mark
 * the parent Running, then issue `PROC_SPAWN`.
 *
 * After the PROC_SPAWN dispatch returns, the `kernelWorkerChannel`-
 * backed default `onSpawnProcess` has already posted a
 * `{kind:"proc:spawn", pid, path, wasmBytes}` message on the
 * messaging channel. Main's spawn router allocates a SAB, spins up
 * a user Worker, and posts `{kind:"proc:sab", pid, sab}` back to
 * the kernel — the entry's onmessage handler adds the pair to
 * `pidMap` and the dispatch loop picks it up on its next pass.
 *
 * The bootstrap pid stays in-process (never enters `pidMap`) because
 * its only syscall is the one PROC_SPAWN above; it never needs a
 * SAB. Only spawned children (init + descendants) live in the
 * dispatch loop.
 *
 * The loop terminates when `pidMap` is empty AND at least one pid
 * has been registered through it. Production: init exits, main
 * posts `proc:exited`, pidMap becomes empty, `halted()` returns
 * true, the loop returns.
 *
 * Errors at any stage propagate up: the caller's `whenReady`
 * rejects, which the entry's pre-boot panic branch surfaces if the
 * subsequent main-thread message arrives.
 */
async function runBootBinary(
  host: KernelWasmHost,
  bootBinary: string,
  pidMap: Map<number, ArrayBufferLike>,
  lifecycle: { hasEverSpawned: boolean },
): Promise<void> {
  const bootstrapPid = host.registerProcess(CAPSET_ALL);
  host.installConsoleFd(bootstrapPid, 0);
  host.installConsoleFd(bootstrapPid, 1);
  host.installConsoleFd(bootstrapPid, 2);
  host.markRunning(bootstrapPid);

  const manifest = encodeSpawnManifest({
    path: bootBinary,
    caps: CAPSET_ALL,
  });
  const { response } = host.dispatch(
    bootstrapPid,
    {
      opcode: OP_EXT.PROC_SPAWN,
      requestId: 1,
      args: manifest.args,
      heapPtr: 0,
      heapLen: manifest.heap.length,
    },
    manifest.heap,
  );
  // PROC_SPAWN is not a parking opcode; a `dispatch` return without
  // a response here is a programming error, not a legitimate Parked
  // outcome.
  if (response === undefined) {
    throw new Error(
      `kernel-worker: PROC_SPAWN(${bootBinary}) returned no response (parked?)`,
    );
  }
  if (response.status !== 0) {
    throw new Error(
      `kernel-worker: PROC_SPAWN(${bootBinary}) failed with status ${response.status}`,
    );
  }

  // The host's default `onSpawnProcess` already posted `proc:spawn`
  // via the messaging channel. Main allocates a SAB, spawns a user
  // Worker, and posts `proc:sab` back — which the entry's onmessage
  // handler routes into `pidMap` and flips `lifecycle.hasEverSpawned`.
  // The dispatch loop picks the new pid up on its next pass; halts
  // when every landed pid has been reaped.
  //
  // The halt predicate uses `lifecycle.hasEverSpawned` tracked at
  // message-receipt time rather than inside the predicate itself to
  // close a race where a pid arrives and exits between halt probes
  // (the parker's 50 ms timeout is longer than a typical short-lived
  // child's lifetime under vitest).
  await host.startDispatchLoop({
    pidSource: () => pidMap,
    halted: (): boolean =>
      lifecycle.hasEverSpawned && pidMap.size === 0,
  });
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
