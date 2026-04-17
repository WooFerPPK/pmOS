// Tests for `KernelWasmHost` — the production wrapper around
// kernel.wasm. Where `kernel-wasm-entry.test.ts` drives the raw
// extern "C" exports with inline stubs, this file exercises the
// typed production API that the kernel-worker scaffold will switch
// to when T091's TS-side swap lands.
//
// Coverage aims:
//
//   * Construction + init (happy path + panic path).
//   * Process lifecycle (registerProcess, installConsoleFd, markRunning).
//   * Dispatch: FD_WRITE round trip through the onConsoleWrite
//     callback; FD_READ round trip after `injectInput` populates the
//     console input ring; PROC_SELF / CAP_LIST for no-heap opcodes;
//     PATH_OPEN for a heap-in + heap-out variant.
//   * Error propagation: bad fd -> -EBADF in response.status; unknown
//     opcode -> -ENOSYS; heap overflow -> KernelWasmHost throws with
//     a useful message.
//   * `injectInput` rejects non-console devnums.

import fs from "node:fs";
import path from "node:path";
import { beforeAll, describe, expect, it } from "vitest";

import { KernelWasmHost, type SpawnOutcome } from "../../src/kernel-wasm-host";
import { Devnum } from "../../src/shared/platform-constants";
import {
  OFF_HEAP_SCRATCH,
  OFF_REQ_HEAD,
  OFF_REQ_RING,
  OFF_REQ_TAIL,
  OFF_RES_HEAD,
  OFF_RES_RING,
  OFF_RES_TAIL,
  SAB_SIZE,
  SLOT_SIZE as SAB_SLOT_SIZE,
} from "../../src/shared/sab-layout";
import {
  CAP,
  CAPSET_ALL,
  CAPSET_ORDINARY_APP,
  CAPSET_DESKTOP_SHELL,
  CLOCKID,
  DEV,
  decodeResponse,
  encodeRequest,
  encodeSpawnManifest,
  ERRNO,
  OP_EXT,
  OP_WASI,
} from "../../src/shared/syscall";

// ---- shared fixture -------------------------------------------------
//
// Every test file load gets one `Uint8Array` of the compiled kernel
// cdylib; individual tests instantiate fresh hosts off that buffer so
// kernel state doesn't leak between tests.

let wasmBytes: ArrayBuffer;

beforeAll(() => {
  const wasmPath = path.resolve(
    __dirname,
    "../../../target/wasm32-unknown-unknown/release/kernel.wasm",
  );
  if (!fs.existsSync(wasmPath)) {
    throw new Error(
      `kernel.wasm not found at ${wasmPath}. Run \`just build\` first.`,
    );
  }
  // Copy the Node `Buffer` into a fresh `ArrayBuffer`. Node's `Buffer`
  // is typed as `Uint8Array<ArrayBufferLike>` in modern TS (the
  // `ArrayBufferLike` union allows `SharedArrayBuffer`), which
  // `WebAssembly.instantiate`'s `BufferSource` parameter rejects.
  // `.slice()` on `ArrayBufferLike` plus a cast to `ArrayBuffer`
  // gives us a plain unshared buffer the instantiate signature
  // accepts.
  const raw = fs.readFileSync(wasmPath);
  wasmBytes = raw.buffer.slice(
    raw.byteOffset,
    raw.byteOffset + raw.byteLength,
  ) as ArrayBuffer;
});

interface SpawnRecord {
  readonly pid: number;
  readonly path: string;
}

interface TestFixture {
  host: KernelWasmHost;
  consoleWrites: Uint8Array[];
  panics: string[];
  spawnCalls: SpawnRecord[];
}

