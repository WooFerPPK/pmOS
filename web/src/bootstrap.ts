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
import { ConsoleTranscript } from "./console-transcript";
import { runEchoCheck } from "./console-check";
import { FbHost } from "./fb-host";
import type { FbFrame } from "./fb-host";
import { FbRenderer } from "./fb-renderer";
import { SAB_SIZE } from "./shared/sab-layout";
import {
  KbdKeyState,
  type KbdKeyStateValue,
  MouseButton,
  MouseButtonState,
  packKbdEvent,
  packMouseButton,
  packMouseMotion,
} from "./shared/input-proto";
import type {
  KernelToMain,
  MainToKernel,
  MainToUser,
  UserToMain,
} from "./shared/worker-proto";
import {
  HOST_FILE_IMPORT_MAX_BYTES,
  HOST_FILE_IMPORT_MAX_FILES,
  HOST_FILE_IMPORT_MAX_TOTAL_BYTES,
} from "./shared/worker-proto";
import { showStorageRecoveryGate } from "./storage-recovery";

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
 * Return the browser capabilities that make the real OS unsafe to start.
 * PMos must not present a volatile filesystem as a usable desktop when the
 * browser has no persistent-storage API at all. A runtime permission or quota
 * failure after these APIs are present is handled separately by the kernel's
 * explicit volatile-recovery path.
 */
export function unsupportedBrowserReasons(): string[] {
  const reasons: string[] = [];
  if (!isCrossOriginIsolated())
    reasons.push("cross-origin isolation (COOP/COEP)");
  if (!hasSharedArrayBuffer()) reasons.push("SharedArrayBuffer");
  if (!hasAtomicsWait()) reasons.push("Atomics.wait");
  if (!hasAtomicsWaitAsync()) reasons.push("Atomics.waitAsync");
  if (typeof Worker === "undefined") reasons.push("dedicated Workers");
  if (!hasOpfs()) reasons.push("Origin Private File System (OPFS)");
  if (
    typeof navigator === "undefined" ||
    typeof navigator.serviceWorker === "undefined"
  ) {
    reasons.push("service workers");
  }
  return reasons;
}

/**
 * Probe for `Atomics.wait`. Required for the SAB-backed syscall
 * transport; absent in non-cross-origin-isolated contexts.
 */
export function hasAtomicsWait(): boolean {
  return typeof Atomics !== "undefined" && typeof Atomics.wait === "function";
}

/**
 * Probe for `Atomics.waitAsync`. The kernel Worker must remain able to handle
 * browser messages while sleeping, so synchronous `Atomics.wait` is not a
 * valid fallback for its dispatcher.
 */
