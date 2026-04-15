// src/drivers/types.ts
var DriverErrorCode = {
  /** Driver isn't wired up yet or its backing resource is gone. */
  NotReady: 1,
  /** Transport error: bad payload, invalid opcode, etc. */
  Transport: 2,
  /** The driver reports a POSIX errno to the kernel. */
  Errno: 3
};

// src/shared/platform-constants.ts
var DriverId = {
  Framebuffer: 0,
  InputKbd: 1,
  InputMouse: 2,
  Block: 3,
  Net: 4,
  Console: 5
};
var Devnum = {
  Null: 1,
  Zero: 2,
  Random: 3,
  Console: 4,
  Fb0: 10,
  InputKbd: 20,
  InputMouse: 21
};

// src/drivers/console.ts
var CONSOLE_DRIVER_ID = DriverId.Console;
var DEV_CONSOLE_NODE = Devnum.Console;
var OP_WRITE_LINE = 1;
function isConsoleInput(m) {
  if (typeof m !== "object" || m === null) {
    return false;
  }
  const cand = m;
  return cand.kind === "console:input" && cand.bytes instanceof Uint8Array;
}
var ConsoleDriver = class {
  driverId = CONSOLE_DRIVER_ID;
  name = "console";
  host;
  init(host) {
    this.host = host;
  }
  call(op, payload) {
    const host = this.host;
    if (!host) {
      return { ok: false, error: DriverErrorCode.NotReady };
    }
    if (op !== OP_WRITE_LINE) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const copy = new Uint8Array(payload.byteLength);
    copy.set(payload);
    const message = { kind: "console:write", bytes: copy };
    host.postToMain(message);
    return { ok: true, value: payload.byteLength };
  }
  onHostMessage(msg) {
    const host = this.host;
    if (!host) {
      return;
    }
    if (isConsoleInput(msg)) {
      const copy = new Uint8Array(msg.bytes.byteLength);
      copy.set(msg.bytes);
      host.pushInputToKernel(DEV_CONSOLE_NODE, copy);
    }
  }
};

// src/drivers/fb.ts
var FB_DRIVER_ID = DriverId.Framebuffer;
var DEV_FB0_NODE = Devnum.Fb0;
var OP_SET_MODE = 1;
var OP_BLIT = 2;
function rgbaByteCount(width, height) {
  return width * height * 4;
}
function readU32LE(bytes, offset) {
  return (bytes[offset] ?? 0) | (bytes[offset + 1] ?? 0) << 8 | (bytes[offset + 2] ?? 0) << 16 | (bytes[offset + 3] ?? 0) * 16777216;
}
var FramebufferDriver = class {
  driverId = FB_DRIVER_ID;
  name = "framebuffer";
  host;
  init(host) {
    this.host = host;
  }
  call(op, payload) {
    const host = this.host;
    if (!host) {
      return { ok: false, error: DriverErrorCode.NotReady };
    }
    switch (op) {
      case OP_SET_MODE:
        return this.handleSetMode(host, payload);
      case OP_BLIT:
        return this.handleBlit(host, payload);
      default:
        return { ok: false, error: DriverErrorCode.Transport };
    }
  }
  // Framebuffer is write-only; no input route.
  handleSetMode(host, payload) {
    if (payload.byteLength < 8) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const width = readU32LE(payload, 0);
    const height = readU32LE(payload, 4);
    const message = { kind: "fb:set-mode", width, height };
    host.postToMain(message);
    return { ok: true, value: 8 };
  }
  handleBlit(host, payload) {
    if (payload.byteLength < 8) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const width = readU32LE(payload, 0);
    const height = readU32LE(payload, 4);
    const needed = rgbaByteCount(width, height);
    const pixelBytes = payload.byteLength - 8;
    if (pixelBytes !== needed) {
      return { ok: false, error: DriverErrorCode.Transport };
    }
    const rgba = new Uint8Array(needed);
    rgba.set(payload.subarray(8));
    const message = { kind: "fb:blit", width, height, rgba };
    host.postToMain(message);
    return { ok: true, value: payload.byteLength };
  }
};

