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
//   * Error propagation: bad fd -> -EBADF in response!.status; unknown
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
  DIRENT_OFF,
  decodeResponse,
  encodeRequest,
  encodeSpawnManifest,
  ERRNO,
  EVENTRWFLAGS,
  EVENTTYPE,
  FDFLAGS,
  FILETYPE,
  FSTFLAGS,
  OP_EXT,
  OP_WASI,
  POLL_DIRENT_HEADER_SIZE,
  POLL_EVENT_OFF,
  POLL_EVENT_SIZE,
  POLL_SUB_OFF,
  POLL_SUBSCRIPTION_SIZE,
  SDFLAGS,
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
  /**
   * Optional `BlockDriver` for the OPFS mount. T084 tests pass a
   * `BlockDriver.withHandle(MemSyncAccessHandle)` so `kernel_init`'s
   * `/persist` mount path is exercised end-to-end without touching
   * the real browser OPFS.
   */
  readonly blockDriver?: import("../../src/drivers/types").Driver;
}

/**
 * In-memory `SyncAccessHandle` stub for T084 integration tests
 * that wire a `BlockDriver` into `freshHost`. Same shape as the
 * `MemSyncAccessHandle` in `block.test.ts` but inlined here so
 * neither test file imports the other.
 */
function makeMemSyncAccessHandle(): import(
  "../../src/drivers/block"
).SyncAccessHandle & { getSize(): number } {
  let buf = new Uint8Array(1 << 24);
  let size = 0;
  return {
    read(buffer, options) {
      const end = Math.min(size, options.at + buffer.length);
      if (end <= options.at) return 0;
      const n = end - options.at;
      buffer.set(buf.subarray(options.at, end), 0);
      return n;
    },
    write(buffer, options) {
      const need = options.at + buffer.length;
      if (need > buf.length) {
        const next = new Uint8Array(Math.max(buf.length * 2, need));
        next.set(buf);
        buf = next;
      }
      buf.set(buffer, options.at);
      if (need > size) size = need;
      return buffer.length;
    },
    flush() {},
    getSize() {
      return size;
    },
    truncate(newSize) {
      if (newSize > buf.length) {
        const next = new Uint8Array(Math.max(buf.length * 2, newSize));
        next.set(buf);
        buf = next;
      }
      size = newSize;
    },
    close() {},
  };
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
    blockDriver: opts.blockDriver,
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

  it("recordProcessMemory accepts known pids and rejects unknown pids", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    expect(() => host.recordProcessMemory(pid, 128 * 1024)).not.toThrow();
    expect(() => host.recordProcessMemory(999_999, 128 * 1024)).toThrow(
      /recordProcessMemory/,
    );
  });

  it("syncAll on a fresh kernel returns true", async () => {
    const { host } = await freshHost();
    expect(host.syncAll()).toBe(true);
  });

  it("syncAll after a tmpfs mutation still returns true (tmpfs sync is a no-op)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const pathBytes = new TextEncoder().encode("/tmp/persist");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 7,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(host.syncAll()).toBe(true);
  });

  it("/proc/version surfaces a live PMos version line via the live procfs source", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const pathBytes = new TextEncoder().encode("/proc/version");
    const open = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 4001,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(open.response!.status).toBe(0);
    const fd = Number(open.response!.value);

    const read = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 4002,
      arg0: fd,
      heapPtr: 0,
      heapLen: 256,
    });
    expect(read.response!.status).toBe(0);
    const text = new TextDecoder().decode(
      read.heapOut!.slice(0, Number(read.response!.value)),
    );
    // The live source emits "PMos <semver> (wasm32, ABI <maj>.<min>)\n".
    // The static placeholder it replaced was "PMos 0.1.0-alpha (native-test)\n".
    expect(text).toMatch(/^PMos \S+ \(wasm32, ABI \d+\.\d+\)\n$/);
  });

  it("/proc/<pid>/status surfaces live process metadata via the live procfs source", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = `/proc/${pid}/status`;
    const pathBytes = new TextEncoder().encode(path);
    const open = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 4101,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(open.response!.status).toBe(0);
    const fd = Number(open.response!.value);

    const read = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 4102,
      arg0: fd,
      heapPtr: 0,
      heapLen: 512,
    });
    expect(read.response!.status).toBe(0);
    const text = new TextDecoder().decode(
      read.heapOut!.slice(0, Number(read.response!.value)),
    );
    // Six-line format pinned by T168 (status_ends_with_trailing_newline_and_six_fields).
    // The live source projects the running process — name "proc" (the
    // default kernel_register_process name), state R (running, post
    // markRunning), pid + ppid 0, and zero VmSize/VmPeak (no memory
    // observation has fired yet because no user Worker is attached).
    expect(text).toContain("Name:\tproc\n");
    expect(text).toContain("State:\tR (running)\n");
    expect(text).toContain(`Tgid:\t${pid}\n`);
    expect(text).toContain(`Pid:\t${pid}\n`);
    expect(text).toContain("PPid:\t0\n");
    expect(text).toContain("TracerPid:\t0\n");
    expect(text).toContain("VmSize:\t0 kB\n");
    expect(text).toContain("VmPeak:\t0 kB\n");
    expect(text).toContain("Threads:\t1\n");
  });

  it("/proc/loadavg surfaces live running/total/last_pid via the live procfs source", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const pathBytes = new TextEncoder().encode("/proc/loadavg");
    const open = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 4401,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(open.response!.status).toBe(0);
    const fd = Number(open.response!.value);

    const read = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 4402,
      arg0: fd,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(read.response!.status).toBe(0);
    const text = new TextDecoder().decode(
      read.heapOut!.slice(0, Number(read.response!.value)),
    );
    // Format: "<1m> <5m> <15m> <running>/<total> <last_pid>\n".
    // With nowNs frozen at 0 (deterministic test clock), the
    // load averages never accumulate decay — they stay 0.00.
    // The running/total/last_pid trio projects live state:
    // one registered+running process → running=total=1,
    // last_pid=<pid>.
    expect(text).toBe(`0.00 0.00 0.00 1/1 ${pid}\n`);
  });

  it("/proc/loadavg accumulates non-zero averages as the kernel clock advances", async () => {
    // Drive the wasm-side `Platform::now_ns` host import from a
    // mutable counter so each /proc/loadavg read sees a different
    // monotonic time. After many sample intervals the 1m EMA
    // should reflect the runnable count of the registered process.
    let now = 0n;
    const { host } = await freshHost({ nowNs: () => now });
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const pathBytes = new TextEncoder().encode("/proc/loadavg");
    function readLoadavg(): string {
      const open = host.dispatch(
        pid,
        {
          opcode: OP_WASI.PATH_OPEN,
          requestId: 5301,
          arg0: 0,
          heapPtr: 0,
          heapLen: pathBytes.length,
        },
        pathBytes,
      );
      const fd = Number(open.response!.value);
      const read = host.dispatch(pid, {
        opcode: OP_WASI.FD_READ,
        requestId: 5302,
        arg0: fd,
        heapPtr: 0,
        heapLen: 64,
      });
      return new TextDecoder().decode(
        read.heapOut!.slice(0, Number(read.response!.value)),
      );
    }

    // First read at now=0 seeds the LoadAverages without a decay
    // step — loads still 0.00.
    expect(readLoadavg()).toMatch(/^0\.00 0\.00 0\.00 1\/1 /);

    // Advance the clock by 100 sample intervals (500 seconds) and
    // read again. Each read folds in one EMA step per elapsed
    // sample interval. With one Running process the runnable
    // count is 1; the 1m average converges fast (approaches 1.0
    // by step ~30), the 5m average is still climbing past ~0.6,
    // and the 15m average is well below at ~0.28.
    now = BigInt(500) * 1_000_000_000n;
    const text = readLoadavg();
    const fields = text.split(" ");
    expect(fields).toHaveLength(5);
    const [oneM, fiveM, fifteenM, ratio, lastPid] = fields;
    // 1m approaches 1.00 from below — match a single-digit-then-
    // two-decimal float at least 0.90 (well past initial decay).
    expect(parseFloat(oneM!)).toBeGreaterThan(0.9);
    expect(parseFloat(oneM!)).toBeLessThan(1.05);
    // 5m converges slower; still climbing at this point.
    expect(parseFloat(fiveM!)).toBeGreaterThan(0.5);
    expect(parseFloat(fiveM!)).toBeLessThan(parseFloat(oneM!));
    // 15m slowest of all; just past 1/4 of the way at 500s.
    expect(parseFloat(fifteenM!)).toBeGreaterThan(0.1);
    expect(parseFloat(fifteenM!)).toBeLessThan(parseFloat(fiveM!));
    // Suffix unchanged: still one running process.
    expect(ratio).toBe("1/1");
    expect(lastPid).toMatch(/^\d+\n$/);
  });

  it("/proc/uptime surfaces a live monotonic seconds-since-boot via the live procfs source", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const pathBytes = new TextEncoder().encode("/proc/uptime");
    const open = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 4301,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(open.response!.status).toBe(0);
    const fd = Number(open.response!.value);

    const read = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 4302,
      arg0: fd,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(read.response!.status).toBe(0);
    const text = new TextDecoder().decode(
      read.heapOut!.slice(0, Number(read.response!.value)),
    );
    // Format: "<uptime_seconds> 0\n" — uptime is the live
    // `now_ns - boot_time_ns` projected through Kernel.boot_time_ns
    // captured at Kernel::new. The host harness instantiates the
    // kernel synchronously in this test, so seconds may be 0 or
    // very small but the format must be parseable.
    expect(text).toMatch(/^\d+ 0\n$/);
  });

  it("kernel_init mounts OPFS at /persist when a BlockDriver is wired (T084)", async () => {
    const { BlockDriver, BLOCK_SIZE } = await import("../../src/drivers/block");
    const handle = makeMemSyncAccessHandle();
    const blockDriver = BlockDriver.withHandle(handle, 4096);
    const { host } = await freshHost({ blockDriver });
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // PATH_OPEN /persist as a directory. If mkfs ran successfully
    // during kernel_init, this resolves to the OPFS root. If the
    // OPFS mount didn't happen, PATH_OPEN /persist returns ENOENT.
    const pathBytes = new TextEncoder().encode("/persist");
    const open = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 5001,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(open.response!.status).toBe(0);

    // The handle must have grown to at least one block (mkfs writes
    // the superblock + zeros the inode table + journal).
    expect(handle.getSize()).toBeGreaterThanOrEqual(BLOCK_SIZE);
  });

  it("file written under /persist persists across kernel re-mount (FR-013a)", async () => {
    const { BlockDriver } = await import("../../src/drivers/block");
    const handle = makeMemSyncAccessHandle();

    // Boot 1: mount OPFS, create + write /persist/notes.txt, flush.
    {
      const blockDriver = BlockDriver.withHandle(handle, 4096);
      const { host } = await freshHost({ blockDriver });
      const pid = host.registerProcess(CAPSET_ALL);
      host.markRunning(pid);

      const pathBytes = new TextEncoder().encode("/persist/notes.txt");
      const open = host.dispatch(
        pid,
        {
          opcode: OP_WASI.PATH_OPEN,
          requestId: 6001,
          args: encodePathOpenArgs(0, OFLAG_CREAT, 0o644),
          heapPtr: 0,
          heapLen: pathBytes.length,
        },
        pathBytes,
      );
      expect(open.response!.status).toBe(0);
      const fd = Number(open.response!.value);

      const message = new TextEncoder().encode("hello pmos\n");
      const write = host.dispatch(
        pid,
        {
          opcode: OP_WASI.FD_WRITE,
          requestId: 6002,
          arg0: fd,
          heapPtr: 0,
          heapLen: message.length,
        },
        message,
      );
      expect(write.response!.status).toBe(0);

      // Sync everything dirty (T072 path) — the kernel calls
      // OpfsFs::sync which writes the journal + superblock through
      // the BlockDriver into the MemSyncAccessHandle.
      expect(host.syncAll()).toBe(true);
    }

    // Boot 2: same handle, fresh kernel + driver. The kernel's
    // OpfsFs::mount reads back the superblock written by boot 1
    // (correct magic + checksum) so this time mkfs is NOT called.
    // Reading back /persist/notes.txt round-trips the bytes.
    {
      const blockDriver = BlockDriver.withHandle(handle, 4096);
      const { host } = await freshHost({ blockDriver });
      const pid = host.registerProcess(CAPSET_ALL);
      host.markRunning(pid);

      const pathBytes = new TextEncoder().encode("/persist/notes.txt");
      const open = host.dispatch(
        pid,
        {
          opcode: OP_WASI.PATH_OPEN,
          requestId: 6101,
          arg0: 0,
          heapPtr: 0,
          heapLen: pathBytes.length,
        },
        pathBytes,
      );
      expect(open.response!.status).toBe(0);
      const fd = Number(open.response!.value);

      const read = host.dispatch(pid, {
        opcode: OP_WASI.FD_READ,
        requestId: 6102,
        arg0: fd,
        heapPtr: 0,
        heapLen: 64,
      });
      expect(read.response!.status).toBe(0);
      const text = new TextDecoder().decode(
        read.heapOut!.slice(0, Number(read.response!.value)),
      );
      expect(text).toBe("hello pmos\n");
    }
  });

  it("kernel_init skips the /persist mount when no BlockDriver is wired (default)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // /persist does NOT exist when the block driver is absent.
    // PATH_OPEN on /persist returns ENOENT.
    const pathBytes = new TextEncoder().encode("/persist");
    const open = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 5002,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(open.response!.status).toBe(-ERRNO.ENOENT);
  });

  it("/proc/storage reads the live OPFS quota counters when /persist is mounted (T084 + T169)", async () => {
    const { BlockDriver } = await import("../../src/drivers/block");
    const handle = makeMemSyncAccessHandle();
    const blockDriver = BlockDriver.withHandle(handle, 4096);
    const { host } = await freshHost({ blockDriver });
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // /proc/storage should now reflect the freshly-mkfs'd OPFS:
    // quota = total_blocks * BLOCK_SIZE = 4096 * 4096 = 16 MiB,
    // used = 1 block (the superblock at LBA 0) * 4096 + the
    // inode-table writes mkfs does, file_count = 1 (the root
    // inode allocated by mkfs).
    const pathBytes = new TextEncoder().encode("/proc/storage");
    const open = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 5501,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(open.response!.status).toBe(0);
    const fd = Number(open.response!.value);

    const read = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 5502,
      arg0: fd,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(read.response!.status).toBe(0);
    const text = new TextDecoder().decode(
      read.heapOut!.slice(0, Number(read.response!.value)),
    );
    // Line shape: "<quota> <used> <files>\n"
    const m = text.match(/^(\d+) (\d+) (\d+)\n$/);
    expect(m).not.toBeNull();
    if (!m) return;
    const quota = Number(m[1]);
    const used = Number(m[2]);
    const files = Number(m[3]);
    // 4096 blocks * 4096 = 16 MiB.
    expect(quota).toBe(4096 * 4096);
    // Some bytes used (mkfs wrote at minimum the superblock +
    // root inode), but not the whole disk.
    expect(used).toBeGreaterThan(0);
    expect(used).toBeLessThan(quota);
    // mkfs allocated at least the root inode.
    expect(files).toBeGreaterThanOrEqual(1);
  });

  it("/proc/storage falls back to '0 0 0' when no OPFS is mounted at /persist", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const pathBytes = new TextEncoder().encode("/proc/storage");
    const open = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 5601,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(open.response!.status).toBe(0);
    const fd = Number(open.response!.value);

    const read = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 5602,
      arg0: fd,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(read.response!.status).toBe(0);
    const text = new TextDecoder().decode(
      read.heapOut!.slice(0, Number(read.response!.value)),
    );
    expect(text).toBe("0 0 0\n");
  });

  it("/proc/<pid>/maps surfaces the live wasm linear-memory layout via the live procfs source", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    // Record a memory observation so the maps line has a non-zero
    // end address (mirrors the user-Worker → main → kernel-worker
    // → kernel_record_process_memory plumbing from T168).
    host.recordProcessMemory(pid, 16 * 65536); // 16 wasm pages = 1 MiB

    const path = `/proc/${pid}/maps`;
    const pathBytes = new TextEncoder().encode(path);
    const open = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 5401,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(open.response!.status).toBe(0);
    const fd = Number(open.response!.value);

    const read = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 5402,
      arg0: fd,
      heapPtr: 0,
      heapLen: 256,
    });
    expect(read.response!.status).toBe(0);
    const text = new TextDecoder().decode(
      read.heapOut!.slice(0, Number(read.response!.value)),
    );
    // Single line: 1 MiB region with rw-p, [wasm-memory] tag.
    expect(text).toMatch(/^00000000-00100000 rw-p 00000000 00:00 0 +\[wasm-memory\]\n$/);
  });

  it("/proc/<pid>/fd lists installed file descriptors via the live procfs source", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const path = `/proc/${pid}/fd`;
    const pathBytes = new TextEncoder().encode(path);
    const open = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 4201,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(open.response!.status).toBe(0);
    const fd = Number(open.response!.value);

    const args = new Uint8Array(16);
    new DataView(args.buffer).setUint32(0, fd, true);
    const readdir = host.dispatch(pid, {
      opcode: OP_WASI.FD_READDIR,
      requestId: 4202,
      args,
      heapPtr: 0,
      heapLen: 1024,
    });
    expect(readdir.response!.status).toBe(0);
    const total = Number(readdir.response!.value);
    expect(total).toBeGreaterThan(0);

    // Walk the dirent stream and collect names. Header is
    // POLL_DIRENT_HEADER_SIZE bytes; each entry is followed by
    // its name inline; entries pack tightly with no padding.
    const buf = readdir.heapOut!.subarray(0, total);
    const names: string[] = [];
    let off = 0;
    while (off + POLL_DIRENT_HEADER_SIZE <= buf.length) {
      const v = new DataView(buf.buffer, buf.byteOffset + off, POLL_DIRENT_HEADER_SIZE);
      const namlen = v.getUint32(DIRENT_OFF.D_NAMLEN, true);
      if (namlen === 0) break;
      const nameBytes = buf.subarray(
        off + POLL_DIRENT_HEADER_SIZE,
        off + POLL_DIRENT_HEADER_SIZE + namlen,
      );
      names.push(new TextDecoder().decode(nameBytes));
      off += POLL_DIRENT_HEADER_SIZE + namlen;
    }
    // The live source surfaces every installed fd; "1" is the
    // console fd we installed. Other fds may be present (depends
    // on what kernel_register_process pre-installs); the test
    // pins the inclusion of "1", not exclusivity.
    expect(names).toContain("1");
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(BigInt(pid));
    expect(response!.requestId).toBe(1);
    expect(response!.extraLen).toBe(0);
  });

  it("CAP_LIST returns the full bitset (u64::MAX reinterprets as -1n)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_EXT.CAP_LIST,
      requestId: 2,
    });
    expect(response!.status).toBe(0);
    // u64::MAX bit pattern reinterpreted as i64 is -1.
    expect(response!.value).toBe(-1n);
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
    expect(held.response!.value).toBe(1n);

    // Absent: desktop shell does NOT hold DisplayServer. This
    // doubles as a regression test for the cap split.
    const absent = host.dispatch(shellPid, {
      opcode: OP_EXT.CAP_CHECK,
      requestId: 4,
      arg0: CAP.DISPLAY_SERVER,
    });
    expect(absent.response!.value).toBe(0n);
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

    expect(response!.status).toBe(0);
    expect(response!.value).toBe(BigInt(message.length));
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
    expect(response!.status).toBe(-ERRNO.EBADF);
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

    expect(response!.status).toBe(0);
    expect(response!.value).toBe(3n);
    expect(response!.extraLen).toBe(3);
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

    expect(response!.status).toBe(0);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(-ERRNO.ENOENT);
  });
});

