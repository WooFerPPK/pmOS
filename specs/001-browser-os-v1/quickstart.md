# Quickstart: Building, Running, and Extending PMos v1

**Branch**: `001-browser-os-v1` | **Date**: 2026-04-13
**Feature**: [spec.md](./spec.md) | **Plan**: [plan.md](./plan.md)

This is the developer-facing getting-started doc. It covers:

1. Prerequisites
2. Building from source
3. Running locally (dev server with COOP/COEP)
4. Running the test suites
5. A worked example — writing a "hello" app
6. A worked example — writing the same app without the toolkit
7. Packaging and installing a third-party app
8. Deploying to a static host

Follow the sections in order on your first read; later, use them
as a reference.

The validation ledger retains both the 2026-08-08 Rust 1.94.1 source-isolated
walk and the definitive 2026-08-11 Rust 1.97.1 Ubuntu 24.04.4 exact-source
walk, including commands, hashes, results, and isolation boundaries, in
[`quickstart-validation.md`](./quickstart-validation.md).

---

## 1. Prerequisites

Install the following on your development machine. Version numbers
are floors; newer is fine.

- **Git**, for cloning and inspecting the source tree.
- A native C linker/build toolchain (`build-essential` on Debian/Ubuntu),
  required by host Rust binaries and tests.
- **Rust** (via rustup), stable channel, latest.
- **Rust targets**: `rustup target add wasm32-unknown-unknown
  wasm32-wasip1`
- **Node.js** 20 or newer (for esbuild, Vitest, Playwright).
- **just** (`cargo install just`) for the build orchestrator.
- A modern browser with OPFS, service workers, cross-origin isolation,
  `SharedArrayBuffer`, `Atomics.wait`, and dedicated Workers. Chromium and
  Firefox are the automated release targets.
- **tar** (any POSIX tar), for the app package format.

Chromium and Firefox pass the persistent-root release gate. Playwright's Linux
WebKit build exposes no OPFS API at all, so PMos classifies that substrate as
unsupported and stops before boot rather than presenting a desktop that would
silently lose files. A Safari build is supported only when the same required
capability probe succeeds; Safari is not otherwise claimed as a release target.

Optional but recommended:

- `wasm-tools` (`cargo install wasm-tools`) for inspecting WASM
  binaries during debugging.

After cloning the repository and entering its root, install the locked web
toolchain and Playwright browser payloads before running the complete test gate:

```shell
$ just node-deps
$ cd web
$ npx --no-install playwright install --with-deps chromium firefox webkit
$ cd ..
```

`just build` also depends on `node-deps`, so a first build or a build after
`just clean` installs the exact `web/package-lock.json` dependency set
automatically. The prerequisite skips `npm ci` while that lock-backed install
is current. The browser payload install is required for the complete `just
test` gate. If the host is not Debian/Ubuntu, install the equivalent native
browser libraries through the platform package manager and run `npx
--no-install playwright install chromium firefox webkit` after `just
node-deps`.

---

## 2. Building from source

From the repository root:

```shell
$ just build
```

The `Justfile` is the authoritative build recipe; inspect the exact current
commands with:

```shell
$ just --show build
```

In summary, it performs a locked release build of the kernel for
`wasm32-unknown-unknown` with its production feature set, a locked release
build of every shipped userland binary for `wasm32-wasip1`, bundles the four
browser entry points (`bootstrap`, kernel Worker, user Worker, and service
worker), and runs `xtask assemble-dist`.

The assembly step builds the static `dist/` directory: it copies the WASM and
JavaScript assets, `index.html`, and `_headers`, then writes
`dist/manifest.json` with the ordered asset/deployment lists, a release digest,
and a SHA-256 digest for every file. These are manifest integrity records, not
HTML Subresource Integrity attributes.

Result: `dist/` is a directory of static files ready to serve.

```shell
$ just clean   # removes target/, build/, dist/, web/node_modules/, and web test reports
$ just build   # restores locked Node dependencies and rebuilds from scratch
$ just test    # runs the complete release gate against that build
```

