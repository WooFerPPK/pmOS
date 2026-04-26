// Packed wire format for input driver messages.
//
// Both the main thread and the worker-side kernel agree on
// these byte layouts so a `Uint8Array` produced by the
// packer on one side round-trips through
// `host.postMessage({kind: "input:mouse", bytes})` and
// comes out decoded on the other side.
//
// The formats mirror the shape of `linux_input_event` just
// enough to support v1's narrowed input model: absolute
// pointer position, a single mouse button press/release,
// and a keyboard scancode press/release. No relative
// motion, no axis events, no modifier tracking in v1.
//
// All integers are little-endian. Fixed-size structs
// (never growing) so decoding is index-math.

// ---- Mouse events ---------------------------------------------

/** Wire size of a packed mouse event in bytes. */
export const MOUSE_EVENT_SIZE = 20;

/** `MouseEvent.kind` values. */
export const MouseEventKind = {
  /** Pointer moved to (x, y) in screen space. */
  Motion: 0,
  /** A mouse button was pressed or released at (x, y). */
  Button: 1,
  /** Wheel scrolled by `(button, state)` reinterpreted as
   *  `(deltaX, deltaY)` — see `packMouseWheel`. v1 reserves
   *  this discriminant for the wheel-scroll path the
   *  display server's window manager will route to focus
   *  windows. */
  Wheel: 2,
} as const;

export type MouseEventKindValue =
  (typeof MouseEventKind)[keyof typeof MouseEventKind];

/** `MouseEvent.state` values for button events. */
export const MouseButtonState = {
  Released: 0,
  Pressed: 1,
} as const;

export type MouseButtonStateValue =
  (typeof MouseButtonState)[keyof typeof MouseButtonState];

/** `MouseEvent.button` values. v1 supports the three
 * standard buttons; other buttons round-trip as their
 * numeric code without any special meaning. */
export const MouseButton = {
  Left: 1,
  Right: 2,
  Middle: 3,
} as const;

/** A decoded mouse event. All fields are present for both
 * kinds; `button` and `state` are 0 on a motion event. */
export interface MouseEvent {
  readonly kind: MouseEventKindValue;
  readonly x: number;
  readonly y: number;
  readonly button: number;
  readonly state: MouseButtonStateValue;
}

/** Pack a motion event as `MOUSE_EVENT_SIZE` bytes. */
export function packMouseMotion(x: number, y: number): Uint8Array {
  return packMouseEvent(MouseEventKind.Motion, x, y, 0, MouseButtonState.Released);
}

/** Pack a button event as `MOUSE_EVENT_SIZE` bytes. */
export function packMouseButton(
  x: number,
  y: number,
  button: number,
  state: MouseButtonStateValue,
): Uint8Array {
  return packMouseEvent(MouseEventKind.Button, x, y, button, state);
}

/**
 * Pack a wheel-scroll event as `MOUSE_EVENT_SIZE` bytes.
 *
 * The wire reuses the existing five-field shape rather than
 * widening the struct: `button` carries `deltaX` (signed) and
 * `state` carries `deltaY` (signed) reinterpreted through their
 * u32 binary form. Unpacking via `unpackMouseEvent` uses
 * [`MouseEventKind.Wheel`] as the discriminant; the wheel-aware
 * decoder is `unpackMouseWheel` below.
 */
export function packMouseWheel(
  x: number,
  y: number,
  deltaX: number,
  deltaY: number,
): Uint8Array {
  const out = new Uint8Array(MOUSE_EVENT_SIZE);
  const view = new DataView(out.buffer);
  view.setUint32(0, MouseEventKind.Wheel, true);
  view.setInt32(4, x, true);
  view.setInt32(8, y, true);
  view.setInt32(12, deltaX, true);
  view.setInt32(16, deltaY, true);
  return out;
}

/** Decoded wheel event with deltas instead of button/state. */
export interface WheelEvent {
  readonly kind: typeof MouseEventKind.Wheel;
  readonly x: number;
  readonly y: number;
  readonly deltaX: number;
  readonly deltaY: number;
}

/**
 * Decode a wheel event. Returns null if the bytes don't carry
 * a wheel discriminant or the buffer is too short.
 */
