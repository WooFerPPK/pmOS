# `proc_wait` blocking semantics (kernel park/wake + SIGTERM interrupt) — slice 2c.1 implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the spec-vs-impl drift at `specs/001-browser-os-v1/contracts/syscalls.md` §3.2 (`proc_wait`'s blocking semantics have been in the spec since v1 but the kernel handler treats every call as non-blocking regardless of `options`) and implement the second half of the park-and-wake follow-up from `multi-process-plan.md §5`. After this slice, `proc_wait(target, options=0)` with matching live-but-not-yet-zombie children blocks the caller; a matching child's Zombie transition (via `proc_exit` or `proc_kill` SIGKILL) reaps the child inline, queues the wake with the packed exit status, and transitions the parent Ready. `SIGTERM` against a parent parked on wait wakes it with `-EINTR`; `SIGKILL` exits the parent cleanly without queuing an EINTR wake. Kernel-only slice — userland migration deferred to 2c.2.

**Architecture:** Single-layer kernel slice composing the slice-2a machinery (`ServiceOutcome::{Done, Parked}` + `Kernel.pending_wakes` + `kernel_take_next_wake_for_pid`) against the second blockable kernel primitive. Adds `Kernel.parked_waiters: BTreeMap<Pid, WaitParker>` keyed by parent pid, `Kernel::park_on_wait`, `Kernel::interrupt_parked_wait` (mirror of 2b's `interrupt_parked_accept`), and a shared `Kernel::wake_parked_waiter_for_child(child_pid, ppid, status)` helper invoked from both child-exit paths (`Kernel::proc_exit` + `Kernel::proc_kill`'s SIGKILL arm). `handle_proc_wait` changes return type from `Response` to `ServiceOutcome` and branches on `WNOHANG` + `WaitOutcome::WouldBlock` to either return `-EAGAIN` (nonblocking path) or call `park_on_wait` (blocking path). `handle_proc_kill`'s `Signal::Term` arm gains a second `interrupt_parked_wait(target_pid)` call alongside the existing `interrupt_parked_accept(target_pid)`. `handle_proc_kill`'s `Signal::Kill` arm gains a surgical `self.parked_waiters.remove(&target_pid)` call alongside the existing `ipc.clear_parked_acceptor_for_pid`. **Open-question resolution (Option A):** the kernel-side wake drain export `kernel_take_next_wake_for_pid` is extended to also surface the user-SAB heap_ptr + copy the 4-byte reaped-child-pid into kernel HEAP_SCRATCH when `Response.extra_len == 4`; the TS drainer `drainWakesForPid` is extended to copy that slice back into the user SAB's heap scratch at the recorded heap_ptr offset. This keeps the synchronous-reap path and the parked-wake path producing response records with identical shape, which lets 2c.2's init supervisor log reaped pids on a `WaitTarget::Any` wake.

**Tech Stack:** Rust (`wasm32-unknown-unknown` kernel target, host-target for tests), TypeScript (vitest unit). Justfile + cargo + esbuild toolchain. `export PATH="$HOME/.cargo/bin:$PATH"` before any cargo/just invocation.

---

## Open-question resolution — `extra_len` on the wake-drain path

Design spec §8 flagged: does `kernel_take_next_wake_for_pid` honour `Response.extra_len` today?

**Answer, verified against source:** NO. Current state (commit `7db38ff`):

- `crates/kernel/src/wasm_entry.rs:317-329` — `kernel_take_next_wake_for_pid(pid)` writes ONLY the 32-byte `Response` record into `RESP_SCRATCH`. No heap bytes are copied. No heap_ptr is surfaced.
- `web/src/kernel-wasm-host.ts:698-730` — `drainWakesForPid(pid, sab)` reads ONLY `RESP_SCRATCH` and pushes the 32-byte response onto the SAB response ring. No heap writeback path.

This is fine for slice 2a's ipc-accept wake (always `extra_len = 0`) but breaks slice 2c.1's design §3.4 intent (the reaped-child-pid rides as `extra_len = 4` in the wake response, matching the synchronous-reap shape).

**Decision: Option A — extend the drainer.** 2c.1's plan includes the kernel-export + TS-host drainer work needed to carry `extra_len` bytes end-to-end. Scope additions:

1. **Kernel-side storage change:** `Kernel.pending_wakes` entries extend from `(Pid, Response)` to `(Pid, Response, WakeHeap)` where `WakeHeap = Option<PendingHeap>` and `PendingHeap { heap_ptr: u32, bytes: alloc::vec::Vec<u8> }`. All existing producers (2a's ipc-connect wake path, 2a's listener-close-EBADF wake path, 2b's SIGTERM-EINTR wake on parked-accept) push entries with `heap: None` — they have no extra_len to deliver. 2c.1's new producers (`wake_parked_waiter_for_child`'s reap-and-wake path) push entries with `heap: Some(PendingHeap { heap_ptr, bytes: child_pid.to_le_bytes().to_vec() })` when the parker's recorded `heap_len >= 4`; otherwise `heap: None`.

2. **Kernel-side export change:** `kernel_take_next_wake_for_pid(pid)` is extended to ALSO write the drained entry's heap bytes (if any) into `HEAP_SCRATCH[0..extra_len]`. The user's original heap_ptr is surfaced via a new scratch slot readable with a new export `kernel_resp_heap_ptr() -> u32`. When the drained entry has `heap: None`, the slot is zeroed (and `extra_len == 0` in the Response tells the TS side to skip the heap copy anyway).

3. **TS-side drainer change:** `drainWakesForPid(pid, sab)` checks `response.extraLen`; if > 0, it reads `response.extraLen` bytes from kernel `HEAP_SCRATCH[0..]`, reads the user heap_ptr from the new `kernel_resp_heap_ptr()` export, and copies the bytes into the SAB at `baseOffset + OFF_HEAP_SCRATCH + heap_ptr`. The push onto the response ring is unchanged.

4. **Parker state change:** `WaitParker` gains `heap_ptr: u32` alongside the design's `heap_len: u32`. Both are captured at park time from the request (`req.heap_ptr`, `req.heap_len`); both are needed by the wake path (heap_ptr to tell the drainer where to copy; heap_len to tell the wake whether to bother emitting `extra_len = 4` at all).

This is additive (no existing 2a/2b caller changes behaviour), touches two new exports + one TS method, and adds ~40 lines of total code. The alternative (Option B — drop heap readback for parked-wait) would force 2c.2's init supervisor to abandon `WaitTarget::Any` (since it couldn't recover the reaped child pid without heap readback), defeating the primary userland observable. Option A is correct.

Tasks 3-4 below include the drainer extension work; Task 2's TS test pins the end-to-end `extra_len` round-trip.

---

## File structure

| File | Responsibility | Action |
|---|---|---|
| `crates/kernel/src/sys.rs` | Top-level `Kernel` struct + process-table wrappers. | Modify (add `parked_waiters` field + `WaitParker` type + `park_on_wait` + `interrupt_parked_wait` + `wake_parked_waiter_for_child` helper; extend `proc_exit` with parker sweep; extend `proc_kill`'s Kill arm with parker sweep + wake check; extend `proc_kill`'s `Signal::Kill` arm to call the wake helper; extend `pending_wakes` entry shape to carry optional heap bytes) |
| `crates/kernel/src/syscall/ext.rs` | Per-opcode ext handlers. | Modify (rewrite `handle_proc_wait` to return `ServiceOutcome` + branch on `WNOHANG`; update `dispatch_ext`'s `PROC_WAIT` arm to pass through `ServiceOutcome` directly; extend `handle_proc_kill`'s `Signal::Term` arm to call `interrupt_parked_wait`) |
| `crates/kernel/src/wasm_entry.rs` | Kernel WASM ABI exports. | Modify (extend `kernel_take_next_wake_for_pid` to copy heap bytes into `HEAP_SCRATCH` + capture user heap_ptr; add new `kernel_resp_heap_ptr` export) |
| `crates/kernel/tests/sys.rs` | Native-target kernel isolation tests. | Modify (append 8 new tests in a `// ---- proc_wait blocking semantics (slice 2c.1) -----` block adjacent to slice 2b's `sigkill_on_parked_accept_exits_without_eintr_wake` test) |
| `web/src/kernel-wasm-host.ts` | TS host around the kernel WASM. | Modify (extend `drainWakesForPid` to copy `extra_len` heap bytes back to SAB; extend `takeNextWakeForPid` to also surface heap bytes for tests; add `kernel_resp_heap_ptr` to the exports interface) |
| `web/tests/unit/kernel-wasm-host.test.ts` | TS dispatcher tests against real `kernel.wasm`. | Modify (add 3 new tests in a new `describe("dispatch: PROC_WAIT blocking")` block placed immediately after the existing `dispatch: PROC_WAIT` block) |
| `dist/assets/kernel.wasm` | Built kernel artefact. | Regenerated by `just build`, staged |
| `SESSION-NOTES.md` | Slice-log narrative. | Append one entry (separate `docs` commit after the feat commit) |

**Not touched** (with rationale):

- `crates/abi/` — zero wire-format bytes; opcode `PROC_WAIT` (0x1101) layout is unchanged; `WNOHANG = 0x1` was always defined. No abi-ext / abi-ring edit.
- `crates/init/` + `crates/display-server/` + `crates/hello-wait-noop/` + `crates/display-client-demo/` + `crates/toolkit/` — all userland is kernel-only-slice-invariant (see design §1's "Scope boundary: 2c.1 is KERNEL-ONLY"). `hello-wait-noop`'s existing `proc_wait(-1, 0, 0)` call continues to hit the `NoChildren → ECHILD` arm before any park check (binary has no children). No userland binary silently becomes blocking.
- `web/src/user-wasm-runtime.ts` — the user-wasm shim already passes the request's `heap_ptr` + `heap_len` through dispatch; with Option A the same heap_out path (used by synchronous `PROC_WAIT(WNOHANG)`) now fires for parked wakes too, with zero shim changes.
- `specs/001-browser-os-v1/contracts/syscalls.md` — §3.2's `proc_wait` semantics paragraph already says *"blocks until a matching child transitions to Zombie"*. 2c.1 implements what the spec has always described; no contract edit needed. (Contrast with 2a, which edited `ipc_accept`'s paragraph because the `flags` parameter was documented-as-reserved; `proc_wait`'s semantics never drifted in prose, only in code.)
- `crates/kernel/src/proc/mod.rs` — `BlockedOnWait` state + `BlockReason::Wait { pid: Pid }` variant + the `(Running, BlockedOnWait)` / `(BlockedOnWait, Ready)` / `(BlockedOnWait, Zombie)` transition entries at lines 66-227 are already defined. 2c.1 is the first slice that exercises them.
- `web/tests/integration/real-kernel.spec.ts` — Playwright is unchanged under 2c.1 (see design §4.3). The `fb blit ok` restoration is a 2c.2 concern.

---

## Task 1 — Write failing Rust kernel-isolation tests (red step)

Eight new tests pin the park / wake / interrupt contract before the implementation exists. All eight fail with compile errors on symbols that don't yet exist (`Kernel::park_on_wait`, `Kernel::interrupt_parked_wait`, `Kernel::parked_waiters`, `WaitParker`) or with behavioural failures on today's non-blocking `handle_proc_wait`.

**Files:**
- Modify: `crates/kernel/tests/sys.rs` (append 8 tests immediately after the existing `sigkill_on_parked_accept_exits_without_eintr_wake` test around line 1770)

- [ ] **Step 1: Append the `proc_wait` blocking test block header + first two tests**

In `crates/kernel/tests/sys.rs`, locate the existing `sigkill_on_parked_accept_exits_without_eintr_wake` test (defined around line 1697, closing around line 1770). Immediately after its closing `}`, append:

```rust
// ---- proc_wait blocking semantics (slice 2c.1) ---------------------

#[test]
fn proc_wait_options_zero_parks_parent_when_no_zombie() {
    // Parent has one live, non-zombie child. proc_wait(target=-1,
    // options=0) must park the parent: no response queued, state
    // transitions to BlockedOnWait, block_reason gains
    // BlockReason::Wait { pid: -1 }, parked_waiters records the
    // (req_id, target, heap_ptr, heap_len).
    let mut k = make_kernel();
    let parent = k
        .register_process(RegisterArgs {
            name: "parent",
            ppid: 0,
            caps: initial::INIT,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(parent).unwrap();
    k.procs
        .transition(parent, kernel::proc::ProcState::Running)
        .unwrap();
    let _child = spawn_ordinary_app(&mut k, parent, "child");

    // Park the parent. `park_on_wait` takes (parent_pid, req_id,
    // target, heap_ptr, heap_len) and performs the same set-of-
    // preconditions the handler layer runs (target is a live
    // child + not already parked).
    let req_id = 0xc0deu32;
    k.park_on_wait(
        parent,
        req_id,
        kernel::sys::WaitTarget::Any,
        0,
        0,
    )
    .unwrap();

    // State transitioned to BlockedOnWait.
    let proc = k.procs.get(parent).unwrap();
    assert_eq!(proc.state, kernel::proc::ProcState::BlockedOnWait);
    assert_eq!(
        proc.block_reason,
        Some(kernel::proc::BlockReason::Wait { pid: -1 }),
    );

    // parked_waiters contains the expected record.
    let parker = k.parked_waiters_get_public(parent).unwrap();
    assert_eq!(parker.req_id, req_id);
    assert_eq!(parker.target, kernel::sys::WaitTarget::Any);
    assert_eq!(parker.heap_ptr, 0);
    assert_eq!(parker.heap_len, 0);

    // No wake queued yet.
    assert!(k.pending_wakes_is_empty());
}

#[test]
fn child_exit_wakes_parked_parent() {
    use abi::ring::Response;

    // Parent parks; child calls proc_exit; wake is queued with the
    // packed exit status + reaped child pid in the heap bytes; the
    // child is reaped (no longer in procs).
    let mut k = make_kernel();
    let parent = k
        .register_process(RegisterArgs {
            name: "parent",
            ppid: 0,
            caps: initial::INIT,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(parent).unwrap();
    k.procs
        .transition(parent, kernel::proc::ProcState::Running)
        .unwrap();
    let child = spawn_ordinary_app(&mut k, parent, "child");

    // Park on `Any` with heap_len = 4 so the wake carries the pid.
    let req_id = 0xfeedu32;
    k.park_on_wait(
        parent,
        req_id,
        kernel::sys::WaitTarget::Any,
        0,
        4,
    )
    .unwrap();

    // Child exits voluntarily.
    k.proc_exit(child, kernel::proc::ExitStatus::Exited(42))
        .unwrap();

    // Parent transitioned back to Ready with block_reason cleared.
    let proc = k.procs.get(parent).unwrap();
    assert_eq!(proc.state, kernel::proc::ProcState::Ready);
    assert!(proc.block_reason.is_none());

    // parked_waiters slot is cleared.
    assert!(k.parked_waiters_get_public(parent).is_none());

    // Child is reaped (fully removed from procs).
    assert!(!k.procs.is_alive(child));

    // Exactly one wake queued: (parent, ok, packed status, heap[0..4] = child_pid).
    let wakes = k.pending_wakes_snapshot();
    assert_eq!(wakes.len(), 1);
    let (wake_pid, wake_resp, wake_heap) = &wakes[0];
    assert_eq!(*wake_pid, parent);
    assert_eq!(wake_resp.request_id, req_id);
    assert_eq!(wake_resp.status, 0);
    assert_eq!(wake_resp.extra_len, 4);
    // value is the packed exit status — identical to what the
    // synchronous-reap path produces.
    let expected_value = {
        let tmp = Response::ok(
            0,
            kernel::sys::pack_exit_status_public(
                kernel::proc::ExitStatus::Exited(42),
            ),
        );
        tmp.value
    };
    assert_eq!(wake_resp.value, expected_value);
    // Heap bytes carry the reaped child pid as u32 LE.
    let heap = wake_heap.as_ref().expect("wake has heap payload");
    assert_eq!(heap.heap_ptr, 0);
    assert_eq!(heap.bytes.len(), 4);
    let decoded = u32::from_le_bytes([
        heap.bytes[0],
        heap.bytes[1],
        heap.bytes[2],
        heap.bytes[3],
    ]);
    assert_eq!(decoded, child as u32);
}
```

`spawn_ordinary_app`, `make_kernel`, `register_process`, `RegisterArgs`, `initial::INIT` are all already imported by `tests/sys.rs` (line 46-58). `WaitTarget` is already re-exported from `kernel::sys` (line 55 top-of-file use). `kernel::sys::pack_exit_status_public` is a new public wrapper Task 3 adds (exposes the `ext.rs` `pack_exit_status` helper for tests). `k.parked_waiters_get_public(pid)` is a new test helper Task 3 adds.

- [ ] **Step 2: Append tests 3 + 4 (WNOHANG preserves EAGAIN; second wait returns EAGAIN)**

Immediately after the second test from Step 1, append:

```rust
#[test]
fn wnohang_preserves_eagain() {
    // Parent with a live non-zombie child calls proc_wait with
    // WNOHANG. The handler must NOT park; it returns EAGAIN
    // synchronously. Parent stays Running; parked_waiters stays
    // empty.
    let mut k = make_kernel();
    let parent = k
        .register_process(RegisterArgs {
            name: "parent",
            ppid: 0,
            caps: initial::INIT,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(parent).unwrap();
    k.procs
        .transition(parent, kernel::proc::ProcState::Running)
        .unwrap();
    let _child = spawn_ordinary_app(&mut k, parent, "child");

    // Direct kernel call — no dispatcher required. proc_wait
    // returns WouldBlock when a live child exists but no zombie;
    // that's the kernel-level precondition the park path
    // consumes. With WNOHANG the handler layer turns that into
    // EAGAIN; this test pins the kernel-level invariant that
    // park_on_wait is NOT called on the WNOHANG path.
    let outcome = k.proc_wait(parent, kernel::sys::WaitTarget::Any).unwrap();
    assert!(matches!(outcome, kernel::sys::WaitOutcome::WouldBlock));

    // Parent stays Running; no parker slot.
    let proc = k.procs.get(parent).unwrap();
    assert_eq!(proc.state, kernel::proc::ProcState::Running);
    assert!(proc.block_reason.is_none());
    assert!(k.parked_waiters_get_public(parent).is_none());
    assert!(k.pending_wakes_is_empty());
}

#[test]
fn second_wait_on_parked_parent_returns_eagain() {
    // v1 invariant: at most one parker per parent. A second
    // park_on_wait against an already-parked pid returns
    // WouldBlock (the kernel-level precondition; the handler
    // layer turns it into EAGAIN).
    let mut k = make_kernel();
    let parent = k
        .register_process(RegisterArgs {
            name: "parent",
            ppid: 0,
            caps: initial::INIT,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(parent).unwrap();
    k.procs
        .transition(parent, kernel::proc::ProcState::Running)
        .unwrap();
    let _child = spawn_ordinary_app(&mut k, parent, "child");

    // First park lands.
    k.park_on_wait(
        parent,
        1,
        kernel::sys::WaitTarget::Any,
        0,
        0,
    )
    .unwrap();

    // Second park against the same parent must fail with
    // WouldBlock. Original parker's req_id is preserved.
    let err = k
        .park_on_wait(parent, 2, kernel::sys::WaitTarget::Any, 0, 0)
        .unwrap_err();
    assert_eq!(err, kernel::sys::KernelError::WouldBlock);

    // Original parker unchanged.
    let parker = k.parked_waiters_get_public(parent).unwrap();
    assert_eq!(parker.req_id, 1);

    // Parent stays BlockedOnWait.
    let proc = k.procs.get(parent).unwrap();
    assert_eq!(proc.state, kernel::proc::ProcState::BlockedOnWait);
}
```

- [ ] **Step 3: Append test 5 (SIGTERM interrupts parked wait with EINTR)**

Immediately after the previous step's tests, append:

```rust
#[test]
fn sigterm_interrupts_parked_wait_with_eintr() {
    // Parent parks on wait; grandparent sends SIGTERM to parent.
    // Mirror of slice 2b's sigterm_interrupts_parked_accept_with_
    // eintr: the handler-layer Term arm calls BOTH
    // interrupt_parked_accept AND interrupt_parked_wait; one of
    // the two fires (the pid parks on at most one primitive at a
    // time).
    let mut k = make_kernel();

    // Grandparent (init) holds INIT caps so it can spawn a parent
    // and later signal it.
    let grandparent = k
        .register_process(RegisterArgs {
            name: "init",
            ppid: 0,
            caps: initial::INIT,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(grandparent).unwrap();
    k.procs
        .transition(grandparent, kernel::proc::ProcState::Running)
        .unwrap();

    // Parent is spawned as a child of grandparent (so the cap
    // check in proc_kill passes via the is_parent arm).
    let parent = spawn_ordinary_app(&mut k, grandparent, "parent");
    k.procs
        .transition(parent, kernel::proc::ProcState::Running)
        .unwrap();

    // Parent has a live child of its own (the child the parent
    // parks waiting on).
    let child = spawn_ordinary_app(&mut k, parent, "child");

    // Parent parks on wait.
    let req_id = 0xe171u32;
    k.park_on_wait(
        parent,
        req_id,
        kernel::sys::WaitTarget::Any,
        0,
        0,
    )
    .unwrap();
    assert_eq!(
        k.procs.get(parent).unwrap().state,
        kernel::proc::ProcState::BlockedOnWait,
    );

    // Grandparent sends SIGTERM to parent. This must do two things:
    //   (a) post Signal::Term to parent's signal inbox
    //   (b) wake the parked wait with -EINTR
    k.proc_kill(grandparent, parent, kernel::proc::Signal::Term)
        .unwrap();
    let interrupted = k.interrupt_parked_wait(parent);
    assert!(interrupted);

    // Parent is Ready; block_reason cleared.
    let proc = k.procs.get(parent).unwrap();
    assert_eq!(proc.state, kernel::proc::ProcState::Ready);
    assert!(proc.block_reason.is_none());
    assert!(k.parked_waiters_get_public(parent).is_none());

    // Signal inbox has Term queued.
    assert_eq!(k.pending_signals(parent).unwrap(), 1);

    // Exactly one wake queued: (parent, err(req_id, EINTR), no heap).
    let wakes = k.pending_wakes_snapshot();
    assert_eq!(wakes.len(), 1);
    let (wake_pid, wake_resp, wake_heap) = &wakes[0];
    assert_eq!(*wake_pid, parent);
    assert_eq!(wake_resp.request_id, req_id);
    assert_eq!(wake_resp.status, abi::errno::EINTR);
    assert_eq!(wake_resp.extra_len, 0);
    assert!(wake_heap.is_none());

    // The child is still alive, not reaped — EINTR fires before
    // any reap happens.
    assert!(k.procs.is_alive(child));
}
```

`k.interrupt_parked_wait(pid) -> bool` is the new method Task 3 adds (mirror of `interrupt_parked_accept`). The test exercises it directly rather than going through `handle_proc_kill` because Task 3's handler wiring calls both methods; asserting the kernel-level shape is the isolation-test point.

- [ ] **Step 4: Append test 6 (SIGKILL on parked parent exits without EINTR wake)**

Immediately after test 5, append:

```rust
#[test]
fn sigkill_on_parked_wait_exits_without_eintr_wake() {
    // SIGKILL is non-catchable. `Kernel::proc_kill`'s Signal::Kill
    // arm calls procs.exit synchronously with
    // ExitStatus::Signaled(9), bypassing proc_exit entirely. The
    // arm must sweep parked_waiters for the target pid (new in
    // 2c.1, mirror of 2b's ipc.clear_parked_acceptor_for_pid call)
    // but MUST NOT queue an EINTR wake (the pid is dead, not
    // interrupted). Mirror of
    // sigkill_on_parked_accept_exits_without_eintr_wake.
    let mut k = make_kernel();

    let grandparent = k
        .register_process(RegisterArgs {
            name: "init",
            ppid: 0,
            caps: initial::INIT,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(grandparent).unwrap();
    k.procs
        .transition(grandparent, kernel::proc::ProcState::Running)
        .unwrap();

    let parent = spawn_ordinary_app(&mut k, grandparent, "parent");
    k.procs
        .transition(parent, kernel::proc::ProcState::Running)
        .unwrap();
    let child = spawn_ordinary_app(&mut k, parent, "child");

    // Parent parks on wait.
    let req_id = 0x9c1du32;
    k.park_on_wait(
        parent,
        req_id,
        kernel::sys::WaitTarget::Any,
        0,
        0,
    )
    .unwrap();

    // Grandparent SIGKILLs parent.
    k.proc_kill(grandparent, parent, kernel::proc::Signal::Kill)
        .unwrap();

    // Parent is zombie with Signaled(9).
    let proc = k.procs.get(parent).unwrap();
    assert_eq!(proc.state, kernel::proc::ProcState::Zombie);
    assert!(matches!(
        proc.exit_status,
        Some(kernel::proc::ExitStatus::Signaled(9)),
    ));

    // parked_waiters swept clean by the Signal::Kill arm's
    // surgical parked_waiters.remove call.
    assert!(k.parked_waiters_get_public(parent).is_none());

    // No EINTR wake queued — the pid is dead, not interrupted.
    // NB: grandparent may have received SIGCHLD via post_sigchld,
    // which does NOT queue anything on pending_wakes (the
    // grandparent is not parked on wait).
    assert!(k.pending_wakes_is_empty());

    // The child is still alive, not reaped (parent didn't reach
    // the reap path — it was killed before any wake fired).
    assert!(k.procs.is_alive(child));
}
```

- [ ] **Step 5: Append test 7 (specific-target wake matches only the specific child)**

Immediately after test 6, append:

```rust
#[test]
fn specific_target_wake_only_matches_specific_child() {
    // Parent with two live children (A and B). Parks on
    // Specific(B). A exits first. Because A doesn't match the
    // park's target, no wake fires and the parent keeps parking;
    // A becomes a regular zombie that a later non-blocking
    // PROC_WAIT could reap. Then B exits. Now the park's target
    // matches; the wake fires + B is reaped.
    let mut k = make_kernel();
    let parent = k
        .register_process(RegisterArgs {
            name: "parent",
            ppid: 0,
            caps: initial::INIT,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(parent).unwrap();
    k.procs
        .transition(parent, kernel::proc::ProcState::Running)
        .unwrap();
    let child_a = spawn_ordinary_app(&mut k, parent, "child_a");
    let child_b = spawn_ordinary_app(&mut k, parent, "child_b");

    // Park specifically on child_b.
    let req_id = 0xb0b0u32;
    k.park_on_wait(
        parent,
        req_id,
        kernel::sys::WaitTarget::Specific(child_b),
        0,
        4,
    )
    .unwrap();

    // A exits first — doesn't match. Parent keeps parking; no
    // wake; A transitions to Zombie but is NOT reaped (reap only
    // happens when the park's target matches).
    k.proc_exit(child_a, kernel::proc::ExitStatus::Exited(1))
        .unwrap();
    assert_eq!(
        k.procs.get(parent).unwrap().state,
        kernel::proc::ProcState::BlockedOnWait,
    );
    assert!(k.parked_waiters_get_public(parent).is_some());
    assert!(k.pending_wakes_is_empty());
    // A is zombie, NOT reaped.
    assert_eq!(
        k.procs.get(child_a).unwrap().state,
        kernel::proc::ProcState::Zombie,
    );

    // B exits — matches the park's Specific(child_b) target.
    k.proc_exit(child_b, kernel::proc::ExitStatus::Exited(7))
        .unwrap();

    // Parent is Ready; parker cleared.
    assert_eq!(
        k.procs.get(parent).unwrap().state,
        kernel::proc::ProcState::Ready,
    );
    assert!(k.parked_waiters_get_public(parent).is_none());

    // B is fully reaped.
    assert!(!k.procs.is_alive(child_b));

    // Exactly one wake queued, for parent, carrying B's packed
    // status + B's pid in the heap bytes.
    let wakes = k.pending_wakes_snapshot();
    assert_eq!(wakes.len(), 1);
    let (wake_pid, wake_resp, wake_heap) = &wakes[0];
    assert_eq!(*wake_pid, parent);
    assert_eq!(wake_resp.request_id, req_id);
    assert_eq!(wake_resp.status, 0);
    assert_eq!(wake_resp.extra_len, 4);
    let heap = wake_heap.as_ref().expect("wake has heap payload");
    let decoded = u32::from_le_bytes([
        heap.bytes[0],
        heap.bytes[1],
        heap.bytes[2],
        heap.bytes[3],
    ]);
    assert_eq!(decoded, child_b as u32);

    // A is still a zombie — a later non-blocking PROC_WAIT(WNOHANG)
    // with WaitTarget::Any would reap it.
    assert_eq!(
        k.procs.get(child_a).unwrap().state,
        kernel::proc::ProcState::Zombie,
    );
}
```

- [ ] **Step 6: Append test 8 (parent-exit sweeps parked_waiters slot)**

Immediately after test 7, append:

```rust
#[test]
fn parent_exit_clears_parked_waiter_slot() {
    // A parent parks on wait, then exits directly via proc_exit
    // (simulating a Worker-crash path observed by the host). The
    // proc_exit call must sweep parked_waiters for the exiting
    // pid — mirror of 2a's ipc.clear_parked_acceptor_for_pid
    // sweep at the top of proc_exit. No spurious wake queued.
    let mut k = make_kernel();

    let grandparent = k
        .register_process(RegisterArgs {
            name: "init",
            ppid: 0,
            caps: initial::INIT,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(grandparent).unwrap();
    k.procs
        .transition(grandparent, kernel::proc::ProcState::Running)
        .unwrap();

    let parent = spawn_ordinary_app(&mut k, grandparent, "parent");
    k.procs
        .transition(parent, kernel::proc::ProcState::Running)
        .unwrap();
    let _child = spawn_ordinary_app(&mut k, parent, "child");

    // Parent parks.
    k.park_on_wait(
        parent,
        0xdeadu32,
        kernel::sys::WaitTarget::Any,
        0,
        0,
    )
    .unwrap();
    assert!(k.parked_waiters_get_public(parent).is_some());

    // Parent exits directly with Crashed (simulates host-side
    // Worker crash observer calling proc_exit on the crashed pid).
    k.proc_exit(parent, kernel::proc::ExitStatus::Crashed)
        .unwrap();

    // parked_waiters is empty — the sweep ran.
    assert!(k.parked_waiters_get_public(parent).is_none());

    // No spurious wake queued. (The parent is Zombie now — a wake
    // against a zombie pid would be dropped by the drainer anyway,
    // but the invariant is cleaner if we never queue it.)
    assert!(k.pending_wakes_is_empty());
}
```

- [ ] **Step 7: Run the Rust tests to confirm they fail**

Run:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cd /opt/webos && cargo test --features native-platform -p kernel --test sys proc_wait 2>&1 | tail -40
cd /opt/webos && cargo test --features native-platform -p kernel --test sys sigterm_interrupts_parked_wait 2>&1 | tail -20
cd /opt/webos && cargo test --features native-platform -p kernel --test sys sigkill_on_parked_wait 2>&1 | tail -20
cd /opt/webos && cargo test --features native-platform -p kernel --test sys specific_target_wake 2>&1 | tail -20
cd /opt/webos && cargo test --features native-platform -p kernel --test sys parent_exit_clears_parked_waiter 2>&1 | tail -20
```

Expected: compile errors. The tests reference symbols that don't yet exist — `Kernel::park_on_wait`, `Kernel::interrupt_parked_wait`, `Kernel::parked_waiters_get_public`, `Kernel::pack_exit_status_public`, `WaitParker`, the `(Pid, Response, WakeHeap)` shape in `pending_wakes_snapshot`. These are the API surface Task 3 adds.

Do NOT commit yet. Red tests + impl land in the Task 4 commit.

## Task 2 — Write failing TS dispatcher tests (red step)

Three new tests in a new `describe("dispatch: PROC_WAIT blocking")` block placed immediately after the existing `dispatch: PROC_WAIT` block (at `web/tests/unit/kernel-wasm-host.test.ts:4785`, closing around line 4839). Analogous shape to slice 2a's `dispatch: IPC_ACCEPT blocking` block.

**Files:**
- Modify: `web/tests/unit/kernel-wasm-host.test.ts`

- [ ] **Step 1: Append the new describe block**

In `web/tests/unit/kernel-wasm-host.test.ts`, locate the closing `});` of the existing `describe("dispatch: PROC_WAIT", ...)` block at line 4839. Immediately after it (before the `// ---- dispatch: PROC_CAPS_GET ---` header at line 4841), insert:

```typescript
// ---- dispatch: PROC_WAIT blocking ----------------------------------
//
// Slice 2c.1. When options=0 and a matching live child exists but
// no zombie, handle_proc_wait returns ServiceOutcome::Parked (no
// response push). A later child-exit transitions the parent Ready
// and queues the wake via kernel.pending_wakes; drainWakesForPid
// copies the wake response + heap bytes (child pid as u32 LE) onto
// the parent's SAB.

describe("dispatch: PROC_WAIT blocking", () => {
  it("options=0 on live child parks caller (no response push)", async () => {
    const { host } = await freshHost();

    // Parent holds CAPSET_ALL. Spawn a child with CAP_SPAWN via
    // PROC_SPAWN so the kernel's proc_wait sees a live child.
    const parent = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(parent, 0);
    host.installConsoleFd(parent, 1);
    host.installConsoleFd(parent, 2);
    host.markRunning(parent);

    // Spawn a child via the kernel's own register_process path —
    // the test-only `spawnChildForTest` helper mirrors what Rust's
    // spawn_ordinary_app does, installing a child pid under the
    // parent with ORDINARY_APP caps + console stdio.
    const child = host.spawnChildForTest(parent, "child");
    expect(child).toBeGreaterThan(parent);

    // Dispatch PROC_WAIT(target=-1, options=0). Because the child
    // is live and non-zombie, the handler parks the parent.
    const waitArgs = new Uint8Array(16);
    const waitView = new DataView(waitArgs.buffer);
    waitView.setInt32(0, -1, true);
    waitView.setUint32(4, 0, true);
    const waitResult = host.dispatch(parent, {
      opcode: OP_EXT.PROC_WAIT,
      requestId: 0x2c01,
      args: waitArgs,
      heapPtr: 0,
      heapLen: 4,
    });

    // Parked: no response, parked flag true.
    expect(waitResult.parked).toBe(true);
    expect(waitResult.response).toBeUndefined();

    // No wake queued yet.
    const wake = host.takeNextWakeForPid(parent);
    expect(wake).toBeNull();
  });

  it("child exit wakes parked parent with packed status + pid heap readback", async () => {
    const { host } = await freshHost();

    const parent = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(parent, 0);
    host.installConsoleFd(parent, 1);
    host.installConsoleFd(parent, 2);
    host.markRunning(parent);

    const child = host.spawnChildForTest(parent, "child");
    host.markRunning(child);

    // Park the parent with heap_len = 4 so the wake carries the
    // reaped child's pid in the heap bytes.
    const waitArgs = new Uint8Array(16);
    const waitView = new DataView(waitArgs.buffer);
    waitView.setInt32(0, -1, true);
    waitView.setUint32(4, 0, true);
    const waitResult = host.dispatch(parent, {
      opcode: OP_EXT.PROC_WAIT,
      requestId: 0x2c02,
      args: waitArgs,
      heapPtr: 0,
      heapLen: 4,
    });
    expect(waitResult.parked).toBe(true);

    // Child calls PROC_EXIT(0). This transitions the child to
    // Zombie; the kernel's wake_parked_waiter_for_child helper
    // runs during proc_exit, reaps the child inline, queues the
    // wake.
    const exitArgs = new Uint8Array(16);
    new DataView(exitArgs.buffer).setInt32(0, 0, true);
    const exitResult = host.dispatch(child, {
      opcode: OP_WASI.PROC_EXIT,
      requestId: 0x2c03,
      args: exitArgs,
    });
    // proc_exit produces no response (the exiting pid is gone);
    // the wake for the PARENT is queued on pending_wakes.
    expect(exitResult.parked).toBeUndefined();

    // Take the wake for the parent — the response carries the
    // packed exit status. WaitOutcome::Reaped with Exited(0)
    // packs to bits 40..48 = 0x01 (Exited flag), bits 0..32 = 0
    // (the exit code): the u64 value is 0x0000_0100_0000_0000.
    const wake = host.takeNextWakeForPidWithHeap(parent);
    expect(wake).not.toBeNull();
    expect(wake!.response.requestId).toBe(0x2c02);
    expect(wake!.response.status).toBe(0);
    // Packed status: Exited(0) — flag 0x01 at bits 40..48, rest 0.
    const expectedValue = BigInt(0x01) << BigInt(40);
    expect(wake!.response.value).toBe(expectedValue);
    // extra_len = 4 with the child pid.
    expect(wake!.response.extraLen).toBe(4);
    expect(wake!.heapBytes).not.toBeNull();
    expect(wake!.heapBytes!.byteLength).toBe(4);
    const heapView = new DataView(
      wake!.heapBytes!.buffer,
      wake!.heapBytes!.byteOffset,
      4,
    );
    expect(heapView.getUint32(0, true)).toBe(child);

    // A second take returns null — queue drained.
    expect(host.takeNextWakeForPidWithHeap(parent)).toBeNull();
  });

  it("SIGTERM wakes parked parent with EINTR", async () => {
    const { host } = await freshHost();

    // Grandparent registers with CAPSET_ALL so it holds
    // ProcKillAny; parent is spawned as a child of grandparent so
    // the is_parent cap-check arm applies to the PROC_KILL.
    const grandparent = host.registerProcess(CAPSET_ALL);
    host.installConsoleFd(grandparent, 0);
    host.installConsoleFd(grandparent, 1);
    host.installConsoleFd(grandparent, 2);
    host.markRunning(grandparent);

    const parent = host.spawnChildForTest(grandparent, "parent");
    host.markRunning(parent);

    // Parent needs a child of its own so the proc_wait doesn't
    // hit the NoChildren → ECHILD early-reject.
    const _child = host.spawnChildForTest(parent, "child");

    // Parent parks on PROC_WAIT(target=-1, options=0).
    const waitArgs = new Uint8Array(16);
    const waitView = new DataView(waitArgs.buffer);
    waitView.setInt32(0, -1, true);
    waitView.setUint32(4, 0, true);
    const waitResult = host.dispatch(parent, {
      opcode: OP_EXT.PROC_WAIT,
      requestId: 0x2c04,
      args: waitArgs,
      heapPtr: 0,
      heapLen: 0,
    });
    expect(waitResult.parked).toBe(true);

    // Grandparent sends SIGTERM (signum=15) to parent.
    // PROC_KILL args: args[0..4] = target_pid (i32),
    // args[4..6] = signum (u16).
    const killArgs = new Uint8Array(16);
    const killView = new DataView(killArgs.buffer);
    killView.setInt32(0, parent, true);
    killView.setUint16(4, 15, true);
    const killResult = host.dispatch(grandparent, {
      opcode: OP_EXT.PROC_KILL,
      requestId: 0x2c05,
      args: killArgs,
    });
    expect(killResult.response).toBeDefined();
    expect(killResult.response!.status).toBe(0);

    // Take the wake for the parent. status === -EINTR, request_id
    // matches the parked PROC_WAIT's request_id. extra_len === 0
    // (EINTR wakes carry no heap payload).
    const wake = host.takeNextWakeForPid(parent);
    expect(wake).not.toBeNull();
    expect(wake!.requestId).toBe(0x2c04);
    expect(wake!.status).toBe(-ERRNO.EINTR);
    expect(wake!.extraLen).toBe(0);

    // A second take returns null — queue drained.
    expect(host.takeNextWakeForPid(parent)).toBeNull();
  });
});
```

The tests reference three test-only helpers:

- `host.spawnChildForTest(parentPid, name): number` — wraps the kernel-side `proc_spawn` with `ORDINARY_APP` caps + console stdio, returns the child pid. Analogous to Rust's `spawn_ordinary_app`. Added in Task 3 Step 8 to `KernelWasmHost`.
- `host.takeNextWakeForPidWithHeap(pid): { response, heapBytes } | null` — extends the existing `takeNextWakeForPid` to also return any heap bytes associated with the wake (for `extra_len > 0` assertions). Added in Task 3 Step 7.
- `OP_WASI.PROC_EXIT` — the WASI opcode 0x0060 (defined at `web/src/shared/syscall.ts:249`), already imported by the test file's top-of-file `import { OP_EXT, OP_WASI, ... }` block at line 55.

- [ ] **Step 2: Run vitest to confirm the tests fail**

Run:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cd /opt/webos/web && npx vitest run tests/unit/kernel-wasm-host.test.ts -t "PROC_WAIT blocking" 2>&1 | tail -40
```

Expected: FAIL. Likely errors: `TypeError: host.spawnChildForTest is not a function`, `TypeError: host.takeNextWakeForPidWithHeap is not a function`, or the dispatch-return's `.parked` field missing on PROC_WAIT. The tests fail cleanly — implementation lands in Task 3 (Rust) + Task 4 (TS + drainer extension).

Do NOT commit yet.

## Task 3 — Implement kernel park / wake / interrupt logic + drainer extension

The meat of the slice. Adds `WaitParker` type, `Kernel.parked_waiters` field, `Kernel::park_on_wait`, `Kernel::interrupt_parked_wait`, the shared `Kernel::wake_parked_waiter_for_child` helper, extends `Kernel::proc_exit` + `Kernel::proc_kill` (both arms) to invoke the helper + sweep the parker slot, rewrites `handle_proc_wait` to return `ServiceOutcome`, extends `handle_proc_kill`'s `Signal::Term` arm, and extends the kernel WASM export + TS drainer with `extra_len` support.

**Files:**
- Modify: `crates/kernel/src/sys.rs`
- Modify: `crates/kernel/src/syscall/ext.rs`
- Modify: `crates/kernel/src/wasm_entry.rs`
- Modify: `web/src/kernel-wasm-host.ts`

- [ ] **Step 1: Add `WaitParker` type + `parked_waiters` field + `pending_wakes` shape change**

In `crates/kernel/src/sys.rs`, locate the `pub struct Kernel` definition. Find the existing `pub(crate) pending_wakes: alloc::vec::Vec<(Pid, abi::ring::Response)>,` field (added in slice 2a). Change its type to carry optional heap bytes:

```rust
    /// Responses queued for pids that were parked on a blocking
    /// syscall and have since been unblocked. Drained per-pid by
    /// the dispatcher via `kernel_take_next_wake_for_pid`, which
    /// writes each entry's 32-byte Response into `RESP_SCRATCH`
    /// AND any heap payload into `HEAP_SCRATCH[0..extra_len]`,
    /// surfacing the user's original `heap_ptr` via the companion
    /// `kernel_resp_heap_ptr` export so the TS drainer can write
    /// the heap bytes back into the user's SAB heap scratch.
    ///
    /// `WakeHeap = None` for wakes that don't need a heap readback
    /// (every slice-2a/2b producer). Slice 2c.1's parked-wait wake
    /// sets it to `Some(PendingHeap { heap_ptr, bytes })` with a
    /// 4-byte reaped-child-pid payload when the parker recorded
    /// `heap_len >= 4`.
    pub(crate) pending_wakes: alloc::vec::Vec<(Pid, abi::ring::Response, WakeHeap)>,
```

Add the supporting types immediately above the `impl Kernel` block (after the `Kernel` struct definition and before the first `impl Kernel`):

```rust
/// Optional heap payload attached to a pending wake. Slice 2c.1.
pub(crate) type WakeHeap = Option<PendingHeap>;

/// Heap bytes the kernel wants the TS drainer to copy into the
/// parker's SAB heap scratch at `heap_ptr`. `bytes` is at most
/// `HEAP_SCRATCH_SIZE` in length; in practice for 2c.1 it's
/// always 4 bytes (the reaped child pid as u32 LE), but the type
/// is general to admit larger heap readbacks in future slices
/// without another shape change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingHeap {
    pub heap_ptr: u32,
    pub bytes: alloc::vec::Vec<u8>,
}

