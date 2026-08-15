// Browser-substrate gate for engines that do not expose OPFS at all.
// PMos must stop before starting the kernel instead of showing a usable-looking
// volatile desktop that would lose the user's files on reload.

import { expect, test } from "@playwright/test";

test("missing persistent-storage substrate produces an explicit unsupported screen", async ({
  page,
}) => {
  const lines: string[] = [];
  page.on("console", (message) => lines.push(message.text()));

  await page.goto("/index.html");

  const unsupported = page.locator("#pmos-unsupported-browser");
  await expect(unsupported).toBeVisible();
  await expect(unsupported).toContainText("PMos cannot start in this browser");
  await expect(unsupported).toContainText("Origin Private File System (OPFS)");
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0);
  expect(lines.some((line) => line.includes("unsupported browser"))).toBe(true);
  expect(lines.some((line) => line.includes("kernel-worker"))).toBe(false);
});