---

## 3. Running locally

PMos requires COOP/COEP headers to enable `SharedArrayBuffer`.
Don't open `dist/index.html` from `file://` — that won't work.
Instead:

```shell
$ just dev
```

Which runs:

```shell
$ cargo run -p xtask -- dev-server --dir=dist --port=8080
```

`xtask dev-server` is a ~60-line Rust program that serves `dist/`
over HTTP on port 8080 with:

- `Cross-Origin-Opener-Policy: same-origin`
- `Cross-Origin-Embedder-Policy: require-corp`
- Correct MIME types for `.wasm`, `.js`, `.toml`, `.png`
- No caching (so edit-and-refresh works)

Now open `http://localhost:8080/` in your browser. You should see:

1. A brief boot message (one or two lines in the devtools
   console).
2. The canvas fill with the desktop background.
3. A taskbar appear along one edge and a launcher button become
   clickable.

From the launcher you can open Terminal, Files, Edit, Settings,
and System Monitor.

**If you see a blank page and `SharedArrayBuffer is not
defined` in the devtools console**: the COOP/COEP headers are
not being served. Check `dist/_headers`, the dev-server output,
and your browser's devtools Network tab → response headers.

---

## 4. Running the tests

PMos keeps focused layer tests separate from the full-stack browser gate. The
focused `just` targets below are useful while iterating; `just test` runs the
complete release sequence.

### 4.1 Kernel isolation tests

```shell
$ just test-kernel
```

Runs `cargo test --locked -p kernel` on the native host. The kernel uses a
`Platform` trait that has a native implementation for tests. No
browser is involved. The tests exercise the VFS, the process
table, IPC pipes and sockets, the syscall dispatcher, and the
capability system.

**These tests run in every CI build as a gate.** A change that
breaks a kernel isolation test cannot reach integration.

### 4.2 Display server isolation tests

```shell
$ just test-display-server
```

Runs `cargo test -p display-server`. The display server uses a
mock client + mock framebuffer. Tests cover the protocol
decoder/encoder, surface/commit state machine, compositor
stacking, input routing, and the toolkit-free conformance client
(cf. `contracts/display-protocol.md §18`).

### 4.3 Toolkit isolation tests

```shell
$ just test-toolkit
```

Runs `cargo test -p toolkit`. The toolkit is tested against a
mock display server that records every request and feeds
synthetic events. This verifies layout, event dispatch, and that
the toolkit produces exactly the protocol messages the contract
describes.

### 4.4 Driver isolation tests

```shell
$ just test-drivers
```

Runs `cd web && npx --no-install vitest run`. Drivers are tested with a mock
kernel ring buffer. Tests cover framebuffer submit + present,
input event normalization, OPFS block I/O (using a jsdom stub
OPFS), net driver fetch stub, and console driver.

### 4.5 Integration tests (Playwright)

```shell
$ just test-integration
```

Runs `cd web && npx --no-install playwright test`. The configured dev server
serves the assembled `dist/` with COOP/COEP headers. Chromium and Firefox each
run the full integration suite. Playwright WebKit runs only
`unsupported-browser.spec.ts`, because that Linux engine does not expose OPFS;
the test requires PMos to stop clearly before starting the kernel.

The most important full-stack workflows are:

- **boot-to-desktop.spec.ts**: starts the real desktop, requires the boot splash
  to disappear, and requires a physical launcher click to cause a presented
  menu-pixel change, all within `< 10 s`.
- **file-persistence.spec.ts**: opens the real Terminal, writes and durably
  flushes `/home/user/notes/hi.txt` through guest VFS operations, closes the
  page, creates a fresh kernel Worker in the same browser context, and reads
  the exact bytes back after the OPFS root remounts at `/`.
- **profile-privacy.spec.ts** and **abrupt-close.spec.ts**: prove real guest
  file isolation across browser storage partitions and remount consistency
  after the kernel Worker is forcibly terminated without lifecycle sync.
