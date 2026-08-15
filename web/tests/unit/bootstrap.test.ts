// T093: Bootstrap unit tests — exercise the small, side-effect-free
// helpers that bootstrap.ts exports. The "boot the kernel" path is
// covered by `bootstrap-spawn.test.ts` (spawn-router) and the
// `real-kernel.spec.ts` Playwright integration.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  classifyFatalConsoleText,
  confirmedHostFilePicker,
  createBrowserControlKeyRouter,
  createGuiDesktopReadyLatch,
  createGuiKeyboardInputHandler,
  createLegacyKeyboardInputHandler,
  hasAtomicsWait,
  hasAtomicsWaitAsync,
  hasOpfs,
  installHostFileDropHandler,
  isBootInteractionAllowed,
  requestHostFilePicker,
  saveHostDownload,
  isCrossOriginIsolated,
  registerServiceWorker,
  serviceWorkerScriptUrl,
  targetsBrowserSubstrateControl,
  unsupportedBrowserReasons,
  wireFramebufferPresentations,
} from "../../src/bootstrap";
import type {
  DropTarget,
  GuiKeyboardEvent,
  HostDragEvent,
  HostDragFile,
  LegacyKeyboardEvent,
} from "../../src/bootstrap";
import { KbdKeyState, unpackKbdEvent } from "../../src/shared/input-proto";
import {
  HOST_FILE_IMPORT_MAX_BYTES,
  HOST_FILE_IMPORT_MAX_FILES,
  HOST_FILE_IMPORT_MAX_TOTAL_BYTES,
  type MainToKernel,
} from "../../src/shared/worker-proto";
import type { FbFrame, FbPatch, FbPatchBatch } from "../../src/fb-host";

// ---- env probes -----------------------------------------------------

describe("classifyFatalConsoleText", () => {
  it("does not treat PID 1's supervised shell replacement as fatal", () => {
    expect(
      classifyFatalConsoleText(
        "init-desktop reaped child pid=4 status=1099511627776\n" +
          "init-desktop shell exited pid=4; scheduling respawn",
      ),
    ).toBeNull();
  });

  it("still classifies terminal display-server failure", () => {
    expect(
      classifyFatalConsoleText(
        "init-desktop reaped child pid=3 status=1099511627776",
      ),
    ).toEqual({
      title: "Display server died",
      subtitle:
        "init-desktop reaped the display-server. The desktop cannot run without it.",
    });
  });
});

describe("hasAtomicsWait", () => {
  it("returns true when Atomics.wait is a function", () => {
    expect(hasAtomicsWait()).toBe(true);
  });

  it("returns false when Atomics is missing entirely", () => {
    const original = globalThis.Atomics;
    Object.defineProperty(globalThis, "Atomics", {
      value: undefined,
      configurable: true,
    });
    try {
      expect(hasAtomicsWait()).toBe(false);
    } finally {
      Object.defineProperty(globalThis, "Atomics", {
        value: original,
        configurable: true,
      });
    }
  });

  it("returns false when Atomics.wait is replaced with a non-function", () => {
    const original = Atomics.wait;
    (Atomics as unknown as { wait: unknown }).wait = "not a function";
    try {
      expect(hasAtomicsWait()).toBe(false);
    } finally {
      (Atomics as unknown as { wait: unknown }).wait = original;
    }
  });
});

describe("hasAtomicsWaitAsync", () => {
  it("returns true when Atomics.waitAsync is a function", () => {
    expect(hasAtomicsWaitAsync()).toBe(true);
  });

  it("returns false when Atomics.waitAsync is unavailable", () => {
    const atomics = Atomics as unknown as { waitAsync: unknown };
    const original = atomics.waitAsync;
    atomics.waitAsync = undefined;
    try {
      expect(hasAtomicsWaitAsync()).toBe(false);
    } finally {
      atomics.waitAsync = original;
    }
  });
});

describe("isCrossOriginIsolated", () => {
  let originalCoi: unknown;
  beforeEach(() => {
    originalCoi = (globalThis as { crossOriginIsolated?: unknown })
      .crossOriginIsolated;
  });
  afterEach(() => {
    Object.defineProperty(globalThis, "crossOriginIsolated", {
      value: originalCoi,
      configurable: true,
    });
  });

  it("returns true when crossOriginIsolated is true", () => {
    Object.defineProperty(globalThis, "crossOriginIsolated", {
      value: true,
      configurable: true,
    });
    expect(isCrossOriginIsolated()).toBe(true);
  });

  it("returns false when crossOriginIsolated is false", () => {
    Object.defineProperty(globalThis, "crossOriginIsolated", {
      value: false,
      configurable: true,
    });
    expect(isCrossOriginIsolated()).toBe(false);
  });

  it("returns false when crossOriginIsolated is undefined", () => {
    Object.defineProperty(globalThis, "crossOriginIsolated", {
      value: undefined,
      configurable: true,
    });
    expect(isCrossOriginIsolated()).toBe(false);
  });
});

