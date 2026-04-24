# Implementation Plan: Browser OS v1 — Initial Release

**Branch**: `001-browser-os-v1` | **Date**: 2026-04-13 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/opt/webos/specs/001-browser-os-v1/spec.md`

## Summary

PMos v1 is a real operating system — kernel, isolated userland processes,
POSIX-style syscall interface, virtual filesystem, drivers, IPC, display
server, window toolkit, desktop shell, and a starter set of apps — that
runs entirely inside a single browser tab with no backend.

**Execution model**: the browser's WebAssembly engine is the CPU. Every
userland process is a WASM module instance running in its own dedicated
Web Worker; process isolation is enforced by WASM linear memory
separation, not by convention. The kernel is a Rust WASM module running
in its own dedicated Worker.

**Syscall transport**: each process shares a `SharedArrayBuffer`
(SAB)-backed ring buffer with the kernel Worker. The process writes a
request, calls `Atomics.wait`, the kernel services the request and
wakes the caller with `Atomics.notify`. This makes blocking syscalls
genuinely block from the program's point of view, which is a
prerequisite for porting any POSIX-ish code. Cross-origin isolation
(COOP/COEP) is required to enable SAB on the deployment host.

**Syscall surface**: WASI preview 1 as the baseline (file I/O, process
info, time, environment, random, clocks, sockets via a shim). Extension
syscalls for IPC (AF_UNIX-equivalent), process management (posix_spawn,
waitpid, signal-equivalent), display server connection, and capability
management. No `fork()` — it does not map cleanly onto the Worker model
and is explicitly out of scope.

**Persistence**: root filesystem is backed by OPFS via
`FileSystemSyncAccessHandle` (the only browser-local storage API that
gives synchronous access, which is required to implement blocking
filesystem syscalls cleanly). `/tmp` is a kernel-resident tmpfs, `/dev`
a devfs, `/proc` a procfs, `/run` a tmpfs holding runtime sockets
including `/run/display`.

**Display server**: a userland Rust WASM program that is the only
process granted the capability to open `/dev/fb0` and `/dev/input/*`.
Clients (apps, shell) connect to `/run/display` and speak a
Wayland-inspired wire protocol (surfaces, buffers, commits, frame
callbacks, input focus, xdg-shell-like window roles) over an IPC
socket. The compositor is software in v1, producing pixel output via
`OffscreenCanvas` / `ImageData` and submitting to the framebuffer
driver.

**Toolkit**: a Rust library statically linked into apps. Provides
windows, a retained-mode widget tree, layout, drawing, and an event
loop integrated with the display server's frame callbacks.

**Desktop shell**: an ordinary userland program holding the "shell"
capability. Draws wallpaper, taskbar, launcher, and client-side
decorations. Replaceable at runtime without touching any other layer
(the Principle II layering test).

**Bundled apps**: `/usr/bin/sh`, `/usr/bin/term`, `/usr/bin/files`,
`/usr/bin/edit`, `/usr/bin/settings`, `/usr/bin/sysmon`. All Rust,
built from the same Cargo workspace.

**Build**: a single Cargo workspace for Rust, a tiny TypeScript source
tree for the JS bootstrap / drivers / service worker, orchestrated by
a Justfile that produces a static deploy directory.

## Technical Context

**Language/Version**:
- Rust (latest stable, targeting wasm32-unknown-unknown for the
  kernel; wasm32-wasip1 for userland: init, display server, toolkit,
  shell, and bundled apps). Kernel is `no_std + alloc`; userland uses
  full `std` via WASI. (Upstream rustup renamed the former `wasm32-wasi`
  target to `wasm32-wasip1`; references elsewhere in this plan use the
  current name.)
- TypeScript 5.x (strict) for the JS bootstrap, drivers, and service
  worker.

**Primary Dependencies**:
- Rust side: a workspace-local `abi` crate that defines the syscall
  numbering, request/response record layouts, and extension opcodes.
  A workspace-local `ring` crate implementing the SAB ring-buffer
  transport on both sides. Minimal external crates — avoid anything
  that drags in heavy dependencies. `pico-args` or similar for small
  CLI parsing in bundled apps. No `wasm-bindgen` in the kernel (it is
  for JS interop from `wasm32-unknown-unknown`; the kernel talks to
  drivers through a narrow postMessage protocol hand-written in a
  Rust↔TS shim, not via wasm-bindgen).
- TS side: esbuild for bundling. No web framework. Vitest for unit
  tests. Playwright for integration tests.

**Storage**:
- Root filesystem: OPFS via `navigator.storage.getDirectory()` and
  `FileSystemSyncAccessHandle` — accessed only from the block-driver
  Worker, where synchronous access is permitted.
- `/tmp`: kernel-resident tmpfs (linear memory).
- `/dev`: devfs (virtual, dispatches to driver nodes).
- `/proc`: procfs (virtual, generated on read from kernel process
  table).
- `/run`: tmpfs for runtime sockets, most importantly `/run/display`.

**Testing**:
- Kernel: `cargo test` on the host target. The kernel is written
  against a thin `Platform` abstraction that has a native
  implementation (for tests) and a WASM implementation (for runtime).
  Tests exercise the VFS, process table, IPC, scheduler, and syscall
  dispatch without any browser present.
- Display server: `cargo test` on the host target, driven by a mock
  client and a mock framebuffer. Software compositor paths are
  testable as pure Rust.
- Toolkit: `cargo test` against a mock display server that records
  protocol messages and feeds synthetic events.
- Drivers (TS): Vitest with a mock kernel ring buffer, verifying the
  postMessage + shared-memory contract.
- Integration: Playwright headless-browser tests. A hidden test
  harness app gets loaded as an autostart program, receives commands
  over a test-only IPC channel, and asserts on filesystem, process
  state, and display-server state. Includes the Principle II layering
  test (kill shell → apps still alive → launch replacement shell → new
  shell sees apps) as a first-class integration test.

**Target Platform**:
- Modern evergreen browsers (Chromium, Firefox, Safari recent
  releases) with: WebAssembly 1.0, Web Workers, `SharedArrayBuffer`
  under COOP/COEP, `Atomics.wait` / `Atomics.notify`, `OffscreenCanvas`
  where available, OPFS with `FileSystemSyncAccessHandle`, service
  workers. Browsers lacking any of these show an "unsupported browser"
  message instead of booting.

**Project Type**: browser-hosted operating system. Single deployable
bundle of static files. Not a web app, not a library, not a CLI — this
is its own category and the directory layout reflects that.

**Performance Goals** (budgets are hard, per Principle IX):
- Cold load (fast connection): **< 10 s** to interactive desktop.
- Warm load (cached): **< 3 s** to interactive desktop.
- Perceived input latency (keystroke, click, window drag, app launch):
  **< 100 ms** in ≥ 95 % of interactions under typical desktop use
  (up to 6 apps open).
- Idle CPU: negligible (no busy loops, no unnecessary wakeups).

**Constraints**:
- Deployment target MUST support serving with the
  `Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: require-corp` headers so that the
  page is cross-origin-isolated and `SharedArrayBuffer` is available.
  This is the single hardest deployment constraint. Known-good hosts:
  Cloudflare Pages (via `_headers`), Netlify (via `_headers`), GitHub
  Pages (via a Cloudflare worker in front), S3+CloudFront (via
  response-header policies). Documented in the deployment section.
- No backend, no origin-side dynamic behavior, no per-user storage on
  infrastructure we operate. Static files only.
- No GPU-accelerated 3D. 2D software compositing only.
- No audio server in v1.
- No raw TCP/IP. Network syscalls wrap `fetch()` and WebSocket only.
- No multi-window-per-app. One top-level surface per app; popups are
  fine.
- English only; UTF-8 everywhere.
- No package signing or verification in v1 (documented non-goal).

**Scale/Scope**:
- Bundled v1 consists of: kernel, init, display server, toolkit
  (library), and six bundled apps (sh, term, files, edit, settings,
  sysmon). A sample third-party app bundle is built for the package
  format test.
- Single local user per browser profile. No remote, no multi-user.
- Expected code size v1: ~15–25k lines of Rust, ~1–2k lines of
  TypeScript.
- Expected running-process count at steady state: kernel + init +
  display server + desktop shell + 4–8 apps = ~7–11 processes.
- **Initramfs preference assets** (per spec FR-034, FR-034a after
  the 2026-04-13 clarification): ≥3 wallpapers, 2 themes
  (light/dark), ≥3 keyboard layouts (e.g., US QWERTY, UK QWERTY,
  Dvorak) consumed by the display server's keymap loader, an
  IANA-subset timezone database consumed by userland via the `TZ`
  environment variable (set by init from `/etc/preferences.toml`
  at process spawn time), and ≥2 bitmap terminal fonts consumed
  by the terminal emulator. All of these are baked into the
  initramfs at build time so the first-boot experience works
  fully offline.
- **Settings app scope**: wallpaper + wallpaper-fit mode, theme,
  keyboard layout, timezone, default terminal font, and an
  about-this-system pane reading `/proc/storage` and
  `/proc/version`. The settings app's preference store is a
  small TOML file under `/etc/` (exact path in data-model).

## Constitution Check

*GATE: MUST PASS before Phase 0 research. RE-EVALUATED after Phase 1
design.*

Each of Principles I–X from `.specify/memory/constitution.md` is
checked explicitly below. Every line is **PASS** or **FAIL**; a FAIL
either redesigns the plan before leaving this gate or is recorded
under **Known Deviations** with justification.

### Pre-Phase-0 Evaluation

| # | Principle | Status | Notes |
|---|-----------|--------|-------|
| I | Real OS, not a simulation | **PASS** | Kernel (Rust wasm32-unknown-unknown), process table, scheduler, POSIX-flavored syscalls (WASI + extensions), VFS with mount points, drivers, IPC, display server. Kernel contains no UI concepts. |
| II | Strict layering, no shortcuts | **PASS** | Browser substrate → drivers (TS) → kernel (WASM) → display server (userland WASM) → toolkit (lib) → desktop shell (userland) → apps (userland). Capability check makes display server the only process with access to `/dev/fb0` and `/dev/input/*`. Shell holds only a documented "shell" capability. Layering test scheduled as a first-class integration test. |
| III | Browser-only, zero backend | **PASS** | Deploy is a static file directory. No backend, no accounts, no telemetry, no origin-side dynamic behaviour. Net stack for user programs is `fetch()`+WebSocket, invoked only by user code. |
| IV | Offline-first and persistent | **PASS** | Service worker caches the entire OS bundle; OPFS holds the root filesystem. Subsequent loads require no network. |
| V | Process isolation is mandatory | **PASS** | Each userland process is a distinct WASM instance in its own Worker. WASM linear memory is physically separate — this is **stronger** than conventional OS process isolation because it is enforced at the execution-substrate level, not by MMU page tables a compromised kernel could tamper with. All IPC goes through the kernel ring-buffer transport and through kernel-owned pipe/socket buffers. |
| VI | Standard syscall surface (WASI-based) | **PASS** | WASI preview 1 is the baseline. Extension syscalls are limited to: IPC endpoint/connect/send/recv with fd passing; `spawn`/`waitpid`/signal-equivalent (no `fork`); display server connect; capability query/grant; `mount`/`umount` (new-filesystem registration); `fs_watch` (preferences-change delivery — the settings app writes `/etc/preferences.toml`, the toolkit and desktop shell watch it, per `contracts/syscalls.md §3.7`); `host_file_recv` (the bootstrap's drag-drop / file-picker handler surfaces a host `File` to the file-manager process as a read-only fd — a JS-to-kernel driver channel, opcode 0x1500, documented in `contracts/syscalls.md §3.6`). Each extension is documented in `contracts/syscalls.md` with an explicit "why WASI does not cover this" justification. |
| VII | Protocol over API for the display server | **PASS** | Clients connect to `/run/display` (a Unix-socket-equivalent IPC endpoint) and speak a Wayland-inspired wire protocol. The toolkit is a convenience library that speaks the same protocol. A hand-written toolkit-free client is explicitly scheduled as a v1 integration test to prove the protocol is the source of truth. |
| VIII | Bottom-up construction | **PASS** | Task graph (built in `/speckit.tasks`) will order: kernel (tested headless) → drivers (tested with mock kernel) → display server (tested with mock client + mock framebuffer) → toolkit (tested against mock display server) → desktop shell → bundled apps. No layer starts before the layer below it is demonstrably working and covered by isolation tests. |
| IX | Performance budget | **PASS (monitored)** | Cold < 10 s, warm < 3 s, input < 100 ms are the plan's budgets. Design choices that respect the budget: main thread reserved for driver event loop and framebuffer `putImageData` (kernel in its own Worker); SAB + Atomics.wait avoids postMessage overhead on hot syscall paths; OPFS SyncAccessHandle avoids the async overhead of IndexedDB; service worker makes warm load cache-only. Each task MUST state its expected budget impact. |
| X | Testability at every layer | **PASS** | Kernel compiles to the host target via a `Platform` abstraction for `cargo test`. Display server has mock-client + mock-framebuffer test mode. Toolkit has mock-display-server test mode. Drivers have Vitest + mock kernel. Playwright integration covers the full stack, including the layering test. A change that breaks isolation tests is rejected before integration. |

**Result**: all ten principles PASS. No Known Deviations. Proceed to
Phase 0.

## Project Structure

### Documentation (this feature)

```text
specs/001-browser-os-v1/
├── spec.md              # Feature spec (already written)
├── plan.md              # This file
├── research.md          # Phase 0 output — decisions + rationale
├── data-model.md        # Phase 1 output — entities & invariants
├── quickstart.md        # Phase 1 output — dev workflow & worked app example
├── contracts/           # Phase 1 output — ABI and protocol references
│   ├── syscalls.md           # WASI preview 1 + extension syscalls
│   ├── display-protocol.md   # wire protocol (objects, opcodes, events)
│   ├── driver-kernel.md      # driver ↔ kernel postMessage + SAB contract
│   ├── package-manifest.md   # manifest.toml schema + bundle layout
│   └── init-conf.md          # /etc/init.conf schema
├── checklists/
│   └── requirements.md  # Spec quality checklist (already written)
└── tasks.md             # Phase 2 output — NOT created by /speckit.plan
```

### Source Code (repository root)

```text
# Rust side — single Cargo workspace
Cargo.toml                       # workspace manifest
rust-toolchain.toml              # pinned stable toolchain, two targets

crates/
├── abi/                         # syscall numbering + request/response layouts
├── ring/                        # SAB ring buffer transport (kernel & userland halves)
├── display-proto/               # Phase-2 extraction: shared wire codec + ID rules (see Known Deviations #1)
├── kernel/                      # wasm32-unknown-unknown, no_std + alloc
│   ├── src/
│   │   ├── main.rs              # WASM entrypoint (init, main loop)
│   │   ├── platform/            # Platform abstraction (native + wasm impls)
│   │   ├── proc/                # process table, scheduler, spawn/wait
│   │   ├── fd/                  # per-process fd table
│   │   ├── vfs/                 # inode VFS, mount table
│   │   ├── fs/                  # filesystems: tmpfs, devfs, procfs, opfs client
│   │   ├── ipc/                 # pipes + unix-socket-equivalents
│   │   ├── syscall/             # WASI + extension dispatch
│   │   ├── cap/                 # capability table
│   │   └── dev/                 # device node dispatch (fb0, input, console, null, ...)
│   └── tests/                   # native-host integration tests (no display server)
├── init/                        # wasm32-wasip1, PID 1, reads /etc/init.conf
├── display-server/              # wasm32-wasip1, owns /dev/fb0 and /dev/input/*
│   ├── src/
│   │   ├── main.rs
│   │   ├── protocol/            # wire format decoder/encoder
│   │   ├── surface/             # surface & buffer objects
│   │   ├── compositor/          # software compositor (stack, clip, blit)
│   │   ├── input/               # keyboard/mouse event routing & focus
│   │   └── wm/                  # window roles, xdg-shell-like extension
│   └── tests/                   # mock client + mock framebuffer
├── toolkit/                     # library (no_main) linked into apps
│   ├── src/
│   │   ├── lib.rs
│   │   ├── window.rs
│   │   ├── widget/              # button, label, text input, list, container
│   │   ├── layout/              # simple flex layout
│   │   ├── draw/                # buffer allocation + primitive drawing
│   │   └── eventloop.rs
│   └── tests/                   # mock display-server server
├── shell/                       # /usr/bin/shell — desktop shell (wallpaper, taskbar, launcher, client-side decorations); distinct from /usr/bin/sh
├── sh/                          # /usr/bin/sh — pipes, redir, env, builtins, jobs
├── term/                        # /usr/bin/term — terminal emulator
├── files/                       # /usr/bin/files — file manager
├── edit/                        # /usr/bin/edit — text editor
├── settings/                    # /usr/bin/settings — wallpaper / theme
├── sysmon/                      # /usr/bin/sysmon — reads /proc
├── sample-app/                  # third-party package test fixture
├── toolkit-free-client/         # integration-test fixture: a client speaking the wire protocol directly
└── integration-tests/           # native-Rust integration-test harness crate (host-target only, not wasm); houses perf/input-latency.rs and future Rust-native full-stack tests that need precise timing the Playwright JS layer cannot provide

# TypeScript side — small, hand-written, no framework
web/
├── index.html                   # single document, boot canvas, service worker registration
├── src/
│   ├── bootstrap.ts             # installs SW, sets up canvas, spawns kernel Worker
│   ├── kernel-worker.ts         # kernel Worker entry: instantiates kernel WASM, wires ring buffer
│   ├── drivers/
│   │   ├── fb.ts                # framebuffer driver (owns canvas + OffscreenCanvas)
│   │   ├── input.ts             # keyboard/mouse/wheel event capture, normalized stream
│   │   ├── block.ts             # OPFS block driver (SyncAccessHandle, in its own Worker)
│   │   ├── net.ts               # minimal fetch()/WebSocket-backed net driver
│   │   └── console.ts           # devtools-console serial-style device
│   ├── shared/
│   │   ├── sab-layout.ts        # SAB ring-buffer offsets — generated from abi crate, checked in
│   │   └── proto.ts             # postMessage message types
│   └── sw.ts                    # service worker: precache OS assets, offline-first fetch
├── tests/
│   ├── unit/                    # Vitest unit tests with mock kernel
│   └── integration/             # Playwright headless-browser tests
│       └── layering-test.spec.ts  # Principle II acceptance test
├── tsconfig.json
└── package.json                 # minimal: esbuild, vitest, playwright

# Build orchestration
Justfile                         # build, test, package, serve, clean
build/                           # (gitignored) intermediate artifacts
dist/                            # (gitignored) final static deploy directory
│
└── dist/ contents at release time:
    ├── index.html
    ├── assets/bootstrap.js      # bundled bootstrap + drivers
    ├── assets/kernel.wasm
    ├── assets/init.wasm
    ├── assets/display-server.wasm
    ├── assets/bin/{sh,term,files,edit,settings,sysmon}.wasm
    ├── assets/toolkit.wasm      # (reserved — toolkit is statically linked today, but cache-busted fingerprint if the pattern changes)
    ├── sw.js
    └── _headers                 # COOP/COEP for Cloudflare Pages / Netlify
```

**Structure Decision**: the project is split into exactly two source
trees because the two languages serve two distinct layers that the
constitution requires to be separated: Rust covers kernel, display
server, toolkit, and every userland process; TypeScript covers the
browser substrate and the driver layer that touches the DOM. There is
no third "shared app framework" layer — this is a deliberate choice to
enforce that JS is the firmware, not the product. The `abi/` and
`ring/` crates plus the `sab-layout.ts` file are the two ends of a
single binary contract, and the task graph will include a consistency
check that `sab-layout.ts` matches what `abi` emits.

## Complexity Tracking

> No Constitution Check items failed. This table is intentionally
> empty.

| Violation | Why Needed | Simpler Alternative Rejected Because |
|-----------|------------|--------------------------------------|
| _(none)_  | _(none)_   | _(none)_                             |

## Phase 0 Status

**Complete.** See [`research.md`](./research.md).

All `NEEDS CLARIFICATION` entries raised during Technical Context
drafting are resolved with a Decision + Rationale + Alternatives
entry. The resolved items are: execution model (WASM-in-Workers),
syscall transport (SAB + `Atomics.wait`), storage backend (OPFS via
`FileSystemSyncAccessHandle`), syscall surface (WASI preview 1 +
documented extensions), extension syscall justifications (IPC,
spawn/wait, display connect, capability management), display
protocol (Wayland-inspired, not wire-compatible), compositor
strategy (software via `OffscreenCanvas`/`ImageData`), driver
location (main-thread TS + dedicated Worker for block), kernel
language/target (Rust `wasm32-unknown-unknown`, `no_std + alloc`),
offline strategy (service worker precache with versioned cache),
deployment constraints (COOP/COEP; Cloudflare Pages / Netlify /
GitHub Pages via CF Worker / S3+CloudFront), integration tooling
(Playwright + in-WASM test harness), build orchestration (Justfile
+ esbuild + `xtask`), and the package format (tar +
`manifest.toml`). No open clarifications remain.

## Phase 1 Status

**Complete.** Artefacts produced:

- [`data-model.md`](./data-model.md) — kernel data structures
  (process, fd table, vnode/mount, pipes/sockets, capabilities,
  display server object model, compositor state), on-disk layout
  (OPFS superblock + inode + journal), `/proc` schema, `init.conf`
  summary, and the state-transition catalogue for boot, spawn,
  exit, and the Principle II shell-replace acceptance flow.
- [`contracts/syscalls.md`](./contracts/syscalls.md) — canonical
  OS ABI. WASI preview 1 baseline table + extension syscalls
  (`ipc_*`, `proc_spawn`/`proc_wait`/`proc_kill`/`proc_self`/
  `proc_parent`/`proc_caps_get`, `display_connect`, `cap_*`,
  `mount`/`umount`), each with a request/response signature and a
  "why WASI doesn't cover this" paragraph.
- [`contracts/display-protocol.md`](./contracts/display-protocol.md)
  — wire framing, object-ID allocation, the thirteen core object
  types (display, registry, compositor, shm, shm_pool, buffer,
  surface, xdg_wm_base, xdg_surface, xdg_toplevel, xdg_popup,
  seat, pointer, keyboard, callback), the `pmd_shell_manager`
  extension for the desktop shell (with the `window_added` replay
  semantics that make the layering test pass), the error-code
  table, and the toolkit-free conformance client.
- [`contracts/driver-kernel.md`](./contracts/driver-kernel.md) —
  the syscall ring SAB layout, the kernel↔driver control
  channel, and per-device ring and ioctl formats for framebuffer,
  input, block (OPFS), net, and console.
- [`contracts/package-manifest.md`](./contracts/package-manifest.md)
  — `manifest.toml` schema with field constraints, the `tar`
  bundle layout, install/uninstall semantics, launcher spawn
  contract, and the explicit v1 non-goals (no signing, no
  dependencies, no registry).
- [`contracts/init-conf.md`](./contracts/init-conf.md) —
  `/etc/init.conf` schema, boot sequence, shell respawn policy,
  and the documented path by which a replacement shell binary
  reattaches to the running system.
- [`quickstart.md`](./quickstart.md) — developer onboarding:
  prerequisites, `just build` / `just dev` / `just test`,
  worked example of a "hello" toolkit app, the same app written
  **without** the toolkit against the raw wire protocol (the
  Principle VII conformance path), packaging and installing a
  third-party app, deployment recipes for each supported host,
  and a troubleshooting table.
- [`../../CLAUDE.md`](../../CLAUDE.md) — agent context updated
  with the active technologies, commands, and conventions for
  this feature.

### Post-design Constitution Re-Check

Each principle is re-evaluated against the Phase 1 artefacts. The
check is stricter than the pre-Phase-0 pass because the artefacts
now pin down concrete shapes that could have drifted from the
principles during research.

| # | Principle | Status | Re-check notes |
|---|-----------|--------|----------------|
| I | Real OS, not a simulation | **PASS** | `data-model.md` defines `Process`, `FdTable`, `Vnode`, `Mount`, `Pipe`, `Socket`, `CapSet`, and `/proc` — the textbook inventory. `syscalls.md` exposes them through a POSIX-style surface. The kernel module tree in plan.md has no UI directories: `proc/`, `fd/`, `vfs/`, `fs/`, `ipc/`, `syscall/`, `cap/`, `dev/`. No window, no button, no DOM. |
| II | Strict layering, no shortcuts | **PASS** | Enforced at three distinct levels: (a) **module tree** — kernel has no display concerns, display server has no toolkit import, toolkit is a library with no privileged access; (b) **capability check** — only `DISPLAY_SERVER` may open `/dev/fb0` and `/dev/input/*`; only `SHELL` may bind `pmd_shell_manager`; (c) **protocol replay** — `display-protocol.md §15` specifies that a subscriber replays `window_added` for every live top-level on subscribe, which is precisely what makes the shell-replacement layering test pass. The test is cited as a first-class Playwright integration test in `quickstart.md §4.5` and is wired into the `just test-integration` target. |
| III | Browser-only, zero backend | **PASS** | `quickstart.md §8` documents static-only deployment on four hosts. No contract, no data model, and no driver references a backend or an account system. The net driver uses `fetch()` and WebSocket, both initiated only by user programs that hold `NET` capability. |
| IV | Offline-first and persistent | **PASS** | `research.md` decision on the service worker (versioned precache, cache-first) + OPFS root filesystem with journaling for abrupt-close consistency. `data-model.md §3.3` specifies the OPFS layout with a journal for crash consistency (FR-014). Integration test `offline-boot.spec.ts` is on the scheduled test list. |
| V | Process isolation is mandatory | **PASS** | `research.md` decision on WASM-in-Workers: each process is a distinct WASM Instance in a distinct Worker with a distinct linear memory. Isolation is physical and enforced by the execution substrate. The `process-isolation.spec.ts` integration test (in `quickstart.md §4.5`) runs an adversarial test program that attempts to read foreign memory through every non-IPC channel; every attempt must fail. All IPC is kernel-mediated through kernel-owned `Pipe` and `Socket` structures. |
| VI | Standard syscall surface | **PASS** | `syscalls.md` pins WASI preview 1 as the baseline and enumerates every extension syscall with an explicit "why WASI doesn't cover this" paragraph. A single `ABI_VERSION` constant in the `abi` crate gates process spawn; mismatched versions fail with `ENOABIVER`. The extension set is deliberately minimal: IPC (AF_UNIX + fd passing), `proc_spawn`/`proc_wait`/`proc_kill`, `display_connect` (for the capability short-circuit), and `cap_*`. |
| VII | Protocol over API for the display server | **PASS** | `display-protocol.md` is the source of truth. The toolkit is a library (see project structure), not a process, and statically linked. §18 specifies the toolkit-free conformance client and requires it to be an integration-test fixture that runs in every build. `quickstart.md §6` shows the worked example of writing an app with no toolkit at all. If the toolkit-free client ever stops working, the integration suite fails before any tagged release. |
| VIII | Bottom-up construction | **PASS** | Confirmed in `plan.md` "Project Structure" (crates listed from infrastructure up), in `research.md` (kernel `Platform` trait lets `cargo test` exercise the kernel before any display code exists), in `quickstart.md §4` (four distinct test layers, run in order), and in the forthcoming task-graph ordering. The "kernel testable with no graphics" gate is explicitly `just test-kernel`. |
| IX | Performance budget | **PASS (monitored)** | Budgets remain cold < 10 s, warm < 3 s, input < 100 ms. Design decisions that support them: (a) kernel in its own Worker so the main thread is free for compositor presentation; (b) SAB + `Atomics.wait` avoids `postMessage` overhead on hot syscall paths; (c) OPFS `SyncAccessHandle` avoids IndexedDB async overhead for file I/O; (d) software compositor's worst case (1920×1080 ARGB blit) stays inside one frame on the target machine class; (e) service worker precache makes warm load local-only. **Task-graph obligation**: every task MUST state its expected performance impact. Any task whose estimate blows a budget is rejected or must be accompanied by an approved deviation. |
| X | Testability at every layer | **PASS** | Four isolation test layers are named in `quickstart.md §4` and each has a `just` target. The kernel builds for the host target via its `Platform` trait. The display server runs its tests against a mock client and mock framebuffer. The toolkit runs its tests against a mock display server. Drivers run with a mock kernel ring. Integration (Playwright) is additional, never a substitute. The `layering-test.spec.ts` is called out as the Principle II acceptance gate and is part of `just test-integration`. |

**Result**: all ten principles PASS on the post-design check. The
plan is internally consistent, and each Phase 1 artefact reinforces
(rather than relaxes) the gates from the pre-Phase-0 check.

### Known Deviations (Phase 2 implementation drift)

All ten principles still PASS. The deviations below are not
principle violations — they are places where the Phase 2
implementation's module layout drifted from the `plan.md`
"Project Structure" section during bottom-up construction. They
are recorded here (per the constitution's "Deviation log"
requirement: deviations MUST live in the plan that introduced
them) so that a future reader looking for the originally-planned
structure understands where the code actually lives and why.
Tasks.md Phase 2 carries per-T-ID deviation notes; this block
is the deviation **register** the constitution refers to.

1. **`crates/display-proto/` crate extraction** — a shared
   protocol codec crate was extracted out of `display-server/`
   so that `display-server/`, `toolkit/`, and
   `toolkit-free-client/` can all depend on the same wire
   encoding, object-ID rules, request/event enums, and
   interface definitions. Absorbs T098 (wire framing) + T099
   (object-ID allocator) + the message-layout work implicit in
   T100–T105. Tests at `crates/display-proto/tests/`. **Future
   protocol work belongs in `display-proto`.** `display-proto/`
   is implicitly in the layer catalogue at "display server"
   since it is just an internal split of that layer — nothing
   above the display server may depend on it except via the
   toolkit (which already may, per the layer catalogue).

2. **`crates/kernel/src/sys.rs` + the `Kernel` struct** —
   rather than building the syscall dispatcher first (T071 /
   T073) and hanging semantic logic off it, the kernel exposes
   a semantic Rust method surface on `Kernel` that native
   tests drive directly. `Kernel::proc_spawn`, `proc_wait`,
   `proc_kill`, `path_open`, `fd_read`, `fd_write`, `fd_close`,
   `display_bind`, `display_connect`, `drain_signals`, etc.
   all live there. Deliberate testability choice (Principle X):
   the T077 headless-shell gate runs without any SAB transport
   because `sys.rs` can be constructed and exercised in-process.
   The opcode dispatcher (T071/T073) sits on top of this
   surface and translates SAB ring requests into method calls;
   it does not replace `sys.rs`.

3. **`crates/toolkit/src/protocol.rs` + `theme.rs`** — the
   toolkit has its own wire-protocol client (`protocol.rs`,
   ~620 lines) that speaks directly to `display-proto`,
   independent of T114's `App::connect`. T114 is now framed as
   a thin facade extraction over `protocol.rs`, not a
   reimplementation. `theme.rs` ships ahead of its nominal home
   (T184, the settings app) because `WindowFrame` needs theme
   colours to paint chrome — the theme struct + default
   light/dark palettes live there now. Settings-app integration
   remains T184; `theme.rs` today is just the data model +
   defaults.

4. **`crates/term/` + the TypeScript preview slice** —
   `crates/term/` and the TS files
   `web/src/{terminal,mock-kernel,console-host,console-check,fb-host}.ts`
   + `web/src/shared/{rasterizer,font,worker-proto,input-proto}.ts`
   are a visible-progress demo slice built before real kernel
   IPC (T072) and the real `/run/display` accept loop (T110)
   exist. The code is real and tested but the wiring is
   **preview-only**: the TS rasterizer paints directly into
   `fb.ts` instead of going through a
   `display-server`→`fb` path, `mock-kernel.ts` stands in for a
   real kernel Worker, and `term::Session` paints through a
   bespoke `rasterize_snapshot` rather than through
   `toolkit::draw::Canvas`. **Reconciliation required** when
   T091 (real kernel Worker) and T110 (`/run/display` accept
   loop) land. The T091 arm has landed via T230–T235 (see
   deviation #5 below); the T110 arm + the `term` → `Canvas`
   migration remain.

5. **Multi-process substrate (M1) sub-slices T230–T235** —
   an earlier Phase 2 implementation ran every user wasm
   instance inside the kernel Worker's global scope, with a
   `KernelWasmHost.drainPendingSpawns()` queue-and-drain loop
   executing children sequentially against the kernel's own
   linear memory. That was a preview stand-in: the plan's
   intended substrate was Worker-per-pid + per-pid SAB ring
   bridge. Sub-slices T230–T235 migrated the code onto the
   plan's intended model: `KernelWasmHost.startDispatchLoop`
   round-robins over a `Map<pid, SAB>` populated by main-thread
   `proc:sab` messages (T233); `SabBackend` implements
   `KernelBackend` over the per-pid SAB ring + wake-slot
   Atomics protocol (T231 + T234); a new
   [`web/src/user-worker-entry.ts`](../../web/src/user-worker-entry.ts)
   bundle is the dedicated user-Worker entry, shipped as
   `dist/assets/user-worker.js` alongside
   `dist/assets/kernel-worker.js` (T232). T235 then deleted
   `drainPendingSpawns` + the `pendingSpawns` queue + the
   legacy conditional boot paths, making the dispatch-loop path
   the only path. **Principle V's isolation is now physical**
   (each pid is a dedicated Web Worker with a distinct WASM
   linear memory; user wasm in pid A cannot access pid B's
   bytes through any non-IPC channel), not conventional (where
   all user wasm shared one linear memory inside the kernel
   Worker and isolation was a property honoured by callers).
   This is the single biggest Principle V delta in the
   project's history.

6. **`web/src/drivers/common.ts` naming bifurcation** —
   the plan (this file, §"Source Code") calls for a single
   `web/src/drivers/common.ts` carrying the `Driver` interface,
   `DevId` enum, message envelopes, and ring-buffer helpers.
   The Phase 2 implementation landed them across
   `web/src/drivers/types.ts` (`Driver`, `DriverHost`,
   `DriverResult`, `DriverErrorCode`) and
   `web/src/shared/platform-constants.ts` (`DriverId`, the
   byte-for-byte mirror of `kernel::platform::DevId`). T079
   (commit `b0baf01`, 2026-04-24) closes the naming gap by
   shipping `web/src/drivers/common.ts` as a re-export facade
   that publishes the existing types under the spec-mandated
   identifiers (including a `DevId` alias of `DriverId`) and
   adds two genuinely new additions: generic message envelopes
   `ToDriver<Op, Payload>` / `FromDriver<Op, Payload>` and the
   test-harness factories `createMockRing()` /
   `createMockRingPair()`. 12 isolation tests at
   `web/tests/unit/drivers-common.test.ts` include a
   byte-for-byte `DevId` kernel-parity assertion so future
   TS↔Rust drift fails at the driver-common boundary rather
   than at a downstream ioctl call site. Migrating the landed
   drivers (`console.ts`, `fb.ts`, `input.ts`) to import from
   `common.ts` is a separate follow-up slice.

### Planning Complete

`/speckit.plan` is finished. The feature is ready for
`/speckit.tasks` to generate the bottom-up task graph.
