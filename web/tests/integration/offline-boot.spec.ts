// T166 — offline-boot: load once online so the service worker caches the
// asset bundle, stop the origin server, then reload and assert the desktop
// boots within the warm-load budget.

import { spawn } from "node:child_process";
import { createServer } from "node:net";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { expect, test, type Page } from "@playwright/test";
import { resolveCargoTargetDirectory } from "../helpers/cargo-target";
import {
  launcherMenuIsOpen,
  openLauncherBefore,
  openLauncherMenuBefore,
  selectLauncherRowBefore,
} from "./launcher-interaction";
import {
  TASKBAR_LIGHT_FOCUSED,
  activeWindowBounds,
  taskbarEntryPoint,
} from "./windows-ui";

test.use({ viewport: { width: 1280, height: 900 } });

const workspaceRoot = fileURLToPath(new URL("../../../", import.meta.url));
// The build and Playwright web server invoke Cargo from the workspace root,
// while this spec itself runs with `web/` as its process working directory.
const cargoTargetDirectory = resolveCargoTargetDirectory(
  workspaceRoot,
  process.env.CARGO_TARGET_DIR,
);
const xtask = path.join(cargoTargetDirectory, "debug", "xtask");

interface BundledAppProbe {
  readonly name: string;
  readonly exec: string;
  readonly rowY: number;
  readonly ready: (line: string) => boolean;
  readonly marker?: {
    readonly rgb: readonly [number, number, number];
    readonly minimumWidth: number;
    readonly minimumHeight: number;
  };
  readonly windowWidth?: number;
}

const BUNDLED_APPS: readonly BundledAppProbe[] = [
  {
    name: "Terminal",
    exec: "/bin/term",
    rowY: 624,
    ready: (line) => line.includes("term: starting"),
    marker: {
      rgb: [0x14, 0x0e, 0x0a],
      minimumWidth: 320,
      minimumHeight: 24,
    },
  },
  {
    name: "Files",
    exec: "/bin/files",
    rowY: 648,
    ready: (line) => line.includes("files: ready /home/user"),
    windowWidth: 640,
  },
  {
    name: "Text Editor",
    exec: "/bin/edit",
    rowY: 672,
    ready: (line) => line.includes("edit: ready path="),
    windowWidth: 640,
  },
  {
    name: "Settings",
    exec: "/bin/settings",
    rowY: 696,
    ready: (line) => /shell: launched \/bin\/settings pid=\d+/.test(line),
    windowWidth: 560,
  },
  {
    name: "System Monitor",
    exec: "/bin/sysmon",
    rowY: 720,
    ready: (line) =>
      /sysmon: ready processes=\d+ terminate=(enabled|read-only)/.test(line),
    marker: {
      rgb: [0xd6, 0xb4, 0xb4],
      minimumWidth: 80,
      minimumHeight: 3,
    },
  },
];

async function findSolidMarker(
  page: Page,
  marker: NonNullable<BundledAppProbe["marker"]>,
): Promise<{ x: number; y: number } | null> {
  return page.locator("#pmos-fb").evaluate(
    (
      canvas: HTMLCanvasElement,
      target: NonNullable<BundledAppProbe["marker"]>,
    ) => {
      const context = canvas.getContext("2d");
      if (context === null) return null;
      const height = Math.min(canvas.height, 736);
      const bytes = context.getImageData(0, 0, canvas.width, height).data;
      const matches = (x: number, y: number): boolean => {
        const offset = (y * canvas.width + x) * 4;
        return (
          bytes[offset] === target.rgb[0] &&
          bytes[offset + 1] === target.rgb[1] &&
          bytes[offset + 2] === target.rgb[2] &&
          bytes[offset + 3] === 0xff
        );
      };
      for (let y = 0; y <= height - target.minimumHeight; y += 1) {
        let runStart = -1;
        for (let x = 0; x <= canvas.width; x += 1) {
          const matched = x < canvas.width && matches(x, y);
          if (matched && runStart < 0) runStart = x;
          if (!matched && runStart >= 0) {
            const runEnd = x;
            for (
              let candidateX = runStart;
              candidateX + target.minimumWidth <= runEnd;
              candidateX += target.minimumWidth
            ) {
              let solid = true;
              for (
                let sampleY = y + 1;
                solid && sampleY < y + target.minimumHeight;
                sampleY += 1
              ) {
                for (
                  let sampleX = candidateX;
                  sampleX < candidateX + target.minimumWidth;
                  sampleX += 1
                ) {
                  if (!matches(sampleX, sampleY)) {
                    solid = false;
                    break;
                  }
                }
              }
              if (solid) return { x: candidateX, y };
            }
            runStart = -1;
          }
        }
      }
      return null;
    },
    marker,
  );
}

