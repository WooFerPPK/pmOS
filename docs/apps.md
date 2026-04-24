# Developing Apps for PMos

## Overview

A PMos app is a `wasm32-wasip1` binary that runs in its own Worker
(its own WASM linear memory, its own process) and talks to the
display server at `/run/display` over a Wayland-inspired wire
protocol. Two paths lead to a functioning window:

1. **The toolkit path.** Link the `toolkit` crate, let it wrap the
   wire protocol behind an `App` / `Window` / `BufferPool` / widget
   API, and write ordinary Rust. This is how almost every app
   should be written.
2. **The raw-protocol path.** Depend on `display-proto` (the wire
   types) and hand-roll the request/event dance yourself. No
   toolkit in the dependency graph. The `toolkit-free-client`
   crate proves this path works and exists as a standing
   conformance test.

Both paths are supported and both MUST work — the wire protocol
is the source of truth, the toolkit is a library wrapper. See
`.specify/memory/constitution.md` Principle VII for the "why."

This document explains *how* to write an app along either path,
how to test it, how to reach the filesystem / IPC / capability
syscalls alongside the display protocol, and how apps are
shipped in v1. For environment setup (installing Rust targets,
`just build`, `just test`) see `specs/001-browser-os-v1/quickstart.md`.

## What you need before starting

- A working PMos build tree — `just build` must succeed on your
  machine. Follow `specs/001-browser-os-v1/quickstart.md §1–§4`
  (prerequisites, building, running locally, running the tests).
- Familiarity with Rust and Cargo workspaces. PMos apps are
  ordinary crates in the workspace at `crates/<name>/`.
- An editor of your choice. No special tooling is required; the
  toolkit is plain Rust.

If `just test` is green against a fresh checkout, you have
everything you need to start.

## The toolkit path (recommended)

### Cargo layout

Create a new crate under `crates/` and register it in the
workspace root `Cargo.toml`. A minimal app needs only the
toolkit as a runtime dependency — the toolkit re-exports every
`display-proto` type it needs, so your `Cargo.toml` stays
small:

```toml
[package]
name = "hello-app"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "hello"
path = "src/main.rs"

[dependencies]
toolkit = { path = "../toolkit" }
```

If you need direct access to wire types not re-exported from
`toolkit`, add `display-proto = { path = "../display-proto" }`.
Most apps do not.

### Minimal `main.rs`

The toolkit exposes `App`, `Window`, `BufferPool`, and the draw
primitives (`Canvas`, `Color`, `Rect`, `Theme`) — grep
`crates/toolkit/src/lib.rs` for the full re-export surface. A
minimal window that paints its background and exits cleanly on
close looks like this:

```rust
use toolkit::protocol::Connection;
use toolkit::{App, BufferPool, ClientError, Window};
use toolkit::theme::Theme;

fn run<C: Connection>(connection: C) -> Result<(), ClientError> {
    let mut app = App::connect(connection)?;

    let mut window = Window::new(&mut app)?;
    window.set_title("Hello PMos")?;
    window.set_app_id("com.example.hello")?;
    window.commit()?;

    let theme = Theme::LIGHT;
    let mut pool: Option<BufferPool> = None;

    loop {
        let _events = window.dispatch()?;

        if window.close_requested() {
            break;
        }

        if window.is_configured() && pool.is_none() {
            let (w, h) = match window.configured_size() {
                (0, 0) => (320, 200),
                size => size,
            };
            let mut fresh = BufferPool::new(window.app_mut(), w, h)?;
            if let Some(mut canvas) = fresh.acquire_back_canvas() {
                canvas.clear(theme.window_background);
            }
            fresh.commit_and_swap(&mut window)?;
            pool = Some(fresh);
        }
    }

    Ok(())
}
```

A few notes on what the snippet assumes:

- `App::connect(connection)` runs the
  `display.get_registry → registry.global* → registry.bind` handshake
  and returns an `App` that has `pmd_compositor`, `pmd_shm`, and
  `pmd_xdg_shell` pre-bound. See `crates/toolkit/src/app.rs` for the
  full doc comment.
- `Window::new(&mut app)` sends `compositor.create_surface` +
  `xdg_shell.get_toplevel`; `set_title` / `set_app_id` / `commit`
  populate metadata and kick the server into sending the first
  `configure` event. `Window::dispatch` internally acks
  configure and records close — the caller just checks
  `is_configured` / `configured_size` / `close_requested`.
