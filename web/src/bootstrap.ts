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
import type { ConsoleLifecycleEvent } from "./console-host";
import { runEchoCheck } from "./console-check";
import { FbHost } from "./fb-host";
import type { FbFrame } from "./fb-host";
import { FbRenderer } from "./fb-renderer";
import { SAB_SIZE } from "./shared/sab-layout";
import {
  MouseButton,
  MouseButtonState,
  packMouseButton,
  packMouseMotion,
} from "./shared/input-proto";
import type {
  KernelToMain,
  MainToKernel,
  MainToUser,
  UserToMain,
} from "./shared/worker-proto";

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

/**
 * Probe for `Atomics.wait`. Required for the SAB-backed syscall
 * transport; absent in non-cross-origin-isolated contexts.
 */
export function hasAtomicsWait(): boolean {
  return typeof Atomics !== "undefined" && typeof Atomics.wait === "function";
}

/**
 * Probe for the `crossOriginIsolated` global. The deployment must
 * set COOP/COEP headers; without isolation, `SharedArrayBuffer`
 * isn't constructible and the SAB transport can't initialize.
 */
export function isCrossOriginIsolated(): boolean {
  return typeof crossOriginIsolated !== "undefined" && crossOriginIsolated;
}

/**
 * Probe for `navigator.storage.getDirectory` — the entry point
 * the OPFS block driver uses. Absent in private mode and older
 * browsers.
 */
export function hasOpfs(): boolean {
  return (
    typeof navigator !== "undefined" &&
    typeof navigator.storage !== "undefined" &&
    typeof (navigator.storage as unknown as { getDirectory?: unknown }).getDirectory === "function"
  );
}

/**
 * Subset of the global `navigator.serviceWorker` API used by
 * [`registerServiceWorker`]. Tests pass a stub that records
 * register calls and resolves with a fake registration handle.
 */
export interface ServiceWorkerContainerLike {
  register(
    scriptURL: string,
    options?: { scope?: string; type?: "classic" | "module" },
  ): Promise<unknown>;
}

/**
 * Register the bootstrap's service worker. Returns the
 * `Promise<unknown>` from `register()`, or `Promise.resolve(null)`
 * if the runtime has no service-worker container (private mode,
 * older browsers, jsdom). Defaults to the global
 * `navigator.serviceWorker`.
 */