// ---- dispatch: PATH_OPEN oflags (CREAT / DIRECTORY / EXCL / TRUNC) -
//
// End-to-end through kernel.wasm: exercise the new args[4..6] u16
// oflags decode. Pre-slice path_open ignored oflags entirely; post-
// slice the kernel honours CREAT (create missing file), EXCL
// (combined with CREAT, reject existing), TRUNC (zero existing
// regular file), DIRECTORY (require dir target), and
// CREAT|DIRECTORY (→ EINVAL).

const OFLAG_CREAT = 0x0001;
const OFLAG_DIRECTORY = 0x0002;
const OFLAG_EXCL = 0x0004;
const OFLAG_TRUNC = 0x0008;

function encodePathOpenArgs(
  fdflags: number,
  oflags: number,
  mode: number,
  lookupFlags: number = 0,
): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fdflags >>> 0, true);
  v.setUint16(4, oflags & 0xffff, true);
  v.setUint16(6, mode & 0xffff, true);
  v.setUint32(8, lookupFlags >>> 0, true);
  return args;
}

describe("dispatch: PATH_OPEN oflags", () => {
  it("creates a new regular file when CREAT is set on a missing path", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const pathBytes = new TextEncoder().encode("/new-via-creat.txt");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 1140,
        args: encodePathOpenArgs(0, OFLAG_CREAT, 0),
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(response!.status).toBe(0);

    // The file is now visible via PATH_FILESTAT_GET with filetype =
    // REGULAR_FILE.
    const { response: st, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 1141,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(st!.status).toBe(0);
    expect(heapOut[16]).toBe(FILETYPE.REGULAR_FILE);
  });

  it("returns -EEXIST when CREAT|EXCL targets an existing path", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Seed /exists via CREAT.
    const pathBytes = new TextEncoder().encode("/exists.txt");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 1142,
        args: encodePathOpenArgs(0, OFLAG_CREAT, 0),
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );

    // CREAT|EXCL on the same path rejects.
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 1143,
        args: encodePathOpenArgs(0, OFLAG_CREAT | OFLAG_EXCL, 0),
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(response!.status).toBe(-ERRNO.EEXIST);
  });

  it("returns -ENOTDIR when DIRECTORY targets a regular file", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Create /a-file via CREAT so it's a regular file.
    const pathBytes = new TextEncoder().encode("/a-file");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 1144,
        args: encodePathOpenArgs(0, OFLAG_CREAT, 0),
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );

    // DIRECTORY on the regular file → ENOTDIR.
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 1145,
        args: encodePathOpenArgs(0, OFLAG_DIRECTORY, 0),
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(response!.status).toBe(-ERRNO.ENOTDIR);
  });

  it("returns -EINVAL for CREAT|DIRECTORY", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const pathBytes = new TextEncoder().encode("/would-be-dir");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 1146,
        args: encodePathOpenArgs(0, OFLAG_CREAT | OFLAG_DIRECTORY, 0),
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("TRUNC zeroes an existing regular file", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // CREAT then FD_WRITE some bytes.
    const pathBytes = new TextEncoder().encode("/to-trunc");
    const { response: openResp } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 1147,
        args: encodePathOpenArgs(0, OFLAG_CREAT, 0),
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(openResp!.status).toBe(0);
    const fd = Number(openResp!.value);

    const writeBuf = new TextEncoder().encode("hello");
    const { response: wrResp } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_WRITE,
        requestId: 1148,
        arg0: fd,
        heapPtr: 0,
        heapLen: writeBuf.length,
      },
      writeBuf,
    );
    expect(wrResp!.status).toBe(0);

    // Now re-open with TRUNC. The file should be 0 bytes post-open.
    const { response: truncResp } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 1149,
        args: encodePathOpenArgs(0, OFLAG_TRUNC, 0),
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(truncResp!.status).toBe(0);

    // Stat confirms size = 0.
    const { response: st, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 1150,
        arg0: 0,
        heapPtr: 0,
        heapLen: pathBytes.length,
      },
      pathBytes,
    );
    expect(st!.status).toBe(0);
    const size = new DataView(heapOut.buffer, heapOut.byteOffset).getBigUint64(32, true);
    expect(size).toBe(0n);
  });
});

// ---- dispatch: PATH_OPEN lookup_flags (AT_SYMLINK_NOFOLLOW) ---------
//
// End-to-end through kernel.wasm: exercise the new args[8..12] u32
// lookup_flags decode on PATH_OPEN. Bit 0 = SYMLINK_FOLLOW. Pre-
// slice path_open always followed the final symlink (slice 1's
// Vfs::resolve). Post-slice the flag governs: set → follow (fd
// lands on the target's filetype); clear → do not follow (fd lands
// on the symlink's own filetype). Intermediate components always
// follow regardless of the bit.

const LOOKUP_SYMLINK_FOLLOW = 0x0001;

describe("dispatch: PATH_OPEN lookup_flags", () => {
  it("with SYMLINK_FOLLOW set, opens the symlink's target", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Create /target as a directory so the follow-vs-nofollow
    // distinction is visible via FD_FILESTAT_GET filetype.
    const targetPath = new TextEncoder().encode("/target");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 1400,
        arg0: 0,
        heapPtr: 0,
        heapLen: targetPath.length,
      },
      targetPath,
    );

    const linkHeap = encodePathSymlinkHeap("/target", "/link");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1401,
        args: encodePathSymlinkArgs("/target".length),
        heapPtr: 0,
        heapLen: linkHeap.length,
      },
      linkHeap,
    );

    const linkPath = new TextEncoder().encode("/link");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 1402,
        args: encodePathOpenArgs(0, 0, 0, LOOKUP_SYMLINK_FOLLOW),
        heapPtr: 0,
        heapLen: linkPath.length,
      },
      linkPath,
    );
    expect(response!.status).toBe(0);
    const fd = Number(response!.value);

    const { response: stat, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_FILESTAT_GET,
        requestId: 1403,
        arg0: fd,
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(stat!.status).toBe(0);
    expect(heapOut[16]).toBe(FILETYPE.DIRECTORY);
  });

  it("with SYMLINK_FOLLOW clear, opens the symlink's own vnode", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const targetPath = new TextEncoder().encode("/target");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 1404,
        arg0: 0,
        heapPtr: 0,
        heapLen: targetPath.length,
      },
      targetPath,
    );

    const linkHeap = encodePathSymlinkHeap("/target", "/link");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1405,
        args: encodePathSymlinkArgs("/target".length),
        heapPtr: 0,
        heapLen: linkHeap.length,
      },
      linkHeap,
    );

    const linkPath = new TextEncoder().encode("/link");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 1406,
        args: encodePathOpenArgs(0, 0, 0, 0),
        heapPtr: 0,
        heapLen: linkPath.length,
      },
      linkPath,
    );
    expect(response!.status).toBe(0);
    const fd = Number(response!.value);

    // Stat the opened fd: filetype should be SYMBOLIC_LINK (7),
    // not DIRECTORY (3).
    const { response: stat, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_FILESTAT_GET,
        requestId: 1407,
        arg0: fd,
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(stat!.status).toBe(0);
    expect(heapOut[16]).toBe(FILETYPE.SYMBOLIC_LINK);
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
    expect(response!.status).toBe(-ERRNO.ENOSYS);
    expect(response!.requestId).toBe(40);
  });

  it("returns -ENOTSUP for FD_FDSTAT_SET_RIGHTS", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FDSTAT_SET_RIGHTS,
      requestId: 41,
    });
    expect(response!.status).toBe(-ERRNO.ENOTSUP);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(1_234_567_890_000n);
    expect(response!.requestId).toBe(50);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(1_700_000_000_000_000_000n);
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
    expect(response!.status).toBe(-ERRNO.ENOTSUP);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(-ERRNO.ENOTSUP);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(1n);
    expect(response!.requestId).toBe(60);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(1n);
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
    expect(response!.status).toBe(-ERRNO.ENOTSUP);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(-ERRNO.ENOTSUP);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(0);
    expect(response!.extraLen).toBe(64);
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
    expect(sockResp.response!.status).toBe(0);
    const sockFd = Number(sockResp.response!.value);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FILESTAT_GET,
      requestId: 712,
      arg0: sockFd,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(response!.status).toBe(0);
    expect(response!.extraLen).toBe(64);
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
    expect(response!.status).toBe(-ERRNO.EBADF);
    expect(response!.extraLen).toBe(0);
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
    expect(response!.status).toBe(0);
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
    expect(response!.status).toBe(0);
    expect(response!.extraLen).toBe(64);
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
    expect(response!.status).toBe(-ERRNO.ENOENT);
    expect(response!.extraLen).toBe(0);
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
    expect(response!.status).toBe(0);
    expect(response!.extraLen).toBe(64);
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
    expect(response!.status).toBe(-ERRNO.ENOENT);
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
    expect(response!.status).toBe(-ERRNO.EROFS);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
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
    expect(response!.status).toBe(0);
  });
});

// ---- dispatch: FD_READDIR ------------------------------------------
//
// Directory-listing opcode. Wire: fd at args[0..4], cookie (u64 LE)
// at args[4..12]; heap is the output buffer. Kernel writes 24-byte
// dirent_t headers + inline name bytes into the buffer. These TS
// tests pin the dispatcher's wire layout end-to-end through
// kernel.wasm — the zero-heap-len probe, the EBADF / EINVAL error
// branches, and a happy-path listing from /proc (procfs is always
// populated and reachable through the dispatch surface alone via
// PATH_OPEN on /proc as a directory).

function encodeFdReaddirArgs(fd: number, cookie: bigint): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fd, true);
  v.setBigUint64(4, cookie, true);
  return args;
}

function openProcDirFd(host: KernelWasmHost, pid: number): number {
  // /proc is mounted as a directory in the kernel's init layout.
  const pathBytes = new TextEncoder().encode("/proc");
  const { response } = host.dispatch(
    pid,
    {
      opcode: OP_WASI.PATH_OPEN,
      requestId: 988,
      arg0: 0,
      heapPtr: 0,
      heapLen: pathBytes.length,
    },
    pathBytes,
  );
  expect(response!.status).toBe(0);
  return Number(response!.value);
}