- `BufferPool::new` allocates a double-buffered `pmd_shm_pool`;
  `acquire_back_canvas` hands you a `Canvas` into the back
  buffer; `commit_and_swap` attaches, damages, commits, and
  flips. See `crates/toolkit/src/draw/buffer.rs` for the
  frame-callback integration hooks that a real app would plug
  into.
- The `Connection` generic is the toolkit's transport-abstraction
  trait; `main` is responsible for constructing one and calling
  `run(connection)`. In tests, `toolkit::MemoryConnection` is the
  in-memory `Connection` implementation. In production, a thin
  wrapper over the `display_connect` syscall fd plays that role —
  that wrapper lands alongside the T110 display-server
  protocol-dispatch loop. See `Using PMos syscalls` below for
  the extern-level detail.

For a longer walk-through with more paint logic and a chromed
window frame, see `specs/001-browser-os-v1/quickstart.md §5`.

## The raw-protocol path

You would reach for the raw path in three situations:

- **Performance-sensitive clients** that don't want the
  toolkit's buffer-management abstraction, event-loop
  overhead, or widget tree. You trade ergonomics for exact
  control of the wire.
- **Principle VII conformance.** Any test, fixture, or demo
  that needs to prove the toolkit isn't privileged writes
  directly against `display-proto`. The `toolkit-free-client`
  crate is the standing example — see
  `crates/toolkit-free-client/src/lib.rs` (276 lines,
  dependency on `display-proto` only, no `toolkit`).
- **Non-Rust clients.** The wire protocol is language-neutral.
  A Zig, C, or TypeScript toolkit would speak the same wire
  format as the Rust toolkit and interoperate with everything
  else in the system. Principle VII is what makes that
  possible.

The shape of a raw-protocol session is
`display_connect → get_registry → bind compositor + shm + xdg_wm_base → allocate SAB pool → create surface + xdg_toplevel → attach + commit`.
Each helper in `FreeClient` maps one-for-one to a spec table
row:

```rust
use toolkit_free_client::{FreeClient, FreeClientError};

fn walk() -> Result<Vec<u8>, FreeClientError> {
    let mut client = FreeClient::new();

    let registry = client.get_registry()?;
    let compositor = client.registry_bind(registry, 1, "pmd_compositor", 1)?;
    let _shm = client.registry_bind(registry, 2, "pmd_shm", 1)?;
    let _xdg_shell = client.registry_bind(registry, 3, "pmd_xdg_shell", 1)?;

    let surface = client.compositor_create_surface(compositor)?;
    // Buffer allocation + attach/damage/commit follow the same
    // shape — see FreeClient::surface_attach / surface_damage /
    // surface_commit, each of which maps to a §9 spec row.

    client.surface_commit(surface)?;
    Ok(client.drain_outbound())
    // ... forward the drained bytes to /run/display via ipc_send.
}
```

The full surface is in `crates/toolkit-free-client/src/lib.rs`;
the `tests/conformance.rs` integration pairs `FreeClient`
against a real `display_server::Server` and walks the full
request sequence.

For the normative reference to every wire message see
`specs/001-browser-os-v1/contracts/display-protocol.md`; for a
longer worked example with prose see
`specs/001-browser-os-v1/quickstart.md §6`.

## Testing your app

### Isolation tests against a mock display server

Apps should have isolation tests that drive the toolkit (or the
raw client) against a mock server — no real display server, no
browser. This is the Principle X shape: the test runs in
`cargo test`, exercises the protocol surface, and asserts on
the request sequence without any integration glue.

The pattern the toolkit uses in `crates/toolkit/tests/window.rs`
lifts cleanly into an app test crate. Define an in-memory
`Connection` implementation that buffers outbound bytes and
lets the test push inbound event bytes:

```rust
use std::collections::VecDeque;
use toolkit::protocol::Connection;

#[derive(Default)]
struct LoopbackConnection {
    outbound: Vec<u8>,
    inbound: VecDeque<Vec<u8>>,
}

impl Connection for LoopbackConnection {
    fn send(&mut self, bytes: &[u8]) {
        self.outbound.extend_from_slice(bytes);
    }
    fn drain_outbound(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbound)
    }
    fn recv(&mut self) -> Vec<u8> {
        self.inbound.pop_front().unwrap_or_default()
    }
}
```

