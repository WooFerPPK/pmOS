import { expect, type Page } from "@playwright/test";

import {
  clickFramebuffer as clickFramebufferPoint,
  launcherMenuIsOpen,
  openLauncherBefore,
  selectLauncherRowBefore,
} from "./launcher-interaction";

const TERMINAL_BACKGROUND = [0x14, 0x0e, 0x0a, 0xff] as const;
const TERMINAL_OUTPUT = [0xe6, 0xe6, 0xe6, 0xff] as const;
const SHIFTED_PHYSICAL_KEYS: Readonly<Record<string, string>> = {
  "~": "Backquote",
  "!": "Digit1",
  "@": "Digit2",
  "#": "Digit3",
  $: "Digit4",
  "%": "Digit5",
  "^": "Digit6",
  "&": "Digit7",
  "*": "Digit8",
  "(": "Digit9",
  ")": "Digit0",
  _: "Minus",
  "+": "Equal",
  "{": "BracketLeft",
  "}": "BracketRight",
  "|": "Backslash",
  ":": "Semicolon",
  '"': "Quote",
  "<": "Comma",
  ">": "Period",
  "?": "Slash",
} as const;
let terminalFocusSequence = 0;
let terminalCommandSequence = 0;

export async function clickFramebuffer(
  page: Page,
  x: number,
  y: number,
): Promise<void> {
  await clickFramebufferPoint(page, x, y);
}

export async function waitForLine(
  lines: readonly string[],
  predicate: (line: string) => boolean,
  timeout = 12_000,
): Promise<string> {
  await expect
    .poll(() => lines.find(predicate) ?? null, {
      timeout,
      message: `expected guest console evidence; observed:\n${lines.join("\n")}`,
    })
    .not.toBeNull();
  return lines.find(predicate)!;
}

export async function bootDesktop(
  page: Page,
  lines: readonly string[],
  timeout = 15_000,
): Promise<void> {
  await page.goto("/index.html");
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout,
  });
  await waitForLine(
    lines,
    (line) => line.includes("persistent OPFS root mounted at /"),
    timeout,
  );
  await waitForLine(
    lines,
    (line) => line.includes("shell: connected to /run/display"),
    timeout,
  );
}

async function terminalContentPoint(
  page: Page,
): Promise<{ x: number; y: number } | null> {
  return page.evaluate((target) => {
    const canvas = document.querySelector<HTMLCanvasElement>("#pmos-fb");
    const context = canvas?.getContext("2d");
    if (canvas === null || canvas === undefined || context == null) return null;
    const { width, height } = canvas;
    const bytes = context.getImageData(0, 0, width, height).data;
    const matches = (x: number, y: number): boolean => {
      const offset = (y * width + x) * 4;
      return (
        bytes[offset] === target[0] &&
        bytes[offset + 1] === target[1] &&
        bytes[offset + 2] === target[2] &&
        bytes[offset + 3] === target[3]
      );
    };
    let best: { x: number; y: number; width: number } | null = null;
    for (let y = 16; y < height - 16; y += 2) {
      let runStart = -1;
      for (let x = 0; x <= width; x += 1) {
        const matched = x < width && matches(x, y);
        if (matched && runStart < 0) runStart = x;
        if (!matched && runStart >= 0) {
          const runWidth = x - runStart;
          const centerX = runStart + Math.floor(runWidth / 2);
          let verticalMatches = 0;
          for (let sampleY = y - 16; sampleY <= y + 16; sampleY += 2) {
            if (matches(centerX, sampleY)) verticalMatches += 1;
          }
          if (
            runWidth >= 320 &&
            verticalMatches >= 12 &&
            (best === null || runWidth > best.width || y > best.y)
          ) {
            best = { x: centerX, y, width: runWidth };
          }
          runStart = -1;
        }
      }
    }
    return best === null ? null : { x: best.x, y: best.y };
  }, TERMINAL_BACKGROUND);
}

async function terminalPromptRegion(
  page: Page,
): Promise<{ x: number; y: number; width: number; height: number } | null> {
  return page.evaluate((target) => {
    const canvas = document.querySelector<HTMLCanvasElement>("#pmos-fb");
    const context = canvas?.getContext("2d");
    if (canvas === null || canvas === undefined || context == null) return null;
    const { width, height } = canvas;
    const bytes = context.getImageData(0, 0, width, height).data;
    let best: { x: number; y: number; width: number } | null = null;
    for (let y = 0; y < height; y += 1) {
      let runStart = -1;
      for (let x = 0; x <= width; x += 1) {
        const offset = (y * width + x) * 4;
        const matched =
          x < width &&
          bytes[offset] === target[0] &&
          bytes[offset + 1] === target[1] &&
          bytes[offset + 2] === target[2] &&
          bytes[offset + 3] === target[3];
        if (matched && runStart < 0) runStart = x;
        if (!matched && runStart >= 0) {
          const runWidth = x - runStart;
          if (runWidth >= 320 && (best === null || y > best.y)) {
            best = { x: runStart, y, width: runWidth };
          }
          runStart = -1;
        }
      }
    }
    if (best === null || best.y < 32) return null;
    return {
      x: best.x + 4,
      y: best.y - 31,
      width: best.width - 8,
      height: 24,
    };
  }, TERMINAL_BACKGROUND);
}

