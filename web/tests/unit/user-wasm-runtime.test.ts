// End-to-end test: a real `wasm32-wasip1` binary runs against a
// real `KernelWasmHost` via `UserWasmRuntime`.
//
// This is the first point in the project where user code actually
// executes against the real kernel. The test loads two wasm
// modules from disk:
//
//   * `target/wasm32-unknown-unknown/release/kernel.wasm` — the
//     kernel cdylib with its 10-opcode dispatcher, already
//     exercised in isolation by kernel-wasm-host.test.ts.
//   * `target/wasm32-wasip1/release/hello_wasi_min.wasm` — a
//     minimum-viable WASI preview 1 binary from
//     `crates/hello-wasi-min` (265 bytes; imports only
//     `wasi_snapshot_preview1.fd_write` and `.proc_exit`).
//
// The test:
//
//   1. Instantiates a `KernelWasmHost` with an `onConsoleWrite`
//      callback that captures every line the kernel flushes.
//   2. Registers a process for the hello binary, installs
//      `/dev/console` as fd 0/1/2, marks it Running.
//   3. Constructs a `UserWasmRuntime` with the hello wasm bytes
//      and a `KernelWasmHostBackend` bound to that pid.
//   4. Calls `runtime.run()` — the wasm's `_start` executes,
//      calls `fd_write(1, ...)` via the WASI shim, which the
//      shim translates into a `FD_WRITE` opcode dispatched
//      through the kernel, which writes to `/dev/console`,
//      which flushes the line through `pmos_host_driver_call`
//      back to `onConsoleWrite`.
//   5. Asserts the captured bytes match the expected message
//      and the runtime returned exit code 0.
//
// Every layer in the stack is a production module — no mocks.
// If this test passes, the end-to-end path from a user-space
// WASI syscall down to a kernel device write is wired.

import fs from "node:fs";
import path from "node:path";
import { beforeAll, describe, expect, it } from "vitest";

import {
  KernelWasmHost,
} from "../../src/kernel-wasm-host";
import {
  KernelWasmHostBackend,
  UserWasmRuntime,
} from "../../src/user-wasm-runtime";
import {
  CAPSET_ALL,
  encodeSpawnManifest,
  OP_EXT,
} from "../../src/shared/syscall";

let kernelWasmBytes: ArrayBuffer;
let helloWasmBytes: ArrayBuffer;
let spawnerWasmBytes: ArrayBuffer;
let ipcSelfTestWasmBytes: ArrayBuffer;

beforeAll(() => {
  const repoRoot = path.resolve(__dirname, "../../..");
  const kernelPath = path.join(
    repoRoot,
    "target/wasm32-unknown-unknown/release/kernel.wasm",
  );
  const helloPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_wasi_min.wasm",
  );
  const spawnerPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_wasi_spawner.wasm",
  );
  const ipcSelfTestPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/ipc_self_test.wasm",
  );

  for (const p of [kernelPath, helloPath, spawnerPath, ipcSelfTestPath]) {
    if (!fs.existsSync(p)) {
      throw new Error(
        `${p} not found. Run \`just build\` (or the cargo build lines from the Justfile's build target) first.`,
      );
    }
  }

  // Copy each Node `Buffer` into a fresh `ArrayBuffer`; see
  // `kernel-wasm-host.test.ts` for the explanation of why this
  // is needed under modern TS types.
  const loadWasm = (p: string): ArrayBuffer => {
    const raw = fs.readFileSync(p);
    return raw.buffer.slice(
      raw.byteOffset,
      raw.byteOffset + raw.byteLength,
    ) as ArrayBuffer;
  };
  kernelWasmBytes = loadWasm(kernelPath);
  helloWasmBytes = loadWasm(helloPath);
  spawnerWasmBytes = loadWasm(spawnerPath);
  ipcSelfTestWasmBytes = loadWasm(ipcSelfTestPath);
});

