import { expect, test, type Page } from "@playwright/test";

import {
  armCausalSample,
  percentile,
  readCausalSample,
  regionFingerprint,
  type CausalLatencySample,
  type Region,
} from "./causal-latency";
import {
  launcherMenuIsOpen,
  launcherMenuRegionFingerprint,
  openLauncherMenuBefore,
  selectLauncherRowBefore,
} from "./launcher-interaction";

test.use({ viewport: { width: 1280, height: 900 } });

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const TASKBAR_Y = 752;
const TASKBAR_ENTRY_SAMPLE_Y = 762;
const TASKBAR_LEFT_MARGIN = 4;
const TASKBAR_LAUNCHER_RESERVED_WIDTH = 86;
const TASKBAR_CLOCK_RESERVED_WIDTH = 68;
const TASKBAR_RIGHT_MARGIN = 4;
const TASKBAR_ENTRY_GAP = 2;
const TASKBAR_ENTRY_WIDTH = 160;
const TASKBAR_MIN_ENTRY_WIDTH = 112;
const TASKBAR_FOCUSED = [0xc2, 0xc6, 0xcf, 0xff] as const;
const TASKBAR_UNFOCUSED = [0xec, 0xee, 0xf4, 0xff] as const;
const FILES_TITLEBAR = [0x35, 0x5f, 0x84, 0xff] as const;
const TERMINAL_BACKGROUND = [0x14, 0x0e, 0x0a, 0xff] as const;
const TERMINAL_WIDTH = 720;
const TERMINAL_HEIGHT = 480;
const AUTO_LAYOUT_STEP = 32;

const SAMPLE_COUNT = 300;
const KEY_SAMPLE_COUNT = 210;
const DRAG_SAMPLE_COUNT = 60;
const FOCUS_SAMPLE_COUNT = 30;
const TARGET_WORKLOAD_MS = 10_000;
const LATENCY_BUDGET_MS = 100;

interface Point {
  readonly x: number;
  readonly y: number;
}

interface ColorBounds extends Point {
  readonly right: number;
  readonly bottom: number;
  readonly count: number;
}

interface BundledApp {
  readonly name: string;
  readonly exec: string;
  readonly launcherY: number;
  readonly started: (line: string) => boolean;
}

const BUNDLED_APPS: readonly BundledApp[] = [
  {
    name: "Terminal 1",
    exec: "/bin/term",
    launcherY: 624,
    started: (line) => line.includes("term: starting"),
  },
  {
    name: "Files",
    exec: "/bin/files",
    launcherY: 648,
    started: (line) => line.includes("files: starting"),
  },
  {
    name: "Edit",
    exec: "/bin/edit",
    launcherY: 672,
    started: (line) => line.includes("edit: starting"),
  },
  {
    name: "Settings",
    exec: "/bin/settings",
    launcherY: 696,
    started: (line) => line.includes("shell: launched /bin/settings pid="),
  },
  {
    name: "System Monitor",
    exec: "/bin/sysmon",
    launcherY: 720,
    started: (line) => line.includes("sysmon: starting"),
  },
  {
    name: "Terminal 2",
    exec: "/bin/term",
    launcherY: 624,
    started: (line) => line.includes("term: starting"),
  },
] as const;

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

async function clickFramebuffer(page: Page, point: Point): Promise<void> {
  const target = await toPagePoint(page, point);
  await page.mouse.click(target.x, target.y);
}

async function pixel(page: Page, point: Point): Promise<number[]> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, sample: Point) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      return Array.from(context.getImageData(sample.x, sample.y, 1, 1).data);
    },
    point,
  );
}

async function colorBounds(
  page: Page,
  rgba: readonly [number, number, number, number],
): Promise<ColorBounds | null> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, target) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      const bytes = context.getImageData(0, 0, canvas.width, 736).data;
      let best: ColorBounds | null = null;
      for (let y = 0; y < 736; y += 1) {
        let minX = canvas.width;
        let maxX = -1;
        let count = 0;
        for (let x = 0; x < canvas.width; x += 1) {
          const offset = (y * canvas.width + x) * 4;
          if (
            bytes[offset] !== target[0] ||
            bytes[offset + 1] !== target[1] ||
            bytes[offset + 2] !== target[2] ||
            bytes[offset + 3] !== target[3]
          ) {
            continue;
          }
          minX = Math.min(minX, x);
          maxX = Math.max(maxX, x);
          count += 1;
        }
        if (best === null || count > best.count) {
          best = { x: minX, y, right: maxX + 1, bottom: y + 1, count };
        }
      }
      return best !== null && best.count >= 300 ? best : null;
    },
    rgba,
  );
}

