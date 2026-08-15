// T081 production-path: FbRenderer tests covering OffscreenCanvas
// capability detection, artifact-safe direct full-frame paints,
// rectangular patches, and present_complete event delivery.

import { describe, expect, it, vi } from "vitest";

import {
  FbRenderer,
  type CanvasLike,
  type CanvasRenderingContext2DLike,
  type ImageDataFactory,
  type ImageDataLike,
  type OffscreenCanvasFactory,
  type OffscreenCanvasLike,
  type OffscreenCanvasRenderingContextLike,
} from "../../src/fb-renderer";

// ---- fakes -----------------------------------------------------------

function makeFakeImageData(): ImageDataFactory {
  return (data, width, height) => ({ width, height, data });
}

interface FakeOffscreenCanvas extends OffscreenCanvasLike {
  ctxPutCalls: Array<{ image: ImageDataLike; dx: number; dy: number }>;
  bitmapCount: number;
}

function makeFakeOffscreenFactory(): {
  factory: OffscreenCanvasFactory;
  readonly created: FakeOffscreenCanvas[];
} {
  const created: FakeOffscreenCanvas[] = [];
  const factory: OffscreenCanvasFactory = (width, height) => {
    const ctxPutCalls: Array<{ image: ImageDataLike; dx: number; dy: number }> = [];
    const ctx: OffscreenCanvasRenderingContextLike = {
      putImageData: (image, dx, dy) => {
        ctxPutCalls.push({ image, dx, dy });
      },
    };
    let bitmapCount = 0;
    const oc: FakeOffscreenCanvas = {
      width,
      height,
      ctxPutCalls,
      get bitmapCount() {
        return bitmapCount;
      },
      getContext: () => ctx,
      transferToImageBitmap: (): ImageBitmap => {
        bitmapCount += 1;
        return { _width: width, _height: height } as unknown as ImageBitmap;
      },
    };
    created.push(oc);
    return oc;
  };
  return { factory, created };
}

interface FakeCanvas extends CanvasLike {
  putCalls: Array<{ image: ImageDataLike; dx: number; dy: number }>;
  drawCalls: Array<{ bitmap: ImageBitmap; dx: number; dy: number }>;
}

function makeFakeCanvas(): FakeCanvas {
  const putCalls: Array<{ image: ImageDataLike; dx: number; dy: number }> = [];
  const drawCalls: Array<{ bitmap: ImageBitmap; dx: number; dy: number }> = [];
  const ctx: CanvasRenderingContext2DLike = {
    putImageData: (image, dx, dy) => {
      putCalls.push({ image, dx, dy });
    },
    drawImage: (bitmap, dx, dy) => {
      drawCalls.push({ bitmap, dx, dy });
    },
  };
  return {
    width: 0,
    height: 0,
    putCalls,
    drawCalls,
    getContext: () => ctx,
  };
}

// ---- fast path ------------------------------------------------------

describe("FbRenderer — OffscreenCanvas fast path", () => {
  it("setMode resizes the visible canvas + constructs an offscreen surface", () => {
    const canvas = makeFakeCanvas();
    const { factory, created } = makeFakeOffscreenFactory();
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: factory,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 320, height: 200 });
    expect(canvas.width).toBe(320);
    expect(canvas.height).toBe(200);
    expect(created).toHaveLength(1);
    expect(created[0]!.width).toBe(320);
    expect(created[0]!.height).toBe(200);
    expect(renderer.usingFastPath).toBe(true);
  });

  it("paintFrame stays on direct putImageData when offscreen is available", () => {
    const canvas = makeFakeCanvas();
    const { factory, created } = makeFakeOffscreenFactory();
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: factory,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 4, height: 4 });
    const rgba = new Uint8Array(64).fill(0x33);
    renderer.paintFrame({ width: 4, height: 4, rgba });

    expect(created[0]!.ctxPutCalls).toHaveLength(0);
    expect(created[0]!.bitmapCount).toBe(0);
    expect(canvas.drawCalls).toHaveLength(0);
    expect(canvas.putCalls).toHaveLength(1);
    expect(canvas.putCalls[0]!.dx).toBe(0);
    expect(canvas.putCalls[0]!.dy).toBe(0);
  });

  it("paintFrame fires present_complete handlers in registration order", () => {
    const canvas = makeFakeCanvas();
    const { factory } = makeFakeOffscreenFactory();
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: factory,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 1, height: 1 });
    const log: string[] = [];
    renderer.onPresentComplete(() => log.push("a"));
    renderer.onPresentComplete(() => log.push("b"));
    renderer.paintFrame({
      width: 1,
      height: 1,
      rgba: new Uint8Array(4),
    });
    expect(log).toEqual(["a", "b"]);
  });

  it("paintFrame increments presentsCompleted exactly once per call", () => {
    const canvas = makeFakeCanvas();
    const { factory } = makeFakeOffscreenFactory();
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: factory,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 1, height: 1 });
    expect(renderer.presentsCompleted).toBe(0);
    renderer.paintFrame({ width: 1, height: 1, rgba: new Uint8Array(4) });
    renderer.paintFrame({ width: 1, height: 1, rgba: new Uint8Array(4) });
    expect(renderer.presentsCompleted).toBe(2);
  });
});

