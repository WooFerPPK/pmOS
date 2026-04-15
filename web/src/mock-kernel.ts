// In-memory mock kernel used by the pre-WASM demo path.
//
// Implements the `Kernel` interface from `./kernel-worker.ts`
// with a simple line-buffered echo loop: whenever bytes arrive
// via `injectInput(devnum, bytes)`, the mock accumulates them
// into a per-devnum buffer; when it sees a `\n`, it calls back
// into the scaffold to flush that line as output.
//
// Supported devnums in v1: `Devnum.Console`. Keyboard and mouse
// input is accepted and dropped for now — a future slice will
// route them to a faux window manager or similar.
//
// The mock exists so `web/src/kernel-worker-entry.ts` has
// SOMETHING to plug in before the Rust WASM kernel is built
// (T085+). Once the WASM kernel lands, the entry point will
// construct a WasmKernelBridge that forwards injectInput calls
// into the WASM instance's `kernel_inject_input` export and
// lets `Platform::driver_call` come back out as
// `callDriver(driverId, op, payload)` on the scaffold.

import type { Kernel, KernelWorker } from "./kernel-worker";
import {
  CONSOLE_DRIVER_ID,
  DEV_CONSOLE_NODE,
  OP_WRITE_LINE,
} from "./drivers/console";
import {
  FB_DRIVER_ID,
  OP_BLIT as FB_OP_BLIT,
  OP_SET_MODE as FB_OP_SET_MODE,
} from "./drivers/fb";
import { rasterizeSnapshot, type RasterizerSnapshot } from "./shared/rasterizer";

/**
 * Handler invoked when the mock kernel has a completed line
 * ready to emit. Separated out so tests can assert on the
 * output directly, without needing a full scaffold.
 */
export type MockEchoHandler = (devnum: number, line: Uint8Array) => void;

/** Policy for how the mock interprets input lines. */
export type MockEchoPolicy =
  /** Echo the raw input verbatim, including the trailing `\n`. */
  | { readonly kind: "echo" }
  /**
   * Handle a tiny command language: lines starting with
   * `echo ` are echoed with the prefix stripped; other lines
   * produce a `?\n` error marker. Mirrors the faux shell used
   * in the Rust-side principle_viii_headless_shell_gate.
   */
  | { readonly kind: "faux-shell" };

export interface MockKernelOptions {
  readonly policy: MockEchoPolicy;
  /**
   * When true, the mock emits a one-shot framebuffer splash
   * (SET_MODE + BLIT) the first time it receives console
   * input, provided the scaffold has a framebuffer driver
   * registered. If the scaffold reports NotReady on the
   * SET_MODE call, the splash is silently skipped and the
   * flag stays set — the mock never retries. Defaults to
   * false so existing tests are unaffected.
   */
  readonly emitSplashOnFirstInput?: boolean;
  /**
   * Optional sink for `panic` events. When set and the
   * mock's faux shell encounters a `panic <message>`
   * command, the message is forwarded here. Production
   * wires this to `messaging.postMessage({kind: "panic",
   * message})` so the bootstrap's panic overlay can show
   * it; tests pass a capturing closure.
   *
   * The kernel-side analogue is `Platform::halt(reason)`.
   * The mock doesn't actually halt — it just emits the
   * notice and lets the caller decide what to do — but
   * the demo's bootstrap reloads after 5s on a panic,
   * matching the real kernel halt path's contract.
   */
  readonly panicEmit?: (message: string) => void;
}

/**
 * The mock kernel. Construct it, bind a scaffold (so it can
 * emit output via `callDriver`), and wire it into the scaffold's
 * `Kernel` slot.
 */
export class MockKernel implements Kernel {
  private scaffold: KernelWorker | undefined;
  private readonly policy: MockEchoPolicy;
  private readonly emitSplashOnFirstInput: boolean;
  private readonly panicEmit: ((message: string) => void) | undefined;
  private splashEmitted = false;
  /** Per-devnum line buffers. Flushed on newline. */
  private readonly lineBuffers = new Map<number, number[]>();

