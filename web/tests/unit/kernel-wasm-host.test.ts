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
  EVENTRWFLAGS,
  EVENTTYPE,
  FILETYPE,
  FSTFLAGS,
  OP_EXT,
  OP_WASI,
  POLL_EVENT_OFF,
  POLL_EVENT_SIZE,
  POLL_SUB_OFF,
  POLL_SUBSCRIPTION_SIZE,
  SUBCLOCKFLAGS,
  WHENCE,
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

    // `FD_READDIR` is in the WASI range (0x002F) but still has no
    // handler; swap this probe when FD_READDIR grows one.
    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_READDIR,
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

// ---- dispatch: FD_FILESTAT_GET --------------------------------------
//
// The kernel handler writes a 64-byte `filestat_t` into the heap-out
// window. The TS tests exercise the kernel-wasm dispatch path end-to-
// end through `host.dispatch()`, mirroring the Rust-level coverage:
// one test per reachable FdObject variant (CharDevice → 2, Socket →
// 6, invalid fd → -EBADF). The Vnode paths (filetype=3 / 4) are
// kernel-side synthesis that TS cannot reach without standing up a
// tmpfs file through the dispatch surface; the Rust tests already
// pin those branches end-to-end.

/**
 * Decode the 64-byte `filestat_t` wire layout into its u64/u8 fields.
 * Shared by the FD_FILESTAT_GET and PATH_FILESTAT_GET describe blocks
 * — both produce the same wire format.
 */
function decodeFilestat(heapOut: Uint8Array): {
  dev: bigint;
  ino: bigint;
  filetype: number;
  nlink: bigint;
  size: bigint;
  atim: bigint;
  mtim: bigint;
  ctim: bigint;
} {
  const view = new DataView(
    heapOut.buffer,
    heapOut.byteOffset,
    heapOut.byteLength,
  );
  return {
    dev: view.getBigUint64(0, true),
    ino: view.getBigUint64(8, true),
    filetype: view.getUint8(16),
    nlink: view.getBigUint64(24, true),
    size: view.getBigUint64(32, true),
    atim: view.getBigUint64(40, true),
    mtim: view.getBigUint64(48, true),
    ctim: view.getBigUint64(56, true),
  };
}

describe("dispatch: FD_FILESTAT_GET", () => {
  it("returns filetype=CHARACTER_DEVICE for a /dev/console fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FILESTAT_GET,
      requestId: 710,
      arg0: 1,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(response.status).toBe(0);
    expect(response.extraLen).toBe(64);
    const st = decodeFilestat(heapOut);
    expect(st.filetype).toBe(FILETYPE.CHARACTER_DEVICE);
    expect(st.nlink).toBe(1n);
    expect(st.size).toBe(0n);
    expect(st.dev).toBe(0n);
    // The first 17 bytes are dev (0..8) + ino (8..16) + filetype (16);
    // bytes 17..24 must stay zero for struct-alignment compliance.
    const view = new DataView(
      heapOut.buffer,
      heapOut.byteOffset,
      heapOut.byteLength,
    );
    for (let i = 17; i < 24; i++) {
      expect(view.getUint8(i)).toBe(0);
    }
  });

  it("returns filetype=SOCKET_STREAM for an IPC socket fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Stand up a socket fd via IPC_SOCKET(Stream=0). Response.value
    // carries the newly allocated fd number.
    const sockResp = host.dispatch(pid, {
      opcode: OP_EXT.IPC_SOCKET,
      requestId: 711,
      arg0: 0,
    });
    expect(sockResp.response.status).toBe(0);
    const sockFd = Number(sockResp.response.value);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FILESTAT_GET,
      requestId: 712,
      arg0: sockFd,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(response.status).toBe(0);
    expect(response.extraLen).toBe(64);
    const st = decodeFilestat(heapOut);
    expect(st.filetype).toBe(FILETYPE.SOCKET_STREAM);
    expect(st.nlink).toBe(1n);
    expect(st.size).toBe(0n);
  });

  it("returns -EBADF for an unopened fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FILESTAT_GET,
      requestId: 713,
      arg0: 99,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(response.status).toBe(-ERRNO.EBADF);
    expect(response.extraLen).toBe(0);
  });

  it("reports the heap-out layout with times and size defaulting to 0 for non-Vnode fds", async () => {
    // Pinning the WASI wire layout: dev/ino/nlink/size/atim/mtim/ctim
    // are u64 LE at their documented offsets; filetype is a single
    // byte at offset 16; the 7 bytes between filetype and nlink are
    // struct-alignment padding and stay zero.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 0);
    host.markRunning(pid);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FILESTAT_GET,
      requestId: 714,
      arg0: 0,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(response.status).toBe(0);
    const st = decodeFilestat(heapOut);
    expect(st.atim).toBe(0n);
    expect(st.mtim).toBe(0n);
    expect(st.ctim).toBe(0n);
    expect(st.filetype).toBe(FILETYPE.CHARACTER_DEVICE);
  });
});