export function registerServiceWorker(
  scriptURL = "/assets/sw.js",
  options: { scope?: string; type?: "classic" | "module" } = { type: "module" },
  container?: ServiceWorkerContainerLike,
): Promise<unknown> {
  const target =
    container ??
    (typeof navigator !== "undefined"
      ? (navigator.serviceWorker as ServiceWorkerContainerLike | undefined)
      : undefined);
  if (target === undefined) {
    return Promise.resolve(null);
  }
  return target.register(scriptURL, options);
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

  // Boot-to-desktop is the default boot path: bare URL spawns
  // `/bin/init-desktop`, which spawns the real display-server +
  // shell binaries and paints the wallpaper + taskbar. Hashes
  // select the alternative paths used by tests + the preview
  // demo:
  //   * `#real-kernel` → `/bin/init` (the legacy four-pid demo
  //     tree: init + hello-std + display-server +
  //     display-client-demo × 2 + IPC round-trip + SIGTERM
  //     teardown). The `real-kernel.spec.ts` Playwright spec
  //     uses this hash explicitly.
  //   * `#input-echo` → `/bin/hello_input_echo` (no_std cdylib
  //     that polls `/dev/input_kbd` + echoes to stdout — the
  //     browser-side proof of the input round-trip).
  //   * `#mock-kernel` → fall through to the legacy MockKernel
  //     boot-screen check rows below (faux shell + live
  //     terminal + capability checks).
  //   * `#boot-to-desktop` → explicit alias for the default.
  if (!window.location.hash.includes("mock-kernel")) {
    const hash = window.location.hash;
    let bootBinary: string;
    if (hash.includes("input-echo")) {
      bootBinary = "/bin/hello_input_echo";
    } else if (hash.includes("real-kernel")) {
      bootBinary = "/bin/init";
    } else {
      bootBinary = "/bin/init-desktop";
    }
    runRealKernelMode(bootBinary);
    return;
  }

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

    // Pointer events → `/dev/input/mouse` via the input
    // driver. The worker-side scaffold auto-registers the
    // driver when `enableInput` is true, and the mock
    // kernel's `injectInput` for this devnum decodes the
    // packed bytes, updates its pointer state, and (in
    // live-terminal mode) re-blits — painting a visible
    // cursor sprite at the new position.
    const sendMouse = (msg: MainToKernel) => session.worker.postMessage(msg);
    const canvasEl = canvas.canvas;
    // Convert a DOM PointerEvent into framebuffer
    // coordinates by inverting the letterbox transform
    // `paintBlitToCanvasFullscreen` uses to draw the
    // latest blit. Returns `null` if no blit has
    // arrived yet (can't know the fb size) or if the
    // event falls outside the blit's painted rectangle
    // (pointer is over the letterbox margin).
    const toFbCoords = (event: PointerEvent): [number, number] | null => {
      const frame = latestFrame;
      if (!frame) {
        return null;
      }
      const rect = canvasEl.getBoundingClientRect();
      const canvasCx = (event.clientX - rect.left) * canvas.dpr;
      const canvasCy = (event.clientY - rect.top) * canvas.dpr;
      const fbW = frame.width;
      const fbH = frame.height;
      const canvasW = canvas.canvas.width;
      const canvasH = canvas.canvas.height;
      const scale = Math.max(
        1,
        Math.floor(Math.min(canvasW / fbW, canvasH / fbH)),
      );
      const dw = fbW * scale;
      const dh = fbH * scale;
      const dx = Math.floor((canvasW - dw) / 2);
      const dy = Math.floor((canvasH - dh) / 2);
      const fbX = Math.floor((canvasCx - dx) / scale);
      const fbY = Math.floor((canvasCy - dy) / scale);
      if (fbX < 0 || fbX >= fbW || fbY < 0 || fbY >= fbH) {
        return null;
      }
      return [fbX, fbY];
    };
    canvasEl.addEventListener("pointermove", (event: PointerEvent) => {
      const coords = toFbCoords(event);
      if (!coords) return;
      const [x, y] = coords;
      sendMouse({ kind: "input:mouse", bytes: packMouseMotion(x, y) });
    });
    canvasEl.addEventListener("pointerdown", (event: PointerEvent) => {
      const coords = toFbCoords(event);
      if (!coords) return;
      const [x, y] = coords;
      const button = domButtonToProtoButton(event.button);
      sendMouse({
        kind: "input:mouse",
        bytes: packMouseButton(x, y, button, MouseButtonState.Pressed),
      });
    });
    canvasEl.addEventListener("pointerup", (event: PointerEvent) => {
      const coords = toFbCoords(event);
      if (!coords) return;
      const [x, y] = coords;
      const button = domButtonToProtoButton(event.button);
      sendMouse({
        kind: "input:mouse",
        bytes: packMouseButton(x, y, button, MouseButtonState.Released),
      });
    });
  };

  /**
   * Translate a DOM `PointerEvent.button` value to the
   * `MouseButton` wire constants the kernel expects. DOM
   * numbering is 0=primary, 1=middle, 2=secondary; the
   * protocol uses 1=left, 2=right, 3=middle. Unknown
   * buttons pass through as their DOM numeric value.
   */
  function domButtonToProtoButton(domButton: number): number {
    switch (domButton) {
      case 0:
        return MouseButton.Left;
      case 1:
        return MouseButton.Middle;
      case 2:
        return MouseButton.Right;
      default:
        return domButton;
    }
  }

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
      // Pointer/keyboard events flow through the scaffold's
      // input driver into `/dev/input/{kbd,mouse}` rings.
      // This slice lights up the pointer path; keyboard
      // still rides the console:input bytes path for the
      // live terminal, and will migrate in a follow-up.
      enableInput: true,
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

