// First Playwright integration test: a real browser loads the
// served `dist/`, the bundled `bootstrap.js` picks real-kernel
// mode as its default boot path, the kernel Worker fetches
// `/assets/kernel.wasm` + every `/assets/bin/*.wasm` listed in
// `/manifest.json`, registers a synthetic boot-loader pid,
// dispatches `PROC_SPAWN(/bin/init)`, and `init` runs to
// completion through the real Rust kernel. Init itself then
// calls `pmos_ext.proc_spawn("/bin/hello-std")`, which queues
// hello-std on the drain loop; once init exits, hello-std runs
// and prints its own payload.
//
// The observable signal is the page console — `bootstrap.ts`
// in real-kernel mode prefixes every flushed `/dev/console`
// line with `[real-kernel]`. The test scrapes those lines via
// `page.on('console', ...)` and asserts the expected ordered
// sequence from init + hello-std reaches the browser.
//
// The bare URL `/index.html` (no hash) is what a fresh visitor
// hits, so the test deliberately uses that: if real-kernel is
// no longer the default, the test fails even when the
// explicit `#real-kernel` hash continues to work.

import { expect, test } from "@playwright/test";

test("real kernel is the default boot path and runs init -> hello-std", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => {
    consoleLines.push(msg.text());
  });
  page.on("pageerror", (err) => {
    consoleLines.push(`[pageerror] ${err.message}`);
  });

  await page.goto("/index.html");

  // Poll until hello-std's line reaches the page. On a local
  // dev-server the full boot (init → proc_spawn → drain →
  // hello-std → fd_write → postMessage) completes in ~200 ms;
  // the 15 s timeout is for cold-start CI.
  const helloStdLine = () =>
    consoleLines.find((l) => l.includes("[real-kernel] hello from std")) ??
    null;
  await expect.poll(helloStdLine, { timeout: 15_000 }).not.toBeNull();

  // With hello-std observed, init's lines MUST already be present
  // (the drain loop is sequential: init ran to completion before
  // hello-std started). Pull them out and assert the ordering +
  // shape explicitly.
  const initStartIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] init starting"),
  );
  const initSpawnIdx = consoleLines.findIndex((l) =>
    /\[real-kernel\] init spawned hello-std pid=\d+/.test(l),
  );
  const initExitIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] init exiting"),
  );
  const helloStdIdx = consoleLines.findIndex((l) =>
    l.includes("[real-kernel] hello from std"),
  );

  expect(initStartIdx).toBeGreaterThanOrEqual(0);
  expect(initSpawnIdx).toBeGreaterThan(initStartIdx);
  expect(initExitIdx).toBeGreaterThan(initSpawnIdx);
  expect(helloStdIdx).toBeGreaterThan(initExitIdx);

  expect(consoleLines.some((l) => l.includes("real kernel ready"))).toBe(true);
  expect(consoleLines.some((l) => l.includes("real kernel panic"))).toBe(false);

  // DOM surface: real-kernel mode renders each captured line into a
  // `<pre id="pmos-real-console">` so the page itself shows the
  // boot output (not just dev tools). The element's text must
  // include every line hello-std + init produced, in the same
  // order as the page-console capture above.
  const domText = await page.locator("#pmos-real-console").innerText();
  expect(domText).toContain("init starting");
  expect(domText).toMatch(/init spawned hello-std pid=\d+/);
  expect(domText).toContain("init exiting");
  expect(domText).toContain("hello from std");
  expect(domText.indexOf("init starting")).toBeLessThan(
    domText.indexOf("hello from std"),
  );

  // T234: init and hello-std MUST run in different user Workers, not
  // sequentially in the kernel Worker's in-process drain loop. The
  // bootstrap exposes the spawn router's peak `liveWorkers.size` via
  // `<body data-pmos-peak-live-workers="N">`; init plus hello-std
  // overlap (init is still printing "init exiting" while hello-std's
  // Worker is already alive and parked on its first FD_WRITE), so
  // peak is at least 2. The kernel Worker is NOT counted — only user
  // Workers under `createSpawnRouter`'s management are.
  const peakAttr = await page
    .locator("body")
    .getAttribute("data-pmos-peak-live-workers");
  expect(peakAttr).not.toBeNull();
  expect(Number(peakAttr)).toBeGreaterThanOrEqual(2);

  // T234: the kernel-wake-slot transport landed before any user Worker
  // spawned. Without this, every spawn would race the SAB-allocation
  // path. The bootstrap stamps `data-pmos-wake-slot-ready="1"` on
  // `<body>` the moment the `kernel:wake-slot` message arrives — the
  // ordering with the four-line console output above implicitly
  // proves it landed first (the FIRST proc:spawn happens AFTER
  // bootRealKernel posts kernel:wake-slot and BEFORE init's println
  // even runs), but checking it explicitly is cheap and forces a
  // clear failure mode if the kernel-worker entry's posting order
  // ever regresses.
  const wakeSlotReady = await page
    .locator("body")
    .getAttribute("data-pmos-wake-slot-ready");
  expect(wakeSlotReady).toBe("1");
});
