import { expect, test, type Page } from "@playwright/test";
import {
  LIGHT_TITLEBAR,
  TASKBAR_DARK_FOCUSED,
  TASKBAR_LIGHT_FOCUSED,
  taskbarEntryPoint,
  titlebarControlPoint,
  waitForActiveWindowBounds,
} from "./windows-ui";

test.use({ viewport: { width: 1280, height: 900 } });

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;

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
        Number((await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0"),
      { timeout: 3_000 },
    )
    .toBeGreaterThan(before);
}

async function waitForLine(
  lines: string[],
  predicate: (line: string) => boolean,
): Promise<string> {
  await expect
    .poll(() => lines.find(predicate) ?? null, {
      timeout: 10_000,
      message: `expected OS console line; observed:\n${lines.join("\n")}`,
    })
    .not.toBeNull();
  return lines.find(predicate)!;
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

async function liveWorkerCount(page: Page): Promise<number> {
  return Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
}

async function framebufferRegionFingerprint(
  page: Page,
  region: { x: number; y: number; width: number; height: number },
): Promise<number> {
  return page.evaluate(({ x, y, width, height }) => {
    const canvas = document.querySelector<HTMLCanvasElement>("#pmos-fb");
    const context = canvas?.getContext("2d");
    if (context === null || context === undefined) return 0;
    const bytes = context.getImageData(x, y, width, height).data;
    let hash = 0x811c9dc5;
    for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193);
    return hash >>> 0;
  }, region);
}

async function typeDialog(
  page: Page,
  value: string,
  filesOrigin: { x: number; y: number },
): Promise<void> {
  await waitForFilesDialog(page, filesOrigin, value);
  const inputRegion = {
    x: filesOrigin.x + 70,
    y: filesOrigin.y + 192,
    width: 490,
    height: 24,
  };
  const before = await framebufferRegionFingerprint(page, inputRegion);
  await page.keyboard.type(value);
  await expect
    .poll(() => framebufferRegionFingerprint(page, inputRegion), {
      timeout: 3_000,
      message: `Files did not paint the dialog value ${value}`,
    })
    .not.toBe(before);
  await page.keyboard.press("Enter");
}

async function waitForFilesDialog(
  page: Page,
  filesOrigin: { x: number; y: number },
  label: string,
): Promise<void> {
  const dialogBackground = filesDialogBackground(filesOrigin);
  await expect
    .poll(() => framebufferRgb(page, dialogBackground.x, dialogBackground.y), {
      timeout: 3_000,
      message: `Files did not present its ${label} dialog`,
    })
    .toBe("89,105,120");
}

function filesDialogBackground(filesOrigin: { x: number; y: number }): {
  x: number;
  y: number;
} {
  return {
    x: filesOrigin.x + 62,
    y: filesOrigin.y + 162,
  };
}

async function framebufferRgb(
  page: Page,
  x: number,
  y: number,
): Promise<string> {
  return page.evaluate(
    ({ sampleX, sampleY }) => {
      const canvas = document.querySelector<HTMLCanvasElement>("#pmos-fb");
      const context = canvas?.getContext("2d");
      if (context === null || context === undefined) return "missing";
      const pixel = context.getImageData(sampleX, sampleY, 1, 1).data;
      return `${pixel[0]},${pixel[1]},${pixel[2]}`;
    },
    { sampleX: x, sampleY: y },
  );
}

async function focusTaskbarEntry(
  page: Page,
  index: number,
  entryCount: number,
): Promise<void> {
  const point = taskbarEntryPoint(index, entryCount);
  const alreadyFocused = await page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, sample) => {
      const context = canvas.getContext("2d");
      if (context === null) return false;
      const rgba = Array.from(
        context.getImageData(sample.point.x, sample.point.y, 1, 1).data,
      );
      return sample.palettes.some((palette) =>
        palette.every((channel, offset) => rgba[offset] === channel),
      );
    },
    {
      point,
      palettes: [TASKBAR_LIGHT_FOCUSED, TASKBAR_DARK_FOCUSED],
    },
  );
  if (!alreadyFocused) await clickFramebuffer(page, point.x, point.y);
  await waitForFocusedTaskbarEntry(page, index, entryCount);
}

