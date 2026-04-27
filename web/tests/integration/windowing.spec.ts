// T134 — Playwright integration test for windowed multitasking (US2).
//
// `#real-kernel` boots a four-pid demo tree where init spawns
// /bin/display-server + /bin/display-client-demo twice + hello-std.
// Two display-client-demos each ship a pixel payload; the server
// emits one `served client` line per successful client turn.
// Verify the server serves at least both clients (and ideally also
// the boot init for a third served-line) within the cold-start
// budget — proves multiple processes own framebuffer surfaces
// concurrently.

import { expect, test } from "@playwright/test";

test("two graphical clients concurrently reach the compositor", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => consoleLines.push(msg.text()));
  page.on("pageerror", (err) => consoleLines.push(`[pageerror] ${err.message}`));

  await page.goto("/index.html#real-kernel");

  await expect
    .poll(
      () =>
        consoleLines.filter((l) =>
          /display-server served client \d+/.test(l),
        ).length,
      { timeout: 15_000 },
    )
    .toBeGreaterThanOrEqual(2);
});
