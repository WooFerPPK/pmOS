// T134 — real shipped-window management through the browser → input driver →
// display protocol → ordinary userland stack. No DOM window surrogate is used:
// every assertion is based on the PMos framebuffer, process logs, or worker
// lifecycle state.

import { expect, test, type Page } from "@playwright/test";
import {
  LIGHT_TITLEBAR,
  TASKBAR_LIGHT_FOCUSED,
  TASKBAR_LIGHT_MINIMIZED,
  activeWindowBounds,
  taskbarEntryPoint,
  titlebarControlPoint,
  waitForActiveWindowBounds,
} from "./windows-ui";

test.use({ viewport: { width: 1280, height: 900 } });

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const TASKBAR_Y = 752;

interface Point {
  readonly x: number;
  readonly y: number;
}

interface ColorBounds extends Point {
  readonly right: number;
  readonly bottom: number;
  readonly count: number;
}

async function framebufferBox(page: Page) {
  const box = await page.locator("#pmos-fb").boundingBox();
  if (box === null) throw new Error("framebuffer canvas has no layout box");
  return box;
}

async function toPagePoint(page: Page, point: Point): Promise<Point> {
  const box = await framebufferBox(page);
  return {
    x: box.x + (point.x / FRAMEBUFFER_WIDTH) * box.width,
    y: box.y + (point.y / FRAMEBUFFER_HEIGHT) * box.height,
  };
}

async function frameSequence(page: Page): Promise<number> {
  return Number(
    (await page
      .locator("#pmos-fb")
      .getAttribute("data-pmos-frame-sequence")) ?? "0",
  );
}

async function clickFramebuffer(
  page: Page,
  x: number,
  y: number,
  waitForFrame = false,
): Promise<void> {
  const before = await frameSequence(page);
  const point = await toPagePoint(page, { x, y });
  await page.mouse.click(point.x, point.y);
  if (waitForFrame) {
    await expect
      .poll(() => frameSequence(page), { timeout: 3_000 })
      .toBeGreaterThan(before);
  }
}

async function pixel(page: Page, x: number, y: number): Promise<number[]> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, point: Point) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      return Array.from(context.getImageData(point.x, point.y, 1, 1).data);
    },
    { x, y },
  );
}

async function waitForLine(
  lines: string[],
  predicate: (line: string) => boolean,
  timeout = 10_000,
  startIndex = 0,
): Promise<string> {
  await expect
    .poll(() => lines.slice(startIndex).find(predicate) ?? null, {
      timeout,
      message: `expected OS console line; observed:\n${lines.join("\n")}`,
    })
    .not.toBeNull();
  return lines.slice(startIndex).find(predicate)!;
}

async function focusedTaskEntryIndex(
  page: Page,
  entryCount: number,
): Promise<number | null> {
  for (let index = 0; index < entryCount; index += 1) {
    const point = taskbarEntryPoint(index, entryCount);
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

async function taskEntryPalette(
  page: Page,
  index: number,
  entryCount: number,
): Promise<number[]> {
  const point = taskbarEntryPoint(index, entryCount);
  return pixel(page, point.x, point.y);
}

async function bootDesktop(page: Page, lines: string[]): Promise<void> {
  await page.goto("/index.html");
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 12_000,
  });
  await waitForLine(lines, (line) =>
    line.includes("shell: loaded 5 applications from /usr/share/applications"),
  );
  await waitForLine(lines, (line) =>
    line.includes("shell: connected to /run/display"),
  );
  await expect.poll(() => frameSequence(page), { timeout: 5_000 }).toBeGreaterThan(0);
}

async function launchFiles(page: Page, lines: string[]): Promise<ColorBounds> {
  await clickFramebuffer(page, 40, TASKBAR_Y, true);
  await clickFramebuffer(page, 100, 648);
  await waitForLine(lines, (line) => line.includes("files: starting"));
  await waitForLine(lines, (line) => /files: ready \/.*$/.test(line));
  const bounds = await waitForActiveWindowBounds(page, {
    expectedWidth: 640,
    timeout: 10_000,
  });
  return {
    ...bounds,
    bottom: bounds.y + 1,
    count: bounds.width,
  };
}