// ---- dispatch: PATH_FILESTAT_GET ------------------------------------
//
// The path-based sibling of FD_FILESTAT_GET. Dispatches stage the
// path bytes into the kernel's heap-scratch region as `heapIn`; the
// kernel reads the path, resolves it, and writes the 64-byte
// `filestat_t` back into the same region. The TS tests cover the
// three kernel branches the dispatch surface can reach without
// standing up a tmpfs file through the syscall path: `/dev/console`
// (char device), `/nonexistent` (ENOENT), and `/` (root directory).
// The Rust tests pin the tmpfs-regular-file + tmpfs-directory paths
// that need `Vfs::create` / `Vfs::mkdir` setup.

describe("dispatch: PATH_FILESTAT_GET", () => {
  function encodePath(s: string): Uint8Array {
    return new TextEncoder().encode(s);
  }

  it("returns filetype=CHARACTER_DEVICE for /dev/console", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = encodePath("/dev/console");
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 740,
        arg0: 0, // dir_fd — ignored
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(response.status).toBe(0);
    expect(response.extraLen).toBe(64);
    const st = decodeFilestat(heapOut);
    expect(st.filetype).toBe(FILETYPE.CHARACTER_DEVICE);
    expect(st.nlink).toBe(1n);
    expect(st.size).toBe(0n);
  });

  it("returns -ENOENT for a path that does not resolve", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = encodePath("/nonexistent/path");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 741,
        arg0: 0,
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(response.status).toBe(-ERRNO.ENOENT);
    expect(response.extraLen).toBe(0);
  });

  it("returns filetype=DIRECTORY for the root path /", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = encodePath("/");
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 742,
        arg0: 0,
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(response.status).toBe(0);
    expect(response.extraLen).toBe(64);
    const st = decodeFilestat(heapOut);
    expect(st.filetype).toBe(FILETYPE.DIRECTORY);
    // Root directory's nlink is 1 in tmpfs; size is 0 (directory).
    expect(st.size).toBe(0n);
  });
});

// ---- dispatch: PATH_FILESTAT_SET_TIMES -----------------------------
//
// Wire layout (kernel side): dir_fd + lookup_flags at args[0..4]
// + [4..8] (ignored in v1); fstflags at args[8..12] (low 4 bits
// meaningful); atim at heap[0..8] (u64 LE ns); mtim at heap[8..16]
// (u64 LE ns); path at heap[16..]. heap_len = 16 + path.len().
// Response carries status only (value = 0 on success).
//
// The TS tests pin the wire layout end-to-end through kernel.wasm:
// the EINVAL paths (invalid flag pairs, short heap) cover the
// dispatcher's decoding rejections; the EROFS path pins devfs's
// read-only contract; the happy path on /proc/version is not
// useful because procfs returns its own EROFS. The end-to-end
// "times actually applied" check lives in the Rust kernel-level
// tests since the TS dispatcher alone can't easily stat a tmpfs
// file through the opcode surface (no FD_FILESTAT_GET via PATH
// ... → actually it can, through a PATH_OPEN + FD_FILESTAT_GET
// fold, but the Rust tests already pin this exhaustively).

function encodeSetTimesHeap(atim: bigint, mtim: bigint, path: string): Uint8Array {
  const pathBytes = new TextEncoder().encode(path);
  const buf = new Uint8Array(16 + pathBytes.length);
  const view = new DataView(buf.buffer);
  view.setBigUint64(0, atim, true);
  view.setBigUint64(8, mtim, true);
  buf.set(pathBytes, 16);
  return buf;
}

function encodeSetTimesArgs(
  dirFd: number,
  lookupFlags: number,
  fstflags: number,
): Uint8Array {
  const args = new Uint8Array(16);
  const view = new DataView(args.buffer);
  view.setUint32(0, dirFd, true);
  view.setUint32(4, lookupFlags, true);
  view.setUint32(8, fstflags, true);
  return args;
}

