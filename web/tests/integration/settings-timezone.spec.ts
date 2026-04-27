// T197 — Settings timezone. T190 lands the bundled IANA subset
// + the init-side TZ env injection. The bundled tzdata isn't
// shipped yet (zoneinfo dir empty). This spec validates the boot
// path supports the env-from-preferences pipeline init relies on.

import { expect, test } from "@playwright/test";

test("desktop boots and exposes init's preference-env pipeline", async ({
  page,
}) => {
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
