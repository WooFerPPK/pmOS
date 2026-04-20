# `proc_wait` blocking semantics (kernel park/wake) — slice 2c.1 design

Date: 2026-04-20
Status: approved
Related: `docs/superpowers/specs/2026-04-19-ipc-accept-blocking-slice-2a-design.md` (sibling kernel-only park/wake slice — introduced `ServiceOutcome::{Done, Parked}` + `Kernel.pending_wakes` + the `kernel_take_next_wake_for_pid` export; landed at commit 516df81), `docs/superpowers/specs/2026-04-19-ipc-accept-blocking-slice-2b-design.md` §3.1 (SIGTERM-interrupts-parked-syscall pattern — `Kernel::interrupt_parked_accept` is the direct template for `interrupt_parked_wait`), `specs/001-browser-os-v1/contracts/syscalls.md` §3.2 `proc_wait` (0x1101), `specs/001-browser-os-v1/data-model.md` §1 (`ProcState::BlockedOnWait` + `BlockReason::Wait { pid }` — spec-blessed since ratification, unused until this slice), `specs/001-browser-os-v1/multi-process-plan.md` §5 ("park-and-wake on fd readiness is a follow-up" — 2a addressed the ipc_accept half, 2c.1 addresses the proc_wait half).
Slice shape: single session, single commit. First of two sub-slices (2c.1 + 2c.2) that close out the long-running display-server accept loop arc's residual supervision work. 2c.1 is kernel-only; 2c.2 migrates userland (init supervision loop + coordinated shutdown).

## 1. Problem

`crates/kernel/src/syscall/ext.rs handle_proc_wait` today returns `-EAGAIN` via `WaitOutcome::WouldBlock` whenever matching children exist but none have transitioned to Zombie — the kernel's v1 `proc_wait` is unconditionally non-blocking. Every userland supervisor that wants to actually wait for a child has to busy-poll, or cannot exist at all. Today's userland is small enough to avoid the problem: init is fire-and-forget, `hello-wait-noop` is a test binary that expects `-ECHILD` (no-children path), and nothing else calls `proc_wait`. With slice 2b's display-server migration, display-server outlives init by construction; a supervision-loop init that would wake on any child exit requires real blocking `proc_wait`.

The ABI spec at `contracts/syscalls.md` §3.2 documents `proc_wait(pid, options) -> (status, signum)` with semantics *"blocks until a matching child transitions to `Zombie`, reaps the zombie, returns its exit status."* The `options` parameter has always carried `WNOHANG` per `abi::ext::wait_opts`, but the kernel handler treats every call as if `WNOHANG` were set regardless of the flag — an existing spec-vs-impl drift analogous to slice 2a's pre-`flags`-honouring `ipc_accept` (see 2a §1). The handler's own comment at the `handle_proc_wait` docstring is explicit: *"v1 always returns non-blocking — the caller retries."* That sentence is the drift 2c.1 closes for the `options=0` case.

Slice 2a made this tractable: the `ServiceOutcome::{Done, Parked}` return type, the `Kernel.pending_wakes: Vec<(Pid, Response)>` queue, and the `kernel_take_next_wake_for_pid` WASM export + JS drain loop are all already live. 2c.1 composes the same machinery against the second blockable kernel primitive. Slice 2b's SIGTERM-interrupts-park pattern (`Kernel::interrupt_parked_accept` + the `handle_proc_kill` Term post-step) is the direct template for the interrupt half.

**Scope boundary: 2c.1 is KERNEL-ONLY.** Userland binaries continue to pass `WNOHANG` on every existing `proc_wait` call site, preserving today's observable behaviour. Slice 2c.2 migrates init to a blocking supervision loop, drops display-server's SIGTERM-stay-alive-by-init-exit-ordering dependency, and restores the Playwright `"display-server fb blit ok"` observable.

## 2. Non-goals

Explicitly out of scope for 2c.1 — deferred to later slices:

- **Userland migration.** init stays fire-and-forget + single `proc_kill(display-server, SIGTERM)`; `hello-wait-noop` stays on its `-ECHILD` path (its caller has no children so it never parks); no other binary calls `proc_wait` today. Migration lands in 2c.2.
- **Multi-parent wait.** PMos's flat `ppid` means a pid has exactly one parent; POSIX's "another process group can reap me" semantics do not exist and are not planned. Nothing in 2c.1 depends on this being true, but the design record notes the invariant so a future arc that adds process groups knows what to revisit.
- **Process-group wait (`target < -1`).** `handle_proc_wait` at `crates/kernel/src/syscall/ext.rs` already returns `-EINVAL` for `target_pid < -1`; 2c.1 preserves that early-reject branch unchanged. `WaitTarget::ProcessGroup(pgid)` is not a v1 variant and is not added.
- **`WIFSTOPPED` / `WIFCONTINUED` semantics.** v1 has no job-control stop/continue signals (`signal::Signal` covers `Term / Interrupt / Pipe / Kill / Child` only). `WUNTRACED` is accepted at the wire level (no `EINVAL`) but has no effect; 2c.1 does not add stop-wait wake paths.
- **`proc_wait` returning on signal inbox population without an exit.** A parked parent whose signal inbox gains `SIGCHLD` (because a *different* child of the parent unrelated to the waited-on target exits) does not wake; only a zombie transition that matches the park's `WaitTarget` triggers a wake. POSIX is stricter — any `SIGCHLD` post can return EINTR from a blocking wait with interruptible handler — but v1 has no signal-handler installation so the interrupt-on-SIGCHLD path is moot. The one wake-by-signal path 2c.1 adds is SIGTERM → EINTR, mirroring 2b's shape.
- **Signal-interrupts-parked-wait for non-SIGTERM catchable signals** (SIGINT, SIGPIPE). Same "defer until a caller needs it" rule 2b used; the match arm is a one-line extension when a userland binary demands it.