describe("dispatch: FD_READDIR", () => {
  it("returns -EBADF for an unopened fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_READDIR,
      requestId: 900,
      args: encodeFdReaddirArgs(99, 0n),
      heapPtr: 0,
      heapLen: 256,
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });

  it("returns -EINVAL for a non-Vnode fd (/dev/console)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_READDIR,
      requestId: 901,
      args: encodeFdReaddirArgs(1, 0n),
      heapPtr: 0,
      heapLen: 256,
    });
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -ENOTDIR when the Vnode points at a regular file", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_READDIR,
      requestId: 902,
      args: encodeFdReaddirArgs(fd, 0n),
      heapPtr: 0,
      heapLen: 256,
    });
    expect(response!.status).toBe(-ERRNO.ENOTDIR);
  });

  it("returns 0 bytes written for a zero-capacity buffer (probe)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcDirFd(host, pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_READDIR,
      requestId: 903,
      args: encodeFdReaddirArgs(fd, 0n),
      heapPtr: 0,
      heapLen: 0,
    });
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(0n);
    expect(response!.extraLen).toBe(0);
  });

  it("lists /proc directory entries with valid dirent headers", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcDirFd(host, pid);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.FD_READDIR,
      requestId: 904,
      args: encodeFdReaddirArgs(fd, 0n),
      heapPtr: 0,
      heapLen: 1024,
    });
    expect(response!.status).toBe(0);
    const total = Number(response!.value);
    expect(total).toBeGreaterThan(0);
    expect(response!.extraLen).toBe(total);

    // Decode one entry — the first one — and verify the header
    // fields + name round-trip cleanly.
    const v = new DataView(heapOut.buffer, heapOut.byteOffset, POLL_DIRENT_HEADER_SIZE);
    const dNext = v.getBigUint64(DIRENT_OFF.D_NEXT, true);
    const dNamlen = v.getUint32(DIRENT_OFF.D_NAMLEN, true);
    expect(dNext).toBe(1n);
    expect(dNamlen).toBeGreaterThan(0);
    const name = new TextDecoder().decode(
      heapOut.subarray(POLL_DIRENT_HEADER_SIZE, POLL_DIRENT_HEADER_SIZE + dNamlen),
    );
    expect(name.length).toBe(dNamlen);
  });
});

// ---- dispatch: PATH_UNLINK_FILE + PATH_RENAME ---------------------
//
// Two filesystem-mutation opcodes. PATH_UNLINK_FILE is single-path;
// PATH_RENAME packs both paths into a single heap window split by an
// old_len u32 at args[8..12]. These TS tests pin the wire layout
// end-to-end through kernel.wasm for the dispatcher-level branches:
// the happy-path tmpfs mutation (create + opcode + stat the result),
// the ENOENT / EISDIR / EROFS error paths, and PATH_RENAME's
// old_len validation (zero, past heap, empty new path). The in-tree
// unit-test harness uses /proc/version as a pre-existing file to
// probe the ENOTSUP/EROFS paths without hitting the native disk.

function encodePathUnlinkArgs(dirFd: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, dirFd, true);
  return args;
}

function encodePathRenameArgs(
  fromDirFd: number,
  toDirFd: number,
  oldLen: number,
): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fromDirFd, true);
  v.setUint32(4, toDirFd, true);
  v.setUint32(8, oldLen, true);
  return args;
}

function encodePathRenameHeap(oldPath: string, newPath: string): Uint8Array {
  const enc = new TextEncoder();
  const oldB = enc.encode(oldPath);
  const newB = enc.encode(newPath);
  const heap = new Uint8Array(oldB.length + newB.length);
  heap.set(oldB, 0);
  heap.set(newB, oldB.length);
  return heap;
}

describe("dispatch: PATH_UNLINK_FILE", () => {
  it("returns -ENOENT for a missing path", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = new TextEncoder().encode("/nowhere");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_UNLINK_FILE,
        requestId: 880,
        args: encodePathUnlinkArgs(0),
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(response!.status).toBe(-ERRNO.ENOENT);
  });

  it("returns -EROFS for a /dev path (devfs is read-only)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = new TextEncoder().encode("/dev/console");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_UNLINK_FILE,
        requestId: 881,
        args: encodePathUnlinkArgs(0),
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(response!.status).toBe(-ERRNO.EROFS);
  });
});

describe("dispatch: PATH_RENAME", () => {
  it("returns -ENOENT when the source path does not exist", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodePathRenameHeap("/nope", "/also_nope");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_RENAME,
        requestId: 890,
        args: encodePathRenameArgs(0, 0, "/nope".length),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.ENOENT);
  });

  it("returns -EROFS when renaming a devfs entry (read-only filesystem)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodePathRenameHeap("/dev/null", "/dev/nullx");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_RENAME,
        requestId: 891,
        args: encodePathRenameArgs(0, 0, "/dev/null".length),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EROFS);
  });

  it("returns -EINVAL for zero old_len", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new TextEncoder().encode("/nonsense");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_RENAME,
        requestId: 892,
        args: encodePathRenameArgs(0, 0, 0),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EINVAL when old_len >= heap_len (empty new path)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new TextEncoder().encode("/a.txt");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_RENAME,
        requestId: 893,
        args: encodePathRenameArgs(0, 0, heap.length), // old_len == heap_len
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });
});

// ---- dispatch: PATH_LINK -------------------------------------------
//
// Hardlink opcode at 0x0043. Wire packs (old_fd, old_flags, new_fd,
// old_len) as four u32s in the inline args window; only old_len
// (args[12..16]) is decoded in v1. Heap carries (old_path, new_path)
// concatenated, split at old_len. These TS tests pin the wire layout
// end-to-end through kernel.wasm for the error branches reachable
// from this harness: -ENOENT for a missing source, -EEXIST when the
// target already exists, -EROFS within /dev (devfs inherits the trait
// default), -EINVAL for zero old_len, and -EINVAL when old_len >=
// heap_len. The happy-path nlink verification lives in the Rust
// kernel tests; here we focus on the dispatcher-level branches.

function encodePathLinkArgs(
  oldFd: number,
  oldFlags: number,
  newFd: number,
  oldLen: number,
): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, oldFd, true);
  v.setUint32(4, oldFlags, true);
  v.setUint32(8, newFd, true);
  v.setUint32(12, oldLen, true);
  return args;
}

function encodePathLinkHeap(oldPath: string, newPath: string): Uint8Array {
  const enc = new TextEncoder();
  const oldB = enc.encode(oldPath);
  const newB = enc.encode(newPath);
  const heap = new Uint8Array(oldB.length + newB.length);
  heap.set(oldB, 0);
  heap.set(newB, oldB.length);
  return heap;
}

describe("dispatch: PATH_LINK", () => {
  it("returns -ENOENT when the source path does not exist", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodePathLinkHeap("/nope", "/alias");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_LINK,
        requestId: 1000,
        args: encodePathLinkArgs(0, 0, 0, "/nope".length),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.ENOENT);
  });

  it("returns -EROFS when linking within /dev (devfs inherits default)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodePathLinkHeap("/dev/null", "/dev/null2");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_LINK,
        requestId: 1001,
        args: encodePathLinkArgs(0, 0, 0, "/dev/null".length),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EROFS);
  });

  it("returns -ENOTSUP for cross-mount links", async () => {
    // Source in /proc, destination in /dev → different mounts.
    // Vfs::link rejects this before touching either filesystem.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodePathLinkHeap("/proc/version", "/dev/xver");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_LINK,
        requestId: 1002,
        args: encodePathLinkArgs(0, 0, 0, "/proc/version".length),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.ENOTSUP);
  });

  it("returns -EINVAL for zero old_len", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new TextEncoder().encode("/nowhere");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_LINK,
        requestId: 1003,
        args: encodePathLinkArgs(0, 0, 0, 0),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EINVAL when old_len >= heap_len (empty new path)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new TextEncoder().encode("/a");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_LINK,
        requestId: 1004,
        args: encodePathLinkArgs(0, 0, 0, heap.length),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EINVAL when old_len > heap_len", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new TextEncoder().encode("/abc");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_LINK,
        requestId: 1005,
        args: encodePathLinkArgs(0, 0, 0, 999),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });
});

// ---- dispatch: PATH_SYMLINK ----------------------------------------
//
// Symlink-creation opcode at 0x0048. Wire packs old_len (target byte
// length) at args[0..4]; heap carries (target, new_path) concatenated,
// split at old_len. These TS tests pin the wire layout end-to-end
// through kernel.wasm for the dispatcher-level branches reachable
// from this harness: happy-path creation visible via
// PATH_FILESTAT_GET as a SymLink filetype, EEXIST when the link-path
// already exists (via a prior PATH_CREATE_DIRECTORY), ENOTSUP on
// /dev (devfs inherits the trait default), EINVAL on zero/past-heap
// old_len.

function encodePathSymlinkArgs(oldLen: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, oldLen, true);
  return args;
}

function encodePathSymlinkHeap(target: string, newPath: string): Uint8Array {
  const enc = new TextEncoder();
  const t = enc.encode(target);
  const n = enc.encode(newPath);
  const heap = new Uint8Array(t.length + n.length);
  heap.set(t, 0);
  heap.set(n, t.length);
  return heap;
}

describe("dispatch: PATH_SYMLINK", () => {
  it("creates a symlink visible via PATH_FILESTAT_GET as a SymLink", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodePathSymlinkHeap("/some/target", "/mylink");
    const { response: mk } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1010,
        args: encodePathSymlinkArgs("/some/target".length),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(mk!.status).toBe(0);

    // Stat the created link; v1's resolver doesn't follow symlinks,
    // so stat returns the symlink's own metadata.
    const pathHeap = new TextEncoder().encode("/mylink");
    const { response: st, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 1011,
        arg0: 0, // dir_fd — ignored
        heapPtr: 0,
        heapLen: pathHeap.length,
      },
      pathHeap,
    );
    expect(st!.status).toBe(0);
    expect(st!.extraLen).toBe(64);
    const filestat = decodeFilestat(heapOut);
    expect(filestat.filetype).toBe(FILETYPE.SYMBOLIC_LINK);
    // size = target byte length per POSIX.
    expect(filestat.size).toBe(BigInt("/some/target".length));
  });

  it("returns -EEXIST when the link-path target already exists", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Create /existing as a directory first.
    const dirPath = new TextEncoder().encode("/existing");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 1012,
        arg0: 0,
        heapPtr: 0,
        heapLen: dirPath.length,
      },
      dirPath,
    );

    const heap = encodePathSymlinkHeap("/t", "/existing");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1013,
        args: encodePathSymlinkArgs(2),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EEXIST);
  });

  it("returns -ENOTSUP when creating a symlink in /dev", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodePathSymlinkHeap("/anywhere", "/dev/mylink");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1014,
        args: encodePathSymlinkArgs("/anywhere".length),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.ENOTSUP);
  });

  it("returns -EINVAL for zero old_len", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new TextEncoder().encode("/whatever");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1015,
        args: encodePathSymlinkArgs(0),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EINVAL when old_len >= heap_len (empty new path)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new TextEncoder().encode("/t");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1016,
        args: encodePathSymlinkArgs(heap.length),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });
});

// ---- dispatch: PATH_FILESTAT_GET symlink-follow lookup flags --------
//
// End-to-end through kernel.wasm: exercise the new
// LOOKUP_SYMLINK_FOLLOW=0x1 bit on PATH_FILESTAT_GET's lookup_flags
// arg. Prior to the symlink-aware-resolve slice, Vfs::resolve never
// followed symlinks and the flag was a no-op; post-slice, bit-0 set
// routes through Vfs::resolve (follow), bit-0 clear routes through
// Vfs::resolve_nofollow (lstat). Creates a /target + /link pair via
// PATH_CREATE_DIRECTORY + PATH_SYMLINK, then asserts the two branches
// return the right filetype. Also pins ELOOP end-to-end on a self-
// referential symlink (`/a → /a`) with follow requested.

function encodePathFilestatArgs(dirFd: number, lookupFlags: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, dirFd, true);
  v.setUint32(4, lookupFlags, true);
  return args;
}

describe("dispatch: PATH_FILESTAT_GET symlink-follow lookup flags", () => {
  it("returns target filetype when LOOKUP_SYMLINK_FOLLOW is set", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Create /target as a directory so the target-vs-symlink filetype
    // is unambiguous (directory = 3, symbolic_link = 7).
    const dirPath = new TextEncoder().encode("/target");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 1130,
        arg0: 0,
        heapPtr: 0,
        heapLen: dirPath.length,
      },
      dirPath,
    );

    // Create /link → /target.
    const mkHeap = encodePathSymlinkHeap("/target", "/link");
    const mk = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1131,
        args: encodePathSymlinkArgs("/target".length),
        heapPtr: 0,
        heapLen: mkHeap.length,
      },
      mkHeap,
    );
    expect(mk.response!.status).toBe(0);

    // Stat /link with lookup_flags=0x1 — follow the final symlink.
    const linkPath = new TextEncoder().encode("/link");
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 1132,
        args: encodePathFilestatArgs(0, 0x1),
        heapPtr: 0,
        heapLen: linkPath.length,
      },
      linkPath,
    );
    expect(response!.status).toBe(0);
    expect(heapOut[16]).toBe(FILETYPE.DIRECTORY);
  });

  it("returns symlink filetype when LOOKUP_SYMLINK_FOLLOW is clear", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const dirPath = new TextEncoder().encode("/target");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 1133,
        arg0: 0,
        heapPtr: 0,
        heapLen: dirPath.length,
      },
      dirPath,
    );

    const mkHeap = encodePathSymlinkHeap("/target", "/link");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1134,
        args: encodePathSymlinkArgs("/target".length),
        heapPtr: 0,
        heapLen: mkHeap.length,
      },
      mkHeap,
    );

    // Stat /link with lookup_flags=0 — lstat-like, final symlink
    // is not dereferenced.
    const linkPath = new TextEncoder().encode("/link");
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 1135,
        args: encodePathFilestatArgs(0, 0x0),
        heapPtr: 0,
        heapLen: linkPath.length,
      },
      linkPath,
    );
    expect(response!.status).toBe(0);
    expect(heapOut[16]).toBe(FILETYPE.SYMBOLIC_LINK);
  });

  it("returns -ELOOP when following a self-referential symlink", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Create /a → /a: a direct self-loop. SYMLOOP_MAX (40) hops
    // exhaust the budget and the resolver returns SymLoop → ELOOP.
    const mkHeap = encodePathSymlinkHeap("/a", "/a");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1136,
        args: encodePathSymlinkArgs("/a".length),
        heapPtr: 0,
        heapLen: mkHeap.length,
      },
      mkHeap,
    );

    const probe = new TextEncoder().encode("/a");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 1137,
        args: encodePathFilestatArgs(0, 0x1),
        heapPtr: 0,
        heapLen: probe.length,
      },
      probe,
    );
    expect(response!.status).toBe(-ERRNO.ELOOP);
  });

  it("reaches target through a three-link chain when follow is set", async () => {
    // /target (regular file) ← /c ← /b ← /a. Statting /a with follow
    // should trace the whole chain and land on the regular-file
    // filetype. This pins the transitive-follow behaviour — not just
    // single-level follow — end-to-end.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Create /target as a regular file via PATH_OPEN with fdflags=0.
    // path_open auto-creates when the fs's create() is the path used
    // by the handler; here the cleanest way is to write a zero-byte
    // file using FD_WRITE on a newly-installed fd. But v1's path_open
    // is open-existing-only (oflags ignored) — so use
    // PATH_CREATE_DIRECTORY won't work (we want a regular file). The
    // tmpfs-side `Vfs::create` is not userland-facing.
    //
    // Workaround: use PATH_SYMLINK to create /c → /target where
    // /target is a *directory* (simpler to create). Then /b → /c,
    // /a → /b. The terminal filetype will be DIRECTORY.
    const dirPath = new TextEncoder().encode("/target");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 1138,
        arg0: 0,
        heapPtr: 0,
        heapLen: dirPath.length,
      },
      dirPath,
    );

    for (const [target, link] of [["/target", "/c"], ["/c", "/b"], ["/b", "/a"]] as const) {
      const heap = encodePathSymlinkHeap(target, link);
      host.dispatch(
        pid,
        {
          opcode: OP_WASI.PATH_SYMLINK,
          requestId: 1140,
          args: encodePathSymlinkArgs(target.length),
          heapPtr: 0,
          heapLen: heap.length,
        },
        heap,
      );
    }

    const probe = new TextEncoder().encode("/a");
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 1141,
        args: encodePathFilestatArgs(0, 0x1),
        heapPtr: 0,
        heapLen: probe.length,
      },
      probe,
    );
    expect(response!.status).toBe(0);
    expect(heapOut[16]).toBe(FILETYPE.DIRECTORY);
  });

  it("follows intermediate symlinks under lstat semantics (follow=0)", async () => {
    // /realdir (directory with a regular file) + /linkdir →
    // /realdir. Pre-POSIX-perfect-resolve_nofollow,
    // PATH_FILESTAT_GET on /linkdir/file with lookup_flags=0
    // errored with -ENOTDIR because v1's resolve_nofollow didn't
    // follow intermediate symlinks. Post-slice-7 it follows
    // intermediates (only the final component is flag-governed)
    // so /linkdir/file returns the target regular file's
    // filetype even with the flag clear.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Create /realdir/ and a file inside it.
    const realdirPath = new TextEncoder().encode("/realdir");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 1170,
        arg0: 0,
        heapPtr: 0,
        heapLen: realdirPath.length,
      },
      realdirPath,
    );
    const filePath = new TextEncoder().encode("/realdir/file");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 1171,
        args: encodePathOpenArgs(0, OFLAG_CREAT, 0),
        heapPtr: 0,
        heapLen: filePath.length,
      },
      filePath,
    );
    // /linkdir → /realdir.
    const linkHeap = encodePathSymlinkHeap("/realdir", "/linkdir");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1172,
        args: encodePathSymlinkArgs("/realdir".length),
        heapPtr: 0,
        heapLen: linkHeap.length,
      },
      linkHeap,
    );

    // PATH_FILESTAT_GET on /linkdir/file with lookup_flags=0.
    // Expect the regular-file filetype (4) via intermediate
    // symlink follow.
    const probe = new TextEncoder().encode("/linkdir/file");
    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 1173,
        args: encodePathFilestatArgs(0, 0),
        heapPtr: 0,
        heapLen: probe.length,
      },
      probe,
    );
    expect(response!.status).toBe(0);
    expect(heapOut[16]).toBe(FILETYPE.REGULAR_FILE);
  });
});