describe("hasOpfs", () => {
  it("returns true when navigator.storage.getDirectory is a function", () => {
    const fakeNav = {
      storage: { getDirectory: () => Promise.resolve({}) },
    };
    Object.defineProperty(globalThis, "navigator", {
      value: fakeNav,
      configurable: true,
    });
    try {
      expect(hasOpfs()).toBe(true);
    } finally {
      Object.defineProperty(globalThis, "navigator", {
        value: undefined,
        configurable: true,
      });
    }
  });

  it("returns false when navigator.storage is absent", () => {
    Object.defineProperty(globalThis, "navigator", {
      value: { storage: undefined },
      configurable: true,
    });
    try {
      expect(hasOpfs()).toBe(false);
    } finally {
      Object.defineProperty(globalThis, "navigator", {
        value: undefined,
        configurable: true,
      });
    }
  });

  it("returns false when navigator is undefined", () => {
    Object.defineProperty(globalThis, "navigator", {
      value: undefined,
      configurable: true,
    });
    expect(hasOpfs()).toBe(false);
  });
});

describe("unsupportedBrowserReasons", () => {
  const originalNavigator = Object.getOwnPropertyDescriptor(
    globalThis,
    "navigator",
  );
  const originalWorker = Object.getOwnPropertyDescriptor(globalThis, "Worker");
  const originalIsolation = Object.getOwnPropertyDescriptor(
    globalThis,
    "crossOriginIsolated",
  );

  afterEach(() => {
    if (originalNavigator === undefined)
      delete (globalThis as { navigator?: unknown }).navigator;
    else Object.defineProperty(globalThis, "navigator", originalNavigator);
    if (originalWorker === undefined)
      delete (globalThis as { Worker?: unknown }).Worker;
    else Object.defineProperty(globalThis, "Worker", originalWorker);
    if (originalIsolation === undefined) {
      delete (globalThis as { crossOriginIsolated?: unknown })
        .crossOriginIsolated;
    } else {
      Object.defineProperty(
        globalThis,
        "crossOriginIsolated",
        originalIsolation,
      );
    }
  });

  function installSupportedSubstrate(): void {
    Object.defineProperty(globalThis, "navigator", {
      value: {
        storage: { getDirectory: () => Promise.resolve({}) },
        serviceWorker: {},
      },
      configurable: true,
    });
    Object.defineProperty(globalThis, "Worker", {
      value: class TestWorker {},
      configurable: true,
    });
    Object.defineProperty(globalThis, "crossOriginIsolated", {
      value: true,
      configurable: true,
    });
  }

  it("accepts a complete persistent browser substrate", () => {
    installSupportedSubstrate();
    expect(unsupportedBrowserReasons()).toEqual([]);
  });

  it("classifies a missing OPFS entry point as unsupported", () => {
    installSupportedSubstrate();
    Object.defineProperty(globalThis, "navigator", {
      value: { storage: undefined, serviceWorker: {} },
      configurable: true,
    });
    expect(unsupportedBrowserReasons()).toContain(
      "Origin Private File System (OPFS)",
    );
  });

  it("classifies missing Atomics.waitAsync as unsupported before boot", () => {
    installSupportedSubstrate();
    const atomics = Atomics as unknown as { waitAsync: unknown };
    const original = atomics.waitAsync;
    atomics.waitAsync = undefined;
    try {
      expect(unsupportedBrowserReasons()).toContain("Atomics.waitAsync");
    } finally {
      atomics.waitAsync = original;
    }
  });
});

// ---- service worker registration ------------------------------------

describe("registerServiceWorker", () => {
  it("calls container.register with the script URL + options", async () => {
    const register = vi.fn(() => Promise.resolve({ scope: "/" }));
    const container = { register };
    const reg = await registerServiceWorker(
      "/assets/sw.js",
      { type: "module" },
      container,
    );
    expect(register).toHaveBeenCalledWith("/assets/sw.js", { type: "module" });
    expect(reg).toEqual({ scope: "/" });
  });

  it("uses sensible defaults when caller omits scriptURL + options", async () => {
    const register = vi.fn(() => Promise.resolve({ scope: "/" }));
    const container = { register };
    await registerServiceWorker(undefined, undefined, container);
    expect(register).toHaveBeenCalledWith("/sw.js", { type: "module" });
  });

  it("resolves the emitted sw.js relative to a subpath deployment", () => {
    expect(serviceWorkerScriptUrl("https://example.test/pmos/index.html")).toBe(
      "/pmos/sw.js",
    );
  });

  it("returns null when no container is provided and navigator.serviceWorker is absent", async () => {
    Object.defineProperty(globalThis, "navigator", {
      value: { serviceWorker: undefined },
      configurable: true,
    });
    try {
      const reg = await registerServiceWorker();
      expect(reg).toBeNull();
    } finally {
      Object.defineProperty(globalThis, "navigator", {
        value: undefined,
        configurable: true,
      });
    }
  });

  it("propagates register() rejections back to the caller", async () => {
    const register = vi.fn(() =>
      Promise.reject(new Error("registration failed")),
    );
    const container = { register };
    await expect(
      registerServiceWorker("/sw.js", undefined, container),
    ).rejects.toThrow("registration failed");
  });

  it("forwards the scope option to register()", async () => {
    const register = vi.fn(() => Promise.resolve({ scope: "/scope" }));
    const container = { register };
    await registerServiceWorker(
      "/assets/sw.js",
      { scope: "/scope", type: "module" },
      container,
    );
    expect(register).toHaveBeenCalledWith("/assets/sw.js", {
      scope: "/scope",
      type: "module",
    });
  });
});