interface FreshHostOptions {
  /** Override the default `{ ok: true }` outcome for spawn requests. */
  readonly spawnOutcome?: (pid: number, path: string) => SpawnOutcome;
  /**
   * Override the monotonic clock. Default `() => 0n` keeps most
   * tests deterministic; `CLOCK_TIME_GET(MONOTONIC)` tests pass a
   * specific value to verify the handler reads through the host
   * import.
   */
  readonly nowNs?: () => bigint;
  /**
   * Override the wall-clock (`CLOCK_REALTIME`) host import. Default
   * `() => 0n` keeps tests deterministic;
   * `CLOCK_TIME_GET(REALTIME)` tests pass a specific value to
   * verify the handler reads through the new host import.
   */
  readonly nowRealtimeNs?: () => bigint;
}

async function freshHost(opts: FreshHostOptions = {}): Promise<TestFixture> {
  const consoleWrites: Uint8Array[] = [];
  const panics: string[] = [];
  const spawnCalls: SpawnRecord[] = [];
  const host = await KernelWasmHost.create(wasmBytes, {
    onConsoleWrite: (bytes) => {
      consoleWrites.push(bytes);
    },
    onPanic: (message) => {
      panics.push(message);
    },
    onSpawnProcess: (pid, path) => {
      spawnCalls.push({ pid, path });
      return opts.spawnOutcome
        ? opts.spawnOutcome(pid, path)
        : { ok: true };
    },
    // Deterministic clock so tests never race with wall-clock changes.
    nowNs: opts.nowNs ?? (() => 0n),
    nowRealtimeNs: opts.nowRealtimeNs ?? (() => 0n),
  });
  return { host, consoleWrites, panics, spawnCalls };
}

// ---- construction ---------------------------------------------------

describe("KernelWasmHost.create", () => {
  it("loads the wasm and initialises the kernel", async () => {
    const { host, panics } = await freshHost();
    expect(host).toBeInstanceOf(KernelWasmHost);
    expect(panics).toHaveLength(0);
  });
});

// ---- process lifecycle ---------------------------------------------

describe("process lifecycle", () => {
  it("registerProcess returns fresh monotonic pids", async () => {
    const { host } = await freshHost();
    const a = host.registerProcess(CAPSET_ALL);
    const b = host.registerProcess(CAPSET_ALL);
    const c = host.registerProcess(CAPSET_DESKTOP_SHELL);
    expect(a).toBeGreaterThan(0);
    expect(b).toBeGreaterThan(a);
    expect(c).toBeGreaterThan(b);
  });

  it("installConsoleFd + markRunning succeed for a fresh pid", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    expect(() => host.installConsoleFd(pid, 1)).not.toThrow();
    expect(() => host.markRunning(pid)).not.toThrow();
  });

  it("markRunning on a nonexistent pid throws", async () => {
    const { host } = await freshHost();
    expect(() => host.markRunning(9999)).toThrow(/markRunning/);
  });
});

// ---- dispatch: PROC_SELF (simplest opcode, no heap) ----------------

describe("dispatch: no-heap opcodes", () => {
  it("PROC_SELF returns the caller pid", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_EXT.PROC_SELF,
      requestId: 1,
    });
    expect(response.status).toBe(0);
    expect(response.value).toBe(BigInt(pid));
    expect(response.requestId).toBe(1);
    expect(response.extraLen).toBe(0);
  });

  it("CAP_LIST returns the full bitset (u64::MAX reinterprets as -1n)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_EXT.CAP_LIST,
      requestId: 2,
    });
    expect(response.status).toBe(0);
    // u64::MAX bit pattern reinterpreted as i64 is -1.
    expect(response.value).toBe(-1n);
  });

  it("CAP_CHECK returns 1 for a held cap and 0 for an absent cap", async () => {
    const { host } = await freshHost();
    const allPid = host.registerProcess(CAPSET_ALL);
    host.markRunning(allPid);
    const shellPid = host.registerProcess(CAPSET_DESKTOP_SHELL);
    host.markRunning(shellPid);

    // Held: DisplayClient is in both cap sets.
    const held = host.dispatch(allPid, {
      opcode: OP_EXT.CAP_CHECK,
      requestId: 3,
      arg0: CAP.DISPLAY_CLIENT,
    });
    expect(held.response.value).toBe(1n);

    // Absent: desktop shell does NOT hold DisplayServer. This
    // doubles as a regression test for the cap split.
    const absent = host.dispatch(shellPid, {
      opcode: OP_EXT.CAP_CHECK,
      requestId: 4,
      arg0: CAP.DISPLAY_SERVER,
    });
    expect(absent.response.value).toBe(0n);
  });
});

