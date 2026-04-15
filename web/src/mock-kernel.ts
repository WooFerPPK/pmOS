// In-memory mock kernel used by the pre-WASM demo path.
//
// Implements the `Kernel` interface from `./kernel-worker.ts`
// with three possible rendering modes:
//
//   1. Default — every input line runs through the faux-shell
//      policy and the output bytes are sent back to the main
//      thread via the console driver. No framebuffer I/O.
//      This is the shape the very first slice needed: a
//      round-trippable `echo` channel over postMessage.
//
//   2. `emitSplashOnFirstInput` — on the first injected input
//      the mock emits a one-shot `SET_MODE + BLIT` through the
//      framebuffer driver with a rasterized banner snapshot.
//      The console path still delivers line output afterwards;
//      the blit is a single visible proof-of-fb in the boot
//      screen demo.
//
//   3. `liveTerminal` — the mock kernel maintains its own
//      scrollback list and current input buffer, processes
//      keystrokes one byte at a time (printable chars, `\n`
//      commit, `\x7f` backspace), runs committed lines through
//      the faux-shell, appends stdout lines to scrollback, and
//      rasterizes + blits the full terminal snapshot on every
//      state change. This is what the live browser demo uses
//      for its interactive REPL so pixels flow through the Fb
//      driver end-to-end.
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
import {
  rasterizeSnapshot,
  type RasterizerLine,
  type RasterizerSnapshot,
} from "./shared/rasterizer";

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
   *
   * Mutually exclusive with [`liveTerminal`] — if both are
   * set, `liveTerminal` wins because it subsumes the splash.
   */
  readonly emitSplashOnFirstInput?: boolean;
  /**
   * When true, the mock maintains a full terminal state
   * machine (scrollback + input buffer) and re-rasterizes +
   * blits the snapshot via the framebuffer driver on every
   * injected byte. This is the mode the interactive demo
   * uses so pixels land on the main-thread canvas via the
   * real Fb driver path instead of a separate main-thread
   * text painter. Defaults to false.
   *
   * Bytes are interpreted as a tiny keystroke protocol:
   *   * `\n` (0x0a) — commit the current input line: append
   *     "{prompt}{input}" to scrollback as an `input` line,
   *     run the committed text through [`MockEchoPolicy`],
   *     push the output as `output` (or `error`) lines, and
   *     reset the input buffer.
   *   * `\x7f` (0x7f, DEL) or `\x08` (0x08, BS) — pop the
   *     last character of the input buffer.
   *   * `0x20..=0x7e` — append as a printable ASCII char.
   *   * Any other byte — silently dropped.
   */
  readonly liveTerminal?: boolean;
  /**
   * Optional scrollback to pre-populate when `liveTerminal`
   * is true. Useful for the demo's boot banner: the main
   * thread seeds a "PMos 0.1.0 / ready / type help"
   * message so the first visible frame already has text.
   * Ignored when `liveTerminal` is false.
   */
  readonly initialScrollback?: readonly RasterizerLine[];
  /**
   * Prompt string used when committing an input line in
   * `liveTerminal` mode. Defaults to `"> "`. Ignored when
   * `liveTerminal` is false.
   */
  readonly prompt?: string;
  /**
   * Framebuffer width in pixels for splash / live-terminal
   * mode. Defaults to [`SPLASH_WIDTH`].
   */
  readonly fbWidth?: number;
  /**
   * Framebuffer height in pixels for splash / live-terminal
   * mode. Defaults to [`SPLASH_HEIGHT`].
   */
  readonly fbHeight?: number;
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
  private readonly liveTerminal: boolean;
  private readonly panicEmit: ((message: string) => void) | undefined;
  private splashEmitted = false;
  /** Per-devnum line buffers — default + splash modes only. */
  private readonly lineBuffers = new Map<number, number[]>();
  /** Live-terminal state. */
  private readonly scrollback: RasterizerLine[] = [];
  private liveInputBuffer = "";
  private readonly prompt: string;
  private readonly fbWidth: number;
  private readonly fbHeight: number;
  private fbModeEmitted = false;
  /**
   * Sticky "we tried to start the fb driver and it rejected
   * us" flag. Set to true after the first `SET_MODE` attempt
   * that returns `NotReady` so subsequent keystrokes don't
   * retry or attempt to blit.
   */
  private fbDisabled = false;

  constructor(options: MockKernelOptions) {
    this.policy = options.policy;
    this.emitSplashOnFirstInput = options.emitSplashOnFirstInput ?? false;
    this.liveTerminal = options.liveTerminal ?? false;
    this.panicEmit = options.panicEmit;
    this.prompt = options.prompt ?? "> ";
    this.fbWidth = options.fbWidth ?? SPLASH_WIDTH;
    this.fbHeight = options.fbHeight ?? SPLASH_HEIGHT;
    if (options.initialScrollback) {
      for (const line of options.initialScrollback) {
        this.scrollback.push({ text: line.text, kind: line.kind });
      }
    }
  }

  /**
   * Bind the scaffold after boot. Called by
   * `kernel-worker-entry.ts` immediately after
   * `bootKernelWorker` returns. Idempotent.
   */
  bindScaffold(scaffold: KernelWorker): void {
    this.scaffold = scaffold;
    // If live-terminal mode is active and scrollback is
    // pre-populated, emit the initial frame now so the
    // user sees the banner before typing anything. In the
    // default/splash modes this is a no-op.
    if (this.liveTerminal) {
      this.renderAndBlit();
    }
  }

  injectInput(devnum: number, bytes: Uint8Array): void {
    if (devnum !== DEV_CONSOLE_NODE) {
      // Non-console devnums are accepted but dropped in v1.
      // Mouse + keyboard event rings land in a later slice.
      return;
    }
    if (this.liveTerminal) {
      this.injectLiveInput(bytes);
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

  /**
   * Live-terminal per-byte keystroke processor. See
   * [`MockKernelOptions.liveTerminal`] for the wire protocol.
   */
  private injectLiveInput(bytes: Uint8Array): void {
    let changed = false;
    for (const b of bytes) {
      if (b === 0x0a /* \n */) {
        this.commitLiveInputLine();
        changed = true;
      } else if (b === 0x7f /* DEL */ || b === 0x08 /* BS */) {
        if (this.liveInputBuffer.length > 0) {
          this.liveInputBuffer = this.liveInputBuffer.slice(0, -1);
          changed = true;
        }
      } else if (b >= 0x20 && b <= 0x7e) {
        this.liveInputBuffer += String.fromCharCode(b);
        changed = true;
      }
      // Everything else (control bytes, bytes above 0x7e)
      // is silently dropped in v1.
    }
    if (changed) {
      this.renderAndBlit();
    }
  }

  /**
   * Commit the current live input line: append it to
   * scrollback as an `input` line, run it through the
   * policy, append the output as `output` / `error` lines,
   * and reset the input buffer.
   */
  private commitLiveInputLine(): void {
    const input = this.liveInputBuffer;
    this.liveInputBuffer = "";
    // Push the typed line with prompt prefix so scrollback
    // renders "> echo hi".
    this.scrollback.push({
      text: `${this.prompt}${input}`,
      kind: "input",
    });

    // Intercept the panic command — it's a kernel lifecycle
    // event, not console output.
    const inputBytesWithNewline = new TextEncoder().encode(`${input}\n`);
    if (this.tryHandlePanicCommand(inputBytesWithNewline)) {
      return;
    }

    const output = this.applyPolicy(inputBytesWithNewline);
    if (output.byteLength > 0) {
      // Still emit through the console driver so callers
      // that listen on `console:write` (e.g. the echo round-
      // trip check) see the output.
      this.scaffold?.callDriver(CONSOLE_DRIVER_ID, OP_WRITE_LINE, output);
      // Also append to the rasterizer scrollback — the
      // policy may produce multi-line output (e.g. `help`).
      const outputText = new TextDecoder().decode(output);
      // Strip a single trailing newline so `help\n` doesn't
      // produce a trailing blank line. Any other internal
      // blank lines are preserved as-is.
      const trimmed = outputText.endsWith("\n")
        ? outputText.slice(0, -1)
        : outputText;
      for (const outLine of trimmed.split("\n")) {
        this.scrollback.push({ text: outLine, kind: "output" });
      }
    }

    // Bound scrollback so it doesn't grow without bound —
    // 256 lines is ample for the demo at 320x240.
    while (this.scrollback.length > 256) {
      this.scrollback.shift();
    }
  }

  /**
   * Rasterize the current live-terminal snapshot and blit
   * it through the framebuffer driver. On the first call
   * also emits `OP_SET_MODE`. No-op if the scaffold isn't
   * bound, the fb driver has been marked disabled after a
   * prior `NotReady`, or the current SET_MODE attempt
   * fails.
   */
  private renderAndBlit(): void {
    const scaffold = this.scaffold;
    if (!scaffold) {
      return;
    }
    if (this.fbDisabled) {
      return;
    }
    if (!this.fbModeEmitted) {
      const setModeResult = scaffold.callDriver(
        FB_DRIVER_ID,
        FB_OP_SET_MODE,
        packFbSetMode(this.fbWidth, this.fbHeight),
      );
      this.fbModeEmitted = true;
      if (!setModeResult.ok) {
        // Disable the fb path entirely — retrying would
        // spam the driver on every keystroke.
        this.fbDisabled = true;
        return;
      }
    }
    const snapshot: RasterizerSnapshot = {
      lines: this.scrollback,
      inputBuffer: this.liveInputBuffer,
      prompt: this.prompt,
    };
    const pixels = rasterizeSnapshot(snapshot, this.fbWidth, this.fbHeight);
    scaffold.callDriver(
      FB_DRIVER_ID,
      FB_OP_BLIT,
      packFbBlit(this.fbWidth, this.fbHeight, pixels),
    );
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

  // ---- Test helpers -------------------------------------

  /**
   * Read-only view of the live-terminal scrollback. Returns
   * an empty array when `liveTerminal` is false. Exposed so
   * tests can assert on internal state without touching
   * private fields.
   */
  get liveScrollback(): ReadonlyArray<RasterizerLine> {
    return this.scrollback;
  }

  /** Read-only view of the live-terminal input buffer. */
  get liveInput(): string {
    return this.liveInputBuffer;
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