function terminalPromptRegionAt(origin: Point): Region {
  // Both shipped 8x14 and 8x16 fonts place the input row at local y=452 in a
  // 720x480 Terminal. A 24-pixel band around it captures prompt, input, and
  // cursor while excluding taskbar pixels and every other app's content.
  return {
    x: origin.x + 4,
    y: origin.y + TERMINAL_HEIGHT - 32,
    width: TERMINAL_WIDTH - 8,
    height: 24,
  };
}

function taskbarEntryWidth(entryCount: number): number {
  const available =
    FRAMEBUFFER_WIDTH -
    TASKBAR_LEFT_MARGIN -
    TASKBAR_LAUNCHER_RESERVED_WIDTH -
    TASKBAR_CLOCK_RESERVED_WIDTH -
    TASKBAR_RIGHT_MARGIN;
  const gaps = TASKBAR_ENTRY_GAP * Math.max(0, entryCount - 1);
  return Math.max(
    TASKBAR_MIN_ENTRY_WIDTH,
    Math.min(TASKBAR_ENTRY_WIDTH, Math.floor((available - gaps) / entryCount)),
  );
}

function taskbarEntryPoint(index: number, entryCount: number): Point {
  const width = taskbarEntryWidth(entryCount);
  return {
    x:
      TASKBAR_LEFT_MARGIN +
      TASKBAR_LAUNCHER_RESERVED_WIDTH +
      index * (width + TASKBAR_ENTRY_GAP) +
      30,
    y: TASKBAR_ENTRY_SAMPLE_Y,
  };
}

function taskbarEntryClickPoint(index: number, entryCount: number): Point {
  const width = taskbarEntryWidth(entryCount);
  const labelWidth = width - 3 * 20 - 3 * TASKBAR_ENTRY_GAP;
  return {
    x:
      TASKBAR_LEFT_MARGIN +
      TASKBAR_LAUNCHER_RESERVED_WIDTH +
      index * (width + TASKBAR_ENTRY_GAP) +
      Math.max(12, Math.floor(labelWidth / 2)),
    y: TASKBAR_Y,
  };
}

async function waitForLineAfter(
  lines: readonly string[],
  startIndex: number,
  predicate: (line: string) => boolean,
  label: string,
  timeout = 10_000,
): Promise<string> {
  await expect
    .poll(() => lines.slice(startIndex).find(predicate) ?? null, {
      timeout,
      message: `expected ${label}; observed:\n${lines.slice(startIndex).join("\n")}`,
    })
    .not.toBeNull();
  return lines.slice(startIndex).find(predicate)!;
}

async function waitForFocusedTaskEntry(
  page: Page,
  index: number,
  entryCount: number,
  label: string,
  afterSequence = -1,
): Promise<void> {
  await expect
    .poll(async () => {
      const sequence = Number(
        (await page
          .locator("#pmos-fb")
          .getAttribute("data-pmos-frame-sequence")) ?? "0",
      );
      if (sequence <= afterSequence) return false;
      const entryPixels = await Promise.all(
        Array.from({ length: entryCount }, (_, entryIndex) =>
          pixel(page, taskbarEntryPoint(entryIndex, entryCount)),
        ),
      );
      return entryPixels.every((entryPixel, entryIndex) => {
        const expected =
          entryIndex === index ? TASKBAR_FOCUSED : TASKBAR_UNFOCUSED;
        return entryPixel.every(
          (channel, channelIndex) => channel === expected[channelIndex],
        );
      });
    }, {
      timeout: 10_000,
      message:
        `${label} did not map as focused task entry ${index} in the ` +
        `${entryCount}-entry post-launch layout`,
    })
    .toBe(true);
}