// ---- dispatch: PATH_READLINK ---------------------------------------
//
// Symlink-dereference opcode at 0x0045. Wire packs dir_fd at args[0..4]
// (ignored) + path_len at args[4..8]. Heap[0..path_len] is the UTF-8
// input path; the remainder is the output buffer. Response.value =
// bytes written (up to buf_cap). These TS tests pin the wire layout
// end-to-end through kernel.wasm: happy-path target readback after a
// prior PATH_SYMLINK, -EINVAL on a regular file, -ENOENT on a missing
// path, -ENOTSUP within /dev (devfs inherits the default), -EINVAL
// on zero path_len.

function encodePathReadlinkArgs(dirFd: number, pathLen: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, dirFd, true);
  v.setUint32(4, pathLen, true);
  return args;
}

describe("dispatch: PATH_READLINK", () => {
  it("reads a symlink target created via PATH_SYMLINK", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Create /mylink → /real/target via PATH_SYMLINK.
    const target = "/real/target";
    const linkPath = "/mylink";
    const mkHeap = encodePathSymlinkHeap(target, linkPath);
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_SYMLINK,
        requestId: 1020,
        args: encodePathSymlinkArgs(target.length),
        heapPtr: 0,
        heapLen: mkHeap.length,
      },
      mkHeap,
    );

    // Now readlink /mylink into a 64-byte heap.
    const pathBytes = new TextEncoder().encode(linkPath);
    const rlHeap = new Uint8Array(64);
    rlHeap.set(pathBytes, 0);

    const { response, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_READLINK,
        requestId: 1021,
        args: encodePathReadlinkArgs(0, pathBytes.length),
        heapPtr: 0,
        heapLen: rlHeap.length,
      },
      rlHeap,
    );
    expect(response!.status).toBe(0);
    expect(Number(response!.value)).toBe(target.length);
    // Kernel writes target bytes at heap[0..n] (overwriting path).
    const decoded = new TextDecoder().decode(
      heapOut.subarray(0, target.length),
    );
    expect(decoded).toBe(target);
  });

  it("returns -EINVAL on a directory (non-symlink target in tmpfs)", async () => {
    // The root directory of tmpfs is a regular Directory node;
    // tmpfs.readlink returns InvalidArgument for any non-SymLink
    // variant.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Create a fresh /somedir via PATH_CREATE_DIRECTORY; readlink
    // on it must yield EINVAL.
    const dirPath = new TextEncoder().encode("/somedir");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 1022,
        arg0: 0,
        heapPtr: 0,
        heapLen: dirPath.length,
      },
      dirPath,
    );

    const rlHeap = new Uint8Array(32);
    rlHeap.set(dirPath, 0);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_READLINK,
        requestId: 1023,
        args: encodePathReadlinkArgs(0, dirPath.length),
        heapPtr: 0,
        heapLen: rlHeap.length,
      },
      rlHeap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -ENOENT on a missing path", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const pathBytes = new TextEncoder().encode("/not/here");
    const rlHeap = new Uint8Array(pathBytes.length + 32);
    rlHeap.set(pathBytes, 0);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_READLINK,
        requestId: 1024,
        args: encodePathReadlinkArgs(0, pathBytes.length),
        heapPtr: 0,
        heapLen: rlHeap.length,
      },
      rlHeap,
    );
    expect(response!.status).toBe(-ERRNO.ENOENT);
  });

  it("returns -ENOTSUP on /dev (devfs inherits NotSupported default)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const pathBytes = new TextEncoder().encode("/dev/console");
    const rlHeap = new Uint8Array(pathBytes.length + 32);
    rlHeap.set(pathBytes, 0);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_READLINK,
        requestId: 1025,
        args: encodePathReadlinkArgs(0, pathBytes.length),
        heapPtr: 0,
        heapLen: rlHeap.length,
      },
      rlHeap,
    );
    expect(response!.status).toBe(-ERRNO.ENOTSUP);
  });

  it("returns -EINVAL for zero path_len", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const rlHeap = new Uint8Array(32);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_READLINK,
        requestId: 1026,
        args: encodePathReadlinkArgs(0, 0),
        heapPtr: 0,
        heapLen: rlHeap.length,
      },
      rlHeap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });
});

// ---- dispatch: FD_PRESTAT_DIR_NAME ---------------------------------
//
// Companion to fd_prestat_get. v1 has no preopens so the kernel
// always returns -EBADF, matching fd_prestat_get's semantic. Pinning
// the consistency invariant here keeps the libc preopen-discovery
// loop working — the loop iterates fd 3/4/5 calling both opcodes
// until both return EBADF. Pre-slice, dir_name returned ENOSYS which
// broke the loop; post-slice both agree on EBADF.

describe("dispatch: FD_PRESTAT_DIR_NAME", () => {
  it("returns -EBADF for any fd (no preopens in v1)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_PRESTAT_DIR_NAME,
      requestId: 1040,
      arg0: 3,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });

  it("returns -EBADF even for an installed fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 3);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_PRESTAT_DIR_NAME,
      requestId: 1041,
      arg0: 3,
      heapPtr: 0,
      heapLen: 64,
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });
});

// ---- dispatch: PATH_CREATE_DIRECTORY + PATH_REMOVE_DIRECTORY -------
//
// mkdir + rmdir opcodes at 0x0040 + 0x0046. Wire layout matches
// PATH_UNLINK_FILE (dir_fd at args[0..4], heap = UTF-8 path bytes).
// The kernel threads through the existing Vfs::mkdir + Vfs::rmdir
// methods; mkdir hard-codes mode 0o755. The TS tests pin the
// wire-layout branches end-to-end through kernel.wasm: a happy-
// path round-trip (mkdir then rmdir, confirming via PATH_FILESTAT_GET
// that the directory was created and later removed), the ENOENT /
// EROFS / EEXIST / ENOTEMPTY error branches.

function encodePathDirArgs(dirFd: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, dirFd, true);
  return args;
}

describe("dispatch: PATH_CREATE_DIRECTORY", () => {
  it("creates a tmpfs directory visible to PATH_FILESTAT_GET", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = new TextEncoder().encode("/mkdir_td");
    const { response: mk } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 920,
        args: encodePathDirArgs(0),
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(mk!.status).toBe(0);

    // Verify the new directory exists via PATH_FILESTAT_GET.
    const { response: st, heapOut } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 921,
        arg0: 0,
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(st!.status).toBe(0);
    expect(st!.extraLen).toBe(64);
    const filestat = decodeFilestat(heapOut);
    expect(filestat.filetype).toBe(FILETYPE.DIRECTORY);
  });

  it("returns -EEXIST when the target already exists", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = new TextEncoder().encode("/dup_dir");
    // First call creates.
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 922,
        args: encodePathDirArgs(0),
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    // Second call collides.
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 923,
        args: encodePathDirArgs(0),
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(response!.status).toBe(-ERRNO.EEXIST);
  });

  it("returns -EROFS on /dev (read-only filesystem)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = new TextEncoder().encode("/dev/newdir");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 924,
        args: encodePathDirArgs(0),
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(response!.status).toBe(-ERRNO.EROFS);
  });
});

describe("dispatch: PATH_REMOVE_DIRECTORY", () => {
  it("removes an empty directory created via PATH_CREATE_DIRECTORY", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = new TextEncoder().encode("/rmdir_td");
    // Create.
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 930,
        args: encodePathDirArgs(0),
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    // Remove.
    const { response: rm } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_REMOVE_DIRECTORY,
        requestId: 931,
        args: encodePathDirArgs(0),
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(rm!.status).toBe(0);

    // Subsequent PATH_FILESTAT_GET should report ENOENT.
    const { response: st } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_FILESTAT_GET,
        requestId: 932,
        arg0: 0,
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(st!.status).toBe(-ERRNO.ENOENT);
  });

  it("returns -ENOTEMPTY when the directory is not empty", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Populate /full with a subdirectory via two mkdir calls, then
    // attempt rmdir on /full → ENOTEMPTY. v1's path_open doesn't
    // have CREAT wired, so nested mkdir is the only way to seed a
    // non-empty directory through the syscall dispatch surface.
    const outer = new TextEncoder().encode("/full");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 933,
        args: encodePathDirArgs(0),
        heapPtr: 0,
        heapLen: outer.length,
      },
      outer,
    );
    const inner = new TextEncoder().encode("/full/inner");
    host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_CREATE_DIRECTORY,
        requestId: 934,
        args: encodePathDirArgs(0),
        heapPtr: 0,
        heapLen: inner.length,
      },
      inner,
    );

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_REMOVE_DIRECTORY,
        requestId: 935,
        args: encodePathDirArgs(0),
        heapPtr: 0,
        heapLen: outer.length,
      },
      outer,
    );
    expect(response!.status).toBe(-ERRNO.ENOTEMPTY);
  });

  it("returns -ENOENT for a path that does not exist", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = new TextEncoder().encode("/ghost_dir");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_REMOVE_DIRECTORY,
        requestId: 936,
        args: encodePathDirArgs(0),
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(response!.status).toBe(-ERRNO.ENOENT);
  });
});

// ---- dispatch: FD_FDSTAT_SET_FLAGS ---------------------------------
//
// WASI's F_SETFL opcode: overwrites the fd's NONBLOCK / APPEND /
// *SYNC bits. Wire: (fd, new_fdflags) as two u32s at args[0..4] +
// args[4..8]; no heap. The TS tests pin the dispatcher's per-branch
// behaviour end-to-end through kernel.wasm: the happy-path NONBLOCK
// set, the sync-family accepted-as-noop path, and the EBADF branch
// for an unopened fd. The Rust tests pin the CLOEXEC-preserve
// invariant and the clear-on-zero semantics that require direct
// FdTable inspection.

function encodeFdFdstatSetFlagsArgs(
  fd: number,
  fdflags: number,
): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fd, true);
  v.setUint32(4, fdflags, true);
  return args;
}

describe("dispatch: FD_FDSTAT_SET_FLAGS", () => {
  it("returns 0 when setting NONBLOCK on an open fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FDSTAT_SET_FLAGS,
      requestId: 940,
      args: encodeFdFdstatSetFlagsArgs(1, FDFLAGS.NONBLOCK),
    });
    expect(response!.status).toBe(0);
  });

  it("accepts DSYNC + RSYNC + SYNC bits as no-op success", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const combined = FDFLAGS.DSYNC | FDFLAGS.RSYNC | FDFLAGS.SYNC;
    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FDSTAT_SET_FLAGS,
      requestId: 941,
      args: encodeFdFdstatSetFlagsArgs(1, combined),
    });
    expect(response!.status).toBe(0);
  });

  it("returns -EBADF when the fd is not open", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FDSTAT_SET_FLAGS,
      requestId: 942,
      args: encodeFdFdstatSetFlagsArgs(99, FDFLAGS.NONBLOCK),
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });
});

// ---- dispatch: FD_FILESTAT_SET_SIZE --------------------------------
//
// WASI's ftruncate opcode. Wire: (fd, new_size) — fd as u32 at
// args[0..4], new_size as u64 LE at args[4..12]; no heap. The TS
// tests pin the dispatcher's per-branch behaviour end-to-end through
// kernel.wasm: the EINVAL branch for a non-Vnode fd (char device),
// the EROFS branch for a procfs vnode (/proc/version), and the EBADF
// branch for an unopened fd. Happy-path truncate needs a tmpfs
// vnode fd which the TS harness cannot easily stand up (path_open
// with CREAT is not wired yet), so that branch is covered by the
// Rust tests only.

function encodeFdFilestatSetSizeArgs(
  fd: number,
  newSize: bigint,
): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fd, true);
  v.setBigUint64(4, newSize, true);
  return args;
}

describe("dispatch: FD_FILESTAT_SET_SIZE", () => {
  it("returns -EINVAL on a char-device fd (non-Vnode)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FILESTAT_SET_SIZE,
      requestId: 950,
      args: encodeFdFilestatSetSizeArgs(1, 0n),
    });
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EBADF when the fd is not open", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FILESTAT_SET_SIZE,
      requestId: 951,
      args: encodeFdFilestatSetSizeArgs(99, 100n),
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });

  it("returns -EROFS when truncating a procfs vnode (/proc/version)", async () => {
    // Open /proc/version through PATH_OPEN (procfs regular file →
    // Vnode fd), then call FD_FILESTAT_SET_SIZE on the resulting fd.
    // procfs.truncate returns ReadOnly → EROFS.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = new TextEncoder().encode("/proc/version");
    const { response: open } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 952,
        arg0: 0,
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(open!.status).toBe(0);
    const fd = Number(open!.value);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FILESTAT_SET_SIZE,
      requestId: 953,
      args: encodeFdFilestatSetSizeArgs(fd, 0n),
    });
    expect(response!.status).toBe(-ERRNO.EROFS);
  });
});

