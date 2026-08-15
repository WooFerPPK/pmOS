// T219 — graceful window close through the real taskbar shell-manager path.

import { expect, test, type Page } from "@playwright/test";

test.use({ viewport: { width: 1280, height: 900 } });

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const TASKBAR_Y = 752;
const FILES_TITLEBAR = [0x35, 0x5f, 0x84, 0xff] as const;
const TASKBAR_FOCUSED = [0xc2, 0xc6, 0xcf, 0xff] as const;
const TASKBAR_FIRST_ENTRY_X = 90;
const TASKBAR_ENTRY_STRIDE = 162;
const FILES_TASK_ENTRY_INDEX = 1;

async function clickFramebuffer(
  page: Page,
  x: number,
  y: number,
): Promise<void> {
  const canvas = page.locator("#pmos-fb");
  const box = await canvas.boundingBox();
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

async function titlebarAnchor(
  page: Page,
): Promise<{ x: number; y: number } | null> {
  return page
    .locator("#pmos-fb")
    .evaluate((canvas: HTMLCanvasElement, target) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      const bytes = context.getImageData(0, 0, canvas.width, 736).data;
      for (let y = 0; y < 736; y += 1) {
        let minX = canvas.width;
        let matches = 0;
        for (let x = 0; x < canvas.width; x += 1) {
          const offset = (y * canvas.width + x) * 4;
          if (
            bytes[offset] === target[0] &&
            bytes[offset + 1] === target[1] &&
            bytes[offset + 2] === target[2] &&
            bytes[offset + 3] === target[3]
          ) {
            minX = Math.min(minX, x);
            matches += 1;
          }
        }
        if (matches >= 300) return { x: minX, y };
      }
      return null;
    }, FILES_TITLEBAR);
}

async function focusedTaskEntryIndex(page: Page): Promise<number | null> {
  for (let index = 0; index < 5; index += 1) {
    const sample = await pixel(
      page,
      TASKBAR_FIRST_ENTRY_X + index * TASKBAR_ENTRY_STRIDE + 95,
      TASKBAR_Y,
    );
    if (
      sample.every((channel, offset) => channel === TASKBAR_FOCUSED[offset])
    ) {
      return index;
    }
  }
  return null;
}

async function workerCount(page: Page): Promise<number> {
  return Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
}

async function waitForLine(
  lines: string[],
  predicate: (line: string) => boolean,
  timeout = 10_000,
): Promise<string> {
  await expect
    .poll(() => lines.find(predicate) ?? null, {
      timeout,
      message: `expected OS console line; observed:\n${lines.join("\n")}`,
    })
    .not.toBeNull();
  return lines.find(predicate)!;
}

test("taskbar close is acknowledged by Files and exits its process within one second", async ({
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
  await waitForLine(consoleLines, (line) =>
    line.includes("shell: connected to /run/display"),
  );

  await clickFramebuffer(page, 40, TASKBAR_Y);
  await clickFramebuffer(page, 100, 648);
  await waitForLine(consoleLines, (line) => line.includes("files: starting"));
  await waitForLine(consoleLines, (line) => /files: ready \/.*$/.test(line));
  await expect.poll(() => titlebarAnchor(page)).not.toBeNull();
  const filesOrigin = (await titlebarAnchor(page))!;
  const titlebarPoint = { x: filesOrigin.x + 500, y: filesOrigin.y + 10 };
  const filesEntry = FILES_TASK_ENTRY_INDEX;
  await expect
    .poll(async () => ({
      focusedEntry: await focusedTaskEntryIndex(page),
      titlebar: await pixel(page, titlebarPoint.x, titlebarPoint.y),
    }))
    .toEqual({
      focusedEntry: FILES_TASK_ENTRY_INDEX,
      titlebar: [...FILES_TITLEBAR],
    });
  const filesEntryX = TASKBAR_FIRST_ENTRY_X + filesEntry * TASKBAR_ENTRY_STRIDE;

  const workersBeforeClose = await workerCount(page);
  expect(workersBeforeClose).toBeGreaterThan(0);
  const closeStarted = performance.now();
  await clickFramebuffer(page, filesEntryX + 150, TASKBAR_Y);
  await expect
    .poll(() => workerCount(page), {
      timeout: 1_000,
      message: "Files Worker did not exit within the FR-028 close budget",
    })
    .toBe(workersBeforeClose - 1);
  expect(performance.now() - closeStarted).toBeLessThan(1_000);
  await waitForLine(
    consoleLines,
    (line) => line.includes("files: close requested by display server"),
    1_000,
  );

  // Disconnect destroys the toplevel: its titlebar pixels disappear and the
  // retired task entry exposes the same taskbar base colour as empty space.
  await expect
    .poll(() => pixel(page, titlebarPoint.x, titlebarPoint.y))
    .not.toEqual([...FILES_TITLEBAR]);
  const emptyTaskbar = await pixel(page, 700, TASKBAR_Y);
  await expect
    .poll(() => pixel(page, filesEntryX + 10, TASKBAR_Y))
    .toEqual(emptyTaskbar);

  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(
    false,
  );
  expect(consoleLines.some((line) => line.startsWith("[pageerror]"))).toBe(
    false,
  );
});