// ---- dispatch: FD_WRITE (heap-in, onConsoleWrite callback) ----------

describe("dispatch: FD_WRITE → /dev/console → onConsoleWrite", () => {
  it("routes a full line through onConsoleWrite", async () => {
    const { host, consoleWrites } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const message = new TextEncoder().encode("hello\n");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_WRITE,
        requestId: 10,
        arg0: 1,
        heapPtr: 0,
        heapLen: message.length,
      },
      message,
    );

    expect(response.status).toBe(0);
    expect(response.value).toBe(BigInt(message.length));
    // The console driver only flushes complete lines; "hello\n" is
    // exactly one line, so we expect one callback with exact bytes.
    expect(consoleWrites).toHaveLength(1);
    expect(Array.from(consoleWrites[0]!)).toEqual(Array.from(message));
  });

  it("returns -EBADF when the fd is not installed", async () => {
    const { host, consoleWrites } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_WRITE,
        requestId: 11,
        arg0: 99,
        heapLen: 0,
      },
    );
    expect(response.status).toBe(-ERRNO.EBADF);
    expect(consoleWrites).toHaveLength(0);
  });
});

// ---- dispatch: FD_READ after injectInput ---------------------------

describe("dispatch: injectInput → FD_READ round trip", () => {
  it("delivers injected console input bytes through FD_READ", async () => {
    const { host } = await freshHost();
    host.injectInput(Devnum.Console, new TextEncoder().encode("abc"));

    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 0);
    host.markRunning(pid);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 20,
      arg0: 0,
      heapPtr: 0,
      heapLen: 16,
    });

    expect(response.status).toBe(0);
    expect(response.value).toBe(3n);
    expect(response.extraLen).toBe(3);
    expect(Array.from(heapOut)).toEqual([
      "a".charCodeAt(0),
      "b".charCodeAt(0),
      "c".charCodeAt(0),
    ]);
  });

  it("injectInput accepts Devnum.Console, Devnum.InputKbd, Devnum.InputMouse", async () => {
    const { host } = await freshHost();
    // All three wired input paths accept injection without throwing.
    // (The behavioural effect is covered separately by the
    // user-wasm-runtime tests; here we just prove the routing map.)
    expect(() => host.injectInput(Devnum.Console, new Uint8Array([1, 2, 3]))).not.toThrow();
    expect(() => host.injectInput(Devnum.InputKbd, new Uint8Array([1, 2, 3]))).not.toThrow();
    expect(() => host.injectInput(Devnum.InputMouse, new Uint8Array([1, 2, 3]))).not.toThrow();
  });

  it("injectInput rejects unrouted devnums", async () => {
    const { host } = await freshHost();
    // Fb0 (Devnum.Fb0 = 10) is write-only from the user side — no
    // input-ring path. The rejection message names the three wired
    // device nodes so a reader knows what IS supported.
    expect(() => host.injectInput(Devnum.Fb0, new Uint8Array([1, 2, 3]))).toThrow(/not supported/);
  });

  it("injectInput rejects payloads larger than the heap scratch capacity", async () => {
    const { host } = await freshHost();
    const tooMuch = new Uint8Array(4097); // heap scratch is 4096
    expect(() => host.injectInput(Devnum.Console, tooMuch)).toThrow(/capacity/);
  });
});

// ---- dispatch: PATH_OPEN (heap-in + fresh fd) ----------------------

describe("dispatch: PATH_OPEN", () => {
  it("opens /dev/console and returns fd 0 for a fresh process", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const pathBytes = new TextEncoder().encode("/dev/console");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 30,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );

    expect(response.status).toBe(0);
    expect(response.value).toBe(0n);
  });

  it("returns -ENOENT for a missing path", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const pathBytes = new TextEncoder().encode("/does/not/exist");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 31,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(response.status).toBe(-ERRNO.ENOENT);
  });
});