// ---- graphical desktop readiness -----------------------------------

describe("createGuiDesktopReadyLatch", () => {
  it("publishes once on the first valid typed presentation fence", () => {
    const onReady = vi.fn();
    const latch = createGuiDesktopReadyLatch(onReady);

    latch.notePresentFence(0);
    latch.notePresentFence(-1);
    latch.notePresentFence(1.5);
    latch.notePresentFence(0x1_0000_0000);
    expect(latch.ready).toBe(false);
    expect(onReady).not.toHaveBeenCalled();

    latch.notePresentFence(7);
    expect(latch.ready).toBe(true);
    expect(onReady).toHaveBeenCalledTimes(1);

    latch.notePresentFence(8);
    expect(onReady).toHaveBeenCalledTimes(1);
  });

  it("has no console parsing surface for either shell diagnostic", () => {
    const latch = createGuiDesktopReadyLatch(vi.fn());

    expect(latch).not.toHaveProperty("observeConsoleOutput");
    expect(latch).not.toHaveProperty("noteFramebufferPresentation");
    expect(latch.ready).toBe(false);
  });
});

describe("isBootInteractionAllowed", () => {
  it("keeps GUI input gated until desktop readiness and storage recovery permit it", () => {
    expect(
      isBootInteractionAllowed({
        kernelReady: true,
        storageDegraded: false,
        temporaryStorageAccepted: false,
        guiDesktopReady: false,
      }),
    ).toBe(false);
    expect(
      isBootInteractionAllowed({
        kernelReady: true,
        storageDegraded: false,
        temporaryStorageAccepted: false,
        guiDesktopReady: true,
      }),
    ).toBe(true);
    expect(
      isBootInteractionAllowed({
        kernelReady: true,
        storageDegraded: true,
        temporaryStorageAccepted: false,
        guiDesktopReady: true,
      }),
    ).toBe(false);
    expect(
      isBootInteractionAllowed({
        kernelReady: true,
        storageDegraded: true,
        temporaryStorageAccepted: true,
        guiDesktopReady: true,
      }),
    ).toBe(true);
  });

  it("preserves the kernel and storage gate for non-GUI boots", () => {
    expect(
      isBootInteractionAllowed({
        kernelReady: false,
        storageDegraded: false,
        temporaryStorageAccepted: false,
        guiDesktopReady: null,
      }),
    ).toBe(false);
    expect(
      isBootInteractionAllowed({
        kernelReady: true,
        storageDegraded: false,
        temporaryStorageAccepted: false,
        guiDesktopReady: null,
      }),
    ).toBe(true);
  });
});

// ---- framebuffer presentation completion ----------------------------