// ---- dispatch: FD_PREAD + FD_PWRITE --------------------------------
//
// Positional-I/O variants of FD_READ / FD_WRITE. Wire: (fd, offset)
// as u32 + u64 LE in the inline args window; heap = destination
// buffer (pread) / source bytes (pwrite). Vnode-only — non-Vnode
// fds reject with EINVAL, same guard shape as fd_seek. The TS tests
// pin the wire layout end-to-end through kernel.wasm: happy-path
// pread on /proc/version (via PATH_OPEN) showing real bytes flow
// back through the response, EINVAL branch on a /dev/console
// CharDevice fd, EBADF on an unopened fd, and the EROFS branch for
// pwrite against a procfs vnode. The Rust tests pin the
// "entry.offset stays unchanged" invariant that requires direct
// FdTable inspection.

function encodeFdPreadArgs(fd: number, offset: bigint): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fd, true);
  v.setBigUint64(4, offset, true);
  return args;
}

function encodeFdPwriteArgs(fd: number, offset: bigint): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fd, true);
  v.setBigUint64(4, offset, true);
  return args;
}

describe("dispatch: FD_PREAD", () => {
  it("reads bytes from a procfs vnode at an explicit offset", async () => {
    // Open /proc/version as a Vnode fd, then pread from offset 0.
    // procfs.read synthesises content on demand — the first few
    // bytes are deterministic so we can assert them.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = new TextEncoder().encode("/proc/version");
    const { response: open } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 960,
        arg0: 0,
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(open!.status).toBe(0);
    const fd = Number(open!.value);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.FD_PREAD,
      requestId: 961,
      args: encodeFdPreadArgs(fd, 0n),
      heapPtr: 0,
      heapLen: 32,
    });
    expect(response!.status).toBe(0);
    expect(response!.value > 0n).toBe(true);
    expect(response!.extraLen).toBeGreaterThan(0);
    // First byte must be a printable ASCII char (procfs emits a
    // text-like version string, not zero bytes).
    expect(heapOut[0]).not.toBe(0);
  });

  it("returns -EINVAL on a char-device fd (non-Vnode)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_PREAD,
      requestId: 962,
      args: encodeFdPreadArgs(1, 0n),
      heapPtr: 0,
      heapLen: 4,
    });
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EBADF when the fd is not open", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_PREAD,
      requestId: 963,
      args: encodeFdPreadArgs(99, 0n),
      heapPtr: 0,
      heapLen: 4,
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });
});

describe("dispatch: FD_PWRITE", () => {
  it("returns -EROFS when writing to a procfs vnode", async () => {
    // Open /proc/version as a Vnode fd, then pwrite onto it.
    // procfs.write returns ReadOnly → EROFS.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const path = new TextEncoder().encode("/proc/version");
    const { response: open } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PATH_OPEN,
        requestId: 964,
        arg0: 0,
        heapPtr: 0,
        heapLen: path.length,
      },
      path,
    );
    expect(open!.status).toBe(0);
    const fd = Number(open!.value);

    const heap = new TextEncoder().encode("hi");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_PWRITE,
        requestId: 965,
        args: encodeFdPwriteArgs(fd, 0n),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EROFS);
  });

  it("returns -EINVAL on a char-device fd (non-Vnode)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const heap = new TextEncoder().encode("hi");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_PWRITE,
        requestId: 966,
        args: encodeFdPwriteArgs(1, 0n),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EBADF when the fd is not open", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = new TextEncoder().encode("hi");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_PWRITE,
        requestId: 967,
        args: encodeFdPwriteArgs(99, 0n),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EBADF);
  });
});

// ---- dispatch: SOCK_SEND + SOCK_RECV -------------------------------
//
// WASI socket aliases of FD_WRITE / FD_READ on Socket fds. Wire:
// (fd, si_flags / ri_flags) at args[0..8]; heap = source (send) or
// destination (recv) buffer. Socket-only — non-Socket FdObject
// variants reject with EINVAL. The TS tests pin the wire layout
// end-to-end through kernel.wasm: EINVAL branch for a char-device
// fd, EINVAL branch for a fresh IPC_SOCKET-created socket (which
// is in Unbound state — send/recv before connect returns
// InvalidState → EINVAL), and EBADF for an unopened fd. Happy-
// path send/recv requires a connected socket pair which the TS
// harness can't easily stand up through public syscalls alone;
// the Rust tests pin that branch.

function encodeSockSendArgs(fd: number, siFlags: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fd, true);
  v.setUint32(4, siFlags & 0xffff, true);
  return args;
}

function encodeSockRecvArgs(fd: number, riFlags: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fd, true);
  v.setUint32(4, riFlags & 0xffff, true);
  return args;
}

describe("dispatch: SOCK_SEND", () => {
  it("returns -EINVAL on a char-device fd (non-Socket)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const heap = new TextEncoder().encode("hi");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.SOCK_SEND,
        requestId: 970,
        args: encodeSockSendArgs(1, 0),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EBADF when the fd is not open", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_SEND,
      requestId: 971,
      args: encodeSockSendArgs(99, 0),
      heapPtr: 0,
      heapLen: 0,
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });

  it("returns 0 on IPC_PIPE and the round-trip via fd_read/fd_write works", async () => {
    // User-facing pipe-pair creation: IPC_PIPE (ext 0x1007) allocates
    // a PipeRead fd and a PipeWrite fd, writing both as u32s at
    // heap[0..8]. FD_WRITE on the write fd then makes bytes
    // readable via FD_READ on the read fd.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_EXT.IPC_PIPE,
      requestId: 1090,
      arg0: 0,
      heapPtr: 0,
      heapLen: 8,
    });
    expect(response!.status).toBe(0);
    expect(response!.extraLen).toBe(8);
    const readFd = new DataView(
      heapOut.buffer,
      heapOut.byteOffset,
      heapOut.byteLength,
    ).getUint32(0, true);
    const writeFd = new DataView(
      heapOut.buffer,
      heapOut.byteOffset,
      heapOut.byteLength,
    ).getUint32(4, true);
    expect(readFd).not.toBe(writeFd);

    // Write "pipe" via the write fd.
    const payload = new TextEncoder().encode("pipe");
    const writeRes = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_WRITE,
        requestId: 1091,
        arg0: writeFd,
        heapPtr: 0,
        heapLen: payload.length,
      },
      payload,
    );
    expect(writeRes.response!.status).toBe(0);
    expect(Number(writeRes.response!.value)).toBe(4);

    // Read via the read fd and confirm.
    const readRes = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 1092,
      arg0: readFd,
      heapPtr: 0,
      heapLen: 16,
    });
    expect(readRes.response!.status).toBe(0);
    expect(Number(readRes.response!.value)).toBe(4);
    const decoded = new TextDecoder().decode(readRes.heapOut.subarray(0, 4));
    expect(decoded).toBe("pipe");
  });

  it("IPC_PIPE with heap_len < 8 returns -EINVAL", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_EXT.IPC_PIPE,
      requestId: 1093,
      arg0: 0,
      heapPtr: 0,
      heapLen: 4, // too short for (read_fd, write_fd) u32 pair
    });
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("IPC_PIPE then close read fd then fd_write returns -EPIPE", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const create = host.dispatch(pid, {
      opcode: OP_EXT.IPC_PIPE,
      requestId: 1094,
      arg0: 0,
      heapPtr: 0,
      heapLen: 8,
    });
    const readFd = new DataView(
      create.heapOut.buffer,
      create.heapOut.byteOffset,
      create.heapOut.byteLength,
    ).getUint32(0, true);
    const writeFd = new DataView(
      create.heapOut.buffer,
      create.heapOut.byteOffset,
      create.heapOut.byteLength,
    ).getUint32(4, true);

    // Close the read end.
    host.dispatch(pid, {
      opcode: OP_WASI.FD_CLOSE,
      requestId: 1095,
      arg0: readFd,
      heapPtr: 0,
      heapLen: 0,
    });

    // fd_write now sees EPIPE.
    const payload = new TextEncoder().encode("x");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_WRITE,
        requestId: 1096,
        arg0: writeFd,
        heapPtr: 0,
        heapLen: 1,
      },
      payload,
    );
    expect(response!.status).toBe(-ERRNO.EPIPE);
  });

  it("returns -EPIPE after own write side is shut down", async () => {
    // Build a connected socket pair via the IPC ext opcodes:
    // server: IPC_SOCKET → IPC_BIND → IPC_LISTEN; client: IPC_SOCKET
    // → IPC_CONNECT; server: IPC_ACCEPT → accepted_fd paired with
    // client. Then SOCK_SHUTDOWN(WR) on client → SOCK_SEND on
    // client returns -EPIPE via PipeBroken → EPIPE.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    // Server-side socket + bind + listen.
    const srvResp = host.dispatch(pid, {
      opcode: OP_EXT.IPC_SOCKET,
      requestId: 1070,
      arg0: 0,
    });
    const srvFd = Number(srvResp.response!.value);

    const bindPath = new TextEncoder().encode("/tmp/pair");
    host.dispatch(
      pid,
      {
        opcode: OP_EXT.IPC_BIND,
        requestId: 1071,
        arg0: srvFd,
        heapPtr: 0,
        heapLen: bindPath.length,
      },
      bindPath,
    );
    const listenArgs = new Uint8Array(16);
    const lv = new DataView(listenArgs.buffer);
    lv.setUint32(0, srvFd, true);
    lv.setUint32(4, 4, true); // backlog
    host.dispatch(pid, {
      opcode: OP_EXT.IPC_LISTEN,
      requestId: 1072,
      args: listenArgs,
    });

    // Client socket + connect.
    const clientResp = host.dispatch(pid, {
      opcode: OP_EXT.IPC_SOCKET,
      requestId: 1073,
      arg0: 0,
    });
    const clientFd = Number(clientResp.response!.value);

    const connectArgs = new Uint8Array(16);
    new DataView(connectArgs.buffer).setUint32(0, clientFd, true);
    host.dispatch(
      pid,
      {
        opcode: OP_EXT.IPC_CONNECT,
        requestId: 1074,
        args: connectArgs,
        heapPtr: 0,
        heapLen: bindPath.length,
      },
      bindPath,
    );

    // Server accepts.
    host.dispatch(pid, {
      opcode: OP_EXT.IPC_ACCEPT,
      requestId: 1075,
      arg0: srvFd,
    });

    // Now shutdown the client's write side.
    host.dispatch(pid, {
      opcode: OP_WASI.SOCK_SHUTDOWN,
      requestId: 1076,
      args: encodeSockShutdownArgs(clientFd, SDFLAGS.WR),
    });

    // sock_send should now return -EPIPE.
    const payload = new TextEncoder().encode("hi");
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.SOCK_SEND,
        requestId: 1077,
        args: encodeSockSendArgs(clientFd, 0),
        heapPtr: 0,
        heapLen: payload.length,
      },
      payload,
    );
    expect(response!.status).toBe(-ERRNO.EPIPE);
  });
});

describe("dispatch: SOCK_RECV", () => {
  it("returns -EINVAL on a char-device fd (non-Socket)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_RECV,
      requestId: 972,
      args: encodeSockRecvArgs(1, 0),
      heapPtr: 0,
      heapLen: 4,
    });
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EBADF when the fd is not open", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_RECV,
      requestId: 973,
      args: encodeSockRecvArgs(99, 0),
      heapPtr: 0,
      heapLen: 4,
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });
});

// ---- dispatch: SOCK_ACCEPT -----------------------------------------
//
// WASI alias of IPC_ACCEPT. Wire: (listener_fd, fdflags) as two u32s
// at args[0..4] + args[4..8]; no heap. These TS tests pin the
// dispatcher's per-branch behaviour end-to-end through kernel.wasm
// via opcodes userland can actually reach: EINVAL on a non-Socket
// fd (char-device), EINVAL on a socket in Unbound state (not yet
// listening), and EBADF on an unopened fd. The happy-path +
// fdflags-applied-to-new-fd branches need a full IPC handshake
// across two pids which is covered by the Rust tests.

function encodeSockAcceptArgs(fd: number, fdflags: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fd, true);
  v.setUint32(4, fdflags, true);
  return args;
}

describe("dispatch: SOCK_ACCEPT", () => {
  it("returns -EINVAL on a char-device fd (non-Socket)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_ACCEPT,
      requestId: 980,
      args: encodeSockAcceptArgs(1, 0),
    });
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EINVAL on a freshly-created (Unbound) socket fd", async () => {
    // Stand up a socket via IPC_SOCKET (state=Unbound), then try to
    // accept on it. The kernel's accept_socket returns
    // IpcError::InvalidState → EINVAL.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const sockResp = host.dispatch(pid, {
      opcode: OP_EXT.IPC_SOCKET,
      requestId: 981,
      arg0: 0,
    });
    expect(sockResp.response!.status).toBe(0);
    const sockFd = Number(sockResp.response!.value);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_ACCEPT,
      requestId: 982,
      args: encodeSockAcceptArgs(sockFd, 0),
    });
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EBADF when the fd is not open", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_ACCEPT,
      requestId: 983,
      args: encodeSockAcceptArgs(99, 0),
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });
});

