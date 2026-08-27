// Principle VII release gate: launch the real standalone WASI binary through
// the guest shell, prove its separately isolated Worker speaks the production
// display protocol without toolkit, then route real keyboard, task-button,
// and standard Alt+F4 close interactions through the display server.

import { expect, test, type Page } from "@playwright/test";

import {
  bootDesktop,
  clickFramebuffer,
  launchTerminal,
  submitTerminalCommand,
  waitForLine,
} from "./guest-terminal";
import {
  TASKBAR_LIGHT_FOCUSED,
  taskbarEntryPoint,
} from "./windows-ui";

test.use({ viewport: { width: 1280, height: 900 } });

const RAW_INITIAL = [0x19, 0xd3, 0xb3, 0xff] as const;
const RAW_INPUT = [0xff, 0x8a, 0x1f, 0xff] as const;
const RAW_ACCENT = [0x9b, 0x2c, 0xff, 0xff] as const;

interface ColourStats {
  count: number;
  minX: number;
  minY: number;
  maxX: number;
  maxY: number;
}

async function colourStats(
  page: Page,
  colour: readonly [number, number, number, number],
): Promise<ColourStats> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, rgba) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
      let count = 0;
      let minX = canvas.width;
      let minY = canvas.height;
      let maxX = -1;
      let maxY = -1;
      for (let y = 0; y < canvas.height; y += 1) {
        for (let x = 0; x < canvas.width; x += 1) {
          const offset = (y * canvas.width + x) * 4;
          if (
            pixels[offset] === rgba[0] &&
            pixels[offset + 1] === rgba[1] &&
            pixels[offset + 2] === rgba[2] &&
            pixels[offset + 3] === rgba[3]
          ) {
            count += 1;
            minX = Math.min(minX, x);
            minY = Math.min(minY, y);
            maxX = Math.max(maxX, x);
            maxY = Math.max(maxY, y);
          }
        }
      }
      return { count, minX, minY, maxX, maxY };
    },
    colour,
  );
}

async function pixel(page: Page, x: number, y: number): Promise<number[]> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, point) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      return Array.from(context.getImageData(point.x, point.y, 1, 1).data);
    },
    { x, y },
  );
}

async function focusedTaskEntryIndex(page: Page): Promise<number | null> {
  for (let index = 0; index < 2; index += 1) {
    const point = taskbarEntryPoint(index, 2);
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

test("a separately isolated toolkit-free client maps, receives input, repaints, minimizes, restores, and closes", async ({
  page,
  browserName,
}) => {
  test.skip(
    browserName === "webkit",
    "WebKit lacks the persistent OPFS substrate required by the supported desktop boot.",
  );

  const lines: string[] = [];
  page.on("console", (message) => lines.push(message.text()));
  page.on("pageerror", (error) => lines.push(`[pageerror] ${error.message}`));

  await bootDesktop(page, lines);
  await launchTerminal(page, lines);

  await submitTerminalCommand(
    page,
    "/bin/toolkit-free-client > /dev/console",
  );
  await waitForLine(lines, (line) =>
    line.includes("toolkit-free-client: starting raw protocol client"),
  );
  await waitForLine(lines, (line) =>
    line.includes(
      "toolkit-free-client: discovered and bound compositor/shm/xdg/seat/keyboard",
    ),
  );
  await waitForLine(lines, (line) =>
    line.includes("toolkit-free-client: presented raw 320x200 frame"),
  );

  await expect
    .poll(async () => (await colourStats(page, RAW_INITIAL)).count, {
      timeout: 10_000,
      message: "raw client did not compose its distinctive initial frame",
    })
    .toBeGreaterThan(45_000);
  const initialStats = await colourStats(page, RAW_INITIAL);
  expect(initialStats.count).toBeGreaterThan(45_000);
  expect(initialStats.maxX - initialStats.minX + 1).toBeGreaterThanOrEqual(280);
  expect(initialStats.maxX - initialStats.minX + 1).toBeLessThanOrEqual(304);
  expect(initialStats.maxY - initialStats.minY + 1).toBeGreaterThanOrEqual(170);
  expect(initialStats.maxY - initialStats.minY + 1).toBeLessThanOrEqual(184);
  expect((await colourStats(page, RAW_ACCENT)).count).toBeGreaterThan(5_000);
  const rawWorkers = await workerCount(page);
  expect(rawWorkers).toBeGreaterThan(0);

  // First-map focus is owned by the raw surface. A physical key crosses the
  // browser input driver, kernel, display server, and pmd_keyboard event path;
  // the client proves receipt by swapping to its orange second buffer.
  await page.keyboard.press("KeyR");
  await waitForLine(lines, (line) =>
    /toolkit-free-client: keyboard event key=\d+ state=1/.test(line),
  );
  await waitForLine(lines, (line) =>
    line.includes("toolkit-free-client: keyboard response frame"),
  );
  await expect
    .poll(async () => (await colourStats(page, RAW_INPUT)).count, {
      timeout: 10_000,
      message: "raw client did not present its keyboard-response buffer",
    })
    .toBeGreaterThan(45_000);
  await expect.poll(async () => (await colourStats(page, RAW_INITIAL)).count).toBe(0);

  // The newly mapped raw window is the focused task entry. Modern task buttons
  // minimize when focused and restore when minimized; the raw client remains
  // independently alive throughout both compositor transitions.
  await expect.poll(() => focusedTaskEntryIndex(page)).not.toBeNull();
  const taskIndex = await focusedTaskEntryIndex(page);
  if (taskIndex === null) throw new Error("raw client task entry disappeared");
  const taskPoint = taskbarEntryPoint(taskIndex, 2);
  await clickFramebuffer(page, taskPoint.x, taskPoint.y);
  await expect.poll(async () => (await colourStats(page, RAW_INPUT)).count).toBe(0);
  expect(await workerCount(page)).toBe(rawWorkers);
  await clickFramebuffer(page, taskPoint.x, taskPoint.y);
  await expect
    .poll(async () => (await colourStats(page, RAW_INPUT)).count)
    .toBeGreaterThan(45_000);
  expect(await workerCount(page)).toBe(rawWorkers);
  await expect.poll(() => focusedTaskEntryIndex(page)).toBe(taskIndex);

  // Closing remains a distinct lifecycle gate. Alt+F4 is routed by the shell
  // to pmd_shell_manager.close_window; the raw protocol client must receive
  // xdg_toplevel.close, destroy its objects, exit cleanly, and lose its Worker.
  await page.keyboard.press("Alt+F4");
  await waitForLine(
    lines,
    (line) => line.includes("toolkit-free-client: close requested"),
    2_000,
  );
  await waitForLine(
    lines,
    (line) => line.includes("toolkit-free-client: clean exit"),
    2_000,
  );
  await expect.poll(async () => (await colourStats(page, RAW_INPUT)).count).toBe(0);
  await expect
    .poll(() => workerCount(page), { timeout: 2_000 })
    .toBe(rawWorkers - 1);

  expect(lines.some((line) => line.includes("real kernel panic"))).toBe(false);
  expect(lines.some((line) => line.includes("user worker crashed pid="))).toBe(false);
  expect(lines.some((line) => line.startsWith("[pageerror]"))).toBe(false);
});
