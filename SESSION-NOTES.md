# Session Notes

Working notes carried between Claude Code sessions. Short-lived by design — delete or rotate entries once the work they describe has landed.

## Last commit

`2948bea feat(001-browser-os-v1): toolkit::layout::{Row, Column} + integration test.`

Branch `001-browser-os-v1`. Workspace totals at this commit: **Rust 779 / TS 212 / 991 passing, 0 failing.**

Recent slice history (newest first):

- `2948bea` — `toolkit::layout::{Row, Column}` + 17 unit tests + 2 integration tests.
- `0a9bdf4` — `toolkit::widget::Button` + `Alignment` promoted out of `Label` + WindowFrame's close button refactored onto `Button`.
- `35d6742` — `toolkit::widget::Label` + `fit_text_to_width` helper extracted from WindowFrame.
- `a51ed2b` — `toolkit::widget::WindowFrame` + two-term integration test.
- `b15a2d8` — `tasks.md` reconciliation + Cargo.lock + .specify/\* harness committed.

## Next slice: `preferred_size()` (Option A)

Defer-until-next-session decision made after the Layout slice landed. The integration test for Layout had to hand-compute widget widths (`label_left_w = 36` with a comment explaining "6 chars × CELL_WIDTH"), which is the first concrete data point that `preferred_size()` has real ergonomic value.

**Design choice: Option A (hint-only, caller decides).** Considered three shapes:

- **(A)** `fn preferred_size(&self) -> (u32, u32)` on each widget. Caller passes the result into `Row::next` or overrides it. Non-binding, no trait, no coupling.
- (B) Layout calls into widgets via a `Widget` trait. Rejected for this slice — couples Layout to a widget trait that doesn't exist yet.
- (C) Builder methods on `Row` like `row.label("left")`, `row.button("ok")`. Rejected — ties Layout to the widget catalogue.

A future slice can add (B) on top of (A) without taking anything away.

**Spec for the slice** (from user, verbatim intent):

- Add `fn preferred_size(&self) -> (u32, u32)` to `Label` and `Button`.
- **Label**: text width = `chars * CELL_WIDTH` plus ~2–4 px horizontal pad; height = `GLYPH_HEIGHT` plus ~4–6 px vertical pad.
- **Button**: caption width plus ~6–8 px horizontal pad per side; height = `GLYPH_HEIGHT` plus ~6–8 px vertical pad total.
- Document values as "design defaults, easy to override later via setters if a real app demands it."
- Tests cover empty text, short text, text wider than any reasonable container.
- Update `crates/integration-tests/tests/layout_with_widgets.rs` to use `widget.preferred_size()` instead of hand-computed widths.
- Target: ~50 lines of code, ~6 tests, under an hour to land.
- Commit message: `feat(001-browser-os-v1): Label and Button preferred_size().`

**Do NOT bundle into the term migration slice.** The user was explicit: `preferred_size` first, then a fresh session for term.

## The big one after preferred_size: `term::Session` migration onto `Canvas`

Deserves its own focused session. Complex and has test-suite / integration-test / cross-crate touch points. Do not try to combine with anything else.

**Motivation**: closes the BGRA rasterizer Known Deviation, fixes the byte-order pin the `two_term_windows` integration test carries, and moves term onto the same rendering path as every other toolkit widget.

**Scope outline — 8 numbered items sketched during the Layout slice**:

1. Delete `crates/term/src/rasterizer.rs` (253 lines) entirely, along with its BGRA byte-order bug (`fill_bg` / `set_pixel` currently write `[b, g, r, a]`).
2. Replace it with something like `term::session::render(&TerminalSnapshot, &mut Canvas, bounds: Rect)` that walks the snapshot and calls `Canvas::draw_text` + `Canvas::fill_rect` for the cursor block.
3. Update `crates/term/src/lib.rs` to stop re-exporting `rasterize_snapshot`, `rasterize_snapshot_with_palette`, `Palette`, `colors`, `PADDING`, `BYTES_PER_PIXEL`.
4. Hunt down every caller of those re-exports. Known touch points: `crates/integration-tests/tests/two_term_windows.rs`, possibly `crates/integration-tests/tests/term_over_display_server.rs`, and any TS counterparts under `web/src/shared/rasterizer.ts`. Grep before starting — I may have missed some.
5. Fix `two_term_windows.rs::TERM_BG_BGRA` → RGBA order. Drop the "byte-order mismatch" comment block. The test should pin clean RGBA behaviour now.
6. Update the **Known Deviations** block in `specs/001-browser-os-v1/tasks.md` to close the BGRA rasterizer entry (item 4 currently: "term crate + TS preview slice"). Note the term crate is now partially reconciled — the rasterizer half is done, the Session-over-IPC half still waits on T110. Preserve the half that's still a deviation.
7. Verify `crates/term/src/session.rs` (497 lines, the protocol-level term client) doesn't need changes. Likely it doesn't — it's the wire-protocol side, not the paint side — but grep for `rasterize_snapshot` there before assuming.
8. Regression check: every existing term test (`crates/term/tests/rasterizer.rs`, `crates/term/tests/terminal.rs`) must either continue to pass or be rewritten in place. `rasterizer.rs` test will almost certainly need deletion or rewrite against the new `render` entry point.

**Open design question for the migration slice** (not yet decided): whether `term` should grow a widget-like entry point (a `toolkit::widget::Term` struct, or a `term::Session::draw(&mut Canvas, Rect)` method) so it composes with `WindowFrame::content_rect()` more naturally than a freestanding `render()` function + manual caller-side plumbing. Decide during the slice based on what the callers actually want.

**Performance note** (Principle IX): term rendering is on the cold-boot + input-latency critical path. The migration shouldn't regress either budget. The new Canvas path should be at least as fast as the old rasterizer (same inner loop, same font lookups, no extra allocations per frame). Spot-check with a microbenchmark if the structure ends up meaningfully different.

## Four Known Deviations (tasks.md Phase 2) — current state

From `specs/001-browser-os-v1/tasks.md` Phase 2 Known Deviations block. Status as of `2948bea`:

1. **`crates/display-proto/` crate extraction** — still a deviation, still correct as written. Future protocol work belongs here.
2. **`crates/kernel/src/sys.rs` + `Kernel` struct** — still a deviation, still correct. Opcode dispatcher (T071/T073) will sit on top of this surface, not replace it.
3. **`toolkit/protocol.rs` + `theme.rs`** — `theme.rs` is no longer a stub; it holds `Theme::LIGHT`, `Theme::DARK`, and every slot the widgets use. `protocol.rs` still deserves the note about T114 becoming a facade over it. Consider amending the deviation text once T114 lands.
4. **`crates/term/` + TS preview slice** — still a deviation. The term migration slice (see above) closes the Rust half. The TS preview half (`web/src/shared/rasterizer.ts`, `web/src/terminal.ts`, `web/src/mock-kernel.ts`, etc.) stays a deviation until T091 + T110 land.

## Untracked files to leave alone

- `certs/` — predates the project, left untracked per user instruction. Not in `.gitignore`.
- `SESSION-NOTES.md` — this file. Short-lived; decide whether to commit case-by-case.

## Conventions carried forward

- Commit author identity: `webos dev <dev@webos.local>`. Set per-command via `git -c user.name=... -c user.email=...`, never `--global`.
- Test count delta reported in every commit message.
- Deviation notes in `tasks.md` point at real file paths with a brief reason for the drift, so a future reader grepping for the originally-planned file path can find the code.
- Toolkit is a `std` crate (no `#![no_std]`), so `Box`, `String`, `Rc`, `Cell` are available.
- Canvas writes **RGBA** byte order. The term rasterizer writes **BGRA** — the known bug the migration slice fixes.

## Toolchain bootstrap (fresh sandbox)

Captured on 2026-04-15. The dev sandbox comes up with Rust but **without** `just` or the WASM targets on the initial PATH / rustup profile, so a brand-new Claude Code session has to do this once before `just build` / `just test-*` works:

