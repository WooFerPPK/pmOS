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

// src/terminal.ts
var Terminal = class {
  maxLines;
  lines = [];
  inputBuffer = "";
  /** Holds bytes from a partial output line (no trailing `\n` yet). */
  pendingOutput = "";
  decoder = new TextDecoder();
  constructor(options) {
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
  get input() {
    return this.inputBuffer;
  }
  /** Current scrollback snapshot, bounded by `maxLines`. */
  snapshot() {
    return {
      lines: this.lines.slice(),
      inputBuffer: this.inputBuffer
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
  feedKey(key) {
    if (key === "Enter") {
      const line = this.inputBuffer;
      this.inputBuffer = "";
      this.pushLine({ text: `> ${line}`, kind: "input" });
      const out = new TextEncoder().encode(`${line}
`);
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
  appendOutput(bytes) {
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
  clear() {
    this.lines.length = 0;
    this.inputBuffer = "";
    this.pendingOutput = "";
  }
  /**
   * True iff the terminal has nothing in scrollback and no
   * active input. Used by tests and the bootstrap's first-
   * paint gate.
   */
  isEmpty() {
    return this.lines.length === 0 && this.inputBuffer.length === 0;
  }
  pushLine(line) {
    this.lines.push(line);
    while (this.lines.length > this.maxLines) {
      this.lines.shift();
    }
  }
  isPrintable(ch) {
    const code = ch.charCodeAt(0);
    return code >= 32 && code !== 127;
  }
};
var DEFAULT_PALETTE = {
  bg: "#0a0e14",
  fg: "#e6e6e6",
  prompt: "#7cb7ff",
  dim: "#808591"
};
function paintTerminal(ctx, canvasWidth, canvasHeight, terminal, options) {
  const { palette, fontSizePx, dpr, title } = options;
  const px = fontSizePx * dpr;
  const lineHeight = Math.floor(px * 1.4);
  const padX = Math.floor(32 * dpr);
  const padY = Math.floor(32 * dpr);
  const monoFont = `${px}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  ctx.fillStyle = palette.bg;
  ctx.fillRect(0, 0, canvasWidth, canvasHeight);
  ctx.font = `bold ${Math.floor(px * 1.15)}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  ctx.fillStyle = palette.prompt;
  ctx.textBaseline = "top";
  ctx.fillText(title, padX, padY);
  ctx.font = `${Math.floor(px * 0.8)}px ui-monospace, "SF Mono", Menlo, Consolas, monospace`;
  ctx.fillStyle = palette.dim;
  ctx.fillText(
    "type a command and press Enter. 'help' for a list.",
    padX,
    padY + Math.floor(px * 1.4)
  );
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
  if (y + lineHeight <= canvasHeight - padY) {
    ctx.fillStyle = palette.prompt;
    ctx.fillText(`> ${inputBuffer}`, padX, y);
    const promptWidth = ctx.measureText(`> ${inputBuffer}`).width;
    ctx.fillStyle = palette.fg;
    ctx.fillRect(padX + promptWidth, y, Math.floor(px * 0.6), lineHeight);
  }
  ctx.textBaseline = "alphabetic";
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
    const terminal = new Terminal({
      maxLines: 64,
      banner: [
        "PMos 0.1.0-demo",
        "kernel worker ready \u2014 type 'help' for commands",
        ""
      ]
    });
    session.console.onOutput((bytes) => {
      terminal.appendOutput(bytes);
    });
    window.addEventListener("keydown", (event) => {
      if (shouldConsumeKey(event.key)) {
        event.preventDefault();
      }
      const committed = terminal.feedKey(event.key);
      if (committed) {
        session.console.sendInput(committed);
      }
    });
    const paintOnce = () => {
      paintTerminal(
        canvas.ctx,
        canvas.canvas.width,
        canvas.canvas.height,
        terminal,
        {
          palette: DEFAULT_PALETTE,
          fontSizePx: 14,
          dpr: canvas.dpr,
          title: "PMos kernel worker \u2014 interactive console"
        }
      );
    };
    paintOnce();
    setInterval(paintOnce, 100);
  };
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
      enableInput: false,
      enableFramebuffer: true
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
function shouldConsumeKey(key) {
  if (key === "Enter" || key === "Backspace") {
    return true;
  }
  return key.length === 1;
}
function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", main);
} else {
  main();
}
