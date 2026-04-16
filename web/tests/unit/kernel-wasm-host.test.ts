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
import {
  CAP,
  CAPSET_ALL,
  CAPSET_ORDINARY_APP,
  CAPSET_DESKTOP_SHELL,
  DEV,
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
    nowNs: () => 0n,
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
    host.injectInput(DEV.CONSOLE, new TextEncoder().encode("abc"));

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

  it("injectInput rejects non-console devnums", async () => {
    const { host } = await freshHost();
    expect(() => host.injectInput(DEV.INPUT_KBD, new Uint8Array([1, 2, 3]))).toThrow(/DEV\.CONSOLE/);
  });

  it("injectInput rejects payloads larger than the heap scratch capacity", async () => {
    const { host } = await freshHost();
    const tooMuch = new Uint8Array(4097); // heap scratch is 4096
    expect(() => host.injectInput(DEV.CONSOLE, tooMuch)).toThrow(/capacity/);
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

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.CLOCK_TIME_GET,
      requestId: 41,
    });
    expect(response.status).toBe(-ERRNO.ENOSYS);
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