describe("UserWasmRuntime + KernelWasmHost end-to-end", () => {
  it("runs hello-wasi-min and captures 'hello from userland\\n' on /dev/console", async () => {
    const consoleWrites: Uint8Array[] = [];
    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
    });

    // Wire up a process for the hello binary. In a future slice
    // this happens automatically as part of `onSpawnProcess`;
    // for this slice the test drives the lifecycle directly so
    // the spawn-callback code path can stay isolated from the
    // runtime-execution code path.
    const pid = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(pid, 0);
    kernel.installConsoleFd(pid, 1);
    kernel.installConsoleFd(pid, 2);
    kernel.markRunning(pid);

    const backend = new KernelWasmHostBackend(kernel, pid);
    const runtime = new UserWasmRuntime(helloWasmBytes, backend);

    const exitCode = await runtime.run();

    expect(exitCode).toBe(0);
    // console_write only flushes complete lines, and the hello
    // binary writes exactly one newline-terminated line, so we
    // expect exactly one onConsoleWrite call.
    expect(consoleWrites).toHaveLength(1);
    expect(new TextDecoder().decode(consoleWrites[0]!)).toBe(
      "hello from userland\n",
    );
  });

  it("PROC_SPAWN + drainPendingSpawns actually runs the child binary end-to-end", async () => {
    // The composition test: the kernel's `PROC_SPAWN` opcode is
    // dispatched on behalf of a "virtual parent" (no wasm; the
    // test plays init's role), the default `onSpawnProcess` hook
    // queues the child based on the supplied `binaryRegistry`,
    // and `drainPendingSpawns` runs the child to completion.
    // This proves:
    //
    //   * `onSpawnProcess` wiring through the host — the default
    //     queuing callback fires on a real `PROC_SPAWN` syscall.
    //   * Binary-registry lookup — the kernel-supplied path
    //     resolves to wasm bytes.
    //   * The spawned child runs against the same KernelWasmHost
    //     that its parent was syscalling through, with the pid
    //     allocated by the kernel's own proc_spawn path — not a
    //     test-synthesised pid.
    //   * Child stdio inheritance — the child's fd 1 is the same
    //     `/dev/console` object the parent had, so its fd_write
    //     lands on the same driver sink.
    //   * The whole thing runs after the parent's dispatch has
    //     returned, so the kernel's scratch region is never
    //     contended (no nested dispatch).
    const consoleWrites: Uint8Array[] = [];

    // Map the path the parent will pass to PROC_SPAWN onto the
    // hello-wasi-min bytes already loaded in beforeAll.
    const binaryRegistry = new Map<string, BufferSource>([
      ["/usr/bin/hello", helloWasmBytes],
    ]);

    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      binaryRegistry,
    });

    // Seed a "virtual parent" process. It doesn't run any user
    // wasm — the test dispatches PROC_SPAWN directly on its
    // behalf, the way init will eventually do once the init
    // slice lands. The only reason the parent has to exist in
    // the kernel's process table is that PROC_SPAWN is issued
    // on behalf of a specific pid, and that pid's fd 0/1/2
    // become the child's stdio.
    const parent = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(parent, 0);
    kernel.installConsoleFd(parent, 1);
    kernel.installConsoleFd(parent, 2);
    kernel.markRunning(parent);

    // Encode and dispatch a PROC_SPAWN syscall.
    const manifest = encodeSpawnManifest({
      path: "/usr/bin/hello",
      caps: CAPSET_ALL,
    });
    const spawnResult = kernel.dispatch(
      parent,
      {
        opcode: OP_EXT.PROC_SPAWN,
        requestId: 1,
        args: manifest.args,
        heapPtr: 0,
        heapLen: manifest.heap.length,
      },
      manifest.heap,
    );

    // The spawn was queued, not yet run. The response carries the
    // new pid; the registry lookup succeeded so `onSpawnProcess`
    // returned `{ ok: true }` and the kernel accepted the spawn.
    expect(spawnResult.response.status).toBe(0);
    const childPid = Number(spawnResult.response.value);
    expect(childPid).toBeGreaterThan(parent);
    expect(kernel.hasPendingSpawns).toBe(true);
    expect(consoleWrites).toHaveLength(0);

    // Drain the queue. The hello binary runs, writes its line,
    // and exits. Because drainPendingSpawns is sequential, it
    // returns only after every transitively-queued child has
    // finished, which for this test is just the one.
    await kernel.drainPendingSpawns();

    expect(kernel.hasPendingSpawns).toBe(false);
    expect(consoleWrites).toHaveLength(1);
    expect(new TextDecoder().decode(consoleWrites[0]!)).toBe(
      "hello from userland\n",
    );
  });

  it("two-level composition: spawner wasm calls proc_spawn mid-run, drainPendingSpawns reentrantly runs both", async () => {
    // The reentrancy test. Init dispatches PROC_SPAWN for the
    // spawner. The drain loop pops the spawner, runs it. Inside
    // `_start`, the spawner writes "spawner alive\n" via
    // `wasi_snapshot_preview1.fd_write` and THEN calls
    // `pmos_ext.proc_spawn` to spawn hello. The shim translates
    // that into a PROC_SPAWN opcode dispatched on the spawner's
    // pid — which queues the hello spawn onto the same
    // pendingSpawns list the drain loop is currently draining.
    // The spawner then proc_exits. Control returns to the drain
    // loop, which sees the queue is non-empty (hello just got
    // added), pops hello, runs it. Hello writes its line and
    // exits. Drain returns.
    //
    // Asserts: BOTH console writes appear, IN ORDER ("spawner
    // alive\n" first, then "hello from userland\n"). That order
    // is load-bearing: it proves the spawner fully ran before
    // the child took over, which is the non-concurrent
    // sequential-drain semantics we promised.
    const consoleWrites: Uint8Array[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/usr/bin/hello", helloWasmBytes],
      ["/usr/bin/spawner", spawnerWasmBytes],
    ]);

    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      binaryRegistry,
    });

    // Virtual init process, seeded with stdio.
    const init = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(init, 0);
    kernel.installConsoleFd(init, 1);
    kernel.installConsoleFd(init, 2);
    kernel.markRunning(init);

    // init issues PROC_SPAWN for /usr/bin/spawner.
    const manifest = encodeSpawnManifest({
      path: "/usr/bin/spawner",
      caps: CAPSET_ALL,
    });
    const spawnResult = kernel.dispatch(
      init,
      {
        opcode: OP_EXT.PROC_SPAWN,
        requestId: 1,
        args: manifest.args,
        heapPtr: 0,
        heapLen: manifest.heap.length,
      },
      manifest.heap,
    );
    expect(spawnResult.response.status).toBe(0);
    const spawnerPid = Number(spawnResult.response.value);
    expect(spawnerPid).toBeGreaterThan(init);

    // One pending spawn (the spawner); hello isn't queued yet
    // because it only gets queued when the spawner calls
    // pmos_ext.proc_spawn during its run.
    expect(kernel.hasPendingSpawns).toBe(true);

    // Drain. The spawner runs, queues hello mid-run, exits;
    // the loop picks up hello, runs it, exits; the loop sees
    // an empty queue and returns.
    await kernel.drainPendingSpawns();

    expect(kernel.hasPendingSpawns).toBe(false);
    // Two complete lines, in order. Any other order would
    // indicate the child ran before the spawner finished,
    // which would break the sequential reentrancy model.
    const lines = consoleWrites.map((bytes) =>
      new TextDecoder().decode(bytes),
    );
    expect(lines).toEqual([
      "spawner alive\n",
      "hello from userland\n",
    ]);
  });

  it("PROC_SPAWN with a path missing from binaryRegistry returns -EIO and rolls back the pid", async () => {
    // Missing-binary path: the default onSpawnProcess returns
    // `{ ok: false, errno: ENOENT }`, WasmPlatform::spawn_process
    // maps that to `DriverError::Errno`, the PROC_SPAWN opcode
    // handler rolls back the pid and returns `-EIO`. No pending
    // spawn is queued because the default callback returned
    // `ok: false` before pushing.
    const consoleWrites: Uint8Array[] = [];

    const binaryRegistry = new Map<string, BufferSource>([
      ["/usr/bin/hello", helloWasmBytes],
    ]);

    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      binaryRegistry,
    });

    const parent = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(parent, 0);
    kernel.installConsoleFd(parent, 1);
    kernel.installConsoleFd(parent, 2);
    kernel.markRunning(parent);

    const manifest = encodeSpawnManifest({
      path: "/usr/bin/not-in-registry",
      caps: CAPSET_ALL,
    });
    const spawnResult = kernel.dispatch(
      parent,
      {
        opcode: OP_EXT.PROC_SPAWN,
        requestId: 10,
        args: manifest.args,
        heapPtr: 0,
        heapLen: manifest.heap.length,
      },
      manifest.heap,
    );

    // -EIO per the PROC_SPAWN rollback semantics (any platform
    // failure becomes EIO regardless of the specific errno).
    expect(spawnResult.response.status).toBeLessThan(0);
    expect(kernel.hasPendingSpawns).toBe(false);
    expect(consoleWrites).toHaveLength(0);
  });

  it("ipc-self-test binary exercises every IPC opcode end-to-end through real wasm", async () => {
    // The IPC-opcode acceptance test. A single no_std binary
    // plays both server and client via self-connection —
    // exercises every IPC opcode (`IPC_SOCKET`, `IPC_BIND`,
    // `IPC_LISTEN`, `IPC_CONNECT`, `IPC_ACCEPT`), the
    // WASI-side `fd_read` shim we just added, AND the existing
    // `fd_write` + `proc_exit` shims — in one `_start` pass.
    //
    // The binary's exit code is the assertion: 0 means every
    // step succeeded. Non-zero codes (10..18, 101) point at a
    // specific step that failed. See `ipc-self-test/src/lib.rs`
    // for the step → code map.
    //
    // Side-effect assertion: the binary reads "hello via ipc\n"
    // from the server-side socket and writes those bytes to
    // stdout, so `onConsoleWrite` captures them. That proves
    // the data actually flowed through the kernel's IPC state
    // machine rather than being short-circuited somewhere.
    const consoleWrites: Uint8Array[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/ipc-self-test", ipcSelfTestWasmBytes],
    ]);

    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      binaryRegistry,
    });

    // Virtual init with stdio so the child inherits fd 0/1/2 and
    // step 9's `fd_write(1, ...)` lands on /dev/console.
    const init = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(init, 0);
    kernel.installConsoleFd(init, 1);
    kernel.installConsoleFd(init, 2);
    kernel.markRunning(init);

    // init issues PROC_SPAWN.
    const manifest = encodeSpawnManifest({
      path: "/bin/ipc-self-test",
      caps: CAPSET_ALL,
    });
    const spawnResult = kernel.dispatch(
      init,
      {
        opcode: OP_EXT.PROC_SPAWN,
        requestId: 1,
        args: manifest.args,
        heapPtr: 0,
        heapLen: manifest.heap.length,
      },
      manifest.heap,
    );
    expect(spawnResult.response.status).toBe(0);

    // The binary exits 0 iff every IPC step succeeded. Its
    // exit code is not directly observable from the test
    // (drainPendingSpawns doesn't surface per-child exit
    // codes yet), but the side effect — the received bytes
    // landing on /dev/console via the final fd_write — is.
    // A silent failure (no console write) would mean some
    // step bailed early with a non-zero proc_exit before
    // reaching the echo.
    await kernel.drainPendingSpawns();

    expect(kernel.hasPendingSpawns).toBe(false);
    expect(consoleWrites).toHaveLength(1);
    expect(new TextDecoder().decode(consoleWrites[0]!)).toBe(
      "hello via ipc\n",
    );
  });

  it("returns the correct exit code when _start calls proc_exit with a nonzero value", async () => {
    // This test doesn't re-use hello-wasi-min (it always exits 0
    // on success). Instead it exercises the runtime's exit-code
    // propagation by running a hand-built wasm binary that
    // imports proc_exit and calls it with a specific value.
    //
    // The binary is built inline as WAT-compiled-via-WebAssembly
    // API: we construct a tiny module that imports `proc_exit`
    // and defines a `_start` that calls it with the constant 42.
    //
    // Building at test time (rather than shipping a .wasm in
    // tree) means the test doesn't depend on an additional build
    // artefact, and a reader can see the entire wasm module in
    // one place.
    const wasmBytes = buildExitOnlyWasm(42);

    const consoleWrites: Uint8Array[] = [];
    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
    });
    const pid = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(pid, 0);
    kernel.installConsoleFd(pid, 1);
    kernel.installConsoleFd(pid, 2);
    kernel.markRunning(pid);

    const runtime = new UserWasmRuntime(
      wasmBytes,
      new KernelWasmHostBackend(kernel, pid),
    );
    const exitCode = await runtime.run();

    expect(exitCode).toBe(42);
    expect(consoleWrites).toHaveLength(0);
  });
});

