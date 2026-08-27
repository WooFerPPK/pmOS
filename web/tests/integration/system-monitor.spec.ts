import { expect, test, type Page } from "@playwright/test";

import { acknowledgeTerminalFocus } from "./guest-terminal";
import {
  clickFramebuffer,
  launcherMenuIsOpen,
  launcherMenuRegionFingerprint,
  openLauncherMenuBefore,
  selectLauncherRowBefore,
} from "./launcher-interaction";
import {
  DARK_ACTIVE_BORDER,
  DARK_TITLEBAR,
  LIGHT_ACTIVE_BORDER,
  LIGHT_TITLEBAR,
  TASKBAR_LIGHT_FOCUSED,
  TASKBAR_LIGHT_UNFOCUSED,
  taskbarEntryPoint,
  waitForActiveWindowBounds,
} from "./windows-ui";

test.use({ viewport: { width: 1280, height: 900 } });

const TERMINAL_BACKGROUND = [0x14, 0x0e, 0x0a] as const;
const ACTIVE_TITLEBAR_PALETTES = [LIGHT_TITLEBAR, DARK_TITLEBAR] as const;
const SELECTED_ROW_PALETTES = [
  LIGHT_ACTIVE_BORDER,
  DARK_ACTIVE_BORDER,
] as const;
const TASKBAR_REGION = { x: 90, y: 738, width: 862, height: 28 } as const;

interface Point {
  readonly x: number;
  readonly y: number;
}

interface Region extends Point {
  readonly width: number;
  readonly height: number;
}

interface BundledApp {
  readonly name: string;
  readonly exec: string;
  readonly launcherY: number;
  readonly started: (line: string) => boolean;
}

const BUNDLED_APPS: readonly BundledApp[] = [
  {
    name: "Terminal",
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
] as const;

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

async function regionFingerprint(page: Page, region: Region): Promise<number> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, sample: Region) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      const bytes = context.getImageData(
        sample.x,
        sample.y,
        sample.width,
        sample.height,
      ).data;
      let hash = 0x811c9dc5;
      for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193);
      return hash >>> 0;
    },
    region,
  );
}

async function workerCount(page: Page): Promise<number> {
  return Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
}

async function waitForLineAfter(
  lines: readonly string[],
  start: number,
  predicate: (line: string) => boolean,
  label: string,
  timeout = 10_000,
): Promise<string> {
  await expect
    .poll(() => lines.slice(start).find(predicate) ?? null, {
      timeout,
      message: `expected ${label}; observed:\n${lines.slice(start).join("\n")}`,
    })
    .not.toBeNull();
  return lines.slice(start).find(predicate)!;
}

function taskbarEntryClickPoint(index: number, entryCount: number): Point {
  return taskbarEntryPoint(index, entryCount);
}

async function waitForFocusedTaskEntry(
  page: Page,
  index: number,
  entryCount: number,
  label: string,
  afterSequence = -1,
): Promise<void> {
  await expect
    .poll(
      async () => {
        const sequence = Number(
          (await page
            .locator("#pmos-fb")
            .getAttribute("data-pmos-frame-sequence")) ?? "0",
        );
        if (sequence <= afterSequence) return false;
        const entries = await Promise.all(
          Array.from({ length: entryCount }, (_, entryIndex) =>
            pixel(page, taskbarEntryPoint(entryIndex, entryCount)),
          ),
        );
        return entries.every((actual, entryIndex) => {
          const expected =
            entryIndex === index
              ? TASKBAR_LIGHT_FOCUSED
              : TASKBAR_LIGHT_UNFOCUSED;
          return actual.every(
            (channel, channelIndex) => channel === expected[channelIndex],
          );
        });
      },
      {
        timeout: 10_000,
        message: `${label} did not become focused in the ${entryCount}-entry taskbar`,
      },
    )
    .toBe(true);
}

async function launchBundledApp(
  page: Page,
  lines: readonly string[],
  app: BundledApp,
  appIndex: number,
): Promise<number> {
  const logStart = lines.length;
  await expect
    .poll(() => launcherMenuIsOpen(page), {
      timeout: 5_000,
      message: `${app.name} launch did not begin with the launcher closed`,
    })
    .toBe(false);
  const closedFingerprint = await launcherMenuRegionFingerprint(page);
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  const closeFrame = await selectLauncherRowBefore(
    page,
    100,
    app.launcherY,
    Date.now() + 5_000,
    closedFingerprint,
  );
  const launchLine = await waitForLineAfter(
    lines,
    logStart,
    (line) => line.includes(`shell: launched ${app.exec} pid=`),
    `${app.name} launcher PID`,
  );
  await waitForLineAfter(
    lines,
    logStart,
    app.started,
    `${app.name} process startup`,
  );
  await waitForFocusedTaskEntry(
    page,
    appIndex,
    appIndex + 1,
    app.name,
    closeFrame,
  );
  const match = launchLine.match(/ pid=(\d+)$/);
  expect(match, launchLine).not.toBeNull();
  const pid = Number(match![1]);
  expect(pid, launchLine).toBeGreaterThan(0);
  return pid;
}

