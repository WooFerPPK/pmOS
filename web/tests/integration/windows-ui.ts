import { expect, type Page } from "@playwright/test";

export const FRAMEBUFFER_WIDTH = 1024;
export const FRAMEBUFFER_HEIGHT = 768;
export const TASKBAR_Y = 752;
export const TASKBAR_ENTRY_Y = 738;
export const TASKBAR_ENTRY_HEIGHT = 28;
export const TASKBAR_ENTRY_SAMPLE_Y = 740;
export const TASKBAR_FIRST_ENTRY_X = 90;
export const TASKBAR_ENTRY_GAP = 2;
export const TASKBAR_ENTRY_WIDTH = 160;
export const TASKBAR_MIN_ENTRY_WIDTH = 112;
export const TASKBAR_AVAILABLE_WIDTH = 862;

export const TASKBAR_LIGHT = [249, 249, 249, 255] as const;
export const TASKBAR_LIGHT_UNFOCUSED = TASKBAR_LIGHT;
export const TASKBAR_LIGHT_FOCUSED = [233, 233, 233, 255] as const;
export const TASKBAR_LIGHT_MINIMIZED = [237, 237, 237, 255] as const;
export const TASKBAR_DARK = [32, 32, 32, 255] as const;
export const TASKBAR_DARK_UNFOCUSED = TASKBAR_DARK;
export const TASKBAR_DARK_FOCUSED = [58, 58, 58, 255] as const;
export const TASKBAR_DARK_MINIMIZED = [43, 43, 43, 255] as const;

export const LIGHT_TITLEBAR = [249, 249, 249, 255] as const;
export const LIGHT_INACTIVE_TITLEBAR = [237, 237, 237, 255] as const;
export const LIGHT_ACTIVE_BORDER = [0, 103, 192, 255] as const;
export const DARK_TITLEBAR = [32, 32, 32, 255] as const;
export const DARK_ACTIVE_BORDER = [96, 205, 255, 255] as const;

export interface Point {
  readonly x: number;
  readonly y: number;
}

export interface WindowBounds extends Point {
  readonly right: number;
  readonly width: number;
  readonly palette: "light" | "dark";
}

export function taskbarEntryWidth(entryCount: number): number {
  if (entryCount <= 0) return TASKBAR_ENTRY_WIDTH;
  const gaps = TASKBAR_ENTRY_GAP * Math.max(0, entryCount - 1);
  return Math.max(
    TASKBAR_MIN_ENTRY_WIDTH,
    Math.min(
      TASKBAR_ENTRY_WIDTH,
      Math.floor((TASKBAR_AVAILABLE_WIDTH - gaps) / entryCount),
    ),
  );
}

export function taskbarEntryPoint(
  index: number,
  entryCount: number,
  offset = 30,
): Point {
  const width = taskbarEntryWidth(entryCount);
  return {
    x: TASKBAR_FIRST_ENTRY_X + index * (width + TASKBAR_ENTRY_GAP) + offset,
    y: TASKBAR_ENTRY_SAMPLE_Y,
  };
}

export function taskbarEntryRegion(
  index: number,
  entryCount: number,
): Point & { readonly width: number; readonly height: number } {
  const width = taskbarEntryWidth(entryCount);
  return {
    x: TASKBAR_FIRST_ENTRY_X + index * (width + TASKBAR_ENTRY_GAP),
    y: TASKBAR_ENTRY_Y,
    width,
    height: TASKBAR_ENTRY_HEIGHT,
  };
}

export function titlebarControlPoint(
  window: Pick<WindowBounds, "right" | "y">,
  control: "minimize" | "maximize" | "close",
): Point {
  const fromRight = control === "close" ? 16 : control === "maximize" ? 46 : 76;
  return { x: window.right - fromRight, y: window.y + 11 };
}

export function activeWindowBorderPoint(
  window: Pick<WindowBounds, "x" | "y" | "width">,
): Point {
  return {
    x: window.x + Math.floor(window.width / 2),
    y: window.y,
  };
}