- **shell-pipeline.spec.ts**: opens Terminal, runs the shipped external
  `echo | grep` pipeline into `/dev/console`, and requires every pipeline stage
  to be reaped back to the steady Worker count.
- **offline-boot.spec.ts**: establishes and verifies the service-worker cache,
  closes the online page, stops the origin server, and requires a fresh offline
  page to boot in the strict `< 3 s` warm-load budget.
- **process-isolation.spec.ts**: launches the shipped `mem-adversary` from the
  real graphical Terminal as an ordinary isolated Worker/WASM instance and
  requires all eight escape probes to report `PASS`, with no breach, panic, or
  browser page error.
- **input-to-pixel-latency.spec.ts** and
  **desktop-taskbar-controls.spec.ts**: exercise real keystroke and pointer
  paths through the full stack and enforce the `< 100 ms` perceived-latency
  budget. These browser workflows, not the native 64×64 sink microbenchmark,
  are the authoritative latency evidence.
- **layering-test.spec.ts**: **the Principle II test**. Files imports alternate
  and default init configurations through the documented host-transfer path;
  Terminal installs the alternate config with the shipped `cp`; the current
  shell closes through its own display-protocol taskbar control; PID 1 rereads
  `/etc/init.conf` and respawns `/usr/bin/alt-shell`. Files and Terminal must
  retain their original processes, the alternate shell must recover the live
  task list, and the surviving Terminal must still accept input. The test then
  restores the default config through the same guest path.

**A change that breaks the layering test is rejected.** It is
the canonical acceptance test for the constitution.

### 4.6 Running everything

```shell
$ just test           # runs the complete release sequence
```

This is the command CI runs. In addition to the focused suites above, it gates
formatting, strict Clippy, the locked Rust workspace, strict TypeScript,
dependency audit, the browser suite, and the supplemental native perf harness.
It also runs the documented non-goal audit; that audit is intentionally
non-blocking in v1.

---

## 5. Worked example — writing a "hello" app with the toolkit

The checked-in [`sample-app`](../../crates/sample-app/src/main.rs) is the
executable reference for this path. The toolkit exports
`toolkit::wasi::FdConnection`, its production WASI adapter for the
`display_connect`, `fd_read`, `fd_write`, and `poll_oneoff` path. The complete
example below mirrors the sample's cfg-gated entry point and bounded event loop;
it is a runnable `main.rs`, not only an app-level fragment.

### 5.1 Create a new crate

```shell
$ cargo new --bin crates/hello-app
```

Edit `crates/hello-app/Cargo.toml`:

```toml
[package]
name = "hello-app"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "hello"
path = "src/main.rs"

[dependencies]
toolkit = { path = "../toolkit" }
```

Modern Cargo normally registers a crate created below an existing workspace in
the root `Cargo.toml` automatically. Inspect the members list and add
`"crates/hello-app"` only if it is absent; do not add a duplicate entry.

### 5.2 Write the app

Replace `crates/hello-app/src/main.rs` with:

```rust
#[cfg(target_arch = "wasm32")]
use toolkit::draw::{Color, Rect};
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
    window.set_title("Hello, PMos!")?;
    window.set_app_id("com.example.hello")?;
    window.commit()?;

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
                canvas.fill_rect(Rect::new(0, 0, width, height), Color::rgb(0x2a, 0x6c, 0x8a));
                canvas.draw_text(14, 24, "Hello, PMos!", Color::rgb(0xff, 0xff, 0xff));
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

`FdConnection::connect` performs the documented display-connect retry and
implements bounded, backpressure-aware transport. A buffer upload may require
multiple quanta, so the loop advances `progress_commit`, flushes outbound bytes
once per turn, continues locally only while staged commit work remains and the
transport has no suffix, and otherwise parks in `window.wait(None)`. Omitting
that progression can leave a large initial frame unpublished; omitting the
wait creates an idle busy loop.

### 5.3 Build and install

Add a source `pkg.toml` next to the app's `Cargo.toml`:

```toml
[package]
name = "hello"
version = "0.1.0"
display_name = "Hello"
author = "You"
summary = "A minimal hello-world app."