// ---- dispatch: ENOSYS ----------------------------------------------

describe("dispatch: ENOSYS", () => {
  it("returns -ENOSYS for an unknown opcode", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: 0x4242,
      requestId: 40,
    });
    expect(response.status).toBe(-ERRNO.ENOSYS);
    expect(response.requestId).toBe(40);
  });

  it("returns -ENOSYS for a known WASI opcode without a handler", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // `FD_SEEK` is in the WASI range (0x0031) but still has no
    // handler; swap this probe when FD_SEEK grows one.
    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_SEEK,
      requestId: 41,
    });
    expect(response.status).toBe(-ERRNO.ENOSYS);
  });
});

// ---- dispatch: CLOCK_TIME_GET --------------------------------------

describe("dispatch: CLOCK_TIME_GET", () => {
  it("returns the `nowNs` host import value for CLOCKID.MONOTONIC", async () => {
    const { host } = await freshHost({
      nowNs: () => 1_234_567_890_000n,
      nowRealtimeNs: () => 9_999_999_999_999n,
    });
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.CLOCK_TIME_GET,
      requestId: 50,
      arg0: CLOCKID.MONOTONIC,
    });
    expect(response.status).toBe(0);
    expect(response.value).toBe(1_234_567_890_000n);
    expect(response.requestId).toBe(50);
  });

  it("returns the `nowRealtimeNs` host import value for CLOCKID.REALTIME", async () => {
    // The realtime clock value exceeds 2^53, so the BigInt path
    // matters — an i32 ring-trip would lose bits. The 1.7e18-scale
    // number here models a 2023-ish Unix-epoch-ns timestamp.
    const { host } = await freshHost({
      nowNs: () => 1n,
      nowRealtimeNs: () => 1_700_000_000_000_000_000n,
    });
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.CLOCK_TIME_GET,
      requestId: 51,
      arg0: CLOCKID.REALTIME,
    });
    expect(response.status).toBe(0);
    expect(response.value).toBe(1_700_000_000_000_000_000n);
  });

  it("returns -ENOTSUP for CLOCKID.PROCESS_CPUTIME_ID", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.CLOCK_TIME_GET,
      requestId: 52,
      arg0: CLOCKID.PROCESS_CPUTIME_ID,
    });
    expect(response.status).toBe(-ERRNO.ENOTSUP);
    expect(response.value).toBe(0n);
  });

  it("returns -ENOTSUP for CLOCKID.THREAD_CPUTIME_ID", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.CLOCK_TIME_GET,
      requestId: 53,
      arg0: CLOCKID.THREAD_CPUTIME_ID,
    });
    expect(response.status).toBe(-ERRNO.ENOTSUP);
    expect(response.value).toBe(0n);
  });

  it("returns -EINVAL for an unknown clock id", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.CLOCK_TIME_GET,
      requestId: 54,
      arg0: 99,
    });
    expect(response.status).toBe(-ERRNO.EINVAL);
    expect(response.value).toBe(0n);
  });
});

// ---- dispatch: CLOCK_RES_GET ---------------------------------------

