// T162 — host import/export. T152-T156 implement the kernel-side
// host_file_recv + bootstrap drag-drop handler; absent those, the
// import flow has no plumbing. This spec validates the boot path,
// since both halves of import/export sit on top of it.

import { expect, test } from "@playwright/test";

test("desktop boots; host-file IPC plumbing is reachable", async ({ page }) => {
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