async function framebufferRegionFingerprint(
  page: Page,
  region: { x: number; y: number; width: number; height: number },
): Promise<number> {
  return page.evaluate(({ x, y, width, height }) => {
    const canvas = document.querySelector<HTMLCanvasElement>("#pmos-fb");
    const context = canvas?.getContext("2d");
    if (canvas === null || canvas === undefined || context == null) return 0;
    const bytes = context.getImageData(x, y, width, height).data;
    let hash = 0x811c9dc5;
    for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193);
    return hash >>> 0;
  }, region);
}

export async function launchTerminal(
  page: Page,
  lines: readonly string[],
  options: {
    launcherAlreadyOpen?: boolean;
    launcherRowY?: number;
    timeout?: number;
  } = {},
): Promise<void> {
  const timeout = options.timeout ?? 10_000;
  const launcherTimeout = Math.min(timeout, 5_000);
  const startsBefore = lines.filter((line) =>
    line.includes("term: starting"),
  ).length;
  const workersBefore = Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
  const closedFingerprint =
    options.launcherAlreadyOpen === true
      ? undefined
      : await openLauncherBefore(page, Date.now() + launcherTimeout);
  if (options.launcherAlreadyOpen === true) {
    expect(
      await launcherMenuIsOpen(page),
      "Terminal launch requires the launcher menu to be open",
    ).toBe(true);
  }
  await selectLauncherRowBefore(
    page,
    100,
    options.launcherRowY ?? 624,
    Date.now() + launcherTimeout,
    closedFingerprint,
  );
  await expect
    .poll(
      () => lines.filter((line) => line.includes("term: starting")).length,
      {
        timeout,
        message: `Terminal did not start:\n${lines.join("\n")}`,
      },
    )
    .toBe(startsBefore + 1);
  await expect
    .poll(() => terminalContentPoint(page), {
      timeout,
      message: "expected a contiguous mapped Terminal content region",
    })
    .not.toBeNull();
  const content = await terminalContentPoint(page);
  if (content === null)
    throw new Error("Terminal content vanished before focus");
  // Pointer routing into a mapped client surface applies focus and the
  // embedded coordinates together. The console acknowledgement below then
  // proves that subsequent keyboard events reached Term's persistent shell.
  await clickFramebuffer(page, content.x, content.y);
  await expect
    .poll(
      async () =>
        Number(
          (await page.locator("body").getAttribute("data-pmos-live-workers")) ??
            "0",
        ),
      {
        timeout,
        message: "Terminal's persistent shell child did not start",
      },
    )
    .toBeGreaterThanOrEqual(workersBefore + 2);

  await acknowledgeTerminalFocus(page, lines, timeout);
}

export async function acknowledgeTerminalFocus(
  page: Page,
  lines: readonly string[],
  timeout = 10_000,
): Promise<void> {
  const focusMarker = `terminal-focus-ready-${++terminalFocusSequence}`;
  const focusStart = lines.length;
  await typePhysicalText(page, `echo ${focusMarker} > /dev/console`);
  await page.keyboard.press("Enter");
  await expect
    .poll(
      () =>
        lines
          .slice(focusStart)
          .some((line) => line === `[real-kernel] ${focusMarker}`),
      {
        timeout,
        message: `Terminal did not execute its focus acknowledgement:\n${lines
          .slice(focusStart)
          .join("\n")}`,
      },
    )
    .toBe(true);
}

async function terminalOutputPixelCount(page: Page): Promise<number> {
  return page.evaluate(
    ({ background, output }) => {
      const canvas = document.querySelector<HTMLCanvasElement>("#pmos-fb");
      const context = canvas?.getContext("2d");
      if (canvas === null || canvas === undefined || context == null) return -1;
      const { width, height } = canvas;
      const bytes = context.getImageData(0, 0, width, height).data;
      const matches = (
        x: number,
        y: number,
        rgba: readonly number[],
      ): boolean => {
        const offset = (y * width + x) * 4;
        return rgba.every(
          (channel, index) => bytes[offset + index] === channel,
        );
      };

      let best: { x: number; y: number; width: number } | null = null;
      for (let y = 0; y < height; y += 1) {
        let runStart = -1;
        for (let x = 0; x <= width; x += 1) {
          const matched = x < width && matches(x, y, background);
          if (matched && runStart < 0) runStart = x;
          if (!matched && runStart >= 0) {
            const runWidth = x - runStart;
            if (runWidth >= 320 && (best === null || runWidth > best.width)) {
              best = { x: runStart, y, width: runWidth };
            }
            runStart = -1;
          }
        }
      }
      if (best === null) return -1;

      const probeX = best.x + best.width - 4;
      let top = best.y;
      let bottom = best.y;
      while (top > 0 && matches(probeX, top - 1, background)) top -= 1;
      while (bottom + 1 < height && matches(probeX, bottom + 1, background)) {
        bottom += 1;
      }

      let count = 0;
      for (let y = top; y <= bottom; y += 1) {
        for (let x = best.x; x < best.x + best.width; x += 1) {
          if (matches(x, y, output)) count += 1;
        }
      }
      return count;
    },
    { background: TERMINAL_BACKGROUND, output: TERMINAL_OUTPUT },
  );
}