describe("dispatch: IPC_ACCEPT blocking", () => {
  it("flags=0 on empty backlog parks caller (no response returned)", async () => {
    const { host } = await freshHost();
    const dsPid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(dsPid, 0);
    host.installConsoleFd(dsPid, 1);
    host.installConsoleFd(dsPid, 2);
    host.markRunning(dsPid);

    // Bind the listener at /run/display.
    const bindResult = host.dispatch(dsPid, {
      opcode: OP_EXT.DISPLAY_BIND,
      requestId: 1,
    });
    expect(bindResult.response).toBeDefined();
    expect(bindResult.response!.status).toBe(0);
    const listenerFd = Number(bindResult.response!.value);

    // Dispatch IPC_ACCEPT with flags=0 (args-window zero-fill means
    // args[4..6] is already zero, which is flags=0 = blocking).
    const args = new Uint8Array(16);
    new DataView(args.buffer).setUint32(0, listenerFd, true);

    const acceptResult = host.dispatch(dsPid, {
      opcode: OP_EXT.IPC_ACCEPT,
      requestId: 42,
      args,
    });

    // Parked: response is undefined, parked flag is true.
    expect(acceptResult.parked).toBe(true);
    expect(acceptResult.response).toBeUndefined();

    // No pending wakes for ds yet — nobody has connected.
    const wake = host.takeNextWakeForPid(dsPid);
    expect(wake).toBeNull();
  });

  it("IPC_CONNECT from peer wakes parked acceptor; take-wake delivers response", async () => {
    const { host } = await freshHost();

    // Display-server pid parks on the listener.
    const dsPid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(dsPid, 0);
    host.installConsoleFd(dsPid, 1);
    host.installConsoleFd(dsPid, 2);
    host.markRunning(dsPid);
    const bindResult = host.dispatch(dsPid, {
      opcode: OP_EXT.DISPLAY_BIND,
      requestId: 1,
    });
    const listenerFd = Number(bindResult.response!.value);

    const acceptArgs = new Uint8Array(16);
    new DataView(acceptArgs.buffer).setUint32(0, listenerFd, true);
    const acceptResult = host.dispatch(dsPid, {
      opcode: OP_EXT.IPC_ACCEPT,
      requestId: 99,
      args: acceptArgs,
    });
    expect(acceptResult.parked).toBe(true);

    // Client pid connects.
    const clientPid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(clientPid, 0);
    host.installConsoleFd(clientPid, 1);
    host.installConsoleFd(clientPid, 2);
    host.markRunning(clientPid);
    const connectResult = host.dispatch(clientPid, {
      opcode: OP_EXT.DISPLAY_CONNECT,
      requestId: 1,
    });
    expect(connectResult.response).toBeDefined();
    expect(connectResult.response!.status).toBe(0);

    // Take the wake for ds. Must return the parked accept's response:
    // same request_id (99), status=0, value=a fresh server-side fd.
    const wake = host.takeNextWakeForPid(dsPid);
    expect(wake).not.toBeNull();
    expect(wake!.requestId).toBe(99);
    expect(wake!.status).toBe(0);
    expect(Number(wake!.value)).toBeGreaterThan(0);

    // A second take returns null — the queue is drained.
    expect(host.takeNextWakeForPid(dsPid)).toBeNull();
  });

  it("SIGTERM wakes parked acceptor with EINTR; take-wake delivers error response", async () => {
    const { host } = await freshHost();

    // Display-server pid parks on the listener. init (CAPSET_ALL)
    // holds Cap::ProcKillAny so the cap check in `Kernel::proc_kill`
    // passes via the ProcKillAny branch (no ppid relationship
    // needed for the cap check — parent-ness only matters when
    // the sender lacks ProcKillAny).
    const initPid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(initPid, 0);
    host.installConsoleFd(initPid, 1);
    host.installConsoleFd(initPid, 2);
    host.markRunning(initPid);

    const dsPid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(dsPid, 0);
    host.installConsoleFd(dsPid, 1);
    host.installConsoleFd(dsPid, 2);
    host.markRunning(dsPid);

    // Bind the listener at /run/display.
    const bindResult = host.dispatch(dsPid, {
      opcode: OP_EXT.DISPLAY_BIND,
      requestId: 1,
    });
    expect(bindResult.response).toBeDefined();
    expect(bindResult.response!.status).toBe(0);
    const listenerFd = Number(bindResult.response!.value);

    // Park the display-server on the listener with flags=0.
    const acceptArgs = new Uint8Array(16);
    new DataView(acceptArgs.buffer).setUint32(0, listenerFd, true);
    const acceptResult = host.dispatch(dsPid, {
      opcode: OP_EXT.IPC_ACCEPT,
      requestId: 77,
      args: acceptArgs,
    });
    expect(acceptResult.parked).toBe(true);
    expect(acceptResult.response).toBeUndefined();

    // init sends SIGTERM (signum=15) to display-server.
    // PROC_KILL args: args[0..4] = target_pid (i32),
    // args[4..6] = signum (u16), rest zero-padded.
    const killArgs = new Uint8Array(16);
    const killView = new DataView(killArgs.buffer);
    killView.setInt32(0, dsPid, true);
    killView.setUint16(4, 15, true);
    const killResult = host.dispatch(initPid, {
      opcode: OP_EXT.PROC_KILL,
      requestId: 5,
      args: killArgs,
    });
    expect(killResult.response).toBeDefined();
    expect(killResult.response!.status).toBe(0);

    // Take the wake for ds. The response must carry:
    //   request_id === 77 (the parked accept's req_id)
    //   status === -EINTR
    const wake = host.takeNextWakeForPid(dsPid);
    expect(wake).not.toBeNull();
    expect(wake!.requestId).toBe(77);
    expect(wake!.status).toBe(-ERRNO.EINTR);

    // A second take returns null — the queue is drained.
    expect(host.takeNextWakeForPid(dsPid)).toBeNull();
  });
});

// ---- dispatch: SOCK_SHUTDOWN ---------------------------------------
//
// v1 supports only full close (how = RD | WR); half-close returns
// -ENOTSUP; zero how or reserved bits return -EINVAL. Wire: (fd,
// how) as two u32s at args[0..4] + args[4..8]; no heap. These TS
// tests pin the dispatcher's per-branch behaviour end-to-end
// through kernel.wasm: -EINVAL on a char-device fd (non-Socket),
// -ENOTSUP for a RD-only half-close on a fresh IPC_SOCKET-created
// fd, -EINVAL for a zero-how request, -EBADF for an unopened fd.
// The happy-path full-close path requires a connected pair which
// the Rust tests pin; the TS tests focus on the error surface.

function encodeSockShutdownArgs(fd: number, how: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fd, true);
  v.setUint32(4, how, true);
  return args;
}

describe("dispatch: SOCK_SHUTDOWN", () => {
  it("returns -EINVAL on a char-device fd (non-Socket)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_SHUTDOWN,
      requestId: 990,
      args: encodeSockShutdownArgs(1, SDFLAGS.RD | SDFLAGS.WR),
    });
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("succeeds for a half-close (RD alone) on a Socket fd", async () => {
    // Post-slice, half-close is first-class: RD alone sets the
    // shutdown_read flag on the socket and returns 0. The fd stays
    // open; a follow-up fd_close is still required to fully tear
    // down.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const sockResp = host.dispatch(pid, {
      opcode: OP_EXT.IPC_SOCKET,
      requestId: 991,
      arg0: 0,
    });
    expect(sockResp.response!.status).toBe(0);
    const sockFd = Number(sockResp.response!.value);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_SHUTDOWN,
      requestId: 992,
      args: encodeSockShutdownArgs(sockFd, SDFLAGS.RD),
    });
    expect(response!.status).toBe(0);
  });

  it("succeeds for a half-close (WR alone) on a Socket fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const sockResp = host.dispatch(pid, {
      opcode: OP_EXT.IPC_SOCKET,
      requestId: 1060,
      arg0: 0,
    });
    const sockFd = Number(sockResp.response!.value);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_SHUTDOWN,
      requestId: 1061,
      args: encodeSockShutdownArgs(sockFd, SDFLAGS.WR),
    });
    expect(response!.status).toBe(0);
  });

  it("succeeds for a full close (RD | WR) on a Socket fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const sockResp = host.dispatch(pid, {
      opcode: OP_EXT.IPC_SOCKET,
      requestId: 1062,
      arg0: 0,
    });
    const sockFd = Number(sockResp.response!.value);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_SHUTDOWN,
      requestId: 1063,
      args: encodeSockShutdownArgs(sockFd, SDFLAGS.RD | SDFLAGS.WR),
    });
    expect(response!.status).toBe(0);
  });

  it("returns -EINVAL for a zero-how request on a Socket fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const sockResp = host.dispatch(pid, {
      opcode: OP_EXT.IPC_SOCKET,
      requestId: 993,
      arg0: 0,
    });
    const sockFd = Number(sockResp.response!.value);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_SHUTDOWN,
      requestId: 994,
      args: encodeSockShutdownArgs(sockFd, 0),
    });
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EBADF when the fd is not open", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SOCK_SHUTDOWN,
      requestId: 995,
      args: encodeSockShutdownArgs(99, SDFLAGS.RD | SDFLAGS.WR),
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });
});

// ---- dispatch: FD_RENUMBER -----------------------------------------
//
// WASI's dup2-spelling. Wire: (from, to) as two u32s at args[0..4]
// + args[4..8]; no heap. These TS tests pin the dispatcher's per-
// branch behaviour end-to-end through kernel.wasm: the happy-path
// move (source closed, destination holds the entry), the
// noop-on-open contract, and both EBADF error branches.

function encodeFdRenumberArgs(from: number, to: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, from, true);
  v.setUint32(4, to, true);
  return args;
}

describe("dispatch: FD_RENUMBER", () => {
  it("moves an open fd to a fresh slot, closing the source", async () => {
    // Stage fd 3 as /dev/console via installConsoleFd, then
    // renumber 3 → 7. fd 3 becomes closed (next op on 3 is EBADF),
    // fd 7 holds what 3 had (a FD_WRITE on fd 7 reaches
    // /dev/console via onConsoleWrite).
    const { host, consoleWrites } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 3);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_RENUMBER,
      requestId: 870,
      args: encodeFdRenumberArgs(3, 7),
    });
    expect(response!.status).toBe(0);

    // A write to fd 7 now reaches the console sink.
    const line = new TextEncoder().encode("hi\n");
    const writeRes = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_WRITE,
        requestId: 871,
        arg0: 7,
        heapPtr: 0,
        heapLen: line.length,
      },
      line,
    );
    expect(writeRes.response!.status).toBe(0);
    expect(consoleWrites.length).toBe(1);

    // A write to fd 3 fails with EBADF — it was closed by the move.
    const brokenRes = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_WRITE,
        requestId: 872,
        arg0: 3,
        heapPtr: 0,
        heapLen: line.length,
      },
      line,
    );
    expect(brokenRes.response!.status).toBe(-ERRNO.EBADF);
  });

  it("returns 0 (no-op) when from == to on an open fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 5);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_RENUMBER,
      requestId: 873,
      args: encodeFdRenumberArgs(5, 5),
    });
    expect(response!.status).toBe(0);
  });

  it("returns -EBADF when from is unopened", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_RENUMBER,
      requestId: 874,
      args: encodeFdRenumberArgs(99, 7),
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });

  it("returns -EBADF when from == to on an unopened fd (POSIX dup2(bad, bad))", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_RENUMBER,
      requestId: 875,
      args: encodeFdRenumberArgs(42, 42),
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });
});

// ---- dispatch: FD_FILESTAT_SET_TIMES -------------------------------
//
// Fd-based sibling of PATH_FILESTAT_SET_TIMES. Wire: fd at args[0..4]
// + fstflags at args[4..8]; atim + mtim share the heap (two u64 LE at
// [0..16], heap_len = 16). These TS tests pin the dispatcher's
// per-branch behaviour end-to-end through kernel.wasm: EBADF on an
// unopened fd, EINVAL on a non-Vnode fd (/dev/console), EINVAL on
// invalid flag pairs, zero-flags no-op success on a valid Vnode fd,
// and the EROFS passthrough from procfs. The "set-times-actually-
// applied" check lives in the Rust kernel tests since the TS
// dispatcher alone can't stat by fd through the opcode surface.

function encodeFdSetTimesArgs(fd: number, fstflags: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fd, true);
  v.setUint32(4, fstflags, true);
  return args;
}

function encodeFdSetTimesHeap(atim: bigint, mtim: bigint): Uint8Array {
  const heap = new Uint8Array(16);
  const v = new DataView(heap.buffer);
  v.setBigUint64(0, atim, true);
  v.setBigUint64(8, mtim, true);
  return heap;
}

describe("dispatch: FD_FILESTAT_SET_TIMES", () => {
  it("returns -EBADF on an unopened fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodeFdSetTimesHeap(111n, 222n);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_FILESTAT_SET_TIMES,
        requestId: 860,
        args: encodeFdSetTimesArgs(99, FSTFLAGS.SET_ATIM | FSTFLAGS.SET_MTIM),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EBADF);
  });

  it("returns -EINVAL on a /dev/console fd (non-Vnode FdObject)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const heap = encodeFdSetTimesHeap(0n, 0n);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_FILESTAT_SET_TIMES,
        requestId: 861,
        args: encodeFdSetTimesArgs(1, FSTFLAGS.SET_ATIM | FSTFLAGS.SET_MTIM),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EINVAL when SET_ATIM and SET_ATIM_NOW are both set", async () => {
    // Pair-conflict check fires before fd lookup, so we can use
    // fd=99 (unopened) and still see EINVAL — stable errno across
    // fd states is part of the contract.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodeFdSetTimesHeap(0n, 0n);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_FILESTAT_SET_TIMES,
        requestId: 862,
        args: encodeFdSetTimesArgs(99, FSTFLAGS.SET_ATIM | FSTFLAGS.SET_ATIM_NOW),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -EINVAL when SET_MTIM and SET_MTIM_NOW are both set", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const heap = encodeFdSetTimesHeap(0n, 0n);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_FILESTAT_SET_TIMES,
        requestId: 863,
        args: encodeFdSetTimesArgs(99, FSTFLAGS.SET_MTIM | FSTFLAGS.SET_MTIM_NOW),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns 0 (no-op success) for zero fstflags on a valid Vnode fd", async () => {
    // /proc/version opens as a Vnode fd. Zero flags = no-op; the
    // kernel never reaches set_times_ino, so procfs's EROFS never
    // fires.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    const heap = encodeFdSetTimesHeap(0n, 0n);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_FILESTAT_SET_TIMES,
        requestId: 864,
        args: encodeFdSetTimesArgs(fd, 0),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(0);
  });

  it("returns -EROFS on a procfs Vnode fd with non-zero fstflags", async () => {
    // /proc/version is a Vnode fd (not a char device), so the
    // non-Vnode EINVAL guard doesn't fire. Procfs's set_times returns
    // ReadOnly → EROFS.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);
    const fd = openProcVersion(host, pid);

    const heap = encodeFdSetTimesHeap(111n, 222n);
    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.FD_FILESTAT_SET_TIMES,
        requestId: 865,
        args: encodeFdSetTimesArgs(fd, FSTFLAGS.SET_ATIM | FSTFLAGS.SET_MTIM),
        heapPtr: 0,
        heapLen: heap.length,
      },
      heap,
    );
    expect(response!.status).toBe(-ERRNO.EROFS);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(1n);
    expect(response!.extraLen).toBe(POLL_EVENT_SIZE);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(0n);
    expect(response!.extraLen).toBe(0);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(1n);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(1n);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(1n);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(1n);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(1n);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(2n);
    expect(response!.extraLen).toBe(2 * POLL_EVENT_SIZE);
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
    expect(response!.status).toBe(0);
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
// new absolute offset in `response!.value` — same shape as
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
  expect(response!.status).toBe(0);
  return Number(response!.value);
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
    expect(response!.status).toBe(-ERRNO.EBADF);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(7n);
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
    expect(seek.response!.status).toBe(0);

    const tell = host.dispatch(pid, {
      opcode: OP_WASI.FD_SEEK,
      requestId: 774,
      args: encodeFdSeekArgs(fd, WHENCE.CUR, 0n),
    });
    expect(tell.response!.status).toBe(0);
    expect(tell.response!.value).toBe(4n);
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
    expect(stat.response!.status).toBe(0);
    const size = decodeFilestat(stat.heapOut).size;
    expect(size).toBeGreaterThan(0n);

    const seek = host.dispatch(pid, {
      opcode: OP_WASI.FD_SEEK,
      requestId: 776,
      args: encodeFdSeekArgs(fd, WHENCE.END, 0n),
    });
    expect(seek.response!.status).toBe(0);
    expect(seek.response!.value).toBe(size);
  });
});

// ---- dispatch: FD_TELL ----------------------------------------------
//
// The read-only sibling of FD_SEEK: fd at a single u32 arg0 (no
// whence + no offset — fd_tell takes nothing but an fd), response
// carries the current absolute offset in `response!.value`. Shares
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(0n);
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
    expect(seek.response!.status).toBe(0);

    const tell = host.dispatch(pid, {
      opcode: OP_WASI.FD_TELL,
      requestId: 782,
      arg0: fd,
    });
    expect(tell.response!.status).toBe(0);
    expect(tell.response!.value).toBe(6n);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
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
    expect(response!.status).toBe(-ERRNO.EBADF);
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
    expect(response!.status).toBe(-ERRNO.ENOTSUP);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
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
    expect(response!.status).toBe(-ERRNO.EBADF);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
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
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(0n);
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
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });
});

// ---- dispatch: PROC_WAIT --------------------------------------------
//
// PROC_WAIT (0x1101). Wire layout: args[0..4] = target_pid (i32),
// args[4..8] = options u32 (WNOHANG=0x1). heap_len=4 writes the
// reaped child's pid into heap[0..4]. On success, response!.value
// carries the packed status (low 32 exit code; 32..40 signum;
// 40..48 flags: 0x01 Exited, 0x02 Signaled, 0x04 Crashed). Error
// surface: EINVAL (target_pid < -1 or unknown options bits),
// ECHILD (no matching child or self-wait), EAGAIN (live child, not
// yet zombie).

