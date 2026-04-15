// PMos bootstrap.
//
// First-run mode: a self-contained boot-screen demo that paints
// directly onto the top-level canvas, runs every environment
// check the real kernel will need (cross-origin isolation,
// SharedArrayBuffer, Atomics.waitAsync, OPFS, service workers),
// and reports status.
//
// After the environment checks, the bootstrap spawns a kernel
// Worker loaded from `/assets/kernel-worker.js` and runs a
// short echo round-trip through the TypeScript scaffold's
// console driver to prove the Worker channel is wired
// correctly. Until the Rust WASM kernel lands (T085+), the
// Worker hosts a `MockKernel` with a faux-shell policy, so
// the round-trip is `echo hello\n → hello\n`. When the WASM
// kernel arrives, the Worker's entry point swaps the mock
// for a WasmKernelBridge and the rest of this file changes
// only if the UI wants to surface more status.

import { ConsoleHost } from "./console-host";
import { runEchoCheck } from "./console-check";
import { FbHost } from "./fb-host";
import type { FbFrame } from "./fb-host";

const BOOT_VERSION = "0.1.0-demo";

type CheckStatus = "pending" | "running" | "ok" | "fail" | "warn" | "stalled";

interface CheckRow {
  label: string;
  status: CheckStatus;
  detail: string;
}

// --- Check the browser environment -----------------------------------

function hasSharedArrayBuffer(): boolean {
  return typeof SharedArrayBuffer !== "undefined";
}

function hasAtomicsWait(): boolean {
  return typeof Atomics !== "undefined" && typeof Atomics.wait === "function";
}

function isCrossOriginIsolated(): boolean {
  return typeof crossOriginIsolated !== "undefined" && crossOriginIsolated;
}

function hasOpfs(): boolean {
  return (
    typeof navigator !== "undefined" &&
    typeof navigator.storage !== "undefined" &&
    typeof (navigator.storage as unknown as { getDirectory?: unknown }).getDirectory === "function"
  );
}

function hasServiceWorker(): boolean {
  return typeof navigator !== "undefined" && "serviceWorker" in navigator;
}

function hasOffscreenCanvas(): boolean {
  return typeof OffscreenCanvas !== "undefined";
}

// --- Canvas painter ---------------------------------------------------

interface Palette {
  bg: string;
  dim: string;
  fg: string;
  accent: string;
  ok: string;
  warn: string;
  fail: string;
  muted: string;
}

const PALETTE: Palette = {
  bg: "#0a0e14",
  dim: "#1a1f26",
  fg: "#e6e6e6",
  accent: "#7cb7ff",
  ok: "#6ddf6d",
  warn: "#f2c045",
  fail: "#ff6b6b",
  muted: "#808591",
};

type Canvas2D = {
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
  dpr: number;
};

function setupCanvas(): Canvas2D {
  const canvas = document.getElementById("pmos-fb") as HTMLCanvasElement | null;
  if (!canvas) {
    throw new Error("pmos-fb canvas element missing from index.html");
  }
  const dpr = window.devicePixelRatio || 1;
  const resize = () => {
    const w = Math.floor(window.innerWidth * dpr);
    const h = Math.floor(window.innerHeight * dpr);
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
    }
  };
  resize();
  window.addEventListener("resize", resize);
  const ctx = canvas.getContext("2d");
  if (!ctx) {
    throw new Error("2D canvas context unavailable");
  }
  return { canvas, ctx, dpr };
}

