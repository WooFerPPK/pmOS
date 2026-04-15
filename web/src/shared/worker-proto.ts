// Message protocol between the main thread and the kernel Worker.
//
// Both sides discriminate on the `kind` field. Every message is
// an interface with a readonly `kind` so TypeScript's narrowing
// through `switch` statements is exhaustive by construction.
//
// This protocol carries HIGH-LEVEL events — boot, shutdown,
// console I/O, driver events. The actual kernel<->user-process
// syscall traffic rides on a SharedArrayBuffer ring buffer
// defined by `../shared/sab-layout.ts`, not by these messages.

import type { ConsoleInputMessage, ConsoleWriteMessage } from "../drivers/console";
import type { FbBlitMessage, FbSetModeMessage } from "../drivers/fb";
import type { InputKbdMessage, InputMouseMessage } from "../drivers/input";

/** Boot-time configuration forwarded from the main thread. */
export interface BootConfig {
  /** When true, the console driver is registered at boot. */
  readonly enableConsole: boolean;
  /** When true, the input (kbd + mouse) driver is registered at boot. */
  readonly enableInput: boolean;
  /** When true, the framebuffer driver is registered at boot. */
  readonly enableFramebuffer: boolean;
  /**
   * When true, the mock kernel runs in live-terminal mode:
   * it maintains its own scrollback + input buffer,
   * processes keystrokes one byte at a time, and
   * rasterizes + blits the full terminal snapshot on every
   * state change. Defaults to false; mutually exclusive
   * with the one-shot splash path.
   */
  readonly liveTerminal?: boolean;
  /**
   * Banner lines to pre-seed the live-terminal scrollback
   * with. Each string becomes a `banner`-kind line. Only
   * honoured when [`liveTerminal`] is true. Defaults to an
   * empty list.
   */
  readonly terminalBanner?: readonly string[];
}

/** Main-thread → kernel-worker. */
export type MainToKernel =
  | { readonly kind: "boot"; readonly config: BootConfig }
  | { readonly kind: "shutdown" }
  | ConsoleInputMessage
  | InputKbdMessage
  | InputMouseMessage;

/** Kernel-worker → main-thread. */
export type KernelToMain =
  | { readonly kind: "ready" }
  | { readonly kind: "panic"; readonly message: string }
  | ConsoleWriteMessage
  | FbSetModeMessage
  | FbBlitMessage;