// src/drivers/input.ts
var INPUT_DRIVER_ID = DriverId.InputKbd;
var DEV_INPUT_KBD_NODE = Devnum.InputKbd;
var DEV_INPUT_MOUSE_NODE = Devnum.InputMouse;
function isInputKbd(m) {
  if (typeof m !== "object" || m === null) {
    return false;
  }
  const cand = m;
  return cand.kind === "input:kbd" && cand.bytes instanceof Uint8Array;
}
function isInputMouse(m) {
  if (typeof m !== "object" || m === null) {
    return false;
  }
  const cand = m;
  return cand.kind === "input:mouse" && cand.bytes instanceof Uint8Array;
}
var InputDriver = class {
  driverId = INPUT_DRIVER_ID;
  name = "input";
  host;
  init(host) {
    this.host = host;
  }
  /**
   * The input device nodes are read-only; every opcode is a
   * caller bug, reported as `Transport`. We DELIBERATELY do
   * not distinguish "driver not initialised" here because the
   * only valid response is "don't call me".
   */
  call(_op, _payload) {
    return { ok: false, error: DriverErrorCode.Transport };
  }
  onHostMessage(msg) {
    const host = this.host;
    if (!host) {
      return;
    }
    if (isInputKbd(msg)) {
      const copy = new Uint8Array(msg.bytes.byteLength);
      copy.set(msg.bytes);
      host.pushInputToKernel(DEV_INPUT_KBD_NODE, copy);
      return;
    }
    if (isInputMouse(msg)) {
      const copy = new Uint8Array(msg.bytes.byteLength);
      copy.set(msg.bytes);
      host.pushInputToKernel(DEV_INPUT_MOUSE_NODE, copy);
      return;
    }
  }
};

// src/kernel-worker.ts
function bootKernelWorker(options) {
  const drivers = /* @__PURE__ */ new Map();
  const host = {
    postToMain(msg) {
      options.postToMain(msg);
    },
    pushInputToKernel(devnum, bytes) {
      options.kernel.injectInput(devnum, bytes);
    }
  };
  if (options.config.enableConsole) {
    const console_ = new ConsoleDriver();
    console_.init(host);
    drivers.set(console_.driverId, console_);
  }
  if (options.config.enableInput) {
    const input = new InputDriver();
    input.init(host);
    drivers.set(input.driverId, input);
  }
  if (options.config.enableFramebuffer) {
    const fb = new FramebufferDriver();
    fb.init(host);
    drivers.set(fb.driverId, fb);
  }
  options.postToMain({ kind: "ready" });
  return {
    handleMainMessage(msg) {
      switch (msg.kind) {
        case "boot": {
          options.postToMain({
            kind: "panic",
            message: "kernel-worker: received boot message while already booted"
          });
          return;
        }
        case "shutdown": {
          drivers.clear();
          return;
        }
        case "console:input": {
          const d = drivers.get(CONSOLE_DRIVER_ID);
          d?.onHostMessage?.(msg);
          return;
        }
        case "input:kbd":
        case "input:mouse": {
          const d = drivers.get(INPUT_DRIVER_ID);
          d?.onHostMessage?.(msg);
          return;
        }
      }
    },
    callDriver(devId, op, payload) {
      const d = drivers.get(devId);
      if (!d) {
        return { ok: false, error: DriverErrorCode.NotReady };
      }
      return d.call(op, payload);
    },
    get driverCount() {
      return drivers.size;
    }
  };
}

