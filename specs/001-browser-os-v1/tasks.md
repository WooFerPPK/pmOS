---
description: "Task list for feature 001-browser-os-v1"
---

# Tasks: Browser OS v1 — Initial Release

**Input**: Design documents from `/opt/webos/specs/001-browser-os-v1/`
**Prerequisites**: `plan.md`, `spec.md`, `research.md`, `data-model.md`, `contracts/` (syscalls, display-protocol, driver-kernel, package-manifest, init-conf), `quickstart.md`, `.specify/memory/constitution.md`

**Tests**: Principle X of the project constitution **mandates** isolation tests at every layer before integration. Test tasks are therefore **not optional** in this feature — they are constitutional requirements. Integration tests (Playwright) are also mandatory per Principle X's "integration tests cover the full stack" clause.

**Organization**: Tasks are grouped by phase. Phase 1 is Setup, Phase 2 is Foundational (all of kernel + drivers + display server + toolkit + init — bottom-up per Principle VIII). Phases 3–12 are one-per-user-story in priority order, each delivering a user-visible increment on top of the foundation. Phase 13 is Polish.

## Format: `[ID] [P?] [Story?] Description`

- **[P]**: Can run in parallel (different files, no dependencies on incomplete tasks in the same phase)
- **[Story]**: Which user story this task belongs to (US1–US10); Setup / Foundational / Polish tasks have no story label
- Every task includes exact file path(s) in its description

## Path Conventions

Paths are repository-relative (repo root is `/opt/webos/`). The Rust workspace lives at the root (`Cargo.toml` + `crates/`). The TypeScript tree lives under `web/`. The final static deploy directory is `dist/` (gitignored). Spec/plan/contracts live under `specs/001-browser-os-v1/`.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Stand up the build system, workspace, toolchains, dev server, and empty crate/module skeletons so Phase 2 tasks can proceed independently and in parallel without worrying about project scaffolding.

- [x] T001 Create Cargo workspace root `Cargo.toml` listing every crate in `crates/` (abi, ring, kernel, init, display-server, toolkit, shell, sh, term, files, edit, settings, sysmon, sample-app, toolkit-free-client, integration-tests, xtask); set `resolver = "2"`
- [x] T002 Add `rust-toolchain.toml` pinning stable and adding targets `wasm32-unknown-unknown` and `wasm32-wasi`
- [x] T003 Create `Justfile` with targets: `build`, `dev`, `test`, `test-kernel`, `test-display-server`, `test-toolkit`, `test-drivers`, `test-integration`, `clean`, `package sample-app`, `push-sample`
- [x] T004 Create `.gitignore` covering `target/`, `build/`, `dist/`, `node_modules/`, `.playwright/`, `*.pmpkg.tar`
- [x] T005 [P] Create `crates/abi/Cargo.toml` and `crates/abi/src/lib.rs` stub (no_std, no deps)
- [x] T006 [P] Create `crates/ring/Cargo.toml` and `crates/ring/src/lib.rs` stub (depends on abi)
- [x] T007 [P] Create `crates/kernel/Cargo.toml` (bin + lib for testability; target wasm32-unknown-unknown; `no_std` + `alloc` feature-gated) and stub `crates/kernel/src/main.rs` + `crates/kernel/src/lib.rs`
- [x] T008 [P] Create `crates/init/Cargo.toml` (target wasm32-wasi) and `crates/init/src/main.rs` stub
- [x] T009 [P] Create `crates/display-server/Cargo.toml` (target wasm32-wasi) and `crates/display-server/src/main.rs` stub
- [x] T010 [P] Create `crates/toolkit/Cargo.toml` (library crate) and `crates/toolkit/src/lib.rs` stub
- [x] T011 [P] Create `crates/shell/Cargo.toml` (the desktop shell — `/usr/bin/shell`) and `crates/shell/src/main.rs` stub
- [x] T012 [P] Create `crates/sh/Cargo.toml` (the CLI shell — `/usr/bin/sh`) and `crates/sh/src/main.rs` stub
- [x] T013 [P] Create `crates/term/Cargo.toml` and `crates/term/src/main.rs` stub
- [x] T014 [P] Create `crates/files/Cargo.toml` and `crates/files/src/main.rs` stub
- [x] T015 [P] Create `crates/edit/Cargo.toml` and `crates/edit/src/main.rs` stub
- [x] T016 [P] Create `crates/settings/Cargo.toml` and `crates/settings/src/main.rs` stub
- [x] T017 [P] Create `crates/sysmon/Cargo.toml` and `crates/sysmon/src/main.rs` stub
- [x] T018 [P] Create `crates/sample-app/Cargo.toml` and `crates/sample-app/src/main.rs` stub
- [x] T019 [P] Create `crates/toolkit-free-client/Cargo.toml` and `crates/toolkit-free-client/src/main.rs` stub (no dependency on the toolkit crate)
- [x] T020 [P] Create `crates/xtask/Cargo.toml` and `crates/xtask/src/main.rs` with subcommand dispatch for `assemble-dist`, `dev-server`, `gen-sab-layout`, `package`. Also create `crates/integration-tests/Cargo.toml` and `crates/integration-tests/src/lib.rs` as an empty native-Rust integration-test harness crate (to be populated by T220 with `perf/input-latency.rs`; host-target builds only, not compiled for wasm)
- [x] T021 [P] Create `web/package.json` with dev dependencies: `esbuild`, `typescript`, `vitest`, `@playwright/test`; no runtime deps
- [x] T022 [P] Create `web/tsconfig.json` with `strict: true`, target `ES2022`, module `ESNext`
- [x] T023 [P] Create `web/index.html` with a single top-level `<canvas id="pmos-fb">` element and a `<script type="module" src="./assets/bootstrap.js">` tag
- [x] T024 [P] Create `web/src/bootstrap.ts` skeleton that registers the service worker, sets up the canvas element, and prints `crossOriginIsolated` to the console (T085 will complete it)
- [x] T025 [P] Create `web/src/sw.ts` skeleton with an install handler that opens a versioned cache (actual precache list is populated in T087)
- [x] T026 Implement `crates/xtask/src/assemble_dist.rs`: copies kernel, init, display-server, and bundled-app WASM into `dist/assets/`, runs `esbuild` on `web/src/bootstrap.ts` and `web/src/sw.ts`, copies `web/index.html`, writes `dist/_headers` with `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp`
- [x] T027 Implement `crates/xtask/src/dev_server.rs`: serves `dist/` on `http://localhost:8080` with COOP/COEP headers and no-caching
- [x] T028 [P] Create `.github/workflows/ci.yml` running `just test` (all layers) on every push
- [ ] T029 Verify `just build` produces a complete `dist/` tree and `just dev` boots to a blank canvas with `crossOriginIsolated === true` logged to the devtools console

**Checkpoint**: the repo builds, the dev server serves COOP/COEP headers, every crate compiles as a stub, and `cargo test --workspace` runs (against stub code).

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: build kernel + drivers + display server + toolkit + init with isolation tests at every layer. This is the bottom-up construction gate (Principle VIII) and the layer-isolation test gate (Principle X). **No user story in Phase 3+ can begin until this phase is complete and its isolation tests are green.**

**⚠️ CRITICAL**: every task in this phase MUST land with its tests. A task that introduces behaviour without a matching isolation test is rejected per Principle X.

### ABI and ring-buffer transport (the two-ended contract)

- [x] T030 Define WASI preview 1 opcode constants, errno values, and record layouts in `crates/abi/src/wasi.rs` mirroring `contracts/syscalls.md §2`
- [x] T031 [P] Define extension syscall opcodes, `SpawnManifest`, signal numbers, and the `ENOTCAPABLE`/`ENOABIVER` additions in `crates/abi/src/ext.rs` mirroring `contracts/syscalls.md §3`
- [x] T032 [P] Define the `Cap` enum, `CapSet` bitset, and default cap grants per role in `crates/abi/src/cap.rs` mirroring `data-model.md §5`
- [x] T033 [P] Define `Request`/`Response` record shapes and magic status values (`IDLE`, `REQUESTED`, `SERVICING`, `READY`) in `crates/abi/src/ring.rs` mirroring `contracts/driver-kernel.md §1`
- [x] T034 Declare `ABI_VERSION = (1, 0)` and a `check_abi` helper in `crates/abi/src/version.rs`
- [x] T035 Implement the SAB ring-buffer producer/consumer sides in `crates/ring/src/lib.rs` using raw shared-memory atomics (producer: push request + notify; consumer: pop request, service, push response + notify)
- [x] T036 [P] Unit tests for ring-buffer ordering, wraparound, and atomic slot transitions in `crates/ring/tests/ring.rs`
- [x] T037 Generate `web/src/shared/sab-layout.ts` from the abi crate's constants via `xtask gen-sab-layout` and write the generator in `crates/xtask/src/gen_sab_layout.rs`
- [x] T038 [P] Unit test in `web/tests/unit/sab-layout.test.ts` asserting that every offset/constant in `sab-layout.ts` matches what `abi` emits (guards against silent drift)