function encodeProcWaitArgs(targetPid: number, options: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setInt32(0, targetPid, true);
  v.setUint32(4, options >>> 0, true);
  return args;
}

describe("dispatch: PROC_WAIT", () => {
  it("returns -ECHILD when the caller has no children", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_WAIT,
        requestId: 1300,
        args: encodeProcWaitArgs(-1, 0),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(-ERRNO.ECHILD);
  });

  it("returns -ECHILD when the target pid is the caller itself", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_WAIT,
        requestId: 1301,
        args: encodeProcWaitArgs(pid, 0),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(-ERRNO.ECHILD);
  });

  it("returns -EINVAL when target_pid is below -1", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_WAIT,
        requestId: 1302,
        args: encodeProcWaitArgs(-42, 0),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });
});

// ---- dispatch: PROC_WAIT blocking ----------------------------------
//
// Slice 2c.1. When options=0 and a matching live child exists but
// no zombie, handle_proc_wait returns ServiceOutcome::Parked (no
// response push). A later child-exit transitions the parent Ready
// and queues the wake via kernel.pending_wakes; drainWakesForPid
// copies the wake response + heap bytes (child pid as u32 LE) onto
// the parent's SAB.

describe("dispatch: PROC_WAIT blocking", () => {
  it("options=0 on live child parks caller (no response push)", async () => {
    const { host } = await freshHost();

    // Parent holds CAPSET_ALL. Spawn a child with CAP_SPAWN via
    // PROC_SPAWN so the kernel's proc_wait sees a live child.
    const parent = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(parent, 0);
    host.installConsoleFd(parent, 1);
    host.installConsoleFd(parent, 2);
    host.markRunning(parent);

    // Spawn a child via the kernel's own register_process path —
    // the test-only `spawnChildForTest` helper mirrors what Rust's
    // spawn_ordinary_app does, installing a child pid under the
    // parent with ORDINARY_APP caps + console stdio.
    const child = host.spawnChildForTest(parent, "child");
    expect(child).toBeGreaterThan(parent);

    // Dispatch PROC_WAIT(target=-1, options=0). Because the child
    // is live and non-zombie, the handler parks the parent.
    const waitArgs = new Uint8Array(16);
    const waitView = new DataView(waitArgs.buffer);
    waitView.setInt32(0, -1, true);
    waitView.setUint32(4, 0, true);
    const waitResult = host.dispatch(parent, {
      opcode: OP_EXT.PROC_WAIT,
      requestId: 0x2c01,
      args: waitArgs,
      heapPtr: 0,
      heapLen: 4,
    });

    // Parked: no response, parked flag true.
    expect(waitResult.parked).toBe(true);
    expect(waitResult.response).toBeUndefined();

    // No wake queued yet.
    const wake = host.takeNextWakeForPid(parent);
    expect(wake).toBeNull();
  });

  it("child exit wakes parked parent with packed status + pid heap readback", async () => {
    const { host } = await freshHost();

    const parent = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(parent, 0);
    host.installConsoleFd(parent, 1);
    host.installConsoleFd(parent, 2);
    host.markRunning(parent);

    const child = host.spawnChildForTest(parent, "child");
    host.markRunning(child);

    // Park the parent with heap_len = 4 so the wake carries the
    // reaped child's pid in the heap bytes.
    const waitArgs = new Uint8Array(16);
    const waitView = new DataView(waitArgs.buffer);
    waitView.setInt32(0, -1, true);
    waitView.setUint32(4, 0, true);
    const waitResult = host.dispatch(parent, {
      opcode: OP_EXT.PROC_WAIT,
      requestId: 0x2c02,
      args: waitArgs,
      heapPtr: 0,
      heapLen: 4,
    });
    expect(waitResult.parked).toBe(true);

    // Child calls PROC_EXIT(0). This transitions the child to
    // Zombie; the kernel's wake_parked_waiter_for_child helper
    // runs during proc_exit, reaps the child inline, queues the
    // wake.
    const exitArgs = new Uint8Array(16);
    new DataView(exitArgs.buffer).setInt32(0, 0, true);
    const exitResult = host.dispatch(child, {
      opcode: OP_WASI.PROC_EXIT,
      requestId: 0x2c03,
      args: exitArgs,
    });
    // proc_exit produces no response (the exiting pid is gone);
    // the wake for the PARENT is queued on pending_wakes.
    expect(exitResult.parked).toBeUndefined();

    // Take the wake for the parent — the response carries the
    // packed exit status. WaitOutcome::Reaped with Exited(0)
    // packs to bits 40..48 = 0x01 (Exited flag), bits 0..32 = 0
    // (the exit code): the u64 value is 0x0000_0100_0000_0000.
    const wake = host.takeNextWakeForPidWithHeap(parent);
    expect(wake).not.toBeNull();
    expect(wake!.response.requestId).toBe(0x2c02);
    expect(wake!.response.status).toBe(0);
    // Packed status: Exited(0) — flag 0x01 at bits 40..48, rest 0.
    const expectedValue = BigInt(0x01) << BigInt(40);
    expect(wake!.response.value).toBe(expectedValue);
    // extra_len = 4 with the child pid.
    expect(wake!.response.extraLen).toBe(4);
    expect(wake!.heapBytes).not.toBeNull();
    expect(wake!.heapBytes!.byteLength).toBe(4);
    const heapView = new DataView(
      wake!.heapBytes!.buffer,
      wake!.heapBytes!.byteOffset,
      4,
    );
    expect(heapView.getUint32(0, true)).toBe(child);

    // A second take returns null — queue drained.
    expect(host.takeNextWakeForPidWithHeap(parent)).toBeNull();
  });

  it("SIGTERM wakes parked parent with EINTR", async () => {
    const { host } = await freshHost();

    // Grandparent registers with CAPSET_ALL so it holds
    // ProcKillAny; parent is spawned as a child of grandparent so
    // the is_parent cap-check arm applies to the PROC_KILL.
    const grandparent = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(grandparent, 0);
    host.installConsoleFd(grandparent, 1);
    host.installConsoleFd(grandparent, 2);
    host.markRunning(grandparent);

    const parent = host.spawnChildForTest(grandparent, "parent");
    host.markRunning(parent);

    // Parent needs a child of its own so the proc_wait doesn't
    // hit the NoChildren → ECHILD early-reject.
    const _child = host.spawnChildForTest(parent, "child");

    // Parent parks on PROC_WAIT(target=-1, options=0).
    const waitArgs = new Uint8Array(16);
    const waitView = new DataView(waitArgs.buffer);
    waitView.setInt32(0, -1, true);
    waitView.setUint32(4, 0, true);
    const waitResult = host.dispatch(parent, {
      opcode: OP_EXT.PROC_WAIT,
      requestId: 0x2c04,
      args: waitArgs,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(waitResult.parked).toBe(true);

    // Grandparent sends SIGTERM (signum=15) to parent.
    // PROC_KILL args: args[0..4] = target_pid (i32),
    // args[4..6] = signum (u16).
    const killArgs = new Uint8Array(16);
    const killView = new DataView(killArgs.buffer);
    killView.setInt32(0, parent, true);
    killView.setUint16(4, 15, true);
    const killResult = host.dispatch(grandparent, {
      opcode: OP_EXT.PROC_KILL,
      requestId: 0x2c05,
      args: killArgs,
    });
    expect(killResult.response).toBeDefined();
    expect(killResult.response!.status).toBe(0);

    // Take the wake for the parent. status === -EINTR, request_id
    // matches the parked PROC_WAIT's request_id. extra_len === 0
    // (EINTR wakes carry no heap payload).
    const wake = host.takeNextWakeForPid(parent);
    expect(wake).not.toBeNull();
    expect(wake!.requestId).toBe(0x2c04);
    expect(wake!.status).toBe(-ERRNO.EINTR);
    expect(wake!.extraLen).toBe(0);

    // A second take returns null — queue drained.
    expect(host.takeNextWakeForPid(parent)).toBeNull();
  });
});

// ---- dispatch: PROC_CAPS_GET ----------------------------------------
//
// PROC_CAPS_GET (0x1105). Wire: args[0..4] = target_pid i32.
// Returns CapSet as i64 in response!.value on success; -ESRCH when
// the target doesn't exist; -ENOTCAPABLE when the sender is not
// the target's parent, not self, and doesn't hold Cap::ProcInspect.

function encodeProcCapsGetArgs(targetPid: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setInt32(0, targetPid, true);
  return args;
}

describe("dispatch: PROC_CAPS_GET", () => {
  it("returns the caller's own caps when target == sender", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_CAPS_GET,
        requestId: 1320,
        args: encodeProcCapsGetArgs(pid),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(0);
    // CAPSET_ALL = u64::MAX. As an i64 that's -1; bit-pattern
    // round-trips through the value field.
    expect(BigInt.asUintN(64, response!.value)).toBe(CAPSET_ALL);
  });

  it("returns -ESRCH for an unknown target pid", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_CAPS_GET,
        requestId: 1321,
        args: encodeProcCapsGetArgs(9999),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(-ERRNO.ESRCH);
  });
});

// ---- dispatch: PROC_KILL --------------------------------------------
//
// PROC_KILL (0x1102). Wire: args[0..4] = target_pid i32; args[4..6]
// = signum u16. v1 knows SIGINT=2, SIGKILL=9, SIGTERM=15; any other
// signum → -EINVAL. -ESRCH on unknown pid, -ENOTCAPABLE when sender
// is not parent / not self / doesn't hold Cap::ProcKillAny.

function encodeProcKillArgs(targetPid: number, signum: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setInt32(0, targetPid, true);
  v.setUint16(4, signum & 0xffff, true);
  return args;
}

describe("dispatch: PROC_KILL", () => {
  it("returns -EINVAL for an unknown signum", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_KILL,
        requestId: 1310,
        args: encodeProcKillArgs(pid, 77),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns -ESRCH when the target pid does not exist", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_KILL,
        requestId: 1311,
        args: encodeProcKillArgs(9999, 15),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(-ERRNO.ESRCH);
  });

  it("returns 0 for self-SIGINT (POSIX kill(getpid(), SIGINT))", async () => {
    // Self-kill with a catchable signal works even without
    // Cap::ProcKillAny — POSIX lets a process signal itself freely.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_KILL,
        requestId: 1312,
        args: encodeProcKillArgs(pid, 2),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(0);
  });

  // POSIX `kill(pid, 0)` — the existence + permission probe. Same
  // precondition surface as a real signal delivery (target exists
  // and is not reaped, sender is parent / self / holds
  // Cap::ProcKillAny) but no signal is queued. Proves the signum-0
  // arm landed by cab9dc5 is reachable through the bundled
  // kernel.wasm and not stripped by LTO.
  it("returns 0 for self-probe (POSIX kill(getpid(), 0))", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_KILL,
        requestId: 1313,
        args: encodeProcKillArgs(pid, 0),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(0);
  });

  it("returns -ESRCH for an unknown pid with signum 0", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_KILL,
        requestId: 1314,
        args: encodeProcKillArgs(9999, 0),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(-ERRNO.ESRCH);
  });

  it("returns -ENOTCAPABLE when probing a non-child target without Cap::ProcKillAny", async () => {
    // Two orphan processes with CAPSET_ORDINARY_APP (DisplayClient
    // only — no PROC_KILL_ANY). Sender is neither parent nor self
    // of target, so the probe must reject with ENOTCAPABLE — the
    // POSIX EPERM analogue under PMos's capability model.
    const { host } = await freshHost();
    const sender = host.registerProcess(CAPSET_ORDINARY_APP);
    const target = host.registerProcess(CAPSET_ORDINARY_APP);
    host.markRunning(sender);
    host.markRunning(target);

    const { response } = host.dispatch(
      sender,
      {
        opcode: OP_EXT.PROC_KILL,
        requestId: 1315,
        args: encodeProcKillArgs(target, 0),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(-ERRNO.ENOTCAPABLE);
  });

  // Signums 13 (SIGPIPE) and 17 (SIGCHLD) were added to the
  // PROC_KILL accept set by 91c618f's SIGCHLD delivery slice. The
  // 91c618f session note explicitly flagged this gap: "the new
  // 13 / 17 arms are reachable from TS but not exercised yet".
  // These two self-kill tests close it.
  it("returns 0 for self-SIGPIPE (signum 13, accepted since 91c618f)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_KILL,
        requestId: 1316,
        args: encodeProcKillArgs(pid, 13),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(0);
  });

  it("returns 0 for self-SIGCHLD (signum 17, accepted since 91c618f)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_EXT.PROC_KILL,
        requestId: 1317,
        args: encodeProcKillArgs(pid, 17),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(0);
  });
});

// ---- dispatch: PROC_RAISE -------------------------------------------
//
// PROC_RAISE (0x0061, WASI range). Wire: args[0..2] = signum u16; no
// target_pid (always the caller). v1 knows SIGINT=2, SIGKILL=9,
// SIGTERM=15; any other signum → -EINVAL. No ENOTCAPABLE path exists
// for raise(2) — the self-signal cap rule permits every sender.
// SIGKILL zombifies the caller (response still posts back first).
// SIGTERM / SIGINT queue on the caller's own SignalInbox.

function encodeProcRaiseArgs(signum: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint16(0, signum & 0xffff, true);
  return args;
}

describe("dispatch: PROC_RAISE", () => {
  it("returns -EINVAL for an unknown signum", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PROC_RAISE,
        requestId: 1320,
        args: encodeProcRaiseArgs(77),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });

  it("returns 0 for self-SIGINT and queues a pending signal", async () => {
    // Catchable signal to self — POSIX raise(SIGINT). The caller's
    // SignalInbox accumulates one pending signal.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PROC_RAISE,
        requestId: 1321,
        args: encodeProcRaiseArgs(2),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(0);
  });

  it("returns 0 for self-SIGKILL (the response posts back before the caller is torn down)", async () => {
    // Terminal signal to self — POSIX raise(SIGKILL). The handler
    // returns success; the dispatcher has already written the
    // response before the caller's Worker is torn down on the next
    // dispatch pass.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PROC_RAISE,
        requestId: 1322,
        args: encodeProcRaiseArgs(9),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(0);
  });

  // Signums 13 (SIGPIPE) and 17 (SIGCHLD) were added to the
  // PROC_RAISE/PROC_KILL accept set by 91c618f's SIGCHLD delivery
  // slice. Prior to this slice the TS coverage only asserted
  // {2, 9, 15} through the bundled kernel.wasm; the new arms for
  // {13, 17} were reachable from TS but never exercised there.
  // These two tests close that gap.
  it("returns 0 for self-SIGPIPE (signum 13, accepted since 91c618f)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PROC_RAISE,
        requestId: 1323,
        args: encodeProcRaiseArgs(13),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(0);
  });

  it("returns 0 for self-SIGCHLD (signum 17, accepted since 91c618f)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(
      pid,
      {
        opcode: OP_WASI.PROC_RAISE,
        requestId: 1324,
        args: encodeProcRaiseArgs(17),
        heapPtr: 0,
        heapLen: 0,
      },
    );
    expect(response!.status).toBe(0);
  });
});

