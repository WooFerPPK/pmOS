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

---

## 1. Prerequisites

Install the following on your development machine. Version numbers
are floors; newer is fine.

- **Rust** (via rustup), stable channel, latest.
- **Rust targets**: `rustup target add wasm32-unknown-unknown
  wasm32-wasi`
- **Node.js** 20.x (for esbuild, Vitest, Playwright).
- **just** (`cargo install just`) for the build orchestrator.
- A modern browser: Chromium, Firefox, or Safari recent release.
- **tar** (any POSIX tar), for the app package format.

Optional but recommended:

- `wasm-tools` (`cargo install wasm-tools`) for inspecting WASM
  binaries during debugging.

---

## 2. Building from source

From the repository root:

```shell
$ just build
```

Under the hood this runs:

```shell
$ cargo build --release --target wasm32-unknown-unknown -p kernel
$ cargo build --release --target wasm32-wasi -p init -p display-server -p toolkit -p sh -p term -p files -p edit -p settings -p sysmon -p sample-app -p toolkit-free-client
$ cd web && npx esbuild src/bootstrap.ts --bundle --outfile=../build/assets/bootstrap.js --format=esm
$ cd web && npx esbuild src/sw.ts --bundle --outfile=../build/sw.js --format=esm
$ cargo run -p xtask -- assemble-dist
```

The `xtask assemble-dist` step is where the static `dist/`
directory is built: WASM binaries are copied to `dist/assets/`,
the HTML is copied to `dist/index.html`, a `_headers` file with
the required COOP/COEP headers is written to `dist/_headers`, and
SRI hashes for each asset are written alongside the HTML.

Result: `dist/` is a directory of static files ready to serve.

```shell
$ just clean   # removes build/ and dist/
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

Four layers of tests, each in its own `just` target.

### 4.1 Kernel isolation tests

```shell
$ just test-kernel
```

Runs `cargo test -p kernel --target x86_64-unknown-linux-gnu`
(or whichever native target you are on). The kernel uses a
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

Runs `cd web && npx vitest run`. Drivers are tested with a mock
kernel ring buffer. Tests cover framebuffer submit + present,
input event normalization, OPFS block I/O (using a jsdom stub
OPFS), net driver fetch stub, and console driver.

### 4.5 Integration tests (Playwright)

```shell
$ just test-integration
```

Runs `cd web && npx playwright test`. Launches headless
Chromium, navigates to a local dev server (started by the test
runner), and executes a battery of integration tests. The most
important ones:

- **boot-to-desktop.spec.ts**: asserts the desktop appears
  within the cold-load budget.
- **file-persistence.spec.ts**: creates a file, reloads, reads
  it back (User Story 3).
- **shell-pipeline.spec.ts**: opens a terminal, runs
  `ls | grep foo > out.txt`, verifies `out.txt` content (User
  Story 4).
- **offline-boot.spec.ts**: loads once online, disables the
  network, reloads, asserts the desktop still appears (User
  Story 6).
- **process-isolation.spec.ts**: runs an adversarial test
  program that attempts to read foreign process memory through
  every non-IPC channel; asserts every attempt fails.
- **layering-test.spec.ts**: **the Principle II test**. Opens
  terminal, file manager, and edit; kills the desktop shell
  from sysmon; asserts the three apps' windows are still on
  screen; launches a replacement shell binary; asserts the new
  shell's taskbar shows the three apps; clicks a taskbar entry
  and asserts the corresponding window raises.

**A change that breaks the layering test is rejected.** It is
the canonical acceptance test for the constitution.

### 4.6 Running everything

```shell
$ just test           # runs all of 4.1 .. 4.5 in sequence
```

This is the command CI runs.

---

## 5. Worked example — writing a "hello" app with the toolkit

This is the happy path an app author should follow.

### 5.1 Create a new crate

```shell
$ cargo new --lib crates/hello-app
```

Edit `crates/hello-app/Cargo.toml`:

```toml
[package]
name = "hello-app"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[[bin]]
name = "hello"
path = "src/main.rs"

[dependencies]
toolkit = { path = "../toolkit" }
```

Add the crate to the workspace `Cargo.toml` members list.

### 5.2 Write the app

`crates/hello-app/src/main.rs`:

```rust
use toolkit::{App, Window, Label, WindowRole};

fn main() {
    let app = App::connect().expect("display connect");
    let mut win = app.new_window(WindowRole::Toplevel {
        title: "Hello".into(),
        app_id: "com.example.hello".into(),
        size: (320, 200),
    });

    let label = Label::new("Hello, PMos!");
    win.set_root(label);

    app.run(win);   // runs until the user closes the window
}
```

### 5.3 Build and install

```shell
$ cargo build --release --target wasm32-wasi -p hello-app
$ mkdir -p build/hello-pkg/bin
$ cp target/wasm32-wasi/release/hello.wasm build/hello-pkg/bin/
$ cat > build/hello-pkg/manifest.toml <<'EOF'
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
EOF
$ (cd build/hello-pkg && tar -cf ../hello-0.1.0.pmpkg.tar .)
```

### 5.4 Run it

Start PMos (`just dev`), open a terminal inside PMos, and run:

```
$ cd /home/user
$ cat << 'EOF' > /home/user/install-hello.sh
#!/usr/bin/sh
mkdir -p /opt/hello
tar -xf /home/user/hello-0.1.0.pmpkg.tar -C /opt/hello
pkginstall-desktop-entry /opt/hello/manifest.toml
EOF
$ sh /home/user/install-hello.sh
```

(In practice you'd copy `hello-0.1.0.pmpkg.tar` into the PMos
filesystem via the host-side build script; a `just push-sample`
target does this.)

Open the launcher. "Hello" appears in the list. Click it. A
window with "Hello, PMos!" appears.

---

## 6. Worked example — writing the same app WITHOUT the toolkit

This exercises the "protocol is the source of truth" guarantee
(Principle VII and FR-020). A hand-written client that speaks the
wire protocol directly, with no toolkit linked in, MUST be able to
create a window, draw, and receive input.

### 6.1 Minimal client skeleton

`crates/hello-raw/src/main.rs`:

```rust
use std::os::wasi::io::FromRawFd;
use std::io::{Read, Write};

