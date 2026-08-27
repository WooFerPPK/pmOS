// T181 — Principle II acceptance gate: replace the live desktop shell from
// ordinary guest workflows while existing applications remain alive.

import { expect, test, type Page } from "@playwright/test";

import {
  launchTerminal,
  runTerminalCommand,
  runTerminalCommandToPrompt,
} from "./guest-terminal";
import {
  launcherMenuRegionFingerprint,
  openLauncherMenuBefore,
  selectLauncherRowBefore,
} from "./launcher-interaction";
import {
  LIGHT_ACTIVE_BORDER,
  LIGHT_TITLEBAR,
  TASKBAR_DARK,
  taskbarEntryPoint,
  taskbarEntryRegion,
  waitForActiveWindowBounds,
} from "./windows-ui";

test.use({ viewport: { width: 1280, height: 900 } });
test.setTimeout(45_000);

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const TASKBAR_Y = 752;

const DEFAULT_INIT_CONFIG = `[boot]
display_server = "/usr/bin/display-server"
shell = "/usr/bin/shell"
autostart = []

[capabilities.display-server]
grant = ["DISPLAY_SERVER", "DEV_BLOCK"]

[capabilities.shell]
grant = ["DISPLAY_CLIENT", "SHELL", "PROC_ENUMERATE", "PROC_KILL_ANY", "KEYMAP_ADMIN", "HOST_TRANSFER"]

[capabilities.sysmon]
grant = ["DISPLAY_CLIENT", "PROC_ENUMERATE", "PROC_KILL_ANY"]

[env]
PATH = "/bin:/usr/bin"
HOME = "/home/user"
USER = "user"
XDG_RUNTIME_DIR = "/run"
PMOS_DISPLAY = "/run/display"

[debug]
kernel_log_level = "info"
serial_shell = false
`;

const ALTERNATE_INIT_CONFIG = DEFAULT_INIT_CONFIG.replace(
  'shell = "/usr/bin/shell"',
  'shell = "/usr/bin/alt-shell"',
);

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

async function framebufferPixel(
  page: Page,
  x: number,
  y: number,
): Promise<readonly number[]> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, point: { x: number; y: number }) => {
      const context = canvas.getContext("2d");
      if (context === null) return [];
      return Array.from(context.getImageData(point.x, point.y, 1, 1).data);
    },
    { x, y },
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

async function waitForLineAfter(
  lines: readonly string[],
  start: number,
  predicate: (line: string) => boolean,
  timeout = 10_000,
): Promise<string> {
  await expect
    .poll(() => lines.slice(start).find(predicate) ?? null, {
      timeout,
      message: `expected OS console line; observed:\n${lines.join("\n")}`,
    })
    .not.toBeNull();
  return lines.slice(start).find(predicate)!;
}

async function dropTextFile(
  page: Page,
  name: string,
  text: string,
): Promise<void> {
  await page.evaluate(
    async ({ fileName, contents }) => {
      const bytes = new TextEncoder().encode(contents);
      const file = new File([bytes], fileName, { type: "text/plain" });
      Object.defineProperty(file, "arrayBuffer", {
        value: async () => bytes.slice().buffer,
      });
      const transfer = { files: [file] };
      for (const type of ["dragover", "drop"] as const) {
        const event = new Event(type, { bubbles: true, cancelable: true });
        Object.defineProperty(event, "dataTransfer", { value: transfer });
        window.dispatchEvent(event);
      }
      await Promise.resolve();
      await Promise.resolve();
    },
    { fileName: name, contents: text },
  );
}

async function workerCount(page: Page): Promise<number> {
  return Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
}

function launchedPid(line: string, executable: string): number {
  const match = line.match(/ pid=(\d+)$/);
  if (match === null || !line.includes(`launched ${executable} pid=`)) {
    throw new Error(`could not parse ${executable} PID from: ${line}`);
  }
  return Number(match[1]);
}