test("real Files window focuses, raises, drags, minimizes, restores, and toggles work-area maximize", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));
  page.on("pageerror", (error) =>
    consoleLines.push(`[pageerror] ${error.message}`),
  );

  await bootDesktop(page, consoleLines);
  const initialFiles = await launchFiles(page, consoleLines);
  expect(initialFiles.count).toBeGreaterThan(500);
  await expect
    .poll(() => focusedTaskEntryIndex(page, 1), { timeout: 5_000 })
    .toBe(0);
  const filesEntry = 0;

  // Launch Terminal so its newer overlapping toplevel covers a known Files
  // toolbar pixel.
  const overlap = { x: initialFiles.x + 80, y: initialFiles.y + 50 };
  const filesOverlapPixel = await pixel(page, overlap.x, overlap.y);
  await clickFramebuffer(page, 40, TASKBAR_Y, true);
  await clickFramebuffer(page, 100, 624);
  await waitForLine(
    consoleLines,
    (line) => line.includes("term: starting"),
  );
  await expect
    .poll(() => pixel(page, overlap.x, overlap.y), { timeout: 10_000 })
    .not.toEqual(filesOverlapPixel);
  await expect
    .poll(async () => {
      const focused = await focusedTaskEntryIndex(page, 2);
      return focused === 1;
    }, { timeout: 5_000 })
    .toBe(true);
  const terminalEntry = 1;

  // The taskbar label routes through the shell-manager focus request. It must
  // focus Files and raise the real surface in the server's global z-order.
  const filesTask = taskbarEntryPoint(filesEntry, 2);
  await clickFramebuffer(page, filesTask.x, filesTask.y);
  await expect
    .poll(() => pixel(page, overlap.x, overlap.y), { timeout: 5_000 })
    .toEqual(filesOverlapPixel);
  await expect
    .poll(() => focusedTaskEntryIndex(page, 2), { timeout: 5_000 })
    .toBe(filesEntry);

  // Press inside the client-painted Files titlebar. Files logs after sending
  // request_move(real pointer serial), which gives the test an event-driven
  // handoff before it advances the physical pointer.
  const dragFrom = {
    x: initialFiles.x + 300,
    y: initialFiles.y + 10,
  };
  const dragTo = { x: dragFrom.x + 120, y: dragFrom.y + 80 };
  const fromPage = await toPagePoint(page, dragFrom);
  const warmPage = await toPagePoint(page, {
    x: dragFrom.x + 24,
    y: dragFrom.y + 16,
  });
  const toPage = await toPagePoint(page, dragTo);
  await page.mouse.move(fromPage.x, fromPage.y);
  const moveLogStart = consoleLines.length;
  await page.mouse.down();
  await waitForLine(
    consoleLines,
    (line) => /files: move requested serial=\d+/.test(line),
    3_000,
    moveLogStart,
  );
  // request_move and physical input arrive over separate kernel-mediated
  // streams. A short warm-up move proves the server has entered its drag
  // state before measuring the larger user-visible displacement.
  await page.mouse.move(warmPage.x, warmPage.y, { steps: 16 });
  await expect
    .poll(async () => {
      const bounds = await activeWindowBounds(page);
      return bounds !== null &&
        (bounds.x > initialFiles.x || bounds.y > initialFiles.y);
    }, { timeout: 3_000 })
    .toBe(true);
  const beforeDragFrame = await frameSequence(page);
  await page.mouse.move(toPage.x, toPage.y);
  await expect
    .poll(() => frameSequence(page), { timeout: 3_000 })
    .toBeGreaterThan(beforeDragFrame);
  await expect
    .poll(async () => {
      const bounds = await activeWindowBounds(page);
      return bounds !== null &&
        bounds.x > initialFiles.x + 60 &&
        bounds.y > initialFiles.y + 40;
    }, { timeout: 3_000 })
    .toBe(true);
  const releaseLogStart = consoleLines.length;
  await page.mouse.up();
  await waitForLine(
    consoleLines,
    (line) => line.includes("display-server: drag completed"),
    3_000,
    releaseLogStart,
  );
  const draggedFiles = await waitForActiveWindowBounds(page, {
    expectedWidth: 640,
  });
  expect(draggedFiles.x).toBeGreaterThan(initialFiles.x + 60);
  expect(draggedFiles.y).toBeGreaterThan(initialFiles.y + 40);
  await expect
    .poll(() => pixel(page, draggedFiles.x + 220, draggedFiles.y + 10))
    .toEqual([...LIGHT_TITLEBAR]);

  // A completed drag release must hand pointer ownership back to normal shell
  // hit-testing. Exercise both real task labels once more and prove the
  // overlapping dragged surface follows the global focus/raise order.
  const terminalTask = taskbarEntryPoint(terminalEntry, 2);
  await clickFramebuffer(page, terminalTask.x, terminalTask.y);
  await expect
    .poll(() => focusedTaskEntryIndex(page, 2), { timeout: 5_000 })
    .toBe(terminalEntry);
  await expect
    .poll(() => pixel(page, draggedFiles.x + 220, draggedFiles.y + 10))
    .not.toEqual([...LIGHT_TITLEBAR]);
  await clickFramebuffer(page, filesTask.x, filesTask.y);
  await expect
    .poll(() => focusedTaskEntryIndex(page, 2), { timeout: 5_000 })
    .toBe(filesEntry);
  await expect
    .poll(() => pixel(page, draggedFiles.x + 220, draggedFiles.y + 10))
    .toEqual([...LIGHT_TITLEBAR]);

  // Minimizing removes Files from composition; clicking its dynamically found
  // label restores and raises the same dragged window.
  await clickFramebuffer(page, filesTask.x, filesTask.y);
  await expect
    .poll(() => pixel(page, draggedFiles.x + 220, draggedFiles.y + 10))
    .not.toEqual([...LIGHT_TITLEBAR]);
  await expect
    .poll(() => taskEntryPalette(page, filesEntry, 2))
    .toEqual([...TASKBAR_LIGHT_MINIMIZED]);
  await clickFramebuffer(page, filesTask.x, filesTask.y);
  await expect
    .poll(() => pixel(page, draggedFiles.x + 220, draggedFiles.y + 10))
    .toEqual([...LIGHT_TITLEBAR]);
  await expect
    .poll(() => taskEntryPalette(page, filesEntry, 2))
    .toEqual([...TASKBAR_LIGHT_FOCUSED]);

  const taskbarPixel = await pixel(page, 700, TASKBAR_Y);
  const maximize = titlebarControlPoint(draggedFiles, "maximize");
  await clickFramebuffer(page, maximize.x, maximize.y);
  await waitForLine(
    consoleLines,
    (line) => line.includes("files: window maximized 1024x736"),
    5_000,
  );
  await expect.poll(() => pixel(page, 900, 10)).toEqual([...LIGHT_TITLEBAR]);
  await expect.poll(() => pixel(page, 700, TASKBAR_Y)).toEqual(taskbarPixel);

  const maximizedFiles = await waitForActiveWindowBounds(page, {
    minimumWidth: 900,
    message: "maximized Files frame vanished",
  });
  const restore = titlebarControlPoint(maximizedFiles, "maximize");
  await clickFramebuffer(page, restore.x, restore.y);
  await waitForLine(
    consoleLines,
    (line) => line.includes("files: window restored 640x420"),
    5_000,
  );
  await expect
    .poll(() => pixel(page, draggedFiles.x + 220, draggedFiles.y + 10))
    .toEqual([...LIGHT_TITLEBAR]);
  await expect.poll(() => pixel(page, 900, 10)).not.toEqual([...LIGHT_TITLEBAR]);

  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(
    false,
  );
  expect(consoleLines.some((line) => line.startsWith("[pageerror]"))).toBe(
    false,
  );
});
