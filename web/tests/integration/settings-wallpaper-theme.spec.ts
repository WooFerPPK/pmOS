// T195 — real Settings wallpaper + theme workflow.
//
// This exercises only OS-visible behavior: Settings atomically updates the
// canonical VFS file, the shell repaints its wallpaper/theme, an already-open
// toolkit client (Sysmon) repaints without restarting, and OPFS restores both
// choices after a full page reload.

import { expect, test, type Page } from "@playwright/test";

test.use({ viewport: { width: 1280, height: 900 } });

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const TASKBAR_Y = 752;
const DARK_TITLEBAR = [0x2b, 0x31, 0x3d, 0xff];

async function clickFramebuffer(page: Page, x: number, y: number): Promise<void> {
  const box = await page.locator("#pmos-fb").boundingBox();
  if (box === null) throw new Error("framebuffer canvas has no layout box");
  await page.mouse.click(
    box.x + (x / FRAMEBUFFER_WIDTH) * box.width,
    box.y + (y / FRAMEBUFFER_HEIGHT) * box.height,
  );
}

async function clickAndWaitForFrame(
  page: Page,
  x: number,
  y: number,
): Promise<void> {
  const canvas = page.locator("#pmos-fb");
  const before = Number(
    (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
  );
  await clickFramebuffer(page, x, y);
  await expect
    .poll(
      async () =>
        Number((await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0"),
      { timeout: 2_000 },
    )
    .toBeGreaterThan(before);
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

async function wallpaperFingerprint(page: Page): Promise<number> {
  // Both bundled app windows end to the left of this region at their default
  // sizes. Hashing the raw desktop pixels is stronger than relying on one
  // hand-picked pixel that two real images might happen to share.
  return page.locator("#pmos-fb").evaluate((canvas: HTMLCanvasElement) => {
    const context = canvas.getContext("2d");
    if (context === null) throw new Error("framebuffer 2d context missing");
    const bytes = context.getImageData(900, 60, 100, 80).data;
    let hash = 0x811c9dc5;
    for (const byte of bytes) {
      hash ^= byte;
      hash = Math.imul(hash, 0x01000193);
    }
    return hash >>> 0;
  });
}

async function findSolidRun(
  page: Page,
  rgb: readonly [number, number, number],
  minimumRun: number,
): Promise<{ x: number; y: number } | null> {
  return page.locator("#pmos-fb").evaluate(
    (
      canvas: HTMLCanvasElement,
      target: { rgb: readonly [number, number, number]; minimumRun: number },
    ) => {
      const context = canvas.getContext("2d");
      if (context === null) return null;
      const height = Math.min(canvas.height, 736); // exclude the taskbar
      const image = context.getImageData(0, 0, canvas.width, height);
      for (let y = 0; y < height; y += 1) {
        let runStart = 0;
        let runLength = 0;
        for (let x = 0; x < canvas.width; x += 1) {
          const offset = (y * canvas.width + x) * 4;
          const matches =
            image.data[offset] === target.rgb[0] &&
            image.data[offset + 1] === target.rgb[1] &&
            image.data[offset + 2] === target.rgb[2] &&
            image.data[offset + 3] === 0xff;
          if (matches) {
            if (runLength === 0) runStart = x;
            runLength += 1;
            if (runLength >= target.minimumRun) {
              return { x: runStart, y };
            }
          } else {
            runLength = 0;
          }
        }
      }
      return null;
    },
    { rgb, minimumRun },
  );
}

test("Settings repaints wallpaper and a running themed app, then persists both", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));
  page.on("pageerror", (error) =>
    consoleLines.push(`[pageerror] ${error.message}`),
  );

  await page.goto("/index.html");
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 12_000,
  });
  await expect
    .poll(() =>
      consoleLines.some((line) =>
        line.includes("shell: loaded 5 applications from /usr/share/applications"),
      ),
    )
    .toBe(true);
  const originalWallpaper = await wallpaperFingerprint(page);

  // Open Sysmon first and keep that exact process alive while Settings writes
  // the preference. Sysmon is launcher row five.
  await clickAndWaitForFrame(page, 40, TASKBAR_Y);
  await clickFramebuffer(page, 100, 720);
  await expect
    .poll(
      () => consoleLines.filter((line) => line.includes("sysmon: starting")).length,
      { timeout: 5_000 },
    )
    .toBe(1);
  await expect
    .poll(() =>
      consoleLines.some((line) =>
        /sysmon: ready processes=\d+ terminate=(enabled|read-only)/.test(line),
      ),
    )
    .toBe(true);

  // Settings is launcher row four. Its custom titlebar is a long solid run,
  // which gives us its framebuffer-space origin without a DOM/UI shortcut.
  await clickAndWaitForFrame(page, 40, TASKBAR_Y);
  await clickFramebuffer(page, 100, 696);
  await expect
    .poll(() => findSolidRun(page, [0x40, 0x60, 0x70], 300), {
      timeout: 5_000,
    })
    .not.toBeNull();
  const settingsOrigin = await findSolidRun(page, [0x40, 0x60, 0x70], 300);
  expect(settingsOrigin).not.toBeNull();

  // Wallpaper is the initially selected tab. The first Apply moves the fresh
  // default from blue.png to green.png and the shell repaints from VFS bytes.
  await clickAndWaitForFrame(
    page,
    settingsOrigin!.x + 500,
    settingsOrigin!.y + 331,
  );
  await expect
    .poll(() => wallpaperFingerprint(page), { timeout: 5_000 })
    .not.toBe(originalWallpaper);

  // Select Appearance (tab two), then Apply. A fresh profile moves light to
  // dark. The same already-running Sysmon process must observe and repaint.
  await clickAndWaitForFrame(
    page,
    settingsOrigin!.x + 139,
    settingsOrigin!.y + 27,
  );
  await clickAndWaitForFrame(
    page,
    settingsOrigin!.x + 500,
    settingsOrigin!.y + 331,
  );
  await expect
    .poll(
      () =>
        consoleLines.filter((line) => line.includes("sysmon: theme changed dark"))
          .length,
      { timeout: 5_000 },
    )
    .toBe(1);
  expect(
    consoleLines.filter((line) => line.includes("sysmon: starting")),
  ).toHaveLength(1);
  await expect.poll(() => pixel(page, 800, TASKBAR_Y)).toEqual(DARK_TITLEBAR);

  // Bring Sysmon back to the front through its original taskbar entry. Its
  // dark toolkit titlebar is framebuffer evidence that the live process used
  // the new palette; no restart or replacement process is accepted.
  await clickAndWaitForFrame(page, 330, TASKBAR_Y);
  await expect
    .poll(() => findSolidRun(page, [0x2b, 0x31, 0x3d], 300), {
      timeout: 5_000,
    })
    .not.toBeNull();
  const sysmonOrigin = await findSolidRun(page, [0x2b, 0x31, 0x3d], 300);
  expect(sysmonOrigin).not.toBeNull();
  await expect
    .poll(() => pixel(page, sysmonOrigin!.x + 300, sysmonOrigin!.y + 10))
    .toEqual(DARK_TITLEBAR);
  expect(
    consoleLines.filter((line) => line.includes("sysmon: starting")),
  ).toHaveLength(1);

  // Appearance also cycles the fit mode, so record the final visible image
  // after both writes. A page reload rebuilds every process from scratch while
  // preserving the OPFS preference file.
  const persistedWallpaper = await wallpaperFingerprint(page);
  const reloadLogStart = consoleLines.length;
  await page.reload();
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 12_000,
  });
  await expect.poll(() => pixel(page, 800, TASKBAR_Y)).toEqual(DARK_TITLEBAR);
  await expect
    .poll(() => wallpaperFingerprint(page), { timeout: 5_000 })
    .toBe(persistedWallpaper);

  expect(
    consoleLines
      .slice(reloadLogStart)
      .some((line) => line.includes("real kernel panic")),
  ).toBe(false);
  expect(consoleLines.some((line) => line.startsWith("[pageerror]"))).toBe(false);
});
