import fs from "node:fs";

import { expect, test, type Page } from "@playwright/test";

test.use({ viewport: { width: 1280, height: 900 } });
test.setTimeout(45_000);

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const HOST_FILE_PICKER_CONFIRM = "#pmos-host-file-picker-confirm";

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

async function findFramebufferColor(
  page: Page,
  rgb: readonly [number, number, number],
): Promise<{ x: number; y: number } | null> {
  return page.evaluate(([red, green, blue]) => {
    const canvas = document.querySelector<HTMLCanvasElement>("#pmos-fb");
    const context = canvas?.getContext("2d");
    if (canvas == null || context == null) return null;
    const pixels = context.getImageData(0, 0, canvas.width, canvas.height).data;
    for (let y = 0; y < canvas.height; y += 2) {
      for (let x = 0; x < canvas.width; x += 2) {
        const offset = (y * canvas.width + x) * 4;
        if (
          pixels[offset] === red &&
          pixels[offset + 1] === green &&
          pixels[offset + 2] === blue
        ) {
          return { x, y };
        }
      }
    }
    return null;
  }, rgb);
}

async function findFilesSelectedRowY(
  page: Page,
  origin: { x: number; y: number },
): Promise<number | null> {
  return page.evaluate(
    ({ originX, originY }) => {
      const canvas = document.querySelector<HTMLCanvasElement>("#pmos-fb");
      const context = canvas?.getContext("2d");
      if (canvas === null || canvas === undefined || context == null)
        return null;
      const x = originX + 4;
      const top = originY + 102;
      const bottom = Math.min(canvas.height, originY + 398);
      const pixels = context.getImageData(x, top, 1, bottom - top).data;
      for (let index = 0; index < bottom - top; index += 1) {
        const offset = index * 4;
        if (
          pixels[offset] === 0x4b &&
          pixels[offset + 1] === 0x78 &&
          pixels[offset + 2] === 0xa5
        ) {
          return top + index;
        }
      }
      return null;
    },
    { originX: origin.x, originY: origin.y },
  );
}

async function dropHostFile(
  page: Page,
  name: string,
  mime: string,
  bytes: Buffer,
): Promise<void> {
  await page.evaluate(
    ({ fileName, fileMime, encoded }) => {
      const binary = atob(encoded);
      const owned = new Uint8Array(binary.length);
      for (let index = 0; index < binary.length; index += 1) {
        owned[index] = binary.charCodeAt(index);
      }
      const file = new File([owned], fileName, { type: fileMime });
      for (const type of ["dragover", "drop"] as const) {
        const event = new Event(type, { bubbles: true, cancelable: true });
        Object.defineProperty(event, "dataTransfer", {
          value: { files: [file] },
        });
        window.dispatchEvent(event);
      }
    },
    { fileName: name, fileMime: mime, encoded: bytes.toString("base64") },
  );
}

async function downloadBytes(
  download: import("@playwright/test").Download,
): Promise<Buffer> {
  const filePath = await download.path();
  if (filePath === null)
    throw new Error("browser did not expose the completed download");
  return fs.readFileSync(filePath);
}