// src/shared/font.ts
var GLYPH_WIDTH = 5;
var GLYPH_HEIGHT = 7;
var CELL_WIDTH = 6;
var CELL_HEIGHT = 8;
var FIRST_CHAR = 32;
var LAST_CHAR = 126;
var GLYPH_COUNT = LAST_CHAR - FIRST_CHAR + 1;
var UNKNOWN_GLYPH = new Uint8Array([
  31,
  17,
  17,
  17,
  17,
  17,
  31
]);
var FONT_DATA = new Uint8Array(GLYPH_COUNT * GLYPH_HEIGHT);
function setGlyph(code, rows) {
  const base = (code - FIRST_CHAR) * GLYPH_HEIGHT;
  for (let i = 0; i < GLYPH_HEIGHT; i += 1) {
    FONT_DATA[base + i] = rows[i] ?? 0;
  }
}
setGlyph(33, [4, 4, 4, 4, 4, 0, 4]);
setGlyph(34, [10, 10, 10, 0, 0, 0, 0]);
setGlyph(35, [10, 10, 31, 10, 31, 10, 10]);
setGlyph(39, [4, 4, 4, 0, 0, 0, 0]);
setGlyph(40, [2, 4, 8, 8, 8, 4, 2]);
setGlyph(41, [8, 4, 2, 2, 2, 4, 8]);
setGlyph(42, [0, 10, 4, 31, 4, 10, 0]);
setGlyph(43, [0, 4, 4, 31, 4, 4, 0]);
setGlyph(44, [0, 0, 0, 0, 0, 4, 8]);
setGlyph(45, [0, 0, 0, 31, 0, 0, 0]);
setGlyph(46, [0, 0, 0, 0, 0, 0, 4]);
setGlyph(47, [1, 2, 2, 4, 8, 8, 16]);
setGlyph(48, [14, 17, 19, 21, 25, 17, 14]);
setGlyph(49, [4, 12, 4, 4, 4, 4, 14]);
setGlyph(50, [14, 17, 1, 2, 4, 8, 31]);
setGlyph(51, [31, 2, 4, 2, 1, 17, 14]);
setGlyph(52, [2, 6, 10, 18, 31, 2, 2]);
setGlyph(53, [31, 16, 30, 1, 1, 17, 14]);
setGlyph(54, [6, 8, 16, 30, 17, 17, 14]);
setGlyph(55, [31, 1, 2, 4, 8, 8, 8]);
setGlyph(56, [14, 17, 17, 14, 17, 17, 14]);
setGlyph(57, [14, 17, 17, 15, 1, 2, 12]);
setGlyph(58, [0, 4, 0, 0, 0, 4, 0]);
setGlyph(59, [0, 4, 0, 0, 4, 4, 8]);
setGlyph(60, [2, 4, 8, 16, 8, 4, 2]);
setGlyph(61, [0, 0, 31, 0, 31, 0, 0]);
setGlyph(62, [8, 4, 2, 1, 2, 4, 8]);
setGlyph(63, [14, 17, 2, 4, 4, 0, 4]);
setGlyph(65, [14, 17, 17, 31, 17, 17, 17]);
setGlyph(66, [30, 17, 17, 30, 17, 17, 30]);
setGlyph(67, [14, 17, 16, 16, 16, 17, 14]);
setGlyph(68, [30, 17, 17, 17, 17, 17, 30]);
setGlyph(69, [31, 16, 16, 30, 16, 16, 31]);
setGlyph(70, [31, 16, 16, 30, 16, 16, 16]);
setGlyph(71, [14, 17, 16, 23, 17, 17, 14]);
setGlyph(72, [17, 17, 17, 31, 17, 17, 17]);
setGlyph(73, [14, 4, 4, 4, 4, 4, 14]);
setGlyph(74, [7, 2, 2, 2, 2, 18, 12]);
setGlyph(75, [17, 18, 20, 24, 20, 18, 17]);
setGlyph(76, [16, 16, 16, 16, 16, 16, 31]);
setGlyph(77, [17, 27, 21, 21, 17, 17, 17]);
setGlyph(78, [17, 17, 25, 21, 19, 17, 17]);
setGlyph(79, [14, 17, 17, 17, 17, 17, 14]);
setGlyph(80, [30, 17, 17, 30, 16, 16, 16]);
setGlyph(81, [14, 17, 17, 17, 21, 18, 13]);
setGlyph(82, [30, 17, 17, 30, 20, 18, 17]);
setGlyph(83, [15, 16, 16, 14, 1, 1, 30]);
setGlyph(84, [31, 4, 4, 4, 4, 4, 4]);
setGlyph(85, [17, 17, 17, 17, 17, 17, 14]);
setGlyph(86, [17, 17, 17, 17, 17, 10, 4]);
setGlyph(87, [17, 17, 17, 21, 21, 27, 17]);
setGlyph(88, [17, 17, 10, 4, 10, 17, 17]);
setGlyph(89, [17, 17, 10, 4, 4, 4, 4]);
setGlyph(90, [31, 1, 2, 4, 8, 16, 31]);
setGlyph(91, [14, 8, 8, 8, 8, 8, 14]);
setGlyph(93, [14, 2, 2, 2, 2, 2, 14]);
setGlyph(95, [0, 0, 0, 0, 0, 0, 31]);
setGlyph(97, [0, 0, 14, 1, 15, 17, 15]);
setGlyph(98, [16, 16, 22, 25, 17, 17, 30]);
setGlyph(99, [0, 0, 14, 17, 16, 17, 14]);
setGlyph(100, [1, 1, 13, 19, 17, 17, 15]);
setGlyph(101, [0, 0, 14, 17, 31, 16, 14]);
setGlyph(102, [6, 9, 8, 30, 8, 8, 8]);
setGlyph(103, [0, 0, 15, 17, 15, 1, 14]);
setGlyph(104, [16, 16, 22, 25, 17, 17, 17]);
setGlyph(105, [4, 0, 12, 4, 4, 4, 14]);
setGlyph(106, [2, 0, 6, 2, 2, 18, 12]);
setGlyph(107, [16, 16, 18, 20, 24, 20, 18]);
setGlyph(108, [12, 4, 4, 4, 4, 4, 14]);
setGlyph(109, [0, 0, 26, 21, 21, 17, 17]);
setGlyph(110, [0, 0, 22, 25, 17, 17, 17]);
setGlyph(111, [0, 0, 14, 17, 17, 17, 14]);
setGlyph(112, [0, 0, 30, 17, 30, 16, 16]);
setGlyph(113, [0, 0, 15, 17, 15, 1, 1]);
setGlyph(114, [0, 0, 22, 25, 16, 16, 16]);
setGlyph(115, [0, 0, 15, 16, 14, 1, 30]);
setGlyph(116, [8, 8, 30, 8, 8, 9, 6]);
setGlyph(117, [0, 0, 17, 17, 17, 19, 13]);
setGlyph(118, [0, 0, 17, 17, 17, 10, 4]);
setGlyph(119, [0, 0, 17, 17, 21, 21, 10]);
setGlyph(120, [0, 0, 17, 10, 4, 10, 17]);
setGlyph(121, [0, 0, 17, 17, 15, 1, 14]);
setGlyph(122, [0, 0, 31, 2, 4, 8, 31]);
function glyphFor(c) {
  if (c.length === 0) {
    return UNKNOWN_GLYPH;
  }
  const code = c.charCodeAt(0);
  if (code === 32) {
    return new Uint8Array(GLYPH_HEIGHT);
  }
  if (code < FIRST_CHAR || code > LAST_CHAR) {
    return UNKNOWN_GLYPH;
  }
  const base = (code - FIRST_CHAR) * GLYPH_HEIGHT;
  const view = FONT_DATA.subarray(base, base + GLYPH_HEIGHT);
  let allZero = true;
  for (let i = 0; i < GLYPH_HEIGHT; i += 1) {
    if (view[i] !== 0) {
      allZero = false;
      break;
    }
  }
  if (allZero) {
    return UNKNOWN_GLYPH;
  }
  return view;
}
function glyphPixel(glyph, col, row) {
  if (col < 0 || col >= GLYPH_WIDTH || row < 0 || row >= GLYPH_HEIGHT) {
    return false;
  }
  const rowBits = glyph[row] ?? 0;
  const shift = GLYPH_WIDTH - 1 - col;
  return (rowBits >> shift & 1) !== 0;
}

