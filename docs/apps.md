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

This document explains _how_ to write an app along either path,
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

The toolkit exposes `App`, `Window`, `BufferPool`, its production WASI
`FdConnection`, and the draw primitives (`Canvas`, `Color`, `Rect`, `Theme`) —
see `crates/toolkit/src/lib.rs` for the full re-export surface. This complete,
cfg-gated `main.rs` paints a minimal window, advances bounded buffer uploads,
parks when idle, and exits cleanly on close:

```rust
#[cfg(target_arch = "wasm32")]
use toolkit::theme::Theme;
#[cfg(target_arch = "wasm32")]
use toolkit::{App, BufferPool, ClientError, CommitProgress, Connection, Window};

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn proc_exit(rval: i32) -> !;
    }

    pub fn run() {
        println!("hello: starting");
        let connection = match toolkit::wasi::FdConnection::connect() {
            Ok(connection) => connection,
            Err(errno) => unsafe { proc_exit(errno) },
        };
        match super::run_window(connection) {
            Ok(()) => unsafe { proc_exit(0) },
            Err(_) => unsafe { proc_exit(1) },
        }
    }
}

#[cfg(target_arch = "wasm32")]
extern crate alloc;

#[cfg(target_arch = "wasm32")]
fn run_window<C: Connection>(connection: C) -> Result<(), ClientError> {
    let mut app = App::connect(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Hello PMos")?;
    window.set_app_id("com.example.hello")?;
    window.commit()?;

    let theme = Theme::LIGHT;
    let mut painted = false;
    let mut pool: Option<BufferPool> = None;

    loop {
        let _events = window.dispatch()?;
        if window.close_requested() {
            return Ok(());
        }

        if let Some(buffers) = pool.as_mut().filter(|buffers| buffers.commit_pending()) {
            if buffers.progress_commit(&mut window)? == CommitProgress::Committed {
                painted = true;
                println!("hello: ready");
            }
        }

        if !painted && window.is_configured() {
            let (width, height) = match window.configured_size() {
                (0, 0) => (320, 200),
                size => size,
            };
            if pool.is_none() {
                pool = Some(BufferPool::new(window.app_mut(), width, height)?);
            }
            let buffers = pool.as_mut().expect("pool created above");
            if let Some(mut canvas) = buffers.acquire_back_canvas() {
                canvas.clear(theme.window_background);
                drop(canvas);
                if buffers.commit_and_swap(&mut window)? == CommitProgress::Committed {
                    painted = true;
                    println!("hello: ready");
                }
            }
        }

        window.flush_outbound()?;
        if pool.as_ref().is_some_and(BufferPool::commit_pending) && !window.outbound_pending() {
            continue;
        }
        window.wait(None)?;
    }
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    wasm_main::run();
    #[cfg(not(target_arch = "wasm32"))]
    println!("hello-app (host build): build for wasm32-wasip1 to run in PMos");
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
- `BufferPool::new` allocates a double-buffered `pmd_shm_pool`, and
  `acquire_back_canvas` hands you a `Canvas` into the back buffer. A frame may
  take multiple bounded upload quanta, so `commit_and_swap` can leave staged
  work for `progress_commit`. The loop flushes once per turn, continues locally
  only while staged work remains and the transport has no suffix, and otherwise
  parks in `window.wait(None)`; this prevents both an unpublished large frame
  and an idle busy loop.
- The `Connection` generic keeps the window logic transport-independent. Tests
  can pass `toolkit::MemoryConnection`; production `main` uses the public
  `toolkit::wasi::FdConnection`, which owns the `display_connect` fd and its
  backpressure-aware WASI read/write/poll path. Keep the cfg-gated allocator,
  WASI exit bridge, and top-level `main` shown above when creating a standalone
  app.

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
  `crates/toolkit-free-client/src/lib.rs` (the shared
  `display-proto` codec is its only runtime dependency; no `toolkit`).
- **Non-Rust clients.** The wire protocol is language-neutral.
  A Zig, C, or TypeScript toolkit would speak the same wire
  format as the Rust toolkit and interoperate with everything
  else in the system. Principle VII is what makes that
  possible.

The shipped-v1 raw session is `display_connect → get_registry → discover and
bind compositor + shm + xdg_shell + seat → create keyboard + surface +
xdg_toplevel → configure/ack → allocate and write a bounded pool → attach +
damage + commit → input → close cleanup`. Numeric registry names are discovered
from events; clients must not assume the current 1–4 values. `FreeSession`
drives those transitions, while its `FreeClient` request helpers still map
one-for-one to contract table rows:

```rust
use toolkit_free_client::{FreeSession, SessionError, SessionSignal};

