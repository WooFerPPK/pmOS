import { expect, test, type Page } from "@playwright/test";

test.use({ viewport: { width: 1280, height: 900 } });

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const DOCUMENT_PATH = "/home/user/Documents/pmos-edit-daily.txt";
const DOCUMENT_NAME = "pmos-edit-daily.txt";
const FIRST_LINE = "daily edit survives reload";
const SECOND_LINE = "not lost";
const FINAL_DOCUMENT = `${FIRST_LINE}\n${SECOND_LINE}`;

async function clickFramebuffer(page: Page, x: number, y: number): Promise<void> {
  const canvas = page.locator("#pmos-fb");
  const box = await canvas.boundingBox();
  if (box === null) throw new Error("framebuffer canvas has no layout box");
  await page.mouse.click(
    box.x + (x / FRAMEBUFFER_WIDTH) * box.width,
    box.y + (y / FRAMEBUFFER_HEIGHT) * box.height,
  );
}

async function clickFramebufferAndWaitForPaint(
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
        Number(
          (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
        ),
      { timeout: 3_000 },
    )
    .toBeGreaterThan(before);
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
      message: `expected OS console line after ${start}; observed:\n${lines.join("\n")}`,
    })
    .not.toBeNull();
  return lines.slice(start).find(predicate)!;
}

async function findEditorOrigin(
  page: Page,
): Promise<{ x: number; y: number } | null> {
  return page.locator("#pmos-fb").evaluate((canvas: HTMLCanvasElement) => {
    const context = canvas.getContext("2d");
    if (context === null) return null;
    const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
    for (let y = 0; y < canvas.height; y += 1) {
      for (let x = 0; x < canvas.width; x += 1) {
        const index = (y * canvas.width + x) * 4;
        if (
          pixels[index] === 0x6a &&
          pixels[index + 1] === 0x4a &&
          pixels[index + 2] === 0x8a
        ) {
          return { x, y };
        }
      }
    }
    return null;
  });
}

async function waitForEditorPaint(page: Page): Promise<void> {
  await expect
    .poll(() => findEditorOrigin(page), { timeout: 10_000 })
    .not.toBeNull();
}

async function bootDesktop(page: Page, lines: readonly string[]): Promise<void> {
  const start = lines.length;
  await page.goto("/index.html");
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 12_000,
  });
  await waitForLineAfter(lines, start, (line) =>
    line.includes("persistent OPFS root mounted at /"),
  );
  await waitForLineAfter(lines, start, (line) =>
    line.includes("shell: loaded 5 applications from /usr/share/applications"),
  );
}

async function launchEditor(page: Page, lines: readonly string[]): Promise<number> {
  const start = lines.length;
  // Bundled launcher order is Terminal, Files, Edit, Settings, Sysmon. Rows
  // grow upward from the taskbar; Edit is the third row at framebuffer y=672.
  await clickFramebufferAndWaitForPaint(page, 40, 752);
  await clickFramebuffer(page, 100, 672);
  await waitForLineAfter(lines, start, (line) => line.includes("edit: starting"));
  await waitForLineAfter(lines, start, (line) => line.includes("edit: ready path="));
  await waitForEditorPaint(page);
  return start;
}

async function workerCount(page: Page): Promise<number> {
  return Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
}

async function waitForOneWorkerToExit(page: Page, before: number): Promise<void> {
  await expect.poll(() => workerCount(page), { timeout: 5_000 }).toBeLessThan(before);
}

async function openSavedDocument(
  page: Page,
  lines: readonly string[],
): Promise<void> {
  const start = lines.length;
  await page.keyboard.press("Control+o");
  await waitForLineAfter(
    lines,
    start,
    (line) => line.includes("edit: open: enter a VFS path"),
  );
  // Open starts in /home/user/Documents/, so only the filename is needed.
  await page.keyboard.type(DOCUMENT_NAME);
  await page.keyboard.press("Enter");
  await waitForLineAfter(
    lines,
    start,
    (line) =>
      line.includes(`edit: opened ${DOCUMENT_PATH} bytes=${FINAL_DOCUMENT.length}`),
  );
}

