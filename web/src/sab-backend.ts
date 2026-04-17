// `SabBackend` — the per-pid [`KernelBackend`] implementation that
// translates a [`SyscallRequest`] into an SAB ring round-trip with
// the kernel.
//
// This is the user-side counterpart to
// [`KernelWasmHost.serviceSab`]. The two together replace today's
// in-process [`KernelWasmHostBackend`] with a wire-format-compatible
// path that crosses thread boundaries: a future user Worker (T232+)
// constructs a `SabBackend` over the SAB it shares with the kernel
// Worker, the kernel Worker's poll loop (T233+) calls
// `serviceSab` for each pid in its pidMap. Today both ends still
// run on the vitest main thread; the ring orchestration is the only
// production-shaped piece — `Atomics.wait` + the cross-thread wake
// protocol land in T233.
//
// ## Wake protocol
//
// In production, `dispatch` will set `user_wait_slot = REQUESTED`,
// bump the shared kernel-wake slot + `Atomics.notify`, then park on
// `Atomics.wait(user_wait_slot, REQUESTED)`. The kernel Worker
// wakes, services the request via `serviceSab`, sets `user_wait_slot
// = READY`, and `Atomics.notify`s. The user wakes and reads the
// response.
//
// For T231 the wake slots stay untouched. The constructor accepts
// an optional `serviceHook` callback that runs synchronously between
// the request push and the response pop — the test harness uses
// this to call `host.serviceSab(pid, sab)` in the same tick. In
// T233 the hook is replaced by the real Atomics.wait round-trip and
// the slot writes that frame it.
//
// ## Memory layout assumptions
//
// The constructor takes a `Uint8Array` view of the per-pid SAB. The
// view's `byteOffset` may be non-zero (the caller can hand in a
// subview), but its `byteLength` must cover the full `SAB_SIZE`
// region. Header atomics go through an `Int32Array` over the same
// backing; ring slots and heap bytes go through `Uint8Array` views
// constructed per access (cheap; the underlying buffer is the same).
//
// ## Why this is not in `kernel-wasm-host.ts`
//
// `KernelWasmHost` lives on the kernel side of the seam. Even though
// the T231 test harness instantiates both halves in the same vitest
// process, the production split is: SabBackend is bundled into
// `user-worker.js`, KernelWasmHost into `kernel-worker.js`. Keeping
// them in separate modules now means the bundle split lands
// mechanically when T232 stands up the user-Worker entry.

import {
  HEAP_SCRATCH_BYTES,
  OFF_HEAP_SCRATCH,
  OFF_REQ_HEAD,
  OFF_REQ_RING,
  OFF_REQ_TAIL,
  OFF_RES_HEAD,
  OFF_RES_RING,
  OFF_RES_TAIL,
  OFF_USER_WAIT_SLOT,
  REQ_SLOT_COUNT,
  RES_SLOT_COUNT,
  SAB_SIZE,
  STATUS_REQUESTED,
} from "./shared/sab-layout";
import {
  decodeResponse,
  encodeRequest,
  SLOT_SIZE,
  type SyscallRequest,
} from "./shared/syscall";
import type { DispatchResult } from "./kernel-wasm-host";
import type { KernelBackend } from "./user-wasm-runtime";

