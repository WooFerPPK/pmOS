// T161 — files + edit GUI integration. T151 (files) and T157
// (edit) currently ship as 10-line stubs. This spec validates the
// pieces that DO ship: the kernel, the desktop, and the boot path
// reach the served-client signal — i.e. the desktop is up and a
// future GUI files/edit slice would land into a working
// compositor.

import { expect, test } from "@playwright/test";

test("desktop is ready for files + edit (boot path green)", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => consoleLines.push(msg.text()));

  await page.goto("/index.html");
  await expect
    .poll(
      () =>
        consoleLines.find((l) =>
          /display-server served client \d+/.test(l),
        ) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();
});
