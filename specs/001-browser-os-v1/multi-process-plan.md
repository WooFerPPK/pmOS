# Multi-Process Substrate Plan — Milestone 1

**Parent plan**: [`plan.md`](./plan.md) (v1 feature plan)
**Scope**: the Worker-per-pid + per-pid SAB ring bridge seam that
replaces today's in-process `drainPendingSpawns` drain loop.
**Branch**: `001-browser-os-v1` | **Date**: 2026-04-17
**Status**: plan only; first sub-slice lands in the next session.

## Summary

Today every user wasm instance runs inside the kernel Worker scope.
`KernelWasmHost.drainPendingSpawns()` pops `{pid, path, bytes}` off a
queue and runs each child to completion via a `UserWasmRuntime` whose
`KernelBackend.dispatch()` synchronously calls
`KernelWasmHost.dispatch(pid, …)` — which mutates the kernel WASM's
`req_ptr` / `heap_ptr` and invokes the `kernel_dispatch(pid)` export.
All bytes are in one linear memory address space, all calls are on one
thread. This is the preview-era stand-in documented in
[tasks.md T091 note](./tasks.md) and in SESSION-NOTES.md's runway.

After this milestone, each user process is a dedicated Web Worker
instantiating one `wasm32-wasip1` module against a `SharedArrayBuffer`
ring shared with the kernel Worker. Syscalls cross a thread boundary:
the user Worker writes a `Request` into its pid's SAB ring, parks on
`Atomics.wait`, the kernel Worker pops the request, services it, writes
a `Response`, notifies the user. Process isolation becomes physical at
the WASM-linear-memory level (Principle V's promised property), not a
convention honoured by callers.

This is Milestone 1 of the "feels like an actual OS" runway. After it
lands, everything downstream — a long-running display server process,
the desktop shell, an interactive terminal that doesn't block while it
waits for input, any app with a non-trivial main loop — is unblocked.
Every v1 principle that Phase 2 promised at the architectural level gets
its empirical proof here.

## Non-goals (v1 multi-process)

- **Pre-emption**. Scheduling stays cooperative: a user Worker that
  loops forever in pure wasm without ever calling a syscall ties up its
  own Worker thread. The kernel Worker is unaffected, other user
  Workers are unaffected, and a future pre-emptive scheduler can steal
  cycles via `Worker.terminate` if we add it. v1 does not.
- **Shared memory between user processes**. Each pid has exactly one
  SAB, shared only with the kernel. `pmd_shm_pool` (display protocol)
  is the supported path for user↔display-server buffer sharing; it
  rides on top of this infrastructure, not around it.
- **Blocking `fd_read` that parks the caller without spinning**. The
  kernel's dispatcher today returns `EAGAIN` on an empty input ring.
  A user Worker that wants to block on input polls with backoff. Real
  park-and-wake on fd readiness is a follow-up (see §5).
- **Worker pool reuse**. Each pid gets a fresh Worker; `proc_exit`
  terminates the Worker. No warm-up pool in v1.
- **Nested Web Workers**. See §2 — the design uses main-thread-mediated
  spawn precisely so the kernel Worker never needs to call
  `new Worker(…)` itself.

## Inventory — what exists, what's new

### Exists, reused verbatim

- `crates/abi/src/ring.rs` — per-pid SAB layout (64 KiB: req ring,
  res ring, heap scratch, wait slots). `Request` and `Response`
  32-byte records with `to_le_bytes`/`from_le_bytes` both sides agree
  on. Not touched by this milestone.
- `crates/ring/src/lib.rs` — `Sab::{try_push_request, try_pop_request,
  try_push_response, try_pop_response}` and wait-slot accessors.
  Non-blocking by design; blocking wrapper is platform-specific and
  lives in the two callers (kernel Worker / user Worker). Already
  covered by `crates/ring/tests/ring.rs`.
- `web/src/shared/sab-layout.ts` — xtask-generated TS mirror of the
  ring constants. Already consistency-checked in vitest.
- `web/src/shared/syscall.ts` — opcode tables, `encodeRequest`,
  `decodeResponse`, `encodeSpawnManifest`, WASI + ext opcode maps.
  Already the single source of truth for the TS side of the ABI.
- `web/src/user-wasm-runtime.ts` — WASI + pmos_ext shim code that
  translates user-wasm calls into opcode dispatches against a
  `KernelBackend`. The `KernelBackend` interface (line 67) is the
  exact seam we swap: `KernelWasmHostBackend` becomes `SabBackend`
  and every call site moves unchanged.
- `crates/kernel/src/syscall/dispatch.rs` (`Dispatcher::service_one`)
  — already designed around "one Dispatcher, one pid, one ring".
  T073's landing note in tasks.md is explicit: "the future kernel-
  Worker loop will need one Dispatcher per process, not a fan-in".
  The fan-in loop is new work; the per-request servicing is not.
- Kernel exports in `crates/kernel/src/wasm_entry.rs`: `kernel_init`,
  `kernel_register_process`, `kernel_inject_console_input`,
  `kernel_inject_input_kbd`, `kernel_inject_input_mouse`,
  `req_ptr`, `resp_ptr`, `heap_ptr`. Reused as-is.
- Host imports the kernel takes: `pmos_host_driver_call`,
  `pmos_host_spawn_process`, `pmos_host_now_ns`, `pmos_host_random_
  bytes`, `pmos_host_halt`, `pmos_host_panic`. Unchanged.

### Changing

- `crates/kernel/src/wasm_entry.rs` — gains a `kernel_service_sab(pid,
  sab_ptr, sab_len, heap_ptr, heap_len) -> i32` export that pops one
  request out of the SAB at `sab_ptr` and services it against the
  kernel's internal state. Wraps `Dispatcher::service_one`. Returns
  `0` if a request was serviced, `1` if the ring was empty, negative
  errno on a malformed request. This is the bridge between the kernel
  Worker's JS poll loop and the Rust dispatcher.
- `web/src/kernel-wasm-host.ts` — `KernelWasmHost.dispatch` stays as
  the in-process path (tests + the sub-slice-1 migration rung still
  use it), but gains a sibling `serviceSab(pid, sabView, ...)`
  that wraps the new `kernel_service_sab` export. `drainPendingSpawns`
  is replaced by `startDispatchLoop(pids: () => Iterable<{pid,
  sab}>)` that runs until every live pid has exited.
- `web/src/kernel-worker-entry.ts` — `runBootBinary` collapses from
  "PROC_SPAWN + drainPendingSpawns" to "PROC_SPAWN + run dispatch
  loop until init reaps everyone". The default `onSpawnProcess`
  stops queuing in-process runs; it posts a `proc:spawn` message to
  main (see §2 for the exact shape) and returns the new pid
  synchronously to the caller.
- `web/src/bootstrap.ts` — main-thread-side Worker pool: receives
  `proc:spawn` messages from the kernel Worker, allocates a fresh
  SAB, spawns a new dedicated Worker from a new entry bundle
  `user-worker.js`, posts the SAB + wasm bytes + pid to the new
  Worker, tells the kernel Worker about the new (pid, SAB) pair so
  the kernel's poll loop picks it up.

### New code

- `web/src/user-worker-entry.ts` — the dedicated user-Worker entry
  bundle. Mirrors `kernel-worker-entry.ts` but for user wasm: waits
  for a `boot` message, instantiates the user wasm module from
  the bytes in the message, constructs a `SabBackend` over the
  SAB from the message, runs `UserWasmRuntime.start()`. On exit
  posts `{kind: "exited", pid, code}` and closes.
- `web/src/sab-backend.ts` — the new `KernelBackend` implementation
  that writes into the pid's SAB and blocks on `Atomics.wait`.
  Replaces `KernelWasmHostBackend` for the Worker path; the
  KernelWasmHostBackend stays for vitest unit tests (keeps the hot
  in-process dispatch path tested independently).
- `xtask` addition in `crates/xtask/src/assemble_dist.rs` — bundles
  `web/src/user-worker-entry.ts` into `dist/assets/user-worker.js`.
  Mirrors the existing `kernel-worker.js` bundling.
- `web/src/shared/worker-proto.ts` — adds message kinds for the
  main↔kernel-Worker and main↔user-Worker channels that the spawn
  seam needs. See §2 for the exact shapes.

## 1. Execution model: Worker lifecycle

**Decision**: main-thread-mediated spawn. The kernel Worker never
calls `new Worker(…)`. Instead, when `Kernel::proc_spawn` succeeds,
the WasmPlatform's `pmos_host_spawn_process` host call triggers
`onSpawnProcess`, which posts a `proc:spawn` message to the main
thread. The main thread creates the user Worker, allocates the SAB,
hands everything to the new Worker, and informs the kernel Worker of
the new (pid → SAB) pair via `proc:sab`.

**Alternatives considered**:

1. **Nested dedicated Workers**. Chromium has supported them since
   ~2015, Firefox since ~2016, Safari since 15 (2021). iOS Safari's
   track record through 16 was patchy. By 2026, nested Workers are
   broadly supported — but making the kernel Worker also be a Worker
   spawner means the kernel's global scope carries a `new Worker`
   capability that it does not need and that we would have to reason
   about in terms of capability leakage. More concretely: the main
   thread already IS the coordination hub for drivers, input, canvas
   blit, the panic overlay, and the service worker. It already owns
   every Worker that exists. Main-mediation keeps the hub-and-spoke
   topology; nested spawn would make the kernel a partial hub with
   duplicated lifecycle logic.

2. **MessageChannel + MessagePort transfer to user Workers from the
   kernel**. Cleaner than nested Workers but still needs the kernel
   to participate in Worker lifecycle. Same objection.

**Why main-mediated is right for PMos specifically**: `onSpawnProcess`
(the existing seam added in the Worker-spawn Platform-hook slice) is
already an asynchronous callback boundary — the kernel gets back a
`SpawnOutcome` synchronously and the actual Worker doesn't need to be
alive yet. The kernel polls the new pid's SAB after the handler
returns; if main hasn't finished setting up the Worker yet, the first
polls see empty rings and move on. No new synchronisation is needed.

**Worker death**: user Worker emits `exit` message (normal path) or
the main thread observes an `error` event (trap / script error). In
both cases main posts `proc:exited(pid, code)` to the kernel Worker,
which routes it to `Kernel::proc_exit(pid, ExitStatus)` and the
process-table reaper. Main then calls `worker.terminate()` and drops
the entry. The SAB itself is GC'd when all references die.

**Kernel Worker death**: the existing `panic` channel handles the
display side. New addition: before loading the overlay and reloading,
the bootstrap `worker.terminate()`s every live user Worker so we don't
ship a half-alive system back to the user. One-line addition.

## 2. SAB ring layout per-pid

**Layout**: unchanged from the v1 plan. 64 KiB per pid, split:

| Offset   | Size    | Purpose                                   |
| -------- | ------- | ----------------------------------------- |
| 0x0000   | 0x0040  | Header (4 ring head/tails, 4 atomic slots) |
| 0x0040   | 0x3FC0  | Request ring (510 slots × 32 bytes)       |
| 0x4000   | 0x3FC0  | Response ring (510 slots × 32 bytes)      |
| 0x8000   | 0x8000  | Heap scratch (32 KiB)                     |

Already constants in `abi::ring` and mirrored to `sab-layout.ts`.
510 slots means a process can have up to 509 syscalls in flight before
backpressure. In v1, syscalls are serialized within a user wasm
instance (one fiber of execution, no green threading), so queue depth
is 1 under normal load and 510 is 508-slot safety margin.

**Allocation**: one SAB per pid, allocated on the main thread at
`proc:spawn` receipt time. The `SharedArrayBuffer` is then posted to:

- the new user Worker as `{kind: "boot", pid, sab, wasmBytes}`,
- the kernel Worker as `{kind: "proc:sab", pid, sab}`.

After that both ends speak to it directly via `Int32Array` /
`Uint8Array` views and atomic operations.

**Kernel-side pid → SAB table**: a new TS-side Map<pid, SharedArray
Buffer> in `kernel-worker-entry.ts` maintained through `proc:sab` and
`proc:exited` messages. The dispatch loop iterates this map.

**Why one SAB per pid rather than a single multi-ring SAB**: (a) it's
what `contracts/driver-kernel.md §1` already specifies; (b) each pid's
SAB becomes GC'd when the pid exits, with no reclamation logic
needed; (c) per-pid isolation makes it a constitution-checked
statement rather than a best-effort one — no chance that a buggy user
Worker writes into another pid's ring region; (d) the `ring` crate's
existing non-blocking helpers already assume one ring per `Sab` value
and one `Sab` per process.

**Shared wake slot**: [`OFF_KERNEL_WAIT_SLOT`](../../crates/abi/src/ring.rs)
is comment-documented as "kernel's Atomics.wait slot (shared across
processes)" but each SAB has its own copy. We need exactly one wake
slot that every user Worker AND the main thread can bump to wake the
kernel Worker.