/** Constructor options for [`SabBackend`]. */
export interface SabBackendOptions {
  /**
   * Per-pid SAB view. `byteLength` must be at least [`SAB_SIZE`];
   * a smaller view throws at construction. The caller retains
   * ownership of the underlying buffer.
   */
  readonly sab: Uint8Array;
  /** Pid this backend dispatches on behalf of. Frozen for the
   * lifetime of the backend. */
  readonly pid: number;
  /**
   * Synchronous stand-in for the production wake protocol. Runs
   * between the request push and the response pop; the test harness
   * uses it to call [`KernelWasmHost.serviceSab`] in the same tick
   * so the response is in the ring by the time `dispatch` reads it.
   *
   * Wins over [`kernelWakeSlot`] when both are set so existing
   * vitest harnesses keep their synchronous semantics. T235 deletes
   * the option once the legacy in-process composition tests have
   * migrated; until then the two coexist.
   */
  readonly serviceHook?: () => void;
  /**
   * Production wake-slot view: a shared `Int32Array` over the
   * kernel's 32-byte wake buffer. When set AND [`serviceHook`] is
   * unset, `dispatch` runs the production wake protocol described
   * in [`multi-process-plan.md §3`](../../specs/001-browser-os-v1/multi-process-plan.md):
   *
   *   1. `Atomics.store(header, OFF_USER_WAIT_SLOT/4, STATUS_REQUESTED)`
   *      — pre-stage the wait sentinel BEFORE the push so a kernel
   *      that pops + services + writes `STATUS_READY` before we even
   *      reach `Atomics.wait` makes the wait return "not-equal"
   *      immediately (race-free).
   *   2. Push request into the SAB request ring (existing path).
   *   3. `Atomics.add(kernelWakeSlot, 0, 1) +
   *      Atomics.notify(kernelWakeSlot, 0)` — wake the kernel's
   *      `Atomics.waitAsync` parker.
   *   4. `Atomics.wait(header, OFF_USER_WAIT_SLOT/4, STATUS_REQUESTED)`
   *      — block the user Worker thread until the kernel writes
   *      `STATUS_READY` + notifies (or returns immediately if the
   *      kernel already wrote `STATUS_READY` in step 3 above).
   *   5. Pop the response from the SAB response ring (existing path).
   *
   * `Atomics.notify` and the synchronous `Atomics.wait` only work
   * on `SharedArrayBuffer`-backed views; when the SAB is a plain
   * `ArrayBuffer` (vitest in environments without cross-origin
   * isolation) the notify silently no-ops and the wait is skipped
   * — the response ring will still have the kernel's reply when the
   * caller drove the kernel synchronously through some other channel
   * (e.g. the legacy `serviceHook` path), or `dispatch` will throw
   * with the existing "response ring empty" message.
   */
  readonly kernelWakeSlot?: Int32Array;
}

export class SabBackend implements KernelBackend {
  private readonly buffer: ArrayBufferLike;
  private readonly baseOffset: number;
  /** `Int32Array` over the SAB header so `Atomics.{load,store}` work
   * on the head/tail slots without per-call view construction. */
  private readonly header: Int32Array;
  private readonly pid: number;
  private readonly serviceHook: (() => void) | undefined;
  private readonly kernelWakeSlot: Int32Array | undefined;
  /** True iff `header.buffer` is a real `SharedArrayBuffer` (the only
   * backing on which `Atomics.notify` + `Atomics.wait` are legal).
   * Captured at construction so the per-call `dispatch` doesn't have
   * to re-check on every syscall. */
  private readonly headerIsShared: boolean;
  /** True iff `kernelWakeSlot.buffer` is a real `SharedArrayBuffer`.
   * Same rationale as `headerIsShared`; the two backings can in
   * principle differ — the kernel-wake buffer is allocated by
   * `KernelWasmHost.create` and the per-pid SAB by the main-thread
   * router, so a partial fallback is possible. */
  private readonly wakeSlotIsShared: boolean;

  constructor(options: SabBackendOptions) {
    if (options.sab.byteLength < SAB_SIZE) {
      throw new Error(
        `SabBackend: sab is ${options.sab.byteLength} bytes, need at least ${SAB_SIZE}`,
      );
    }
    this.buffer = options.sab.buffer;
    this.baseOffset = options.sab.byteOffset;
    this.header = new Int32Array(
      options.sab.buffer,
      options.sab.byteOffset,
      OFF_HEAP_SCRATCH / 4,
    );
    this.pid = options.pid;
    this.serviceHook = options.serviceHook;
    this.kernelWakeSlot = options.kernelWakeSlot;
    this.headerIsShared =
      typeof SharedArrayBuffer !== "undefined" &&
      this.buffer instanceof SharedArrayBuffer;
    this.wakeSlotIsShared =
      this.kernelWakeSlot !== undefined &&
      typeof SharedArrayBuffer !== "undefined" &&
      this.kernelWakeSlot.buffer instanceof SharedArrayBuffer;
  }

