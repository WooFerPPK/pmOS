// T191 — Settings default-terminal-font workflow.
//
// The first terminal starts with the safe 8x14 default. Settings then writes
// the alternate font through the canonical VFS preference, and a newly launched
// terminal reports 8x16 startup metrics. The first process remains alive, which
// also pins the contract that running terminals are not restarted.

import { expect, test, type Page } from "@playwright/test";

import { launchTerminal } from "./guest-terminal";
import {
  launcherMenuRegionFingerprint,
  openLauncherMenuBefore,
  selectLauncherRowBefore,
} from "./launcher-interaction";

test.use({ viewport: { width: 1280, height: 900 } });

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;

async function clickFramebuffer(
  page: Page,
  x: number,
  y: number,
): Promise<void> {
  const box = await page.locator("#pmos-fb").boundingBox();
  if (box === null) throw new Error("framebuffer canvas has no layout box");
  await page.mouse.click(
    box.x + (x / FRAMEBUFFER_WIDTH) * box.width,
    box.y + (y / FRAMEBUFFER_HEIGHT) * box.height,
  );
}

async function framebufferRegionFingerprint(
  page: Page,
  region: { x: number; y: number; width: number; height: number },
): Promise<number> {
  return page
    .locator("#pmos-fb")
    .evaluate(
      (
        canvas: HTMLCanvasElement,
        bounds: { x: number; y: number; width: number; height: number },
      ) => {
        const context = canvas.getContext("2d");
        if (context === null) return 0;
        const bytes = context.getImageData(
          bounds.x,
          bounds.y,
          bounds.width,
          bounds.height,
        ).data;
        let hash = 0x811c9dc5;
        for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193);
        return hash >>> 0;
      },
      region,
    );
}

async function clickAndWaitForSettingsPaint(
  page: Page,
  x: number,
  y: number,
  settingsRegion: { x: number; y: number; width: number; height: number },
): Promise<void> {
  const before = await framebufferRegionFingerprint(page, settingsRegion);
  await clickFramebuffer(page, x, y);
  await expect
    .poll(() => framebufferRegionFingerprint(page, settingsRegion), {
      timeout: 2_000,
      message: "Settings did not repaint after its pointer action",
    })
    .not.toBe(before);
}

async function findFramebufferColor(
  page: Page,
  rgb: readonly [number, number, number],
): Promise<{ x: number; y: number } | null> {
  return page
    .locator("#pmos-fb")
    .evaluate(
      (
        canvas: HTMLCanvasElement,
        target: readonly [number, number, number],
      ) => {
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

test("Settings font applies to a new Terminal only", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));
  page.on("pageerror", (error) =>
    consoleLines.push(`[pageerror] ${error.message}`),
  );

  await page.goto("/index.html");
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 10_000,
  });

  await launchTerminal(page, consoleLines, { timeout: 5_000 });
  expect(
    consoleLines.filter((line) => line.includes("term: starting font=8x14")),
  ).toHaveLength(1);

  const closedLauncherFingerprint = await launcherMenuRegionFingerprint(page);
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  const settingsLogStart = consoleLines.length;
  await selectLauncherRowBefore(
    page,
    100,
    696,
    Date.now() + 5_000,
    closedLauncherFingerprint,
  );
  await expect
    .poll(
      () =>
        consoleLines
          .slice(settingsLogStart)
          .some((line) => /shell: launched \/bin\/settings pid=\d+/.test(line)),
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
  const settingsRegion = {
    x: settingsOrigin!.x,
    y: settingsOrigin!.y,
    width: 560,
    height: 360,
  };

  // Terminal is tab five (zero-based index four); Apply cycles the absent
  // preference from the compact default to pc-vga-16.pbm.
  await clickAndWaitForSettingsPaint(
    page,
    settingsOrigin!.x + 418,
    settingsOrigin!.y + 27,
    settingsRegion,
  );
  await clickAndWaitForSettingsPaint(
    page,
    settingsOrigin!.x + 500,
    settingsOrigin!.y + 331,
    settingsRegion,
  );

  await launchTerminal(page, consoleLines, { timeout: 5_000 });
  expect(
    consoleLines.filter((line) => line.includes("term: starting font=8x16")),
  ).toHaveLength(1);
  expect(
    consoleLines.filter((line) => line.includes("term: starting font=8x14")),
  ).toHaveLength(1);
  expect(consoleLines.filter((line) => line.startsWith("[pageerror]"))).toEqual(
    [],
  );
});
