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

test.use({ viewport: { width: 1280, height: 900 } });
test.setTimeout(45_000);

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const TASKBAR_Y = 752;
const DARK_TASKBAR = [0x2b, 0x31, 0x3d, 0xff] as const;

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
        const pixels = context.getImageData(
          0,
          0,
          canvas.width,
          canvas.height,
        ).data;
        for (let y = 0; y < canvas.height; y += 1) {
          for (let x = 0; x < canvas.width; x += 1) {
            const offset = (y * canvas.width + x) * 4;
            if (
              pixels[offset] === target[0] &&
              pixels[offset + 1] === target[1] &&
              pixels[offset + 2] === target[2]
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
  await waitForLineAfter(consoleLines, 0, (line) =>
    line.includes("init-desktop shell path=/usr/bin/shell"),
  );
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
  await expect
    .poll(() => findFramebufferColor(page, [0x6a, 0x4a, 0x8a]), {
      timeout: 10_000,
      message: "expected Edit's committed titlebar before shell replacement",
    })
    .not.toBeNull();
  expect(
    new Set([filesPid, terminalPid, editPid]).size,
    `application PIDs must be unique: Files=${filesPid}, Terminal=${terminalPid}, Edit=${editPid}`,
  ).toBe(3);

  const workersBeforeReplacement = await workerCount(page);
  const termStartsBefore = consoleLines.filter((line) =>
    line.includes("term: starting"),
  ).length;
  const filesStartsBefore = consoleLines.filter((line) =>
    line.includes("files: starting"),
  ).length;
  const editStartsBefore = consoleLines.filter((line) =>
    line.includes("edit: starting"),
  ).length;

  // Entry zero is the current shell; its close control is x=230..249. This
  // is a real display-protocol close request. PID 1 must re-read init.conf,
  // wait out the crash-loop limiter, and spawn the new binary.
  start = consoleLines.length;
  await clickFramebuffer(page, 240, TASKBAR_Y);
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
    .toEqual(DARK_TASKBAR);
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

  // The replacement enumerates the current bottom-to-top z-order: itself,
  // Files, Terminal, then Edit. Each exact label click must yield a distinct
  // server-global window id and an app-specific input response.
  const filesTaskEntry = { x: 252, y: 738, width: 160, height: 28 };
  const terminalTaskEntry = { x: 414, y: 738, width: 160, height: 28 };
  const editTaskEntry = { x: 576, y: 738, width: 160, height: 28 };
  const terminalWindowId = await focusReplacementTask(
    page,
    consoleLines,
    460,
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
    300,
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
    620,
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
    460,
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