async function launchBundledApp(
  page: Page,
  lines: readonly string[],
  app: BundledApp,
  appIndex: number,
): Promise<void> {
  const logStart = lines.length;
  await expect
    .poll(() => launcherMenuIsOpen(page), {
      timeout: 5_000,
      message: `${app.name} launch did not begin with the launcher closed`,
    })
    .toBe(false);
  const closedLauncherFingerprint = await launcherMenuRegionFingerprint(page);
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  const menuCloseFrame = await selectLauncherRowBefore(
    page,
    100,
    app.launcherY,
    Date.now() + 5_000,
    closedLauncherFingerprint,
  );
  await waitForLineAfter(
    lines,
    logStart,
    (line) => line.includes(`shell: launched ${app.exec} pid=`),
    `${app.name} launcher acknowledgement`,
  );
  await waitForLineAfter(
    lines,
    logStart,
    app.started,
    `${app.name} process startup`,
  );
  // The shell itself is entry zero. Requiring the newly appended entry's
  // focused palette proves this exact process completed its display handshake
  // and mapped a graphical toplevel; a spawn log alone is insufficient.
  await waitForFocusedTaskEntry(
    page,
    appIndex + 1,
    appIndex + 2,
    app.name,
    menuCloseFrame,
  );
}

async function focusTaskEntry(
  page: Page,
  index: number,
  entryCount: number,
  label: string,
): Promise<void> {
  await clickFramebuffer(page, taskbarEntryClickPoint(index, entryCount));
  await waitForFocusedTaskEntry(page, index, entryCount, label);
}

async function warmTerminal(page: Page, region: Region): Promise<void> {
  const empty = await regionFingerprint(page, region);
  await page.keyboard.press("a");
  await expect
    .poll(() => regionFingerprint(page, region), {
      timeout: 3_000,
      message: "Terminal did not repaint the warm-up character",
    })
    .not.toBe(empty);
  const typed = await regionFingerprint(page, region);
  await page.keyboard.press("Backspace");
  await expect
    .poll(() => regionFingerprint(page, region), {
      timeout: 3_000,
      message: "Terminal did not repaint the warm-up Backspace",
    })
    .not.toBe(typed);
  expect(await regionFingerprint(page, region)).toBe(empty);
}

function sampleDeadline(workloadStartedAt: number, sampleId: number): number {
  return workloadStartedAt + (sampleId * TARGET_WORKLOAD_MS) / SAMPLE_COUNT;
}

async function measureKey(
  page: Page,
  id: number,
  workloadStartedAt: number,
  region: Region,
  pressed: boolean,
  consoleLines: readonly string[],
): Promise<CausalLatencySample> {
  const key = pressed ? "Backspace" : "a";
  const code = pressed ? "Backspace" : "KeyA";
  const before = await regionFingerprint(page, region);
  await armCausalSample(page, {
    id,
    kind: "key",
    input: "keydown",
    code,
    notBefore: sampleDeadline(workloadStartedAt, id),
    evidence: { kind: "fingerprint", region, before },
  });
  await page.keyboard.down(key);
  try {
    return await readCausalSample(page, id, consoleLines);
  } finally {
    await page.keyboard.up(key);
  }
}

async function measureFocusClick(
  page: Page,
  id: number,
  workloadStartedAt: number,
  targetIndex: number,
  entryCount: number,
  consoleLines: readonly string[],
): Promise<CausalLatencySample> {
  const evidencePoint = taskbarEntryPoint(targetIndex, entryCount);
  expect(await pixel(page, evidencePoint)).toEqual([...TASKBAR_UNFOCUSED]);
  await armCausalSample(page, {
    id,
    kind: "focus",
    input: "pointerdown",
    notBefore: sampleDeadline(workloadStartedAt, id),
    evidence: {
      kind: "pixel",
      point: evidencePoint,
      expected: TASKBAR_FOCUSED,
    },
  });
  await clickFramebuffer(page, taskbarEntryClickPoint(targetIndex, entryCount));
  return readCausalSample(page, id, consoleLines);
}

