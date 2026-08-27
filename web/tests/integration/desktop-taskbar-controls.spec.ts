import { expect, test, type Page } from "@playwright/test";
import {
  TASKBAR_LIGHT,
  TASKBAR_LIGHT_FOCUSED,
  TASKBAR_LIGHT_MINIMIZED,
  activeWindowBounds,
  taskbarEntryPoint,
  titlebarControlPoint,
  waitForActiveWindowBounds,
} from "./windows-ui";

test.use({ viewport: { width: 1280, height: 900 } });

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const TASKBAR_Y = 752;

async function waitForPresentationIdle(page: Page): Promise<void> {
  await page.locator("#pmos-fb").evaluate(
    (framebuffer: HTMLCanvasElement) =>
      new Promise<void>((resolve, reject) => {
        let quiet = window.setTimeout(finish, 75);
        const deadline = window.setTimeout(() => {
          cleanup();
          reject(new Error("framebuffer did not become idle"));
        }, 2_000);
        function cleanup(): void {
          window.clearTimeout(quiet);
          window.clearTimeout(deadline);
          framebuffer.removeEventListener("pmos:frame", onFrame);
        }
        function finish(): void {
          cleanup();
          resolve();
        }
        function onFrame(): void {
          window.clearTimeout(quiet);
          quiet = window.setTimeout(finish, 75);
        }
        framebuffer.addEventListener("pmos:frame", onFrame);
      }),
  );
}

interface ClickPresentationTiming {
  readonly totalMs: number;
  readonly inputToMainMs: number;
  readonly mainPaintMs: number;
}

interface CausalPixelChange {
  readonly x: number;
  readonly y: number;
  readonly before: readonly number[];
}

async function clickToPresentedFrame(
  page: Page,
  x: number,
  y: number,
  consoleLines: readonly string[],
  causalPixelChange?: CausalPixelChange,
): Promise<ClickPresentationTiming> {
  const canvas = page.locator("#pmos-fb");
  const box = await canvas.boundingBox();
  if (box === null) throw new Error("framebuffer canvas has no layout box");
  const sequenceBefore = Number(
    (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
  );
  await canvas.evaluate((framebuffer: HTMLCanvasElement, causalPixel) => {
    const before = Number(framebuffer.dataset.pmosFrameSequence ?? "0");
    let startedAt: number | null = null;
    delete framebuffer.dataset.pmosClickLatency;
    delete framebuffer.dataset.pmosClickInputToMain;
    delete framebuffer.dataset.pmosClickMainPaint;
    function onPointerDown(): void {
      startedAt = performance.now();
      framebuffer.removeEventListener("pointerdown", onPointerDown);
    }
    function onFrame(event: Event): void {
      const detail = (
        event as CustomEvent<{
          sequence: number;
          receivedAt: number;
          paintedAt: number;
        }>
      ).detail;
      if (detail.sequence <= before || startedAt === null) return;
      if (causalPixel !== undefined) {
        const context = framebuffer.getContext("2d");
        if (context === null) return;
        const current = Array.from(
          context.getImageData(causalPixel.x, causalPixel.y, 1, 1).data,
        );
        if (current.every((channel, index) => channel === causalPixel.before[index])) {
          return;
        }
      }
      framebuffer.removeEventListener("pmos:frame", onFrame);
      framebuffer.dataset.pmosClickLatency = String(detail.paintedAt - startedAt);
      framebuffer.dataset.pmosClickInputToMain = String(
        detail.receivedAt - startedAt,
      );
      framebuffer.dataset.pmosClickMainPaint = String(
        detail.paintedAt - detail.receivedAt,
      );
    }
    framebuffer.addEventListener("pointerdown", onPointerDown);
    framebuffer.addEventListener("pmos:frame", onFrame);
  }, causalPixelChange);
  await page.mouse.click(
    box.x + (x / FRAMEBUFFER_WIDTH) * box.width,
    box.y + (y / FRAMEBUFFER_HEIGHT) * box.height,
  );
  try {
    await expect
      .poll(() => canvas.getAttribute("data-pmos-click-latency"), { timeout: 2_000 })
      .not.toBeNull();
  } catch (cause) {
    const sequenceAfter = Number(
      (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
    );
    const liveWorkers = await page
      .locator("body")
      .getAttribute("data-pmos-live-workers");
    const causalPixelAfter =
      causalPixelChange === undefined
        ? "n/a"
        : JSON.stringify(
            await pixel(page, causalPixelChange.x, causalPixelChange.y),
          );
    throw new Error(
      `click (${x},${y}) produced no causal presentation; ` +
        `frame_sequence=${sequenceBefore}->${sequenceAfter} ` +
        `live_workers=${liveWorkers ?? "missing"} ` +
        `causal_pixel=${causalPixelAfter}\n${consoleLines.slice(-40).join("\n")}`,
      { cause },
    );
  }
  return {
    totalMs: Number(await canvas.getAttribute("data-pmos-click-latency")),
    inputToMainMs: Number(
      await canvas.getAttribute("data-pmos-click-input-to-main"),
    ),
    mainPaintMs: Number(
      await canvas.getAttribute("data-pmos-click-main-paint"),
    ),
  };
}

async function pixel(page: Page, x: number, y: number): Promise<number[]> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, point: { x: number; y: number }) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      return Array.from(context.getImageData(point.x, point.y, 1, 1).data);
    },
    { x, y },
  );
}

