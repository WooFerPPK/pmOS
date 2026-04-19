# `ipc_accept` blocking semantics (display-server migration + SIGTERM interrupt) — slice 2b design

Date: 2026-04-19
Status: approved
Related: `docs/superpowers/specs/2026-04-19-ipc-accept-blocking-slice-2a-design.md` (slice 2a — kernel park/wake mechanics; landed at commit 516df81), `docs/superpowers/specs/2026-04-19-display-server-multi-client-accept-loop-design.md` (arc slice 1 — outer accept loop; landed at commit b1449da), `specs/001-browser-os-v1/contracts/syscalls.md` §§`ipc_accept` + `proc_kill`, `specs/001-browser-os-v1/multi-process-plan.md` §5, `SESSION-NOTES.md` line 262 (slice 2a forward-runway paragraph).
Slice shape: single session, single commit. Second of two sub-slices (2a + 2b) that together resolve the "long-running display-server accept loop" runway item's blocking-syscall leg — 2a added the kernel mechanism, 2b wires userland into it.

## 1. Problem

Slice 2a landed the kernel park/wake primitive for `ipc_accept(flags=0)` but deliberately kept userland untouched. Today's bundled display-server at `crates/display-server/src/main.rs:118-123` still wraps every accept in the `MAX_POLLS = 10_000` inner busy-poll + the `MAX_CLIENTS = 4` outer ceiling — dead code that now succeeds on iteration 0 of every inner poll because the kernel's `flags=0` blocking parks the caller until a peer connects. The ceilings were testing fictions (see `crates/display-server/src/main.rs:34-39`), not architectural; 2a's kernel support was the precondition to deleting them.

More acutely: slice 2a introduced a new user-visible regression that 2b closes. Under Playwright, display-server now serves its two clients, re-enters the outer loop for iteration 2, parks indefinitely on the empty listener backlog, and **never prints the `"display-server fb blit ok"` trailing line that earlier arc slices relied on** (`web/tests/integration/real-kernel.spec.ts:72-82` documents the shift + `real-kernel.spec.ts:169-173` pins the gap). The Playwright terminal observable had to fall back to `"display-server served client 1"` — a weaker assertion. The correct shape for a long-running server is "park until signalled, drain fd 3, exit 0" — the POSIX `accept()`-loop pattern. 2b implements exactly that.

There's a third problem, tied to the first two: `init` today fires-and-forgets three (now four, after slice 1) children and exits. With slice 2a's blocking accept, display-server outlives init and parks forever — there's no code anywhere in the userland that signals it to exit. Under Playwright this is currently acceptable (the test scrapes console lines and terminates the page when it has observed enough), but it leaves an orphan `BlockedOnIpc` pid in the kernel state; under a `proc_wait`-driven supervisor (slice 2c / later arc) it would block the reaper forever. 2b threads a final `proc_kill(display_server_pid, SIGTERM)` into init so the shutdown path closes.

## 2. Non-goals

Explicitly out of scope for 2b — deferred to later slices:

- **init `proc_wait` supervision loop.** 2a's §8 flagged this as "may move to slice 2c / later"; 2b confirms the defer. init stays fire-and-forget except for the one new SIGTERM on display-server. A real supervisor would `proc_wait` on every spawned pid, respawn on crash, and reap zombies; that's a separate arc with its own cap-surface and signal-delivery questions.
- **Other blockable syscalls.** `fd_read` on empty pipes, `ipc_connect` waiting for server accept, and `proc_wait` on a running child still return `-EAGAIN` (the pipe pre-existing `waiting_readers`/`waiting_writers` Vecs in `crates/kernel/src/ipc/pipe.rs` are tempting but out of arc scope). 2b's SIGTERM-interrupts-park semantic is confined to `ipc_accept`.
- **Non-`SIGTERM` catchable-signal interruption.** The signal-interrupt-parked-accept path accepts `SIGTERM` only in 2b (see §3.1 for the accepted-signum decision). Adding `SIGINT`/`SIGPIPE` later is a trivial extension — the dispatch is already a match arm — but 2b keeps the surface minimal and lands only what display-server actually needs.
- **Multi-parker per listener.** v1's one-parker-per-listener invariant (documented at `crates/kernel/src/ipc/socket.rs:105-108`) stays. Nothing in 2b changes it.
- **`runAllSpawns` vitest harness calling `markRunning`.** 2a documented that the sequential composition helper leaves spawned pids in `Ready` state (not `Running`), which is why the vitest layer sees display-server exit 12 rather than the production park-and-wake path. 2b keeps this quirk — fixing `runAllSpawns` to match production is a fallback slice, and the exit code the vitest test pins after 2b is a function of this quirk (see §4.3).

## 3. Design

### 3.1. Kernel: signal-interrupts-parked-accept semantics

**Accepted-signum set for interruption (decision): SIGTERM only.**