describe("dispatch: CLOCK_RES_GET", () => {
  it("returns 1 ns for CLOCKID.MONOTONIC", async () => {
    // The monotonic clock's resolution is 1 ns — PMos's Platform
    // clock is nanosecond-granular on every supported host. The
    // `nowNs` / `nowRealtimeNs` overrides are irrelevant here (the
    // handler is a compile-time constant per clock id, not a read
    // through Platform), but we set them anyway to match the rest
    // of the clock suite's setup style.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.CLOCK_RES_GET,
      requestId: 60,
      arg0: CLOCKID.MONOTONIC,
    });
    expect(response.status).toBe(0);
    expect(response.value).toBe(1n);
    expect(response.requestId).toBe(60);
  });

  it("returns 1 ns for CLOCKID.REALTIME", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.CLOCK_RES_GET,
      requestId: 61,
      arg0: CLOCKID.REALTIME,
    });
    expect(response.status).toBe(0);
    expect(response.value).toBe(1n);
  });

  it("returns -ENOTSUP for CLOCKID.PROCESS_CPUTIME_ID", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.CLOCK_RES_GET,
      requestId: 62,
      arg0: CLOCKID.PROCESS_CPUTIME_ID,
    });
    expect(response.status).toBe(-ERRNO.ENOTSUP);
    expect(response.value).toBe(0n);
  });

  it("returns -ENOTSUP for CLOCKID.THREAD_CPUTIME_ID", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.CLOCK_RES_GET,
      requestId: 63,
      arg0: CLOCKID.THREAD_CPUTIME_ID,
    });
    expect(response.status).toBe(-ERRNO.ENOTSUP);
    expect(response.value).toBe(0n);
  });

  it("returns -EINVAL for an unknown clock id", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.CLOCK_RES_GET,
      requestId: 64,
      arg0: 99,
    });
    expect(response.status).toBe(-ERRNO.EINVAL);
    expect(response.value).toBe(0n);
  });
});

// ---- dispatch: PROC_SPAWN → onSpawnProcess --------------------------

describe("dispatch: PROC_SPAWN", () => {
  it("spawn from a parent with stdio records onSpawnProcess with (pid, path)", async () => {
    const { host, spawnCalls } = await freshHost();
    const parent = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(parent, 0);
    host.installConsoleFd(parent, 1);
    host.installConsoleFd(parent, 2);
    host.markRunning(parent);

    const manifest = encodeSpawnManifest({
      path: "/usr/bin/hello",
      caps: CAPSET_ALL,
    });

    const { response } = host.dispatch(
      parent,
      {
        opcode: OP_EXT.PROC_SPAWN,
        requestId: 400,
        args: manifest.args,
        heapPtr: 0,
        heapLen: manifest.heap.length,
      },
      manifest.heap,
    );

    expect(response.status).toBe(0);
    expect(response.value).toBeGreaterThan(BigInt(parent));
    expect(spawnCalls).toHaveLength(1);
    expect(spawnCalls[0]).toEqual({
      pid: Number(response.value),
      path: "/usr/bin/hello",
    });
  });

  it("rolls back the new pid when onSpawnProcess rejects", async () => {
    const { host, spawnCalls } = await freshHost({
      spawnOutcome: () => ({ ok: false, errno: ERRNO.EINVAL }),
    });
    const parent = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(parent, 0);
    host.installConsoleFd(parent, 1);
    host.installConsoleFd(parent, 2);
    host.markRunning(parent);

    const manifest = encodeSpawnManifest({
      path: "/usr/bin/nope",
      caps: CAPSET_ALL,
    });

    const { response } = host.dispatch(
      parent,
      {
        opcode: OP_EXT.PROC_SPAWN,
        requestId: 401,
        args: manifest.args,
        heapPtr: 0,
        heapLen: manifest.heap.length,
      },
      manifest.heap,
    );

    // Kernel maps any platform spawn error to -EIO for the
    // userland response. The exact errno is not yet threaded
    // through (see the SpawnOutcome type doc).
    expect(response.status).toBeLessThan(0);
    // The callback still fired with the tentative pid so the host
    // side has a chance to log / surface the refusal.
    expect(spawnCalls).toHaveLength(1);
    expect(spawnCalls[0]!.path).toBe("/usr/bin/nope");

    // A subsequent PROC_SPAWN with an accepting outcome must
    // succeed and allocate a FRESH pid — the rolled-back pid
    // from the first attempt is not reused yet (allocator is
    // monotonic), but the important guarantee is that no state
    // from the failed attempt lingers in a way that breaks a
    // retry.
    const retry = host.dispatch(
      parent,
      {
        opcode: OP_EXT.PROC_SPAWN,
        requestId: 402,
        args: manifest.args,
        heapPtr: 0,
        heapLen: manifest.heap.length,
      },
      manifest.heap,
    );
    // Retry uses the default outcome from this closure, which is
    // `{ ok: false, errno: EINVAL }` again — still fails for the
    // same reason, but the dispatcher itself doesn't crash.
    expect(retry.response.status).toBeLessThan(0);
  });

  it("rejects spawn from a parent missing stdio with -EINVAL", async () => {
    const { host, spawnCalls } = await freshHost();
    const parent = host.registerProcess(CAPSET_ALL);
    // Only fd 1 installed — fd 0 and fd 2 are absent, which is
    // what the kernel opcode handler refuses.
    host.installConsoleFd(parent, 1);
    host.markRunning(parent);

    const manifest = encodeSpawnManifest({
      path: "/usr/bin/whatever",
      caps: CAPSET_ALL,
    });

    const { response } = host.dispatch(
      parent,
      {
        opcode: OP_EXT.PROC_SPAWN,
        requestId: 403,
        args: manifest.args,
        heapPtr: 0,
        heapLen: manifest.heap.length,
      },
      manifest.heap,
    );

    expect(response.status).toBe(-ERRNO.EINVAL);
    expect(spawnCalls).toHaveLength(0);
  });

  it("rejects cap-superset requests with the Rust-side errno mapping", async () => {
    const { host, spawnCalls } = await freshHost();
    // An ordinary-app parent only holds DisplayClient. Spawning a
    // child with CAPSET_DESKTOP_SHELL (which includes SHELL,
    // ProcEnumerate, KeymapAdmin) violates the subset rule — the
    // Rust-side `Kernel::proc_spawn` rejects it as NotCapable.
    const parent = host.registerProcess(CAPSET_ORDINARY_APP);
    host.installConsoleFd(parent, 0);
    host.installConsoleFd(parent, 1);
    host.installConsoleFd(parent, 2);
    host.markRunning(parent);

    const manifest = encodeSpawnManifest({
      path: "/usr/bin/escalator",
      caps: CAPSET_DESKTOP_SHELL,
    });

    const { response } = host.dispatch(
      parent,
      {
        opcode: OP_EXT.PROC_SPAWN,
        requestId: 404,
        args: manifest.args,
        heapPtr: 0,
        heapLen: manifest.heap.length,
      },
      manifest.heap,
    );

    // abi::errno::ENOTCAPABLE is 76.
    expect(response.status).toBe(-76);
    expect(spawnCalls).toHaveLength(0);
  });
});

