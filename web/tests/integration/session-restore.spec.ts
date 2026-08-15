// Constitution IV release gate: a durable six-app session survives a normal
// tab close and is restored before the next desktop becomes interactive.
// Assertions use only guest console output and pixels presented through the
// real framebuffer path.

import { expect, test, type Page } from "@playwright/test";

import {
  acknowledgeTerminalFocus,
  bootDesktop,
  runTerminalCommand,
  runTerminalCommandToPrompt,
} from "./guest-terminal";
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
test.setTimeout(120_000);

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
const TASKBAR_CONTROL_WIDTH = 20;
const TASKBAR_CONTROL_GAP = 2;
const TASKBAR_FOCUSED = [0xc2, 0xc6, 0xcf, 0xff] as const;
const TASKBAR_UNFOCUSED = [0xec, 0xee, 0xf4, 0xff] as const;
const TASKBAR_MINIMIZED = [0xe2, 0xe4, 0xea, 0xff] as const;
const TASKBAR_CLOSE_FILL = [0xd8, 0xdc, 0xe4, 0xff] as const;
const TASKBAR_TAIL_POINT = { x: 945, y: TASKBAR_ENTRY_SAMPLE_Y } as const;
const FILES_TITLEBAR = [0x35, 0x5f, 0x84, 0xff] as const;
const SETTINGS_TITLEBAR = [0x40, 0x60, 0x70, 0xff] as const;
const TERMINAL_BACKGROUND = [0x14, 0x0e, 0x0a, 0xff] as const;
const SESSION_HEADER = "PMOS_SESSION_V1";
const SESSION_PATH = "/home/user/.config/pmos/session-v1";
const SENTINEL_PATH = "/home/user/session-restore-sentinel.txt";
const SENTINEL_TEXT = "pmos-session-restore-sentinel";
const SESSION_FLAG_MINIMIZED = 1;
const SESSION_FLAG_MAXIMIZED = 2;
const APP_WINDOW_COUNT = 6;
const TASK_ENTRY_COUNT = APP_WINDOW_COUNT + 1;
const RESTORED_INPUT_SAMPLE_COUNT = 20;
const LATENCY_BUDGET_MS = 100;

interface Point {
  readonly x: number;
  readonly y: number;
}

interface ColorBounds extends Point {
  readonly right: number;
  readonly count: number;
}

interface BundledApp {
  readonly name: string;
  readonly exec: string;
  readonly launcherY: number;
  readonly started: (line: string) => boolean;
}

interface StoredInstance {
  readonly id: number;
  readonly desktopEntryId: string;
}

interface StoredWindow {
  readonly id: number;
  readonly instanceId: number;
  readonly ordinal: number;
  readonly zRank: number;
  readonly normalX: number;
  readonly normalY: number;
  readonly normalWidth: number;
  readonly normalHeight: number;
  readonly flags: number;
}

interface StoredSession {
  readonly outputWidth: number;
  readonly outputHeight: number;
  readonly focusedWindow: number | null;
  readonly instances: readonly StoredInstance[];
  readonly windows: readonly StoredWindow[];
}

interface DurableSnapshot {
  readonly revision: number;
  readonly text: string;
  readonly session: StoredSession;
}

interface DurableDiagnostic {
  readonly revision: number;
  readonly apps: number;
  readonly windows: number;
  readonly bytes: number;
  readonly digest: string;
}

type TaskPalette = "focused" | "unfocused" | "minimized" | "other";

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
    started: (line) => /shell: launched \/bin\/settings pid=\d+/.test(line),
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

async function frameSequence(page: Page): Promise<number> {
  return Number(
    (await page.locator("#pmos-fb").getAttribute("data-pmos-frame-sequence")) ??
      "0",
  );
}

async function clickFramebuffer(page: Page, point: Point): Promise<void> {
  const target = await toPagePoint(page, point);
  await page.mouse.click(target.x, target.y);
}

async function pixel(page: Page, point: Point): Promise<number[]> {
  return page
    .locator("#pmos-fb")
    .evaluate((canvas: HTMLCanvasElement, sample: Point) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      return Array.from(context.getImageData(sample.x, sample.y, 1, 1).data);
    }, point);
}

async function colorBounds(
  page: Page,
  rgba: readonly [number, number, number, number],
): Promise<ColorBounds | null> {
  return page
    .locator("#pmos-fb")
    .evaluate((canvas: HTMLCanvasElement, target) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      const bytes = context.getImageData(0, 0, canvas.width, 736).data;
      let best: ColorBounds | null = null;
      for (let y = 0; y < 736; y += 1) {
        let runStart = -1;
        for (let x = 0; x <= canvas.width; x += 1) {
          const offset = (y * canvas.width + x) * 4;
          const matched =
            x < canvas.width &&
            bytes[offset] === target[0] &&
            bytes[offset + 1] === target[1] &&
            bytes[offset + 2] === target[2] &&
            bytes[offset + 3] === target[3];
          if (matched && runStart < 0) runStart = x;
          if (!matched && runStart >= 0) {
            const count = x - runStart;
            if (best === null || count > best.count) {
              best = { x: runStart, y, right: x, count };
            }
            runStart = -1;
          }
        }
      }
      return best !== null && best.count >= 300 ? best : null;
    }, rgba);
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