describe("wireFramebufferPresentations", () => {
  it("does not release the typed desktop-ready fence after an ordinary paint", () => {
    let onFrame: ((frame: FbFrame) => void) | undefined;
    let onPresent: (() => void) | undefined;
    const dismiss = vi.fn();
    const firstPresent = vi.fn();
    const readiness = createGuiDesktopReadyLatch(dismiss);
    const interactionAllowed = (): boolean =>
      isBootInteractionAllowed({
        kernelReady: true,
        storageDegraded: false,
        temporaryStorageAccepted: false,
        guiDesktopReady: readiness.ready,
      });

    wireFramebufferPresentations({
      host: {
        onFrame: (handler) => {
          onFrame = handler;
        },
        onPatch: () => {},
        onPatchBatch: () => {},
      },
      renderer: {
        onPresentComplete: (handler) => {
          onPresent = handler;
        },
        paintFrame: () => {},
        paintPatch: () => {},
        paintPatchBatch: () => {},
      },
      target: {
        dataset: {},
        dispatchEvent: () => true,
      },
      now: () => 1,
      makeFrameEvent: () => ({ type: "pmos:frame" }) as Event,
      onFirstPresent: firstPresent,
    });

    onFrame?.({ width: 1, height: 1, rgba: new Uint8Array(4) });
    onPresent?.();
    expect(firstPresent).toHaveBeenCalledTimes(1);
    expect(dismiss).not.toHaveBeenCalled();
    expect(interactionAllowed()).toBe(false);

    readiness.notePresentFence(11);
    expect(readiness.ready).toBe(true);
    expect(dismiss).toHaveBeenCalledTimes(1);
    expect(firstPresent).toHaveBeenCalledTimes(1);
    expect(interactionAllowed()).toBe(true);
  });

  it("routes blits, patches, and atomic batches through one completion sequence", () => {
    let onFrame: ((frame: FbFrame) => void) | undefined;
    let onPatch: ((patch: FbPatch) => void) | undefined;
    let onPatchBatch: ((batch: FbPatchBatch) => void) | undefined;
    let onPresent: (() => void) | undefined;
    const paintedFrames: FbFrame[] = [];
    const paintedPatches: FbPatch[] = [];
    const paintedBatches: FbPatch[][] = [];
    const events: Array<{
      type: string;
      detail: { sequence: number; receivedAt: number; paintedAt: number };
    }> = [];
    const firstPresent = vi.fn();
    const dataset: { pmosFrameSequence?: string } = {};

    wireFramebufferPresentations({
      host: {
        onFrame: (handler) => {
          onFrame = handler;
        },
        onPatch: (handler) => {
          onPatch = handler;
        },
        onPatchBatch: (handler) => {
          onPatchBatch = handler;
        },
      },
      renderer: {
        onPresentComplete: (handler) => {
          onPresent = handler;
        },
        paintFrame: (frame) => {
          paintedFrames.push(frame);
          onPresent?.();
        },
        paintPatch: (patch) => {
          paintedPatches.push(patch);
          onPresent?.();
        },
        paintPatchBatch: (patches) => {
          paintedBatches.push([...patches]);
          onPresent?.();
        },
      },
      target: {
        dataset,
        dispatchEvent: (event) => {
          events.push(event as unknown as (typeof events)[number]);
          return true;
        },
      },
      now: vi
        .fn()
        .mockReturnValueOnce(100)
        .mockReturnValueOnce(102.5)
        .mockReturnValueOnce(200)
        .mockReturnValueOnce(203.5)
        .mockReturnValueOnce(300)
        .mockReturnValueOnce(304.5),
      makeFrameEvent: (detail) =>
        ({ type: "pmos:frame", detail }) as unknown as Event,
      onFirstPresent: firstPresent,
    });

    const frame: FbFrame = {
      width: 2,
      height: 1,
      rgba: new Uint8Array(8),
    };
    const patch: FbPatch = {
      x: 1,
      y: 0,
      width: 1,
      height: 1,
      rgba: new Uint8Array(4),
    };
    onFrame?.(frame);
    onPatch?.(patch);
    onPatchBatch?.({ patches: [patch, { ...patch, x: 0 }] });

    expect(paintedFrames).toEqual([frame]);
    expect(paintedPatches).toEqual([patch]);
    expect(paintedBatches).toEqual([[patch, { ...patch, x: 0 }]]);
    expect(dataset.pmosFrameSequence).toBe("3");
    expect(events).toEqual([
      {
        type: "pmos:frame",
        detail: { sequence: 1, receivedAt: 100, paintedAt: 102.5 },
      },
      {
        type: "pmos:frame",
        detail: { sequence: 2, receivedAt: 200, paintedAt: 203.5 },
      },
      {
        type: "pmos:frame",
        detail: { sequence: 3, receivedAt: 300, paintedAt: 304.5 },
      },
    ]);
    expect(firstPresent).toHaveBeenCalledTimes(1);
  });
});

// ---- graphical desktop keyboard input -------------------------------