// ---- fallback path --------------------------------------------------

describe("FbRenderer — fallback path", () => {
  it("setMode picks the fallback when the offscreen factory returns null", () => {
    const canvas = makeFakeCanvas();
    const factory: OffscreenCanvasFactory = () => null;
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: factory,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 100, height: 80 });
    expect(canvas.width).toBe(100);
    expect(canvas.height).toBe(80);
    expect(renderer.usingFastPath).toBe(false);
  });

  it("paintFrame uses putImageData directly on the visible canvas", () => {
    const canvas = makeFakeCanvas();
    const factory: OffscreenCanvasFactory = () => null;
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: factory,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 4, height: 4 });
    renderer.paintFrame({
      width: 4,
      height: 4,
      rgba: new Uint8Array(64).fill(0xff),
    });
    expect(canvas.putCalls).toHaveLength(1);
    expect(canvas.drawCalls).toHaveLength(0);
    expect(renderer.presentsCompleted).toBe(1);
  });
});

// ---- rectangular patches -------------------------------------------

describe("FbRenderer — rectangular patches", () => {
  it("paintPatch uploads only rect-sized ImageData at the requested offset", () => {
    const canvas = makeFakeCanvas();
    const { factory, created } = makeFakeOffscreenFactory();
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: factory,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 10, height: 8 });
    const rgba = new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]);

    renderer.paintPatch({ x: 3, y: 4, width: 2, height: 1, rgba });

    expect(canvas.putCalls).toHaveLength(1);
    expect(canvas.putCalls[0]).toMatchObject({ dx: 3, dy: 4 });
    expect(canvas.putCalls[0]!.image.width).toBe(2);
    expect(canvas.putCalls[0]!.image.height).toBe(1);
    expect(Array.from(canvas.putCalls[0]!.image.data)).toEqual(Array.from(rgba));
    expect(canvas.drawCalls).toHaveLength(0);
    expect(created[0]!.ctxPutCalls).toHaveLength(0);
    expect(created[0]!.bitmapCount).toBe(0);
  });

  it("paintPatch completes one presentation and notifies subscribers", () => {
    const canvas = makeFakeCanvas();
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: () => null,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 4, height: 4 });
    const handler = vi.fn();
    renderer.onPresentComplete(handler);

    renderer.paintPatch({
      x: 1,
      y: 1,
      width: 1,
      height: 1,
      rgba: new Uint8Array(4),
    });

    expect(renderer.presentsCompleted).toBe(1);
    expect(handler).toHaveBeenCalledTimes(1);
  });

  it("paintPatchBatch paints every rectangle and completes one presentation", () => {
    const canvas = makeFakeCanvas();
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: () => null,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 6, height: 5 });
    const handler = vi.fn();
    renderer.onPresentComplete(handler);

    renderer.paintPatchBatch([
      { x: 0, y: 1, width: 2, height: 1, rgba: new Uint8Array(8).fill(1) },
      { x: 4, y: 3, width: 1, height: 2, rgba: new Uint8Array(8).fill(2) },
    ]);

    expect(canvas.putCalls).toHaveLength(2);
    expect(canvas.putCalls.map((call) => [call.dx, call.dy])).toEqual([
      [0, 1],
      [4, 3],
    ]);
    expect(renderer.presentsCompleted).toBe(1);
    expect(handler).toHaveBeenCalledTimes(1);
  });

  it("paintPatchBatch validates the complete batch before changing pixels", () => {
    const canvas = makeFakeCanvas();
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: () => null,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 4, height: 4 });
    const handler = vi.fn();
    renderer.onPresentComplete(handler);

    renderer.paintPatchBatch([
      { x: 0, y: 0, width: 1, height: 1, rgba: new Uint8Array(4) },
      { x: 4, y: 0, width: 1, height: 1, rgba: new Uint8Array(4) },
    ]);

    expect(canvas.putCalls).toHaveLength(0);
    expect(renderer.presentsCompleted).toBe(0);
    expect(handler).not.toHaveBeenCalled();
  });

  it("paintPatch rejects pre-mode, empty, malformed, and out-of-bounds damage", () => {
    const canvas = makeFakeCanvas();
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: () => null,
      imageDataFactory: makeFakeImageData(),
    });
    const handler = vi.fn();
    renderer.onPresentComplete(handler);
    renderer.paintPatch({
      x: 0,
      y: 0,
      width: 1,
      height: 1,
      rgba: new Uint8Array(4),
    });
    renderer.setMode({ width: 4, height: 4 });

    const invalid = [
      { x: 0, y: 0, width: 0, height: 1, rgba: new Uint8Array(0) },
      { x: 0, y: 0, width: 1, height: 1, rgba: new Uint8Array(3) },
      { x: 4, y: 0, width: 1, height: 1, rgba: new Uint8Array(4) },
      { x: 0, y: 4, width: 1, height: 1, rgba: new Uint8Array(4) },
      {
        x: Number.MAX_SAFE_INTEGER,
        y: 0,
        width: 1,
        height: 1,
        rgba: new Uint8Array(4),
      },
    ];
    for (const patch of invalid) {
      renderer.paintPatch(patch);
    }

    expect(canvas.putCalls).toHaveLength(0);
    expect(renderer.presentsCompleted).toBe(0);
    expect(handler).not.toHaveBeenCalled();
  });
});

