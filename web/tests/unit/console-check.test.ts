// Unit tests for `runEchoCheck`. Drives the function with a
// ConsoleHost whose Worker is a FakeWorker, so the full
// bootstrap → Worker → mock kernel → echo → bootstrap loop is
// exercised deterministically.

import { describe, expect, it } from "vitest";
import { ConsoleHost } from "../../src/console-host";
import type { WorkerLike } from "../../src/console-host";
import { runEchoCheck } from "../../src/console-check";
import type { EchoCheckOptions, TimerHandle } from "../../src/console-check";
import type { KernelToMain, MainToKernel } from "../../src/shared/worker-proto";

interface FakeWorker extends WorkerLike {
  readonly posted: MainToKernel[];
  emit(msg: KernelToMain): void;
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

interface FakeClock {
  now: number;
  tick(ms: number): void;
}

interface FakeScheduler {
  readonly pending: Array<{
    handler: () => void;
    ms: number;
    cancelled: boolean;
  }>;
  /** Fire every pending timer whose deadline is <= `atMs`. */
  fireDue(atMs: number): void;
}

/**
 * Build `{ now, setTimer, cancelTimer }` for EchoCheckOptions
 * with a controllable clock.
 */
function makeClockAndScheduler(): EchoCheckOptions["now"] extends infer _N
  ? {
      now: () => number;
      setTimer: (h: () => void, ms: number) => TimerHandle;
      cancelTimer: (h: TimerHandle) => void;
      clock: FakeClock;
      scheduler: FakeScheduler;
    }
  : never {
  const clock: FakeClock = {
    now: 0,
    tick(ms: number): void {
      clock.now += ms;
    },
  };
  const pending: FakeScheduler["pending"] = [];
  const scheduler: FakeScheduler = {
    pending,
    fireDue(atMs: number): void {
      for (const entry of pending) {
        if (!entry.cancelled && entry.ms <= atMs) {
          entry.cancelled = true;
          entry.handler();
        }
      }
    },
  };
  return {
    now: () => clock.now,
    setTimer: (handler, ms) => {
      const entry = { handler, ms: clock.now + ms, cancelled: false };
      pending.push(entry);
      return entry;
    },
    cancelTimer: (handle) => {
      const entry = handle as { cancelled: boolean };
      entry.cancelled = true;
    },
    clock,
    scheduler,
  };
}

function bootHost(fake: FakeWorker): ConsoleHost {
  const host = new ConsoleHost({
    worker: fake,
    bootConfig: { enableConsole: true, enableInput: false, enableFramebuffer: false },
  });
  fake.emit({ kind: "ready" });
  return host;
}

describe("runEchoCheck", () => {
  it("resolves ok when the expected bytes arrive", async () => {
    const w = makeFakeWorker();
    const host = bootHost(w);
    const { now, setTimer, cancelTimer, clock } = makeClockAndScheduler();

    const check = runEchoCheck(host, {
      input: "echo hello\n",
      expect: "hello\n",
      timeoutMs: 5000,
      now,
      setTimer,
      cancelTimer,
    });

    // Simulate a 42ms kernel delay.
    clock.tick(42);
    w.emit({ kind: "console:write", bytes: new TextEncoder().encode("hello\n") });

    const result = await check;
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.roundtripMs).toBe(42);
    }
  });

  it("sends the input line after subscribing so the output is observed", async () => {
    const w = makeFakeWorker();
    const host = bootHost(w);
    const { now, setTimer, cancelTimer } = makeClockAndScheduler();

    runEchoCheck(host, {
      input: "echo hi\n",
      expect: "hi\n",
      timeoutMs: 1000,
      now,
      setTimer,
      cancelTimer,
    });

    // The first message after boot is the console:input from the check.
    const ci = w.posted.filter((m) => m.kind === "console:input");
    expect(ci).toHaveLength(1);
    expect(ci[0]?.kind).toBe("console:input");
    if (ci[0]?.kind === "console:input") {
      expect(new TextDecoder().decode(ci[0].bytes)).toBe("echo hi\n");
    }
  });