  constructor(options: MockKernelOptions) {
    this.policy = options.policy;
    this.emitSplashOnFirstInput = options.emitSplashOnFirstInput ?? false;
    this.panicEmit = options.panicEmit;
  }

  /**
   * Bind the scaffold after boot. Called by
   * `kernel-worker-entry.ts` immediately after
   * `bootKernelWorker` returns. Idempotent.
   */
  bindScaffold(scaffold: KernelWorker): void {
    this.scaffold = scaffold;
  }

  injectInput(devnum: number, bytes: Uint8Array): void {
    if (devnum !== DEV_CONSOLE_NODE) {
      // Non-console devnums are accepted but dropped in v1.
      // Mouse + keyboard event rings land in a later slice.
      return;
    }
    if (this.emitSplashOnFirstInput) {
      this.maybeEmitSplash();
    }
    let buf = this.lineBuffers.get(devnum);
    if (!buf) {
      buf = [];
      this.lineBuffers.set(devnum, buf);
    }
    for (const b of bytes) {
      buf.push(b);
      if (b === 0x0a /* \n */) {
        this.flushLine(devnum, buf);
        buf = [];
        this.lineBuffers.set(devnum, buf);
      }
    }
  }

  private maybeEmitSplash(): void {
    if (this.splashEmitted) {
      return;
    }
    const scaffold = this.scaffold;
    if (!scaffold) {
      return;
    }
    // Set the flag before any callDriver calls so we never
    // retry the splash, even if SET_MODE returns NotReady.
    this.splashEmitted = true;
    const setModeResult = scaffold.callDriver(
      FB_DRIVER_ID,
      FB_OP_SET_MODE,
      packFbSetMode(SPLASH_WIDTH, SPLASH_HEIGHT),
    );
    if (!setModeResult.ok) {
      return;
    }
    // Build a terminal snapshot for the splash — a short
    // banner + prompt that exercises the TS-side rasterizer
    // end-to-end (bytes produced here land on the TS
    // `FramebufferDriver` and then on the main thread's
    // canvas as real text pixels).
    const snapshot: RasterizerSnapshot = {
      lines: [
        { text: "PMos 0.1.0-demo", kind: "banner" },
        { text: "kernel worker ready", kind: "banner" },
        { text: "type 'help' for commands", kind: "banner" },
        { text: "", kind: "output" },
      ],
      inputBuffer: "",
      prompt: "> ",
    };
    const pixels = rasterizeSnapshot(snapshot, SPLASH_WIDTH, SPLASH_HEIGHT);
    scaffold.callDriver(
      FB_DRIVER_ID,
      FB_OP_BLIT,
      packFbBlit(SPLASH_WIDTH, SPLASH_HEIGHT, pixels),
    );
  }

  private flushLine(devnum: number, lineBytes: number[]): void {
    const scaffold = this.scaffold;
    if (!scaffold) {
      return;
    }
    const line = Uint8Array.from(lineBytes);
    // The `panic <message>` command intercepts before the
    // policy runs because panic is a kernel-level
    // lifecycle event, not console output.
    if (this.tryHandlePanicCommand(line)) {
      return;
    }
    const output = this.applyPolicy(line);
    if (output.byteLength === 0) {
      return;
    }
    // v1 always echoes through the console driver because
    // console is the only devnum we understand.
    void devnum;
    scaffold.callDriver(CONSOLE_DRIVER_ID, OP_WRITE_LINE, output);
  }

  /**
   * If `line` is a `panic <message>` command, forward
   * the message to `panicEmit` (if wired) and return
   * true to short-circuit the rest of line handling.
   * Returns false otherwise.
   */
  private tryHandlePanicCommand(line: Uint8Array): boolean {
    // Strip trailing newline.
    let end = line.byteLength;
    if (end > 0 && line[end - 1] === 0x0a) {
      end -= 1;
    }
    const body = line.subarray(0, end);
    const text = new TextDecoder().decode(body);
    if (text === "panic") {
      this.panicEmit?.("kernel: panic command received with no message");
      return true;
    }
    if (text.startsWith("panic ")) {
      const message = text.slice("panic ".length);
      this.panicEmit?.(`kernel: ${message}`);
      return true;
    }
    return false;
  }