### Kernel platform abstraction (for native testability per Principle X)

- [x] T039 Define the `Platform` trait (driver transport, clock, shutdown, panic hook) in `crates/kernel/src/platform/mod.rs`
- [x] T040 [P] Implement `NativePlatform` for `cargo test` (in-process driver stubs, monotonic clock, panic → `panic!`) in `crates/kernel/src/platform/native.rs` (gated `cfg(not(target_arch = "wasm32"))`)
- [x] T041 [P] Implement `WasmPlatform` for the `wasm32-unknown-unknown` runtime target in `crates/kernel/src/platform/wasm.rs` (gated `cfg(target_arch = "wasm32")`)
- [x] T042 Wire up a minimal `alloc` crate allocator (bump or linked-list) in `crates/kernel/src/alloc.rs` with a `#[global_allocator]` for the wasm target

### Kernel process table, fd table, scheduler

- [x] T043 Implement `Process`, `ProcState`, `ExitStatus` in `crates/kernel/src/proc/mod.rs` per `data-model.md §1`
- [x] T044 Implement PID allocator + process table in `crates/kernel/src/proc/table.rs`
- [x] T045 Implement cooperative scheduler + ready-queue in `crates/kernel/src/proc/sched.rs`
- [x] T046 [P] Process-table isolation tests (create / ready / running / blocked / zombie / dead transitions) in `crates/kernel/tests/proc.rs`
- [x] T047 Implement `FdTable`, `FdEntry`, `FdObject` in `crates/kernel/src/fd/mod.rs` per `data-model.md §2`
- [x] T048 [P] fd-table tests (allocation lowest-free, close releases, dup, O_CLOEXEC) in `crates/kernel/tests/fd.rs`

### Kernel VFS and in-memory filesystems

- [ ] T049 Define `Vnode`, `NodeType`, `Filesystem` trait in `crates/kernel/src/vfs/mod.rs` per `data-model.md §3.1`
- [ ] T050 Implement the mount table in `crates/kernel/src/vfs/mount.rs`
- [ ] T051 Implement path resolution (absolute, cwd-relative, symlink traversal, `..` handling) in `crates/kernel/src/vfs/path.rs`
- [ ] T052 [P] tmpfs implementation in `crates/kernel/src/fs/tmpfs.rs`
- [ ] T053 [P] devfs implementation (node registry, open/read/write dispatch to device drivers) in `crates/kernel/src/fs/devfs.rs`
- [ ] T054 [P] procfs implementation reading from process table, exposing `/proc/<pid>/{status,cmdline,environ,fd,comm,stat}` and `/proc/{version,uptime,meminfo,loadavg,storage}` per `data-model.md §10`
- [ ] T055 [P] VFS isolation tests (mount, lookup, read/write, readdir, cross-mount traversal) in `crates/kernel/tests/vfs.rs`

### Kernel OPFS-backed root filesystem

- [ ] T056 Implement the on-disk superblock / inode / extent layout in `crates/kernel/src/fs/opfs/layout.rs` matching `data-model.md §3.3`
- [ ] T057 Implement OPFS block-device client (talks to the TS block driver via the driver control channel) in `crates/kernel/src/fs/opfs/block.rs`
- [ ] T058 Implement the journaling layer (ring journal, replay on mount, atomic superblock commit) in `crates/kernel/src/fs/opfs/journal.rs` to satisfy FR-014
- [ ] T059 Implement the OPFS `Filesystem` trait front-end (lookup, read, write, create, unlink, rename, readdir) in `crates/kernel/src/fs/opfs/mod.rs`
- [ ] T060 Implement `mkfs` (first-boot initialiser): allocates root inode, creates `/bin`, `/etc`, `/dev`, `/proc`, `/run`, `/tmp`, `/home/user`, `/opt`, `/usr/bin`, `/usr/share/applications`, copies bundled binaries from the embedded initramfs, AND installs the FR-013a starter kit (`/home/user/README.md`, `/home/user/Downloads/`, `/home/user/Documents/welcome.txt`, `/home/user/Documents/editing.md`, `/home/user/Pictures/`) in `crates/kernel/src/fs/opfs/mkfs.rs`
- [ ] T061 [P] OPFS client tests against a mock block device (round-trip write/read, journal replay after simulated crash, mkfs produces the expected tree) in `crates/kernel/tests/opfs.rs`

### Kernel IPC (pipes and unix-socket-equivalents)

- [ ] T062 Implement `Pipe` with kernel-owned ring buffer in `crates/kernel/src/ipc/pipe.rs`
- [ ] T063 Implement `Socket` (STREAM + DGRAM) with bind, listen, connect, accept, send, recv, and fd-passing queue in `crates/kernel/src/ipc/socket.rs` per `data-model.md §4.2`
- [ ] T064 [P] IPC isolation tests: pipe read/write, reader-closed → writer EPIPE, writer-closed → reader EOF, socket bind + connect + send/recv, fd-passing round-trip in `crates/kernel/tests/ipc.rs`

### Kernel capabilities

- [ ] T065 Implement cap storage per process and the `cap_check`, `cap_list`, `cap_grant` handlers in `crates/kernel/src/cap/mod.rs`
- [ ] T066 [P] Capability isolation tests: grant-subset rule enforced, widening rejected, only `CAP_GRANT` holders can call `cap_grant` in `crates/kernel/tests/cap.rs`

### Kernel device node dispatch

- [ ] T067 Device dispatch framework and built-in nodes (`/dev/null`, `/dev/zero`, `/dev/random`) in `crates/kernel/src/dev/mod.rs`
- [ ] T068 `/dev/fb0` dispatch to the framebuffer driver via the driver control channel in `crates/kernel/src/dev/fb.rs`; enforces the `DISPLAY_SERVER` capability on open
- [ ] T069 `/dev/input/kbd` and `/dev/input/mouse` dispatch to the input driver in `crates/kernel/src/dev/input.rs`; enforces `DISPLAY_SERVER` on open
- [ ] T070 `/dev/console` dispatch to the console driver (reads + writes both work) in `crates/kernel/src/dev/console.rs`

### Kernel syscall dispatch

- [ ] T071 WASI preview 1 handler in `crates/kernel/src/syscall/wasi.rs` implementing every opcode from `contracts/syscalls.md §2` (args, environ, clock, fd, path, poll, proc, random, sock, sched)
- [ ] T072 Extension syscall handler in `crates/kernel/src/syscall/ext.rs` implementing `ipc_*` (0x1000..0x1006), `proc_spawn`/`wait`/`kill`/`self`/`parent`/`caps_get` (0x1100..0x1105), `display_connect` (0x1200), `cap_*` (0x1300..0x1302), `mount`/`umount` (0x1400..0x1401), **`fs_watch` (0x1402)** per `contracts/syscalls.md §3.7` (with the VFS-side notifier hook that tmpfs and OPFS call on mutations), and **`host_file_recv` (0x1500)** per `contracts/syscalls.md §3.6` (with the bootstrap-to-kernel `host_file_dropped` notification path)
- [ ] T073 Syscall dispatcher main loop: reads requests from every process's SAB ring, services them, writes responses, manages blocked/ready transitions in `crates/kernel/src/syscall/dispatch.rs`
- [ ] T074 `proc_spawn` implementation: asks the platform to instantiate a new user Worker for the named binary, allocates a fresh SAB, wires fd table entries from the spawn manifest, applies a subset of the caller's caps, registers the new pid in the process table in `crates/kernel/src/proc/spawn.rs`
- [ ] T075 `proc_wait` / `proc_kill` / signal delivery (signal inbox IPC channel, default dispositions, `SIGKILL`/`SIGTERM` semantics) in `crates/kernel/src/proc/signal.rs`
- [ ] T076 Kernel-panic detection path: any unrecoverable kernel error caught at the top of the dispatch loop emits a `kernel_panic` postMessage to the bootstrap and halts, to satisfy FR-009a (the bootstrap-side overlay lives in T085)
- [ ] T077 [P] **Headless shell test** (Principle VIII gate): an isolation test that spawns `/bin/sh` with stdin/stdout wired to `/dev/console`, runs `echo hello; exit 0`, and asserts the correct output on the console — all without any display server in `crates/kernel/tests/headless_shell.rs`
- [ ] T078 [P] Syscall-dispatch isolation tests covering every WASI opcode and every extension opcode (happy path + one error path each) in `crates/kernel/tests/syscall.rs`

