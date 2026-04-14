// Main-thread subscription side of /dev/fb0.
//
// Listens for `fb:set-mode` and `fb:blit` messages on the
// kernel Worker and fans them out to caller-provided
// handlers. Does NOT own a canvas: callers wire up a handler
// that translates `FbFrame` events into actual DOM paints.
//
// Multiple handlers may subscribe via `onFrame` /
// `onModeChange`; they fire in registration order.
//
// FbHost does NOT send a boot message of its own — the
// `ConsoleHost` (or whatever other main-thread wrapper owns
// the Worker's lifecycle) is responsible for boot. FbHost
// just adds its own `message` listener to the shared worker,
// mirroring how multiple DOM EventTarget listeners coexist on
// the same event source.

import type { WorkerLike } from "./console-host";
import type { KernelToMain } from "./shared/worker-proto";

/** Current logical framebuffer geometry, if any. */
export interface FbMode {
  readonly width: number;
  readonly height: number;
}

/** A single blit event. */
export interface FbFrame {
  readonly width: number;
  readonly height: number;
  readonly rgba: Uint8Array;
}

export type FbFrameHandler = (frame: FbFrame) => void;
export type FbModeHandler = (mode: FbMode) => void;

export interface FbHostOptions {
  readonly worker: WorkerLike;
}

export class FbHost {
  private readonly frameHandlers: FbFrameHandler[] = [];
  private readonly modeHandlers: FbModeHandler[] = [];
  private currentMode: FbMode | null = null;
  private blitCount = 0;

  constructor(options: FbHostOptions) {
    options.worker.addEventListener("message", (ev) => {
      this.handleMessage(ev.data);
    });
  }

  /** Most recent mode set, or null if none has been set. */
  get mode(): FbMode | null {
    return this.currentMode;
  }

  /** Number of blits observed since construction. */
  get blitsObserved(): number {
    return this.blitCount;
  }

  /** Subscribe to blit events. */
  onFrame(handler: FbFrameHandler): void {
    this.frameHandlers.push(handler);
  }

  /** Subscribe to mode-change events. */
  onModeChange(handler: FbModeHandler): void {
    this.modeHandlers.push(handler);
  }

  private handleMessage(msg: KernelToMain): void {
    switch (msg.kind) {
      case "fb:set-mode": {
        const mode: FbMode = { width: msg.width, height: msg.height };
        this.currentMode = mode;
        for (const h of this.modeHandlers) {
          h(mode);
        }
        return;
      }
      case "fb:blit": {
        this.blitCount += 1;
        const frame: FbFrame = {
          width: msg.width,
          height: msg.height,
          rgba: msg.rgba,
        };
        for (const h of this.frameHandlers) {
          h(frame);
        }
        return;
      }
      default:
        // Ignore console:write, ready, panic — those are
        // handled by ConsoleHost.
        return;
    }
  }
}
