import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test, type Page } from "@playwright/test";

import {
  launchTerminal,
  runTerminalCommand,
  runTerminalCommandWithStatus,
  submitTerminalCommand,
} from "./guest-terminal";
import {
  launcherMenuRegionFingerprint,
  openLauncherMenuBefore,
  selectLauncherRowBefore,
} from "./launcher-interaction";

test.use({ viewport: { width: 1280, height: 900 } });
test.setTimeout(60_000);

const PACKAGE_NAME = "hello-0.1.0.pmpkg.tar";
const PACKAGE_PATH = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  `../../../dist/pkgs/staging/${PACKAGE_NAME}`,
);

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

function corruptExecutablePayload(bundle: Buffer): Buffer {
  const corrupted = Buffer.from(bundle);
  let offset = 0;
  while (offset + 512 <= corrupted.length) {
    const header = corrupted.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) break;
    const nameEnd = header.indexOf(0);
    const name = header
      .subarray(0, nameEnd < 0 ? 100 : nameEnd)
      .toString("utf8");
    const sizeText = header
      .subarray(124, 136)
      .toString("ascii")
      .replaceAll("\0", "")
      .trim();
    const size = Number.parseInt(sizeText || "0", 8);
    const dataOffset = offset + 512;
    if (name === "bin/hello.wasm") {
      if (size < 9) throw new Error("sample executable is unexpectedly short");
      corrupted[dataOffset + 8] ^= 0x01;
      return corrupted;
    }
    offset = dataOffset + Math.ceil(size / 512) * 512;
  }
  throw new Error("sample package has no bin/hello.wasm payload");
}

async function dropHostFile(
  page: Page,
  name: string,
  bytes: Buffer,
): Promise<void> {
  const base64 = bytes.toString("base64");
  await page.evaluate(
    async ({ fileName, encoded }) => {
      const binary = atob(encoded);
      const owned = new Uint8Array(binary.length);
      for (let index = 0; index < binary.length; index += 1) {
        owned[index] = binary.charCodeAt(index);
      }
      const file = new File([owned], fileName, {
        type: "application/x-pmos-package",
      });
      Object.defineProperty(file, "arrayBuffer", {
        value: async () => owned.slice().buffer,
      });
      const transfer = { files: [file] };
      for (const type of ["dragover", "drop"] as const) {
        const event = new Event(type, { bubbles: true, cancelable: true });
        Object.defineProperty(event, "dataTransfer", { value: transfer });
        window.dispatchEvent(event);
      }
      // The production listener reads the File asynchronously. Playwright's
      // synthetic DataTransfer does not retain native Blob backing after
      // synchronous dispatch, so keep an explicit owned reader and yield until
      // deliverDropFile has copied it into the Worker message.
      await Promise.resolve();
      await Promise.resolve();
    },
    { fileName: name, encoded: base64 },
  );
}

async function framebufferPixel(
  page: Page,
  x: number,
  y: number,
): Promise<readonly number[]> {
  return page.evaluate(
    ({ px, py }) => {
      const canvas = document.querySelector<HTMLCanvasElement>("#pmos-fb");
      const context = canvas?.getContext("2d");
      if (canvas === null || canvas === undefined || context == null) return [];
      return Array.from(context.getImageData(px, py, 1, 1).data);
    },
    { px: x, py: y },
  );
}