async function measureDragMove(
  page: Page,
  id: number,
  workloadStartedAt: number,
  pointer: Point,
  evidencePoint: Point,
  consoleLines: readonly string[],
): Promise<CausalLatencySample> {
  expect(await pixel(page, evidencePoint)).not.toEqual([...FILES_TITLEBAR]);
  await armCausalSample(page, {
    id,
    kind: "drag",
    input: "pointermove",
    notBefore: sampleDeadline(workloadStartedAt, id),
    evidence: {
      kind: "pixel",
      point: evidencePoint,
      expected: FILES_TITLEBAR,
    },
  });
  const target = await toPagePoint(page, pointer);
  await page.mouse.move(target.x, target.y);
  return readCausalSample(page, id, consoleLines);
}

function metricSummary(samples: readonly CausalLatencySample[]): string {
  const total = samples.map((sample) => sample.totalMs);
  const inputToMain = samples.map((sample) => sample.inputToMainMs);
  const mainPaint = samples.map((sample) => sample.mainPaintMs);
  const presentations = samples.map((sample) => sample.presentations);
  return [
    `count=${samples.length}`,
    `p50_ms=${percentile(total, 0.5).toFixed(1)}`,
    `p95_ms=${percentile(total, 0.95).toFixed(1)}`,
    `p99_ms=${percentile(total, 0.99).toFixed(1)}`,
    `input_to_main_p50_ms=${percentile(inputToMain, 0.5).toFixed(1)}`,
    `input_to_main_p95_ms=${percentile(inputToMain, 0.95).toFixed(1)}`,
    `main_paint_p50_ms=${percentile(mainPaint, 0.5).toFixed(1)}`,
    `main_paint_p95_ms=${percentile(mainPaint, 0.95).toFixed(1)}`,
    `presentations_p50=${percentile(presentations, 0.5)}`,
    `presentations_p95=${percentile(presentations, 0.95)}`,
  ].join(" ");
}

