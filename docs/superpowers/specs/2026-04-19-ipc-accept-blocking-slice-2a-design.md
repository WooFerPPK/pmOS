# `ipc_accept` blocking semantics (kernel park/wake) — slice 2a design

Date: 2026-04-19
Status: approved
Related: `specs/001-browser-os-v1/plan.md`, `specs/001-browser-os-v1/contracts/syscalls.md`, `specs/001-browser-os-v1/multi-process-plan.md` §5 ("park-and-wake on fd readiness is a follow-up"), `specs/001-browser-os-v1/data-model.md` (`BlockedOnIpc` transition, already spec-blessed but unused), `docs/superpowers/specs/2026-04-19-display-server-multi-client-accept-loop-design.md` (slice 1 of the arc — outer accept loop; landed at commit b1449da)
Slice shape: single session, single commit. First of two sub-slices (2a + 2b) that together resolve the "long-running display-server accept loop" runway item's blocking-syscall leg.

## 1. Problem

`crates/kernel/src/syscall/ext.rs:273 handle_ipc_accept` today returns `-EAGAIN` (via `KernelError::WouldBlock`) whenever the listener backlog is empty. Every caller — display-server in particular — has to busy-poll with a bounded inner loop (`MAX_POLLS = 10_000`) to emulate blocking. This burns CPU in the caller's Worker even when the caller has no work, and forces the `MAX_CLIENTS = 4` outer ceiling in display-server because the sequential vitest `runAllSpawns` harness can't distinguish "temporary empty backlog" from "no more clients will ever arrive".

Additionally, `specs/001-browser-os-v1/contracts/syscalls.md:203-210` documents `ipc_accept(fd, flags: u16) -> fd` — the ABI spec has always carried a `flags` parameter, but the kernel handler ignores it. The spec's semantics line (*"Blocks (or returns EAGAIN under NONBLOCK)…"*) describes `flags=0` as the blocking default, which is also the POSIX `accept(2)` default. Today's non-blocking-by-default behaviour is an existing spec-vs-impl drift.

`specs/001-browser-os-v1/multi-process-plan.md:48-51` explicitly defers blocking-syscall work to after M1 (the Worker-per-pid + SAB substrate): *"Real park-and-wake on fd readiness is a follow-up (see §5)."* M1.1–M1.6 landed in the recent batch; this slice is that follow-up, scoped to `ipc_accept` only.

The arc as a whole (display-server multi-client accept loop) was laid out in the slice-1 design spec's §6. This slice (**2a**) handles kernel park/wake mechanics + the `flags` parameter — a kernel-only change. The follow-up slice (**2b**, same arc) migrates display-server, removes the busy-poll ceiling, and adds signal-interrupt semantics.

## 2. Non-goals

Explicitly out of scope for 2a — deferred to later slices:

- display-server userland migration (drop `MAX_POLLS`, drop `MAX_CLIENTS`) — slice 2b.
- Signal-interrupt-wakes-blocked-accept-with-`EINTR` — slice 2b.
- Other blockable syscalls (`proc_wait`, `fd_read` on pipes, `ipc_connect` waiting for server accept) — future arcs; outside this arc entirely.
- Multi-parker per listener (POSIX allows many concurrent `accept()` calls on the same listener fd). v1 enforces at most one parker per listener — a second blocking accept while one is parked returns `-EAGAIN` even on `flags=0`. Documented as a deferred invariant; display-server only needs one acceptor at a time.
- `SharedArrayBuffer`-backed push from Rust to the main thread's listener. We already bump the response wake slot via `Sab::try_push_response`; the double-check-park-pattern mitigation landed in M1.

## 3. Design

### 3.1. ABI wire format for `IPC_ACCEPT` (opcode 0x1004)

**Before 2a:** `args[0..4] = listener_fd: u32`; bytes `[4..16]` unused.

**After 2a:** `args[0..4] = listener_fd: u32`; `args[4..6] = flags: u16`; bytes `[6..16]` unused.

Key property: the TS shim at `web/src/user-wasm-runtime.ts:2435` constructs the request with `arg0: listenerFd` only and leaves the rest of the 16-byte inline args window zero-initialized (the `encodeRequest` producer zero-fills). Old shims that don't pass `flags` therefore get `flags = 0` — the blocking default. **No user-wasm rebuild is required for 2a.** display-server's existing call resolves to "blocking with flags=0" and its `MAX_POLLS` loop succeeds on the first iteration's single blocking call.