test("imports, installs, rolls back failure, refreshes launcher, and launches an isolated package", async ({
  page,
  browserName,
}) => {
  test.skip(
    browserName === "webkit",
    "The supported package workflow is gated in Chromium and Firefox; WebKit lacks OPFS in Playwright Linux.",
  );
  const packageBytes = fs.readFileSync(PACKAGE_PATH);
  expect(packageBytes.length).toBeLessThanOrEqual(16 * 1024 * 1024);
  const corruptBytes = corruptExecutablePayload(packageBytes);

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

  // Files is the ordinary HOST_TRANSFER-capability client. Opening it first
  // subscribes to /run/host-files; the synthetic DOM drop below then follows
  // the same token -> host_file_recv -> VFS path as a user's drag/drop.
  let closedLauncherFingerprint = await launcherMenuRegionFingerprint(page);
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  await selectLauncherRowBefore(
    page,
    100,
    648,
    Date.now() + 5_000,
    closedLauncherFingerprint,
  );
  const filesReady = await waitForLine(consoleLines, (line) =>
    /files: ready \/.*$/.test(line),
  );
  const importRoot = filesReady.match(/files: ready (\/.*)$/)?.[1];
  expect(importRoot, filesReady).toBeDefined();
  const validGuestPath =
    importRoot === "/" ? `/${PACKAGE_NAME}` : `${importRoot}/${PACKAGE_NAME}`;
  const corruptName = "hello-corrupt-0.1.0.pmpkg.tar";
  const corruptGuestPath =
    importRoot === "/" ? `/${corruptName}` : `${importRoot}/${corruptName}`;

  await dropHostFile(page, PACKAGE_NAME, packageBytes);
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: imported ${validGuestPath}`,
  );
  await dropHostFile(page, corruptName, corruptBytes);
  await waitForLine(
    consoleLines,
    (line) => line === `[real-kernel] files: imported ${corruptGuestPath}`,
  );

  // Launch Terminal from the still-five-entry catalog and run the real
  // pkginstall WASM. Its stdout is redirected through the VFS /dev/console so
  // the browser assertion observes guest output, not a DOM test hook.
  await launchTerminal(page, consoleLines);
  await runTerminalCommand(
    page,
    consoleLines,
    `pkginstall ${validGuestPath} > /dev/console`,
    (line) => line === "[real-kernel] pkginstall: installed hello",
    10_000,
  );

  // A payload byte changed without its declared SHA-256 changing. Queue the
  // status query behind the failed child: Term's persistent shell cannot read
  // it until wait/reap completes, so `$?` still names this exact upgrade.
  const upgradeStatus = await runTerminalCommandWithStatus(
    page,
    consoleLines,
    `pkginstall --upgrade ${corruptGuestPath}`,
    "pkg-upgrade-status",
    10_000,
  );
  expect(upgradeStatus).toBe(1);

  // Opening the five-row launcher does not cover framebuffer y=600. Once the
  // live five-second VFS rescan sees hello.desktop, the six-row menu grows over
  // that pixel. This directly observes the catalog refresh without a timer.
  const closedMenuPixel = await framebufferPixel(page, 100, 600);
  closedLauncherFingerprint = await launcherMenuRegionFingerprint(page);
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  await expect
    .poll(() => framebufferPixel(page, 100, 600), { timeout: 7_000 })
    .not.toEqual(closedMenuPixel);

  const workersBeforeLaunch = Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
  await selectLauncherRowBefore(
    page,
    100,
    728,
    Date.now() + 5_000,
    closedLauncherFingerprint,
  );
  await waitForLine(consoleLines, (line) =>
    /shell: launched \/opt\/hello\/bin\/hello\.wasm pid=\d+/.test(line),
  );
  await waitForLine(
    consoleLines,
    (line) => line === "[real-kernel] hello: starting",
  );
  await waitForLine(
    consoleLines,
    (line) => line === "[real-kernel] hello: ready",
  );
  await expect
    .poll(
      async () =>
        Number(
          (await page.locator("body").getAttribute("data-pmos-live-workers")) ??
            "0",
        ),
      { timeout: 5_000 },
    )
    .toBeGreaterThan(workersBeforeLaunch);

  // Sysmon is an ordinary /proc reader. Starting it after the package means
  // its first snapshot must include the exact installed VFS executable path,
  // proving the launcher did not substitute a bundled registry fixture.
  closedLauncherFingerprint = await launcherMenuRegionFingerprint(page);
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  await selectLauncherRowBefore(
    page,
    100,
    696,
    Date.now() + 5_000,
    closedLauncherFingerprint,
  );
  await waitForLine(consoleLines, (line) => line.includes("sysmon: starting"));
  await waitForLine(consoleLines, (line) =>
    /sysmon: (observed|updated) pid=\d+ name=\/opt\/hello\/bin\/hello\.wasm vm_kib=[1-9]\d* fds=[1-9]\d*/.test(
      line,
    ),
  );

  // Uninstall through the shipped rm process, then observe the same live
  // catalog contract in reverse. A five-row launcher no longer covers y=600;
  // equality with the closed-menu pixel therefore proves hello.desktop was
  // removed from the visible catalog without restarting the shell.
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  await launchTerminal(page, consoleLines, {
    launcherAlreadyOpen: true,
    launcherRowY: 600,
  });
  const uninstallOptStatus = await runTerminalCommandWithStatus(
    page,
    consoleLines,
    "rm -r /opt/hello",
    "pkg-uninstall-opt-status",
    10_000,
  );
  expect(uninstallOptStatus).toBe(0);

  const uninstallDesktopStatus = await runTerminalCommandWithStatus(
    page,
    consoleLines,
    "rm /usr/share/applications/hello.desktop",
    "pkg-uninstall-desktop-status",
    10_000,
  );
  expect(uninstallDesktopStatus).toBe(0);

  // Exit the command terminal after uninstall. Its cascaded window overlaps
  // the launcher's extra sixth-row probe, so leaving it mapped would conceal
  // the shell catalog paint that this final assertion intentionally observes.
  const workersBeforeTerminalExit = Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
  await submitTerminalCommand(page, "exit");
  await expect
    .poll(
      async () =>
        Number(
          (await page.locator("body").getAttribute("data-pmos-live-workers")) ??
            "0",
        ),
      { timeout: 5_000 },
    )
    .toBeLessThan(workersBeforeTerminalExit);

  const closedAfterUninstall = await framebufferPixel(page, 100, 600);
  await openLauncherMenuBefore(page, Date.now() + 5_000);
  await expect
    .poll(() => framebufferPixel(page, 100, 600), { timeout: 7_000 })
    .toEqual(closedAfterUninstall);

  expect(consoleLines.some((line) => line.includes("real kernel panic"))).toBe(
    false,
  );
  expect(consoleLines.filter((line) => line.startsWith("[pageerror]"))).toEqual(
    [],
  );
});
