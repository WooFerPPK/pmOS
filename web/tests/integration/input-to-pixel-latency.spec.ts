import { expect, test, type Page } from "@playwright/test";

test.use({ viewport: { width: 1280, height: 900 } });

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const LATENCY_BUDGET_MS = 100;

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
      { timeout: 2_000 },
    )
    .toBeGreaterThan(before);
}

test("cold and steady focused Terminal input paint in < 100 ms", async ({
  page,
}) => {
  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));

  await page.goto("/index.html");
  await expect(page.locator("#pmos-boot-splash")).toHaveCount(0, {
    timeout: 10_000,
  });

  // Open the shell launcher, then choose its Term row. Coordinates are in
  // framebuffer space and converted through the canvas CSS box above.
  // Wait for the launcher's painted-open state before targeting a
  // row. The two clicks are separate OS interactions; posting them
  // back-to-back can make the second land before the shell has
  // installed the launcher hit map on slower engines.
  await clickFramebufferAndWaitForPaint(page, 40, 752);
  await clickFramebuffer(page, 100, 624);
  try {
    await expect
      .poll(
        () => consoleLines.some((line) => line.includes("term: starting")),
        {
          timeout: 5_000,
        },
      )
      .toBe(true);
  } catch (error) {
    throw new Error(
      `terminal did not launch; console follows:\n${consoleLines.join("\n")}`,
      { cause: error },
    );
  }

  const canvas = page.locator("#pmos-fb");
  await expect
    .poll(async () =>
      Number(await canvas.getAttribute("data-pmos-frame-sequence")),
    )
    .toBeGreaterThan(1);
  await expect
    .poll(() =>
      canvas.evaluate((framebuffer: HTMLCanvasElement) => {
        const context = framebuffer.getContext("2d");
        if (context === null) throw new Error("framebuffer 2d context missing");
        return Array.from(context.getImageData(708, 440, 1, 1).data);
      }),
    )
    .toEqual([0x14, 0x0e, 0x0a, 0xff]);

  let result: { coldFirstKeyMs: number; samples: number[] };
  try {
    result = await page.evaluate(async () => {
      const target = document.querySelector<HTMLCanvasElement>("#pmos-fb");
      if (target === null) throw new Error("framebuffer canvas missing");
      const framebuffer: HTMLCanvasElement = target;

      const inputRegionFingerprint = (): number => {
        const context = framebuffer.getContext("2d");
        if (context === null) throw new Error("framebuffer 2d context missing");
        const bytes = context.getImageData(4, 448, 712, 24).data;
        let hash = 0x811c9dc5;
        for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193);
        return hash >>> 0;
      };

      const waitForInputPaintAfter = (
        sequence: number,
        before: number,
      ): Promise<number> =>
        new Promise((resolve, reject) => {
          const timeout = window.setTimeout(() => {
            framebuffer.removeEventListener("pmos:frame", onFrame);
            reject(new Error("input did not produce a framebuffer frame"));
          }, 2_000);
          const onFrame = (event: Event): void => {
            const detail = (
              event as CustomEvent<{ sequence: number; paintedAt: number }>
            ).detail;
            if (
              detail.sequence <= sequence ||
              inputRegionFingerprint() === before
            ) {
              return;
            }
            window.clearTimeout(timeout);
            framebuffer.removeEventListener("pmos:frame", onFrame);
            resolve(detail.paintedAt);
          };
          framebuffer.addEventListener("pmos:frame", onFrame);
        });

      const waitForPresentationIdle = (): Promise<void> =>
        new Promise((resolve, reject) => {
          let quietTimer = window.setTimeout(finish, 75);
          const deadline = window.setTimeout(() => {
            cleanup();
            reject(new Error("framebuffer did not become idle"));
          }, 2_000);
          function cleanup(): void {
            window.clearTimeout(quietTimer);
            window.clearTimeout(deadline);
            framebuffer.removeEventListener("pmos:frame", onFrame);
          }
          function finish(): void {
            cleanup();
            resolve();
          }
          function onFrame(): void {
            window.clearTimeout(quietTimer);
            quietTimer = window.setTimeout(finish, 75);
          }
          framebuffer.addEventListener("pmos:frame", onFrame);
        });

      const dispatchKey = (code: string, key: string): void => {
        window.dispatchEvent(new KeyboardEvent("keydown", { code, key }));
        window.dispatchEvent(new KeyboardEvent("keyup", { code, key }));
      };

      // `term: starting` precedes its display handshake. Wait until the
      // launcher's close paint and Terminal's first frame settle, then measure
      // the very first edit before any alternate buffer can be warmed. The
      // fingerprint keeps unrelated taskbar presentations from satisfying the
      // causal boundary.
      await waitForPresentationIdle();
      let sequence = Number(framebuffer.dataset.pmosFrameSequence ?? "0");
      let before = inputRegionFingerprint();
      let painted = waitForInputPaintAfter(sequence, before);
      const coldStartedAt = performance.now();
      dispatchKey("KeyA", "a");
      const coldFirstKeyMs = (await painted) - coldStartedAt;

      // Restore the empty prompt before the steady-state sample set while
      // retaining a causal pixel requirement for the cleanup edit.
      await waitForPresentationIdle();
      sequence = Number(framebuffer.dataset.pmosFrameSequence ?? "0");
      before = inputRegionFingerprint();
      painted = waitForInputPaintAfter(sequence, before);
      dispatchKey("Backspace", "Backspace");
      await painted;
      await waitForPresentationIdle();

      const measured: number[] = [];
      for (let i = 0; i < 20; i += 1) {
        const sequence = Number(framebuffer.dataset.pmosFrameSequence ?? "0");
        const before = inputRegionFingerprint();
        const painted = waitForInputPaintAfter(sequence, before);
        const startedAt = performance.now();
        dispatchKey("KeyA", "a");
        measured.push((await painted) - startedAt);
      }
      return { coldFirstKeyMs, samples: measured };
    });
  } catch (error) {
    throw new Error(
      `input did not reach a painted terminal; console follows:\n${consoleLines.join("\n")}`,
      { cause: error },
    );
  }

  const sorted = [...result.samples].sort((a, b) => a - b);
  const p95 = sorted[Math.ceil(sorted.length * 0.95) - 1];
  console.log(
    `[input-to-pixel] cold_first_ms=${result.coldFirstKeyMs.toFixed(1)} samples_ms=${result.samples.map((value) => value.toFixed(1)).join(",")} p95_ms=${p95?.toFixed(1)}`,
  );
  expect(result.coldFirstKeyMs).toBeGreaterThanOrEqual(0);
  expect(result.coldFirstKeyMs).toBeLessThan(LATENCY_BUDGET_MS);
  expect(p95).toBeDefined();
  expect(p95).toBeLessThan(LATENCY_BUDGET_MS);
});
