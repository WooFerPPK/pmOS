# Debugging PMos

## Overview

PMos has a textbook OS structure — kernel, drivers, display server,
toolkit, userland apps — but the whole stack runs inside a browser
tab. Debugging is a matter of picking the right layer and using the
tool that exists at that layer: a native `cargo test` for the
kernel, a Vitest isolation test for a driver, a Playwright session
for the full stack, and the browser DevTools for anything that
requires a running tab. The authoritative layer catalogue lives in
[`.specify/memory/constitution.md`](../.specify/memory/constitution.md)
(Principles I, II, VIII); this document is the operational companion
— what to reach for when something is broken.

## Kernel-panic recovery

**What the user sees.** When the kernel hits a `panic!`, the main
thread mounts a full-page red overlay titled "PMos kernel panic"
with the panic message and a 5-second countdown to auto-reload.
This is FR-009a. The overlay wiring landed end-to-end in commit
`859d527` (kernel panic hook → `ConsoleHost` lifecycle message →
`bootstrap.ts` overlay). The Vitest isolation coverage for the
overlay DOM contract (panel visibility, message text, countdown
5→0, reload after 5000 ms, empty-message handling) lives at
`web/tests/unit/panic-overlay.test.ts` and landed in T094
(commit `786f4aa`).

**What the developer does.** If the panic was caused by a bug in
your app's syscall usage, the panic message surfaces inside the
overlay and should contain enough context to diagnose it directly.
If the panic was in the kernel itself, open the browser DevTools
console *before* triggering the failure — the kernel's
`#[panic_handler]` (see `crates/kernel/src/lib.rs`) routes
`PanicInfo` through `Platform::on_panic`, and the browser
`Platform` impl logs a stack trace via `postMessage` to the main
thread before the overlay mounts. Auto-reload does **not** clear
OPFS state, so your root filesystem survives the reset. If you
actually want to wipe storage and reboot clean, use
**DevTools → Application → Storage → "Clear site data"** — that
clears OPFS, `localStorage`, service-worker cache, and everything
else the origin holds.

## The `/proc` filesystem

`/proc` is a read-only in-memory VFS surface populated by the
kernel from its in-memory process + device tables. The backing
code lives at `crates/kernel/src/fs/procfs.rs`. Three files are
interesting:

- `/proc/version` — kernel version string (the first `/proc`
  entry; landed with the initial procfs skeleton).
- `/proc/<pid>/status` — per-process info: `Name`, `State`, `Pid`,
  `PPid`, `VmSize`, `VmPeak`. **T168 is pending**: the directory
  entry exists but population of each field depends on T168
  landing. Before relying on a specific field, `cat` the file and
  check what actually comes back.
- `/proc/storage` — block-driver quota + used counters. **T169 is
  pending**; treat reads as best-effort until it lands.

Reading from an app is ordinary WASI `std::fs`:

```rust
let status = std::fs::read_to_string("/proc/self/status")?;
eprintln!("{status}");
```

For a GUI view of `/proc`, the bundled `sysmon` app
(`crates/sysmon/src/main.rs`, **T170 pending**) is the intended
destination — a toolkit window with a live-refreshing process list
and a Terminate button. Until T170 lands, the CLI path is
`crates/settings/src/main.rs` (T184 partial, landed in `1b22510`),
which reads `/etc/preferences.toml` via the `preferences` crate
and prints a six-line snapshot — useful as a quick debug sanity
check that TOML parsing and the VFS path both work.

## `/dev/console`

`/dev/console` is the serial-line debug channel. The kernel side
lives at `crates/kernel/src/dev/mod.rs`: a bounded input ring
(`inject_console_input` pushes, `read(/dev/console)` pops) and a
line-buffered output sink flushed to the platform via
`driver_call(Console, DEV_CONSOLE, bytes)`. The TS side lives at
`web/src/drivers/console.ts` and routes kernel output to a
browser-side sink — a hidden textarea in the demo host today, a
full terminal emulator in userland once `/usr/bin/term` (**T124
pending**) lands.

Apps write diagnostics via `stderr`. In the current driver
wiring, wasip1 `stderr` is plumbed to `/dev/console`:

