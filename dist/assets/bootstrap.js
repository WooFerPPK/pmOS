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

// src/console-transcript.ts
var CONSOLE_TRANSCRIPT_MAX_BYTES = 256 * 1024;
var CONSOLE_TRANSCRIPT_MAX_LINES = 512;
function tailByLines(text, maxLines) {
  if (text === "" || maxLines <= 0) return "";
  let retainedLines = text.endsWith("\n") ? 0 : 1;
  let cursor = text.length;
  while (cursor > 0) {
    const newline = text.lastIndexOf("\n", cursor - 1);
    if (newline < 0) return text;
    retainedLines += 1;
    if (retainedLines > maxLines) return text.slice(newline + 1);
    cursor = newline;
  }
  return text;
}
function tailByUtf8Bytes(text, maxBytes) {
  if (text === "" || maxBytes <= 0) return "";
  const encoded = new TextEncoder().encode(text);
  if (encoded.byteLength <= maxBytes) return text;
  let start = encoded.byteLength - maxBytes;
  while (start < encoded.byteLength && (encoded[start] & 192) === 128) {
    start += 1;
  }
  return new TextDecoder().decode(encoded.subarray(start));
}
function boundedConsoleTail(text, limits) {
  return tailByUtf8Bytes(
    tailByLines(text, limits.maxLines),
    limits.maxBytes
  );
}
var ConsoleTranscript = class {
  constructor(sink, limits = {
    maxBytes: CONSOLE_TRANSCRIPT_MAX_BYTES,
    maxLines: CONSOLE_TRANSCRIPT_MAX_LINES
  }) {
    this.sink = sink;
    this.limits = limits;
    if (limits.maxBytes <= 0 || limits.maxLines <= 0) {
      throw new RangeError("console transcript limits must be positive");
    }
  }
  sink;
  limits;
  value = "";
  append(text) {
    if (text === "") return;
    const incoming = boundedConsoleTail(text, this.limits);
    this.value = boundedConsoleTail(`${this.value}${incoming}`, this.limits);
    this.sink.textContent = this.value;
  }
  /** Current retained text, exposed for diagnostics and isolation tests. */
  get text() {
    return this.value;
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
  patchHandlers = [];
  patchBatchHandlers = [];
  modeHandlers = [];
  presentFenceHandlers = [];
  currentMode = null;
  blitCount = 0;
  patchCount = 0;
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
  /** Number of rectangular patches observed since construction. */
  get patchesObserved() {
    return this.patchCount;
  }
  /** Subscribe to blit events. */
  onFrame(handler) {
    this.frameHandlers.push(handler);
  }
  /** Subscribe to rectangular patch events. */
  onPatch(handler) {
    this.patchHandlers.push(handler);
  }
  /** Subscribe to atomic rectangular-patch batches. */
  onPatchBatch(handler) {
    this.patchBatchHandlers.push(handler);
  }
  /** Subscribe to mode-change events. */
  onModeChange(handler) {
    this.modeHandlers.push(handler);
  }
  /** Subscribe to display-server presentation fences. */
  onPresentFence(handler) {
    this.presentFenceHandlers.push(handler);
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
      case "fb:patch": {
        this.patchCount += 1;
        const patch = {
          x: msg.x,
          y: msg.y,
          width: msg.width,
          height: msg.height,
          rgba: msg.rgba
        };
        for (const h of this.patchHandlers) {
          h(patch);
        }
        return;
      }
      case "fb:patch-batch": {
        this.patchCount += msg.patches.length;
        const batch = {
          patches: msg.patches.map((patch) => ({
            x: patch.x,
            y: patch.y,
            width: patch.width,
            height: patch.height,
            rgba: patch.rgba
          }))
        };
        for (const h of this.patchBatchHandlers) {
          h(batch);
        }
        return;
      }
      case "fb:present-fence": {
        for (const h of this.presentFenceHandlers) {
          h(msg.serial);
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
  handlers = [];
  currentMode = null;
  /**
   * Number of frames painted since construction. Read by tests
   * to assert the render loop ran the expected number of times
   * without needing a canvas-pixel comparison.
   */
  presentsCompleted = 0;
  /** Legacy diagnostic reporting whether OffscreenCanvas 2D is available. */
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
   * Resize the canvas and refresh the OffscreenCanvas capability probe.
   * Idempotent: passing the same geometry as the current mode is a no-op.
   */
  setMode(mode) {
    if (this.currentMode !== null && this.currentMode.width === mode.width && this.currentMode.height === mode.height) {
      return;
    }
    this.currentMode = mode;
    this.canvas.width = mode.width;
    this.canvas.height = mode.height;
    const offscreen = this.offscreenFactory(mode.width, mode.height);
    const offscreenCtx = offscreen?.getContext("2d") ?? null;
    this.usingFastPath = offscreenCtx !== null;
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
    this.finishPresent();
  }
  /**
   * Paint one tightly packed RGBA8 rectangle into the current mode.
   * Invalid, empty, out-of-bounds, or byte-count-mismatched patches
   * are dropped without firing a present-complete notification.
   *
   * The visible context is updated directly, keeping work proportional to
   * the damage rectangle. Full frames use the same visible-canvas path.
   */
  paintPatch(patch) {
    const ctx = this.canvas.getContext("2d");
    if (ctx === null) {
      return;
    }
    const prepared = this.preparePatch(patch);
    if (prepared === null) {
      return;
    }
    ctx.putImageData(prepared.imageData, prepared.x, prepared.y);
    this.finishPresent();
  }
  /**
   * Validate every rectangle before painting any of them, then update the
   * visible canvas and emit exactly one completion. Canvas writes happen in
   * one main-thread task, so no partially updated batch is observable.
   */
  paintPatchBatch(patches) {
    if (patches.length === 0) {
      return;
    }
    const ctx = this.canvas.getContext("2d");
    if (ctx === null) {
      return;
    }
    const prepared = [];
    for (const patch of patches) {
      const next = this.preparePatch(patch);
      if (next === null) {
        return;
      }
      prepared.push(next);
    }
    for (const patch of prepared) {
      ctx.putImageData(patch.imageData, patch.x, patch.y);
    }
    this.finishPresent();
  }
  preparePatch(patch) {
    const mode = this.currentMode;
    if (mode === null) {
      return null;
    }
    if (!Number.isSafeInteger(patch.x) || !Number.isSafeInteger(patch.y) || !Number.isSafeInteger(patch.width) || !Number.isSafeInteger(patch.height) || patch.x < 0 || patch.y < 0 || patch.width <= 0 || patch.height <= 0) {
      return null;
    }
    const right = patch.x + patch.width;
    const bottom = patch.y + patch.height;
    const pixelCount = patch.width * patch.height;
    const pixelBytes = pixelCount * 4;
    if (!Number.isSafeInteger(right) || !Number.isSafeInteger(bottom) || !Number.isSafeInteger(pixelCount) || !Number.isSafeInteger(pixelBytes) || right > mode.width || bottom > mode.height || pixelBytes !== patch.rgba.byteLength) {
      return null;
    }
    const rgba = new Uint8ClampedArray(
      patch.rgba.buffer,
      patch.rgba.byteOffset,
      patch.rgba.byteLength
    );
    const imageData = this.imageDataFactory(
      rgba,
      patch.width,
      patch.height
    );
    return { x: patch.x, y: patch.y, imageData };
  }
  finishPresent() {
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
var KBD_EVENT_SIZE = 8;
var KbdKeyState = {
  Released: 0,
  Pressed: 1
};
function packKbdEvent(key, state) {
  const out = new Uint8Array(KBD_EVENT_SIZE);
  const view = new DataView(out.buffer);
  view.setUint32(0, key, true);
  view.setUint32(4, state, true);
  return out;
}

// src/shared/worker-proto.ts
var HOST_FILE_IMPORT_MAX_BYTES = 16 * 1024 * 1024;
var HOST_FILE_IMPORT_MAX_TOTAL_BYTES = 32 * 1024 * 1024;
var HOST_FILE_IMPORT_MAX_FILES = 64;

// src/storage-recovery.ts
var gates = /* @__PURE__ */ new WeakMap();
function describeReason(reason) {
  switch (reason) {
    case "opfs-open-failed":
      return "The browser could not open PMos persistent storage.";
    case "persistent-root-unavailable":
      return "The persistent filesystem could not be installed as the root filesystem.";
    case "persistent-root-invalid":
      return "The existing PMos filesystem image could not be validated or mounted.";
  }
}
function showStorageRecoveryGate(message, options = {}) {
  const targetDocument = options.document ?? document;
  const existing = gates.get(targetDocument);
  if (existing !== void 0) {
    if (existing.blocked) {
      existing.detail.textContent = `${describeReason(message.reason)} ${message.detail}`;
    }
    return existing.controller;
  }
  const overlay = targetDocument.createElement("section");
  overlay.id = "pmos-storage-recovery";
  overlay.dataset["state"] = "blocked";
  overlay.style.cssText = [
    "position: fixed",
    "inset: 0",
    "z-index: 2147483647",
    "display: grid",
    "place-items: center",
    "padding: 2rem",
    "box-sizing: border-box",
    "color: #f3f6fa",
    "background: rgba(5, 9, 14, 0.98)",
    'font-family: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
    "pointer-events: auto"
  ].join("; ");
  const panel = targetDocument.createElement("div");
  panel.style.cssText = [
    "width: min(42rem, 100%)",
    "padding: 2rem",
    "box-sizing: border-box",
    "border: 1px solid #cf6a52",
    "border-radius: 0.75rem",
    "background: #151b24",
    "box-shadow: 0 1.5rem 5rem rgba(0, 0, 0, 0.65)"
  ].join("; ");
  const title = targetDocument.createElement("h1");
  title.id = "pmos-storage-recovery-title";
  title.textContent = "Persistent storage needs attention";
  title.style.cssText = "margin: 0 0 1rem; font-size: 1.65rem";
  const summary = targetDocument.createElement("p");
  summary.textContent = "PMos paused the desktop because it cannot currently guarantee that files will survive a reload.";
  summary.style.cssText = "margin: 0 0 1rem; line-height: 1.55";
  const detail = targetDocument.createElement("p");
  detail.id = "pmos-storage-recovery-detail";
  detail.textContent = `${describeReason(message.reason)} ${message.detail}`;
  detail.style.cssText = [
    "margin: 0 0 1rem",
    "padding: 0.75rem",
    "border-radius: 0.4rem",
    "background: #0d1219",
    "color: #d9e1eb",
    'font-family: ui-monospace, "SF Mono", Menlo, Consolas, monospace',
    "font-size: 0.85rem",
    "overflow-wrap: anywhere"
  ].join("; ");
  const preservation = targetDocument.createElement("p");
  preservation.textContent = "Any existing pmos.img was left in place and was not reformatted or overwritten.";
  preservation.style.cssText = "margin: 0 0 1.5rem; color: #b9c5d3; line-height: 1.55";
  const actions = targetDocument.createElement("div");
  actions.style.cssText = "display: flex; flex-wrap: wrap; gap: 0.75rem";
  const retry = targetDocument.createElement("button");
  retry.id = "pmos-storage-retry";
  retry.type = "button";
  retry.textContent = "Retry persistent storage";
  retry.style.cssText = [
    "padding: 0.75rem 1rem",
    "border: 0",
    "border-radius: 0.4rem",
    "font: inherit",
    "font-weight: 650",
    "color: #091018",
    "background: #8fc8ff",
    "cursor: pointer"
  ].join("; ");
  const continueTemporary = targetDocument.createElement("button");
  continueTemporary.id = "pmos-storage-continue-temporary";
  continueTemporary.type = "button";
  continueTemporary.textContent = "Continue temporary session \u2014 files will be lost on reload";
  continueTemporary.style.cssText = [
    "padding: 0.75rem 1rem",
    "border: 1px solid #cf6a52",
    "border-radius: 0.4rem",
    "font: inherit",
    "color: #ffd8cf",
    "background: transparent",
    "cursor: pointer"
  ].join("; ");
  actions.append(retry, continueTemporary);
  panel.append(title, summary, detail, preservation, actions);
  overlay.append(panel);
  targetDocument.body.append(overlay);
  const previousOverflow = targetDocument.body.style.overflow;
  targetDocument.body.style.overflow = "hidden";
  targetDocument.body.dataset["pmosStorageState"] = "degraded-blocked";
  const blockOutsideGate = (event) => {
    const target = event.target;
    if (target !== null && overlay.contains(target)) return;
    event.preventDefault();
    event.stopImmediatePropagation();
  };
  targetDocument.addEventListener("keydown", blockOutsideGate, true);
  targetDocument.addEventListener("keyup", blockOutsideGate, true);
  const state = {};
  const controller = {
    get blocked() {
      return state.blocked;
    }
  };
  state.blocked = true;
  state.controller = controller;
  state.detail = detail;
  gates.set(targetDocument, state);
  retry.addEventListener("click", () => {
    options.onRetry?.();
  });
  continueTemporary.addEventListener("click", () => {
    if (!state.blocked) return;
    state.blocked = false;
    overlay.dataset["state"] = "temporary";
    targetDocument.body.dataset["pmosStorageState"] = "temporary";
    targetDocument.body.style.overflow = previousOverflow;
    targetDocument.removeEventListener("keydown", blockOutsideGate, true);
    targetDocument.removeEventListener("keyup", blockOutsideGate, true);
    overlay.remove();
    options.onContinueTemporary?.();
  });
  return controller;
}

// src/bootstrap.ts
var BOOT_VERSION = "0.1.0-demo";
function hasSharedArrayBuffer() {
  return typeof SharedArrayBuffer !== "undefined";
}
function unsupportedBrowserReasons() {
  const reasons = [];
  if (!isCrossOriginIsolated())
    reasons.push("cross-origin isolation (COOP/COEP)");
  if (!hasSharedArrayBuffer()) reasons.push("SharedArrayBuffer");
  if (!hasAtomicsWait()) reasons.push("Atomics.wait");
  if (!hasAtomicsWaitAsync()) reasons.push("Atomics.waitAsync");
  if (typeof Worker === "undefined") reasons.push("dedicated Workers");
  if (!hasOpfs()) reasons.push("Origin Private File System (OPFS)");
  if (typeof navigator === "undefined" || typeof navigator.serviceWorker === "undefined") {
    reasons.push("service workers");
  }
  return reasons;
}
function hasAtomicsWait() {
  return typeof Atomics !== "undefined" && typeof Atomics.wait === "function";
}
function hasAtomicsWaitAsync() {
  return typeof Atomics !== "undefined" && typeof Atomics.waitAsync === "function";
}
function isCrossOriginIsolated() {
  return typeof crossOriginIsolated !== "undefined" && crossOriginIsolated;
}
function hasOpfs() {
  return typeof navigator !== "undefined" && typeof navigator.storage !== "undefined" && typeof navigator.storage.getDirectory === "function";
}
function serviceWorkerScriptUrl(baseUrl) {
  const baseHref = baseUrl ?? (typeof document !== "undefined" ? document.baseURI : void 0);
  if (baseHref === void 0) {
    return "/sw.js";
  }
  const base = new URL(baseHref);
  const script = new URL("./sw.js", base);
  return `${script.pathname}${script.search}`;
}
function registerServiceWorker(scriptURL = serviceWorkerScriptUrl(), options = { type: "module" }, container) {
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
  const unsupported = unsupportedBrowserReasons();
  if (unsupported.length > 0) {
    const detail = `Missing required browser capabilities: ${unsupported.join(", ")}`;
    console.error(`[pmos-bootstrap] unsupported browser: ${detail}`);
    showUnsupportedBrowserMessage(detail);
    return;
  }
  void registerServiceWorker().catch((error) => {
    console.warn(
      `[pmos-bootstrap] service-worker registration failed: ${String(error)}`
    );
  });
  if (!window.location.hash.includes("mock-kernel")) {
    const hash = window.location.hash;
    let bootBinary;
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
  const rows = [
    {
      label: "Cross-origin isolation (COOP/COEP)",
      status: "pending",
      detail: ""
    },
    { label: "SharedArrayBuffer", status: "pending", detail: "" },
    { label: "Atomics.wait / waitAsync", status: "pending", detail: "" },
    {
      label: "Origin Private Filesystem (OPFS)",
      status: "pending",
      detail: ""
    },
    { label: "Service worker", status: "pending", detail: "" },
    { label: "OffscreenCanvas", status: "pending", detail: "" },
    {
      label: "Kernel WASM load (/assets/kernel.wasm)",
      status: "pending",
      detail: ""
    },
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
      void message;
    },
    { once: true }
  );
  return { worker, console: consoleHost, fb: fbHost };
}
function domCodeToScancode(code) {
  if (code.length === 4 && code.startsWith("Key")) {
    const ch = code.charCodeAt(3);
    if (ch >= 65 && ch <= 90) {
      return 4 + (ch - 65);
    }
  }
  if (code.length === 6 && code.startsWith("Digit")) {
    const ch = code.charCodeAt(5);
    if (ch === 48) return 39;
    if (ch >= 49 && ch <= 57) {
      return 30 + (ch - 49);
    }
  }
  switch (code) {
    case "Enter":
      return 40;
    case "Escape":
      return 41;
    case "Backspace":
      return 42;
    case "Tab":
      return 43;
    case "Space":
      return 44;
    case "Minus":
      return 45;
    case "Equal":
      return 46;
    case "BracketLeft":
      return 47;
    case "BracketRight":
      return 48;
    case "Backslash":
      return 49;
    case "Semicolon":
      return 51;
    case "Quote":
      return 52;
    case "Backquote":
      return 53;
    case "Comma":
      return 54;
    case "Period":
      return 55;
    case "Slash":
      return 56;
    case "F4":
      return 61;
    case "Insert":
      return 73;
    case "Home":
      return 74;
    case "PageUp":
      return 75;
    case "Delete":
      return 76;
    case "End":
      return 77;
    case "PageDown":
      return 78;
    case "ArrowRight":
      return 79;
    case "ArrowLeft":
      return 80;
    case "ArrowDown":
      return 81;
    case "ArrowUp":
      return 82;
    case "ShiftLeft":
      return 225;
    case "ShiftRight":
      return 229;
    case "ControlLeft":
      return 224;
    case "ControlRight":
      return 228;
    case "AltLeft":
      return 226;
    case "AltRight":
      return 230;
    default:
      return null;
  }
}
function targetsBrowserSubstrateControl(event) {
  const target = event.target;
  if (typeof target?.closest !== "function") return false;
  const recovery = target.closest("#pmos-storage-recovery");
  const hostPicker = target.closest(`#${HOST_FILE_PICKER_CONFIRM_ID}`);
  return recovery !== null && recovery !== void 0 || hostPicker !== null && hostPicker !== void 0;
}
function createBrowserControlKeyRouter(targetsControl = targetsBrowserSubstrateControl) {
  const routeToGuest = /* @__PURE__ */ new Map();
  return {
    keydown(event) {
      const existingRoute = routeToGuest.get(event.code);
      if (existingRoute !== void 0) {
        if (event.repeat === true) return existingRoute;
        routeToGuest.delete(event.code);
      }
      const guest = !targetsControl(event);
      routeToGuest.set(event.code, guest);
      return guest;
    },
    keyup(event) {
      const existingRoute = routeToGuest.get(event.code);
      if (existingRoute !== void 0) {
        routeToGuest.delete(event.code);
        return existingRoute;
      }
      return !targetsControl(event);
    }
  };
}
function createGuiKeyboardInputHandler(kernelWorker, state) {
  return (event) => {
    if (event.metaKey) return null;
    const scancode = domCodeToScancode(event.code);
    if (scancode === null) return null;
    event.preventDefault();
    kernelWorker.postMessage({
      kind: "input:kbd",
      bytes: packKbdEvent(scancode, state)
    });
    return scancode;
  };
}
function createGuiKeyboardInputBridge(kernelWorker, userInteractionAllowed = () => true, targetsControl = targetsBrowserSubstrateControl) {
  const press = createGuiKeyboardInputHandler(
    kernelWorker,
    KbdKeyState.Pressed
  );
  const browserControlKeys = createBrowserControlKeyRouter(targetsControl);
  const heldGuestKeys = /* @__PURE__ */ new Map();
  const releasedForFocusLoss = /* @__PURE__ */ new Set();
  const postRelease = (scancode) => {
    kernelWorker.postMessage({
      kind: "input:kbd",
      bytes: packKbdEvent(scancode, KbdKeyState.Released)
    });
  };
  return {
    keydown(event) {
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
        if (staleScancode !== void 0) {
          postRelease(staleScancode);
          heldGuestKeys.delete(event.code);
        }
      }
      const scancode = press(event);
      if (scancode !== null) heldGuestKeys.set(event.code, scancode);
    },
    keyup(event) {
      if (!browserControlKeys.keyup(event)) return;
      if (releasedForFocusLoss.delete(event.code)) {
        event.preventDefault();
        return;
      }
      const scancode = heldGuestKeys.get(event.code);
      if (scancode !== void 0) {
        event.preventDefault();
        postRelease(scancode);
        heldGuestKeys.delete(event.code);
        return;
      }
      if (!userInteractionAllowed()) event.preventDefault();
    },
    releaseHeldKeys() {
      for (const [code, scancode] of heldGuestKeys) {
        postRelease(scancode);
        releasedForFocusLoss.add(code);
      }
      heldGuestKeys.clear();
    }
  };
}
function installGuiKeyboardFocusLossHandlers(windowTarget, documentTarget, releaseHeldKeys) {
  const onBlur = () => releaseHeldKeys();
  const onVisibilityChange = () => {
    if (documentTarget.hidden) releaseHeldKeys();
  };
  windowTarget.addEventListener("blur", onBlur);
  documentTarget.addEventListener("visibilitychange", onVisibilityChange);
  return () => {
    windowTarget.removeEventListener("blur", onBlur);
    documentTarget.removeEventListener("visibilitychange", onVisibilityChange);
  };
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
function createLegacyKeyboardInputHandler(kernelWorker, maxLineBytes = 4096) {
  if (!Number.isSafeInteger(maxLineBytes) || maxLineBytes < 1) {
    throw new Error(
      `createLegacyKeyboardInputHandler: maxLineBytes must be a positive integer, got ${maxLineBytes}`
    );
  }
  const chunks = [];
  let bufferedBytes = 0;
  return (event) => {
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
function createGuiDesktopReadyLatch(onReady) {
  let ready = false;
  return {
    get ready() {
      return ready;
    },
    notePresentFence(serial) {
      if (ready || !Number.isInteger(serial) || serial <= 0 || serial > 4294967295) {
        return;
      }
      ready = true;
      onReady();
    }
  };
}
function isBootInteractionAllowed(state) {
  return state.kernelReady && (!state.storageDegraded || state.temporaryStorageAccepted) && (state.guiDesktopReady === null || state.guiDesktopReady);
}
function wireFramebufferPresentations(options) {
  const now = options.now ?? (() => performance.now());
  const makeFrameEvent = options.makeFrameEvent ?? ((detail) => new CustomEvent("pmos:frame", { detail }));
  let sequence = 0;
  let firstPresentSeen = false;
  let receivedAt = null;
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
        paintedAt
      })
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
function runRealKernelMode(bootBinary) {
  console.log(
    `[pmos-bootstrap] real-kernel mode enabled via URL (bootBinary=${bootBinary})`
  );
  const isGuiBoot = bootBinary === "/bin/init-desktop";
  const consoleEl = mountRealKernelConsole(isGuiBoot);
  const splash = isGuiBoot ? mountBootSplash() : null;
  const worker = new Worker("/assets/kernel-worker.js", { type: "module" });
  let kernelReady = false;
  let storageDegraded = false;
  let temporaryStorageAccepted = false;
  const guiDesktopReady = isGuiBoot ? createGuiDesktopReadyLatch(() => {
    splash?.markStarted("Desktop ready");
    splash?.dismiss();
    consoleEl.hidden = true;
  }) : null;
  const userInteractionAllowed = () => isBootInteractionAllowed({
    kernelReady,
    storageDegraded,
    temporaryStorageAccepted,
    guiDesktopReady: guiDesktopReady?.ready ?? null
  });
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
          splash.markStarted(
            `Framebuffer mode set ${mode.width}\xD7${mode.height}`
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
        }
      });
      fbHost.onPresentFence((serial) => {
        guiDesktopReady?.notePresentFence(serial);
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
        if (!userInteractionAllowed()) return;
        const coords = toFbCoords(event);
        if (!coords) return;
        const [x, y] = coords;
        worker.postMessage({
          kind: "input:mouse",
          bytes: packMouseMotion(x, y)
        });
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
          bytes: packMouseButton(x, y, button, MouseButtonState.Pressed)
        });
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
          bytes: packMouseButton(x, y, button, MouseButtonState.Released)
        });
      });
      canvas.addEventListener("contextmenu", (event) => {
        event.preventDefault();
      });
      const guiKeyboard = createGuiKeyboardInputBridge(
        worker,
        userInteractionAllowed
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
        guiKeyboard.releaseHeldKeys
      );
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
    getKernelWakeSlot: () => kernelWakeSlot,
    onLiveWorkersChanged: (count) => {
      document.body.dataset["pmosLiveWorkers"] = String(count);
    }
  });
  worker.addEventListener("message", (ev) => {
    const msg = ev.data;
    if (msg.kind === "kernel:wake-slot") {
      kernelWakeSlot = msg.sab;
      document.body.dataset["pmosWakeSlotReady"] = "1";
      return;
    }
    if (msg.kind === "host:pick") {
      requestHostFilePicker(worker, void 0, userInteractionAllowed);
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
            "[pmos-bootstrap] temporary storage session explicitly accepted; files will be lost on reload"
          );
        }
      });
      return;
    }
    router.handleKernelMessage(msg);
    if (msg.kind === "proc:terminate") {
      document.body.dataset["pmosLastTerminatedPid"] = String(msg.pid);
      document.body.dataset["pmosLastTerminatedSignal"] = String(msg.signal);
    }
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
  const transcript = new ConsoleTranscript(consoleEl);
  consoleHost.onOutput((bytes) => {
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
  consoleHost.onLifecycle((event) => {
    if (event.kind === "ready") {
      kernelReady = true;
      console.log("[pmos-bootstrap] real kernel ready");
      if (splash) {
        splash.markStarted("Kernel ready");
      }
    } else if (event.kind === "panic") {
      console.error(`[pmos-bootstrap] real kernel panic: ${event.message}`);
      transcript.append(`
[panic] ${event.message}
`);
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
  installPagehideSync(worker);
  installBeforeUnloadSync(worker);
  installPeriodicSync(worker);
  installHostFileDropHandler(
    worker,
    window,
    userInteractionAllowed
  );
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
    "opacity: 1",
    // The bootstrap's explicit input gate discards canvas input until desktop
    // readiness. The overlay never owns pointer input, including while booting.
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
    {
      label: "Browser environment ready",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "Kernel worker spawned",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "Kernel ready",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "init (PID 1) running",
      consoleHints: ["init-desktop starting"],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "display-server spawned",
      consoleHints: ["init-desktop spawned display-server"],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "shell spawned",
      consoleHints: ["init-desktop spawned shell"],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "init supervising children",
      consoleHints: ["init-desktop entering supervision loop"],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "display-server bound /run/display",
      consoleHints: ["display-server starting"],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "shell connected to display",
      consoleHints: ["shell: connected to /run/display"],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "shell handshake complete",
      consoleHints: ["display-server served client 0"],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "Framebuffer mode set 1024\xD7768",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "Framebuffer presentation completed",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null
    },
    {
      label: "Desktop ready",
      consoleHints: [],
      startedAt: null,
      failed: false,
      failureMessage: null
    }
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
    overlay.remove();
  }
  return { markStarted, markFailed, observeConsoleLine, dismiss };
}
function classifyFatalConsoleText(text) {
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
      subtitle: "init-desktop reaped the display-server. The desktop cannot run without it."
    };
  }
  return null;
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
    const diagnosis = classifyFatalConsoleText(text);
    if (!shown && diagnosis !== null) {
      window.setTimeout(() => {
        if (shown) return;
        show({
          title: diagnosis.title,
          subtitle: diagnosis.subtitle,
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
function showUnsupportedBrowserMessage(detail) {
  document.body.innerHTML = `
    <main id="pmos-unsupported-browser" style="box-sizing:border-box;padding:2rem;font-family:ui-monospace,monospace;color:#e6e6e6;background:#0a0e14;min-height:100vh">
      <h1 style="color:#ffb454">PMos cannot start in this browser</h1>
      <p>${escapeHtml(detail)}</p>
      <p>PMos requires persistent browser-local storage so it never presents a desktop that silently loses your files.</p>
      <p style="color:#a8adb7">Use a current browser with OPFS, service workers, cross-origin isolation, SharedArrayBuffer, Atomics.wait, Atomics.waitAsync, and dedicated Workers.</p>
    </main>`;
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
var MAX_LIVE_USER_WORKERS = 256;
var USER_WORKER_TRAP_LOG_MAX = 240;
function sanitizeUserWorkerTrap(trap) {
  const singleLine = trap.replace(/[\u0000-\u001f\u007f-\u009f\u2028\u2029]+/g, " ").replace(/\s+/g, " ").trim();
  const visible = singleLine === "" ? "(empty trap)" : singleLine;
  return visible.length <= USER_WORKER_TRAP_LOG_MAX ? visible : `${visible.slice(0, USER_WORKER_TRAP_LOG_MAX - 3)}...`;
}
function installPagehideSync(kernelWorker, target = window) {
  const listener = () => {
    kernelWorker.postMessage({ kind: "sync:request" });
  };
  target.addEventListener("pagehide", listener);
  return listener;
}
var HOST_FILE_PICKER_CONFIRM_ID = "pmos-host-file-picker-confirm";
function confirmedHostFilePicker(picker, confirmation) {
  return {
    pick(onFiles, interactionAllowed = () => true) {
      let consumed = false;
      confirmation.request(() => {
        if (consumed) return;
        consumed = true;
        if (!interactionAllowed()) {
          console.warn(
            "[pmos-bootstrap] host file picker blocked while storage recovery is active"
          );
          return;
        }
        picker.pick(onFiles, interactionAllowed);
      });
    }
  };
}
function browserNativeHostFilePicker() {
  return {
    pick(onFiles) {
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
        { once: true }
      );
      input.addEventListener("cancel", () => input.remove(), { once: true });
      document.body.append(input);
      input.click();
    }
  };
}
function browserHostFilePickerConfirmation() {
  return {
    request(onConfirm) {
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
      button.style.cssText = "position:fixed;left:50%;top:16px;transform:translateX(-50%);z-index:2147483647;padding:10px 16px;border:1px solid #365f84;border-radius:4px;background:#f4f5f7;color:#161b20;font:600 14px system-ui,sans-serif;box-shadow:0 3px 12px #0005;";
      button.addEventListener(
        "click",
        () => {
          button.remove();
          onConfirm();
        },
        { once: true }
      );
      document.body.append(button);
      button.focus();
    }
  };
}
function browserHostFilePicker() {
  return confirmedHostFilePicker(
    browserNativeHostFilePicker(),
    browserHostFilePickerConfirmation()
  );
}
function requestHostFilePicker(kernelWorker, picker = browserHostFilePicker(), interactionAllowed = () => true) {
  if (!interactionAllowed()) {
    console.warn(
      "[pmos-bootstrap] host file picker blocked while storage recovery is active"
    );
    return;
  }
  picker.pick((files) => {
    if (!interactionAllowed()) {
      console.warn(
        "[pmos-bootstrap] host file selection ignored while storage recovery is active"
      );
      return;
    }
    enqueueHostFileBatch(kernelWorker, files, interactionAllowed);
  }, interactionAllowed);
}
function browserHostDownloadTarget() {
  return {
    save(name, mime, bytes) {
      const owned = new Uint8Array(bytes.byteLength);
      owned.set(bytes);
      const blob = new Blob([owned], {
        type: mime || "application/octet-stream"
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
    }
  };
}
function safeHostDownloadName(name) {
  const normalised = name.replaceAll("\\", "/");
  const leaf = normalised.split("/").pop()?.trim() ?? "";
  return leaf === "" || leaf === "." || leaf === ".." ? "download" : leaf;
}
function saveHostDownload(name, mime, bytes, target = browserHostDownloadTarget()) {
  const owned = new Uint8Array(bytes.byteLength);
  owned.set(bytes);
  target.save(
    safeHostDownloadName(name),
    mime || "application/octet-stream",
    owned
  );
}
var nextHostFileToken = 1;
function allocateHostFileToken() {
  const token = nextHostFileToken;
  nextHostFileToken = nextHostFileToken === 4294967295 ? 1 : nextHostFileToken + 1;
  return token;
}
function installHostFileDropHandler(kernelWorker, target = window, interactionAllowed = () => true) {
  const dragover = (e) => {
    e.preventDefault();
  };
  const drop = (e) => {
    e.preventDefault();
    if (!interactionAllowed()) {
      console.warn(
        "[pmos-bootstrap] host file drop ignored while storage recovery is active"
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
function preadmitHostFileBatch(files) {
  if (!Number.isSafeInteger(files.length) || files.length < 0 || files.length > HOST_FILE_IMPORT_MAX_FILES) {
    console.warn(
      `[pmos-bootstrap] host import batch contains ${files.length} files; cap is ${HOST_FILE_IMPORT_MAX_FILES}; ignored`
    );
    return null;
  }
  const admitted = [];
  let aggregateBytes = 0;
  for (let i = 0; i < files.length; i += 1) {
    const file = files[i];
    if (!file) continue;
    if (!Number.isSafeInteger(file.size) || file.size < 0) {
      console.warn(
        `[pmos-bootstrap] host import ${file.name}: invalid browser File.size; ignored`
      );
      continue;
    }
    if (file.size > HOST_FILE_IMPORT_MAX_BYTES) {
      console.warn(
        `[pmos-bootstrap] host import ${file.name}: ${file.size} bytes exceeds v1 cap of ${HOST_FILE_IMPORT_MAX_BYTES}; ignored`
      );
      continue;
    }
    aggregateBytes += file.size;
    if (aggregateBytes > HOST_FILE_IMPORT_MAX_TOTAL_BYTES) {
      console.warn(
        `[pmos-bootstrap] host import batch exceeds v1 aggregate cap of ${HOST_FILE_IMPORT_MAX_TOTAL_BYTES} bytes; ignored`
      );
      return null;
    }
    admitted.push(file);
  }
  return { files: admitted, bytes: aggregateBytes };
}
var pendingHostFileCount = 0;
var pendingHostFileBytes = 0;
var hostFileReadTail = Promise.resolve();
function enqueueHostFileBatch(kernelWorker, files, interactionAllowed) {
  const admitted = preadmitHostFileBatch(files);
  if (admitted === null) return;
  const nextCount = pendingHostFileCount + admitted.files.length;
  const nextBytes = pendingHostFileBytes + admitted.bytes;
  if (nextCount > HOST_FILE_IMPORT_MAX_FILES || nextBytes > HOST_FILE_IMPORT_MAX_TOTAL_BYTES) {
    console.warn(
      "[pmos-bootstrap] host import queue is at the v1 live-import limit; selection ignored"
    );
    return;
  }
  pendingHostFileCount = nextCount;
  pendingHostFileBytes = nextBytes;
  hostFileReadTail = hostFileReadTail.then(async () => {
    try {
      for (const file of admitted.files) {
        if (!interactionAllowed()) {
          console.warn(
            "[pmos-bootstrap] queued host import cancelled while storage recovery is active"
          );
          return;
        }
        await deliverDropFile(kernelWorker, file);
      }
    } finally {
      pendingHostFileCount -= admitted.files.length;
      pendingHostFileBytes -= admitted.bytes;
    }
  }).catch((error) => {
    console.warn(
      `[pmos-bootstrap] host import queue failed: ${String(error)}`
    );
  });
}
async function deliverDropFile(kernelWorker, file) {
  try {
    if (file.size > HOST_FILE_IMPORT_MAX_BYTES) {
      console.warn(
        `[pmos-bootstrap] host import ${file.name}: ${file.size} bytes exceeds v1 cap of ${HOST_FILE_IMPORT_MAX_BYTES}; ignored`
      );
      return;
    }
    const buf = await file.arrayBuffer();
    const bytes = new Uint8Array(buf);
    if (bytes.length > HOST_FILE_IMPORT_MAX_BYTES) {
      console.warn(
        `[pmos-bootstrap] host drop ${file.name}: ${bytes.length} bytes exceeds v1 cap of ${HOST_FILE_IMPORT_MAX_BYTES}; ignored`
      );
      return;
    }
    if (bytes.length !== file.size) {
      console.warn(
        `[pmos-bootstrap] host import ${file.name}: browser size changed from ${file.size} to ${bytes.length}; ignored`
      );
      return;
    }
    const token = allocateHostFileToken();
    kernelWorker.postMessage({
      kind: "host:dropped",
      token,
      name: file.name,
      mime: file.type ?? "application/octet-stream",
      bytes
    });
  } catch (e) {
    console.warn(`[pmos-bootstrap] host drop failed: ${e.message}`);
  }
}
function installBeforeUnloadSync(kernelWorker, target = window) {
  const listener = () => {
    kernelWorker.postMessage({ kind: "sync:request" });
  };
  target.addEventListener("beforeunload", listener);
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
  const maxLiveWorkers = deps.maxLiveWorkers ?? MAX_LIVE_USER_WORKERS;
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
    if (trap !== void 0) {
      console.error(
        `[pmos-bootstrap] user worker crashed pid=${pid}: ${sanitizeUserWorkerTrap(trap)}`
      );
    }
    live.delete(pid);
    deps.onLiveWorkersChanged?.(live.size);
    entry.worker.removeEventListener("message", entry.onMessage);
    entry.worker.removeEventListener("error", entry.onError);
    entry.worker.terminate();
    deps.kernelWorker.postMessage(
      trap !== void 0 ? { kind: "proc:exited", pid, code, trap } : { kind: "proc:exited", pid, code }
    );
  }
  return {
    liveWorkers: live,
    handleKernelMessage(msg) {
      if (msg.kind === "proc:terminate") {
        reap(msg.pid, 128 + msg.signal, void 0);
        return;
      }
      if (msg.kind !== "proc:spawn") {
        return;
      }
      if (live.has(msg.pid)) {
        return;
      }
      if (live.size >= maxLiveWorkers) {
        deps.kernelWorker.postMessage({
          kind: "proc:exited",
          pid: msg.pid,
          code: -1,
          trap: `user worker limit exceeded (${maxLiveWorkers})`
        });
        return;
      }
      let worker;
      try {
        const sab = deps.allocSab();
        worker = deps.workerFactory();
        const onMessage = (ev) => {
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
              sab
            });
            return;
          }
          if (m.kind === "memory") {
            recordMemory(msg.pid, m.bytes);
            return;
          }
          if (m.kind === "exited" && m.memoryBytes !== void 0) {
            recordMemory(msg.pid, m.memoryBytes);
          }
          if (m.kind === "exited") {
            reap(msg.pid, m.code, m.trap);
          }
        };
        const onError = (ev) => {
          reap(msg.pid, -1, ev.message ?? "user worker error");
        };
        live.set(msg.pid, {
          worker,
          sab,
          sabPublished: false,
          onMessage,
          onError
        });
        deps.onLiveWorkersChanged?.(live.size);
        worker.addEventListener("message", onMessage);
        worker.addEventListener("error", onError);
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
          trap: `user worker boot failed: ${trap}`
        });
      }
    },
    terminateAll() {
      for (const [pid, entry] of live) {
        entry.worker.removeEventListener("message", entry.onMessage);
        entry.worker.removeEventListener("error", entry.onError);
        entry.worker.terminate();
        live.delete(pid);
      }
      deps.onLiveWorkersChanged?.(live.size);
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
  HOST_FILE_PICKER_CONFIRM_ID,
  MAX_LIVE_USER_WORKERS,
  SAB_SIZE,
  classifyFatalConsoleText,
  confirmedHostFilePicker,
  createBrowserControlKeyRouter,
  createGuiDesktopReadyLatch,
  createGuiKeyboardInputBridge,
  createGuiKeyboardInputHandler,
  createLegacyKeyboardInputHandler,
  createSpawnRouter,
  hasAtomicsWait,
  hasAtomicsWaitAsync,
  hasOpfs,
  installBeforeUnloadSync,
  installGuiKeyboardFocusLossHandlers,
  installHostFileDropHandler,
  installPagehideSync,
  installPeriodicSync,
  isBootInteractionAllowed,
  isCrossOriginIsolated,
  registerServiceWorker,
  requestHostFilePicker,
  saveHostDownload,
  serviceWorkerScriptUrl,
  showPanic,
  targetsBrowserSubstrateControl,
  unsupportedBrowserReasons,
  wireFramebufferPresentations
};