async function launchBundledAppOffline(
  page: Page,
  lines: readonly string[],
  app: BundledAppProbe,
  appIndex: number,
  launcherAlreadyOpen: boolean,
  closedLauncherFingerprint: number,
): Promise<void> {
  const previouslyActive = await activeWindowBounds(page);
  if (!launcherAlreadyOpen) {
    await expect
      .poll(() => launcherMenuIsOpen(page), { timeout: 5_000 })
      .toBe(false);
    await openLauncherMenuBefore(page, Date.now() + 5_000);
  }
  const start = lines.length;
  const menuCloseFrame = await selectLauncherRowBefore(
    page,
    100,
    app.rowY,
    Date.now() + 5_000,
    closedLauncherFingerprint,
  );
  await expect
    .poll(
      () =>
        lines
          .slice(start)
          .some((line) =>
            new RegExp(`shell: launched ${app.exec} pid=\\d+`).test(line),
          ),
      {
        timeout: 10_000,
        message: `${app.name} was not launched while the origin was stopped`,
      },
    )
    .toBe(true);
  await expect
    .poll(() => lines.slice(start).some(app.ready), {
      timeout: 10_000,
      message: `${app.name} did not reach its ready boundary offline`,
    })
    .toBe(true);
  try {
    await expect
      .poll(async () => {
        const frameSequence = Number(
          (await page
            .locator("#pmos-fb")
            .getAttribute("data-pmos-frame-sequence")) ?? "0",
        );
        const taskPoint = taskbarEntryPoint(appIndex, appIndex + 1);
        const taskPixel = await page.locator("#pmos-fb").evaluate(
          (canvas: HTMLCanvasElement, point: { x: number; y: number }) => {
            const context = canvas.getContext("2d");
            if (context === null) return [];
            return Array.from(
              context.getImageData(point.x, point.y, 1, 1).data,
            );
          },
          taskPoint,
        );
        const taskFocused = TASKBAR_LIGHT_FOCUSED.every(
          (channel, index) => taskPixel[index] === channel,
        );
        const active =
          app.windowWidth === undefined ? null : await activeWindowBounds(page);
        const appMarkerPresented =
          app.windowWidth !== undefined
            ? active !== null &&
              active.width === app.windowWidth &&
              (previouslyActive === null ||
                active.x !== previouslyActive.x ||
                active.y !== previouslyActive.y ||
                active.width !== previouslyActive.width)
            : app.marker !== undefined &&
              (await findSolidMarker(page, app.marker)) !== null;
        return (
          frameSequence > menuCloseFrame &&
          !(await launcherMenuIsOpen(page)) &&
          taskFocused &&
          appMarkerPresented
        );
      }, {
        timeout: 10_000,
        message:
          `${app.name} did not map its contiguous app marker on a post-close ` +
          `frame with the launcher still closed`,
      })
      .toBe(true);
  } catch (cause) {
    const frameSequence = Number(
      (await page
        .locator("#pmos-fb")
        .getAttribute("data-pmos-frame-sequence")) ?? "0",
    );
    throw new Error(
      `${app.name} post-close map boundary failed; ` +
        `close_frame=${menuCloseFrame} final_frame=${frameSequence} ` +
        `menu_open=${await launcherMenuIsOpen(page)} ` +
        `active=${JSON.stringify(await activeWindowBounds(page))} ` +
        `marker=${JSON.stringify(
          app.marker === undefined
            ? null
            : await findSolidMarker(page, app.marker),
        )}`,
      { cause },
    );
  }
}

async function unusedLoopbackPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const address = probe.address();
      if (address === null || typeof address === "string") {
        probe.close();
        reject(new Error("failed to allocate loopback port"));
        return;
      }
      probe.close((error) => {
        if (error) {
          reject(error);
        } else {
          resolve(address.port);
        }
      });
    });
  });
}

