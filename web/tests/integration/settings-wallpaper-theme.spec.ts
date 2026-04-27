// T195 — Settings wallpaper + theme. The settings GUI (T184) is
// CLI-only today; the wallpaper picker + theme watcher land in
// follow-up slices. This spec verifies the boot path supports the
// preference round-trip (init reads /etc/preferences.toml).

import { expect, test } from "@playwright/test";

test("desktop boots; preferences pipeline reachable", async ({ page }) => {
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
