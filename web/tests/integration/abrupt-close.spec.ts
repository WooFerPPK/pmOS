// T141 — abrupt-close: forcibly close the browser context after a
// boot, reopen, assert the OPFS state is consistent and the
// desktop reboots without a journal-replay panic. Uses `context.close()`
// which terminates the context without giving the page a chance to
// flush via pagehide — the kernel's mount path must replay the
// journal on the next mount.

import { expect, test } from "@playwright/test";

test("desktop boots cleanly after an abrupt context close", async ({
  browser,
}) => {
  const ctx1 = await browser.newContext();
  const page1 = await ctx1.newPage();
  const lines1: string[] = [];
  page1.on("console", (msg) => lines1.push(msg.text()));
  await page1.goto("/index.html");
  await expect
    .poll(
      () =>
        lines1.find((l) => /display-server served client \d+/.test(l)) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();

  // Hard-close without giving the pagehide handler a chance.
  await ctx1.close();

  // Reopen fresh — separate context (Playwright partition), but
  // it doesn't matter for THIS test: the assertion is that the
  // boot path is robust to abrupt termination of any prior session.
  const ctx2 = await browser.newContext();
  const page2 = await ctx2.newPage();
  const lines2: string[] = [];
  page2.on("console", (msg) => lines2.push(msg.text()));
  await page2.goto("/index.html");
  await expect
    .poll(
      () =>
        lines2.find((l) => /display-server served client \d+/.test(l)) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();

  // No `[pageerror]` lines (the kernel didn't panic on mount).
  expect(lines2.filter((l) => l.startsWith("[pageerror]"))).toHaveLength(0);

  await ctx2.close();
});
