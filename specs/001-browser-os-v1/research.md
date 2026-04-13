# Phase 0 Research: Browser OS v1

**Branch**: `001-browser-os-v1` | **Date**: 2026-04-13
**Feature**: [spec.md](./spec.md) | **Plan**: [plan.md](./plan.md)

This document consolidates the technical decisions behind the plan and
the rationale for each. Every item in the Plan's Technical Context
section has a matching entry here. No `NEEDS CLARIFICATION` markers
remain.

---

## Decision: WASM instances in Web Workers as "processes"

**Decision**: every userland process is one Rust program compiled to
`wasm32-wasi`, instantiated as a fresh WebAssembly `Instance` inside a
dedicated Web Worker. One process = one Worker = one WASM Instance =
one linear memory.

**Rationale**:

- **Isolation is physical, not conventional.** WASM linear memory is
  cryptographically walled off between Instances. A compromised
  process cannot craft a pointer that reaches another process's
  memory, because there is no shared address space and the Worker
  boundary prevents one Instance from observing another's bytes at
  all. This directly satisfies Principle V ("enforced by the
  execution substrate, not by convention") and is strictly stronger
  than a conventional OS where a kernel bug can expose cross-process
  memory.
- **Workers are the browser's only real preemption unit.** Each
  Worker gets its own event loop; blocking one does not block
  another. This lets the kernel park a process on a blocking syscall
  with `Atomics.wait` without freezing the rest of the system.
- **Main thread stays free.** Because drivers live on the main thread
  (so they can touch the DOM) and the kernel runs in a dedicated
  Worker, the main thread is available for the framebuffer driver's
  `OffscreenCanvas` transfer and the compositor's `putImageData`
  without contention.

**Alternatives considered**:

- **All userland in one Worker with asyncify**: smaller memory
  footprint but loses isolation entirely — one process could trample
  another's memory through compiler bugs or unsafe blocks. Rejected:
  Principle V explicitly forbids conventional-only isolation.
- **iframes as processes**: gives cross-origin isolation but is
  heavy-weight, hard to script from the kernel, and cannot share
  memory for the syscall transport. Rejected.
- **SharedWorker for the kernel**: would let multiple browser tabs
  share one OS instance, which is a different product (persistent
  user session across tabs). Out of scope for v1; explicitly not
  built.

**References**: WebAssembly Core 1.0 (memory isolation), HTML Living
Standard (Web Workers), `Atomics.wait` semantics (MDN,
`Atomics.wait()` returns `"ok"` / `"not-equal"` / `"timed-out"`).

---

## Decision: SharedArrayBuffer ring buffer + Atomics.wait for syscalls

**Decision**: syscalls are not `postMessage` calls. Each process
Worker shares a fixed-size `SharedArrayBuffer` (SAB) with the kernel
Worker. The SAB holds a single-producer / single-consumer ring buffer
and a small header of atomic slots used for `Atomics.wait`.

Flow for a blocking syscall:

1. The user process writes a request record (opcode, argument bytes)
   into the ring, flips a "request pending" slot, and calls
   `Atomics.wait(slot, REQUESTED)`.
2. The kernel Worker is already waiting on a kernel-side slot shared
   with every process's SAB; the user process's write of the "request
   pending" slot is followed by an `Atomics.notify` on the kernel
   slot.
3. The kernel reads the request, services it (possibly calling out to
   a driver on the main thread, possibly blocking on another Worker),
   writes the response, flips the status slot to `READY`, and calls
   `Atomics.notify` on the caller's slot.
4. The caller wakes up from `Atomics.wait`, reads the response, and
   returns to user code.

Asynchronous syscalls (e.g., WASI `poll_oneoff`, display-server
events) use the same ring buffer in a different mode: responses
accumulate until the caller asks for them, and `Atomics.wait` is used
only when the user explicitly blocks for an event.

**Rationale**:

- **Blocking semantics are mandatory.** POSIX code expects `read()`
  to block until bytes arrive. WASI preview 1 calls expect the same.
  `postMessage` cannot give blocking semantics without asyncify-ing
  the entire guest program, which is slow, fragile, and doesn't work
  for existing Rust `std` code.
- **Hot-path overhead must be tiny.** A syscall that hits in-kernel
  state (e.g., `clock_time_get`, `fd_tell`) should complete in
  microseconds. SAB + atomic reads avoids the full `postMessage`
  serialization path (structured clone, event queue, message-loop
  tick). Perceived input latency has a 100 ms budget (Principle IX);
  we need to spend none of it on syscall overhead.
- **No alternative exists.** `SharedArrayBuffer` is the only way to
  share mutable memory between Workers, and `Atomics.wait` /
  `Atomics.notify` is the only way for a Worker to block on a memory
  condition.

**Cost**: cross-origin isolation is required. The deployment host
MUST set `Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp`. This is documented in
the Plan's Constraints section and in the deployment section below.

**Alternatives considered**:

- **postMessage-based syscalls with asyncify**: every guest function
  that might ever block gets rewritten by Binaryen's asyncify pass to
  yield control back to a scheduler loop. Rejected: 2-5x binary size
  increase, significant runtime overhead on hot paths, blocks a
  naïve port of Rust `std` because asyncify is not transparent across
  `extern "C"` boundaries.
- **Synchronous XHR to a Service Worker**: works but is deprecated,
  single-threaded on the main thread, has terrible perf, and cannot
  be used off the main thread anyway.

**References**: COOP/COEP (web.dev "Cross-Origin Isolation"),
`SharedArrayBuffer` security requirements, `Atomics.wait` / `Atomics.notify`.

---

## Decision: OPFS via FileSystemSyncAccessHandle for the block device

**Decision**: the root filesystem is stored in OPFS (Origin Private
File System). The block driver is a TypeScript module running in its
own Worker (not the kernel Worker) that opens a handful of OPFS files
(one per extent/segment of the filesystem image) via
`FileSystemSyncAccessHandle`. The kernel sends read/write block
requests to the block driver through the same SAB ring-buffer
transport used for syscalls. Because the block driver Worker can
block synchronously on `SyncAccessHandle.read()` /
`.write()` / `.flush()`, the kernel's filesystem layer can present
blocking semantics to user processes.

**Rationale**:

- **OPFS is the only browser-local storage API that gives synchronous
  access** (via `FileSystemSyncAccessHandle`, which is only available
  inside a Worker). This matches the blocking-syscall requirement and
  delivers dramatically better throughput than IndexedDB for
  bulk/structured I/O.
- **IndexedDB is asynchronous by nature**, has poor perf for small
  random writes, has been shown in multiple benchmarks to be 10–100×
  slower for filesystem-like workloads, and makes implementing
  blocking filesystem syscalls painful. The user's technical decision
  explicitly requires OPFS and forbids IndexedDB.
- **OPFS is origin-private**: no other site, no other origin, and no
  extension can read it by default. This matches the spec's
  per-browser-profile privacy requirement (FR-015).
- **SyncAccessHandle must be opened from a Worker**, not the main
  thread. The block driver lives in its own Worker for exactly that
  reason.

**Layout on OPFS**: not one file per FS file — that explodes
metadata overhead and is slow on OPFS's backends. Instead we store the
filesystem as a small number of "image" files (e.g., a
metadata/inode segment and one or more data segments) and the
in-kernel VFS addresses into them as if they were a block device. The
exact layout is a data-model concern (see `data-model.md`).

**Alternatives considered**:

- **IndexedDB**: rejected above. User decision is explicit.
- **localStorage**: 5 MB cap, synchronous main-thread only, not a
  serious option.
- **Cache API**: key/value, async only, no fsync — not a filesystem.
- **Browser `FileSystemWritableFileStream` (async OPFS)**: works
  from the main thread but is asynchronous — rejected because it
  cannot implement blocking syscalls cleanly.

**Risk**: OPFS has a per-origin quota (typically a large fraction of
free disk, but browser-dependent and user-revocable). The block
driver MUST handle `QuotaExceededError` as the filesystem's
`ENOSPC`. The VFS MUST stay consistent under this condition (cf.
FR-014).

**References**: WHATWG File System Access API — OPFS and
`FileSystemSyncAccessHandle`; MDN "Origin private file system".

---

## Decision: WASI preview 1 as the syscall baseline

**Decision**: the kernel implements WASI preview 1 as the primary
syscall surface. Wasmtime's WASI snapshot defines the opcodes and
record layouts; we reimplement the server side ourselves in the
kernel.

**Rationale**:

- **Principle VI requires a POSIX-style syscall interface based on
  WASI.** Using WASI preview 1 gives direct compatibility with Rust
  `std` when userland crates target `wasm32-wasi`, with C and C++
  code compiled by `clang --target=wasm32-wasi`, and with any
  existing WASI-compatible library.
- **Preview 1 over preview 2**: preview 1 is frozen, widely
  implemented, and covers everything we need (file I/O, process
  info, time, environment, randomness, sockets via the
  `sock_*` extensions). Preview 2 (component model) is more powerful
  but overkill for v1 and not yet universally supported in toolchains.
  v2 adoption can be a later minor amendment to the OS ABI.
- **Toolchain reuse**: Rust targets `wasm32-wasi` out of the box; no
  patched toolchain, no custom syscall layer in userland.

**What "WASI" covers for free**: `fd_*` (read, write, seek, pread,
pwrite, pwrite, close, dup-ish via fd_renumber, fsync via fd_sync,
stat via fd_filestat_get), `path_*` (open, create, rename, unlink,
symlink, remove_directory, readlink, filestat_get/set_times),
`clock_time_get`, `random_get`, `environ_*`, `args_*`, `proc_exit`,
`sched_yield`, `poll_oneoff`, `sock_*`.

**What WASI does NOT cover** (motivating the extension syscalls in
the next decision): unix-socket-equivalent IPC with fd passing;
`posix_spawn`-like process creation; signal-equivalent delivery;
display-server-connect. See `contracts/syscalls.md` for the exact
extension list and the justification-per-syscall.

**Alternatives considered**:

- **Roll our own custom syscall set**: faster to prototype, much
  worse for portability. Rejected: Principle VI.
- **WASI preview 2 (component model)**: rejected for v1 as above; may
  be revisited post-v1.
- **Emscripten-style asyncify**: not a syscall surface — it's an ABI
  mapping to JS. Rejected.

**References**: WASI preview 1 spec
(`github.com/WebAssembly/WASI/blob/main/legacy/preview1/docs.md`);
Rust `wasm32-wasi` target documentation.

---

## Decision: Extension syscalls (IPC, spawn/wait, display, caps)

**Decision**: on top of WASI, the kernel exposes a small set of
extension syscalls. Each one is documented in
`contracts/syscalls.md` with its opcode, request/response layout, and
a "why WASI doesn't cover this" paragraph. The extensions are:

1. **IPC**:
   - `ipc_socket(type, flags) -> fd`: create a unix-socket-equivalent
     endpoint (STREAM or DGRAM-ish).
   - `ipc_bind(fd, path)`: bind to a path in the VFS.
   - `ipc_listen(fd, backlog)`: server-side listen.
   - `ipc_connect(fd, path)`: client-side connect to a bound socket.
   - `ipc_accept(fd) -> fd`: accept an incoming connection.
   - `ipc_send(fd, buf, fds_to_pass) -> n`: send bytes plus
     (optionally) an array of file descriptors to pass.
   - `ipc_recv(fd, buf, fds_out) -> (n, fds_received)`: receive
     bytes plus any passed fds.

   **Why WASI doesn't cover this**: WASI `sock_*` models
   network sockets, not AF_UNIX; it has no `bind(path)` and no file-
   descriptor passing. Both are load-bearing here — file-descriptor
   passing is how a process hands a display-server buffer handle to
   the display server.

2. **Process management (posix_spawn-style)**:
   - `proc_spawn(manifest) -> pid`: spawn a new process. `manifest`
     is an in-memory struct containing: binary path, argv, envp,
     stdin/stdout/stderr fd assignments (dup-ed from the parent's fd
     table), and initial working directory.
   - `proc_wait(pid, options) -> (status, signal)`: blocking wait on
     a child. Maps to `waitpid`.
   - `proc_kill(pid, signum)`: deliver a signal-equivalent.
   - `proc_self() -> pid`, `proc_parent() -> pid`.

   **Why WASI doesn't cover this**: WASI has `proc_exit` but no
   process creation at all — under the preview 1 model a WASI
   program is expected to be a one-shot invocation with no
   children. A real OS needs spawn.
   **No `fork`**: intentionally not provided. The user's decision is
   explicit: `fork()` cannot be implemented cleanly on the Worker
   model (there is no way to duplicate a WASM linear memory and its
   host resources atomically), and attempts to fake it with
   copy-on-write in userland are a well-known rabbit hole. Shells
   and toolchains that assume `fork()` will need to call
   `proc_spawn` instead. Porting notes are captured in the toolkit /
   shell crates.

3. **Signal-equivalent**:
   - Signals are delivered as messages on a per-process IPC endpoint
     that every process inherits at spawn. This avoids the
     complications of interrupting WASM execution asynchronously,
     which WASM does not support. A process that chooses not to
     poll its signal endpoint simply does not receive signals
     until its next blocking syscall (which is the same as
     uninterruptible kernel state on a traditional OS). Default
     dispositions (`SIGKILL` == terminate unconditionally, `SIGTERM`
     == default terminate) are enforced by the kernel at delivery
     time.

4. **Display-server connect**:
   - `display_connect() -> fd`: equivalent to
     `ipc_connect(/run/display)`, but encoded as a dedicated syscall
     so the kernel can enforce that only processes with the
     `DISPLAY_CLIENT` capability may open it.
   - Everything after the initial handshake is plain protocol over
     an IPC socket.

5. **Capability management**:
   - `cap_check(cap) -> bool`: is this process holding `cap`?
   - `cap_list() -> [cap]`: list this process's capabilities.
   - `cap_grant(pid, cap)` (privileged): grant a capability to
     another process. Only processes holding `CAP_GRANT` may call
     this.

**Rationale**: each extension is the minimum needed to satisfy a
constitution principle the spec requires. Every other impulse to add
a custom syscall is rejected at review. The syscall contract file
is the single source of truth; any new syscall is a spec/plan
amendment, not a spontaneous code change.

---

## Decision: Wayland-inspired wire protocol for the display server

**Decision**: the display server is reached by connecting to
`/run/display` and speaking a Wayland-inspired wire protocol over
that socket. The protocol is structured around object IDs, opcodes,
and events. Objects have types (registry, surface, buffer, output,
seat, pointer, keyboard, xdg_surface, xdg_toplevel, xdg_popup, ...);
each object type exposes a fixed set of request opcodes and emits a
fixed set of events.

**Core objects and interactions** (full spec in
`contracts/display-protocol.md`):

- `pmd_display` (object 1): the global registry. Clients bind to
  global singletons via `get_registry`.
- `pmd_compositor`: lets clients create `pmd_surface`s.
- `pmd_surface`: a rectangle of pixels with a role. Clients `attach`
  a buffer, `commit` it, and receive `frame` callbacks.
- `pmd_shm_pool` / `pmd_buffer`: shared-memory buffers allocated in a
  SAB region shared between the client Worker and the display
  server Worker, addressed by offset/stride/format.
- `pmd_output`: the one output ("monitor") this OS has.
- `pmd_seat`, `pmd_pointer`, `pmd_keyboard`: input.
- `pmd_xdg_wm_base`, `pmd_xdg_surface`, `pmd_xdg_toplevel`,
  `pmd_xdg_popup`: window roles (top-level, popup) with title,
  app id, min/max size, move/resize drags, close, state transitions
  (maximized, minimized).

**Rationale**:

- **Principle VII requires "protocol over API".** A wire protocol
  is the only architecture where "app without toolkit" is trivially
  possible. A hand-written client opens the socket, writes the right
  bytes, reads events, and draws — no hidden shared library, no
  private coupling.
- **Wayland is the right model.** Surfaces / buffers / commits /
  frame callbacks / roles is the distilled architecture that the
  industry has converged on. There is no need to invent a new
  model. Stealing the model without being wire-compatible lets us
  drop the 30+ protocol extensions that don't apply to a v1
  browser OS (DRM modifiers, DMA-BUF, tablet protocols, etc.) and
  ship a minimal protocol that fits in a single documented file.
- **Shared-memory buffers are the fast path.** Clients allocate a
  SAB region, tell the display server "the buffer for surface X
  lives at offset Y, stride Z, format ARGB8888," and commit it.
  The compositor reads pixels directly — no copy across the
  worker boundary.

**Why NOT wire-compatible with Wayland**: real Wayland assumes
file descriptors for shm pool fds and for DMA-BUFs. WASM/Web
Workers cannot pass fds across processes, we pass SAB handles
instead. Trying to be wire-compatible would mean faking fd-passing
semantics over a wire encoding that is incompatible with the
browser's primitives — pure pain for no benefit, since we can't run
real Wayland clients anyway.

**Alternatives considered**:

- **Pure library API (à la old-school X extensions)**: rejected,
  Principle VII.
- **Roll our own non-Wayland-shaped protocol**: no advantage; would
  lose all the benefits of having a model that has been thought
  through already.
- **Full Wayland wire compat**: rejected above.

**References**: Wayland protocol documentation; `xdg-shell` spec
(https://wayland.app/protocols/xdg-shell, for the window-role
shape only).

---

## Decision: Software compositor via OffscreenCanvas / ImageData

**Decision**: the display server composites all visible surfaces
entirely in software, in WASM, producing a final `ImageData`
(pixel array) that is submitted to the framebuffer driver via the
kernel's driver-call mechanism. The framebuffer driver, running
on the main thread, receives the buffer and does one
`putImageData` (or `transferFromImageBitmap` if
`OffscreenCanvas` is available) to the top-level canvas.

**Rationale**:

- **Spec and constitution explicitly forbid GPU in v1.** "No
  GPU-accelerated 3D rendering" and "No audio server in v1" are
  non-goals. A 2D software compositor is the simplest thing that
  can possibly work.
- **WebGPU is out of v1.** The future hook ("WebGPU-accelerated
  compositing") is documented in the plan but not implemented —
  that is a v2 amendment. The protocol and compositor are
  structured so that the software back end is swappable without
  touching the protocol.
- **`OffscreenCanvas` avoids main-thread contention on the
  compositor path.** The compositor Worker can hold an
  `OffscreenCanvas` obtained from the framebuffer driver and
  paint into it directly; only the final presentation tick
  touches the main thread.
- **Perceived latency budget.** A 1920×1080 `ImageData` copy is
  ~8 MB, which modern browsers blit in well under a frame on the
  target machine class. We stay inside the 100 ms input-to-pixel
  budget with comfortable margin at typical laptop resolutions.

**Alternatives considered**:

- **WebGL/WebGPU compositing**: ruled out as a v1 non-goal.
- **CSS compositing on DOM elements per surface**: rejected —
  that is the "UI in a trench coat" failure mode the project is
  explicitly built to avoid. The display server draws pixels into
  a framebuffer; it does not manipulate a DOM tree.

**References**: HTML `ImageData`, `OffscreenCanvas`,
`putImageData`, `transferFromImageBitmap`.

---

## Decision: Drivers as main-thread TypeScript modules

**Decision**: drivers (framebuffer, input, block [in its own
Worker], net, console) are TypeScript modules. Framebuffer and
input live on the main thread because that is where the DOM,
the `<canvas>`, and the input event handlers live. Block lives in
a dedicated Worker because `FileSystemSyncAccessHandle` requires
it. Net and console live on the main thread.

**Kernel↔driver contract**: drivers are called by the kernel
through a narrow postMessage + SAB protocol documented in
`contracts/driver-kernel.md`. For hot paths (framebuffer submit,
input event delivery) the contract uses shared memory rings; for
cold paths (device open, ioctl-equivalent) the contract uses
plain postMessage with an acknowledgement on a SAB slot. The
kernel never synchronously waits on the main thread (that would
deadlock the browser); any operation that requires a main-thread
driver response is modelled as asynchronous from the kernel's
point of view.

**Rationale**:

- **DOM can only be touched from the main thread.** Input event
  listeners, canvas element, and DOM APIs are main-thread only;
  there is no way around this.
- **Keeping drivers out of the kernel Worker keeps the kernel
  pure Rust and testable on the host target.** All browser-
  specific glue lives in TypeScript, not in Rust.
- **Principle II (strict layering) is satisfied**: drivers only
  talk to the browser substrate below and to the kernel above;
  user processes never touch a driver directly, they go through
  a device node in `/dev` and a syscall.

**Alternatives considered**:

- **Drivers in Rust inside the kernel**: would require binding
  kernel Rust to every browser API; loses testability; rejected.
- **Drivers as separate userland processes**: would be the most
  architecturally pure thing but requires proxy layers to get
  the driver the DOM access it needs, because a Worker cannot
  touch the DOM. The extra indirection is not worth it in v1;
  the main-thread-JS driver model is the pragmatic middle.

---

## Decision: Kernel in Rust, wasm32-unknown-unknown, no_std + alloc

**Decision**: the kernel is written in Rust and compiled to
`wasm32-unknown-unknown` (not `wasm32-wasi`, because the kernel
*implements* WASI and cannot consume it). It uses `#![no_std]`
with `extern crate alloc`. A minimal allocator (linked-list or
`talc`) sits on top of the WASM linear memory. The kernel
exports a small set of functions to its host JS (the
`kernel-worker.ts` bootstrap) — `kernel_init`, `kernel_step`,
`kernel_on_driver_message` — and imports a handful of JS-side
helpers for driver dispatch.

**Rationale**:

- **Rust's type system and borrow checker are a compelling fit
  for kernel code.** The alternatives (C, hand-rolled unsafe
  Rust, Zig) are all viable but the user's technical decision is
  explicit: Rust.
- **`wasm32-unknown-unknown`, not `wasm32-wasi`**, because the
  kernel is the server side of WASI — its code path for `fd_read`
  is "look up the fd in the process table and dispatch to the
  right filesystem back end," not "ask the host for bytes".
- **`no_std + alloc`** lets the kernel use `Vec`, `Box`,
  `BTreeMap`, and `String` without pulling in `std` (which would
  require a host to implement WASI for the kernel, defeating the
  point).

**Testability**: the kernel uses a `Platform` trait that
abstracts the three things that differ between "native test
build" and "wasm deployment build": the driver-message
transport, the clock, and a shutdown hook. In native tests, the
kernel is built for the host target with a mock `Platform`, and
`cargo test` runs all VFS / proc / IPC / syscall-dispatch tests
with zero browser involvement. This is how Principle X
(testability at every layer) is satisfied for the kernel.

**Alternatives considered**:

- **Kernel in TypeScript**: faster to bootstrap, far worse for
  correctness and perf, loses the "real OS in Rust" character of
  the project. Rejected.
- **Kernel in C**: fine technically, but doubles the toolchain
  footprint and loses Rust's safety properties. Rejected.

---

## Decision: Service worker precache for offline boot

**Decision**: the bootstrap page registers a service worker that
precaches the entire OS bundle (index.html, bootstrap JS, kernel
WASM, all WASM binaries, drivers, SW itself) on first install. The
service worker uses a cache-first strategy: requests are answered
from cache if available, otherwise fetched from the network, with
a fallback for the offline case.

**Rationale**:

- **Principle IV requires offline boot after first load.** A
  service worker is the only browser mechanism that can
  intercept navigation requests and serve them from local cache
  when the network is down.
- **Cache-first over stale-while-revalidate**: revalidation is
  nice but costs a network request per asset; we only update the
  cache when the SW itself is updated (by bumping a version string
  in `sw.ts`). This gives instant warm boots and a clear
  update/upgrade story.
- **Precaching on install** is simple and reliable; an
  alternative (lazy caching on first request) could leave the
  user with a half-cached system if they close the tab before
  all assets have been touched.

**Rationale for using a versioned cache name**: when we ship a
new build, the service worker's `install` handler creates a new
cache (`pmos-vX`), populates it, and then `activate` removes
old caches. This gives atomic upgrades: either the user gets the
whole new version or they stay on the old one.

**Alternatives considered**:

- **No service worker, HTTP cache only**: works until the user
  is offline on reload; rejected because Principle IV demands
  offline boot.
- **Service worker with runtime caching only**: fragile; rejected.

---

## Decision: Deployment — static host with COOP/COEP

**Decision**: the product is deployed as a directory of static
files (`dist/`) to any static file host that supports setting the
`Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp` headers.

**Known-good hosts** (each documented in `quickstart.md` with
the concrete config):

- **Cloudflare Pages**: drop `dist/` into the project, add a
  `_headers` file at the root with the two headers.
- **Netlify**: same approach using Netlify's `_headers` file.
- **GitHub Pages**: does not support custom headers directly;
  serve through a Cloudflare Worker (or similar) that injects
  the headers. Documented as a slightly more involved path.
- **S3 + CloudFront**: add a response-header policy to the
  CloudFront distribution that sets the two headers. Documented
  with the exact policy JSON.

**Rationale**:

- **COOP/COEP are mandatory**: `SharedArrayBuffer` is gated on
  cross-origin isolation in all evergreen browsers. Without the
  headers, `SharedArrayBuffer` is undefined and the syscall
  transport cannot work at all.
- **Static-host-only**: Principle III requires zero backend.

**Consequence for third-party assets**: every asset the OS loads
at boot must either be same-origin or be served with CORP
(`Cross-Origin-Resource-Policy`) set to `same-origin` /
`same-site`. We avoid this problem entirely by bundling
everything into `dist/` at build time — no runtime third-party
CDN fetches.

**Alternatives considered**:

- **Deploying to a backend-providing host** (Vercel, Netlify
  Functions, etc.): Principle III. Rejected.
- **Shipping inside an Electron-style wrapper**: out of scope —
  the product is explicitly a browser-tab OS.

**References**: web.dev "COOP and COEP explained", MDN
`Cross-Origin-Opener-Policy`, `Cross-Origin-Embedder-Policy`.

---

## Decision: Playwright for integration and layering tests

**Decision**: the integration test harness is Playwright. A
hidden test-harness app is built into the default image and is
auto-started (or on-demand started) when a URL parameter
(`?pmos-test=1`) is present. The harness app opens a test-only
IPC channel to the Playwright driver via the console driver and
accepts commands like "spawn X," "read /proc/pid/status,"
"assert that N processes are alive." Playwright drives the
browser, ships test scripts to the harness, and asserts on the
results.

**Rationale**:

- **Full-stack integration tests are the only way to verify the
  layering acceptance test (Principle II).** "Kill the shell,
  launch a replacement, verify apps are still alive and
  reattach" cannot be unit-tested in isolation; it requires the
  whole stack running.
- **Playwright is the current-best headless-browser test
  driver**. It supports Chromium, Firefox, and WebKit; it can
  set COOP/COEP headers on its internal file server; it has
  reliable hooks for reading console output (which is how the
  harness communicates back).
- **Running the harness as a real WASM app inside PMos** (rather
  than by injecting JS into the page) means the harness
  exercises the same syscall surface and IPC primitives as any
  other app — we are testing the real system, not a test-special
  shortcut.

**Alternatives considered**:

- **Puppeteer**: Chromium-only; rejected.
- **Cypress**: weaker at cross-origin-isolated configurations
  and at multi-tab scenarios; rejected.
- **Pure Rust headless-browser test via `wasm-bindgen-test`**:
  good for per-crate tests but cannot orchestrate the full
  multi-Worker system. Complementary, not a substitute.

---

## Decision: Build orchestration via Justfile + esbuild

**Decision**: the top-level build is driven by a `Justfile`. It
invokes `cargo build` (with `--target wasm32-unknown-unknown`
for the kernel and `--target wasm32-wasi` for userland),
`esbuild` for the TS bundle, and a small Rust `xtask`-style
post-build step that assembles `dist/` (copies WASM binaries to
the right paths, generates `_headers`, computes SRIs, etc.).
There is no npm framework, no webpack, no Vite, no bundler
magic.

**Rationale**:

- **User decision is explicit**: "no npm framework, no bundler
  beyond what is necessary (esbuild is fine). Keep the JS
  surface tiny; it should feel like firmware." The plan honours
  that.
- **`just` has no runtime dependency** on Node or anything else,
  which keeps the bootstrap story simple for contributors.
- **`esbuild` is the smallest fast bundler** that can handle
  TypeScript → ES2022 emit cleanly.

**Alternatives considered**:

- **npm scripts + webpack**: too much machinery, too many
  dependencies, conflicts with the "firmware, not a web app"
  ethos.
- **Bazel**: overkill for this repo size.
- **Pure Makefile**: works, but `just` is tidier for recipe
  organisation and argument handling.

---

## Decision: Package format — tar + manifest.toml

**Decision**: an application package is a single tar archive
containing:

```
manifest.toml       # name, version, exec path, icon, display name, declared caps
bin/<name>.wasm     # the WASM binary
<optional assets>
```

Installing an application means extracting the tar under
`/opt/<name>/` and writing a desktop entry under
`/usr/share/applications/<name>.desktop` (a small text file
pointing to the exec path and icon). The launcher enumerates
`/usr/share/applications/*.desktop` at startup (and refreshes
periodically or on demand).

**Rationale**:

- **Simplicity wins.** Tar is universal, has a Rust
  implementation (`tar` crate) that works on `wasm32-wasi`, and
  is easy to produce with `tar` on the developer's host.
- **manifest.toml over JSON**: TOML is friendlier to humans who
  are editing manifests by hand, and we already have a TOML
  parser in the Rust workspace for `Cargo.toml` reuse.
- **No central registry, no signing (v1)**: the spec explicitly
  forbids a central app store. Signing is a v1 non-goal; the
  threat model is "a user who copies a bundle they trust" and
  not "an adversary distributing malware through a marketplace".
  Signing is a v2 amendment.

**Alternatives considered**:

- **zip instead of tar**: more ubiquitous on Windows dev
  machines, but the compression is inconsistent and random
  access is not actually needed (we extract once at install
  time). Tar is simpler.
- **Custom archive format**: adds complexity for no benefit.

---

## Summary: NEEDS CLARIFICATION review

Every `NEEDS CLARIFICATION` marker raised by the plan's
Technical Context is resolved above. A final sweep confirms:

- Language choice — Rust (kernel, display server, toolkit, all
  userland) + TypeScript (bootstrap, drivers, SW). **Resolved.**
- Storage backend — OPFS via `FileSystemSyncAccessHandle`.
  **Resolved.**
- Syscall transport — SAB ring buffer + `Atomics.wait`.
  **Resolved.**
- Syscall surface — WASI preview 1 + a documented minimal
  extension set. **Resolved.**
- Display server protocol — Wayland-inspired, not wire-compatible,
  documented in `contracts/display-protocol.md`. **Resolved.**
- Compositor strategy — software, via `OffscreenCanvas` /
  `ImageData`. **Resolved.**
- Offline strategy — service worker precache, versioned cache.
  **Resolved.**
- Deployment target — static host with COOP/COEP; concrete
  configs documented for CF Pages, Netlify, GitHub Pages via
  CF Worker, and S3+CloudFront. **Resolved.**
- Integration test tooling — Playwright + an in-WASM test
  harness app. **Resolved.**
- Build orchestration — Justfile + esbuild + `xtask`.
  **Resolved.**
- Package format — tar + manifest.toml. **Resolved.**

No open clarifications remain. Ready for Phase 1.
