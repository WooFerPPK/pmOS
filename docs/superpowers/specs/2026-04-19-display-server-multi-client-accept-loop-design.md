# display-server multi-client outer accept loop — design

Date: 2026-04-19
Status: approved
Related: `specs/001-browser-os-v1/plan.md`, `SESSION-NOTES.md` (runway items a/b/c), commit f03cf74 (`fd_close` shim)
Slice shape: single session, single commit (Alt 1 in the brainstorming dialogue)

## 1. Problem

`crates/display-server/src/main.rs` is currently a **single-shot** binary: it binds `/run/display`, accepts exactly one client, relays the client's 16-byte RGBA payload to `/dev/fb0`, and exits. A real display server serves N clients over its lifetime.

The SESSION-NOTES "long-running display-server accept loop" runway item breaks into three coupled changes:

1. `ipc_accept` blocking semantics (today returns `-EAGAIN` on empty backlog; the shim busy-polls).
2. init-side supervision via `proc_wait` so a supervisor can respawn a crashed display-server.
3. Per-client teardown on the server side without exiting the server process.

This slice addresses **only (3)** — the smallest architectural step that moves the loop shape forward without touching the kernel seam and without dragging in supervision or signal machinery.

## 2. Non-goals

Explicitly deferred to later slices of the same arc:

- Kernel `ipc_accept` blocking semantics (slice 2 of the arc).
- init `proc_wait` supervision / respawn loop (slice 4).
- Signal-driven server exit — needs a SignalChannel poll between accepts (slice 3; itself depends on slice 2 to wake the server).
- Raising or removing the `MAX_CLIENTS = 4` ceiling — stays a testing fiction until slice 2 lands.
- Rebuilding `kernel.wasm` — the kernel seam is unchanged by this slice.

## 3. Design

### 3.1. `crates/display-server/src/main.rs` reshape

```
main:
  listener = display_bind()                                      [-> 10 on fail]
  fb_fd    = path_open("/dev/fb0")                               [-> 15 on fail]        // opened once, reused per client
  served_any = false
  for i in 0..MAX_CLIENTS (= 4):
    server = ipc_accept[bounded EAGAIN poll on listener]          [-> 12 on non-EAGAIN err]
    if accept poll exhausts:
      if served_any: break                                        // clean drain, exit 0
      else:          exit 17                                      // unchanged from today
    fd_read(server, recv_buf)[bounded EAGAIN poll]                [-> 14, 18]
    fd_write(fb_fd, recv_buf[..nread])                            [-> 16]
    fd_close(server)                                              [-> 19]
    served_any = true
    println!("display-server served client {i}")                  // new per-iteration line
  println!("display-server fb blit ok")                           // kept as trailing line
  exit 0
```

Key moves:

1. `path_open("/dev/fb0")` lifts out of the per-client body. Opened once at startup, reused for every served client.
2. New `for i in 0..MAX_CLIENTS` outer loop. `MAX_CLIENTS = 4` — a placeholder testing ceiling, documented in-code as a deviation to close when slice 2 lands.
3. New per-iteration `fd_close(server)` with a positive-errno check. `fd_close` is WASI; shim pre-landed in commit f03cf74. New exit code **19** for "fd_close on client fd returned non-zero errno".
4. New clean-drain semantic: if the accept EAGAIN poll exhausts AFTER serving ≥ 1 client, break and exit 0. If it exhausts on iteration 0, keep the existing **17** behaviour (used by the sequential `runAllSpawns` harness to terminate without a concurrent client).
5. New per-iteration `"display-server served client {i}"` line (0-indexed). Provides Playwright an observable ordering checkpoint for per-client completion.
6. `fd_close` joins the existing `wasi_snapshot_preview1` extern block.

### 3.2. `crates/init/src/main.rs` second client

Add one additional `proc_spawn("/bin/display-client-demo", ...)` call immediately after the existing one. Two identical spawns → two identical `init spawned display-client-demo pid=<dynamic>` lines → init's console output grows by one line. `hello-std` and `display-server` spawns are unchanged.

### 3.3. Exit-code table

| Code | Meaning | Status |
|-----:|:---|:---|
|   0 | success — served ≥ 1 client, outer loop drained or hit `MAX_CLIENTS` | **reshaped** (was: served exactly 1 client) |
|  10 | `display_bind` failed | unchanged |
|  12 | `ipc_accept` non-EAGAIN error | unchanged |
|  14 | `fd_read` non-EAGAIN / read-0-bytes error | unchanged |
|  15 | `path_open("/dev/fb0")` failed | unchanged |
|  16 | framebuffer `fd_write` failed / short-wrote | unchanged |
|  17 | first `ipc_accept` poll exhausted (0 clients served) | unchanged |
|  18 | `fd_read` poll exhausted for a connected client | unchanged |
|**19**| `fd_close(server)` returned non-zero errno | **new** |

`fd_close` surfaces positive errno per the WASI convention; any non-zero return hits exit 19. The negative-errno / positive-errno split stays consistent with the pre-slice pattern (PMos-ext negate, WASI positive).

### 3.4. fd lifecycle invariants