describe("dispatch: PATH_FILESTAT_SET_TIMES", () => {
  it("returns -ENOENT for a path that does not resolve", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodeSetTimesHeap(111n, 222n, "/nowhere");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_SET_TIMES,
        requestId: 810,
        args: encodeSetTimesArgs(0, 0, FSTFLAGS.SET_ATIM | FSTFLAGS.SET_MTIM),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(-ERRNO.ENOENT);
  });

  it("returns -EROFS for a /dev path (devfs is read-only)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodeSetTimesHeap(0n, 0n, "/dev/console");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_SET_TIMES,
        requestId: 811,
        args: encodeSetTimesArgs(0, 0, FSTFLAGS.SET_ATIM | FSTFLAGS.SET_MTIM),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(-ERRNO.EROFS);
  });

  it("returns -EINVAL when SET_ATIM and SET_ATIM_NOW are both set", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Path doesn't need to resolve — the flag-pair check fires
    // before path resolution. Asserting the EINVAL errno without
    // creating a real file also keeps the test isolated.
    const heap = encodeSetTimesHeap(0n, 0n, "/whatever");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_SET_TIMES,
        requestId: 812,
        args: encodeSetTimesArgs(
          0,
          0,
          FSTFLAGS.SET_ATIM | FSTFLAGS.SET_ATIM_NOW,
        ),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EINVAL when SET_MTIM and SET_MTIM_NOW are both set", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodeSetTimesHeap(0n, 0n, "/whatever");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_SET_TIMES,
        requestId: 813,
        args: encodeSetTimesArgs(
          0,
          0,
          FSTFLAGS.SET_MTIM | FSTFLAGS.SET_MTIM_NOW,
        ),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EINVAL when heap is shorter than the 16-byte atim/mtim prefix", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const shortHeap = new Uint8Array(8); // only room for atim, no mtim + no path
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_SET_TIMES,
        requestId: 814,
        args: encodeSetTimesArgs(0, 0, FSTFLAGS.SET_ATIM),
        heapPtr: 0,
        heapLen: shortHeap.length,
      },
      shortHeap,
    );
    expect(response.status).toBe(-ERRNO.EINVAL);
  });

  it("returns 0 (no-op success) when fstflags is 0 on an existing path", async () => {
    // Zero-flags is a legal permission probe — resolve the path,
    // validate the caller, do nothing else. Use /proc/version since
    // it exists and is reachable via the dispatch surface alone.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodeSetTimesHeap(0n, 0n, "/proc/version");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_SET_TIMES,
        requestId: 815,
        args: encodeSetTimesArgs(0, 0, 0),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(0);
  });
});

// ---- dispatch: POLL_ONEOFF -----------------------------------------
//
// Multi-subscription readiness check. Wire: (n_subs, n_events_cap) as
// two u32s in the inline args window; heap laid out as subs at
// [0..n_subs*48] and events at [n_subs*48..n_subs*48 + n_events*32].
// Response carries the actual emitted-event count in `value` and
// mirrors it in `extraLen`.
//
// These TS tests pin the wire layout end-to-end through kernel.wasm
// for the dispatcher-level branches: n_subs=0 rejection, the CLOCK
// ready/not-ready splits, per-subscription EBADF/EINVAL/ENOTSUP, and
// the events-cap clamp. The FD_READ/FD_WRITE Vnode/Socket fine-grained
// paths are covered by the Rust kernel tests (which exercise the same
// dispatcher through the same native-platform harness).

function packPollArgs(nSubs: number, nEventsCap: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, nSubs, true);
  v.setUint32(4, nEventsCap, true);
  return args;
}

function packSubClock(
  userdata: bigint,
  clockId: number,
  timeout: bigint,
  flags: number,
): Uint8Array {
  const s = new Uint8Array(POLL_SUBSCRIPTION_SIZE);
  const v = new DataView(s.buffer);
  v.setBigUint64(POLL_SUB_OFF.USERDATA, userdata, true);
  s[POLL_SUB_OFF.TAG] = EVENTTYPE.CLOCK;
  v.setUint32(POLL_SUB_OFF.CLOCK_ID, clockId, true);
  v.setBigUint64(POLL_SUB_OFF.CLOCK_TIMEOUT, timeout, true);
  v.setUint16(POLL_SUB_OFF.CLOCK_FLAGS, flags, true);
  return s;
}

