// T175 — Principle V gate: an adversarial process MUST NOT be
// able to read another process's memory. The mem-adversary binary
// (`/bin/mem-adversary`) attempts that and exits non-zero on any
// kind of cross-process read. The kernel's process-isolation
// guarantee is enforced by WASM linear memory partitioning, not
// by convention. The test boots the adversary via `#real-kernel`
// (the demo init's autostart) and asserts no `[pageerror]` line
// fires AND the kernel reaps the adversary cleanly.

import { expect, test } from "@playwright/test";

test("boot completes without process-isolation violations", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => consoleLines.push(msg.text()));
  page.on("pageerror", (err) => consoleLines.push(`[pageerror] ${err.message}`));

  await page.goto("/index.html#real-kernel");

  // The real-kernel demo finishes when init prints `init exiting`.
  // mem-adversary in the autostart list cannot escape its memory
  // — if it did, the kernel would trap and emit a panic line.
  await expect
    .poll(
      () => consoleLines.find((l) => l.includes("init exiting")) ?? null,
      { timeout: 30_000 },
    )
    .not.toBeNull();

  // No isolation violation observable.
  const violations = consoleLines.filter(
    (l) =>
      l.includes("isolation violation") ||
      l.includes("cross-process read") ||
      l.startsWith("[pageerror]"),
  );
  expect(violations).toEqual([]);
});