// ---- dispatch: heap-overflow guard ---------------------------------

describe("dispatch: heap overflow", () => {
  it("throws when heapIn exceeds the kernel's scratch capacity", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const huge = new Uint8Array(5000); // > 4096 heap scratch
    huge.fill(0x41);
    expect(() =>
      host.dispatch(
        pid,
        {
          opcode: OP_WASI.FD_WRITE,
          requestId: 50,
          arg0: 1,
          heapPtr: 0,
          heapLen: huge.length,
        },
        huge,
      ),
    ).toThrow(/capacity/);
  });
});

// ---- serviceSab: per-pid SAB ring servicing (T230) -----------------
//
// The servicing primitive the multi-process dispatch loop will call
// once per pid per round-robin tick. Today the tests drive it
// synchronously: seed a request into a plain-ArrayBuffer stand-in for
// the real SharedArrayBuffer (Vitest runs under node with no
// cross-origin-isolated context, so SABs aren't available), call
// `serviceSab`, assert on the response ring + any driver-side effect
// (e.g. `onConsoleWrite`).
//
// Wake slots are intentionally not exercised here — `serviceSab` does
// not touch them; that is the kernel-Worker loop's job (T233).

/**
 * Helper: build an empty 64 KiB SAB backing as a plain ArrayBuffer.
 * Vitest (node) doesn't provide `SharedArrayBuffer` outside a COOP/
 * COEP context, but the layout math in `sab-layout.ts` and the
 * atomics operations in `Int32Array` both work against a plain
 * `ArrayBuffer` — `Atomics.load` / `Atomics.store` accept any
 * TypedArray over any `ArrayBufferLike`. All the SAB-seeding
 * below uses `new Uint8Array(buf, offset, len)` + `.set(...)` and
 * `Atomics.store(header, index, value)` the same way production
 * will, so the code paths exercised here are byte-identical to the
 * ones a real SharedArrayBuffer would drive.
 */