### TypeScript drivers

- [ ] T079 Driver common module (`Driver` interface, `DevId` enum, `ToDriver`/`FromDriver` message shapes, ring-buffer attachment helper) in `web/src/drivers/common.ts` matching `contracts/driver-kernel.md §2`
- [ ] T080 Framebuffer driver in `web/src/drivers/fb.ts` (owns the top-level canvas, uses `OffscreenCanvas` + `transferToImageBitmap` where available, `putImageData` fallback, emits `present_complete` events)
- [ ] T081 [P] Framebuffer driver tests with a mock kernel ring and a jsdom-stubbed canvas in `web/tests/unit/fb.test.ts`
- [ ] T082 Input driver in `web/src/drivers/input.ts` (listens to `keydown`/`keyup`/`mousemove`/`mousedown`/`mouseup`/`wheel` on the canvas, normalises to internal keycodes, pushes into two SAB rings for `/dev/input/kbd` and `/dev/input/mouse`)
- [ ] T083 [P] Input driver tests with synthetic DOM events in `web/tests/unit/input.test.ts`
- [ ] T084 Block driver in `web/src/drivers/block.ts` running in its own dedicated Worker, using `FileSystemSyncAccessHandle` against `pmos.img/{superblock,inodes.segment,data.segment.0,journal}`, handling first-boot creation and `QuotaExceededError` → `ENOSPC`
- [ ] T085 [P] Block driver tests using a stub `FileSystemSyncAccessHandle` in `web/tests/unit/block.test.ts` (read/write round-trip, first-boot superblock creation, quota-exceeded mapping)
- [ ] T086 Net driver in `web/src/drivers/net.ts` exposing `FETCH_BEGIN`/`FETCH_POLL`/`WS_OPEN`/`WS_SEND`/`WS_RECV`/`WS_CLOSE` ioctls over postMessage
- [ ] T087 [P] Net driver tests with a stub `fetch` and a stub WebSocket in `web/tests/unit/net.test.ts`
- [ ] T088 Console driver in `web/src/drivers/console.ts` (writes → `console.log`, reads ← a hidden `<textarea>` with a test-harness injection hook)
- [ ] T089 [P] Console driver tests in `web/tests/unit/console.test.ts`

### Bootstrap, kernel Worker, service worker

- [ ] T090 Complete `web/src/bootstrap.ts`: verify `crossOriginIsolated`, create canvas, instantiate each driver, create the kernel Worker, pass driver `MessagePort`s to it, hook a top-level error handler that paints the **kernel-panic overlay** and triggers an auto-reload after ~5 seconds (FR-009a)
- [ ] T091 Kernel Worker entrypoint `web/src/kernel-worker.ts`: instantiates the kernel WASM, wires the driver ports, starts the kernel's main loop
- [ ] T092 Complete `web/src/sw.ts`: versioned cache (`pmos-v<N>`), `install` precaches every asset listed by `assemble-dist`, `activate` cleans old caches, `fetch` is cache-first with network fallback
- [ ] T093 [P] Bootstrap unit tests (SW registration, kernel-Worker spawn, COOP/COEP check) in `web/tests/unit/bootstrap.test.ts`
- [ ] T094 [P] Kernel-panic overlay test: inject a simulated kernel Worker error event, assert overlay appears with the expected diagnostic, assert auto-reload timer fires in `web/tests/unit/panic-overlay.test.ts`

### Init (PID 1)