/**
 * Find the top edge of the focused client-painted frame. Focused light and
 * dark windows use a one-pixel accent border around the whole surface. The
 * top edge is distinguished from the bottom edge by the continuing left
 * border on the following row. This avoids coupling tests to an app-specific
 * titlebar colour now that all bundled apps share the same chrome.
 */
export async function activeWindowBounds(
  page: Page,
  minimumWidth = 180,
): Promise<WindowBounds | null> {
  return page.locator("#pmos-fb").evaluate(
    (
      canvas: HTMLCanvasElement,
      options: {
        minimumWidth: number;
        palettes: readonly {
          readonly name: "light" | "dark";
          readonly rgba: readonly [number, number, number, number];
        }[];
      },
    ) => {
      const context = canvas.getContext("2d");
      if (context === null) return null;
      const height = Math.min(canvas.height, 736);
      const image = context.getImageData(0, 0, canvas.width, height);
      const matches = (
        x: number,
        y: number,
        rgba: readonly [number, number, number, number],
      ): boolean => {
        if (x < 0 || x >= image.width || y < 0 || y >= image.height) {
          return false;
        }
        const offset = (y * image.width + x) * 4;
        return rgba.every(
          (channel, index) => image.data[offset + index] === channel,
        );
      };

      let best: WindowBounds | null = null;
      for (const palette of options.palettes) {
        for (let y = 0; y < image.height - 1; y += 1) {
          let x = 0;
          while (x < image.width) {
            if (!matches(x, y, palette.rgba)) {
              x += 1;
              continue;
            }
            const start = x;
            while (x < image.width && matches(x, y, palette.rgba)) x += 1;
            const width = x - start;
            if (width < options.minimumWidth) continue;
            const middle = start + Math.floor(width / 2);
            if (
              !matches(start, y + 1, palette.rgba) ||
              matches(middle, y + 1, palette.rgba)
            ) {
              continue;
            }
            if (
              best === null ||
              width > best.width ||
              (width === best.width && y < best.y)
            ) {
              best = {
                x: start,
                y,
                right: start + width,
                width,
                palette: palette.name,
              };
            }
          }
        }
      }
      return best;
    },
    {
      minimumWidth,
      palettes: [
        { name: "light" as const, rgba: LIGHT_ACTIVE_BORDER },
        { name: "dark" as const, rgba: DARK_ACTIVE_BORDER },
      ],
    },
  );
}

/**
 * Wait for a complete focused-frame presentation and retain the exact bounds
 * observed by the successful poll. Reading the canvas again after a poll can
 * land on the compositor's intermediate focus/reconfigure frame.
 */
export async function waitForActiveWindowBounds(
  page: Page,
  options: {
    readonly differentFrom?: Pick<WindowBounds, "x" | "y" | "width">;
    readonly expectedX?: number;
    readonly expectedY?: number;
    readonly expectedWidth?: number;
    readonly minimumWidth?: number;
    readonly timeout?: number;
    readonly message?: string;
  } = {},
): Promise<WindowBounds> {
  let observed: WindowBounds | null = null;
  await expect
    .poll(
      async () => {
        const bounds = await activeWindowBounds(
          page,
          options.minimumWidth ?? 180,
        );
        const accepted =
          bounds !== null &&
          (options.expectedX === undefined || bounds.x === options.expectedX) &&
          (options.expectedY === undefined || bounds.y === options.expectedY) &&
          (options.expectedWidth === undefined ||
            bounds.width === options.expectedWidth) &&
          (options.differentFrom === undefined ||
            bounds.x !== options.differentFrom.x ||
            bounds.y !== options.differentFrom.y ||
            bounds.width !== options.differentFrom.width)
            ? bounds
            : null;
        if (accepted !== null) observed = accepted;
        return accepted;
      },
      {
        timeout: options.timeout ?? 5_000,
        message: options.message ?? "focused window frame did not appear",
      },
    )
    .not.toBeNull();
  if (observed === null) {
    throw new Error(options.message ?? "focused window frame did not appear");
  }
  return observed;
}