**Decision**: a separate dedicated 32-byte "kernel-wake SAB" held by
the kernel Worker, allocated at boot, shared with main and every user
Worker. Every notifier does `Atomics.add(wakeSlot, 0, 1); Atomics.
notify(wakeSlot, 0)`. The kernel Worker's main loop does
`Atomics.wait(wakeSlot, 0, last_value, timeout_ms)`. When it wakes,
it scans every live pid's request ring and main-thread message queue,
services a batch, then waits again.

**Timeout value**: 50 ms. Chosen so that even if a notify is lost
(which Atomics.notify under contention cannot technically lose, but
belt-and-suspenders), syscall latency stays within the Principle IX
input budget. Tunable; the perf harness (T220) will measure.

## 3. Syscall marshaling

**User-wasm-runtime's `KernelBackend` interface stays identical**:

```ts
interface KernelBackend {
  dispatch(request: SyscallRequest, heapIn?: Uint8Array): DispatchResult;
}
```

The user-wasm-runtime never learns it switched implementations. What
changes is which `KernelBackend` the Worker entry instantiates:

- **vitest unit path (today)**: `KernelWasmHostBackend` — calls
  `host.dispatch(pid, request, heapIn)` synchronously.
- **user Worker runtime path (new)**: `SabBackend` — writes the
  request into the SAB, copies `heapIn` into the heap scratch
  region, parks on `Atomics.wait(userWaitSlot, REQUESTED)`, wakes on
  `READY`, reads the response + heap scratch, returns.

