// Unit tests for the pre-WASM MockKernel.
//
// Covers the line-buffering logic, both echo and faux-shell
// policies, and the `fauxShellTransform` helper in isolation.
// The `MockKernel.bindScaffold` integration is deliberately
// minimal — we stub `callDriver` with a capturing fake rather
// than wiring up a full `bootKernelWorker`, because the
// scaffold's routing is already covered by
// `kernel-worker.test.ts`.

import { describe, expect, it } from "vitest";
import {
  MockKernel,
  SPLASH_HEIGHT,
  SPLASH_WIDTH,
  fauxShellTransform,
  generateSplashPixels,
} from "../../src/mock-kernel";
import type { KernelWorker } from "../../src/kernel-worker";
import { DriverErrorCode } from "../../src/drivers/types";
import type { DriverResult } from "../../src/drivers/types";
import {
  CONSOLE_DRIVER_ID,
  DEV_CONSOLE_NODE,
  OP_WRITE_LINE,
} from "../../src/drivers/console";
import {
  FB_DRIVER_ID,
  OP_BLIT as FB_OP_BLIT,
  OP_SET_MODE as FB_OP_SET_MODE,
} from "../../src/drivers/fb";
import { Devnum } from "../../src/shared/platform-constants";
import {
  KbdKeyState,
  MouseButton,
  MouseButtonState,
  MouseEventKind,
  packKbdEvent,
  packMouseButton,
  packMouseMotion,
} from "../../src/shared/input-proto";

interface CapturingScaffold extends KernelWorker {
  readonly calls: Array<{ driverId: number; op: number; payload: Uint8Array }>;
}

function makeScaffold(): CapturingScaffold {
  const calls: Array<{ driverId: number; op: number; payload: Uint8Array }> = [];
  return {
    calls,
    handleMainMessage(): void {
      // Unused in these tests.
    },
    callDriver(driverId: number, op: number, payload: Uint8Array): DriverResult {
      const copy = new Uint8Array(payload.byteLength);
      copy.set(payload);
      calls.push({ driverId, op, payload: copy });
      return { ok: true, value: payload.byteLength };
    },
    get driverCount(): number {
      return 1;
    },
  };
}

describe("MockKernel echo policy", () => {
  it("buffers input until newline, then echoes the whole line", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    // Partial line — no output yet.
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("hel"));
    expect(scaffold.calls).toHaveLength(0);

    // Completing newline — flush.
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("lo\n"));
    expect(scaffold.calls).toHaveLength(1);
    expect(scaffold.calls[0]?.driverId).toBe(CONSOLE_DRIVER_ID);
    expect(scaffold.calls[0]?.op).toBe(OP_WRITE_LINE);
    expect(new TextDecoder().decode(scaffold.calls[0]?.payload)).toBe("hello\n");
  });

  it("flushes multiple lines in one injected chunk", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(
      DEV_CONSOLE_NODE,
      new TextEncoder().encode("one\ntwo\nthree\n"),
    );
    expect(scaffold.calls).toHaveLength(3);
    expect(new TextDecoder().decode(scaffold.calls[0]?.payload)).toBe("one\n");
    expect(new TextDecoder().decode(scaffold.calls[1]?.payload)).toBe("two\n");
    expect(new TextDecoder().decode(scaffold.calls[2]?.payload)).toBe("three\n");
  });

  it("drops non-console devnums in v1", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(Devnum.InputKbd, new TextEncoder().encode("ignored\n"));
    mock.injectInput(Devnum.InputMouse, new TextEncoder().encode("ignored\n"));
    expect(scaffold.calls).toHaveLength(0);
  });

  it("before bindScaffold, lines are buffered but no output is emitted", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("stray\n"));
    // No scaffold bound yet → the line is lost. Should not throw.
  });
});