## 3. Design

### 3.1. Parent-parks-on-wait semantics

**Invariant: at most one parked waiter per parent pid.** A parent that calls `proc_wait(options=0)` with matching live children parks; a second blocking `proc_wait` from the same parent while it is already parked is impossible under single-threaded user processes (a process cannot issue a second syscall while still parked in the first), but the kernel is defence-in-depth here: if the race ever fires (test harness, re-entrant dispatcher bug), the second park request returns `-EAGAIN` rather than clobbering the first parker's request_id. Mirror of slice 2a's one-parker-per-listener rule. POSIX allows reentrant waits from multiple threads sharing a pid; PMos v1 rejects them. Documented as a deferred invariant lifted by a future multi-threaded-pid arc if that ever arrives.

**Parking conditions.** Park only when all four hold: (a) `options & WNOHANG == 0`; (b) `Kernel::proc_wait` returned `WaitOutcome::WouldBlock` (live matching child exists, but none are zombies); (c) the parent is not already parked on wait; (d) the parent is not `Dead`/`Zombie` (the dispatcher guards this — a dead pid cannot issue syscalls). The `NoChildren` outcome skips parking and returns `-ECHILD` unchanged; `Reaped` skips parking and returns the packed status unchanged. Target rejection (self-wait, `target_pid < -1`) happens at the handler level before `Kernel::proc_wait` is called and is unchanged.

**Parker state.** The kernel tracks a single record per parked parent: `(req_id, target)`. `req_id` is the request's id from the inline args window (the response drained later must reuse it so the user Worker can correlate). `target` is the `WaitTarget` the handler already computed — either `Any` (for `target_pid` of `-1` or `0`) or `Specific(child_pid)`. Keeping the `target` on the parker lets the child-exit wake path decide match-vs-skip without re-parsing the original request. `ZombieTarget` (the existing scheduler-side enum) is an isomorphic shape but confined to `ProcessTable::find_zombie_child`; the parker uses `WaitTarget` because that's what the syscall layer speaks.

### 3.2. Wake on child Zombie transition

Two kernel paths transition a child to Zombie today:

1. `Kernel::proc_exit(pid, status)` — voluntary exit (user called `proc_exit`).
2. `Kernel::proc_kill(sender, target, Signal::Kill)`'s SIGKILL arm — involuntary termination (synchronous `procs.exit(target, Signaled(9))` call in the Kill match arm).

Both paths must trigger the same wake check:

> *After the child transitions to Zombie (or has already done so — the wake check is idempotent), look up `self.parked_waiters.get(&child.ppid)`. If present and its `target` matches the child (`Any` always matches; `Specific(p)` matches iff `p == child.pid`), reap the child inline, queue `(child.ppid, Response::ok(req_id, packed_status))` onto `pending_wakes`, transition the parent Ready, clear its `block_reason`, remove the parker from `parked_waiters`.*

The reap runs inside the child-exit path rather than inside the wake drain because that keeps the kernel's "exit is the single source of truth for no-more-kernel-resources" invariant intact (the exit-time fd-table drain at `Kernel::proc_exit` already depends on this). Reaping inside the child-exit call means the zombie's resources are released exactly once — the wake response carries the pre-computed `pack_exit_status(status)` value; the user Worker observes the reap via the response drain, not via a second round-trip.

The packed exit-status encoding at `handle_proc_wait`'s `pack_exit_status` is reused verbatim (bits 40..48 = flags `0x01 Exited / 0x02 Signaled / 0x04 Crashed`; bits 32..40 = signum when Signaled; bits 0..32 = exit code when Exited). No wire-format change.