Seed the connection with the three required `registry.global`
advertisements before calling `App::connect`, drain the
outbound buffer to inspect the exact request sequence the app
sent, and push framed event bytes to simulate server responses
(configure, close, input). See `crates/toolkit/tests/window.rs`
for the framing helpers and the `seed_registry` / parse
scaffold that test each step of the handshake.

### End-to-end integration

Full browser integration (kernel spawn, real display server,
Playwright assertions) lives under `web/tests/integration/`.
The pattern is blocked today on T110 — the display server's
protocol-dispatch loop is still landing, and the existing
`real-kernel.spec.ts` fixture exercises the kernel spawn path
rather than the wire-framed display protocol. Once T110's
dispatch is in tree, app-level integration tests will follow
the `boot-to-desktop.spec.ts` shape described in
`quickstart.md §4.5`.

## Using PMos syscalls

Most apps never need to hit a syscall directly. The toolkit
handles the display protocol; `std::fs`, `std::io`, and
`std::env` cover the WASI baseline for files, pipes, stdin/stdout,
arguments, and environment. If all your app does is paint a
window, stop here.

Apps that need capabilities beyond WASI preview 1 — spawning
subprocesses, sending signals, watching a filesystem path,
receiving a host-side file drop, or opening `/run/display`
directly — reach the PMos extension syscalls via the
`pmos_ext` import namespace. The canonical reference is
`specs/001-browser-os-v1/contracts/syscalls.md` (full list,
errno semantics, request/response layouts). A one-line
extern block pins `display_connect` in a raw client:

```rust
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn display_connect() -> i32;
}
```

The same block shape takes `proc_spawn`, `fs_watch`,
`host_file_recv`, `cap_check`, and the rest; see
`crates/abi/src/ext.rs` for the opcode numbers the kernel
recognises and `crates/hello-cap-check/src/lib.rs` for a
minimal working example of a raw extern call.

## Packaging and distribution

**In v1**, apps live inside the Rust workspace. Adding one is
three edits:

1. Create `crates/<name>/` with a `Cargo.toml` and `src/main.rs`.
2. Register `<name>` in the workspace root `Cargo.toml`
   `members` list.
3. Add `<name>` to the `just build` invocation in the
   `Justfile` (or let `xtask assemble-dist` pick it up via the
   conventional crate-name lookup).

`just build` compiles everything to `wasm32-wasip1`. The
`xtask assemble-dist` step copies the resulting binaries to
`dist/assets/bin/<name>.wasm`. A `/usr/share/applications/<name>.desktop`
entry (bundled in the initramfs for built-in apps) wires the
app into the launcher.

**Coming:** third-party apps — packages written outside the
workspace, distributed without a PMos source checkout — will
ship as `.pmpkg.tar` bundles installed via a `pkginstall` CLI.
The bundle format is defined in
`specs/001-browser-os-v1/contracts/package-manifest.md`; the
tooling is tracked in T198 (`pkginstall` bundled CLI), T203
(`xtask package` subcommand), and T204 (`just push-sample`
target). **None of these tasks has landed yet.** Do not plan
your distribution flow around third-party packaging until they
do — until then, in-workspace is the only path.

## Accessibility — a v1 non-goal

v1 makes no accessibility claim (FR-045 in
`specs/001-browser-os-v1/spec.md`). Screen-reader integration,
keyboard-only navigation, high-contrast themes, and
ARIA-equivalent metadata are deliberately deferred to a v2
constitutional amendment, at which point the toolkit's
event-routing and widget-state surfaces are the documented
amendment target. Do not block app development waiting for
a11y to land; equally, do not ship apps that claim
accessibility support in v1 — the claim can't be backed by
anything in the substrate today.

## Further reading

- `specs/001-browser-os-v1/quickstart.md` — building, running,
  testing; worked toolkit + raw-protocol examples in §§5–6.
- `specs/001-browser-os-v1/contracts/display-protocol.md` —
  normative wire-protocol reference.
- `specs/001-browser-os-v1/contracts/syscalls.md` — WASI
  preview 1 baseline + PMos extension syscalls (opcodes,
  layouts, errnos).
- `specs/001-browser-os-v1/contracts/package-manifest.md` —
  the future `.pmpkg.tar` bundle format.
- `.specify/memory/constitution.md` — the ten principles;
  Principle VII (protocol over API) and Principle VIII
  (bottom-up construction) are the ones that shape the
  two-path app story described here.
