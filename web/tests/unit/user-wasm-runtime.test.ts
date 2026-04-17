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
  type SpawnOutcome,
} from "../../src/kernel-wasm-host";
import { FramebufferDriver } from "../../src/drivers/fb";
import {
  KernelWasmHostBackend,
  UserWasmRuntime,
} from "../../src/user-wasm-runtime";
import {
  CAPSET_ALL,
  DEV,
  encodeSpawnManifest,
  ERRNO,
  OP_EXT,
} from "../../src/shared/syscall";

/**
 * Test-local replacement for the preview-era
 * `KernelWasmHost.drainPendingSpawns` (removed in T235). Composition
 * tests accumulate each spawn the kernel hands out into `captures`
 * via a custom [`onSpawnProcess`] callback, then call `runAllSpawns`
 * once to run every captured child to completion via a fresh
 * `UserWasmRuntime + KernelWasmHostBackend` per pid. Reentrant: if a
 * running child issues its own `PROC_SPAWN` syscalls mid-run, the
 * kernel's `onSpawnProcess` appends to the SAME `captures` array
 * that the drain loop is currently popping from, so transitive
 * spawns are picked up on subsequent loop iterations — exactly the
 * behaviour the old production drain had. Sequential by design, one
 * runtime at a time. Returns a per-child history with pid/path/
 * exitCode.
 */
interface CapturedSpawn {
  readonly pid: number;
  readonly path: string;
  readonly bytes: BufferSource;
}
async function runAllSpawns(
  kernel: KernelWasmHost,
  captures: CapturedSpawn[],
): Promise<Array<{ pid: number; path: string; exitCode: number }>> {
  const history: Array<{ pid: number; path: string; exitCode: number }> = [];
  while (captures.length > 0) {
    const spawn = captures.shift()!;
    const backend = new KernelWasmHostBackend(kernel, spawn.pid);
    const runtime = new UserWasmRuntime(spawn.bytes, backend);
    const exitCode = await runtime.run();
    history.push({ pid: spawn.pid, path: spawn.path, exitCode });
  }
  return history;
}

/**
 * Build an `onSpawnProcess` callback that resolves `path` against
 * `registry`, pushes `{pid, path, bytes}` into `captures`, and
 * returns `{ ok: true }` on hit or `{ ok: false, errno: ENOENT }`
 * on miss. The kernel maps the miss to `-EIO` on the `PROC_SPAWN`
 * response and rolls back the pid, matching the production path's
 * missing-binary semantics (bootstrap.ts's spawn router exposes the
 * same failure shape).
 */
function captureSpawn(
  registry: ReadonlyMap<string, BufferSource>,
  captures: CapturedSpawn[],
): (pid: number, path: string) => SpawnOutcome {
  return (pid: number, path: string): SpawnOutcome => {
    const bytes = registry.get(path);
    if (bytes === undefined) {
      return { ok: false, errno: ERRNO.ENOENT };
    }
    captures.push({ pid, path, bytes });
    return { ok: true };
  };
}