mod abi {
    // Imports the PMos syscall extensions. These are the entries
    // defined in contracts/syscalls.md; the `abi` crate wraps them
    // in safe Rust functions.
    extern "C" {
        pub fn display_connect() -> i32;   // syscall 0x1200
        // ... ipc_send, ipc_recv wrappers are in the abi crate
    }
}

mod proto {
    // Hand-written message builders for the display protocol,
    // mirroring contracts/display-protocol.md exactly.
    pub fn header(object: u32, opcode: u16, len: u16, fd_count: u8) -> [u8; 8] {
        let mut h = [0u8; 8];
        h[0..4].copy_from_slice(&object.to_le_bytes());
        h[4..6].copy_from_slice(&opcode.to_le_bytes());
        h[6..8].copy_from_slice(&len.to_le_bytes());
        // fd_count lives in byte 6..7 — see contract; abbreviated here
        h
    }
    // ... build_get_registry, build_bind, build_create_surface,
    //     build_create_pool, build_create_buffer, build_attach,
    //     build_commit, parse_event, etc.
}

fn main() {
    let fd = unsafe { abi::display_connect() };
    assert!(fd >= 0);

    // 1. get_registry on display (object 1)
    write_msg(fd, proto::build_get_registry(0x3));   // new id 3 for registry

    // 2. read `global` events from registry, bind pmd_compositor,
    //    pmd_shm, pmd_xdg_wm_base.
    let (compositor, shm, xdg_wm_base) = bind_globals(fd);

    // 3. allocate a SAB shm pool (via a PMos-specific helper that
    //    gets a shared buffer fd from the kernel) and pass it as a pool.
    let pool_fd = alloc_sab_fd(320 * 200 * 4);
    send_create_pool(fd, shm, /*new_id*/ 0x10, pool_fd, 320 * 200 * 4);

    // 4. create a surface + xdg_surface + xdg_toplevel with title "Hello".
    send_create_surface(fd, compositor, 0x20);
    send_get_xdg_surface(fd, xdg_wm_base, /*new_id*/ 0x30, /*surface*/ 0x20);
    send_get_toplevel(fd, /*xdg_surface*/ 0x30, /*new_id*/ 0x40);
    send_set_title(fd, /*xdg_toplevel*/ 0x40, "Hello");
    send_set_app_id(fd, 0x40, "com.example.hello-raw");

    // 5. create a buffer from the pool, fill it with ARGB solid colour
    send_create_buffer(fd, /*pool*/ 0x10, /*new_id*/ 0x50, 0, 320, 200, 320*4, /*ARGB8888*/ 0);
    fill_pool_solid(0xffff_8040);      // orange
    send_attach(fd, /*surface*/ 0x20, /*buffer*/ 0x50);
    send_damage(fd, 0x20, 0, 0, 320, 200);
    send_commit(fd, 0x20);

    // 6. event loop: read events, handle xdg_toplevel.close, redraw on frame callbacks.
    event_loop(fd);
}
```

This is ~200 lines of hand-written code and it is a real v1
client. It does not link `toolkit` and does not use its types.
The integration test
(`tests/integration/toolkit-free-client.spec.ts`) runs this
binary under Playwright and asserts that a window appears on the
compositor surface.

### 6.2 Why you would actually do this

Almost never. The point is not that end-users write apps like
this, but that the protocol *admits* such an app. If the toolkit
were the source of truth, adding a new language (say, a C or Zig
toolkit) would require either reimplementing the toolkit or
binding against its internals. Because the protocol is the
source of truth, another language writes its own toolkit from
scratch against the same wire format and interoperates with
everything else. This is why Principle VII is phrased the way it
is.

---

## 7. Packaging and installing a third-party app

See `contracts/package-manifest.md` for the full format. The
minimum:

1. Build your app to `wasm32-wasi` → `bin/<name>.wasm`.
2. Write a `manifest.toml` with `[package]`, `[exec]`,
   `[capabilities]`.
3. `tar -cf <name>-<ver>.pmpkg.tar manifest.toml bin/`
4. Ship the `.pmpkg.tar` by any means you like — email, USB,
   download link on your static site.
5. The user drops the bundle into `/home/user/Downloads/` in
   their PMos instance, runs `pkginstall` from the terminal or
   right-clicks in the file manager, and the app appears in
   the launcher.

No signing, no registry, no app store. This is deliberate.

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

Verified loaders: Chromium, Firefox, Safari.

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
| Layering test fails locally                  | Check that sysmon was granted `PROC_KILL_ANY` and `PROC_ENUMERATE` in `/etc/init.conf` |
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
