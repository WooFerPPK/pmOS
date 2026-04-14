// Console driver — the TS half of /dev/console.
//
// The kernel's in-kernel console handling lives in
// `crates/kernel/src/dev/mod.rs`: input arrives via
// `inject_console_input`, output is line-buffered and flushed
// via `platform::driver_call(Console, DEV_CONSOLE, bytes)`. This
// module is the other end of that `driver_call`:
//
//   * Output: when the kernel invokes `call(OP_WRITE_LINE,
//     payload)`, the driver posts a `console:write` message to
//     the main thread. The main thread displays the bytes in
//     its host sink (a hidden textarea in the demo page, or a
//     real terminal emulator in a later slice).
//   * Input: the main thread captures keystrokes and forwards
//     them as `console:input` messages. The scaffold routes
//     them here via `onHostMessage`, and the driver pushes the
//     bytes into the kernel's internal console input ring via
//     `DriverHost::pushInputToKernel`.
//
// This driver does no line editing, no escape parsing, and no
// echo: the kernel's `console_write` already handles line
// buffering, and a real terminal emulator in the main thread
// will handle cooked-mode semantics in a later slice.

import type { Driver, DriverHost, DriverResult } from "./types";
import { DriverErrorCode } from "./types";
import { DriverId, Devnum } from "../shared/platform-constants";

/** Driver-class identifier for the console driver. */
export const CONSOLE_DRIVER_ID = DriverId.Console;

/**
 * Device-node number for `/dev/console`. Used when pushing
 * captured input bytes into the kernel — the kernel's
 * `DeviceDispatcher` keys per-device input rings by devnum, not
 * by driver class. Matches `kernel::fs::devfs::DEV_CONSOLE`.
 */
export const DEV_CONSOLE_NODE = Devnum.Console;

/** Output opcode: "write a chunk to the console sink". */
export const OP_WRITE_LINE = 0x01;

/** Main-thread-bound message: "here is a chunk the kernel wrote". */
export interface ConsoleWriteMessage {
  readonly kind: "console:write";
  readonly bytes: Uint8Array;
}

/** Worker-bound message: "here is a chunk the user typed". */
export interface ConsoleInputMessage {
  readonly kind: "console:input";
  readonly bytes: Uint8Array;
}

function isConsoleInput(m: unknown): m is ConsoleInputMessage {
  if (typeof m !== "object" || m === null) {
    return false;
  }
  const cand = m as { kind?: unknown; bytes?: unknown };
  return cand.kind === "console:input" && cand.bytes instanceof Uint8Array;
}

export class ConsoleDriver implements Driver {
  readonly driverId = CONSOLE_DRIVER_ID;
  readonly name = "console";
  private host: DriverHost | undefined;

  init(host: DriverHost): void {
    this.host = host;
  }

  call(op: number, payload: Uint8Array): DriverResult {
    const host = this.host;
    if (!host) {
      return { ok: false, error: DriverErrorCode.NotReady };
    }
    if (op !== OP_WRITE_LINE) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    // Copy the payload defensively so the kernel can reuse its
    // outbound buffer immediately after this call returns.
    const copy = new Uint8Array(payload.byteLength);
    copy.set(payload);
    const message: ConsoleWriteMessage = { kind: "console:write", bytes: copy };
    host.postToMain(message);
    return { ok: true, value: payload.byteLength };
  }

  onHostMessage(msg: unknown): void {
    const host = this.host;
    if (!host) {
      return;
    }
    if (isConsoleInput(msg)) {
      // Copy defensively: the main thread may reuse the underlying
      // ArrayBuffer for later keystrokes.
      const copy = new Uint8Array(msg.bytes.byteLength);
      copy.set(msg.bytes);
      host.pushInputToKernel(DEV_CONSOLE_NODE, copy);
    }
  }
}