**Heap readback.** `handle_proc_wait`'s synchronous `Reaped` path additionally writes the reaped child's pid as 4 bytes LE into `heap[0..4]` when `req.heap_len >= 4`, using `Response::extra_len = 4`. The park path cannot write to the user's heap later (the heap is owned by the user Worker, which is parked on `Atomics.wait`; the kernel Worker has no direct memory reference to it once the request is popped). **Decision:** the wake response carries the reaped pid in a form the user Worker recovers without heap readback — specifically, the user-wasm-runtime shim that drives `PROC_WAIT` pre-subscribes to heap readback by placing the 4-byte scratch at a fixed offset relative to the request_id, and the response drainer copies `child_pid` into that scratch before waking. **Simpler alternative** (selected): the wake response's `extra_len` stays 0, but the packed status stays a u64 in `Response::value`; if the user Worker needs the reaped pid it reads it off the response's encoded `value` *or* calls `proc_self` on the child (which is impossible post-reap). In practice: the current PMos user-wasm shim at `web/src/user-wasm-runtime.ts`'s `proc_wait` reads `heapOut[0..4]` to recover the child pid; for the park-path wake, the kernel packs the child pid into the high 32 bits of an extended `value` encoding OR the shim fetches it from `heap_out` filled at park-response-assembly time. **Open question (see §8):** which encoding does 2c.2's userland adopt? 2c.1's kernel side can emit the pid via `Response.extra_len = 4` by reserving the scratch slot at park time (the park path captures `req.heap_ptr` and `req.heap_len` so the wake response carries the correct extra window). The design prefers this: the synchronous-reap path and the park path produce response records with the same shape, so userland is agnostic to which path fired.

### 3.3. SIGTERM-interrupts-parked-wait with -EINTR

Mirror of slice 2b §3.1. New `Kernel` method:

```rust
/// Interrupt any parked `proc_wait` on `pid` with `-EINTR`. Clears
/// `parked_waiters[&pid]`, queues `(pid, Response::err(req_id, EINTR))`
/// onto `pending_wakes`, transitions `pid` Ready, clears block_reason.
/// No-op if `pid` is not parked on wait. Returns true iff a park was
/// interrupted (tests use the bool; production callers discard).
///
/// Called from `handle_proc_kill` when `Signal::Term` targets a
/// BlockedOnWait pid. Runs AFTER the signal-inbox delivery inside
/// `Kernel::proc_kill` so a userland caller draining fd 3 on observing
/// the EINTR wake finds the Term signal queued.
pub fn interrupt_parked_wait(&mut self, pid: Pid) -> bool;
```

Composition, body, and semantics are an exact analogue of `interrupt_parked_accept` — take the parker slot, build an `Response::err(req_id, EINTR)` wake, push it onto `pending_wakes`, transition Ready, clear `block_reason`. Best-effort transition (ignore the error) so a race between wake paths doesn't leave the process-table in an inconsistent state.

`handle_proc_kill`'s `Signal::Term` arm is extended to call **both** `interrupt_parked_accept(target_pid)` AND `interrupt_parked_wait(target_pid)` after the `Kernel::proc_kill` call returns. Order doesn't matter: v1 invariant is that a pid parks on at most one primitive at a time (a process either issued `ipc_accept` or `proc_wait` — not both simultaneously), so at most one of the two interrupt methods will observe the parker slot. The kernel is robust if both are called anyway — each method's `take`-and-clear is idempotent.

```text
Signal::Term arm, post `Kernel::proc_kill` call:
    if signal == Signal::Term {
        let _ = kernel.interrupt_parked_accept(target_pid);
        let _ = kernel.interrupt_parked_wait(target_pid);
    }
```

Same `SIGTERM-only` accepted-signum set as 2b. `signum=0` (POSIX probe) continues to route through `proc_check_signal` without touching either park. `Signal::Kill` continues to synchronously exit the target; the existing SIGKILL sweep at `Kernel::proc_kill`'s Kill arm gains a surgical `self.parked_waiters.remove(&target_pid)` call (mirror of `self.ipc.clear_parked_acceptor_for_pid(target_pid)` already there) so a SIGKILL'd parent parked on wait doesn't leave a stale parker slot. No EINTR wake is queued on SIGKILL — the pid is dead, not interrupted.

### 3.4. Handler semantics

`handle_proc_wait` changes return type from `Response` to `ServiceOutcome` (same pattern `handle_ipc_accept` took in 2a) and gains the park path:

```text
let target_pid = i32(req.args[0..4]);
let options    = args_u32(req, 4);
if options & !wait_opts::WNOHANG != 0 {
    return Done(err(req_id, EINVAL));
}
if target_pid < -1 {
    return Done(err(req_id, EINVAL));
}
let target = match target_pid {
    0 | -1 => WaitTarget::Any,
    p if p == pid => return Done(err(req_id, ECHILD)),  // self-wait
    p => WaitTarget::Specific(p),
};
let nohang = (options & wait_opts::WNOHANG) != 0;

match kernel.proc_wait(pid, target) {
    Ok(Reaped(child, status)) => {
        // Synchronous-reap path — unchanged from today.
        Done(ok_with_packed_status_and_heap_readback(child, status, req, heap))
    }
    Ok(WouldBlock) if nohang => Done(err(req_id, EAGAIN)),
    Ok(WouldBlock) /* blocking */ => match kernel.park_on_wait(pid, req_id, target, req.heap_len) {
        Ok(())                        => Parked,
        Err(WouldBlock)               => Done(err(req_id, EAGAIN)),  // already-parked invariant
        Err(e)                        => Done(err(req_id, kerr_to_errno(e))),
    },
    Ok(NoChildren)  => Done(err(req_id, ECHILD)),
    Err(e)          => Done(err(req_id, kerr_to_errno(e))),
}
```

