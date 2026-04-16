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
import { CAPSET_ALL } from "../../src/shared/syscall";

let kernelWasmBytes: ArrayBuffer;
let helloWasmBytes: ArrayBuffer;

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

  for (const p of [kernelPath, helloPath]) {
    if (!fs.existsSync(p)) {
      throw new Error(
        `${p} not found. Run \`just build\` (or the cargo build lines from the Justfile's build target) first.`,
      );
    }
  }

  // Copy each Node `Buffer` into a fresh `ArrayBuffer`; see
  // `kernel-wasm-host.test.ts` for the explanation of why this
  // is needed under modern TS types.
  {
    const raw = fs.readFileSync(kernelPath);
    kernelWasmBytes = raw.buffer.slice(
      raw.byteOffset,
      raw.byteOffset + raw.byteLength,
    ) as ArrayBuffer;
  }
  {
    const raw = fs.readFileSync(helloPath);
    helloWasmBytes = raw.buffer.slice(
      raw.byteOffset,
      raw.byteOffset + raw.byteLength,
    ) as ArrayBuffer;
  }
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