test("typical six-app desktop keeps 300 causal interactions below 100 ms p95", async ({
  page,
  browserName,
}) => {
  test.skip(
    browserName === "webkit",
    "The persistent PMos substrate is unsupported by Playwright WebKit on Linux.",
  );
  test.setTimeout(120_000);

  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));
  page.on("pageerror", (error) =>
    consoleLines.push(`[pageerror] ${error.message}`),
  );

  await page.goto("/index.html");
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 15_000,
  });
  await waitForLineAfter(
    consoleLines,
    0,
    (line) => line.includes("shell: connected to /run/display"),
    "desktop shell display connection",
  );
  await waitForLineAfter(
    consoleLines,
    0,
    (line) =>
      line.includes("shell: loaded 5 applications from /usr/share/applications"),
    "five-entry launcher catalog",
  );
  const workersBefore = Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );

  for (let index = 0; index < BUNDLED_APPS.length; index += 1) {
    await launchBundledApp(
      page,
      consoleLines,
      BUNDLED_APPS[index]!,
      index,
    );
  }

  const entryCount = BUNDLED_APPS.length + 1;
  for (let index = 0; index < entryCount; index += 1) {
    const taskPixel = await pixel(page, taskbarEntryPoint(index, entryCount));
    expect(
      [TASKBAR_FOCUSED, TASKBAR_UNFOCUSED].some((palette) =>
        taskPixel.every((channel, offset) => channel === palette[offset]),
      ),
      `task entry ${index} was not visibly mapped: ${taskPixel.join(",")}`,
    ).toBe(true);
  }
  await expect
    .poll(
      async () =>
        Number(
          (await page
            .locator("body")
            .getAttribute("data-pmos-live-workers")) ?? "0",
        ),
      {
        timeout: 10_000,
        message: "six apps and both Terminal shell children did not remain live",
      },
    )
    .toBeGreaterThanOrEqual(workersBefore + 8);

  // Capture each Terminal's prompt only while that exact task entry is raised,
  // then prove the region responds to input and returns to its empty baseline.
  const terminalEntryIndexes = [1, 6] as const;
  const terminalOrigins = [
    { x: 0, y: 0 },
    {
      x: AUTO_LAYOUT_STEP * (BUNDLED_APPS.length - 1),
      y: AUTO_LAYOUT_STEP * (BUNDLED_APPS.length - 1),
    },
  ] as const;
  const promptRegions: Region[] = [];
  for (let terminal = 0; terminal < terminalEntryIndexes.length; terminal += 1) {
    const index = terminalEntryIndexes[terminal]!;
    await focusTaskEntry(page, index, entryCount, `Terminal entry ${index}`);
    const origin = terminalOrigins[terminal]!;
    const backgroundProbe = {
      x: origin.x + TERMINAL_WIDTH - 12,
      y: origin.y + TERMINAL_HEIGHT - 40,
    };
    await expect
      .poll(() => pixel(page, backgroundProbe), {
        timeout: 5_000,
        message: `Terminal entry ${index} did not expose its mapped surface`,
      })
      .toEqual([...TERMINAL_BACKGROUND]);
    const region = terminalPromptRegionAt(origin);
    await warmTerminal(page, region);
    promptRegions.push(region);
  }

  // Files is entry two by deterministic launch order. Its distinctive blue
  // client-painted titlebar gives every drag step exact newly occupied pixels.
  await focusTaskEntry(page, 2, entryCount, "Files");
  await expect
    .poll(() => colorBounds(page, FILES_TITLEBAR), {
      timeout: 5_000,
      message: "Files titlebar did not become fully visible",
    })
    .not.toBeNull();
  const initialFiles = await colorBounds(page, FILES_TITLEBAR);
  if (initialFiles === null) throw new Error("Files titlebar vanished");
  const dragStart = { x: initialFiles.x + 300, y: initialFiles.y + 10 };
  const dragStartPage = await toPagePoint(page, dragStart);
  await page.mouse.move(dragStartPage.x, dragStartPage.y);
  const dragLogStart = consoleLines.length;
  await page.mouse.down();
  await waitForLineAfter(
    consoleLines,
    dragLogStart,
    (line) => /files: move requested serial=\d+/.test(line),
    "Files interactive-move request",
    3_000,
  );
  let dragPointer = { x: dragStart.x + 16, y: dragStart.y };
  const warmDragPage = await toPagePoint(page, dragPointer);
  await page.mouse.move(warmDragPage.x, warmDragPage.y, { steps: 16 });
  await expect
    .poll(async () => {
      const bounds = await colorBounds(page, FILES_TITLEBAR);
      return bounds?.x ?? null;
    }, {
      timeout: 3_000,
      message: "display server did not present the complete Files drag warm-up",
    })
    .toBe(initialFiles.x + 16);
  const warmFiles = await colorBounds(page, FILES_TITLEBAR);
  if (warmFiles === null) throw new Error("Files titlebar vanished during drag warm-up");

  const samples: CausalLatencySample[] = [];
  const workloadStartedAt = await page.evaluate(() => performance.now());
  let sampleId = 1;
  let filesX = warmFiles.x;
  const filesRight = warmFiles.right;

  // Sixty real drag motions alternate by eight framebuffer pixels. The sample
  // point is always in the newly occupied titlebar stripe, so a Sysmon refresh,
  // shell clock paint, or any other unrelated frame cannot complete the sample.
  for (let index = 0; index < DRAG_SAMPLE_COUNT; index += 1) {
    const delta = index % 2 === 0 ? 8 : -8;
    const evidencePoint = {
      x: delta > 0 ? filesRight + 4 : filesX - 4,
      y: warmFiles.y + 10,
    };
    dragPointer = { x: dragPointer.x + delta, y: dragPointer.y };
    samples.push(
      await measureDragMove(
        page,
        sampleId,
        workloadStartedAt,
        dragPointer,
        evidencePoint,
        consoleLines,
      ),
    );
    filesX += delta;
    sampleId += 1;
  }
  const dragReleaseStart = consoleLines.length;
  await page.mouse.up();
  await waitForLineAfter(
    consoleLines,
    dragReleaseStart,
    (line) => line.includes("display-server: drag completed"),
    "Files drag completion",
    3_000,
  );

  await focusTaskEntry(page, terminalEntryIndexes[1], entryCount, "Terminal 2");
  let focusedTerminal = 1;
  const terminalHasCharacter = [false, false];

  // Thirty deterministic rounds each perform seven keystrokes and one focus
  // click. Together with the drag phase this is exactly 210 key + 90 pointer
  // interactions (70/30), with both Terminal instances receiving real input.
  for (let round = 0; round < FOCUS_SAMPLE_COUNT; round += 1) {
    for (let keyIndex = 0; keyIndex < 7; keyIndex += 1) {
      const pressed = terminalHasCharacter[focusedTerminal]!;
      samples.push(
        await measureKey(
          page,
          sampleId,
          workloadStartedAt,
          promptRegions[focusedTerminal]!,
          pressed,
          consoleLines,
        ),
      );
      terminalHasCharacter[focusedTerminal] = !pressed;
      sampleId += 1;
    }

    const targetTerminal = focusedTerminal === 0 ? 1 : 0;
    samples.push(
      await measureFocusClick(
        page,
        sampleId,
        workloadStartedAt,
        terminalEntryIndexes[targetTerminal]!,
        entryCount,
        consoleLines,
      ),
    );
    focusedTerminal = targetTerminal;
    sampleId += 1;
  }

  const workloadFinishedAt = await page.evaluate(() => performance.now());
  const workloadMs = workloadFinishedAt - workloadStartedAt;
  const keySamples = samples.filter((sample) => sample.kind === "key");
  const dragSamples = samples.filter((sample) => sample.kind === "drag");
  const focusSamples = samples.filter((sample) => sample.kind === "focus");
  expect(samples).toHaveLength(SAMPLE_COUNT);
  expect(keySamples).toHaveLength(KEY_SAMPLE_COUNT);
  expect(dragSamples).toHaveLength(DRAG_SAMPLE_COUNT);
  expect(focusSamples).toHaveLength(FOCUS_SAMPLE_COUNT);
  expect(workloadMs).toBeGreaterThanOrEqual(TARGET_WORKLOAD_MS);
  await expect
    .poll(
      async () =>
        Number(
          (await page
            .locator("body")
            .getAttribute("data-pmos-live-workers")) ?? "0",
        ),
      {
        timeout: 3_000,
        message: "the six apps and both Terminal children did not stay live",
      },
    )
    .toBeGreaterThanOrEqual(workersBefore + 8);
  await waitForFocusedTaskEntry(
    page,
    terminalEntryIndexes[focusedTerminal]!,
    entryCount,
    "post-workload six-app desktop",
  );

  const totals = samples.map((sample) => sample.totalMs);
  const p50 = percentile(totals, 0.5);
  const p95 = percentile(totals, 0.95);
  const p99 = percentile(totals, 0.99);
  const keyP95 = percentile(
    keySamples.map((sample) => sample.totalMs),
    0.95,
  );
  const dragP95 = percentile(
    dragSamples.map((sample) => sample.totalMs),
    0.95,
  );
  const focusP95 = percentile(
    focusSamples.map((sample) => sample.totalMs),
    0.95,
  );
  console.log(
    `[typical-desktop-latency] engine=${browserName} apps=6 interactions=${samples.length} ` +
      `keys=${keySamples.length} drag_moves=${dragSamples.length} ` +
      `focus_clicks=${focusSamples.length} workload_ms=${workloadMs.toFixed(1)} ` +
      `p50_ms=${p50.toFixed(1)} p95_ms=${p95.toFixed(1)} p99_ms=${p99.toFixed(1)}`,
  );
  console.log(`[typical-desktop-latency:key] ${metricSummary(keySamples)}`);
  console.log(`[typical-desktop-latency:drag] ${metricSummary(dragSamples)}`);
  console.log(`[typical-desktop-latency:focus] ${metricSummary(focusSamples)}`);

  expect(p95, "SC-003 six-app input-to-pixel p95").toBeLessThan(
    LATENCY_BUDGET_MS,
  );
  expect(keyP95, "SC-003 six-app keystroke p95").toBeLessThan(
    LATENCY_BUDGET_MS,
  );
  expect(dragP95, "SC-003 six-app window-drag p95").toBeLessThan(
    LATENCY_BUDGET_MS,
  );
  expect(focusP95, "SC-003 six-app pointer-click p95").toBeLessThan(
    LATENCY_BUDGET_MS,
  );
  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(
    false,
  );
  expect(
    consoleLines.some((line) => line.includes("user worker crashed pid=")),
  ).toBe(false);
  expect(consoleLines.some((line) => line.startsWith("[pageerror]"))).toBe(
    false,
  );
});