fn begin() -> Result<(FreeSession, Vec<u8>), SessionError> {
    let mut session = FreeSession::new()?;
    let get_registry = session.drain_outbound();
    Ok((session, get_registry))
}

fn receive(
    session: &mut FreeSession,
    server_bytes: &[u8],
) -> Result<(Vec<SessionSignal>, Vec<u8>), SessionError> {
    let signals = session.push_server_bytes(server_bytes)?;
    let protocol_replies = session.drain_outbound();
    Ok((signals, protocol_replies))
}
```

The production fd read/write loop is in
`crates/toolkit-free-client/src/main.rs`. Native isolation in
`tests/conformance.rs` pairs the same state machine with a real
`display_server::Server`; no production code imports that server crate.

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
`toolkit-free-client.spec.ts` is the raw-client reference: it launches the
actual `/bin/toolkit-free-client` WASI artifact in a separate Worker, observes
its exact framebuffer colours, routes a physical key through
`pmd_keyboard.key`, closes it through the shell/display protocol, and requires
the Worker to disappear. Run the focused Chromium/Firefox gate shown in
`quickstart.md §6` after assembling `dist/`.

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

Built-in binaries live inside the Rust workspace. Shipping a new one requires
four explicit registrations:

1. Create `crates/<name>/` with a `Cargo.toml` and `src/main.rs`.
2. Register `<name>` in the workspace root `Cargo.toml`
   `members` list.
3. Add the package to the `wasm32-wasip1` release build in the `Justfile`.
4. Add each shipped `(crate_name, bin_name)` pair to `USERLAND_BINS` in
   `crates/xtask/src/assemble_dist.rs`.

`just build` compiles everything to `wasm32-wasip1`. The
`xtask assemble-dist` step copies the resulting binaries to
`dist/assets/bin/<bin_name>.wasm`. Assembly deliberately copies only the fixed
`USERLAND_BINS` inventory and fails when a listed artifact is missing; it does
not discover workspace crates or binary targets by naming convention.

To make a built-in GUI app visible in the launcher, also add its desktop entry
under `crates/kernel/assets/usr/share/applications/` and register that entry in
both the fresh-image table in `crates/kernel/src/fs/opfs/mkfs.rs` and the
existing-image migration table in `crates/kernel/src/fs/seed.rs`.

Third-party apps ship as `.pmpkg.tar` bundles. Put a `pkg.toml` next to the
crate's `Cargo.toml`, build the WASI binary, then package it from the PMos
checkout:

```shell
cargo build --locked --release --target wasm32-wasip1 -p sample-app
cargo run --locked -p xtask -- package sample-app
```

`xtask package` embeds the final executable under the manifest's `exec.binary`
path, generates a SHA-256 entry for every payload file, validates the completed
archive, and writes `dist/pkgs/<name>-<version>.pmpkg.tar`. The user explicitly
drops that archive into PMos, then runs:

```text
pkginstall /home/user/Downloads/<name>-<version>.pmpkg.tar
```

The installer verifies tar structure, payload integrity, WASM magic, and the
v1 capability policy before touching the live filesystem. It publishes
`/opt/<name>/` and `/usr/share/applications/<name>.desktop` transactionally;
the launcher discovers the entry without a reboot. V1 third-party packages may
require only `DISPLAY_CLIENT`. Optional privileged capabilities are retained as
metadata but never granted. Full details are in
`specs/001-browser-os-v1/contracts/package-manifest.md`.

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
  the `.pmpkg.tar` bundle format and install policy.
- `.specify/memory/constitution.md` — the ten principles;
  Principle VII (protocol over API) and Principle VIII
  (bottom-up construction) are the ones that shape the
  two-path app story described here.
