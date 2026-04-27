// T196 — Settings keymap pick up by display server. T188/T189
// land the bundled keymaps + display-server `pmd_keymap_manager`
// global; today only us-qwerty.bin is bundled. Boot the desktop
// and verify the input-echo demo round-trips a known key under
// the default keymap (no Dvorak swap until the full T188/T189
// stack lands).

import { expect, test } from "@playwright/test";

test("default keymap delivers a key end-to-end", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (msg) => consoleLines.push(msg.text()));

  await page.goto("/index.html#input-echo");

  // hello_input_echo polls /dev/input_kbd; pressing 'a' makes it
  // echo the byte to /dev/console.
  await page.keyboard.press("a");

  await expect
    .poll(
      () => consoleLines.find((l) => l.includes("hello_input_echo: ")) ?? null,
      { timeout: 15_000 },
    )
    .not.toBeNull();
});
