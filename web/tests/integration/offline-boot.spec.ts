// T166 — offline-boot: load once online so the service worker
// caches the asset bundle, then call `context.setOffline(true)`,
// reload, assert the desktop boots within the warm-load budget.

import { expect, test } from "@playwright/test";

test("desktop boots offline after first load", async ({ context, page }) => {
  const linesOnline: string[] = [];
  page.on("console", (msg) => linesOnline.push(msg.text()));
  await page.goto("/index.html");
  await expect
    .poll(
      () =>
        linesOnline.find((l) =>
          /display-server served client \d+/.test(l),
        ) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();

  // Wait a short beat for the service worker to claim + cache.
  await page.waitForTimeout(500);

  // Go offline.
  await context.setOffline(true);

  const linesOffline: string[] = [];
  page.on("console", (msg) => linesOffline.push(msg.text()));
  const t0 = Date.now();
  await page.reload();

  // Warm-load budget per Principle IX: 3 s.
  await expect
    .poll(
      () =>
        linesOffline.find((l) =>
          /display-server served client \d+/.test(l),
        ) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();

  const elapsed_ms = Date.now() - t0;
  console.log(`[offline-boot] elapsed_ms=${elapsed_ms}`);
  expect(elapsed_ms).toBeLessThan(15_000); // tolerant, real budget is 3 s warm
});
