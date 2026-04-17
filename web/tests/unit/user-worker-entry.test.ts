// Unit tests for user-worker-entry.ts — the dedicated user-Worker
// entry bundle that waits for a `boot` message, runs the user wasm
// against a `SabBackend`, and posts an `exited` reply on completion
// or trap.
//
// Driven through a `FakeWorkerMessaging` (matching the pattern in
// `kernel-worker-entry.test.ts`) so the entry's full message loop is
// exercised without spawning a real Worker. The wasm execution path
// is real: hello-std.wasm is loaded from disk, a real
// `KernelWasmHost` services the SAB ring synchronously via the
// entry's `serviceHook` test affordance, and the resulting console
// output is captured through the host's `onConsoleWrite` hook — the
// same byte-for-byte path T231 pinned for `SabBackend` directly.

import fs from "node:fs";
import path from "node:path";
import { beforeAll, describe, expect, it } from "vitest";

import { installUserWorkerEntry } from "../../src/user-worker-entry";
import type { UserWorkerMessaging } from "../../src/user-worker-entry";
import { KernelWasmHost } from "../../src/kernel-wasm-host";
import { SAB_SIZE } from "../../src/shared/sab-layout";
import type { MainToUser, UserToMain } from "../../src/shared/worker-proto";

let kernelWasmBytes: ArrayBuffer;
let helloStdWasmBytes: ArrayBuffer;

beforeAll(() => {
  const kPath = path.resolve(
    __dirname,
    "../../../target/wasm32-unknown-unknown/release/kernel.wasm",
  );
  if (!fs.existsSync(kPath)) {
    throw new Error(`kernel.wasm not found at ${kPath}. Run \`just build\` first.`);
  }
  const k = fs.readFileSync(kPath);
  kernelWasmBytes = k.buffer.slice(
    k.byteOffset,
    k.byteOffset + k.byteLength,
  ) as ArrayBuffer;

  const hPath = path.resolve(
    __dirname,
    "../../../target/wasm32-wasip1/release/hello-std.wasm",
  );
  if (!fs.existsSync(hPath)) {
    throw new Error(`hello-std.wasm not found at ${hPath}. Run \`just build\` first.`);
  }
  const h = fs.readFileSync(hPath);
  helloStdWasmBytes = h.buffer.slice(
    h.byteOffset,
    h.byteOffset + h.byteLength,
  ) as ArrayBuffer;
});

interface FakeMessaging extends UserWorkerMessaging {
  readonly posted: UserToMain[];
  send(msg: MainToUser): void;
}

function makeMessaging(): FakeMessaging {
  const posted: UserToMain[] = [];
  const fake: FakeMessaging = {
    onmessage: null,
    posted,
    postMessage(msg: UserToMain): void {
      posted.push(msg);
    },
    send(msg: MainToUser): void {
      fake.onmessage?.({ data: msg });
    },
  };
  return fake;
}

