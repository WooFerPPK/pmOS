// Input driver — TS half of /dev/input/kbd and /dev/input/mouse.
//
// Keyboard and mouse events are captured on the main thread
// (bootstrap-side event listeners, later a real display-server
// compositor) and forwarded as `input:kbd` and `input:mouse`
// messages to the kernel Worker. This driver is the
// receive-side: when a message arrives via `onHostMessage`, it
// pushes the event bytes into the kernel's per-device-node
// input ring, matching `DeviceDispatcher::inject_kbd_event`
// and `inject_mouse_event` on the Rust side.
//
// The input driver has **no output path**: `call(op, payload)`
// always returns `NotSupported`. The kernel's device dispatch
// already rejects writes to input device nodes with
// `DevError::NotSupported` (see `kernel::dev::dispatch::write`),
// so the TS-side error path is there only to catch buggy
// callers — there are no legitimate ops for this driver.
//
// One driver instance handles BOTH `/dev/input/kbd` and
// `/dev/input/mouse` because they share a driver class
// (`DriverId::InputKbd` is used as the class id, and the
// devnum routing happens inside `onHostMessage`). If a future
// Rust-side refactor splits InputKbd and InputMouse into two
// driver classes, this module is trivial to split.

import type { Driver, DriverHost, DriverResult } from "./types";
import { DriverErrorCode } from "./types";
import { Devnum, DriverId } from "../shared/platform-constants";

/**
 * Driver-class id. The input driver registers under
 * `DriverId.InputKbd` as the "primary" id of the pair; see the
 * module header for why. Tests that explicitly want to check
 * the mouse path drive it via `onHostMessage`, which is what
 * the real scaffold also does.
 */
export const INPUT_DRIVER_ID = DriverId.InputKbd;

/** Devnum for the keyboard device node. */
export const DEV_INPUT_KBD_NODE = Devnum.InputKbd;

/** Devnum for the mouse device node. */
export const DEV_INPUT_MOUSE_NODE = Devnum.InputMouse;

/** Worker-bound: "here is a packed keyboard event record". */
export interface InputKbdMessage {
  readonly kind: "input:kbd";
  readonly bytes: Uint8Array;
}

/** Worker-bound: "here is a packed mouse event record". */
export interface InputMouseMessage {
  readonly kind: "input:mouse";
  readonly bytes: Uint8Array;
}

function isInputKbd(m: unknown): m is InputKbdMessage {
  if (typeof m !== "object" || m === null) {
    return false;
  }
  const cand = m as { kind?: unknown; bytes?: unknown };
  return cand.kind === "input:kbd" && cand.bytes instanceof Uint8Array;
}

function isInputMouse(m: unknown): m is InputMouseMessage {
  if (typeof m !== "object" || m === null) {
    return false;
  }
  const cand = m as { kind?: unknown; bytes?: unknown };
  return cand.kind === "input:mouse" && cand.bytes instanceof Uint8Array;
}

export class InputDriver implements Driver {
  readonly driverId = INPUT_DRIVER_ID;
  readonly name = "input";
  private host: DriverHost | undefined;

  init(host: DriverHost): void {
    this.host = host;
  }

  /**
   * The input device nodes are read-only; every opcode is a
   * caller bug, reported as `Transport`. We DELIBERATELY do
   * not distinguish "driver not initialised" here because the
   * only valid response is "don't call me".
   */
  call(_op: number, _payload: Uint8Array): DriverResult {
    return { ok: false, error: DriverErrorCode.Transport };
  }

  onHostMessage(msg: unknown): void {
    const host = this.host;
    if (!host) {
      return;
    }
    if (isInputKbd(msg)) {
      const copy = new Uint8Array(msg.bytes.byteLength);
      copy.set(msg.bytes);
      host.pushInputToKernel(DEV_INPUT_KBD_NODE, copy);
      return;
    }
    if (isInputMouse(msg)) {
      const copy = new Uint8Array(msg.bytes.byteLength);
      copy.set(msg.bytes);
      host.pushInputToKernel(DEV_INPUT_MOUSE_NODE, copy);
      return;
    }
    // Unrelated message kinds are ignored silently, matching
    // the console driver's behaviour.
  }
}