```rust
eprintln!("hello from userland");
```

or, equivalently:

```rust
use std::io::Write;
writeln!(std::io::stderr(), "hello from userland")?;
```

This is the `printf`-style way to get bytes out of a userland
process without any graphical dependency. The driver-side type
vocabulary (`Driver`, `DriverHost`, `DriverResult`, `DriverErrorCode`,
the `ToDriver` / `FromDriver` message envelopes) is re-exported
from `web/src/drivers/common.ts` (T079, commit `b0baf01`) — that
is the module downstream TS code should import when writing driver
tests.

## Running tests

Every layer has a dedicated isolation test target. They run native
(no browser) except the last:

```shell
just test-kernel           # cargo test for the kernel (no display server, no browser)
just test-display-server   # cargo test with mock client + mock framebuffer
just test-toolkit          # cargo test against a mock display server
just test-drivers          # Vitest + mock kernel for TS drivers
just test-integration      # Playwright, full-stack
just test                  # every layer above, in order
```

`just test-integration` includes the **layering test** (T181
**pending**), which is the Principle II acceptance gate — a full
boot with the stock shell swapped for an alternative, asserting
that no layer below the shell changed. Until T181 lands, integration
coverage is the real-kernel boot path in
`web/tests/integration/real-kernel.spec.ts`.

