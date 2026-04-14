// Unit tests for the main-thread `FbHost` wrapper.
//
// Covers `fb:set-mode` and `fb:blit` dispatch, subscriber
// fan-out, and the "ignore unrelated messages" path that
// lets an FbHost coexist with a ConsoleHost on the same
// worker.

import { describe, expect, it } from "vitest";
import { FbHost } from "../../src/fb-host";
import type { FbFrame, FbMode } from "../../src/fb-host";
import type { WorkerLike } from "../../src/console-host";
import type { KernelToMain, MainToKernel } from "../../src/shared/worker-proto";

interface FakeWorker extends WorkerLike {
  /** Inject a message as if the Worker had sent it. */
  emit(msg: KernelToMain): void;
  readonly posted: MainToKernel[];
}

function makeFakeWorker(): FakeWorker {
  const posted: MainToKernel[] = [];
  let handler: ((ev: { data: KernelToMain }) => void) | null = null;
  return {
    posted,
    postMessage(msg: MainToKernel): void {
      posted.push(msg);
    },
    addEventListener(
      _type: "message",
      h: (ev: { data: KernelToMain }) => void,
    ): void {
      handler = h;
    },
    terminate(): void {
      /* no-op */
    },
    emit(msg: KernelToMain): void {
      handler?.({ data: msg });
    },
  };
}

describe("FbHost", () => {
  it("starts with no mode and zero blits", () => {
    const w = makeFakeWorker();
    const host = new FbHost({ worker: w });
    expect(host.mode).toBeNull();
    expect(host.blitsObserved).toBe(0);
  });

  it("fb:set-mode updates `mode` and fires every mode handler in order", () => {
    const w = makeFakeWorker();
    const host = new FbHost({ worker: w });
    const events: FbMode[] = [];
    host.onModeChange((m) => events.push(m));
    host.onModeChange((m) => events.push({ width: m.width + 1, height: m.height + 1 }));

    w.emit({ kind: "fb:set-mode", width: 640, height: 480 });
    expect(host.mode).toEqual({ width: 640, height: 480 });
    expect(events).toEqual([
      { width: 640, height: 480 },
      { width: 641, height: 481 },
    ]);
  });

  it("fb:blit increments blitsObserved and fires every frame handler", () => {
    const w = makeFakeWorker();
    const host = new FbHost({ worker: w });
    const frames: FbFrame[] = [];
    host.onFrame((f) => frames.push(f));

    const rgba = new Uint8Array([0xaa, 0xbb, 0xcc, 0xdd]);
    w.emit({ kind: "fb:blit", width: 1, height: 1, rgba });
    expect(host.blitsObserved).toBe(1);
    expect(frames).toHaveLength(1);
    expect(frames[0]?.width).toBe(1);
    expect(frames[0]?.height).toBe(1);
    expect(Array.from(frames[0]?.rgba ?? [])).toEqual([0xaa, 0xbb, 0xcc, 0xdd]);
  });

  it("multiple blits accumulate the blit counter", () => {
    const w = makeFakeWorker();
    const host = new FbHost({ worker: w });
    for (let i = 0; i < 5; i += 1) {
      w.emit({ kind: "fb:blit", width: 1, height: 1, rgba: new Uint8Array(4) });
    }
    expect(host.blitsObserved).toBe(5);
  });

  it("ignores console:write, ready, panic and any other message kinds", () => {
    const w = makeFakeWorker();
    const host = new FbHost({ worker: w });
    const frames: FbFrame[] = [];
    const modes: FbMode[] = [];
    host.onFrame((f) => frames.push(f));
    host.onModeChange((m) => modes.push(m));

    w.emit({ kind: "ready" });
    w.emit({
      kind: "console:write",
      bytes: new TextEncoder().encode("hello\n"),
    });
    w.emit({ kind: "panic", message: "nope" });

    expect(frames).toHaveLength(0);
    expect(modes).toHaveLength(0);
    expect(host.mode).toBeNull();
    expect(host.blitsObserved).toBe(0);
  });

  it("mode handler is NOT called for blits; frame handler is NOT called for mode changes", () => {
    const w = makeFakeWorker();
    const host = new FbHost({ worker: w });
    let frameHits = 0;
    let modeHits = 0;
    host.onFrame(() => (frameHits += 1));
    host.onModeChange(() => (modeHits += 1));

    w.emit({ kind: "fb:set-mode", width: 320, height: 240 });
    expect(frameHits).toBe(0);
    expect(modeHits).toBe(1);

    w.emit({
      kind: "fb:blit",
      width: 1,
      height: 1,
      rgba: new Uint8Array(4),
    });
    expect(frameHits).toBe(1);
    expect(modeHits).toBe(1);
  });

  it("posts nothing back on its own — FbHost is receive-only", () => {
    const w = makeFakeWorker();
    new FbHost({ worker: w });
    // Emit a blit event to make sure no side-effects post
    // anything.
    w.emit({
      kind: "fb:blit",
      width: 1,
      height: 1,
      rgba: new Uint8Array(4),
    });
    expect(w.posted).toHaveLength(0);
  });
});
