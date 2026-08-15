// Framebuffer renderer — main-thread painter that consumes
// `FbHost` mode + frame events and writes pixels into a
// `<canvas>` element.
//
// Full frames paint directly through visible-canvas `putImageData`.
// An OffscreenCanvas factory is still probed at mode-set time for the
// existing capability diagnostic, but its context is not retained or used:
// the former transferToImageBitmap path produced striped partial-frame
// artifacts in Chromium.
//
// Rectangular patches always use one rect-sized `ImageData` and one
// visible-canvas `putImageData(x, y)`. They do not allocate or copy a
// full framebuffer and do not snapshot the offscreen full-frame surface.
//
// After every full-frame, patch, or atomic patch-batch paint completes the renderer fires a
// `present_complete` event so callers (a future
// frame-rate-aware compositor) can pace their next blit
// request. The event is fire-and-forget; multiple subscribers
// are notified in registration order.
//
// The renderer is independent of `FbHost` — callers wire
// `host.onFrame(renderer.paintFrame.bind(renderer))` and
// `host.onPatch(renderer.paintPatch.bind(renderer))`, and
// `host.onModeChange(renderer.setMode.bind(renderer))` to
// connect them.

/** Subset of `OffscreenCanvas` actually used by the renderer. */
export interface OffscreenCanvasLike {
  width: number;
  height: number;
  getContext(contextId: "2d"): OffscreenCanvasRenderingContextLike | null;
  transferToImageBitmap(): ImageBitmap;
}

/** Subset of the offscreen 2D context the renderer uses. */
export interface OffscreenCanvasRenderingContextLike {
  putImageData(data: ImageDataLike, dx: number, dy: number): void;
}

/**
 * Subset of `ImageData`. Constructed manually because jsdom's
 * `ImageData` constructor is missing.
 */
export interface ImageDataLike {
  readonly width: number;
  readonly height: number;
  readonly data: Uint8ClampedArray;
}

/** Subset of the visible-canvas 2D context the renderer uses. */
export interface CanvasRenderingContext2DLike {
  putImageData(data: ImageDataLike, dx: number, dy: number): void;
  drawImage(bitmap: ImageBitmap, dx: number, dy: number): void;
}

/** Subset of HTMLCanvasElement actually used by the renderer. */
export interface CanvasLike {
  width: number;
  height: number;
  getContext(contextId: "2d"): CanvasRenderingContext2DLike | null;
}

/** Factory the renderer uses to create the offscreen canvas. */
export type OffscreenCanvasFactory = (width: number, height: number) => OffscreenCanvasLike | null;

/** Factory the renderer uses to construct ImageData. */
export type ImageDataFactory = (rgba: Uint8ClampedArray, width: number, height: number) => ImageDataLike;

export interface FbRendererOptions {
  /**
   * Visible canvas to paint onto.
   */
  readonly canvas: CanvasLike;
  /** Override the factory used for the OffscreenCanvas capability probe. */
  readonly offscreenCanvasFactory?: OffscreenCanvasFactory;
  /**
   * Override the `ImageData` constructor. Defaults to the
   * global `ImageData` when present; jsdom-based tests pass a
   * shim that builds a plain `{ width, height, data }` object
   * since jsdom's ImageData is missing.
   */
  readonly imageDataFactory?: ImageDataFactory;
}

export type PresentCompleteHandler = () => void;

export interface FramebufferPatch {
  readonly x: number;
  readonly y: number;
  readonly width: number;
  readonly height: number;
  readonly rgba: Uint8Array;
}

interface PreparedPatch {
  readonly x: number;
  readonly y: number;
  readonly imageData: ImageDataLike;
}

export class FbRenderer {
  private readonly canvas: CanvasLike;
  private readonly offscreenFactory: OffscreenCanvasFactory;
  private readonly imageDataFactory: ImageDataFactory;
  private readonly handlers: PresentCompleteHandler[] = [];
  private currentMode: { width: number; height: number } | null = null;
  /**
   * Number of frames painted since construction. Read by tests
   * to assert the render loop ran the expected number of times
   * without needing a canvas-pixel comparison.
   */
  presentsCompleted = 0;
  /** Legacy diagnostic reporting whether OffscreenCanvas 2D is available. */
  usingFastPath = false;

  constructor(options: FbRendererOptions) {
    this.canvas = options.canvas;
    this.offscreenFactory =
      options.offscreenCanvasFactory ?? defaultOffscreenFactory;
    this.imageDataFactory =
      options.imageDataFactory ?? defaultImageDataFactory;
  }

  /** Subscribe to present-complete events. */
  onPresentComplete(handler: PresentCompleteHandler): void {
    this.handlers.push(handler);
  }

  /**
   * Resize the canvas and refresh the OffscreenCanvas capability probe.
   * Idempotent: passing the same geometry as the current mode is a no-op.
   */
  setMode(mode: { width: number; height: number }): void {
    if (
      this.currentMode !== null &&
      this.currentMode.width === mode.width &&
      this.currentMode.height === mode.height
    ) {
      return;
    }
    this.currentMode = mode;
    this.canvas.width = mode.width;
    this.canvas.height = mode.height;
    const offscreen = this.offscreenFactory(mode.width, mode.height);
    const offscreenCtx = offscreen?.getContext("2d") ?? null;
    this.usingFastPath = offscreenCtx !== null;
  }