[exec]
binary = "bin/hello.wasm"

[capabilities]
required = ["DISPLAY_CLIENT"]
optional = []
```

Creating a new workspace member intentionally changes `Cargo.lock`, so the
first build must be unlocked. After that one update, retain `--locked` for
repeatable package and release commands:

```shell
$ cargo build --release --target wasm32-wasip1 -p hello-app
$ cargo run --locked -p xtask -- package hello-app
```

The packager writes `dist/pkgs/hello-0.1.0.pmpkg.tar`, generates the
`[integrity].sha256` table from the final payload bytes, and validates the
archive. Do not hand-edit generated hashes.

### 5.4 Run it

Start the already-assembled tree without rebuilding over the package you just
wrote:

```shell
$ cargo run --locked -p xtask -- dev-server --dir=dist --port=8080
```

Alternatively, start `just dev` in one terminal before running the app build and
package commands in a second terminal. Then open a terminal inside PMos and run:

```text
$ pkginstall /home/user/Downloads/hello-0.1.0.pmpkg.tar
pkginstall: installed hello
```

Drop the archive onto an open Files window to import it into the PMos
filesystem. `pkginstall` validates it before mutation and publishes both the
application directory and desktop entry transactionally, so an interrupted or
failed upgrade leaves the previous version launchable.

Open the launcher. "Hello" appears in the list. Click it. A
window with "Hello, PMos!" appears.

---

## 6. Worked example — speaking the display protocol without the toolkit

Principle VII and FR-020 are exercised by the standalone
[`toolkit-free-client`](../../crates/toolkit-free-client/) application. Its
production binary imports `display_connect` plus WASI `fd_read` / `fd_write`;
it does not link `toolkit` or call display-server APIs. `FreeSession` discovers
the registry's numeric names, binds the four shipped globals, creates a
keyboard, surface, collapsed-v1 `pmd_xdg_toplevel`, double-buffered pool, and
distinctive 320×200 frame, then handles configure/ack, input, release, and
close events.

Start with the native server-isolation and WASI compile gates:

```shell
$ cargo test --locked -p toolkit-free-client
$ cargo check --locked --target wasm32-wasip1 -p toolkit-free-client
```

Then assemble and serve PMos:

```shell
$ just build
$ just dev
```

Open Terminal inside PMos and run the registered production artifact:

```text
$ /bin/toolkit-free-client > /dev/console
```

A bounded teal window with a purple border and marker appears. Press a key
while it is focused; the same raw client receives `pmd_keyboard.key` and swaps
to its orange buffer. Close its focused taskbar entry; the shell request
becomes `pmd_xdg_toplevel.close`, the client destroys its role, surface,
buffers, and pool, and its isolated user Worker exits.

The focused production gate repeats that workflow in both supported automated
browsers and checks exact framebuffer colours and Worker lifetime:

```shell
$ cd web
$ npx --no-install playwright test tests/integration/toolkit-free-client.spec.ts \
    --project=chromium --project=firefox
