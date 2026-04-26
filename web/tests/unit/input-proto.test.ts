// Packed input event wire format tests.

import { describe, expect, it } from "vitest";
import {
  KBD_EVENT_SIZE,
  KbdKeyState,
  MOUSE_EVENT_SIZE,
  MouseButton,
  MouseButtonState,
  MouseEventKind,
  packKbdEvent,
  packMouseButton,
  packMouseMotion,
  packMouseWheel,
  unpackKbdEvent,
  unpackMouseEvent,
  unpackMouseWheel,
} from "../../src/shared/input-proto";

describe("mouse event packing", () => {
  it("packs a motion event at exactly MOUSE_EVENT_SIZE bytes", () => {
    const bytes = packMouseMotion(100, 50);
    expect(bytes.byteLength).toBe(MOUSE_EVENT_SIZE);
  });

  it("round-trips a motion event through pack + unpack", () => {
    const bytes = packMouseMotion(42, -7);
    const evt = unpackMouseEvent(bytes);
    expect(evt).not.toBeNull();
    if (!evt) return;
    expect(evt.kind).toBe(MouseEventKind.Motion);
    expect(evt.x).toBe(42);
    expect(evt.y).toBe(-7);
    expect(evt.button).toBe(0);
    expect(evt.state).toBe(MouseButtonState.Released);
  });

  it("round-trips a button press event", () => {
    const bytes = packMouseButton(
      200,
      150,
      MouseButton.Left,
      MouseButtonState.Pressed,
    );
    const evt = unpackMouseEvent(bytes);
    expect(evt).not.toBeNull();
    if (!evt) return;
    expect(evt.kind).toBe(MouseEventKind.Button);
    expect(evt.x).toBe(200);
    expect(evt.y).toBe(150);
    expect(evt.button).toBe(MouseButton.Left);
    expect(evt.state).toBe(MouseButtonState.Pressed);
  });

  it("round-trips a button release event", () => {
    const bytes = packMouseButton(
      0,
      0,
      MouseButton.Right,
      MouseButtonState.Released,
    );
    const evt = unpackMouseEvent(bytes);
    expect(evt).not.toBeNull();
    if (!evt) return;
    expect(evt.button).toBe(MouseButton.Right);
    expect(evt.state).toBe(MouseButtonState.Released);
  });

  it("supports negative coordinates via i32 encoding", () => {
    const bytes = packMouseMotion(-100, -200);
    const evt = unpackMouseEvent(bytes);
    expect(evt?.x).toBe(-100);
    expect(evt?.y).toBe(-200);
  });

  it("returns null for a truncated payload", () => {
    const truncated = new Uint8Array(MOUSE_EVENT_SIZE - 1);
    expect(unpackMouseEvent(truncated)).toBeNull();
  });

  it("returns null for an unknown kind", () => {
    const bytes = new Uint8Array(MOUSE_EVENT_SIZE);
    const view = new DataView(bytes.buffer);
    view.setUint32(0, 99, true); // not Motion (0) or Button (1)
    expect(unpackMouseEvent(bytes)).toBeNull();
  });

  it("returns null for an out-of-range state", () => {
    const bytes = packMouseButton(0, 0, 1, MouseButtonState.Pressed);
    const view = new DataView(bytes.buffer);
    view.setUint32(16, 99, true); // garbage state
    expect(unpackMouseEvent(bytes)).toBeNull();
  });
});

describe("keyboard event packing", () => {
  it("packs a keyboard event at exactly KBD_EVENT_SIZE bytes", () => {
    const bytes = packKbdEvent(0x1e, KbdKeyState.Pressed);
    expect(bytes.byteLength).toBe(KBD_EVENT_SIZE);
  });

  it("round-trips a key press + release", () => {
    const press = packKbdEvent(0x1e, KbdKeyState.Pressed);
    const release = packKbdEvent(0x1e, KbdKeyState.Released);
    expect(unpackKbdEvent(press)).toEqual({
      key: 0x1e,
      state: KbdKeyState.Pressed,
    });
    expect(unpackKbdEvent(release)).toEqual({
      key: 0x1e,
      state: KbdKeyState.Released,
    });
  });

  it("returns null for a truncated keyboard payload", () => {
    expect(unpackKbdEvent(new Uint8Array(KBD_EVENT_SIZE - 1))).toBeNull();
  });

  it("returns null for an out-of-range key state", () => {
    const bytes = packKbdEvent(0x1e, KbdKeyState.Pressed);
    const view = new DataView(bytes.buffer);
    view.setUint32(4, 99, true);
    expect(unpackKbdEvent(bytes)).toBeNull();
  });
});

// ---- mouse wheel events ---------------------------------------------

describe("mouse wheel event packing (T082)", () => {
  it("packs a wheel event at exactly MOUSE_EVENT_SIZE bytes", () => {
    const bytes = packMouseWheel(100, 50, 0, -3);
    expect(bytes.byteLength).toBe(MOUSE_EVENT_SIZE);
  });

  it("round-trips position + deltas through pack + unpackMouseWheel", () => {
    const bytes = packMouseWheel(42, -7, 1, -10);
    const decoded = unpackMouseWheel(bytes);
    expect(decoded).not.toBeNull();
    if (!decoded) return;
    expect(decoded.kind).toBe(MouseEventKind.Wheel);
    expect(decoded.x).toBe(42);
    expect(decoded.y).toBe(-7);
    expect(decoded.deltaX).toBe(1);
    expect(decoded.deltaY).toBe(-10);
  });

  it("returns null when the bytes carry a non-wheel discriminant", () => {
    const bytes = packMouseMotion(0, 0);
    expect(unpackMouseWheel(bytes)).toBeNull();
  });

  it("returns null for a truncated payload", () => {
    const bytes = packMouseWheel(0, 0, 0, 0);
    expect(unpackMouseWheel(bytes.subarray(0, 12))).toBeNull();
  });

  it("unpackMouseEvent recognises the wheel discriminant", () => {
    const bytes = packMouseWheel(10, 20, 0, 5);
    const decoded = unpackMouseEvent(bytes);
    expect(decoded).not.toBeNull();
    if (!decoded) return;
    expect(decoded.kind).toBe(MouseEventKind.Wheel);
    expect(decoded.x).toBe(10);
    expect(decoded.y).toBe(20);
  });
});