// src/shared/rasterizer.ts
var PADDING = 4;
var BYTES_PER_PIXEL = 4;
var colors = {
  BG: 4278849044,
  FG_OUTPUT: 4293322470,
  FG_INPUT: 4286363647,
  FG_ERROR: 4294930544,
  FG_BANNER: 4286612881,
  CURSOR: 4294967295
};
var DEFAULT_PALETTE = {
  bg: colors.BG,
  banner: colors.FG_BANNER,
  input: colors.FG_INPUT,
  output: colors.FG_OUTPUT,
  error: colors.FG_ERROR,
  cursor: colors.CURSOR
};
function rasterizeSnapshot(snapshot, width, height, palette = DEFAULT_PALETTE) {
  const pixels = new Uint8Array(width * height * BYTES_PER_PIXEL);
  fillBg(pixels, palette.bg);
  if (width <= 2 * PADDING || height <= 2 * PADDING) {
    return pixels;
  }
  const textOriginX = PADDING;
  const textOriginY = PADDING;
  const textWidth = width - 2 * PADDING;
  const textHeight = height - 2 * PADDING;
  const cols = Math.floor(textWidth / CELL_WIDTH);
  const rowsTotal = Math.floor(textHeight / CELL_HEIGHT);
  if (cols === 0 || rowsTotal === 0) {
    return pixels;
  }
  const scrollbackRows = Math.max(0, rowsTotal - 1);
  const lines = snapshot.lines;
  const start = Math.max(0, lines.length - scrollbackRows);
  const visible = lines.slice(start);
  for (let rowIdx = 0; rowIdx < visible.length; rowIdx += 1) {
    const line = visible[rowIdx];
    if (!line) {
      continue;
    }
    const pixelY2 = textOriginY + rowIdx * CELL_HEIGHT;
    const fg = fgForKind(palette, line.kind);
    drawLine(pixels, width, height, textOriginX, pixelY2, cols, line.text, fg);
  }
  const inputRow = scrollbackRows;
  const pixelY = textOriginY + inputRow * CELL_HEIGHT;
  const combined = snapshot.prompt + snapshot.inputBuffer;
  drawLine(pixels, width, height, textOriginX, pixelY, cols, combined, palette.input);
  const cursorCol = combined.length;
  if (cursorCol < cols) {
    const cursorX = textOriginX + cursorCol * CELL_WIDTH;
    fillRect(
      pixels,
      width,
      height,
      cursorX,
      pixelY,
      GLYPH_WIDTH,
      GLYPH_HEIGHT,
      palette.cursor
    );
  }
  return pixels;
}
function fgForKind(p, kind) {
  switch (kind) {
    case "banner":
      return p.banner;
    case "input":
      return p.input;
    case "output":
      return p.output;
    case "error":
      return p.error;
  }
}
function fillBg(pixels, argb) {
  const [b, g, r, a] = splitArgb(argb);
  for (let i = 0; i < pixels.length; i += BYTES_PER_PIXEL) {
    pixels[i] = b;
    pixels[i + 1] = g;
    pixels[i + 2] = r;
    pixels[i + 3] = a;
  }
}
function drawLine(pixels, fbWidth, fbHeight, originX, originY, cols, text, fg) {
  for (let i = 0; i < text.length; i += 1) {
    if (i >= cols) {
      break;
    }
    const ch = text.charAt(i);
    const glyph = glyphFor(ch);
    const x0 = originX + i * CELL_WIDTH;
    drawGlyph(pixels, fbWidth, fbHeight, glyph, x0, originY, fg);
  }
}
function drawGlyph(pixels, fbWidth, fbHeight, glyph, x0, y0, fg) {
  for (let row = 0; row < GLYPH_HEIGHT; row += 1) {
    for (let col = 0; col < GLYPH_WIDTH; col += 1) {
      if (!glyphPixel(glyph, col, row)) {
        continue;
      }
      setPixel(pixels, fbWidth, fbHeight, x0 + col, y0 + row, fg);
    }
  }
}
function fillRect(pixels, fbWidth, fbHeight, x0, y0, w, h, argb) {
  for (let dy = 0; dy < h; dy += 1) {
    for (let dx = 0; dx < w; dx += 1) {
      setPixel(pixels, fbWidth, fbHeight, x0 + dx, y0 + dy, argb);
    }
  }
}
function setPixel(pixels, fbWidth, fbHeight, x, y, argb) {
  if (x < 0 || x >= fbWidth || y < 0 || y >= fbHeight) {
    return;
  }
  const idx = (y * fbWidth + x) * BYTES_PER_PIXEL;
  if (idx + BYTES_PER_PIXEL > pixels.length) {
    return;
  }
  const [b, g, r, a] = splitArgb(argb);
  pixels[idx] = b;
  pixels[idx + 1] = g;
  pixels[idx + 2] = r;
  pixels[idx + 3] = a;
}
function splitArgb(argb) {
  return [
    argb & 255,
    argb >>> 8 & 255,
    argb >>> 16 & 255,
    argb >>> 24 & 255
  ];
}

