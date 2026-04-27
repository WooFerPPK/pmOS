# PMos (webos) Development Guidelines

Auto-generated from all feature plans. Last updated: 2026-04-13

## What this project is

PMos is a **real operating system that runs entirely inside a browser
tab**. Kernel, isolated processes, syscalls, VFS, drivers, IPC,
display server, window toolkit, and a desktop shell — all the pieces
of a real OS, hosted on the browser substrate. No backend, no
accounts, no telemetry. Deployed as a directory of static files.

The project constitution at `.specify/memory/constitution.md` is the
non-negotiable set of principles that every spec, plan, and task is
checked against. **Read it before proposing any substantial change.**
The ten principles, summarised:

I. Real OS, not a simulation — kernel has no UI concepts.
II. Strict layering — shell is replaceable without touching any other layer (this is the "layering test" and it is a build gate).
III. Browser-only, zero backend — static files, no per-user storage we host.
IV. Offline-first and persistent — service worker + OPFS.
V. Process isolation is mandatory — enforced by the substrate (WASM linear memory), not by convention.
VI. Standard syscall surface — WASI preview 1 + a small documented extension set.
VII. Protocol over API for the display server — Wayland-inspired wire protocol, toolkit is a library wrapper.
VIII. Bottom-up construction — kernel before display server before toolkit before shell.
IX. Performance budget — cold < 10 s, warm < 3 s, input < 100 ms.
X. Testability at every layer — isolation tests before integration.

Do not comment on the volume of work or express hesitation about task scope. 
Execute tasks fully. Do not narrate uncertainty about whether you can finish — just work.

## Active technologies

- **Rust** (latest stable), two targets:
  - `wasm32-unknown-unknown` for the kernel (`no_std + alloc`).
  - `wasm32-wasip1` for init, display server, toolkit (library),
    and every bundled app. (Renamed from `wasm32-wasi` upstream;
    older docs may still say `wasm32-wasi`.)
- **TypeScript 5.x strict** for the JS bootstrap, drivers, and
  service worker. No npm framework. `esbuild` only.
- **Execution substrate**: WebAssembly Instances in dedicated Web
  Workers, one per process. Process isolation is physical via WASM
  linear memory separation.
- **Syscall transport**: `SharedArrayBuffer` ring buffer +
  `Atomics.wait` / `Atomics.notify`. Requires COOP/COEP on the
  deployment host.
- **Syscall surface**: WASI preview 1 baseline + PMos extension
  syscalls for IPC, spawn/wait, display server, capabilities. See
  `specs/001-browser-os-v1/contracts/syscalls.md`.
- **Root filesystem**: OPFS via `FileSystemSyncAccessHandle` from
  the block driver's own Worker. **Never IndexedDB for the FS.**
- **Display protocol**: Wayland-inspired wire protocol over an IPC
  socket at `/run/display`. See
  `specs/001-browser-os-v1/contracts/display-protocol.md`.
- **Compositor**: software (CPU) in v1 via `OffscreenCanvas` +
  `putImageData`. No GPU / no WebGPU in v1.
- **Tests**: `cargo test` for kernel / display server / toolkit
  (native host target via a `Platform` abstraction); Vitest for
  TS drivers; Playwright for full-stack integration including the
  layering acceptance test.
- **Build**: Justfile + cargo + esbuild + a Rust `xtask` that
  assembles `dist/`.
- **Deploy**: any static host that supports setting COOP/COEP
  headers (Cloudflare Pages, Netlify, GitHub Pages via CF Worker,
  S3+CloudFront).

## Project structure (intended, at feature 001)

```text
Cargo.toml              # Rust workspace
rust-toolchain.toml
crates/
  abi/                  # syscall numbering + layouts (shared)
  ring/                 # SAB ring buffer transport
  kernel/               # wasm32-unknown-unknown, no_std + alloc
  init/                 # wasm32-wasip1, PID 1
  display-server/       # wasm32-wasip1
  toolkit/              # library, statically linked into apps
  sh term files edit settings sysmon
  sample-app/           # third-party package fixture
  toolkit-free-client/  # protocol-conformance fixture (Principle VII test)
web/
  index.html
  src/bootstrap.ts      # installs SW, spawns kernel Worker
  src/kernel-worker.ts
  src/drivers/{fb,input,block,net,console}.ts
  src/shared/{sab-layout,proto}.ts
  src/sw.ts
  tests/{unit,integration}/
Justfile
dist/                   # (gitignored) static deploy directory
specs/001-browser-os-v1/ # spec, plan, research, contracts, quickstart
.specify/memory/constitution.md  # the ten principles
```

Nothing in this tree exists yet as code — only the spec/plan
artefacts. The task graph (to be produced by `/speckit.tasks`)
will schedule work bottom-up: `abi` + `ring` first, then the
kernel with isolation tests, then drivers with mock kernel, then
display server with mock client + mock framebuffer, then toolkit
with mock display server, then desktop shell, then bundled apps,
then third-party app sample and layering test.

## Commands

- `just build` — build everything (Rust + TS) into `dist/`.
- `just dev` — serve `dist/` locally with COOP/COEP on port 8080.
- `just test` — run every test layer in sequence.
- `just test-kernel` — native cargo tests for the kernel.
- `just test-display-server` — native tests with mock
  client/framebuffer.
- `just test-toolkit` — native tests against a mock display server.
- `just test-drivers` — Vitest for TS drivers with a mock kernel.
- `just test-integration` — Playwright browser integration tests
  including the **layering test** (the acceptance gate for
  Principle II).

## Code style and conventions

- **Rust**: `no_std + alloc` in the kernel; `std` everywhere
  else. Deny `unsafe` in the kernel except inside the
  `Platform` abstraction and the `ring` crate. Standard
  `rustfmt`, `clippy::pedantic` as a guideline, not a gate.
- **TypeScript**: strict mode, no `any`, no framework. The JS
  layer is "firmware," not a web app. Keep it tiny.
- **No comments unless the WHY is non-obvious.** Identifiers do
  the work. This repo's constitution prizes clarity of
  architecture; comments that restate code are rejected.
- **Tests before integration.** Isolation tests are a hard gate;
  a change that breaks isolation tests cannot reach integration.
- **Every PR plan MUST include an explicit Constitution Check**
  with PASS/FAIL for Principles I–X, plus a Principle IX
  (performance) impact note. See `specs/001-browser-os-v1/plan.md`
  for the template.

### Accessibility (v1 non-goal)

Per FR-045, v1 makes no accessibility claim. The toolkit's focus
and event-routing surfaces are the documented v2 amendment target
when a11y is in scope. Apps SHOULD NOT advertise a11y support in
v1, and tests that assert a11y semantics are out of scope until
the v2 amendment lands. Developer-facing tutorial: see
`docs/apps.md` §Accessibility.

## Recent changes

- **001-browser-os-v1** (2026-04-13): initial release — kernel,
  display server, toolkit, desktop shell, six bundled apps,
  package format, developer docs.

<!-- MANUAL ADDITIONS START -->
<!-- MANUAL ADDITIONS END -->