test("Edit saves a new file, guards dirty close, and preserves exact VFS contents across reload", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));
  page.on("pageerror", (error) =>
    consoleLines.push(`[pageerror] ${error.message}`),
  );

  await bootDesktop(page, consoleLines);
  await launchEditor(page, consoleLines);

  let start = consoleLines.length;
  await page.keyboard.press("Control+n");
  await waitForLineAfter(
    consoleLines,
    start,
    (line) => line.includes("edit: new document"),
  );
  await page.keyboard.type(FIRST_LINE);
  await page.keyboard.press("Enter");

  start = consoleLines.length;
  await page.keyboard.press("Control+Shift+s");
  await waitForLineAfter(
    consoleLines,
    start,
    (line) => line.includes("edit: save as: enter a VFS path"),
  );
  await page.keyboard.type(DOCUMENT_NAME);
  await page.keyboard.press("Enter");
  await waitForLineAfter(
    consoleLines,
    start,
    (line) => line.includes(`edit: saved ${DOCUMENT_PATH} bytes=${FIRST_LINE.length + 1}`),
  );

  // Make the bound document dirty, cancel the first close request, then use
  // the prompt's Save choice. The worker surviving Cancel proves the prompt
  // did not merely paint before the process exited.
  await page.keyboard.type(SECOND_LINE);
  const editorWorkers = await workerCount(page);
  start = consoleLines.length;
  // Use the shell taskbar's close control for the first request. The display
  // server must reactivate Edit before delivering advisory xdg_toplevel.close;
  // otherwise the full-screen shell surface hides the veto prompt and steals
  // its keyboard input. With only Edit open, (400, 752) is its close button.
  await clickFramebuffer(page, 400, 752);
  await waitForLineAfter(
    consoleLines,
    start,
    (line) => line.includes("edit: unsaved changes: save, discard, or cancel"),
  );
  await page.keyboard.press("c");
  await waitForLineAfter(
    consoleLines,
    start,
    (line) => line.includes("edit: close cancelled"),
  );
  expect(await workerCount(page)).toBe(editorWorkers);

  start = consoleLines.length;
  await page.keyboard.press("Control+q");
  await waitForLineAfter(
    consoleLines,
    start,
    (line) => line.includes("edit: unsaved changes: save, discard, or cancel"),
  );
  await page.keyboard.press("s");
  await waitForLineAfter(
    consoleLines,
    start,
    (line) => line.includes(`edit: saved ${DOCUMENT_PATH} bytes=${FINAL_DOCUMENT.length}`),
  );
  await waitForOneWorkerToExit(page, editorWorkers);

  // Recreate the full page/kernel while retaining this browser context's
  // OPFS, then open the saved path through Edit's ordinary path dialog.
  await bootDesktop(page, consoleLines);
  await launchEditor(page, consoleLines);
  await openSavedDocument(page, consoleLines);

  // Discard one in-memory byte and close. A second kernel recreation must
  // still report the exact saved byte count, proving Discard did not write.
  await page.keyboard.type("x");
  const reopenedWorkers = await workerCount(page);
  start = consoleLines.length;
  await page.keyboard.press("Control+q");
  await waitForLineAfter(
    consoleLines,
    start,
    (line) => line.includes("edit: unsaved changes: save, discard, or cancel"),
  );
  await page.keyboard.press("d");
  await waitForOneWorkerToExit(page, reopenedWorkers);

  await bootDesktop(page, consoleLines);
  await launchEditor(page, consoleLines);
  await openSavedDocument(page, consoleLines);

  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(false);
  expect(consoleLines.some((line) => line.startsWith("[pageerror]"))).toBe(false);
});
