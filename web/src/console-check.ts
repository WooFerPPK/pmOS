// A one-shot "ping the kernel through the console driver"
// health check used by bootstrap.ts to turn the "kernel
// worker" row of the boot screen green.
//
// Separated from bootstrap.ts so the messaging logic can be
// unit-tested against a fake Worker without a browser. The
// bootstrap is the only production caller, and all it does is
// `new Worker(...)` + `new ConsoleHost(...)` + `runEchoCheck`.

import type { ConsoleHost } from "./console-host";

/** Result of a `runEchoCheck` call. */
export type EchoCheckResult =
  /** The kernel echoed the expected bytes back on stdout. */
  | { readonly ok: true; readonly roundtripMs: number }
  /** The kernel responded with different bytes. */
  | { readonly ok: false; readonly reason: "mismatch"; readonly got: string }
  /** No response arrived within the timeout. */
  | { readonly ok: false; readonly reason: "timeout" }
  /** The kernel panicked during the check. */
  | { readonly ok: false; readonly reason: "panic"; readonly message: string };

export interface EchoCheckOptions {
  /** Input the check sends to the kernel (must include trailing `\n`). */
  readonly input: string;
  /** Expected output bytes (must include trailing `\n`). */
  readonly expect: string;
  /** Timeout in milliseconds. The check resolves with `timeout` past this. */
  readonly timeoutMs: number;
  /**
   * Clock. Pass `Date.now` in production; tests pass a fake
   * so they can measure deterministic roundtripMs values.
   */
  readonly now: () => number;
  /**
   * One-shot timer scheduler. Pass `setTimeout` in production;
   * tests pass a fake that fires synchronously.
   *
   * The scheduler must return an opaque handle that
   * `cancel(handle)` unwinds — a real `setTimeout` returns a
   * handle compatible with `clearTimeout`.
   */
  readonly setTimer: (handler: () => void, ms: number) => TimerHandle;
  readonly cancelTimer: (handle: TimerHandle) => void;
}

export type TimerHandle = unknown;

/**
 * Run a single echo round-trip against `host`.
 *
 * The promise resolves when either the expected bytes arrive,
 * a kernel panic is observed, or the timeout fires — whichever
 * happens first. Callers should update their UI from the
 * returned result without caring how the check terminated.
 */
export function runEchoCheck(
  host: ConsoleHost,
  options: EchoCheckOptions,
): Promise<EchoCheckResult> {
  return new Promise<EchoCheckResult>((resolve) => {
    const started = options.now();
    let accumulator = "";
    let timer: TimerHandle | null = null;
    let settled = false;

    const settle = (result: EchoCheckResult): void => {
      if (settled) {
        return;
      }
      settled = true;
      if (timer !== null) {
        options.cancelTimer(timer);
        timer = null;
      }
      resolve(result);
    };

    host.onOutput((bytes) => {
      if (settled) {
        return;
      }
      accumulator += new TextDecoder().decode(bytes);
      if (accumulator.length < options.expect.length) {
        return;
      }
      if (accumulator === options.expect) {
        settle({ ok: true, roundtripMs: options.now() - started });
      } else {
        settle({ ok: false, reason: "mismatch", got: accumulator });
      }
    });

    host.onLifecycle((event) => {
      if (settled) {
        return;
      }
      if (event.kind === "panic") {
        settle({ ok: false, reason: "panic", message: event.message });
      }
    });

    host.sendLine(options.input);

    timer = options.setTimer(() => {
      settle({ ok: false, reason: "timeout" });
    }, options.timeoutMs);
  });
}