function taskbarEntryX(index: number, entryCount: number): number {
  return (
    TASKBAR_LEFT_MARGIN +
    TASKBAR_LAUNCHER_RESERVED_WIDTH +
    index * (taskbarEntryWidth(entryCount) + TASKBAR_ENTRY_GAP)
  );
}

function taskbarEntryPoint(index: number, entryCount: number): Point {
  return {
    x: taskbarEntryX(index, entryCount) + 30,
    y: TASKBAR_ENTRY_SAMPLE_Y,
  };
}

function taskbarLabelPoint(index: number, entryCount: number): Point {
  const width = taskbarEntryWidth(entryCount);
  const labelWidth =
    width - 3 * TASKBAR_CONTROL_WIDTH - 3 * TASKBAR_CONTROL_GAP;
  return {
    x:
      taskbarEntryX(index, entryCount) +
      Math.max(12, Math.floor(labelWidth / 2)),
    y: TASKBAR_Y,
  };
}

function taskbarControlPoint(
  index: number,
  entryCount: number,
  control: "minimize" | "maximize",
): Point {
  const right =
    taskbarEntryX(index, entryCount) + taskbarEntryWidth(entryCount);
  return {
    x:
      control === "maximize"
        ? right - TASKBAR_CONTROL_WIDTH - TASKBAR_CONTROL_GAP - 10
        : right - 2 * TASKBAR_CONTROL_WIDTH - 2 * TASKBAR_CONTROL_GAP - 10,
    y: TASKBAR_Y,
  };
}

function channelsEqual(
  actual: readonly number[],
  expected: readonly number[],
): boolean {
  return actual.every((channel, index) => channel === expected[index]);
}

function classifyTaskPalette(channels: readonly number[]): TaskPalette {
  if (channelsEqual(channels, TASKBAR_FOCUSED)) return "focused";
  if (channelsEqual(channels, TASKBAR_UNFOCUSED)) return "unfocused";
  if (channelsEqual(channels, TASKBAR_MINIMIZED)) return "minimized";
  return "other";
}

async function taskPalettes(
  page: Page,
  entryCount: number,
): Promise<TaskPalette[]> {
  return Promise.all(
    Array.from({ length: entryCount }, async (_, index) =>
      classifyTaskPalette(
        await pixel(page, taskbarEntryPoint(index, entryCount)),
      ),
    ),
  );
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
    .poll(
      async () => {
        if ((await frameSequence(page)) <= afterSequence) return false;
        const palettes = await taskPalettes(page, entryCount);
        return palettes.every((palette, entryIndex) =>
          entryIndex === index ? palette === "focused" : palette !== "focused",
        );
      },
      {
        timeout: 10_000,
        message: `${label} did not become focused task entry ${index}`,
      },
    )
    .toBe(true);
}