- **Rust toolchain**: already installed under `~/.cargo/bin` (cargo 1.94.1, rustc 1.94.1). Not on the default `PATH` — prefix every command with `export PATH="$HOME/.cargo/bin:$PATH"` (or set it once at the top of a Bash invocation).
- **WASM targets**: install both via `rustup target add wasm32-unknown-unknown wasm32-wasip1`. `wasm32-unknown-unknown` is for the kernel (cdylib); `wasm32-wasip1` is for every userland crate. (The repo used to reference `wasm32-wasi`; upstream rustup renamed that target to `wasm32-wasip1`. The T029 resolution slice updated every reference.)
- **`just`**: not preinstalled. Run `cargo install just --locked` (takes ~45 s to build on a cold cache; lands in `~/.cargo/bin`). Verified with `just --version` → `just 1.49.0`.
- **Node + npm**: already at `/bin/node` and `/bin/npm`, on the default `PATH`. No action needed for `npx vitest run` / `npx playwright test`.

After the bootstrap, the following layer targets are green:

- `just build` — full pipeline: kernel cdylib + 14 userland crates + esbuild + `xtask assemble-dist` → `dist/`.
- `just test-kernel`, `just test-display-server`, `just test-toolkit`, `just test-drivers` — per-layer isolation tests.
- `cargo test --workspace` — the authoritative full Rust count (787 as of the T029 slice). This covers crates the `just test-*` targets don't touch (integration-tests, term, ring, display-proto, etc.).
- `(cd web && npx vitest run)` — TS unit tests (212).

**The full `just test` meta-target still fails**, but the residual gaps are now downstream, not toolchain:

- `just test-integration` needs Phase 3+ Playwright scaffolding (no `web/tests/integration/` directory exists and no `playwright.config.*`; T127 onward). A vitest/playwright dep conflict also surfaces when Playwright tries to run with no test files — addressing that is part of the same Phase 3 slice.
- `just test-perf` needs T220 (`crates/integration-tests/src/bin/input-latency.rs` — the p95 input-latency harness — does not exist yet).

