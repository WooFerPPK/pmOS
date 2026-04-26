// T093: Bootstrap unit tests — exercise the small, side-effect-free
// helpers that bootstrap.ts exports. The "boot the kernel" path is
// covered by `bootstrap-spawn.test.ts` (spawn-router) and the
// `real-kernel.spec.ts` Playwright integration.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  hasAtomicsWait,
  hasOpfs,
  isCrossOriginIsolated,
  registerServiceWorker,
} from "../../src/bootstrap";

// ---- env probes -----------------------------------------------------

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
    expect(register).toHaveBeenCalledWith("/assets/sw.js", { type: "module" });
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
