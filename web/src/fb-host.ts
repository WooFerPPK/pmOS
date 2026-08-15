// Main-thread subscription side of /dev/fb0.
//
// Listens for `fb:set-mode`, `fb:blit`, `fb:patch`, atomic patch-batch, and
// `fb:present-fence` messages on the
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

/** A tightly packed rectangular RGBA8 update. */
export interface FbPatch {
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
  readonly rgba: Uint8Array;
}

/** Multiple rectangles belonging to one atomic presentation. */
export interface FbPatchBatch {
  readonly patches: readonly FbPatch[];
}

export type FbFrameHandler = (frame: FbFrame) => void;
export type FbPatchHandler = (patch: FbPatch) => void;
export type FbPatchBatchHandler = (batch: FbPatchBatch) => void;
export type FbModeHandler = (mode: FbMode) => void;
export type FbPresentFenceHandler = (serial: number) => void;

export interface FbHostOptions {
  readonly worker: WorkerLike;
}

export class FbHost {
  private readonly frameHandlers: FbFrameHandler[] = [];
  private readonly patchHandlers: FbPatchHandler[] = [];
  private readonly patchBatchHandlers: FbPatchBatchHandler[] = [];
  private readonly modeHandlers: FbModeHandler[] = [];
  private readonly presentFenceHandlers: FbPresentFenceHandler[] = [];
  private currentMode: FbMode | null = null;
  private blitCount = 0;
  private patchCount = 0;

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

  /** Number of rectangular patches observed since construction. */
  get patchesObserved(): number {
    return this.patchCount;
  }

  /** Subscribe to blit events. */
  onFrame(handler: FbFrameHandler): void {
    this.frameHandlers.push(handler);
  }

  /** Subscribe to rectangular patch events. */
  onPatch(handler: FbPatchHandler): void {
    this.patchHandlers.push(handler);
  }

  /** Subscribe to atomic rectangular-patch batches. */
  onPatchBatch(handler: FbPatchBatchHandler): void {
    this.patchBatchHandlers.push(handler);
  }

  /** Subscribe to mode-change events. */
  onModeChange(handler: FbModeHandler): void {
    this.modeHandlers.push(handler);
  }

  /** Subscribe to display-server presentation fences. */
  onPresentFence(handler: FbPresentFenceHandler): void {
    this.presentFenceHandlers.push(handler);
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
      case "fb:patch": {
        this.patchCount += 1;
        const patch: FbPatch = {
          x: msg.x,
          y: msg.y,
          width: msg.width,
          height: msg.height,
          rgba: msg.rgba,
        };
        for (const h of this.patchHandlers) {
          h(patch);
        }
        return;
      }
      case "fb:patch-batch": {
        this.patchCount += msg.patches.length;
        const batch: FbPatchBatch = {
          patches: msg.patches.map((patch) => ({
            x: patch.x,
            y: patch.y,
            width: patch.width,
            height: patch.height,
            rgba: patch.rgba,
          })),
        };
        for (const h of this.patchBatchHandlers) {
          h(batch);
        }
        return;
      }
      case "fb:present-fence": {
        for (const h of this.presentFenceHandlers) {
          h(msg.serial);
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