So "the full test suite" in practice today is:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --workspace          # all Rust crates, host target
(cd web && npx vitest run)      # TS unit tests
```

= 787 + 212 = **999 passing, 0 failing** as of the T029 resolution slice. These are the same numbers the `preferred_size` slice landed — T029 changed build plumbing, not test code.

After the **T073 dispatcher first landing**: 809 + 212 = **1021 passing, 0 failing** (+22 new isolation tests at `crates/kernel/tests/syscall.rs`).

After the **extern-C seam slice** on top of T073: 811 + 229 = **1040 passing, 0 failing**. Rust gained +2 `abi::ring` byte-roundtrip tests; TS gained +17 `kernel-wasm-entry.test.ts` integration tests that load `kernel.wasm` from disk and drive the exports. `kernel.wasm` itself went from 86 bytes to 112 KiB — the exports pinned the whole dispatcher + `Kernel` graph alive through LLVM's dead-code elimination. A pre-existing bump-allocator bug where the cursor started at 0 (clobbering the static data section) was fixed in the same slice; initial allocations now correctly start at `__heap_base`.

After the **KernelWasmHost slice** (T091 TS-side first landing): 811 + 246 = **1057 passing, 0 failing**. TS gained +17 `kernel-wasm-host.test.ts` tests (Rust unchanged; only one new no-op Rust export for `kernel_inject_console_input`). `web/src/kernel-wasm-host.ts` + `web/src/shared/syscall.ts` are the new production modules; the class is not yet wired into `kernel-worker-entry.ts`, which still defaults to `MockKernel` for backwards compatibility. The syscall shared module is hand-maintained against `abi::wasi` / `abi::ext` / `abi::errno` / `abi::cap` / `platform::DevId` and should be promoted to xtask autogeneration (like `sab-layout.ts`) when the opcode set grows.

After the **Worker-spawn Platform-hook slice** (T074 progression + the PROC_SPAWN opcode): 816 + 250 = **1066 passing, 0 failing**. Rust +5 (new `PROC_SPAWN` tests in `crates/kernel/tests/syscall.rs` — happy path, platform-refusal rollback, missing-stdio EINVAL, cap-superset ENOTCAPABLE, invalid-UTF-8 EINVAL). TS +4 (new tests in `kernel-wasm-host.test.ts` exercising the `onSpawnProcess` callback round trip). `kernel.wasm` grew from 112 KiB → 127 KiB because the PROC_SPAWN handler + `Kernel::proc_spawn`'s platform call + rollback path are now reachable from an export. New host import `pmos_host_spawn_process` on the Rust side; new `KernelWasmHostOptions.onSpawnProcess` callback + `encodeSpawnManifest` helper on the TS side. The single biggest unknown on the MVP path (the Worker-spawn seam) is now proven end-to-end in the same sense as T073 and T091: the callback fires, the pid is allocated, the platform hook rolls back cleanly on refusal. What remains is the ACTUAL `new Worker(...)` + SAB allocation + user-wasm loading + WASI-shim path on the TS side — a follow-up slice where `onSpawnProcess` stops being a spy and starts actually spawning a Worker. The kernel-side of PROC_SPAWN is done (for the minimal path + caps manifest shape).

After the **user-wasm runtime slice** (first real user wasm end-to-end): 816 + 252 = **1068 passing, 0 failing**. TS +2 (new tests in `web/tests/unit/user-wasm-runtime.test.ts` — hello-wasi-min runs through a real `KernelWasmHost` and produces "hello from userland\n" on `/dev/console`; plus a synthetic exit-code-only wasm built at test time via hand-emitted binary to prove exit-code propagation). Rust test count unchanged; the new `crates/hello-wasi-min/` cdylib crate is `#[cfg(target_arch = "wasm32")]`-gated everywhere so `cargo test --workspace` sees a native-target no-op and `cargo build --target wasm32-wasip1 -p hello-wasi-min` produces a 265-byte binary with exactly two imports (`wasi_snapshot_preview1.fd_write` + `.proc_exit`) and two exports (`memory` + `_start`). New production modules: `web/src/user-wasm-runtime.ts` (`UserWasmRuntime` class + `KernelBackend` interface + `KernelWasmHostBackend` adapter + `UserProcessExited` unwind sentinel + minimal WASI shim for `fd_write` + `proc_exit`). `just build` now copies the hello binary to `dist/assets/bin/hello_wasi_min.wasm` (manifest entry count: 16 → 17). This is the first slice where a REAL `wasm32-wasip1` user binary runs against a REAL kernel via REAL WASI imports with no mocks on any layer — proves end-to-end that a WASI syscall from userland lands on a kernel device write and the observable effect bubbles back through `KernelWasmHost.onConsoleWrite`.

After the **spawn-callback wiring slice**: 816 + 254 = **1070 passing, 0 failing**. TS +2 (the composition test: `PROC_SPAWN` dispatched on behalf of a virtual parent → default `onSpawnProcess` queues a pending spawn from `binaryRegistry` → `drainPendingSpawns` runs the child to completion; plus the rollback test: missing-binary path returns `-EIO` and does not queue). `KernelWasmHost` now accepts a `binaryRegistry: ReadonlyMap<string, BufferSource>` option; when set and no explicit `onSpawnProcess` is provided, the host installs a default callback that looks the path up and queues `{ pid, path, bytes }` into an internal `pendingSpawns` queue. New public method `kernel.drainPendingSpawns()` pops the queue and runs each entry via a `UserWasmRuntime` + `KernelWasmHostBackend` bound to the spawn's pid — sequential, wait-for-child semantics, reentrant (a child that queues more spawns gets picked up on subsequent loop iterations). The module graph stays acyclic because `user-wasm-runtime.ts` uses `import type` for `KernelWasmHost`. This slice closes the `PROC_SPAWN`-to-running-binary loop: for the first time, a kernel-created pid actually maps to a running wasm process.