function packSubFdRw(userdata: bigint, tag: number, fd: number): Uint8Array {
  const s = new Uint8Array(POLL_SUBSCRIPTION_SIZE);
  const v = new DataView(s.buffer);
  v.setBigUint64(POLL_SUB_OFF.USERDATA, userdata, true);
  s[POLL_SUB_OFF.TAG] = tag;
  v.setUint32(POLL_SUB_OFF.FDRW_FD, fd, true);
  return s;
}

interface DecodedEvent {
  readonly userdata: bigint;
  readonly error: number;
  readonly type: number;
  readonly nbytes: bigint;
  readonly rwflags: number;
}

function decodeEvent(heap: Uint8Array, offset: number): DecodedEvent {
  const v = new DataView(heap.buffer, heap.byteOffset + offset, POLL_EVENT_SIZE);
  return {
    userdata: v.getBigUint64(POLL_EVENT_OFF.USERDATA, true),
    error: v.getUint16(POLL_EVENT_OFF.ERROR, true),
    type: v.getUint8(POLL_EVENT_OFF.TYPE),
    nbytes: v.getBigUint64(POLL_EVENT_OFF.RW_NBYTES, true),
    rwflags: v.getUint16(POLL_EVENT_OFF.RW_FLAGS, true),
  };
}

