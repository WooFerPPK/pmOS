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
//   3. SC-001: the boot splash disappears and a launcher click
//      produces a causally presented menu pixel within 10 s.
//
// Performance budget (T128): the elapsed wall-clock starts
// before `page.goto` and ends only after that interactive
// desktop response. The strict 10 s SC-001 threshold includes
// navigation, boot, first paint, input routing, and menu paint.

import { expect, test } from "@playwright/test";
import {
  launchTerminal,
  runEchoHelloAndWaitForOutput,
} from "./guest-terminal";
import { openLauncherBefore } from "./launcher-interaction";

test.use({ viewport: { width: 1280, height: 900 } });

test("boot-to-desktop: cold boot reaches an interactive desktop within 10s", async ({
  browserName,
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
  const deadline = t0 + 10_000;
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

  // Phase 3 (SC-001 + T128 cold-load budget): prove both visible boot and
  // interactivity. The launcher helper accepts only a menu-pixel change on a
  // framebuffer presentation causally following the physical click.
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: Math.max(1, deadline - Date.now()),
  });
  await openLauncherBefore(page, deadline);

  const elapsed_ms = Date.now() - t0;
  // Preserve this marker for the canonical performance artifact.
  console.log(`[boot-to-desktop] elapsed_ms=${elapsed_ms} engine=${browserName}`);
  expect(elapsed_ms).toBeLessThan(10_000);

  // T127 continues beyond the performance endpoint: use the launcher that the
  // timed interaction left open, establish real Terminal keyboard focus, then
  // require literal shell stdout to appear in the guest framebuffer.
  await launchTerminal(page, consoleLines, { launcherAlreadyOpen: true });
  await runEchoHelloAndWaitForOutput(page);
  expect(consoleLines.filter((line) => line.startsWith("[pageerror]"))).toEqual(
    [],
  );
});