export function unpackMouseWheel(bytes: Uint8Array): WheelEvent | null {
  if (bytes.byteLength < MOUSE_EVENT_SIZE) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const kind = view.getUint32(0, true);
  if (kind !== MouseEventKind.Wheel) return null;
  return {
    kind: MouseEventKind.Wheel,
    x: view.getInt32(4, true),
    y: view.getInt32(8, true),
    deltaX: view.getInt32(12, true),
    deltaY: view.getInt32(16, true),
  };
}

function packMouseEvent(
  kind: MouseEventKindValue,
  x: number,
  y: number,
  button: number,
  state: MouseButtonStateValue,
): Uint8Array {
  const out = new Uint8Array(MOUSE_EVENT_SIZE);
  const view = new DataView(out.buffer);
  view.setUint32(0, kind, true);
  view.setInt32(4, x, true);
  view.setInt32(8, y, true);
  view.setUint32(12, button, true);
  view.setUint32(16, state, true);
  return out;
}

/**
 * Decode a packed mouse event. Returns null if `bytes` is
 * shorter than `MOUSE_EVENT_SIZE` or the `kind` field
 * isn't a recognised variant.
 */
export function unpackMouseEvent(bytes: Uint8Array): MouseEvent | null {
  if (bytes.byteLength < MOUSE_EVENT_SIZE) {
    return null;
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const kind = view.getUint32(0, true);
  if (
    kind !== MouseEventKind.Motion &&
    kind !== MouseEventKind.Button &&
    kind !== MouseEventKind.Wheel
  ) {
    return null;
  }
  // Wheel events carry signed deltas in the button/state slots,
  // not button-press semantics; callers that need deltas should
  // use `unpackMouseWheel` instead. Returning the wheel
  // discriminant via `unpackMouseEvent` is harmless for
  // motion/button-only consumers since they branch on `kind`.
  if (kind === MouseEventKind.Wheel) {
    return {
      kind: kind as MouseEventKindValue,
      x: view.getInt32(4, true),
      y: view.getInt32(8, true),
      // `button` reinterprets `deltaX` u32-bits as a u32; tests
      // that use unpackMouseEvent on a wheel will see the raw
      // bits, which is fine because they're expected to use
      // unpackMouseWheel anyway.
      button: view.getUint32(12, true),
      state: MouseButtonState.Released,
    };
  }
  const x = view.getInt32(4, true);
  const y = view.getInt32(8, true);
  const button = view.getUint32(12, true);
  const stateRaw = view.getUint32(16, true);
  if (
    stateRaw !== MouseButtonState.Released &&
    stateRaw !== MouseButtonState.Pressed
  ) {
    return null;
  }
  return {
    kind: kind as MouseEventKindValue,
    x,
    y,
    button,
    state: stateRaw as MouseButtonStateValue,
  };
}

// ---- Keyboard events (wire format reserved for next slice) ----

/** Wire size of a packed keyboard event in bytes. */
export const KBD_EVENT_SIZE = 8;

/** `KbdEvent.state` values. */
export const KbdKeyState = {
  Released: 0,
  Pressed: 1,
} as const;

export type KbdKeyStateValue = (typeof KbdKeyState)[keyof typeof KbdKeyState];

export interface KbdEvent {
  readonly key: number;
  readonly state: KbdKeyStateValue;
}

/** Pack a keyboard event as `KBD_EVENT_SIZE` bytes. */
export function packKbdEvent(key: number, state: KbdKeyStateValue): Uint8Array {
  const out = new Uint8Array(KBD_EVENT_SIZE);
  const view = new DataView(out.buffer);
  view.setUint32(0, key, true);
  view.setUint32(4, state, true);
  return out;
}

export function unpackKbdEvent(bytes: Uint8Array): KbdEvent | null {
  if (bytes.byteLength < KBD_EVENT_SIZE) {
    return null;
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const key = view.getUint32(0, true);
  const stateRaw = view.getUint32(4, true);
  if (
    stateRaw !== KbdKeyState.Released &&
    stateRaw !== KbdKeyState.Pressed
  ) {
    return null;
  }
  return { key, state: stateRaw as KbdKeyStateValue };
}