Rationale: display-server is the only caller currently parked on `ipc_accept` in the v1 userland, and SIGTERM is the canonical cooperative-shutdown signal. Extending to SIGINT (ctrl-c semantics) or SIGPIPE (broken-peer semantics) adds surface area without a real caller to validate it — deferred to "the first process that needs it." `SIGKILL` is categorically excluded: it's non-catchable (`crates/kernel/src/proc/signal.rs:66-70`'s `is_catchable` pins this) and today's `proc_kill` already synchronously exits the target with `ExitStatus::Signaled(9)` (`crates/kernel/src/sys.rs:1316-1329`). A SIGKILL'd parked pid goes straight to Zombie via `exit`; the `cleanup_proc` path already in `proc_exit` (which calls `ipc.clear_parked_acceptor_for_pid` at `sys.rs:1101`) sweeps the listener's `parked_acceptor` slot. No EINTR response is needed — the pid is dead, not interrupted. `SIGCHLD` is kernel-generated (parent-side, never sender-side) and never reaches a `proc_kill`-driven path against a blocked-on-ipc target. `SIGINT` and `SIGPIPE` fall into the "post to inbox, no park interrupt" arm unchanged from today.

**signum=0 semantics (decision): no-op on park.** POSIX `kill(pid, 0)` is an existence+permission probe with no signal delivered. `handle_proc_kill` at `crates/kernel/src/syscall/ext.rs:620-625` already routes signum=0 through `proc_check_signal` (which returns `Ok(())` without touching the inbox). 2b preserves this: a signum=0 probe against a `BlockedOnIpc` pid returns 0 without waking, without clearing `parked_acceptor`, without transitioning state. The test at §4.1 pins this invariant.

**Wake behaviour on SIGTERM.** When `handle_proc_kill` is called with `signum=15` and the target is `BlockedOnIpc`, the kernel:

1. Delivers SIGTERM to the target's `SignalInbox` via the existing `inbox.post(Signal::Term)` path at `sys.rs:1331-1334` (unchanged — SignalInbox delivery is orthogonal to parker state).
2. Scans the listener socket table (`IpcTable::sockets`) for any `parked_acceptor == Some((target_pid, _))` slot; on hit, takes the `(pid, req_id)` tuple, clears the slot.
3. If a parker was found, appends `(target_pid, Response::err(req_id, EINTR))` onto `Kernel.pending_wakes` (same queue 2a landed at `crates/kernel/src/sys.rs:229`).
4. Transitions `target_pid` from `BlockedOnIpc` to `Ready` and clears `block_reason` (same two-call sequence `wake_parked_acceptor_if_any` already uses at `sys.rs:832-833`).

A parker that is woken by SIGTERM therefore observes *two* events: the `-EINTR` response on its SAB (from the pending-wake drain) and SIGTERM in its signal inbox (readable via fd 3 on the next `fd_read(SignalChannel)`). Userland handles both — the accept-call-site sees rc=-EINTR and knows to check the signal channel.

### 3.2. Kernel state + API changes

**No new fields.** `parked_acceptor` (the `Option<(Pid, u32)>` at `crates/kernel/src/ipc/socket.rs:109`) + `pending_wakes` (the `Vec<(Pid, Response)>` at `crates/kernel/src/sys.rs:229`) both landed in 2a and are sufficient for 2b's needs. The `IpcTable::clear_parked_acceptor_for_pid` method at `crates/kernel/src/ipc/mod.rs:502-510` already does the linear scan-and-clear walk 2b needs — 2b adds one method that wraps it to *also* emit the `-EINTR` wake.

**New `Kernel` method.**

```rust
/// Interrupt any parked ipc_accept on `pid` with -EINTR. Clears
/// the listener's `parked_acceptor` slot, queues a wake with
/// `Response::err(req_id, EINTR)`, transitions `pid` Ready +
/// clears block_reason. No-op if `pid` is not parked on any
/// listener. Called from `proc_kill` when a catchable signal
/// targets a BlockedOnIpc pid.
///
/// Returns true iff a park was interrupted (useful for tests;
/// production callers discard the bool).
pub fn interrupt_parked_accept(&mut self, pid: Pid) -> bool;
```

Implementation: walk `self.ipc.sockets` (via a new `IpcTable::take_parked_acceptor_for_pid(pid) -> Option<u32>` that returns the `req_id` on match + clears the slot, mirroring `clear_parked_acceptor_for_pid` at `mod.rs:502` but returning the req_id instead of silently dropping it). On `Some(req_id)`: push `(pid, Response::err(req_id, EINTR))` onto `pending_wakes`, `procs.transition(pid, Ready)`, `procs.clear_block_reason(pid)`, return true. On `None`: return false.

**`handle_proc_kill` extension at `crates/kernel/src/syscall/ext.rs:617-639`.** The existing body matches on signum, routes signum=0 through `proc_check_signal`, maps other signums to `Signal` variants, and calls `kernel.proc_kill`. 2b inserts one post-step after the `kernel.proc_kill(pid, target_pid, signal)` call returns `Ok(())`: if `signal == Signal::Term`, call `kernel.interrupt_parked_accept(target_pid)`. The interrupt runs *after* the signal-inbox delivery so a userland caller that drains fd 3 after observing the EINTR wake finds SIGTERM there.

Placement decision: interrupt-parked-accept lives on `Kernel` rather than inside `proc_kill` itself because `proc_kill` today is a pure "post to inbox / synchronous exit" call with no wake-queue side effects. Keeping the interrupt as a separately-named method clarifies the contract — the surface of `proc_kill` stays "deliver signal"; the surface of `interrupt_parked_accept` is "wake any ipc_accept parker." The extension-handler composes the two.