describe("MockKernel faux-shell policy", () => {
  it("translates 'echo X' into 'X\\n'", () => {
    const mock = new MockKernel({ policy: { kind: "faux-shell" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("echo hello\n"));
    expect(scaffold.calls).toHaveLength(1);
    expect(new TextDecoder().decode(scaffold.calls[0]?.payload)).toBe("hello\n");
  });

  it("unknown commands emit '?\\n'", () => {
    const mock = new MockKernel({ policy: { kind: "faux-shell" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("ls\n"));
    expect(scaffold.calls).toHaveLength(1);
    expect(new TextDecoder().decode(scaffold.calls[0]?.payload)).toBe("?\n");
  });

  it("empty lines produce no output", () => {
    const mock = new MockKernel({ policy: { kind: "faux-shell" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("\n"));
    expect(scaffold.calls).toHaveLength(0);
  });
});

// ---- splash emission -----------------------------------------------

/**
 * Build a scaffold whose `callDriver` always succeeds EXCEPT
 * the framebuffer path, which returns a caller-specified
 * value. Used by the splash tests that want to simulate
 * "fb driver missing".
 */
function makeScaffoldWithFbResult(fbResult: DriverResult): CapturingScaffold {
  const calls: Array<{ driverId: number; op: number; payload: Uint8Array }> = [];
  return {
    calls,
    handleMainMessage(): void {
      /* unused */
    },
    callDriver(driverId: number, op: number, payload: Uint8Array): DriverResult {
      const copy = new Uint8Array(payload.byteLength);
      copy.set(payload);
      calls.push({ driverId, op, payload: copy });
      if (driverId === FB_DRIVER_ID) {
        return fbResult;
      }
      return { ok: true, value: payload.byteLength };
    },
    get driverCount(): number {
      return 2;
    },
  };
}

describe("MockKernel splash emission", () => {
  it("is opt-in: default constructor does not emit on input", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("hi\n"));
    const fbCalls = scaffold.calls.filter((c) => c.driverId === FB_DRIVER_ID);
    expect(fbCalls).toHaveLength(0);
  });

  it("emitSplashOnFirstInput=true emits SET_MODE + BLIT on the first injected input", () => {
    const mock = new MockKernel({
      policy: { kind: "echo" },
      emitSplashOnFirstInput: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    // Before input: no scaffold calls.
    expect(scaffold.calls).toHaveLength(0);

    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("hi\n"));

    const fbCalls = scaffold.calls.filter((c) => c.driverId === FB_DRIVER_ID);
    expect(fbCalls).toHaveLength(2);
    expect(fbCalls[0]?.op).toBe(FB_OP_SET_MODE);
    expect(fbCalls[1]?.op).toBe(FB_OP_BLIT);

    // The SET_MODE payload is a 2x u32 LE geometry blob.
    const setModePayload = fbCalls[0]?.payload;
    if (setModePayload) {
      const view = new DataView(
        setModePayload.buffer,
        setModePayload.byteOffset,
        setModePayload.byteLength,
      );
      expect(view.getUint32(0, true)).toBe(SPLASH_WIDTH);
      expect(view.getUint32(4, true)).toBe(SPLASH_HEIGHT);
    }

    // The BLIT payload is header + width*height*4 pixel bytes.
    const blitPayload = fbCalls[1]?.payload;
    if (blitPayload) {
      expect(blitPayload.byteLength).toBe(8 + SPLASH_WIDTH * SPLASH_HEIGHT * 4);
    }
  });

  it("emits the splash only once even across many inputs", () => {
    const mock = new MockKernel({
      policy: { kind: "echo" },
      emitSplashOnFirstInput: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(
      DEV_CONSOLE_NODE,
      new TextEncoder().encode("one\ntwo\nthree\nfour\n"),
    );
    const fbCalls = scaffold.calls.filter((c) => c.driverId === FB_DRIVER_ID);
    expect(fbCalls).toHaveLength(2);
  });

  it("skips the splash when the scaffold reports NotReady for the fb driver", () => {
    const scaffold = makeScaffoldWithFbResult({
      ok: false,
      error: DriverErrorCode.NotReady,
    });
    const mock = new MockKernel({
      policy: { kind: "echo" },
      emitSplashOnFirstInput: true,
    });
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("go\n"));

    const fbCalls = scaffold.calls.filter((c) => c.driverId === FB_DRIVER_ID);
    // SET_MODE was ATTEMPTED (one call recorded on the fake
    // scaffold), then the mock bailed out and never posted BLIT.
    expect(fbCalls).toHaveLength(1);
    expect(fbCalls[0]?.op).toBe(FB_OP_SET_MODE);
  });

  it("still delivers the echo response after emitting the splash", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      emitSplashOnFirstInput: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(
      DEV_CONSOLE_NODE,
      new TextEncoder().encode("echo hello\n"),
    );
    const consoleCalls = scaffold.calls.filter(
      (c) => c.driverId === CONSOLE_DRIVER_ID,
    );
    expect(consoleCalls).toHaveLength(1);
    expect(consoleCalls[0]?.op).toBe(OP_WRITE_LINE);
    expect(new TextDecoder().decode(consoleCalls[0]?.payload)).toBe("hello\n");
  });

  it("skips the splash when no scaffold is bound", () => {
    const mock = new MockKernel({
      policy: { kind: "echo" },
      emitSplashOnFirstInput: true,
    });
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("stray\n"));
    // No scaffold => nothing to verify, but the mock must
    // not throw.
  });

  it("generateSplashPixels produces width*height*4 bytes", () => {
    const rgba = generateSplashPixels(8, 4);
    expect(rgba.byteLength).toBe(8 * 4 * 4);
    // The top-left pixel should differ from the center pixel
    // (center is brighter under the radial gradient).
    const cornerG = rgba[1] ?? 0;
    const centerIdx = (2 * 8 + 4) * 4;
    const centerG = rgba[centerIdx + 1] ?? 0;
    expect(centerG).toBeGreaterThan(cornerG);
    // Every alpha byte is 255.
    for (let i = 3; i < rgba.byteLength; i += 4) {
      expect(rgba[i]).toBe(255);
    }
  });
});

// ---- /dev/input/{kbd,mouse} routing --------------------------------

describe("MockKernel input device ring consumption", () => {
  it("decodes a packed mouse motion event into pointerPosition", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    mock.bindScaffold(makeScaffold());
    mock.injectInput(Devnum.InputMouse, packMouseMotion(42, 50));
    expect(mock.pointerPosition).toEqual({ x: 42, y: 50 });
    expect(mock.mouseEventsObserved).toBe(1);
  });

  it("tracks sequential motion events and exposes the latest position", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    mock.bindScaffold(makeScaffold());
    for (const [x, y] of [
      [0, 0],
      [10, 10],
      [100, -5],
    ] as const) {
      mock.injectInput(Devnum.InputMouse, packMouseMotion(x, y));
    }
    expect(mock.pointerPosition).toEqual({ x: 100, y: -5 });
    expect(mock.mouseEventsObserved).toBe(3);
  });

  it("records the latest button event separately from motion", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    mock.bindScaffold(makeScaffold());
    mock.injectInput(Devnum.InputMouse, packMouseMotion(20, 30));
    mock.injectInput(
      Devnum.InputMouse,
      packMouseButton(20, 30, MouseButton.Left, MouseButtonState.Pressed),
    );
    const btn = mock.lastMouseButton;
    expect(btn).not.toBeNull();
    if (!btn) return;
    expect(btn.kind).toBe(MouseEventKind.Button);
    expect(btn.button).toBe(MouseButton.Left);
    expect(btn.state).toBe(MouseButtonState.Pressed);
    expect(btn.x).toBe(20);
    expect(btn.y).toBe(30);
  });

  it("drops a malformed mouse payload without throwing", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    mock.bindScaffold(makeScaffold());
    // Too-short buffer — unpacker returns null, kernel ignores it.
    mock.injectInput(Devnum.InputMouse, new Uint8Array(4));
    expect(mock.mouseEventsObserved).toBe(0);
    expect(mock.pointerPosition).toBeNull();
  });

  it("a pointer motion event triggers a re-blit in live-terminal mode", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    const baselineBlits = scaffold.calls.filter(
      (c) => c.driverId === FB_DRIVER_ID && c.op === FB_OP_BLIT,
    ).length;
    mock.injectInput(Devnum.InputMouse, packMouseMotion(50, 60));
    const after = scaffold.calls.filter(
      (c) => c.driverId === FB_DRIVER_ID && c.op === FB_OP_BLIT,
    ).length;
    expect(after - baselineBlits).toBe(1);
  });

  it("pointer events produce no fb traffic when live-terminal is off", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(Devnum.InputMouse, packMouseMotion(10, 10));
    const fbCalls = scaffold.calls.filter(
      (c) => c.driverId === FB_DRIVER_ID,
    );
    expect(fbCalls).toHaveLength(0);
  });

  it("decodes a packed keyboard event into the kbd counter", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    mock.bindScaffold(makeScaffold());
    mock.injectInput(Devnum.InputKbd, packKbdEvent(0x1e, KbdKeyState.Pressed));
    mock.injectInput(
      Devnum.InputKbd,
      packKbdEvent(0x1e, KbdKeyState.Released),
    );
    expect(mock.kbdEventsObserved).toBe(2);
  });

  it("drops a malformed kbd payload without throwing", () => {
    const mock = new MockKernel({ policy: { kind: "echo" } });
    mock.bindScaffold(makeScaffold());
    mock.injectInput(Devnum.InputKbd, new Uint8Array(3));
    expect(mock.kbdEventsObserved).toBe(0);
  });
});

