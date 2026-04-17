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
      if (msg.config.useRealKernel === true) {
        void bootRealKernel(messaging, msg.config, options).then(
          ({ scaffold: s, host }) => {
            scaffold = s;
            realKernel = host;
            resolveReady();
          },
        );
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
): Promise<{ scaffold: KernelWorker; host: KernelWasmHost }> {
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
  });
  const scaffold = bootKernelWorker({
    kernel: host,
    config,
    postToMain(out: KernelToMain): void {
      messaging.postMessage(out);
    },
  });
  if (config.bootBinary !== undefined) {
    await runBootBinary(host, config.bootBinary);
  }
  return { scaffold, host };
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
 * "init" pid and drain pending spawns until every transitive child
 * exits. Mirrors the manual choreography that
 * `kernel-wasm-host.test.ts` and the user-wasm-runtime composition
 * tests perform: register a parent process holding `CAPSET_ALL` so
 * the spawn is permitted under the cap-subset rule, install the
 * three console fds the kernel demands of any spawn parent, mark the
 * parent Running, then issue `PROC_SPAWN` and let the
 * `binaryRegistry`-backed default `onSpawnProcess` queue the work.
 *
 * Errors at any stage propagate up: the caller's `whenReady` rejects,
 * which the entry's pre-boot panic branch surfaces if the subsequent
 * main-thread message arrives.
 */
async function runBootBinary(
  host: KernelWasmHost,
  bootBinary: string,
): Promise<void> {
  const initPid = host.registerProcess(CAPSET_ALL);
  host.installConsoleFd(initPid, 0);
  host.installConsoleFd(initPid, 1);
  host.installConsoleFd(initPid, 2);
  host.markRunning(initPid);

  const manifest = encodeSpawnManifest({
    path: bootBinary,
    caps: CAPSET_ALL,
  });
  const { response } = host.dispatch(
    initPid,
    {
      opcode: OP_EXT.PROC_SPAWN,
      requestId: 1,
      args: manifest.args,
      heapPtr: 0,
      heapLen: manifest.heap.length,
    },
    manifest.heap,
  );
  if (response.status !== 0) {
    throw new Error(
      `kernel-worker: PROC_SPAWN(${bootBinary}) failed with status ${response.status}`,
    );
  }
  await host.drainPendingSpawns();
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