async function focusReplacementTask(
  page: Page,
  lines: readonly string[],
  clickX: number,
  entryRegion: { x: number; y: number; width: number; height: number },
): Promise<number> {
  const before = await framebufferRegionFingerprint(page, entryRegion);
  const start = lines.length;
  await clickFramebuffer(page, clickX, TASKBAR_Y);
  const request = await waitForLineAfter(
    lines,
    start,
    (line) => /shell: taskbar focus requested window_id=\d+/.test(line),
    5_000,
  );
  await expect
    .poll(() => framebufferRegionFingerprint(page, entryRegion), {
      timeout: 5_000,
      message: `replacement shell did not repaint task entry at x=${entryRegion.x}`,
    })
    .not.toBe(before);
  const match = request.match(/window_id=(\d+)$/);
  if (match === null)
    throw new Error(`taskbar focus omitted window id: ${request}`);
  return Number(match[1]);
}

test("replaces the shell while Files, Terminal, and Edit survive", async ({
  page,
  browserName,
}) => {
  test.skip(
    browserName === "webkit",
    "WebKit's Playwright runtime has no OPFS root; the release layering gate targets Chromium and Firefox.",
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
  const shellStartupLine = await waitForLineAfter(consoleLines, 0, (line) =>
    line.includes("init-desktop shell path=/usr/bin/shell"),
  );
  const shellPid = Number(shellStartupLine.match(/ pid=(\d+)$/)?.[1]);
  expect(shellPid, shellStartupLine).toBeGreaterThan(0);
  await waitForLineAfter(consoleLines, 0, (line) =>
    line.includes("shell: loaded 5 applications from /usr/share/applications"),
  );

  // Files receives both replacement configurations through the documented
  // host-transfer path. The terminal then installs the alternate one using
  // the shipped `cp`, with no browser-side VFS mutation shortcut.
  const closedLauncherFingerprint = await launcherMenuRegionFingerprint(page);
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  const filesLaunchStart = consoleLines.length;
  await selectLauncherRowBefore(
    page,
    100,
    648,
    Date.now() + 5_000,
    closedLauncherFingerprint,
  );
  const filesLaunchLine = await waitForLineAfter(
    consoleLines,
    filesLaunchStart,
    (line) => /shell: launched \/bin\/files pid=\d+/.test(line),
  );
  const filesPid = launchedPid(filesLaunchLine, "/bin/files");
  await waitForLineAfter(consoleLines, 0, (line) =>
    line.includes("files: host transfer ready"),
  );
  await waitForLineAfter(consoleLines, 0, (line) =>
    line.includes("files: ready /home/user"),
  );

  let start = consoleLines.length;
  await dropTextFile(page, "alt-init.conf", ALTERNATE_INIT_CONFIG);
  await waitForLineAfter(consoleLines, start, (line) =>
    line.includes("files: imported /home/user/alt-init.conf"),
  );
  start = consoleLines.length;
  await dropTextFile(page, "default-init.conf", DEFAULT_INIT_CONFIG);
  await waitForLineAfter(consoleLines, start, (line) =>
    line.includes("files: imported /home/user/default-init.conf"),
  );

  const terminalLaunchStart = consoleLines.length;
  await launchTerminal(page, consoleLines);
  const terminalLaunchLine = await waitForLineAfter(
    consoleLines,
    terminalLaunchStart,
    (line) => /shell: launched \/bin\/term pid=\d+/.test(line),
  );
  const terminalPid = launchedPid(terminalLaunchLine, "/bin/term");
  await runTerminalCommandToPrompt(
    page,
    consoleLines,
    "cp /home/user/alt-init.conf /etc/init.conf",
  );
  const terminalWindow = await waitForActiveWindowBounds(page, {
    expectedX: 32,
    expectedY: 32,
    expectedWidth: 720,
    message: "Terminal frame vanished before launching Edit",
  });

  // SC-008's third independent application is the bundled editor. Wait for
  // its first committed surface, not just its spawn log, before replacing the
  // shell so all three windows are genuinely mapped at the boundary.
  const editClosedFingerprint = await launcherMenuRegionFingerprint(page);
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  const editLaunchStart = consoleLines.length;
  await selectLauncherRowBefore(
    page,
    100,
    672,
    Date.now() + 5_000,
    editClosedFingerprint,
  );
  const editLaunchLine = await waitForLineAfter(
    consoleLines,
    editLaunchStart,
    (line) => /shell: launched \/bin\/edit pid=\d+/.test(line),
  );
  const editPid = launchedPid(editLaunchLine, "/bin/edit");
  await waitForLineAfter(consoleLines, editLaunchStart, (line) =>
    line.includes("edit: starting"),
  );
  await waitForLineAfter(consoleLines, editLaunchStart, (line) =>
    line.includes("edit: ready path="),
  );
  await waitForActiveWindowBounds(page, {
    differentFrom: terminalWindow,
    expectedX: 64,
    expectedY: 64,
    expectedWidth: 640,
    timeout: 10_000,
    message: "expected Edit's committed frame before shell replacement",
  });
  expect(
    new Set([filesPid, terminalPid, editPid]).size,
    `application PIDs must be unique: Files=${filesPid}, Terminal=${terminalPid}, Edit=${editPid}`,
  ).toBe(3);

  const termStartsBefore = consoleLines.filter((line) =>
    line.includes("term: starting"),
  ).length;
  const filesStartsBefore = consoleLines.filter((line) =>
    line.includes("files: starting"),
  ).length;
  const editStartsBefore = consoleLines.filter((line) =>
    line.includes("edit: starting"),
  ).length;

  // Use System Monitor's capability-scoped process UI to stop the current
  // shell now that the shell itself is deliberately omitted from app tasks.
  // PID 1 must re-read init.conf, wait out the crash-loop limiter, and spawn
  // the configured replacement binary.
  const sysmonClosedFingerprint = await launcherMenuRegionFingerprint(page);
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  const sysmonStart = consoleLines.length;
  await selectLauncherRowBefore(
    page,
    100,
    720,
    Date.now() + 5_000,
    sysmonClosedFingerprint,
  );
  await waitForLineAfter(consoleLines, sysmonStart, (line) =>
    line.includes("sysmon: starting"),
  );
  await waitForLineAfter(consoleLines, sysmonStart, (line) =>
    line.includes(`sysmon: observed pid=${shellPid} name=/usr/bin/shell `),
  );
  const sysmonOrigin = await waitForActiveWindowBounds(page, {
    expectedX: 96,
    expectedY: 96,
    expectedWidth: 720,
    message: "System Monitor frame vanished",
  });
  const observedPids = consoleLines.slice(sysmonStart).flatMap((line) => {
    const match = line.match(/sysmon: (?:observed|updated) pid=(\d+) name=/);
    return match === null ? [] : [Number(match[1])];
  });
  const sortedPids = [...new Set(observedPids)].sort(
    (left, right) => left - right,
  );
  const shellRow = sortedPids.indexOf(shellPid);
  expect(shellRow).toBeGreaterThanOrEqual(0);
  const workersBeforeReplacement = await workerCount(page);
  const shellRowY = sysmonOrigin.y + 81 + shellRow * 18;
  await expect
    .poll(async () => {
      const selected = await framebufferPixel(
        page,
        sysmonOrigin.x + 680,
        shellRowY,
      );
      if (
        LIGHT_ACTIVE_BORDER.every(
          (channel, index) => selected[index] === channel,
        )
      ) {
        return selected;
      }
      await clickFramebuffer(page, sysmonOrigin.x + 300, shellRowY);
      return [];
    })
    .toEqual([...LIGHT_ACTIVE_BORDER]);
  await page.keyboard.press("k");
  await expect
    .poll(() =>
      framebufferPixel(page, sysmonOrigin.x + 620, sysmonOrigin.y + 170),
    )
    .toEqual([...LIGHT_TITLEBAR]);
  start = consoleLines.length;
  await page.keyboard.press("Enter");
  await waitForLineAfter(consoleLines, start, (line) =>
    line.includes("init-desktop shell exited"),
  );
  await waitForLineAfter(consoleLines, start, (line) =>
    line.includes("init-desktop respawned shell path=/usr/bin/alt-shell"),
  );
  await waitForLineAfter(consoleLines, start, (line) =>
    line.includes(
      "alt-shell: loaded 5 applications from /usr/share/applications",
    ),
  );
  await waitForLineAfter(consoleLines, start, (line) =>
    line.includes("alt-shell: connected to /run/display"),
  );

  // The alternate taskbar's dark chrome is the visible replacement marker.
  // Files, Terminal, and Edit were not restarted, and the replacement brings
  // the live Worker count back to the same value after the old shell exits.
  await expect
    .poll(() => framebufferPixel(page, 800, 760), { timeout: 10_000 })
    .toEqual([...TASKBAR_DARK]);
  await expect
    .poll(() => workerCount(page), { timeout: 10_000 })
    .toBe(workersBeforeReplacement);
  expect(
    consoleLines.filter((line) => line.includes("term: starting")),
  ).toHaveLength(termStartsBefore);
  expect(
    consoleLines.filter((line) => line.includes("files: starting")),
  ).toHaveLength(filesStartsBefore);
  expect(
    consoleLines.filter((line) => line.includes("edit: starting")),
  ).toHaveLength(editStartsBefore);

  // The replacement enumerates only application windows: Files, Terminal,
  // Edit, then System Monitor. Each exact label click must yield a distinct
  // server-global window id and an app-specific input response.
  const filesTaskEntry = taskbarEntryRegion(0, 4);
  const terminalTaskEntry = taskbarEntryRegion(1, 4);
  const editTaskEntry = taskbarEntryRegion(2, 4);
  const terminalTask = taskbarEntryPoint(1, 4);
  const terminalWindowId = await focusReplacementTask(
    page,
    consoleLines,
    terminalTask.x,
    terminalTaskEntry,
  );
  await runTerminalCommand(
    page,
    consoleLines,
    "echo layering-app-survived > /dev/console",
    (line) => line === "[real-kernel] layering-app-survived",
  );

  const filesWindowId = await focusReplacementTask(
    page,
    consoleLines,
    taskbarEntryPoint(0, 4).x,
    filesTaskEntry,
  );
  start = consoleLines.length;
  await page.keyboard.press("g");
  await waitForLineAfter(consoleLines, start, (line) =>
    line.includes("files: refreshed /home/user"),
  );

  const editWindowId = await focusReplacementTask(
    page,
    consoleLines,
    taskbarEntryPoint(2, 4).x,
    editTaskEntry,
  );
  start = consoleLines.length;
  await page.keyboard.type("z");
  await waitForLineAfter(consoleLines, start, (line) =>
    line.includes("edit: modified"),
  );
  expect(
    new Set([filesWindowId, terminalWindowId, editWindowId]).size,
    "replacement task entries must enumerate three distinct app windows",
  ).toBe(3);

  // Restore the standard boot policy through the same guest path so the
  // persistent root is clean even if this browser context is reused.
  const restoredTerminalWindowId = await focusReplacementTask(
    page,
    consoleLines,
    terminalTask.x,
    terminalTaskEntry,
  );
  expect(restoredTerminalWindowId).toBe(terminalWindowId);
  await runTerminalCommandToPrompt(
    page,
    consoleLines,
    "cp /home/user/default-init.conf /etc/init.conf",
  );

  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(
    false,
  );
  expect(consoleLines.some((line) => line.startsWith("[pageerror]"))).toBe(
    false,
  );
});