test("Files imports drag/drop and picker files, then exports exact bytes", async ({
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
    line.includes("shell: connected to /run/display"),
  );

  await clickFramebuffer(page, 40, 752);
  await clickFramebuffer(page, 100, 648);
  const readyLine = await waitForLine(consoleLines, (line) =>
    /files: ready \/.*$/.test(line),
  );
  const root = readyLine.match(/files: ready (\/.*)$/)?.[1];
  expect(root, readyLine).toBeDefined();
  await waitForLine(consoleLines, (line) =>
    line.includes("files: host transfer ready"),
  );
  await expect
    .poll(() => findFramebufferColor(page, [0x35, 0x5f, 0x84]), {
      timeout: 10_000,
    })
    .not.toBeNull();
  const origin = await findFramebufferColor(page, [0x35, 0x5f, 0x84]);
  expect(origin).not.toBeNull();
  await clickFramebuffer(page, origin!.x + 88, origin!.y + 109);
  const selectedBeforeImport = await findFilesSelectedRowY(page, origin!);
  expect(selectedBeforeImport).not.toBeNull();

  // Larger than both the old 60 KiB bootstrap cap and the 32 KiB syscall
  // scratch, so success proves the bounded worker-to-kernel chunk path.
  const droppedName = "zz-host-roundtrip.bin";
  const dropped = Buffer.alloc(96 * 1024 + 17);
  for (let index = 0; index < dropped.length; index += 1) {
    dropped[index] = (index * 31) & 0xff;
  }
  await dropHostFile(page, droppedName, "application/octet-stream", dropped);
  const droppedPath =
    root === "/" ? `/${droppedName}` : `${root}/${droppedName}`;
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: imported ${droppedPath}`,
  );
  // The import completion reloads the VFS directory and selects the imported
  // basename. Wait for that exact state to reach the framebuffer before the
  // export shortcut; the console line is emitted before the surface commit.
  await expect
    .poll(
      async () => {
        const selectedAfterImport = await findFilesSelectedRowY(page, origin!);
        return (
          selectedAfterImport !== null &&
          selectedAfterImport !== selectedBeforeImport
        );
      },
      {
        timeout: 5_000,
        message: `Files did not present ${droppedPath} as the imported selection`,
      },
    )
    .toBe(true);
  const importedRowY = await findFilesSelectedRowY(page, origin!);
  expect(importedRowY).not.toBeNull();
  const selectionStart = consoleLines.length;
  await clickFramebuffer(page, origin!.x + 88, importedRowY! + 9);
  await expect
    .poll(
      () =>
        consoleLines
          .slice(selectionStart)
          .some(
            (line) => line === `[real-kernel] files: selected ${droppedPath}`,
          ),
      {
        timeout: 5_000,
        message: `Files did not pointer-select ${droppedPath} before export`,
      },
    )
    .toBe(true);

  const downloadPromise = page.waitForEvent("download");
  await page.keyboard.press("e");
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toBe(droppedName);
  expect(await downloadBytes(download)).toEqual(dropped);
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: exported ${droppedPath}`,
  );

  // Exercise the reverse control path too: Files → kernel capability check →
  // browser-substrate confirmation → DOM picker → token notification → Files
  // VFS write. The second-row Import button is app-relative (146..209,
  // 48..73); use its centre so this also covers the explicit UI required by
  // FR-032a rather than relying on the keyboard shortcut.
  const pickedName = "zz-picked.txt";
  const picked = Buffer.from("picked through the PMos host bridge\n", "utf8");
  await clickFramebuffer(page, origin!.x + 178, origin!.y + 61);
  const confirmation = page.locator(HOST_FILE_PICKER_CONFIRM);
  await expect(confirmation).toBeVisible();
  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    confirmation.press("Enter"),
  ]);
  await chooser.setFiles({
    name: pickedName,
    mimeType: "text/plain",
    buffer: picked,
  });
  const pickedPath = root === "/" ? `/${pickedName}` : `${root}/${pickedName}`;
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: imported ${pickedPath}`,
  );

  // Leave the persistent profile clean so repeat runs are deterministic.
  await page.keyboard.press("d");
  await page.keyboard.press("Enter");
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: deleted ${pickedPath}`,
  );
  const refreshStart = consoleLines.length;
  await page.keyboard.press("g");
  await expect
    .poll(
      () =>
        consoleLines
          .slice(refreshStart)
          .some((line) => line === `[real-kernel] files: refreshed ${root}`),
      {
        timeout: 5_000,
        message: `Files did not refresh ${root} before cleanup navigation`,
      },
    )
    .toBe(true);
  await page.keyboard.press("Home");
  // Select the remaining zz-host file by finding its row through End; seeded
  // home entries sort before the zz prefix.
  await page.keyboard.press("End");
  await expect
    .poll(() => findFilesSelectedRowY(page, origin!), {
      timeout: 5_000,
      message: `Files did not present ${droppedPath} as the cleanup selection`,
    })
    .toBe(importedRowY);
  const cleanupSelectionStart = consoleLines.length;
  await clickFramebuffer(page, origin!.x + 88, importedRowY! + 9);
  await expect
    .poll(
      () =>
        consoleLines
          .slice(cleanupSelectionStart)
          .some(
            (line) => line === `[real-kernel] files: selected ${droppedPath}`,
          ),
      {
        timeout: 5_000,
        message: `Files did not select ${droppedPath} before cleanup delete`,
      },
    )
    .toBe(true);
  await page.keyboard.press("d");
  await page.keyboard.press("Enter");
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: deleted ${droppedPath}`,
  );

  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(
    false,
  );
  expect(consoleLines.some((line) => line.startsWith("[pageerror]"))).toBe(
    false,
  );
});