/// Parked-waiter record. One entry per parked parent in
/// `Kernel.parked_waiters`. Constructed by
/// [`Kernel::park_on_wait`]; consumed by child-exit wake paths +
/// [`Kernel::interrupt_parked_wait`] + the SIGKILL arm's surgical
/// `parked_waiters.remove` call.
///
/// `target` determines which child-exit events this parker
/// responds to: `Any` matches every child of the parked pid;
/// `Specific(p)` matches only child pid `p`.
///
/// `heap_ptr` + `heap_len` are captured at park time so the wake
/// path knows where in the user's SAB heap scratch to write the
/// 4-byte reaped-child-pid readback. `heap_len >= 4` means the
/// wake emits `Response.extra_len = 4`; otherwise the wake is
/// status-only.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WaitParker {
    pub req_id: u32,
    pub target: WaitTarget,
    pub heap_ptr: u32,
    pub heap_len: u32,
}
```

Then in the `pub struct Kernel` body (anywhere near the existing `pending_wakes` field), add:

```rust
    /// Parents parked on a blocking `proc_wait`. Keyed by parent
    /// pid so the child-exit wake path does an O(log n) lookup on
    /// `ppid`.
    ///
    /// v1 invariant: at most one parker per parent. A second
    /// blocking `proc_wait` from a parent that's already parked
    /// returns -EAGAIN regardless of WNOHANG (see design §3.1).
    /// POSIX allows reentrant waits from multiple threads sharing
    /// a pid; PMos v1 rejects them. Future slice can lift this if
    /// a multi-threaded-pid arc lands.
    pub(crate) parked_waiters: alloc::collections::BTreeMap<Pid, WaitParker>,