  private applyPolicy(line: Uint8Array): Uint8Array {
    switch (this.policy.kind) {
      case "echo":
        return line;
      case "faux-shell":
        return fauxShellTransform(line);
    }
  }
}

/**
 * Static help text printed by the `help` command. Extracted
 * so the demo UI can show the command list even before the
 * user types anything.
 */
export const FAUX_SHELL_HELP: ReadonlyArray<string> = [
  "commands:",
  "  help     — this list",
  "  echo X   — print X",
  "  date     — print build date",
  "  whoami   — print current user",
  "  uname    — print system banner",
  "  panic X  — trigger a kernel panic with message X",
];

/**
 * Parses a single newline-terminated input line the way the
 * Rust-side T077 gate's faux shell does, plus a handful of
 * extra commands that make the bootstrap demo visually
 * interesting. Exported for the unit tests;
 * `kernel-worker-entry.ts` does not call it directly.
 *
 * Commands (v1):
 *   * `echo X\n`   → `X\n`
 *   * `help\n`     → the lines from `FAUX_SHELL_HELP`
 *                    joined by `\n` with a trailing `\n`
 *   * `date\n`     → a fake fixed date
 *   * `whoami\n`   → `pmos`
 *   * `uname\n`    → `PMos 0.1.0-demo`
 *   * empty line   → no output
 *   * anything else → `?\n`  (matches the T077 gate contract)
 */
export function fauxShellTransform(line: Uint8Array): Uint8Array {
  // Strip trailing newline for parsing.
  let end = line.byteLength;
  if (end > 0 && line[end - 1] === 0x0a) {
    end -= 1;
  }
  const body = line.subarray(0, end);
  const bodyText = new TextDecoder().decode(body);

  if (bodyText.length === 0) {
    return new Uint8Array(0);
  }
  if (bodyText.startsWith("echo ")) {
    const rest = bodyText.slice("echo ".length);
    return new TextEncoder().encode(`${rest}\n`);
  }
  if (bodyText === "help") {
    return new TextEncoder().encode(`${FAUX_SHELL_HELP.join("\n")}\n`);
  }
  if (bodyText === "date") {
    return new TextEncoder().encode("2026-04-14\n");
  }
  if (bodyText === "whoami") {
    return new TextEncoder().encode("pmos\n");
  }
  if (bodyText === "uname") {
    return new TextEncoder().encode("PMos 0.1.0-demo\n");
  }
  return new TextEncoder().encode("?\n");
}

// ---- Framebuffer splash helpers --------------------------------

/** Splash canvas dimensions (kept small so the RGBA blob is small). */
export const SPLASH_WIDTH = 320;
export const SPLASH_HEIGHT = 240;

function packFbSetMode(width: number, height: number): Uint8Array {
  const out = new Uint8Array(8);
  const v = new DataView(out.buffer);
  v.setUint32(0, width, true);
  v.setUint32(4, height, true);
  return out;
}

function packFbBlit(
  width: number,
  height: number,
  pixels: Uint8Array,
): Uint8Array {
  const out = new Uint8Array(8 + pixels.byteLength);
  const v = new DataView(out.buffer);
  v.setUint32(0, width, true);
  v.setUint32(4, height, true);
  out.set(pixels, 8);
  return out;
}

/**
 * Generate a simple radial gradient that stands in for a
 * "PMos is alive" splash until the real compositor lands. The
 * center of the image is a brighter blue, falling off to a
 * dark navy at the edges.
 */
export function generateSplashPixels(
  width: number,
  height: number,
): Uint8Array {
  const out = new Uint8Array(width * height * 4);
  const cx = width / 2;
  const cy = height / 2;
  const maxDist = Math.sqrt(cx * cx + cy * cy);
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      const i = (y * width + x) * 4;
      const dx = x - cx;
      const dy = y - cy;
      const d = Math.sqrt(dx * dx + dy * dy) / maxDist;
      const t = Math.max(0, 1 - d);
      out[i] = Math.floor(20 + t * 60);
      out[i + 1] = Math.floor(35 + t * 130);
      out[i + 2] = Math.floor(80 + t * 175);
      out[i + 3] = 255;
    }
  }
  return out;
}