// ---- Live-terminal mode --------------------------------------------

describe("MockKernel live-terminal mode", () => {
  it("emits SET_MODE and an initial BLIT on bindScaffold when scrollback is pre-seeded", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      liveTerminal: true,
      initialScrollback: [
        { text: "PMos 0.1.0", kind: "banner" },
        { text: "ready", kind: "banner" },
      ],
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    const fbCalls = scaffold.calls.filter((c) => c.driverId === FB_DRIVER_ID);
    // SET_MODE + initial BLIT for the pre-seeded banner.
    expect(fbCalls).toHaveLength(2);
    expect(fbCalls[0]?.op).toBe(FB_OP_SET_MODE);
    expect(fbCalls[1]?.op).toBe(FB_OP_BLIT);

    // The BLIT carries width*height*4 bytes of pixels.
    const blitPayload = fbCalls[1]?.payload;
    if (blitPayload) {
      expect(blitPayload.byteLength).toBe(
        8 + SPLASH_WIDTH * SPLASH_HEIGHT * 4,
      );
    }
  });

  it("blits once per injectInput call that changes state", () => {
    // Each `injectInput` call is one batch — the mock
    // rasterizes + blits exactly once at the end of the
    // batch if anything changed, not per byte. Real
    // keyboard typing goes one key per injectInput, so
    // the user sees a fresh frame after each keystroke;
    // a pasted multi-char batch becomes a single frame.
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    // Baseline: SET_MODE + initial blit from bindScaffold.
    const baseline = scaffold.calls.filter(
      (c) => c.driverId === FB_DRIVER_ID,
    ).length;

    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("h"));
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("i"));

    const fbCalls = scaffold.calls.filter((c) => c.driverId === FB_DRIVER_ID);
    // Two more BLITs on top of the baseline, one per call.
    expect(fbCalls.length - baseline).toBe(2);
    expect(fbCalls[fbCalls.length - 2]?.op).toBe(FB_OP_BLIT);
    expect(fbCalls[fbCalls.length - 1]?.op).toBe(FB_OP_BLIT);
  });

  it("a single multi-byte injectInput batch produces exactly one blit", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    const baseline = scaffold.calls.filter(
      (c) => c.driverId === FB_DRIVER_ID && c.op === FB_OP_BLIT,
    ).length;

    // Paste-style batch — 5 chars in a single call.
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("hello"));
    const blits = scaffold.calls.filter(
      (c) => c.driverId === FB_DRIVER_ID && c.op === FB_OP_BLIT,
    ).length;
    expect(blits - baseline).toBe(1);
    expect(mock.liveInput).toBe("hello");
  });

  it("accumulates printable characters in the input buffer", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("echo hi"));
    expect(mock.liveInput).toBe("echo hi");
    expect(mock.liveScrollback).toHaveLength(0);
  });

  it("commits the input line to scrollback on newline and clears the input buffer", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(
      DEV_CONSOLE_NODE,
      new TextEncoder().encode("echo hi\n"),
    );

    expect(mock.liveInput).toBe("");
    const scrollback = mock.liveScrollback;
    // "> echo hi" as input, then "hi" as output.
    expect(scrollback).toHaveLength(2);
    expect(scrollback[0]?.text).toBe("> echo hi");
    expect(scrollback[0]?.kind).toBe("input");
    expect(scrollback[1]?.text).toBe("hi");
    expect(scrollback[1]?.kind).toBe("output");

    // And the console driver still got the output bytes
    // so the echo round-trip test keeps working.
    const consoleCalls = scaffold.calls.filter(
      (c) => c.driverId === CONSOLE_DRIVER_ID,
    );
    expect(consoleCalls).toHaveLength(1);
    expect(new TextDecoder().decode(consoleCalls[0]?.payload)).toBe("hi\n");
  });

  it("backspace (0x7f) pops the last character from the input buffer", () => {
    const mock = new MockKernel({
      policy: { kind: "echo" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("abc"));
    expect(mock.liveInput).toBe("abc");
    mock.injectInput(DEV_CONSOLE_NODE, new Uint8Array([0x7f]));
    expect(mock.liveInput).toBe("ab");
    mock.injectInput(DEV_CONSOLE_NODE, new Uint8Array([0x7f, 0x7f]));
    expect(mock.liveInput).toBe("");
    // Backspacing an empty buffer is a no-op (doesn't
    // change state, so no new blit either).
    const blitsBefore = scaffold.calls.filter(
      (c) => c.driverId === FB_DRIVER_ID && c.op === FB_OP_BLIT,
    ).length;
    mock.injectInput(DEV_CONSOLE_NODE, new Uint8Array([0x7f]));
    const blitsAfter = scaffold.calls.filter(
      (c) => c.driverId === FB_DRIVER_ID && c.op === FB_OP_BLIT,
    ).length;
    expect(blitsAfter).toBe(blitsBefore);
  });

  it("backspace 0x08 pops like 0x7f", () => {
    const mock = new MockKernel({
      policy: { kind: "echo" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("ab"));
    mock.injectInput(DEV_CONSOLE_NODE, new Uint8Array([0x08]));
    expect(mock.liveInput).toBe("a");
  });

  it("drops non-printable bytes outside 0x20..0x7e", () => {
    const mock = new MockKernel({
      policy: { kind: "echo" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    // 0x00 NUL, 0x07 BEL, 0x1b ESC — all ignored.
    mock.injectInput(
      DEV_CONSOLE_NODE,
      new Uint8Array([0x00, 0x07, 0x1b, 0x61 /* 'a' */]),
    );
    expect(mock.liveInput).toBe("a");
  });

  it("help command spans multiple scrollback output lines", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("help\n"));

    const scrollback = mock.liveScrollback;
    // "> help" input + 7 help lines from FAUX_SHELL_HELP.
    expect(scrollback[0]?.text).toBe("> help");
    expect(scrollback[0]?.kind).toBe("input");
    // The first output line is "commands:".
    expect(scrollback[1]?.text).toBe("commands:");
    expect(scrollback[1]?.kind).toBe("output");
    // Full length is 1 input + 7 help lines.
    expect(scrollback).toHaveLength(1 + 7);
  });

  it("panic command short-circuits scrollback and forwards to panicEmit", () => {
    const panicMessages: string[] = [];
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      liveTerminal: true,
      panicEmit: (m) => panicMessages.push(m),
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(
      DEV_CONSOLE_NODE,
      new TextEncoder().encode("panic boom\n"),
    );

    expect(panicMessages).toEqual(["kernel: boom"]);
    // Input was still added to scrollback (the user typed
    // it) but no output was evaluated — "> panic boom" is
    // the only scrollback entry produced by the commit.
    const scrollback = mock.liveScrollback;
    expect(scrollback[scrollback.length - 1]?.text).toBe("> panic boom");
  });

  it("multiple commands accumulate scrollback in order", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(
      DEV_CONSOLE_NODE,
      new TextEncoder().encode("echo one\necho two\n"),
    );
    const scrollback = mock.liveScrollback;
    expect(scrollback.map((l) => l.text)).toEqual([
      "> echo one",
      "one",
      "> echo two",
      "two",
    ]);
  });

  it("unknown command lands on the scrollback as the policy '?' response", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("wat\n"));
    const scrollback = mock.liveScrollback;
    expect(scrollback).toHaveLength(2);
    expect(scrollback[0]?.text).toBe("> wat");
    expect(scrollback[1]?.text).toBe("?");
  });

  it("bindScaffold without pre-seeded scrollback still emits SET_MODE on first input", () => {
    // Edge case: constructor takes liveTerminal: true but
    // no initialScrollback. bindScaffold triggers a
    // SET_MODE + empty blit; subsequent keystrokes add to
    // the pixel counter.
    const mock = new MockKernel({
      policy: { kind: "echo" },
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    const setModeCalls = scaffold.calls.filter(
      (c) => c.driverId === FB_DRIVER_ID && c.op === FB_OP_SET_MODE,
    );
    expect(setModeCalls).toHaveLength(1);
  });

  it("liveTerminal wins when both splash and live flags are set", () => {
    // Mutually-exclusive flags: liveTerminal has
    // precedence. The splash-once path never runs.
    const mock = new MockKernel({
      policy: { kind: "echo" },
      emitSplashOnFirstInput: true,
      liveTerminal: true,
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    const setModeCount = scaffold.calls.filter(
      (c) => c.driverId === FB_DRIVER_ID && c.op === FB_OP_SET_MODE,
    ).length;
    // Only ONE SET_MODE (from the live path's bindScaffold
    // initial render), not two from both paths.
    expect(setModeCount).toBe(1);

    // Typing still goes through the live-terminal handler.
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("a"));
    expect(mock.liveInput).toBe("a");
  });

  it("skips SET_MODE retries when the fb driver reports NotReady on the first attempt", () => {
    const calls: Array<{ driverId: number; op: number; payload: Uint8Array }> =
      [];
    const scaffold: CapturingScaffold = {
      calls,
      handleMainMessage(): void {
        /* unused */
      },
      callDriver(driverId: number, op: number, payload: Uint8Array): DriverResult {
        const copy = new Uint8Array(payload.byteLength);
        copy.set(payload);
        calls.push({ driverId, op, payload: copy });
        if (driverId === FB_DRIVER_ID) {
          return { ok: false, error: DriverErrorCode.NotReady };
        }
        return { ok: true, value: payload.byteLength };
      },
      get driverCount(): number {
        return 2;
      },
    };
    const mock = new MockKernel({
      policy: { kind: "echo" },
      liveTerminal: true,
    });
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("abc"));

    // One SET_MODE attempt on bindScaffold, no BLIT (driver
    // returned NotReady), and no further SET_MODE attempts
    // on subsequent keystrokes.
    const setModeCount = calls.filter(
      (c) => c.driverId === FB_DRIVER_ID && c.op === FB_OP_SET_MODE,
    ).length;
    const blitCount = calls.filter(
      (c) => c.driverId === FB_DRIVER_ID && c.op === FB_OP_BLIT,
    ).length;
    expect(setModeCount).toBe(1);
    expect(blitCount).toBe(0);
  });
});

describe("fauxShellTransform", () => {
  it("extracts the echo argument and appends a newline", () => {
    const out = fauxShellTransform(new TextEncoder().encode("echo hi\n"));
    expect(new TextDecoder().decode(out)).toBe("hi\n");
  });

  it("handles a line without a trailing newline", () => {
    const out = fauxShellTransform(new TextEncoder().encode("echo hi"));
    expect(new TextDecoder().decode(out)).toBe("hi\n");
  });

  it("returns '?\\n' for an unknown command", () => {
    const out = fauxShellTransform(new TextEncoder().encode("wat\n"));
    expect(new TextDecoder().decode(out)).toBe("?\n");
  });

  it("returns an empty buffer for a bare newline", () => {
    const out = fauxShellTransform(new TextEncoder().encode("\n"));
    expect(out.byteLength).toBe(0);
  });

  it("echoes an empty argument as just a newline", () => {
    const out = fauxShellTransform(new TextEncoder().encode("echo \n"));
    expect(new TextDecoder().decode(out)).toBe("\n");
  });

  it("'help' prints the full command list", async () => {
    const { FAUX_SHELL_HELP } = await import("../../src/mock-kernel");
    const out = fauxShellTransform(new TextEncoder().encode("help\n"));
    const text = new TextDecoder().decode(out);
    expect(text.endsWith("\n")).toBe(true);
    // Every line from FAUX_SHELL_HELP appears in the output.
    for (const line of FAUX_SHELL_HELP) {
      expect(text).toContain(line);
    }
  });

  it("'date' prints a fixed date string", () => {
    const out = fauxShellTransform(new TextEncoder().encode("date\n"));
    expect(new TextDecoder().decode(out)).toBe("2026-04-14\n");
  });

  it("'whoami' prints 'pmos'", () => {
    const out = fauxShellTransform(new TextEncoder().encode("whoami\n"));
    expect(new TextDecoder().decode(out)).toBe("pmos\n");
  });

  it("'uname' prints the system banner", () => {
    const out = fauxShellTransform(new TextEncoder().encode("uname\n"));
    expect(new TextDecoder().decode(out)).toBe("PMos 0.1.0-demo\n");
  });

  it("commands are case-sensitive: 'HELP' is unknown", () => {
    const out = fauxShellTransform(new TextEncoder().encode("HELP\n"));
    expect(new TextDecoder().decode(out)).toBe("?\n");
  });
});

describe("MockKernel panic command", () => {
  it("'panic <message>' calls the configured panicEmit sink", () => {
    const panics: string[] = [];
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      panicEmit: (m) => panics.push(m),
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(
      DEV_CONSOLE_NODE,
      new TextEncoder().encode("panic kernel exploded\n"),
    );

    expect(panics).toEqual(["kernel: kernel exploded"]);
    // Panic short-circuits — no console:write goes out.
    const consoleCalls = scaffold.calls.filter(
      (c) => c.driverId === CONSOLE_DRIVER_ID,
    );
    expect(consoleCalls).toHaveLength(0);
  });

  it("'panic' with no message emits a default notice", () => {
    const panics: string[] = [];
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      panicEmit: (m) => panics.push(m),
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("panic\n"));
    expect(panics).toHaveLength(1);
    expect(panics[0]).toContain("no message");
  });

  it("'panic' command in echo policy mode also fires the sink", () => {
    // The panic interceptor lives at the line-flush
    // layer, not inside the policy switch, so it works
    // regardless of which policy is active.
    const panics: string[] = [];
    const mock = new MockKernel({
      policy: { kind: "echo" },
      panicEmit: (m) => panics.push(m),
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("panic boom\n"));
    expect(panics).toEqual(["kernel: boom"]);
  });

  it("without a panicEmit sink, 'panic' is silently swallowed", () => {
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      // No panicEmit configured.
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("panic test\n"));
    // No console:write either — panic short-circuits.
    expect(scaffold.calls.filter((c) => c.driverId === CONSOLE_DRIVER_ID)).toHaveLength(0);
  });

  it("a normal command after a panic still works", () => {
    const panics: string[] = [];
    const mock = new MockKernel({
      policy: { kind: "faux-shell" },
      panicEmit: (m) => panics.push(m),
    });
    const scaffold = makeScaffold();
    mock.bindScaffold(scaffold);

    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("panic first\n"));
    mock.injectInput(DEV_CONSOLE_NODE, new TextEncoder().encode("echo hello\n"));

    expect(panics).toEqual(["kernel: first"]);
    const writes = scaffold.calls.filter((c) => c.driverId === CONSOLE_DRIVER_ID);
    expect(writes).toHaveLength(1);
    expect(new TextDecoder().decode(writes[0]?.payload)).toBe("hello\n");
  });

  it("FAUX_SHELL_HELP advertises the panic command", async () => {
    const { FAUX_SHELL_HELP } = await import("../../src/mock-kernel");
    const text = FAUX_SHELL_HELP.join("\n");
    expect(text).toContain("panic");
  });
});