New ABI constant in `crates/abi/src/ext.rs`:

```rust
pub mod accept_flags {
    /// Return -EAGAIN on empty backlog instead of parking the caller.
    /// Mirrors POSIX SOCK_NONBLOCK semantics for accept4(2).
    pub const NONBLOCK: u16 = 0x0001;
}
```

(Same bit as the existing top-level `ext::NONBLOCK = 0x0001`, but namespaced for clarity at call sites.)

Contracts update: `specs/001-browser-os-v1/contracts/syscalls.md:203-210` gains a semantics paragraph:

> `flags = 0` blocks the caller until a client `ipc_connect`s to the listener or the process is signalled (see slice 2b). `flags & NONBLOCK` preserves v1's non-blocking-by-default historical behaviour (returns `-EAGAIN` on empty backlog). Other bits are reserved and MUST be zero.

### 3.2. Kernel state additions

**`crates/kernel/src/ipc/socket.rs:58`** (`struct Socket`) gains one field:

```rust
/// If Some((pid, request_id)), a process is parked waiting for
/// this listener's backlog to grow. Cleared by:
///   * ipc_connect completing the parked accept,
///   * close_socket on the listener (drains with -EBADF),
///   * proc_exit on the parked pid (cleanup_proc scans and clears),
///   * (slice 2b) signal-driven interrupt with -EINTR.
///
/// v1 invariant: at most one parker per listener. A second blocking
/// accept while one is parked returns -EAGAIN regardless of flags.
pub parked_acceptor: Option<(Pid, u32)>,
```

`Socket::new` initialises it to `None`.

**`crates/kernel/src/sys.rs`** (`struct Kernel`) gains one field:

```rust
/// Responses queued for pids that were parked on a blocking syscall
/// and have since been unblocked (by a peer's action or a kernel
/// event). Drained per-pid by `kernel_drain_wakes_for_pid`, which
/// pushes each entry onto the target pid's SAB response ring.
pub(crate) pending_wakes: alloc::vec::Vec<(Pid, Response)>,
```

`Kernel::new` initialises it to `Vec::new()`.

### 3.3. Dispatcher return type

`crates/kernel/src/syscall/dispatch.rs` adds:

```rust
pub enum ServiceOutcome {
    /// Handler produced a response — push to caller's SAB and bump
    /// the response wake slot, as today.
    Done(Response),
    /// Handler parked the caller — skip the response push. The
    /// caller's user Worker stays on Atomics.wait until a future
    /// dispatch pass drains a wake for it.
    Parked,
}
```

`dispatch()` returns `ServiceOutcome` (was `Response`). `Dispatcher::service_one` interprets:

- `Done(resp)` → `sab.try_push_response(&resp)` + wake-slot bump (today's path).
- `Parked` → skip.

Every existing handler returns `ServiceOutcome::Done(resp)` unchanged — only `handle_ipc_accept` can produce `Parked`. This keeps the blast radius on one call site plus the `dispatch` / `service_one` signature change.

### 3.4. New kernel WASM export

```rust
#[no_mangle]
pub extern "C" fn kernel_drain_wakes_for_pid(
    pid: Pid,
    sab_ptr: *mut u8,
    sab_len: usize,
) -> i32
```

Body: constructs `Sab::from_raw(sab_ptr, sab_len)`, walks `kernel.pending_wakes` in order, for each `(p, resp)` where `p == pid` calls `sab.try_push_response(&resp)` (which handles the wake-slot bump), removes the entry. Returns the count pushed (non-negative) or a negative errno on SAB misconstruction.

The pid filter means the JS dispatch loop only needs the CURRENT pid's SAB — the kernel doesn't have to maintain a pid→SAB map.

### 3.5. Handler semantics

**`handle_ipc_accept` (flags + park path):**

```text
let listener_fd = args_u32(req, 0);
let flags       = args_u16(req, 4);                         // new helper + new read
let nonblock    = (flags & accept_flags::NONBLOCK) != 0;

match kernel.accept_socket(pid, listener_fd) {
    Ok(fd)                                    => Done(ok(req_id, fd as i64)),
    Err(WouldBlock) if nonblock               => Done(err(req_id, EAGAIN)),
    Err(WouldBlock) /* blocking */            => match kernel.park_on_accept(pid, listener_fd, req_id) {
        Ok(())                                => Parked,                     // transitions + block_reason set inside
        Err(WouldBlock)                       => Done(err(req_id, EAGAIN)),  // already-parked race (one-parker invariant)
        Err(e)                                => Done(err(req_id, kerr_to_errno(e))),
    },
    Err(e)                                    => Done(err(req_id, kerr_to_errno(e))),
}
```

`dispatch.rs` currently exposes `pub(super) fn args_u32(req, offset) -> u32` (line 104) and `pub(super) fn args_u64(req, offset) -> u64` (line 120). Slice 2a adds `pub(super) fn args_u16(req, offset) -> u16` next to them, with the same `debug_assert!(offset + 2 <= 16)` bounds guard. Trivially reusable by any future handler that reads a u16 out of the inline args window.

`Kernel::park_on_accept(pid, listener_fd, req_id)`:
1. Resolve `listener_fd` → `SocketId` via caller's fd table (same lookup `accept_socket` does).
2. Verify `Socket::state == Listening`.
3. If `parked_acceptor.is_some()`, return `WouldBlock` (one-parker invariant).
4. Set `parked_acceptor = Some((pid, req_id))`.
5. Transition pid `Running` → `BlockedOnIpc`; set `block_reason = Some(Ipc { endpoint_id: sock_id.0 })`.

**`handle_ipc_connect` (wake path):**

`Kernel::ipc_connect` already pushes the client's socket onto the listener's backlog. Post-step (new):

```text
after the existing push-to-backlog succeeds:
    if let Some((acceptor_pid, req_id)) = listener.parked_acceptor.take() {
        let accepted_fd = kernel.complete_parked_accept(acceptor_pid, listener_id)?;
        kernel.pending_wakes.push((acceptor_pid, Response::ok(req_id, accepted_fd as i64)));
        kernel.procs.transition(acceptor_pid, ProcState::Ready);
        kernel.procs.clear_block_reason(acceptor_pid);
    }
```

`complete_parked_accept(acceptor_pid, listener_id)` = `accept_socket` with the fd-allocation target set to `acceptor_pid`'s fd table (vs the caller's). Shared code path; factor out the common guts.