**Wire format on the SAB** (unchanged from `abi::ring`):

- Request slot is 32 bytes. Opcode + flags + request_id + 16-byte
  inline args + heap_ptr + heap_len, all little-endian.
- Heap scratch payload at `heap_ptr`, length `heap_len`. The user
  Worker writes in-memory bytes at `heap_ptr = 0` (start of heap
  scratch) and sets `heap_len` to their length. The kernel writes
  responses at the same region; `extra_len` on the response carries
  the reply's length. No per-syscall allocation.
- Response slot is 32 bytes: request_id + status + value + extra_len.

**Wake protocol per syscall**:

1. User writes `Request` via `Sab::try_push_request`. If the ring
   is full (never in v1 practice), block until the kernel services
   one; drop-on-full is not allowed.
2. User sets `user_wait_slot = REQUESTED`.
3. User bumps the shared `kernel_wake_slot` + Atomics.notify.
4. User `Atomics.wait(user_wait_slot, REQUESTED)`.
5. Kernel Worker wakes on the shared wake slot, polls this pid's
   ring, pops request, services via `Dispatcher::service_one`,
   `Sab::try_push_response`, sets `user_wait_slot = READY`,
   `Atomics.notify(user_wait_slot)`.
6. User wakes, reads response + heap scratch, continues.

**Wasm's synchronous view is preserved**: `Atomics.wait` inside a
Worker blocks that Worker's thread. From the user wasm's point of
view, `fd_write` returns the number of bytes written — same contract
as POSIX, same contract as today. The JS-layer async is invisible to
the wasm program.

**Why this works in browsers**: `Atomics.wait` is explicitly legal in
Worker scope (it would throw on the main thread). COOP/COEP is already
required for `SharedArrayBuffer`. Both constraints are already
constitution-level prerequisites in the v1 plan.

## 4. Kernel-side dispatch loop

**Control flow inside the kernel Worker**:

```
kernel Worker boot:
  instantiate kernel.wasm
  kernel_init()
  pidMap = new Map<pid, SharedArrayBuffer>()
  wakeSlot = shared 32-byte SAB (provided in the boot message)
  forever:
    Atomics.wait(wakeSlot, 0, lastValue, 50)
    drain port messages (inject_input_kbd, inject_input_mouse, inject_console_input, proc:sab, proc:exited, halt)
    for pid in pidMap (round-robin):
      budget = 8
      while budget-- > 0:
        rc = kernel_service_sab(pid, sabPtr, sabLen, scratchPtr, scratchLen)
        if rc == 1: break (ring empty)
        if rc < 0: post panic (corrupt ring) and break
```

**Round-robin budget**: per-loop-iteration cap of 8 requests per pid
keeps one chatty process from starving the others. Tunable. The perf
harness (T220) validates the 100 ms input budget still holds.

**Why `kernel_service_sab` instead of JS-side ring ops**: the kernel
WASM already has the dispatcher + request decoder + kernel state; we
would otherwise duplicate the decoder in TS. Importantly, the kernel
WASM's linear memory is where request/response byte-wrangling happens,
not the SAB. The new export takes a SAB pointer + slot-read bytes,
copies them into the kernel's existing `req_ptr` / `heap_ptr`
scratchpads, calls `Dispatcher::service_one`, copies the response and
any `extra_len` heap bytes back to the SAB. One memcpy each way. The
kernel's existing dispatcher is unchanged.

**Port message drain**: the kernel Worker already handles main-thread
`injectInput` messages today. Multi-process adds `proc:sab` (register a
new SAB) and `proc:exited` (retire a pid) handlers. `halt` remains.

**Yielding between batches**: the `Atomics.wait` with 50 ms timeout is
the yield. When all rings are empty and no main-thread message is
pending, the Worker parks on the wake slot and uses zero CPU until
somebody notifies.