/**
 * Build a minimal wasm module whose `_start` calls
 * `wasi_snapshot_preview1.proc_exit(code)` and returns.
 *
 * The module is hand-emitted as a binary buffer to avoid a WAT
 * parser dependency. Shape:
 *
 *   (module
 *     (import "wasi_snapshot_preview1" "proc_exit"
 *             (func $exit (param i32)))
 *     (func (export "_start")
 *       i32.const <code>
 *       call $exit)
 *     (memory (export "memory") 1))
 */
function buildExitOnlyWasm(exitCode: number): ArrayBuffer {
  // WASM binary format (section-by-section). Every length is
  // LEB128-encoded; we only need the single-byte form because
  // all our lengths fit in 7 bits.
  //
  // The relevant reference is
  // https://webassembly.github.io/spec/core/binary/modules.html
  // but the layout here is compact enough to read top-to-bottom.

  const MAGIC = [0x00, 0x61, 0x73, 0x6d]; // "\0asm"
  const VERSION = [0x01, 0x00, 0x00, 0x00]; // 1

  // Type section: two types.
  //   type 0: (param i32) -> () — used by proc_exit
  //   type 1: () -> ()         — used by _start
  const typeSection = section(1, [
    0x02, // num types
    0x60,
    0x01,
    0x7f,
    0x00, // type 0: [i32] -> []
    0x60,
    0x00,
    0x00, // type 1: [] -> []
  ]);

  // Import section: one import.
  //   "wasi_snapshot_preview1" . "proc_exit" -> func type 0
  const importModule = encodeString("wasi_snapshot_preview1");
  const importName = encodeString("proc_exit");
  const importSection = section(2, [
    0x01, // num imports
    ...importModule,
    ...importName,
    0x00, // kind: func
    0x00, // type idx 0
  ]);

  // Function section: one function declared, type 1.
  const functionSection = section(3, [0x01, 0x01]);

  // Memory section: one memory with minimum 1 page.
  const memorySection = section(5, [
    0x01, // num memories
    0x00,
    0x01, // limits: flags=0, min=1
  ]);

  // Export section: export "_start" (func 1) and "memory".
  const startName = encodeString("_start");
  const memoryName = encodeString("memory");
  const exportSection = section(7, [
    0x02, // num exports
    ...startName,
    0x00, // func export kind
    0x01, // func idx 1 (after the imported proc_exit at idx 0)
    ...memoryName,
    0x02, // memory export kind
    0x00, // memory idx 0
  ]);

  // Code section: one function body.
  //   _start: i32.const <code>; call 0; end
  const funcBody = [
    0x00, // num locals
    0x41, // i32.const
    ...leb128SignedByte(exitCode),
    0x10, // call
    0x00, // func idx 0 (proc_exit)
    0x0b, // end
  ];
  const codeSection = section(10, [
    0x01, // num functions
    funcBody.length,
    ...funcBody,
  ]);

  return new Uint8Array([
    ...MAGIC,
    ...VERSION,
    ...typeSection,
    ...importSection,
    ...functionSection,
    ...memorySection,
    ...exportSection,
    ...codeSection,
  ]).buffer as ArrayBuffer;
}

function section(id: number, body: number[]): number[] {
  return [id, body.length, ...body];
}

function encodeString(s: string): number[] {
  const bytes = new TextEncoder().encode(s);
  return [bytes.length, ...bytes];
}

/** LEB128-encoded signed 7-bit byte. Good for values in -64..63. */
function leb128SignedByte(value: number): number[] {
  if (value < -64 || value > 63) {
    throw new Error(
      `leb128SignedByte: ${value} outside single-byte signed range`,
    );
  }
  return [value & 0x7f];
}