let kernelWasmBytes: ArrayBuffer;
let helloWasmBytes: ArrayBuffer;
let spawnerWasmBytes: ArrayBuffer;
let ipcSelfTestWasmBytes: ArrayBuffer;
let helloFramebufferWasmBytes: ArrayBuffer;
let displayServerLiteWasmBytes: ArrayBuffer;
let helloWasiBootstrapWasmBytes: ArrayBuffer;
let helloFbBlitWasmBytes: ArrayBuffer;
let helloInputEchoWasmBytes: ArrayBuffer;
let helloStdWasmBytes: ArrayBuffer;
let initWasmBytes: ArrayBuffer;

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
  const helloFramebufferPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_framebuffer.wasm",
  );
  const displayServerLitePath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/display_server_lite.wasm",
  );
  const helloWasiBootstrapPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_wasi_bootstrap.wasm",
  );
  const helloFbBlitPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_fb_blit.wasm",
  );
  const helloInputEchoPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_input_echo.wasm",
  );
  // `hello-std` is a bin target (not cdylib), so cargo keeps the
  // dashes in the output filename.
  const helloStdPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello-std.wasm",
  );
  // `init` is also a bin target, no dash-preservation concerns.
  const initPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/init.wasm",
  );

  for (const p of [
    kernelPath,
    helloPath,
    spawnerPath,
    ipcSelfTestPath,
    helloFramebufferPath,
    displayServerLitePath,
    helloWasiBootstrapPath,
    helloFbBlitPath,
    helloInputEchoPath,
    helloStdPath,
    initPath,
  ]) {
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
  helloFramebufferWasmBytes = loadWasm(helloFramebufferPath);
  displayServerLiteWasmBytes = loadWasm(displayServerLitePath);
  helloWasiBootstrapWasmBytes = loadWasm(helloWasiBootstrapPath);
  helloFbBlitWasmBytes = loadWasm(helloFbBlitPath);
  helloInputEchoWasmBytes = loadWasm(helloInputEchoPath);
  helloStdWasmBytes = loadWasm(helloStdPath);
  initWasmBytes = loadWasm(initPath);
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

  it("PROC_SPAWN + runAllSpawns actually runs the child binary end-to-end", async () => {
    // The composition test: the kernel's `PROC_SPAWN` opcode is
    // dispatched on behalf of a "virtual parent" (no wasm; the
    // test plays init's role), a caller-supplied `onSpawnProcess`
    // captures the spawn, and the test's `runAllSpawns` helper
    // runs the child to completion via `KernelWasmHostBackend`.
    // This proves:
    //
    //   * `onSpawnProcess` wiring through the host — the callback
    //     fires on a real `PROC_SPAWN` syscall with the kernel-
    //     allocated pid and the caller's path.
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
    const captures: CapturedSpawn[] = [];

    // Map the path the parent will pass to PROC_SPAWN onto the
    // hello-wasi-min bytes already loaded in beforeAll.
    const binaryRegistry = new Map<string, BufferSource>([
      ["/usr/bin/hello", helloWasmBytes],
    ]);

    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
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

    // The spawn was captured, not yet run. The response carries
    // the new pid; the capture callback returned `{ ok: true }`
    // so the kernel accepted the spawn.
    expect(spawnResult.response.status).toBe(0);
    const childPid = Number(spawnResult.response.value);
    expect(childPid).toBeGreaterThan(parent);
    expect(captures).toHaveLength(1);
    expect(captures[0]!.pid).toBe(childPid);
    expect(captures[0]!.path).toBe("/usr/bin/hello");
    expect(consoleWrites).toHaveLength(0);

    // Drain the captures. The hello binary runs, writes its line,
    // and exits. `runAllSpawns` is sequential: it returns only
    // after every transitively-captured child has finished, which
    // for this test is just the one.
    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);
    expect(consoleWrites).toHaveLength(1);
    expect(new TextDecoder().decode(consoleWrites[0]!)).toBe(
      "hello from userland\n",
    );
  });

  it("two-level composition: spawner wasm calls proc_spawn mid-run, runAllSpawns reentrantly runs both", async () => {
    // The reentrancy test. Init dispatches PROC_SPAWN for the
    // spawner. `runAllSpawns` pops the spawner, runs it. Inside
    // `_start`, the spawner writes "spawner alive\n" via
    // `wasi_snapshot_preview1.fd_write` and THEN calls
    // `pmos_ext.proc_spawn` to spawn hello. The shim translates
    // that into a PROC_SPAWN opcode dispatched on the spawner's
    // pid — which appends the hello spawn onto the same
    // `captures` array the loop is currently draining. The
    // spawner then proc_exits. Control returns to the drain
    // loop, which sees the array is non-empty (hello just got
    // added), pops hello, runs it. Hello writes its line and
    // exits. Drain returns.
    //
    // Asserts: BOTH console writes appear, IN ORDER ("spawner
    // alive\n" first, then "hello from userland\n"). That order
    // is load-bearing: it proves the spawner fully ran before
    // the child took over, which is the non-concurrent
    // sequential-drain semantics the helper preserves.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/usr/bin/hello", helloWasmBytes],
      ["/usr/bin/spawner", spawnerWasmBytes],
    ]);

    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
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

    // One captured spawn (the spawner); hello isn't captured yet
    // because it only gets captured when the spawner calls
    // pmos_ext.proc_spawn during its run.
    expect(captures).toHaveLength(1);

    // Drain. The spawner runs, appends hello mid-run, exits;
    // the loop picks up hello, runs it, exits; the loop sees
    // an empty array and returns.
    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history.map((h) => h.path)).toEqual([
      "/usr/bin/spawner",
      "/usr/bin/hello",
    ]);
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

  it("PROC_SPAWN with a path missing from the capture registry returns -EIO and rolls back the pid", async () => {
    // Missing-binary path: `captureSpawn` returns
    // `{ ok: false, errno: ENOENT }`, WasmPlatform::spawn_process
    // maps that to `DriverError::Errno`, the PROC_SPAWN opcode
    // handler rolls back the pid and returns `-EIO`. Nothing is
    // pushed into `captures` because the callback returned
    // `ok: false` before appending.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];

    const binaryRegistry = new Map<string, BufferSource>([
      ["/usr/bin/hello", helloWasmBytes],
    ]);

    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
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
    expect(captures).toHaveLength(0);
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
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/ipc-self-test", ipcSelfTestWasmBytes],
    ]);

    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
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

    // The binary exits 0 iff every IPC step succeeded. The exit
    // code shows up in `history` below; the side effect — the
    // received bytes landing on /dev/console via the final
    // fd_write — is the primary acceptance assertion. A silent
    // failure (no console write) would mean some step bailed
    // early with a non-zero proc_exit before reaching the echo.
    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);
    expect(consoleWrites).toHaveLength(1);
    expect(new TextDecoder().decode(consoleWrites[0]!)).toBe(
      "hello via ipc\n",
    );
  });

  it("hello-framebuffer writes RGBA pixel bytes to /dev/fb0 and the TS host's onFramebufferWrite callback observes them", async () => {
    // The framebuffer-pipeline acceptance test. A user wasm
    // binary does `path_open("/dev/fb0")` (which requires the
    // `DisplayServer` cap per `DeviceDispatcher::check_open`)
    // and `fd_write(fd, pixels)`. The kernel routes the write
    // through `framebuffer_write` →
    // `platform::current().driver_call(Framebuffer, ...)` →
    // `pmos_host_driver_call` host import → `KernelWasmHost`'s
    // routing closure → `options.onFramebufferWrite(bytes)`.
    //
    // Asserts the callback received the exact 16 bytes the
    // binary wrote (4 RGBA pixels: red, green, blue, white).
    // Failure modes:
    //   * binary exit code 10 = path_open failed (capability
    //     bug or devfs regression)
    //   * binary exit code 11 = fd_write returned nonzero or
    //     short-wrote
    //   * callback not invoked = the
    //     kernel → host_driver_call → callback route broke
    //   * bytes mismatch = the route delivered the wrong
    //     bytes (e.g. stale buffer after a grow)
    const consoleWrites: Uint8Array[] = [];
    const fbWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-framebuffer", helloFramebufferWasmBytes],
    ]);

    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      onFramebufferWrite: (bytes) => {
        fbWrites.push(bytes);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
    });

    // Virtual init with CAPSET_ALL so the child inherits
    // `DisplayServer` (required to open /dev/fb0).
    const init = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(init, 0);
    kernel.installConsoleFd(init, 1);
    kernel.installConsoleFd(init, 2);
    kernel.markRunning(init);

    const manifest = encodeSpawnManifest({
      path: "/bin/hello-framebuffer",
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

    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);
    // No console output — the binary doesn't write to /dev/console.
    expect(consoleWrites).toHaveLength(0);
    // Exactly one framebuffer write with the 16 RGBA bytes.
    expect(fbWrites).toHaveLength(1);
    expect(Array.from(fbWrites[0]!)).toEqual([
      0xff, 0x00, 0x00, 0xff, // red
      0x00, 0xff, 0x00, 0xff, // green
      0x00, 0x00, 0xff, 0xff, // blue
      0xff, 0xff, 0xff, 0xff, // white
    ]);
  });

  it("display-server-lite composes bind + connect + accept + fd_read + path_open + fd_write end-to-end, pixels reach the framebuffer via the display socket", async () => {
    // The full-pipeline acceptance test. A single binary plays
    // server, client, AND framebuffer-writer by:
    //
    //   1. display_bind() as server
    //   2. display_connect() as client
    //   3. ipc_accept() as server
    //   4. fd_write(client, pixels) — pixels cross the kernel
    //      IPC boundary
    //   5. fd_read(server, buf) — server receives same bytes
    //      back
    //   6. path_open("/dev/fb0") — server opens the framebuffer
    //      device
    //   7. fd_write(fb_fd, buf) — server relays the pixels it
    //      received over IPC to the framebuffer device
    //
    // The FB write at step 7 uses the buffer populated by step
    // 5 (NOT the original `PIXELS` constant), so the bytes
    // that reach `onFramebufferWrite` are proof that the IPC
    // round-trip actually transported the payload — a
    // regression that broke IPC but left everything else intact
    // would produce either the wrong bytes or an exit code 14.
    const consoleWrites: Uint8Array[] = [];
    const fbWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/display-server-lite", displayServerLiteWasmBytes],
    ]);

    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      onFramebufferWrite: (bytes) => {
        fbWrites.push(bytes);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
    });

    // Virtual init with CAPSET_ALL — the demo binary needs both
    // DisplayServer (for display_bind + /dev/fb0) and DisplayClient
    // (for display_connect) caps, which CAPSET_ALL includes.
    const init = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(init, 0);
    kernel.installConsoleFd(init, 1);
    kernel.installConsoleFd(init, 2);
    kernel.markRunning(init);

    const manifest = encodeSpawnManifest({
      path: "/bin/display-server-lite",
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

    // Exit code is the step-by-step diagnostic; exit 0 means every
    // step succeeded. Codes 10..16 indicate which step bailed —
    // see `display-server-lite/src/lib.rs` for the map.
    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);
    // No console output — the binary doesn't write to /dev/console.
    expect(consoleWrites).toHaveLength(0);
    // Exactly one framebuffer write with the 4 RGBA pixels that
    // traversed the IPC socket.
    expect(fbWrites).toHaveLength(1);
    expect(Array.from(fbWrites[0]!)).toEqual([
      0xff, 0x00, 0x00, 0xff, // red
      0x00, 0xff, 0x00, 0xff, // green
      0x00, 0x00, 0xff, 0xff, // blue
      0xff, 0xff, 0xff, 0xff, // white
    ]);
  });

  it("hello-wasi-bootstrap exercises args_sizes/args_get/environ_sizes/environ_get/fd_fdstat_get/fd_prestat_get end-to-end through real wasm", async () => {
    // All six handlers from the "kernel opcode breadth #3" slice
    // exercised through a real user binary:
    //
    //   * args_sizes_get  → (0, 0)
    //   * args_get        → success (nothing to write, argc=0)
    //   * environ_sizes_get → (0, 0)
    //   * environ_get     → success (nothing to write, envc=0)
    //   * fd_fdstat_get(1) → filetype byte == 2 (CharDevice)
    //   * fd_prestat_get(3) → errno 8 (EBADF)
    //
    // If any of those returns something the binary doesn't expect,
    // `_start` calls proc_exit with a step-specific code (10..16)
    // and the test fails. On full success the binary writes
    // "bootstrap ok\n" to /dev/console and exits 0.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-wasi-bootstrap", helloWasiBootstrapWasmBytes],
    ]);
    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
    });

    const init = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(init, 0);
    kernel.installConsoleFd(init, 1);
    kernel.installConsoleFd(init, 2);
    kernel.markRunning(init);

    const manifest = encodeSpawnManifest({
      path: "/bin/hello-wasi-bootstrap",
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

    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);
    // The binary writes exactly one line on full success.
    const combined = new TextDecoder().decode(
      new Uint8Array(
        consoleWrites.reduce<number[]>(
          (acc, b) => acc.concat(Array.from(b)),
          [],
        ),
      ),
    );
    expect(combined).toBe("bootstrap ok\n");
  });

  it("hello-fb-blit + FramebufferDriver decodes OP_SET_MODE + OP_BLIT into fb:set-mode + fb:blit messages through KernelWasmHost.framebufferDriver", async () => {
    // The driver-framed framebuffer path: user wasm writes a
    // `[op, ...payload]` buffer to /dev/fb0, KernelWasmHost's
    // driver_call handler strips the op byte and calls
    // `FramebufferDriver.call(op, payload)`, the driver decodes the
    // typed message and posts it through its `DriverHost` — which
    // the host wires to `options.onFramebufferMessage`.
    //
    // This closes the gap between the raw-bytes `onFramebufferWrite`
    // callback (what hello-framebuffer + display-server-lite use)
    // and the typed `fb:set-mode` / `fb:blit` surface that FbHost
    // + canvas blit already consume on the mock-kernel path. After
    // this slice, a future display-server binary can talk OP_BLIT
    // to /dev/fb0 and its output reaches the same main-thread
    // handler chain a MockKernel blit does.
    const driverMessages: unknown[] = [];
    const fbDriver = new FramebufferDriver();
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-fb-blit", helloFbBlitWasmBytes],
    ]);
    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      framebufferDriver: fbDriver,
      onFramebufferMessage: (msg) => {
        driverMessages.push(msg);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
    });

    const init = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(init, 0);
    kernel.installConsoleFd(init, 1);
    kernel.installConsoleFd(init, 2);
    kernel.markRunning(init);

    const manifest = encodeSpawnManifest({
      path: "/bin/hello-fb-blit",
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

    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    // Two messages: fb:set-mode then fb:blit, in that order.
    expect(driverMessages).toHaveLength(2);
    const setMode = driverMessages[0] as {
      kind: string;
      width: number;
      height: number;
    };
    expect(setMode.kind).toBe("fb:set-mode");
    expect(setMode.width).toBe(2);
    expect(setMode.height).toBe(2);

    const blit = driverMessages[1] as {
      kind: string;
      width: number;
      height: number;
      rgba: Uint8Array;
    };
    expect(blit.kind).toBe("fb:blit");
    expect(blit.width).toBe(2);
    expect(blit.height).toBe(2);
    expect(Array.from(blit.rgba)).toEqual([
      0xff, 0x00, 0x00, 0xff, // red
      0x00, 0xff, 0x00, 0xff, // green
      0x00, 0x00, 0xff, 0xff, // blue
      0xff, 0xff, 0xff, 0xff, // white
    ]);
  });

  it("hello-input-echo reads injected keyboard bytes from /dev/input/kbd and echoes them to /dev/console", async () => {
    // The input path end-to-end:
    //   KernelWasmHost.injectInput(DEV.INPUT_KBD, bytes) →
    //   kernel_inject_input_kbd → inject_kbd_event → input ring →
    //   binary's path_open + fd_read → fd_write(stdout) →
    //   onConsoleWrite.
    //
    // `fd_read` on an empty input ring returns EAGAIN rather than
    // parking, so this test injects the input BEFORE running the
    // child. The binary's fd_read finds a non-empty ring and returns
    // immediately with the queued bytes.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-input-echo", helloInputEchoWasmBytes],
    ]);
    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
    });

    const init = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(init, 0);
    kernel.installConsoleFd(init, 1);
    kernel.installConsoleFd(init, 2);
    kernel.markRunning(init);

    // Inject the keystrokes that the child will read. The kernel's
    // input ring is format-agnostic — any bytes round-trip verbatim.
    // The trailing newline is mandatory: /dev/console writes are
    // line-buffered and only flush to the driver callback on '\n',
    // so without it the echoed bytes would sit in the kernel's line
    // buffer and never reach onConsoleWrite.
    const kbdBytes = new Uint8Array([0x48, 0x69, 0x21, 0x0a]); // "Hi!\n"
    kernel.injectInput(DEV.INPUT_KBD, kbdBytes);

    const manifest = encodeSpawnManifest({
      path: "/bin/hello-input-echo",
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

    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    // Exactly one console write, containing the four injected bytes
    // ("Hi!\n" — the console driver flushes on newline).
    expect(consoleWrites).toHaveLength(1);
    expect(Array.from(consoleWrites[0]!)).toEqual([0x48, 0x69, 0x21, 0x0a]);
  });

  it("hello-std: a real Rust `std` binary (println! + fn main) runs to completion through the PMos WASI shim", async () => {
    // The capstone test. Unlike every other hello-* crate in this
    // workspace, hello-std is NOT #![no_std]: it uses the full Rust
    // `std` crate with all the libc/WASI startup machinery. The
    // binary cargo produces is 40 KiB (vs. ~800 bytes for the
    // no_std cdylibs) and imports exactly four WASI functions:
    // fd_write, proc_exit, environ_get, environ_sizes_get. All
    // four are wired on both the kernel side (handler in
    // syscall/wasi.rs) and the TS shim side (user-wasm-runtime.ts).
    //
    // This test proves every opcode on the Rust std startup path
    // works end-to-end through real user wasm. If it regresses,
    // the regression is in one of: the kernel opcode handler, the
    // user-runtime shim, or the kernel->TS-host driver_call
    // routing. The failure mode of each is visible from the
    // console output + exit code.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-std", helloStdWasmBytes],
    ]);
    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
    });

    const init = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(init, 0);
    kernel.installConsoleFd(init, 1);
    kernel.installConsoleFd(init, 2);
    kernel.markRunning(init);

    const manifest = encodeSpawnManifest({
      path: "/bin/hello-std",
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

    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    const combined = new TextDecoder().decode(
      new Uint8Array(
        consoleWrites.reduce<number[]>(
          (acc, b) => acc.concat(Array.from(b)),
          [],
        ),
      ),
    );
    expect(combined).toBe("hello from std\n");
  });

  it("init (std) spawns hello-std via pmos_ext.proc_spawn, child runs after init exits", async () => {
    // The first proof that a real Rust `std` binary can issue a
    // PMos extension syscall (not just WASI) and reach a second
    // std binary through the drain loop. The two-level
    // composition was already proven with no_std cdylibs in the
    // earlier `hello-wasi-spawner` test; this is the "both sides
    // are std" progression: init uses `println!` for every line,
    // calls `pmos_ext.proc_spawn` through an `extern "C"` block,
    // and the spawned child is itself a std binary (hello-std)
    // linking its own libc + WASI startup machinery.
    //
    // Ordering is load-bearing: `runAllSpawns` is sequential (one
    // runtime at a time), so hello-std can only start running after
    // init's `main()` returns. The assertion on the console-line
    // order is what certifies that guarantee; a concurrent drain
    // would surface as interleaved output. (Production, post-T234,
    // uses real user Workers that DO run concurrently — the
    // composition-test semantics live only in this in-process
    // `runAllSpawns` helper.)
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/init", initWasmBytes],
      ["/bin/hello-std", helloStdWasmBytes],
    ]);
    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
    });

    // Kernel-side synthetic parent that dispatches PROC_SPAWN on
    // behalf of an imaginary "boot loader" — the real boot path
    // uses `kernel-worker-entry.ts`'s `runBootBinary`, which does
    // the same choreography.
    const bootLoader = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(bootLoader, 0);
    kernel.installConsoleFd(bootLoader, 1);
    kernel.installConsoleFd(bootLoader, 2);
    kernel.markRunning(bootLoader);

    const manifest = encodeSpawnManifest({
      path: "/bin/init",
      caps: CAPSET_ALL,
    });
    const spawnResult = kernel.dispatch(
      bootLoader,
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

    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history).toHaveLength(2);
    expect(history[0]!.path).toBe("/bin/init");
    expect(history[0]!.exitCode).toBe(0);
    expect(history[1]!.path).toBe("/bin/hello-std");
    expect(history[1]!.exitCode).toBe(0);

    const combined = new TextDecoder().decode(
      new Uint8Array(
        consoleWrites.reduce<number[]>(
          (acc, b) => acc.concat(Array.from(b)),
          [],
        ),
      ),
    );
    // init writes three lines; hello-std writes one. The pid the
    // kernel allocates is dynamic, so line 2 matches on prefix.
    const lines = combined.split("\n").filter((l) => l.length > 0);
    expect(lines[0]).toBe("init starting");
    expect(lines[1]).toMatch(/^init spawned hello-std pid=\d+$/);
    expect(lines[2]).toBe("init exiting");
    expect(lines[3]).toBe("hello from std");
    expect(lines).toHaveLength(4);
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
