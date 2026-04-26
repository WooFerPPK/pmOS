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
import { Devnum } from "../../src/shared/platform-constants";
import {
  KernelWasmHostBackend,
  UserWasmRuntime,
} from "../../src/user-wasm-runtime";
import {
  CAPSET_ALL,
  CAPSET_ORDINARY_APP,
  DEV,
  encodeSpawnManifest,
  ERRNO,
  OP_EXT,
  OP_WASI,
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
let helloSigchldWasmBytes: ArrayBuffer;
let helloKillProbeWasmBytes: ArrayBuffer;
let helloPidWasmBytes: ArrayBuffer;
let helloSelfProbeWasmBytes: ArrayBuffer;
let helloPpidWasmBytes: ArrayBuffer;
let helloCapsWasmBytes: ArrayBuffer;
let helloRaiseWasmBytes: ArrayBuffer;
let helloWaitNoopWasmBytes: ArrayBuffer;
let helloCapCheckWasmBytes: ArrayBuffer;
let helloRandomWasmBytes: ArrayBuffer;
let helloFdCloseBadWasmBytes: ArrayBuffer;
let helloFdCloseGoodWasmBytes: ArrayBuffer;
let helloYieldLoopWasmBytes: ArrayBuffer;
let helloCapListWasmBytes: ArrayBuffer;
let helloStdWasmBytes: ArrayBuffer;
let helloClockWasmBytes: ArrayBuffer;
let memAdversaryWasmBytes: ArrayBuffer;
let initWasmBytes: ArrayBuffer;
let displayServerWasmBytes: ArrayBuffer;
let displayClientDemoWasmBytes: ArrayBuffer;

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
  const helloSigchldPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_sigchld.wasm",
  );
  const helloKillProbePath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_kill_probe.wasm",
  );
  const helloPidPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_pid.wasm",
  );
  const helloSelfProbePath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_self_probe.wasm",
  );
  const helloPpidPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_ppid.wasm",
  );
  const helloCapsPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_caps.wasm",
  );
  const helloRaisePath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_raise.wasm",
  );
  const helloWaitNoopPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_wait_noop.wasm",
  );
  const helloCapCheckPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_cap_check.wasm",
  );
  const helloRandomPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_random.wasm",
  );
  const helloFdCloseBadPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_fd_close_bad.wasm",
  );
  const helloFdCloseGoodPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_fd_close_good.wasm",
  );
  const helloYieldLoopPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_yield_loop.wasm",
  );
  const helloCapListPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello_cap_list.wasm",
  );
  // `hello-std` is a bin target (not cdylib), so cargo keeps the
  // dashes in the output filename.
  const helloStdPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello-std.wasm",
  );
  // `hello-clock` is also a bin target (dashes preserved).
  const helloClockPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/hello-clock.wasm",
  );
  // T172: mem-adversary is the Principle V acceptance gate —
  // a wasm32-wasip1 cdylib (so dashes → underscores in the
  // filename) that runs every probe a malicious user-wasm could
  // attempt and asserts each one is rejected by the kernel.
  const memAdversaryPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/mem_adversary.wasm",
  );
  // `init` is also a bin target, no dash-preservation concerns.
  const initPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/init.wasm",
  );
  // `display-server` is the std bin-target; dashes preserved.
  const displayServerPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/display-server.wasm",
  );
  // `display-client-demo` is the std bin-target; dashes preserved.
  const displayClientDemoPath = path.join(
    repoRoot,
    "target/wasm32-wasip1/release/display-client-demo.wasm",
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
    helloSigchldPath,
    helloKillProbePath,
    helloPidPath,
    helloSelfProbePath,
    helloPpidPath,
    helloCapsPath,
    helloRaisePath,
    helloWaitNoopPath,
    helloCapCheckPath,
    helloRandomPath,
    helloFdCloseBadPath,
    helloFdCloseGoodPath,
    helloYieldLoopPath,
    helloCapListPath,
    helloStdPath,
    helloClockPath,
    memAdversaryPath,
    initPath,
    displayServerPath,
    displayClientDemoPath,
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
  helloSigchldWasmBytes = loadWasm(helloSigchldPath);
  helloKillProbeWasmBytes = loadWasm(helloKillProbePath);
  helloPidWasmBytes = loadWasm(helloPidPath);
  helloSelfProbeWasmBytes = loadWasm(helloSelfProbePath);
  helloPpidWasmBytes = loadWasm(helloPpidPath);
  helloCapsWasmBytes = loadWasm(helloCapsPath);
  helloRaiseWasmBytes = loadWasm(helloRaisePath);
  helloWaitNoopWasmBytes = loadWasm(helloWaitNoopPath);
  helloCapCheckWasmBytes = loadWasm(helloCapCheckPath);
  helloRandomWasmBytes = loadWasm(helloRandomPath);
  helloFdCloseBadWasmBytes = loadWasm(helloFdCloseBadPath);
  helloFdCloseGoodWasmBytes = loadWasm(helloFdCloseGoodPath);
  helloYieldLoopWasmBytes = loadWasm(helloYieldLoopPath);
  helloCapListWasmBytes = loadWasm(helloCapListPath);
  helloStdWasmBytes = loadWasm(helloStdPath);
  helloClockWasmBytes = loadWasm(helloClockPath);
  memAdversaryWasmBytes = loadWasm(memAdversaryPath);
  initWasmBytes = loadWasm(initPath);
  displayServerWasmBytes = loadWasm(displayServerPath);
  displayClientDemoWasmBytes = loadWasm(displayClientDemoPath);
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
    expect(spawnResult.response!.status).toBe(0);
    const childPid = Number(spawnResult.response!.value);
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
    expect(spawnResult.response!.status).toBe(0);
    const spawnerPid = Number(spawnResult.response!.value);
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
    expect(spawnResult.response!.status).toBeLessThan(0);
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
    expect(spawnResult.response!.status).toBe(0);

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
    expect(spawnResult.response!.status).toBe(0);

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
    expect(spawnResult.response!.status).toBe(0);

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
    expect(spawnResult.response!.status).toBe(0);

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
    expect(spawnResult.response!.status).toBe(0);

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
    kernel.injectInput(Devnum.InputKbd, kbdBytes);

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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    // Exactly one console write, containing the four injected bytes
    // ("Hi!\n" — the console driver flushes on newline).
    expect(consoleWrites).toHaveLength(1);
    expect(Array.from(consoleWrites[0]!)).toEqual([0x48, 0x69, 0x21, 0x0a]);
  });

  it("hello-sigchld reads fd 3 (auto-installed SignalChannel) and echoes the u16 LE signum", async () => {
    // The signal-delivery pipeline end-to-end through a real user
    // wasm binary:
    //
    //   init PROC_SPAWN(/bin/hello-sigchld) -> child pid allocated
    //   with fd 3 = SignalChannel auto-installed (9fbe708).
    //   init PROC_KILL(child, SIGTERM=15) -> parent-child cap path
    //   queues Signal::Term on child's SignalInbox.
    //   runAllSpawns starts child -> fd_read(3, buf) drains the
    //   2-byte u16 LE (15) record -> fd_write(1, buf) -> the 2
    //   bytes appear on onConsoleWrite.
    //
    // Load-bearing ordering: PROC_KILL runs BEFORE runAllSpawns so
    // the child's inbox already has the signal when its first
    // fd_read fires. The binary's EAGAIN-polling loop handles the
    // alternative timing (post-spawn signal) but composition tests
    // pre-stage, matching how hello-input-echo does injection.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-sigchld", helloSigchldWasmBytes],
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
      path: "/bin/hello-sigchld",
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
    expect(spawnResult.response!.status).toBe(0);
    const childPid = Number(spawnResult.response!.value);
    expect(captures).toHaveLength(1);
    expect(captures[0]!.pid).toBe(childPid);

    // Dispatch PROC_KILL(child, SIGTERM=15) from init. Wire layout:
    // args[0..4] = target_pid i32 LE, args[4..6] = signum u16 LE.
    const killArgs = new Uint8Array(16);
    const killView = new DataView(killArgs.buffer);
    killView.setInt32(0, childPid, true);
    killView.setUint16(4, 15, true);
    const killResult = kernel.dispatch(init, {
      opcode: OP_EXT.PROC_KILL,
      requestId: 2,
      args: killArgs,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(killResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    // The binary reads 2 bytes from fd 3 (u16 LE signum) and
    // writes them plus a trailing newline to fd 1 in a single
    // 3-byte fd_write. The line-buffered console flushes on the
    // newline and observes exactly 3 bytes.
    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(3);
    const signum = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getUint16(0, true);
    expect(signum).toBe(15);
    expect(consoleWrites[0]![2]).toBe(0x0a); // '\n'
  });

  it("hello-pid calls proc_self() and writes its own pid (i32 LE) + newline to /dev/console", async () => {
    // End-to-end proof that the new `proc_self` PMos-ext shim is
    // reachable from a real wasm32-wasip1 binary. The child calls
    // pmos_ext::proc_self() (PROC_SELF = 0x1103), packs the
    // returned pid as i32 LE plus a trailing newline into a 5-byte
    // buffer, and writes it to fd 1. The composition test asserts
    // the decoded pid matches the value the kernel allocated for
    // the spawned child (captured via `onSpawnProcess`).
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-pid", helloPidWasmBytes],
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
      path: "/bin/hello-pid",
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
    expect(spawnResult.response!.status).toBe(0);
    const childPid = Number(spawnResult.response!.value);
    expect(captures).toHaveLength(1);
    expect(captures[0]!.pid).toBe(childPid);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(5);
    const reportedPid = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getInt32(0, true);
    expect(reportedPid).toBe(childPid);
    expect(consoleWrites[0]![4]).toBe(0x0a); // '\n'
  });

  it("hello-kill-probe calls proc_kill(9999, 0) and writes -ESRCH (i32 LE) + newline to /dev/console", async () => {
    // End-to-end proof of the POSIX kill(pid, 0) existence-probe arm
    // (cab9dc5) through a real wasm32-wasip1 binary. The child calls
    // pmos_ext::proc_kill(9999, 0) — signum 0 + a pid that's never
    // allocated. Pre-cab9dc5 the dispatcher's signum match had no arm
    // for 0 and rejected with -EINVAL = -28; post-cab9dc5 the arm
    // routes through Kernel::proc_check_signal which surfaces -ESRCH
    // = -71 because the target doesn't exist. The binary packs the
    // i32 LE return value into a 5-byte write (4 bytes errno + '\n')
    // so the line-buffered console flushes through onConsoleWrite in
    // one shot.
    //
    // The distinct errno (-71 vs -28) makes this test sharp: a
    // regression that reverted to pre-cab9dc5 behaviour would surface
    // as a wrong byte sequence on console, not a silent pass.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-kill-probe", helloKillProbeWasmBytes],
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
      path: "/bin/hello-kill-probe",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(5);
    const rc = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getInt32(0, true);
    expect(rc).toBe(-ERRNO.ESRCH);
    expect(consoleWrites[0]![4]).toBe(0x0a); // '\n'
  });

  it("hello-self-probe calls proc_kill(proc_self(), 0) and writes 0 (i32 LE) + newline — the success arm of cab9dc5", async () => {
    // End-to-end proof of the SUCCESS arm of cab9dc5's POSIX
    // kill(getpid(), 0) existence + permission probe through a
    // real wasm32-wasip1 binary. Companion to hello-kill-probe
    // (which exercises the ESRCH arm via probing pid 9999) — this
    // binary self-targets, which always succeeds because the
    // kernel's proc_check_signal permits any sender to signal
    // itself regardless of caps.
    //
    // The composition of two PMos-ext shims (proc_self + proc_kill)
    // in one binary also proves the c7d5c9b proc_self shim and the
    // proc_kill signum-0 path are wired through the same
    // dispatcher without interference.
    //
    // Pre-cab9dc5: -EINVAL = -28 -> bytes [0xe4, 0xff, 0xff, 0xff].
    // Post-cab9dc5: 0 -> bytes [0x00, 0x00, 0x00, 0x00]. The 0 vs
    // -28 distinction makes the test sharp.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-self-probe", helloSelfProbeWasmBytes],
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
      path: "/bin/hello-self-probe",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(5);
    const rc = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getInt32(0, true);
    expect(rc).toBe(0);
    expect(consoleWrites[0]![4]).toBe(0x0a); // '\n'
  });

  it("hello-ppid calls proc_parent() and writes its parent's pid (i32 LE) + newline to /dev/console", async () => {
    // End-to-end proof that the new `proc_parent` PMos-ext shim is
    // reachable from a real wasm32-wasip1 binary. Sister to
    // hello-pid: hello-pid writes its own pid via proc_self,
    // hello-ppid writes its parent's pid via proc_parent. The
    // composition test asserts the decoded ppid equals init's pid
    // (the spawning process), proving the supervisor-introspection
    // direction is wired end-to-end.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-ppid", helloPpidWasmBytes],
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
      path: "/bin/hello-ppid",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(5);
    const reportedPpid = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getInt32(0, true);
    expect(reportedPpid).toBe(init);
    expect(consoleWrites[0]![4]).toBe(0x0a); // '\n'
  });

  it("hello-caps calls proc_caps_get(proc_self()) and writes the u64 LE CapSet + newline to /dev/console", async () => {
    // End-to-end proof that proc_self (c7d5c9b) and the existing
    // proc_caps_get shim compose cleanly through the dispatcher,
    // and that the heap-out u64 write path is reachable from a
    // real wasm32-wasip1 binary. Self-querying always succeeds
    // because the kernel's handle_proc_caps_get short-circuits the
    // cap check when target == sender.
    //
    // The spawn manifest's caps argument is CAPSET_ALL, so the
    // child's CapSet equals 0xffff_ffff_ffff_ffff (all 64 bits
    // set). The binary writes those 8 bytes LE plus a trailing
    // newline = 9 bytes total. Test decodes the first 8 bytes as
    // u64 LE and asserts CAPSET_ALL.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-caps", helloCapsWasmBytes],
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
      path: "/bin/hello-caps",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(9);
    const reportedCaps = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getBigUint64(0, true);
    expect(reportedCaps).toBe(CAPSET_ALL);
    expect(consoleWrites[0]![8]).toBe(0x0a); // '\n'
  });

  it("hello-raise self-raises SIGTERM via WASI proc_raise and drains it from fd 3 as u16 LE", async () => {
    // End-to-end proof of the WASI proc_raise path (cbe8959's
    // self-signal arm) through a real wasm32-wasip1 binary —
    // sister to hello-sigchld, which exercises the
    // kernel-generated parent-kill path. Origin difference:
    // hello-sigchld observes SIGTERM queued by its parent's
    // PROC_KILL; hello-raise observes SIGTERM queued by its own
    // proc_raise call. The fd 3 drain path is identical.
    //
    // Because proc_raise is synchronous (the signal is queued on
    // the caller's inbox before the shim returns), no EAGAIN
    // polling is required: the fd_read that follows always finds
    // the pending signal on the first dispatch. The binary writes
    // 2 bytes (u16 LE signum 15) plus a trailing newline = 3
    // bytes.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-raise", helloRaiseWasmBytes],
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
      path: "/bin/hello-raise",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(3);
    const signum = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getUint16(0, true);
    expect(signum).toBe(15);
    expect(consoleWrites[0]![2]).toBe(0x0a); // '\n'
  });

  it("hello-wait-noop calls proc_wait(-1, 0, 0) on a childless process and writes -ECHILD (i32 LE) + newline", async () => {
    // End-to-end proof of the proc_wait pmos_ext shim's error path
    // (98a3341 ext opcode wiring) through a real wasm32-wasip1
    // binary. The child has no children of its own — it's a leaf
    // process — so proc_wait(-1) hits the kernel's
    // WaitOutcome::NoChildren -> ECHILD arm at
    // crates/kernel/src/syscall/ext.rs:563. The binary writes the
    // 4-byte i32 LE -9 plus a trailing newline = 5 bytes.
    //
    // ECHILD = 9, so -ECHILD = -9, distinct from EAGAIN (-6),
    // EINVAL (-28), ESRCH (-71) — the byte sequence is sharp.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-wait-noop", helloWaitNoopWasmBytes],
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
      path: "/bin/hello-wait-noop",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(5);
    const rc = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getInt32(0, true);
    expect(rc).toBe(-ERRNO.ECHILD);
    expect(consoleWrites[0]![4]).toBe(0x0a); // '\n'
  });

  it("hello-cap-check calls cap_check(SHELL) and writes 1 (i32 LE) + newline to /dev/console", async () => {
    // End-to-end proof that the new `cap_check` PMos-ext shim is
    // reachable from a real wasm32-wasip1 binary. cap_check is the
    // per-cap yes/no probe — companion to cap_list /
    // proc_caps_get's bitset query — useful when userland wants to
    // gate a code path on one specific cap without materialising
    // the full bitset.
    //
    // Composition test spawns the binary with CAPSET_ALL, so
    // cap_check(CAP_SHELL = 3) returns 1. Bytes [0x01, 0x00,
    // 0x00, 0x00, 0x0a].
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-cap-check", helloCapCheckWasmBytes],
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
      path: "/bin/hello-cap-check",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(5);
    const rc = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getInt32(0, true);
    expect(rc).toBe(1);
    expect(consoleWrites[0]![4]).toBe(0x0a); // '\n'
  });

  it("hello-random calls WASI random_get twice and writes 16 distinct random bytes + newline to /dev/console", async () => {
    // End-to-end proof of the new WASI random_get shim through a
    // real wasm32-wasip1 binary. The binary calls random_get(buf,
    // 8) twice into adjacent stack buffers. If both reads return
    // the same bytes it exits 12 (catastrophic — 1 in 2^64 with a
    // real entropy source); otherwise it writes both 8-byte
    // records plus a trailing newline = 17 bytes.
    //
    // Asserting "the bytes differ between two reads" is a much
    // stronger sanity check than "any bytes were written" — it
    // catches both a no-op shim (would return zeros) and a
    // stuck-value shim (would return identical buffers). Test
    // assertions verify the byte count + the inequality of the
    // two halves; the actual byte values are unpredictable so
    // we don't decode them.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-random", helloRandomWasmBytes],
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
      path: "/bin/hello-random",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(17);
    expect(consoleWrites[0]![16]).toBe(0x0a); // '\n'

    // The two 8-byte halves must differ — the binary already
    // exits 12 if they match, but the assertion here is the
    // explicit form of the same invariant against console-side
    // observation.
    const firstHalf = consoleWrites[0]!.slice(0, 8);
    const secondHalf = consoleWrites[0]!.slice(8, 16);
    expect(firstHalf).not.toEqual(secondHalf);
  });

  it("hello-fd-close-bad calls fd_close(99) and writes EBADF (i32 LE) + newline to /dev/console", async () => {
    // End-to-end proof of the new WASI fd_close shim through a
    // real wasm32-wasip1 binary. fd 99 is well beyond any
    // allocation (spawned children get fd 0/1/2 = console + fd 3
    // = SignalChannel auto-installed), so the kernel's fd_close
    // returns KernelError::NoSuchFd -> EBADF immediately.
    //
    // Unlike the proc_kill / proc_caps_get error paths which
    // surface as NEGATIVE errno (PMos-ext convention), WASI
    // shims return POSITIVE errno on failure (WASI standard).
    // So the binary writes 8 (= EBADF), not -8. Bytes [0x08,
    // 0x00, 0x00, 0x00, 0x0a].
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-fd-close-bad", helloFdCloseBadWasmBytes],
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
      path: "/bin/hello-fd-close-bad",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(5);
    const rc = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getInt32(0, true);
    expect(rc).toBe(ERRNO.EBADF);
    expect(consoleWrites[0]![4]).toBe(0x0a); // '\n'
  });

  it("hello-cap-list calls cap_list(&caps) and writes the u64 LE CapSet + newline to /dev/console", async () => {
    // End-to-end proof of the new `cap_list` PMos-ext shim
    // through a real wasm32-wasip1 binary. cap_list is the
    // no-args "give me my own caps" primitive — functionally
    // equivalent to proc_caps_get(proc_self(), out) (which
    // hello-caps already exercises) but saves the proc_self
    // round-trip and avoids passing a pid in.
    //
    // Composition test spawns with CAPSET_ALL, so the returned
    // u64 = 0xffff_ffff_ffff_ffff. Bytes [0xff..0xff, 0x0a] —
    // identical output shape to hello-caps but exercises a
    // different shim path through the same kernel handler.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-cap-list", helloCapListWasmBytes],
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
      path: "/bin/hello-cap-list",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(9);
    const reportedCaps = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getBigUint64(0, true);
    expect(reportedCaps).toBe(CAPSET_ALL);
    expect(consoleWrites[0]![8]).toBe(0x0a); // '\n'
  });

  it("hello-fd-close-good closes fd 2, re-closes to verify -EBADF, and writes 0 (i32 LE) + newline", async () => {
    // End-to-end proof of the SUCCESS arm of f03cf74's fd_close
    // shim through a real wasm32-wasip1 binary. Companion to
    // hello-fd-close-bad (which exercises the EBADF arm via
    // closing an unopened fd).
    //
    // The binary closes fd 2 (auto-installed /dev/console
    // stderr), then immediately re-closes fd 2 to verify the
    // slot was actually freed (a no-op shim that returned 0 on
    // every close would silently pass the first close but the
    // re-close would also return 0 — the binary exits 14 on that
    // invariant violation, surfacing as a non-zero exit code
    // observable in the test). After the close + re-check the
    // binary writes the FIRST close's rc (0) plus a newline to
    // fd 1 (still open).
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-fd-close-good", helloFdCloseGoodWasmBytes],
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
      path: "/bin/hello-fd-close-good",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(5);
    const rc = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getInt32(0, true);
    expect(rc).toBe(0);
    expect(consoleWrites[0]![4]).toBe(0x0a); // '\n'
  });

  it("hello-yield-loop calls sched_yield 4 times and writes the iteration count + newline", async () => {
    // End-to-end proof of the new WASI sched_yield shim from
    // 5dd1714 through a real wasm32-wasip1 binary. Completes the
    // user-wasm coverage of this session's WASI shim trio
    // (random_get + fd_close + sched_yield).
    //
    // PMos's scheduler is a single-threaded round-robin so yield
    // has no behavioural effect — the load-bearing assertion is
    // purely "the shim is callable and returns 0", but the loop
    // iterates four times so a regression that broke the shim
    // on the second call would still surface (binary exits 12
    // immediately). Bytes [0x04, 0x00, 0x00, 0x00, 0x0a].
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-yield-loop", helloYieldLoopWasmBytes],
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
      path: "/bin/hello-yield-loop",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);

    expect(consoleWrites).toHaveLength(1);
    expect(consoleWrites[0]!.length).toBe(5);
    const iterations = new DataView(
      consoleWrites[0]!.buffer,
      consoleWrites[0]!.byteOffset,
    ).getInt32(0, true);
    expect(iterations).toBe(4);
    expect(consoleWrites[0]![4]).toBe(0x0a); // '\n'
  });

  it("init's fd 3 observes SIGCHLD after a spawned user wasm child exits cleanly", async () => {
    // End-to-end proof that 91c618f's SIGCHLD-on-child-exit path
    // reaches userland through fd 3 after running REAL user
    // wasm. init stages fd 3 as SignalChannel via the test-harness
    // install export (init uses registerProcess, which
    // deliberately does not auto-install — only proc_spawn'd
    // children inherit fd 3 per 9fbe708), spawns hello-wasi-min,
    // runAllSpawns runs the child which writes "hello from
    // userland\n" and proc_exits. The child's PROC_EXIT posts
    // SIGCHLD to init's inbox; init's fd_read on fd 3 drains
    // the u16 LE record = 17.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-wasi-min", helloWasmBytes],
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
    kernel.installSignalChannelFd(init, 3);
    kernel.markRunning(init);

    const manifest = encodeSpawnManifest({
      path: "/bin/hello-wasi-min",
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    expect(history[0]!.exitCode).toBe(0);
    // Child wrote its message to the shared /dev/console.
    expect(consoleWrites).toHaveLength(1);
    expect(new TextDecoder().decode(consoleWrites[0]!)).toBe(
      "hello from userland\n",
    );

    // init's inbox now has SIGCHLD. FD_READ on fd 3 drains it
    // as a u16 LE signum = 17.
    const readResult = kernel.dispatch(init, {
      opcode: OP_WASI.FD_READ,
      requestId: 2,
      arg0: 3,
      heapPtr: 0,
      heapLen: 4,
    });
    expect(readResult.response!.status).toBe(0);
    expect(Number(readResult.response!.value)).toBe(2);
    const sigchld = new DataView(
      readResult.heapOut.buffer,
      readResult.heapOut.byteOffset,
      readResult.heapOut.byteLength,
    ).getUint16(0, true);
    expect(sigchld).toBe(17);

    // A subsequent read finds an empty inbox.
    const drainedResult = kernel.dispatch(init, {
      opcode: OP_WASI.FD_READ,
      requestId: 3,
      arg0: 3,
      heapPtr: 0,
      heapLen: 4,
    });
    expect(drainedResult.response!.status).toBe(-ERRNO.EAGAIN);
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
    expect(spawnResult.response!.status).toBe(0);

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

  it("init (std) spawns hello-std AND display-server AND display-client-demo via pmos_ext.proc_spawn, all three children run after init exits", async () => {
    // The four-pid substrate slice: init fires four fire-and-forget
    // `pmos_ext.proc_spawn` calls (`/bin/hello-std`,
    // `/bin/display-server`, `/bin/display-client-demo` ×2), then
    // enters a blocking `proc_wait` supervision loop (T095).
    //
    // Under `runAllSpawns` (the vitest composition helper) children
    // run strictly sequentially AND spawned pids stay Ready (no
    // `markRunning`). Init's first blocking `proc_wait` trips
    // `park_on_wait`'s Running→BlockedOnWait transition check —
    // init itself is Ready, so the transition fails with
    // `KernelError::NoSuchPid` → shim surfaces `-ESRCH`. Init's
    // early-exit arm prints
    // `init proc_wait returned errno=71; exiting with 4 children
    // unreaped` + `init exiting` and falls through. Children then
    // run one at a time: hello-std exits 0, display-server's first
    // `ipc_accept` trips the same Ready→BlockedOnIpc transition
    // failure (→ `-ESRCH` → exit 12), display-client-demo's
    // `display_connect` exhausts against the torn-down listener
    // (→ exit 10).
    //
    // The vitest layer validates that:
    //   1. init spawns all four children (three distinct paths);
    //   2. init's proc_wait supervision loop degrades gracefully
    //      under the sequential harness (no infinite spin);
    //   3. hello-std still runs cleanly alongside the new siblings
    //      (no regression in the std startup path);
    //   4. display-server + display-client-demo both survive their
    //      bounded loops and exit through `std::process::exit`
    //      rather than hanging or trapping.
    //
    // The four-binary IPC round-trip + SIGTERM-driven
    // display-server shutdown is validated only under Playwright,
    // where real concurrent Workers in separate WASM linear
    // memories give the required interleaving. See
    // `web/tests/integration/real-kernel.spec.ts`.
    const consoleWrites: Uint8Array[] = [];
    const fbWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/init", initWasmBytes],
      ["/bin/hello-std", helloStdWasmBytes],
      ["/bin/display-server", displayServerWasmBytes],
      ["/bin/display-client-demo", displayClientDemoWasmBytes],
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);

    expect(captures).toHaveLength(0);
    expect(history).toHaveLength(5);
    expect(history[0]!.path).toBe("/bin/init");
    expect(history[0]!.exitCode).toBe(0);
    expect(history[1]!.path).toBe("/bin/hello-std");
    expect(history[1]!.exitCode).toBe(0);
    // After T095 (init proc_wait supervision loop) + T110
    // (display-server unbounded accept with signal-driven exit):
    // display-server's first ipc_accept call sends flags=0
    // (blocking by default). The kernel's park_on_accept attempts
    // Running→BlockedOnIpc, but the sequential runAllSpawns
    // harness leaves spawned pids in the Ready state that
    // PROC_SPAWN installs — it doesn't call markRunning the way
    // production (kernel-worker-entry) does. The illegal
    // Ready→BlockedOnIpc transition produces KernelError::
    // NoSuchPid, which the shim surfaces as errno -ESRCH. The
    // new display-server body sees `rc < 0 && rc != -EINTR` and
    // exits 12. In production (Playwright), the kernel-worker
    // dispatch loop marks spawned pids Running, blocking accept
    // parks cleanly, peer's display_connect wakes it, and
    // SIGTERM from init drives a clean exit 0 — see
    // real-kernel.spec.ts.
    expect(history[2]!.path).toBe("/bin/display-server");
    expect(history[2]!.exitCode).toBe(12);
    // Both display-client-demo spawns run after display-server has
    // torn down, so each `display_connect` poll exhausts
    // -ECONNREFUSED and exits with code 10.
    expect(history[3]!.path).toBe("/bin/display-client-demo");
    expect(history[3]!.exitCode).toBe(10);
    expect(history[4]!.path).toBe("/bin/display-client-demo");
    expect(history[4]!.exitCode).toBe(10);

    const combined = new TextDecoder().decode(
      new Uint8Array(
        consoleWrites.reduce<number[]>(
          (acc, b) => acc.concat(Array.from(b)),
          [],
        ),
      ),
    );
    // Sequential ordering: init's 7 lines (starting, 4× spawned,
    // proc_wait early-exit note, exiting), then hello-std's 1,
    // then display-server's 1 ("starting" only — no "fb blit ok"
    // because accept never succeeded), then display-client-demo's
    // 2× "starting" (both exhaust display_connect and never print
    // "sent pixels"). The pids the kernel allocates are dynamic
    // so each "spawned" line matches on prefix, and the exact
    // unreaped-count in the proc_wait note depends on how many
    // spawns actually succeeded (expect 4).
    const lines = combined.split("\n").filter((l) => l.length > 0);
    expect(lines[0]).toBe("init starting");
    expect(lines[1]).toMatch(/^init spawned hello-std pid=\d+$/);
    expect(lines[2]).toMatch(/^init spawned display-server pid=\d+$/);
    expect(lines[3]).toMatch(/^init spawned display-client-demo pid=\d+$/);
    expect(lines[4]).toMatch(/^init spawned display-client-demo pid=\d+$/);
    expect(lines[5]).toBe(
      "init proc_wait returned errno=71; exiting with 4 children unreaped",
    );
    expect(lines[6]).toBe("init exiting");
    expect(lines[7]).toBe("hello from std");
    expect(lines[8]).toBe("display-server starting");
    expect(lines[9]).toBe("display-client-demo starting");
    expect(lines[10]).toBe("display-client-demo starting");
    expect(lines).toHaveLength(11);

    // No /dev/fb0 writes — the sequential in-process harness can't
    // drive the IPC round-trip, so neither binary reaches its fb
    // write step. Playwright's four-pid concurrent-Worker test is
    // the observer that captures the framebuffer payload.
    expect(fbWrites).toHaveLength(0);
  });

  it("hello-clock: std binary drives CLOCK_TIME_GET(MONOTONIC + REALTIME) + CLOCK_RES_GET through the WASI shim end-to-end", async () => {
    // The CLOCK_TIME_GET + CLOCK_RES_GET acceptance test.
    // `hello-clock`'s `_start` calls `Instant::now()` (which lowers
    // to `clock_time_get(MONOTONIC, ...)`) twice and asserts
    // non-decreasing, then calls `SystemTime::now()` (which lowers
    // to `clock_time_get(REALTIME, ...)`) and asserts the wall
    // clock is after 2020, then directly calls
    // `clock_res_get(MONOTONIC)` + `clock_res_get(REALTIME)` via
    // a hand-written FFI extern block (std doesn't expose
    // `clock_getres`) and asserts both report 1 ns.
    //
    // The binary prints three lines ("monotonic ok", "realtime ok",
    // "res ok") and exits 0. All three lines arriving means every
    // clock_id routes through its shim, lands on the kernel
    // handler's correct branch, and travels back as an 8-byte i64
    // the shim writes into user memory.
    //
    // Failure modes:
    //   * LinkError at instantiate = `clock_time_get` or
    //     `clock_res_get` shim missing
    //   * exit nonzero + panic output = one of the std asserts
    //     tripped (monotonicity regression / realtime before 2020
    //     / clock_res_get wrong value or errno)
    //   * missing "monotonic ok" = CLOCK_TIME_GET(MONOTONIC) broken
    //   * missing "realtime ok" = CLOCK_TIME_GET(REALTIME) broken
    //     or `nowRealtimeNs` defaulting to 0
    //   * missing "res ok" = CLOCK_RES_GET handler broken or shim
    //     not writing the resolution value
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

    const backend = new KernelWasmHostBackend(kernel, pid);
    const runtime = new UserWasmRuntime(helloClockWasmBytes, backend);

    const exitCode = await runtime.run();

    expect(exitCode).toBe(0);
    const combined = new TextDecoder().decode(
      new Uint8Array(
        consoleWrites.reduce<number[]>(
          (acc, b) => acc.concat(Array.from(b)),
          [],
        ),
      ),
    );
    const lines = combined.split("\n").filter((l) => l.length > 0);
    expect(lines).toEqual([
      "hello-clock monotonic ok",
      "hello-clock realtime ok",
      "hello-clock res ok",
    ]);
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

  it("mem-adversary runs every Principle V probe and reports OK on full isolation hold (T172)", async () => {
    // T172: Principle V acceptance gate. Spawn the mem-adversary
    // wasm binary with a deliberately limited cap set
    // (CAPSET_ORDINARY_APP — just DisplayClient) so the cap-
    // gated probes (proc_kill on a stranger's pid, proc_spawn
    // with a cap superset) actually exercise the
    // ENOTCAPABLE rejection path. The binary prints PASS lines
    // for every rejected probe and `mem-adversary OK\n` on
    // success, exits 0; any breach exits with the probe index
    // (1..=8) and prints `mem-adversary BREACH probe N\n`.
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/mem-adversary", memAdversaryWasmBytes],
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

    // Spawn the adversary with ORDINARY_APP caps so cap-gated
    // probes hit ENOTCAPABLE rather than getting waved through.
    const manifest = encodeSpawnManifest({
      path: "/bin/mem-adversary",
      caps: CAPSET_ORDINARY_APP,
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
    expect(spawnResult.response!.status).toBe(0);

    const history = await runAllSpawns(kernel, captures);
    expect(history).toHaveLength(1);
    // Concatenate every console write into one byte stream so
    // the assertion failure surfaces the probe details.
    const totalLen = consoleWrites.reduce((s, b) => s + b.length, 0);
    const merged = new Uint8Array(totalLen);
    let off = 0;
    for (const b of consoleWrites) {
      merged.set(b, off);
      off += b.length;
    }
    const text = new TextDecoder().decode(merged);
    // Exit 0 means every probe was correctly rejected. A non-
    // zero exit code is the index of the probe that succeeded
    // when it should have failed (i.e., a Principle V breach).
    expect(history[0]!.exitCode, `console:\n${text}`).toBe(0);
    expect(text).toContain("mem-adversary OK\n");
    expect(text).not.toContain("BREACH");

    // Each universal probe (1, 3, 4, 5, 6, 7) should have a PASS
    // line. Probes 2 and 8 depend on cap presence; with
    // ORDINARY_APP, both are exercised (no PROC_KILL_ANY for 2,
    // not CAPSET_ALL for 8) — so all 8 probes should PASS.
    for (const name of [
      "PASS cap_check_invalid_id",
      "PASS proc_kill_fake_pid",
      "PASS proc_caps_get_fake_pid",
      "PASS fd_read_unowned_fd",
      "PASS fd_close_unowned_fd",
      "PASS path_open_nonexistent_path",
      "PASS path_open_unknown_pid_status",
      "PASS proc_spawn_cap_superset",
    ]) {
      expect(text).toContain(name);
    }
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