/**
 * Real-kernel boot path: spawns the kernel Worker with
 * `useRealKernel: true` + the caller-chosen `bootBinary`, then
 * forwards every byte the kernel flushes from `/dev/console` to
 * the page console with a `[real-kernel]` prefix. Playwright
 * scrapes the page console to assert on the boot binary's
 * output; once a real terminal-mode wiring lands the prefix
 * goes away and the bytes flow into the visible terminal
 * surface instead.
 *
 * `/bin/init` is a real Rust `std` binary (`crates/init/`) that
 * announces itself, spawns `/bin/hello-std` via
 * `pmos_ext.proc_spawn`, and exits — the drain loop picks up
 * hello-std next and runs it to completion. Both binaries'
 * output reach the page console in order.
 *
 * `/bin/hello_input_echo` is an alternative boot binary wired
 * via `#input-echo`: it polls `/dev/input_kbd` in an EAGAIN
 * loop, so pressing a key on the page posts an `input:kbd`
 * message to the kernel Worker (via the keydown listener this
 * function installs), the kernel writes the bytes into the kbd
 * ring, and hello_input_echo's next `fd_read` iteration echoes
 * them to `/dev/console` and exits.
 */
function runRealKernelMode(bootBinary: string): void {
  console.log(
    `[pmos-bootstrap] real-kernel mode enabled via URL (bootBinary=${bootBinary})`,
  );
  // Boot paths that paint to /dev/fb0 (init-desktop's shell
  // wallpaper, eventually any GUI app boot) should keep the
  // canvas visible. Text-only boots (hello_input_echo, the
  // legacy demo flow) get the console-pre overlay.
  const isGuiBoot = bootBinary === "/bin/init-desktop";
  const consoleEl = mountRealKernelConsole(isGuiBoot);
  const worker = new Worker("/assets/kernel-worker.js", { type: "module" });

  // GUI boot: wire the FbHost → FbRenderer pair so `fb:set-mode` /
  // `fb:blit` messages from the kernel Worker actually paint pixels
  // onto the visible canvas. Without this, the display-server's
  // wallpaper paint reaches the kernel host but stops there — the
  // canvas stays black. Text-only boots skip this entirely (no
  // paint pipeline to wire).
  let guiFbMode: { width: number; height: number } | null = null;
  if (isGuiBoot) {
    const canvas = document.getElementById("pmos-fb");
    if (canvas instanceof HTMLCanvasElement) {
      const fbHost = new FbHost({ worker });
      const renderer = new FbRenderer({ canvas });
      fbHost.onModeChange((mode) => {
        renderer.setMode(mode);
        guiFbMode = mode;
      });
      fbHost.onFrame((frame) => {
        renderer.paintFrame(frame);
      });

      // Pointer + keyboard input for the GUI desktop. Each
      // DOM event is converted to framebuffer-space
      // coordinates (the canvas's intrinsic buffer is sized
      // to the framebuffer; CSS stretches it across the
      // viewport, so we invert that mapping with the
      // canvas's bounding rect) and posted to the kernel
      // worker as the existing `input:mouse` /  `input:kbd`
      // packed envelopes. The display server's
      // `drain_input_events` path then injects each event
      // into `Server::inject_pointer_*` and the click
      // ultimately reaches the shell's pointer object.
      const toFbCoords = (
        event: PointerEvent,
      ): [number, number] | null => {
        if (!guiFbMode) return null;
        const rect = canvas.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return null;
        const fx = ((event.clientX - rect.left) / rect.width) * guiFbMode.width;
        const fy = ((event.clientY - rect.top) / rect.height) * guiFbMode.height;
        const x = Math.max(0, Math.min(guiFbMode.width - 1, Math.floor(fx)));
        const y = Math.max(0, Math.min(guiFbMode.height - 1, Math.floor(fy)));
        return [x, y];
      };
      const domButtonToProtoButton = (domButton: number): number => {
        switch (domButton) {
          case 0: return MouseButton.Left;
          case 1: return MouseButton.Middle;
          case 2: return MouseButton.Right;
          default: return domButton;
        }
      };
      canvas.addEventListener("pointermove", (event) => {
        const coords = toFbCoords(event);
        if (!coords) return;
        const [x, y] = coords;
        worker.postMessage({
          kind: "input:mouse",
          bytes: packMouseMotion(x, y),
        } satisfies MainToKernel);
      });
      canvas.addEventListener("pointerdown", (event) => {
        const coords = toFbCoords(event);
        if (!coords) return;
        const [x, y] = coords;
        const button = domButtonToProtoButton(event.button);
        worker.postMessage({
          kind: "input:mouse",
          bytes: packMouseButton(x, y, button, MouseButtonState.Pressed),
        } satisfies MainToKernel);
      });
      canvas.addEventListener("pointerup", (event) => {
        const coords = toFbCoords(event);
        if (!coords) return;
        const [x, y] = coords;
        const button = domButtonToProtoButton(event.button);
        worker.postMessage({
          kind: "input:mouse",
          bytes: packMouseButton(x, y, button, MouseButtonState.Released),
        } satisfies MainToKernel);
      });
      // Suppress the browser's default right-click menu so
      // a future shell context-menu has somewhere to land.
      canvas.addEventListener("contextmenu", (event) => {
        event.preventDefault();
      });
    }
  }

  // T234: track the kernel wake-slot buffer as it arrives (kernel
  // posts `kernel:wake-slot` exactly once after `KernelWasmHost.
  // create`, before the first `proc:spawn`). The spawn router reads
  // it lazily via `getKernelWakeSlot` so each new user Worker's
  // boot message can include the wake slot for the production wake
  // protocol.
  let kernelWakeSlot: ArrayBufferLike | null = null;
  // Test affordance: peak count of live user Workers at any moment
  // during boot. The Playwright `real-kernel.spec.ts` reads this off
  // `<body>`'s `data-pmos-peak-live-workers` attribute to assert
  // that init + hello-std actually ran in different Workers (>= 2).
  let peakLiveWorkers = 0;

  const router = createSpawnRouter({
    kernelWorker: worker,
    workerFactory: () =>
      new Worker("/assets/user-worker.js", { type: "module" }),
    allocSab: (): ArrayBufferLike => {
      try {
        return new SharedArrayBuffer(SAB_SIZE);
      } catch {
        return new ArrayBuffer(SAB_SIZE);
      }
    },
    getKernelWakeSlot: () => kernelWakeSlot,
  });

  // Listen for kernel-Worker messages alongside ConsoleHost. ConsoleHost
  // also installs an `addEventListener("message", ...)` on the same
  // worker, but Worker message events fan out to every listener; they
  // don't compete. This handler skims off the multi-process control
  // messages (`kernel:wake-slot`, `proc:spawn`) and lets ConsoleHost
  // handle the rest (`ready`, `console:write`, `panic`).
  worker.addEventListener("message", (ev) => {
    const msg = ev.data as KernelToMain;
    if (msg.kind === "kernel:wake-slot") {
      kernelWakeSlot = msg.sab;
      // Tests can wait for this attribute before asserting on
      // anything wake-slot-dependent; production code doesn't
      // observe it.
      document.body.dataset["pmosWakeSlotReady"] = "1";
      return;
    }
    router.handleKernelMessage(msg);
    // Track peak after the router applied any state change.
    if (router.liveWorkers.size > peakLiveWorkers) {
      peakLiveWorkers = router.liveWorkers.size;
      document.body.dataset["pmosPeakLiveWorkers"] = String(peakLiveWorkers);
    }
  });

  const consoleHost = new ConsoleHost({
    worker,
    bootConfig: {
      enableConsole: true,
      // Register the InputDriver so `input:kbd` / `input:mouse`
      // messages posted by the keydown listener below route through
      // the scaffold to `KernelWasmHost.injectInput`. The driver is
      // shared with the preview-slice MockKernel path; real-kernel
      // mode wires it to the real kernel's `kernel_inject_input_kbd`
      // export.
      enableInput: true,
      enableFramebuffer: false,
      useRealKernel: true,
      bootBinary,
    },
  });
  consoleHost.onOutput((bytes: Uint8Array) => {
    const text = new TextDecoder().decode(bytes);
    consoleEl.textContent += text;
    console.log(`[real-kernel] ${text.replace(/\n$/, "")}`);
  });
  consoleHost.onLifecycle((event: ConsoleLifecycleEvent) => {
    if (event.kind === "ready") {
      console.log("[pmos-bootstrap] real kernel ready");
    } else if (event.kind === "panic") {
      console.error(`[pmos-bootstrap] real kernel panic: ${event.message}`);
      consoleEl.textContent += `\n[panic] ${event.message}\n`;
    }
  });

  // Keyboard input: a `keydown` on `document` so the handler fires
  // regardless of which element has focus (real-kernel mode hides the
  // canvas and the DOM console pre-element is non-interactive by
  // default). Each keydown is converted to the raw bytes the kernel's
  // input ring expects and posted as an `input:kbd` message on the
  // kernel Worker channel. The kernel worker's scaffold routes these
  // through `InputDriver.onHostMessage` into
  // `KernelWasmHost.injectInput(Devnum.InputKbd, bytes)`, which lands
  // the bytes in `/dev/input_kbd`. A user process polling `fd_read` on
  // the node (e.g. `/bin/hello_input_echo` under `#input-echo`) picks
  // them up on its next iteration.
  document.addEventListener("keydown", (event: KeyboardEvent) => {
    // Ignore modifier-heavy chords (Ctrl+R, Cmd+S, etc.) so the
    // browser shortcut path still works. F-keys and similar non-
    // printable keys fall through `keyToBytes` as `null` too.
    if (event.ctrlKey || event.metaKey || event.altKey) {
      return;
    }
    const bytes = keyToBytes(event.key);
    if (bytes === null) {
      return;
    }
    event.preventDefault();
    const msg: MainToKernel = { kind: "input:kbd", bytes };
    worker.postMessage(msg);
  });

  // T137: pagehide-driven persistence sync. The kernel's per-process
  // proc_exit hook covers normal exits; this covers the "user closes
  // the tab while a process is mid-flight" case so OPFS-backed
  // mutations are not lost.
  installPagehideSync(worker);
  // T137 follow-up: periodic sync for long-running tabs. 60s default
  // trades I/O frequency against window-of-loss size — a hard browser
  // crash loses at most one minute of writes.
  installPeriodicSync(worker);
}