// src/mock-kernel.ts
var MockKernel = class {
  scaffold;
  policy;
  emitSplashOnFirstInput;
  liveTerminal;
  panicEmit;
  splashEmitted = false;
  /** Per-devnum line buffers — default + splash modes only. */
  lineBuffers = /* @__PURE__ */ new Map();
  /** Live-terminal state. */
  scrollback = [];
  liveInputBuffer = "";
  prompt;
  fbWidth;
  fbHeight;
  fbModeEmitted = false;
  /**
   * Sticky "we tried to start the fb driver and it rejected
   * us" flag. Set to true after the first `SET_MODE` attempt
   * that returns `NotReady` so subsequent keystrokes don't
   * retry or attempt to blit.
   */
  fbDisabled = false;
  constructor(options) {
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
  bindScaffold(scaffold) {
    this.scaffold = scaffold;
    if (this.liveTerminal) {
      this.renderAndBlit();
    }
  }
  injectInput(devnum, bytes) {
    if (devnum !== DEV_CONSOLE_NODE) {
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
      if (b === 10) {
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
  injectLiveInput(bytes) {
    let changed = false;
    for (const b of bytes) {
      if (b === 10) {
        this.commitLiveInputLine();
        changed = true;
      } else if (b === 127 || b === 8) {
        if (this.liveInputBuffer.length > 0) {
          this.liveInputBuffer = this.liveInputBuffer.slice(0, -1);
          changed = true;
        }
      } else if (b >= 32 && b <= 126) {
        this.liveInputBuffer += String.fromCharCode(b);
        changed = true;
      }
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
  commitLiveInputLine() {
    const input = this.liveInputBuffer;
    this.liveInputBuffer = "";
    this.scrollback.push({
      text: `${this.prompt}${input}`,
      kind: "input"
    });
    const inputBytesWithNewline = new TextEncoder().encode(`${input}
`);
    if (this.tryHandlePanicCommand(inputBytesWithNewline)) {
      return;
    }
    const output = this.applyPolicy(inputBytesWithNewline);
    if (output.byteLength > 0) {
      this.scaffold?.callDriver(CONSOLE_DRIVER_ID, OP_WRITE_LINE, output);
      const outputText = new TextDecoder().decode(output);
      const trimmed = outputText.endsWith("\n") ? outputText.slice(0, -1) : outputText;
      for (const outLine of trimmed.split("\n")) {
        this.scrollback.push({ text: outLine, kind: "output" });
      }
    }
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
  renderAndBlit() {
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
        OP_SET_MODE,
        packFbSetMode(this.fbWidth, this.fbHeight)
      );
      this.fbModeEmitted = true;
      if (!setModeResult.ok) {
        this.fbDisabled = true;
        return;
      }
    }
    const snapshot = {
      lines: this.scrollback,
      inputBuffer: this.liveInputBuffer,
      prompt: this.prompt
    };
    const pixels = rasterizeSnapshot(snapshot, this.fbWidth, this.fbHeight);
    scaffold.callDriver(
      FB_DRIVER_ID,
      OP_BLIT,
      packFbBlit(this.fbWidth, this.fbHeight, pixels)
    );
  }
  maybeEmitSplash() {
    if (this.splashEmitted) {
      return;
    }
    const scaffold = this.scaffold;
    if (!scaffold) {
      return;
    }
    this.splashEmitted = true;
    const setModeResult = scaffold.callDriver(
      FB_DRIVER_ID,
      OP_SET_MODE,
      packFbSetMode(SPLASH_WIDTH, SPLASH_HEIGHT)
    );
    if (!setModeResult.ok) {
      return;
    }
    const snapshot = {
      lines: [
        { text: "PMos 0.1.0-demo", kind: "banner" },
        { text: "kernel worker ready", kind: "banner" },
        { text: "type 'help' for commands", kind: "banner" },
        { text: "", kind: "output" }
      ],
      inputBuffer: "",
      prompt: "> "
    };
    const pixels = rasterizeSnapshot(snapshot, SPLASH_WIDTH, SPLASH_HEIGHT);
    scaffold.callDriver(
      FB_DRIVER_ID,
      OP_BLIT,
      packFbBlit(SPLASH_WIDTH, SPLASH_HEIGHT, pixels)
    );
  }
  flushLine(devnum, lineBytes) {
    const scaffold = this.scaffold;
    if (!scaffold) {
      return;
    }
    const line = Uint8Array.from(lineBytes);
    if (this.tryHandlePanicCommand(line)) {
      return;
    }
    const output = this.applyPolicy(line);
    if (output.byteLength === 0) {
      return;
    }
    scaffold.callDriver(CONSOLE_DRIVER_ID, OP_WRITE_LINE, output);
  }
  /**
   * If `line` is a `panic <message>` command, forward
   * the message to `panicEmit` (if wired) and return
   * true to short-circuit the rest of line handling.
   * Returns false otherwise.
   */
  tryHandlePanicCommand(line) {
    let end = line.byteLength;
    if (end > 0 && line[end - 1] === 10) {
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
  applyPolicy(line) {
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
  get liveScrollback() {
    return this.scrollback;
  }
  /** Read-only view of the live-terminal input buffer. */
  get liveInput() {
    return this.liveInputBuffer;
  }
};
var FAUX_SHELL_HELP = [
  "commands:",
  "  help     \u2014 this list",
  "  echo X   \u2014 print X",
  "  date     \u2014 print build date",
  "  whoami   \u2014 print current user",
  "  uname    \u2014 print system banner",
  "  panic X  \u2014 trigger a kernel panic with message X"
];
function fauxShellTransform(line) {
  let end = line.byteLength;
  if (end > 0 && line[end - 1] === 10) {
    end -= 1;
  }
  const body = line.subarray(0, end);
  const bodyText = new TextDecoder().decode(body);
  if (bodyText.length === 0) {
    return new Uint8Array(0);
  }
  if (bodyText.startsWith("echo ")) {
    const rest = bodyText.slice("echo ".length);
    return new TextEncoder().encode(`${rest}
`);
  }
  if (bodyText === "help") {
    return new TextEncoder().encode(`${FAUX_SHELL_HELP.join("\n")}
`);
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
var SPLASH_WIDTH = 320;
var SPLASH_HEIGHT = 240;
function packFbSetMode(width, height) {
  const out = new Uint8Array(8);
  const v = new DataView(out.buffer);
  v.setUint32(0, width, true);
  v.setUint32(4, height, true);
  return out;
}
function packFbBlit(width, height, pixels) {
  const out = new Uint8Array(8 + pixels.byteLength);
  const v = new DataView(out.buffer);
  v.setUint32(0, width, true);
  v.setUint32(4, height, true);
  out.set(pixels, 8);
  return out;
}

// src/kernel-worker-entry.ts
function installWorkerEntry(messaging) {
  let scaffold;
  messaging.onmessage = (ev) => {
    const msg = ev.data;
    if (scaffold === void 0) {
      if (msg.kind !== "boot") {
        messaging.postMessage({
          kind: "panic",
          message: `kernel-worker: ${msg.kind} received before boot`
        });
        return;
      }
      const liveTerminal = msg.config.liveTerminal === true && msg.config.enableFramebuffer;
      const initialScrollback = liveTerminal ? (msg.config.terminalBanner ?? []).map(
        (text) => ({ text, kind: "banner" })
      ) : void 0;
      const mock = new MockKernel({
        policy: { kind: "faux-shell" },
        // When the main thread asked for a framebuffer AND
        // didn't pick live-terminal mode, fall back to the
        // one-shot splash. The scaffold's fb driver routes
        // the blit to the main-thread FbHost.
        emitSplashOnFirstInput: msg.config.enableFramebuffer && !liveTerminal,
        liveTerminal,
        ...initialScrollback ? { initialScrollback } : {},
        // Wire the kernel panic sink to a postMessage on
        // the main-thread channel. This is what
        // bootstrap.ts's panic overlay listens on.
        panicEmit: (message) => {
          messaging.postMessage({ kind: "panic", message });
        }
      });
      scaffold = bootKernelWorker({
        kernel: mock,
        config: msg.config,
        postToMain(out) {
          messaging.postMessage(out);
        }
      });
      mock.bindScaffold(scaffold);
      return;
    }
    scaffold.handleMainMessage(msg);
  };
  return {
    get scaffold() {
      return scaffold;
    }
  };
}
if (typeof DedicatedWorkerGlobalScope !== "undefined" && typeof self !== "undefined" && self instanceof DedicatedWorkerGlobalScope) {
  installWorkerEntry(self);
}
export {
  installWorkerEntry
};
