// Framebuffer renderer — main-thread painter that consumes
// `FbHost` mode + frame events and writes pixels into a
// `<canvas>` element.
//
// Two render paths, picked by feature detection at
// construction time:
//
//   * **Fast path (production)** — an offscreen `OffscreenCanvas`
//     is created with the same backing-store geometry as the
//     visible canvas. Each frame is composed offscreen with
//     `putImageData`, then snapshot via `transferToImageBitmap`
//     and drawn onto the visible canvas via `drawImage`. The
//     bitmap transfer is zero-copy on Chrome/Firefox, and the
//     visible canvas's compositor only sees one drawImage per
//     frame instead of a putImageData (which forces a full
//     CPU→GPU upload on most browsers).
//
//   * **Fallback** — when `OffscreenCanvas` or
//     `transferToImageBitmap` is unavailable (older Safari,
//     jsdom, the early bootstrap path before T080 settled),
//     paint directly via `putImageData` onto the visible
//     canvas's 2D context.
//
// Either way, after every paint completes the renderer fires a
// `present_complete` event so callers (a future
// frame-rate-aware compositor) can pace their next blit
// request. The event is fire-and-forget; multiple subscribers
// are notified in registration order.
//
// The renderer is independent of `FbHost` — callers wire
// `host.onFrame(renderer.paintFrame.bind(renderer))` and
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
  /**
   * Override the offscreen-canvas factory. Defaults to
   * `globalThis.OffscreenCanvas` when present, else returns
   * null (fallback path engages).
   */
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

export class FbRenderer {
  private readonly canvas: CanvasLike;
  private readonly offscreenFactory: OffscreenCanvasFactory;
  private readonly imageDataFactory: ImageDataFactory;
  private offscreen: OffscreenCanvasLike | null = null;
  private offscreenCtx: OffscreenCanvasRenderingContextLike | null = null;
  private readonly handlers: PresentCompleteHandler[] = [];
  private currentMode: { width: number; height: number } | null = null;
  /**
   * Number of frames painted since construction. Read by tests
   * to assert the render loop ran the expected number of times
   * without needing a canvas-pixel comparison.
   */
  presentsCompleted = 0;
  /**
   * Tracks whether the renderer is using the OffscreenCanvas
   * fast path. Read by tests; the value is decided by
   * `setMode` based on the factory's return value.
   */
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
   * Resize the canvas + (re-)create the offscreen surface for
   * the new mode. Idempotent: passing the same geometry as the
   * current mode is a no-op.
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
    if (offscreen !== null) {
      this.offscreen = offscreen;
      this.offscreenCtx = offscreen.getContext("2d");
      this.usingFastPath = this.offscreenCtx !== null;
    } else {
      this.offscreen = null;
      this.offscreenCtx = null;
      this.usingFastPath = false;
    }
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

    // Direct putImageData on the visible canvas. The
    // OffscreenCanvas + transferToImageBitmap fast path
    // exhibited row-stride / partial-paint artifacts on
    // some Chromium builds that produced striped rendering;
    // the slow path is straightforward and visibly correct.
    const ctx = this.canvas.getContext("2d");
    if (ctx !== null) {
      ctx.putImageData(imageData, 0, 0);
    }

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