async function waitForFocusedTaskbarEntry(
  page: Page,
  index: number,
  entryCount: number,
): Promise<void> {
  const point = taskbarEntryPoint(index, entryCount);
  await expect
    .poll(
      async () => {
        const rgba = await page.locator("#pmos-fb").evaluate(
          (canvas: HTMLCanvasElement, sample) => {
            const context = canvas.getContext("2d");
            if (context === null) return [];
            return Array.from(
              context.getImageData(sample.x, sample.y, 1, 1).data,
            );
          },
          point,
        );
        return [TASKBAR_LIGHT_FOCUSED, TASKBAR_DARK_FOCUSED].some(
          (palette) =>
            palette.every(
              (channel, channelIndex) => rgba[channelIndex] === channel,
            ),
        );
      },
      { timeout: 5_000 },
    )
    .toBe(true);
}

test("Files creates, navigates, pointer-selects, renames, deletes, and closes through the real OS", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));
  page.on("pageerror", (error) =>
    consoleLines.push(`[pageerror] ${error.message}`),
  );

  await page.goto("/index.html");
  try {
    await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
      timeout: 10_000,
    });
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    throw new Error(`${detail}\nOS console:\n${consoleLines.join("\n")}`);
  }
  const storageLine = await waitForLine(consoleLines, (line) =>
    line.includes("persistent OPFS root mounted at /"),
  );
  expect(storageLine).toContain("persistent OPFS root mounted at /");
  await waitForLine(consoleLines, (line) =>
    line.includes("shell: loaded 5 applications from /usr/share/applications"),
  );
  await waitForLine(consoleLines, (line) =>
    line.includes("shell: connected to /run/display"),
  );
  await waitForLine(consoleLines, (line) =>
    /display-server served client 0/.test(line),
  );
  await expect
    .poll(
      async () =>
        Number(
          (await page
            .locator("#pmos-fb")
            .getAttribute("data-pmos-frame-sequence")) ?? "0",
        ),
      { timeout: 5_000 },
    )
    .toBeGreaterThan(0);

  // Open the shell launcher and click its Files row. The popup grows upward
  // from the taskbar: Term is row 1 at y=624 and Files is row 2 at y=648.
  await clickFramebufferAndWaitForPaint(page, 40, 752);
  await clickFramebuffer(page, 100, 648);
  await waitForLine(consoleLines, (line) => line.includes("files: starting"));
  const readyLine = await waitForLine(consoleLines, (line) =>
    /files: ready \/.*$/.test(line),
  );
  const base = readyLine.match(/files: ready (\/.*)$/)?.[1];
  expect(base, readyLine).toBeDefined();
  const root = base!;
  const workflow =
    root === "/" ? "/zz-files-workflow" : `${root}/zz-files-workflow`;
  const drafts = `${workflow}/drafts`;
  const archive = `${workflow}/archive`;

  // `files: ready` is emitted immediately after the client submits its first
  // surface commit. Wait until the compositor has actually presented Files'
  // focused shared frame before sending a hit-tested pointer event.
  const filesOrigin = await waitForActiveWindowBounds(page, {
    timeout: 10_000,
  });

  const firstFrame = Number(
    (await page.locator("#pmos-fb").getAttribute("data-pmos-frame-sequence")) ??
      "0",
  );

  // Click the resolved Files frame's first list row to focus the ordinary
  // display client before typing.
  await clickFramebuffer(page, filesOrigin!.x + 88, filesOrigin!.y + 109);

  await page.keyboard.press("n");
  await typeDialog(page, "zz-files-workflow", filesOrigin!);
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: created folder ${workflow}`,
  );

  // Create selects the new entry; Enter opens it through the PMos VFS.
  await page.keyboard.press("Enter");
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: cwd ${workflow}`,
  );

  await page.keyboard.press("n");
  await typeDialog(page, "drafts", filesOrigin!);
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: created folder ${drafts}`,
  );

  // Escape clears the keyboard selection, then a real framebuffer pointer
  // press selects row zero. Keyboard and mouse use independent device queues,
  // so observe each visible state before crossing devices; otherwise one
  // display turn may drain the later mouse press before the earlier Escape.
  // Rename therefore depends on pointer routing, not on the create action's
  // previous selection.
  const firstRowFill = {
    x: filesOrigin!.x + 4,
    y: filesOrigin!.y + 109,
  };
  await expect
    .poll(() => framebufferRgb(page, firstRowFill.x, firstRowFill.y), {
      timeout: 3_000,
      message: "Files did not present the created-folder selection",
    })
    .toBe("0,103,192");
  const frameBeforeClear = Number(
    (await page.locator("#pmos-fb").getAttribute("data-pmos-frame-sequence")) ??
      "0",
  );
  await page.keyboard.press("Escape");
  await expect
    .poll(
      async () => {
        const frame = Number(
          (await page
            .locator("#pmos-fb")
            .getAttribute("data-pmos-frame-sequence")) ?? "0",
        );
        return (
          frame > frameBeforeClear &&
          (await framebufferRgb(page, firstRowFill.x, firstRowFill.y)) ===
            "243,243,243"
        );
      },
      {
        timeout: 3_000,
        message: "Files did not present its cleared selection",
      },
    )
    .toBe(true);
  const frameBeforePointerSelection = Number(
    (await page.locator("#pmos-fb").getAttribute("data-pmos-frame-sequence")) ??
      "0",
  );
  const pointerSelectionStart = consoleLines.length;
  await clickFramebuffer(page, filesOrigin!.x + 88, filesOrigin!.y + 109);
  await waitForLineAfter(
    consoleLines,
    pointerSelectionStart,
    (line) => line === `[real-kernel] files: selected ${drafts}`,
  );
  await expect
    .poll(
      async () => {
        const frame = Number(
          (await page
            .locator("#pmos-fb")
            .getAttribute("data-pmos-frame-sequence")) ?? "0",
        );
        return (
          frame > frameBeforePointerSelection &&
          (await framebufferRgb(page, firstRowFill.x, firstRowFill.y)) ===
            "0,103,192"
        );
      },
      {
        timeout: 3_000,
        message: "Files did not present the pointer-selected row",
      },
    )
    .toBe(true);
  await page.keyboard.press("r");
  await typeDialog(page, "archive", filesOrigin!);
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: renamed ${drafts} -> ${archive}`,
  );

  await page.keyboard.press("d");
  await waitForFilesDialog(page, filesOrigin!, "delete confirmation");
  const deleteDialogFrame = Number(
    (await page.locator("#pmos-fb").getAttribute("data-pmos-frame-sequence")) ??
      "0",
  );
  await page.keyboard.press("Enter");
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: deleted ${archive}`,
  );
  const deleteDialogBackground = filesDialogBackground(filesOrigin!);
  await expect
    .poll(
      async () => {
        const frame = Number(
          (await page
            .locator("#pmos-fb")
            .getAttribute("data-pmos-frame-sequence")) ?? "0",
        );
        return (
          frame > deleteDialogFrame &&
          (await framebufferRgb(
            page,
            deleteDialogBackground.x,
            deleteDialogBackground.y,
          )) !== "89,105,120"
        );
      },
      {
        timeout: 3_000,
        message: "Files did not present the confirmed deletion result",
      },
    )
    .toBe(true);

  // Backspace navigates to the parent and reselects the directory just left,
  // so the same confirmed-delete flow removes the now-empty workflow folder.
  const beforeParentNavigation = consoleLines.length;
  const parentListRegion = {
    x: filesOrigin!.x + 12,
    y: filesOrigin!.y + 96,
    width: 560,
    height: 120,
  };
  const beforeParentPaint = await framebufferRegionFingerprint(
    page,
    parentListRegion,
  );
  await page.keyboard.press("Backspace");
  await waitForLineAfter(
    consoleLines,
    beforeParentNavigation,
    (line) => line === `[real-kernel] files: cwd ${root}`,
  );
  await expect
    .poll(() => framebufferRegionFingerprint(page, parentListRegion), {
      timeout: 3_000,
      message: "Files did not present the parent directory selection",
    })
    .not.toBe(beforeParentPaint);
  await page.keyboard.press("d");
  await waitForFilesDialog(page, filesOrigin!, "delete confirmation");
  await page.keyboard.press("Enter");
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: deleted ${workflow}`,
  );

  await page.keyboard.press("g");
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: refreshed ${root}`,
  );
  await page.keyboard.press("Control+q");
  await waitForLine(
    consoleLines,
    (line) => line === "[real-kernel] files: close requested",
  );

  const finalFrame = Number(
    (await page.locator("#pmos-fb").getAttribute("data-pmos-frame-sequence")) ??
      "0",
  );
  expect(finalFrame).toBeGreaterThan(firstFrame + 5);
  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(
    false,
  );
  expect(consoleLines.some((line) => line.startsWith("[pageerror]"))).toBe(
    false,
  );
  expect(
    consoleLines.some((line) => line.includes("using built-in fallback")),
  ).toBe(false);
});

test("Files resolves Edit's installed desktop entry and Edit saves through the renamed open inode", async ({
  page,
  browserName,
}) => {
  test.skip(
    browserName === "webkit",
    "The supported persistent OS workflow is Chromium and Firefox; Playwright WebKit lacks OPFS.",
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
  await waitForLine(consoleLines, (line) =>
    line.includes("persistent OPFS root mounted at /"),
  );
  await waitForLine(consoleLines, (line) =>
    line.includes("shell: loaded 5 applications from /usr/share/applications"),
  );

  await clickFramebufferAndWaitForPaint(page, 40, 752);
  await clickFramebuffer(page, 100, 648);
  await waitForLine(consoleLines, (line) =>
    line.includes("files: ready /home/user"),
  );
  const filesOrigin = await waitForActiveWindowBounds(page, {
    timeout: 10_000,
  });

  // The background session writer may create .config before or after Files
  // scans the seeded home. Observe row zero causally, then select Documents'
  // second row only when .config is already present.
  let selectionStart = consoleLines.length;
  await clickFramebuffer(page, filesOrigin!.x + 88, filesOrigin!.y + 109);
  const firstSelection = await waitForLineAfter(
    consoleLines,
    selectionStart,
    (line) => line.startsWith("[real-kernel] files: selected /home/user/"),
  );
  if (firstSelection !== "[real-kernel] files: selected /home/user/Documents") {
    expect(firstSelection).toBe(
      "[real-kernel] files: selected /home/user/.config",
    );
    selectionStart = consoleLines.length;
    await clickFramebufferAndWaitForPaint(
      page,
      filesOrigin!.x + 88,
      filesOrigin!.y + 127,
    );
    await waitForLineAfter(
      consoleLines,
      selectionStart,
      (line) => line === "[real-kernel] files: selected /home/user/Documents",
    );
  }
  await page.keyboard.press("Enter");
  await waitForLine(
    consoleLines,
    (line) => line === "[real-kernel] files: cwd /home/user/Documents",
  );
  await clickFramebufferAndWaitForPaint(
    page,
    filesOrigin!.x + 88,
    filesOrigin!.y + 109,
  );
  await waitForLine(
    consoleLines,
    (line) =>
      line === "[real-kernel] files: selected /home/user/Documents/editing.md",
  );
  const beforeOpen = consoleLines.length;
  const workersWithFiles = await liveWorkerCount(page);
  await page.keyboard.press("Enter");
  const dispatchLine = await waitForLineAfter(
    consoleLines,
    beforeOpen,
    (line) =>
      line.includes(
        "files: opened /home/user/Documents/editing.md via /usr/share/applications/edit.desktop exec=/bin/edit",
      ),
  );
  expect(dispatchLine).toContain("caps=0x2");
  const ready = await waitForLineAfter(consoleLines, beforeOpen, (line) =>
    line.includes(
      "edit: ready path=/home/user/Documents/editing.md status=opened /home/user/Documents/editing.md bytes=",
    ),
  );
  const initialBytes = Number(ready.match(/bytes=(\d+)/)?.[1]);
  expect(initialBytes).toBeGreaterThan(0);
  const firstEditWindow = await waitForActiveWindowBounds(page, {
    expectedX: filesOrigin.x + 32,
    expectedY: filesOrigin.y + 32,
    expectedWidth: 640,
    timeout: 10_000,
    message: "Edit did not present its focused cascaded frame",
  });
  await expect
    .poll(() => liveWorkerCount(page), { timeout: 5_000 })
    .toBe(workersWithFiles + 1);

  // The taskbar contains only application windows: Files then its Edit child.
  // Rename while Edit keeps the document fd open, then save from the child.
  const stackingMarker = {
    x: filesOrigin!.x + 450,
    y: filesOrigin!.y + 35,
  };
  await expect
    .poll(() => framebufferRgb(page, stackingMarker.x, stackingMarker.y), {
      timeout: 5_000,
      message: "Edit did not paint its focused titlebar after mapping",
    })
    .toBe(LIGHT_TITLEBAR.slice(0, 3).join(","));
  await focusTaskbarEntry(page, 0, 2);
  await expect
    .poll(() => framebufferRgb(page, stackingMarker.x, stackingMarker.y), {
      timeout: 5_000,
    })
    .toBe("237,237,237");
  await page.keyboard.press("r");
  await typeDialog(page, "zz-editing-renamed.md", filesOrigin!);
  await waitForLine(
    consoleLines,
    (line) =>
      line ===
      "[real-kernel] files: renamed /home/user/Documents/editing.md -> /home/user/Documents/zz-editing-renamed.md",
  );
  await focusTaskbarEntry(page, 1, 2);
  await expect
    .poll(() => framebufferRgb(page, stackingMarker.x, stackingMarker.y), {
      timeout: 5_000,
    })
    .toBe(LIGHT_TITLEBAR.slice(0, 3).join(","));
  const beforeSave = consoleLines.length;
  await page.keyboard.type("X");
  await page.keyboard.press("Control+s");
  await waitForLineAfter(consoleLines, beforeSave, (line) =>
    line.includes(
      `edit: saved /home/user/Documents/editing.md bytes=${initialBytes + 1}`,
    ),
  );
  await page.keyboard.press("Control+q");
  await expect
    .poll(() => liveWorkerCount(page), { timeout: 5_000 })
    .toBe(workersWithFiles);
  await waitForLineAfter(
    consoleLines,
    beforeSave,
    (line) => line.includes("files: reaped child pid="),
    5_000,
  );
  // Closing the focused child transfers focus back to Files. Clicking that
  // already-focused task would exercise the Windows-style minimize toggle.
  await waitForFocusedTaskbarEntry(page, 0, 1);
  await expect
    .poll(() => framebufferRgb(page, stackingMarker.x, stackingMarker.y), {
      timeout: 5_000,
    })
    .toBe("237,237,237");

  // Reopen the renamed entry through Files. The exact +1 byte count proves
  // Save updated that inode. Renaming it back to the old pathname proves Save
  // did not recreate a stale editing.md beside it.
  const beforeRefresh = consoleLines.length;
  await clickFramebuffer(page, filesOrigin!.x + 350, filesOrigin!.y + 35);
  await waitForLineAfter(
    consoleLines,
    beforeRefresh,
    (line) => line === "[real-kernel] files: refreshed /home/user/Documents",
  );
  const beforeReopen = consoleLines.length;
  await page.keyboard.press("Enter");
  await waitForLineAfter(consoleLines, beforeReopen, (line) =>
    line.includes(
      `edit: ready path=/home/user/Documents/zz-editing-renamed.md status=opened /home/user/Documents/zz-editing-renamed.md bytes=${initialBytes + 1}`,
    ),
  );
  // Reopening from Files maps and focuses the new Edit child already.
  await waitForFocusedTaskbarEntry(page, 1, 2);
  const beforeSecondClose = consoleLines.length;
  const secondEditWindow = await waitForActiveWindowBounds(page, {
    expectedX: firstEditWindow.x + 32,
    expectedY: firstEditWindow.y + 32,
    expectedWidth: 640,
    message: "Edit window disappeared",
  });
  const secondEditClose = titlebarControlPoint(secondEditWindow, "close");
  await clickFramebuffer(page, secondEditClose.x, secondEditClose.y);
  await expect
    .poll(() => liveWorkerCount(page), { timeout: 5_000 })
    .toBe(workersWithFiles);
  await waitForLineAfter(
    consoleLines,
    beforeSecondClose,
    (line) => line.includes("files: reaped child pid="),
    5_000,
  );
  await waitForFocusedTaskbarEntry(page, 0, 1);
  await expect
    .poll(() => framebufferRgb(page, stackingMarker.x, stackingMarker.y), {
      timeout: 5_000,
    })
    .toBe("237,237,237");

  const beforeFinalSelect = consoleLines.length;
  await clickFramebuffer(page, filesOrigin!.x + 88, filesOrigin!.y + 127);
  await waitForLineAfter(
    consoleLines,
    beforeFinalSelect,
    (line) =>
      line ===
      "[real-kernel] files: selected /home/user/Documents/zz-editing-renamed.md",
  );
  await page.keyboard.press("r");
  await typeDialog(page, "editing.md", filesOrigin!);
  await waitForLine(
    consoleLines,
    (line) =>
      line ===
      "[real-kernel] files: renamed /home/user/Documents/zz-editing-renamed.md -> /home/user/Documents/editing.md",
  );

  expect(
    consoleLines.some((line) => line.includes("shell: launched /bin/edit")),
  ).toBe(false);
  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(
    false,
  );
  expect(consoleLines.some((line) => line.startsWith("[pageerror]"))).toBe(
    false,
  );
});