After the **two-level composition slice** (reentrancy under real wasm): 816 + 255 = **1071 passing, 0 failing**. TS +1 (two-level composition test). New crate `crates/hello-wasi-spawner/` (322-byte cdylib) imports `wasi_snapshot_preview1.{fd_write,proc_exit}` AND `pmos_ext.proc_spawn`. The `UserWasmRuntime` WASI shim gains a `pmos_ext` import namespace alongside `wasi_snapshot_preview1`; the `pmos_ext.proc_spawn(path_ptr, path_len, caps: u64) -> i32` shim reads the path from user memory, builds a `PROC_SPAWN` manifest via `encodeSpawnManifest`, dispatches through the backend, and returns the new pid (positive) or negative errno. The reentrancy test: init spawns `spawner`, the drain loop runs `spawner` which writes `"spawner alive\n"` and then calls `pmos_ext.proc_spawn("/usr/bin/hello", ...)` mid-run, queuing `hello` into the same pendingSpawns list the drain loop is currently iterating over; spawner exits, the drain loop picks up `hello`, runs it, `hello` writes `"hello from userland\n"` and exits; drain returns. Asserts the capture is `["spawner alive\n", "hello from userland\n"]` in that exact order — the sequential ordering is load-bearing, not incidental, because it proves the spawner fully ran before the child took over. First proof that PMos extension syscalls (not just WASI) can flow from user wasm through the runtime shim into the kernel dispatcher, and the first proof that `drainPendingSpawns` is reentrancy-safe under real wasm execution (not just "in theory"). `just build` manifest entry count: 17 → 18.

After the **kernel opcode breadth slice #1**: 820 + 255 = **1075 passing, 0 failing**. Rust +4 tests; TS unchanged (two existing tests that probed CLOCK_TIME_GET for ENOSYS were swapped to probe FD_SEEK instead, which is still unhandled, plus a stub for `pmos_host_now_ns` was added to `kernel-wasm-entry.test.ts` since CLOCK_TIME_GET's handler pulled the import back into the live set). New kernel opcodes: `CLOCK_TIME_GET` (wraps `Platform::now_ns`), `RANDOM_GET` (wraps `Platform::random_bytes`, fills the heap scratch in place), `SCHED_YIELD` (trivial OK return — required because Rust std's wasi libc calls it from a few spin-wait loops and would panic on `-ENOSYS`). Dispatcher opcode count 10 → 13. This is the first expansion slice where the kernel gains capabilities that every "real Rust std binary" needs at startup — the next slice pairs these with WASI shim entries on the TS side and a real Rust std test binary that exercises all three.

