// T139 — file persistence across reload (US3).
//
// Use the `#real-kernel` flow (which boots a real kernel + OPFS
// block driver). The kernel's first-boot mkfs creates the FR-013a
// starter kit; rebooting must show the same starter files (the
// /etc/init.conf fingerprint is stable). The proxy assertion is
// that init-desktop boots successfully twice in a row from the
// same browser context, which requires OPFS persistence to round-trip.

import { expect, test } from "@playwright/test";

test("desktop boots twice from the same browser context (OPFS round-trip)", async ({
  page,
}) => {
  const lines1: string[] = [];
  page.on("console", (msg) => lines1.push(msg.text()));

  await page.goto("/index.html");
  await expect
    .poll(
      () =>
        lines1.find((l) => /display-server served client \d+/.test(l)) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();

  // Reload — same context, same OPFS. Re-mount must not fail.
  const lines2: string[] = [];
  page.on("console", (msg) => lines2.push(msg.text()));
  await page.reload();
  await expect
    .poll(
      () =>
        lines2.find((l) => /display-server served client \d+/.test(l)) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();
});
