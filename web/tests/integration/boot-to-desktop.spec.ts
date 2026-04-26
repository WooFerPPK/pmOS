// T127/T128 — boot-to-desktop Playwright integration test.
//
// Boots PMos via the `#boot-to-desktop` URL hash (which
// makes `bootstrap.ts` spawn `/bin/init-desktop` as the
// initial process instead of the legacy demo flow's
// `/bin/init`). init-desktop spawns the real binaries:
//
//   * `/bin/display-server` — claims `/run/display`, opens
//     `/dev/fb0` + `/dev/input/{mouse,kbd}`, and enters its
//     accept-then-input-drain loop.
//   * `/bin/shell` — calls `pmos_ext.display_connect()` to
//     open `/run/display`, wraps the returned fd in
//     `FdConnection`, allocates a `Taskbar`, and runs the
//     paint loop in `shell::run_shell_with_taskbar`.
//
// The spec asserts:
//
//   1. init-desktop reaches its supervision loop (so it
//      successfully spawned both children).
//   2. display-server starts (the binary's startup banner).
//   3. SC-001: a desktop-rendered observable arrives within
//      10 s of the `goto`. The "desktop rendered" proxy is
//      the display-server emitting its first `served client`
//      log line for the shell — that means the shell
//      connected, sent the bind requests, and the server
//      processed them. Since the shell's run_shell loop
//      paints the wallpaper on the first configure event,
//      seeing the served-client line means the wallpaper
//      paint has reached the framebuffer.
//
// Performance budget (T128): the elapsed wall-clock from
// `page.goto` to the served-client line is the cold-load
// time per Constitution Principle IX. The spec uses the
// 10 s timeout as the SC-001 threshold; the timeout firing
// is the failure mode.

import { expect, test } from "@playwright/test";

test("boot-to-desktop: init-desktop spawns display-server + shell, shell paints wallpaper within 10s", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => {
    consoleLines.push(msg.text());
  });
  page.on("pageerror", (err) => {
    consoleLines.push(`[pageerror] ${err.message}`);
  });

  const t0 = Date.now();
  // The bare URL now boots /bin/init-desktop (wallpaper + shell)
  // by default; the `#boot-to-desktop` hash is an explicit alias
  // for the same path. Target the bare URL so the spec doubles as
  // a smoke test that the default boot path works.
  await page.goto("/index.html");

  // Phase 1: init-desktop announces and spawns the two
  // children. Cold-start CI gets a generous timeout because
  // the kernel + init wasm have to fetch + instantiate
  // before the first println lands.
  const initStartingLine = () =>
    consoleLines.find((l) => l.includes("init-desktop starting")) ?? null;
  await expect
    .poll(initStartingLine, { timeout: 10_000 })
    .not.toBeNull();

  const initSupervisionLine = () =>
    consoleLines.find((l) => l.includes("init-desktop entering supervision loop")) ??
    null;
  await expect
    .poll(initSupervisionLine, { timeout: 10_000 })
    .not.toBeNull();

  // Phase 2: display-server reaches its accept loop. Once
  // this line lands the binary has bound /run/display +
  // opened /dev/fb0; the shell's display_connect call can
  // succeed.
  const displayServerStartingLine = () =>
    consoleLines.find((l) => l.includes("display-server starting")) ?? null;
  await expect
    .poll(displayServerStartingLine, { timeout: 10_000 })
    .not.toBeNull();

  // Phase 3 (SC-001 + T128 cold-load budget): the shell
  // connected, the server dispatched its registry walk and
  // bind sequence, and the framebuffer received its first
  // composed pixel write. The display-server prints a
  // `served client {i}` line on every successful client
  // turn; the very first one is the proof that the shell's
  // `run_shell_with_taskbar` reached its paint pass.
  const servedClientLine = () =>
    consoleLines.find((l) =>
      /display-server served client \d+/.test(l),
    ) ?? null;
  await expect
    .poll(servedClientLine, { timeout: 10_000 })
    .not.toBeNull();

  const elapsed_ms = Date.now() - t0;
  // The 10s timeout is itself the SC-001 / T128 gate: if
  // the served-client line fails to arrive within 10s the
  // expect.poll above fails the test. This logs the actual
  // wall-clock for the visible record.
  console.log(`[boot-to-desktop] elapsed_ms=${elapsed_ms}`);
  expect(elapsed_ms).toBeLessThan(10_000);
});