describe("createGuiKeyboardInputHandler", () => {
  function keyEvent(
    code: string,
    metaKey = false,
    target?: EventTarget,
  ): GuiKeyboardEvent & { readonly prevented: () => boolean } {
    let wasPrevented = false;
    return {
      code,
      metaKey,
      ...(target === undefined ? {} : { target }),
      preventDefault: () => {
        wasPrevented = true;
      },
      prevented: () => wasPrevented,
    };
  }

  it("forwards Ctrl shortcuts to the focused PMos client", () => {
    const messages: MainToKernel[] = [];
    const handler = createGuiKeyboardInputHandler(
      { postMessage: (message) => messages.push(message) },
      KbdKeyState.Pressed,
    );
    const control = keyEvent("ControlLeft");
    const save = keyEvent("KeyS");

    handler(control);
    handler(save);

    expect(control.prevented()).toBe(true);
    expect(save.prevented()).toBe(true);
    expect(
      messages.map((message) => {
        if (message.kind !== "input:kbd") throw new Error("unexpected message");
        return unpackKbdEvent(message.bytes);
      }),
    ).toEqual([
      { key: 0xe0, state: KbdKeyState.Pressed },
      { key: 0x16, state: KbdKeyState.Pressed },
    ]);
  });

  it("forwards Alt and release transitions but reserves Meta chords", () => {
    const messages: MainToKernel[] = [];
    const handler = createGuiKeyboardInputHandler(
      { postMessage: (message) => messages.push(message) },
      KbdKeyState.Released,
    );
    const alt = keyEvent("AltRight");
    const browserShortcut = keyEvent("KeyR", true);

    handler(alt);
    handler(browserShortcut);

    expect(alt.prevented()).toBe(true);
    expect(browserShortcut.prevented()).toBe(false);
    expect(messages).toHaveLength(1);
    const message = messages[0];
    if (message?.kind !== "input:kbd")
      throw new Error("missing keyboard event");
    expect(unpackKbdEvent(message.bytes)).toEqual({
      key: 0xe6,
      state: KbdKeyState.Released,
    });
  });

  it("owns a confirmation key through body-retargeted keyup", () => {
    const messages: MainToKernel[] = [];
    const target = {
      postMessage: (message: MainToKernel) => messages.push(message),
    };
    const press = createGuiKeyboardInputHandler(target, KbdKeyState.Pressed);
    const release = createGuiKeyboardInputHandler(target, KbdKeyState.Released);
    const button = {
      closest(selector: string): unknown {
        return selector === "#pmos-host-file-picker-confirm" ? this : null;
      },
    } as unknown as EventTarget;
    const body = {
      closest(): null {
        return null;
      },
    } as unknown as EventTarget;
    const down = keyEvent("Enter", false, button);
    const up = keyEvent("Enter", false, body);
    const router = createBrowserControlKeyRouter();

    if (router.keydown(down)) press(down);
    if (router.keyup(up)) release(up);

    expect(down.prevented()).toBe(false);
    expect(up.prevented()).toBe(false);
    expect(messages).toHaveLength(0);
  });

  it("keeps guest shortcut and modifier releases routed after the prompt focuses", () => {
    const messages: MainToKernel[] = [];
    const target = {
      postMessage: (message: MainToKernel) => messages.push(message),
    };
    const press = createGuiKeyboardInputHandler(target, KbdKeyState.Pressed);
    const release = createGuiKeyboardInputHandler(target, KbdKeyState.Released);
    const body = {
      closest(): null {
        return null;
      },
    } as unknown as EventTarget;
    const button = {
      closest(selector: string): unknown {
        return selector === "#pmos-host-file-picker-confirm" ? this : null;
      },
    } as unknown as EventTarget;
    const shiftDown = keyEvent("ShiftLeft", false, body);
    const shortcutDown = keyEvent("KeyI", false, body);
    const shortcutUp = keyEvent("KeyI", false, button);
    const shiftUp = keyEvent("ShiftLeft", false, button);
    const router = createBrowserControlKeyRouter();

    if (router.keydown(shiftDown)) press(shiftDown);
    if (router.keydown(shortcutDown)) press(shortcutDown);
    if (router.keyup(shortcutUp)) release(shortcutUp);
    if (router.keyup(shiftUp)) release(shiftUp);

    expect(
      messages.map((message) => {
        if (message.kind !== "input:kbd") throw new Error("unexpected message");
        return unpackKbdEvent(message.bytes);
      }),
    ).toEqual([
      { key: 0xe1, state: KbdKeyState.Pressed },
      { key: 0x0c, state: KbdKeyState.Pressed },
      { key: 0x0c, state: KbdKeyState.Released },
      { key: 0xe1, state: KbdKeyState.Released },
    ]);
  });

  it("recognises a detached one-shot confirmation for its keyup", () => {
    const detachedButton = {
      closest(selector: string): unknown {
        return selector === "#pmos-host-file-picker-confirm" ? this : null;
      },
    } as unknown as EventTarget;

    expect(targetsBrowserSubstrateControl({ target: detachedButton })).toBe(
      true,
    );
    expect(targetsBrowserSubstrateControl({ target: null })).toBe(false);
  });

  it("forwards navigation-key press and release transitions as USB HID codes", () => {
    const messages: MainToKernel[] = [];
    const target = {
      postMessage: (message: MainToKernel) => messages.push(message),
    };
    const press = createGuiKeyboardInputHandler(target, KbdKeyState.Pressed);
    const release = createGuiKeyboardInputHandler(target, KbdKeyState.Released);
    const navigation = [
      ["Escape", 0x29],
      ["Insert", 0x49],
      ["Home", 0x4a],
      ["PageUp", 0x4b],
      ["Delete", 0x4c],
      ["End", 0x4d],
      ["PageDown", 0x4e],
      ["ArrowRight", 0x4f],
      ["ArrowLeft", 0x50],
      ["ArrowDown", 0x51],
      ["ArrowUp", 0x52],
    ] as const;

    for (const [code] of navigation) {
      const event = keyEvent(code);
      press(event);
      release(event);
      expect(event.prevented()).toBe(true);
    }

    expect(
      messages.map((message) => {
        if (message.kind !== "input:kbd") throw new Error("unexpected message");
        return unpackKbdEvent(message.bytes);
      }),
    ).toEqual(
      navigation.flatMap(([, key]) => [
        { key, state: KbdKeyState.Pressed },
        { key, state: KbdKeyState.Released },
      ]),
    );
  });
});

// ---- text-only real-kernel keyboard input ---------------------------