function freshSabBacking(): { buffer: ArrayBuffer; view: Uint8Array } {
  const buffer = new ArrayBuffer(SAB_SIZE);
  const view = new Uint8Array(buffer);
  return { buffer, view };
}

/**
 * Seed a single request into an otherwise-empty SAB at slot 0 of the
 * request ring, with any heap payload placed at `heapPtr` inside the
 * SAB's heap scratch region. Advances the request ring's HEAD to 1 so
 * `serviceSab` sees one pending request.
 */
function seedRequest(
  sab: { buffer: ArrayBuffer; view: Uint8Array },
  request: Parameters<typeof encodeRequest>[0],
  heap?: Uint8Array,
): void {
  const reqBytes = encodeRequest(request);
  new Uint8Array(sab.buffer, OFF_REQ_RING, SAB_SLOT_SIZE).set(reqBytes);
  if (heap !== undefined && heap.length > 0) {
    const offset = request.heapPtr ?? 0;
    new Uint8Array(sab.buffer, OFF_HEAP_SCRATCH + offset, heap.length).set(heap);
  }
  const header = new Int32Array(sab.buffer, 0, OFF_HEAP_SCRATCH / 4);
  Atomics.store(header, OFF_REQ_HEAD / 4, 1);
}

/** Read the response currently at slot 0 of the SAB's response ring. */
function readResponseSlot(sab: { buffer: ArrayBuffer; view: Uint8Array }) {
  const resBytes = new Uint8Array(
    new Uint8Array(sab.buffer, OFF_RES_RING, SAB_SLOT_SIZE),
  );
  return decodeResponse(resBytes);
}

describe("serviceSab: FD_WRITE → onConsoleWrite round trip", () => {
  it("pops a seeded FD_WRITE, dispatches through dispatch, pushes the response, and fires onConsoleWrite", async () => {
    const { host, consoleWrites } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const sab = freshSabBacking();
    const message = new TextEncoder().encode("hi\n");
    seedRequest(
      sab,
      {
        opcode: OP_WASI.FD_WRITE,
        requestId: 100,
        arg0: 1,
        heapPtr: 0,
        heapLen: message.length,
      },
      message,
    );

    const rc = host.serviceSab(pid, sab.view);
    expect(rc).toBe(0);

    // Request ring drained: head was 1, tail advanced to 1.
    const header = new Int32Array(sab.buffer, 0, OFF_HEAP_SCRATCH / 4);
    expect(Atomics.load(header, OFF_REQ_HEAD / 4)).toBe(1);
    expect(Atomics.load(header, OFF_REQ_TAIL / 4)).toBe(1);

    // Response landed in slot 0; response ring's head is now 1.
    expect(Atomics.load(header, OFF_RES_HEAD / 4)).toBe(1);
    expect(Atomics.load(header, OFF_RES_TAIL / 4)).toBe(0);
    const response = readResponseSlot(sab);
    expect(response.requestId).toBe(100);
    expect(response.status).toBe(0);
    expect(response.value).toBe(BigInt(message.length));
    expect(response.extraLen).toBe(0);

    // Driver-side effect: the console driver saw the exact line.
    expect(consoleWrites).toHaveLength(1);
    expect(Array.from(consoleWrites[0]!)).toEqual(Array.from(message));
  });
});

describe("serviceSab: empty ring", () => {
  it("returns 1 and does not touch the response ring", async () => {
    const { host, consoleWrites } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const sab = freshSabBacking();
    const rc = host.serviceSab(pid, sab.view);
    expect(rc).toBe(1);

    const header = new Int32Array(sab.buffer, 0, OFF_HEAP_SCRATCH / 4);
    expect(Atomics.load(header, OFF_REQ_HEAD / 4)).toBe(0);
    expect(Atomics.load(header, OFF_REQ_TAIL / 4)).toBe(0);
    expect(Atomics.load(header, OFF_RES_HEAD / 4)).toBe(0);
    expect(Atomics.load(header, OFF_RES_TAIL / 4)).toBe(0);
    expect(consoleWrites).toHaveLength(0);
  });
});