// ---- guard rails ----------------------------------------------------

describe("FbRenderer — geometry guards", () => {
  it("paintFrame before setMode is a no-op", () => {
    const canvas = makeFakeCanvas();
    const factory: OffscreenCanvasFactory = () => null;
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: factory,
      imageDataFactory: makeFakeImageData(),
    });
    const handler = vi.fn();
    renderer.onPresentComplete(handler);
    renderer.paintFrame({
      width: 4,
      height: 4,
      rgba: new Uint8Array(64),
    });
    expect(canvas.putCalls).toHaveLength(0);
    expect(handler).not.toHaveBeenCalled();
    expect(renderer.presentsCompleted).toBe(0);
  });

  it("paintFrame with mismatched geometry is dropped", () => {
    const canvas = makeFakeCanvas();
    const factory: OffscreenCanvasFactory = () => null;
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: factory,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 4, height: 4 });
    // Stale frame from a previous mode.
    renderer.paintFrame({
      width: 8,
      height: 8,
      rgba: new Uint8Array(256),
    });
    expect(canvas.putCalls).toHaveLength(0);
    expect(renderer.presentsCompleted).toBe(0);
  });

  it("setMode with the same geometry is idempotent (no offscreen rebuild)", () => {
    const canvas = makeFakeCanvas();
    const { factory, created } = makeFakeOffscreenFactory();
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: factory,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 4, height: 4 });
    renderer.setMode({ width: 4, height: 4 });
    expect(created).toHaveLength(1);
  });

  it("setMode to new geometry rebuilds the offscreen surface", () => {
    const canvas = makeFakeCanvas();
    const { factory, created } = makeFakeOffscreenFactory();
    const renderer = new FbRenderer({
      canvas,
      offscreenCanvasFactory: factory,
      imageDataFactory: makeFakeImageData(),
    });
    renderer.setMode({ width: 4, height: 4 });
    renderer.setMode({ width: 8, height: 8 });
    expect(created).toHaveLength(2);
    expect(canvas.width).toBe(8);
    expect(canvas.height).toBe(8);
  });
});
