// T197 — real Settings timezone workflow.
//
// A Terminal spawned before the edit reports its own UTC environment through
// the guest /dev/console. Settings then atomically selects America/New_York;
// the shell repaints its framebuffer-owned clock, the existing Terminal keeps
// UTC, and a newly spawned Terminal + /bin/sh receive the new TZ. A full page
// reload proves both the clock choice and subsequent spawn environment came
// from the persistent VFS preference rather than live test state.

import { expect, test, type Page } from "@playwright/test";
import {
  TASKBAR_LIGHT_FOCUSED,
  taskbarEntryPoint,
  waitForActiveWindowBounds,
} from "./windows-ui";

test.use({ viewport: { width: 1280, height: 900 } });
test.setTimeout(60_000);

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const TASKBAR_Y = 752;
// `Color::rgb(0x0a, 0x0e, 0x14)` is stored in the guest's ARGB8888 buffer;
// the browser's ImageData view presents those bytes as RGBA in B/G/R order.
const TERMINAL_BACKGROUND = [0x14, 0x0e, 0x0a, 0xff] as const;

// The taskbar clock is right-aligned six pixels from the 1024px edge. Both
// `HH:MM UTC` and `HH:MM EDT/EST` are nine 6px cells, so their suffix begins at
// this stable framebuffer coordinate. These masks are the toolkit's checked-in
// 5x7 glyphs; matching them observes presented guest pixels, not DOM state.
const CLOCK_SUFFIX_X = 1000;
const CLOCK_SUFFIX_Y = 748;
const CLOCK_FOREGROUND = [0x1b, 0x1b, 0x1b] as const;
const CLOCK_GLYPHS: Readonly<Record<string, readonly number[]>> = {
  C: [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
  D: [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
  E: [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
  S: [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
  T: [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
  U: [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
};

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
      { timeout: 5_000 },
    )
    .toBeGreaterThan(before);
}

async function framebufferPixel(
  page: Page,
  x: number,
  y: number,
): Promise<readonly number[]> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, point: { x: number; y: number }) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      return Array.from(context.getImageData(point.x, point.y, 1, 1).data);
    },
    { x, y },
  );
}

async function presentedClockSuffix(page: Page): Promise<string | null> {
  const foreground = await page.locator("#pmos-fb").evaluate(
    (
      canvas: HTMLCanvasElement,
      region: {
        x: number;
        y: number;
        rgb: readonly [number, number, number];
      },
    ) => {
      const context = canvas.getContext("2d");
      if (context === null) return null;
      const image = context.getImageData(region.x, region.y, 17, 7);
      const mask: boolean[][] = [];
      for (let row = 0; row < 7; row += 1) {
        const outputRow: boolean[] = [];
        for (let column = 0; column < 17; column += 1) {
          const offset = (row * 17 + column) * 4;
          outputRow.push(
            image.data[offset] === region.rgb[0] &&
              image.data[offset + 1] === region.rgb[1] &&
              image.data[offset + 2] === region.rgb[2] &&
              image.data[offset + 3] === 0xff,
          );
        }
        mask.push(outputRow);
      }
      return mask;
    },
    { x: CLOCK_SUFFIX_X, y: CLOCK_SUFFIX_Y, rgb: CLOCK_FOREGROUND },
  );
  if (foreground === null) return null;

  for (const candidate of ["UTC", "EDT", "EST"] as const) {
    let matches = true;
    for (let row = 0; row < 7 && matches; row += 1) {
      for (let index = 0; index < candidate.length && matches; index += 1) {
        const glyph = CLOCK_GLYPHS[candidate[index]];
        for (let column = 0; column < 5; column += 1) {
          const expected = ((glyph[row] >> (4 - column)) & 1) === 1;
          if (foreground[row][index * 6 + column] !== expected) {
            matches = false;
            break;
          }
        }
        if (index < candidate.length - 1 && foreground[row][index * 6 + 5]) {
          matches = false;
        }
      }
    }
    if (matches) return candidate;
  }
  return null;
}