Every layer's isolation tests **must** pass before integration
tests run. A broken isolation test is a build-break — see
constitution §X ("Testability at every layer; isolation tests
before integration").

## The Playwright harness

Playwright tests live under `web/tests/integration/*.spec.ts` and
drive a real Chromium against a locally-served `dist/`.

Start the dev server in one terminal (serves `dist/` on port 8080
with COOP/COEP headers — see
[`docs/deploy-github-pages.md`](deploy-github-pages.md) for why
those headers are load-bearing):

```shell
just dev
```

Run a single spec file:

```shell
(cd web && npx playwright test real-kernel.spec.ts)
```

Drop into the interactive Playwright Inspector by adding a pause
point inside your test:

```ts
test("my broken thing", async ({ page }) => {
  await page.goto("http://localhost:8080/");
  await page.pause();        // hands off to the Inspector
});
```

Run with step-wise debugging by exporting `PWDEBUG=1`:

```shell
(cd web && PWDEBUG=1 npx playwright test real-kernel.spec.ts)
```

Synthetic input events are the standard way to drive a running OS
from a test. The Rust-side pattern lives in
`crates/display-server/tests/server.rs` — see `encode_request_bytes`
+ the `registry_bind_payload` helper for how a test crafts a framed
wire message and hands it to `Server::handle_request`. The
TS-side synthetic-input pattern will ship alongside the full boot
integration slice; until `web/tests/integration/` grows beyond
`real-kernel.spec.ts`, reach for the kernel-side
`inject_console_input` hook (see `crates/kernel/src/dev/mod.rs`) to
drive `/dev/console` from a host-driven test harness.

Full-stack boot-to-desktop integration is blocked on **T126 +
T127** (both **pending**): T126 wires `just dev` to assemble every
P1 binary into `dist/`, and T127 is the `boot-to-desktop.spec.ts`
Playwright test asserting SC-001 (desktop visible under 10 s,
terminal spawns, `echo hello` round-trips). Until those land,
Playwright runs against a partial stack.

## Inspecting the running OS from the browser

DevTools gives you a cheap set of oscilloscopes on a running
PMos tab. Five panels you will reach for regularly:

- **Console.** The kernel logs via `console.log` / `console.error`
  through the console driver path. Anything the kernel panic
  handler emits shows here before the overlay mounts.
- **Network.** Should show **zero** requests after the first
  load. If you see any after boot, you have violated Principle III
  (browser-only, zero backend) — check what just ran and work
  back to the dependency that reached out.
- **Application → Storage.** Expand the **Origin Private File
  System** tree to walk the PMos root filesystem. This is the
  fastest way to verify that a write actually hit disk and to
  clear state when a test run gets sticky ("Clear site data"
  also lives here).
- **Performance.** The Principle IX triad — cold-load (< 10 s),
  warm-load (< 3 s), input-to-pixel (< 100 ms) — shows up as
  timeline clusters here. The native-Rust backstop for the
  input-path budget is
  `crates/integration-tests/src/bin/input-latency.rs` (T220
  partial, landed `1432f1b`); it is a synthetic proxy that omits
  the browser hops, so a DevTools Performance trace is how you
  confirm the real path still fits the budget.
- **Sources → Workers.** The kernel Worker and each user-process
  Worker are listed separately. Breakpoints work; pause the
  kernel Worker at a syscall dispatch and you can single-step
  through the ring-buffer handler.

## Common failure modes

### `crossOriginIsolated` is false

PMos refuses to boot because the deployment origin is missing
`Cross-Origin-Opener-Policy: same-origin` and/or
`Cross-Origin-Embedder-Policy: require-corp`. Without both
headers, `SharedArrayBuffer` and `Atomics.wait` are gated off,
which means no kernel↔app syscall transport. Fix is a deploy-side
configuration change — see
[`docs/deploy-github-pages.md`](deploy-github-pages.md) for the
Cloudflare-Worker pre-pend recipe or
[`docs/deploy-s3-cloudfront.md`](deploy-s3-cloudfront.md) for the
CloudFront Response Headers Policy. Verify with:

```js
console.log(window.crossOriginIsolated)  // must print: true
```

### Kernel Worker never responds

The kernel Worker is running but your app is parked forever on a
syscall. Two checks:

1. Open the DevTools console and look for a panic — the overlay
   only mounts for an in-kernel panic; a kernel-Worker
   *exception* can surface only as a console error.
2. Confirm that your user WASM's `_start` eventually calls
   `__wasi_proc_exit(0)`. Rust `std` binaries sometimes return
   from `main` without an explicit exit, which leaves
   `wasm._start()` returning normally rather than throwing the
   proc-exit sentinel. The TS user-wasm runtime handles this
   returns-normally path as of the T095 / T110 slice — see
   `web/src/user-wasm-runtime.ts` (doc comment on `_start`
   unwinding via the `ProcExit` sentinel). If you see a parent
   parked forever in `proc_wait`, inspect that file first.

### My file disappears on reload

You are in a private / incognito window, OR browser-local storage
has been denied for the origin. PMos' persistence is OPFS-backed,
and OPFS is refused in private browsing. Open a non-private window
and confirm with **DevTools → Application → Storage** that the OPFS
tree exists. Cross-link:
[`.specify/memory/constitution.md`](../.specify/memory/constitution.md)
Principle IV (Offline-First And Persistent).

## Where to file bugs

- **Constitution-relevant issues** — Principle I–X violations,
  layering drift, performance-budget regressions — go through the
  plan's Known Deviations section at
  `specs/001-browser-os-v1/plan.md` and the tasks.md slice process.
  They are not implementation bugs; they are spec-level deltas.
- **Implementation bugs** — a crash, a wrong result, a hanging
  syscall — go in the repo issue tracker with a minimal
  reproduction (ideally a failing test at the appropriate layer).
- **Spec ambiguities** — a section of
  `specs/001-browser-os-v1/spec.md` that reads two ways, a
  contract that under-specifies an edge case — go through
  `/speckit.analyze` and surface in
  `specs/001-browser-os-v1/analyze-findings.md`.

## Further reading

- [`docs/apps.md`](apps.md) — developing PMos apps along the
  toolkit path and the raw-protocol path.
- [`docs/deploy-github-pages.md`](deploy-github-pages.md) —
  hosting on GitHub Pages via a Cloudflare Worker front.
- [`docs/deploy-s3-cloudfront.md`](deploy-s3-cloudfront.md) —
  hosting on S3 + CloudFront with a Response Headers Policy.
- [`specs/001-browser-os-v1/quickstart.md`](../specs/001-browser-os-v1/quickstart.md)
  — prerequisites, build, run, test from a fresh checkout.
- [`.specify/memory/constitution.md`](../.specify/memory/constitution.md)
  — the ten principles; this document's layer catalogue and
  testing gates come from Principles II, VIII, IX, X.
</content>
</invoke>