describe("installUserWorkerEntry", () => {
  it("installs an onmessage handler before any messages arrive", () => {
    const msg = makeMessaging();
    installUserWorkerEntry(msg);
    expect(msg.onmessage).not.toBeNull();
  });

  it("runs the boot wasm against a SAB-backed kernel and posts exited(code=0)", async () => {
    // End-to-end: load real kernel.wasm + real hello-std.wasm, allocate
    // an SAB, install the user-worker entry with a `serviceHook` that
    // synchronously runs `host.serviceSab(pid, sab)` between every
    // request push and response pop, send the boot message, await the
    // entry's exit reply.  Asserts the same observable T231 pins for
    // SabBackend directly: the user's console writes reach
    // `onConsoleWrite` byte-for-byte.
    const consoleWrites: Uint8Array[] = [];
    const host = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (bytes) => {
        consoleWrites.push(bytes);
      },
      nowNs: () => 0n,
    });
    const pid = host.registerProcess(0xffff_ffff_ffff_ffffn);
    host.installConsoleFd(pid, 0);
    host.installConsoleFd(pid, 1);
    host.installConsoleFd(pid, 2);
    host.markRunning(pid);

    const sab = new ArrayBuffer(SAB_SIZE);
    const sabView = new Uint8Array(sab);
    const msg = makeMessaging();
    const entry = installUserWorkerEntry(msg, {
      serviceHook: () => {
        host.serviceSab(pid, sabView);
      },
    });
    msg.send({
      kind: "boot",
      pid,
      sab,
      wasmBytes: helloStdWasmBytes,
    });
    await entry.whenExited;

    expect(msg.posted).toHaveLength(1);
    const exited = msg.posted[0];
    expect(exited?.kind).toBe("exited");
    if (exited?.kind === "exited") {
      expect(exited.pid).toBe(pid);
      expect(exited.code).toBe(0);
      expect(exited.trap).toBeUndefined();
    }

    const concatenated = consoleWrites
      .map((b) => new TextDecoder().decode(b))
      .join("");
    expect(concatenated).toBe("hello from std\n");
  });

  it("non-boot first message posts exited(code=-1) with a trap describing the wrong kind", async () => {
    const msg = makeMessaging();
    const entry = installUserWorkerEntry(msg);
    // The user worker's only legal first message is `boot`. The fake
    // messaging type only allows MainToUser, so simulate a legitimately-
    // unexpected message kind via a cast.
    msg.send({ kind: "shutdown" } as unknown as MainToUser);
    await entry.whenExited;
    expect(msg.posted).toHaveLength(1);
    const e = msg.posted[0];
    expect(e?.kind).toBe("exited");
    if (e?.kind === "exited") {
      expect(e.code).toBe(-1);
      expect(e.trap).toMatch(/before boot/);
    }
  });

  it("forwards boot.kernelWakeSlot from the boot message into the user-side SabBackend (T234)", async () => {
    // Drive the entry with NO `serviceHook` AND a `kernelWakeSlot`
    // in the boot message. SabBackend's production wake protocol is
    // active: every dispatch (a) pre-stages STATUS_REQUESTED in the
    // SAB's user_wait_slot before push, and (b) bumps `kernelWakeSlot
    // [0]` via `Atomics.add` after push. On a plain ArrayBuffer SAB +
    // plain ArrayBuffer wake slot (vitest under node, no COI), the
    // gated `Atomics.notify` and `Atomics.wait` are skipped; the
    // dispatch then falls through to the response-pop, finds the ring
    // empty, and throws — `UserWasmRuntime` catches the throw and
    // posts `exited(code: -1, trap: ...)`. The wake slot was
    // incremented BEFORE the throw — its non-zero value proves
    // `boot.kernelWakeSlot` made it into the SabBackend ctor and
    // `dispatch`'s wake-protocol writes ran.
    const host = await KernelWasmHost.create(kernelWasmBytes, {
      onConsoleWrite: (): void => {},
      nowNs: () => 0n,
    });
    const pid = host.registerProcess(0xffff_ffff_ffff_ffffn);
    host.installConsoleFd(pid, 0);
    host.installConsoleFd(pid, 1);
    host.installConsoleFd(pid, 2);
    host.markRunning(pid);

    const sab = new ArrayBuffer(SAB_SIZE);
    const wakeSlotBuf = new ArrayBuffer(32);
    const wakeSlot = new Int32Array(wakeSlotBuf, 0, 8);

    const msg = makeMessaging();
    const entry = installUserWorkerEntry(msg);
    msg.send({
      kind: "boot",
      pid,
      sab,
      wasmBytes: helloStdWasmBytes,
      kernelWakeSlot: wakeSlotBuf,
    });
    await entry.whenExited;

    // The runtime crashed on its first syscall (no kernel servicing
    // available without a serviceHook + no real Atomics.wait on a
    // plain ArrayBuffer). The crash is the expected outcome here —
    // we only care that the wake-slot path was exercised.
    expect(msg.posted).toHaveLength(1);
    const exited = msg.posted[0];
    expect(exited?.kind).toBe("exited");
    if (exited?.kind === "exited") {
      expect(exited.pid).toBe(pid);
      expect(exited.code).toBe(-1);
      expect(exited.trap).toBeDefined();
    }
    // The key assertion: SabBackend bumped the wake slot at least once
    // before the dispatch crashed — proving the `kernelWakeSlot` from
    // the boot message reached the SabBackend ctor + the production
    // wake protocol ran.
    expect(Atomics.load(wakeSlot, 0)).toBeGreaterThan(0);
  });
});