  /**
   * Paint one RGBA8 frame. The renderer assumes the frame's
   * geometry matches the most recent `setMode`; mismatched
   * frames are dropped so a stale blit doesn't smear wrong-
   * sized pixels into the canvas.
   */
  paintFrame(frame: { width: number; height: number; rgba: Uint8Array }): void {
    if (this.currentMode === null) {
      return;
    }
    if (
      frame.width !== this.currentMode.width ||
      frame.height !== this.currentMode.height
    ) {
      return;
    }
    const rgba = new Uint8ClampedArray(
      frame.rgba.buffer,
      frame.rgba.byteOffset,
      frame.rgba.byteLength,
    );
    const imageData = this.imageDataFactory(rgba, frame.width, frame.height);

    // Paint full frames directly on the visible canvas. The
    // OffscreenCanvas + transferToImageBitmap path previously produced
    // reproducible row-stride/partial-paint artifacts in Chromium. Steady-
    // state performance comes from the rectangular patch path below, so
    // there is no reason to trade full-frame correctness for that fast path.
    const ctx = this.canvas.getContext("2d");
    if (ctx !== null) {
      ctx.putImageData(imageData, 0, 0);
    }

    this.finishPresent();
  }

  /**
   * Paint one tightly packed RGBA8 rectangle into the current mode.
   * Invalid, empty, out-of-bounds, or byte-count-mismatched patches
   * are dropped without firing a present-complete notification.
   *
   * The visible context is updated directly, keeping work proportional to
   * the damage rectangle. Full frames use the same visible-canvas path.
   */
  paintPatch(patch: FramebufferPatch): void {
    const ctx = this.canvas.getContext("2d");
    if (ctx === null) {
      return;
    }
    const prepared = this.preparePatch(patch);
    if (prepared === null) {
      return;
    }
    ctx.putImageData(prepared.imageData, prepared.x, prepared.y);
    this.finishPresent();
  }

  /**
   * Validate every rectangle before painting any of them, then update the
   * visible canvas and emit exactly one completion. Canvas writes happen in
   * one main-thread task, so no partially updated batch is observable.
   */
  paintPatchBatch(patches: readonly FramebufferPatch[]): void {
    if (patches.length === 0) {
      return;
    }
    const ctx = this.canvas.getContext("2d");
    if (ctx === null) {
      return;
    }
    const prepared: PreparedPatch[] = [];
    for (const patch of patches) {
      const next = this.preparePatch(patch);
      if (next === null) {
        return;
      }
      prepared.push(next);
    }
    for (const patch of prepared) {
      ctx.putImageData(patch.imageData, patch.x, patch.y);
    }
    this.finishPresent();
  }

  private preparePatch(patch: FramebufferPatch): PreparedPatch | null {
    const mode = this.currentMode;
    if (mode === null) {
      return null;
    }
    if (
      !Number.isSafeInteger(patch.x) ||
      !Number.isSafeInteger(patch.y) ||
      !Number.isSafeInteger(patch.width) ||
      !Number.isSafeInteger(patch.height) ||
      patch.x < 0 ||
      patch.y < 0 ||
      patch.width <= 0 ||
      patch.height <= 0
    ) {
      return null;
    }
    const right = patch.x + patch.width;
    const bottom = patch.y + patch.height;
    const pixelCount = patch.width * patch.height;
    const pixelBytes = pixelCount * 4;
    if (
      !Number.isSafeInteger(right) ||
      !Number.isSafeInteger(bottom) ||
      !Number.isSafeInteger(pixelCount) ||
      !Number.isSafeInteger(pixelBytes) ||
      right > mode.width ||
      bottom > mode.height ||
      pixelBytes !== patch.rgba.byteLength
    ) {
      return null;
    }
    const rgba = new Uint8ClampedArray(
      patch.rgba.buffer,
      patch.rgba.byteOffset,
      patch.rgba.byteLength,
    );
    const imageData = this.imageDataFactory(
      rgba,
      patch.width,
      patch.height,
    );
    return { x: patch.x, y: patch.y, imageData };
  }

  private finishPresent(): void {
    this.presentsCompleted += 1;
    for (const h of this.handlers) {
      h();
    }
  }
}

function defaultOffscreenFactory(width: number, height: number): OffscreenCanvasLike | null {
  const Ctor = (globalThis as unknown as { OffscreenCanvas?: { new (w: number, h: number): OffscreenCanvasLike } })
    .OffscreenCanvas;
  if (Ctor === undefined) return null;
  try {
    const oc = new Ctor(width, height);
    // OffscreenCanvas without `transferToImageBitmap` is rare
    // but technically possible (older Firefox); detect and bail
    // out so the fallback path engages.
    if (typeof (oc as unknown as { transferToImageBitmap?: unknown }).transferToImageBitmap !== "function") {
      return null;
    }
    return oc;
  } catch {
    return null;
  }
}

function defaultImageDataFactory(
  data: Uint8ClampedArray,
  width: number,
  height: number,
): ImageDataLike {
  const Ctor = (globalThis as unknown as { ImageData?: { new (data: Uint8ClampedArray, width: number, height: number): ImageDataLike } })
    .ImageData;
  if (Ctor !== undefined) {
    try {
      return new Ctor(data, width, height);
    } catch {
      // Fall through to the plain-object shape below.
    }
  }
  return { width, height, data };
}