/**
 * Create (and return) the `<pre id="pmos-real-console">` element
 * the real-kernel mode renders boot output into.
 *
 * `gui` selects between two layouts:
 *   * `gui = false` (legacy demo + input-echo): canvas hidden,
 *     console pre fills the viewport with a dark-monospace look.
 *   * `gui = true` (init-desktop): canvas visible at 100vw×100vh,
 *     console pre overlays as a small transparent log at the
 *     top-right so boot trace stays observable without obscuring
 *     the desktop.
 */
function mountRealKernelConsole(gui: boolean = false): HTMLPreElement {
  const existing = document.getElementById("pmos-real-console");
  if (existing instanceof HTMLPreElement) {
    return existing;
  }
  const canvas = document.getElementById("pmos-fb");
  if (canvas instanceof HTMLElement && !gui) {
    canvas.style.display = "none";
  }
  const pre = document.createElement("pre");
  pre.id = "pmos-real-console";
  if (gui) {
    pre.style.cssText = [
      "position: fixed",
      "top: 0.5rem",
      "right: 0.5rem",
      "max-width: 30vw",
      "max-height: 40vh",
      "margin: 0",
      "padding: 0.5rem 0.75rem",
      "font-family: ui-monospace, \"SF Mono\", Menlo, Consolas, monospace",
      "font-size: 11px",
      "line-height: 1.3",
      "color: #e6e6e6",
      "background: rgba(10, 14, 20, 0.6)",
      "border-radius: 4px",
      "white-space: pre-wrap",
      "overflow: auto",
      "pointer-events: none",
      "z-index: 100",
    ].join("; ");
  } else {
    pre.style.cssText = [
      "margin: 0",
      "padding: 1.5rem",
      "font-family: ui-monospace, \"SF Mono\", Menlo, Consolas, monospace",
      "font-size: 14px",
      "line-height: 1.5",
      "color: #e6e6e6",
      "background: #0a0e14",
      "min-height: 100vh",
      "white-space: pre-wrap",
      "overflow-wrap: anywhere",
    ].join("; ");
  }
  document.body.appendChild(pre);
  return pre;
}

