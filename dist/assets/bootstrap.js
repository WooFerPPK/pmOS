// src/console-host.ts
var ConsoleHost = class {
  worker;
  outputHandlers = [];
  lifecycleHandlers = [];
  isReady = false;
  terminated = false;
  queuedInput = [];
  constructor(options) {
    this.worker = options.worker;
    this.worker.addEventListener("message", (ev) => {
      this.handleMessage(ev.data);
    });
    this.worker.postMessage({ kind: "boot", config: options.bootConfig });
  }
  /** True once the Worker has posted `ready`. */
  get ready() {
    return this.isReady;
  }
  /** Send raw bytes as console input. */
  sendInput(bytes) {
    if (this.terminated) {
      return;
    }
    const copy = new Uint8Array(bytes.byteLength);
    copy.set(bytes);
    if (!this.isReady) {
      this.queuedInput.push(copy);
      return;
    }
    this.worker.postMessage({ kind: "console:input", bytes: copy });
  }
  /** Convenience: encode a UTF-8 string and send it as input. */
  sendLine(line) {
    const bytes = new TextEncoder().encode(line);
    this.sendInput(bytes);
  }
  /** Subscribe to output bytes. Handlers are called in registration order. */
  onOutput(handler) {
    this.outputHandlers.push(handler);
  }
  /** Subscribe to lifecycle events. */
  onLifecycle(handler) {
    this.lifecycleHandlers.push(handler);
  }
  /** Post a shutdown message and terminate the worker. */
  shutdown() {
    if (this.terminated) {
      return;
    }
    this.terminated = true;
    this.worker.postMessage({ kind: "shutdown" });
    this.worker.terminate();
    this.isReady = false;
  }
  handleMessage(msg) {
    switch (msg.kind) {
      case "ready": {
        this.isReady = true;
        for (const q of this.queuedInput) {
          this.worker.postMessage({ kind: "console:input", bytes: q });
        }
        this.queuedInput = [];
        for (const h of this.lifecycleHandlers) {
          h({ kind: "ready" });
        }
        return;
      }
      case "console:write": {
        for (const h of this.outputHandlers) {
          h(msg.bytes);
        }
        return;
      }
      case "panic": {
        this.isReady = false;
        for (const h of this.lifecycleHandlers) {
          h({ kind: "panic", message: msg.message });
        }
        return;
      }
    }
  }
};

// src/console-check.ts
function runEchoCheck(host, options) {
  return new Promise((resolve) => {
    const started = options.now();
    let accumulator = "";
    let timer = null;
    let settled = false;
    const settle = (result) => {
      if (settled) {
        return;
      }
      settled = true;
      if (timer !== null) {
        options.cancelTimer(timer);
        timer = null;
      }
      resolve(result);
    };
    host.onOutput((bytes) => {
      if (settled) {
        return;
      }
      accumulator += new TextDecoder().decode(bytes);
      if (accumulator.length < options.expect.length) {
        return;
      }
      if (accumulator === options.expect) {
        settle({ ok: true, roundtripMs: options.now() - started });
      } else {
        settle({ ok: false, reason: "mismatch", got: accumulator });
      }
    });
    host.onLifecycle((event) => {
      if (settled) {
        return;
      }
      if (event.kind === "panic") {
        settle({ ok: false, reason: "panic", message: event.message });
      }
    });
    host.sendLine(options.input);
    timer = options.setTimer(() => {
      settle({ ok: false, reason: "timeout" });
    }, options.timeoutMs);
  });
}

// src/fb-host.ts
var FbHost = class {
  frameHandlers = [];
  modeHandlers = [];
  currentMode = null;
  blitCount = 0;
  constructor(options) {
    options.worker.addEventListener("message", (ev) => {
      this.handleMessage(ev.data);
    });
  }
  /** Most recent mode set, or null if none has been set. */
  get mode() {
    return this.currentMode;
  }
  /** Number of blits observed since construction. */
  get blitsObserved() {
    return this.blitCount;
  }
  /** Subscribe to blit events. */
  onFrame(handler) {
    this.frameHandlers.push(handler);
  }
  /** Subscribe to mode-change events. */
  onModeChange(handler) {
    this.modeHandlers.push(handler);
  }
  handleMessage(msg) {
    switch (msg.kind) {
      case "fb:set-mode": {
        const mode = { width: msg.width, height: msg.height };
        this.currentMode = mode;
        for (const h of this.modeHandlers) {
          h(mode);
        }
        return;
      }
      case "fb:blit": {
        this.blitCount += 1;
        const frame = {
          width: msg.width,
          height: msg.height,
          rgba: msg.rgba
        };
        for (const h of this.frameHandlers) {
          h(frame);
        }
        return;
      }
      default:
        return;
    }
  }
};