export function hasAtomicsWaitAsync(): boolean {
  return (
    typeof Atomics !== "undefined" &&
    typeof (Atomics as unknown as { waitAsync?: unknown }).waitAsync ===
      "function"
  );
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
    typeof (navigator.storage as unknown as { getDirectory?: unknown })
      .getDirectory === "function"
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

/** Resolve the root-emitted service worker relative to the deployed page. */
export function serviceWorkerScriptUrl(baseUrl?: string): string {
  const baseHref =
    baseUrl ?? (typeof document !== "undefined" ? document.baseURI : undefined);
  if (baseHref === undefined) {
    return "/sw.js";
  }
  const base = new URL(baseHref);
  const script = new URL("./sw.js", base);
  return `${script.pathname}${script.search}`;
}

/**
 * Register the bootstrap's service worker. Returns the
 * `Promise<unknown>` from `register()`, or `Promise.resolve(null)`
 * if the runtime has no service-worker container (private mode,
 * older browsers, jsdom). Defaults to the global
 * `navigator.serviceWorker`.
 */
export function registerServiceWorker(
  scriptURL = serviceWorkerScriptUrl(),
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

function paintBoot(
  c: Canvas2D,
  rows: CheckRow[],
  animationFrame: number,
): void {
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

  const unsupported = unsupportedBrowserReasons();
  if (unsupported.length > 0) {
    const detail = `Missing required browser capabilities: ${unsupported.join(", ")}`;
    console.error(`[pmos-bootstrap] unsupported browser: ${detail}`);
    showUnsupportedBrowserMessage(detail);
    return;
  }

  void registerServiceWorker().catch((error: unknown) => {
    console.warn(
      `[pmos-bootstrap] service-worker registration failed: ${String(error)}`,
    );
  });

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
  //   * `#process-trap` / `#process-sigkill` → dedicated lifecycle
  //     fixtures used to prove trap reconciliation and forced Worker
  //     teardown in a real browser.
  //   * `#mock-kernel` → fall through to the legacy MockKernel
  //     boot-screen check rows below (faux shell + live
  //     terminal + capability checks).
  //   * `#boot-to-desktop` → explicit alias for the default.
  if (!window.location.hash.includes("mock-kernel")) {
    const hash = window.location.hash;
    let bootBinary: string;
    if (hash.includes("input-echo")) {
      bootBinary = "/bin/hello_input_echo";
    } else if (hash.includes("process-trap")) {
      bootBinary = "/bin/hello_trap";
    } else if (hash.includes("process-sigkill")) {
      bootBinary = "/bin/hello_self_kill";
    } else if (hash.includes("real-kernel")) {
      bootBinary = "/bin/init";
    } else {
      bootBinary = "/bin/init-desktop";
    }
    runRealKernelMode(bootBinary);
    return;
  }

  const rows: CheckRow[] = [
    {
      label: "Cross-origin isolation (COOP/COEP)",
      status: "pending",
      detail: "",
    },
    { label: "SharedArrayBuffer", status: "pending", detail: "" },
    { label: "Atomics.wait / waitAsync", status: "pending", detail: "" },
    {
      label: "Origin Private Filesystem (OPFS)",
      status: "pending",
      detail: "",
    },
    { label: "Service worker", status: "pending", detail: "" },
    { label: "OffscreenCanvas", status: "pending", detail: "" },
    {
      label: "Kernel WASM load (/assets/kernel.wasm)",
      status: "pending",
      detail: "",
    },
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
    if (hasAtomicsWait() && hasAtomicsWaitAsync()) {
      rows[2].status = "ok";
      rows[2].detail = "blocking user + async kernel waits available";
    } else {
      rows[2].status = "fail";
      rows[2].detail = "Atomics.wait or Atomics.waitAsync missing";
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

async function attemptKernelFetch(): Promise<
  { ok: true; size: number } | { ok: false; reason: string }
> {
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
/**
 * Translate a DOM `KeyboardEvent.code` into its USB HID
 * scancode — the same `display_server::Scancode` enum the
 * display-server's `inject_keyboard_key` and the term's
 * `term::keymap::translate` both speak. Returns null for
 * unmapped codes (most F-keys, IME composition keys, etc.)
 * so the caller can fall through to the browser's default
 * handling.
 */
function domCodeToScancode(code: string): number | null {
  if (code.length === 4 && code.startsWith("Key")) {
    const ch = code.charCodeAt(3);
    if (ch >= 65 /* 'A' */ && ch <= 90 /* 'Z' */) {
      return 0x04 + (ch - 65);
    }
  }
  if (code.length === 6 && code.startsWith("Digit")) {
    const ch = code.charCodeAt(5);
    if (ch === 48 /* '0' */) return 0x27;
    if (ch >= 49 /* '1' */ && ch <= 57 /* '9' */) {
      return 0x1e + (ch - 49);
    }
  }
  switch (code) {
    case "Enter":
      return 0x28;
    case "Escape":
      return 0x29;
    case "Backspace":
      return 0x2a;
    case "Tab":
      return 0x2b;
    case "Space":
      return 0x2c;
    case "Minus":
      return 0x2d;
    case "Equal":
      return 0x2e;
    case "BracketLeft":
      return 0x2f;
    case "BracketRight":
      return 0x30;
    case "Backslash":
      return 0x31;
    case "Semicolon":
      return 0x33;
    case "Quote":
      return 0x34;
    case "Backquote":
      return 0x35;
    case "Comma":
      return 0x36;
    case "Period":
      return 0x37;
    case "Slash":
      return 0x38;
    case "F4":
      return 0x3d;
    case "Insert":
      return 0x49;
    case "Home":
      return 0x4a;
    case "PageUp":
      return 0x4b;
    case "Delete":
      return 0x4c;
    case "End":
      return 0x4d;
    case "PageDown":
      return 0x4e;
    case "ArrowRight":
      return 0x4f;
    case "ArrowLeft":
      return 0x50;
    case "ArrowDown":
      return 0x51;
    case "ArrowUp":
      return 0x52;
    case "ShiftLeft":
      return 0xe1;
    case "ShiftRight":
      return 0xe5;
    case "ControlLeft":
      return 0xe0;
    case "ControlRight":
      return 0xe4;
    case "AltLeft":
      return 0xe2;
    case "AltRight":
      return 0xe6;
    default:
      return null;
  }
}

/** Keyboard-event surface used by the graphical desktop input bridge. */
export interface GuiKeyboardEvent {
  readonly code: string;
  readonly metaKey: boolean;
  readonly repeat?: boolean;
  readonly target?: EventTarget | null;
  preventDefault(): void;
}

/** True when a keyboard event belongs to a privileged browser-substrate
 * control rather than the guest desktop. `closest()` also works after a
 * one-shot control removes itself during Enter's default click, so keyup does
 * not leak an unmatched release into PMos. */
export function targetsBrowserSubstrateControl(event: {
  readonly target?: EventTarget | null;
}): boolean {
  const target = event.target as
    { closest?: (selector: string) => unknown } | null | undefined;
  if (typeof target?.closest !== "function") return false;
  const recovery = target.closest("#pmos-storage-recovery");
  const hostPicker = target.closest(`#${HOST_FILE_PICKER_CONFIRM_ID}`);
  return (
    (recovery !== null && recovery !== undefined) ||
    (hostPicker !== null && hostPicker !== undefined)
  );
}

/** Keep every key transition with the destination selected on its first
 * keydown. A browser control may disappear before keyup, and a guest shortcut
 * may mount and focus one before keyup; neither focus change may split the
 * press/release pair across the browser and guest. `true` routes to the guest. */
export function createBrowserControlKeyRouter(
  targetsControl: (
    event: GuiKeyboardEvent,
  ) => boolean = targetsBrowserSubstrateControl,
): {
  keydown(event: GuiKeyboardEvent): boolean;
  keyup(event: GuiKeyboardEvent): boolean;
} {
  const routeToGuest = new Map<string, boolean>();
  return {
    keydown(event): boolean {
      const existingRoute = routeToGuest.get(event.code);
      if (existingRoute !== undefined) {
        if (event.repeat === true) return existingRoute;
        // A new non-repeat press means the old release was lost while a modal
        // control held focus. Discard the stale route before classifying this
        // new physical transition.
        routeToGuest.delete(event.code);
      }
      const guest = !targetsControl(event);
      routeToGuest.set(event.code, guest);
      return guest;
    },
    keyup(event): boolean {
      const existingRoute = routeToGuest.get(event.code);
      if (existingRoute !== undefined) {
        routeToGuest.delete(event.code);
        return existingRoute;
      }
      return !targetsControl(event);
    },
  };
}

/**
 * Build one half of the graphical keyboard bridge. Ctrl and Alt are operating
 * system input, including shortcuts such as Ctrl+C and Ctrl+S, so their own
 * key transitions and the modified key are forwarded. The host Meta key stays
 * reserved for browser/desktop shortcuts.
 */
export function createGuiKeyboardInputHandler(
  kernelWorker: { postMessage(msg: MainToKernel): void },
  state: KbdKeyStateValue,
): (event: GuiKeyboardEvent) => number | null {
  return (event: GuiKeyboardEvent): number | null => {
    if (event.metaKey) return null;
    const scancode = domCodeToScancode(event.code);
    if (scancode === null) return null;
    event.preventDefault();
    kernelWorker.postMessage({
      kind: "input:kbd",
      bytes: packKbdEvent(scancode, state),
    });
    return scancode;
  };
}

/** Graphical keyboard bridge with physical press/release ownership. Only keys
 * whose press reached the guest are held, so focus-loss recovery cannot inject
 * releases for browser-substrate controls or reserved host shortcuts. */
export function createGuiKeyboardInputBridge(
  kernelWorker: { postMessage(msg: MainToKernel): void },
  userInteractionAllowed: () => boolean = () => true,
  targetsControl: (event: GuiKeyboardEvent) => boolean =
    targetsBrowserSubstrateControl,
): {
  keydown(event: GuiKeyboardEvent): void;
  keyup(event: GuiKeyboardEvent): void;
  releaseHeldKeys(): void;
} {
  const press = createGuiKeyboardInputHandler(
    kernelWorker,
    KbdKeyState.Pressed,
  );
  const browserControlKeys = createBrowserControlKeyRouter(targetsControl);
  const heldGuestKeys = new Map<string, number>();
  const releasedForFocusLoss = new Set<string>();
  const postRelease = (scancode: number): void => {
    kernelWorker.postMessage({
      kind: "input:kbd",
      bytes: packKbdEvent(scancode, KbdKeyState.Released),
    });
  };

  return {
    keydown(event): void {
      if (!browserControlKeys.keydown(event)) return;

      if (releasedForFocusLoss.has(event.code)) {
        if (event.repeat === true) {
          event.preventDefault();
          return;
        }
        releasedForFocusLoss.delete(event.code);
      }
      if (!userInteractionAllowed()) {
        event.preventDefault();
        return;
      }

      if (event.repeat !== true) {
        const staleScancode = heldGuestKeys.get(event.code);
        if (staleScancode !== undefined) {
          postRelease(staleScancode);
          heldGuestKeys.delete(event.code);
        }
      }
      const scancode = press(event);
      if (scancode !== null) heldGuestKeys.set(event.code, scancode);
    },
    keyup(event): void {
      if (!browserControlKeys.keyup(event)) return;
      if (releasedForFocusLoss.delete(event.code)) {
        event.preventDefault();
        return;
      }

      const scancode = heldGuestKeys.get(event.code);
      if (scancode !== undefined) {
        event.preventDefault();
        postRelease(scancode);
        heldGuestKeys.delete(event.code);
        return;
      }
      if (!userInteractionAllowed()) event.preventDefault();
    },
    releaseHeldKeys(): void {
      for (const [code, scancode] of heldGuestKeys) {
        postRelease(scancode);
        releasedForFocusLoss.add(code);
      }
      heldGuestKeys.clear();
    },
  };
}

/** Release guest-owned keys when the browser can no longer promise matching
 * DOM keyup events. The returned disposer exists for isolated hosts/tests. */
export function installGuiKeyboardFocusLossHandlers(
  windowTarget: EventTarget,
  documentTarget: EventTarget & { readonly hidden: boolean },
  releaseHeldKeys: () => void,
): () => void {
  const onBlur = (): void => releaseHeldKeys();
  const onVisibilityChange = (): void => {
    if (documentTarget.hidden) releaseHeldKeys();
  };
  windowTarget.addEventListener("blur", onBlur);
  documentTarget.addEventListener("visibilitychange", onVisibilityChange);
  return () => {
    windowTarget.removeEventListener("blur", onBlur);
    documentTarget.removeEventListener("visibilitychange", onVisibilityChange);
  };
}

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

/** Minimal keyboard-event surface used by the text-only boot input bridge. */
export interface LegacyKeyboardEvent {
  readonly key: string;
  readonly ctrlKey: boolean;
  readonly metaKey: boolean;
  readonly altKey: boolean;
  preventDefault(): void;
}

/**
 * Build the canonical input handler used by text-only real-kernel boots.
 * Printable keys are accumulated until Enter, then delivered to the kernel as
 * one line-atomic input message. GUI boots do not use this path: they continue
 * to send packed press/release scancodes immediately.
 *
 * The distinction matters because `/dev/console` is line-buffered while the
 * diagnostic input reader may legally receive a short read. Sending `x` and
 * the following newline as unrelated worker messages allowed the reader to
 * consume and echo bare `x`, exit, and leave it permanently buffered before
 * the newline reached `/dev/input_kbd`.
 */
export function createLegacyKeyboardInputHandler(
  kernelWorker: { postMessage(msg: MainToKernel): void },
  maxLineBytes: number = 4096,
): (event: LegacyKeyboardEvent) => void {
  if (!Number.isSafeInteger(maxLineBytes) || maxLineBytes < 1) {
    throw new Error(
      `createLegacyKeyboardInputHandler: maxLineBytes must be a positive integer, got ${maxLineBytes}`,
    );
  }

  const chunks: Uint8Array[] = [];
  let bufferedBytes = 0;

  return (event: LegacyKeyboardEvent): void => {
    if (event.ctrlKey || event.metaKey || event.altKey) {
      return;
    }

    if (event.key === "Backspace") {
      if (chunks.length > 0) {
        const removed = chunks.pop();
        bufferedBytes -= removed?.byteLength ?? 0;
      }
      event.preventDefault();
      return;
    }

    const bytes = keyToBytes(event.key);
    if (bytes === null) {
      return;
    }
    event.preventDefault();

    if (event.key !== "Enter") {
      if (bufferedBytes + bytes.byteLength <= maxLineBytes) {
        chunks.push(bytes);
        bufferedBytes += bytes.byteLength;
      }
      return;
    }

    const line = new Uint8Array(bufferedBytes + bytes.byteLength);
    let offset = 0;
    for (const chunk of chunks) {
      line.set(chunk, offset);
      offset += chunk.byteLength;
    }
    line.set(bytes, offset);
    chunks.length = 0;
    bufferedBytes = 0;
    kernelWorker.postMessage({ kind: "input:kbd", bytes: line });
  };
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

/** DOM surface observed by framebuffer presentation tests and telemetry. */
export interface FramebufferPresentationTarget {
  readonly dataset: { pmosFrameSequence?: string };
  dispatchEvent(event: Event): boolean;
}

export interface FramebufferPresentationOptions {
  readonly host: Pick<FbHost, "onFrame" | "onPatch" | "onPatchBatch">;
  readonly renderer: Pick<
    FbRenderer,
    "onPresentComplete" | "paintFrame" | "paintPatch" | "paintPatchBatch"
  >;
  readonly target: FramebufferPresentationTarget;
  /** Compatibility hook for one-time first-frame milestones. */
  readonly onFirstPresent?: () => void;
  readonly now?: () => number;
  readonly makeFrameEvent?: (detail: {
    readonly sequence: number;
    /** Main thread received the framebuffer update and began painting. */
    readonly receivedAt: number;
    readonly paintedAt: number;
  }) => Event;
}

/** One-shot GUI readiness latch shared by splash dismissal and input gating. */
export interface GuiDesktopReadyLatch {
  readonly ready: boolean;
  notePresentFence(serial: number): void;
}

/**
 * Interpret the first valid display-server presentation fence as the GUI's
 * trusted desktop-ready boundary. The framebuffer driver owns the typed route,
 * so ordinary console output cannot spoof readiness. Standard and alternate
 * shells share this layer-neutral fence.
 */
export function createGuiDesktopReadyLatch(
  onReady: () => void,
): GuiDesktopReadyLatch {
  let ready = false;

  return {
    get ready(): boolean {
      return ready;
    },
    notePresentFence(serial: number): void {
      if (
        ready ||
        !Number.isInteger(serial) ||
        serial <= 0 ||
        serial > 0xffff_ffff
      ) {
        return;
      }
      ready = true;
      onReady();
    },
  };
}

export interface BootInteractionGateState {
  readonly kernelReady: boolean;
  readonly storageDegraded: boolean;
  readonly temporaryStorageAccepted: boolean;
  /** `null` identifies a non-GUI boot with no desktop-ready handshake. */
  readonly guiDesktopReady: boolean | null;
}

/** Keep GUI input outside the OS until the fully presented desktop is ready. */
export function isBootInteractionAllowed(
  state: BootInteractionGateState,
): boolean {
  return (
    state.kernelReady &&
    (!state.storageDegraded || state.temporaryStorageAccepted) &&
    (state.guiDesktopReady === null || state.guiDesktopReady)
  );
}

/**
 * Connect full blits and rectangular patches to one renderer completion
 * stream. Both presentation forms advance the same observable sequence and
 * emit the same `pmos:frame` event only after the renderer completes a paint.
 */
export function wireFramebufferPresentations(
  options: FramebufferPresentationOptions,
): void {
  const now = options.now ?? (() => performance.now());
  const makeFrameEvent =
    options.makeFrameEvent ??
    ((detail) => new CustomEvent("pmos:frame", { detail }));
  let sequence = 0;
  let firstPresentSeen = false;
  let receivedAt: number | null = null;

  options.renderer.onPresentComplete(() => {
    sequence += 1;
    const presentationReceivedAt = receivedAt ?? now();
    receivedAt = null;
    const paintedAt = now();
    options.target.dataset.pmosFrameSequence = String(sequence);
    options.target.dispatchEvent(
      makeFrameEvent({
        sequence,
        receivedAt: presentationReceivedAt,
        paintedAt,
      }),
    );
    if (!firstPresentSeen) {
      firstPresentSeen = true;
      options.onFirstPresent?.();
    }
  });
  options.host.onFrame((frame) => {
    receivedAt = now();
    options.renderer.paintFrame(frame);
  });
  options.host.onPatch((patch) => {
    receivedAt = now();
    options.renderer.paintPatch(patch);
  });
  options.host.onPatchBatch((batch) => {
    receivedAt = now();
    options.renderer.paintPatchBatch(batch.patches);
  });
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
  // Boot splash — fullscreen progress overlay covering the page while the
  // kernel, display server, and shell come up. It remains until the active
  // shell publishes a trusted framebuffer presentation fence after its initial
  // work has settled.
  const splash = isGuiBoot ? mountBootSplash() : null;
  const worker = new Worker("/assets/kernel-worker.js", { type: "module" });
  let kernelReady = false;
  let storageDegraded = false;
  let temporaryStorageAccepted = false;
  const guiDesktopReady = isGuiBoot
    ? createGuiDesktopReadyLatch(() => {
        splash?.markStarted("Desktop ready");
        splash?.dismiss();
        // The console is useful while the OS is booting (and remains visible
        // if readiness never arrives), but a settled desktop should own the
        // complete screen. Keep the populated node in the DOM for diagnostics
        // and integration assertions without leaving developer chrome over
        // ordinary application windows.
        consoleEl.hidden = true;
      })
    : null;
  const userInteractionAllowed = (): boolean =>
    isBootInteractionAllowed({
      kernelReady,
      storageDegraded,
      temporaryStorageAccepted,
      guiDesktopReady: guiDesktopReady?.ready ?? null,
    });
  if (splash) {
    splash.markStarted("Browser environment ready");
    splash.markStarted("Kernel worker spawned");
  }

  // GUI boot: wire the FbHost → FbRenderer pair so `fb:set-mode`,
  // `fb:blit`, and `fb:patch` messages from the kernel Worker paint pixels
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
        // Pin the canvas's CSS box to the framebuffer's
        // intrinsic pixel dims so the OS renders at native
        // resolution instead of stretching to fill the
        // viewport. The CSS class triggers flex-centering
        // on body + `width:auto/height:auto` on the canvas
        // (which then renders at the canvas element's
        // intrinsic pixel size = the framebuffer mode).
        document.body.classList.add("pmos-gui-mode");
        canvas.style.width = `${mode.width}px`;
        canvas.style.height = `${mode.height}px`;
        if (splash) {
          splash.markStarted(
            `Framebuffer mode set ${mode.width}×${mode.height}`,
          );
        }
      });
      wireFramebufferPresentations({
        host: fbHost,
        renderer,
        target: canvas,
        onFirstPresent: () => {
          if (splash) {
            splash.markStarted("Framebuffer presentation completed");
          }
        },
      });
      fbHost.onPresentFence((serial) => {
        guiDesktopReady?.notePresentFence(serial);
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
      const toFbCoords = (event: PointerEvent): [number, number] | null => {
        if (!guiFbMode) return null;
        const rect = canvas.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return null;
        const fx = ((event.clientX - rect.left) / rect.width) * guiFbMode.width;
        const fy =
          ((event.clientY - rect.top) / rect.height) * guiFbMode.height;
        const x = Math.max(0, Math.min(guiFbMode.width - 1, Math.floor(fx)));
        const y = Math.max(0, Math.min(guiFbMode.height - 1, Math.floor(fy)));
        return [x, y];
      };
      const domButtonToProtoButton = (domButton: number): number => {
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
      };
      canvas.addEventListener("pointermove", (event) => {
        if (!userInteractionAllowed()) return;
        const coords = toFbCoords(event);
        if (!coords) return;
        const [x, y] = coords;
        worker.postMessage({
          kind: "input:mouse",
          bytes: packMouseMotion(x, y),
        } satisfies MainToKernel);
      });
      canvas.addEventListener("pointerdown", (event) => {
        if (!userInteractionAllowed()) {
          event.preventDefault();
          return;
        }
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
        if (!userInteractionAllowed()) {
          event.preventDefault();
          return;
        }
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

      // Keyboard input for the GUI desktop. The display-server
      // reads `/dev/input_kbd` as packed 8-byte
      // `(scancode_u32_le, state_u32_le)` records via
      // `drain_kbd_events_into`; anything shorter is silently
      // dropped. Convert each `KeyboardEvent.code` to its USB
      // HID scancode (matching `display_server::Scancode`),
      // pack via `packKbdEvent`, and post on both keydown +
      // keyup so modifier transitions track on the term side.
      const guiKeyboard = createGuiKeyboardInputBridge(
        worker,
        userInteractionAllowed,
      );
      window.addEventListener("keydown", (event) => {
        guiKeyboard.keydown(event);
      });
      window.addEventListener("keyup", (event) => {
        guiKeyboard.keyup(event);
      });
      installGuiKeyboardFocusLossHandlers(
        window,
        document,
        guiKeyboard.releaseHeldKeys,
      );
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
    onLiveWorkersChanged: (count) => {
      document.body.dataset["pmosLiveWorkers"] = String(count);
    },
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
    if (msg.kind === "host:pick") {
      requestHostFilePicker(worker, undefined, userInteractionAllowed);
      return;
    }
    if (msg.kind === "host:download") {
      saveHostDownload(msg.name, msg.mime, msg.bytes);
      return;
    }
    if (msg.kind === "storage:degraded") {
      storageDegraded = true;
      showStorageRecoveryGate(msg, {
        onRetry: () => window.location.reload(),
        onContinueTemporary: () => {
          temporaryStorageAccepted = true;
          console.warn(
            "[pmos-bootstrap] temporary storage session explicitly accepted; files will be lost on reload",
          );
        },
      });
      return;
    }
    router.handleKernelMessage(msg);
    if (msg.kind === "proc:terminate") {
      document.body.dataset["pmosLastTerminatedPid"] = String(msg.pid);
      document.body.dataset["pmosLastTerminatedSignal"] = String(msg.signal);
    }
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
  // Crash screen — fullscreen "PMos has stopped responding"
  // overlay shown when the OS panics, a child reaps, or the
  // console goes silent for >5s (the wedge case the user has
  // been chasing). Captures the recent console tail so the
  // BSOD shows what the OS was doing at the moment it died.
  const crashScreen = isGuiBoot ? mountCrashScreen() : null;
  const recentLines: string[] = [];
  const RECENT_LINES_CAP = 80;
  const transcript = new ConsoleTranscript(consoleEl);
  consoleHost.onOutput((bytes: Uint8Array) => {
    const text = new TextDecoder().decode(bytes);
    transcript.append(text);
    console.log(`[real-kernel] ${text.replace(/\n$/, "")}`);
    if (splash) {
      splash.observeConsoleLine(text);
    }
    if (crashScreen) {
      crashScreen.observeConsoleLine(text, recentLines, RECENT_LINES_CAP);
    }
  });
  consoleHost.onLifecycle((event: ConsoleLifecycleEvent) => {
    if (event.kind === "ready") {
      kernelReady = true;
      console.log("[pmos-bootstrap] real kernel ready");
      if (splash) {
        splash.markStarted("Kernel ready");
      }
    } else if (event.kind === "panic") {
      console.error(`[pmos-bootstrap] real kernel panic: ${event.message}`);
      transcript.append(`\n[panic] ${event.message}\n`);
      if (splash) {
        splash.markFailed(`Kernel panic: ${event.message}`);
      }
      if (crashScreen) {
        crashScreen.show({
          title: "Kernel panic",
          subtitle: event.message,
          recent: recentLines,
        });
      }
    }
  });

  // Worker errors — uncaught exceptions inside the kernel
  // worker itself. These are the "wasm trap" path, distinct
  // from the kernel's own panic event.
  worker.addEventListener("error", (ev: ErrorEvent) => {
    const msg = ev.message || "(unknown worker error)";
    console.error(`[pmos-bootstrap] kernel worker error: ${msg}`);
    if (crashScreen) {
      crashScreen.show({
        title: "Kernel worker crashed",
        subtitle: msg,
        recent: recentLines,
      });
    }
  });
  worker.addEventListener("messageerror", (ev) => {
    console.error(`[pmos-bootstrap] kernel worker messageerror`, ev);
    if (crashScreen) {
      crashScreen.show({
        title: "Kernel worker message decode failed",
        subtitle: "MessageEvent.data could not be cloned",
        recent: recentLines,
      });
    }
  });

  // Unhandled promise rejections + window errors. Catches
  // bootstrap-side bugs that aren't worker-scoped.
  window.addEventListener("unhandledrejection", (ev: PromiseRejectionEvent) => {
    const reason = String(ev.reason ?? "(unknown)");
    console.error(`[pmos-bootstrap] unhandled rejection: ${reason}`);
    if (crashScreen) {
      crashScreen.show({
        title: "Bootstrap promise rejected",
        subtitle: reason,
        recent: recentLines,
      });
    }
  });

  // Wedge watchdog removed: was triggering false positives now
  // that the shell + display-server have stopped emitting periodic
  // heartbeats during steady-state idle. A genuinely-wedged OS still
  // surfaces a crash screen via the worker `error` / `messageerror`
  // listeners, the `unhandledrejection` listener, the
  // consoleHost.onLifecycle({kind:"panic"}) path, and the fatal-line
  // scanner inside crashScreen.observeConsoleLine (which catches a dead
  // display server, a real kernel panic, and dispatch errors without needing
  // a periodic-output expectation). Shell exits are supervised by PID 1 and
  // are therefore not terminal browser-substrate failures.

  // Keyboard input: a `keydown` on `document` so the handler fires
  // regardless of which element has focus (real-kernel mode hides the
  // canvas and the DOM console pre-element is non-interactive by
  // default). Text bytes are held in canonical mode until Enter and posted
  // as one line-atomic `input:kbd` message on the kernel Worker channel. The
  // kernel worker's scaffold routes it
  // through `InputDriver.onHostMessage` into
  // `KernelWasmHost.injectInput(Devnum.InputKbd, bytes)`, which lands
  // the bytes in `/dev/input_kbd`. A user process polling `fd_read` on
  // the node (e.g. `/bin/hello_input_echo` under `#input-echo`) picks
  // them up on its next iteration.
  // Legacy UTF-8 keydown path for non-GUI boots
  // (`hello_input_echo` etc., where a userland process reads
  // raw character bytes off `/dev/input_kbd`). GUI boots
  // (init-desktop) install their own scancode-encoded path
  // above so the display-server's 8-byte
  // `drain_kbd_events_into` parser sees the right format;
  // running both at once would interleave incompatible
  // records in the ring buffer.
  if (!isGuiBoot) {
    const legacyKeyDown = createLegacyKeyboardInputHandler(worker);
    document.addEventListener("keydown", (event) => {
      if (!userInteractionAllowed()) {
        if (!targetsBrowserSubstrateControl(event)) event.preventDefault();
        return;
      }
      legacyKeyDown(event);
    });
  }

  // T137: pagehide-driven persistence sync. The kernel's per-process
  // proc_exit hook covers normal exits; this covers the "user closes
  // the tab while a process is mid-flight" case so OPFS-backed
  // mutations are not lost.
  installPagehideSync(worker);
  // T137 follow-up: beforeunload as a secondary fallback. Some
  // older browser embeddings skip pagehide on certain navigation
  // paths; both wire to the same `sync:request` and the kernel
  // dedupes a flush against a clean VFS.
  installBeforeUnloadSync(worker);
  // T137 follow-up: periodic sync for long-running tabs. 60s default
  // trades I/O frequency against window-of-loss size — a hard browser
  // crash loses at most one minute of writes.
  installPeriodicSync(worker);
  // T154: bootstrap-side drag-drop import. Each file the user drops
  // onto the canvas turns into a `host:dropped` MainToKernel
  // message; the kernel registers the bytes under the supplied
  // token and a userland `host_file_recv(token)` consumes them.
  installHostFileDropHandler(
    worker,
    window as unknown as DropTarget,
    userInteractionAllowed,
  );
}

/**
 * Create (and return) the `<pre id="pmos-real-console">` element
 * the real-kernel mode renders boot output into.
 *
 * `gui` selects between two layouts:
 *   * `gui = false` (legacy demo + input-echo): canvas hidden,
 *     console pre fills the viewport with a dark-monospace look.
 *   * `gui = true` (init-desktop): canvas visible at 100vw×100vh,
 *     console pre overlays as a small transparent boot log at the
 *     top-right, then hides at the trusted desktop-ready fence.
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
      'font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace',
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
      'font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace',
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

/**
 * Boot-splash overlay. Paints a fullscreen progress screen
 * during the kernel + display server + shell startup. Each
 * milestone (env checks, kernel spawn, init-desktop spawn,
 * display-server bind, shell connect, framebuffer presentation,
 * interactive readiness) lights up a row. The bootstrap dismisses
 * it only after the standard shell and framebuffer readiness latch fires.
 *
 * Designed to be observable: every line carries a status icon
 * and a short timestamp so the user can see the OS coming up
 * step by step instead of staring at a black page.
 */
interface BootSplash {
  /** Mark a known step as completed (or move "in progress"
   *  forward). Idempotent — duplicate marks are ignored. */
  markStarted(label: string): void;
  /** Mark a step that the boot raised an error on. The
   *  splash stays visible so the user can read the failure. */
  markFailed(label: string): void;
  /** Forward a console line; the splash matches it against
   *  known kernel/userland milestones (`init-desktop spawned
   *  display-server`, `shell: connected to /run/display`, …)
   *  and updates the corresponding step. */
  observeConsoleLine(text: string): void;
  /** Remove the overlay when the desktop-ready latch fires. */
  dismiss(): void;
}

interface BootStep {
  readonly label: string;
  /** Substrings the splash watches for in console output to
   *  auto-mark this step as started. */
  readonly consoleHints: readonly string[];
  /** Timestamp (ms since boot) when started. */
  startedAt: number | null;
  /** True if a failure was reported against this step. */
  failed: boolean;
  /** Custom failure message override. */
  failureMessage: string | null;
}

function mountBootSplash(): BootSplash {
  const t0 = performance.now();
  const overlay = document.createElement("div");
  overlay.id = "pmos-boot-splash";
  overlay.style.cssText = [
    "position: fixed",
    "inset: 0",
    "display: flex",
    "flex-direction: column",
    "align-items: center",
    "justify-content: center",
    "background: radial-gradient(circle at 50% 35%, #1a2638 0%, #060a12 100%)",
    "color: #e6e6e6",
    'font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace',
    "z-index: 1000",
    "opacity: 1",
    // The bootstrap's explicit input gate discards canvas input until desktop
    // readiness. The overlay never owns pointer input, including while booting.
    "pointer-events: none",
  ].join("; ");

  const titleBlock = document.createElement("div");
  titleBlock.style.cssText = [
    "margin-bottom: 2.5rem",
    "text-align: center",
  ].join("; ");
  const title = document.createElement("div");
  title.textContent = "PMos";
  title.style.cssText = [
    "font-size: 56px",
    "font-weight: 200",
    "letter-spacing: 0.4em",
    "color: #ffffff",
    "margin-bottom: 0.4rem",
  ].join("; ");
  const subtitle = document.createElement("div");
  subtitle.textContent = "browser-native operating system";
  subtitle.style.cssText = [
    "font-size: 12px",
    "letter-spacing: 0.25em",
    "text-transform: uppercase",
    "color: #5b7fa9",
  ].join("; ");
  titleBlock.appendChild(title);
  titleBlock.appendChild(subtitle);

  const list = document.createElement("div");
  list.style.cssText = [
    "min-width: 480px",
    "max-width: 80vw",
    "padding: 1rem 1.25rem",
    "background: rgba(10, 14, 20, 0.55)",
    "border: 1px solid rgba(91, 127, 169, 0.25)",
    "border-radius: 6px",
    "font-size: 13px",
    "line-height: 1.9",
  ].join("; ");

  overlay.appendChild(titleBlock);
  overlay.appendChild(list);
  document.body.appendChild(overlay);

  const steps: BootStep[] = [
    {
      label: "Browser environment ready",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "Kernel worker spawned",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "Kernel ready",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "init (PID 1) running",
      consoleHints: ["init-desktop starting"],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "display-server spawned",
      consoleHints: ["init-desktop spawned display-server"],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "shell spawned",
      consoleHints: ["init-desktop spawned shell"],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "init supervising children",
      consoleHints: ["init-desktop entering supervision loop"],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "display-server bound /run/display",
      consoleHints: ["display-server starting"],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "shell connected to display",
      consoleHints: ["shell: connected to /run/display"],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "shell handshake complete",
      consoleHints: ["display-server served client 0"],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "Framebuffer mode set 1024×768",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "Framebuffer presentation completed",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
    {
      label: "Desktop ready",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null,
    },
  ];

  const rows: HTMLDivElement[] = steps.map((step) => {
    const row = document.createElement("div");
    row.style.cssText = [
      "display: flex",
      "align-items: baseline",
      "gap: 0.75rem",
      "color: #5b7fa9",
      "transition: color 150ms ease",
    ].join("; ");
    const icon = document.createElement("span");
    icon.textContent = "○";
    icon.style.cssText = "width: 1.2rem; text-align: center; font-size: 14px;";
    const label = document.createElement("span");
    label.textContent = step.label;
    label.style.cssText = "flex: 1;";
    const elapsed = document.createElement("span");
    elapsed.style.cssText =
      "color: #4a5f7a; font-size: 11px; min-width: 4.5rem; text-align: right;";
    row.appendChild(icon);
    row.appendChild(label);
    row.appendChild(elapsed);
    list.appendChild(row);
    return row;
  });

  function findStep(label: string): number {
    return steps.findIndex((s) => s.label === label);
  }

  function paintRow(i: number): void {
    const step = steps[i];
    const row = rows[i];
    if (!step || !row) return;
    const icon = row.children[0] as HTMLElement;
    const elapsed = row.children[2] as HTMLElement;
    if (step.failed) {
      icon.textContent = "✗";
      icon.style.color = "#ff7a7a";
      row.style.color = "#ff9c9c";
      elapsed.textContent = step.failureMessage ?? "failed";
      return;
    }
    if (step.startedAt !== null) {
      icon.textContent = "✓";
      icon.style.color = "#7ad0a3";
      row.style.color = "#dde7f0";
      const ms = Math.round(step.startedAt - t0);
      elapsed.textContent = `+${ms}ms`;
      return;
    }
    icon.textContent = "○";
    icon.style.color = "#3e5474";
    row.style.color = "#5b7fa9";
    elapsed.textContent = "";
  }

  for (let i = 0; i < steps.length; i += 1) {
    paintRow(i);
  }

  function markStarted(label: string): void {
    const i = findStep(label);
    if (i < 0) return;
    if (steps[i].startedAt !== null) return;
    steps[i].startedAt = performance.now();
    paintRow(i);
  }

  function markFailed(label: string): void {
    // Not necessarily one of the canned steps — append a
    // synthetic row at the bottom so the user sees the
    // failure in context.
    const synthetic: BootStep = {
      label,
      consoleHints: [],
      startedAt: null,
      failed: true,
      failureMessage: "failed",
    };
    steps.push(synthetic);
    const row = document.createElement("div");
    row.style.cssText = [
      "display: flex",
      "align-items: baseline",
      "gap: 0.75rem",
      "color: #ff9c9c",
    ].join("; ");
    const icon = document.createElement("span");
    icon.textContent = "✗";
    icon.style.cssText = "width: 1.2rem; text-align: center; color: #ff7a7a;";
    const labelEl = document.createElement("span");
    labelEl.textContent = label;
    labelEl.style.cssText = "flex: 1;";
    const elapsed = document.createElement("span");
    elapsed.textContent = "failed";
    elapsed.style.cssText = "color: #ff7a7a; font-size: 11px;";
    row.appendChild(icon);
    row.appendChild(labelEl);
    row.appendChild(elapsed);
    list.appendChild(row);
    rows.push(row);
  }

  function observeConsoleLine(text: string): void {
    for (const line of text.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      for (const step of steps) {
        if (step.startedAt !== null) continue;
        for (const hint of step.consoleHints) {
          if (trimmed.includes(hint)) {
            markStarted(step.label);
            break;
          }
        }
      }
    }
  }

  function dismiss(): void {
    overlay.remove();
  }

  return { markStarted, markFailed, observeConsoleLine, dismiss };
}

/**
 * Crash-screen overlay — the "PMos has stopped responding"
 * fullscreen red panel a user gets when the OS panics,
 * deadlocks, or the kernel-worker itself crashes. Captures
 * the recent console tail so the user can see what the OS
 * was doing at the moment it died, plus a Reload button to
 * reboot the substrate.
 *
 * Triggered by the runRealKernelMode wiring's
 *   * worker `error` / `messageerror` events
 *   * unhandledrejection / window error
 *   * `consoleHost.onLifecycle({kind:"panic"})`
 *   * 8-second console-silence watchdog
 *   * the unsupervised display-server exit marker
 */
interface CrashScreen {
  observeConsoleLine(text: string, recentSink: string[], cap: number): void;
  show(args: { title: string; subtitle: string; recent: string[] }): void;
}

export interface FatalConsoleDiagnosis {
  readonly title: string;
  readonly subtitle: string;
}

/**
 * Classify only console evidence that means the running browser substrate can
 * no longer provide a desktop. PID 1 deliberately supervises and respawns the
 * shell, so its reap marker is not fatal and must not cover the replacement
 * shell with a blocking crash screen.
 */
export function classifyFatalConsoleText(
  text: string,
): FatalConsoleDiagnosis | null {
  const lower = text.toLowerCase();
  const lastLine = text.trim().split("\n").slice(-1)[0] ?? "";
  if (lower.includes("real kernel panic")) {
    return { title: "Kernel panic", subtitle: lastLine };
  }
  if (lower.includes("dispatch error")) {
    return { title: "Shell dispatch error", subtitle: lastLine };
  }
  if (lower.includes("init-desktop reaped child pid=3")) {
    return {
      title: "Display server died",
      subtitle:
        "init-desktop reaped the display-server. The desktop cannot run without it.",
    };
  }
  return null;
}

function mountCrashScreen(): CrashScreen {
  const overlay = document.createElement("div");
  overlay.id = "pmos-crash-screen";
  overlay.style.cssText = [
    "position: fixed",
    "inset: 0",
    "display: none",
    "flex-direction: column",
    "align-items: center",
    "justify-content: center",
    "background: linear-gradient(180deg, #1a0606 0%, #0a0202 100%)",
    "color: #ffd0d0",
    'font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace',
    "z-index: 10000",
    "padding: 2rem",
    "box-sizing: border-box",
    "overflow: auto",
  ].join("; ");

  const container = document.createElement("div");
  container.style.cssText = ["max-width: 880px", "width: 100%"].join("; ");

  const banner = document.createElement("div");
  banner.style.cssText = [
    "background: #ff4040",
    "color: #1a0606",
    "padding: 0.5rem 1rem",
    "font-weight: 700",
    "letter-spacing: 0.3em",
    "text-transform: uppercase",
    "font-size: 11px",
    "margin-bottom: 1.5rem",
    "border-radius: 4px",
  ].join("; ");
  banner.textContent = "Kernel halted";

  const title = document.createElement("h1");
  title.style.cssText = [
    "color: #ff7a7a",
    "font-size: 32px",
    "font-weight: 300",
    "margin: 0 0 0.5rem 0",
    "letter-spacing: 0.05em",
  ].join("; ");

  const subtitle = document.createElement("div");
  subtitle.style.cssText = [
    "color: #ffb0b0",
    "font-size: 14px",
    "margin-bottom: 2rem",
    "white-space: pre-wrap",
    "word-break: break-word",
  ].join("; ");

  const intro = document.createElement("div");
  intro.style.cssText = [
    "color: #d0a0a0",
    "font-size: 12px",
    "margin-bottom: 0.5rem",
    "letter-spacing: 0.1em",
    "text-transform: uppercase",
  ].join("; ");
  intro.textContent = "Last 80 lines of kernel output";

  const log = document.createElement("pre");
  log.style.cssText = [
    "background: rgba(0, 0, 0, 0.4)",
    "border: 1px solid rgba(255, 122, 122, 0.25)",
    "border-radius: 4px",
    "padding: 1rem",
    "margin: 0 0 1.5rem 0",
    "max-height: 50vh",
    "overflow: auto",
    "font-size: 11px",
    "line-height: 1.5",
    "color: #e6c8c8",
    "white-space: pre-wrap",
    "word-break: break-word",
  ].join("; ");

  const buttons = document.createElement("div");
  buttons.style.cssText = ["display: flex", "gap: 0.75rem"].join("; ");

  const reload = document.createElement("button");
  reload.textContent = "Reload";
  reload.style.cssText = [
    "padding: 0.6rem 1.4rem",
    "background: #ff7a7a",
    "color: #1a0606",
    "border: none",
    "border-radius: 4px",
    "font-family: inherit",
    "font-weight: 600",
    "font-size: 14px",
    "cursor: pointer",
  ].join("; ");
  reload.addEventListener("click", () => {
    window.location.reload();
  });

  const dismiss = document.createElement("button");
  dismiss.textContent = "Dismiss";
  dismiss.style.cssText = [
    "padding: 0.6rem 1.4rem",
    "background: transparent",
    "color: #ffb0b0",
    "border: 1px solid rgba(255, 176, 176, 0.4)",
    "border-radius: 4px",
    "font-family: inherit",
    "font-size: 14px",
    "cursor: pointer",
  ].join("; ");
  dismiss.addEventListener("click", () => {
    overlay.style.display = "none";
  });

  buttons.appendChild(reload);
  buttons.appendChild(dismiss);

  container.appendChild(banner);
  container.appendChild(title);
  container.appendChild(subtitle);
  container.appendChild(intro);
  container.appendChild(log);
  container.appendChild(buttons);
  overlay.appendChild(container);
  document.body.appendChild(overlay);

  let shown = false;

  function observeConsoleLine(
    text: string,
    recentSink: string[],
    cap: number,
  ): void {
    for (const line of text.split("\n")) {
      const trimmed = line.replace(/\r$/, "");
      if (trimmed === "") continue;
      recentSink.push(trimmed);
      while (recentSink.length > cap) {
        recentSink.shift();
      }
    }
    // Auto-trigger on a few well-known fatal patterns the
    // kernel can't surface as a panic event (because the
    // panicking pid is userland, not the kernel itself).
    const diagnosis = classifyFatalConsoleText(text);
    if (!shown && diagnosis !== null) {
      // Defer slightly so the offending line lands in
      // recentSink before the screen renders.
      window.setTimeout(() => {
        if (shown) return;
        show({
          title: diagnosis.title,
          subtitle: diagnosis.subtitle,
          recent: recentSink,
        });
      }, 50);
    }
  }

  function show(args: {
    title: string;
    subtitle: string;
    recent: string[];
  }): void {
    if (shown) return;
    shown = true;
    title.textContent = args.title;
    subtitle.textContent = args.subtitle;
    log.textContent = args.recent.join("\n");
    overlay.style.display = "flex";
    // Scroll the log to the bottom so the failure line is
    // visible without scrolling.
    log.scrollTop = log.scrollHeight;
  }

  return { observeConsoleLine, show };
}

function showFallbackMessage(error: string): void {
  document.body.innerHTML = `
    <div style="padding:2rem;font-family:ui-monospace,monospace;color:#e6e6e6;background:#0a0e14;height:100vh">
      <h1 style="color:#ff6b6b">PMos bootstrap failed</h1>
      <p>${escapeHtml(error)}</p>
      <p style="color:#808591">See devtools console for details.</p>
    </div>`;
}

function showUnsupportedBrowserMessage(detail: string): void {
  document.body.innerHTML = `
    <main id="pmos-unsupported-browser" style="box-sizing:border-box;padding:2rem;font-family:ui-monospace,monospace;color:#e6e6e6;background:#0a0e14;min-height:100vh">
      <h1 style="color:#ffb454">PMos cannot start in this browser</h1>
      <p>${escapeHtml(detail)}</p>
      <p>PMos requires persistent browser-local storage so it never presents a desktop that silently loses your files.</p>
      <p style="color:#a8adb7">Use a current browser with OPFS, service workers, cross-origin isolation, SharedArrayBuffer, Atomics.wait, Atomics.waitAsync, and dedicated Workers.</p>
    </main>`;
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
// the new Worker, and waits for its `booted` acknowledgement before
// posting `proc:sab` back to the kernel so the kernel's dispatch
// loop adds the pid to its pidMap. When the user
// Worker posts `exited` (or fires an `error` event), main posts
// `proc:exited` to the kernel and terminates the Worker.
//
// Production and tests share this router so process publication,
// rollback, and host teardown have one lifecycle implementation.

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
  /** Called after the router adds or removes a host Worker route. */
  readonly onLiveWorkersChanged?: (count: number) => void;
  /**
   * Defense-in-depth cap for live user Workers. Production uses the kernel's
   * global process limit; tests may lower it to exercise the rejection path.
   */
  readonly maxLiveWorkers?: number;
}

/** Must match `kernel::proc::PROCESS_LIMIT_GLOBAL`. */
export const MAX_LIVE_USER_WORKERS = 256;

const USER_WORKER_TRAP_LOG_MAX = 240;

function sanitizeUserWorkerTrap(trap: string): string {
  const singleLine = trap
    .replace(/[\u0000-\u001f\u007f-\u009f\u2028\u2029]+/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  const visible = singleLine === "" ? "(empty trap)" : singleLine;
  return visible.length <= USER_WORKER_TRAP_LOG_MAX
    ? visible
    : `${visible.slice(0, USER_WORKER_TRAP_LOG_MAX - 3)}...`;
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
 * Subset of an `EventTarget` needed by
 * [`installHostFileDropHandler`]. Production passes `window` (so
 * drops anywhere in the page land); tests pass a stub that
 * records the listeners and lets the test fire them.
 */
export interface DropTarget {
  addEventListener(
    type: "dragover" | "drop",
    listener: (event: HostDragEvent) => void,
  ): void;
}

/**
 * The drop-event surface this module reaches into. A subset of
 * the DOM `DragEvent` so tests can synthesise a value without
 * faking the entire DOM. The two real fields we touch are
 * `dataTransfer.files` (a list of host `File` objects) and
 * `preventDefault()` (so the browser doesn't try to navigate to
 * the dropped resource).
 */
export interface HostDragEvent {
  preventDefault(): void;
  readonly dataTransfer: {
    readonly files: ArrayLike<HostDragFile>;
  } | null;
}

/**
 * The file surface — `name`, optional `type`, and an
 * `arrayBuffer()` reader. Real `File` instances satisfy this
 * directly; tests pass a literal.
 */
export interface HostDragFile {
  readonly name: string;
  readonly type?: string;
  /** Browser-owned immutable byte length, available before reading the file. */
  readonly size: number;
  arrayBuffer(): Promise<ArrayBuffer>;
}

/** Browser picker seam. Production creates a transient `<input type=file>`;
 * unit tests inject a deterministic implementation with no DOM dependency. */
export interface HostFilePicker {
  pick(
    onFiles: (files: ArrayLike<HostDragFile>) => void,
    interactionAllowed?: () => boolean,
  ): void;
}

/** A browser-substrate confirmation surface. The kernel has already
 * authorised the request before this prompt is mounted; its trusted click is
 * the user activation that opens the native picker. */
export interface HostFilePickerConfirmation {
  request(onConfirm: () => void): void;
}

export const HOST_FILE_PICKER_CONFIRM_ID = "pmos-host-file-picker-confirm";

/** Compose an authorised confirmation prompt with the native picker. Keeping
 * the picker open in the confirmation callback avoids relying on transient
 * activation surviving a kernel and two Worker message turns. */
export function confirmedHostFilePicker(
  picker: HostFilePicker,
  confirmation: HostFilePickerConfirmation,
): HostFilePicker {
  return {
    pick(onFiles, interactionAllowed = () => true): void {
      let consumed = false;
      confirmation.request(() => {
        if (consumed) return;
        consumed = true;
        if (!interactionAllowed()) {
          console.warn(
            "[pmos-bootstrap] host file picker blocked while storage recovery is active",
          );
          return;
        }
        picker.pick(onFiles, interactionAllowed);
      });
    },
  };
}

function browserNativeHostFilePicker(): HostFilePicker {
  return {
    pick(onFiles): void {
      const input = document.createElement("input");
      input.type = "file";
      input.multiple = true;
      input.hidden = true;
      input.addEventListener(
        "change",
        () => {
          if (input.files !== null) onFiles(input.files);
          input.remove();
        },
        { once: true },
      );
      input.addEventListener("cancel", () => input.remove(), { once: true });
      document.body.append(input);
      // This call runs synchronously inside the trusted confirmation click.
      input.click();
    },
  };
}

function browserHostFilePickerConfirmation(): HostFilePickerConfirmation {
  return {
    request(onConfirm): void {
      const existing = document.getElementById(HOST_FILE_PICKER_CONFIRM_ID);
      if (existing instanceof HTMLButtonElement) {
        existing.focus();
        return;
      }
      existing?.remove();

      const button = document.createElement("button");
      button.id = HOST_FILE_PICKER_CONFIRM_ID;
      button.type = "button";
      button.textContent = "Choose files for PMos";
      button.style.cssText =
        "position:fixed;left:50%;top:16px;transform:translateX(-50%);" +
        "z-index:2147483647;padding:10px 16px;border:1px solid #365f84;" +
        "border-radius:4px;background:#f4f5f7;color:#161b20;" +
        "font:600 14px system-ui,sans-serif;box-shadow:0 3px 12px #0005;";
      button.addEventListener(
        "click",
        () => {
          button.remove();
          onConfirm();
        },
        { once: true },
      );
      document.body.append(button);
      button.focus();
    },
  };
}

function browserHostFilePicker(): HostFilePicker {
  return confirmedHostFilePicker(
    browserNativeHostFilePicker(),
    browserHostFilePickerConfirmation(),
  );
}

/** Open the host picker and route every chosen file through the same
 * tokenised, kernel-mediated import path used by drag-and-drop. */
export function requestHostFilePicker(
  kernelWorker: { postMessage(msg: MainToKernel): void },
  picker: HostFilePicker = browserHostFilePicker(),
  interactionAllowed: () => boolean = () => true,
): void {
  if (!interactionAllowed()) {
    console.warn(
      "[pmos-bootstrap] host file picker blocked while storage recovery is active",
    );
    return;
  }
  picker.pick((files) => {
    // The user may leave the picker open while storage enters degraded mode.
    // Re-check at delivery time so selection cannot bypass the recovery gate.
    if (!interactionAllowed()) {
      console.warn(
        "[pmos-bootstrap] host file selection ignored while storage recovery is active",
      );
      return;
    }
    enqueueHostFileBatch(kernelWorker, files, interactionAllowed);
  }, interactionAllowed);
}

/** Injectable browser-download seam used to keep Blob/DOM behavior testable. */
export interface HostDownloadTarget {
  save(name: string, mime: string, bytes: Uint8Array): void;
}

function browserHostDownloadTarget(): HostDownloadTarget {
  return {
    save(name, mime, bytes): void {
      const owned = new Uint8Array(bytes.byteLength);
      owned.set(bytes);
      const blob = new Blob([owned], {
        type: mime || "application/octet-stream",
      });
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = name;
      anchor.hidden = true;
      document.body.append(anchor);
      anchor.click();
      anchor.remove();
      window.setTimeout(() => URL.revokeObjectURL(url), 0);
    },
  };
}

function safeHostDownloadName(name: string): string {
  const normalised = name.replaceAll("\\", "/");
  const leaf = normalised.split("/").pop()?.trim() ?? "";
  return leaf === "" || leaf === "." || leaf === ".." ? "download" : leaf;
}

/** Save kernel-owned bytes through the browser download surface. A defensive
 * owned copy prevents later mutation of a transferred worker buffer. */
export function saveHostDownload(
  name: string,
  mime: string,
  bytes: Uint8Array,
  target: HostDownloadTarget = browserHostDownloadTarget(),
): void {
  const owned = new Uint8Array(bytes.byteLength);
  owned.set(bytes);
  target.save(
    safeHostDownloadName(name),
    mime || "application/octet-stream",
    owned,
  );
}

/**
 * Per-tab token allocator for host-file drops. Bumps a counter
 * starting at 1; the 0 token is reserved as "no token" so a
 * userland `host_file_recv(0)` always fails. Counter wraps at
 * `0xFFFFFFFF` — never expected to wrap in practice (drag-drop
 * is a human-pace event), but the mod keeps the export's u32
 * argument well-defined under perversely-long sessions.
 */
let nextHostFileToken = 1;
function allocateHostFileToken(): number {
  const token = nextHostFileToken;
  nextHostFileToken =
    nextHostFileToken === 0xffffffff ? 1 : nextHostFileToken + 1;
  return token;
}

/**
 * T154: install the bootstrap-side drag-drop handler that turns
 * each dropped host file into a `host:dropped` MainToKernel
 * message. v1 caps each import at 16 MiB; the Worker copies accepted
 * bytes into the kernel through bounded heap-scratch chunks. Larger
 * drops are skipped with a console warning. Returns the registered
 * listeners so tests can fire them directly without a real DOM.
 */
export function installHostFileDropHandler(
  kernelWorker: { postMessage(msg: MainToKernel): void },
  target: DropTarget = window as unknown as DropTarget,
  interactionAllowed: () => boolean = () => true,
): {
  dragover: (e: HostDragEvent) => void;
  drop: (e: HostDragEvent) => void;
} {
  const dragover = (e: HostDragEvent): void => {
    // preventDefault on dragover is required by the DnD spec
    // for the subsequent drop event to fire on the same target.
    e.preventDefault();
  };
  const drop = (e: HostDragEvent): void => {
    e.preventDefault();
    if (!interactionAllowed()) {
      console.warn(
        "[pmos-bootstrap] host file drop ignored while storage recovery is active",
      );
      return;
    }
    const dt = e.dataTransfer;
    if (!dt || !dt.files) {
      return;
    }
    enqueueHostFileBatch(kernelWorker, dt.files, interactionAllowed);
  };
  target.addEventListener("dragover", dragover);
  target.addEventListener("drop", drop);
  return { dragover, drop };
}

function preadmitHostFileBatch(
  files: ArrayLike<HostDragFile>,
): { readonly files: HostDragFile[]; readonly bytes: number } | null {
  if (
    !Number.isSafeInteger(files.length) ||
    files.length < 0 ||
    files.length > HOST_FILE_IMPORT_MAX_FILES
  ) {
    console.warn(
      `[pmos-bootstrap] host import batch contains ${files.length} files; cap is ${HOST_FILE_IMPORT_MAX_FILES}; ignored`,
    );
    return null;
  }

  const admitted: HostDragFile[] = [];
  let aggregateBytes = 0;
  for (let i = 0; i < files.length; i += 1) {
    const file = files[i];
    if (!file) continue;
    if (!Number.isSafeInteger(file.size) || file.size < 0) {
      console.warn(
        `[pmos-bootstrap] host import ${file.name}: invalid browser File.size; ignored`,
      );
      continue;
    }
    if (file.size > HOST_FILE_IMPORT_MAX_BYTES) {
      console.warn(
        `[pmos-bootstrap] host import ${file.name}: ${file.size} bytes exceeds v1 cap of ${HOST_FILE_IMPORT_MAX_BYTES}; ignored`,
      );
      continue;
    }
    aggregateBytes += file.size;
    if (aggregateBytes > HOST_FILE_IMPORT_MAX_TOTAL_BYTES) {
      console.warn(
        `[pmos-bootstrap] host import batch exceeds v1 aggregate cap of ${HOST_FILE_IMPORT_MAX_TOTAL_BYTES} bytes; ignored`,
      );
      return null;
    }
    admitted.push(file);
  }
  return { files: admitted, bytes: aggregateBytes };
}

let pendingHostFileCount = 0;
let pendingHostFileBytes = 0;
let hostFileReadTail: Promise<void> = Promise.resolve();

function enqueueHostFileBatch(
  kernelWorker: { postMessage(msg: MainToKernel): void },
  files: ArrayLike<HostDragFile>,
  interactionAllowed: () => boolean,
): void {
  const admitted = preadmitHostFileBatch(files);
  if (admitted === null) return;
  const nextCount = pendingHostFileCount + admitted.files.length;
  const nextBytes = pendingHostFileBytes + admitted.bytes;
  if (
    nextCount > HOST_FILE_IMPORT_MAX_FILES ||
    nextBytes > HOST_FILE_IMPORT_MAX_TOTAL_BYTES
  ) {
    console.warn(
      "[pmos-bootstrap] host import queue is at the v1 live-import limit; selection ignored",
    );
    return;
  }
  pendingHostFileCount = nextCount;
  pendingHostFileBytes = nextBytes;

  hostFileReadTail = hostFileReadTail
    .then(async () => {
      try {
        // File.arrayBuffer() materialises the whole browser File. One shared
        // queue covers picker and drop batches, so overlapping user gestures can
        // never allocate their file bodies concurrently on main.
        for (const file of admitted.files) {
          if (!interactionAllowed()) {
            console.warn(
              "[pmos-bootstrap] queued host import cancelled while storage recovery is active",
            );
            return;
          }
          await deliverDropFile(kernelWorker, file);
        }
      } finally {
        pendingHostFileCount -= admitted.files.length;
        pendingHostFileBytes -= admitted.bytes;
      }
    })
    .catch((error: unknown) => {
      console.warn(
        `[pmos-bootstrap] host import queue failed: ${String(error)}`,
      );
    });
}

async function deliverDropFile(
  kernelWorker: { postMessage(msg: MainToKernel): void },
  file: HostDragFile,
): Promise<void> {
  try {
    if (file.size > HOST_FILE_IMPORT_MAX_BYTES) {
      console.warn(
        `[pmos-bootstrap] host import ${file.name}: ${file.size} bytes exceeds v1 cap of ${HOST_FILE_IMPORT_MAX_BYTES}; ignored`,
      );
      return;
    }
    const buf = await file.arrayBuffer();
    const bytes = new Uint8Array(buf);
    if (bytes.length > HOST_FILE_IMPORT_MAX_BYTES) {
      console.warn(
        `[pmos-bootstrap] host drop ${file.name}: ${bytes.length} bytes exceeds v1 cap of ${HOST_FILE_IMPORT_MAX_BYTES}; ignored`,
      );
      return;
    }
    if (bytes.length !== file.size) {
      console.warn(
        `[pmos-bootstrap] host import ${file.name}: browser size changed from ${file.size} to ${bytes.length}; ignored`,
      );
      return;
    }
    const token = allocateHostFileToken();
    kernelWorker.postMessage({
      kind: "host:dropped",
      token,
      name: file.name,
      mime: file.type ?? "application/octet-stream",
      bytes,
    });
  } catch (e) {
    console.warn(`[pmos-bootstrap] host drop failed: ${(e as Error).message}`);
  }
}

/**
 * Subset of `EventTarget` needed by
 * [`installBeforeUnloadSync`] — production code passes `window`,
 * tests pass a stub that captures the listener and fires it on
 * demand.
 */
export interface BeforeUnloadTarget {
  addEventListener(type: "beforeunload", listener: () => void): void;
}

/**
 * Secondary `beforeunload` fallback for [`installPagehideSync`].
 *
 * In every modern browser, `pagehide` is the lifecycle event the
 * platform guarantees fires on tab close. But the platform also
 * documents two cases where `pagehide` may NOT fire:
 *
 *   * The browser was force-killed (OS kill, OOM, hard crash) —
 *     no JS event of any kind runs. No fallback can rescue this.
 *   * Some chromium-derived embeddings (older webview builds) skip
 *     `pagehide` on certain navigation paths. `beforeunload` runs
 *     in those, and is the documented secondary fallback.
 *
 * Both events fire identically into the same `sync:request` post,
 * so the worst case is a duplicate flush — which the kernel's
 * `kernel_sync_all` handles cleanly: a flush against a non-dirty
 * VFS is a constant-time no-op (the dirty set is empty).
 *
 * Note: we deliberately do NOT call `event.preventDefault()` or
 * set `returnValue` — the goal is a silent flush, not a "are you
 * sure?" UI prompt.
 */
export function installBeforeUnloadSync(
  kernelWorker: { postMessage(msg: MainToKernel): void },
  target: BeforeUnloadTarget = window,
): () => void {
  const listener = (): void => {
    kernelWorker.postMessage({ kind: "sync:request" });
  };
  target.addEventListener("beforeunload", listener);
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
  interface RoutedEntry extends SpawnedEntry {
    sabPublished: boolean;
    readonly onMessage: (ev: { data: UserToMain }) => void;
    readonly onError: (ev: { message?: string }) => void;
  }

  const live = new Map<number, RoutedEntry>();
  const maxLiveWorkers = deps.maxLiveWorkers ?? MAX_LIVE_USER_WORKERS;

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
    if (trap !== undefined) {
      console.error(
        `[pmos-bootstrap] user worker crashed pid=${pid}: ${sanitizeUserWorkerTrap(trap)}`,
      );
    }
    live.delete(pid);
    deps.onLiveWorkersChanged?.(live.size);
    entry.worker.removeEventListener("message", entry.onMessage);
    entry.worker.removeEventListener("error", entry.onError);
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
      if (msg.kind === "proc:terminate") {
        // The kernel has already made the pid terminal. This host
        // acknowledgement releases the Worker and routing entry;
        // its proc:exited reply is deliberately idempotent against
        // the kernel-side SIGKILL transition.
        reap(msg.pid, 128 + msg.signal, undefined);
        return;
      }
      if (msg.kind !== "proc:spawn") {
        return;
      }
      // A repeated publication for one pid is never legitimate. Do
      // not replace the existing Worker — doing so would orphan the
      // first instance and split one kernel identity across workers.
      if (live.has(msg.pid)) {
        return;
      }
      // The kernel rejects the corresponding proc_spawn before allocating a
      // pid, so production should never reach this branch. Keep the browser
      // substrate independently bounded in case of a corrupt or mismatched
      // kernel image. Crucially, reject before allocating either the SAB or
      // the Worker.
      if (live.size >= maxLiveWorkers) {
        deps.kernelWorker.postMessage({
          kind: "proc:exited",
          pid: msg.pid,
          code: -1,
          trap: `user worker limit exceeded (${maxLiveWorkers})`,
        });
        return;
      }

      let worker: UserWorkerLike | undefined;
      try {
        const sab = deps.allocSab();
        worker = deps.workerFactory();
        const onMessage = (ev: { data: UserToMain }): void => {
          const m = ev.data;
          if (m.pid !== msg.pid) {
            return;
          }
          if (m.kind === "booted") {
            const entry = live.get(msg.pid);
            if (!entry || entry.sabPublished) {
              return;
            }
            entry.sabPublished = true;
            deps.kernelWorker.postMessage({
              kind: "proc:sab",
              pid: msg.pid,
              sab,
            });
            return;
          }
          if (m.kind === "memory") {
            recordMemory(msg.pid, m.bytes);
            return;
          }
          if (m.kind === "exited" && m.memoryBytes !== undefined) {
            recordMemory(msg.pid, m.memoryBytes);
          }
          if (m.kind === "exited") {
            reap(msg.pid, m.code, m.trap);
          }
        };
        const onError = (ev: { message?: string }): void => {
          reap(msg.pid, -1, ev.message ?? "user worker error");
        };
        live.set(msg.pid, {
          worker,
          sab,
          sabPublished: false,
          onMessage,
          onError,
        });
        deps.onLiveWorkersChanged?.(live.size);
        worker.addEventListener("message", onMessage);
        worker.addEventListener("error", onError);
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
      } catch (error) {
        const entry = live.get(msg.pid);
        if (entry) {
          live.delete(msg.pid);
          deps.onLiveWorkersChanged?.(live.size);
          entry.worker.removeEventListener("message", entry.onMessage);
          entry.worker.removeEventListener("error", entry.onError);
          entry.worker.terminate();
        } else {
          worker?.terminate();
        }
        const trap = error instanceof Error ? error.message : String(error);
        deps.kernelWorker.postMessage({
          kind: "proc:exited",
          pid: msg.pid,
          code: -1,
          trap: `user worker boot failed: ${trap}`,
        });
      }
    },
    terminateAll(): void {
      for (const [pid, entry] of live) {
        entry.worker.removeEventListener("message", entry.onMessage);
        entry.worker.removeEventListener("error", entry.onError);
        entry.worker.terminate();
        live.delete(pid);
      }
      deps.onLiveWorkersChanged?.(live.size);
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
