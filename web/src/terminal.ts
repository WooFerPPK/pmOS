// Lightweight canvas terminal for the bootstrap demo.
//
// This is NOT a real TTY. It's the minimum amount of state
// needed to demonstrate the kernel-worker round-trip
// visually in the boot-screen demo:
//
//   * a scrollback buffer (bounded so older lines fall off)
//   * a current input buffer the user types into
//   * a key feeder that commits on Enter and edits on
//     Backspace
//   * an output sink that accepts raw bytes from the kernel
//     and splits them on `\n` into scrollback lines
//
// The rendering side lives in a separate paint function
// (`paintTerminal`) that takes a `Terminal` plus a
// `CanvasRenderingContext2D` and repaints. Splitting lets
// the state machine be unit-tested without a DOM.

/** One line in the scrollback buffer. */
export interface TerminalLine {
  /** Rendered text (already UTF-8 decoded). */
  readonly text: string;
  /**
   * Whether this line represents user input (shown with a
   * prompt prefix) or kernel output (plain).
   */
  readonly kind: "input" | "output";
}

export interface TerminalOptions {
  /**
   * Maximum number of scrollback lines. Older lines are
   * dropped when the buffer would exceed this. Must be > 0.
   */
  readonly maxLines: number;
  /**
   * Initial banner lines printed before any interaction.
   * Rendered as `output` lines.
   */
  readonly banner?: ReadonlyArray<string>;
}

/** Visible snapshot of the terminal for rendering. */
export interface TerminalSnapshot {
  readonly lines: ReadonlyArray<TerminalLine>;
  readonly inputBuffer: string;
}

export class Terminal {
  private readonly maxLines: number;
  private readonly lines: TerminalLine[] = [];
  private inputBuffer = "";
  /** Holds bytes from a partial output line (no trailing `\n` yet). */
  private pendingOutput = "";
  private readonly decoder = new TextDecoder();

  constructor(options: TerminalOptions) {
    if (options.maxLines <= 0) {
      throw new Error("Terminal: maxLines must be > 0");
    }
    this.maxLines = options.maxLines;
    if (options.banner) {
      for (const line of options.banner) {
        this.pushLine({ text: line, kind: "output" });
      }
    }
  }

  /** Current input buffer (what the user has typed but not yet committed). */
  get input(): string {
    return this.inputBuffer;
  }

  /** Current scrollback snapshot, bounded by `maxLines`. */
  snapshot(): TerminalSnapshot {
    return {
      lines: this.lines.slice(),
      inputBuffer: this.inputBuffer,
    };
  }

  /**
   * Feed a single keydown event. Returns the committed
   * line bytes when the user pressed Enter (the caller
   * forwards them to the kernel), or `null` otherwise.
   *
   * `key` is the DOM `KeyboardEvent.key` string:
   *   * A single printable char → append to the input
   *     buffer.
   *   * `"Enter"` → commit the buffer, push it to
   *     scrollback as an `input` line, clear the buffer,
   *     and return the line as bytes with a trailing `\n`.
   *   * `"Backspace"` → remove the last character from
   *     the buffer.
   *   * Anything else (Shift, Alt, arrow keys, ...) →
   *     ignored.
   */
  feedKey(key: string): Uint8Array | null {
    if (key === "Enter") {
      const line = this.inputBuffer;
      this.inputBuffer = "";
      this.pushLine({ text: `> ${line}`, kind: "input" });
      const out = new TextEncoder().encode(`${line}\n`);
      return out;
    }
    if (key === "Backspace") {
      if (this.inputBuffer.length > 0) {
        this.inputBuffer = this.inputBuffer.slice(0, -1);
      }
      return null;
    }
    if (key.length === 1 && this.isPrintable(key)) {
      this.inputBuffer += key;
    }
    return null;
  }

  /**
   * Append raw output bytes from the kernel. The bytes are
   * decoded as UTF-8 and split on `\n`. Bytes with no
   * trailing newline land in a pending buffer until the
   * next append completes them.
   */
  appendOutput(bytes: Uint8Array): void {
    const text = this.decoder.decode(bytes, { stream: true });
    this.pendingOutput += text;
    while (true) {
      const newlineIdx = this.pendingOutput.indexOf("\n");
      if (newlineIdx < 0) {
        break;
      }
      const line = this.pendingOutput.slice(0, newlineIdx);
      this.pendingOutput = this.pendingOutput.slice(newlineIdx + 1);
      this.pushLine({ text: line, kind: "output" });
    }
  }

