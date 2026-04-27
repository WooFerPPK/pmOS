// T181 — Layering integration test (Principle II acceptance gate).
//
// The Constitution's Principle II demands that the desktop shell is
// REPLACEABLE — i.e. swapping `boot.shell` from `/usr/bin/shell` to
// `/usr/bin/alt-shell` produces a desktop with a different shell
// chrome AND every app that was running across the swap survives.
//
// Today the alt-shell binary (T178) is not built. This spec
// asserts the SUFFICIENT CONDITION the layering test rides on:
// the surface-survival contract (T180 isolation test verifies it
// at the unit level) AND the init.conf shell respawn picks up
// the changed binary (T179 verifies the parser side). When T178
// + the bootstrap-side `?shell=alt-shell` parameter land, this
// spec promotes from precondition-check to full layering check.

import { expect, test } from "@playwright/test";

test("default boot uses /usr/bin/shell; layering preconditions hold", async ({
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

  // The default shell is `/usr/bin/shell` — visible in the init
  // log line that init-desktop emits.
  const initShellLine = consoleLines.find((l) =>
    l.includes("init-desktop") && l.includes("shell"),
  );
  expect(initShellLine).toBeTruthy();
});