async function startDisposableOrigin(): Promise<{
  origin: string;
  stop: () => Promise<void>;
}> {
  const port = await unusedLoopbackPort();
  const child = spawn(
    xtask,
    ["dev-server", "--dir=dist", `--port=${port}`],
    {
      cwd: workspaceRoot,
      stdio: ["ignore", "pipe", "pipe"],
    },
  );

  await new Promise<void>((resolve, reject) => {
    let output = "";
    const timeout = setTimeout(() => {
      reject(new Error(`offline origin did not start:\n${output}`));
    }, 10_000);
    const cleanup = (): void => {
      clearTimeout(timeout);
      child.stdout.off("data", onOutput);
      child.stderr.off("data", onOutput);
      child.off("error", onError);
      child.off("exit", onExit);
    };
    const onOutput = (chunk: Buffer): void => {
      output += chunk.toString();
      if (output.includes(`http://127.0.0.1:${port}`)) {
        cleanup();
        resolve();
      }
    };
    const onError = (error: Error): void => {
      cleanup();
      reject(error);
    };
    const onExit = (code: number | null): void => {
      cleanup();
      reject(new Error(`offline origin exited with ${code}:\n${output}`));
    };
    child.stdout.on("data", onOutput);
    child.stderr.on("data", onOutput);
    child.once("error", onError);
    child.once("exit", onExit);
  });

  return {
    origin: `http://127.0.0.1:${port}`,
    stop: async () => {
      if (child.exitCode !== null || child.signalCode !== null) return;
      child.kill("SIGTERM");
      await new Promise<void>((resolve) => child.once("exit", () => resolve()));
    },
  };
}

test("offline warm boot reaches an interactive desktop within 3s", async ({
  browserName,
  context,
  page,
}) => {
  const server = await startDisposableOrigin();
  const linesOnline: string[] = [];
  page.on("console", (msg) => linesOnline.push(msg.text()));
  try {
    await page.goto(`${server.origin}/index.html`);
    await expect
      .poll(
        () =>
          linesOnline.find((l) =>
            /display-server served client \d+/.test(l),
          ) ?? null,
        { timeout: 15_000 },
      )
      .not.toBeNull();

    // The production bundle emits the worker at the distribution root. Wait
    // for both atomic installation and clients.claim() instead of sleeping and
    // racing the install on slower machines.
    await expect
      .poll(
        () =>
          page.evaluate(async () => {
            const registration =
              await navigator.serviceWorker.getRegistration();
            return registration?.active
              ? new URL(registration.active.scriptURL).pathname
              : null;
          }),
        { timeout: 15_000 },
      )
      .toBe("/sw.js");
    await expect
      .poll(
        () =>
          page.evaluate(() =>
            navigator.serviceWorker.controller
              ? new URL(navigator.serviceWorker.controller.scriptURL).pathname
              : null,
          ),
        { timeout: 15_000 },
      )
      .toBe("/sw.js");

    // Close the online tab so its kernel and user Workers cannot survive into
    // the warm boot. The service-worker registration and cache belong to the
    // browser context and remain available to the next tab.
    await page.close();

    // A stopped origin is a real network outage. It also avoids WebKit's
    // synthetic offline mode, which rejects requests before a controlling
    // service worker can satisfy them.
    await server.stop();

    const linesOffline: string[] = [];
    const offlinePage = await context.newPage();
    offlinePage.on("console", (msg) => linesOffline.push(msg.text()));
    const t0 = Date.now();
    const deadline = t0 + 3_000;
    await offlinePage.goto(`${server.origin}/index.html`);

    // SC-002 / Principle IX: the same strict 3 s clock includes the offline
    // navigation, visible desktop paint, and a causally painted launcher menu.
    await expect
      .poll(
        () =>
          linesOffline.find((l) =>
            /display-server served client \d+/.test(l),
          ) ?? null,
        { timeout: Math.max(1, deadline - Date.now()) },
      )
      .not.toBeNull();
    await expect(offlinePage.locator("#pmos-boot-splash")).toHaveCount(0, {
      timeout: Math.max(1, deadline - Date.now()),
    });
    const closedLauncherFingerprint = await openLauncherBefore(
      offlinePage,
      deadline,
    );

    const elapsed_ms = Date.now() - t0;
    console.log(`[offline-boot] elapsed_ms=${elapsed_ms} engine=${browserName}`);
    expect(elapsed_ms).toBeLessThan(3_000);

    // T166 continues after the warm-load endpoint while the origin remains
    // stopped. The timed launcher is already open for the first row; every
    // subsequent app gets a new causal launcher-open interaction. Exact spawn
    // evidence plus an app-specific framebuffer marker proves start + map.
    for (const [index, app] of BUNDLED_APPS.entries()) {
      await launchBundledAppOffline(
        offlinePage,
        linesOffline,
        app,
        index,
        index === 0,
        closedLauncherFingerprint,
      );
    }
  } finally {
    await server.stop();
  }
});