```

Then in `Kernel::new` (the constructor near the top of the `impl Kernel` block), add the initialiser alongside the existing `pending_wakes: alloc::vec::Vec::new(),`:

```rust
            parked_waiters: alloc::collections::BTreeMap::new(),
```

The existing `pending_wakes: alloc::vec::Vec::new(),` initialiser stays — the shape change propagates because `Vec::new()` is polymorphic.

- [ ] **Step 2: Update the existing `pending_wakes` producers to emit `WakeHeap::None`**

Every existing producer of `pending_wakes` must change from `.push((pid, resp))` to `.push((pid, resp, None))` because the Vec's element type is now a 3-tuple. Find the three producers:

1. `Kernel::wake_parked_acceptor_if_any` (in `sys.rs`, the 2a helper) — the existing `self.pending_wakes.push((acceptor_pid, wake_resp));` line. Change to:

```rust
        self.pending_wakes.push((acceptor_pid, wake_resp, None));
```

2. `Kernel::release_object`'s `FdObject::Socket` arm (in `sys.rs`, the 2a listener-close-drain path) — the existing `self.pending_wakes.push((parker_pid, Response::err(req_id, abi::errno::EBADF)));` line. Change to:

```rust
                    self.pending_wakes.push((
                        parker_pid,
                        abi::ring::Response::err(req_id, abi::errno::EBADF),
                        None,
                    ));
```

3. `Kernel::interrupt_parked_accept` (in `sys.rs`, the 2b method) — the existing `self.pending_wakes.push((pid, wake_resp));` line. Change to:

```rust
        self.pending_wakes.push((pid, wake_resp, None));