describe("createLegacyKeyboardInputHandler", () => {
  function keyEvent(
    key: string,
    modifiers: Partial<
      Pick<LegacyKeyboardEvent, "ctrlKey" | "metaKey" | "altKey">
    > = {},
  ): LegacyKeyboardEvent & { readonly prevented: () => boolean } {
    let wasPrevented = false;
    return {
      key,
      ctrlKey: modifiers.ctrlKey ?? false,
      metaKey: modifiers.metaKey ?? false,
      altKey: modifiers.altKey ?? false,
      preventDefault: () => {
        wasPrevented = true;
      },
      prevented: () => wasPrevented,
    };
  }

  it("delivers a complete text line as one atomic keyboard message", () => {
    const messages: MainToKernel[] = [];
    const handler = createLegacyKeyboardInputHandler({
      postMessage: (msg) => messages.push(msg),
    });
    const x = keyEvent("x");
    const enter = keyEvent("Enter");

    handler(x);
    expect(messages).toEqual([]);
    handler(enter);

    expect(x.prevented()).toBe(true);
    expect(enter.prevented()).toBe(true);
    expect(messages).toHaveLength(1);
    const message = messages[0];
    expect(message?.kind).toBe("input:kbd");
    if (message?.kind === "input:kbd") {
      expect(Array.from(message.bytes)).toEqual([0x78, 0x0a]);
    }
  });

  it("applies canonical backspace editing and leaves browser chords alone", () => {
    const messages: MainToKernel[] = [];
    const handler = createLegacyKeyboardInputHandler({
      postMessage: (msg) => messages.push(msg),
    });
    handler(keyEvent("a"));
    handler(keyEvent("b"));
    handler(keyEvent("Backspace"));
    const browserChord = keyEvent("r", { ctrlKey: true });
    handler(browserChord);
    handler(keyEvent("c"));
    handler(keyEvent("Enter"));

    expect(browserChord.prevented()).toBe(false);
    const message = messages[0];
    if (message?.kind !== "input:kbd") throw new Error("missing keyboard line");
    expect(new TextDecoder().decode(message.bytes)).toBe("ac\n");
  });
});

// ---- T154: host-file drop handler ----------------------------------

