// Canonical driver "common" module.
//
// T079 in `specs/001-browser-os-v1/tasks.md` asked for a single
// `web/src/drivers/common.ts` that holds:
//
//   * the `Driver` interface,
//   * the `DevId` enum,
//   * `ToDriver` / `FromDriver` message envelope shapes,
//   * ring-buffer attachment helpers,
//   * test-harness support (a mock ring factory).
//
// The initial landing split the surface across
// `./types.ts` (interface + result shapes) and
// `../shared/platform-constants.ts` (`DriverId` enum, which mirrors
// `kernel::platform::DevId`). Both of those files are still the
// source of truth — this module re-exports their public names under
// the identifiers the task spec called out, so downstream code can
// depend on `common` without caring about where the underlying
// types happen to live.
//
// Why not collapse into one file? `types.ts` + `platform-constants.ts`
// already ship and are imported by `console.ts`, `fb.ts`, and
// `input.ts`. Rewriting them would ripple through the driver layer
// with no behaviour change. The re-export pattern lets us:
//
//   * publish the spec-mandated API surface today, and
//   * migrate driver imports to `common.ts` in a follow-up slice,
//     without a big-bang rename.
//
// The `attachDriverRing` helper is intentionally deferred: every
// driver today goes through the `DriverHost` abstraction (see
// `./types.ts`), so there is no duplicated SAB-attach code to lift.
// When a driver eventually needs its own SAB ring (e.g. the block
// driver's OPFS Worker in a later slice), that is the right moment
// to introduce the helper. Adding it speculatively now would be
// dead code, which the kernel's no-UI-shortcut discipline rejects.

export type {
  Driver,
  DriverHost,
  DriverOk,
  DriverErr,
  DriverResult,
  DriverOp,
  Devnum,
} from "./types";

export { DriverErrorCode } from "./types";

// The spec's `DevId` name is spelled `DriverId` in the landed
// constants file (the same object mirrors `kernel::platform::DevId`
// on the Rust side — see `platform-constants.ts` header). We
// re-export it under both names so callers can use whichever spelling
// matches the abstraction level they are working at.
export { DriverId, DriverId as DevId } from "../shared/platform-constants";
export type {
  DriverIdValue,
  DriverIdValue as DevIdValue,
  DevnumValue,
} from "../shared/platform-constants";

import type { DriverOp } from "./types";

// --- Message envelopes -----------------------------------------------
//
// Every driver message — whether kernel→driver or driver→kernel —
// carries (at minimum) an opcode and a payload. A `requestId` is
// optional: simple write-only drivers (fb0) do not need it, but
// request/response drivers (block, net) do.
//
// These envelopes are GENERIC on the opcode and payload types so
// drivers can refine them. A block driver might declare
// `type BlockRead = ToDriver<typeof OP_READ, { lba: number; count: number }>`
// and the type system will flow the literal opcode through.
//
// Why not force every driver onto these envelopes today? The live
// drivers (console, fb, input) already use typed per-driver message
// shapes with their own `kind` discriminators, because they post
// messages through `DriverHost` — these envelopes are the
// contract for a future ring-backed driver path, not a retrofit of
// the existing message shapes.

/** Kernel→driver message envelope. */
export interface ToDriver<Op extends DriverOp = DriverOp, Payload = Uint8Array> {
  readonly opcode: Op;
  readonly payload: Payload;
  readonly requestId?: number;
}

/** Driver→kernel message envelope. */
export interface FromDriver<Op extends DriverOp = DriverOp, Payload = Uint8Array> {
  readonly opcode: Op;
  readonly payload: Payload;
  readonly requestId?: number;
}

// --- Test harness ----------------------------------------------------
//
// `createMockRing()` returns an in-memory ring that implements the
// same push/pop surface as the eventual SAB-backed driver ring.
// Tests use it to simulate kernel↔driver traffic without touching
// `SharedArrayBuffer` or `Atomics`. The ring is FIFO, unbounded,
// and single-threaded — it is NOT a fidelity model of the real SAB
// ring's backpressure; it just lets a test script a sequence of
// envelopes and read them back in order.

/** Public surface of the mock ring. */
export interface MockRing<T> {
  /** Append one message to the tail. */
  push(msg: T): void;
  /** Remove and return the oldest message, or `undefined` if empty. */
  pop(): T | undefined;
  /** Current queue length. */
  readonly length: number;
  /** Peek at the oldest message without removing it. */
  peek(): T | undefined;
  /** Drop all queued messages. */
  clear(): void;
}

/**
 * A pair of mock rings — the usual arrangement for test-harness
 * kernel↔driver simulation. `toDriver` is the ring the fake
 * kernel pushes into; `fromDriver` is the ring the fake driver
 * pushes into.
 */
export interface MockRingPair<
  ToOp extends DriverOp = DriverOp,
  ToPayload = Uint8Array,
  FromOp extends DriverOp = DriverOp,
  FromPayload = Uint8Array,
> {
  readonly toDriver: MockRing<ToDriver<ToOp, ToPayload>>;
  readonly fromDriver: MockRing<FromDriver<FromOp, FromPayload>>;
}

/** Create a single mock ring. */
export function createMockRing<T>(): MockRing<T> {
  const q: T[] = [];
  return {
    push(msg: T): void {
      q.push(msg);
    },
    pop(): T | undefined {
      return q.shift();
    },
    peek(): T | undefined {
      return q[0];
    },
    clear(): void {
      q.length = 0;
    },
    get length(): number {
      return q.length;
    },
  };
}

/** Create a paired mock ring for kernel↔driver round-trip tests. */
export function createMockRingPair<
  ToOp extends DriverOp = DriverOp,
  ToPayload = Uint8Array,
  FromOp extends DriverOp = DriverOp,
  FromPayload = Uint8Array,
>(): MockRingPair<ToOp, ToPayload, FromOp, FromPayload> {
  return {
    toDriver: createMockRing<ToDriver<ToOp, ToPayload>>(),
    fromDriver: createMockRing<FromDriver<FromOp, FromPayload>>(),
  };
}
