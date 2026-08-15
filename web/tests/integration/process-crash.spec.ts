// T176 — FR-009 end-to-end evidence: an app that panics is reaped
// by the kernel without taking down its peers. The `#real-kernel`
// boot path runs init → display-server + display-client-demo × 2
// + hello-std; init's reap loop drains every Zombie via proc_wait
// and prints `init reaped child pid=N` per child.

import { expect, test } from "@playwright/test";

test("kernel reaps every child cleanly without panicking peers", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => consoleLines.push(msg.text()));
  page.on("pageerror", (err) => consoleLines.push(`[pageerror] ${err.message}`));

  await page.goto("/index.html#real-kernel");

  // Wait for the trailing `init exiting` line.
  await expect
    .poll(
      () => consoleLines.find((l) => l.includes("init exiting")) ?? null,
      { timeout: 30_000 },
    )
    .not.toBeNull();

  // At least two reap lines (one per display-client-demo).
  const reaped = consoleLines.filter((l) =>
    /init reaped child pid=\d+/.test(l),
  );
  expect(reaped.length).toBeGreaterThanOrEqual(2);

  // No pageerror — no peer was taken down by a child's exit.
  expect(consoleLines.filter((l) => l.startsWith("[pageerror]"))).toHaveLength(0);
});

test("a trapped user wasm is reconciled and its Worker route is released", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => consoleLines.push(msg.text()));
  page.on("pageerror", (err) => consoleLines.push(`[pageerror] ${err.message}`));

  await page.goto("/index.html#process-trap");
  await expect
    .poll(
      () =>
        consoleLines.find((line) =>
          line.includes("[real-kernel] process trap armed"),
        ) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();
  await expect
    .poll(
      async () =>
        page.locator("body").getAttribute("data-pmos-live-workers"),
      { timeout: 10_000 },
    )
    .toBe("0");

  expect(
    await page.locator("body").getAttribute("data-pmos-peak-live-workers"),
  ).toBe("1");
  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(false);
  expect(consoleLines.filter((line) => line.startsWith("[pageerror]"))).toEqual([]);
});

test("SIGKILL terminates the backing Worker and clears host routing", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => consoleLines.push(msg.text()));
  page.on("pageerror", (err) => consoleLines.push(`[pageerror] ${err.message}`));

  await page.goto("/index.html#process-sigkill");
  await expect
    .poll(
      () =>
        consoleLines.find((line) =>
          line.includes("[real-kernel] process SIGKILL armed"),
        ) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();
  await expect
    .poll(
      async () =>
        page.locator("body").getAttribute("data-pmos-live-workers"),
      { timeout: 10_000 },
    )
    .toBe("0");

  expect(
    await page.locator("body").getAttribute("data-pmos-last-terminated-signal"),
  ).toBe("9");
  expect(
    Number(
      await page.locator("body").getAttribute("data-pmos-last-terminated-pid"),
    ),
  ).toBeGreaterThan(0);
  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(false);
  expect(consoleLines.filter((line) => line.startsWith("[pageerror]"))).toEqual([]);
});