  /**
   * Translate `request` (+ optional `heapIn`) into an SAB ring
   * round-trip. Mirrors [`KernelWasmHostBackend.dispatch`]
   * byte-for-byte: same encoded request, same heap-scratch use of
   * `request.heapPtr` as the in/out offset, same decoded response.
   *
   * Throws if the request ring is full (production v1 keeps this at
   * a defensive check — single-threaded user wasm has at most one
   * in-flight syscall), if the response ring is empty after the
   * `serviceHook` runs (the hook should have serviced before
   * returning), or if a heap payload exceeds [`HEAP_SCRATCH_BYTES`].
   */
  dispatch(request: SyscallRequest, heapIn?: Uint8Array): DispatchResult {
    const heapPtr = request.heapPtr ?? 0;

    // Stage the heap payload into the SAB heap-scratch region at
    // the user-chosen offset. `KernelWasmHost.serviceSab` reads
    // from the same offset, so input + output share one buffer
    // window per syscall (matching how the in-process backend uses
    // the kernel's own heap scratch).
    if (heapIn !== undefined && heapIn.length > 0) {
      if (
        heapPtr > HEAP_SCRATCH_BYTES ||
        heapPtr + heapIn.length > HEAP_SCRATCH_BYTES
      ) {
        throw new Error(
          `SabBackend.dispatch: heap payload ${heapPtr}+${heapIn.length} > capacity ${HEAP_SCRATCH_BYTES}`,
        );
      }
      new Uint8Array(
        this.buffer,
        this.baseOffset + OFF_HEAP_SCRATCH + heapPtr,
        heapIn.length,
      ).set(heapIn);
    }

    // T234 production wake protocol step 1: pre-stage `STATUS_REQUESTED`
    // BEFORE the push so a kernel that pops + services + writes
    // `STATUS_READY` before we reach `Atomics.wait` makes the wait
    // return "not-equal" immediately (the standard double-check pattern
    // applied to the wait sentinel itself). Skipped on the
    // `serviceHook` path because that path is purely synchronous and
    // never blocks on the wait slot.
    if (this.serviceHook === undefined && this.kernelWakeSlot !== undefined) {
      Atomics.store(this.header, OFF_USER_WAIT_SLOT / 4, STATUS_REQUESTED);
    }

    // Producer-push. Mirrors `ring::Sab::try_push_request` in
    // `crates/ring/src/lib.rs`: load HEAD (this side is the only
    // writer for this pid; Atomics.load is SeqCst in JS — strictly
    // stronger than the Rust-side `Relaxed`), load TAIL (which the
    // kernel may have advanced concurrently), bail if full, write
    // the slot, advance HEAD with Release semantics. Atomics.store
    // is the JS spelling of Release.
    const reqHead = Atomics.load(this.header, OFF_REQ_HEAD / 4);
    const reqTail = Atomics.load(this.header, OFF_REQ_TAIL / 4);
    const nextReqHead = ((reqHead + 1) >>> 0) % REQ_SLOT_COUNT;
    if (nextReqHead === reqTail) {
      throw new Error(
        `SabBackend.dispatch: request ring full for pid ${this.pid}`,
      );
    }
    const reqSlotIx = (reqHead >>> 0) % REQ_SLOT_COUNT;
    const reqSlotOffset =
      this.baseOffset + OFF_REQ_RING + reqSlotIx * SLOT_SIZE;
    const reqBytes = encodeRequest(request);
    new Uint8Array(this.buffer, reqSlotOffset, SLOT_SIZE).set(reqBytes);
    Atomics.store(this.header, OFF_REQ_HEAD / 4, nextReqHead);

    if (this.serviceHook !== undefined) {
      // T231/T232 stand-in: the harness services the request inline so
      // the response is in the ring by the time we read it below. Stays
      // alive for legacy composition tests until T235 retires them.
      this.serviceHook();
    } else if (this.kernelWakeSlot !== undefined) {
      // T234 production wake protocol steps 3-4. Step 1 (the
      // `STATUS_REQUESTED` pre-stage) ran above, step 2 was the push.
      //
      // Bump the kernel-wake counter monotonically. The kernel's
      // parker (`KernelWasmHost.defaultPark`) compares against the
      // value it loaded before parking; any change wakes it.
      Atomics.add(this.kernelWakeSlot, 0, 1);
      if (this.wakeSlotIsShared) {
        // `Atomics.notify` only accepts views over a real
        // `SharedArrayBuffer`. On a plain `ArrayBuffer` (vitest in a
        // non-cross-origin-isolated env) the notify is unreachable
        // anyway because the kernel can't be parked across threads.
        Atomics.notify(this.kernelWakeSlot, 0);
      }
      if (this.headerIsShared) {
        // `Atomics.wait` is sync — legal in dedicated Worker scope (the
        // user-worker bundle's runtime). Returns when the kernel writes
        // any value other than `STATUS_REQUESTED` to the wait slot;
        // returns "not-equal" immediately if the kernel already wrote
        // such a value before we reached this call.
        Atomics.wait(this.header, OFF_USER_WAIT_SLOT / 4, STATUS_REQUESTED);
      }
      // No wait possible on a plain ArrayBuffer header — fall through
      // to the response pop, which will throw "ring empty" if the
      // caller forgot to pre-service.
    }
    // No serviceHook, no kernelWakeSlot: the caller is driving both
    // ends of the ring synchronously (e.g. byte-equivalence tests
    // against `KernelWasmHostBackend`). The response pop below will
    // throw "ring empty" if it isn't there.

    // Consumer-pop. Mirrors `ring::Sab::try_pop_response`. The user
    // is the only consumer for this pid's response ring, so an
    // empty ring at this point means the kernel never published —
    // in T231 that means the `serviceHook` neglected to service
    // (a test harness bug); in T233+ it means the wait spuriously
    // woke without a kernel-side notify (impossible per the wake
    // protocol, so still a bug).
    const resHead = Atomics.load(this.header, OFF_RES_HEAD / 4);
    const resTail = Atomics.load(this.header, OFF_RES_TAIL / 4);
    if (resHead === resTail) {
      throw new Error(
        `SabBackend.dispatch: response ring empty for pid ${this.pid} after serviceHook; production path would have parked on Atomics.wait until READY`,
      );
    }
    const resSlotIx = (resTail >>> 0) % RES_SLOT_COUNT;
    const resSlotOffset =
      this.baseOffset + OFF_RES_RING + resSlotIx * SLOT_SIZE;
    const resBytes = new Uint8Array(
      new Uint8Array(this.buffer, resSlotOffset, SLOT_SIZE),
    );
    const response = decodeResponse(resBytes);
    const nextResTail = ((resTail + 1) >>> 0) % RES_SLOT_COUNT;
    Atomics.store(this.header, OFF_RES_TAIL / 4, nextResTail);

    // Read heap output (if any) from the same SAB heap-scratch
    // window the input was staged in. `serviceSab` writes here so
    // the user reads from the same offset it wrote its input —
    // matches `KernelWasmHost.dispatch`'s in-process behaviour.
    let heapOut = new Uint8Array(0);
    if (response.extraLen > 0) {
      const heapOffset = this.baseOffset + OFF_HEAP_SCRATCH + heapPtr;
      heapOut = new Uint8Array(
        new Uint8Array(this.buffer, heapOffset, response.extraLen),
      );
    }

    return { response, heapOut };
  }
}
