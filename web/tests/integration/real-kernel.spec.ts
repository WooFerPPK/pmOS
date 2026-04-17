// First Playwright integration test: a real browser loads the
// served `dist/`, the bundled `bootstrap.js` opts into
// real-kernel mode via the URL hash, the kernel Worker fetches
// `/assets/kernel.wasm` + every `/assets/bin/*.wasm` listed in
// `/manifest.json`, registers an init pid, dispatches
// `PROC_SPAWN(/bin/hello-std)`, and `hello-std` runs to
// completion through the real Rust kernel and the real WASI
// shim layer.
//
// The observable signal is the page console — `bootstrap.ts`
// in real-kernel mode prefixes every flushed `/dev/console`
// line with `[real-kernel]`. The test scrapes those lines via
// `page.on('console', ...)` and asserts hello-std's payload
// (`"hello from std"`) appears.

import { expect, test } from "@playwright/test";

test("real kernel boots and runs hello-std end-to-end", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => {
    consoleLines.push(msg.text());
  });
  page.on("pageerror", (err) => {
    consoleLines.push(`[pageerror] ${err.message}`);
  });

  await page.goto("/index.html#real-kernel");

  // Poll until hello-std's output reaches the page console. The
  // boot path involves: kernel-worker.js spawn → fetch
  // /assets/kernel.wasm + /manifest.json + /assets/bin/hello-std.wasm
  // → KernelWasmHost.create → PROC_SPAWN init pid → drain. On a
  // local dev-server this completes well under a second; the
  // 15s timeout is for cold-start CI machines.
  await expect.poll(
    () => consoleLines.find((line) => line.includes("[real-kernel] hello from std")) ?? null,
    { timeout: 15_000 },
  ).not.toBeNull();

  // The real-kernel boot also logs a "real kernel ready"
  // lifecycle line; absence of a kernel panic line is the
  // companion negative assertion.
  expect(consoleLines.some((l) => l.includes("real kernel ready"))).toBe(true);
  expect(consoleLines.some((l) => l.includes("real kernel panic"))).toBe(false);
});