Error path: if `complete_parked_accept` fails (e.g. acceptor's fd table is `EMFILE`), push `Response::err(req_id, <errno>)` into `pending_wakes`. The acceptor observes the error rather than sitting parked forever.

### 3.6. JS dispatch loop integration

`web/src/kernel-wasm-host.ts`'s `startDispatchLoop` round-robins over `pidMap` calling `serviceSab(pid, sab)` per pid. One call is inserted:

```text
for each pid in pidMap:
    drainWakesForPid(pid, sab)        // NEW — calls kernel_drain_wakes_for_pid
    const busy = serviceSab(pid, sab)
    totalBusy = totalBusy || busy
```

`drainWakesForPid` is a thin TS wrapper over the new `kernel_drain_wakes_for_pid` export. Its return value (count pushed) is discarded outside tests.

Halt-predicate change: **none.** `pidMap` eviction is driven by `proc:exited`, which flows through Zombie not Blocked. A parked pid stays in `pidMap` so the loop picks it up next pass and attempts a drain. Safety is positional.

## 4. Testing

Three-layer coverage. Rust + TS-unit gain tests; Playwright stays green without changes.

### 4.1. Kernel isolation (Rust, `crates/kernel/tests/`)

Five new tests. Three in `sys.rs` (adjacent to the existing `multiple_clients_accept_into_distinct_server_side_fds` at line 1385), two in `syscall.rs`.

1. **`ipc_accept_flags_zero_parks_caller_on_empty_backlog`** — seed listener + empty backlog. Dispatch `IPC_ACCEPT(listener_fd, flags=0)` from pid A. Assert: outcome is `Parked`, `A.state == BlockedOnIpc`, `A.block_reason == Some(Ipc { endpoint_id: sock_id.0 })`, listener's `parked_acceptor == Some((A, req_id))`. Assert no response was pushed to A's SAB.
2. **`ipc_connect_wakes_parked_acceptor`** — continues from 1. Dispatch `IPC_CONNECT(/tmp/sock)` from pid B. Assert: outcome for B is `Done(ok)`; listener's `parked_acceptor` is `None`; A's state is `Ready`; `kernel.pending_wakes` contains exactly `[(A, Response::ok(req_id, new_fd))]`. Call `kernel_drain_wakes_for_pid(A, sab_A)` and assert A's SAB response ring contains the expected Response.
3. **`ipc_accept_flags_nonblock_preserves_eagain`** — empty backlog. Dispatch `IPC_ACCEPT(listener_fd, flags=NONBLOCK)` from A. Assert: `Done(Response::err(req_id, EAGAIN))`; A.state stays `Running`; `parked_acceptor` stays `None`.
4. **`second_accept_on_parked_listener_returns_eagain`** — two pids both dispatch `IPC_ACCEPT(flags=0)`. First parks. Second gets `Done(Response::err(req_id, EAGAIN))` — the one-parker-per-listener invariant.
5. **`park_on_accept_clears_on_listener_close`** — pid A parked. Listener's owner closes the listener fd. Assert: `pending_wakes` gains `(A, Response::err(req_id, EBADF))`; `A.state == Ready`; `parked_acceptor == None`.

**Rust test delta:** 1192 → 1197 (+5).

### 4.2. TS dispatcher (`web/tests/unit/kernel-wasm-host.test.ts`)

Two new tests in a new `describe("dispatch: IPC_ACCEPT blocking")` block, placed adjacent to the existing `dispatch: IPC_ACCEPT` coverage (established in the f295cbe batch):

6. **`IPC_ACCEPT flags=0 on empty backlog parks caller (no response push)`** — real `kernel.wasm` via `KernelWasmHost`. Register listener pid + acceptor pid, bind listener, dispatch `IPC_ACCEPT(listener_fd, flags=0)`. Assert: the response ring remains empty (`sab.try_pop_response() === null`) and `kernelDrainWakesForPid(acceptor_pid)` returns 0.
7. **`IPC_CONNECT from peer wakes parked acceptor; drain delivers response`** — continuation. Register a second pid, dispatch `IPC_CONNECT(/tmp/sock)` from it. Assert connect response is `{status: 0}`. Call `kernelDrainWakesForPid(acceptor_pid, sab_acceptor)`; assert return is 1. Pop the acceptor's SAB response ring; assert the response is `{status: 0, value: <fd>, request_id: <original>}`.

**TS-unit delta:** 493 → 495 (+2).

### 4.3. Playwright integration

No changes. The display-server's `MAX_POLLS` loop masks the new blocking behaviour at the integration layer — it still succeeds on iteration 0 (one blocking call instead of thousands of `-EAGAIN` polls). Playwright's existing assertions (`served client 0/1`, `sent pixels` × 2, `fb blit ok`, peak workers ≥ 5) stay green without modification. Slice 2b is the one that meaningfully changes Playwright.

**Playwright delta:** 2 → 2.

### 4.4. Test-count summary

| Layer | Before | After | Δ |
|---|---:|---:|---:|
| Rust | 1192 | 1197 | +5 |
| TS-unit | 493 | 495 | +2 |
| Playwright | 2 | 2 | 0 |
| **Workspace total** | **1687** | **1694** | **+7** |

## 5. Edge cases

- **Parked acceptor exits.** `Kernel::cleanup_proc` (called from `proc_exit`) scans `sockets` for entries with `parked_acceptor == Some((this_pid, _))` and clears them. Bounded by socket count (≤ dozens in v1).
- **Listener closes while acceptor is parked** (test 5). `close_socket` drains `parked_acceptor` by pushing `Response::err(req_id, EBADF)` into `pending_wakes` + transitioning the pid to Ready.
- **Signal arrives while parked.** Out of scope for 2a. A parked pid that receives SIGTERM stays parked (signal inbox fills but nothing checks it). 2b adds the wake-with-EINTR semantic. Safe for v1 because init doesn't kill display-server during normal lifecycle.
- **Kernel panic with queued wakes.** `pending_wakes` is discarded; parked user Workers stay parked forever. Same failure class as today's kernel panic; the panic overlay terminates every user Worker (`multi-process-plan.md:442-445`).
- **`pidMap` eviction of a parked pid.** Cannot happen — eviction is driven by `proc:exited`, which only fires after Zombie (not Blocked). Safety is positional.

## 6. Risks

- **Wake-slot lost-notification race.** Mitigated by the double-check park pattern (`multi-process-plan.md:551-554`): user Worker does `if (slot === expected) wait(slot, expected)`. M1 proved this pattern.
- **New kernel export ABI.** `kernel_drain_wakes_for_pid` extends the `kernel.wasm` export surface. `kernel-wasm-host.ts`'s `KernelInterface` type adds one method. Follows the `kernel_service_sab` precedent from M1.1.
- **Borrow-checker against `Socket.parked_acceptor` mutation in `ipc_connect`.** `handle_ipc_connect` needs `&mut Kernel` to push into backlog, clear `parked_acceptor`, mutate the acceptor's fd table, and append to `pending_wakes`. These touch three `BTreeMap`s on `Kernel` — doable with scoped mutable borrows (same pattern `ipc_connect` already uses). Low risk; surfaces at compile time if it bites.

## 7. Constitution check

| # | Principle | Status | Rationale |
|---|---|---|---|
| I | Real OS, not a simulation | PASS | Kernel gains actual blocking syscall semantics. No UI concepts enter the kernel. |
| II | Strict layering | PASS | Opcode + args window surface unchanged; userland call sites don't move. `parked_acceptor` is a kernel-internal field. |
| III | Browser-only, zero backend | PASS | No new network touchpoints. |
| IV | Offline-first | PASS | No SW / OPFS changes. |
| V | Process isolation | PASS — **strengthens** | Today's busy-poll burns caller-Worker CPU while "idle". Blocking on `Atomics.wait` means the parked Worker is genuinely idle until the kernel pushes a response. Wake crosses the SAB boundary — no shared-memory shortcut. |
| VI | Standard syscall surface | PASS | ABI expands by 2 bytes (the `flags: u16` at args[4..6]); closes an existing spec-vs-impl drift at `contracts/syscalls.md:203-210`. No new opcodes. |
| VII | Protocol over API | PASS | Display protocol bytes untouched. |
| VIII | Bottom-up | PASS | Kernel-layer-only slice; every upper layer unchanged. |
| IX | Performance budget | PASS — **improves** | Before: 10 000-iteration busy-poll per client arrival. After: 0 CPU in parked Worker; wake latency = round-robin period × N pids = microseconds at v1 scale. Input latency budget unaffected (input path doesn't traverse `ipc_accept`). |
| X | Testability at every layer | PASS | +5 kernel tests, +2 dispatcher tests, Playwright stays green. |

**Deviation introduced:** one-parker-per-listener (a second blocking accept returns `-EAGAIN`). Documented in-code as a future-slice-lifting invariant. POSIX allows many; display-server needs only one at a time.

**Deviation closed:** `contracts/syscalls.md:203-210`'s `flags: u16` parameter is now honoured. The spec semantics line (`flags=0` blocks, `NONBLOCK` returns `EAGAIN`) matches the implementation for the first time.

## 8. Slice 2b outline (forward context, not a commitment)

For the reader's orientation — out of this slice:

- Replace display-server's `MAX_POLLS` inner busy-poll with a single blocking `ipc_accept(listener, flags=0)` call.
- Drop the `MAX_CLIENTS = 4` outer ceiling (the loop becomes unbounded).
- Add signal-driven exit: display-server polls fd 3 (SignalChannel) between accepts; on SIGTERM, exits cleanly. The `ipc_accept` blocking call must itself be interruptible by SIGTERM — the kernel wakes the parked acceptor with `Response::err(req_id, EINTR)` + transitions to Ready + clears `parked_acceptor`. Tests: kernel-isolation test for SIGTERM-interrupts-parked-accept, updated Playwright for the `MAX_POLLS`-gone assertion.
- Init `proc_wait` supervision loop (may move to slice 2c / later in the arc).

Slice 2a is the precondition for all of the above.
