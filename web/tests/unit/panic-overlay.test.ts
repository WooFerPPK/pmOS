// Unit tests for the kernel-panic overlay behaviour exported from
// `bootstrap.ts` (T094).
//
// These tests run in the default Vitest Node environment (no jsdom /
// happy-dom). A minimal DOM surface is wired up via `global.document`
// and `global.window` before each test and torn down after, so the
// tests remain hermetic and require no extra devDependency.
//
// Panic overlay DOM contract (as implemented in bootstrap.ts):
//   #pmos-panic          — outer panel; hidden until panic fires
//   #pmos-panic-message  — text content set to the panic message
//   #pmos-panic-countdown — text content counts down 5 → 4 → … → 0
//   window.location.reload() is called after 5 × 1000 ms

import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import { showPanic } from "../../src/bootstrap";

// ---------------------------------------------------------------------------
// Minimal in-process DOM stub
// ---------------------------------------------------------------------------

interface FakeElement {
  id: string;
  textContent: string;
  style: { display: string };
}

function makeElement(id: string): FakeElement {
  return { id, textContent: "", style: { display: "" } };
}

interface FakeDom {
  panel: FakeElement;
  message: FakeElement;
  countdown: FakeElement;
  getElementById(id: string): FakeElement | null;
}

function makeDom(): FakeDom {
  const panel = makeElement("pmos-panic");
  const message = makeElement("pmos-panic-message");
  const countdown = makeElement("pmos-panic-countdown");
  return {
    panel,
    message,
    countdown,
    getElementById(id: string): FakeElement | null {
      if (id === "pmos-panic") return panel;
      if (id === "pmos-panic-message") return message;
      if (id === "pmos-panic-countdown") return countdown;
      return null;
    },
  };
}

// ---------------------------------------------------------------------------
// Test suite
// ---------------------------------------------------------------------------

describe("showPanic (kernel-panic overlay)", () => {
  let dom: FakeDom;
  let reloadSpy: ReturnType<typeof vi.fn>;
  let originalDocument: typeof globalThis.document;
  let originalWindow: typeof globalThis.window;

  beforeEach(() => {
    vi.useFakeTimers();

    dom = makeDom();

    // Stash originals so we can restore them after each test.
    originalDocument = globalThis.document;
    originalWindow = globalThis.window;

    // Install fake document into the global scope that bootstrap.ts reads.
    Object.defineProperty(globalThis, "document", {
      value: dom,
      writable: true,
      configurable: true,
    });

    // Install a fake window with a spied reload.
    reloadSpy = vi.fn();
    Object.defineProperty(globalThis, "window", {
      value: { location: { reload: reloadSpy } },
      writable: true,
      configurable: true,
    });
  });

  afterEach(() => {
    vi.useRealTimers();
    Object.defineProperty(globalThis, "document", {
      value: originalDocument,
      writable: true,
      configurable: true,
    });
    Object.defineProperty(globalThis, "window", {
      value: originalWindow,
      writable: true,
      configurable: true,
    });
  });

  it("makes the panel visible", () => {
    showPanic("oops");
    expect(dom.panel.style.display).toBe("block");
  });

  it("sets the message text content", () => {
    showPanic("kernel trap: illegal instruction");
    expect(dom.message.textContent).toBe("kernel trap: illegal instruction");
  });

  it("handles an empty message gracefully — panel still becomes visible", () => {
    showPanic("");
    expect(dom.panel.style.display).toBe("block");
    expect(dom.message.textContent).toBe("");
  });

  it("countdown label starts at 5 on the first tick", () => {
    showPanic("boom");
    // tick() is called synchronously on entry, so the first label update
    // happens before any timer fires.
    expect(dom.countdown.textContent).toBe("5");
  });

  it("countdown ticks 5 → 4 → 3 → 2 → 1 → 0 each second", () => {
    showPanic("boom");
    expect(dom.countdown.textContent).toBe("5");

    vi.advanceTimersByTime(1000);
    expect(dom.countdown.textContent).toBe("4");

    vi.advanceTimersByTime(1000);
    expect(dom.countdown.textContent).toBe("3");

    vi.advanceTimersByTime(1000);
    expect(dom.countdown.textContent).toBe("2");

    vi.advanceTimersByTime(1000);
    expect(dom.countdown.textContent).toBe("1");

    vi.advanceTimersByTime(1000);
    expect(dom.countdown.textContent).toBe("0");
  });

  it("calls window.location.reload() after 5 seconds", () => {
    showPanic("boom");
    expect(reloadSpy).not.toHaveBeenCalled();

    vi.advanceTimersByTime(4999);
    expect(reloadSpy).not.toHaveBeenCalled();

    vi.advanceTimersByTime(1);
    expect(reloadSpy).toHaveBeenCalledOnce();
  });

  it("does not call reload before the 5-second countdown elapses", () => {
    showPanic("boom");
    vi.advanceTimersByTime(4000);
    expect(reloadSpy).not.toHaveBeenCalled();
  });
});