- `listener` fd: allocated once in `display_bind`, closed only by kernel at `proc_exit`.
- `fb_fd`: allocated once in `path_open`, closed only by kernel at `proc_exit`. No per-client reopen.
- `server` fd: allocated per iteration by `ipc_accept`, closed by the new `fd_close(server)` at end of iteration. **No fd-table leak across iterations.**

## 4. Testing

Three-layer coverage, matching the workspace convention.

### 4.1. Kernel isolation (Rust — no new test)

`crates/kernel/tests/sys.rs:1385 multiple_clients_accept_into_distinct_server_side_fds` already pins that a single listener can be accepted twice and that the two server-side fds are distinct. The kernel seam is unchanged by this slice.

### 4.2. TS dispatcher (vitest — updated, not added)

`web/tests/unit/user-wasm-runtime.test.ts:2118` — the "init (std) spawns three children" test absorbs the new shape:

- `history.length` 4 → 5.
- `history[2]!.exitCode` stays 17 (display-server poll exhausts with 0 served under sequential `runAllSpawns`).
- `history[3]!` and `history[4]!` are both display-client-demo with exit 10 (connect poll exhausts; server has already torn down).
- Console `lines.length` 8 → 10 (two `init spawned display-client-demo pid=…` lines + two `display-client-demo starting` lines).
- `fbWrites.length` stays 0 (no concurrent IPC under sequential `runAllSpawns`).

The existing binary-registry map (path → bytes) dedups the display-client-demo entry naturally; two `proc_spawn` calls map to the same wasm bytes.

### 4.3. Playwright integration (updated, not added)

`web/tests/integration/real-kernel.spec.ts` — pin the two-client round-trip:

- Expect two `init spawned display-client-demo pid=<dynamic>` lines.
- Expect two `display-server served client <index>` lines (indices 0 and 1).
- Expect two `display-client-demo sent pixels` lines.
- Expect the trailing `display-server fb blit ok` line AFTER both served-client lines.
- Expect two framebuffer writes, each 16 bytes, both matching the shared `PIXELS` constant (`[0xff, 0, 0, 0xff, 0, 0xff, 0, 0xff, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]`).

Client ordering under Playwright is not pinned — both clients run concurrently and either may arrive first. The `{index}` in the served-client line is display-server's outer-loop iteration, not the client pid, so the assertion is well-defined on the server's internal sequence.

### 4.4. Test-count delta

- Rust: 1191 → 1191 (+0, no new native tests).
- TS-unit: 493 → 493 (+0, existing test updated in place).
- Playwright: 2 → 2 (+0, existing spec updated in place).

The test count stays flat; the existing tests' assertions strengthen.

## 5. Constitution check

| # | Principle | Status | Rationale |
|---|---|---|---|
| I | Real OS, not a simulation | PASS | Kernel gets no UI additions; display-server remains userland. The outer-loop shape is purely a userland concern. |
| II | Strict layering | PASS | Kernel seam unchanged; toolkit not touched; shell replaceable. Display-server still sits atop syscalls it already uses. |
| III | Browser-only, zero backend | PASS | No network, no deployment changes. |
| IV | Offline-first and persistent | PASS | No service-worker or OPFS changes. |
| V | Process isolation | PASS | Per-iteration `fd_close(server)` is the load-bearing proof that per-client fd-table state is released between clients. Each of the two display-client-demo pids runs in its own WASM linear memory. |
| VI | Standard syscall surface | PASS | No new opcodes. Reuses `ipc_accept` (0x1006), `display_bind` (0x1200), `fd_close` (0x0004), `fd_read`, `fd_write`, `path_open`. |
| VII | Protocol over API | PASS | The Wayland-inspired wire protocol bytes are untouched. This slice is about client count, not protocol shape. |
| VIII | Bottom-up construction | PASS | No new crate. Operates within two existing crates (`display-server`, `init`) + two existing tests. Kernel layer already built. |
| IX | Performance budget | PASS | Per-client overhead = one `fd_close` = O(1). `path_open("/dev/fb0")` moves out of the per-iteration body → strictly fewer syscalls than a naïve per-client reopen. Cold-boot impact ~nil. Input-latency budget unaffected (input path does not traverse display-server's accept loop). |
| X | Testability at every layer | PASS | Three-layer coverage preserved: kernel-isolation existing, vitest updated, Playwright updated. |

**Deviation introduced:** `MAX_CLIENTS = 4` is a testing fiction. Documented in-code with a reference to the follow-up slice (kernel `ipc_accept` blocking + signal-driven exit) that removes it.

## 6. Slice 2+ outline (context, not a commitment)

For the reader's orientation — these are outside this slice:

- **Slice 2:** kernel `ipc_accept` blocking semantics. Replace the `-EAGAIN` response with a per-socket wait queue on the listener. Teach the scheduler to park the caller and wake on connect. Remove the `MAX_POLLS` busy-poll from display-server's accept call.
- **Slice 3:** signal-driven exit. display-server polls fd 3 (the SignalChannel) between accepts; exits cleanly on SIGTERM. Remove the `MAX_CLIENTS` ceiling from the outer loop.
- **Slice 4:** init `proc_wait` supervision. init loops over `proc_wait(-1, 0, &status)` and respawns critical children (currently: display-server).

These four slices together resolve the "long-running display-server accept loop" runway item. This slice is the precondition for all of them.