After the **IPC + DISPLAY_CONNECT opcodes slice** (kernel opcode breadth #2): 830 + 255 = **1085 passing, 0 failing**. Rust +10 tests; TS unchanged. Six new opcode handlers on the kernel side: `IPC_SOCKET`, `IPC_BIND`, `IPC_LISTEN`, `IPC_CONNECT`, `IPC_ACCEPT`, `DISPLAY_CONNECT`. Dispatcher opcode count 13 → 19. Four new thin `Kernel` method wrappers (`ipc_socket`, `ipc_bind`, `ipc_listen`, `ipc_connect`) following the existing `display_bind`/`display_connect`/`accept_socket` pattern — each validates fd/pid/cap preconditions then calls into the pre-existing `IpcTable` primitives. Plus a private `Kernel::socket_id_from_fd` helper that centralises the "fd → SocketId" lookup the new wrappers share. `DISPLAY_CONNECT` is a trivial wrapper over the semantic `Kernel::display_connect` method (which already existed). `kernel.wasm` grew 131 KiB → 143 KiB as the IPC handler graph became reachable. The slice's crown jewel test (`ipc_round_trip_server_accepts_client_and_they_exchange_bytes`) runs two processes in one kernel — server binds + listens + accepts, client connects, both ends exchange bytes via the existing `FD_READ` / `FD_WRITE` opcodes (which already route `FdObject::Socket` to the right path). First proof that generic IPC works end-to-end through the opcode layer. What remains: the TS-side shim entries for each of the 6 new opcodes, and — as the real payoff — a no_std server/client pair of user wasm binaries that exchange bytes through a real `KernelWasmHost`. That's the follow-up slice where the display-server runway actually opens up.

After the **IPC-through-real-wasm slice** (TS shim + `ipc-self-test` binary + composition test): 830 + 256 = **1086 passing, 0 failing**. TS +1 (new composition test). Rust unchanged (the new crate is `#[cfg(target_arch = "wasm32")]`-gated like the earlier `hello-wasi-*` crates, so `cargo test --workspace` sees a native-target no-op). New crate `crates/ipc-self-test/` (772-byte cdylib) imports 8 functions across two namespaces — `wasi_snapshot_preview1.{fd_write, fd_read, proc_exit}` + `pmos_ext.{ipc_socket, ipc_bind, ipc_listen, ipc_connect, ipc_accept}` — and plays both server and client via self-connection in a single `_start` pass. Self-connection works because the kernel's IPC state machine indexes sockets by `SocketId`, not by pid. TS shim additions: `wasi_snapshot_preview1.fd_read` (scatter-gather mirror of the existing `fd_write`, one kernel dispatch with total capacity, distribute bytes across iovecs); `pmos_ext.{ipc_socket, ipc_bind, ipc_listen, ipc_connect, ipc_accept}` (five new PMos-extension shims that translate each call into the corresponding opcode). `shared/syscall.ts` `OP_EXT` table now covers 12 opcodes (was 5). `just build` manifest entry count 18 → 19. First proof that every IPC opcode works through REAL user wasm — not just direct `KernelWasmHost.dispatch` calls from a Rust test. The binary's exit code is the step-by-step diagnostic (codes 10-18 for each failed step; 0 for green), and the test's observable — the received bytes echoed to `/dev/console` via the final `fd_write(1, ...)` — proves the bytes actually traversed the kernel's IPC state machine. This closes the "runway to display server" — what remains is: a no_std display-server-ish binary that binds `/run/display` via the now-working IPC opcodes, plus the framebuffer-driver TS-side wiring that routes `pmos_host_driver_call(Framebuffer, ...)` into `fb.ts`'s canvas blit.

After the **framebuffer pipeline slice** (path_open WASI shim + onFramebufferWrite callback + hello-framebuffer binary): 830 + 257 = **1087 passing, 0 failing**. TS +1 (framebuffer acceptance test). Rust unchanged. New crate `crates/hello-framebuffer/` (379-byte cdylib) imports exactly three WASI functions (`path_open`, `fd_write`, `proc_exit`) and writes 16 bytes of RGBA pixel data (red/green/blue/white) to `/dev/fb0`. The kernel routes `fd_write` on `FdObject::CharDevice(DEV_FB0)` through `DeviceDispatcher::framebuffer_write` → `Platform::driver_call(DevId::Framebuffer, ...)` → `pmos_host_driver_call` host import → `KernelWasmHost`'s routing closure → `options.onFramebufferWrite(bytes)` callback. TS shim additions: `wasi_snapshot_preview1.path_open` (9-arg WASI signature collapsed to a PMos PATH_OPEN dispatch with just `path` + `flags=0`; the other WASI args — dirfd, dirflags, oflags, rights_base/inheriting, fdflags — are ignored for now since the kernel's path_open semantics don't yet distinguish them). `KernelWasmHostOptions` gains `onFramebufferWrite`; the `pmos_host_driver_call` closure now routes `DEV.FRAMEBUFFER` as well as `DEV.CONSOLE` (input/block/net still unrouted but accept the call without error). `just build` manifest entry count 19 → 20. This is the first slice where user wasm sends bytes the TS host recognizes as "pixels destined for the framebuffer" — a Playwright-level test could take these bytes and actually blit to canvas; in vitest we only prove the bytes reach the callback intact. Combined with the IPC slice before it, the "pixels via the constitutional path" runway is now really short: what's left is a Rust display-server-ish binary that owns `/run/display` + receives client draw commands + composites + writes resulting pixels to `/dev/fb0`, then Playwright/browser-side wiring of `onFramebufferWrite` to an actual canvas blit.
