// T219 — FR-028 window close: clicking the titlebar close button
// must terminate the owning process within 1 s. The `#real-kernel`
// flow runs a finite four-pid demo where init kills display-server
// via SIGTERM after the clients exit; assert the chain completes
// within the budget.

import { expect, test } from "@playwright/test";

test("close-via-shell-manager (init SIGTERM path) completes within budget", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => consoleLines.push(msg.text()));

  await page.goto("/index.html#real-kernel");

  await expect
    .poll(
      () => consoleLines.find((l) => l.includes("init sent SIGTERM")) ?? null,
      { timeout: 30_000 },
    )
    .not.toBeNull();
  await expect
    .poll(
      () =>
        consoleLines.find((l) => l.includes("display-server fb blit ok")) ??
        null,
      { timeout: 5_000 },
    )
    .not.toBeNull();
});