- [ ] T095 Implement `crates/init/src/main.rs`: waits for the kernel to finish mounting, reads `/etc/init.conf` (TOML), spawns the display server, waits for `/run/display` to exist, spawns the desktop shell (`/usr/bin/shell`), then enters its reap loop with the shell-respawn policy from `contracts/init-conf.md §3.2`. **At every `proc_spawn` (direct or via the reap loop's respawn path), init also reads `/etc/preferences.toml` and applies the dynamic env-var whitelist (`TZ` from `timezone.iana`, with `UTC` as fallback) to the child's `envp` per `contracts/init-conf.md §3.6`.** Init links the `preferences` crate introduced in T183.
- [ ] T096 [P] Write the default `/etc/init.conf` file into the initramfs at `crates/kernel/assets/etc/init.conf` per `contracts/init-conf.md §4`
- [ ] T097 [P] Init unit tests: parses the default conf, falls back to built-in defaults on parse error, enforces the 1/second respawn cap in `crates/init/tests/init.rs`

### Display server

- [ ] T098 Wire framing (encode/decode `MessageHeader` + payload + fd-passing count, little-endian, length-prefixed strings) in `crates/display-server/src/protocol/wire.rs` matching `contracts/display-protocol.md §1`
- [ ] T099 Object-ID allocator (odd client, even server) in `crates/display-server/src/protocol/objects.rs`
- [ ] T100 `pmd_display` (object 1) and `pmd_registry` in `crates/display-server/src/protocol/display.rs` and `.../registry.rs` (advertise `pmd_compositor`, `pmd_shm`, `pmd_xdg_wm_base`, `pmd_seat`, `pmd_output`, and `pmd_shell_manager` — the last only to clients holding `SHELL`)
- [ ] T101 `pmd_compositor` + `pmd_surface` with double-buffered `SurfaceState`, `attach`/`damage`/`frame`/`commit` in `crates/display-server/src/protocol/surface.rs`
- [ ] T102 `pmd_shm` + `pmd_shm_pool` + `pmd_buffer` with a SAB-backed shared memory pool, `release` event back to clients in `crates/display-server/src/protocol/shm.rs`
- [ ] T103 `pmd_xdg_wm_base` + `pmd_xdg_surface` + `pmd_xdg_toplevel` + `pmd_xdg_popup` in `crates/display-server/src/protocol/xdg.rs` covering title, app id, min/max size, maximize/unmaximize, minimize, move, resize, close, configure/ack_configure
- [ ] T104 `pmd_seat` + `pmd_pointer` + `pmd_keyboard` with focus-tracking, enter/leave events, modifier state in `crates/display-server/src/protocol/seat.rs`
- [ ] T105 **`pmd_shell_manager` extension** in `crates/display-server/src/protocol/shell_manager.rs`: `subscribe_windows` MUST replay `window_added` for every currently live top-level in current z-order before any future event — this is the contract (`contracts/display-protocol.md §15`) that makes the Principle II layering test pass
- [ ] T106 Keymap loader (binary xkb_v1 format) in `crates/display-server/src/protocol/keymap.rs`; loads the default US-QWERTY keymap from `crates/display-server/assets/keymaps/`
- [ ] T107 Software compositor: stacking (per-surface layer + z-order), damage clipping, ARGB8888 blit into an `ImageData` buffer, submission via the framebuffer driver in `crates/display-server/src/compositor/mod.rs`
- [ ] T108 Input routing: click-to-focus policy, pointer enter/leave, modifier tracking, focus assignment on click, in `crates/display-server/src/input/mod.rs`
- [ ] T109 Frame-callback scheduler integrated with framebuffer driver `present_complete` events in `crates/display-server/src/compositor/frame.rs`
- [ ] T110 Display server main loop: listens on `/run/display`, accepts clients, dispatches protocol messages, composites and presents at the framebuffer's refresh rate, in `crates/display-server/src/main.rs`
- [ ] T111 [P] Display server isolation tests with mock framebuffer + mock client: create_surface, attach + commit, frame callback fires, input routing delivers to focused surface, multi-client stacking, in `crates/display-server/tests/protocol.rs`
- [ ] T112 [P] Display server `window_added` replay test: connect as a shell client mid-session with three pre-existing top-levels and assert exactly three `window_added` events arrive before any other event (the Principle II contract), in `crates/display-server/tests/shell_manager_replay.rs`
- [ ] T113 [P] Toolkit-free conformance client in `crates/toolkit-free-client/src/main.rs` implementing `contracts/display-protocol.md §18` (display_connect → get_registry → bind compositor/shm/xdg_wm_base → allocate SAB pool → create surface/xdg_surface/xdg_toplevel → attach buffer → commit → handle close). The display server test suite runs this binary against itself and asserts a window is produced with no toolkit linked in.

### Toolkit

- [ ] T114 `App::connect` + event loop wrapping the display protocol in `crates/toolkit/src/app.rs`
- [ ] T115 `Window` wrapping `pmd_xdg_toplevel` with title/size/role/close callbacks in `crates/toolkit/src/window.rs`
- [ ] T116 Widget trait + primitives: `Label`, `Button`, `TextInput`, `List`, `Container`, in `crates/toolkit/src/widget/mod.rs` and one file per widget
- [ ] T117 Simple flex-style layout engine (row/column, gap, grow) in `crates/toolkit/src/layout/mod.rs`
- [ ] T118 Drawing primitives: solid rectangle, bitmap font text, image blit, in `crates/toolkit/src/draw/primitive.rs`
- [ ] T119 SAB buffer allocation + surface commit loop, integrated with `frame` callbacks, in `crates/toolkit/src/draw/buffer.rs`
- [ ] T120 [P] Toolkit isolation tests against a mock display server: window creation dispatches the right sequence of protocol messages, layout produces expected child rects, keyboard focus routes to text input, in `crates/toolkit/tests/toolkit.rs`

**Checkpoint — Phase 2 complete**: all four layer test targets (`just test-kernel`, `just test-display-server`, `just test-toolkit`, `just test-drivers`) are green. The Principle VIII headless-shell gate (T077) passes — the kernel is demonstrably runnable without any display server. The toolkit-free conformance client (T113) runs against the display server in isolation. User story work (Phase 3 onward) can now begin.

---

## Phase 3: User Story 1 — First Boot to a Usable Desktop (Priority: P1) 🎯 MVP

**Goal**: open the URL on a fresh profile → brief boot → desktop (wallpaper + taskbar + launcher) → open Terminal → run `echo hello` → see output.

**Independent Test**: in Playwright, navigate to the dev server on a fresh browser context. Assert the desktop is visible within 10 s. Click the launcher, select Terminal, type `echo hello`, assert `hello` appears in the terminal.

### Implementation for US1

- [ ] T121 [US1] Minimal desktop shell in `crates/shell/src/main.rs` that requests the `SHELL` + `DISPLAY_CLIENT` + `PROC_ENUMERATE` caps at startup, connects to the display server, draws a wallpaper top-level surface (solid colour in P1, FR-034 promotes to asset in US9), a taskbar top-level surface anchored to the bottom edge, and a launcher popup on taskbar click
- [ ] T122 [US1] Launcher content source: reads `/usr/share/applications/*.desktop` at startup and on a 5 s poll interval in `crates/shell/src/launcher.rs`
- [ ] T123 [US1] Minimal `/usr/bin/sh` implementation: no pipes/redirection yet (US4 expands it), supports `echo`, `exit`, `cd`, `pwd`, reads a line, tokenizes, executes, in `crates/sh/src/main.rs`
- [ ] T124 [US1] Minimal `/usr/bin/term` implementation: toolkit window with a scrollback area + input line, spawns `/usr/bin/sh` as a child process with stdin/stdout piped to itself, renders ANSI/VT-minimal output, in `crates/term/src/main.rs`
- [ ] T125 [US1] Ship the default initramfs `/usr/share/applications/{terminal,files,edit,settings,sysmon}.desktop` entries in `crates/kernel/assets/usr/share/applications/` (entries for apps that don't work yet still appear — launcher is tolerant)
- [ ] T126 [US1] Wire `just dev` to actually boot and land on a desktop: update `xtask assemble-dist` to include all WASM binaries needed for P1 (kernel, init, display server, shell, sh, term)
- [ ] T127 [P] [US1] Playwright integration test `boot-to-desktop.spec.ts` in `web/tests/integration/boot-to-desktop.spec.ts`: asserts SC-001 (desktop visible < 10 s), launch terminal, type `echo hello`, assert output
- [ ] T128 [P] [US1] Performance-budget check: same test records the cold-load wall-clock time and fails if it exceeds 10 s (Principle IX gate)

**Checkpoint**: US1 works end-to-end. The MVP is shippable: a user can boot to a desktop, open the terminal, and run commands.

---

## Phase 4: User Story 2 — Windowed Multitasking (Priority: P1)

**Goal**: two or more apps open concurrently in separate windows, each movable / resizable / minimizable / maximizable / closable, with click-to-focus routing keystrokes to the right window.

**Independent Test**: open Terminal and System Monitor; drag one over the other; click the underneath one and assert it raises and receives input; minimize it, restore from taskbar; maximize and restore; close.

### Implementation for US2

- [ ] T129 [US2] Client-side decorations in the toolkit: titlebar (title text, close/min/max buttons, drag-to-move), resize borders, in `crates/toolkit/src/window/decoration.rs`
- [ ] T130 [US2] Taskbar entries in the desktop shell driven by `pmd_shell_manager.window_added`/`window_changed`/`window_removed`, each entry clickable to focus the window, in `crates/shell/src/taskbar.rs`
- [ ] T131 [US2] Minimize/restore path: shell calls `pmd_shell_manager.minimize_window`, display server unmaps the surface without destroying it, restore remaps it, in `crates/display-server/src/wm/minimize.rs`
- [ ] T132 [US2] Maximize/restore path: toolkit receives `xdg_toplevel.configure` with the maximized state, re-lays-out to fill the work area, in `crates/toolkit/src/window/maximize.rs`
- [ ] T133 [US2] Move/resize interactive-drag initiation from toolkit titlebar / borders in `crates/toolkit/src/window/interact.rs`
- [ ] T134 [P] [US2] Playwright integration test `windowing.spec.ts`: opens two apps, drags, clicks, focuses, minimizes, maximizes, closes, in `web/tests/integration/windowing.spec.ts`

**Checkpoint**: US1 + US2 both work. Users can run multiple apps side by side with full window management.

---

## Phase 5: User Story 3 — Persistent Filesystem (Priority: P1)

**Goal**: files created during a session survive tab close, browser restart, and machine reboot. Each browser profile's filesystem is private. Abrupt close never corrupts.

**Independent Test**: create `/home/user/notes/hi.txt` with content "hello" from the terminal, close the tab, reopen, read `/home/user/notes/hi.txt`, assert content matches.

### Implementation for US3

- [ ] T135 [US3] Verify T060's first-boot `mkfs` produces exactly the FR-013a starter kit (end-to-end verification that the block-driver mkfs path runs when OPFS is empty)
- [ ] T136 [US3] Flush policy: VFS `write` marks dirty, kernel issues `FLUSH` ioctl on the block driver before `proc_exit` and periodically, in `crates/kernel/src/fs/opfs/flush.rs`
- [ ] T137 [US3] `pagehide`/`beforeunload` hook in `web/src/bootstrap.ts` that sends a best-effort `sync all` message to the kernel before the tab closes
- [ ] T138 [US3] Journal replay on mount verified end-to-end via a fault-injection path in `crates/kernel/src/fs/opfs/journal_test.rs`
- [ ] T139 [P] [US3] Playwright integration test `file-persistence.spec.ts`: creates a file, reloads the page, reads it back, in `web/tests/integration/file-persistence.spec.ts`
- [ ] T140 [P] [US3] Playwright integration test `profile-privacy.spec.ts`: opens two separate browser contexts, writes a file in one, asserts the other doesn't see it, in `web/tests/integration/profile-privacy.spec.ts`
- [ ] T141 [P] [US3] Playwright integration test `abrupt-close.spec.ts`: writes a file, forces a tab close (context close), reloads, asserts the file is there and the filesystem is consistent, in `web/tests/integration/abrupt-close.spec.ts`

**Checkpoint**: US1 + US2 + US3 — the P1 MVP is complete. Open URL, get a desktop, run apps, create files, come back later and everything is there.

---

## Phase 6: User Story 4 — Shell, Pipes, and Redirection (Priority: P2)

**Goal**: `ls | grep something > out.txt` runs correctly: separate processes per stage, real pipes, redirection to a file, and `out.txt` appears in the file manager.

**Independent Test**: Playwright runs the exact pipeline in a terminal, asserts `out.txt` content is exactly the filtered lines, asserts no zombies remain.

### Implementation for US4

- [ ] T142 [US4] Shell tokenizer + parser supporting pipes (`|`), redirection (`>`, `>>`, `<`), environment variables (`FOO=bar cmd`, `$VAR`), and quoting, in `crates/sh/src/parser.rs`
- [ ] T143 [US4] Pipeline runner: creates pipes with `ipc_socket`/`ipc_bind` (or a dedicated `pipe` syscall wrapper), sets up fd 0/1 per stage via the spawn manifest, calls `proc_spawn` for each stage, waits on the last stage, in `crates/sh/src/pipeline.rs`
- [ ] T144 [US4] Builtins: `cd`, `pwd`, `echo`, `exit`, `export`, `env` in `crates/sh/src/builtin.rs`
- [ ] T145 [US4] Job control: foreground spawn + `Ctrl-C` → `SIGINT`, background `&` + `jobs` builtin, in `crates/sh/src/jobs.rs`
- [ ] T146 [US4] Minimal coreutils shipped in `/bin/`: `ls`, `grep`, `cat`, `mkdir`, `rm`, `cp`, `mv` as tiny Rust binaries in `crates/coreutils/src/bin/`
- [ ] T147 [US4] Extend `xtask assemble-dist` to copy the coreutils binaries into `dist/assets/bin/`
- [ ] T148 [P] [US4] Shell parser unit tests in `crates/sh/tests/parser.rs`
- [ ] T149 [P] [US4] Pipeline isolation tests that run real pipelines against a mock VFS in `crates/sh/tests/pipeline.rs`
- [ ] T150 [P] [US4] Playwright integration test `shell-pipeline.spec.ts`: runs `ls | grep foo > out.txt`, asserts `out.txt` content and zero zombie processes via `/proc`, in `web/tests/integration/shell-pipeline.spec.ts`

**Checkpoint**: users can run real shell pipelines. The "it's a real OS" claim has its most direct demonstration.

---

## Phase 7: User Story 5 — File Manager + Text Editor (Priority: P2)

**Goal**: file manager browses directories, creates/renames/moves/deletes; text editor opens/edits/saves; double-click in file manager opens in editor; host-to-PMos import and PMos-to-host export both work.

**Independent Test**: create folder in files, create file with edit, save, double-click to reopen, assert content; import a file from the host via drag-drop AND via the Import menu; export via Download menu.

### Implementation for US5

- [ ] T151 [US5] File manager app in `crates/files/src/main.rs`: toolkit window with a directory tree / list view, address bar, browse by clicking folders, right-click context menu for rename/delete/new-folder/import/export
- [ ] T152 [US5] File manager **drag-and-drop import** (FR-032a path a): listens for `drop` events on its window and copies each dropped host file via a browser-side helper exposed through the display server's shell-capability-free client surface — wait, this needs careful thought. Drag-and-drop from the host lands in the DOM as a `DataTransfer` on the canvas, which is in the JS layer. The display server / kernel have no direct access. Approach: the file manager registers a "drop target" region with the display server; the display server forwards drop events as a special protocol message carrying an opaque file handle; the kernel exposes the file data via a syscall wrapping `File.arrayBuffer()`. This is a new syscall. Add it to the ABI extension set here.
- [ ] T153 [US5] Implement the `host_file_recv` extension syscall (opcode 0x1500) per `contracts/syscalls.md §3.6`: accepts a host-file token produced by the bootstrap's drag-drop / file-picker handler, returns a read-only fd streaming the `File` bytes, with `fd_seek` returning `ENOTSUP`. Exposes the `/run/host-files` IPC endpoint the file manager subscribes to for `host_file_dropped` notifications. Implementation lives in `crates/kernel/src/syscall/ext.rs` and the kernel-side token table lives in `crates/kernel/src/platform/host_files.rs`. Kernel-side changes are already covered by T072; this task is the US5-phase integration wiring (bootstrap token table, file-manager subscription, end-to-end path).
- [ ] T154 [US5] Bootstrap-side drag-drop handler in `web/src/bootstrap.ts` that catches `dragover`/`drop` on the canvas, stores the `File` object in a token table, and sends a `host_file_dropped(token, name, size)` event to the kernel via a driver control-channel message
- [ ] T155 [US5] File manager **Import menu** (FR-032a path b): menu item that triggers a standard `<input type="file">` click via the same bootstrap token mechanism used for drag-drop
- [ ] T156 [US5] File manager **Export / Download menu** (FR-032b): reads the selected PMos file and posts a `host_file_download(name, bytes)` driver message; bootstrap side creates a `Blob` URL and triggers `<a download>` on the host
- [ ] T157 [US5] Text editor app in `crates/edit/src/main.rs`: toolkit window with a plain-text editing area, File menu (New, Open, Save, Save As, Close), unsaved-changes prompt on close
- [ ] T158 [US5] MIME-based default-app dispatch: file manager launches `edit` for `text/*` files by spawning via `/usr/share/applications/edit.desktop`
- [ ] T159 [US5] Rename-while-open invariant: when a file is renamed in files and an open fd exists in edit, edit's fd keeps working (POSIX semantics — fd → inode, not fd → name)
- [ ] T160 [P] [US5] Files and edit isolation tests against a mock toolkit in `crates/files/tests/files.rs` and `crates/edit/tests/edit.rs`
- [ ] T161 [P] [US5] Playwright integration test `files-edit.spec.ts`: create folder, edit a file, save, reopen, verify — in `web/tests/integration/files-edit.spec.ts`
- [ ] T162 [P] [US5] Playwright integration test `host-import-export.spec.ts`: uses Playwright's `setInputFiles` + drag-drop helpers to exercise both import paths and the Download button — in `web/tests/integration/host-import-export.spec.ts`

**Checkpoint**: file management and text editing work end-to-end, including both directions of host ↔ PMos file transfer. US5 is the most "normal user" story in the release.

---

## Phase 8: User Story 6 — Offline After First Load (Priority: P2)

**Goal**: after first load, disabling the network lets the system still boot and run indefinitely; the OS itself makes no network requests; filesystem changes persist offline.

**Independent Test**: load the URL with network on, close the tab, disable the network, reload, assert the desktop boots and every bundled app launches.

### Implementation for US6

- [ ] T163 [US6] Finalise the service worker precache list: `assemble-dist` emits `dist/manifest.json` listing every asset; `sw.ts` reads it at install time and precaches all of them
- [ ] T164 [US6] Versioned cache rotation: the cache name in `sw.ts` embeds a build-time version string; `activate` deletes any cache that does not match
- [ ] T165 [US6] Audit that no asset loaded by `bootstrap.ts`, `kernel-worker.ts`, or any driver is fetched from a third-party origin at runtime
- [ ] T166 [P] [US6] Playwright integration test `offline-boot.spec.ts`: load once online, call `context.setOffline(true)`, reload, assert the desktop boots within the warm-load budget and every bundled app opens, in `web/tests/integration/offline-boot.spec.ts`
- [ ] T167 [P] [US6] Playwright integration test `zero-os-network-traffic.spec.ts`: load the page, record every network request via `page.on('request')`, navigate the OS for 30 seconds without triggering any user-program network call, assert that no request was made to any origin other than the service worker cache, in `web/tests/integration/zero-os-network-traffic.spec.ts`

**Checkpoint**: offline-first and zero-backend are empirically verified, not just claimed. Principles III and IV are both covered by Playwright gates.

---

## Phase 9: User Story 7 — System Monitor & OS Introspection (Priority: P2)

**Goal**: sysmon lists every running process with PID, name, memory use, open fds; terminate kills a process; a deliberately adversarial test program cannot read foreign process memory through any non-IPC channel.

**Independent Test**: run terminal, files, edit, sysmon; assert sysmon lists all four (plus init, display server, desktop shell) with unique PIDs and non-overlapping memory; terminate one and assert it exits within 1 s.

### Implementation for US7

- [ ] T168 [US7] Extend procfs (T054) to populate `/proc/<pid>/status` with `Name`, `State`, `Pid`, `PPid`, `VmSize`, `VmPeak`, and `/proc/<pid>/fd/` with symlinks to open file descriptors
- [ ] T169 [US7] Add `/proc/storage` reading from the block driver's quota + used counters
- [ ] T170 [US7] sysmon app in `crates/sysmon/src/main.rs`: toolkit window with a process list, refresh every 1 s, Terminate button (sends `proc_kill` with `SIGKILL`), requires `PROC_ENUMERATE` + `PROC_KILL_ANY` caps per `data-model.md §5`
- [ ] T171 [US7] `/usr/share/applications/sysmon.desktop` declaring `X-PMos-Caps=DISPLAY_CLIENT;PROC_ENUMERATE;PROC_KILL_ANY`
- [ ] T172 [US7] Adversarial test program `crates/mem-adversary/src/main.rs`: attempts to read foreign memory through every non-IPC channel (guessing Worker global state, crafting out-of-bounds pointers, calling into every capability-gated device, etc.); every attempt MUST fail
- [ ] T173 [P] [US7] sysmon isolation tests against a mock kernel exposing a synthetic /proc in `crates/sysmon/tests/sysmon.rs`
- [ ] T174 [P] [US7] Playwright integration test `system-monitor.spec.ts`: opens four apps, opens sysmon, asserts all five processes are listed with unique PIDs, terminates one, asserts it disappears and the others stay alive, in `web/tests/integration/system-monitor.spec.ts`
- [ ] T175 [P] [US7] Playwright integration test `process-isolation.spec.ts`: spawns the adversarial program, asserts every memory-reading attempt fails, in `web/tests/integration/process-isolation.spec.ts` — **this is the Principle V gate**
- [ ] T176 [P] [US7] Playwright integration test `process-crash.spec.ts`: spawns an app that panics, asserts the kernel reaps it, asserts sysmon reflects the exit, asserts other processes are unaffected, in `web/tests/integration/process-crash.spec.ts`

**Checkpoint**: OS-level introspection is a real feature, process isolation is empirically verified.

---

## Phase 10: User Story 8 — Desktop Shell Replacement (Layering Test) (Priority: P2)

**Goal**: kill the desktop shell, keep the running apps alive on screen, launch a different shell binary, new shell enumerates the running apps in its taskbar. This is the Principle II acceptance gate.

**Independent Test**: with terminal, files, and edit open, kill the shell via sysmon; assert the three windows stay visible and interactive; launch `alt-shell` (a second desktop shell binary with different chrome); assert its taskbar lists all three; click an entry and assert focus transfers.

### Implementation for US8

- [ ] T177 [US8] Verify T105's `pmd_shell_manager` replay: a new shell connection receives `window_added` for every existing top-level before any other event (already covered by T112's isolation test; this task verifies the whole integration path)
- [ ] T178 [US8] Alternative desktop shell binary `crates/alt-shell/src/main.rs`: minimal reimplementation of the shell using the toolkit, with visibly different chrome (different taskbar position, different wallpaper) so the test can distinguish them
- [ ] T179 [US8] Init's shell respawn-with-replacement path: when `/etc/init.conf`'s `boot.shell` changes between kills, init spawns the new binary — already implemented in T095, but verify this path with a dedicated test
- [ ] T180 [US8] Surface-survival guarantee: the display server MUST NOT destroy surfaces on shell exit; verify in `crates/display-server/src/protocol/shell_manager.rs`
- [ ] T181 [P] [US8] **Layering integration test `layering-test.spec.ts`** in `web/tests/integration/layering-test.spec.ts` (the Principle II acceptance gate):
  1. Open terminal, files, edit.
  2. Use sysmon to kill the desktop shell (PID looked up via `/proc`).
  3. Assert the three app windows are still visible and interactive (type into the terminal, click in the file manager).
  4. From a test-only helper, rewrite `/etc/init.conf`'s `boot.shell` to `/usr/bin/alt-shell` and kill the placeholder so init respawns the replacement.
  5. Assert the new shell starts and its taskbar lists the three running apps (verified via `pmd_shell_manager` event replay).
  6. Click each taskbar entry and assert the corresponding window raises and receives input.
  7. Confirm that the kernel, display server, toolkit, and app processes were never restarted (PIDs unchanged since step 1).
- [ ] T182 [US8] **A change that breaks T181 is build-breaking**: wire the test into `just test-integration` as a required gate and document it as the Principle II acceptance test in `CLAUDE.md` and in `quickstart.md` § troubleshooting

**Checkpoint**: the project's hardest acceptance test passes. The architecture is real.

---

## Phase 11: User Story 9 — Settings (Wallpaper / Fit / Theme / Keymap / Timezone / Font / About) (Priority: P3)

**Goal**: the v1 settings app with the full clarified scope (Q5): wallpaper + fit mode, theme (light/dark), keyboard layout, timezone, default terminal font, about-this-system. Every change persists.

**Independent Test**: change the wallpaper; the desktop updates within one frame. Change the keyboard layout; a subsequent key event routes through the new keymap. Change the timezone; `date` in the terminal reflects it. Change the terminal font; a newly-launched terminal uses it. Close the tab, reopen, all changes persisted.

### Implementation for US9

- [ ] T183 [US9] Preferences store at `/etc/preferences.toml` with a typed accessor library in `crates/preferences/src/lib.rs` (shared crate for init, settings app, desktop shell, toolkit, and terminal). Schema includes `theme.name`, `theme.fit`, `wallpaper.name`, `keyboard.layout`, `timezone.iana`, `terminal.font`. Library is `#![no_std]`-compatible (needed by the kernel for init's preference read-at-spawn path).
- [ ] T184 [US9] Settings app main UI in `crates/settings/src/main.rs`: tab-based layout with Wallpaper, Appearance (theme + fit), Keyboard, Timezone, Terminal, About panes
- [ ] T185 [US9] Wallpaper picker reading `/usr/share/wallpapers/*.png`; preview + apply writes the filename to preferences and emits a preferences-changed event that the desktop shell listens for
- [ ] T186 [US9] Wallpaper fit mode (`stretch`/`tile`/`center`/`fill`) in the desktop shell: the shell re-renders its wallpaper surface according to the preference, in `crates/shell/src/wallpaper.rs`
- [ ] T187 [US9] Theme picker (light/dark): toolkit ships a `watch_theme()` helper in `crates/toolkit/src/theme.rs` that calls `fs_watch("/etc/preferences.toml", FS_WATCH_COALESCE_MODIFY)` (opcode 0x1402 per `contracts/syscalls.md §3.7`), parses the `theme.name` field from the TOML on each change event, and fires a `theme_changed(new_theme)` callback into the app's event loop. Apps that opt in by calling `toolkit::watch_theme(|new| { … })` redraw on the callback; apps that do not opt in keep their startup theme until they exit. The settings app's Theme pane writes `theme.name` to `/etc/preferences.toml` via the preferences library (T183); **no display-protocol messages are involved** in theme delivery. Initial theme is read synchronously at `App::connect` time.
- [ ] T188 [US9] Keyboard layout picker: bundled keymaps (US QWERTY, UK QWERTY, Dvorak) shipped in `crates/display-server/assets/keymaps/`; settings writes the choice to preferences and sends a `set_keymap` request to the display server
- [ ] T189 [US9] Display server `pmd_keymap_manager` global + `list_keymaps` / `set_keymap` request handlers per `contracts/display-protocol.md §15a`: loads a new keymap file from `/usr/share/keymaps/`, broadcasts a `pmd_keyboard.keymap(fd, size)` event to every bound keyboard on every client, emits `keymap_changed(name, serial)` back to the requester. Binding `pmd_keymap_manager` requires the `KEYMAP_ADMIN` capability (see `data-model.md §5`); non-`KEYMAP_ADMIN` clients receive `pmd_display.error(PERMISSION_DENIED)`. Ship `/usr/share/applications/settings.desktop` with `X-PMos-Caps=DISPLAY_CLIENT;KEYMAP_ADMIN` so the launcher passes the cap through at spawn time. Implementation lives in a new file `crates/display-server/src/protocol/keymap_manager.rs`, separate from the keymap loader added in T106.
- [ ] T190 [US9] Timezone picker: bundled IANA subset (Americas/US, Europe, Asia, UTC) shipped as a simple TZif-compatible archive in `crates/kernel/assets/etc/zoneinfo/`. Settings writes `timezone.iana` into `/etc/preferences.toml` via the preferences library. Init reads `/etc/preferences.toml` at every `proc_spawn` and sets `TZ` in the new child's environment per `contracts/init-conf.md §3.6`. **Already running processes keep their spawn-time `TZ` until they exit** — no signal-broadcast infrastructure, no synthetic `/etc/localtime`, no fs_watch on `TZ` from userland. The corresponding Playwright test (`settings-timezone.spec.ts`, T197) validates the spawn-time-only semantics by asserting that a new terminal launched after the change sees the new timezone while an existing terminal keeps the old one.
- [ ] T191 [US9] Default terminal font: bundled bitmap fonts in `crates/term/assets/fonts/` (e.g. `unifont-mono-14.pbm`, `pc-vga-16.pbm`); terminal reads `terminal.font` preference on startup
- [ ] T192 [US9] About pane: reads `/proc/version`, `/proc/storage`, the kernel ABI version, and a bundled `LICENSE.txt` / `CREDITS.txt`
- [ ] T193 [US9] Initramfs asset bundling (FR-034a): ≥3 wallpapers, 2 themes, ≥3 keymaps, ≥2 bitmap terminal fonts, tzdata subset, LICENSE/CREDITS — wired into `xtask assemble-dist` via T026
- [ ] T194 [P] [US9] Settings isolation tests against a mock preferences store + mock display server in `crates/settings/tests/settings.rs`
- [ ] T195 [P] [US9] Playwright integration test `settings-wallpaper-theme.spec.ts`: change wallpaper, assert desktop updates; change theme, assert a theme-aware app redraws; reload tab, assert persistence
- [ ] T196 [P] [US9] Playwright integration test `settings-keymap.spec.ts`: switch to Dvorak, type a key, assert the delivered keycode uses the Dvorak mapping
- [ ] T197 [P] [US9] Playwright integration test `settings-timezone.spec.ts`: set timezone to `America/New_York`, open a new terminal, run a helper that prints the current wall-clock, assert the offset matches

**Checkpoint**: the settings app is complete. Most user-configurable behaviours that matter for v1 are reachable from the GUI.

---

## Phase 12: User Story 10 — Third-Party App Installation (Priority: P3)

**Goal**: a user installs a third-party app from a `.pmpkg.tar` they obtained from outside PMos, the launcher picks it up, and the app runs as a separate process.

**Independent Test**: import `hello-0.1.0.pmpkg.tar` via the file manager (drag-drop), extract it to `/opt/hello/` via a bundled `pkginstall` command, assert the launcher lists "Hello" within 5 s, launch it, assert it runs as a separate process in sysmon.

### Implementation for US10

- [ ] T198 [US10] `pkginstall` bundled CLI in `crates/pkginstall/src/main.rs`: validates a `.pmpkg.tar` against the schema in `contracts/package-manifest.md`, extracts to `/opt/<name>/`, writes `/usr/share/applications/<name>.desktop`
- [ ] T199 [US10] `pkginstall-desktop-entry` sub-command for the case where the bundle is already extracted
- [ ] T200 [US10] Launcher file watcher: detects new `.desktop` files under `/usr/share/applications/` within 5 s (poll-based for v1) in `crates/shell/src/launcher_watcher.rs`
- [ ] T201 [US10] Package-format validation library shared between `pkginstall` and the launcher in `crates/pkg/src/lib.rs`: parses manifest.toml, validates semver, validates WASM magic, validates declared caps against the known cap set
- [ ] T202 [US10] Implement the `crates/sample-app/` hello-world app per `quickstart.md §5`: trivial toolkit window with a label
- [ ] T203 [US10] `xtask package` subcommand that takes a crate name and produces `dist/pkgs/<name>-<ver>.pmpkg.tar`
- [ ] T204 [US10] `just push-sample` target that builds the sample app, copies the resulting bundle into the running PMos's OPFS via a test-harness syscall (for the integration test to use)
- [ ] T205 [P] [US10] `pkginstall` unit tests with fixture bundles in `crates/pkginstall/tests/`
- [ ] T206 [P] [US10] Malformed-bundle tests: absolute paths, `..` segments, bad WASM magic, duplicate name, missing required fields — each MUST be rejected with a clear error, in `crates/pkg/tests/validate.rs`
- [ ] T207 [P] [US10] Playwright integration test `third-party-install.spec.ts`: imports `hello-0.1.0.pmpkg.tar` via the file manager's drag-drop path, runs `pkginstall` in the terminal, asserts the launcher picks it up within 5 s, launches it, verifies it shows as a separate process in sysmon, then uninstalls and asserts it disappears

**Checkpoint**: end-to-end third-party app install works without a central registry. FR-036 / FR-037 / FR-038 are covered.

---

## Phase 13: Polish & Cross-Cutting Concerns

**Purpose**: close the gaps the per-story phases don't naturally cover — full-stack performance verification, developer documentation, and final release readiness.

- [ ] T208 Performance audit pass: measure cold load, warm load, and input-to-pixel latency on a representative mid-range-from-5-years-ago laptop profile; record the numbers in `specs/001-browser-os-v1/perf-results.md`; fail the release if any budget is blown (Principle IX gate)
- [ ] T209 [P] Write `docs/deploy-github-pages.md` with the Cloudflare Worker pre-pend pattern for COOP/COEP
- [ ] T210 [P] Write `docs/deploy-s3-cloudfront.md` with the response-header policy JSON
- [ ] T211 [P] Write a "Developing apps for PMos" tutorial in `docs/apps.md` cross-referencing `quickstart.md §5`/`§6` and the contracts
- [ ] T212 [P] Write `docs/debugging.md` explaining kernel panic recovery, `/proc`, `/dev/console`, and Playwright test harness use
- [ ] T213 Verify all 10 user stories' Playwright tests pass from a cold `just clean && just build && just test`
- [ ] T214 Run the spec's edge-case suite manually (the 11 entries in `spec.md` "Edge Cases"): storage denied, process crash, closed pipe, many windows, quota exhausted, killed-with-open-fds, input-before-focus, off-screen window, unsupported browser, unauthorized memory access, corrupt bundle — each either has an automated test or is verified by hand and documented
- [ ] T215 [P] Accessibility **non-goal** documentation: explicitly state in `CLAUDE.md` and `docs/apps.md` that v1 has no a11y claim and that the toolkit focus/event paths are a v2 amendment target (per FR-045)
- [ ] T216 [P] Update `CLAUDE.md`'s "Active technologies" section if anything new (e.g. the tzdata subset, the bitmap font format) has been added during implementation
- [ ] T217 Final Constitution audit: walk each of the ten principles and cite the Playwright test or isolation test that gates it; update `plan.md`'s Constitution Check section if any evidence changed
- [ ] T218 Cut `v1.0.0` git tag, publish `dist/` to at least one target host (Cloudflare Pages is the canonical), and verify the deployed site passes `boot-to-desktop.spec.ts` against the deployed URL

**Post-analyze polish tasks** (appended 2026-04-13 to close carried-over MEDIUM findings U2, U3, U4, U5 from the `/speckit.analyze` pass):

- [ ] T219 [P] [US2] Playwright integration test `web/tests/integration/window-close.spec.ts`: launch a graphical app (use the bundled text editor `/usr/bin/edit`), wait for its top-level surface to appear in the compositor, click the titlebar close button via synthetic input routed through the input driver's test-harness injection hook, then assert via a `/proc` snapshot that the owning process has exited within 1 second. Addresses FR-028 and `/speckit.analyze` finding **U2**. Budget impact: none (test-only).
- [ ] T220 [P] Define and gate the SC-003 p95 input-latency methodology. Add a perf harness at `crates/integration-tests/perf/input-latency.rs` that boots the full system, opens 6 graphical apps (terminal, files, edit, sysmon, settings, and a second terminal), injects 300 synthetic input events over 10 seconds at random focused windows (weighted 70 % keystrokes / 30 % pointer motion), and measures the time from event injection to the next framebuffer `present_complete` callback. Compute the p95 across all 300 samples and fail the test if it exceeds 100 ms. Wire it into the CI gate alongside T208 via a new `just test-perf` target. Addresses SC-003 and `/speckit.analyze` finding **U3**. Budget impact: none (test-only, but this is the test that enforces Principle IX's input-path budget for every other task in the project).
- [ ] T221 [P] External-developer quickstart validation. Spin up a fresh container (e.g. a minimal Ubuntu or Alpine image) with only the prerequisites listed in `specs/001-browser-os-v1/quickstart.md §1`, clone the repo from scratch, and walk the quickstart end-to-end as written: `just build`, `just dev`, `just test`, the worked "hello" example from §5, and the toolkit-free example from §6. Record every step where the docs are wrong, ambiguous, or missing a prerequisite. Fix `quickstart.md` and any referenced docs until a clean container run succeeds with zero deviations. Addresses SC-012 and `/speckit.analyze` finding **U4**. Budget impact: none (docs).
- [ ] T222 [P] Non-goal compliance audit. Create a re-runnable audit script at `scripts/non-goal-audit.sh` that greps the entire repo (excluding `target/`, `node_modules/`, `dist/`, `build/`, and `.git/`) for patterns indicating accidental violation of FR-040 through FR-044: cloud service URLs (`s3://`, `gs://`, `azure`, `supabase`, `firebase`); authentication keywords (`login`, `signup`, `oauth`, `jwt`, `session_token`); WebGL/WebGPU imports outside the documented compositor stub (`webgl`, `webgpu`, `GPUDevice`); raw TCP/IP imports (`net::TcpStream`, `net::TcpListener`, `net::UdpSocket`); and multi-user APIs (`uid`, `gid`, `getpwnam`, `/etc/passwd`, `/etc/shadow`). Run the script and produce `docs/non-goal-compliance.md` listing every match with a one-line justification or a "false positive — reason" note. The script MUST be idempotent, deterministic, and re-runnable in CI (wire it into `just test` as a non-blocking gate in v1). Addresses FR-040 through FR-044 and `/speckit.analyze` finding **U5**. Budget impact: none (audit-only).

**Checkpoint — Release**: every Playwright test is green, every Principle has an audit citation, and the deployed static bundle boots to a working desktop.

---

## Dependencies & Execution Order

### Phase Dependencies

- **Phase 1 (Setup)**: no dependencies; can start immediately.
- **Phase 2 (Foundational)**: requires Phase 1. **Blocks all user story phases.** This is the constitution's bottom-up gate and is non-negotiable.
- **Phase 3 (US1 — MVP)**: requires Phase 2.
- **Phases 4 (US2), 5 (US3)**: require Phase 2; independent of each other and of Phase 3 in design, but US2 and US3 add minor integration glue that is easier to validate against a running US1.
- **Phases 6–12 (US4–US10)**: require Phase 2 and benefit from Phases 3–5 being in place (they share the desktop shell, file manager, and terminal).
- **Phase 13 (Polish)**: requires all desired user-story phases to be complete.

### User Story Dependencies

- **US1**: the absolute minimum. Every later story builds on top of its shell + terminal.
- **US2**: extends US1 with window management chrome. Independent in design.
- **US3**: the persistence story; end-to-end tests depend on US1 existing so that `mkfs` can seed a usable home directory. Logically independent.
- **US4**: extends `/usr/bin/sh` from US1's minimal stub to a full shell. Independent in design.
- **US5**: introduces the files and edit apps. Independent but benefits from US4 existing for the terminal-side install workflow shown in `quickstart.md §5`.
- **US6**: offline verification; depends on US1/US2/US3/US5 for the apps it tests launching offline.
- **US7**: sysmon; can be implemented as soon as `/proc` works (US7's real dependency is T054, not on earlier user stories).
- **US8**: the layering test. Depends on US2 (window management), US7 (sysmon for the "kill shell" step), and the `pmd_shell_manager` replay from Phase 2.
- **US9**: the settings app. Independent of US4/US5/US6/US7/US8 in design; touches the toolkit's theme path and the display server's keymap path.
- **US10**: third-party install. Depends on US5 (the file manager's drag-drop import path) and the launcher watcher added in this story.

### Within Each User Story

- Test tasks marked [P] in the same story can run in parallel (they touch different files).
- Implementation tasks that touch the same file must run sequentially.
- Per Principle X, the isolation tests at the crate level must be green before the Playwright integration tests can claim the story is done.

### Parallel Opportunities

- All Phase 1 `[P]` tasks (T005–T025 excluding sequential ones) can run simultaneously; the crate skeletons are independent.
- In Phase 2, once T030–T038 (abi + ring + layout generator) are complete, the kernel, drivers, display server, and toolkit tracks can all proceed concurrently against a frozen ABI. Specifically:
  - Kernel track (T039–T078)
  - TS driver track (T079–T094)
  - Display server track (T098–T113) (once kernel IPC and caps exist)
  - Toolkit track (T114–T120) (once display protocol decoder exists)
  - Init track (T095–T097)
- All Playwright integration tests in a given story can run in parallel against separate browser contexts.

---

## Parallel Example: Phase 2 Foundational

With a frozen ABI (T030–T038 complete), four developers can work concurrently:

```text
# Developer A (kernel)
T039..T078 — Platform, proc, fd, vfs, tmpfs, devfs, procfs, opfs, ipc, cap, dev, syscall, dispatch, panic hook, headless shell test

# Developer B (drivers, TS)
T079..T094 — common, fb, input, block, net, console drivers + bootstrap + kernel Worker + service worker + panic overlay

# Developer C (display server)
T098..T113 — wire, objects, display, registry, compositor/surface, shm/buffer, xdg, seat, shell_manager replay, keymap, compositor, input routing, frame callback, main loop, tests, toolkit-free client

# Developer D (init + toolkit)
T095..T097 + T114..T120 — init + toolkit app, window, widgets, layout, draw, event loop, tests
```

At the end of Phase 2, the four tracks merge and isolation-test suites run — all four MUST be green before any Phase 3+ work begins.

---

## Implementation Strategy

### MVP First (Phases 1 + 2 + 3)

1. Complete Phase 1 (Setup).
2. Complete Phase 2 (Foundational) — this is the big one. No short-cutting.
3. Complete Phase 3 (US1 — First Boot to a Usable Desktop).
4. **STOP and VALIDATE**: run `just test`. Must pass four isolation test targets and `boot-to-desktop.spec.ts`. Open `just dev` in a fresh browser profile; confirm the desktop boots and `echo hello` in the terminal works.
5. Deploy to Cloudflare Pages and run the deployed Playwright test. The MVP is shippable.

### Incremental Delivery (Phases 4–12)

Run each user-story phase sequentially, validating and deploying after each:

- Phase 4 (US2) → full window management.
- Phase 5 (US3) → end-to-end persistence verified.
- Phase 6 (US4) → real shell pipelines.
- Phase 7 (US5) → files + edit + host ↔ PMos transfer.
- Phase 8 (US6) → offline-first empirically gated.
- Phase 9 (US7) → system monitor.
- Phase 10 (US8) → **layering test passes** (the constitution's acceptance gate).
- Phase 11 (US9) → full settings app.
- Phase 12 (US10) → third-party app install.

After US8 passes, the architectural claim of the project is verified. Everything afterwards is feature breadth.

### Parallel Team Strategy

With multiple developers:

1. Team completes Phases 1 and 2 together (with internal parallelism as above).
2. Once Phase 2 is green:
   - Developer A: Phase 3 (US1 MVP) → Phase 4 (US2 windowing)
   - Developer B: Phase 5 (US3 persistence) → Phase 8 (US6 offline)
   - Developer C: Phase 6 (US4 shell) → Phase 7 (US5 files + edit)
   - Developer D: Phase 9 (US7 sysmon) → Phase 10 (US8 layering test)
3. Re-converge for Phase 11 (US9 settings) and Phase 12 (US10 third-party install).
4. Phase 13 (Polish) together.

---

## Notes

- Principle VIII (bottom-up construction) is encoded in the phase order and specifically in the T077 headless-shell gate that ends the kernel's sub-phase of Phase 2 — the kernel is required to be demonstrably useful before the display server is started.
- Principle X (testability at every layer) is encoded in the requirement that every crate in Phase 2 lands with a matching isolation test task (T036, T038, T046, T048, T055, T061, T064, T066, T077, T078, T081, T083, T085, T087, T089, T093, T094, T097, T111, T112, T120), that no user-story phase has implementation tasks without matching tests, and that integration tests (Playwright) never substitute for isolation tests.
- Principle II (strict layering) is encoded in the `pmd_shell_manager` replay (T105) + the layering-test integration test (T181). T182 explicitly marks it as a build gate.
- Principle IX (performance budget) is encoded in T128 (US1 cold-load budget check) + T208 (final audit). Every task whose expected budget impact is uncertain should record it in a comment at implementation time.
- Every task above names exact file paths so an LLM or a developer can open the task, create the file, and work without re-reading the plan. Tasks without file paths ought to be split further.
- Test tasks are mandatory per Principle X; a PR that adds implementation without its matching test task being closed cannot be merged.
- Commit after each task or each tightly-related group. The auto-commit hooks (`after_tasks`, `after_implement`) exist for exactly this.