export async function typePhysicalText(
  page: Page,
  text: string,
): Promise<void> {
  let ordinary = "";
  const flush = async (): Promise<void> => {
    if (ordinary.length > 0) {
      await page.keyboard.type(ordinary, { delay: 2 });
      ordinary = "";
    }
  };
  for (const character of text) {
    const shiftedCode = SHIFTED_PHYSICAL_KEYS[character];
    if (shiftedCode !== undefined) {
      await flush();
      await page.keyboard.press(`Shift+${shiftedCode}`);
    } else if (/^[A-Z]$/.test(character)) {
      await flush();
      await page.keyboard.press(`Shift+Key${character}`);
    } else {
      ordinary += character;
    }
  }
  await flush();
}

async function waitForTerminalCommandMarker(
  page: Page,
  lines: readonly string[],
  label: string,
  timeout = 12_000,
): Promise<void> {
  const marker = `terminal-command-ready-${++terminalCommandSequence}`;
  const start = lines.length;
  await typePhysicalText(page, `echo ${marker} > /dev/console`);
  await page.keyboard.press("Enter");
  await expect
    .poll(() => lines.slice(start).some((line) => line.includes(marker)), {
      timeout,
      message: `Terminal command did not return: ${label}\n${lines
        .slice(start)
        .join("\n")}`,
    })
    .toBe(true);
}

export async function submitTerminalCommand(
  page: Page,
  command: string,
): Promise<void> {
  const prompt = await terminalPromptRegion(page);
  if (prompt === null) throw new Error("Terminal prompt region was not found");
  const before = await framebufferRegionFingerprint(page, prompt);
  await typePhysicalText(page, command);
  await expect
    .poll(() => framebufferRegionFingerprint(page, prompt), {
      timeout: 5_000,
      message: `Terminal did not paint command input: ${command}`,
    })
    .not.toBe(before);
  await page.keyboard.press("Enter");
}

export async function runEchoHelloAndWaitForOutput(page: Page): Promise<void> {
  const before = await terminalOutputPixelCount(page);
  expect(
    before,
    "expected a mapped Terminal content region",
  ).toBeGreaterThanOrEqual(0);
  await submitTerminalCommand(page, "echo hello");
  await expect
    .poll(() => terminalOutputPixelCount(page), {
      timeout: 5_000,
      message: "`echo hello` did not paint guest stdout in Terminal",
    })
    .toBeGreaterThan(before);
}

export async function runTerminalCommandToPrompt(
  page: Page,
  lines: readonly string[],
  command: string,
  timeout = 12_000,
): Promise<void> {
  const prompt = await terminalPromptRegion(page);
  if (prompt === null) throw new Error("Terminal prompt region was not found");
  const empty = await framebufferRegionFingerprint(page, prompt);
  await typePhysicalText(page, command);
  await expect
    .poll(() => framebufferRegionFingerprint(page, prompt), {
      timeout: 5_000,
      message: `Terminal did not paint command input: ${command}`,
    })
    .not.toBe(empty);
  await page.keyboard.press("Enter");
  await waitForTerminalCommandMarker(page, lines, command, timeout);
}

export async function runTerminalCommand(
  page: Page,
  lines: readonly string[],
  command: string,
  evidence: (line: string) => boolean,
  timeout = 12_000,
): Promise<string> {
  const start = lines.length;
  await submitTerminalCommand(page, command);
  await expect
    .poll(() => lines.slice(start).find(evidence) ?? null, {
      timeout,
      message: `expected command evidence; observed:\n${lines.slice(start).join("\n")}`,
    })
    .not.toBeNull();
  const matched = lines.slice(start).find(evidence)!;
  await waitForTerminalCommandMarker(page, lines, command, timeout);
  return matched;
}

export async function runTerminalCommandWithStatus(
  page: Page,
  lines: readonly string[],
  command: string,
  label: string,
  timeout = 12_000,
): Promise<number> {
  const start = lines.length;
  await submitTerminalCommand(page, command);
  await typePhysicalText(page, `echo ${label}=$? > /dev/console`);
  await page.keyboard.press("Enter");
  const statusPattern = new RegExp(`^\\[real-kernel\\] ${label}=([0-9]+)$`);
  await expect
    .poll(
      () =>
        lines
          .slice(start)
          .map((line) => line.match(statusPattern))
          .find((match) => match !== null)?.[1] ?? null,
      {
        timeout,
        message: `Terminal command did not report status: ${command}\n${lines
          .slice(start)
          .join("\n")}`,
      },
    )
    .not.toBeNull();
  const match = lines
    .slice(start)
    .map((line) => line.match(statusPattern))
    .find((candidate) => candidate !== null);
  if (match === undefined) throw new Error(`missing status for ${label}`);
  await waitForTerminalCommandMarker(page, lines, command, timeout);
  return Number(match[1]);
}