```

The normative framing and opcode tables are in
[`contracts/display-protocol.md`](./contracts/display-protocol.md), especially
§§1, 6–11, 14, and 18. Shipped v1 deliberately uses
`pmd_xdg_shell.get_toplevel` plus merged toplevel configure/ack and bounded
inline pool writes. `pmd_xdg_wm_base` ping/pong, a separate xdg-surface object,
and fd-backed display pools are post-v1 protocol work, not hidden production
opcodes.

### 6.2 Why you would actually do this

Almost never. The architectural goal is that another language can implement the
wire contract without binding toolkit internals. The native state-machine gate
pinpoints protocol errors quickly; the separately isolated Chromium/Firefox
workflow proves the same bytes traverse the production kernel and display
server.

---

## 7. Packaging and installing a third-party app

See `contracts/package-manifest.md` for the full format. The
minimum:

1. Build your app to `wasm32-wasip1` → `bin/<name>.wasm`.
2. Write a source `pkg.toml` with `[package]`, `[exec]`, and
   `[capabilities]`.
3. Run `xtask package`; it generates the payload SHA-256 table and validates
   the resulting `<name>-<ver>.pmpkg.tar`.
4. Ship the `.pmpkg.tar` by any means you like — email, USB,
   download link on your static site.
5. The user drops the bundle into `/home/user/Downloads/` in
   their PMos instance, runs `pkginstall` from the terminal or
   right-clicks in the file manager, and the app appears in
   the launcher.

There is no registry or app store. Payload SHA-256 detects corruption, but the
archive is unsigned and therefore does not authenticate its author. V1 also
confines third-party required capabilities to `DISPLAY_CLIENT`; privileged
optional declarations are never granted.

---

## 8. Deploying to a static host

The entire v1 is a directory of static files. The only
deployment constraint is that the host must support COOP/COEP
headers.

### 8.1 Cloudflare Pages

Drop `dist/` into the project root. Commit. Push. Cloudflare
serves `dist/_headers`, which the build already wrote:

```
/*
  Cross-Origin-Opener-Policy: same-origin
  Cross-Origin-Embedder-Policy: require-corp
```

Verified release loaders: Chromium and Firefox. Safari is supported only when
the startup capability probe confirms the required OPFS and isolation APIs;
otherwise PMos displays the unsupported-browser screen before kernel boot.

### 8.2 Netlify

Identical to Cloudflare Pages — Netlify honours the same
`_headers` file format at the site root.

### 8.3 GitHub Pages

GitHub Pages does not let you set response headers directly.
The workaround is to put a Cloudflare Worker (free tier) in
front of the Pages site that adds the two headers on every
response. The repo's `docs/deploy-github-pages.md` gives the
Worker source.

### 8.4 S3 + CloudFront

Create the CloudFront distribution, attach a response-header
policy that sets `Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp`. The repo ships a
sample Terraform and a sample AWS CLI script in
`docs/deploy-s3-cloudfront.md`.

### 8.5 Verification

After deploying, open the site and check the devtools console.
Look for a line:

```
crossOriginIsolated === true
```

(The bootstrap prints this explicitly during boot.) If it is
`false`, the SAB transport cannot work and the system will fail
to boot with a clear message. Fix the headers before doing
anything else.

---

## 9. Troubleshooting

| Symptom                                      | Probable cause                                |
|----------------------------------------------|-----------------------------------------------|
| Blank page, `SharedArrayBuffer undefined`    | Missing COOP/COEP headers                     |
| Blank page, "OPFS unavailable"               | Private browsing mode or an unsupported browser |
| "Quota exceeded" during file save            | OPFS quota reached; clear data or free space |
| Layering test fails locally                  | Inspect PID 1's shell-path/respawn logs and verify the shell capabilities in `/etc/init.conf` |
| `crossOriginIsolated: false`                 | COOP/COEP headers are set but an asset has wrong CORP; see deploy guide |
| App launches, no window appears              | Display server not running; check `/proc/1` (init) and `/proc/*` for a process named `display-server` |
| "ENOTCAPABLE"                                | Your app's manifest doesn't declare a capability you're trying to use |

---

## 10. Where to go from here

- **`spec.md`** — the contract of behaviour for the v1 release.
- **`plan.md`** — the technical choices, grouped by layer.
- **`research.md`** — why each choice was made.
- **`data-model.md`** — kernel data structures and on-disk
  layout.
- **`contracts/syscalls.md`** — the authoritative OS ABI.
- **`contracts/display-protocol.md`** — the authoritative
  display server wire protocol.
- **`contracts/driver-kernel.md`** — driver ↔ kernel SAB layouts.
- **`contracts/package-manifest.md`** — the app bundle format.
- **`contracts/init-conf.md`** — the init configuration.
- **`.specify/memory/constitution.md`** — the non-negotiable
  principles. Read this first if you are proposing a
  substantial change.