  /** Wipe all scrollback + any pending output, reset the input buffer. */
  clear(): void {
    this.lines.length = 0;
    this.inputBuffer = "";
    this.pendingOutput = "";
  }

  /**
   * True iff the terminal has nothing in scrollback and no
   * active input. Used by tests and the bootstrap's first-
   * paint gate.
   */
  isEmpty(): boolean {
    return this.lines.length === 0 && this.inputBuffer.length === 0;
  }

  private pushLine(line: TerminalLine): void {
    this.lines.push(line);
    while (this.lines.length > this.maxLines) {
      this.lines.shift();
    }
  }

  private isPrintable(ch: string): boolean {
    // Rough ASCII+ printable check: codepoint >= 0x20 and
    // not DEL. Sufficient for the demo terminal.
    const code = ch.charCodeAt(0);
    return code >= 0x20 && code !== 0x7f;
  }
}

// ---- Canvas rendering --------------------------------------------

/** Colors used by the canvas terminal painter. */
export interface TerminalPalette {
  readonly bg: string;
  readonly fg: string;
  readonly prompt: string;
  readonly dim: string;
}

export const DEFAULT_PALETTE: TerminalPalette = {
  bg: "#0a0e14",
  fg: "#e6e6e6",
  prompt: "#7cb7ff",
  dim: "#808591",
};

export interface PaintOptions {
  readonly palette: TerminalPalette;
  /** Font size in CSS pixels (pre-DPR scale). */
  readonly fontSizePx: number;
  /** Device pixel ratio, passed through from the caller. */
  readonly dpr: number;
  /** Title rendered at the top of the terminal pane. */
  readonly title: string;
}

/**
 * Paint the terminal onto a `CanvasRenderingContext2D`. The
 * canvas is assumed to already be sized at `canvas.width x
 * canvas.height` (i.e. `dpr` has been applied). The painter
 * fills the full canvas with `palette.bg` and then draws
 * lines top-down with a two-line gap at the top for the
 * title.
 */
export function paintTerminal(
  ctx: CanvasRenderingContext2D,
  canvasWidth: number,
  canvasHeight: number,
  terminal: Terminal,
  options: PaintOptions,
): void {
  const { palette, fontSizePx, dpr, title } = options;
  const px = fontSizePx * dpr;
  const lineHeight = Math.floor(px * 1.4);
  const padX = Math.floor(32 * dpr);
  const padY = Math.floor(32 * dpr);
  const monoFont = `${px}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;

  ctx.fillStyle = palette.bg;
  ctx.fillRect(0, 0, canvasWidth, canvasHeight);

  // Title at the top.
  ctx.font = `bold ${Math.floor(px * 1.15)}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  ctx.fillStyle = palette.prompt;
  ctx.textBaseline = "top";
  ctx.fillText(title, padX, padY);

  ctx.font = `${Math.floor(px * 0.8)}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  ctx.fillStyle = palette.dim;
  ctx.fillText(
    "type a command and press Enter. 'help' for a list.",
    padX,
    padY + Math.floor(px * 1.4),
  );

  // Scrollback lines + current input.
  ctx.font = monoFont;
  let y = padY + Math.floor(px * 3.2);
  const { lines, inputBuffer } = terminal.snapshot();
  for (const line of lines) {
    if (y + lineHeight > canvasHeight - padY) {
      break;
    }
    ctx.fillStyle = line.kind === "input" ? palette.prompt : palette.fg;
    ctx.fillText(line.text, padX, y);
    y += lineHeight;
  }

  // Active input line with a prompt and a cursor.
  if (y + lineHeight <= canvasHeight - padY) {
    ctx.fillStyle = palette.prompt;
    ctx.fillText(`> ${inputBuffer}`, padX, y);
    // Cursor block.
    const promptWidth = ctx.measureText(`> ${inputBuffer}`).width;
    ctx.fillStyle = palette.fg;
    ctx.fillRect(padX + promptWidth, y, Math.floor(px * 0.6), lineHeight);
  }

  ctx.textBaseline = "alphabetic";
}