  it("resolves mismatch when the bytes are the right length but wrong content", async () => {
    const w = makeFakeWorker();
    const host = bootHost(w);
    const { now, setTimer, cancelTimer } = makeClockAndScheduler();

    const check = runEchoCheck(host, {
      input: "echo hi\n",
      expect: "hi\n",
      timeoutMs: 1000,
      now,
      setTimer,
      cancelTimer,
    });
    w.emit({ kind: "console:write", bytes: new TextEncoder().encode("??\n") });

    const result = await check;
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.reason).toBe("mismatch");
      if (result.reason === "mismatch") {
        expect(result.got).toBe("??\n");
      }
    }
  });

  it("resolves timeout when no output arrives within timeoutMs", async () => {
    const w = makeFakeWorker();
    const host = bootHost(w);
    const { now, setTimer, cancelTimer, clock, scheduler } =
      makeClockAndScheduler();

    const check = runEchoCheck(host, {
      input: "echo slow\n",
      expect: "slow\n",
      timeoutMs: 500,
      now,
      setTimer,
      cancelTimer,
    });
    // Advance clock past the deadline and fire due timers.
    clock.tick(600);
    scheduler.fireDue(clock.now);

    const result = await check;
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.reason).toBe("timeout");
    }
  });

  it("resolves panic when a lifecycle panic event is observed", async () => {
    const w = makeFakeWorker();
    const host = bootHost(w);
    const { now, setTimer, cancelTimer } = makeClockAndScheduler();

    const check = runEchoCheck(host, {
      input: "echo anything\n",
      expect: "anything\n",
      timeoutMs: 5000,
      now,
      setTimer,
      cancelTimer,
    });
    w.emit({ kind: "panic", message: "simulated panic" });

    const result = await check;
    expect(result.ok).toBe(false);
    if (!result.ok && result.reason === "panic") {
      expect(result.message).toBe("simulated panic");
    }
  });

  it("accumulates multi-chunk output before resolving", async () => {
    // The kernel driver may, in principle, split a line across
    // multiple WRITE_LINE calls (it won't in v1, but the check
    // is defensive against future changes). Verify the
    // accumulator handles it.
    const w = makeFakeWorker();
    const host = bootHost(w);
    const { now, setTimer, cancelTimer } = makeClockAndScheduler();

    const check = runEchoCheck(host, {
      input: "echo split\n",
      expect: "split\n",
      timeoutMs: 1000,
      now,
      setTimer,
      cancelTimer,
    });
    w.emit({ kind: "console:write", bytes: new TextEncoder().encode("sp") });
    w.emit({ kind: "console:write", bytes: new TextEncoder().encode("lit\n") });

    const result = await check;
    expect(result.ok).toBe(true);
  });

  it("only settles once even if more events arrive after the first success", async () => {
    const w = makeFakeWorker();
    const host = bootHost(w);
    const { now, setTimer, cancelTimer, clock, scheduler } =
      makeClockAndScheduler();

    const check = runEchoCheck(host, {
      input: "echo go\n",
      expect: "go\n",
      timeoutMs: 500,
      now,
      setTimer,
      cancelTimer,
    });
    w.emit({ kind: "console:write", bytes: new TextEncoder().encode("go\n") });
    // Subsequent events must not double-resolve; the Promise
    // would throw in dev mode if it did.
    w.emit({ kind: "console:write", bytes: new TextEncoder().encode("garbage") });
    w.emit({ kind: "panic", message: "late panic" });
    clock.tick(600);
    scheduler.fireDue(clock.now);

    const result = await check;
    expect(result.ok).toBe(true);
  });

  it("cancels the timeout when the check settles early", async () => {
    const w = makeFakeWorker();
    const host = bootHost(w);
    const { now, setTimer, cancelTimer, scheduler } = makeClockAndScheduler();

    const check = runEchoCheck(host, {
      input: "echo cancel\n",
      expect: "cancel\n",
      timeoutMs: 1000,
      now,
      setTimer,
      cancelTimer,
    });
    w.emit({ kind: "console:write", bytes: new TextEncoder().encode("cancel\n") });
    await check;

    // The pending timeout handle was cancelled, so scheduler.fireDue won't invoke it.
    const timerEntry = scheduler.pending[0];
    expect(timerEntry?.cancelled).toBe(true);
  });
});
