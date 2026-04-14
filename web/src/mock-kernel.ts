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
}

/**
 * The mock kernel. Construct it, bind a scaffold (so it can
 * emit output via `callDriver`), and wire it into the scaffold's
 * `Kernel` slot.
 */
export class MockKernel implements Kernel {
  private scaffold: KernelWorker | undefined;
  private readonly policy: MockEchoPolicy;
  /** Per-devnum line buffers. Flushed on newline. */
  private readonly lineBuffers = new Map<number, number[]>();

  constructor(options: MockKernelOptions) {
    this.policy = options.policy;
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

  private flushLine(devnum: number, lineBytes: number[]): void {
    const scaffold = this.scaffold;
    if (!scaffold) {
      return;
    }
    const line = Uint8Array.from(lineBytes);
    const output = this.applyPolicy(line);
    if (output.byteLength === 0) {
      return;
    }
    // v1 always echoes through the console driver because
    // console is the only devnum we understand.
    void devnum;
    scaffold.callDriver(CONSOLE_DRIVER_ID, OP_WRITE_LINE, output);
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
 * Parses a single newline-terminated input line the way the
 * Rust-side T077 gate's faux shell does. Exported for the
 * unit tests; `kernel-worker-entry.ts` does not call it
 * directly.
 */
export function fauxShellTransform(line: Uint8Array): Uint8Array {
  // Strip trailing newline for parsing, restore at end.
  let end = line.byteLength;
  if (end > 0 && line[end - 1] === 0x0a) {
    end -= 1;
  }
  const body = line.subarray(0, end);
  const prefix = new TextEncoder().encode("echo ");
  if (startsWith(body, prefix)) {
    const rest = body.subarray(prefix.byteLength);
    const out = new Uint8Array(rest.byteLength + 1);
    out.set(rest, 0);
    out[rest.byteLength] = 0x0a;
    return out;
  }
  // Empty lines flush nothing visible.
  if (body.byteLength === 0) {
    return new Uint8Array(0);
  }
  return new TextEncoder().encode("?\n");
}

function startsWith(haystack: Uint8Array, needle: Uint8Array): boolean {
  if (haystack.byteLength < needle.byteLength) {
    return false;
  }
  for (let i = 0; i < needle.byteLength; i += 1) {
    if (haystack[i] !== needle[i]) {
      return false;
    }
  }
  return true;
}