async function focusTaskEntry(
  page: Page,
  index: number,
  entryCount: number,
  label: string,
): Promise<void> {
  const canvas = page.locator("#pmos-fb");
  const sequence = Number(
    (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
  );
  const point = taskbarEntryClickPoint(index, entryCount);
  const current = await pixel(page, point);
  const alreadyFocused = current.every(
    (channel, channelIndex) =>
      channel === TASKBAR_LIGHT_FOCUSED[channelIndex],
  );
  if (alreadyFocused) return;
  await clickFramebuffer(page, point.x, point.y);
  await waitForFocusedTaskEntry(page, index, entryCount, label, sequence);
}

async function findSolidRun(
  page: Page,
  rgb: readonly [number, number, number],
  minimumRun: number,
): Promise<Point | null> {
  return page.locator("#pmos-fb").evaluate(
    (
      canvas: HTMLCanvasElement,
      target: {
        rgb: readonly [number, number, number];
        minimumRun: number;
      },
    ) => {
      const context = canvas.getContext("2d");
      if (context === null) return null;
      const height = Math.min(canvas.height, 736);
      const bytes = context.getImageData(0, 0, canvas.width, height).data;
      for (let y = 0; y < height; y += 1) {
        let runStart = 0;
        let runLength = 0;
        for (let x = 0; x < canvas.width; x += 1) {
          const offset = (y * canvas.width + x) * 4;
          const matches =
            bytes[offset] === target.rgb[0] &&
            bytes[offset + 1] === target.rgb[1] &&
            bytes[offset + 2] === target.rgb[2] &&
            bytes[offset + 3] === 0xff;
          if (matches) {
            if (runLength === 0) runStart = x;
            runLength += 1;
            if (runLength >= target.minimumRun) return { x: runStart, y };
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

function matchesPalette(
  actual: readonly number[],
  palettes: readonly (readonly number[])[],
): boolean {
  return palettes.some((expected) =>
    expected.every((channel, index) => actual[index] === channel),
  );
}

function observedProcessPids(
  lines: readonly string[],
  start: number,
): Set<number> {
  return new Set(
    lines.slice(start).flatMap((line) => {
      const match = line.match(/sysmon: (?:observed|updated) pid=(\d+) name=/);
      return match === null ? [] : [Number(match[1])];
    }),
  );
}

test("System Monitor lists five exact graphical app PIDs, terminates one within one second, and preserves four interactive peers", async ({
  page,
  browserName,
}) => {
  test.setTimeout(90_000);
  test.skip(
    browserName === "webkit",
    "This process-lifecycle gate targets Chromium and Firefox; WebKit is covered by the unsupported-substrate gate.",
  );

  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));
  page.on("pageerror", (error) =>
    consoleLines.push(`[pageerror] ${error.message}`),
  );

  await page.goto("/index.html");
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 12_000,
  });
  await waitForLineAfter(
    consoleLines,
    0,
    (line) =>
      line.includes("shell: loaded 5 applications from /usr/share/applications"),
    "the five-entry launcher catalog",
  );
  await waitForLineAfter(
    consoleLines,
    0,
    (line) => line.includes("shell: connected to /run/display"),
    "the desktop shell display connection",
  );

  const appPids = new Map<string, number>();
  let sysmonLogStart = -1;
  for (const [appIndex, app] of BUNDLED_APPS.entries()) {
    if (app.exec === "/bin/sysmon") sysmonLogStart = consoleLines.length;
    appPids.set(
      app.exec,
      await launchBundledApp(page, consoleLines, app, appIndex),
    );
    if (app.exec === "/bin/files") {
      await waitForLineAfter(
        consoleLines,
        0,
        (line) => line.includes("files: ready /home/user"),
        "Files initial mapped frame",
      );
    } else if (app.exec === "/bin/edit") {
      await waitForLineAfter(
        consoleLines,
        0,
        (line) => line.includes("edit: ready path="),
        "Edit initial mapped frame",
      );
    } else if (app.exec === "/bin/settings") {
      await waitForActiveWindowBounds(page, {
        expectedWidth: 560,
        message: "Settings did not present its graphical window",
      });
    }
  }
  expect(sysmonLogStart).toBeGreaterThanOrEqual(0);
  expect(new Set(appPids.values()).size).toBe(BUNDLED_APPS.length);

  const ready = await waitForLineAfter(
    consoleLines,
    sysmonLogStart,
    (line) => /sysmon: ready processes=\d+ terminate=(enabled|read-only)/.test(line),
    "System Monitor readiness",
  );
  expect(ready).toContain("terminate=enabled");

  for (const app of BUNDLED_APPS) {
    const pid = appPids.get(app.exec)!;
    const row = await waitForLineAfter(
      consoleLines,
      sysmonLogStart,
      (line) =>
        line.includes(`sysmon: observed pid=${pid} name=${app.exec} `) ||
        line.includes(`sysmon: updated pid=${pid} name=${app.exec} `),
      `System Monitor row for ${app.exec} PID ${pid}`,
    );
    const metrics = row.match(/ vm_kib=(\d+) fds=(\d+)$/);
    expect(metrics, row).not.toBeNull();
    expect(Number(metrics![1]), row).toBeGreaterThan(0);
    expect(Number(metrics![2]), row).toBeGreaterThan(0);
  }

  const observedPids = observedProcessPids(consoleLines, sysmonLogStart);
  for (const pid of appPids.values()) expect(observedPids.has(pid)).toBe(true);

  const settingsPid = appPids.get("/bin/settings")!;
  const sortedPids = [...observedPids].sort((left, right) => left - right);
  const settingsRowIndex = sortedPids.indexOf(settingsPid);
  expect(settingsRowIndex).toBeGreaterThanOrEqual(0);

  // Sysmon is the fifth application task after the four peer apps. Retain the
  // exact focused-frame bounds rather than deriving an origin from a colour
  // that can also occur in wallpaper pixels.
  const sysmonOrigin = await waitForActiveWindowBounds(page, {
    expectedX: 128,
    expectedY: 128,
    expectedWidth: 720,
    message: "System Monitor focused frame was not presented",
  });

  const settingsRowProbe = {
    x: sysmonOrigin.x + 680,
    y: sysmonOrigin.y + 72 + settingsRowIndex * 18 + 9,
  };
  await expect
    .poll(
      async () => {
        if (
          matchesPalette(
            await pixel(page, settingsRowProbe),
            SELECTED_ROW_PALETTES,
          )
        ) {
          return true;
        }
        await clickFramebuffer(page, sysmonOrigin.x + 300, settingsRowProbe.y);
        return false;
      },
      {
        timeout: 3_000,
        message: `System Monitor did not select Settings PID ${settingsPid}`,
      },
    )
    .toBe(true);

  const dialogProbe = {
    x: sysmonOrigin.x + 620,
    y: sysmonOrigin.y + 170,
  };
  expect(
    matchesPalette(await pixel(page, dialogProbe), ACTIVE_TITLEBAR_PALETTES),
  ).toBe(false);
  await page.keyboard.down("k");
  try {
    await expect
      .poll(
        async () =>
          matchesPalette(
            await pixel(page, dialogProbe),
            ACTIVE_TITLEBAR_PALETTES,
          ),
        {
          timeout: 3_000,
          message: "System Monitor did not paint its terminate confirmation",
        },
      )
      .toBe(true);
  } finally {
    await page.keyboard.up("k");
  }

  const workersBeforeTermination = await workerCount(page);
  const taskbarBeforeTermination = await regionFingerprint(page, TASKBAR_REGION);
  const terminationLogStart = consoleLines.length;
  const terminationStarted = performance.now();
  await page.keyboard.down("Enter");
  try {
    await expect
      .poll(
        async () =>
          (await workerCount(page)) === workersBeforeTermination - 1 &&
          (await regionFingerprint(page, TASKBAR_REGION)) !==
            taskbarBeforeTermination &&
          (await page
            .locator("body")
            .getAttribute("data-pmos-last-terminated-pid")) ===
            String(settingsPid) &&
          (await page
            .locator("body")
            .getAttribute("data-pmos-last-terminated-signal")) === "9",
        {
          timeout: 1_000,
          message:
            `Settings PID ${settingsPid} host termination, Worker, and task ` +
            "entry did not all disappear within the FR-028 budget",
        },
      )
      .toBe(true);
    expect(performance.now() - terminationStarted).toBeLessThan(1_000);
  } finally {
    await page.keyboard.up("Enter");
  }
  await waitForLineAfter(
    consoleLines,
    terminationLogStart,
    (line) => line.includes(`sysmon: Terminate requested for PID ${settingsPid}`),
    `termination acknowledgement for Settings PID ${settingsPid}`,
    3_000,
  );
  await waitForLineAfter(
    consoleLines,
    terminationLogStart,
    (line) =>
      line.includes(
        `sysmon: process exited pid=${settingsPid} name=/bin/settings`,
      ),
    `System Monitor row removal for Settings PID ${settingsPid}`,
    3_000,
  );
  await waitForFocusedTaskEntry(
    page,
    3,
    4,
    "the four surviving apps after Settings exits",
  );
  await expect
    .poll(
      async () =>
        !matchesPalette(
          await pixel(page, dialogProbe),
          ACTIVE_TITLEBAR_PALETTES,
        ),
      { timeout: 3_000, message: "terminate confirmation did not dismiss" },
    )
    .toBe(true);

  // Each survivor now receives app-specific input through its original window.
  // There is no relaunch: the exact launch PID remains the identity under test.
  try {
    await focusTaskEntry(page, 0, 4, "surviving Terminal");
  } catch (cause) {
    throw new Error(
      `surviving Terminal focus failed; recent console:\n${consoleLines
        .slice(-60)
        .join("\n")}`,
      { cause },
    );
  }
  await expect
    .poll(() => findSolidRun(page, TERMINAL_BACKGROUND, 320), {
      timeout: 5_000,
      message: "surviving Terminal did not return to the front",
    })
    .not.toBeNull();
  await acknowledgeTerminalFocus(page, consoleLines, 8_000);

  await focusTaskEntry(page, 1, 4, "surviving Files");
  const filesOrigin = await waitForActiveWindowBounds(page, {
    expectedX: 32,
    expectedY: 32,
    expectedWidth: 640,
    message: "surviving Files did not return to the front",
  });
  const filesInputStart = consoleLines.length;
  await clickFramebuffer(page, filesOrigin.x + 300, filesOrigin.y + 109);
  await waitForLineAfter(
    consoleLines,
    filesInputStart,
    (line) => line.includes("files: selected /home/user/"),
    "surviving Files pointer selection",
    5_000,
  );

  await focusTaskEntry(page, 2, 4, "surviving Edit");
  const editOrigin = await waitForActiveWindowBounds(page, {
    expectedX: 64,
    expectedY: 64,
    expectedWidth: 640,
    message: "surviving Edit did not return to the front",
  });
  const editContent = {
    x: editOrigin.x + 36,
    y: editOrigin.y + 44,
    width: 180,
    height: 24,
  };
  const editBeforeInput = await regionFingerprint(page, editContent);
  await page.keyboard.type("z");
  await expect
    .poll(() => regionFingerprint(page, editContent), {
      timeout: 3_000,
      message: "surviving Edit did not repaint its typed character",
    })
    .not.toBe(editBeforeInput);

  await focusTaskEntry(page, 3, 4, "surviving System Monitor");
  const survivingSysmonOrigin = await waitForActiveWindowBounds(page, {
    expectedX: 128,
    expectedY: 128,
    expectedWidth: 720,
    message: "surviving System Monitor did not return to the front",
  });
  const postKillPids = sortedPids.filter((pid) => pid !== settingsPid);
  const termPid = appPids.get("/bin/term")!;
  const termRowIndex = postKillPids.indexOf(termPid);
  expect(termRowIndex).toBeGreaterThanOrEqual(0);
  const termRowProbe = {
    x: survivingSysmonOrigin.x + 680,
    y: survivingSysmonOrigin.y + 72 + termRowIndex * 18 + 9,
  };
  await expect
    .poll(
      async () => {
        if (
          matchesPalette(
            await pixel(page, termRowProbe),
            SELECTED_ROW_PALETTES,
          )
        ) {
          return true;
        }
        await clickFramebuffer(
          page,
          survivingSysmonOrigin.x + 300,
          termRowProbe.y,
        );
        return false;
      },
      {
        timeout: 3_000,
        message: `surviving System Monitor did not select Terminal PID ${termPid}`,
      },
    )
    .toBe(true);

  const survivors = BUNDLED_APPS.filter(
    (app) => app.exec !== "/bin/settings",
  );
  for (const app of survivors) {
    const pid = appPids.get(app.exec)!;
    expect(
      consoleLines.some((line) =>
        line.includes(`sysmon: process exited pid=${pid} name=${app.exec}`),
      ),
      `${app.exec} PID ${pid} unexpectedly exited`,
    ).toBe(false);
    expect(
      consoleLines.filter((line) =>
        line.includes(`shell: launched ${app.exec} pid=${pid}`),
      ),
      `${app.exec} PID ${pid} must not be replaced or relaunched`,
    ).toHaveLength(1);
  }
  expect(await workerCount(page)).toBe(workersBeforeTermination - 1);
  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(
    false,
  );
  expect(
    consoleLines.some((line) => line.includes("user worker crashed pid=")),
  ).toBe(false);
  expect(consoleLines.some((line) => line.startsWith("[pageerror]"))).toBe(
    false,
  );
  expect(
    consoleLines.some((line) => line.includes("using built-in fallback")),
  ).toBe(false);
});