test("task buttons, titlebar controls, and app transitions paint below 100 ms", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));
  page.on("pageerror", (error) => consoleLines.push(`[pageerror] ${error.message}`));

  await page.goto("/index.html");
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 10_000,
  });
  await waitForPresentationIdle(page);

  const latencies: ClickPresentationTiming[] = [];
  const launcherEvidence = { x: 190, y: 610 };
  const launcherPixelBefore = await pixel(
    page,
    launcherEvidence.x,
    launcherEvidence.y,
  );
  latencies.push(
    await clickToPresentedFrame(page, 40, TASKBAR_Y, consoleLines, {
      ...launcherEvidence,
      before: launcherPixelBefore,
    }),
  );
  await waitForPresentationIdle(page);
  await expect
    .poll(() => pixel(page, launcherEvidence.x, launcherEvidence.y))
    .not.toEqual(launcherPixelBefore);
  latencies.push(await clickToPresentedFrame(page, 100, 624, consoleLines));
  await expect
    .poll(() => consoleLines.some((line) => line.includes("term: starting")), {
      timeout: 5_000,
    })
    .toBe(true);
  await waitForPresentationIdle(page);

  // The shell is not exposed as an application task. Terminal is the first
  // visible task button and is initially focused.
  const terminalTask = taskbarEntryPoint(0, 1);
  await expect
    .poll(() => pixel(page, terminalTask.x, terminalTask.y))
    .toEqual([...TASKBAR_LIGHT_FOCUSED]);
  const terminalWindow = await waitForActiveWindowBounds(page, {
    expectedWidth: 720,
  });

  // Clicking a focused task minimizes; clicking its dimmed task restores it.
  latencies.push(
    await clickToPresentedFrame(
      page,
      terminalTask.x,
      terminalTask.y,
      consoleLines,
    ),
  );
  await waitForPresentationIdle(page);
  await expect
    .poll(() => pixel(page, terminalTask.x, terminalTask.y))
    .toEqual([...TASKBAR_LIGHT_MINIMIZED]);

  latencies.push(
    await clickToPresentedFrame(
      page,
      terminalTask.x,
      terminalTask.y,
      consoleLines,
    ),
  );
  await waitForPresentationIdle(page);
  await expect
    .poll(() => pixel(page, terminalTask.x, terminalTask.y))
    .toEqual([...TASKBAR_LIGHT_FOCUSED]);

  // Window lifecycle controls now live where users expect them: in the
  // client-painted titlebar.
  const workersBeforeClose = Number(
    (await page.locator("body").getAttribute("data-pmos-live-workers")) ?? "0",
  );
  const close = titlebarControlPoint(terminalWindow, "close");
  latencies.push(
    await clickToPresentedFrame(page, close.x, close.y, consoleLines),
  );
  await waitForPresentationIdle(page);
  await expect
    .poll(() => pixel(page, terminalTask.x, terminalTask.y))
    .toEqual([...TASKBAR_LIGHT]);
  await expect
    .poll(async () =>
      Number(
        (await page.locator("body").getAttribute("data-pmos-live-workers")) ??
          "0",
      ),
    )
    .toBeLessThan(workersBeforeClose);
  await expect.poll(() => activeWindowBounds(page)).toBeNull();

  console.log(
    `[desktop-taskbar] click_to_frame_ms=${latencies
      .map((latency) => latency.totalMs.toFixed(1))
      .join(",")} input_to_main_ms=${latencies
      .map((latency) => latency.inputToMainMs.toFixed(1))
      .join(",")} main_paint_ms=${latencies
      .map((latency) => latency.mainPaintMs.toFixed(1))
      .join(",")}`,
  );
  // Every measured interaction is user-visible and covered by Principle IX:
  // launcher open, app transition, minimize, restore, and close. Keep the
  // same strict bound across engines; the stage split above makes an upstream
  // scheduling regression distinguishable from main-thread canvas painting.
  for (const latency of latencies) {
    expect(latency.totalMs).toBeLessThan(100);
  }
});