## 5. Driver routing across threads

**Kernel → main (output devices)**: unchanged. `onConsoleWrite`,
`onFramebufferWrite`, `onFramebufferMessage` all already post from
kernel Worker to main thread via `postMessage`. The main thread's
`ConsoleHost` / `FbHost` receive and handle. Works because these are
main-initiated connections (the fb/console drivers live on main).

**Main → kernel (input devices)**: unchanged. `injectInput(devnum,
bytes)` today goes main-thread → `KernelWorker.handleMainMessage`
→ `KernelWasmHost.injectInput` → kernel export. Multi-process adds
one bump of the kernel wake slot after `injectInput` so the kernel
loop wakes up and services the new input ring state. The user Worker
reading `/dev/input_kbd` via its SAB then sees bytes available on the
next `fd_read`.

**Main → kernel → user (input routing)**: a user Worker's `fd_read`
on `/dev/input_kbd` still returns `EAGAIN` when no bytes are queued,
because that is how the kernel's DeviceDispatcher works in v1. The
user Worker either spins with backoff (fine for short waits) or uses
a future blocking read syscall (not in this milestone). The v1 term
binary — which is the primary consumer of keyboard input — already
has the right shape: a main loop that polls stdin. Polling on a
dedicated Worker with yield-via-`setTimeout(0)` is acceptable.

**User → main (direct)**: not allowed. User wasm goes through the
kernel; there is no user-Worker-to-main postMessage channel. This is
Principle V: all IPC is kernel-mediated.

**Display server connections**: unchanged. A display client sends
`display_connect` → kernel returns an fd → client writes protocol
bytes → kernel routes through the IPC socket to the display server's
listener fd on `/run/display`. The fact that client and server now
run in different Workers changes nothing about the protocol — both
ends hit the kernel's IPC state machine, which already correctly
multiplexes clients.

## 6. Scheduler replacement

`drainPendingSpawns` goes away. In its place:

- **`onSpawnProcess` default callback** posts `proc:spawn` to the main
  thread and returns the new pid. No in-process wasm execution.
- **Main thread** receives `proc:spawn`, allocates SAB, creates user
  Worker, posts boot message to it. Posts `proc:sab` to kernel.
- **Kernel Worker** starts polling the new pid's SAB on its next wake.
- **Kernel Worker's main loop** runs until `shouldHalt` — triggered
  when all spawned pids have exited and init has exited. Then
  messages `ready` / reports final exit code to main.

**`runBootBinary` reshape**:

```
async runBootBinary(bootBinary):
  initPid = host.registerProcess(CAPSET_ALL)
  host.installConsoleFd(initPid, 0..2)
  host.markRunning(initPid)
  host.dispatch(initPid, PROC_SPAWN(bootBinary))  // in-process dispatch for the bootstrap pid
  await host.runDispatchLoop()                    // runs until every pid exited
```

The bootstrap pid (the synthetic parent of init) stays in-process
inside the kernel Worker because there's nothing for it to do — it
just dispatches one syscall and exits. Every later pid is a real user
Worker. This keeps the number of Workers in play to N+1 (kernel + N
user pids), where N = the deepest live process tree at any moment.

**Worker lifecycle summary**:

- Worker created at `proc:spawn` on main thread.
- SAB + wasm bytes + pid sent to Worker as `boot` message.
- Worker runs `UserWasmRuntime` against `SabBackend`.
- Worker terminates itself after `proc_exit` by posting `exited` and
  calling `close()`.
- On Worker `error` event, main treats it as a trap, posts
  `proc:exited(pid, -1)` to kernel, then `worker.terminate()`.

## 7. Error handling

**User wasm panic / trap**: JS side observes the thrown `RuntimeError`
in `UserWasmRuntime.start()`. The user Worker posts `{kind: "exited",
pid, code: -1, trap: errorMsg}` to main and closes. Main reports
`proc:exited` to the kernel Worker. Kernel reaps.

**User Worker script error** (broken bundle, etc.): main sees the
`error` event on the Worker object. Same treatment: `proc:exited(pid,
-1)`, terminate, reap.

**`proc_exit(code)` call**: `UserWasmRuntime` catches the
`UserProcessExited` unwind sentinel, posts `{kind: "exited", pid,
code}`, closes. Main reports. Kernel reaps.

**Kernel Worker panic**: existing path. New addition: `bootstrap.ts`'s
panic overlay handler terminates every user Worker before the
reload-timer fires, so the user does not see half-live windows. One
line: `for (const [pid, entry] of userWorkers) entry.worker.terminate()`.

