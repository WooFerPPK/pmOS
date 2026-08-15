import { expect, type Page } from "@playwright/test";

const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const LAUNCHER_X = 40;
const TASKBAR_Y = 752;
const MENU_EVIDENCE_X = 190;
const MENU_EVIDENCE_Y = 610;
const MENU_REGION = { x: 4, y: 608, width: 200, height: 128 } as const;
const MENU_MARKER = {
  x: 4,
  width: 200,
  bottom: 736,
  palettes: [
    { background: [0xf2, 0xf2, 0xf2], border: [0x3b, 0x5b, 0x8d] },
    { background: [0x1c, 0x1f, 0x26], border: [0x5b, 0x7c, 0xc2] },
  ],
} as const;

async function framebufferPixel(
  page: Page,
  x: number,
  y: number,
): Promise<number[]> {
  return page.locator("#pmos-fb").evaluate(
    (canvas: HTMLCanvasElement, point: { x: number; y: number }) => {
      const context = canvas.getContext("2d");
      if (context === null) throw new Error("framebuffer 2d context missing");
      return Array.from(context.getImageData(point.x, point.y, 1, 1).data);
    },
    { x, y },
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
        if (context === null) throw new Error("framebuffer 2d context missing");
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

export async function launcherMenuIsOpen(page: Page): Promise<boolean> {
  return page.locator("#pmos-fb").evaluate(
    (
      canvas: HTMLCanvasElement,
      marker: {
        x: number;
        width: number;
        bottom: number;
        palettes: readonly {
          background: readonly [number, number, number];
          border: readonly [number, number, number];
        }[];
      },
    ) => {
      const context = canvas.getContext("2d");
      if (context === null) return false;
      const left = context.getImageData(marker.x, 0, 1, marker.bottom).data;
      const innerLeft = context.getImageData(
        marker.x + 6,
        0,
        1,
        marker.bottom,
      ).data;
      const innerRight = context.getImageData(
        marker.x + marker.width - 10,
        0,
        1,
        marker.bottom,
      ).data;
      const rgbAt = (column: Uint8ClampedArray, y: number): number[] => {
        const offset = y * 4;
        return Array.from(column.slice(offset, offset + 3));
      };
      const matches = (
        actual: number[],
        expected: readonly number[],
      ): boolean =>
        actual.every((channel, index) => channel === expected[index]);

      // The launcher grows upward as packages add rows. Detect its real top
      // border instead of baking in the five-entry catalog's y=608 geometry.
      for (let top = marker.bottom - 32; top >= 0; top -= 24) {
        for (const palette of marker.palettes) {
          if (
            matches(rgbAt(left, top), palette.border) &&
            matches(rgbAt(left, marker.bottom - 1), palette.border) &&
            matches(rgbAt(innerLeft, top + 2), palette.background) &&
            matches(rgbAt(innerRight, top + 2), palette.background)
          ) {
            return true;
          }
        }
      }
      return false;
    },
    MENU_MARKER,
  );
}

export async function clickFramebuffer(
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

export async function openLauncherBefore(
  page: Page,
  deadlineMs: number,
): Promise<number> {
  const canvas = page.locator("#pmos-fb");

  const closedRegionFingerprint = await framebufferRegionFingerprint(
    page,
    MENU_REGION,
  );
  expect(await launcherMenuIsOpen(page), "launcher starts closed").toBe(false);
  const before = await framebufferPixel(page, MENU_EVIDENCE_X, MENU_EVIDENCE_Y);
  const sequenceBefore = Number(
    (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
  );
  await canvas.evaluate(
    (
      framebuffer: HTMLCanvasElement,
      marker: {
        x: number;
        width: number;
        bottom: number;
        palettes: readonly {
          background: readonly [number, number, number];
          border: readonly [number, number, number];
        }[];
      },
    ) => {
      const frameBefore = Number(framebuffer.dataset.pmosFrameSequence ?? "0");
      let pointerDownSeen = false;
      delete framebuffer.dataset.pmosLauncherMenuFrame;

      const menuIsOpen = (): boolean => {
        const context = framebuffer.getContext("2d");
        if (context === null) return false;
        const left = context.getImageData(marker.x, 0, 1, marker.bottom).data;
        const innerLeft = context.getImageData(
          marker.x + 6,
          0,
          1,
          marker.bottom,
        ).data;
        const innerRight = context.getImageData(
          marker.x + marker.width - 10,
          0,
          1,
          marker.bottom,
        ).data;
        const rgbAt = (column: Uint8ClampedArray, y: number): number[] => {
          const offset = y * 4;
          return Array.from(column.slice(offset, offset + 3));
        };
        const matches = (
          actual: number[],
          expected: readonly number[],
        ): boolean =>
          actual.every((channel, index) => channel === expected[index]);
        for (let top = marker.bottom - 32; top >= 0; top -= 24) {
          for (const palette of marker.palettes) {
            if (
              matches(rgbAt(left, top), palette.border) &&
              matches(rgbAt(left, marker.bottom - 1), palette.border) &&
              matches(rgbAt(innerLeft, top + 2), palette.background) &&
              matches(rgbAt(innerRight, top + 2), palette.background)
            ) {
              return true;
            }
          }
        }
        return false;
      };

      function onPointerDown(): void {
        pointerDownSeen = true;
        framebuffer.removeEventListener("pointerdown", onPointerDown);
      }

      function onFrame(event: Event): void {
        const sequence = (event as CustomEvent<{ sequence: number }>).detail
          .sequence;
        if (!pointerDownSeen || sequence <= frameBefore || !menuIsOpen()) {
          return;
        }
        framebuffer.removeEventListener("pmos:frame", onFrame);
        framebuffer.dataset.pmosLauncherMenuFrame = String(sequence);
      }

      framebuffer.addEventListener("pointerdown", onPointerDown);
      framebuffer.addEventListener("pmos:frame", onFrame);
    },
    MENU_MARKER,
  );

  await clickFramebuffer(page, LAUNCHER_X, TASKBAR_Y);

  const timeout = deadlineMs - Date.now();
  if (timeout <= 0) {
    throw new Error("launcher click missed the boot deadline");
  }
  try {
    await expect
      .poll(() => canvas.getAttribute("data-pmos-launcher-menu-frame"), {
        timeout,
      })
      .not.toBeNull();
  } catch (cause) {
    const sequenceAfter = Number(
      (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
    );
    const after = await framebufferPixel(
      page,
      MENU_EVIDENCE_X,
      MENU_EVIDENCE_Y,
    );
    throw new Error(
      `launcher produced no causal menu presentation; ` +
        `frame_sequence=${sequenceBefore}->${sequenceAfter} ` +
        `menu_pixel=${JSON.stringify(before)}->${JSON.stringify(after)}`,
      { cause },
    );
  }

  expect(await launcherMenuIsOpen(page)).toBe(true);
  return closedRegionFingerprint;
}

export async function launcherMenuRegionFingerprint(
  page: Page,
): Promise<number> {
  return framebufferRegionFingerprint(page, MENU_REGION);
}

export async function openLauncherMenuBefore(
  page: Page,
  deadlineMs: number,
): Promise<void> {
  const canvas = page.locator("#pmos-fb");
  expect(await launcherMenuIsOpen(page), "launcher starts closed").toBe(false);
  const before = await framebufferRegionFingerprint(page, MENU_REGION);
  const sequenceBefore = Number(
    (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
  );
  await canvas.evaluate(
    (
      framebuffer: HTMLCanvasElement,
      marker: {
        x: number;
        width: number;
        bottom: number;
        palettes: readonly {
          background: readonly [number, number, number];
          border: readonly [number, number, number];
        }[];
      },
    ) => {
      const frameBefore = Number(framebuffer.dataset.pmosFrameSequence ?? "0");
      let pointerDownSeen = false;
      delete framebuffer.dataset.pmosLauncherMenuRegionFrame;

      const menuIsOpen = (): boolean => {
        const context = framebuffer.getContext("2d");
        if (context === null) return false;
        const left = context.getImageData(marker.x, 0, 1, marker.bottom).data;
        const innerLeft = context.getImageData(
          marker.x + 6,
          0,
          1,
          marker.bottom,
        ).data;
        const innerRight = context.getImageData(
          marker.x + marker.width - 10,
          0,
          1,
          marker.bottom,
        ).data;
        const rgbAt = (column: Uint8ClampedArray, y: number): number[] => {
          const offset = y * 4;
          return Array.from(column.slice(offset, offset + 3));
        };
        const matches = (
          actual: number[],
          expected: readonly number[],
        ): boolean =>
          actual.every((channel, index) => channel === expected[index]);
        for (let top = marker.bottom - 32; top >= 0; top -= 24) {
          for (const palette of marker.palettes) {
            if (
              matches(rgbAt(left, top), palette.border) &&
              matches(rgbAt(left, marker.bottom - 1), palette.border) &&
              matches(rgbAt(innerLeft, top + 2), palette.background) &&
              matches(rgbAt(innerRight, top + 2), palette.background)
            ) {
              return true;
            }
          }
        }
        return false;
      };

      function onPointerDown(): void {
        pointerDownSeen = true;
        framebuffer.removeEventListener("pointerdown", onPointerDown);
      }

      function onFrame(event: Event): void {
        const sequence = (event as CustomEvent<{ sequence: number }>).detail
          .sequence;
        if (!pointerDownSeen || sequence <= frameBefore || !menuIsOpen()) {
          return;
        }
        framebuffer.removeEventListener("pmos:frame", onFrame);
        framebuffer.dataset.pmosLauncherMenuRegionFrame = String(sequence);
      }

      framebuffer.addEventListener("pointerdown", onPointerDown);
      framebuffer.addEventListener("pmos:frame", onFrame);
    },
    MENU_MARKER,
  );

  await clickFramebuffer(page, LAUNCHER_X, TASKBAR_Y);
  const timeout = deadlineMs - Date.now();
  if (timeout <= 0) throw new Error("launcher click missed its deadline");
  try {
    await expect
      .poll(() => canvas.getAttribute("data-pmos-launcher-menu-region-frame"), {
        timeout,
      })
      .not.toBeNull();
  } catch (cause) {
    const sequenceAfter = Number(
      (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
    );
    const after = await framebufferRegionFingerprint(page, MENU_REGION);
    throw new Error(
      `launcher produced no causal menu-region presentation; ` +
        `frame_sequence=${sequenceBefore}->${sequenceAfter} ` +
        `menu_fingerprint=${before}->${after}`,
      { cause },
    );
  }
  expect(await launcherMenuIsOpen(page)).toBe(true);
}

/**
 * Select one row from an already-open launcher and wait until Shell has
 * causally repainted the bounded launcher region. Waiting only for the spawn
 * log can race the menu-close commit because spawning precedes Shell's paint.
 */
export async function selectLauncherRowBefore(
  page: Page,
  x: number,
  y: number,
  deadlineMs: number,
  closedFingerprint?: number,
): Promise<number> {
  const canvas = page.locator("#pmos-fb");
  const before = await framebufferRegionFingerprint(page, MENU_REGION);
  expect(
    await launcherMenuIsOpen(page),
    "launcher row selection requires an open menu",
  ).toBe(true);
  const sequenceBefore = Number(
    (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
  );
  await canvas.evaluate(
    (
      framebuffer: HTMLCanvasElement,
      marker: {
        x: number;
        width: number;
        bottom: number;
        palettes: readonly {
          background: readonly [number, number, number];
          border: readonly [number, number, number];
        }[];
      },
    ) => {
      const frameBefore = Number(framebuffer.dataset.pmosFrameSequence ?? "0");
      let pointerDownSeen = false;
      delete framebuffer.dataset.pmosLauncherMenuCloseFrame;

      const menuIsOpen = (): boolean => {
        const context = framebuffer.getContext("2d");
        if (context === null) return true;
        const left = context.getImageData(marker.x, 0, 1, marker.bottom).data;
        const innerLeft = context.getImageData(
          marker.x + 6,
          0,
          1,
          marker.bottom,
        ).data;
        const innerRight = context.getImageData(
          marker.x + marker.width - 10,
          0,
          1,
          marker.bottom,
        ).data;
        const rgbAt = (column: Uint8ClampedArray, y: number): number[] => {
          const offset = y * 4;
          return Array.from(column.slice(offset, offset + 3));
        };
        const matches = (
          actual: number[],
          expected: readonly number[],
        ): boolean =>
          actual.every((channel, index) => channel === expected[index]);
        for (let top = marker.bottom - 32; top >= 0; top -= 24) {
          for (const palette of marker.palettes) {
            if (
              matches(rgbAt(left, top), palette.border) &&
              matches(rgbAt(left, marker.bottom - 1), palette.border) &&
              matches(rgbAt(innerLeft, top + 2), palette.background) &&
              matches(rgbAt(innerRight, top + 2), palette.background)
            ) {
              return true;
            }
          }
        }
        return false;
      };

      function onPointerDown(): void {
        pointerDownSeen = true;
        framebuffer.removeEventListener("pointerdown", onPointerDown);
      }

      function onFrame(event: Event): void {
        const sequence = (event as CustomEvent<{ sequence: number }>).detail
          .sequence;
        if (!pointerDownSeen || sequence <= frameBefore || menuIsOpen()) {
          return;
        }
        framebuffer.removeEventListener("pmos:frame", onFrame);
        framebuffer.dataset.pmosLauncherMenuCloseFrame = String(sequence);
      }

      framebuffer.addEventListener("pointerdown", onPointerDown);
      framebuffer.addEventListener("pmos:frame", onFrame);
    },
    MENU_MARKER,
  );

  await clickFramebuffer(page, x, y);
  const timeout = deadlineMs - Date.now();
  if (timeout <= 0) throw new Error("launcher selection missed its deadline");
  try {
    await expect
      .poll(() => canvas.getAttribute("data-pmos-launcher-menu-close-frame"), {
        timeout,
      })
      .not.toBeNull();
  } catch (cause) {
    const sequenceAfter = Number(
      (await canvas.getAttribute("data-pmos-frame-sequence")) ?? "0",
    );
    const after = await framebufferRegionFingerprint(page, MENU_REGION);
    throw new Error(
      `launcher row produced no causal close presentation; ` +
        `frame_sequence=${sequenceBefore}->${sequenceAfter} ` +
        `menu_fingerprint=${before}->${after} ` +
        `expected_closed=${closedFingerprint ?? "unavailable"}`,
      { cause },
    );
  }
  const closeFrame = Number(
    (await canvas.getAttribute("data-pmos-launcher-menu-close-frame")) ?? "0",
  );
  expect(closeFrame).toBeGreaterThan(sequenceBefore);
  return closeFrame;
}
