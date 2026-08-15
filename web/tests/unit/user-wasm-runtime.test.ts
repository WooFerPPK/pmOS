// End-to-end test: a real `wasm32-wasip1` binary runs against a
// real `KernelWasmHost` via `UserWasmRuntime`.
//
// This is the first point in the project where user code actually
// executes against the real kernel. The test loads two wasm
// modules from disk:
//
//   * `kernel.wasm` under Cargo's configured target directory — the
//     kernel cdylib with its 10-opcode dispatcher, already exercised
//     in isolation by kernel-wasm-host.test.ts.
//   * `hello_wasi_min.wasm` under the same target directory — a
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
  type KernelBackend,
  UserWasmRuntime,
} from "../../src/user-wasm-runtime";
import { SabBackend } from "../../src/sab-backend";
import {
  CAPSET_ALL,
  CAPSET_ORDINARY_APP,
  encodeSpawnManifest,
  ERRNO,
  OP_EXT,
  OP_WASI,
} from "../../src/shared/syscall";
import {
  HEAP_SCRATCH_BYTES,
  OFF_HEAP_SCRATCH,
  OFF_REQ_RING,
  OFF_RES_HEAD,
  OFF_RES_RING,
  OFF_USER_WAIT_SLOT,
  SAB_SIZE,
  STATUS_READY,
} from "../../src/shared/sab-layout";
import { resolveCargoTargetDirectory } from "../helpers/cargo-target";

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

interface DirectRuntimeImports {
  readonly wasi_snapshot_preview1: {
    fd_write(fd: number, iovsPtr: number, iovsLen: number, resultPtr: number): number;
    fd_read(fd: number, iovsPtr: number, iovsLen: number, resultPtr: number): number;
    sock_accept(fd: number, flags: number, resultPtr: number): number;
    sock_send(
      fd: number,
      iovsPtr: number,
      iovsLen: number,
      flags: number,
      resultPtr: number,
    ): number;
    sock_recv(
      fd: number,
      iovsPtr: number,
      iovsLen: number,
      flags: number,
      resultPtr: number,
      resultFlagsPtr: number,
    ): number;
  };
  readonly pmos_ext: {
    ipc_socket(ty: number): number;
    ipc_bind(fd: number, pathPtr: number, pathLen: number): number;
    ipc_connect(fd: number, pathPtr: number, pathLen: number): number;
    ipc_send(
      fd: number,
      bufPtr: number,
      len: number,
      fdToPass: number,
      flags: number,
    ): number;
    ipc_recv(
      fd: number,
      bufPtr: number,
      len: number,
      recvFdOutPtr: number,
      flags: number,
    ): number;
    ipc_pipe(fdsPtr: number): number;
    ipc_peer_caps(fd: number, capsOutPtr: number): number;
    ipc_peer_pid(fd: number, pidOutPtr: number): number;
    fs_watch(pathPtr: number, pathLen: number, mask: number, flags: number): number;
  };
}