### 3.3. Display-server userland reshape

**File:** `crates/display-server/src/main.rs` (full rewrite of the outer loop + docstring).

**New imports / bindings:**

- `ipc_accept` already imported at `main.rs:86`. No signature change — the TS shim at `web/src/user-wasm-runtime.ts:2435` already zero-fills the args window, so `ipc_accept(listener_fd)` from Rust surfaces `flags=0` on the wire. The 2a spec's §3.1 confirmed this is the blocking default.
- Add `fn fd_read(fd: i32, ...) -> i32` — already imported at `main.rs:73-78` for the client payload read; reused verbatim for fd 3 polling.
- No new extern imports. SIGTERM byte-pattern decoding is a 2-byte LE compare against `15u16`, done inline.

**New constants.**

```rust
const EINTR: i32 = 27;
const SIGNAL_FD: i32 = 3;
const SIGTERM_NUM: u16 = 15;
```

Matches `crates/abi/src/errno.rs:38` (EINTR=27), the auto-installed SignalChannel fd at `crates/kernel/src/sys.rs:1212` (`install_at(3, ...)`), and the wire-format defined at `crates/kernel/src/proc/signal.rs:43-45` + `sys.rs:506-529` (each signal = 2 bytes LE signum).

**Remove** `MAX_POLLS: u32 = 10_000` (`main.rs:118`) and `MAX_CLIENTS: u32 = 4` (`main.rs:123`). Both ceilings were testing fictions documented in `main.rs:34-39` as deferred to "a future slice once `ipc_accept` has real blocking semantics and the server exits on SIGTERM instead of by draining." 2b is that slice.

**New outer loop shape:**

```text
'outer: loop {
    // Signal-driven exit check BEFORE the blocking accept call.
    // If SIGTERM is already queued on fd 3 (e.g. init signalled us
    // while we were finishing the previous client), drain + exit
    // without ever entering the blocking accept.
    if poll_sigterm_nonblock() {
        break 'outer;
    }

    // Blocking accept. Returns a positive server fd on success, or
    // -EINTR if a signal interrupted the park.
    let rc = ipc_accept(listener);
    if rc == -EINTR {
        if poll_sigterm_nonblock() {
            break 'outer;
        }
        // Some other catchable signal woke us (future extension);
        // re-enter the blocking accept.
        continue 'outer;
    }
    if rc < 0 {
        std::process::exit(12);
    }
    let server = rc;

    // ... fd_read + fd_write + fd_close + println! for client `i` ...
}

println!("display-server fb blit ok");
```

Where `poll_sigterm_nonblock()` is a local helper:

```rust
fn poll_sigterm_nonblock() -> bool {
    let mut buf = [0u8; 16];  // room for 8 queued signals
    let iov = Iovec { buf: buf.as_mut_ptr(), buf_len: buf.len() as u32 };
    let mut nread: u32 = 0;
    let rc = unsafe { fd_read(SIGNAL_FD, &iov, 1, &mut nread) };
    if rc != 0 || nread == 0 {
        return false;
    }
    // Each signal = 2 bytes LE u16. Walk them looking for SIGTERM.
    let n = nread as usize;
    let mut i = 0;
    while i + 1 < n {
        let sig = u16::from_le_bytes([buf[i], buf[i + 1]]);
        if sig == SIGTERM_NUM {
            return true;
        }
        i += 2;
    }
    false
}
```

`fd_read` on `SignalChannel` returns `-EAGAIN` (=6) when the inbox is empty (`sys.rs:520-522`). Non-zero rc + nread=0 is treated as "no signals, keep going." The helper consumes any queued non-SIGTERM signals silently — the server doesn't care about SIGINT/SIGPIPE in v1. A future extension that wants to route them inverts this helper into a `drain_signals() -> Vec<Signal>` shape; keeping it boolean-only for 2b is the minimum viable surface.

**New docstring.** Update `main.rs:1-52`:

```text
//! PMos display server binary — long-running std binary with
//! blocking accept + signal-driven exit.
//!
//! Binds `/run/display`, opens `/dev/fb0` once, then runs an
//! unbounded outer accept loop. Each iteration:
//!
//!   1. Non-blocking drain of fd 3 (SignalChannel). If SIGTERM
//!      is queued, exit 0 cleanly.
//!   2. `ipc_accept(listener, flags=0)` — blocks until a client
//!      connects or a signal interrupts. `-EINTR` means the
//!      park was woken by SIGTERM (slice 2b kernel contract);
//!      re-enter the signal drain.
//!   3. On a fresh server fd: `fd_read(16 bytes)` → `fd_write
//!      (/dev/fb0)` → `fd_close(server)` → `println!("served
//!      client i")`.
//!
//! After the outer loop breaks on SIGTERM: `println!("display-
//! server fb blit ok")` is the trailing observable line the
//! integration test keys on, followed by fall-off-main-std →
//! `__wasi_proc_exit(0)`.
//!
//! Exit codes:
//!
//!   * 0  = success (outer loop broke cleanly on SIGTERM)
//!   * 10 = `display_bind` failed
//!   * 12 = `ipc_accept` returned a non-EINTR, non-positive
//!          error (surface of any kernel-level failure)
//!   * 14 = `fd_read` on the client fd returned a non-EAGAIN
//!          error or read 0 bytes
//!   * 15 = `path_open("/dev/fb0")` failed
//!   * 16 = framebuffer `fd_write` failed or short-wrote
//!   * 19 = `fd_close` on the client fd returned a non-zero
//!          errno
//!
//! Exit codes 17 (first-accept poll exhausted) and 18 (fd_read
//! poll exhausted) are removed from the table — blocking accept
//! makes iteration 0 always succeed-or-EINTR, and fd_read on a
//! connected socket always has bytes to read (the client wrote
//! 16 bytes before this accept returned).
```

Exit code 17 goes away entirely — the "first-accept exhaustion" arm at `main.rs:162-167` is unreachable under blocking accept. Exit code 18 *(fd_read poll exhausted)* likewise goes away — with blocking accept, the accept only returned because a client successfully connected and wrote its payload; the fd_read immediately sees bytes. The inner `MAX_POLLS` loop around `fd_read` at `main.rs:176-187` collapses to a single call, same as accept.

Per-client handling (`fd_read` + `fd_write` + `fd_close`) is unchanged in structure — the rewrite is limited to the outer loop shape + signal polling.

### 3.4. Init reshape

**File:** `crates/init/src/main.rs`.

**New import:** `fn proc_kill(pid: i32, signum: u32) -> i32;` from `pmos_ext`. The `handle_proc_kill` wire format at `crates/kernel/src/syscall/ext.rs:617-639` takes `args[0..4] = target_pid (i32)` + `args[4..6] = signum (u16)` — the userland shim treats `signum: u32` and the TS shim zero-fills bytes [6..8], matching every other PROC_KILL caller in the tree. A `signum` of 15 decodes to `Signal::Term`.

**State to capture:** modify the existing display-server spawn at `main.rs:65-73` to retain the returned pid:

```rust
const DISPLAY_SERVER: &[u8] = b"/bin/display-server";
let ds_pid = unsafe {
    proc_spawn(DISPLAY_SERVER.as_ptr(), DISPLAY_SERVER.len() as u32, u64::MAX)
};
if ds_pid < 0 {
    println!("init: proc_spawn /bin/display-server failed errno={}", -ds_pid);
} else {
    println!("init spawned display-server pid={}", ds_pid);
}
```

The existing println already prints the pid — the change is just binding the return into a named variable.

**New call sequence before exit.** Between the final `println!("init spawned display-client-demo pid=...")` at `main.rs:103-105` and the trailing `println!("init exiting")` at `main.rs:108`, insert:

```rust
if ds_pid > 0 {
    let rc = unsafe { proc_kill(ds_pid, 15) };
    if rc < 0 {
        println!("init: proc_kill(display-server, SIGTERM) failed errno={}", -rc);
    } else {
        println!("init sent SIGTERM to display-server pid={}", ds_pid);
    }
}
```

Guard on `ds_pid > 0` so a spawn-failure case (where `ds_pid` is negative) doesn't fire a bogus kill. The cap check in `handle_proc_kill` + `Kernel::proc_kill` passes: init is display-server's parent (ppid match at `sys.rs:1296`), which satisfies the `is_parent` branch of the cap test at `sys.rs:1305`.

**Decision: init does NOT `proc_wait` on display-server after signalling.** Rationale: the 2a spec's §8 flagged `proc_wait` supervision as "may move to slice 2c / later"; deferring keeps 2b's blast radius focused on the blocking-accept close-out. init remains fire-and-forget + one SIGTERM. The observable consequence: display-server may still be cleaning up (or already zombie-awaiting-reap) when init exits; neither case is an error under v1's no-reaper substrate. Slice 2c can add the `proc_wait` loop without disturbing 2b's wire.

The println after `proc_kill` is a deliberate observable — Playwright will assert it lands (see §4.4) so a regression that drops the signal fires a test failure at the init layer rather than the display-server layer.

### 3.5. Three-layer coverage milestone

2b closes the "long-running display-server accept loop" runway item. The arc has shipped four integrated capabilities:

1. (slice 1, b1449da) Outer accept loop — display-server is a bounded multi-client server.
2. (slice 2a, 516df81) Kernel park/wake — `ipc_accept(flags=0)` blocks + wakes on connect.
3. (slice 2b, this slice) Signal-driven exit + SIGTERM-interrupts-park — display-server shuts down cleanly on init's signal.

Together: display-server is now shaped correctly for the role it plays in the v1 architecture (a userland service that runs until the system asks it to stop). Every remaining arc item (init `proc_wait`, multi-parker per listener, other blockable syscalls) is optional polish — nothing in the v1 userland exercises paths that would benefit from them until shell / toolkit work brings more processes to the IPC surface.

### 3.6. ABI / wire-format impact statement

**Zero new bytes.** SIGTERM delivery uses the existing `PROC_KILL` opcode (0x1102 per `contracts/syscalls.md:304-308`) with the existing signum=15 mapping to `Signal::Term`; the `handle_proc_kill` dispatcher at `ext.rs:617-639` already accepts signum=15 and routes it through `Kernel::proc_kill`'s Term arm. `-EINTR` on the blocking accept uses the existing `Response::err(req_id, EINTR)` + `pending_wakes` queue from 2a — same shape 2a's `EBADF`-on-listener-close path produces. Display-server's SignalChannel poll goes through the auto-installed fd 3 + `fd_read` path existing since the SIGCHLD-delivery batch (same path every child uses to observe SIGCHLD).

The only change visible to specs is operational: slice 2a's `contracts/syscalls.md:213` line ("blocks the caller until a client `ipc_connect`s to the listener **or the process is signalled**") becomes true for the first time — 2a documented the contract, 2b implements the or-clause.

## 4. Testing

Three-layer coverage. Rust + TS-unit + Playwright all gain tests; 2a's green count 1694 → 2b's new total per §4.5.

### 4.1. Kernel isolation (Rust, `crates/kernel/tests/`)

Three new tests. All three adjacent to 2a's block at `crates/kernel/tests/sys.rs` (around line 1413, immediately after `park_on_accept_clears_on_listener_close`).

1. **`sigterm_interrupts_parked_accept_with_einti`** — pid A (display-server stand-in) parks on `ipc_accept`. Pid B (init stand-in; set as A's ppid) calls `proc_kill(A, Signal::Term)`. Assert: A's state transitions `BlockedOnIpc → Ready`; listener's `parked_acceptor == None`; `pending_wakes` contains exactly `(A, Response::err(req_id, EINTR))`; A's SignalInbox holds `[Signal::Term]` (the signal delivery path runs orthogonally to the park interrupt). A `kernel_drain_wakes_for_pid(A, sab_A)` call pushes the EINTR response onto A's SAB.
2. **`signum_zero_probe_does_not_wake_parked_accept`** — pid A parked on `ipc_accept`. Pid B (parent) calls `PROC_KILL(A, signum=0)` — POSIX existence/permission probe. Assert: response is `Ok(0)` (probe succeeded); A's state stays `BlockedOnIpc`; listener's `parked_acceptor == Some((A, req_id))`; `pending_wakes` is empty; A's SignalInbox is empty (probe never delivered a signal). Pins the decision in §3.1.
3. **`sigkill_on_parked_accept_exits_without_eintr_wake`** — pid A parked on `ipc_accept`. Pid B (parent) calls `proc_kill(A, Signal::Kill)`. Assert: A's state is `Zombie` with `ExitStatus::Signaled(9)` (the existing SIGKILL path at `sys.rs:1316-1329`); listener's `parked_acceptor == None` (cleared by `cleanup_proc`'s `clear_parked_acceptor_for_pid` at `sys.rs:1101`); `pending_wakes` is empty (no EINTR wake queued — the pid is dead, not interrupted). Pins the "non-catchable path is orthogonal" invariant.

All three tests are constructed with pid B set as A's parent so the cap check at `sys.rs:1296-1307` passes without needing `Cap::ProcKillAny`.

**Rust test delta:** 1197 → 1200 (+3).

### 4.2. TS dispatcher (`web/tests/unit/kernel-wasm-host.test.ts`)

One new test in the existing `describe("dispatch: IPC_ACCEPT blocking")` block at `kernel-wasm-host.test.ts:3450`, placed immediately after the `IPC_CONNECT from peer wakes parked acceptor` test at line 3488.

4. **`SIGTERM wakes parked acceptor with EINTR; take-wake delivers error response`** — real `kernel.wasm` via `KernelWasmHost`. Register pid A (display-server stand-in) + pid B as A's parent via `registerProcessWithParent` (or equivalent — the helper exists for SIGCHLD tests). Bind A on `/run/display`, dispatch `IPC_ACCEPT(flags=0)` — assert `parked === true`. Dispatch `PROC_KILL(A, signum=15)` from B — assert response status=0. Call `takeNextWakeForPid(A)` — assert the response has `status === EINTR (27)` and `requestId === <the accept's requestId>`. Assert a second `takeNextWakeForPid` returns `null`. (The SignalChannel-delivery half of the contract is pinned by the existing SIGCHLD tests and the kernel-isolation test 1 above; not duplicated here.)