describe("dispatch: POLL_ONEOFF", () => {
  it("returns -EINVAL for n_subs == 0", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new Uint8Array(128);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 830,
        args: packPollArgs(0, 4),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EINVAL when heap is too short to hold the max of subs and events windows", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // 2 subs needs 96 bytes; heap of 60 is short → EINVAL.
    const heap = new Uint8Array(60);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 831,
        args: packPollArgs(2, 2),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(-ERRNO.EINVAL);
  });

  it("CLOCK monotonic with ABSTIME in the past fires one ready event", async () => {
    const { host } = await freshHost({ nowNs: () => 1_000_000_000n });
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new Uint8Array(POLL_SUBSCRIPTION_SIZE);
    const sub = packSubClock(
      42n,
      CLOCKID.MONOTONIC,
      1n, // abs 1 ns; far in the past of any non-zero now.
      SUBCLOCKFLAGS.ABSTIME,
    );
    heap.set(sub, 0);
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 832,
        args: packPollArgs(1, 1),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(0);
    expect(response.value).toBe(1n);
    expect(response.extraLen).toBe(POLL_EVENT_SIZE);
    const ev = decodeEvent(heapOut, 0);
    expect(ev.userdata).toBe(42n);
    expect(ev.error).toBe(0);
    expect(ev.type).toBe(EVENTTYPE.CLOCK);
  });

  it("CLOCK monotonic with ABSTIME far in the future emits zero events", async () => {
    const { host } = await freshHost({ nowNs: () => 1_000n });
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new Uint8Array(POLL_SUBSCRIPTION_SIZE);
    const sub = packSubClock(
      7n,
      CLOCKID.MONOTONIC,
      0xFFFF_FFFF_FFFF_FFFFn,
      SUBCLOCKFLAGS.ABSTIME,
    );
    heap.set(sub, 0);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 833,
        args: packPollArgs(1, 1),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(0);
    expect(response.value).toBe(0n);
    expect(response.extraLen).toBe(0);
  });

  it("CLOCK with relative timeout 0 is ready (non-blocking semantics)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new Uint8Array(POLL_SUBSCRIPTION_SIZE);
    heap.set(packSubClock(1n, CLOCKID.MONOTONIC, 0n, 0), 0);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 834,
        args: packPollArgs(1, 1),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(0);
    expect(response.value).toBe(1n);
  });

  it("CLOCK invalid id emits one event with per-sub EINVAL", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new Uint8Array(POLL_SUBSCRIPTION_SIZE);
    heap.set(packSubClock(9n, 99, 0n, 0), 0);
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 835,
        args: packPollArgs(1, 1),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(0);
    expect(response.value).toBe(1n);
    const ev = decodeEvent(heapOut, 0);
    expect(ev.error).toBe(ERRNO.EINVAL);
    expect(ev.type).toBe(EVENTTYPE.CLOCK);
  });

  it("CLOCK cputime id emits one event with per-sub ENOTSUP", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new Uint8Array(POLL_SUBSCRIPTION_SIZE);
    heap.set(packSubClock(3n, CLOCKID.PROCESS_CPUTIME_ID, 0n, 0), 0);
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 836,
        args: packPollArgs(1, 1),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(0);
    expect(response.value).toBe(1n);
    const ev = decodeEvent(heapOut, 0);
    expect(ev.error).toBe(ERRNO.ENOTSUP);
    expect(ev.type).toBe(EVENTTYPE.CLOCK);
  });

  it("FD_READ on an unopened fd emits one event with per-sub EBADF", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new Uint8Array(POLL_SUBSCRIPTION_SIZE);
    heap.set(packSubFdRw(17n, EVENTTYPE.FD_READ, 99), 0);
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 837,
        args: packPollArgs(1, 1),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(0);
    expect(response.value).toBe(1n);
    const ev = decodeEvent(heapOut, 0);
    expect(ev.userdata).toBe(17n);
    expect(ev.error).toBe(ERRNO.EBADF);
    expect(ev.type).toBe(EVENTTYPE.FD_READ);
  });

  it("FD_WRITE on a /dev/console fd is always ready", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const heap = new Uint8Array(POLL_SUBSCRIPTION_SIZE);
    heap.set(packSubFdRw(5n, EVENTTYPE.FD_WRITE, 1), 0);
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 838,
        args: packPollArgs(1, 1),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(0);
    expect(response.value).toBe(1n);
    const ev = decodeEvent(heapOut, 0);
    expect(ev.error).toBe(0);
    expect(ev.type).toBe(EVENTTYPE.FD_WRITE);
    expect(ev.rwflags).toBe(0);
  });

  it("FD_READ on an empty /dev/console is not yet ready (zero events)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 0);
    host.markRunning(pid);

    const heap = new Uint8Array(POLL_SUBSCRIPTION_SIZE);
    heap.set(packSubFdRw(6n, EVENTTYPE.FD_READ, 0), 0);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 839,
        args: packPollArgs(1, 1),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(0);
    expect(response.value).toBe(0n);
  });

  it("events_cap clamps the output when more subs are ready than the cap allows", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Three ready clock subs; cap = 2 → only two events emitted.
    // Heap sized for 3 * 48-byte subs (which also covers 2 * 32-byte
    // events — events overwrite subs in place).
    const heap = new Uint8Array(3 * POLL_SUBSCRIPTION_SIZE);
    heap.set(packSubClock(1n, CLOCKID.MONOTONIC, 0n, 0), 0);
    heap.set(packSubClock(2n, CLOCKID.MONOTONIC, 0n, 0), POLL_SUBSCRIPTION_SIZE);
    heap.set(
      packSubClock(3n, CLOCKID.REALTIME, 0n, 0),
      2 * POLL_SUBSCRIPTION_SIZE,
    );
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 840,
        args: packPollArgs(3, 2),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(0);
    expect(response.value).toBe(2n);
    expect(response.extraLen).toBe(2 * POLL_EVENT_SIZE);
  });

  it("echoes userdata verbatim in the emitted event", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new Uint8Array(POLL_SUBSCRIPTION_SIZE);
    const ud = 0xDEAD_BEEF_CAFE_BABEn;
    heap.set(packSubClock(ud, CLOCKID.MONOTONIC, 0n, 0), 0);
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.POLL_ONEOFF,
        requestId: 841,
        args: packPollArgs(1, 1),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response.status).toBe(0);
    const ev = decodeEvent(heapOut, 0);
    expect(ev.userdata).toBe(ud);
  });

  it("suppress: EVENTRWFLAGS.FD_READWRITE_HANGUP const matches the Rust-side bit value", () => {
    // Quick sanity check so a drift between the TS mirror and the
    // abi crate is caught mechanically rather than waiting for a
    // semantic mismatch in a downstream test.
    expect(EVENTRWFLAGS.FD_READWRITE_HANGUP).toBe(0x1);
  });
});

// ---- dispatch: FD_SEEK ----------------------------------------------
//
// FD_SEEK packs `(fd, whence, offset)` into the inline args window
// (fd at 0..4, whence at 4..8, offset i64 at 8..16) and gets back the
// new absolute offset in `response.value` — same shape as
// `clock_time_get` (one u64 result, no heap round-trip). The TS tests
// pin the wire layout end-to-end through `kernel.wasm`'s dispatcher:
// the EBADF + EINVAL paths cover the non-Vnode rejection branches
// without a setup pipeline, and the SeekSet/Cur/End round trips against
// a `/proc/version` Vnode fd (procfs is mounted at /proc by the kernel
// init, so PATH_OPEN gives us a regular-file fd through the dispatch
// surface alone). The Rust kernel-side tests pin every whence-decoding
// branch and the underflow EINVAL paths exhaustively against tmpfs.