function directRuntimeImports(
  backend: KernelBackend,
  pages = 1,
): { readonly memory: WebAssembly.Memory; readonly imports: DirectRuntimeImports } {
  const runtime = new UserWasmRuntime(buildPeerCapsProbeWasm(), backend);
  const exposed = runtime as unknown as {
    memory: WebAssembly.Memory;
    buildImports(): DirectRuntimeImports;
  };
  const memory = new WebAssembly.Memory({ initial: pages });
  exposed.memory = memory;
  return { memory, imports: exposed.buildImports() };
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
  const cargoTargetDirectory = resolveCargoTargetDirectory(
    path.resolve(__dirname, "../../.."),
    process.env.CARGO_TARGET_DIR,
  );
  const kernelPath = path.join(
    cargoTargetDirectory,
    "wasm32-unknown-unknown/release/kernel.wasm",
  );
  const helloPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_wasi_min.wasm",
  );
  const spawnerPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_wasi_spawner.wasm",
  );
  const ipcSelfTestPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/ipc_self_test.wasm",
  );
  const helloFramebufferPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_framebuffer.wasm",
  );
  const displayServerLitePath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/display_server_lite.wasm",
  );
  const helloWasiBootstrapPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_wasi_bootstrap.wasm",
  );
  const helloFbBlitPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_fb_blit.wasm",
  );
  const helloInputEchoPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_input_echo.wasm",
  );
  const helloSigchldPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_sigchld.wasm",
  );
  const helloKillProbePath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_kill_probe.wasm",
  );
  const helloPidPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_pid.wasm",
  );
  const helloSelfProbePath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_self_probe.wasm",
  );
  const helloPpidPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_ppid.wasm",
  );
  const helloCapsPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_caps.wasm",
  );
  const helloRaisePath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_raise.wasm",
  );
  const helloWaitNoopPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_wait_noop.wasm",
  );
  const helloCapCheckPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_cap_check.wasm",
  );
  const helloRandomPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_random.wasm",
  );
  const helloFdCloseBadPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_fd_close_bad.wasm",
  );
  const helloFdCloseGoodPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_fd_close_good.wasm",
  );
  const helloYieldLoopPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_yield_loop.wasm",
  );
  const helloCapListPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello_cap_list.wasm",
  );
  // `hello-std` is a bin target (not cdylib), so cargo keeps the
  // dashes in the output filename.
  const helloStdPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello-std.wasm",
  );
  // `hello-clock` is also a bin target (dashes preserved).
  const helloClockPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/hello-clock.wasm",
  );
  // T172: mem-adversary is the Principle V acceptance gate —
  // a wasm32-wasip1 cdylib (so dashes → underscores in the
  // filename) that runs every probe a malicious user-wasm could
  // attempt and asserts each one is rejected by the kernel.
  const memAdversaryPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/mem_adversary.wasm",
  );
  // `init` is also a bin target, no dash-preservation concerns.
  const initPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/init.wasm",
  );
  // `display-server` is the std bin-target; dashes preserved.
  const displayServerPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/display-server.wasm",
  );
  // `display-client-demo` is the std bin-target; dashes preserved.
  const displayClientDemoPath = path.join(
    cargoTargetDirectory,
    "wasm32-wasip1/release/display-client-demo.wasm",
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
    //   * fd_prestat_get(3) → `/` directory preopen
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

  it("hello-random preserves two random_get records across console line chunks", async () => {
    // End-to-end proof of the new WASI random_get shim through a
    // real wasm32-wasip1 binary. The binary calls random_get(buf,
    // 8) twice into adjacent stack buffers. If both reads return
    // the same bytes it exits 12 (catastrophic — 1 in 2^64 with a
    // real entropy source); otherwise it writes both 8-byte
    // records plus a trailing newline = 17 bytes.
    //
    // One record deliberately contains an embedded newline. The
    // console contract publishes complete lines, so the single
    // 17-byte fd_write must arrive in two host callbacks without
    // changing the byte stream. Pinning the source also proves
    // exactly two random_get calls and removes entropy-dependent
    // callback boundaries from this regression.
    const randomRecords = [
      new Uint8Array([0x41, 0x42, 0x0a, 0x43, 0x44, 0x45, 0x46, 0x47]),
      new Uint8Array([0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58]),
    ] as const;
    let randomCallCount = 0;
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-random", helloRandomWasmBytes],
    ]);
    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      randomBytes: (out) => {
        const record = randomRecords[randomCallCount];
        if (record === undefined || out.length !== record.length) {
          throw new Error("unexpected hello-random random_get call");
        }
        out.set(record);
        randomCallCount += 1;
      },
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

    expect(randomCallCount).toBe(2);
    expect(consoleWrites).toHaveLength(2);
    expect(consoleWrites[0]).toEqual(randomRecords[0].slice(0, 3));
    expect(consoleWrites[1]).toEqual(
      new Uint8Array([...randomRecords[0].slice(3), ...randomRecords[1], 0x0a]),
    );

    const output = new Uint8Array(
      consoleWrites.reduce((length, chunk) => length + chunk.length, 0),
    );
    let outputOffset = 0;
    for (const chunk of consoleWrites) {
      output.set(chunk, outputOffset);
      outputOffset += chunk.length;
    }
    expect(output).toEqual(
      new Uint8Array([...randomRecords[0], ...randomRecords[1], 0x0a]),
    );
    expect(output).toHaveLength(17);
    expect(output[16]).toBe(0x0a); // '\n'

    // The two 8-byte halves must differ — the binary already
    // exits 12 if they match, but the assertion here is the
    // explicit form of the same invariant against console-side
    // observation.
    const firstHalf = output.slice(0, 8);
    const secondHalf = output.slice(8, 16);
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

  it("real init spawns all four children before entering blocking supervision", async () => {
    const consoleWrites: Uint8Array[] = [];
    const captures: CapturedSpawn[] = [];
    const binaryRegistry = new Map<string, BufferSource>([
      ["/bin/hello-std", helloStdWasmBytes],
      ["/bin/display-server", displayServerWasmBytes],
      ["/bin/display-client-demo", displayClientDemoWasmBytes],
    ]);
    const kernel = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      onSpawnProcess: captureSpawn(binaryRegistry, captures),
    });

    const initPid = kernel.registerProcess(CAPSET_ALL);
    kernel.installConsoleFd(initPid, 0);
    kernel.installConsoleFd(initPid, 1);
    kernel.installConsoleFd(initPid, 2);
    kernel.markRunning(initPid);

    // The in-process backend can execute init's non-blocking startup calls,
    // but it cannot represent a parked PROC_WAIT. Forward every real syscall
    // until that boundary, then unwind the wasm with an identity-checked
    // test sentinel before the blocking request reaches the kernel.
    const directBackend = new KernelWasmHostBackend(kernel, initPid);
    const supervisionBoundary = new Error(
      "test reached init's blocking supervision boundary",
    );
    const lifecycleOpcodes: number[] = [];
    const boundedBackend: KernelBackend = {
      dispatch(request, heapIn) {
        if (
          request.opcode === OP_EXT.PROC_SPAWN ||
          request.opcode === OP_EXT.PROC_WAIT
        ) {
          lifecycleOpcodes.push(request.opcode);
        }
        if (request.opcode === OP_EXT.PROC_WAIT) {
          throw supervisionBoundary;
        }
        return directBackend.dispatch(request, heapIn);
      },
    };

    const runtime = new UserWasmRuntime(initWasmBytes, boundedBackend);
    await expect(runtime.run()).rejects.toBe(supervisionBoundary);

    expect(lifecycleOpcodes).toEqual([
      OP_EXT.PROC_SPAWN,
      OP_EXT.PROC_SPAWN,
      OP_EXT.PROC_SPAWN,
      OP_EXT.PROC_SPAWN,
      OP_EXT.PROC_WAIT,
    ]);
    expect(captures.map(({ path }) => path)).toEqual([
      "/bin/hello-std",
      "/bin/display-server",
      "/bin/display-client-demo",
      "/bin/display-client-demo",
    ]);
    expect(new Set(captures.map(({ pid }) => pid)).size).toBe(4);
    expect(captures.every(({ pid }) => pid > initPid)).toBe(true);

    const combined = new TextDecoder().decode(
      new Uint8Array(
        consoleWrites.reduce<number[]>(
          (acc, b) => acc.concat(Array.from(b)),
          [],
        ),
      ),
    );
    const lines = combined.split("\n").filter((l) => l.length > 0);
    expect(lines[0]).toBe("init starting");
    expect(lines[1]).toMatch(/^init spawned hello-std pid=\d+$/);
    expect(lines[2]).toMatch(/^init spawned display-server pid=\d+$/);
    expect(lines[3]).toMatch(/^init spawned display-client-demo pid=\d+$/);
    expect(lines[4]).toMatch(/^init spawned display-client-demo pid=\d+$/);
    expect(lines).toHaveLength(5);
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

  it("ipc_peer_caps import dispatches the fd-scoped query and writes the u64 result", async () => {
    const peerCaps = 0x1234_5678n;
    const opcodes: number[] = [];
    const backend: KernelBackend = {
      dispatch(request) {
        opcodes.push(request.opcode);
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: request.opcode === OP_EXT.IPC_PEER_CAPS ? peerCaps : 0n,
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const runtime = new UserWasmRuntime(buildPeerCapsProbeWasm(), backend);

    await expect(runtime.run()).resolves.toBe(0);

    expect(opcodes).toEqual([OP_EXT.IPC_PEER_CAPS, OP_WASI.PROC_EXIT]);
    const exposed = runtime as unknown as { memory: WebAssembly.Memory };
    expect(new DataView(exposed.memory.buffer).getBigUint64(8, true)).toBe(
      peerCaps,
    );
  });

  it("ipc_peer_pid import dispatches the fd-scoped query and writes the i32 result", async () => {
    const peerPid = 73n;
    const opcodes: number[] = [];
    const backend: KernelBackend = {
      dispatch(request) {
        opcodes.push(request.opcode);
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: request.opcode === OP_EXT.IPC_PEER_PID ? peerPid : 0n,
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const runtime = new UserWasmRuntime(buildPeerPidProbeWasm(), backend);

    await expect(runtime.run()).resolves.toBe(0);

    expect(opcodes).toEqual([OP_EXT.IPC_PEER_PID, OP_WASI.PROC_EXIT]);
    const exposed = runtime as unknown as { memory: WebAssembly.Memory };
    expect(new DataView(exposed.memory.buffer).getInt32(8, true)).toBe(
      Number(peerPid),
    );
  });

  it("ipc_peer_pid preserves the output on backend errors and invalid pid values", () => {
    let status = -ERRNO.EBADF;
    let value = 73n;
    const backend: KernelBackend = {
      dispatch(request) {
        return {
          response: {
            requestId: request.requestId,
            status,
            value,
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const { memory, imports } = directRuntimeImports(backend);
    const view = new DataView(memory.buffer);
    view.setInt32(8, 0x1234_5678, true);

    expect(imports.pmos_ext.ipc_peer_pid(41, 8)).toBe(-ERRNO.EBADF);
    expect(view.getInt32(8, true)).toBe(0x1234_5678);

    status = 0;
    value = 0x8000_0000n;
    expect(imports.pmos_ext.ipc_peer_pid(41, 8)).toBe(-ERRNO.EIO);
    expect(view.getInt32(8, true)).toBe(0x1234_5678);
  });

  it("ipc_socket forwards reserved DGRAM type 1 and propagates ENOTSUP", () => {
    const calls: Array<{ opcode: number; arg0: number | undefined }> = [];
    const backend: KernelBackend = {
      dispatch(request) {
        calls.push({ opcode: request.opcode, arg0: request.arg0 });
        return {
          response: {
            requestId: request.requestId,
            status: -ERRNO.ENOTSUP,
            value: 0n,
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const { imports } = directRuntimeImports(backend);

    expect(imports.pmos_ext.ipc_socket(1)).toBe(-ERRNO.ENOTSUP);
    expect(calls).toEqual([{ opcode: OP_EXT.IPC_SOCKET, arg0: 1 }]);
  });

  it("host transfer imports preserve metadata and fd return semantics", () => {
    const calls: Array<{ opcode: number; args?: Uint8Array; heap?: Uint8Array }> = [];
    const backend: KernelBackend = {
      dispatch(request, heap) {
        calls.push({
          opcode: request.opcode,
          ...(request.args !== undefined ? { args: new Uint8Array(request.args) } : {}),
          ...(heap !== undefined ? { heap: new Uint8Array(heap) } : {}),
        });
        const value =
          request.opcode === OP_EXT.HOST_FILE_RECV
            ? 41n
            : request.opcode === OP_EXT.HOST_FILE_SEND
              ? 42n
              : 0n;
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value,
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const runtime = new UserWasmRuntime(buildPeerCapsProbeWasm(), backend);
    const exposed = runtime as unknown as {
      memory: WebAssembly.Memory;
      buildImports(): {
        pmos_ext: {
          fs_chmod(pathPtr: number, pathLen: number, mode: number): number;
          host_file_recv(token: number): number;
          host_file_pick(): number;
          host_file_send(
            namePtr: number,
            nameLen: number,
            mimePtr: number,
            mimeLen: number,
          ): number;
        };
      };
    };
    exposed.memory = new WebAssembly.Memory({ initial: 1 });
    const name = new TextEncoder().encode("notes.txt");
    const mime = new TextEncoder().encode("text/plain");
    const memory = new Uint8Array(exposed.memory.buffer);
    memory.set(name, 64);
    memory.set(mime, 96);
    const host = exposed.buildImports().pmos_ext;

    expect(host.fs_chmod(64, name.length, 0o755)).toBe(0);
    expect(host.host_file_recv(9)).toBe(41);
    expect(host.host_file_pick()).toBe(0);
    expect(host.host_file_send(64, name.length, 96, mime.length)).toBe(42);
    expect(calls.map((call) => call.opcode)).toEqual([
      OP_EXT.FS_CHMOD,
      OP_EXT.HOST_FILE_RECV,
      OP_EXT.HOST_FILE_PICK,
      OP_EXT.HOST_FILE_SEND,
    ]);
    const chmod = calls[0]!;
    expect(new DataView(chmod.args!.buffer).getUint32(0, true)).toBe(name.length);
    expect(new DataView(chmod.args!.buffer).getUint32(4, true)).toBe(0o755);
    expect(chmod.heap).toEqual(name);
    const send = calls[3]!;
    expect(new DataView(send.args!.buffer).getUint32(0, true)).toBe(name.length);
    expect(new DataView(send.args!.buffer).getUint32(4, true)).toBe(mime.length);
    expect(send.heap).toEqual(
      new Uint8Array([...name, ...mime]),
    );
  });

  it("fs_watch copies a bounded path and rejects malformed calls before dispatch", () => {
    const calls: Array<{ args: Uint8Array; heap: Uint8Array }> = [];
    const backend: KernelBackend = {
      dispatch(request, heap) {
        calls.push({
          args: new Uint8Array(request.args ?? []),
          heap: new Uint8Array(heap ?? []),
        });
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: 37n,
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const runtime = new UserWasmRuntime(buildPeerCapsProbeWasm(), backend);
    const exposed = runtime as unknown as {
      memory: WebAssembly.Memory;
      buildImports(): {
        pmos_ext: {
          fs_watch(pathPtr: number, pathLen: number, mask: number, flags: number): number;
        };
      };
    };
    exposed.memory = new WebAssembly.Memory({ initial: 1 });
    const memory = new Uint8Array(exposed.memory.buffer);
    const pathBytes = new TextEncoder().encode("/etc");
    memory.set(pathBytes, 64);
    const watch = exposed.buildImports().pmos_ext.fs_watch;

    expect(watch(64, pathBytes.length, 0x0007, 0)).toBe(37);
    expect(calls).toHaveLength(1);
    expect(calls[0]!.heap).toEqual(pathBytes);
    const args = new DataView(calls[0]!.args.buffer);
    expect(args.getUint32(0, true)).toBe(0);
    expect(args.getUint32(4, true)).toBe(pathBytes.length);
    expect(args.getUint32(8, true)).toBe(0x0007);
    expect(args.getUint32(12, true)).toBe(0);

    expect(watch(64, pathBytes.length, 0, 0)).toBe(-ERRNO.EINVAL);
    expect(watch(64, pathBytes.length, 0x0008, 0)).toBe(-ERRNO.EINVAL);
    expect(watch(64, pathBytes.length, 0x0001, 1)).toBe(-ERRNO.EINVAL);
    expect(watch(-1, pathBytes.length, 0x0001, 0)).toBe(-ERRNO.EFAULT);
    expect(watch(memory.length - 3, 4, 0x0001, 0)).toBe(-ERRNO.EFAULT);
    expect(calls).toHaveLength(1);

    // Exact-end ranges are valid; no `ptr + len` overflow arithmetic is used.
    expect(watch(memory.length - 4, 4, 0x0001, 0)).toBe(37);
    expect(calls).toHaveLength(2);
  });

  it("ipc bind and connect reject invalid path ranges before dispatch", () => {
    const calls: Array<{ opcode: number; arg0?: number; heap: Uint8Array }> = [];
    const backend: KernelBackend = {
      dispatch(request, heap) {
        calls.push({
          opcode: request.opcode,
          ...(request.arg0 !== undefined ? { arg0: request.arg0 } : {}),
          heap: new Uint8Array(heap ?? []),
        });
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: 0n,
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const runtime = new UserWasmRuntime(buildPeerCapsProbeWasm(), backend);
    const exposed = runtime as unknown as {
      memory: WebAssembly.Memory;
      buildImports(): {
        pmos_ext: {
          ipc_bind(fd: number, pathPtr: number, pathLen: number): number;
          ipc_connect(fd: number, pathPtr: number, pathLen: number): number;
        };
      };
    };
    exposed.memory = new WebAssembly.Memory({ initial: 1 });
    const memory = new Uint8Array(exposed.memory.buffer);
    const pathBytes = new TextEncoder().encode("/run/test");
    memory.set(pathBytes, 64);
    const { ipc_bind: bind, ipc_connect: connect } = exposed.buildImports().pmos_ext;

    expect(bind(3, 64, pathBytes.length)).toBe(0);
    expect(connect(4, 64, pathBytes.length)).toBe(0);
    expect(calls).toEqual([
      { opcode: OP_EXT.IPC_BIND, arg0: 3, heap: pathBytes },
      { opcode: OP_EXT.IPC_CONNECT, arg0: 4, heap: pathBytes },
    ]);

    for (const call of [bind, connect]) {
      expect(call(3, 64, 0)).toBe(-ERRNO.EINVAL);
      expect(call(3, Number.NaN, 1)).toBe(-ERRNO.EINVAL);
      expect(call(3, 64, 1.5)).toBe(-ERRNO.EINVAL);
      expect(call(3, -1, 1)).toBe(-ERRNO.EFAULT);
      expect(call(3, memory.length - 3, 4)).toBe(-ERRNO.EFAULT);
      expect(call(3, memory.length + 1, 1)).toBe(-ERRNO.EFAULT);
    }
    expect(calls).toHaveLength(2);

    memory.set(pathBytes, memory.length - pathBytes.length);
    expect(bind(5, memory.length - pathBytes.length, pathBytes.length)).toBe(0);
    expect(calls).toHaveLength(3);
    expect(calls[2]).toEqual({
      opcode: OP_EXT.IPC_BIND,
      arg0: 5,
      heap: pathBytes,
    });
  });

  it("path imports preserve directory fds for nested relative removal", () => {
    const calls: Array<{
      opcode: number;
      arg0?: number;
      args?: Uint8Array;
      heap: Uint8Array;
    }> = [];
    const backend: KernelBackend = {
      dispatch(request, heap) {
        calls.push({
          opcode: request.opcode,
          ...(request.arg0 !== undefined ? { arg0: request.arg0 } : {}),
          ...(request.args !== undefined
            ? { args: new Uint8Array(request.args) }
            : {}),
          heap: heap === undefined ? new Uint8Array() : new Uint8Array(heap),
        });
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: request.opcode === OP_WASI.PATH_OPEN ? 41n : 0n,
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const runtime = new UserWasmRuntime(buildPeerCapsProbeWasm(), backend);
    const exposed = runtime as unknown as {
      memory: WebAssembly.Memory;
      buildImports(): {
        wasi_snapshot_preview1: {
          path_open(
            dirfd: number,
            dirflags: number,
            pathPtr: number,
            pathLen: number,
            oflags: number,
            rightsBase: bigint,
            rightsInheriting: bigint,
            fdflags: number,
            fdOutPtr: number,
          ): number;
          path_unlink_file(
            dirfd: number,
            pathPtr: number,
            pathLen: number,
          ): number;
          path_remove_directory(
            dirfd: number,
            pathPtr: number,
            pathLen: number,
          ): number;
        };
      };
    };
    exposed.memory = new WebAssembly.Memory({ initial: 1 });
    const bytes = new Uint8Array(exposed.memory.buffer);
    const bin = new TextEncoder().encode("bin");
    const child = new TextEncoder().encode("hello.wasm");
    bytes.set(bin, 64);
    bytes.set(child, 96);
    const wasi = exposed.buildImports().wasi_snapshot_preview1;

    expect(wasi.path_open(10, 0, 64, bin.length, 2, 0n, 0n, 0, 160)).toBe(0);
    expect(new DataView(exposed.memory.buffer).getUint32(160, true)).toBe(41);
    expect(wasi.path_unlink_file(41, 96, child.length)).toBe(0);
    expect(wasi.path_remove_directory(10, 64, bin.length)).toBe(0);

    expect(calls.map((call) => call.opcode)).toEqual([
      OP_WASI.PATH_OPEN,
      OP_WASI.PATH_UNLINK_FILE,
      OP_WASI.PATH_REMOVE_DIRECTORY,
    ]);
    const openArgs = new DataView(
      calls[0]!.args!.buffer,
      calls[0]!.args!.byteOffset,
      calls[0]!.args!.byteLength,
    );
    expect(openArgs.getUint32(12, true)).toBe(10);
    expect(calls[0]!.heap).toEqual(bin);
    expect(calls[1]!.arg0).toBe(41);
    expect(calls[1]!.heap).toEqual(child);
    expect(calls[2]!.arg0).toBe(10);
    expect(calls[2]!.heap).toEqual(bin);
  });

  it("poll_oneoff rejects a negative subscription count before allocating", () => {
    let dispatches = 0;
    const backend: KernelBackend = {
      dispatch() {
        dispatches += 1;
        throw new Error("malformed poll must not reach the backend");
      },
    };
    const runtime = new UserWasmRuntime(buildPeerCapsProbeWasm(), backend);
    const exposed = runtime as unknown as {
      memory: WebAssembly.Memory;
      buildImports(): {
        wasi_snapshot_preview1: {
          poll_oneoff(
            inPtr: number,
            outPtr: number,
            nsubscriptions: number,
            neventsPtr: number,
          ): number;
        };
      };
    };
    exposed.memory = new WebAssembly.Memory({ initial: 1 });

    const errno = exposed
      .buildImports()
      .wasi_snapshot_preview1.poll_oneoff(0, 64, -1, 128);

    expect(errno).toBe(ERRNO.EINVAL);
    expect(
      exposed
        .buildImports()
        .wasi_snapshot_preview1.poll_oneoff(0, 64, 257, 128),
    ).toBe(ERRNO.EINVAL);
    expect(dispatches).toBe(0);
  });

  it("poll_oneoff validates negative, exact-end, and cross-end pointer ranges", () => {
    let dispatches = 0;
    const backend: KernelBackend = {
      dispatch() {
        dispatches += 1;
        return {
          response: {
            requestId: 0,
            status: 0,
            value: 0n,
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const runtime = new UserWasmRuntime(buildPeerCapsProbeWasm(), backend);
    const exposed = runtime as unknown as {
      memory: WebAssembly.Memory;
      buildImports(): {
        wasi_snapshot_preview1: {
          poll_oneoff(
            inPtr: number,
            outPtr: number,
            nsubscriptions: number,
            neventsPtr: number,
          ): number;
        };
      };
    };
    exposed.memory = new WebAssembly.Memory({ initial: 1 });
    const poll = exposed.buildImports().wasi_snapshot_preview1.poll_oneoff;
    const end = exposed.memory.buffer.byteLength;

    expect(poll(-1, end - 32, 1, end - 4)).toBe(ERRNO.EFAULT);
    expect(poll(end - 48, end - 31, 1, end - 4)).toBe(ERRNO.EFAULT);
    expect(dispatches).toBe(0);

    expect(poll(end - 48, end - 32, 1, end - 4)).toBe(0);
    expect(dispatches).toBe(1);
    expect(new DataView(exposed.memory.buffer).getUint32(end - 4, true)).toBe(0);
  });

  it("poll_oneoff rejects impossible backend event counts and short output", () => {
    let responseValue = 2n;
    let heapOut = new Uint8Array(64);
    const backend: KernelBackend = {
      dispatch() {
        return {
          response: {
            requestId: 0,
            status: 0,
            value: responseValue,
            extraLen: heapOut.byteLength,
          },
          heapOut,
        };
      },
    };
    const runtime = new UserWasmRuntime(buildPeerCapsProbeWasm(), backend);
    const exposed = runtime as unknown as {
      memory: WebAssembly.Memory;
      buildImports(): {
        wasi_snapshot_preview1: {
          poll_oneoff(
            inPtr: number,
            outPtr: number,
            nsubscriptions: number,
            neventsPtr: number,
          ): number;
        };
      };
    };
    exposed.memory = new WebAssembly.Memory({ initial: 1 });
    const poll = exposed.buildImports().wasi_snapshot_preview1.poll_oneoff;

    expect(poll(0, 128, 1, 256)).toBe(ERRNO.EIO);
    responseValue = 1n;
    heapOut = new Uint8Array(31);
    expect(poll(0, 128, 1, 256)).toBe(ERRNO.EIO);
  });

  it("fd_write chunks payloads larger than the per-process syscall heap", () => {
    const writes: Uint8Array[] = [];
    const backend: KernelBackend = {
      dispatch(request, heap) {
        expect(request.opcode).toBe(OP_WASI.FD_WRITE);
        const bytes =
          heap === undefined ? new Uint8Array() : new Uint8Array(heap);
        writes.push(bytes);
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: BigInt(bytes.length),
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const runtime = new UserWasmRuntime(buildPeerCapsProbeWasm(), backend);
    const exposed = runtime as unknown as {
      memory: WebAssembly.Memory;
      buildImports(): {
        wasi_snapshot_preview1: {
          fd_write(
            fd: number,
            iovsPtr: number,
            iovsLen: number,
            nwrittenPtr: number,
          ): number;
        };
      };
    };
    exposed.memory = new WebAssembly.Memory({ initial: 2 });

    const payload = Uint8Array.from(
      { length: 65_024 },
      (_, index) => (index * 31 + 7) & 0xff,
    );
    const iovsPtr = 16;
    const nwrittenPtr = 32;
    const payloadPtr = 256;
    new Uint8Array(exposed.memory.buffer).set(payload, payloadPtr);
    const view = new DataView(exposed.memory.buffer);
    view.setUint32(iovsPtr, payloadPtr, true);
    view.setUint32(iovsPtr + 4, payload.length, true);

    const fdWrite = exposed.buildImports().wasi_snapshot_preview1.fd_write;
    let totalWritten = 0;
    while (totalWritten < payload.length) {
      view.setUint32(iovsPtr, payloadPtr + totalWritten, true);
      view.setUint32(iovsPtr + 4, payload.length - totalWritten, true);
      expect(fdWrite(41, iovsPtr, 1, nwrittenPtr)).toBe(0);
      const written = view.getUint32(nwrittenPtr, true);
      expect(written).toBeGreaterThan(0);
      totalWritten += written;
    }

    expect(totalWritten).toBe(payload.length);
    expect(writes.map((chunk) => chunk.length)).toEqual([
      HEAP_SCRATCH_BYTES,
      payload.length - HEAP_SCRATCH_BYTES,
    ]);
    const written = new Uint8Array(payload.length);
    let offset = 0;
    for (const chunk of writes) {
      written.set(chunk, offset);
      offset += chunk.length;
    }
    expect(written).toEqual(payload);
  });

  it("fd_read returns a bounded short read for oversized iovecs", () => {
    const expected = Uint8Array.from(
      { length: HEAP_SCRATCH_BYTES },
      (_, index) => (index * 17 + 11) & 0xff,
    );
    const capacities: number[] = [];
    const backend: KernelBackend = {
      dispatch(request) {
        expect(request.opcode).toBe(OP_WASI.FD_READ);
        capacities.push(request.heapLen ?? 0);
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: BigInt(expected.length),
            extraLen: expected.length,
          },
          heapOut: expected,
        };
      },
    };
    const runtime = new UserWasmRuntime(buildPeerCapsProbeWasm(), backend);
    const exposed = runtime as unknown as {
      memory: WebAssembly.Memory;
      buildImports(): {
        wasi_snapshot_preview1: {
          fd_read(
            fd: number,
            iovsPtr: number,
            iovsLen: number,
            nreadPtr: number,
          ): number;
        };
      };
    };
    exposed.memory = new WebAssembly.Memory({ initial: 2 });
    const iovsPtr = 16;
    const nreadPtr = 40;
    const firstPtr = 256;
    const firstLen = 20_000;
    const secondPtr = 24_000;
    const secondLen = 45_000;
    const bytes = new Uint8Array(exposed.memory.buffer);
    bytes.fill(0xaa, firstPtr, firstPtr + firstLen);
    bytes.fill(0xaa, secondPtr, secondPtr + secondLen);
    const view = new DataView(exposed.memory.buffer);
    view.setUint32(iovsPtr, firstPtr, true);
    view.setUint32(iovsPtr + 4, firstLen, true);
    view.setUint32(iovsPtr + 8, secondPtr, true);
    view.setUint32(iovsPtr + 12, secondLen, true);

    const errno = exposed
      .buildImports()
      .wasi_snapshot_preview1.fd_read(41, iovsPtr, 2, nreadPtr);

    expect(errno).toBe(0);
    expect(capacities).toEqual([HEAP_SCRATCH_BYTES]);
    expect(view.getUint32(nreadPtr, true)).toBe(HEAP_SCRATCH_BYTES);
    expect(bytes.slice(firstPtr, firstPtr + firstLen)).toEqual(
      expected.slice(0, firstLen),
    );
    const secondWritten = HEAP_SCRATCH_BYTES - firstLen;
    expect(bytes.slice(secondPtr, secondPtr + secondWritten)).toEqual(
      expected.slice(firstLen),
    );
    expect(bytes[secondPtr + secondWritten]).toBe(0xaa);
  });

  it("ipc_send and ipc_recv preserve bounded payload, fd, and blocking flag wire semantics", () => {
    expect(OP_EXT.IPC_SEND).toBe(0x1005);
    expect(OP_EXT.IPC_RECV).toBe(0x1006);
    const calls: Array<{ opcode: number; args: Uint8Array; heapLen: number; heap: Uint8Array }> = [];
    let recvIndex = 0;
    const backend: KernelBackend = {
      dispatch(request, heap) {
        calls.push({
          opcode: request.opcode,
          args: new Uint8Array(request.args ?? []),
          heapLen: request.heapLen ?? 0,
          heap: new Uint8Array(heap ?? []),
        });
        if (request.opcode === OP_EXT.IPC_SEND) {
          return {
            response: {
              requestId: request.requestId,
              status: 0,
              value: 3n,
              extraLen: 0,
            },
            heapOut: new Uint8Array(),
          };
        }
        recvIndex += 1;
        if (recvIndex === 1) {
          return {
            response: {
              requestId: request.requestId,
              status: 0,
              value: 3n,
              extraLen: 3,
            },
            heapOut: new Uint8Array([6, 7, 8]),
          };
        }
        const out = new Uint8Array(6);
        new DataView(out.buffer).setUint32(0, 55, true);
        out.set([9, 10], 4);
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: 2n,
            extraLen: 6,
          },
          heapOut: out,
        };
      },
    };
    const { memory, imports } = directRuntimeImports(backend);
    const bytes = new Uint8Array(memory.buffer);
    bytes.set([1, 2, 3, 4, 5], 64);

    expect(imports.pmos_ext.ipc_send(41, 64, 5, 17, 0)).toBe(3);
    expect(imports.pmos_ext.ipc_recv(42, 128, 8, -1, 0)).toBe(3);
    expect(bytes.slice(128, 131)).toEqual(new Uint8Array([6, 7, 8]));
    expect(imports.pmos_ext.ipc_recv(42, 160, 8, 192, 1)).toBe(2);
    expect(bytes.slice(160, 162)).toEqual(new Uint8Array([9, 10]));
    expect(new DataView(memory.buffer).getInt32(192, true)).toBe(55);

    expect(calls.map((call) => call.opcode)).toEqual([
      OP_EXT.IPC_SEND,
      OP_EXT.IPC_RECV,
      OP_EXT.IPC_RECV,
    ]);
    const sendArgs = new DataView(calls[0]!.args.buffer);
    expect(sendArgs.getUint32(0, true)).toBe(41);
    expect(sendArgs.getUint32(4, true)).toBe(5);
    expect(sendArgs.getInt32(8, true)).toBe(17);
    expect(sendArgs.getUint32(12, true)).toBe(0);
    expect(calls[0]!.heap).toEqual(new Uint8Array([1, 2, 3, 4, 5]));

    const blockingRecv = new DataView(calls[1]!.args.buffer);
    expect(blockingRecv.getInt32(8, true)).toBe(-1);
    expect(blockingRecv.getUint32(12, true)).toBe(0);
    expect(calls[1]!.heapLen).toBe(8);
    const nonblockingRecv = new DataView(calls[2]!.args.buffer);
    expect(nonblockingRecv.getInt32(8, true)).toBe(0);
    expect(nonblockingRecv.getUint32(12, true)).toBe(1);
    expect(calls[2]!.heapLen).toBe(12);
  });

  it("blocking ipc_recv crosses the production SAB park/wake bridge and decodes an fd-prefixed wake", () => {
    const sab = new Uint8Array(new SharedArrayBuffer(SAB_SIZE));
    const header = new Int32Array(sab.buffer, 0, OFF_HEAP_SCRATCH / 4);
    const wakeSlot = new Int32Array(new SharedArrayBuffer(32));
    const backend = new SabBackend({ sab, pid: 7, kernelWakeSlot: wakeSlot });
    const { memory, imports } = directRuntimeImports(backend);

    type Wait = (
      view: Int32Array,
      index: number,
      value: number,
      timeout?: number,
    ) => "ok" | "not-equal" | "timed-out";
    const atomics = Atomics as unknown as { wait: Wait };
    const originalWait = atomics.wait;
    let waitCalls = 0;
    atomics.wait = () => {
      waitCalls += 1;
      const heap = new Uint8Array(sab.buffer, OFF_HEAP_SCRATCH, 7);
      new DataView(heap.buffer, heap.byteOffset, heap.byteLength).setUint32(0, 73, true);
      heap.set([31, 32, 33], 4);

      const response = new Uint8Array(32);
      const fields = new DataView(response.buffer);
      fields.setUint32(0, 0, true);
      fields.setInt32(4, 0, true);
      fields.setBigInt64(8, 3n, true);
      fields.setUint32(16, 7, true);
      new Uint8Array(sab.buffer, OFF_RES_RING, response.length).set(response);
      Atomics.store(header, OFF_RES_HEAD / 4, 1);
      Atomics.store(header, OFF_USER_WAIT_SLOT / 4, STATUS_READY);
      return "ok";
    };

    try {
      expect(imports.pmos_ext.ipc_recv(41, 128, 8, 160, 0)).toBe(3);
    } finally {
      atomics.wait = originalWait;
    }

    expect(waitCalls).toBe(1);
    expect(new Uint8Array(memory.buffer).slice(128, 131)).toEqual(
      new Uint8Array([31, 32, 33]),
    );
    expect(new DataView(memory.buffer).getInt32(160, true)).toBe(73);
    const request = new DataView(sab.buffer, OFF_REQ_RING, 32);
    expect(request.getUint16(0, true)).toBe(OP_EXT.IPC_RECV);
    expect(request.getUint32(8 + 12, true)).toBe(0);
    expect(request.getUint32(28, true)).toBe(12);
  });

  it("ipc send and recv validate every guest output before dispatch and accept exact-end ranges", () => {
    let dispatches = 0;
    const backend: KernelBackend = {
      dispatch(request, heap) {
        dispatches += 1;
        const len = request.args === undefined
          ? 0
          : new DataView(request.args.buffer, request.args.byteOffset).getUint32(4, true);
        const payload = request.opcode === OP_EXT.IPC_RECV
          ? Uint8Array.from({ length: len }, (_, index) => index + 1)
          : new Uint8Array();
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: BigInt(request.opcode === OP_EXT.IPC_SEND ? heap?.length ?? 0 : len),
            extraLen: payload.length,
          },
          heapOut: payload,
        };
      },
    };
    const { memory, imports } = directRuntimeImports(backend);
    const end = memory.buffer.byteLength;
    const send = imports.pmos_ext.ipc_send;
    const recv = imports.pmos_ext.ipc_recv;

    expect(send(3, end - 3, 4, -1, 0)).toBe(-ERRNO.EFAULT);
    expect(send(3, -1, 1, -1, 0)).toBe(-ERRNO.EFAULT);
    expect(send(3, 0, 1, -1, 1)).toBe(-ERRNO.EINVAL);
    expect(recv(3, end - 3, 4, -1, 0)).toBe(-ERRNO.EFAULT);
    expect(recv(3, 0, 1, end - 3, 0)).toBe(-ERRNO.EFAULT);
    expect(recv(3, 0, 1, -1, 2)).toBe(-ERRNO.EINVAL);
    expect(dispatches).toBe(0);

    new Uint8Array(memory.buffer).set([21, 22, 23, 24], end - 4);
    expect(send(3, end - 4, 4, -1, 0)).toBe(4);
    expect(recv(3, end - 4, 4, -1, 0)).toBe(4);
    expect(recv(3, end, 0, end - 4, 0)).toBe(0);
    expect(new DataView(memory.buffer).getInt32(end - 4, true)).toBe(-1);
    expect(dispatches).toBe(3);
  });

  it("ipc_send and ipc_recv cap transport heaps and retain owned send bytes", () => {
    let guestBytes: Uint8Array;
    const requests: Array<{ opcode: number; args: Uint8Array; heapLen: number }> = [];
    let capturedSend = new Uint8Array();
    const backend: KernelBackend = {
      dispatch(request, heap) {
        requests.push({
          opcode: request.opcode,
          args: new Uint8Array(request.args ?? []),
          heapLen: request.heapLen ?? 0,
        });
        if (request.opcode === OP_EXT.IPC_SEND) {
          capturedSend = new Uint8Array(heap ?? []);
          guestBytes[1024] = 0xff;
          return {
            response: {
              requestId: request.requestId,
              status: 0,
              value: BigInt(capturedSend.length),
              extraLen: 0,
            },
            heapOut: new Uint8Array(),
          };
        }
        const len = new DataView(request.args!.buffer, request.args!.byteOffset)
          .getUint32(4, true);
        const out = Uint8Array.from({ length: len }, (_, index) => index & 0xff);
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: BigInt(len),
            extraLen: len,
          },
          heapOut: out,
        };
      },
    };
    const { memory, imports } = directRuntimeImports(backend, 2);
    guestBytes = new Uint8Array(memory.buffer);
    guestBytes.fill(0x44, 1024, 41_024);

    expect(imports.pmos_ext.ipc_send(3, 1024, 40_000, -1, 0)).toBe(
      HEAP_SCRATCH_BYTES,
    );
    expect(capturedSend).toHaveLength(HEAP_SCRATCH_BYTES);
    expect(capturedSend[0]).toBe(0x44);
    expect(guestBytes[1024]).toBe(0xff);

    expect(imports.pmos_ext.ipc_recv(4, 50_000, 40_000, 100_000, 1)).toBe(
      HEAP_SCRATCH_BYTES - 4,
    );
    expect(requests[1]!.heapLen).toBe(HEAP_SCRATCH_BYTES);
    const recvArgs = new DataView(requests[1]!.args.buffer);
    expect(recvArgs.getUint32(4, true)).toBe(HEAP_SCRATCH_BYTES - 4);
    expect(new DataView(memory.buffer).getInt32(100_000, true)).toBe(-1);
    expect(guestBytes[50_000]).toBe(0);
    expect(guestBytes[50_001]).toBe(1);
  });

  it("fd_read and fd_write reject malformed iovec and result ranges without dispatch", () => {
    let dispatches = 0;
    const backend: KernelBackend = {
      dispatch(request, heap) {
        dispatches += 1;
        const count = request.opcode === OP_WASI.FD_WRITE
          ? heap?.length ?? 0
          : request.heapLen ?? 0;
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: BigInt(count),
            extraLen: request.opcode === OP_WASI.FD_READ ? count : 0,
          },
          heapOut: request.opcode === OP_WASI.FD_READ
            ? new Uint8Array(count).fill(0x5a)
            : new Uint8Array(),
        };
      },
    };
    const { memory, imports } = directRuntimeImports(backend);
    const end = memory.buffer.byteLength;
    const view = new DataView(memory.buffer);
    const { fd_read: read, fd_write: write } = imports.wasi_snapshot_preview1;

    expect(write(1, 0, -1, 32)).toBe(ERRNO.EINVAL);
    expect(read(1, end - 7, 1, 32)).toBe(ERRNO.EFAULT);
    expect(write(1, end, 0, end - 3)).toBe(ERRNO.EFAULT);
    expect(read(1, end, 0, end - 3)).toBe(ERRNO.EFAULT);
    view.setUint32(64, end - 3, true);
    view.setUint32(68, 4, true);
    expect(write(1, 64, 1, 80)).toBe(ERRNO.EFAULT);
    expect(read(1, 64, 1, 80)).toBe(ERRNO.EFAULT);
    expect(dispatches).toBe(0);

    view.setUint32(64, end - 4, true);
    view.setUint32(68, 4, true);
    expect(write(1, 64, 1, 80)).toBe(0);
    expect(read(1, 64, 1, 80)).toBe(0);
    expect(view.getUint32(80, true)).toBe(4);
    expect(new Uint8Array(memory.buffer).slice(end - 4)).toEqual(
      new Uint8Array([0x5a, 0x5a, 0x5a, 0x5a]),
    );
    expect(write(1, end, 0, end - 4)).toBe(0);
    expect(read(1, end, 0, end - 4)).toBe(0);
    expect(dispatches).toBe(2);
  });

  it("sock_send gathers and sock_recv scatters real bounded WASI iovec arrays", () => {
    const calls: Array<{ opcode: number; args: Uint8Array; heapLen: number; heap: Uint8Array }> = [];
    const recvBytes = Uint8Array.from(
      { length: 25_000 },
      (_, index) => (index * 13 + 9) & 0xff,
    );
    const backend: KernelBackend = {
      dispatch(request, heap) {
        calls.push({
          opcode: request.opcode,
          args: new Uint8Array(request.args ?? []),
          heapLen: request.heapLen ?? 0,
          heap: new Uint8Array(heap ?? []),
        });
        return request.opcode === OP_WASI.SOCK_SEND
          ? {
              response: {
                requestId: request.requestId,
                status: 0,
                value: 30_000n,
                extraLen: 0,
              },
              heapOut: new Uint8Array(),
            }
          : {
              response: {
                requestId: request.requestId,
                status: 0,
                value: BigInt(recvBytes.length),
                extraLen: recvBytes.length,
              },
              heapOut: recvBytes,
            };
      },
    };
    const { memory, imports } = directRuntimeImports(backend, 2);
    const bytes = new Uint8Array(memory.buffer);
    const view = new DataView(memory.buffer);
    const firstSend = Uint8Array.from({ length: 20_000 }, (_, i) => i & 0xff);
    const secondSend = Uint8Array.from({ length: 20_000 }, (_, i) => (i + 77) & 0xff);
    bytes.set(firstSend, 1024);
    bytes.set(secondSend, 22_000);
    view.setUint32(64, 1024, true);
    view.setUint32(68, firstSend.length, true);
    view.setUint32(72, 22_000, true);
    view.setUint32(76, secondSend.length, true);

    expect(imports.wasi_snapshot_preview1.sock_send(41, 64, 2, 0, 100_000)).toBe(0);
    expect(view.getUint32(100_000, true)).toBe(30_000);
    expect(calls[0]!.heapLen).toBe(HEAP_SCRATCH_BYTES);
    expect(calls[0]!.heap.slice(0, firstSend.length)).toEqual(firstSend);
    expect(calls[0]!.heap.slice(firstSend.length)).toEqual(
      secondSend.slice(0, HEAP_SCRATCH_BYTES - firstSend.length),
    );

    view.setUint32(64, 50_000, true);
    view.setUint32(68, 10_000, true);
    view.setUint32(72, 65_000, true);
    view.setUint32(76, 30_000, true);
    expect(imports.wasi_snapshot_preview1.sock_recv(42, 64, 2, 0, 100_000, 100_004)).toBe(0);
    expect(calls[1]!.heapLen).toBe(HEAP_SCRATCH_BYTES);
    expect(view.getUint32(100_000, true)).toBe(recvBytes.length);
    expect(view.getUint16(100_004, true)).toBe(0);
    expect(bytes.slice(50_000, 60_000)).toEqual(recvBytes.slice(0, 10_000));
    expect(bytes.slice(65_000, 80_000)).toEqual(recvBytes.slice(10_000));
    expect(new DataView(calls[0]!.args.buffer).getUint32(4, true)).toBe(0);
    expect(new DataView(calls[1]!.args.buffer).getUint32(4, true)).toBe(0);
  });

  it("WASI socket and PMos fd-output imports prevalidate exact ranges before mutating backends", () => {
    const opcodes: number[] = [];
    const backend: KernelBackend = {
      dispatch(request) {
        opcodes.push(request.opcode);
        if (request.opcode === OP_EXT.IPC_PIPE) {
          const heapOut = new Uint8Array(8);
          const view = new DataView(heapOut.buffer);
          view.setUint32(0, 21, true);
          view.setUint32(4, 22, true);
          return {
            response: { requestId: 0, status: 0, value: 0n, extraLen: 8 },
            heapOut,
          };
        }
        return {
          response: {
            requestId: request.requestId,
            status: 0,
            value: request.opcode === OP_WASI.SOCK_ACCEPT ? 23n : 0x1234n,
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const { memory, imports } = directRuntimeImports(backend);
    const end = memory.buffer.byteLength;
    const wasi = imports.wasi_snapshot_preview1;
    const ext = imports.pmos_ext;
    const view = new DataView(memory.buffer);

    expect(wasi.sock_accept(3, 0, end - 3)).toBe(ERRNO.EFAULT);
    expect(wasi.sock_send(3, end - 7, 1, 0, 64)).toBe(ERRNO.EFAULT);
    expect(wasi.sock_recv(3, end, 0, 0, end - 3, 64)).toBe(ERRNO.EFAULT);
    expect(wasi.sock_recv(3, end, 0, 0, 64, end - 1)).toBe(ERRNO.EFAULT);
    expect(ext.ipc_pipe(end - 7)).toBe(-ERRNO.EFAULT);
    expect(ext.ipc_peer_caps(3, end - 7)).toBe(-ERRNO.EFAULT);
    expect(ext.ipc_peer_caps(3, 0)).toBe(-ERRNO.EINVAL);
    expect(ext.ipc_peer_pid(3, end - 3)).toBe(-ERRNO.EFAULT);
    expect(ext.ipc_peer_pid(3, 0)).toBe(-ERRNO.EINVAL);
    expect(opcodes).toHaveLength(0);

    expect(wasi.sock_accept(3, 0, end - 4)).toBe(0);
    expect(view.getUint32(end - 4, true)).toBe(23);
    expect(ext.ipc_pipe(end - 8)).toBe(0);
    expect(view.getUint32(end - 8, true)).toBe(21);
    expect(view.getUint32(end - 4, true)).toBe(22);
    expect(ext.ipc_peer_caps(3, end - 8)).toBe(0);
    expect(view.getBigUint64(end - 8, true)).toBe(0x1234n);
    expect(ext.ipc_peer_pid(3, end - 4)).toBe(0);
    expect(view.getInt32(end - 4, true)).toBe(0x1234);
    expect(opcodes).toEqual([
      OP_WASI.SOCK_ACCEPT,
      OP_EXT.IPC_PIPE,
      OP_EXT.IPC_PEER_CAPS,
      OP_EXT.IPC_PEER_PID,
    ]);
  });

  it("socket flags retain their full u32 wire value and malformed wider values do not dispatch", () => {
    const flagsSeen: number[] = [];
    let queued = new Uint8Array([71, 72]);
    const backend: KernelBackend = {
      dispatch(request, heap) {
        const flags = new DataView(
          request.args!.buffer,
          request.args!.byteOffset,
        ).getUint32(4, true);
        flagsSeen.push(flags);
        if (flags === 0 && request.opcode === OP_WASI.SOCK_SEND) {
          queued = new Uint8Array(heap ?? []);
          return {
            response: {
              requestId: request.requestId,
              status: 0,
              value: BigInt(queued.length),
              extraLen: 0,
            },
            heapOut: new Uint8Array(),
          };
        }
        if (flags === 0 && request.opcode === OP_WASI.SOCK_RECV) {
          const out = queued;
          queued = new Uint8Array();
          return {
            response: {
              requestId: request.requestId,
              status: 0,
              value: BigInt(out.length),
              extraLen: out.length,
            },
            heapOut: out,
          };
        }
        return {
          response: {
            requestId: request.requestId,
            status: -ERRNO.EINVAL,
            value: 0n,
            extraLen: 0,
          },
          heapOut: new Uint8Array(),
        };
      },
    };
    const { memory, imports } = directRuntimeImports(backend);
    const end = memory.buffer.byteLength;
    const wasi = imports.wasi_snapshot_preview1;
    const view = new DataView(memory.buffer);
    view.setUint32(64, 128, true);
    view.setUint32(68, 2, true);
    view.setUint32(160, 0xfeed_beef, true);
    view.setUint16(164, 0xabcd, true);

    expect(wasi.sock_send(3, end, 0, 0x1_0000, end - 4)).toBe(ERRNO.EINVAL);
    expect(wasi.sock_recv(3, 64, 1, 0x8000_0000, 160, 164)).toBe(ERRNO.EINVAL);
    expect(flagsSeen).toEqual([0x1_0000, 0x8000_0000]);
    expect(view.getUint32(160, true)).toBe(0xfeed_beef);
    expect(view.getUint16(164, true)).toBe(0xabcd);
    expect(wasi.sock_recv(3, 64, 1, 0, 160, 164)).toBe(0);
    expect(new Uint8Array(memory.buffer).slice(128, 130)).toEqual(
      new Uint8Array([71, 72]),
    );
    expect(view.getUint32(160, true)).toBe(2);
    expect(view.getUint16(164, true)).toBe(0);
    expect(wasi.sock_send(3, end, 0, 0x1_0000_0000, end - 4)).toBe(ERRNO.EINVAL);
    expect(wasi.sock_recv(3, end, 0, -1, end - 4, end - 6)).toBe(ERRNO.EINVAL);
    expect(flagsSeen).toEqual([0x1_0000, 0x8000_0000, 0]);
  });

  it("I/O shims preserve signed errno conventions and reject impossible success shapes", () => {
    let mode: "positive-status" | "negative-status" | "oversized" | "bad-extra" =
      "positive-status";
    const backend: KernelBackend = {
      dispatch(request) {
        const status = mode === "positive-status"
          ? ERRNO.EAGAIN
          : mode === "negative-status"
            ? -ERRNO.EAGAIN
            : 0;
        const value = mode === "oversized" ? 2n : 1n;
        const extraLen = mode === "bad-extra" ? 2 : 0;
        return {
          response: { requestId: request.requestId, status, value, extraLen },
          heapOut: mode === "bad-extra" ? new Uint8Array([99, 100]) : new Uint8Array(),
        };
      },
    };
    const { memory, imports } = directRuntimeImports(backend);
    const bytes = new Uint8Array(memory.buffer);
    const view = new DataView(memory.buffer);
    bytes[128] = 7;
    view.setUint32(64, 128, true);
    view.setUint32(68, 1, true);
    view.setUint32(160, 0xfeed_beef, true);

    expect(imports.wasi_snapshot_preview1.fd_write(1, 64, 1, 160)).toBe(ERRNO.EIO);
    expect(view.getUint32(160, true)).toBe(0xfeed_beef);
    expect(imports.pmos_ext.ipc_send(3, 128, 1, -1, 0)).toBe(-ERRNO.EIO);

    mode = "negative-status";
    expect(imports.wasi_snapshot_preview1.fd_write(1, 64, 1, 160)).toBe(ERRNO.EAGAIN);
    expect(imports.pmos_ext.ipc_send(3, 128, 1, -1, 0)).toBe(-ERRNO.EAGAIN);

    mode = "oversized";
    expect(imports.wasi_snapshot_preview1.sock_send(3, 64, 1, 0, 160)).toBe(ERRNO.EIO);
    expect(view.getUint32(160, true)).toBe(0xfeed_beef);

    mode = "bad-extra";
    bytes[192] = 0xaa;
    view.setInt32(196, 0x1234_5678, true);
    expect(imports.pmos_ext.ipc_recv(3, 192, 1, 196, 0)).toBe(-ERRNO.EIO);
    expect(bytes[192]).toBe(0xaa);
    expect(view.getInt32(196, true)).toBe(0x1234_5678);
  });

  it("oversized bind, connect, and watch paths fail deterministically before transport dispatch", () => {
    let dispatches = 0;
    const backend: KernelBackend = {
      dispatch(request) {
        dispatches += 1;
        return {
          response: { requestId: request.requestId, status: 0, value: 40n, extraLen: 0 },
          heapOut: new Uint8Array(),
        };
      },
    };
    const { imports } = directRuntimeImports(backend, 2);
    const ext = imports.pmos_ext;

    expect(ext.ipc_bind(3, 0, HEAP_SCRATCH_BYTES + 1)).toBe(-ERRNO.EINVAL);
    expect(ext.ipc_connect(3, 0, HEAP_SCRATCH_BYTES + 1)).toBe(-ERRNO.EINVAL);
    expect(ext.fs_watch(0, HEAP_SCRATCH_BYTES + 1, 1, 0)).toBe(-ERRNO.EINVAL);
    expect(dispatches).toBe(0);

    expect(ext.ipc_bind(3, 0, HEAP_SCRATCH_BYTES)).toBe(0);
    expect(ext.ipc_connect(3, 0, HEAP_SCRATCH_BYTES)).toBe(0);
    expect(ext.fs_watch(0, HEAP_SCRATCH_BYTES, 1, 0)).toBe(40);
    expect(dispatches).toBe(3);
  });
});

/**
 * Build a tiny module that calls `pmos_ext.ipc_peer_caps(41, 8)`
 * and drops the errno result. The test reads memory offset 8 after
 * `_start` returns to verify the shim's out-pointer write.
 */
function buildPeerCapsProbeWasm(): ArrayBuffer {
  const typeSection = section(1, [
    0x02,
    0x60,
    0x02,
    0x7f,
    0x7f,
    0x01,
    0x7f, // (i32, i32) -> i32
    0x60,
    0x00,
    0x00, // () -> ()
  ]);
  const importSection = section(2, [
    0x01,
    ...encodeString("pmos_ext"),
    ...encodeString("ipc_peer_caps"),
    0x00,
    0x00,
  ]);
  const functionSection = section(3, [0x01, 0x01]);
  const memorySection = section(5, [0x01, 0x00, 0x01]);
  const exportSection = section(7, [
    0x02,
    ...encodeString("_start"),
    0x00,
    0x01,
    ...encodeString("memory"),
    0x02,
    0x00,
  ]);
  const body = [
    0x00, // no locals
    0x41,
    0x29, // i32.const 41 (fd)
    0x41,
    0x08, // i32.const 8 (out pointer)
    0x10,
    0x00, // call imported function 0
    0x1a, // drop errno
    0x0b,
  ];
  const codeSection = section(10, [0x01, body.length, ...body]);

  return new Uint8Array([
    0x00,
    0x61,
    0x73,
    0x6d,
    0x01,
    0x00,
    0x00,
    0x00,
    ...typeSection,
    ...importSection,
    ...functionSection,
    ...memorySection,
    ...exportSection,
    ...codeSection,
  ]).buffer as ArrayBuffer;
}

/**
 * Build a tiny module that calls `pmos_ext.ipc_peer_pid(41, 8)`
 * and drops the errno result. The test reads memory offset 8 after
 * `_start` returns to verify the shim's signed i32 out-pointer write.
 */
function buildPeerPidProbeWasm(): ArrayBuffer {
  const typeSection = section(1, [
    0x02,
    0x60,
    0x02,
    0x7f,
    0x7f,
    0x01,
    0x7f, // (i32, i32) -> i32
    0x60,
    0x00,
    0x00, // () -> ()
  ]);
  const importSection = section(2, [
    0x01,
    ...encodeString("pmos_ext"),
    ...encodeString("ipc_peer_pid"),
    0x00,
    0x00,
  ]);
  const functionSection = section(3, [0x01, 0x01]);
  const memorySection = section(5, [0x01, 0x00, 0x01]);
  const exportSection = section(7, [
    0x02,
    ...encodeString("_start"),
    0x00,
    0x01,
    ...encodeString("memory"),
    0x02,
    0x00,
  ]);
  const body = [
    0x00, // no locals
    0x41,
    0x29, // i32.const 41 (fd)
    0x41,
    0x08, // i32.const 8 (out pointer)
    0x10,
    0x00, // call imported function 0
    0x1a, // drop errno
    0x0b,
  ];
  const codeSection = section(10, [0x01, body.length, ...body]);

  return new Uint8Array([
    0x00,
    0x61,
    0x73,
    0x6d,
    0x01,
    0x00,
    0x00,
    0x00,
    ...typeSection,
    ...importSection,
    ...functionSection,
    ...memorySection,
    ...exportSection,
    ...codeSection,
  ]).buffer as ArrayBuffer;
}

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