Rationale for adding only one dispatcher test: 2a added two tests that together pin "park + wake via IPC_CONNECT"; this new test pins "wake via SIGTERM" as the mirror-image. Adding tests for signum=0 (probe) and SIGKILL at the dispatcher layer would duplicate kernel-isolation coverage without adding a new integration surface — the dispatcher's role is to route the opcode, not to enforce the park/wake semantic.

**TS-unit delta:** 495 → 496 (+1).

### 4.3. Vitest composition test update (`web/tests/unit/user-wasm-runtime.test.ts`)

The "init (std) spawns hello-std AND display-server AND display-client-demo" test at `user-wasm-runtime.test.ts:2118-2261` needs its expectations updated.

**Current state (after 2a):** `history.length === 5`, display-server exits 12 (the `-ESRCH` arm from the `markRunning` quirk in `runAllSpawns`), `lines.length === 10`.

**After 2b:** `history.length === 5` (unchanged — still 5 spawned pids under the sequential harness), **display-server still exits 12** (the `runAllSpawns` markRunning quirk documented at `user-wasm-runtime.test.ts:2202-2215` is NOT fixed in 2b; the blocking accept still fails with ESRCH because the pid transitions Ready→BlockedOnIpc which fails on a Ready pid). The init change adds **one new console line** (`init sent SIGTERM to display-server pid=<N>`) and **one new proc_kill syscall** init issues before exit. The shim-level behaviour is:

1. init calls `proc_kill(ds_pid, 15)` as its last act.
2. Under `runAllSpawns`, display-server has *already* exited (sequential execution — init runs to completion first). So `Kernel::proc_kill` observes target-pid-Dead and returns `-ESRCH` at `sys.rs:1298-1300` (dead target rejection). init's branch prints `"init: proc_kill(display-server, SIGTERM) failed errno=71"`.
3. Alternatively: under a scenario where display-server is still in Ready (the actual `runAllSpawns` pattern — init exits, children run sequentially, display-server runs next), init has already issued the `proc_kill` *before* display-server runs. The dispatcher's proc_kill on a Ready target succeeds (no state check at `sys.rs` bars Ready from receiving SIGTERM) and queues Term on the inbox. Then display-server starts, calls `ipc_accept(flags=0)`, which fails with `-ESRCH` (the markRunning quirk), exits 12 before ever polling fd 3.

Decision: expect the **proc_kill-after-exit** shape (option 2 above with Dead-target rejection). `runAllSpawns` runs pids strictly sequentially; init completes (all its spawns + println + proc_exit) before any spawned child runs — that's the documented harness shape at `user-wasm-runtime.test.ts:2123-2132`. **But**: spawned pids are transitioned to `Ready` by the PROC_SPAWN handler and stay Ready (not Running) under `runAllSpawns` until the harness picks them up. A pid in `Ready` state with a live entry in the process table survives init's proc_kill — `Kernel::proc_kill` accepts Ready targets (the state check at `sys.rs:1298-1300` only rejects `Dead`). So init's proc_kill lands SIGTERM on display-server's inbox before display-server even runs, and prints `"init sent SIGTERM to display-server pid=<N>"`.

Expected line ordering post-2b:

```
init starting
init spawned hello-std pid=<N>
init spawned display-server pid=<M>
init spawned display-client-demo pid=<P>
init spawned display-client-demo pid=<Q>
init sent SIGTERM to display-server pid=<M>     ← NEW LINE
init exiting
hello from std
display-server starting
display-client-demo starting
display-client-demo starting
```

`lines.length === 11` (was 10; +1 for the new init line). `history.length === 5` (unchanged). display-server's exit code **stays 12** — the markRunning quirk is orthogonal to the new SIGTERM; the blocking-accept ESRCH happens before fd 3 is polled. The test update adds the new expected line at index 5 and shifts every subsequent assertion by 1.

**Vitest composition test delta:** 0 new tests, 1 updated test (same test, new assertions). Total TS-unit count: 495 + 1 (§4.2) = 496.

### 4.4. Playwright integration (`web/tests/integration/real-kernel.spec.ts`)

The existing `real-kernel.spec.ts` test needs substantive updates — this is the slice where observables actually shift in the expected direction.

**New console-line expectations:**