/** Encode `(fd, whence, offset)` into the 16-byte inline args window
 * the FD_SEEK opcode expects. */
function encodeFdSeekArgs(
  fd: number,
  whence: number,
  offset: bigint,
): Uint8Array {
  const args = new Uint8Array(16);
  const view = new DataView(args.buffer);
  view.setUint32(0, fd, true);
  view.setUint32(4, whence, true);
  view.setBigInt64(8, offset, true);
  return args;
}

/** Open `/proc/version` via PATH_OPEN and return the resulting Vnode
 * fd. ProcFs is mounted at /proc by the kernel init, so the file
 * always resolves; the size is whatever StaticProcFsSource emits for
 * the version string (non-zero, so SeekEnd assertions are meaningful). */
function openProcVersion(host: KernelWasmHost, pid: number): number {
  const pathBytes = new TextEncoder().encode("/proc/version");
  const { response } = host.dispatch(
    pid,
    {
      opcode: OP_WASI.PATH_OPEN,
      requestId: 999,
      arg0: 0,
      heapPtr: 0,
      heapLen: pathBytes.length,
    },
    pathBytes,
  );
  expect(response.status).toBe(0);
  return Number(response.value);
}

describe("dispatch: FD_SEEK", () => {
  it("returns -EBADF for an unopened fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_SEEK,
      requestId: 770,
      args: encodeFdSeekArgs(99, WHENCE.SET, 0n),
    });
    expect(response.status).toBe(-ERRNO.EBADF);
    expect(response.value).toBe(0n);
  });

  it("returns -EINVAL for a /dev/console fd (whence has no meaning on a CharDevice)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_SEEK,
      requestId: 771,
      args: encodeFdSeekArgs(1, WHENCE.SET, 0n),
    });
    expect(response.status).toBe(-ERRNO.EINVAL);
  });

  it("SeekSet on a /proc/version Vnode fd returns the new absolute offset", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_SEEK,
      requestId: 772,
      args: encodeFdSeekArgs(fd, WHENCE.SET, 7n),
    });
    expect(response.status).toBe(0);
    expect(response.value).toBe(7n);
  });

  it("SeekCur 0 reports the current offset (the fd_tell idiom)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    // First SeekSet to position 4, then SeekCur 0 to read it back.
    const seek = host.dispatch(pid, {
      opcode: OP_WASI.FD_SEEK,
      requestId: 773,
      args: encodeFdSeekArgs(fd, WHENCE.SET, 4n),
    });
    expect(seek.response.status).toBe(0);

    const tell = host.dispatch(pid, {
      opcode: OP_WASI.FD_SEEK,
      requestId: 774,
      args: encodeFdSeekArgs(fd, WHENCE.CUR, 0n),
    });
    expect(tell.response.status).toBe(0);
    expect(tell.response.value).toBe(4n);
  });

  it("SeekEnd 0 returns the file size of /proc/version", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    // Cross-check the size via FD_FILESTAT_GET so the test doesn't
    // hard-code the StaticProcFsSource string length — both paths
    // should agree on whatever the procfs source happens to produce.
    const stat = host.dispatch(pid, {
      opcode: OP_WASI.FD_FILESTAT_GET,
      requestId: 775,
      arg0: fd,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(stat.response.status).toBe(0);
    const size = decodeFilestat(stat.heapOut).size;
    expect(size).toBeGreaterThan(0n);

    const seek = host.dispatch(pid, {
      opcode: OP_WASI.FD_SEEK,
      requestId: 776,
      args: encodeFdSeekArgs(fd, WHENCE.END, 0n),
    });
    expect(seek.response.status).toBe(0);
    expect(seek.response.value).toBe(size);
  });
});

// ---- dispatch: FD_TELL ----------------------------------------------
//
// The read-only sibling of FD_SEEK: fd at a single u32 arg0 (no
// whence + no offset — fd_tell takes nothing but an fd), response
// carries the current absolute offset in `response.value`. Shares
// FD_SEEK's EBADF + non-Vnode-EINVAL guards; reuses the
// `openProcVersion` + `encodeFdSeekArgs` helpers from the FD_SEEK
// block for the after-seek integration check.