// src/fb-renderer.ts
var FbRenderer = class {
  canvas;
  offscreenFactory;
  imageDataFactory;
  offscreen = null;
  offscreenCtx = null;
  handlers = [];
  currentMode = null;
  /**
   * Number of frames painted since construction. Read by tests
   * to assert the render loop ran the expected number of times
   * without needing a canvas-pixel comparison.
   */
  presentsCompleted = 0;
  /**
   * Tracks whether the renderer is using the OffscreenCanvas
   * fast path. Read by tests; the value is decided by
   * `setMode` based on the factory's return value.
   */
  usingFastPath = false;
  constructor(options) {
    this.canvas = options.canvas;
    this.offscreenFactory = options.offscreenCanvasFactory ?? defaultOffscreenFactory;
    this.imageDataFactory = options.imageDataFactory ?? defaultImageDataFactory;
  }
  /** Subscribe to present-complete events. */
  onPresentComplete(handler) {
    this.handlers.push(handler);
  }
  /**
   * Resize the canvas + (re-)create the offscreen surface for
   * the new mode. Idempotent: passing the same geometry as the
   * current mode is a no-op.
   */
  setMode(mode) {
    if (this.currentMode !== null && this.currentMode.width === mode.width && this.currentMode.height === mode.height) {
      return;
    }
    this.currentMode = mode;
    this.canvas.width = mode.width;
    this.canvas.height = mode.height;
    const offscreen = this.offscreenFactory(mode.width, mode.height);
    if (offscreen !== null) {
      this.offscreen = offscreen;
      this.offscreenCtx = offscreen.getContext("2d");
      this.usingFastPath = this.offscreenCtx !== null;
    } else {
      this.offscreen = null;
      this.offscreenCtx = null;
      this.usingFastPath = false;
    }
  }
  /**
   * Paint one RGBA8 frame. The renderer assumes the frame's
   * geometry matches the most recent `setMode`; mismatched
   * frames are dropped so a stale blit doesn't smear wrong-
   * sized pixels into the canvas.
   */
  paintFrame(frame) {
    if (this.currentMode === null) {
      return;
    }
    if (frame.width !== this.currentMode.width || frame.height !== this.currentMode.height) {
      return;
    }
    const rgba = new Uint8ClampedArray(
      frame.rgba.buffer,
      frame.rgba.byteOffset,
      frame.rgba.byteLength
    );
    const imageData = this.imageDataFactory(rgba, frame.width, frame.height);
    const ctx = this.canvas.getContext("2d");
    if (ctx !== null) {
      ctx.putImageData(imageData, 0, 0);
    }
    this.presentsCompleted += 1;
    for (const h of this.handlers) {
      h();
    }
  }
};
function defaultOffscreenFactory(width, height) {
  const Ctor = globalThis.OffscreenCanvas;
  if (Ctor === void 0) return null;
  try {
    const oc = new Ctor(width, height);
    if (typeof oc.transferToImageBitmap !== "function") {
      return null;
    }
    return oc;
  } catch {
    return null;
  }
}
function defaultImageDataFactory(data, width, height) {
  const Ctor = globalThis.ImageData;
  if (Ctor !== void 0) {
    try {
      return new Ctor(data, width, height);
    } catch {
    }
  }
  return { width, height, data };
}

// src/shared/sab-layout.ts
var SAB_SIZE = 65536;

// src/shared/input-proto.ts
var MOUSE_EVENT_SIZE = 20;
var MouseEventKind = {
  /** Pointer moved to (x, y) in screen space. */
  Motion: 0,
  /** A mouse button was pressed or released at (x, y). */
  Button: 1,
  /** Wheel scrolled by `(button, state)` reinterpreted as
   *  `(deltaX, deltaY)` — see `packMouseWheel`. v1 reserves
   *  this discriminant for the wheel-scroll path the
   *  display server's window manager will route to focus
   *  windows. */
  Wheel: 2
};
var MouseButtonState = {
  Released: 0,
  Pressed: 1
};
var MouseButton = {
  Left: 1,
  Right: 2,
  Middle: 3
};
function packMouseMotion(x, y) {
  return packMouseEvent(MouseEventKind.Motion, x, y, 0, MouseButtonState.Released);
}
function packMouseButton(x, y, button, state) {
  return packMouseEvent(MouseEventKind.Button, x, y, button, state);
}
function packMouseEvent(kind, x, y, button, state) {
  const out = new Uint8Array(MOUSE_EVENT_SIZE);
  const view = new DataView(out.buffer);
  view.setUint32(0, kind, true);
  view.setInt32(4, x, true);
  view.setInt32(8, y, true);
  view.setUint32(12, button, true);
  view.setUint32(16, state, true);
  return out;
}