**Malformed ring** (shouldn't happen; defensive): `kernel_service_sab`
returns a negative code. Kernel Worker JS catches and posts `panic`.
Treat as a kernel panic.

**Corrupt SAB header** (shouldn't happen; defensive): detected by
`Sab::try_pop_request`'s bounds checks, which today `assert!`. Assert
failure in the kernel Worker panics the Worker → existing panic path.

## 8. Testability

**Vitest unit coverage** (maximize here):

- **Ring round-trip tests**: already exist in `crates/ring/tests/ring.rs`.
  Extend to exercise the "user writes request → kernel pops → kernel
  pushes response → user pops" full cycle with realistic `Request`
  bytes through a shared `Vec<u8>` backing. No Atomics.wait needed;
  native tests busy-spin. Covers the wire format on both sides.
- **SabBackend vitest**: instantiate two `SharedArrayBuffer`s (or
  plain ArrayBuffer when SAB isn't available in node — the ring code
  is agnostic) + a mock kernel that pops from one and pushes into the
  other, assert the user-side `dispatch` return values match the in-
  process path byte-for-byte. This is the key regression guard on
  the backend swap.
- **`kernel_service_sab` export**: new vitest that loads `kernel.wasm`,
  registers a pid, seeds a SAB with a `FD_WRITE` to `/dev/console`,
  calls `kernel_service_sab`, asserts the response ring has the
  expected bytes. One test per opcode pair (FD_WRITE, PROC_SPAWN,
  PROC_EXIT, FD_READ with input pre-injected). Proves the export
  bridges the ring and the dispatcher correctly without any Workers.
- **Main-thread Worker pool (mocked Workers)**: FakeWorker class that
  captures `postMessage` and exposes a `receiveMessage` trigger, used
  to drive the `bootstrap.ts` spawn choreography without a real
  Worker. Asserts on message ordering: `proc:spawn` → allocate SAB
  → `boot` message to Worker → `proc:sab` to kernel.

**Playwright integration coverage** (only where vitest cannot reach):

- **Worker isolation**: spawn a binary that writes a canary to its own
  linear memory; spawn a second binary that tries to find that canary
  through every non-IPC channel. Second binary must fail every time.
  This is the empirical Principle V gate, deferred until US7's
  `process-isolation.spec.ts` (T175). Multi-process makes that test's
  assertions actually meaningful — today it would trivially fail
  because there's only one Worker.
- **Real-kernel input round-trip in Worker scope**: a follow-up to
  today's `real-kernel.spec.ts`. Spawn `hello-input-echo` via init.
  Type a key on the page. Assert the echo reaches `#pmos-real-
  console`. The new test proves the main-thread → kernel-Worker →
  user-Worker input path works end-to-end.
- **Parent waits for child**: init spawns two children (hello-std +
  hello-wasi-bootstrap) and waits for both. Playwright asserts all
  three exit and the output ordering is interleaved — proving they
  run concurrently in different Workers, not serially.

**What this milestone does NOT need to prove**:

- The full Principle II layering test. That's US8 (T181) — requires
  the real display server and shell, which are later milestones.
- The full p95 input-latency budget. That's T220 — requires 6 apps
  open, which is later milestones.
- Block-driver integration. That's T084 — separate milestone.

## 9. Constitution check

Every principle is evaluated against the multi-process substrate
specifically. The parent plan already passed all ten at the v1 scope;
this check is for the sub-scope.

| # | Principle | Status | Notes |
|---|-----------|--------|-------|
| I | Real OS, not a simulation | **PASS** | This milestone replaces the preview in-process drain with an actual scheduler. The substrate grows in the "real OS" direction, not away from it. No UI concepts enter the kernel. |
| II | Strict layering | **PASS** | The seam respects the existing layer catalogue: kernel Worker owns the dispatcher, user Worker owns wasm execution, main thread owns drivers and Worker lifecycle. No cross-layer shortcuts. The layering test (T181) remains the acceptance gate; this milestone is a prerequisite for making it meaningful. |
| III | Browser-only, zero backend | **PASS** | No new network touchpoints. The only new file shipped in `dist/` is `user-worker.js`, bundled from `web/src/user-worker-entry.ts`, precached by the service worker alongside `kernel-worker.js`. |
| IV | Offline-first and persistent | **PASS** | Extend the service worker precache list to include `user-worker.js`. One-line change in `xtask assemble-dist` and the sw.ts cache manifest. Persistent state (OPFS root fs) is untouched. |
| V | Process isolation is mandatory | **PASS — first real gain** | Today isolation is only in-principle; all user wasm shares one linear memory inside the kernel Worker. After this milestone, each pid is a dedicated Worker with a distinct linear memory. Isolation becomes physical, not conventional. This is the single biggest Principle V delta in the project's history. |
| VI | Standard syscall surface | **PASS** | No new opcodes added. The transport changes from an in-process function call to SAB+Atomics, but opcodes + record layouts + errno values are unchanged. `abi` crate untouched. |
| VII | Protocol over API for the display server | **PASS** | Untouched. Display protocol rides on top of IPC sockets, which ride on top of the syscall surface. The syscall transport change is invisible to the protocol. |
| VIII | Bottom-up construction | **PASS** | Sub-slices are ordered: SAB wire format (already exists, just exercised) → kernel-side sab-servicing export → user Worker scaffold → main-thread spawn choreography → reconcile boot path → delete preview drain. Each sub-slice has its test gate before the next begins. |
| IX | Performance budget | **PASS (monitored)** | Hot-path overhead is one main-thread-initiated `new Worker(…)` per `proc_spawn` (an expensive one-off on spawn) + per-syscall cost of one `Atomics.wait` round-trip. Chrome's `Atomics.wait` + `Atomics.notify` round-trip is ~1-5 μs under no contention. For the v1 input budget (100 ms p95), this is 5 orders of magnitude below the budget. `proc_spawn` Worker creation is ~5-20 ms, visible but only at spawn, not on the steady-state critical path. T220 measures. |
| X | Testability at every layer | **PASS** | Every sub-slice ends with a test gate (see §12). The kernel-side `kernel_service_sab` export is testable via vitest without any Workers. `SabBackend` is testable against a mock kernel. Worker lifecycle is testable via `FakeWorkerMessaging`-style mocks. Playwright handles only the end-to-end in-browser assertions that unit tests cannot reach. |

**Known deviations introduced**: none. The new files land in the
directories the parent plan predicted. `user-worker.js` is a sibling
of `kernel-worker.js` under `dist/assets/`.

## 10. Risks & open questions

- **`SharedArrayBuffer` availability in the CI browser**: Playwright
  launches Chromium with COOP/COEP-compliant headers today (because
  the dev server sets them). Verify early in sub-slice 0 that SAB is
  actually available in the Playwright runtime. Mitigation: xtask
  dev-server already sets the headers; vitest tests don't touch SAB
  directly.
- **`Atomics.wait` in vitest (node)**: vitest runs under node, which
  supports `SharedArrayBuffer` + `Atomics` natively but doesn't have
  Workers in the same form. Mitigation: unit tests don't block on
  `Atomics.wait`; they drive both sides of the ring synchronously
  with busy-spin (native tests today already do this).
- **Main-thread Worker creation cost**: ~5-20 ms per spawn. If a
  bootstrap sequence spawns >30 processes before interactive, we'd
  spend 150-600 ms in Worker creation alone. Mitigation: PMos v1 has
  ~7-11 steady-state processes per the plan; spawn count is low. If
  it ever becomes a bottleneck, a Worker pool with reset-on-checkout
  is a well-known fix.
- **Lost notifications**: `Atomics.notify` never drops notifications
  to parked waiters. The risk is the notifier bumping the slot
  BEFORE the waiter parks. Mitigation: the standard double-check
  pattern — waiter does `if (slot === expected) wait(slot, expected)`
  — which is what the stdlib wrapper does.
- **Debuggability**: 1 kernel Worker + N user Workers spread logs
  across N+1 Chrome DevTools contexts. Mitigation: every user
  Worker's console output is forwarded through `/dev/console` to the
  main thread (already the case), and the panic overlay aggregates.

## 11. Sub-slice breakdown

Six sub-slices, each sized for one session. Each ends with a `just
test`-green gate and a commit. Dependency order is strict; don't
parallelise.

### M1.1 — Kernel-side `kernel_service_sab` export + vitest round-trip

**What**: add a new kernel export `kernel_service_sab(sab_ptr, sab_len,
pid) -> i32` that wraps `Dispatcher::service_one` against a byte slice
view of the SAB. The user side stays synchronous (still using
`KernelWasmHostBackend`). The kernel side gains the path it will use
from the Worker loop, but no Worker exists yet.

**Files touched**:
- `crates/kernel/src/wasm_entry.rs` — new `#[no_mangle] extern "C"`
  export `kernel_service_sab(pid, sab_ptr, sab_len, scratch_ptr,
  scratch_len) -> i32`. Body: construct a `ring::Sab::from_raw` over
  the SAB view, call `Dispatcher::service_one`, return rc.
- `web/src/kernel-wasm-host.ts` — new `serviceSab(pid,
  sabView)` method that calls the new export.
- `web/tests/unit/kernel-wasm-host.test.ts` — +3 tests: (a) seed
  SAB with `FD_WRITE(/dev/console, "hi\n")`, call serviceSab, assert
  SAB response ring has the expected Response + `onConsoleWrite` fired;
  (b) empty ring returns `1`; (c) PROC_EXIT through SAB retires pid.

**Test gate**: vitest green. `just test-kernel` + `just test-drivers`
still green. Expected test count delta: +3 TS.

**No Workers yet**. Nothing user-visible changes. This is the seam
slice — it proves the bridge byte-for-byte.

### M1.2 — `SabBackend` + mock-kernel round-trip

**What**: add a `SabBackend: KernelBackend` implementation that writes
to an SAB + blocks on `Atomics.wait`, and a matching vitest that uses
a synchronous fake kernel in the same vitest process to service it.
No Workers yet; both ends run on the vitest main thread and busy-spin
instead of Atomics.wait.

**Files touched**:
- `web/src/sab-backend.ts` — new file. `class SabBackend implements
  KernelBackend` with `dispatch(request, heapIn)`. Uses
  `sab-layout.ts` constants + `Atomics.load/store/wait/notify`.
  Takes a `SharedArrayBuffer` + pid + a `wakeFn` callback (the
  callback rather than direct wake-slot access because vitest
  needs a synchronous hook to simulate the kernel servicing the
  request before the user side tries to block).
- `web/tests/unit/sab-backend.test.ts` — new file. Constructs two
  ArrayBuffers (SAB isn't available in node by default but the
  backend is layout-agnostic), runs `SabBackend.dispatch` against a
  mock kernel that pops from the request ring + pushes into the
  response ring synchronously, asserts the return value is the same
  as `KernelWasmHostBackend`'s return for the same call.

**Test gate**: vitest green. Expected test count delta: +5 TS (one
per opcode class: FD_WRITE, FD_READ with pre-seeded input, PATH_OPEN
error path, PROC_EXIT, heap overflow rejected).

**Still no Workers**. Proves the backend swap is byte-equivalent.

### M1.3 — `user-worker.js` entry bundle + FakeWorker drive

**What**: create the user Worker entry bundle and the main-thread side
of the spawn choreography, both driven by fake Worker mocks in vitest.
No real Workers yet; this slice proves the message plumbing.

**Files touched**:
- `web/src/user-worker-entry.ts` — new file. Waits for a `boot`
  message `{pid, sab, wasmBytes}`, loads + instantiates the user
  wasm module, constructs a `SabBackend` over the SAB, runs
  `UserWasmRuntime`, posts `{kind: "exited", pid, code}` on exit.
  Structure mirrors `kernel-worker-entry.ts`.
- `web/src/bootstrap.ts` — `onSpawnProcess` message handler on the
  main thread: allocate SAB, call `new Worker("/assets/user-
  worker.js", {type:"module"})`, post `boot` message to the user
  Worker, post `proc:sab` to the kernel Worker. Tracks live user
  Workers in a Map<pid, {worker, sab}>. Installs `exit` + `error`
  handlers that post `proc:exited` to the kernel Worker.
- `web/src/shared/worker-proto.ts` — add the new message kinds:
  `MainToKernel.proc_sab`, `MainToKernel.proc_exited`, `KernelToMain.
  proc_spawn`, and the `UserToMain` / `MainToUser` types.
- `crates/xtask/src/assemble_dist.rs` — also bundles
  `user-worker-entry.ts` into `dist/assets/user-worker.js`.
  Manifest entry count grows by 1.
- `web/tests/unit/user-worker-entry.test.ts` — new file. Uses a
  `FakeWorkerMessaging` to drive `user-worker-entry.ts` with a
  `boot` message carrying a real `hello-std.wasm` loaded from disk,
  backed by a mock kernel that services its syscalls through a
  `SabBackend`-compatible mock. Asserts the Worker emits `"hello
  from std\n"` through the SAB's FD_WRITE path and exits 0.
- `web/tests/unit/bootstrap-spawn.test.ts` — new file. Mocks `new
  Worker` via a FakeWorker class that captures posted messages.
  Drives the main-thread spawn path: trigger `proc:spawn` from a
  fake kernel Worker, assert main allocates a SAB, assert the fake
  user Worker receives a `boot` message with the right shape,
  assert main posts `proc:sab` to the kernel with the same SAB.

**Test gate**: vitest green. Expected test count delta: +4-6 TS.

**Still no real Workers**. Proves main-thread choreography + user-
entry bundle shape.

### M1.4 — Kernel Worker dispatch loop

**What**: wire `kernel-worker-entry.ts` to service N pids. Replace
the single-pid `drainPendingSpawns` call with a round-robin loop that
calls `serviceSab` across a pidMap maintained through `proc:sab` and
`proc:exited` main messages. The `onSpawnProcess` default callback
posts `proc:spawn` to main instead of queuing.

**Files touched**:
- `web/src/kernel-wasm-host.ts` — new method `startDispatchLoop(
  pidSource, halted): Promise<void>` that runs the round-robin +
  `Atomics.wait` on wake slot. Keeps `drainPendingSpawns` as a
  deprecated shim that delegates to it (for the transition), or
  deletes it outright if M1.5 clears the last caller — pick based
  on how many tests still reference it at sub-slice time.
- `web/src/kernel-worker-entry.ts` — `runBootBinary` collapses to
  `PROC_SPAWN init` + `startDispatchLoop`. Add `proc:sab` +
  `proc:exited` handlers on the kernel-worker messaging channel.
- `crates/kernel/src/wasm_entry.rs` — add a wake-slot host import
  if the kernel's Atomics.wait isn't done from JS (design decision:
  JS does the wait, WASM only services — prefer this). Likely no
  WASM-side change needed if the wait is entirely in JS.
- `web/tests/unit/kernel-worker-entry.test.ts` — drives the full
  spawn choreography end-to-end through fake Workers: kernel
  triggers `onSpawnProcess` → main spawns fake user Worker → main
  sends `proc:sab` to kernel → kernel services the user's first
  syscall (pre-seeded in the FakeUserWorker) → assertions on the
  exchanged bytes.
- `web/tests/unit/dispatch-loop.test.ts` — new file. Registers 3
  fake pids with pre-seeded request rings, calls
  `startDispatchLoop`, asserts round-robin + budget + termination
  on all-exited.

**Test gate**: vitest green. Rust tests unchanged. Expected test count
delta: +6-8 TS.

**Still no real Workers** in the test harness, but the production
kernel-worker-entry is now ready for them.

### M1.5 — Playwright browser integration

**What**: the first slice that spawns a real user Worker. Wire
`bootstrap.ts`'s real-kernel mode to use the new spawn path instead
of `drainPendingSpawns`. The existing Playwright test
(`real-kernel.spec.ts`) must keep passing — that's the gate.

**Files touched**:
- `web/src/bootstrap.ts` — swap the spawn path from the fake
  `onSpawnProcess` → drain combo to the real main-thread Worker
  spawner. Keep the existing `#pmos-real-console` DOM surface.
- Delete the `useRealKernel` preview-era short-circuits that went
  straight to `drainPendingSpawns`.
- `web/tests/integration/real-kernel.spec.ts` — assertions extended:
  assert `init` and `hello-std` run in different Workers (check via
  a test-only hook that exposes the pidMap size), plus the existing
  four-line ordered output assertions.

**Test gate**: `just build` green, all unit tests green, Playwright
green including the existing real-kernel spec. Expected TS-unit
delta: 0. Playwright delta: 0 new tests, strengthened assertions.

**Real Workers now run in the browser**. First empirical Principle V
gain.

### M1.6 — Cleanup + reconciliation

**What**: delete the in-process preview drain. Remove
`KernelWasmHost.drainPendingSpawns` from the production code path.
Move composition tests that relied on it (there are several in
`user-wasm-runtime.test.ts` and `kernel-wasm-host.test.ts`) onto
the new path — they either keep using the in-process
`KernelWasmHostBackend` directly with explicit calls (they don't
need the drain loop) or migrate to the fake-Worker harness.

**Files touched**:
- `web/src/kernel-wasm-host.ts` — `drainPendingSpawns` and
  `pendingSpawns` queue deleted. `SpawnHistoryEntry` stays (it's
  useful; just not auto-populated by a drain loop anymore).
- Existing vitest composition tests — rewrite onto `startDispatch
  Loop` with a fake pid source, or switch to direct
  `KernelWasmHostBackend.dispatch` calls for tests that don't need
  the loop semantics.
- Update `tasks.md` T091 deviation note: "Done. Worker-per-pid +
  SAB rings landed in M1.1–M1.6."
- Update `plan.md` "Known Deviations" block: remove the preview
  stand-in entry; acknowledge the new `user-worker.js` shipped
  asset.
- Update SESSION-NOTES.md runway: promote the remaining four
  runway items, remove the Multi-process one.

**Test gate**: every test green, no references to the deleted queue
in the codebase. Expected test count delta: net 0 (tests rewritten
in place).

**Production now has no legacy drain-loop code path**.

## 12. Task rows to add

Append to `tasks.md` Phase 2 "Bootstrap, kernel Worker, service
worker" block. Chosen numbering: T230–T235, contiguous, each pinned
to its sub-slice above. Existing T091 and T074 deviation notes get a
cross-reference added to the new block.

```
- [ ] T230 M1.1 Kernel-side `kernel_service_sab` export: add
  `#[no_mangle] extern "C" fn kernel_service_sab(pid, sab_ptr,
  sab_len, scratch_ptr, scratch_len) -> i32` in
  `crates/kernel/src/wasm_entry.rs` wrapping `Dispatcher::service_one`
  against a `ring::Sab::from_raw` view. Add `KernelWasmHost.serviceSab`
  in `web/src/kernel-wasm-host.ts`. +3 vitest tests at
  `web/tests/unit/kernel-wasm-host.test.ts`: FD_WRITE through the ring
  fires `onConsoleWrite`; empty ring returns 1; PROC_EXIT retires pid.
  No Workers yet.
- [ ] T231 M1.2 `SabBackend` implementing `KernelBackend` in
  `web/src/sab-backend.ts`. +5 vitest tests in
  `web/tests/unit/sab-backend.test.ts` asserting byte-for-byte
  equivalence with `KernelWasmHostBackend` across FD_WRITE, FD_READ
  with pre-seeded input, PATH_OPEN ENOENT, PROC_EXIT, and heap
  overflow rejection. Ring round-trip on vitest main thread via
  busy-spin instead of Atomics.wait. No Workers yet.
- [ ] T232 M1.3 `user-worker-entry.ts` + main-thread spawn
  choreography, both driven by FakeWorker mocks. New files:
  `web/src/user-worker-entry.ts`, `web/tests/unit/user-worker-
  entry.test.ts`, `web/tests/unit/bootstrap-spawn.test.ts`. Extends
  `web/src/shared/worker-proto.ts` with `proc:sab`, `proc:exited`,
  `proc:spawn` message kinds. Extends `crates/xtask/src/assemble_dist.rs`
  to bundle `user-worker.js` (manifest count +1). +4-6 vitest tests
  covering the spawn message plumbing. Still no real Workers.
- [ ] T233 M1.4 Kernel Worker dispatch loop: `KernelWasmHost.
  startDispatchLoop(pidSource, halted)` in `web/src/kernel-wasm-
  host.ts`. `kernel-worker-entry.ts`'s `runBootBinary` collapses to
  `PROC_SPAWN init + startDispatchLoop`. `proc:sab` + `proc:exited`
  handlers added. +6-8 vitest tests in
  `web/tests/unit/kernel-worker-entry.test.ts` (spawn
  choreography end-to-end) and `web/tests/unit/dispatch-loop.test.ts`
  (round-robin, budget, termination). Rust test count unchanged.
- [ ] T234 M1.5 Playwright browser integration: `bootstrap.ts`'s
  real-kernel mode swaps from the fake `onSpawnProcess`→drain combo
  to the real main-thread Worker spawner. Extends
  `web/tests/integration/real-kernel.spec.ts` assertions: `init` +
  `hello-std` run in different Workers, full four-line ordered output
  reaches `#pmos-real-console`. First slice with real user Workers
  in the browser. TS-unit delta 0; Playwright strengthens existing
  assertions (no new spec files).
- [ ] T235 M1.6 Reconcile + delete the preview drain:
  `KernelWasmHost.drainPendingSpawns` + `pendingSpawns` queue
  removed. Existing vitest composition tests in
  `user-wasm-runtime.test.ts` / `kernel-wasm-host.test.ts` migrated
  onto `startDispatchLoop` + fake pid sources, or onto direct
  `KernelWasmHostBackend.dispatch` calls for tests that don't need
  the loop semantics. T091 + T074 deviation notes updated in
  `tasks.md`; parent `plan.md` Known Deviations block refreshed;
  SESSION-NOTES runway updated. Net test count delta 0.
```

T091 cross-reference (amend in place):

> (existing T091 deviation block continues) … The residual T091 work
> — the round-robin service loop over multiple process SABs — is
> scheduled as T230–T235 under the "Multi-process substrate (M1)"
> block below.

T074 cross-reference (amend in place):

> (existing T074 deviation block continues) … The SAB-allocation arm
> deferred in the T074 landing note lands in T232 (main-thread
> allocator) + T233 (kernel registers the SAB via `proc:sab`).

## 13. Execution notes for the next session

- First session after this plan: T230. Scope is tight — one new
  export, one new TS method, three vitest tests. Goal is a green
  commit in under two hours.
- Don't skip the `just build` rebuild step when touching anything
  under `kernel-worker-entry.ts` or its imports. `dist/assets/kernel-
  worker.js` is tracked; a stale bundle ships old behaviour and
  Playwright will fail in a confusing way. (Gotcha carried forward
  from the real-kernel-mode slice in SESSION-NOTES.)
- Every sub-slice ends with `just test-kernel` + `(cd web && npx
  vitest run)` at minimum. T234 additionally runs `just test-integration`.
- Commit author identity stays `webos dev <dev@webos.local>`.
- Every commit message reports the test-count delta.
- This plan document is the single source of truth for the
  architecture. If a sub-slice uncovers a design need that contradicts
  this plan, update the plan first, then land the sub-slice.

## 13. Plan corrections (appended during execution)

Corrections to the plan text above as sub-slices land and design
claims are validated against reality. The plan body is left intact
for historical continuity; corrections are appended here.

### T230 (M1.1) — no kernel-side export

§2 "Changing" and §4 "Kernel-side dispatch loop" described a new
kernel export `kernel_service_sab(pid, sab_ptr, ...) -> i32` that
wraps `Dispatcher::service_one` against a `ring::Sab::from_raw`
view of the SAB. That design does not work: a WASM module's linear
memory is a distinct address space from a `SharedArrayBuffer`, so a
`*mut u8` pointing into the SAB is not a valid pointer in the
kernel's memory. The kernel cannot construct a `Sab` over SAB bytes
without a memcpy-each-way through its own scratch region, and once
the memcpy is on the JS side, there is no remaining work for the
kernel to do that it is not already doing inside `kernel_dispatch`.

**Landed instead**: a pure-TS `KernelWasmHost.serviceSab(pid, sab:
Uint8Array): 0 | 1` method that pops a request from the SAB, calls
the existing synchronous `dispatch`, and pushes the response back.
The `decodeRequest` + `encodeResponse` helpers were added to
`shared/syscall.ts` to round out the encode/decode pairs. Wake
slots are deliberately untouched — those are T233's concern.

**Impact on downstream sub-slices**: §4 "Kernel-side dispatch
loop" needs its pseudocode updated — the kernel Worker will call
`host.serviceSab(pid, sabView)` in its round-robin loop rather than
`kernel_service_sab(pid, sabPtr, ...)`. Semantics are identical.
Performance cost is a few JS-level `Atomics.load/store` calls plus
the existing `dispatch` path — lower than the original plan's
memcpy-each-way estimate because the two ring-ops and the decode
both happen in one JS function with no cross-language hop.

**Impact on Principle IX budget**: unchanged. The per-syscall
`Atomics.wait` round-trip is still the dominant cost; JS-side
ring orchestration is constant-time.

## Planning Complete

The seam is fully specified. Future sessions execute T230 → T231 →
T232 → T233 → T234 → T235 in order. `/speckit.implement` against
tasks.md T230 is the first follow-up.