describe("installHostFileDropHandler", () => {
  function makeFile(
    name: string,
    type: string,
    body: Uint8Array,
  ): HostDragFile {
    return {
      name,
      type,
      size: body.byteLength,
      arrayBuffer: () => {
        const copy = new ArrayBuffer(body.byteLength);
        new Uint8Array(copy).set(body);
        return Promise.resolve(copy);
      },
    };
  }

  function makeTarget() {
    type Listener = (e: HostDragEvent) => void;
    const listeners = new Map<string, Listener>();
    const target: DropTarget = {
      addEventListener(type, listener) {
        listeners.set(type, listener);
      },
    };
    return {
      target,
      fireDragover(e: HostDragEvent) {
        listeners.get("dragover")?.(e);
      },
      fireDrop(e: HostDragEvent) {
        listeners.get("drop")?.(e);
      },
    };
  }

  it("posts host:dropped messages with monotonically growing tokens", async () => {
    const messages: MainToKernel[] = [];
    const worker = { postMessage: (m: MainToKernel) => messages.push(m) };
    const { target, fireDrop } = makeTarget();
    installHostFileDropHandler(worker, target);

    const a = makeFile("a.txt", "text/plain", new Uint8Array([1, 2, 3]));
    const b = makeFile("b.txt", "text/plain", new Uint8Array([4, 5]));
    const ev: HostDragEvent = {
      preventDefault: () => {},
      dataTransfer: { files: [a, b] },
    };
    fireDrop(ev);

    // arrayBuffer() is async; flush microtasks.
    await new Promise((r) => setTimeout(r, 0));

    expect(messages).toHaveLength(2);
    const first = messages[0]!;
    const second = messages[1]!;
    expect(first.kind).toBe("host:dropped");
    if (first.kind === "host:dropped" && second.kind === "host:dropped") {
      expect(first.name).toBe("a.txt");
      expect(first.mime).toBe("text/plain");
      expect(Array.from(first.bytes)).toEqual([1, 2, 3]);
      expect(second.name).toBe("b.txt");
      expect(Array.from(second.bytes)).toEqual([4, 5]);
      expect(second.token).toBeGreaterThan(first.token);
    }
  });

  it("ignores drop events without a dataTransfer", () => {
    const messages: MainToKernel[] = [];
    const worker = { postMessage: (m: MainToKernel) => messages.push(m) };
    const { target, fireDrop } = makeTarget();
    installHostFileDropHandler(worker, target);
    fireDrop({ preventDefault: () => {}, dataTransfer: null });
    expect(messages).toHaveLength(0);
  });

  it("calls preventDefault on dragover and drop so the browser does not navigate", () => {
    const worker = { postMessage: () => {} };
    const { target, fireDragover, fireDrop } = makeTarget();
    installHostFileDropHandler(worker, target);
    let dragoverPrevented = false;
    let dropPrevented = false;
    fireDragover({
      preventDefault: () => {
        dragoverPrevented = true;
      },
      dataTransfer: null,
    });
    fireDrop({
      preventDefault: () => {
        dropPrevented = true;
      },
      dataTransfer: null,
    });
    expect(dragoverPrevented).toBe(true);
    expect(dropPrevented).toBe(true);
  });

  it("rejects oversized drops with a console warning rather than crashing", async () => {
    const messages: MainToKernel[] = [];
    const worker = { postMessage: (m: MainToKernel) => messages.push(m) };
    const { target, fireDrop } = makeTarget();
    installHostFileDropHandler(worker, target);

    let reads = 0;
    const file: HostDragFile = {
      name: "big.bin",
      type: "application/octet-stream",
      size: HOST_FILE_IMPORT_MAX_BYTES + 1,
      arrayBuffer: async () => {
        reads += 1;
        return new ArrayBuffer(0);
      },
    };
    const warnings: string[] = [];
    const original = console.warn;
    console.warn = (msg: string) => warnings.push(msg);
    try {
      fireDrop({
        preventDefault: () => {},
        dataTransfer: { files: [file] },
      });
      await new Promise((r) => setTimeout(r, 0));
    } finally {
      console.warn = original;
    }
    expect(messages).toHaveLength(0);
    expect(reads).toBe(0);
    expect(warnings.some((w) => w.includes("exceeds v1 cap"))).toBe(true);
  });

  it("rejects an over-count batch before reading any browser File", async () => {
    const messages: MainToKernel[] = [];
    const worker = { postMessage: (m: MainToKernel) => messages.push(m) };
    const { target, fireDrop } = makeTarget();
    installHostFileDropHandler(worker, target);
    let reads = 0;
    const files = Array.from(
      { length: HOST_FILE_IMPORT_MAX_FILES + 1 },
      (_, i): HostDragFile => ({
        name: `${i}.txt`,
        type: "text/plain",
        size: 1,
        arrayBuffer: async () => {
          reads += 1;
          return new Uint8Array([i & 0xff]).buffer;
        },
      }),
    );

    fireDrop({ preventDefault: () => {}, dataTransfer: { files } });
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(reads).toBe(0);
    expect(messages).toHaveLength(0);
  });

  it("rejects an over-byte batch before reading any browser File", async () => {
    const messages: MainToKernel[] = [];
    const worker = { postMessage: (m: MainToKernel) => messages.push(m) };
    const { target, fireDrop } = makeTarget();
    installHostFileDropHandler(worker, target);
    let reads = 0;
    const file = (name: string, size: number): HostDragFile => ({
      name,
      size,
      arrayBuffer: async () => {
        reads += 1;
        return new ArrayBuffer(size);
      },
    });

    fireDrop({
      preventDefault: () => {},
      dataTransfer: {
        files: [
          file("a.bin", HOST_FILE_IMPORT_MAX_BYTES),
          file("b.bin", HOST_FILE_IMPORT_MAX_BYTES),
          file("c.bin", 1),
        ],
      },
    });
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(HOST_FILE_IMPORT_MAX_TOTAL_BYTES).toBe(
      2 * HOST_FILE_IMPORT_MAX_BYTES,
    );
    expect(reads).toBe(0);
    expect(messages).toHaveLength(0);
  });

  it("reads an admitted batch sequentially", async () => {
    const messages: MainToKernel[] = [];
    const worker = { postMessage: (m: MainToKernel) => messages.push(m) };
    const { target, fireDrop } = makeTarget();
    installHostFileDropHandler(worker, target);
    let activeReads = 0;
    let peakReads = 0;
    const file = (name: string): HostDragFile => ({
      name,
      size: 1,
      arrayBuffer: async () => {
        activeReads += 1;
        peakReads = Math.max(peakReads, activeReads);
        await new Promise((resolve) => setTimeout(resolve, 1));
        activeReads -= 1;
        return new Uint8Array([name.charCodeAt(0)]).buffer;
      },
    });

    fireDrop({
      preventDefault: () => {},
      dataTransfer: { files: [file("a"), file("b"), file("c")] },
    });
    await new Promise((resolve) => setTimeout(resolve, 15));

    expect(peakReads).toBe(1);
    expect(messages).toHaveLength(3);
  });

  it("serializes reads across overlapping drop batches", async () => {
    const messages: MainToKernel[] = [];
    const worker = { postMessage: (m: MainToKernel) => messages.push(m) };
    const { target, fireDrop } = makeTarget();
    installHostFileDropHandler(worker, target);
    let releaseFirst: (() => void) | undefined;
    let activeReads = 0;
    let peakReads = 0;
    const first: HostDragFile = {
      name: "first",
      size: 1,
      arrayBuffer: async () => {
        activeReads += 1;
        peakReads = Math.max(peakReads, activeReads);
        await new Promise<void>((resolve) => {
          releaseFirst = resolve;
        });
        activeReads -= 1;
        return new Uint8Array([1]).buffer;
      },
    };
    const second: HostDragFile = {
      name: "second",
      size: 1,
      arrayBuffer: async () => {
        activeReads += 1;
        peakReads = Math.max(peakReads, activeReads);
        activeReads -= 1;
        return new Uint8Array([2]).buffer;
      },
    };

    fireDrop({ preventDefault: () => {}, dataTransfer: { files: [first] } });
    fireDrop({ preventDefault: () => {}, dataTransfer: { files: [second] } });
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(releaseFirst).toBeTypeOf("function");
    expect(activeReads).toBe(1);
    releaseFirst?.();
    await new Promise((resolve) => setTimeout(resolve, 5));

    expect(peakReads).toBe(1);
    expect(messages).toHaveLength(2);
  });

  it("blocks drop delivery while the storage-recovery gate is active", async () => {
    const messages: MainToKernel[] = [];
    const worker = { postMessage: (m: MainToKernel) => messages.push(m) };
    const { target, fireDrop } = makeTarget();
    installHostFileDropHandler(worker, target, () => false);
    let reads = 0;
    let prevented = false;
    const file: HostDragFile = {
      name: "hidden.txt",
      size: 1,
      arrayBuffer: async () => {
        reads += 1;
        return new Uint8Array([1]).buffer;
      },
    };

    fireDrop({
      preventDefault: () => {
        prevented = true;
      },
      dataTransfer: { files: [file] },
    });
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(prevented).toBe(true);
    expect(reads).toBe(0);
    expect(messages).toHaveLength(0);
  });
});