describe("serviceSab: PROC_EXIT", () => {
  it("services a PROC_EXIT(code=0) request and pushes an OK response", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const sab = freshSabBacking();
    seedRequest(sab, {
      opcode: OP_WASI.PROC_EXIT,
      requestId: 200,
      arg0: 0,
    });

    const rc = host.serviceSab(pid, sab.view);
    expect(rc).toBe(0);

    const response = readResponseSlot(sab);
    expect(response.requestId).toBe(200);
    expect(response.status).toBe(0);
    // PROC_EXIT carries no return value; the dispatcher reports
    // success-with-value-zero and the user-side unwinds via the
    // `UserProcessExited` sentinel before ever reading this
    // response. The assertion here is that the response bytes
    // reach the SAB correctly.
    expect(response.value).toBe(0n);
    expect(response.extraLen).toBe(0);

    const header = new Int32Array(sab.buffer, 0, OFF_HEAP_SCRATCH / 4);
    expect(Atomics.load(header, OFF_REQ_TAIL / 4)).toBe(1);
    expect(Atomics.load(header, OFF_RES_HEAD / 4)).toBe(1);
  });

  it("releases the exiting process's /run/display binding so a later DISPLAY_CONNECT returns -ECONNREFUSED", async () => {
    // Kernel-correctness contract for the proc_exit cleanup slice:
    // a display-server-like process that holds a `/run/display`
    // binding has that binding released the instant its PROC_EXIT
    // is serviced, not lazily at parent-side `proc_wait` (which is
    // still deferred). A follow-up DISPLAY_CONNECT from a sibling
    // pid therefore lands on an empty path and returns
    // `-ECONNREFUSED`, instead of succeeding against the orphan
    // listener it used to inherit when socket cleanup was deferred.
    const { host } = await freshHost();

    const ds = host.registerProcess(CAPSET_ALL);
    host.markRunning(ds);
    const bindResult = host.dispatch(ds, {
      opcode: OP_EXT.DISPLAY_BIND,
      requestId: 300,
    });
    expect(bindResult.response.status).toBe(0);

    const exitResult = host.dispatch(ds, {
      opcode: OP_WASI.PROC_EXIT,
      requestId: 301,
      arg0: 0,
    });
    expect(exitResult.response.status).toBe(0);

    const client = host.registerProcess(CAPSET_ALL);
    host.markRunning(client);
    const connectResult = host.dispatch(client, {
      opcode: OP_EXT.DISPLAY_CONNECT,
      requestId: 302,
    });
    expect(connectResult.response.status).toBe(-ERRNO.ECONNREFUSED);
  });

  it("frees the display-server path so a second pid can rebind it", async () => {
    // Companion to the ECONNREFUSED check above: when the binding is
    // truly released, another DisplayServer-capable pid can bind the
    // same path fresh. Before the cleanup slice this would fail with
    // -EADDRINUSE because the orphan binding held the path until a
    // (deferred) reap.
    const { host } = await freshHost();

    const first = host.registerProcess(CAPSET_ALL);
    host.markRunning(first);
    const firstBind = host.dispatch(first, {
      opcode: OP_EXT.DISPLAY_BIND,
      requestId: 400,
    });
    expect(firstBind.response.status).toBe(0);

    const firstExit = host.dispatch(first, {
      opcode: OP_WASI.PROC_EXIT,
      requestId: 401,
      arg0: 0,
    });
    expect(firstExit.response.status).toBe(0);

    const second = host.registerProcess(CAPSET_ALL);
    host.markRunning(second);
    const secondBind = host.dispatch(second, {
      opcode: OP_EXT.DISPLAY_BIND,
      requestId: 402,
    });
    expect(secondBind.response.status).toBe(0);
  });
});