async function launchBundledApp(
  page: Page,
  lines: readonly string[],
  app: BundledApp,
  appIndex: number,
): Promise<void> {
  const logStart = lines.length;
  expect(await launcherMenuIsOpen(page)).toBe(false);
  const closedFingerprint = await launcherMenuRegionFingerprint(page);
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  const menuCloseFrame = await selectLauncherRowBefore(
    page,
    100,
    app.launcherY,
    Date.now() + 5_000,
    closedFingerprint,
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
  const before = await frameSequence(page);
  await clickFramebuffer(page, taskbarLabelPoint(index, entryCount));
  await waitForFocusedTaskEntry(page, index, entryCount, label, before);
}

async function dragFiles(
  page: Page,
  lines: readonly string[],
  initial: ColorBounds,
): Promise<ColorBounds> {
  const dragFrom = { x: initial.x + 300, y: initial.y + 10 };
  const warm = { x: dragFrom.x + 24, y: dragFrom.y + 16 };
  const dragTo = { x: dragFrom.x + 120, y: dragFrom.y + 80 };
  const fromPage = await toPagePoint(page, dragFrom);
  const warmPage = await toPagePoint(page, warm);
  const toPage = await toPagePoint(page, dragTo);

  await page.mouse.move(fromPage.x, fromPage.y);
  const moveLogStart = lines.length;
  await page.mouse.down();
  await waitForLineAfter(
    lines,
    moveLogStart,
    (line) => /files: move requested serial=\d+/.test(line),
    "Files interactive-move request",
    3_000,
  );
  await page.mouse.move(warmPage.x, warmPage.y, { steps: 16 });
  await expect
    .poll(
      async () => {
        const bounds = await colorBounds(page, FILES_TITLEBAR);
        return (
          bounds !== null && (bounds.x > initial.x || bounds.y > initial.y)
        );
      },
      {
        timeout: 3_000,
        message: "Files did not enter the interactive move",
      },
    )
    .toBe(true);

  const beforeMove = await frameSequence(page);
  await page.mouse.move(toPage.x, toPage.y);
  await expect
    .poll(() => frameSequence(page), { timeout: 3_000 })
    .toBeGreaterThan(beforeMove);
  await expect
    .poll(
      async () => {
        const bounds = await colorBounds(page, FILES_TITLEBAR);
        return (
          bounds !== null &&
          bounds.x > initial.x + 60 &&
          bounds.y > initial.y + 40
        );
      },
      {
        timeout: 3_000,
        message: "Files did not present the requested settled geometry",
      },
    )
    .toBe(true);

  const releaseLogStart = lines.length;
  await page.mouse.up();
  await waitForLineAfter(
    lines,
    releaseLogStart,
    (line) => line.includes("display-server: drag completed"),
    "Files drag completion",
    3_000,
  );
  const dragged = await colorBounds(page, FILES_TITLEBAR);
  if (dragged === null) throw new Error("Files titlebar vanished after drag");
  return dragged;
}

function parseInteger(field: string, label: string): number {
  if (!/^-?(0|[1-9][0-9]*)$/.test(field)) {
    throw new Error(`non-canonical ${label}: ${field}`);
  }
  const parsed = Number(field);
  if (!Number.isSafeInteger(parsed))
    throw new Error(`invalid ${label}: ${field}`);
  return parsed;
}

function parseStoredSession(text: string): StoredSession {
  if (!text.endsWith("\n") || text.endsWith("\n\n") || text.includes("\r")) {
    throw new Error("session is not canonical newline-terminated UTF-8 text");
  }
  const rows = text.slice(0, -1).split("\n");
  if (rows.shift() !== SESSION_HEADER)
    throw new Error("invalid session header");
  let output: readonly [number, number] | null = null;
  let focusedWindow: number | null | undefined;
  const instances: StoredInstance[] = [];
  const windows: StoredWindow[] = [];

  for (const row of rows) {
    const fields = row.split(" ");
    if (fields[0] === "output" && fields.length === 3) {
      output = [
        parseInteger(fields[1]!, "output width"),
        parseInteger(fields[2]!, "output height"),
      ];
    } else if (fields[0] === "focus" && fields.length === 2) {
      const id = parseInteger(fields[1]!, "focus id");
      focusedWindow = id === 0 ? null : id;
    } else if (fields[0] === "instance" && fields.length === 3) {
      instances.push({
        id: parseInteger(fields[1]!, "instance id"),
        desktopEntryId: fields[2]!,
      });
    } else if (fields[0] === "window" && fields.length === 10) {
      windows.push({
        id: parseInteger(fields[1]!, "window id"),
        instanceId: parseInteger(fields[2]!, "window instance id"),
        ordinal: parseInteger(fields[3]!, "window ordinal"),
        zRank: parseInteger(fields[4]!, "window z rank"),
        normalX: parseInteger(fields[5]!, "window normal x"),
        normalY: parseInteger(fields[6]!, "window normal y"),
        normalWidth: parseInteger(fields[7]!, "window normal width"),
        normalHeight: parseInteger(fields[8]!, "window normal height"),
        flags: parseInteger(fields[9]!, "window flags"),
      });
    } else {
      throw new Error(`invalid session row: ${row}`);
    }
  }
  if (output === null || focusedWindow === undefined) {
    throw new Error("session is missing output or focus");
  }
  return {
    outputWidth: output[0],
    outputHeight: output[1],
    focusedWindow,
    instances,
    windows,
  };
}

function normalizedSession(session: StoredSession) {
  const instanceNames = new Map(
    session.instances.map((instance) => [instance.id, instance.desktopEntryId]),
  );
  return {
    output: [session.outputWidth, session.outputHeight],
    applications: session.instances
      .map((instance) => instance.desktopEntryId)
      .sort(),
    windows: [...session.windows]
      .sort((left, right) => left.zRank - right.zRank)
      .map((window) => {
        const desktopEntryId = instanceNames.get(window.instanceId);
        if (desktopEntryId === undefined) {
          throw new Error(
            `window references missing instance ${window.instanceId}`,
          );
        }
        return {
          desktopEntryId,
          ordinal: window.ordinal,
          zRank: window.zRank,
          normalX: window.normalX,
          normalY: window.normalY,
          normalWidth: window.normalWidth,
          normalHeight: window.normalHeight,
          flags: window.flags,
          focused: session.focusedWindow === window.id,
        };
      }),
  };
}

function expectedApplicationIds(session: StoredSession): boolean {
  return (
    session.instances
      .map((instance) => instance.desktopEntryId)
      .sort()
      .join(",") === "edit,files,settings,sysmon,terminal,terminal"
  );
}

function hasExpectedState(
  session: StoredSession,
  filesGeometry: ColorBounds,
  focusedTerminal: 0 | 1,
): boolean {
  if (
    session.outputWidth !== FRAMEBUFFER_WIDTH ||
    session.outputHeight !== FRAMEBUFFER_HEIGHT ||
    session.instances.length !== APP_WINDOW_COUNT ||
    session.windows.length !== APP_WINDOW_COUNT ||
    !expectedApplicationIds(session)
  ) {
    return false;
  }
  const instances = new Map(
    session.instances.map((instance) => [instance.id, instance.desktopEntryId]),
  );
  const files = session.windows.find(
    (window) => instances.get(window.instanceId) === "files",
  );
  const edit = session.windows.find(
    (window) => instances.get(window.instanceId) === "edit",
  );
  const terminalIds = session.instances
    .filter((instance) => instance.desktopEntryId === "terminal")
    .map((instance) => instance.id)
    .sort((left, right) => left - right);
  const focused = session.windows.find(
    (window) => window.id === session.focusedWindow,
  );
  return (
    files !== undefined &&
    files.flags === SESSION_FLAG_MAXIMIZED &&
    files.normalX === filesGeometry.x &&
    files.normalY === filesGeometry.y &&
    files.normalWidth === filesGeometry.right - filesGeometry.x &&
    files.normalHeight === 420 &&
    edit !== undefined &&
    edit.flags === SESSION_FLAG_MINIMIZED &&
    focused !== undefined &&
    focused.instanceId === terminalIds[focusedTerminal] &&
    focused.zRank === APP_WINDOW_COUNT - 1 &&
    session.windows
      .map((window) => window.zRank)
      .sort((left, right) => left - right)
      .every((rank, index) => rank === index)
  );
}

async function readCanonicalSession(
  page: Page,
  lines: readonly string[],
): Promise<{ text: string; session: StoredSession }> {
  const start = lines.length;
  await runTerminalCommand(
    page,
    lines,
    `cat ${SESSION_PATH} > /dev/console`,
    (line) => line.includes(SESSION_HEADER),
  );
  const outputRows = lines
    .slice(start)
    .flatMap((message) => message.split("\n"))
    .filter((message) => message.startsWith("[real-kernel] "))
    .map((message) => message.slice("[real-kernel] ".length));
  const header = outputRows.indexOf(SESSION_HEADER);
  if (header < 0) throw new Error("session output omitted its header");
  const sessionRows = [SESSION_HEADER];
  for (const row of outputRows.slice(header + 1)) {
    if (!/^(output|focus|instance|window) /.test(row)) break;
    sessionRows.push(row);
  }
  // `/dev/console` forwards each complete line as a distinct driver callback.
  // Reassemble those callbacks and restore the serializer's final newline
  // before computing the byte-level durability fingerprint.
  const text = `${sessionRows.join("\n")}\n`;
  return { text, session: parseStoredSession(text) };
}

function canonicalFingerprint(text: string): {
  readonly bytes: number;
  readonly digest: string;
} {
  const encoded = new TextEncoder().encode(text);
  let digest = 0xcbf29ce484222325n;
  for (const byte of encoded) {
    digest ^= BigInt(byte);
    digest = BigInt.asUintN(64, digest * 0x100000001b3n);
  }
  return {
    bytes: encoded.byteLength,
    digest: digest.toString(16).padStart(16, "0"),
  };
}

function durableDiagnostics(lines: readonly string[]): DurableDiagnostic[] {
  const diagnostics: DurableDiagnostic[] = [];
  const pattern =
    /shell: session durable revision=([0-9]+) apps=([0-9]+) windows=([0-9]+) bytes=([0-9]+) digest=([0-9a-f]{16})(?=\s|$)/g;
  for (const line of lines) {
    for (const match of line.matchAll(pattern)) {
      diagnostics.push({
        revision: Number(match[1]),
        apps: Number(match[2]),
        windows: Number(match[3]),
        bytes: Number(match[4]),
        digest: match[5]!,
      });
    }
  }
  return diagnostics.sort((left, right) => left.revision - right.revision);
}

function durableRevisions(lines: readonly string[]): number[] {
  return durableDiagnostics(lines).map((diagnostic) => diagnostic.revision);
}

function latestDurableRevision(lines: readonly string[]): number {
  return durableRevisions(lines).at(-1) ?? 0;
}

async function waitForDurableSnapshot(
  page: Page,
  lines: readonly string[],
  afterRevision: number,
  predicate: (session: StoredSession) => boolean,
): Promise<DurableSnapshot> {
  let cursor = afterRevision;
  let lastText = "";
  for (let attempt = 0; attempt < 8; attempt += 1) {
    await expect
      .poll(
        () =>
          durableDiagnostics(lines).find(
            (diagnostic) => diagnostic.revision > cursor,
          )?.revision ?? null,
        {
          timeout: 10_000,
          message: `expected a durable session revision after ${cursor}`,
        },
      )
      .not.toBeNull();
    const diagnostic = durableDiagnostics(lines)
      .filter((candidate) => candidate.revision > cursor)
      .at(-1)!;
    const observed = await readCanonicalSession(page, lines);
    lastText = observed.text;
    const fingerprint = canonicalFingerprint(observed.text);
    const newestAfterRead = durableDiagnostics(lines).at(-1)!;
    if (
      newestAfterRead.revision === diagnostic.revision &&
      diagnostic.apps === APP_WINDOW_COUNT &&
      diagnostic.windows === APP_WINDOW_COUNT &&
      diagnostic.bytes === fingerprint.bytes &&
      diagnostic.digest === fingerprint.digest &&
      predicate(observed.session)
    ) {
      return {
        revision: diagnostic.revision,
        text: observed.text,
        session: observed.session,
      };
    }
    cursor = diagnostic.revision;
  }
  const fingerprint = canonicalFingerprint(lastText);
  throw new Error(
    `durable session never reached expected state (bytes=${fingerprint.bytes} digest=${fingerprint.digest}):\n${lastText}`,
  );
}

function countOccurrences(lines: readonly string[], needle: string): number {
  return lines.join("\n").split(needle).length - 1;
}

function assertHealthy(lines: readonly string[]): void {
  expect(lines.some((line) => line.includes("real kernel panic"))).toBe(false);
  expect(lines.some((line) => line.includes("user worker crashed pid="))).toBe(
    false,
  );
  expect(lines.some((line) => line.startsWith("[pageerror]"))).toBe(false);
}

function focusedTerminalPromptRegion(session: StoredSession): Region {
  const focused = session.windows.find(
    (window) => window.id === session.focusedWindow,
  );
  if (focused === undefined) {
    throw new Error("durable session has no focused window");
  }
  const instance = session.instances.find(
    (candidate) => candidate.id === focused.instanceId,
  );
  if (instance?.desktopEntryId !== "terminal" || focused.flags !== 0) {
    throw new Error("durable focus is not a visible normal Terminal window");
  }
  const region = {
    x: focused.normalX + 4,
    y: focused.normalY + focused.normalHeight - 32,
    width: focused.normalWidth - 8,
    height: 24,
  };
  if (
    region.x < 0 ||
    region.y < 0 ||
    region.width <= 0 ||
    region.height <= 0 ||
    region.x + region.width > FRAMEBUFFER_WIDTH ||
    region.y + region.height > FRAMEBUFFER_HEIGHT
  ) {
    throw new Error(
      `focused Terminal input region is outside the framebuffer: ${JSON.stringify(region)}`,
    );
  }
  return region;
}

async function measureRestoredTerminalInput(
  page: Page,
  lines: readonly string[],
  region: Region,
): Promise<CausalLatencySample[]> {
  const samples: CausalLatencySample[] = [];
  let hasCharacter = false;
  for (let index = 0; index < RESTORED_INPUT_SAMPLE_COUNT; index += 1) {
    const key = hasCharacter ? "Backspace" : "a";
    const code = hasCharacter ? "Backspace" : "KeyA";
    const before = await regionFingerprint(page, region);
    const id = index + 1;
    await armCausalSample(page, {
      id,
      kind: "key",
      input: "keydown",
      code,
      notBefore: 0,
      evidence: { kind: "fingerprint", region, before },
    });
    await page.keyboard.down(key);
    try {
      samples.push(await readCausalSample(page, id, lines));
    } finally {
      await page.keyboard.up(key);
    }
    hasCharacter = !hasCharacter;
  }
  if (hasCharacter) {
    throw new Error("restored input sample sequence did not return to baseline");
  }
  return samples;
}

test("a tab close restores the durable six-app session before warm readiness", async ({
  context,
  page,
  browserName,
}) => {
  test.skip(
    browserName === "webkit",
    "WebKit lacks the persistent OPFS substrate required by session restore.",
  );

  const firstLines: string[] = [];
  page.on("console", (message) => firstLines.push(message.text()));
  page.on("pageerror", (error) =>
    firstLines.push(`[pageerror] ${error.message}`),
  );
  await bootDesktop(page, firstLines);
  await waitForLineAfter(
    firstLines,
    0,
    (line) =>
      line.includes(
        "shell: loaded 5 applications from /usr/share/applications",
      ),
    "five-entry launcher catalog",
  );

  for (let index = 0; index < BUNDLED_APPS.length; index += 1) {
    await launchBundledApp(page, firstLines, BUNDLED_APPS[index]!, index);
  }
  await acknowledgeTerminalFocus(page, firstLines);
  await runTerminalCommandToPrompt(
    page,
    firstLines,
    `echo ${SENTINEL_TEXT} > /home/user/.session-restore-sentinel.tmp`,
  );
  await runTerminalCommandToPrompt(
    page,
    firstLines,
    `cp /home/user/.session-restore-sentinel.tmp ${SENTINEL_PATH}`,
  );
  await runTerminalCommand(
    page,
    firstLines,
    "echo session-sentinel-durable > /dev/console",
    (line) => line.includes("session-sentinel-durable"),
  );

  await focusTaskEntry(page, 2, TASK_ENTRY_COUNT, "Files");
  await expect
    .poll(() => colorBounds(page, FILES_TITLEBAR), { timeout: 5_000 })
    .not.toBeNull();
  const initialFiles = await colorBounds(page, FILES_TITLEBAR);
  if (initialFiles === null) throw new Error("Files titlebar was not visible");
  const draggedFiles = await dragFiles(page, firstLines, initialFiles);

  const maximizeLogStart = firstLines.length;
  const maximizeFrame = await frameSequence(page);
  await clickFramebuffer(
    page,
    taskbarControlPoint(2, TASK_ENTRY_COUNT, "maximize"),
  );
  await waitForLineAfter(
    firstLines,
    maximizeLogStart,
    (line) => line.includes("files: window maximized 1024x736"),
    "Files maximized configure",
    5_000,
  );
  await expect
    .poll(() => frameSequence(page), { timeout: 3_000 })
    .toBeGreaterThan(maximizeFrame);
  await expect
    .poll(() => pixel(page, { x: 900, y: 10 }), { timeout: 5_000 })
    .toEqual([...FILES_TITLEBAR]);

  await focusTaskEntry(page, 3, TASK_ENTRY_COUNT, "Edit");
  const minimizeFrame = await frameSequence(page);
  await clickFramebuffer(
    page,
    taskbarControlPoint(3, TASK_ENTRY_COUNT, "minimize"),
  );
  await expect
    .poll(async () => (await taskPalettes(page, TASK_ENTRY_COUNT))[3], {
      timeout: 5_000,
    })
    .toBe("minimized");
  await expect
    .poll(() => frameSequence(page), { timeout: 3_000 })
    .toBeGreaterThan(minimizeFrame);

  await focusTaskEntry(page, 5, TASK_ENTRY_COUNT, "System Monitor");
  const beforeTerminalOne = latestDurableRevision(firstLines);
  await focusTaskEntry(page, 1, TASK_ENTRY_COUNT, "Terminal 1");
  await expect
    .poll(() => pixel(page, { x: 700, y: 140 }), { timeout: 5_000 })
    .toEqual([...TERMINAL_BACKGROUND]);
  const interim = await waitForDurableSnapshot(
    page,
    firstLines,
    beforeTerminalOne,
    (session) => hasExpectedState(session, draggedFiles, 0),
  );

  // Settings is raised after Terminal 1, but it does not cover the x=700
  // Terminal/Sysmon overlap. The two probes therefore pin both neighboring
  // z-order relationships without relying on restored taskbar creation order.
  await focusTaskEntry(page, 4, TASK_ENTRY_COUNT, "Settings");
  await expect
    .poll(() => pixel(page, { x: 400, y: 100 }), { timeout: 5_000 })
    .toEqual([...SETTINGS_TITLEBAR]);
  await focusTaskEntry(page, 6, TASK_ENTRY_COUNT, "Terminal 2");
  const finalSnapshot = await waitForDurableSnapshot(
    page,
    firstLines,
    interim.revision,
    (session) => hasExpectedState(session, draggedFiles, 1),
  );
  const expectedSemanticSession = normalizedSession(finalSnapshot.session);
  const expectedMaxPixel = await pixel(page, { x: 900, y: 10 });
  const expectedSettingsPixel = await pixel(page, { x: 400, y: 100 });
  const expectedStackPixel = await pixel(page, { x: 700, y: 140 });
  const expectedTaskbarTailPixel = await pixel(page, TASKBAR_TAIL_POINT);
  expect(expectedMaxPixel).toEqual([...FILES_TITLEBAR]);
  expect(expectedSettingsPixel).toEqual([...SETTINGS_TITLEBAR]);
  expect(expectedStackPixel).toEqual([...TERMINAL_BACKGROUND]);
  // Seven entries end in the last close control at x=945. An eighth entry
  // replaces this pixel with the taskbar's overflow control.
  expect(expectedTaskbarTailPixel).toEqual([...TASKBAR_CLOSE_FILL]);
  expect(await taskPalettes(page, TASK_ENTRY_COUNT)).toEqual([
    "unfocused",
    "unfocused",
    "unfocused",
    "minimized",
    "unfocused",
    "unfocused",
    "focused",
  ]);
  assertHealthy(firstLines);

  await page.close();

  const secondPage = await context.newPage();
  await secondPage.addInitScript(() => {
    const state = window as unknown as {
      __pmosSplashDismissedAt?: number;
    };
    const observer = new MutationObserver((records) => {
      if (state.__pmosSplashDismissedAt !== undefined) return;
      for (const record of records) {
        for (const removed of record.removedNodes) {
          if (
            removed instanceof Element &&
            (removed.id === "pmos-boot-splash" ||
              removed.querySelector("#pmos-boot-splash") !== null)
          ) {
            state.__pmosSplashDismissedAt = performance.now();
            observer.disconnect();
            return;
          }
        }
      }
    });
    observer.observe(document, { childList: true, subtree: true });
  });
  const secondLines: string[] = [];
  secondPage.on("console", (message) => secondLines.push(message.text()));
  secondPage.on("pageerror", (error) =>
    secondLines.push(`[pageerror] ${error.message}`),
  );
  await secondPage.goto("/index.html");
  const restoredLine = await waitForLineAfter(
    secondLines,
    0,
    (line) =>
      line.endsWith(
        "shell: session restored status=completed apps=6 windows=6",
      ),
    "completed six-app restore",
  );
  const restoredLineIndex = secondLines.indexOf(restoredLine);
  if (restoredLineIndex < 0) {
    throw new Error("completed restore line vanished from the transcript");
  }
  const readyLine = await waitForLineAfter(
    secondLines,
    restoredLineIndex + 1,
    (line) => line.endsWith("shell: desktop ready"),
    "post-restore desktop-ready acknowledgement",
  );
  expect(restoredLine).toMatch(
    /shell: session restored status=completed apps=6 windows=6$/,
  );
  expect(readyLine).toMatch(/shell: desktop ready$/);
  await expect(secondPage.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 5_000,
  });
  const warmReadyMs = await secondPage.evaluate(
    () =>
      (window as unknown as { __pmosSplashDismissedAt?: number })
        .__pmosSplashDismissedAt ?? null,
  );
  if (
    warmReadyMs === null ||
    !Number.isFinite(warmReadyMs) ||
    warmReadyMs < 0
  ) {
    throw new Error("trusted splash dismissal timestamp is missing or invalid");
  }
  console.log(
    `[session-restore] warm_ready_ms=${warmReadyMs.toFixed(1)} engine=${browserName} apps=6 windows=6`,
  );
  expect(warmReadyMs, "six-app restored warm desktop readiness").toBeLessThan(
    3_000,
  );

  expect(countOccurrences(secondLines, "term: starting")).toBe(2);
  expect(countOccurrences(secondLines, "files: starting")).toBe(1);
  expect(countOccurrences(secondLines, "edit: starting")).toBe(1);
  expect(countOccurrences(secondLines, "sysmon: starting")).toBe(1);
  expect(countOccurrences(secondLines, "shell: launched ")).toBe(0);
  const restoredPalettes = await taskPalettes(secondPage, TASK_ENTRY_COUNT);
  expect(
    restoredPalettes.filter((palette) => palette === "focused"),
  ).toHaveLength(1);
  expect(
    restoredPalettes.filter((palette) => palette === "minimized"),
  ).toHaveLength(1);
  expect(
    restoredPalettes.filter((palette) => palette === "unfocused"),
  ).toHaveLength(5);
  expect(
    restoredPalettes[0],
    "the late shell wallpaper map must not steal restored app focus",
  ).toBe("unfocused");
  expect(restoredPalettes).not.toContain("other");
  expect(await pixel(secondPage, TASKBAR_TAIL_POINT)).toEqual(
    expectedTaskbarTailPixel,
  );
  expect(await pixel(secondPage, { x: 900, y: 10 })).toEqual(expectedMaxPixel);
  expect(await pixel(secondPage, { x: 400, y: 100 })).toEqual(
    expectedSettingsPixel,
  );
  expect(await pixel(secondPage, { x: 700, y: 140 })).toEqual(
    expectedStackPixel,
  );

  const restoredInputSamples = await measureRestoredTerminalInput(
    secondPage,
    secondLines,
    focusedTerminalPromptRegion(finalSnapshot.session),
  );
  expect(restoredInputSamples).toHaveLength(RESTORED_INPUT_SAMPLE_COUNT);
  const restoredInputTotals = restoredInputSamples.map(
    (sample) => sample.totalMs,
  );
  const restoredInputToMain = restoredInputSamples.map(
    (sample) => sample.inputToMainMs,
  );
  const restoredMainPaint = restoredInputSamples.map(
    (sample) => sample.mainPaintMs,
  );
  const restoredInputP50 = percentile(restoredInputTotals, 0.5);
  const restoredInputP95 = percentile(restoredInputTotals, 0.95);
  const restoredInputP99 = percentile(restoredInputTotals, 0.99);
  const restoredInputToMainP95 = percentile(restoredInputToMain, 0.95);
  const restoredMainPaintP95 = percentile(restoredMainPaint, 0.95);
  const restoredMetrics = [
    restoredInputP50,
    restoredInputP95,
    restoredInputP99,
    restoredInputToMainP95,
    restoredMainPaintP95,
  ];
  if (!restoredMetrics.every(Number.isFinite)) {
    throw new Error(
      `restored input produced a non-finite metric: ${restoredMetrics.join(",")}`,
    );
  }
  console.log(
    `[session-restore-input] engine=${browserName} apps=6 samples=${restoredInputSamples.length} ` +
      `p50_ms=${restoredInputP50.toFixed(1)} p95_ms=${restoredInputP95.toFixed(1)} ` +
      `p99_ms=${restoredInputP99.toFixed(1)} input_to_main_p95_ms=${restoredInputToMainP95.toFixed(1)} ` +
      `main_paint_p95_ms=${restoredMainPaintP95.toFixed(1)}`,
  );
  expect(
    restoredInputP95,
    "restored six-app Terminal input-to-pixel p95",
  ).toBeLessThan(LATENCY_BUDGET_MS);

  const sentinel = await runTerminalCommand(
    secondPage,
    secondLines,
    `cat ${SENTINEL_PATH} > /dev/console`,
    (line) => line.includes(SENTINEL_TEXT),
  );
  expect(sentinel).toContain(SENTINEL_TEXT);
  // This background rewrite is intentionally outside the warm-ready budget.
  // Always bind a fresh canonical-file read to the post-restore writer event,
  // even when the resulting bytes happen to equal the pre-close snapshot.
  const restoredSession = await waitForDurableSnapshot(
    secondPage,
    secondLines,
    0,
    (session) =>
      JSON.stringify(normalizedSession(session)) ===
      JSON.stringify(expectedSemanticSession),
  );
  expect(normalizedSession(restoredSession.session)).toEqual(
    expectedSemanticSession,
  );
  expect(countOccurrences(secondLines, "term: starting")).toBe(2);
  expect(countOccurrences(secondLines, "shell: launched ")).toBe(0);

  const previouslyFocusedEntry = restoredPalettes.findIndex(
    (palette) => palette === "focused",
  );
  const filesFocusFrame = await frameSequence(secondPage);
  await clickFramebuffer(secondPage, { x: 900, y: 100 });
  await expect
    .poll(() => frameSequence(secondPage), { timeout: 3_000 })
    .toBeGreaterThan(filesFocusFrame);
  await expect
    .poll(
      async () => {
        const palettes = await taskPalettes(secondPage, TASK_ENTRY_COUNT);
        const focused = palettes.findIndex((palette) => palette === "focused");
        return focused >= 0 && focused !== previouslyFocusedEntry
          ? focused
          : null;
      },
      { timeout: 5_000 },
    )
    .not.toBeNull();
  const filesEntry = (
    await taskPalettes(secondPage, TASK_ENTRY_COUNT)
  ).findIndex((palette) => palette === "focused");
  expect(filesEntry).toBeGreaterThanOrEqual(0);

  const restoreLogStart = secondLines.length;
  const unmaximizeFrame = await frameSequence(secondPage);
  await clickFramebuffer(
    secondPage,
    taskbarControlPoint(filesEntry, TASK_ENTRY_COUNT, "maximize"),
  );
  await waitForLineAfter(
    secondLines,
    restoreLogStart,
    (line) => line.includes("files: window restored 640x420"),
    "Files normal-size restore",
    5_000,
  );
  await expect
    .poll(
      async () => {
        if ((await frameSequence(secondPage)) <= unmaximizeFrame) return null;
        const bounds = await colorBounds(secondPage, FILES_TITLEBAR);
        return bounds === null
          ? null
          : { x: bounds.x, y: bounds.y, right: bounds.right };
      },
      {
        timeout: 5_000,
        message: "Files did not present its exact stored normal geometry",
      },
    )
    .toEqual({
      x: draggedFiles.x,
      y: draggedFiles.y,
      right: draggedFiles.right,
    });

  expect(countOccurrences(secondLines, "term: starting")).toBe(2);
  expect(countOccurrences(secondLines, "files: starting")).toBe(1);
  expect(countOccurrences(secondLines, "edit: starting")).toBe(1);
  expect(countOccurrences(secondLines, "sysmon: starting")).toBe(1);
  expect(countOccurrences(secondLines, "shell: launched ")).toBe(0);
  assertHealthy([...firstLines, ...secondLines]);
});