The `dispatch_ext` match arm at `crates/kernel/src/syscall/ext.rs` changes from `op::PROC_WAIT => ServiceOutcome::Done(handle_proc_wait(kernel, pid, req, heap))` to `op::PROC_WAIT => handle_proc_wait(kernel, pid, req, heap)`. Mirror of the `IPC_ACCEPT` wiring 2a introduced.

`Kernel::park_on_wait(parent_pid, req_id, target, heap_len)`:
1. If `self.parked_waiters.contains_key(&parent_pid)`, return `WouldBlock` (one-waiter invariant).
2. Insert `self.parked_waiters.insert(parent_pid, WaitParker { req_id, target, heap_len })`.
3. Transition `parent_pid` `Running` → `BlockedOnWait` via `self.procs.transition(parent_pid, BlockedOnWait)`; map `TransitionError` to `NoSuchPid` (same pattern the existing park-on-accept uses).
4. Set `block_reason = Some(BlockReason::Wait { pid: target_pid_as_i32 })` — where `target_pid_as_i32` is `-1` for `WaitTarget::Any` or the specific pid otherwise. Existing `BlockReason::Wait { pid: Pid }` variant is reused verbatim (already defined at `crates/kernel/src/proc/mod.rs`; spec-blessed since the original data-model ratification, unused until this slice).

`heap_len` is stored on the parker so the wake path can attach the 4-byte child-pid readback to the wake response if the original request asked for it. Recorded at park time because the wake construction doesn't see the original request.

### 3.5. Kernel state additions

**New field on `Kernel`:**

```rust
/// Parents parked on a blocking `proc_wait`. Keyed by parent pid so
/// the child-exit wake path does an O(log n) lookup on `ppid`.
///
/// v1 invariant: at most one parker per parent. A second blocking
/// `proc_wait` from a parent that's already parked returns -EAGAIN
/// regardless of WNOHANG (see §3.1).
pub(crate) parked_waiters: alloc::collections::BTreeMap<Pid, WaitParker>,
```

**New type in `crates/kernel/src/sys.rs`:**

```rust
/// Parked-waiter record. One entry per parked parent in
/// `Kernel.parked_waiters`. Constructed by `park_on_wait`, consumed by
/// child-exit wake paths + `interrupt_parked_wait` + SIGKILL sweep.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct WaitParker {
    pub req_id: u32,
    pub target: WaitTarget,
    /// Heap-scratch length the original request reserved for the
    /// reaped-child pid readback. 0 or 4 in practice; the wake path
    /// writes a 4-byte child pid into Response.extra_len when this
    /// is 4, matching the synchronous-reap shape.
    pub heap_len: u32,
}
```

`Kernel::new` initialises `parked_waiters` to `BTreeMap::new()`.

No change to `BlockReason`: the existing `BlockReason::Wait { pid: Pid }` variant is used verbatim with `pid = -1` for `WaitTarget::Any` or `pid = specific_child_pid` otherwise. The variant has been spec-blessed at `proc/mod.rs` since ratification but has not been used until this slice.

### 3.6. Wire-format impact statement

**Zero new bytes.** `PROC_WAIT` (opcode 0x1101) has always been `args[0..4] = target_pid (i32)`, `args[4..8] = options (u32)`, `heap_len = 0` or `4`. `WNOHANG = 0x1` is an existing recognised bit. `options = 0` is the POSIX blocking default that `contracts/syscalls.md`'s semantics line ("blocks until a matching child transitions to `Zombie`") has always described — 2c.1 is the first slice whose behaviour matches the spec.

The only change visible to specs is operational: the `handle_proc_wait` docstring's line *"v1 always returns non-blocking — the caller retries"* (in the `ext.rs` comment block documenting the wire format) is replaced with *"v1 blocks the caller when `options & WNOHANG == 0` and matching children exist but none are zombies, matching the spec semantics. Wake fires on the first matching-child Zombie transition, SIGTERM wakes with -EINTR, SIGKILL on the parker exits it cleanly without a wake"*.

Response record shape is identical for the park path and the synchronous-reap path: `value = packed exit status`, `extra_len = 4` (child pid in heap scratch) or `0` — see §3.4. No new opcode, no new constants beyond the `WaitParker` internal type, and no change to `abi::ext` or `abi::ring`.

### 3.7. Dispatcher integration summary

Existing slice-2a wiring applies verbatim — 2c.1 is a pure additional producer of `ServiceOutcome::Parked` against the same machinery:

- `ServiceOutcome::Parked` skips the `sab.try_push_response` in `Dispatcher::service_one` (unchanged from 2a).
- JS dispatch loop's `startDispatchLoop` calls `kernel_take_next_wake_for_pid(pid)` before `serviceSab(pid, sab)` on each pass (unchanged from 2a).
- `kernel.pending_wakes` is the single queue drained across all producers (2a's ipc-connect-wake path, 2a's listener-close path, 2b's SIGTERM EINTR-wake, 2c.1's child-exit wake, 2c.1's SIGTERM EINTR-wake on parked-wait). Drained per-pid in insertion order, which is already "wake ordering" for any single parker.

No new JS. No new WASM export. No new TS shim. The integration surface is the single Kernel API method `park_on_wait` + the field `parked_waiters` + the field-walk inside the child-exit paths — everything else is already live.

## 4. Testing

Three-layer coverage. Rust + TS-unit gain tests; Playwright stays green without changes.

### 4.1. Kernel isolation (Rust, `crates/kernel/tests/`)

Eight new tests, placed in `crates/kernel/tests/sys.rs` adjacent to slice 2a's `ipc_accept` blocking block (around the existing `park_on_accept_clears_on_listener_close` test). A new `// ---- proc_wait blocking semantics (slice 2c.1) -----` header groups them.

1. **`proc_wait_options_zero_parks_parent_when_no_zombie`** — parent + one live non-zombie child. Parent dispatches `PROC_WAIT(target=-1, options=0)`. Assert: outcome is `Parked`; `parent.state == BlockedOnWait`; `parent.block_reason == Some(BlockReason::Wait { pid: -1 })`; `kernel.parked_waiters.get(&parent) == Some(WaitParker { req_id, target: Any, heap_len: 0 or 4 })`; `pending_wakes.is_empty()`; no response pushed to parent's SAB.

2. **`child_exit_wakes_parked_parent`** — continues from 1. Child calls `Kernel::proc_exit(child, ExitStatus::Exited(42))`. Assert: `parent.state == Ready`; `parent.block_reason == None`; `kernel.parked_waiters.get(&parent) == None`; `pending_wakes` contains exactly `(parent, Response::ok(req_id, pack_exit_status(Exited(42))))`; child has been reaped (`procs.get(child) == None`).

3. **`wnohang_preserves_eagain`** — parent with live non-zombie child. Parent dispatches `PROC_WAIT(target=-1, options=WNOHANG)`. Assert: outcome is `Done(Response::err(req_id, EAGAIN))`; `parent.state` stays `Running`; `kernel.parked_waiters.is_empty()`; no transition to `BlockedOnWait`.

4. **`second_wait_on_parked_parent_returns_eagain`** — parent parks on wait (as in test 1). While parked, a second `PROC_WAIT(options=0)` dispatch from the same pid arrives (re-entrant dispatch simulation). Assert: outcome is `Done(Response::err(req_id2, EAGAIN))`; `kernel.parked_waiters[&parent].req_id == req_id1` (original parker unchanged); `parent.state` stays `BlockedOnWait`. Pins the one-waiter invariant.

5. **`sigterm_interrupts_parked_wait_with_eintr`** — parent (parked on wait) receives `Signal::Term` via `handle_proc_kill` from its own parent (or self — cap check passes on self-signal). Assert: `parent.state == Ready`; `parent.block_reason == None`; `kernel.parked_waiters.is_empty()`; `pending_wakes` contains exactly `(parent, Response::err(req_id, EINTR))`; `parent.signal_inbox` holds `[Signal::Term]`; the original child is still alive + not reaped.

6. **`sigkill_on_parked_parent_exits_without_eintr_wake`** — parent parked on wait. Parent's own parent calls `Kernel::proc_kill(grandparent, parent, Signal::Kill)`. Assert: `parent.state == Zombie` with `ExitStatus::Signaled(9)`; `kernel.parked_waiters.is_empty()` (slot cleared by the SIGKILL sweep); `pending_wakes.is_empty()` (no EINTR wake — the pid is dead, not interrupted); the original child is still alive + not reaped. Mirror of slice 2b's `sigkill_on_parked_accept_exits_without_eintr_wake`.

7. **`specific_target_wake_only_matches_specific_child`** — parent with two live children `child_A`, `child_B`. Parent parks on `PROC_WAIT(target=child_B, options=0)`. `child_A` exits via `proc_exit` FIRST. Assert: `parent.state` stays `BlockedOnWait`; `parked_waiters[&parent]` intact; `pending_wakes.is_empty()` (child_A doesn't match); child_A is now Zombie (unreaped — the parent's target doesn't match, so no reap fires). Then `child_B` exits. Assert: parent wakes; `pending_wakes` contains `(parent, Response::ok(req_id, pack(child_B's status)))`; child_B reaped; child_A still Zombie (a later non-blocking wait with `WaitTarget::Any` or `Specific(child_A)` would reap it).

8. **`parent_exit_clears_parked_waiter_slot`** — sanity edge. Parent parks on wait. Parent is SIGKILL'd by its own parent (covered functionally by test 6, but test 8 adds a second variant exercising `Kernel::proc_exit` rather than `proc_kill`: a test directly calling `kernel.proc_exit(parent, Crashed)` — simulating a Worker crash observed via the host-side crash path — must also sweep the parker slot so `kernel.parked_waiters.is_empty()` after.). Assert: `parked_waiters.is_empty()`, no spurious wake on pending_wakes.

**Rust test delta:** 1200 (after 2b) → 1208 (+8).

### 4.2. TS dispatcher (`web/tests/unit/kernel-wasm-host.test.ts`)

Three new tests in a new `describe("dispatch: PROC_WAIT blocking")` block placed immediately after the existing `dispatch: PROC_WAIT` block at the `kernel-wasm-host.test.ts` PROC_WAIT coverage location, analogous to slice 2a's `describe("dispatch: IPC_ACCEPT blocking")`.

1. **`PROC_WAIT options=0 on live child parks caller (no response push)`** — real `kernel.wasm` via `KernelWasmHost`. Register parent pid + one live child via the existing helpers; transition parent to `Running`. Dispatch `PROC_WAIT(target=-1, options=0)`. Assert: the response ring is empty (`sab.try_pop_response() === null`); `kernelTakeNextWakeForPid(parent)` returns 0 (no wake queued yet); the parent is parked (queryable via the existing helpers that inspect process state).

2. **`child exit wakes parked parent with packed status`** — continuation of 1. Dispatch `PROC_EXIT(0)` from the child. Assert: a wake was queued; `kernelTakeNextWakeForPid(parent, sab_parent)` pushes the wake response onto the parent's SAB; the popped response has `status === 0`, `value === pack_exit_status(Exited(0))`, `requestId === original`.

3. **`SIGTERM wakes parked parent with EINTR`** — register parent + child + grandparent (parent's ppid). Transition parent to Running. Park the parent on wait (dispatch `PROC_WAIT(options=0)` — assert `parked === true`). Dispatch `PROC_KILL(parent, signum=15)` from grandparent. Assert the proc_kill response `status === 0`. `takeNextWakeForPid(parent)` returns a wake; the pushed response has `status === -EINTR (-27)`, `requestId === original`. A second `takeNextWakeForPid` returns null. Mirror of slice 2b's dispatcher SIGTERM test.

**TS-unit delta:** 496 (after 2b) → 499 (+3).

### 4.3. Playwright integration

**No changes.** Userland still passes `WNOHANG` on every existing call site. `hello-wait-noop`'s `proc_wait(-1, 0, 0)` still hits the `NoChildren → ECHILD` arm (the binary has no children) — the park path is unreachable under 2c.1's userland. Playwright's existing 2-test suite stays green without modification. Slice 2c.2 is the one that meaningfully changes Playwright (restores `"fb blit ok"` + adds "init reaped <pid>" observables).

**Playwright delta:** 2 → 2.

### 4.4. Test-count summary

| Layer | Before (2b close-out) | After (2c.1) | Δ |
|---|---:|---:|---:|
| Rust | 1200 | 1208 | +8 |
| TS-unit | 496 | 499 | +3 |
| Playwright | 2 | 2 | 0 |
| **Workspace total** | **1698** | **1709** | **+11** |

## 5. Edge cases

- **Parent exits while parked on wait.** `Kernel::proc_exit(parent, ...)` (either voluntary or called by the host-side Worker-crash path) must sweep the parker slot: `self.parked_waiters.remove(&parent)`. Covered by test 8. Symmetric to the `self.ipc.clear_parked_acceptor_for_pid` call at `proc_exit`.
- **Parent SIGKILL'd while parked on wait.** `Kernel::proc_kill`'s Kill arm must sweep too: `self.parked_waiters.remove(&target_pid)` next to the existing `self.ipc.clear_parked_acceptor_for_pid(target_pid)` call. Covered by test 6. No EINTR wake is queued — the pid is dead, not interrupted (same rule 2b established for parked-accept).
- **Multiple children exit before parent re-enters wait.** PMos does not queue multiple wakes. The first child-exit-that-matches fires one wake; subsequent child-exits-that-match (still with the parent Ready) fire no wake (the parent has already been transitioned out of `BlockedOnWait` and `parked_waiters.get(&parent)` returns `None`). The parent's next non-blocking `PROC_WAIT(WNOHANG)` sees the remaining zombies via `find_zombie_child` and reaps them one-at-a-time, same as today. A parent that wants to loop-reap-until-empty issues a non-blocking wait after every blocking wait — idiom identical to POSIX `while (waitpid(-1, ..., WNOHANG) > 0);`.
- **Child exits via SIGKILL (non-voluntary).** Covered by the `Kernel::proc_kill`'s `Signal::Kill` arm — that arm calls `procs.exit(target, Signaled(9))` which transitions the child to Zombie and then calls `post_sigchld(target_ppid)`. The wake check on child-Zombie must fire here AND on `Kernel::proc_exit` — both child-exit paths must trigger the parker lookup. Design: extract a `wake_parked_waiter_on_child_exit(&mut self, child_pid, ppid, status)` helper and call it from both paths post-exit (same structural pattern 2a used for `wake_parked_acceptor_if_any`).
- **Target == parent's own pid.** Rejected with `-ECHILD` BEFORE parking (the existing early-reject at `handle_proc_wait` above `Kernel::proc_wait` — unchanged by 2c.1). POSIX `waitpid(self, ...)` semantically means "a pid that can't be my child"; returning ECHILD matches Linux.
- **Target == 0 or -1 → Any.** Handled at the syscall handler level before `Kernel::proc_wait`; unchanged by 2c.1.
- **Target > 0 but target pid doesn't exist or isn't a child of parent.** `Kernel::proc_wait` returns `NoChildren` → `-ECHILD`; the park path is not reached. Unchanged.
- **`options` with bits beyond WNOHANG set.** Early-rejected with `-EINVAL` at the handler level, unchanged.
- **Orphaned parker on child exit race.** Impossible — the kernel is single-threaded; an exit path that runs the wake check cannot interleave with a concurrent re-entry of `park_on_wait` on the same parent.
- **Parker's `heap_len` was 0 at park time but the wake has a pid to report.** The synchronous-reap path already tolerates this (writes no extra_len when `heap_len < 4`); the park path mirrors: if `parker.heap_len < 4`, the wake response has `extra_len = 0` and the user shim reads only the packed status from `value`. Both behaviours match today's handler shape.

## 6. Risks

- **Borrow-checker against `&mut Kernel` in child-exit path.** `Kernel::proc_exit` already does three mutable sub-borrows sequentially (fd-table drain at `release_fd_table_resources`, IPC parker sweep at `ipc.clear_parked_acceptor_for_pid`, procs table mutation at `procs.exit`, then `post_sigchld`). Adding a fourth — the `parked_waiters` lookup + reap-and-wake — is similar scope and same borrow pattern. The wake path needs `&mut self` to call `reap` (which mutates `procs`) + push onto `pending_wakes` + transition parent to Ready + remove from `parked_waiters` — all four live on `Kernel`, no sub-object borrow contention. Same risk as 2a, resolved the same way (scoped mutable borrows; surfaces at compile time if it bites).
- **Silent promotion of existing `options=0` call sites to blocking.** After 2c.1, any existing userland binary that passes `options=0` with a live matching child will park forever unless it also has a SIGTERM path. Current userland audit: (a) `crates/hello-wait-noop` calls `proc_wait(-1, 0, 0)` — its process has no children (it's itself a child of init), so the `NoChildren → ECHILD` arm fires BEFORE parking; not affected. (b) `crates/init` — fire-and-forget, never calls `proc_wait`; not affected. (c) `crates/display-server`, `crates/display-client-demo`, `crates/hello-std` — none call `proc_wait`; not affected. (d) Test harnesses — the vitest composition tests and the playwright spec do not exercise `proc_wait` directly. No userland caller silently becomes blocking under 2c.1. Verification method: grep for `proc_wait\|PROC_WAIT` across `crates/` and `web/src/` — every hit is accounted for above.
- **Packed exit-status encoding unchanged.** `pack_exit_status` at the existing `handle_proc_wait` site is reused verbatim. If the wake path builds the `Response::value` using the same helper, the two paths produce identical response bytes (modulo `extra_len` per §3.4). Risk is low; enforced by test 2 (`child_exit_wakes_parked_parent`) comparing the wake response byte-for-byte against `pack_exit_status(status)`.
- **Child-exit path borrow split between `proc_exit` and `proc_kill::Kill`.** Factoring the wake check out as a single `Kernel::wake_parked_waiter_for_child(child_pid, ppid)` helper is cleaner but the two callers have slightly different shapes (`proc_exit` runs post-`procs.exit`, with the child already in Zombie state; `proc_kill::Kill` runs after the target_ppid capture but before the `post_sigchld` call). The helper signature should take `(child_pid, ppid, status)` and look up `self.parked_waiters.get(&ppid)` — both callers can compute these three values at the same call-site shape. If factoring adds friction, inline both is acceptable; the tests validate either shape equally.
- **Heap readback in wake response (see §3.4 open question).** The decision sketched above — the wake response carries `extra_len = 4` with the child pid — requires the wake construction to have access to the heap-scratch buffer at wake time. Since `pending_wakes` is drained by the JS side via `kernel_take_next_wake_for_pid` which reuses the kernel's `RESP_SCRATCH` region, the 4-byte child-pid extra fits within existing scratch. Risk: if the wake-drain path doesn't currently copy `extra_len` bytes, a small JS-side adjustment is required. Pinned as an open question for the plan slice to verify against the actual `kernel_take_next_wake_for_pid` implementation.

## 7. Constitution check

| # | Principle | Status | Rationale |
|---|---|---|---|
| I | Real OS, not a simulation | PASS | Kernel gains actual blocking `proc_wait` semantics — POSIX `waitpid(... , options=0)` is a textbook OS kernel primitive. No UI concepts enter the kernel. The `BlockedOnWait` state + `BlockReason::Wait` variant have been spec-blessed since ratification; 2c.1 is the first slice that fires them. |
| II | Strict layering | PASS | No new opcodes. No new wire-format bytes. `parked_waiters` is a kernel-internal field. The ServiceOutcome + pending_wakes + kernel_take_next_wake_for_pid machinery all landed in 2a; 2c.1 is a new producer against them. The layering test (desktop shell replaceable without touching kernel) is unaffected — the desktop shell does not call `proc_wait` today and will not need to in v1. |
| III | Browser-only, zero backend | PASS | No new network touchpoints. |
| IV | Offline-first | PASS | No SW / OPFS changes. |
| V | Process isolation | PASS — **strengthens** | Today a supervision-loop parent that wants to block on child exit has to busy-poll `PROC_WAIT(WNOHANG)` from user code, burning the parent Worker's CPU. After 2c.1, the parent parks on `Atomics.wait` and the Worker is genuinely idle until the kernel pushes a response. Wake crosses the SAB boundary — no shared-memory shortcut. Same strengthening 2a landed for the ipc_accept half; 2c.1 extends it to the proc_wait half. |
| VI | Standard syscall surface | PASS | `proc_wait`'s `options=0` blocking semantic + `WNOHANG` non-blocking are POSIX `waitpid(2)`. `-EINTR` on SIGTERM-interrupted blocking wait matches POSIX §7. No new opcodes; no new ABI constants (the existing `WNOHANG` bit and the implicit `options=0` semantic were always part of the spec). The kernel's comment saying *"v1 always returns non-blocking — the caller retries"* was a documented impl-vs-spec drift; 2c.1 closes it. |
| VII | Protocol over API | PASS | Display protocol bytes untouched. |
| VIII | Bottom-up | PASS | Kernel-layer-only slice. Every upper layer unchanged. Test tier 1 (Rust kernel isolation) exercises the park + wake + interrupt paths with no userland binary; test tier 2 (dispatcher) exercises the kernel-wasm-host surface; tier 3 (Playwright) is unchanged because no userland exercises the new path under 2c.1. Test-first cadence preserved. |
| IX | Performance budget | PASS — **improves only when userland migrates** | 2c.1 itself deletes no busy-poll (userland is unchanged). Strictly, cold/warm/input budgets are neutral under 2c.1. When 2c.2 migrates init to a blocking supervision loop, the supervisor's CPU drops from "busy-poll + wake slot bump every SIGCHLD" to "park on `Atomics.wait` until kernel pushes a response" — same magnitude as 2a's display-server CPU reduction. 2c.1 is the precondition; the budget win is deferred but already committed. |
| X | Testability at every layer | PASS | +8 kernel tests (§4.1), +3 dispatcher tests (§4.2), 0 Playwright changes. Every new code path is covered at the layer where it lives. Kernel isolation tests pass before dispatcher tests are exercised. |

**No new deviations introduced.** 2a's one-parker-per-listener deviation is unchanged; 2c.1 lands the analogous one-waiter-per-parent deviation, documented in-code at the `parked_waiters` field comment as a future-slice-lifting invariant.

**Deviation closed:** `handle_proc_wait`'s *"v1 always returns non-blocking"* docstring drift against `contracts/syscalls.md` §3.2's *"blocks until a matching child transitions to Zombie"* is closed for the first time — 2c.1 implements the spec's semantics paragraph exactly.

## 8. Slice 2c.2 outline (forward context, not a commitment)

2c.2 composes 2c.1's kernel primitive at the userland layer. Sketch:

- **init supervision loop.** Replace init's current fire-and-forget main with `spawn all children`, then a loop: `match proc_wait(Any, options=0) { pid > 0 => println!("init reaped pid={}"); }`. When every child has been reaped (next call returns `-ECHILD`), init sends `SIGTERM` to display-server (currently its pre-exit act today) and waits once more to reap display-server; on `-ECHILD` thereafter, init exits. Shape matches POSIX `init(1)`.
- **display-server unbounded outer loop.** The `MAX_CLIENTS = 4` ceiling in `crates/display-server/src/main.rs` drops entirely — the loop becomes `loop { ... }` with a signal-driven exit via fd 3. Signal channel wiring is already in place (slice 2b landed `poll_sigterm_nonblock` + the fd-3 drain shape); 2c.2 replaces the current init-drives-shutdown-ordering with supervisor-drives-shutdown-ordering (init reaps both display-client-demos first, THEN SIGTERMs display-server).
- **Playwright restoration.** Restore the `"display-server fb blit ok"` console-line assertion + add `"init reaped pid=<M>"` observables for each of init's four spawned children. The line-count strengthens from 2b's + 1 ("init sent SIGTERM") to + 5 (one SIGTERM + four reaps).

### Open questions (note for 2c.2 planning, not a 2c.1 commitment)

- **Heap readback encoding for reaped child pid.** §3.4 chose "wake response carries `extra_len = 4` with the child pid" — requires the `kernel_take_next_wake_for_pid` drainer to honour `Response.extra_len` on the way to the SAB. The 2a `kernel_take_next_wake_for_pid` implementation should be verified against this assumption at plan-writing time; if it currently only copies the 32-byte `Response` record and not the extra_len bytes, a small adjustment is needed in the plan slice. Alternative shape: pack the child pid into an unused high bit of `Response::value` (the packed-status encoding uses only bits 0..48 today, leaving bits 48..64 free for the child pid as u16 — but pid is i32 in PMos, so this doesn't fit). The extra_len path is the more orthogonal shape.
- **`WUNTRACED` accepted-but-ignored surface.** `handle_proc_wait` today accepts `WUNTRACED` silently (the `options & !WNOHANG != 0` check rejects bits beyond `WNOHANG` — so `WUNTRACED` actually returns `-EINVAL`). The `abi::ext::wait_opts::WUNTRACED` constant is defined but unreachable. 2c.1 leaves this quirk unchanged (unrelated to the park/wake work); 2c.2 may clean up by either (a) removing the `WUNTRACED` constant, (b) making the handler accept + ignore it, or (c) documenting the intentional `EINVAL`. None of the three is blocking.
