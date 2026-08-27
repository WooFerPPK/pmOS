// T219 — graceful window close through a real client-painted caption button.

import { expect, test, type Page } from "@playwright/test";
import {
  LIGHT_TITLEBAR,
  TASKBAR_LIGHT,
  TASKBAR_LIGHT_FOCUSED,
  taskbarEntryPoint,
  titlebarControlPoint,
  waitForActiveWindowBounds,
} from "./windows-ui";

test.use({ viewport: { width: 1280, height: 900 } });

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const TASKBAR_Y = 752;

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

async function focusedTaskEntryIndex(page: Page): Promise<number | null> {
  for (let index = 0; index < 1; index += 1) {
    const point = taskbarEntryPoint(index, 1);
    const sample = await pixel(page, point.x, point.y);
    if (
      sample.every(
        (channel, offset) => channel === TASKBAR_LIGHT_FOCUSED[offset],
      )
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

test("titlebar close is acknowledged by Files and exits its process within one second", async ({
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
  const filesOrigin = await waitForActiveWindowBounds(page, {
    expectedWidth: 640,
  });
  const titlebarPoint = { x: filesOrigin.x + 500, y: filesOrigin.y + 10 };
  await expect
    .poll(async () => ({
      focusedEntry: await focusedTaskEntryIndex(page),
      titlebar: await pixel(page, titlebarPoint.x, titlebarPoint.y),
    }))
    .toEqual({
      focusedEntry: 0,
      titlebar: [...LIGHT_TITLEBAR],
    });

  const workersBeforeClose = await workerCount(page);
  expect(workersBeforeClose).toBeGreaterThan(0);
  const closeStarted = performance.now();
  const close = titlebarControlPoint(filesOrigin, "close");
  await clickFramebuffer(page, close.x, close.y);
  await expect
    .poll(() => workerCount(page), {
      timeout: 1_000,
      message: "Files Worker did not exit within the FR-028 close budget",
    })
    .toBe(workersBeforeClose - 1);
  expect(performance.now() - closeStarted).toBeLessThan(1_000);
  await waitForLine(
    consoleLines,
    (line) => line.includes("files: close requested by client titlebar"),
    1_000,
  );
  // Disconnect destroys the toplevel: its titlebar pixels disappear and the
  // retired task entry exposes the same taskbar base colour as empty space.
  await expect
    .poll(() => pixel(page, titlebarPoint.x, titlebarPoint.y))
    .not.toEqual([...LIGHT_TITLEBAR]);
  await expect
    .poll(() => {
      const task = taskbarEntryPoint(0, 1);
      return pixel(page, task.x, task.y);
    })
    .toEqual([...TASKBAR_LIGHT]);

  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(
    false,
  );
  expect(consoleLines.some((line) => line.startsWith("[pageerror]"))).toBe(
    false,
  );
});