function showFallbackMessage(error: string): void {
  document.body.innerHTML = `
    <div style="padding:2rem;font-family:ui-monospace,monospace;color:#e6e6e6;background:#0a0e14;height:100vh">
      <h1 style="color:#ff6b6b">PMos bootstrap failed</h1>
      <p>${escapeHtml(error)}</p>
      <p style="color:#808591">See devtools console for details.</p>
    </div>`;
}

export function showPanic(message: string): void {
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

// ---- Main-thread spawn router (T232 / M1.3) ---------------------
//
// When the kernel Worker posts `proc:spawn`, main allocates a
// per-pid SAB, spawns a dedicated user Worker from
// `/assets/user-worker.js`, posts a `boot {pid, sab, wasmBytes}` to
// the new Worker, and posts `proc:sab` back to the kernel so the
// kernel's dispatch loop adds the pid to its pidMap. When the user
// Worker posts `exited` (or fires an `error` event), main posts
// `proc:exited` to the kernel and terminates the Worker.
//
// T232 lands the router as test-callable plumbing only. The
// production `runRealKernelMode()` path above is untouched and still
// uses the kernel's in-process drain. T234 swaps the bootstrap
// production wiring onto this router and drops the in-process drain
// for spawned children. The split keeps `real-kernel.spec.ts` green
// across the M1 sub-slices instead of waiting on the full Worker
// path to land before any tests can run.

/**
 * Minimal user-Worker interface — the subset of the dedicated
 * `Worker` API the spawn router uses. A real `Worker` satisfies this
 * structurally; tests pass a fake that captures `postMessage` and
 * exposes injection hooks for `message` / `error` events.
 */
export interface UserWorkerLike {
  postMessage(msg: MainToUser): void;
  addEventListener(
    type: "message",
    listener: (ev: { data: UserToMain }) => void,
  ): void;
  addEventListener(
    type: "error",
    listener: (ev: { message?: string }) => void,
  ): void;
  removeEventListener(
    type: "message",
    listener: (ev: { data: UserToMain }) => void,
  ): void;
  removeEventListener(
    type: "error",
    listener: (ev: { message?: string }) => void,
  ): void;
  terminate(): void;
}

/** Dependencies the spawn router takes at construction. */
export interface SpawnRouterDeps {
  /**
   * Channel back to the kernel Worker. The router posts `proc:sab`
   * and `proc:exited` here. In production this is the same `Worker`
   * the bootstrap created for `/assets/kernel-worker.js`.
   */
  readonly kernelWorker: { postMessage(msg: MainToKernel): void };
  /**
   * Construct a fresh user Worker. In production:
   * `() => new Worker("/assets/user-worker.js", {type:"module"})`.
   * Tests inject a fake.
   */
  readonly workerFactory: () => UserWorkerLike;
  /**
   * Allocate a fresh per-pid SAB. In production:
   * `() => new SharedArrayBuffer(SAB_SIZE)`. Tests inject a plain
   * `ArrayBuffer` because vitest under node has no SAB; the layout
   * math the router cares about is byte-identical against either.
   */
  readonly allocSab: () => ArrayBufferLike;
  /**
   * T234: production wake-slot accessor. Returns the kernel's
   * 32-byte wake-slot buffer (allocated by `KernelWasmHost.create`
   * and forwarded to main via the `kernel:wake-slot` message), or
   * `null` if the wake slot hasn't arrived yet. When non-null, the
   * router includes the buffer in the `boot` message it posts to
   * each freshly-spawned user Worker; the user-worker entry
   * constructs an `Int32Array` view and hands it to `SabBackend`
   * for the production wake protocol. When the option is unset
   * (T232 vitest harness — the spawn-router unit tests don't
   * exercise the wake protocol), the boot message omits
   * `kernelWakeSlot` and `SabBackend` falls back to its synchronous
   * `serviceHook` path.
   */
  readonly getKernelWakeSlot?: () => ArrayBufferLike | null;
}

/** A live entry in the router's pid → user-Worker map. */
export interface SpawnedEntry {
  readonly worker: UserWorkerLike;
  readonly sab: ArrayBufferLike;
}

/**
 * Subset of the `EventTarget` surface needed by
 * [`installPagehideSync`] — production code passes `window`, tests
 * pass a stub that captures the listener and fires it on demand.
 */
export interface PagehideTarget {
  addEventListener(type: "pagehide", listener: () => void): void;
}

/**
 * Post a best-effort `sync:request` to the kernel Worker on the
 * `pagehide` lifecycle event so OPFS-backed mutations survive the
 * user closing the tab. Mirrors the per-process `proc_exit` sync
 * hook: `proc_exit` flushes the exiting pid's dirty mounts;
 * `pagehide` flushes everything still dirty.
 *
 * `pagehide` is preferred over `beforeunload` (which has UX
 * implications) and `unload` (which is unreliable in modern
 * browsers — bfcache short-circuits it). Returns the listener so
 * tests can fire it synchronously.
 */
export function installPagehideSync(
  kernelWorker: { postMessage(msg: MainToKernel): void },
  target: PagehideTarget = window,
): () => void {
  const listener = (): void => {
    kernelWorker.postMessage({ kind: "sync:request" });
  };
  target.addEventListener("pagehide", listener);
  return listener;
}

/**
 * Subset of the host timer surface needed by
 * [`installPeriodicSync`] — production code passes the global
 * `setInterval` / `clearInterval`, tests pass a fake-timer harness
 * that captures the callback + interval and fires it on demand.
 */
export interface IntervalScheduler {
  setInterval(handler: () => void, ms: number): number;
  clearInterval(handle: number): void;
}

/**
 * Periodic companion to [`installPagehideSync`] — fires a
 * `sync:request` to the kernel Worker every `intervalMs`
 * milliseconds so a long-running tab doesn't accumulate hours of
 * un-flushed writes between the user's last interaction and the
 * eventual pagehide.
 *
 * `pagehide` is the right primitive for "tab is going away";
 * periodic sync is the right primitive for "tab has been open for
 * hours". They're complementary — both wire to the same
 * `sync:request` message kind, so the kernel-side handler is
 * unchanged.
 *
 * Returns a `dispose()` function that cancels the interval. The
 * default interval is 60_000ms (one minute) which trades I/O
 * frequency against window-of-loss size.
 */
export function installPeriodicSync(
  kernelWorker: { postMessage(msg: MainToKernel): void },
  intervalMs: number = 60_000,
  scheduler: IntervalScheduler = {
    setInterval: (h, ms) => globalThis.setInterval(h, ms) as unknown as number,
    clearInterval: (h) => globalThis.clearInterval(h),
  },
): () => void {
  if (intervalMs <= 0 || !Number.isFinite(intervalMs)) {
    throw new Error(
      `installPeriodicSync: intervalMs must be a positive finite number, got ${intervalMs}`,
    );
  }
  const handle = scheduler.setInterval(() => {
    kernelWorker.postMessage({ kind: "sync:request" });
  }, intervalMs);
  return () => scheduler.clearInterval(handle);
}

/** The handle [`createSpawnRouter`] returns. */
export interface SpawnRouter {
  /** Forward one `KernelToMain` message into the router. Non-spawn
   * kinds are silently ignored so the caller can pipe every message
   * without a discriminator switch. */
  handleKernelMessage(msg: KernelToMain): void;
  /** Live pid → user-Worker map. Test-readable; production code
   * shouldn't mutate it. */
  readonly liveWorkers: ReadonlyMap<number, SpawnedEntry>;
  /**
   * Terminate every live user Worker without posting `proc:exited`.
   * For the kernel-panic path: the existing panic overlay's reload
   * timer terminates user Workers so we don't ship a half-alive
   * system back to the user.
   */
  terminateAll(): void;
}

export function createSpawnRouter(deps: SpawnRouterDeps): SpawnRouter {
  const live = new Map<number, SpawnedEntry>();

  function recordMemory(pid: number, bytes: number): void {
    if (!live.has(pid)) {
      return;
    }
    deps.kernelWorker.postMessage({ kind: "proc:memory", pid, bytes });
  }

  function reap(pid: number, code: number, trap: string | undefined): void {
    const entry = live.get(pid);
    if (!entry) {
      // Either an `exited` for a pid we never spawned (shouldn't
      // happen) or a duplicate exit (the user Worker posts exited
      // AND fires error on the same trap; main side reaps once).
      return;
    }
    live.delete(pid);
    entry.worker.terminate();
    deps.kernelWorker.postMessage(
      trap !== undefined
        ? { kind: "proc:exited", pid, code, trap }
        : { kind: "proc:exited", pid, code },
    );
  }

  return {
    liveWorkers: live,
    handleKernelMessage(msg: KernelToMain): void {
      if (msg.kind !== "proc:spawn") {
        return;
      }
      const sab = deps.allocSab();
      const worker = deps.workerFactory();
      live.set(msg.pid, { worker, sab });
      worker.addEventListener("message", (ev) => {
        const m = ev.data;
        if (m.pid !== msg.pid) {
          return;
        }
        if (m.kind === "memory") {
          recordMemory(msg.pid, m.bytes);
          return;
        }
        if (m.memoryBytes !== undefined) {
          recordMemory(msg.pid, m.memoryBytes);
        }
        if (m.kind === "exited") {
          reap(msg.pid, m.code, m.trap);
        }
      });
      worker.addEventListener("error", (ev) => {
        reap(msg.pid, -1, ev.message ?? "user worker error");
      });
      // T234: forward the kernel's wake-slot buffer when the host
      // exposed one. The user-worker entry constructs an
      // `Int32Array` view and hands it to `SabBackend`; the
      // production wake protocol lights up. When `getKernelWakeSlot`
      // is unset (T232 unit tests) or returns `null` (production
      // raced ahead of the kernel:wake-slot arrival, which our
      // protocol prevents by posting wake-slot before the first
      // proc:spawn — but defensive), the boot message omits the
      // field and `SabBackend` stays on the legacy path.
      const kernelWakeSlot = deps.getKernelWakeSlot?.() ?? null;
      worker.postMessage(
        kernelWakeSlot !== null
          ? {
              kind: "boot",
              pid: msg.pid,
              sab,
              wasmBytes: msg.wasmBytes,
              kernelWakeSlot,
            }
          : {
              kind: "boot",
              pid: msg.pid,
              sab,
              wasmBytes: msg.wasmBytes,
            },
      );
      deps.kernelWorker.postMessage({ kind: "proc:sab", pid: msg.pid, sab });
    },
    terminateAll(): void {
      for (const [pid, entry] of live) {
        entry.worker.terminate();
        live.delete(pid);
      }
    },
  };
}

// `SAB_SIZE` is re-exported so the production wiring in T234 can
// pass `() => new SharedArrayBuffer(SAB_SIZE)` without an extra
// import alongside `createSpawnRouter`.
export { SAB_SIZE };

// Go. Gated on `document` so this module is safely importable from
// vitest tests (which run under node and don't have a DOM). The
// production esbuild bundle runs in a real browser where `document`
// is always defined.
if (typeof document !== "undefined") {
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", main);
  } else {
    main();
  }
}