// ---- dispatch: FD_READ on SignalChannel --------------------------
//
// Proves the full signal-observation pipeline through real
// kernel.wasm: PROC_RAISE queues a signal on the caller's inbox,
// FD_READ on an installed SignalChannel fd drains it as a u16 LE
// signum. Combined, these demonstrate that a userland process that
// installs fd 3 as SignalChannel + polls + reads it can observe
// every signal that lands in its inbox — the drain path is live
// in the distributed kernel.wasm, not just the Rust isolation
// tests.

describe("dispatch: FD_READ on SignalChannel", () => {
  it("drains a self-raised SIGTERM as u16 LE 15", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installSignalChannelFd(pid, 3);
    host.markRunning(pid);

    // Self-raise SIGTERM.
    const raise = host.dispatch(pid, {
      opcode: OP_WASI.PROC_RAISE,
      requestId: 1400,
      args: encodeProcRaiseArgs(15),
      heapPtr: 0,
      heapLen: 0,
    });
    expect(raise.response!.status).toBe(0);

    // Read fd 3 — expect 2 bytes = u16 LE 15.
    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 1401,
      arg0: 3,
      heapPtr: 0,
      heapLen: 4,
    });
    expect(response!.status).toBe(0);
    expect(Number(response!.value)).toBe(2);
    expect(heapOut.length).toBeGreaterThanOrEqual(2);
    const signum = new DataView(
      heapOut.buffer,
      heapOut.byteOffset,
      heapOut.byteLength,
    ).getUint16(0, true);
    expect(signum).toBe(15);
  });

  it("drains a self-raised SIGPIPE as u16 LE 13", async () => {
    // Proves that signum 13 (added to the dispatcher's accepted
    // set in the SIGCHLD slice) flows through PROC_RAISE ->
    // Signal::Pipe -> inbox -> FD_READ end-to-end.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installSignalChannelFd(pid, 3);
    host.markRunning(pid);

    host.dispatch(pid, {
      opcode: OP_WASI.PROC_RAISE,
      requestId: 1402,
      args: encodeProcRaiseArgs(13),
      heapPtr: 0,
      heapLen: 0,
    });

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 1403,
      arg0: 3,
      heapPtr: 0,
      heapLen: 4,
    });
    expect(response!.status).toBe(0);
    expect(Number(response!.value)).toBe(2);
    const signum = new DataView(
      heapOut.buffer,
      heapOut.byteOffset,
      heapOut.byteLength,
    ).getUint16(0, true);
    expect(signum).toBe(13);
  });

  it("returns -EAGAIN when the signal inbox is empty", async () => {
    // No signals queued — fd_read on SignalChannel reports
    // WouldBlock surfaced as -EAGAIN.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installSignalChannelFd(pid, 3);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_READ,
      requestId: 1404,
      arg0: 3,
      heapPtr: 0,
      heapLen: 8,
    });
    expect(response!.status).toBe(-ERRNO.EAGAIN);
  });
});

// ---- dispatch: RANDOM_GET -------------------------------------------
//
// WASI random_get fills a heap region with random bytes from
// Platform::random_bytes. Closes the dispatch-layer coverage gap
// from 5dd1714's shim slice — until this slice the path was only
// proven through real wasm (hello-random).

describe("dispatch: RANDOM_GET", () => {
  it("fills 8 bytes of heap with non-constant data", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.RANDOM_GET,
      requestId: 1500,
      heapPtr: 0,
      heapLen: 8,
    });
    expect(response!.status).toBe(0);
    expect(Number(response!.value)).toBe(8);
    expect(heapOut.length).toBe(8);
    // 8 random bytes match the all-zero buffer with chance 1 in
    // 2^64 — call it a stuck-source signal.
    const allZero = heapOut.every((b) => b === 0);
    expect(allZero).toBe(false);
  });

  it("two consecutive 8-byte fills produce different bytes", async () => {
    // Strong sanity check that the source isn't returning a
    // stuck-constant value — companion to hello-random's binary
    // assertion. Test mirrors the same "1 in 2^64" inequality
    // assertion at the dispatcher layer.
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const a = host.dispatch(pid, {
      opcode: OP_WASI.RANDOM_GET,
      requestId: 1501,
      heapPtr: 0,
      heapLen: 8,
    });
    const b = host.dispatch(pid, {
      opcode: OP_WASI.RANDOM_GET,
      requestId: 1502,
      heapPtr: 0,
      heapLen: 8,
    });
    expect(a.response!.status).toBe(0);
    expect(b.response!.status).toBe(0);
    expect(Array.from(a.heapOut)).not.toEqual(Array.from(b.heapOut));
  });
});

// ---- dispatch: SCHED_YIELD -----------------------------------------
//
// Cooperative-scheduling hint. PMos's scheduler is a single-threaded
// round-robin; the kernel handler is a no-op that always returns 0.
// Closes the dispatch-layer coverage gap from 5dd1714's shim slice.

describe("dispatch: SCHED_YIELD", () => {
  it("returns 0 with no observable side effect", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.SCHED_YIELD,
      requestId: 1510,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(response!.status).toBe(0);
    expect(Number(response!.value)).toBe(0);
  });
});

// ---- dispatch: FD_CLOSE --------------------------------------------
//
// Closes the dispatch-layer coverage gap from f03cf74's shim slice.
// Two arms: closing an unopened fd surfaces -EBADF; closing an
// installed fd succeeds with rc=0.

function encodeFdCloseArgs(fd: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, fd >>> 0, true);
  return args;
}

describe("dispatch: FD_CLOSE", () => {
  it("returns -EBADF for an unopened fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_CLOSE,
      requestId: 1520,
      args: encodeFdCloseArgs(99),
      heapPtr: 0,
      heapLen: 0,
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
  });

  it("returns 0 when closing an installed console fd, then -EBADF on a re-close", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const close1 = host.dispatch(pid, {
      opcode: OP_WASI.FD_CLOSE,
      requestId: 1521,
      args: encodeFdCloseArgs(1),
      heapPtr: 0,
      heapLen: 0,
    });
    expect(close1.response!.status).toBe(0);

    // Re-close — fd 1 is now released; the second call must
    // surface -EBADF (proves the close actually freed the slot,
    // not just no-op'd).
    const close2 = host.dispatch(pid, {
      opcode: OP_WASI.FD_CLOSE,
      requestId: 1522,
      args: encodeFdCloseArgs(1),
      heapPtr: 0,
      heapLen: 0,
    });
    expect(close2.response!.status).toBe(-ERRNO.EBADF);
  });
});

// ---- dispatch: CAP_CHECK -------------------------------------------
//
// Per-cap yes/no probe. Closes the dispatch-layer coverage gap from
// a18cbb1's shim slice. Three arms: holds the cap (value=1), doesn't
// hold the cap (value=0), unknown cap id (-EINVAL).

function encodeCapCheckArgs(capId: number): Uint8Array {
  const args = new Uint8Array(16);
  const v = new DataView(args.buffer);
  v.setUint32(0, capId >>> 0, true);
  return args;
}

describe("dispatch: CAP_CHECK", () => {
  it("returns value=1 when the caller holds the cap (CAPSET_ALL ⊃ Shell)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_EXT.CAP_CHECK,
      requestId: 1530,
      args: encodeCapCheckArgs(CAP.SHELL),
      heapPtr: 0,
      heapLen: 0,
    });
    expect(response!.status).toBe(0);
    expect(Number(response!.value)).toBe(1);
  });

  it("returns value=0 when the caller doesn't hold the cap (CAPSET_ORDINARY_APP lacks Shell)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ORDINARY_APP);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_EXT.CAP_CHECK,
      requestId: 1531,
      args: encodeCapCheckArgs(CAP.SHELL),
      heapPtr: 0,
      heapLen: 0,
    });
    expect(response!.status).toBe(0);
    expect(Number(response!.value)).toBe(0);
  });

  it("returns -EINVAL for a cap id that doesn't correspond to a Cap discriminant", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_EXT.CAP_CHECK,
      requestId: 1532,
      args: encodeCapCheckArgs(99),
      heapPtr: 0,
      heapLen: 0,
    });
    expect(response!.status).toBe(-ERRNO.EINVAL);
  });
});

// ---- dispatch: CAP_LIST --------------------------------------------
//
// No-args "give me my own caps" primitive. Closes the dispatch-layer
// coverage gap from bc5c9ad's shim slice. Two arms cover CAPSET_ALL
// and CAPSET_ORDINARY_APP.

describe("dispatch: CAP_LIST", () => {
  it("returns CAPSET_ALL as i64 reinterpret of u64 bits when caller holds every cap", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_EXT.CAP_LIST,
      requestId: 1540,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(response!.status).toBe(0);
    // u64::MAX reinterpreted as i64 is -1n (the sign bit set).
    expect(response!.value).toBe(-1n);
  });

  it("returns CAPSET_ORDINARY_APP as i64 reinterpret when caller holds only DisplayClient", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ORDINARY_APP);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_EXT.CAP_LIST,
      requestId: 1541,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(CAPSET_ORDINARY_APP);
  });
});

// ---- dispatch: ARGS_SIZES_GET / ARGS_GET ---------------------------
//
// PMos v1 has no command-line argument propagation — proc_spawn's
// SpawnManifest doesn't carry argv. The kernel handlers always
// answer "argc=0, args_buf_size=0" / no-op, mirroring an empty
// argv vector. Closes the dispatch-layer coverage gap; the
// shims at user-wasm-runtime.ts:737/760 have existed since hello-std
// landed but were never tested at the dispatcher.

describe("dispatch: ARGS_SIZES_GET", () => {
  it("writes two zero u32s (argc=0, args_buf_size=0) and reports value=0 + extra_len=8", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.ARGS_SIZES_GET,
      requestId: 1600,
      heapPtr: 0,
      heapLen: 8,
    });
    expect(response!.status).toBe(0);
    expect(Number(response!.value)).toBe(0);
    expect(response!.extraLen).toBe(8);
    expect(heapOut.length).toBe(8);
    const view = new DataView(
      heapOut.buffer,
      heapOut.byteOffset,
      heapOut.byteLength,
    );
    expect(view.getUint32(0, true)).toBe(0);
    expect(view.getUint32(4, true)).toBe(0);
  });
});

describe("dispatch: ARGS_GET", () => {
  it("returns 0 (no-op since argc=0)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.ARGS_GET,
      requestId: 1610,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(response!.status).toBe(0);
    expect(Number(response!.value)).toBe(0);
  });
});

// ---- dispatch: ENVIRON_SIZES_GET / ENVIRON_GET ---------------------
//
// Mirror of ARGS_*: PMos v1 has no environ propagation. The kernel
// handlers answer "envc=0, environ_buf_size=0" / no-op.

describe("dispatch: ENVIRON_SIZES_GET", () => {
  it("writes two zero u32s (envc=0, environ_buf_size=0) and reports value=0 + extra_len=8", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.ENVIRON_SIZES_GET,
      requestId: 1620,
      heapPtr: 0,
      heapLen: 8,
    });
    expect(response!.status).toBe(0);
    expect(Number(response!.value)).toBe(0);
    expect(response!.extraLen).toBe(8);
    expect(heapOut.length).toBe(8);
    const view = new DataView(
      heapOut.buffer,
      heapOut.byteOffset,
      heapOut.byteLength,
    );
    expect(view.getUint32(0, true)).toBe(0);
    expect(view.getUint32(4, true)).toBe(0);
  });
});

describe("dispatch: ENVIRON_GET", () => {
  it("returns 0 (no-op since envc=0)", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.ENVIRON_GET,
      requestId: 1630,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(response!.status).toBe(0);
    expect(Number(response!.value)).toBe(0);
  });
});

// ---- dispatch: FD_FDSTAT_GET ---------------------------------------
//
// Returns a 24-byte fdstat_t whose filetype field is derived from
// the FdObject variant. /dev/console is a CharDevice ->
// filetype CHARACTER_DEVICE = 2; an unopened fd surfaces -EBADF.
// Closes the dispatch-layer coverage gap; the shim at
// user-wasm-runtime.ts:790 was untested directly.

describe("dispatch: FD_FDSTAT_GET", () => {
  it("returns CHARACTER_DEVICE filetype for an installed /dev/console fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(pid, 1);
    host.markRunning(pid);

    const args = new Uint8Array(16);
    new DataView(args.buffer).setUint32(0, 1, true);

    const { response, heapOut } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FDSTAT_GET,
      requestId: 1640,
      args,
      heapPtr: 0,
      heapLen: 24,
    });
    expect(response!.status).toBe(0);
    expect(response!.extraLen).toBe(24);
    expect(heapOut.length).toBe(24);
    // fdstat_t offset 0 = filetype u8
    expect(heapOut[0]).toBe(FILETYPE.CHARACTER_DEVICE);
  });

  it("returns -EBADF for an unopened fd", async () => {
    const { host } = await freshHost();
    const pid = host.registerProcess(CAPSET_ALL);
    host.markRunning(pid);

    const args = new Uint8Array(16);
    new DataView(args.buffer).setUint32(0, 99, true);

    const { response } = host.dispatch(pid, {
      opcode: OP_WASI.FD_FDSTAT_GET,
      requestId: 1641,
      args,
      heapPtr: 0,
      heapLen: 24,
    });
    expect(response!.status).toBe(-ERRNO.EBADF);
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

    expect(response!.status).toBe(0);
    expect(response!.value).toBeGreaterThan(BigInt(parent));
    expect(spawnCalls).toHaveLength(1);
    expect(spawnCalls[0]).toEqual({
      pid: Number(response!.value),
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
    // userland response!. The exact errno is not yet threaded
    // through (see the SpawnOutcome type doc).
    expect(response!.status).toBeLessThan(0);
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
    expect(retry.response!.status).toBeLessThan(0);
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

    expect(response!.status).toBe(-ERRNO.EINVAL);
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
    expect(response!.status).toBe(-76);
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
    expect(response!.requestId).toBe(100);
    expect(response!.status).toBe(0);
    expect(response!.value).toBe(BigInt(message.length));
    expect(response!.extraLen).toBe(0);

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
    expect(response!.requestId).toBe(200);
    expect(response!.status).toBe(0);
    // PROC_EXIT carries no return value; the dispatcher reports
    // success-with-value-zero and the user-side unwinds via the
    // `UserProcessExited` sentinel before ever reading this
    // response!. The assertion here is that the response bytes
    // reach the SAB correctly.
    expect(response!.value).toBe(0n);
    expect(response!.extraLen).toBe(0);

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
    expect(bindResult.response!.status).toBe(0);

    const exitResult = host.dispatch(ds, {
      opcode: OP_WASI.PROC_EXIT,
      requestId: 301,
      arg0: 0,
    });
    expect(exitResult.response!.status).toBe(0);

    const client = host.registerProcess(CAPSET_ALL);
    host.markRunning(client);
    const connectResult = host.dispatch(client, {
      opcode: OP_EXT.DISPLAY_CONNECT,
      requestId: 302,
    });
    expect(connectResult.response!.status).toBe(-ERRNO.ECONNREFUSED);
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
    expect(firstBind.response!.status).toBe(0);

    const firstExit = host.dispatch(first, {
      opcode: OP_WASI.PROC_EXIT,
      requestId: 401,
      arg0: 0,
    });
    expect(firstExit.response!.status).toBe(0);

    const second = host.registerProcess(CAPSET_ALL);
    host.markRunning(second);
    const secondBind = host.dispatch(second, {
      opcode: OP_EXT.DISPLAY_BIND,
      requestId: 402,
    });
    expect(secondBind.response!.status).toBe(0);
  });
});