describe("dispatch: FD_TELL", () => {
  it("returns 0 initially for a freshly-opened /proc/version Vnode fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_TELL,
      requestId: 780,
      arg0: fd,
    });
    expect(response.status).toBe(0);
    expect(response.value).toBe(0n);
  });

  it("returns the seek'd offset after a SeekSet via FD_SEEK", async () => {
    // Pin the integration: fd_tell sees whatever fd_seek just wrote.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    const seek = host.dispatch(pid, {
      opcode: OP_WASI.FD_SEEK,
      requestId: 781,
      args: encodeFdSeekArgs(fd, WHENCE.SET, 6n),
    });
    expect(seek.response.status).toBe(0);

    const tell = host.dispatch(pid, {
      opcode: OP_WASI.FD_TELL,
      requestId: 782,
      arg0: fd,
    });
    expect(tell.response.status).toBe(0);
    expect(tell.response.value).toBe(6n);
  });

  it("returns -EINVAL for a /dev/console fd (fd_tell has no meaning on a CharDevice)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_TELL,
      requestId: 783,
      arg0: 1,
    });
    expect(response.status).toBe(-ERRNO.EINVAL);
  });
});

// ---- dispatch: fd-state opcodes (FD_ADVISE / FD_ALLOCATE / FD_SYNC /
// ---- FD_DATASYNC) ----------------------------------------------------
//
// Four related "fd-state" opcodes bundled into one describe block.
// All four take an fd as a u32 at `arg0` and collapse to trivial
// semantics in v1's tmpfs-backed VFS:
//
//   FD_ADVISE / FD_SYNC / FD_DATASYNC = no-op success on a Vnode.
//   FD_ALLOCATE                       = ENOTSUP on a Vnode.
//   All four                          = EBADF on an unopened fd,
//                                       EINVAL on every non-Vnode
//                                       FdObject (the state these
//                                       opcodes touch only has meaning
//                                       for seekable regular files).
//
// Reuses the `openProcVersion` helper from the FD_SEEK block to stage
// a Vnode fd through the dispatch surface alone.

describe("dispatch: FD_ADVISE", () => {
  it("returns 0 on a /proc/version Vnode fd (no-op success)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_ADVISE,
      requestId: 790,
      arg0: fd,
    });
    expect(response.status).toBe(0);
    expect(response.value).toBe(0n);
  });

  it("returns -EINVAL for a /dev/console fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_ADVISE,
      requestId: 791,
      arg0: 1,
    });
    expect(response.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EBADF for an unopened fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_ADVISE,
      requestId: 792,
      arg0: 99,
    });
    expect(response.status).toBe(-ERRNO.EBADF);
  });
});

describe("dispatch: FD_ALLOCATE", () => {
  it("returns -ENOTSUP on a /proc/version Vnode fd (v1 tmpfs has no preallocation)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_ALLOCATE,
      requestId: 793,
      arg0: fd,
    });
    expect(response.status).toBe(-ERRNO.ENOTSUP);
  });

  it("returns -EINVAL for a /dev/console fd (non-Vnode rejection fires before ENOTSUP)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_ALLOCATE,
      requestId: 794,
      arg0: 1,
    });
    expect(response.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EBADF for an unopened fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_ALLOCATE,
      requestId: 795,
      arg0: 99,
    });
    expect(response.status).toBe(-ERRNO.EBADF);
  });
});

describe("dispatch: FD_SYNC", () => {
  it("returns 0 on a /proc/version Vnode fd (no-op success)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_SYNC,
      requestId: 796,
      arg0: fd,
    });
    expect(response.status).toBe(0);
    expect(response.value).toBe(0n);
  });

  it("returns -EINVAL for a /dev/console fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_SYNC,
      requestId: 797,
      arg0: 1,
    });
    expect(response.status).toBe(-ERRNO.EINVAL);
  });
});

describe("dispatch: FD_DATASYNC", () => {
  it("returns 0 on a /proc/version Vnode fd (no-op success)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_DATASYNC,
      requestId: 798,
      arg0: fd,
    });
    expect(response.status).toBe(0);
    expect(response.value).toBe(0n);
  });

  it("returns -EINVAL for a /dev/console fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_DATASYNC,
      requestId: 799,
      arg0: 1,
    });
    expect(response.status).toBe(-ERRNO.EINVAL);
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
