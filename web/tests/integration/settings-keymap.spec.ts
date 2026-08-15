// T196 — live Settings keyboard-layout workflow.
//
// The test keeps Terminal alive while Settings atomically updates the VFS
// preference, waits for the existing display-server process to reload Dvorak,
// then types the physical Dvorak keys for `exit`. Terminal closing proves the
// running client received the new logical mapping without a reboot or a
// browser-layer input shortcut.

import { expect, test, type Page } from "@playwright/test";

test.use({ viewport: { width: 1280, height: 900 } });

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const TASKBAR_Y = 752;
const TASKBAR_BACKGROUND = [216, 220, 228, 255];
const TASKBAR_FOCUSED = [194, 198, 207, 255];

async function clickFramebuffer(page: Page, x: number, y: number): Promise<void> {
  const box = await page.locator("#pmos-fb").boundingBox();
  if (box === null) throw new Error("framebuffer canvas has no layout box");
  await page.mouse.click(
    box.x + (x / FRAMEBUFFER_WIDTH) * box.width,
    box.y + (y / FRAMEBUFFER_HEIGHT) * box.height,
  );
}

async function pixel(page: Page, x: number, y: number): Promise<number[]> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, point: { x: number; y: number }) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      return Array.from(context.getImageData(point.x, point.y, 1, 1).data);
    },
    { x, y },
  );
}

async function pressAndWaitForFrame(page: Page, key: string): Promise<void> {
  const canvas = page.locator("#pmos-fb");
  const before = Number(
    (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
  );
  await page.keyboard.press(key);
  await expect
    .poll(
      async () =>
        Number((await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0"),
      { timeout: 2_000 },
    )
    .toBeGreaterThan(before);
}

async function findFramebufferColor(
  page: Page,
  rgb: readonly [number, number, number],
): Promise<{ x: number; y: number } | null> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, target: readonly [number, number, number]) => {
      const context = canvas.getContext("2d");
      if (context === null) return null;
      const image = context.getImageData(0, 0, canvas.width, canvas.height);
      for (let y = 0; y < canvas.height; y += 1) {
        for (let x = 0; x < canvas.width; x += 1) {
          const offset = (y * canvas.width + x) * 4;
          if (
            image.data[offset] === target[0] &&
            image.data[offset + 1] === target[1] &&
            image.data[offset + 2] === target[2]
          ) {
            return { x, y };
          }
        }
      }
      return null;
    },
    rgb,
  );
}

test("Settings applies Dvorak to an already-running Terminal", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));

  await page.goto("/index.html");
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 10_000,
  });

  // Launch Terminal (launcher row one) and keep that exact client alive.
  await clickFramebuffer(page, 40, TASKBAR_Y);
  await clickFramebuffer(page, 100, 624);
  await expect
    .poll(() => consoleLines.some((line) => line.includes("term: starting")), {
      timeout: 5_000,
    })
    .toBe(true);
  await expect.poll(() => pixel(page, 330, TASKBAR_Y)).toEqual(TASKBAR_FOCUSED);

  // Launch Settings (launcher row four) and locate its distinctive titlebar.
  await clickFramebuffer(page, 40, TASKBAR_Y);
  await clickFramebuffer(page, 100, 696);
  await expect
    .poll(
      () =>
        consoleLines.some((line) =>
          /shell: launched \/bin\/settings pid=\d+/.test(line),
        ),
      { timeout: 5_000 },
    )
    .toBe(true);
  await expect
    .poll(() => findFramebufferColor(page, [0x40, 0x60, 0x70]), {
      timeout: 5_000,
    })
    .not.toBeNull();
  const settingsOrigin = await findFramebufferColor(page, [0x40, 0x60, 0x70]);
  expect(settingsOrigin).not.toBeNull();

  // Select Keyboard, then cycle at most three times. This is robust to a
  // persisted prior selection: every supported layout reaches Dvorak within
  // three presses, and each press waits for the display-server reload event.
  await clickFramebuffer(page, settingsOrigin!.x + 232, settingsOrigin!.y + 27);
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const before = consoleLines.filter((line) =>
      line.includes("display-server: keymap changed"),
    ).length;
    await clickFramebuffer(page, settingsOrigin!.x + 500, settingsOrigin!.y + 331);
    await expect
      .poll(
        () =>
          consoleLines.filter((line) =>
            line.includes("display-server: keymap changed"),
          ).length,
        { timeout: 2_000 },
      )
      .toBeGreaterThan(before);
    if (
      consoleLines.some((line) =>
        line.includes("display-server: keymap changed dvorak"),
      )
    ) {
      break;
    }
  }
  expect(
    consoleLines.some((line) =>
      line.includes("display-server: keymap changed dvorak"),
    ),
  ).toBe(true);

  // Close Settings, refocus the original Terminal, then press the physical
  // Dvorak keys D/B/G/K = logical e/x/i/t. Under the old boot-only US path
  // this would type `dbdk` and the terminal would remain open.
  await expect
    .poll(
      async () => {
        await clickFramebuffer(page, 562, TASKBAR_Y);
        return pixel(page, 490, TASKBAR_Y);
      },
      { timeout: 5_000 },
    )
    .toEqual(TASKBAR_BACKGROUND);
  // Poll the observable server round-trip, not elapsed time. Retrying also
  // handles a host pointer-button event overtaking its preceding motion in
  // Firefox: once the guest has consumed the coordinates, the next press
  // focuses the intended entry. The focused palette is painted only after the
  // display server broadcasts window_focused back to the shell.
  await expect
    .poll(
      async () => {
        await clickFramebuffer(page, 330, TASKBAR_Y);
        return pixel(page, 330, TASKBAR_Y);
      },
      { timeout: 5_000 },
    )
    .toEqual(TASKBAR_FOCUSED);
  for (const physicalKey of ["KeyD", "KeyB", "KeyG", "KeyK"]) {
    await pressAndWaitForFrame(page, physicalKey);
  }
  await page.keyboard.press("Enter");
  await expect
    .poll(() => pixel(page, 330, TASKBAR_Y), { timeout: 5_000 })
    .toEqual(TASKBAR_BACKGROUND);
});