function expectedNewYorkSuffix(now = new Date()): "EDT" | "EST" {
  const year = now.getUTCFullYear();
  const marchFirst = new Date(Date.UTC(year, 2, 1)).getUTCDay();
  const secondSundayInMarch = 1 + ((7 - marchFirst) % 7) + 7;
  const novemberFirst = new Date(Date.UTC(year, 10, 1)).getUTCDay();
  const firstSundayInNovember = 1 + ((7 - novemberFirst) % 7);
  const dstStart = Date.UTC(year, 2, secondSundayInMarch, 7);
  const dstEnd = Date.UTC(year, 10, firstSundayInNovember, 6);
  return now.getTime() >= dstStart && now.getTime() < dstEnd ? "EDT" : "EST";
}

async function clockMatchesNewYork(page: Page): Promise<boolean> {
  return (await presentedClockSuffix(page)) === expectedNewYorkSuffix();
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

async function waitForLineAfter(
  lines: readonly string[],
  startIndex: number,
  predicate: (line: string) => boolean,
  timeout = 10_000,
): Promise<string> {
  await expect
    .poll(() => lines.slice(startIndex).find(predicate) ?? null, {
      timeout,
      message: `expected OS console line after ${startIndex}; observed:\n${lines.join("\n")}`,
    })
    .not.toBeNull();
  return lines.slice(startIndex).find(predicate)!;
}

async function assertSpawnTimezone(
  page: Page,
  consoleLines: string[],
  expectedTimezone: string,
): Promise<void> {
  const outputStart = consoleLines.length;
  // `env` is a shell builtin and prints the environment owned by this exact
  // isolated /bin/sh. Send `>` as a physical modifier chord because PMos
  // consumes HID-style key transitions rather than browser text values.
  await page.keyboard.type("env ");
  await page.keyboard.press("Shift+Period");
  await page.keyboard.type(" /dev/console");
  await page.keyboard.press("Enter");
  await expect
    .poll(
      () =>
        consoleLines
          .slice(outputStart)
          .some((line) => line === `[real-kernel] TZ=${expectedTimezone}`),
      {
        timeout: 10_000,
        message: `expected new guest TZ=${expectedTimezone} output; observed:\n${consoleLines.join("\n")}`,
      },
    )
    .toBe(true);
}

async function waitForTerminalPaint(
  page: Page,
  x: number,
  y: number,
): Promise<void> {
  await expect
    .poll(() => framebufferPixel(page, x, y), { timeout: 10_000 })
    .toEqual(TERMINAL_BACKGROUND);
}

async function focusMappedTerminal(
  page: Page,
  contentX: number,
  contentY: number,
  taskbarEntryIndex: number,
  taskbarEntryCount: number,
): Promise<void> {
  // Mapping and focus are separate display-server events. Require the shell's
  // focused-entry repaint before sending keyboard input; retrying the click
  // also handles Firefox delivering a button before its preceding motion.
  await expect
    .poll(
      async () => {
        const task = taskbarEntryPoint(taskbarEntryIndex, taskbarEntryCount);
        const current = await framebufferPixel(page, task.x, task.y);
        if (
          current.every(
            (channel, index) => channel === TASKBAR_LIGHT_FOCUSED[index],
          )
        ) {
          return current;
        }
        await clickFramebuffer(page, contentX, contentY);
        return framebufferPixel(page, task.x, task.y);
      },
      { timeout: 5_000 },
    )
    .toEqual([...TASKBAR_LIGHT_FOCUSED]);
  await waitForActiveWindowBounds(page, {
    expectedX: taskbarEntryIndex * 32,
    expectedY: taskbarEntryIndex * 32,
    expectedWidth: 720,
    message: `Terminal task ${taskbarEntryIndex} did not present its focused frame`,
  });
}

test("Settings applies timezone to new processes only and persists it", async ({
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
    line.includes("shell: loaded 5 applications from /usr/share/applications"),
  );
  await expect.poll(() => presentedClockSuffix(page)).toBe("UTC");

  // Start the baseline Terminal and wait for its first real surface commit;
  // `term: starting` alone precedes display mapping and keyboard focus.
  await clickAndWaitForFrame(page, 40, TASKBAR_Y);
  await clickFramebuffer(page, 100, 624);
  await waitForLine(consoleLines, (line) => line.includes("term: starting"));
  await waitForTerminalPaint(page, 700, 450);
  await focusMappedTerminal(page, 700, 450, 0, 1);
  await assertSpawnTimezone(page, consoleLines, "UTC");

  // Open Settings, select the fourth tab (Timezone), and Apply once. A fresh
  // profile starts at UTC, so the finite picker advances to America/New_York.
  await clickAndWaitForFrame(page, 40, TASKBAR_Y);
  await clickFramebuffer(page, 100, 696);
  await waitForLine(consoleLines, (line) =>
    /shell: launched \/bin\/settings pid=\d+/.test(line),
  );
  const settingsOrigin = await waitForActiveWindowBounds(page, {
    expectedWidth: 560,
  });
  await clickAndWaitForFrame(
    page,
    settingsOrigin.x + 325,
    settingsOrigin.y + 27,
  );
  await clickAndWaitForFrame(
    page,
    settingsOrigin.x + 500,
    settingsOrigin.y + 331,
  );

  // The taskbar is ordinary shell-owned framebuffer output. Its IANA-aware
  // clock must repaint to the New York abbreviation on the bounded VFS poll.
  await expect
    .poll(() => clockMatchesNewYork(page), { timeout: 5_000 })
    .toBe(true);

  // Focus the exact pre-existing Terminal taskbar entry and prove its child
  // shell retained the environment captured before the Settings edit.
  const terminalTask = taskbarEntryPoint(0, 2);
  const terminalPalette = await framebufferPixel(
    page,
    terminalTask.x,
    terminalTask.y,
  );
  if (
    !terminalPalette.every(
      (channel, index) => channel === TASKBAR_LIGHT_FOCUSED[index],
    )
  ) {
    await clickFramebuffer(page, terminalTask.x, terminalTask.y);
  }
  await expect
    .poll(() => framebufferPixel(page, terminalTask.x, terminalTask.y), {
      timeout: 5_000,
    })
    .toEqual([...TASKBAR_LIGHT_FOCUSED]);
  await waitForActiveWindowBounds(page, {
    expectedX: 0,
    expectedY: 0,
    expectedWidth: 720,
    message: "pre-existing Terminal did not complete its focused frame",
  });
  // The shell's focus event and the compositor's raised scene travel through
  // separate framebuffer commits. This overlap point is light while Settings
  // is on top and dark only once the original Terminal is visibly frontmost.
  await waitForTerminalPaint(page, 400, 300);
  await assertSpawnTimezone(page, consoleLines, "UTC");

  // A second Terminal is spawned through the shell after the edit. Its own
  // inherited TZ is forwarded to its separately isolated persistent /bin/sh.
  const secondTerminalLogStart = consoleLines.length;
  const terminalStarts = consoleLines.filter((line) =>
    line.includes("term: starting"),
  ).length;
  await clickAndWaitForFrame(page, 40, TASKBAR_Y);
  await clickFramebuffer(page, 100, 624);
  await expect
    .poll(
      () =>
        consoleLines.filter((line) => line.includes("term: starting")).length,
      { timeout: 5_000 },
    )
    .toBeGreaterThan(terminalStarts);
  // This point is inside the third cascaded app surface but outside both the
  // first Terminal and Settings, so it cannot be satisfied by stale pixels.
  await waitForTerminalPaint(page, 760, 500);
  await focusMappedTerminal(page, 760, 500, 2, 3);
  await assertSpawnTimezone(page, consoleLines, "America/New_York");

  await waitForLineAfter(consoleLines, secondTerminalLogStart, (line) =>
    /shell: session durable revision=\d+ apps=3 windows=3 bytes=\d+ digest=[0-9a-f]{16}$/.test(
      line,
    ),
  );

  // Reboot the complete browser OS. OPFS must restore the clock preference,
  // and a process spawned by the new shell must receive the same validated TZ.
  const reloadStart = consoleLines.length;
  await page.reload();
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 12_000,
  });
  await expect
    .poll(
      () =>
        consoleLines
          .slice(reloadStart)
          .some((line) =>
            line.includes(
              "shell: loaded 5 applications from /usr/share/applications",
            ),
          ),
      { timeout: 10_000 },
    )
    .toBe(true);
  await expect
    .poll(() => clockMatchesNewYork(page), { timeout: 5_000 })
    .toBe(true);

  await clickAndWaitForFrame(page, 40, TASKBAR_Y);
  await clickFramebuffer(page, 100, 624);
  await waitForTerminalPaint(page, 700, 450);
  await focusMappedTerminal(page, 700, 450, 3, 4);
  await assertSpawnTimezone(page, consoleLines, "America/New_York");

  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(
    false,
  );
  expect(consoleLines.filter((line) => line.startsWith("[pageerror]"))).toEqual(
    [],
  );
});