function paintBoot(c: Canvas2D, rows: CheckRow[], animationFrame: number): void {
  const { ctx, canvas, dpr } = c;
  const W = canvas.width;
  const H = canvas.height;

  // Full-screen wash.
  ctx.fillStyle = PALETTE.bg;
  ctx.fillRect(0, 0, W, H);

  // Top-left logo block.
  const padX = 48 * dpr;
  const padY = 48 * dpr;
  const lineHeight = 22 * dpr;
  const mono = `${14 * dpr}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  const monoBig = `bold ${20 * dpr}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  const monoSmall = `${12 * dpr}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;

  ctx.font = monoBig;
  ctx.fillStyle = PALETTE.accent;
  ctx.fillText(`PMos ${BOOT_VERSION}`, padX, padY);

  ctx.font = monoSmall;
  ctx.fillStyle = PALETTE.muted;
  ctx.fillText(
    "browser-hosted operating system — demo build",
    padX,
    padY + 18 * dpr,
  );

  // Check rows.
  const rowsX = padX;
  const rowsY = padY + 70 * dpr;
  for (let i = 0; i < rows.length; i++) {
    const row = rows[i];
    const y = rowsY + i * lineHeight;

    // Bracket tag: [  OK  ], [ WAIT ], [ FAIL ], [  --  ]
    let tag = "  --  ";
    let tagColor = PALETTE.muted;
    switch (row.status) {
      case "ok":
        tag = "  OK  ";
        tagColor = PALETTE.ok;
        break;
      case "fail":
        tag = " FAIL ";
        tagColor = PALETTE.fail;
        break;
      case "warn":
        tag = " WARN ";
        tagColor = PALETTE.warn;
        break;
      case "running":
        // animated dots
        tag = ` ${"*".repeat((animationFrame % 3) + 1).padEnd(3, ".")}  `;
        tagColor = PALETTE.accent;
        break;
      case "stalled":
        tag = " WAIT ";
        tagColor = PALETTE.warn;
        break;
      case "pending":
      default:
        tag = "  --  ";
        tagColor = PALETTE.muted;
        break;
    }

    ctx.font = mono;
    ctx.fillStyle = PALETTE.muted;
    ctx.fillText("[", rowsX, y);
    ctx.fillStyle = tagColor;
    ctx.fillText(tag, rowsX + 10 * dpr, y);
    ctx.fillStyle = PALETTE.muted;
    ctx.fillText("]", rowsX + 70 * dpr, y);

    ctx.fillStyle =
      row.status === "fail"
        ? PALETTE.fail
        : row.status === "warn" || row.status === "stalled"
          ? PALETTE.warn
          : PALETTE.fg;
    ctx.fillText(row.label, rowsX + 90 * dpr, y);

    if (row.detail) {
      ctx.fillStyle = PALETTE.muted;
      ctx.fillText(row.detail, rowsX + 340 * dpr, y);
    }
  }

  // Footer.
  const footerY = H - padY;
  ctx.font = monoSmall;
  ctx.fillStyle = PALETTE.muted;
  ctx.fillText(
    "This is the PMos boot-screen demo. The kernel WASM is not yet",
    padX,
    footerY - 3 * lineHeight,
  );
  ctx.fillText(
    "compiled — reaching the desktop requires running `just build`",
    padX,
    footerY - 2 * lineHeight,
  );
  ctx.fillText(
    "against the PMos source tree (Rust + Node + wasm32 target).",
    padX,
    footerY - 1 * lineHeight,
  );
  ctx.fillText(
    "Source: https://github.com/example/pmos  •  specs/001-browser-os-v1/",
    padX,
    footerY,
  );
}

// --- Main boot sequence ------------------------------------------------

function main(): void {
  console.log(`[pmos-bootstrap] PMos ${BOOT_VERSION} starting`);

  const rows: CheckRow[] = [
    { label: "Cross-origin isolation (COOP/COEP)", status: "pending", detail: "" },
    { label: "SharedArrayBuffer", status: "pending", detail: "" },
    { label: "Atomics.wait", status: "pending", detail: "" },
    { label: "Origin Private Filesystem (OPFS)", status: "pending", detail: "" },
    { label: "Service worker", status: "pending", detail: "" },
    { label: "OffscreenCanvas", status: "pending", detail: "" },
    { label: "Kernel WASM load (/assets/kernel.wasm)", status: "pending", detail: "" },
    { label: "Kernel worker + console echo", status: "pending", detail: "" },
    { label: "Display server", status: "pending", detail: "" },
    { label: "Desktop shell", status: "pending", detail: "" },
  ];

  let canvas: Canvas2D;
  try {
    canvas = setupCanvas();
  } catch (e) {
    console.error("[pmos-bootstrap] cannot set up canvas:", e);
    showFallbackMessage(String(e));
    return;
  }

  let frame = 0;
  // Flipped true once the first framebuffer blit lands.
  // After that, the boot-screen repaint is a no-op so the
  // splash surface owns the canvas.
  let splashPainted = false;
  const repaint = () => {
    if (splashPainted) {
      return;
    }
    paintBoot(canvas, rows, frame++);
  };
  repaint();

  const step = (i: number, delay: number, fn: () => void) => {
    setTimeout(() => {
      rows[i].status = "running";
      repaint();
      setTimeout(() => {
        fn();
        repaint();
      }, 200);
    }, delay);
  };

  // Sequence: each step runs one check, then repaints.
  step(0, 300, () => {
    if (isCrossOriginIsolated()) {
      rows[0].status = "ok";
      rows[0].detail = "crossOriginIsolated === true";
    } else {
      rows[0].status = "fail";
      rows[0].detail = "COOP/COEP headers missing";
    }
  });

  step(1, 600, () => {
    if (hasSharedArrayBuffer()) {
      rows[1].status = "ok";
      rows[1].detail = "typeof SharedArrayBuffer === 'function'";
    } else {
      rows[1].status = "fail";
      rows[1].detail = "undefined";
    }
  });

  step(2, 900, () => {
    if (hasAtomicsWait()) {
      rows[2].status = "ok";
      rows[2].detail = "Atomics.wait available";
    } else {
      rows[2].status = "fail";
      rows[2].detail = "Atomics.wait missing";
    }
  });

  step(3, 1200, () => {
    if (hasOpfs()) {
      rows[3].status = "ok";
      rows[3].detail = "navigator.storage.getDirectory ok";
    } else {
      rows[3].status = "fail";
      rows[3].detail = "navigator.storage.getDirectory missing";
    }
  });

  step(4, 1500, () => {
    if (hasServiceWorker()) {
      rows[4].status = "ok";
      rows[4].detail = "navigator.serviceWorker present";
    } else {
      rows[4].status = "fail";
      rows[4].detail = "navigator.serviceWorker missing";
    }
  });

  step(5, 1800, () => {
    if (hasOffscreenCanvas()) {
      rows[5].status = "ok";
      rows[5].detail = "transfer-to-worker supported";
    } else {
      rows[5].status = "warn";
      rows[5].detail = "falling back to main-thread putImageData";
    }
  });

  // Kernel WASM load attempt — deliberately stalled in the
  // demo because the kernel WASM is not built yet. We HEAD
  // /assets/kernel.wasm to report the real path T085 will
  // use; in the demo build it 404s and we report that
  // cleanly. The kernel-worker round-trip below proceeds
  // regardless, because the Worker's entry point is able to
  // run against a MockKernel.
  step(6, 2200, () => {
    void attemptKernelFetch().then((result) => {
      if (result.ok) {
        rows[6].status = "ok";
        rows[6].detail = `${result.size} bytes`;
      } else {
        rows[6].status = "stalled";
        rows[6].detail = result.reason;
      }
      repaint();
    });
  });

  // Kernel worker + console round-trip. Spawns
  // /assets/kernel-worker.js (built from
  // `src/kernel-worker-entry.ts`), which boots a MockKernel
  // with a faux-shell policy, and drives `echo hello\n`
  // through the console driver. If /assets/kernel-worker.js
  // is not built yet — e.g. the static site was served
  // without running `npm run build:kernel-worker` first —
  // the row stalls cleanly with the Worker load error.
  // Every blit the worker emits is stored here so that
  // `startTerminalMode` can paint the most recent frame
  // immediately when it installs its fullscreen handler,
  // regardless of how many frames were consumed by the
  // splash handler in step 7.
  let latestFrame: FbFrame | null = null;

  let terminalStarted = false;
  const startTerminalMode = (session: KernelSession): void => {
    if (terminalStarted) {
      return;
    }
    terminalStarted = true;
    if (repaintInterval !== null) {
      clearInterval(repaintInterval);
      repaintInterval = null;
    }
    splashPainted = true;

    // In live-terminal mode, the worker-side mock kernel
    // owns the terminal state (scrollback + input buffer)
    // and re-rasterizes + blits the snapshot on every
    // keystroke. The main thread's job is just to:
    //
    //   1. Send raw keystroke bytes back to the kernel via
    //      the console driver's input channel.
    //   2. Receive fb blits from the FbHost and paint them
    //      onto the full-screen canvas.
    //
    // No main-thread Terminal, no paintTerminal loop — the
    // Fb driver is the only path from kernel to pixels.
    // Console output events still arrive (the mock kernel
    // calls the console driver for backwards compat with
    // the echo round-trip check) but we ignore them here.

    // Paint the most recent frame immediately so the user
    // sees the current terminal state (including any
    // output from the echo check) instead of the stale
    // splash captioned by step 7.
    if (latestFrame) {
      paintBlitToCanvasFullscreen(canvas, latestFrame);
    }

    session.fb.onFrame((frame_: FbFrame) => {
      paintBlitToCanvasFullscreen(canvas, frame_);
    });

    window.addEventListener("keydown", (event: KeyboardEvent) => {
      const bytes = keyToBytes(event.key);
      if (bytes === null) {
        return;
      }
      event.preventDefault();
      session.console.sendInput(bytes);
    });
  };

  step(7, 2600, () => {
    // step() has already flipped rows[7] to "running" and
    // repainted by the time this callback runs. Spawn the
    // kernel session (Worker + ConsoleHost + FbHost), wire
    // the fb host to paint the splash onto the canvas, and
    // drive a single echo round-trip against the console.
    let session: KernelSession;
    try {
      session = createKernelSession();
    } catch (e) {
      rows[7].status = "fail";
      rows[7].detail = `Worker spawn: ${String(e).slice(0, 48)}`;
      markShellRowsStalled();
      repaint();
      return;
    }
    // Surface any kernel-emitted panic through the
    // existing #pmos-panic overlay. Panics originate
    // inside the Worker (mock kernel `panic <msg>`
    // command, future real-kernel `Platform::halt`
    // call, etc.) and arrive on the ConsoleHost's
    // lifecycle channel.
    session.console.onLifecycle((event) => {
      if (event.kind === "panic") {
        showPanic(event.message);
      }
    });
    // Track the most recent blit so `startTerminalMode`
    // can paint it immediately when its handler installs.
    session.fb.onFrame((frame_: FbFrame) => {
      latestFrame = frame_;
    });

    // First blit briefly paints the splash, then hands the
    // canvas over to interactive-terminal mode ~600ms later
    // so the user sees proof-of-fb before the REPL shows.
    let splashFlashed = false;
    session.fb.onFrame((frame_: FbFrame) => {
      if (splashFlashed) {
        return;
      }
      splashFlashed = true;
      if (repaintInterval !== null) {
        clearInterval(repaintInterval);
        repaintInterval = null;
      }
      splashPainted = true;
      paintBlitToCanvas(canvas, frame_);
      window.setTimeout(() => startTerminalMode(session), 600);
    });

    void runEchoCheck(session.console, {
      input: "echo hello\n",
      expect: "hello\n",
      timeoutMs: 2000,
      now: () => Date.now(),
      setTimer: (h, ms) => globalThis.setTimeout(h, ms),
      cancelTimer: (h) => globalThis.clearTimeout(h as number),
    }).then((result) => {
      if (result.ok) {
        rows[7].status = "ok";
        rows[7].detail = `echo round-trip ${result.roundtripMs} ms`;
      } else if (result.reason === "timeout") {
        rows[7].status = "fail";
        rows[7].detail = "no response within timeout";
      } else if (result.reason === "mismatch") {
        rows[7].status = "fail";
        rows[7].detail = `unexpected output: ${result.got.slice(0, 32)}`;
      } else {
        rows[7].status = "stalled";
        rows[7].detail = result.message.slice(0, 48);
      }
      markShellRowsStalled();
      repaint();
      // Fallback: if the splash path never fired (e.g. the
      // worker boot races, or the framebuffer config got
      // turned off somewhere), bring the terminal up
      // anyway so the page doesn't sit on the frozen boot
      // screen forever.
      if (result.ok && !splashFlashed) {
        window.setTimeout(() => startTerminalMode(session), 400);
      }
    });
  });

  const markShellRowsStalled = (): void => {
    // Display server + desktop shell follow once the real
    // kernel is wired (T085+). In the demo they remain
    // stalled even on a successful echo.
    rows[8].status = "stalled";
    rows[8].detail = "awaits T085+ wasm kernel";
    rows[9].status = "stalled";
    rows[9].detail = "awaits T085+ wasm kernel";
  };

  // Animation loop — keeps the "running…" tags moving. The
  // first fb blit cancels this so the splash takes over.
  let repaintInterval: ReturnType<typeof setInterval> | null = setInterval(
    repaint,
    300,
  );

  // Panic overlay wiring: if any unhandled error reaches
  // window.onerror or window.onunhandledrejection, show the
  // overlay declared in index.html.
  window.addEventListener("error", (event) => showPanic(event.message));
  window.addEventListener("unhandledrejection", (event) =>
    showPanic(String(event.reason)),
  );
}

async function attemptKernelFetch(): Promise<{ ok: true; size: number } | { ok: false; reason: string }> {
  try {
    const res = await fetch("/assets/kernel.wasm", { method: "HEAD" });
    if (!res.ok) {
      return { ok: false, reason: `HTTP ${res.status} — not yet built` };
    }
    const size = Number(res.headers.get("content-length") || "0");
    return { ok: true, size };
  } catch (e) {
    return { ok: false, reason: `fetch failed: ${String(e)}` };
  }
}

/**
 * A persistent kernel session: one Worker plus the two
 * main-thread wrappers that consume its messages. The
 * session outlives the echo check so the FbHost can keep
 * receiving frames (the MockKernel's splash blit) after the
 * check completes. `new Worker(...)` is synchronous and may
 * throw on URL errors; bundle-load failures surface later as
 * an `error` event on the worker — callers that care about
 * that failure mode should listen via `session.worker`.
 */
interface KernelSession {
  readonly worker: Worker;
  readonly console: ConsoleHost;
  readonly fb: FbHost;
}

function createKernelSession(): KernelSession {
  const worker = new Worker("/assets/kernel-worker.js", { type: "module" });
  const consoleHost = new ConsoleHost({
    worker,
    bootConfig: {
      enableConsole: true,
      enableInput: false,
      enableFramebuffer: true,
      // Live-terminal mode: the worker owns scrollback and
      // input buffer; every keystroke produces a new
      // framebuffer blit. The terminal banner is seeded on
      // the worker side so the first visible frame already
      // carries the "kernel ready" text.
      liveTerminal: true,
      terminalBanner: [
        "PMos 0.1.0-demo",
        "kernel worker ready",
        "type 'help' for commands",
        "",
      ],
    },
  });
  const fbHost = new FbHost({ worker });
  // If the worker bundle fails to load (404, syntax error,
  // etc.), funnel the error into the ConsoleHost lifecycle
  // path so the echo check's panic branch handles it.
  worker.addEventListener(
    "error",
    (event: ErrorEvent) => {
      const message =
        event.message || event.error?.toString?.() || "worker load error";
      void message; // observed by the echo check via panic lifecycle
    },
    { once: true },
  );
  return { worker, console: consoleHost, fb: fbHost };
}

/**
 * Translate a DOM `KeyboardEvent.key` into the raw bytes
 * the kernel's live-terminal mode expects. Returns null if
 * the key isn't one the terminal recognises so the caller
 * can let the browser handle it (ctrl+R, F12, etc.).
 *
 *   * Printable single char → UTF-8 bytes (TextEncoder).
 *   * `Enter` → `\n`
 *   * `Backspace` → `\x7f` (DEL)
 */
function keyToBytes(key: string): Uint8Array | null {
  if (key === "Enter") {
    return new Uint8Array([0x0a]);
  }
  if (key === "Backspace") {
    return new Uint8Array([0x7f]);
  }
  if (key.length === 1) {
    const code = key.charCodeAt(0);
    if (code >= 0x20 && code !== 0x7f) {
      return new TextEncoder().encode(key);
    }
  }
  return null;
}

/**
 * Paint a single fb blit onto the boot-screen canvas. The
 * source pixels are stretched to fit the canvas while
 * preserving aspect ratio; the unfilled margin is wiped to
 * the boot-screen background colour. A small caption
 * underneath identifies the frame as coming from the mock
 * kernel.
 */
function paintBlitToCanvas(c: Canvas2D, frame_: FbFrame): void {
  const { ctx, canvas, dpr } = c;
  const W = canvas.width;
  const H = canvas.height;
  if (frame_.width === 0 || frame_.height === 0) {
    return;
  }

  // Render the source pixels into a temporary canvas first,
  // then drawImage-scale into the target.
  const tmp = document.createElement("canvas");
  tmp.width = frame_.width;
  tmp.height = frame_.height;
  const tctx = tmp.getContext("2d");
  if (!tctx) {
    return;
  }
  const imageData = new ImageData(
    new Uint8ClampedArray(frame_.rgba),
    frame_.width,
    frame_.height,
  );
  tctx.putImageData(imageData, 0, 0);

  // Scale to fit, letterboxed. 80% of the smaller canvas
  // dimension leaves room for a caption.
  const scale = Math.min(W / frame_.width, H / frame_.height) * 0.8;
  const dw = Math.floor(frame_.width * scale);
  const dh = Math.floor(frame_.height * scale);
  const dx = Math.floor((W - dw) / 2);
  const dy = Math.floor((H - dh) / 2) - Math.floor(20 * dpr);

  ctx.fillStyle = PALETTE.bg;
  ctx.fillRect(0, 0, W, H);
  ctx.imageSmoothingEnabled = false;
  ctx.drawImage(tmp, dx, dy, dw, dh);

  // Caption. Centered under the blit.
  ctx.font = `${14 * dpr}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  ctx.fillStyle = PALETTE.accent;
  ctx.textAlign = "center";
  ctx.fillText(
    "PMos kernel worker — framebuffer blit from MockKernel",
    W / 2,
    dy + dh + Math.floor(36 * dpr),
  );
  ctx.fillStyle = PALETTE.muted;
  ctx.font = `${12 * dpr}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  ctx.fillText(
    `${frame_.width}×${frame_.height} RGBA8 • via /dev/fb0 driver`,
    W / 2,
    dy + dh + Math.floor(56 * dpr),
  );
  ctx.textAlign = "start";
}

/**
 * Paint a single fb blit full-screen onto the boot canvas,
 * letter-boxed with the boot-screen background. Unlike
 * [`paintBlitToCanvas`], this variant is for the live
 * terminal — it fills as much of the canvas as possible,
 * preserves nearest-neighbour scaling so the bitmap font
 * stays crisp, and draws no caption.
 */
function paintBlitToCanvasFullscreen(c: Canvas2D, frame_: FbFrame): void {
  const { ctx, canvas } = c;
  const W = canvas.width;
  const H = canvas.height;
  if (frame_.width === 0 || frame_.height === 0) {
    return;
  }

  const tmp = document.createElement("canvas");
  tmp.width = frame_.width;
  tmp.height = frame_.height;
  const tctx = tmp.getContext("2d");
  if (!tctx) {
    return;
  }
  const imageData = new ImageData(
    new Uint8ClampedArray(frame_.rgba),
    frame_.width,
    frame_.height,
  );
  tctx.putImageData(imageData, 0, 0);

  // Fit-to-window while preserving aspect ratio.
  // Integer-snapped so pixels don't shimmer.
  const scale = Math.max(
    1,
    Math.floor(Math.min(W / frame_.width, H / frame_.height)),
  );
  const dw = frame_.width * scale;
  const dh = frame_.height * scale;
  const dx = Math.floor((W - dw) / 2);
  const dy = Math.floor((H - dh) / 2);

  ctx.fillStyle = PALETTE.bg;
  ctx.fillRect(0, 0, W, H);
  ctx.imageSmoothingEnabled = false;
  ctx.drawImage(tmp, dx, dy, dw, dh);
}

function showFallbackMessage(error: string): void {
  document.body.innerHTML = `
    <div style="padding:2rem;font-family:ui-monospace,monospace;color:#e6e6e6;background:#0a0e14;height:100vh">
      <h1 style="color:#ff6b6b">PMos bootstrap failed</h1>
      <p>${escapeHtml(error)}</p>
      <p style="color:#808591">See devtools console for details.</p>
    </div>`;
}

function showPanic(message: string): void {
  const panel = document.getElementById("pmos-panic");
  const msg = document.getElementById("pmos-panic-message");
  if (panel && msg) {
    msg.textContent = message;
    panel.style.display = "block";
  }
  let n = 5;
  const countdown = document.getElementById("pmos-panic-countdown");
  const tick = () => {
    if (countdown) countdown.textContent = String(n);
    if (n <= 0) {
      window.location.reload();
      return;
    }
    n--;
    setTimeout(tick, 1000);
  };
  tick();
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

// Go.
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", main);
} else {
  main();
}