// src/bootstrap.ts
var BOOT_VERSION = "0.1.0-demo";
function hasSharedArrayBuffer() {
  return typeof SharedArrayBuffer !== "undefined";
}
function hasAtomicsWait() {
  return typeof Atomics !== "undefined" && typeof Atomics.wait === "function";
}
function isCrossOriginIsolated() {
  return typeof crossOriginIsolated !== "undefined" && crossOriginIsolated;
}
function hasOpfs() {
  return typeof navigator !== "undefined" && typeof navigator.storage !== "undefined" && typeof navigator.storage.getDirectory === "function";
}
function registerServiceWorker(scriptURL = "/assets/sw.js", options = { type: "module" }, container) {
  const target = container ?? (typeof navigator !== "undefined" ? navigator.serviceWorker : void 0);
  if (target === void 0) {
    return Promise.resolve(null);
  }
  return target.register(scriptURL, options);
}
function hasServiceWorker() {
  return typeof navigator !== "undefined" && "serviceWorker" in navigator;
}
function hasOffscreenCanvas() {
  return typeof OffscreenCanvas !== "undefined";
}
var PALETTE = {
  bg: "#0a0e14",
  dim: "#1a1f26",
  fg: "#e6e6e6",
  accent: "#7cb7ff",
  ok: "#6ddf6d",
  warn: "#f2c045",
  fail: "#ff6b6b",
  muted: "#808591"
};
function setupCanvas() {
  const canvas = document.getElementById("pmos-fb");
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
function paintBoot(c, rows, animationFrame) {
  const { ctx, canvas, dpr } = c;
  const W = canvas.width;
  const H = canvas.height;
  ctx.fillStyle = PALETTE.bg;
  ctx.fillRect(0, 0, W, H);
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
    "browser-hosted operating system \u2014 demo build",
    padX,
    padY + 18 * dpr
  );
  const rowsX = padX;
  const rowsY = padY + 70 * dpr;
  for (let i = 0; i < rows.length; i++) {
    const row = rows[i];
    const y = rowsY + i * lineHeight;
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
        tag = ` ${"*".repeat(animationFrame % 3 + 1).padEnd(3, ".")}  `;
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
    ctx.fillStyle = row.status === "fail" ? PALETTE.fail : row.status === "warn" || row.status === "stalled" ? PALETTE.warn : PALETTE.fg;
    ctx.fillText(row.label, rowsX + 90 * dpr, y);
    if (row.detail) {
      ctx.fillStyle = PALETTE.muted;
      ctx.fillText(row.detail, rowsX + 340 * dpr, y);
    }
  }
  const footerY = H - padY;
  ctx.font = monoSmall;
  ctx.fillStyle = PALETTE.muted;
  ctx.fillText(
    "This is the PMos boot-screen demo. The kernel WASM is not yet",
    padX,
    footerY - 3 * lineHeight
  );
  ctx.fillText(
    "compiled \u2014 reaching the desktop requires running `just build`",
    padX,
    footerY - 2 * lineHeight
  );
  ctx.fillText(
    "against the PMos source tree (Rust + Node + wasm32 target).",
    padX,
    footerY - 1 * lineHeight
  );
  ctx.fillText(
    "Source: https://github.com/example/pmos  \u2022  specs/001-browser-os-v1/",
    padX,
    footerY
  );
}
function main() {
  console.log(`[pmos-bootstrap] PMos ${BOOT_VERSION} starting`);
  if (!window.location.hash.includes("mock-kernel")) {
    const hash = window.location.hash;
    let bootBinary;
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
  const rows = [
    { label: "Cross-origin isolation (COOP/COEP)", status: "pending", detail: "" },
    { label: "SharedArrayBuffer", status: "pending", detail: "" },
    { label: "Atomics.wait", status: "pending", detail: "" },
    { label: "Origin Private Filesystem (OPFS)", status: "pending", detail: "" },
    { label: "Service worker", status: "pending", detail: "" },
    { label: "OffscreenCanvas", status: "pending", detail: "" },
    { label: "Kernel WASM load (/assets/kernel.wasm)", status: "pending", detail: "" },
    { label: "Kernel worker + console echo", status: "pending", detail: "" },
    { label: "Display server", status: "pending", detail: "" },
    { label: "Desktop shell", status: "pending", detail: "" }
  ];
  let canvas;
  try {
    canvas = setupCanvas();
  } catch (e) {
    console.error("[pmos-bootstrap] cannot set up canvas:", e);
    showFallbackMessage(String(e));
    return;
  }
  let frame = 0;
  let splashPainted = false;
  const repaint = () => {
    if (splashPainted) {
      return;
    }
    paintBoot(canvas, rows, frame++);
  };
  repaint();
  const step = (i, delay, fn) => {
    setTimeout(() => {
      rows[i].status = "running";
      repaint();
      setTimeout(() => {
        fn();
        repaint();
      }, 200);
    }, delay);
  };
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
  let latestFrame = null;
  let terminalStarted = false;
  const startTerminalMode = (session) => {
    if (terminalStarted) {
      return;
    }
    terminalStarted = true;
    if (repaintInterval !== null) {
      clearInterval(repaintInterval);
      repaintInterval = null;
    }
    splashPainted = true;
    if (latestFrame) {
      paintBlitToCanvasFullscreen(canvas, latestFrame);
    }
    session.fb.onFrame((frame_) => {
      paintBlitToCanvasFullscreen(canvas, frame_);
    });
    window.addEventListener("keydown", (event) => {
      const bytes = keyToBytes(event.key);
      if (bytes === null) {
        return;
      }
      event.preventDefault();
      session.console.sendInput(bytes);
    });
    const sendMouse = (msg) => session.worker.postMessage(msg);
    const canvasEl = canvas.canvas;
    const toFbCoords = (event) => {
      const frame2 = latestFrame;
      if (!frame2) {
        return null;
      }
      const rect = canvasEl.getBoundingClientRect();
      const canvasCx = (event.clientX - rect.left) * canvas.dpr;
      const canvasCy = (event.clientY - rect.top) * canvas.dpr;
      const fbW = frame2.width;
      const fbH = frame2.height;
      const canvasW = canvas.canvas.width;
      const canvasH = canvas.canvas.height;
      const scale = Math.max(
        1,
        Math.floor(Math.min(canvasW / fbW, canvasH / fbH))
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
    canvasEl.addEventListener("pointermove", (event) => {
      const coords = toFbCoords(event);
      if (!coords) return;
      const [x, y] = coords;
      sendMouse({ kind: "input:mouse", bytes: packMouseMotion(x, y) });
    });
    canvasEl.addEventListener("pointerdown", (event) => {
      const coords = toFbCoords(event);
      if (!coords) return;
      const [x, y] = coords;
      const button = domButtonToProtoButton(event.button);
      sendMouse({
        kind: "input:mouse",
        bytes: packMouseButton(x, y, button, MouseButtonState.Pressed)
      });
    });
    canvasEl.addEventListener("pointerup", (event) => {
      const coords = toFbCoords(event);
      if (!coords) return;
      const [x, y] = coords;
      const button = domButtonToProtoButton(event.button);
      sendMouse({
        kind: "input:mouse",
        bytes: packMouseButton(x, y, button, MouseButtonState.Released)
      });
    });
  };
  function domButtonToProtoButton(domButton) {
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
    let session;
    try {
      session = createKernelSession();
    } catch (e) {
      rows[7].status = "fail";
      rows[7].detail = `Worker spawn: ${String(e).slice(0, 48)}`;
      markShellRowsStalled();
      repaint();
      return;
    }
    session.console.onLifecycle((event) => {
      if (event.kind === "panic") {
        showPanic(event.message);
      }
    });
    session.fb.onFrame((frame_) => {
      latestFrame = frame_;
    });
    let splashFlashed = false;
    session.fb.onFrame((frame_) => {
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
      timeoutMs: 2e3,
      now: () => Date.now(),
      setTimer: (h, ms) => globalThis.setTimeout(h, ms),
      cancelTimer: (h) => globalThis.clearTimeout(h)
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
      if (result.ok && !splashFlashed) {
        window.setTimeout(() => startTerminalMode(session), 400);
      }
    });
  });
  const markShellRowsStalled = () => {
    rows[8].status = "stalled";
    rows[8].detail = "awaits T085+ wasm kernel";
    rows[9].status = "stalled";
    rows[9].detail = "awaits T085+ wasm kernel";
  };
  let repaintInterval = setInterval(
    repaint,
    300
  );
  window.addEventListener("error", (event) => showPanic(event.message));
  window.addEventListener(
    "unhandledrejection",
    (event) => showPanic(String(event.reason))
  );
}
async function attemptKernelFetch() {
  try {
    const res = await fetch("/assets/kernel.wasm", { method: "HEAD" });
    if (!res.ok) {
      return { ok: false, reason: `HTTP ${res.status} \u2014 not yet built` };
    }
    const size = Number(res.headers.get("content-length") || "0");
    return { ok: true, size };
  } catch (e) {
    return { ok: false, reason: `fetch failed: ${String(e)}` };
  }
}
function createKernelSession() {
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
        ""
      ]
    }
  });
  const fbHost = new FbHost({ worker });
  worker.addEventListener(
    "error",
    (event) => {
      const message = event.message || event.error?.toString?.() || "worker load error";
    },
    { once: true }
  );
  return { worker, console: consoleHost, fb: fbHost };
}
function keyToBytes(key) {
  if (key === "Enter") {
    return new Uint8Array([10]);
  }
  if (key === "Backspace") {
    return new Uint8Array([127]);
  }
  if (key.length === 1) {
    const code = key.charCodeAt(0);
    if (code >= 32 && code !== 127) {
      return new TextEncoder().encode(key);
    }
  }
  return null;
}
function paintBlitToCanvas(c, frame_) {
  const { ctx, canvas, dpr } = c;
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
    frame_.height
  );
  tctx.putImageData(imageData, 0, 0);
  const scale = Math.min(W / frame_.width, H / frame_.height) * 0.8;
  const dw = Math.floor(frame_.width * scale);
  const dh = Math.floor(frame_.height * scale);
  const dx = Math.floor((W - dw) / 2);
  const dy = Math.floor((H - dh) / 2) - Math.floor(20 * dpr);
  ctx.fillStyle = PALETTE.bg;
  ctx.fillRect(0, 0, W, H);
  ctx.imageSmoothingEnabled = false;
  ctx.drawImage(tmp, dx, dy, dw, dh);
  ctx.font = `${14 * dpr}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  ctx.fillStyle = PALETTE.accent;
  ctx.textAlign = "center";
  ctx.fillText(
    "PMos kernel worker \u2014 framebuffer blit from MockKernel",
    W / 2,
    dy + dh + Math.floor(36 * dpr)
  );
  ctx.fillStyle = PALETTE.muted;
  ctx.font = `${12 * dpr}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  ctx.fillText(
    `${frame_.width}\xD7${frame_.height} RGBA8 \u2022 via /dev/fb0 driver`,
    W / 2,
    dy + dh + Math.floor(56 * dpr)
  );
  ctx.textAlign = "start";
}
function paintBlitToCanvasFullscreen(c, frame_) {
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
    frame_.height
  );
  tctx.putImageData(imageData, 0, 0);
  const scale = Math.max(
    1,
    Math.floor(Math.min(W / frame_.width, H / frame_.height))
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
function runRealKernelMode(bootBinary) {
  console.log(
    `[pmos-bootstrap] real-kernel mode enabled via URL (bootBinary=${bootBinary})`
  );
  const isGuiBoot = bootBinary === "/bin/init-desktop";
  const consoleEl = mountRealKernelConsole(isGuiBoot);
  const splash = isGuiBoot ? mountBootSplash() : null;
  const worker = new Worker("/assets/kernel-worker.js", { type: "module" });
  if (splash) {
    splash.markStarted("Browser environment ready");
    splash.markStarted("Kernel worker spawned");
  }
  let guiFbMode = null;
  if (isGuiBoot) {
    const canvas = document.getElementById("pmos-fb");
    if (canvas instanceof HTMLCanvasElement) {
      const fbHost = new FbHost({ worker });
      const renderer = new FbRenderer({ canvas });
      fbHost.onModeChange((mode) => {
        renderer.setMode(mode);
        guiFbMode = mode;
        document.body.classList.add("pmos-gui-mode");
        canvas.style.width = `${mode.width}px`;
        canvas.style.height = `${mode.height}px`;
        if (splash) {
          splash.markStarted(`Framebuffer mode set ${mode.width}\xD7${mode.height}`);
        }
      });
      let firstFrameSeen = false;
      fbHost.onFrame((frame) => {
        renderer.paintFrame(frame);
        if (!firstFrameSeen) {
          firstFrameSeen = true;
          if (splash) {
            splash.markStarted("Desktop wallpaper rendered");
            splash.dismiss();
          }
        }
      });
      const toFbCoords = (event) => {
        if (!guiFbMode) return null;
        const rect = canvas.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return null;
        const fx = (event.clientX - rect.left) / rect.width * guiFbMode.width;
        const fy = (event.clientY - rect.top) / rect.height * guiFbMode.height;
        const x = Math.max(0, Math.min(guiFbMode.width - 1, Math.floor(fx)));
        const y = Math.max(0, Math.min(guiFbMode.height - 1, Math.floor(fy)));
        return [x, y];
      };
      const domButtonToProtoButton = (domButton) => {
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
        const coords = toFbCoords(event);
        if (!coords) return;
        const [x, y] = coords;
        worker.postMessage({
          kind: "input:mouse",
          bytes: packMouseMotion(x, y)
        });
      });
      canvas.addEventListener("pointerdown", (event) => {
        const coords = toFbCoords(event);
        if (!coords) return;
        const [x, y] = coords;
        const button = domButtonToProtoButton(event.button);
        worker.postMessage({
          kind: "input:mouse",
          bytes: packMouseButton(x, y, button, MouseButtonState.Pressed)
        });
      });
      canvas.addEventListener("pointerup", (event) => {
        const coords = toFbCoords(event);
        if (!coords) return;
        const [x, y] = coords;
        const button = domButtonToProtoButton(event.button);
        worker.postMessage({
          kind: "input:mouse",
          bytes: packMouseButton(x, y, button, MouseButtonState.Released)
        });
      });
      canvas.addEventListener("contextmenu", (event) => {
        event.preventDefault();
      });
    }
  }
  let kernelWakeSlot = null;
  let peakLiveWorkers = 0;
  const router = createSpawnRouter({
    kernelWorker: worker,
    workerFactory: () => new Worker("/assets/user-worker.js", { type: "module" }),
    allocSab: () => {
      try {
        return new SharedArrayBuffer(SAB_SIZE);
      } catch {
        return new ArrayBuffer(SAB_SIZE);
      }
    },
    getKernelWakeSlot: () => kernelWakeSlot
  });
  worker.addEventListener("message", (ev) => {
    const msg = ev.data;
    if (msg.kind === "kernel:wake-slot") {
      kernelWakeSlot = msg.sab;
      document.body.dataset["pmosWakeSlotReady"] = "1";
      return;
    }
    router.handleKernelMessage(msg);
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
      bootBinary
    }
  });
  const crashScreen = isGuiBoot ? mountCrashScreen() : null;
  const recentLines = [];
  const RECENT_LINES_CAP = 80;
  consoleHost.onOutput((bytes) => {
    const text = new TextDecoder().decode(bytes);
    consoleEl.textContent += text;
    console.log(`[real-kernel] ${text.replace(/\n$/, "")}`);
    if (splash) {
      splash.observeConsoleLine(text);
    }
    if (crashScreen) {
      crashScreen.observeConsoleLine(text, recentLines, RECENT_LINES_CAP);
    }
  });
  consoleHost.onLifecycle((event) => {
    if (event.kind === "ready") {
      console.log("[pmos-bootstrap] real kernel ready");
      if (splash) {
        splash.markStarted("Kernel ready");
      }
    } else if (event.kind === "panic") {
      console.error(`[pmos-bootstrap] real kernel panic: ${event.message}`);
      consoleEl.textContent += `
[panic] ${event.message}
`;
      if (splash) {
        splash.markFailed(`Kernel panic: ${event.message}`);
      }
      if (crashScreen) {
        crashScreen.show({
          title: "Kernel panic",
          subtitle: event.message,
          recent: recentLines
        });
      }
    }
  });
  worker.addEventListener("error", (ev) => {
    const msg = ev.message || "(unknown worker error)";
    console.error(`[pmos-bootstrap] kernel worker error: ${msg}`);
    if (crashScreen) {
      crashScreen.show({
        title: "Kernel worker crashed",
        subtitle: msg,
        recent: recentLines
      });
    }
  });
  worker.addEventListener("messageerror", (ev) => {
    console.error(`[pmos-bootstrap] kernel worker messageerror`, ev);
    if (crashScreen) {
      crashScreen.show({
        title: "Kernel worker message decode failed",
        subtitle: "MessageEvent.data could not be cloned",
        recent: recentLines
      });
    }
  });
  window.addEventListener("unhandledrejection", (ev) => {
    const reason = String(ev.reason ?? "(unknown)");
    console.error(`[pmos-bootstrap] unhandled rejection: ${reason}`);
    if (crashScreen) {
      crashScreen.show({
        title: "Bootstrap promise rejected",
        subtitle: reason,
        recent: recentLines
      });
    }
  });
  document.addEventListener("keydown", (event) => {
    if (event.ctrlKey || event.metaKey || event.altKey) {
      return;
    }
    const bytes = keyToBytes(event.key);
    if (bytes === null) {
      return;
    }
    event.preventDefault();
    const msg = { kind: "input:kbd", bytes };
    worker.postMessage(msg);
  });
  installPagehideSync(worker);
  installPeriodicSync(worker);
}
function mountRealKernelConsole(gui = false) {
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
      "z-index: 100"
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
      "overflow-wrap: anywhere"
    ].join("; ");
  }
  document.body.appendChild(pre);
  return pre;
}
function mountBootSplash() {
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
    "transition: opacity 350ms ease-out",
    "opacity: 1",
    // Don't intercept clicks; once the canvas is alive the
    // user's clicks need to reach it. (The splash dismiss
    // animation overlaps for ~350ms; pointer-events:none
    // makes the splash transparent to input during that
    // window.)
    "pointer-events: none"
  ].join("; ");
  const titleBlock = document.createElement("div");
  titleBlock.style.cssText = [
    "margin-bottom: 2.5rem",
    "text-align: center"
  ].join("; ");
  const title = document.createElement("div");
  title.textContent = "PMos";
  title.style.cssText = [
    "font-size: 56px",
    "font-weight: 200",
    "letter-spacing: 0.4em",
    "color: #ffffff",
    "margin-bottom: 0.4rem"
  ].join("; ");
  const subtitle = document.createElement("div");
  subtitle.textContent = "browser-native operating system";
  subtitle.style.cssText = [
    "font-size: 12px",
    "letter-spacing: 0.25em",
    "text-transform: uppercase",
    "color: #5b7fa9"
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
    "line-height: 1.9"
  ].join("; ");
  overlay.appendChild(titleBlock);
  overlay.appendChild(list);
  document.body.appendChild(overlay);
  const steps = [
    { label: "Browser environment ready", consoleHints: [], startedAt: null, failed: false, failureMessage: null },
    { label: "Kernel worker spawned", consoleHints: [], startedAt: null, failed: false, failureMessage: null },
    { label: "Kernel ready", consoleHints: [], startedAt: null, failed: false, failureMessage: null },
    { label: "init (PID 1) running", consoleHints: ["init-desktop starting"], startedAt: null, failed: false, failureMessage: null },
    { label: "display-server spawned", consoleHints: ["init-desktop spawned display-server"], startedAt: null, failed: false, failureMessage: null },
    { label: "shell spawned", consoleHints: ["init-desktop spawned shell"], startedAt: null, failed: false, failureMessage: null },
    { label: "init supervising children", consoleHints: ["init-desktop entering supervision loop"], startedAt: null, failed: false, failureMessage: null },
    { label: "display-server bound /run/display", consoleHints: ["display-server starting"], startedAt: null, failed: false, failureMessage: null },
    { label: "shell connected to display", consoleHints: ["shell: connected to /run/display"], startedAt: null, failed: false, failureMessage: null },
    { label: "shell handshake complete", consoleHints: ["display-server served client 0"], startedAt: null, failed: false, failureMessage: null },
    { label: "Framebuffer mode set 1024\xD7768", consoleHints: [], startedAt: null, failed: false, failureMessage: null },
    { label: "Desktop wallpaper rendered", consoleHints: [], startedAt: null, failed: false, failureMessage: null }
  ];
  const rows = steps.map((step) => {
    const row = document.createElement("div");
    row.style.cssText = [
      "display: flex",
      "align-items: baseline",
      "gap: 0.75rem",
      "color: #5b7fa9",
      "transition: color 150ms ease"
    ].join("; ");
    const icon = document.createElement("span");
    icon.textContent = "\u25CB";
    icon.style.cssText = "width: 1.2rem; text-align: center; font-size: 14px;";
    const label = document.createElement("span");
    label.textContent = step.label;
    label.style.cssText = "flex: 1;";
    const elapsed = document.createElement("span");
    elapsed.style.cssText = "color: #4a5f7a; font-size: 11px; min-width: 4.5rem; text-align: right;";
    row.appendChild(icon);
    row.appendChild(label);
    row.appendChild(elapsed);
    list.appendChild(row);
    return row;
  });
  function findStep(label) {
    return steps.findIndex((s) => s.label === label);
  }
  function paintRow(i) {
    const step = steps[i];
    const row = rows[i];
    if (!step || !row) return;
    const icon = row.children[0];
    const elapsed = row.children[2];
    if (step.failed) {
      icon.textContent = "\u2717";
      icon.style.color = "#ff7a7a";
      row.style.color = "#ff9c9c";
      elapsed.textContent = step.failureMessage ?? "failed";
      return;
    }
    if (step.startedAt !== null) {
      icon.textContent = "\u2713";
      icon.style.color = "#7ad0a3";
      row.style.color = "#dde7f0";
      const ms = Math.round(step.startedAt - t0);
      elapsed.textContent = `+${ms}ms`;
      return;
    }
    icon.textContent = "\u25CB";
    icon.style.color = "#3e5474";
    row.style.color = "#5b7fa9";
    elapsed.textContent = "";
  }
  for (let i = 0; i < steps.length; i += 1) {
    paintRow(i);
  }
  function markStarted(label) {
    const i = findStep(label);
    if (i < 0) return;
    if (steps[i].startedAt !== null) return;
    steps[i].startedAt = performance.now();
    paintRow(i);
  }
  function markFailed(label) {
    const synthetic = {
      label,
      consoleHints: [],
      startedAt: null,
      failed: true,
      failureMessage: "failed"
    };
    steps.push(synthetic);
    const row = document.createElement("div");
    row.style.cssText = [
      "display: flex",
      "align-items: baseline",
      "gap: 0.75rem",
      "color: #ff9c9c"
    ].join("; ");
    const icon = document.createElement("span");
    icon.textContent = "\u2717";
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
  function observeConsoleLine(text) {
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
  function dismiss() {
    overlay.style.opacity = "0";
    window.setTimeout(() => {
      overlay.remove();
    }, 400);
  }
  return { markStarted, markFailed, observeConsoleLine, dismiss };
}
function mountCrashScreen() {
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
    "overflow: auto"
  ].join("; ");
  const container = document.createElement("div");
  container.style.cssText = [
    "max-width: 880px",
    "width: 100%"
  ].join("; ");
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
    "border-radius: 4px"
  ].join("; ");
  banner.textContent = "Kernel halted";
  const title = document.createElement("h1");
  title.style.cssText = [
    "color: #ff7a7a",
    "font-size: 32px",
    "font-weight: 300",
    "margin: 0 0 0.5rem 0",
    "letter-spacing: 0.05em"
  ].join("; ");
  const subtitle = document.createElement("div");
  subtitle.style.cssText = [
    "color: #ffb0b0",
    "font-size: 14px",
    "margin-bottom: 2rem",
    "white-space: pre-wrap",
    "word-break: break-word"
  ].join("; ");
  const intro = document.createElement("div");
  intro.style.cssText = [
    "color: #d0a0a0",
    "font-size: 12px",
    "margin-bottom: 0.5rem",
    "letter-spacing: 0.1em",
    "text-transform: uppercase"
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
    "word-break: break-word"
  ].join("; ");
  const buttons = document.createElement("div");
  buttons.style.cssText = [
    "display: flex",
    "gap: 0.75rem"
  ].join("; ");
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
    "cursor: pointer"
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
    "cursor: pointer"
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
  function observeConsoleLine(text, recentSink, cap) {
    for (const line of text.split("\n")) {
      const trimmed = line.replace(/\r$/, "");
      if (trimmed === "") continue;
      recentSink.push(trimmed);
      while (recentSink.length > cap) {
        recentSink.shift();
      }
    }
    const lower = text.toLowerCase();
    if (!shown && (lower.includes("dispatch error") || lower.includes("real kernel panic") || lower.includes("init-desktop reaped child pid=4") || lower.includes("init-desktop reaped child pid=3"))) {
      window.setTimeout(() => {
        if (shown) return;
        let title2 = "Userland process crashed";
        let subtitle2 = text.trim().split("\n").slice(-1)[0] ?? "";
        if (lower.includes("real kernel panic")) {
          title2 = "Kernel panic";
        } else if (lower.includes("dispatch error")) {
          title2 = "Shell dispatch error";
        } else if (lower.includes("init-desktop reaped child pid=3")) {
          title2 = "Display server died";
          subtitle2 = "init-desktop reaped the display-server. The desktop cannot run without it.";
        } else if (lower.includes("init-desktop reaped child pid=4")) {
          title2 = "Shell died";
          subtitle2 = "init-desktop reaped the shell. The desktop cannot run without it.";
        }
        show({
          title: title2,
          subtitle: subtitle2,
          recent: recentSink
        });
      }, 50);
    }
  }
  function show(args) {
    if (shown) return;
    shown = true;
    title.textContent = args.title;
    subtitle.textContent = args.subtitle;
    log.textContent = args.recent.join("\n");
    overlay.style.display = "flex";
    log.scrollTop = log.scrollHeight;
  }
  return { observeConsoleLine, show };
}
function showFallbackMessage(error) {
  document.body.innerHTML = `
    <div style="padding:2rem;font-family:ui-monospace,monospace;color:#e6e6e6;background:#0a0e14;height:100vh">
      <h1 style="color:#ff6b6b">PMos bootstrap failed</h1>
      <p>${escapeHtml(error)}</p>
      <p style="color:#808591">See devtools console for details.</p>
    </div>`;
}
function showPanic(message) {
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
    setTimeout(tick, 1e3);
  };
  tick();
}
function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
function installPagehideSync(kernelWorker, target = window) {
  const listener = () => {
    kernelWorker.postMessage({ kind: "sync:request" });
  };
  target.addEventListener("pagehide", listener);
  return listener;
}
function installPeriodicSync(kernelWorker, intervalMs = 6e4, scheduler = {
  setInterval: (h, ms) => globalThis.setInterval(h, ms),
  clearInterval: (h) => globalThis.clearInterval(h)
}) {
  if (intervalMs <= 0 || !Number.isFinite(intervalMs)) {
    throw new Error(
      `installPeriodicSync: intervalMs must be a positive finite number, got ${intervalMs}`
    );
  }
  const handle = scheduler.setInterval(() => {
    kernelWorker.postMessage({ kind: "sync:request" });
  }, intervalMs);
  return () => scheduler.clearInterval(handle);
}
function createSpawnRouter(deps) {
  const live = /* @__PURE__ */ new Map();
  function recordMemory(pid, bytes) {
    if (!live.has(pid)) {
      return;
    }
    deps.kernelWorker.postMessage({ kind: "proc:memory", pid, bytes });
  }
  function reap(pid, code, trap) {
    const entry = live.get(pid);
    if (!entry) {
      return;
    }
    live.delete(pid);
    entry.worker.terminate();
    deps.kernelWorker.postMessage(
      trap !== void 0 ? { kind: "proc:exited", pid, code, trap } : { kind: "proc:exited", pid, code }
    );
  }
  return {
    liveWorkers: live,
    handleKernelMessage(msg) {
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
        if (m.memoryBytes !== void 0) {
          recordMemory(msg.pid, m.memoryBytes);
        }
        if (m.kind === "exited") {
          reap(msg.pid, m.code, m.trap);
        }
      });
      worker.addEventListener("error", (ev) => {
        reap(msg.pid, -1, ev.message ?? "user worker error");
      });
      const kernelWakeSlot = deps.getKernelWakeSlot?.() ?? null;
      worker.postMessage(
        kernelWakeSlot !== null ? {
          kind: "boot",
          pid: msg.pid,
          sab,
          wasmBytes: msg.wasmBytes,
          kernelWakeSlot
        } : {
          kind: "boot",
          pid: msg.pid,
          sab,
          wasmBytes: msg.wasmBytes
        }
      );
      deps.kernelWorker.postMessage({ kind: "proc:sab", pid: msg.pid, sab });
    },
    terminateAll() {
      for (const [pid, entry] of live) {
        entry.worker.terminate();
        live.delete(pid);
      }
    }
  };
}
if (typeof document !== "undefined") {
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", main);
  } else {
    main();
  }
}
export {
  SAB_SIZE,
  createSpawnRouter,
  hasAtomicsWait,
  hasOpfs,
  installPagehideSync,
  installPeriodicSync,
  isCrossOriginIsolated,
  registerServiceWorker,
  showPanic
};