```

Use the `Grep` tool to find each `pending_wakes.push(` call in `sys.rs` before editing; there should be exactly three today (2a's two + 2b's one) plus the one this slice adds in Step 4.

- [ ] **Step 3: Update existing test helpers + `pack_exit_status` visibility**

The existing `pending_wakes_snapshot` test helper returns `Vec<(Pid, Response)>`; update to the new shape. In `crates/kernel/src/sys.rs`, locate the existing helper (added in slice 2a around the `impl Kernel` test-helper block):

```rust
    /// Test helper: clone of `pending_wakes` for assertions.
    #[doc(hidden)]
    pub fn pending_wakes_snapshot(
        &self,
    ) -> alloc::vec::Vec<(Pid, abi::ring::Response, WakeHeap)> {
        self.pending_wakes.clone()
    }
```

Add two new test helpers immediately below:

```rust
    /// Test helper: look up a parker by parent pid.
    #[doc(hidden)]
    pub fn parked_waiters_get_public(&self, pid: Pid) -> Option<WaitParker> {
        self.parked_waiters.get(&pid).copied()
    }

    /// Test helper: expose the `ext.rs`-private `pack_exit_status`
    /// helper so tests can build expected-value assertions
    /// identical to what the synchronous-reap path produces.
    #[doc(hidden)]
    pub fn pack_exit_status_public(status: crate::proc::ExitStatus) -> i64 {
        crate::syscall::ext::pack_exit_status(status)
    }
```

The test helper `pack_exit_status_public` requires the `ext.rs`-local `pack_exit_status` to be `pub(crate)` instead of the current private visibility. In `crates/kernel/src/syscall/ext.rs`, locate the existing function at line 537:

```rust
fn pack_exit_status(status: ExitStatus) -> i64 {
```

Change to:

```rust
pub(crate) fn pack_exit_status(status: ExitStatus) -> i64 {
```

The function body stays verbatim (the existing bit-pack logic is what the wake path reuses — byte-for-byte identical encoding).

- [ ] **Step 4: Add `Kernel::park_on_wait`**

In `crates/kernel/src/sys.rs`, in the `impl Kernel` block, locate the existing `pub fn proc_wait` at line 1264. Immediately after its closing `}` (around line 1302), insert:

```rust
    /// Park `parent_pid` on a blocking `proc_wait`. Records the
    /// parker's request_id + target + heap_ptr + heap_len, then
    /// transitions the parent Running -> BlockedOnWait and sets
    /// `block_reason = BlockReason::Wait { pid }` where `pid =
    /// -1` for `WaitTarget::Any` or the specific child pid
    /// otherwise.
    ///
    /// Precondition: the handler layer has already called
    /// `Kernel::proc_wait` and observed `WaitOutcome::WouldBlock`
    /// (a live matching child exists but no zombie). This method
    /// does NOT re-check that precondition; calling it on a
    /// parent with no live matching child would leave it parked
    /// forever. The handler layer's branching (§3.4 of the
    /// design) is the single source of truth for the
    /// "should-park vs return-ECHILD" decision.
    ///
    /// Returns `WouldBlock` if `parent_pid` is already parked
    /// (v1 one-waiter-per-parent invariant). `NoSuchPid` if the
    /// state transition fails (e.g. the parent was reaped
    /// between the `proc_wait` check and this call — a
    /// harness-only race).
    pub fn park_on_wait(
        &mut self,
        parent_pid: Pid,
        req_id: u32,
        target: WaitTarget,
        heap_ptr: u32,
        heap_len: u32,
    ) -> Result<(), KernelError> {
        if self.parked_waiters.contains_key(&parent_pid) {
            return Err(KernelError::WouldBlock);
        }
        self.parked_waiters.insert(
            parent_pid,
            WaitParker { req_id, target, heap_ptr, heap_len },
        );
        self.procs
            .transition(parent_pid, ProcState::BlockedOnWait)
            .map_err(|_| {
                // Roll back the parker insertion on transition
                // failure so we don't leak a parker slot on a
                // now-dead parent.
                self.parked_waiters.remove(&parent_pid);
                KernelError::NoSuchPid
            })?;
        let reason_pid = match target {
            WaitTarget::Any => -1,
            WaitTarget::Specific(p) => p,
        };
        self.procs
            .set_block_reason(parent_pid, crate::proc::BlockReason::Wait { pid: reason_pid });
        Ok(())
    }
```

`WaitTarget` is already re-exported from `kernel::sys` (used by `Kernel::proc_wait`'s existing signature). `ProcState::BlockedOnWait` is already defined in `proc/mod.rs`. The `(Running, BlockedOnWait)` transition is already in the `TRANSITIONS` table (confirmed at `proc/mod.rs:217`).

- [ ] **Step 5: Add `Kernel::wake_parked_waiter_for_child` shared helper**

Immediately below `park_on_wait` from Step 4, insert:

```rust
    /// Companion to `park_on_wait`. Invoked from
    /// `Kernel::proc_exit` and `Kernel::proc_kill`'s Signal::Kill
    /// arm when a child transitions to Zombie. If the child's
    /// parent has a parked waiter whose target matches the child,
    /// this:
    ///
    ///   1. Reaps the child inline (releases its kernel-side
    ///      resources, removes it from procs).
    ///   2. Queues the wake on `pending_wakes` with the packed
    ///      exit status + (if the parker's heap_len >= 4) the
    ///      child pid in the heap bytes.
    ///   3. Transitions the parent Ready + clears block_reason.
    ///   4. Removes the parker slot from `parked_waiters`.
    ///
    /// Returns true iff a wake was fired. Production callers
    /// discard the bool; tests use it.
    ///
    /// If the parent has no parked waiter, or the parker's target
    /// doesn't match the child, this is a no-op — the child stays
    /// Zombie (to be reaped by a later non-blocking wait) and no
    /// wake is queued.
    pub(crate) fn wake_parked_waiter_for_child(
        &mut self,
        child_pid: Pid,
        ppid: Pid,
        status: ExitStatus,
    ) -> bool {
        // ppid == 0 = orphan; cannot have a parker.
        if ppid == 0 {
            return false;
        }
        // Target-match check: any parker whose target is Any
        // matches every child; Specific(p) matches only p.
        let matches = match self.parked_waiters.get(&ppid) {
            Some(p) => match p.target {
                WaitTarget::Any => true,
                WaitTarget::Specific(target_pid) => target_pid == child_pid,
            },
            None => return false,
        };
        if !matches {
            return false;
        }

        // Reap inline. The reap removes the child from procs +
        // releases its cap set. Failure shouldn't happen — the
        // caller has just transitioned the child Zombie — but we
        // treat a reap error as a no-op wake (the child's state
        // is inconsistent, but the parent is better off parked
        // than woken with an incorrect status).
        let reap_ok = self.reap(child_pid).is_ok();
        if !reap_ok {
            return false;
        }

        // Dequeue the parker + build the wake response.
        let parker = self
            .parked_waiters
            .remove(&ppid)
            .expect("parked_waiters entry vanished between get and remove");
        let packed = crate::syscall::ext::pack_exit_status(status);
        let mut resp = abi::ring::Response::ok(parker.req_id, packed);
        let heap = if parker.heap_len >= 4 {
            resp.extra_len = 4;
            Some(PendingHeap {
                heap_ptr: parker.heap_ptr,
                bytes: (child_pid as u32).to_le_bytes().to_vec(),
            })
        } else {
            None
        };
        self.pending_wakes.push((ppid, resp, heap));

        // Best-effort state transition. If the parent isn't in
        // BlockedOnWait for some reason (race with another wake
        // path), ignore the transition error — the wake is still
        // queued and the user Worker will observe it.
        let _ = self.procs.transition(ppid, ProcState::Ready);
        self.procs.clear_block_reason(ppid);
        true
    }
```

`ExitStatus` is already imported at top of `sys.rs` (used by `proc_exit`). `WaitTarget` is in scope. `Kernel::reap` is the existing method at `sys.rs:1034` (called by the synchronous `proc_wait` path at `sys.rs:1280`). `alloc::vec::Vec::to_vec` is available because `alloc::vec::Vec` is already imported.

- [ ] **Step 6: Add `Kernel::interrupt_parked_wait`**

Immediately below `wake_parked_waiter_for_child` from Step 5, insert:

```rust
    /// Interrupt any parked `proc_wait` on `pid` with `-EINTR`.
    /// Clears `parked_waiters[&pid]`, queues a wake with
    /// `Response::err(req_id, EINTR)` onto `pending_wakes`,
    /// transitions `pid` Ready + clears `block_reason`. No-op if
    /// `pid` is not parked on wait.
    ///
    /// Called from `handle_proc_kill`'s `Signal::Term` arm
    /// alongside the existing `interrupt_parked_accept` call. The
    /// two methods are idempotent against each other: at most one
    /// of them observes the parker slot (a pid parks on at most
    /// one primitive at a time in v1).
    ///
    /// Returns `true` iff a park was interrupted. Production
    /// callers discard the bool; tests use it.
    pub fn interrupt_parked_wait(&mut self, pid: Pid) -> bool {
        let Some(parker) = self.parked_waiters.remove(&pid) else {
            return false;
        };
        let wake_resp = abi::ring::Response::err(parker.req_id, abi::errno::EINTR);
        self.pending_wakes.push((pid, wake_resp, None));
        // Best-effort state transition. If the pid isn't in
        // BlockedOnWait for some reason (race with another wake
        // path), ignore the transition error — the wake is still
        // queued and the user Worker will observe it.
        let _ = self.procs.transition(pid, ProcState::Ready);
        self.procs.clear_block_reason(pid);
        true
    }
```

- [ ] **Step 7: Wire `Kernel::proc_exit` to call the helper + sweep parked_waiters slot**

In `crates/kernel/src/sys.rs`, locate the existing `pub fn proc_exit` at line 1128. Its current body (after slice 2a) runs in this order:

```text
1. self.sched.remove(pid)
2. self.ipc.clear_parked_acceptor_for_pid(pid)
3. self.release_fd_table_resources(pid)
4. let ppid = self.procs.get(pid).map(|p| p.ppid).unwrap_or(0)
5. self.procs.exit(pid, status)
6. self.post_sigchld(ppid)
```

Add two changes:

(a) Immediately after step 2's `self.ipc.clear_parked_acceptor_for_pid(pid);`, add:

```rust
        // If the exiting pid is itself parked on a blocking
        // proc_wait, clear the slot so the exit-time sweep
        // mirrors the ipc_accept side.
        self.parked_waiters.remove(&pid);
```

(b) After step 6's `self.post_sigchld(ppid);` line (after the final semicolon, before the closing `Ok(())` of the function), add:

```rust
        // If the exiting pid's parent is parked on wait AND the
        // parker's target matches this child, reap + wake inline.
        // The helper is a no-op if either condition fails.
        self.wake_parked_waiter_for_child(pid, ppid, status);
```

Note: the call happens AFTER `self.procs.exit(pid, status)` transitions the child to Zombie, which is the precondition `wake_parked_waiter_for_child` assumes (it then calls `self.reap(pid)` which succeeds only against a Zombie). The `status` parameter is the same `status: ExitStatus` the function received.

- [ ] **Step 8: Wire `Kernel::proc_kill`'s `Signal::Kill` arm to call the helper + sweep parked_waiters slot**

In `crates/kernel/src/sys.rs`, locate `Kernel::proc_kill` at line 1314. Find the `Signal::Kill =>` arm starting around line 1348. Its current body runs:

```text
1. self.sched.remove(target_pid)
2. self.ipc.clear_parked_acceptor_for_pid(target_pid)   // 2b-added
3. let target_ppid = target.ppid                         // captured BEFORE exit
4. self.procs.exit(target_pid, ExitStatus::Signaled(signal.number()))
5. self.post_sigchld(target_ppid)
```

Add two changes:

(a) Immediately after step 2's `self.ipc.clear_parked_acceptor_for_pid(target_pid);`, add:

```rust
                // Same surgical sweep for parked_waiters. A
                // SIGKILL'd parent parked on wait is dead, not
                // interrupted — no EINTR wake is queued (that's
                // `interrupt_parked_wait`'s job from the
                // catchable-signal arm). This just clears the
                // stale slot.
                self.parked_waiters.remove(&target_pid);
```

(b) After step 5's `self.post_sigchld(target_ppid);` line (after the semicolon, before the closing `}` of the `Signal::Kill =>` arm), add:

```rust
                // SIGKILL'd target transitioned Zombie — if its
                // parent is parked on wait AND the parker's
                // target matches, reap + wake. Status is
                // Signaled(9) per the exit call above.
                self.wake_parked_waiter_for_child(
                    target_pid,
                    target_ppid,
                    ExitStatus::Signaled(signal.number()),
                );
```

- [ ] **Step 9: Wire `handle_proc_kill`'s `Signal::Term` arm to call `interrupt_parked_wait`**

In `crates/kernel/src/syscall/ext.rs`, locate `handle_proc_kill`. The existing Term arm (added by slice 2b) already calls `kernel.interrupt_parked_accept(target_pid)` after the `kernel.proc_kill(...)` call returns success. Find the exact location — grep for `interrupt_parked_accept` in the file:

```rust
        if signal == kernel::proc::Signal::Term {
            let _ = kernel.interrupt_parked_accept(target_pid);
        }
```

Extend the block to also call `interrupt_parked_wait`:

```rust
        if signal == kernel::proc::Signal::Term {
            let _ = kernel.interrupt_parked_accept(target_pid);
            let _ = kernel.interrupt_parked_wait(target_pid);
        }
```

Order doesn't matter (v1 pid parks on at most one primitive; at most one of the two methods returns true). Robustness: both being called is safe because each method's `take`-and-clear is idempotent.

- [ ] **Step 10: Rewrite `handle_proc_wait` to return `ServiceOutcome`**

In `crates/kernel/src/syscall/ext.rs`, locate `fn handle_proc_wait` at line 554. Replace the entire function (keeping the `fn pack_exit_status` above it untouched — its visibility was already bumped to `pub(crate)` in Step 3):

```rust
fn handle_proc_wait(
    kernel: &mut Kernel,
    pid: Pid,
    req: &Request,
    heap: &mut [u8],
) -> ServiceOutcome {
    let target_pid = i32::from_le_bytes([req.args[0], req.args[1], req.args[2], req.args[3]]);
    let options = args_u32(req, 4);
    if options & !abi::ext::wait_opts::WNOHANG != 0 {
        return ServiceOutcome::Done(Response::err(req.request_id, EINVAL));
    }
    if target_pid < -1 {
        return ServiceOutcome::Done(Response::err(req.request_id, EINVAL));
    }
    let target = if target_pid == 0 || target_pid == -1 {
        WaitTarget::Any
    } else {
        if target_pid == pid {
            return ServiceOutcome::Done(Response::err(req.request_id, ECHILD));
        }
        WaitTarget::Specific(target_pid)
    };
    let nohang = (options & abi::ext::wait_opts::WNOHANG) != 0;

    match kernel.proc_wait(pid, target) {
        Ok(WaitOutcome::Reaped(child, status)) => {
            // Synchronous-reap path — unchanged from pre-2c.1.
            let packed = pack_exit_status(status);
            let mut resp = Response::ok(req.request_id, packed);
            if (req.heap_len as usize) >= 4 {
                if let Some(out) = heap_out_mut(req, heap, 4) {
                    out[0..4].copy_from_slice(&(child as u32).to_le_bytes());
                    resp.extra_len = 4;
                }
            }
            ServiceOutcome::Done(resp)
        }
        Ok(WaitOutcome::WouldBlock) if nohang => {
            ServiceOutcome::Done(Response::err(req.request_id, EAGAIN))
        }
        Ok(WaitOutcome::WouldBlock) => {
            // Blocking path: park the caller. If park_on_wait
            // reports WouldBlock (one-waiter-per-parent
            // invariant), surface EAGAIN.
            match kernel.park_on_wait(pid, req.request_id, target, req.heap_ptr, req.heap_len) {
                Ok(()) => ServiceOutcome::Parked,
                Err(crate::sys::KernelError::WouldBlock) => {
                    ServiceOutcome::Done(Response::err(req.request_id, EAGAIN))
                }
                Err(e) => ServiceOutcome::Done(Response::err(
                    req.request_id,
                    kerr_to_errno(e),
                )),
            }
        }
        Ok(WaitOutcome::NoChildren) => {
            ServiceOutcome::Done(Response::err(req.request_id, ECHILD))
        }
        Err(e) => ServiceOutcome::Done(Response::err(req.request_id, kerr_to_errno(e))),
    }
}
```

The imports at the top of `ext.rs` already include `WaitOutcome` and `WaitTarget` (they appear in the existing body); `ServiceOutcome` is imported from slice 2a's edit; `EINVAL`, `EAGAIN`, `ECHILD` are imported from `abi::errno`. No new `use` lines required.

- [ ] **Step 11: Update `dispatch_ext`'s `PROC_WAIT` arm to pass through `ServiceOutcome`**

In `crates/kernel/src/syscall/ext.rs`, locate `fn dispatch_ext`. Find the `op::PROC_WAIT` arm (it currently wraps in `ServiceOutcome::Done(...)` because the old handler returned `Response`):

```rust
        op::PROC_WAIT => ServiceOutcome::Done(handle_proc_wait(kernel, pid, req, heap)),
```

Change to:

```rust
        op::PROC_WAIT => handle_proc_wait(kernel, pid, req, heap),
```

Mirror of 2a's `IPC_ACCEPT` wiring. The handler now returns `ServiceOutcome` directly.

- [ ] **Step 12: Extend `kernel_take_next_wake_for_pid` to surface heap bytes + heap_ptr**

In `crates/kernel/src/wasm_entry.rs`, locate the existing `kernel_take_next_wake_for_pid` at line 317. Replace it with:

```rust
/// Take the next pending wake for `pid` out of `Kernel.pending_wakes`,
/// write its 32-byte Response into RESP_SCRATCH, and if the entry
/// has a heap payload write it into `HEAP_SCRATCH[0..extra_len]`
/// AND record the user's original heap_ptr in `RESP_HEAP_PTR`
/// (readable via `kernel_resp_heap_ptr`). Returns 1 if an entry
/// was drained, 0 if nothing is queued for this pid.
#[no_mangle]
pub extern "C" fn kernel_take_next_wake_for_pid(pid: Pid) -> i32 {
    let kernel = kernel_mut();
    let idx = match kernel.pending_wakes.iter().position(|(p, _, _)| *p == pid) {
        Some(i) => i,
        None => return 0,
    };
    let (_, resp, heap) = kernel.pending_wakes.remove(idx);
    unsafe {
        RESP_SCRATCH = resp.to_le_bytes();
    }
    if let Some(h) = heap {
        // Copy heap bytes into HEAP_SCRATCH[0..len]. Capped at
        // HEAP_SCRATCH_SIZE — the TS drainer reads `resp.extra_len`
        // bytes, which the handler layer guaranteed <= heap_len <=
        // HEAP_SCRATCH_SIZE at park time.
        let len = h.bytes.len().min(HEAP_SCRATCH_SIZE);
        unsafe {
            HEAP_SCRATCH[..len].copy_from_slice(&h.bytes[..len]);
            RESP_HEAP_PTR = h.heap_ptr;
        }
    } else {
        unsafe {
            RESP_HEAP_PTR = 0;
        }
    }
    1
}

/// Pointer-equivalent getter: returns the user-SAB heap_ptr
/// recorded by the most recent `kernel_take_next_wake_for_pid`
/// call. Meaningful only when that call returned 1 AND the
/// response's `extra_len > 0`; otherwise zero.
#[no_mangle]
pub extern "C" fn kernel_resp_heap_ptr() -> u32 {
    unsafe { RESP_HEAP_PTR }
}
```

Then add the supporting static near `RESP_SCRATCH` at line 89. Insert immediately after the existing `static mut RESP_SCRATCH` declaration:

```rust
/// User-SAB heap_ptr associated with the most recent wake drained
/// via `kernel_take_next_wake_for_pid`. Readable by the TS side
/// via `kernel_resp_heap_ptr`. Zero when no heap bytes were
/// written. Slice 2c.1.
static mut RESP_HEAP_PTR: u32 = 0;
```

- [ ] **Step 13: Extend `KernelWasmHost` with `drainWakesForPid` heap writeback + new test helpers**

In `web/src/kernel-wasm-host.ts`, locate the exports interface (the one declaring `readonly kernel_take_next_wake_for_pid: (pid: number) => number;` at line 100). Add the new export's type alongside it:

```typescript
  readonly kernel_resp_heap_ptr: () => number;
```

Then locate `drainWakesForPid` at line 698. Replace the entire method body with a version that copies heap bytes back to the SAB:

```typescript
  drainWakesForPid(pid: number, sab: Uint8Array): number {
    if (sab.byteLength < SAB_SIZE) {
      throw new Error(
        `KernelWasmHost.drainWakesForPid: sab is ${sab.byteLength} bytes, need ${SAB_SIZE}`,
      );
    }
    const buffer = sab.buffer;
    const baseOffset = sab.byteOffset;
    const header = new Int32Array(buffer, baseOffset, OFF_HEAP_SCRATCH / 4);
    let pushed = 0;
    while (this.exports.kernel_take_next_wake_for_pid(pid) === 1) {
      const respPtr = this.exports.kernel_resp_ptr();
      const respBytes = new Uint8Array(
        new Uint8Array(this.exports.memory.buffer, respPtr, SLOT_SIZE),
      );
      const response = decodeResponse(respBytes);
      const resHead = Atomics.load(header, OFF_RES_HEAD / 4);
      const resTail = Atomics.load(header, OFF_RES_TAIL / 4);
      const nextResHead = ((resHead + 1) >>> 0) % RES_SLOT_COUNT;
      if (nextResHead === resTail) {
        throw new Error(
          `KernelWasmHost.drainWakesForPid: response ring full for pid ${pid}`,
        );
      }
      const resSlotIx = (resHead >>> 0) % RES_SLOT_COUNT;
      const resSlotOffset = baseOffset + OFF_RES_RING + resSlotIx * SLOT_SIZE;
      const encoded = encodeResponse(response);
      new Uint8Array(buffer, resSlotOffset, SLOT_SIZE).set(encoded);

      // Slice 2c.1: if the wake carries heap bytes (extra_len > 0),
      // read them from kernel HEAP_SCRATCH[0..extra_len] and copy
      // to the SAB's heap scratch at baseOffset + OFF_HEAP_SCRATCH
      // + heap_ptr. heap_ptr is surfaced by `kernel_resp_heap_ptr`.
      if (response.extraLen > 0) {
        const heapPtrInSab = this.exports.kernel_resp_heap_ptr();
        const heapPtrInKernel = this.exports.kernel_heap_ptr();
        const kernelHeap = new Uint8Array(
          this.exports.memory.buffer,
          heapPtrInKernel,
          response.extraLen,
        );
        const copy = new Uint8Array(kernelHeap);
        const sabHeapOffset = baseOffset + OFF_HEAP_SCRATCH + heapPtrInSab;
        new Uint8Array(buffer, sabHeapOffset, response.extraLen).set(copy);
      }

      Atomics.store(header, OFF_RES_HEAD / 4, nextResHead);
      pushed += 1;
    }
    return pushed;
  }
```

Then locate `takeNextWakeForPid` at line 677. It currently returns `SyscallResponse | null`. Add a companion method immediately below that also returns heap bytes for tests:

```typescript
  /**
   * Test-only: pop one wake for `pid` and return the decoded
   * Response PLUS any heap bytes the wake carries. Heap bytes
   * are cloned from kernel HEAP_SCRATCH[0..extra_len]; `heapBytes
   * === null` when `response.extraLen === 0`.
   *
   * Mirrors `takeNextWakeForPid` but surfaces the `extra_len`
   * payload that `drainWakesForPid` would copy back into the SAB.
   * Used by dispatcher tests that assert the reaped-child-pid
   * readback shape without a full SAB round-trip.
   */
  takeNextWakeForPidWithHeap(pid: number): {
    response: SyscallResponse;
    heapBytes: Uint8Array | null;
  } | null {
    if (this.exports.kernel_take_next_wake_for_pid(pid) !== 1) {
      return null;
    }
    const respPtr = this.exports.kernel_resp_ptr();
    const respBytes = new Uint8Array(
      new Uint8Array(this.exports.memory.buffer, respPtr, SLOT_SIZE),
    );
    const response = decodeResponse(respBytes);
    if (response.extraLen === 0) {
      return { response, heapBytes: null };
    }
    const heapPtrInKernel = this.exports.kernel_heap_ptr();
    const kernelHeap = new Uint8Array(
      this.exports.memory.buffer,
      heapPtrInKernel,
      response.extraLen,
    );
    const heapBytes = new Uint8Array(kernelHeap);
    return { response, heapBytes };
  }
```

Then add the `spawnChildForTest` helper. Locate the existing `registerProcess` / `markRunning` test-setup methods near the top of the `KernelWasmHost` class body. Add adjacent to them:

```typescript
  /**
   * Test-only: spawn a child process of `parent` with
   * ORDINARY_APP caps + console stdio, returning the child pid.
   * Mirror of the kernel-tests `spawn_ordinary_app` Rust helper.
   * Used by slice-2c.1 dispatcher tests to build parent/child
   * pairs for PROC_WAIT blocking scenarios.
   *
   * `parent` must already be registered + markedRunning; the
   * child is left in `Ready` state (the kernel's `proc_spawn`
   * auto-transitions children to `Ready`; the test harness may
   * bump to `Running` via `markRunning(child)` if the child
   * needs to dispatch its own syscalls).
   */
  spawnChildForTest(parent: number, name: string): number {
    const rc = this.exports.kernel_register_process_for_spawn(
      parent,
      this.encodeStringToHeap(name),
      name.length,
    );
    if (rc < 0) {
      throw new Error(
        `KernelWasmHost.spawnChildForTest: kernel_register_process_for_spawn returned ${rc}`,
      );
    }
    return rc;
  }
```

`kernel_register_process_for_spawn` is a new kernel-side test-only export. Add it to `crates/kernel/src/wasm_entry.rs` below the existing `kernel_register_process` at wherever registerProcess is defined — grep for `kernel_register_process` to find the exact location. Append:

```rust
/// Test-only: register a child process under `parent` with the
/// `ORDINARY_APP` cap set + console stdio, equivalent to the
/// Rust-level `spawn_ordinary_app` test helper but callable from
/// the TS side for dispatcher tests. Returns the child pid on
/// success, -1 on failure.
///
/// The name is read from `HEAP_SCRATCH[0..name_len]` as UTF-8 + ASCII.
/// Typical name_len is 8-16; shorter names are zero-padded by the
/// caller (irrelevant since the exact bytes are copied).
#[no_mangle]
pub extern "C" fn kernel_register_process_for_spawn(
    parent: Pid,
    _name_ptr: u32,
    name_len: u32,
) -> i32 {
    let kernel = kernel_mut();
    let name_bytes = unsafe { &HEAP_SCRATCH[..name_len as usize] };
    let name = match core::str::from_utf8(name_bytes) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    match kernel.proc_spawn(
        parent,
        kernel::sys::SpawnArgs {
            name,
            caps: abi::cap::initial::ORDINARY_APP,
            cwd: "/",
            argv: alloc::vec::Vec::new(),
            envp: alloc::collections::BTreeMap::new(),
            stdin: kernel::fd::FdObject::CharDevice(kernel::fs::devfs::DEV_CONSOLE),
            stdout: kernel::fd::FdObject::CharDevice(kernel::fs::devfs::DEV_CONSOLE),
            stderr: kernel::fd::FdObject::CharDevice(kernel::fs::devfs::DEV_CONSOLE),
        },
    ) {
        Ok(pid) => pid as i32,
        Err(_) => -1,
    }
}
```

Adjust the `name_ptr` parameter to be `_name_ptr` because the kernel reads from `HEAP_SCRATCH` at offset 0 by convention (mirror of how `kernel_inject_*_input` functions work at the same file — grep for `kernel_inject_console_input` for the existing pattern). The TS side writes the bytes into the heap via the existing `encodeStringToHeap` helper (which already exists on `KernelWasmHost` — grep `encodeStringToHeap` in `kernel-wasm-host.ts` to confirm; if missing, add a trivial copy-to-heap method).

Finally, add `kernel_register_process_for_spawn` to the exports interface:

```typescript
  readonly kernel_register_process_for_spawn: (
    parent: number,
    name_ptr: number,
    name_len: number,
  ) => number;
```

If `encodeStringToHeap` does NOT exist on `KernelWasmHost` (verify via grep), add this minimal helper adjacent to `spawnChildForTest`:

```typescript
  private encodeStringToHeap(s: string): number {
    const encoder = new TextEncoder();
    const bytes = encoder.encode(s);
    const heapCap = this.exports.kernel_heap_len();
    if (bytes.length > heapCap) {
      throw new Error(
        `KernelWasmHost.encodeStringToHeap: ${bytes.length} > heap capacity ${heapCap}`,
      );
    }
    const heapPtr = this.exports.kernel_heap_ptr();
    new Uint8Array(this.exports.memory.buffer, heapPtr, bytes.length).set(bytes);
    return 0;
  }
```

The return is `0` because the kernel's convention is to read heap payloads starting at offset 0 of `HEAP_SCRATCH`; the actual pointer the kernel sees is `kernel_heap_ptr() + 0`.

- [ ] **Step 14: Verify the workspace compiles end-to-end**

Run:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cd /opt/webos && cargo check --workspace 2>&1 | tail -30
cd /opt/webos/web && npx tsc --noEmit 2>&1 | tail -20
```

Expected: both compile cleanly. If a type error surfaces on a `pending_wakes.push` call not covered in Step 2, grep `pending_wakes.push` across `crates/kernel/src` and add the missing `, None` third tuple-element.

Do NOT commit yet. Tests pass in Task 4.

## Task 4 — Build, run all test layers, confirm green, commit

Bundle all source changes + regenerated `kernel.wasm` in one `feat` commit. TDD validation: Task 1 + Task 2's red tests now go green.

**Files staged:**
- `crates/kernel/src/sys.rs`
- `crates/kernel/src/syscall/ext.rs`
- `crates/kernel/src/wasm_entry.rs`
- `crates/kernel/tests/sys.rs`
- `web/src/kernel-wasm-host.ts`
- `web/tests/unit/kernel-wasm-host.test.ts`
- `dist/assets/kernel.wasm` (regenerated by `just build`)

- [ ] **Step 1: Ensure cargo on PATH**

Run:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo --version && just --version
```

- [ ] **Step 2: Build the full pipeline**

Run:

```bash
cd /opt/webos && just build 2>&1 | tail -20
```

Expected: success. `kernel.wasm` is rebuilt with the new exports (`kernel_resp_heap_ptr`, `kernel_register_process_for_spawn`, extended `kernel_take_next_wake_for_pid`). Every userland binary rebuilds too but is unchanged behaviourally.

If build fails, fix the surfaced error. Most likely class of failure: a `pending_wakes.push(X, Y)` call that was added in slice 2a or 2b but wasn't visible to Task 3 Step 2's grep scan. Find it with:

```bash
cd /opt/webos && grep -rn 'pending_wakes.push' crates/kernel/src 2>&1
```

Change any 2-tuple push to a 3-tuple push with `, None` as the third element.

- [ ] **Step 3: Run the full Rust test suite**

Run:

```bash
cd /opt/webos && cargo test --features native-platform --workspace 2>&1 | tail -30
```

Expected: 1208 passing, 0 failing (prior 1200 + 8 new). If a pre-existing Rust test fails, investigate — the shape change on `pending_wakes` may have broken an assertion on an old test that consumed the 2-tuple shape (e.g. a test that did `let (pid, resp) = &wakes[0];` needs `let (pid, resp, _heap) = &wakes[0];`). Fix forward by updating the assertion to the 3-tuple shape.

- [ ] **Step 4: Run the full vitest suite**

Run:

```bash
cd /opt/webos/web && npx vitest run 2>&1 | tail -30
```

Expected: 499 passing, 0 failing (prior 496 + 3 new).

If any new test fails:

- `options=0 on live child parks caller (no response push)` failing on a response being pushed → Task 3 Step 10's `park_on_wait(...)` match arm isn't returning `ServiceOutcome::Parked`. Verify the `dispatch_ext` `PROC_WAIT` arm no longer wraps in `ServiceOutcome::Done`.
- `child exit wakes parked parent with packed status + pid heap readback` failing on `heapBytes === null` → Task 3 Step 12's `kernel_take_next_wake_for_pid` isn't copying heap bytes OR `drainWakesForPid` isn't using `kernel_resp_heap_ptr`. Grep for both in the host file and verify the method bodies match Step 13's replacement text.
- `SIGTERM wakes parked parent with EINTR` failing on a missing wake → Task 3 Step 9's `handle_proc_kill` Term arm isn't calling `interrupt_parked_wait`. Grep `interrupt_parked_wait` in `ext.rs`; exactly one call site should exist in `handle_proc_kill`.

- [ ] **Step 5: Run Playwright to confirm no regression**

Run:

```bash
cd /opt/webos/web && npx playwright test real-kernel.spec.ts 2>&1 | tail -20
```

Expected: both existing tests pass unchanged. Userland is unchanged under 2c.1 (see design §4.3).

- [ ] **Step 6: Verify dist artefact rebuilt**

Run:

```bash
ls -l /opt/webos/dist/assets/kernel.wasm
```

Expected: mtime newer than the commit before this slice.

- [ ] **Step 7: Stage the file list + commit**

Run:

```bash
cd /opt/webos && git add \
  crates/kernel/src/sys.rs \
  crates/kernel/src/syscall/ext.rs \
  crates/kernel/src/wasm_entry.rs \
  crates/kernel/tests/sys.rs \
  web/src/kernel-wasm-host.ts \
  web/tests/unit/kernel-wasm-host.test.ts \
  dist/assets/kernel.wasm
```

Then run `git status --short` and stage any additional `M dist/...` entries if present (e.g. `dist/manifest.json` if esbuild touched it). Leave untracked files outside `dist/` alone (per SESSION-NOTES convention).

Commit with the multi-line message:

```bash
cd /opt/webos && git -c user.name='webos dev' -c user.email='dev@webos.local' commit -m "$(cat <<'EOF'
feat(001-browser-os-v1): proc_wait blocking kernel park/wake (slice 2c.1)

Close the spec-vs-impl drift on contracts/syscalls.md §3.2's
proc_wait("blocks until a matching child transitions to Zombie")
semantics paragraph and implement the second half of the park-
and-wake follow-up from multi-process-plan.md §5. options=0 now
parks the caller on a matching live child that isn't yet a
zombie; the child's Zombie transition (via proc_exit or SIGKILL)
reaps the child inline, queues the wake with the packed exit
status + reaped child pid in the heap bytes, and transitions the
parent Ready. WNOHANG preserves today's -EAGAIN semantic.
SIGTERM against a parked-on-wait parent wakes it with -EINTR via
handle_proc_kill's Term arm now calling both
interrupt_parked_accept AND interrupt_parked_wait. SIGKILL on a
parked parent exits it cleanly without queuing an EINTR wake;
parked_waiters is swept surgically inside the Signal::Kill arm
next to the existing ipc.clear_parked_acceptor_for_pid call.

Kernel-side: parked_waiters: BTreeMap<Pid, WaitParker> records
parked parents keyed by parent pid; WaitParker { req_id, target,
heap_ptr, heap_len } captures everything the wake path needs.
Kernel::park_on_wait enforces the one-waiter-per-parent invariant
(v1; lifted later if multi-threaded pids arrive). Shared
wake_parked_waiter_for_child(child_pid, ppid, status) helper fires
the reap + wake sequence from both proc_exit and proc_kill's
SIGKILL arm; targets match via WaitTarget::Any (all children) or
Specific(pid) (one child). BlockedOnWait + BlockReason::Wait
(spec-blessed since ratification, unused until this slice) are
now load-bearing.

pending_wakes tuple shape extends from (Pid, Response) to (Pid,
Response, WakeHeap) where WakeHeap = Option<PendingHeap { heap_ptr,
bytes }>. Every 2a/2b producer pushes WakeHeap::None; 2c.1's
parked-wait wake pushes Some(PendingHeap { heap_ptr,
child_pid.to_le_bytes() }) when the parker's heap_len >= 4.

Drainer extension: kernel_take_next_wake_for_pid now also copies
heap bytes into HEAP_SCRATCH + captures user heap_ptr in new
RESP_HEAP_PTR slot. New kernel_resp_heap_ptr export reads the
slot. TS drainWakesForPid copies extra_len bytes from kernel
HEAP_SCRATCH to the user SAB's heap scratch at the recorded
heap_ptr. Synchronous-reap and parked-wake paths now produce
responses with identical (value=packed_status, extra_len=4,
heap[0..4]=child_pid) shape — userland is agnostic to which path
fired.

Zero userland diff in 2c.1: hello-wait-noop still hits the
NoChildren → ECHILD path (has no children); init stays fire-and-
forget. 2c.2 migrates init to a blocking supervision loop.

Test coverage: +8 Rust kernel-isolation tests (park, child exit
wake, WNOHANG preserves EAGAIN, second park invariant, SIGTERM
EINTR, SIGKILL sweep, specific-target match logic, parent-exit
sweep). +3 TS dispatcher tests (park + connect-wake round-trip +
SIGTERM EINTR, all through real kernel.wasm). Playwright stays
green. Workspace 1698 -> 1709 passing (+11).

Constitution check PASS on all ten; Principle V strengthens
(parked Worker genuinely idle vs busy-polling — same pattern 2a
landed for ipc_accept), Principle IX improves (will improve in
2c.2 when init migrates to blocking supervision). Deviation
introduced: one-waiter-per-parent invariant (mirror of 2a's
one-parker-per-listener). Deviation closed: contracts/syscalls.md
§3.2 "blocks until a matching child transitions to Zombie"
semantics now match the implementation.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 8: Capture the commit hash**

Run:

```bash
cd /opt/webos && git log -1 --format=%h
```

Record the 7-character hash — it goes into Task 5's SESSION-NOTES entry.

## Task 5 — Append SESSION-NOTES slice-log entry

Separate `docs` commit matching the workspace cadence (mirror of 2a/2b's two-commit shape).

**Files:**
- Modify: `SESSION-NOTES.md` (append one paragraph)

- [ ] **Step 1: Read the current tail for format match**

Run:

```bash
tail -20 /opt/webos/SESSION-NOTES.md
```

- [ ] **Step 2: Append the slice-log entry**

Open `SESSION-NOTES.md` and append a new blank line followed by this paragraph (substitute `<HASH>` for the hash captured in Task 4 Step 8):

```
After the **proc_wait blocking kernel park/wake slice (2c.1)**: workspace **1709 passing, 0 failing** across all layers (Rust 1200 → 1208 +8, TS-unit 496 → 499 +3, Playwright 2 → 2). Commit <HASH>. Third slice in the kernel-park/wake arc (sibling of 2a/2b — see `docs/superpowers/specs/2026-04-20-proc-wait-blocking-slice-2c1-design.md`). Closes the spec-vs-impl drift at `specs/001-browser-os-v1/contracts/syscalls.md` §3.2: `proc_wait`'s *"blocks until a matching child transitions to Zombie"* semantics now match the implementation (pre-2c.1 the kernel comment *"v1 always returns non-blocking — the caller retries"* was the documented drift). Composes slice 2a's `ServiceOutcome::{Done, Parked}` + `Kernel.pending_wakes` + `kernel_take_next_wake_for_pid` machinery against the second blockable kernel primitive. New `Kernel.parked_waiters: BTreeMap<Pid, WaitParker>` keyed by parent pid; `Kernel::park_on_wait` parks a parent on a blocking `proc_wait` when a live matching child exists but no zombie; `Kernel::wake_parked_waiter_for_child(child_pid, ppid, status)` helper fires the reap + wake sequence from both `Kernel::proc_exit` and `Kernel::proc_kill`'s `Signal::Kill` arm (covers voluntary + SIGKILL child-exit paths); `Kernel::interrupt_parked_wait(pid) -> bool` mirrors slice 2b's `interrupt_parked_accept` and is called alongside it in `handle_proc_kill`'s `Signal::Term` arm. v1 invariant: one parker per parent (second blocking `proc_wait` while parked returns `-EAGAIN` regardless of WNOHANG) — mirror of 2a's one-parker-per-listener. `WaitTarget::Any` matches every child; `Specific(pid)` matches only that child (a child exiting with a mismatched target becomes a regular zombie for a later non-blocking wait to reap). `pending_wakes` tuple shape extended from `(Pid, Response)` to `(Pid, Response, WakeHeap)` where `WakeHeap = Option<PendingHeap { heap_ptr, bytes }>`; every 2a/2b producer pushes `None`; 2c.1's parked-wait wake pushes `Some` with the 4-byte reaped-child-pid when the parker's `heap_len >= 4`. **Drainer extension** (open question §8 resolution): `kernel_take_next_wake_for_pid` now copies heap bytes into `HEAP_SCRATCH[0..extra_len]` + captures the user's SAB heap_ptr in a new `RESP_HEAP_PTR` slot readable via new `kernel_resp_heap_ptr` export; TS `drainWakesForPid` copies `response.extraLen` bytes from kernel HEAP_SCRATCH to the user SAB's heap scratch at the recorded heap_ptr. Synchronous-reap and parked-wake paths now produce responses with identical `(value = packed_status, extra_len = 4, heap[0..4] = child_pid)` shape — userland is agnostic to which path fired. `BlockedOnWait` + `BlockReason::Wait { pid }` (spec-blessed since ratification at `crates/kernel/src/proc/mod.rs:66`, unused until this slice) are now load-bearing. **Zero userland diff in 2c.1**: `hello-wait-noop`'s `proc_wait(-1, 0, 0)` still hits the `NoChildren → ECHILD` arm (the binary has no children); init stays fire-and-forget + single `proc_kill(display-server, SIGTERM)`; display-server unchanged; no other binary calls `proc_wait`. Slice 2c.2 is the next runway: init migrates to a blocking supervision loop (`loop { match proc_wait(Any, 0) { pid => println!("init reaped pid={}") } }`), display-server drops `MAX_CLIENTS = 4` entirely with an unbounded outer loop, Playwright restores the `"display-server fb blit ok"` observable + adds `"init reaped pid=<M>"` observables. Constitution check PASS on all ten; Principle V **strengthens** (parked Worker genuinely idle vs busy-poll — same pattern 2a/2b landed for ipc_accept, extended to proc_wait), Principle IX **improves** (neutral under 2c.1 because userland is unchanged; strictly when 2c.2 migrates, init's supervisor CPU drops from busy-poll-every-SIGCHLD to parked-until-child-exits, same magnitude as 2a's display-server CPU reduction). Deviation introduced: one-waiter-per-parent invariant. Deviation closed: `contracts/syscalls.md` §3.2's blocking semantics now match impl. **Architectural milestone**: every blockable-in-spec kernel primitive (ipc_accept + proc_wait) now has real park/wake. The remaining busy-polled primitives (`fd_read` on empty pipe/socket/signal-channel, `proc_check_signal` on empty inbox) can adopt the same pattern — add a parker slot on the underlying object, route wake-on-readiness through `pending_wakes`, reuse `kernel_take_next_wake_for_pid` + `drainWakesForPid`. Next single-slice runway candidates: (slice 2c.2) userland migration — init supervisor loop + display-server unbounded accept + `"fb blit ok"` restoration; (fallback) parked `fd_read` on pipe with `Pipe::waiting_readers` / `waiting_writers` — the Vec parker slots already exist at `crates/kernel/src/ipc/pipe.rs`, 2c.1's drainer-extra_len path covers the N-byte readback they'd need.
```

- [ ] **Step 3: Commit the SESSION-NOTES append**

Run:

```bash
cd /opt/webos && git add SESSION-NOTES.md && git -c user.name='webos dev' -c user.email='dev@webos.local' commit -m "$(cat <<'EOF'
docs(001-browser-os-v1): SESSION-NOTES — append slice log entry for <HASH>

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

Replace `<HASH>` with the hash from Task 4 Step 8.

- [ ] **Step 4: Confirm the two-commit sequence**

Run:

```bash
cd /opt/webos && git log --oneline -n 3
```

Expected shape:

```
<hash-docs>   docs(001-browser-os-v1): SESSION-NOTES — append slice log entry for <HASH>
<HASH>        feat(001-browser-os-v1): proc_wait blocking kernel park/wake (slice 2c.1)
7db38ff       docs(001-browser-os-v1): proc_wait blocking slice 2c.1 design spec
```

---

## Self-review checklist

1. **Spec coverage** — every design-spec section maps to a plan task:
   - Design §1 (problem statement + scope boundary) → Task 3 Step 10 (rewrite `handle_proc_wait` to branch on WNOHANG) + Task 3 Step 11 (dispatch_ext PROC_WAIT arm unwraps).
   - Design §2 (non-goals) → not implemented (by construction); userland migration deferred to 2c.2; `target < -1 → EINVAL` preserved at Task 3 Step 10's early-reject.
   - Design §3.1 (parent-parks-on-wait semantics, one-waiter invariant) → Task 3 Step 4 (`park_on_wait` enforces invariant) + Task 1 test 4 (`second_wait_on_parked_parent_returns_eagain`) + Task 2 (TS dispatcher tests exercise park).
   - Design §3.2 (wake on child Zombie transition, reap inline) → Task 3 Step 5 (`wake_parked_waiter_for_child` helper) + Task 3 Step 7 (wire from `proc_exit`) + Task 3 Step 8 (wire from `proc_kill` SIGKILL arm) + Task 1 test 2 (`child_exit_wakes_parked_parent`) + Task 1 test 7 (`specific_target_wake_only_matches_specific_child`).
   - Design §3.3 (SIGTERM interrupts parked wait with EINTR) → Task 3 Step 6 (`interrupt_parked_wait`) + Task 3 Step 9 (`handle_proc_kill` Term arm extension) + Task 1 test 5 (`sigterm_interrupts_parked_wait_with_eintr`) + Task 2 test 3 (dispatcher SIGTERM).
   - Design §3.4 (handler semantics + heap readback decision) → Task 3 Step 10 (full handler rewrite) + open-question resolution preamble + Task 3 Step 12 (wake-drain extension for extra_len) + Task 3 Step 13 (TS drainer extension).
   - Design §3.5 (kernel state additions — `parked_waiters` field + `WaitParker` type) → Task 3 Step 1.
   - Design §3.6 (wire-format impact: zero new bytes) → preserved by construction; no `abi/` edit.
   - Design §3.7 (dispatcher integration reuses 2a machinery) → Task 3 Step 11 (PROC_WAIT arm passes through ServiceOutcome); no new JS dispatch loop changes.
   - Design §4.1 (8 kernel isolation tests) → Task 1 Steps 1-6.
   - Design §4.2 (3 TS dispatcher tests) → Task 2 Step 1.
   - Design §4.3 (Playwright unchanged) → Task 4 Step 5.
   - Design §5 (edge cases — parent exit sweep, parent SIGKILL sweep, multiple children, SIGKILL'd child, target==self, target==0 or -1, target > 0 non-child, unknown options bits, orphaned parker race, parker heap_len == 0) → covered by Task 3 Steps 7-8 (sweeps) + Task 1 tests 6 + 8 + Task 3 Step 10 (handler early-rejects) + §3.5 mentions orphans via `ppid == 0` short-circuit in `wake_parked_waiter_for_child`.
   - Design §6 (risks) → addressed by Task 3 Step 14 (compile verification catches borrow issues early) + Task 4 Step 3 (test suite catches regressions) + Task 4 Step 4 (TS test exercises end-to-end heap readback).
   - Design §7 (constitution check) → captured in the commit message of Task 4 Step 7.
   - Design §8 (open question) → resolved in the preamble "Open-question resolution — extra_len on the wake-drain path"; Option A baked into Task 3 Steps 12-13.

2. **Placeholder scan** — grep the file for `TODO`, `TBD`, `<placeholder>`, `fill in later`, `similar to Task N` (excluding `<HASH>` substitution variables which are explained in-context):
   - `<HASH>` appears in Task 4 Step 8 + Task 5 Step 2-3 as a substitution variable explained by Task 4 Step 8's `git log -1 --format=%h` capture. Acceptable — cannot be known at plan-writing time.
   - No `TODO`, `TBD`, `<placeholder>`, `fill in later` anywhere in the plan.
   - No "similar to Task N" / "similar to X" / "adapt as needed" — every code block is self-contained.
   - The phrase "Mirror of slice 2a's one-parker-per-listener" etc. appears in prose narrative and commit message only, NOT inside a code block's placeholder position.

3. **Type consistency** — names reconciled across design, plan code blocks, test bodies:
   - `WNOHANG` constant: `abi::ext::wait_opts::WNOHANG = 0x1` (existing) — used in Task 3 Step 10 exactly.
   - `BlockReason::Wait { pid: Pid }` (existing variant) — Task 3 Step 4 `set_block_reason(pid, BlockReason::Wait { pid: reason_pid })`; Task 1 test 1 asserts `Some(BlockReason::Wait { pid: -1 })`. Consistent.
   - `WaitTarget::Any` / `WaitTarget::Specific(Pid)` — Task 3 Step 4 (`park_on_wait` stores it), Task 3 Step 5 (`wake_parked_waiter_for_child` matches), Task 1 tests (`WaitTarget::Any`, `WaitTarget::Specific(child_b)`), Task 3 Step 10 (handler computes it). Consistent.
   - `WaitParker { req_id, target, heap_ptr, heap_len }` — Task 3 Step 1 struct decl uses all four fields; Task 3 Step 4 inserts with all four; Task 1 test 1 asserts on all four; Task 3 Step 5 reads all four. Consistent.
   - `WakeHeap = Option<PendingHeap>` with `PendingHeap { heap_ptr, bytes }` — Task 3 Step 1 decl, Task 3 Step 2 existing-producer migration uses `None`, Task 3 Step 5 new-producer uses `Some(PendingHeap { heap_ptr, bytes })`, Task 1 tests destructure as `(wake_pid, wake_resp, wake_heap)`, Task 3 Step 12 (kernel export) reads it, Task 3 Step 13 (TS drainer) expects extra_len in Response separately. Consistent.
   - `pending_wakes: Vec<(Pid, Response, WakeHeap)>` field type matches across Task 3 Step 1 (decl), Task 1 tests (`pending_wakes_snapshot` returns this shape), Task 3 Step 3 (test helper signature), Task 3 Step 12 (kernel export destructure `(_, resp, heap)`).
   - `Kernel::park_on_wait(parent_pid, req_id, target, heap_ptr, heap_len) -> Result<(), KernelError>` signature matches across Task 3 Step 4 impl, Task 1 tests' call sites, Task 3 Step 10 handler call.
   - `Kernel::interrupt_parked_wait(pid) -> bool` matches across Task 3 Step 6 impl, Task 3 Step 9 handler call, Task 1 test 5.
   - `Kernel::wake_parked_waiter_for_child(child_pid, ppid, status) -> bool` matches across Task 3 Step 5 impl, Task 3 Steps 7-8 call sites.
   - `kernel_take_next_wake_for_pid(pid) -> i32` (unchanged signature — the only change is that the existing return 1 now also means "heap bytes written to HEAP_SCRATCH if extra_len > 0") — Task 3 Step 12.
   - `kernel_resp_heap_ptr() -> u32` new export signature matches across Task 3 Step 12 (kernel), Task 3 Step 13 (TS interface decl + usage in `drainWakesForPid` + `takeNextWakeForPidWithHeap`).
   - `kernel_register_process_for_spawn(parent: Pid, name_ptr: u32, name_len: u32) -> i32` matches across Task 3 Step 13 (kernel impl + TS interface + TS caller `spawnChildForTest`).

4. **Test-count deltas match design §4.4:**
   - Rust 1200 → 1208 (+8 new tests in Task 1, all passing after Task 4 Step 3). ✓
   - TS-unit 496 → 499 (+3 new tests in Task 2, all passing after Task 4 Step 4). ✓
   - Playwright 2 → 2 (unchanged; verified in Task 4 Step 5). ✓
   - Workspace total 1698 → 1709 (+11). ✓

5. **Commit structure matches 2a/2b cadence:**
   - Single `feat` commit for source + tests + dist artefact (Task 4 Step 7).
   - Separate `docs` commit for SESSION-NOTES append (Task 5 Step 3).
   - Both use the `git -c user.name='webos dev' -c user.email='dev@webos.local' commit` form per project convention.
   - Both include `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>` trailer.

---

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-04-20-proc-wait-blocking-slice-2c1.md`. Hand off to the execution session. The red tests in Tasks 1-2 pin the full park/wake/interrupt contract before any production code exists; Task 3's production code is designed to make exactly those tests pass; Task 4 builds + verifies + commits; Task 5 appends SESSION-NOTES.

Expected workspace shape on completion: 1709 passing across all layers, two new commits on `main` (`feat` + `docs`), one regenerated `dist/assets/kernel.wasm`, zero userland-binary diff, constitution check clean on all ten principles.