describe("host file picker and download bridge", () => {
  it("opens the native picker only from a one-shot confirmation", () => {
    let confirm: (() => void) | undefined;
    let nativePicks = 0;
    const picker = confirmedHostFilePicker(
      {
        pick() {
          nativePicks += 1;
        },
      },
      {
        request(onConfirm) {
          confirm = onConfirm;
        },
      },
    );

    picker.pick(() => {});
    expect(nativePicks).toBe(0);
    confirm?.();
    expect(nativePicks).toBe(1);
    confirm?.();
    expect(nativePicks).toBe(1);
  });

  it("rechecks the recovery gate before confirmation opens the native picker", () => {
    let confirm: (() => void) | undefined;
    let nativePicks = 0;
    let allowed = true;
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const picker = confirmedHostFilePicker(
      {
        pick() {
          nativePicks += 1;
        },
      },
      {
        request(onConfirm) {
          confirm = onConfirm;
        },
      },
    );

    picker.pick(
      () => {},
      () => allowed,
    );
    allowed = false;
    confirm?.();

    expect(nativePicks).toBe(0);
    expect(warn).toHaveBeenCalledOnce();
    warn.mockRestore();
  });

  it("routes picker selections through the tokenised host:dropped path", async () => {
    const messages: MainToKernel[] = [];
    requestHostFilePicker(
      { postMessage: (message) => messages.push(message) },
      {
        pick(onFiles) {
          onFiles([
            {
              name: "picked.txt",
              type: "text/plain",
              size: 3,
              arrayBuffer: async () => new Uint8Array([7, 8, 9]).buffer,
            },
          ]);
        },
      },
    );

    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(messages).toHaveLength(1);
    const message = messages[0];
    expect(message?.kind).toBe("host:dropped");
    if (message?.kind === "host:dropped") {
      expect(message.name).toBe("picked.txt");
      expect(message.mime).toBe("text/plain");
      expect(Array.from(message.bytes)).toEqual([7, 8, 9]);
    }
  });

  it("does not open the picker while storage recovery blocks interaction", () => {
    let picks = 0;
    requestHostFilePicker(
      { postMessage: () => {} },
      {
        pick() {
          picks += 1;
        },
      },
      () => false,
    );
    expect(picks).toBe(0);
  });

  it("rechecks the storage gate before reading a delayed picker selection", async () => {
    let callback: ((files: ArrayLike<HostDragFile>) => void) | undefined;
    let allowed = true;
    let reads = 0;
    const messages: MainToKernel[] = [];
    requestHostFilePicker(
      { postMessage: (message) => messages.push(message) },
      {
        pick(onFiles) {
          callback = onFiles;
        },
      },
      () => allowed,
    );
    allowed = false;
    callback?.([
      {
        name: "delayed.txt",
        size: 1,
        arrayBuffer: async () => {
          reads += 1;
          return new Uint8Array([1]).buffer;
        },
      },
    ]);
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(reads).toBe(0);
    expect(messages).toHaveLength(0);
  });

  it("sanitises download names, defaults MIME, and owns the byte buffer", () => {
    const source = new Uint8Array([1, 2, 3]);
    const saved: Array<{ name: string; mime: string; bytes: Uint8Array }> = [];
    saveHostDownload("../notes.txt", "", source, {
      save: (name, mime, bytes) => saved.push({ name, mime, bytes }),
    });
    source.fill(9);

    expect(saved).toHaveLength(1);
    expect(saved[0]?.name).toBe("notes.txt");
    expect(saved[0]?.mime).toBe("application/octet-stream");
    expect(Array.from(saved[0]?.bytes ?? [])).toEqual([1, 2, 3]);
  });
});