1. `"init sent SIGTERM to display-server pid=<N>"` — init's new observable; assert it lands before `"init exiting"` and after all four `"init spawned ..."` lines.
2. `"display-server fb blit ok"` — **restored** after being unreachable post-2a (the arc's milestone). Assert it lands; the current test's comment at `real-kernel.spec.ts:169-173` explicitly flags its absence and pins the follow-up to 2b.

The trailing observable (the `secondServedLine` poll at `real-kernel.spec.ts:83-89`) shifts back to `"display-server fb blit ok"` — it's the last thing display-server prints before exit-0, and its presence implies every earlier line is on the console.

**Ordering assertions:** add `expect(fbBlitIdx).toBeGreaterThan(displayServerServedIndices[1]!)` — blit-ok follows both served-client lines (the SIGTERM arrives after the second served client because init fires it immediately before its own exit, which happens before display-server's second accept completes). Add `expect(initSigtermIdx).toBeGreaterThan(initSpawnDisplayClientIdx)` + `expect(initExitIdx).toBeGreaterThan(initSigtermIdx)` — the SIGTERM println lands between the last spawn and init's own exit.

**DOM-text assertions:** add `expect(domText).toContain("display-server fb blit ok")` + `expect(domText).toContain("init sent SIGTERM to display-server")` mirroring the console assertions.

**Peak-workers assertion:** unchanged (still ≥ 5 — init + hello-std + display-server + two display-client-demos overlap at the peak before display-server's SIGTERM exit).

**Test count:** 2 → 2 (no new test; the existing test strengthens).

**Playwright delta:** 0 new tests; 1 updated test. The `fb blit ok` restoration is the key milestone — with it back, the integration layer has end-to-end evidence that SIGTERM interrupted display-server's park, signal draining worked, and the server printed its trailing line before exiting 0.

### 4.5. Test-count summary

| Layer | Before (2a close-out) | After (2b) | Δ |
|---|---:|---:|---:|
| Rust | 1197 | 1200 | +3 |
| TS-unit | 495 | 496 | +1 |
| Playwright | 2 | 2 | 0 |
| **Workspace total** | **1694** | **1698** | **+4** |

## 5. Edge cases

- **SIGTERM arrives before accept parks.** init's `proc_kill` lands SIGTERM on display-server's inbox before display-server even starts (under sequential vitest) or before it reaches its first `ipc_accept` (under a pathological Playwright timing). 2b's flow handles this: the outer loop starts with the `poll_sigterm_nonblock()` check — a signal that arrived before the first accept is drained on iteration 0 and the loop breaks cleanly. `-EINTR` is never returned because no park happened.
- **SIGTERM arrives while acceptor is mid-dispatch** (between accept-return and next-iteration signal-poll). The signal-inbox delivery is synchronous (queued immediately by `handle_proc_kill`); the parker is not currently parked. The next iteration's `poll_sigterm_nonblock()` picks the signal up and the loop exits before the next blocking accept. No EINTR wake is generated (no park to interrupt).
- **Multiple signals queued on fd 3.** `poll_sigterm_nonblock()`'s 16-byte buffer can hold 8 signals. A batch arrival (SIGTERM + SIGCHLD + SIGINT) is drained in one `fd_read`; the helper walks all 8 slots looking for SIGTERM and returns true on hit. Non-SIGTERM signals are silently discarded in v1 — that's the documented 2b scope limit (§2 non-goal: other signals).
- **Parked acceptor is orphaned after init exits** (no SIGTERM fired or lost). Under slice 2b's init, the SIGTERM IS fired synchronously before init's exit, so this path should not fire in normal operation. If it does (bug or reordering), display-server parks forever — same failure class as 2a's "no 3rd client" deadlock, already flagged in 2a §5. A future slice's `proc_wait` supervisor is what closes this gap fully; 2b relies on init's ordering.
- **signum=0 probe on parked pid** (test 2 in §4.1). Pins the no-op-on-park invariant — probes never wake parkers.
- **SIGKILL on parked pid** (test 3 in §4.1). Pins the synchronous-exit path — SIGKILL'd pid transitions Zombie via `proc_kill`'s own body before any park-interrupt logic runs.
- **EINTR-wake races a concurrent connect-wake.** Listener's `parked_acceptor` is a single `Option<(Pid, u32)>`. `interrupt_parked_accept` and `wake_parked_acceptor_if_any` both `.take()` the slot — only one wins. If connect lands first, the EINTR path finds `None` and no-ops (the parker is already Ready). If SIGTERM lands first, the connect path finds `None` in the listener's `parked_acceptor` — the backlog push still happens (the server will accept that client on its next non-blocking iteration, post-SIGTERM-drain). Under 2b's flow, the server breaks on the SIGTERM rather than accepting; the client's connect succeeds from its perspective (the listener accepted the connection into its backlog) but the server never picks it up. Acceptable for v1 — the race window is microseconds and neither outcome violates any invariant.
- **`fd_close` on the server fd after EINTR-wake.** The EINTR-wake carries `-EINTR` in the response, not a new fd. Display-server's flow doesn't call `fd_close` on the EINTR path — the accept never materialised an fd. No leak.

## 6. Risks

- **Playwright timing sensitivity.** The new `"fb blit ok"` assertion depends on SIGTERM arriving after both clients are served — if init's proc_kill lands too fast (before display-client-demo's connect completes), display-server might exit on SIGTERM before it accepts client 2. Mitigated by init's ordering: SIGTERM is fired *after* both display-client-demo spawns, which means the SIGTERM lands on display-server's inbox at some point during display-server's first client's processing (by the time the second client connects, the SIGTERM may already be queued). Display-server checks fd 3 *between* accepts; the check at iteration-start on iteration 1 is what catches it. In practice, the 15-second Playwright timeout at `real-kernel.spec.ts:89` gives a generous window. If flake emerges, the fix is a minor fd_read scheduler tweak, not an architectural change.
- **SIGTERM-after-SIGKILL ordering.** If a future caller fires SIGKILL then SIGTERM against a parked pid in quick succession, the SIGKILL path at `sys.rs:1316-1329` transitions the pid to Zombie before SIGTERM's delivery can post to the inbox (the inbox is removed by `signal_inboxes.remove(pid)` in `cleanup_proc`). SIGTERM then hits a Dead target and returns ESRCH at `handle_proc_kill`. No bug, but worth pinning in a test if/when a multi-signal-in-flight scenario emerges. Out of 2b scope.
- **EINTR double-delivery.** If `interrupt_parked_accept` is called twice (spurious or racy), the second call's `take_parked_acceptor_for_pid` returns `None` (the slot was already cleared by the first call), so no second wake is queued. Idempotent by construction.
- **Unused-warning cleanup.** Removing `MAX_POLLS` and `MAX_CLIENTS` also removes the `served_any` local variable (`main.rs:148`). Rust's unused-var lint will flag anything left behind — the cleanup is the outer-loop rewrite itself; verified by `cargo build --release -p display-server --target wasm32-wasip1` green.

## 7. Constitution check

| # | Principle | Status | Rationale |
|---|---|---|---|
| I | Real OS, not a simulation | PASS | SIGTERM-interrupts-accept is textbook POSIX accept(2) behaviour; `EINTR` return on a signal-interrupted blocking syscall is POSIX §7 language. No UI concepts enter the kernel. |
| II | Strict layering | PASS | No new wire-format bytes. SIGTERM delivery uses the existing PROC_KILL (0x1102) opcode path + existing `SignalInbox::post`; the EINTR wake uses 2a's `pending_wakes` queue + existing `Response::err`; display-server reads fd 3 via the existing SignalChannel `fd_read` path. No new opcode, no new shim, no new ABI constant. The layer test (replace shell without touching kernel) is unaffected. |
| III | Browser-only, zero backend | PASS | No network touchpoints. |
| IV | Offline-first | PASS | No SW / OPFS changes. |
| V | Process isolation | PASS | SIGTERM crosses the process boundary via the kernel (the only legitimate path). Parked pid's user Worker stays blocked on `Atomics.wait` until the kernel pushes the EINTR response — the isolation boundary is load-bearing across the wake. Userland has no mechanism to reach into another pid's memory or kernel state. |
| VI | Standard syscall surface | PASS | `ipc_accept`'s blocking + EINTR semantics match POSIX accept(2). `proc_kill(pid, SIGTERM)` matches POSIX kill(2). The combined "kill interrupts blocking syscall with EINTR" is the standard POSIX contract; 2b implements it for the first time in PMos. |
| VII | Protocol over API | PASS | Display protocol bytes untouched. display-server still speaks the existing 16-byte RGBA payload over `/run/display`; only its accept-loop control flow changes. |
| VIII | Bottom-up | PASS | Kernel gains the interrupt primitive first (§3.1–3.2), then userland (§3.3–3.4) composes it. Each layer tested in isolation before integration — test tier 1 (§4.1) exercises the kernel path without any userland binary; test tier 2 (§4.2) exercises the kernel-wasm-host surface; tier 3 (§4.3–4.4) exercises the full stack. |
| IX | Performance budget | PASS — **improves** | Deleting the `MAX_POLLS = 10_000` busy-poll cuts display-server's worst-case CPU on a missed-accept from 10 000 loop iterations per client gap to 0 (the Worker parks on `Atomics.wait` until the kernel pushes either the accept result or the EINTR wake). Same cache-line / ring-buffer touches as 2a measured. Cold-start < 10 s / warm < 3 s / input < 100 ms budgets all untouched — display-server is not on the input path. |
| X | Testability at every layer | PASS | +3 kernel tests (§4.1), +1 dispatcher test (§4.2), 1 updated vitest composition test (§4.3), 1 updated Playwright test (§4.4). Kernel tests pass before the dispatcher test is written; dispatcher test passes before Playwright's SIGTERM-path assertions exercise. Test-first cadence preserved. |

**No new deviations introduced.** 2a's one-parker-per-listener deviation (docs `syscalls.md:223-226`) is unchanged. The `accept_flags::NONBLOCK` constant from 2a stays orthogonal to SIGTERM interruption — NONBLOCK callers never park, so EINTR is unreachable for them.

**Deviations closed:** `contracts/syscalls.md:213`'s "**or the process is signalled**" clause becomes true for the first time (2a documented, 2b implements).

## 8. Slice 2c / arc close-out outline (forward context, not a commitment)

The "long-running display-server accept loop" arc is substantially closed with 2b. Items deferred out of the arc that could be picked up as follow-up slices:

- **init `proc_wait` supervision loop.** Replace init's fire-and-forget + single-SIGTERM with a real supervisor: `proc_wait` each spawned child, reap zombies, log exit statuses. Requires a blocking-`proc_wait` kernel primitive similar to 2a's blocking `ipc_accept` — same park/wake pattern, different block_reason variant (`BlockReason::Wait` already exists at `crates/kernel/src/proc/mod.rs:110`). Once landed, init becomes a POSIX-shaped init(1): spawn → wait → respawn-on-crash. Estimated 1–2 sessions.
- **`runAllSpawns` markRunning quirk fix.** The sequential vitest harness leaves spawned pids in `Ready` rather than transitioning them to `Running` like production's `kernel-worker-entry`. This forces the exit-12-on-ESRCH divergence documented at `user-wasm-runtime.test.ts:2202-2215`. Fixing it (add a `markRunning` call inside `runAllSpawns`) would let the vitest test exercise the same blocking-accept + connect-wake path Playwright does, closing the last remaining vitest-vs-Playwright behavioural divergence for display-server. Estimated single-slice.
- **Other blockable syscalls.** `fd_read` on empty pipes + `ipc_connect` against an unbound/unlistening path are the two most-tempting extensions of 2a's pattern — the pipe code already has `waiting_readers` / `waiting_writers` Vecs at `crates/kernel/src/ipc/pipe.rs` ready to be wired through. A separate arc whose first slice is blocking `fd_read` mirrors 2a's shape almost exactly.
- **Multi-parker per listener lift.** v1's one-parker invariant (`crates/kernel/src/ipc/socket.rs:105-108`) is fine for display-server but blocks any real networking server. Lifting it is `Option<(Pid, u32)>` → `VecDeque<(Pid, u32)>` with a FIFO wake on each connect. Estimated single-slice, low-risk once a second caller appears.
- **SIGINT / SIGPIPE interruption.** Extend `handle_proc_kill`'s interrupt-parked-accept branch from SIGTERM-only to any catchable signal. Trivial match-arm extension; tests mirror 2b's structure. Defer until a caller needs it.

None of these is blocking the v1 userland story. Arc-level close: 2b ships the last critical piece.
