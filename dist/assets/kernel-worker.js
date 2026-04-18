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

// src/shared/sab-layout.ts
var SAB_SIZE = 65536;
var OFF_REQ_HEAD = 0;
var OFF_REQ_TAIL = 4;
var OFF_RES_HEAD = 8;
var OFF_RES_TAIL = 12;
var OFF_USER_WAIT_SLOT = 16;
var OFF_REQ_RING = 64;
var OFF_RES_RING = 16384;
var OFF_HEAP_SCRATCH = 32768;
var HEAP_SCRATCH_BYTES = 32768;
var REQ_SLOT_COUNT = 510;
var RES_SLOT_COUNT = 510;
var STATUS_READY = 3;

// src/shared/syscall.ts
var SLOT_SIZE = 32;
function encodeRequest(req) {
  const buf = new Uint8Array(SLOT_SIZE);
  const view = new DataView(buf.buffer);
  view.setUint16(0, req.opcode, true);
  view.setUint16(2, req.flags ?? 0, true);
  view.setUint32(4, req.requestId, true);
  if (req.args !== void 0) {
    if (req.args.length !== 16) {
      throw new Error(`syscall.encodeRequest: args must be 16 bytes, got ${req.args.length}`);
    }
    if (req.arg0 !== void 0) {
      throw new Error("syscall.encodeRequest: pass either args or arg0, not both");
    }
    buf.set(req.args, 8);
  } else if (req.arg0 !== void 0) {
    view.setUint32(8, req.arg0, true);
  }
  view.setUint32(24, req.heapPtr ?? 0, true);
  view.setUint32(28, req.heapLen ?? 0, true);
  return buf;
}
function decodeResponse(bytes) {
  if (bytes.length !== SLOT_SIZE) {
    throw new Error(`syscall.decodeResponse: expected ${SLOT_SIZE} bytes, got ${bytes.length}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, SLOT_SIZE);
  return {
    requestId: view.getUint32(0, true),
    status: view.getInt32(4, true),
    value: view.getBigInt64(8, true),
    extraLen: view.getUint32(16, true)
  };
}
function encodeResponse(res) {
  const buf = new Uint8Array(SLOT_SIZE);
  const view = new DataView(buf.buffer);
  view.setUint32(0, res.requestId, true);
  view.setInt32(4, res.status, true);
  view.setBigInt64(8, res.value, true);
  view.setUint32(16, res.extraLen, true);
  return buf;
}
function decodeRequest(bytes) {
  if (bytes.length !== SLOT_SIZE) {
    throw new Error(`syscall.decodeRequest: expected ${SLOT_SIZE} bytes, got ${bytes.length}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, SLOT_SIZE);
  return {
    opcode: view.getUint16(0, true),
    flags: view.getUint16(2, true),
    requestId: view.getUint32(4, true),
    args: bytes.slice(8, 24),
    heapPtr: view.getUint32(24, true),
    heapLen: view.getUint32(28, true)
  };
}
var OP_EXT = {
  IPC_SOCKET: 4096,
  IPC_BIND: 4097,
  IPC_LISTEN: 4098,
  IPC_CONNECT: 4099,
  IPC_ACCEPT: 4100,
  PROC_SPAWN: 4352,
  PROC_SELF: 4355,
  PROC_PARENT: 4356,
  PROC_WAIT: 4353,
  DISPLAY_CONNECT: 4608,
  DISPLAY_BIND: 4609,
  CAP_CHECK: 4864,
  CAP_LIST: 4865
};
var ERRNO = {
  EAGAIN: 6,
  EBADF: 8,
  ECONNREFUSED: 14,
  EEXIST: 20,
  EINVAL: 28,
  EISDIR: 31,
  ENOENT: 44,
  ENOSYS: 52,
  ENOTDIR: 54,
  ENOTEMPTY: 55,
  ENOTSUP: 58,
  EROFS: 69
};
var DEV = {
  FRAMEBUFFER: 0,
  INPUT_KBD: 1,
  INPUT_MOUSE: 2,
  BLOCK: 3,
  NET: 4,
  CONSOLE: 5
};
var CAP = {
  DISPLAY_CLIENT: 1,
  DISPLAY_SERVER: 2,
  SHELL: 3,
  PROC_ENUMERATE: 4,
  PROC_KILL_ANY: 5,
  NET: 6,
  MOUNT: 7,
  CAP_GRANT: 8,
  DEV_BLOCK: 9,
  KEYMAP_ADMIN: 10
};
function capBit(cap) {
  return 1n << BigInt(cap);
}
function encodeSpawnManifest(manifest) {
  const path = new TextEncoder().encode(manifest.path);
  const args = new Uint8Array(16);
  const view = new DataView(args.buffer);
  view.setUint32(0, path.length, true);
  view.setBigUint64(4, manifest.caps, true);
  return { args, heap: path };
}
var CAPSET_ALL = 0xffffffffffffffffn;
var CAPSET_DESKTOP_SHELL = capBit(CAP.DISPLAY_CLIENT) | capBit(CAP.SHELL) | capBit(CAP.PROC_ENUMERATE) | capBit(CAP.KEYMAP_ADMIN);
var CAPSET_ORDINARY_APP = capBit(CAP.DISPLAY_CLIENT);

// src/kernel-wasm-host.ts
var KernelWasmHost = class _KernelWasmHost {
  // Note: the class deliberately does NOT retain the caller's
  // `KernelWasmHostOptions` past construction. Every field of that
  // options bag is captured by the host-import closures built in
  // `create()`. The only state the class itself owns is the WASM
  // exports record and the shared 32-byte wake slot every user
  // Worker + the main thread bumps to wake the kernel's dispatch
  // loop.
  constructor(exports, wakeBuffer) {
    this.exports = exports;
    this.wakeBuffer = wakeBuffer;
    this.wakeView = new Int32Array(wakeBuffer, 0, 8);
  }
  /** `Int32Array` view over [`wakeBuffer`]; index 0 is the wake slot. */
  wakeView;
  /**
   * Load `wasmBytes`, satisfy the host imports, and call
   * `kernel_init`. Returns a ready-to-use host.
   *
   * Throws if instantiation fails, if any import is missing from
   * `wasmBytes`, or if `kernel_init` returns non-zero.
   */
  static async create(wasmBytes, options = {}) {
    let memory;
    const binaryRegistry = options.binaryRegistry;
    const kernelWorkerChannel = options.kernelWorkerChannel;
    const resolvedOnSpawnProcess = options.onSpawnProcess ?? (binaryRegistry !== void 0 && kernelWorkerChannel !== void 0 ? (pid, path) => {
      const bytes = binaryRegistry.get(path);
      if (bytes === void 0) {
        return { ok: false, errno: ERRNO.ENOENT };
      }
      const wasmBytes2 = bytes instanceof ArrayBuffer ? bytes : ArrayBuffer.isView(bytes) ? bytes.buffer.slice(
        bytes.byteOffset,
        bytes.byteOffset + bytes.byteLength
      ) : bytes;
      kernelWorkerChannel.postMessage({
        kind: "proc:spawn",
        pid,
        path,
        wasmBytes: wasmBytes2
      });
      return { ok: true };
    } : void 0);
    const randomBytes = options.randomBytes ?? ((out) => {
      crypto.getRandomValues(out);
    });
    const nowNs = options.nowNs ?? (() => {
      return BigInt(Math.floor(performance.now() * 1e6));
    });
    const nowRealtimeNs = options.nowRealtimeNs ?? (() => {
      return BigInt(Date.now()) * 1000000n;
    });
    const onPanic = options.onPanic ?? ((message) => {
      throw new Error(`KernelWasmHost panic: ${message}`);
    });
    const framebufferDriver = options.framebufferDriver;
    if (framebufferDriver !== void 0) {
      const fbDriverHost = {
        postToMain: (msg) => {
          options.onFramebufferMessage?.(msg);
        },
        pushInputToKernel: () => {
        }
      };
      framebufferDriver.init(fbDriverHost);
    }
    const imports = {
      env: {
        pmos_host_now_ns: () => nowNs(),
        pmos_host_now_realtime_ns: () => nowRealtimeNs(),
        pmos_host_driver_call: (dev, _op, argsPtr, argsLen, _resultPtr) => {
          if (memory === void 0) return 0;
          if (dev === DEV.CONSOLE && options.onConsoleWrite !== void 0) {
            const src = new Uint8Array(memory.buffer, argsPtr, argsLen);
            options.onConsoleWrite(new Uint8Array(src));
          } else if (dev === DEV.FRAMEBUFFER) {
            const src = new Uint8Array(memory.buffer, argsPtr, argsLen);
            const copy = new Uint8Array(src);
            if (options.onFramebufferWrite !== void 0) {
              options.onFramebufferWrite(copy);
            }
            if (framebufferDriver !== void 0 && copy.length >= 1) {
              framebufferDriver.call(copy[0], copy.subarray(1));
            }
          }
          return 0;
        },
        pmos_host_random_bytes: (ptr, len) => {
          if (memory === void 0) return;
          const dest = new Uint8Array(memory.buffer, ptr, len);
          randomBytes(dest);
        },
        pmos_host_halt: (ptr, len) => {
          let message = "kernel halted";
          if (memory !== void 0 && len > 0) {
            const bytes = new Uint8Array(memory.buffer, ptr, len);
            message = new TextDecoder().decode(bytes);
          }
          onPanic(message);
          throw new Error(`kernel halted: ${message}`);
        },
        pmos_host_panic: (ptr, len) => {
          if (memory === void 0) return;
          const bytes = new Uint8Array(memory.buffer, ptr, len);
          const message = new TextDecoder().decode(bytes);
          onPanic(message);
        },
        pmos_host_spawn_process: (pid, pathPtr, pathLen) => {
          if (memory === void 0) return 0;
          const pathBytes = new Uint8Array(memory.buffer, pathPtr, pathLen);
          const path = new TextDecoder().decode(pathBytes);
          if (resolvedOnSpawnProcess === void 0) return 0;
          const outcome = resolvedOnSpawnProcess(pid, path);
          if (outcome.ok) return 0;
          return -outcome.errno;
        }
      }
    };
    const { instance } = await WebAssembly.instantiate(wasmBytes, imports);
    const exports = instance.exports;
    memory = exports.memory;
    const rc = exports.kernel_init();
    if (rc !== 0) {
      throw new Error(`KernelWasmHost: kernel_init returned ${rc}`);
    }
    let wakeBuffer;
    try {
      wakeBuffer = new SharedArrayBuffer(32);
    } catch {
      wakeBuffer = new ArrayBuffer(32);
    }
    return new _KernelWasmHost(exports, wakeBuffer);
  }
  // ---- process lifecycle --------------------------------------------
  /**
   * Register a process with the given cap bitset. Returns the newly
   * allocated pid. Throws if the kernel rejects the registration (the
   * current implementation always succeeds, so the throw path is
   * defensive).
   */
  registerProcess(caps) {
    const pid = this.exports.kernel_register_process(caps);
    if (pid < 0) {
      throw new Error(`KernelWasmHost.registerProcess: kernel_register_process returned ${pid}`);
    }
    return pid;
  }
  /**
   * Install `/dev/console` at `fd` in `pid`'s fd table. Convenience
   * wrapper over the kernel export of the same name.
   */
  installConsoleFd(pid, fd) {
    const rc = this.exports.kernel_install_console_fd(pid, fd);
    if (rc !== 0) {
      throw new Error(`KernelWasmHost.installConsoleFd(${pid}, ${fd}): rc=${rc}`);
    }
  }
  /**
   * Transition a newly-registered process from `Starting` through
   * `Ready` to `Running`. Required before the process can issue any
   * syscall that needs the caller to be in `Running` state (most
   * notably `PROC_EXIT`).
   */
  markRunning(pid) {
    const rc = this.exports.kernel_mark_running(pid);
    if (rc !== 0) {
      throw new Error(`KernelWasmHost.markRunning(${pid}): rc=${rc}`);
    }
  }
  // ---- syscall dispatch ---------------------------------------------
  /**
   * Dispatch one syscall on behalf of `pid`. Encodes `request`,
   * writes `heapIn` to the kernel's heap scratch region if provided,
   * calls `kernel_dispatch`, and reads back the decoded response plus
   * any heap output the handler wrote.
   *
   * `request.heapPtr` is interpreted as an offset inside the heap
   * scratch region, not as a linear-memory pointer. The kernel's
   * handlers use the same convention — the heap scratch is a
   * contiguous buffer addressed starting at offset 0.
   */
  dispatch(pid, request, heapIn) {
    const reqBytes = encodeRequest(request);
    {
      const buf = this.exports.memory.buffer;
      const reqPtr = this.exports.kernel_req_ptr();
      new Uint8Array(buf, reqPtr, SLOT_SIZE).set(reqBytes);
      if (heapIn !== void 0 && heapIn.length > 0) {
        const heapPtr = this.exports.kernel_heap_ptr();
        const heapCap = this.exports.kernel_heap_len();
        const offset = request.heapPtr ?? 0;
        if (offset + heapIn.length > heapCap) {
          throw new Error(
            `KernelWasmHost.dispatch: heap payload ${offset}+${heapIn.length} > capacity ${heapCap}`
          );
        }
        new Uint8Array(buf, heapPtr + offset, heapIn.length).set(heapIn);
      }
    }
    const rc = this.exports.kernel_dispatch(pid);
    if (rc !== 0) {
      throw new Error(`KernelWasmHost.dispatch: kernel_dispatch returned ${rc}`);
    }
    const respBuf = this.exports.memory.buffer;
    const respPtr = this.exports.kernel_resp_ptr();
    const respBytes = new Uint8Array(new Uint8Array(respBuf, respPtr, SLOT_SIZE));
    const response = decodeResponse(respBytes);
    let heapOut = new Uint8Array(0);
    if (response.extraLen > 0) {
      const heapBuf = this.exports.memory.buffer;
      const heapPtr = this.exports.kernel_heap_ptr();
      const offset = request.heapPtr ?? 0;
      const src = new Uint8Array(heapBuf, heapPtr + offset, response.extraLen);
      heapOut = new Uint8Array(src);
    }
    return { response, heapOut };
  }
  /**
   * Service one pending request on the per-pid SAB ring.
   *
   * Pops one request out of the SAB's request ring, calls
   * [`dispatch`] on behalf of `pid`, pushes the response into the
   * SAB's response ring, and copies any heap output back into the
   * SAB's heap scratch region at the request's declared `heap_ptr`
   * offset.
   *
   * Return values:
   *
   *   * `0` — one request was serviced.
   *   * `1` — the request ring was empty; no work done.
   *
   * `sab` is a `Uint8Array` view over the full `SAB_SIZE` bytes of
   * the per-pid `SharedArrayBuffer`. Header atomics go through an
   * `Int32Array` view constructed over the same backing; slot bytes
   * are read and written directly.
   *
   * Wake-slot semaphores (`OFF_USER_WAIT_SLOT`,
   * `OFF_KERNEL_WAIT_SLOT`) are intentionally NOT touched by this
   * method — those are the kernel-Worker loop's concern. The caller
   * is responsible for notifying the user after the response lands.
   *
   * Design note — why this orchestration lives in TS rather than in
   * a kernel-side `kernel_service_sab` export (as
   * `multi-process-plan.md §2 Changing` speculated): the kernel's
   * WASM linear memory is a distinct address space from the SAB; a
   * `*mut u8` pointing into the SAB is not a valid pointer in the
   * kernel's memory, so the kernel cannot construct a
   * `ring::Sab::from_raw` over SAB bytes without a memcpy-each-way
   * through its own scratch region — and once the memcpy is on the
   * JS side, there is no remaining work for the kernel to do that
   * it does not already do inside the existing `kernel_dispatch`
   * export. The plan's §4 block is correct in substance; only the
   * language split moves.
   */
  serviceSab(pid, sab) {
    if (sab.byteLength < SAB_SIZE) {
      throw new Error(
        `KernelWasmHost.serviceSab: sab is ${sab.byteLength} bytes, need ${SAB_SIZE}`
      );
    }
    const buffer = sab.buffer;
    const baseOffset = sab.byteOffset;
    const header = new Int32Array(buffer, baseOffset, OFF_HEAP_SCRATCH / 4);
    const reqHead = Atomics.load(header, OFF_REQ_HEAD / 4);
    const reqTail = Atomics.load(header, OFF_REQ_TAIL / 4);
    if (reqHead === reqTail) {
      return 1;
    }
    const reqSlotIx = (reqTail >>> 0) % REQ_SLOT_COUNT;
    const reqSlotOffset = baseOffset + OFF_REQ_RING + reqSlotIx * SLOT_SIZE;
    const requestBytes = new Uint8Array(buffer, reqSlotOffset, SLOT_SIZE);
    const decoded = decodeRequest(requestBytes);
    let heapIn;
    if (decoded.heapLen > 0) {
      if (decoded.heapPtr + decoded.heapLen > HEAP_SCRATCH_BYTES || decoded.heapPtr > HEAP_SCRATCH_BYTES) {
        throw new Error(
          `KernelWasmHost.serviceSab: request heap ${decoded.heapPtr}+${decoded.heapLen} out of bounds (${HEAP_SCRATCH_BYTES})`
        );
      }
      const heapOffset = baseOffset + OFF_HEAP_SCRATCH + decoded.heapPtr;
      heapIn = new Uint8Array(
        new Uint8Array(buffer, heapOffset, decoded.heapLen)
      );
    }
    const { response, heapOut } = this.dispatch(
      pid,
      {
        opcode: decoded.opcode,
        flags: decoded.flags,
        requestId: decoded.requestId,
        args: decoded.args,
        heapPtr: 0,
        heapLen: decoded.heapLen
      },
      heapIn
    );
    const nextTail = (reqTail + 1 >>> 0) % REQ_SLOT_COUNT;
    Atomics.store(header, OFF_REQ_TAIL / 4, nextTail);
    const resHead = Atomics.load(header, OFF_RES_HEAD / 4);
    const resTail = Atomics.load(header, OFF_RES_TAIL / 4);
    const nextResHead = (resHead + 1 >>> 0) % RES_SLOT_COUNT;
    if (nextResHead === resTail) {
      throw new Error(
        `KernelWasmHost.serviceSab: response ring full for pid ${pid}`
      );
    }
    const resSlotIx = (resHead >>> 0) % RES_SLOT_COUNT;
    const resSlotOffset = baseOffset + OFF_RES_RING + resSlotIx * SLOT_SIZE;
    const resBytes = encodeResponse(response);
    new Uint8Array(buffer, resSlotOffset, SLOT_SIZE).set(resBytes);
    if (response.extraLen > 0 && heapOut.length > 0) {
      const heapOffset = baseOffset + OFF_HEAP_SCRATCH + decoded.heapPtr;
      new Uint8Array(buffer, heapOffset, response.extraLen).set(heapOut);
    }
    Atomics.store(header, OFF_RES_HEAD / 4, nextResHead);
    return 0;
  }
  // ---- Kernel interface --------------------------------------------
  /**
   * Push bytes into a kernel device's input ring. Implements the
   * tight `Kernel` interface the existing driver scaffold uses.
   *
   * `devnum` is a [`Devnum`] value (`kernel::fs::devfs::DEV_*`) —
   * one per device NODE. This matches the convention the driver
   * scaffold's `pushInputToKernel` passes through and the
   * preview-slice `MockKernel.injectInput` also uses. The three
   * wired nodes are `/dev/console`, `/dev/input_kbd`, and
   * `/dev/input_mouse`; block/net input is deferred (those devices
   * are driven by the TS drivers from the other direction and don't
   * have a kernel-side input ring).
   */
  injectInput(devnum, bytes) {
    let injectFn;
    let fnName;
    if (devnum === Devnum.Console) {
      injectFn = this.exports.kernel_inject_console_input;
      fnName = "kernel_inject_console_input";
    } else if (devnum === Devnum.InputKbd) {
      injectFn = this.exports.kernel_inject_input_kbd;
      fnName = "kernel_inject_input_kbd";
    } else if (devnum === Devnum.InputMouse) {
      injectFn = this.exports.kernel_inject_input_mouse;
      fnName = "kernel_inject_input_mouse";
    } else {
      throw new Error(
        `KernelWasmHost.injectInput: devnum ${devnum} not supported; wired device nodes are Devnum.Console (${Devnum.Console}), Devnum.InputKbd (${Devnum.InputKbd}), Devnum.InputMouse (${Devnum.InputMouse})`
      );
    }
    const heapCap = this.exports.kernel_heap_len();
    if (bytes.length > heapCap) {
      throw new Error(
        `KernelWasmHost.injectInput: ${bytes.length} bytes > heap capacity ${heapCap}`
      );
    }
    if (bytes.length === 0) return;
    const buf = this.exports.memory.buffer;
    const heapPtr = this.exports.kernel_heap_ptr();
    new Uint8Array(buf, heapPtr, bytes.length).set(bytes);
    const rc = injectFn(bytes.length);
    if (rc !== 0) {
      throw new Error(`KernelWasmHost.injectInput: ${fnName} returned ${rc}`);
    }
  }
  // ---- dispatch loop -------------------------------------------------
  /**
   * Shared kernel wake slot. 32 bytes backed by a `SharedArrayBuffer`
   * when the environment allows; a plain `ArrayBuffer` otherwise
   * (vitest under node). Every user Worker's `SabBackend` and the
   * main thread's `injectInput` routing bumps `index 0` via
   * `Atomics.add` + `Atomics.notify` so the kernel's dispatch loop
   * wakes from its `Atomics.waitAsync` park.
   *
   * The slot is semantically "wake counter": notifiers increment it,
   * the parker reads it before waiting, and a spurious-wake-free
   * park returns as soon as the counter changes. Production code
   * should NEVER mutate the counter directly — use `Atomics.add` +
   * `Atomics.notify` via a helper when that helper lands in T234.
   */
  get wakeSlot() {
    return this.wakeView;
  }
  /**
   * Round-robin dispatch loop. Services every live pid's SAB ring up
   * to `budget` requests per pass; parks on `parkFn` when a pass
   * completes without work; exits when `halted()` returns true.
   *
   * The dispatch loop is the kernel Worker's steady-state after boot
   * (T233 / M1.4): the bootstrap pid (synthetic parent of init) runs
   * one in-process `dispatch(PROC_SPAWN init)` to kick the system
   * into motion, then the loop takes over. Spawned children arrive
   * via `proc:sab` messages from main (router in `bootstrap.ts`),
   * exits arrive via `proc:exited` — both bump the pidMap the caller
   * passes through `pidSource`, so the loop picks up every
   * lifecycle change at the start of the next pass.
   *
   * `parkFn` defaults to a `SharedArrayBuffer`-backed
   * `Atomics.waitAsync` on the shared wake slot with a 50 ms timeout.
   * Under vitest (no cross-origin-isolated context), tests pass a
   * microtask-yield stub so the loop never actually blocks — the
   * test seeds the rings synchronously anyway.
   *
   * The loop is purely cooperative: a user Worker that never calls
   * a syscall ties up only its own Worker thread. That matches
   * `multi-process-plan.md §1` "Non-goals: pre-emption".
   */
  async startDispatchLoop(args) {
    const budget = args.budget ?? 8;
    const parkFn = args.parkFn ?? (() => this.defaultPark());
    const haveSharedArrayBuffer = typeof SharedArrayBuffer !== "undefined";
    while (!args.halted()) {
      let anyServiced = false;
      const pids = args.pidSource();
      for (const [pid, sab] of pids) {
        const view = new Uint8Array(sab);
        const header = new Int32Array(sab, 0, OFF_HEAP_SCRATCH / 4);
        const sabIsShared = haveSharedArrayBuffer && sab instanceof SharedArrayBuffer;
        for (let i = 0; i < budget; i++) {
          const rc = this.serviceSab(pid, view);
          if (rc === 1) break;
          anyServiced = true;
          Atomics.store(header, OFF_USER_WAIT_SLOT / 4, STATUS_READY);
          if (sabIsShared) {
            Atomics.notify(header, OFF_USER_WAIT_SLOT / 4);
          }
        }
      }
      if (args.halted()) return;
      if (!anyServiced) {
        await parkFn();
      }
    }
  }
  /**
   * Default [`startDispatchLoop`] park. `Atomics.waitAsync` on the
   * shared wake slot with a 50 ms timeout when the wake buffer is a
   * real `SharedArrayBuffer` and `Atomics.waitAsync` is available;
   * otherwise a microtask yield (`setTimeout(0)`) so the event loop
   * still gets a chance to drain messages between busy-spin passes.
   *
   * The 50 ms timeout exists as a belt-and-suspenders against lost
   * notifications; `Atomics.notify` never drops under contention, but
   * a caller that writes the wake slot before the kernel parks would
   * be a lost wake if the kernel waited forever. The plan §10 notes
   * 50 ms keeps well below the Principle IX 100 ms input budget.
   */
  async defaultPark() {
    if (typeof SharedArrayBuffer !== "undefined" && this.wakeBuffer instanceof SharedArrayBuffer && typeof Atomics.waitAsync === "function") {
      const last = Atomics.load(this.wakeView, 0);
      const r = Atomics.waitAsync(this.wakeView, 0, last, 50);
      if (r.async) {
        await r.value;
      }
      return;
    }
    await new Promise((resolve) => {
      setTimeout(resolve, 0);
    });
  }
};

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
  if (snapshot.cursor) {
    drawMouseCursor(
      pixels,
      width,
      height,
      snapshot.cursor.x,
      snapshot.cursor.y,
      palette.cursor
    );
  }
  return pixels;
}
var MOUSE_CURSOR_SPRITE = [
  // Horizontal bar.
  [-2, 0],
  [-1, 0],
  [0, 0],
  [1, 0],
  [2, 0],
  // Vertical bar (excluding center, already drawn).
  [0, -2],
  [0, -1],
  [0, 1],
  [0, 2]
];
function drawMouseCursor(pixels, fbWidth, fbHeight, x, y, argb) {
  for (const [dx, dy] of MOUSE_CURSOR_SPRITE) {
    setPixel(pixels, fbWidth, fbHeight, x + dx, y + dy, argb);
  }
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
  const r = argb >>> 16 & 255;
  const g = argb >>> 8 & 255;
  const b = argb & 255;
  const a = argb >>> 24 & 255;
  for (let i = 0; i < pixels.length; i += BYTES_PER_PIXEL) {
    pixels[i] = r;
    pixels[i + 1] = g;
    pixels[i + 2] = b;
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
  const r = argb >>> 16 & 255;
  const g = argb >>> 8 & 255;
  const b = argb & 255;
  const a = argb >>> 24 & 255;
  pixels[idx] = r;
  pixels[idx + 1] = g;
  pixels[idx + 2] = b;
  pixels[idx + 3] = a;
}

// src/shared/input-proto.ts
var MOUSE_EVENT_SIZE = 20;
var MouseEventKind = {
  /** Pointer moved to (x, y) in screen space. */
  Motion: 0,
  /** A mouse button was pressed or released at (x, y). */
  Button: 1
};
var MouseButtonState = {
  Released: 0,
  Pressed: 1
};
function unpackMouseEvent(bytes) {
  if (bytes.byteLength < MOUSE_EVENT_SIZE) {
    return null;
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const kind = view.getUint32(0, true);
  if (kind !== MouseEventKind.Motion && kind !== MouseEventKind.Button) {
    return null;
  }
  const x = view.getInt32(4, true);
  const y = view.getInt32(8, true);
  const button = view.getUint32(12, true);
  const stateRaw = view.getUint32(16, true);
  if (stateRaw !== MouseButtonState.Released && stateRaw !== MouseButtonState.Pressed) {
    return null;
  }
  return {
    kind,
    x,
    y,
    button,
    state: stateRaw
  };
}
var KBD_EVENT_SIZE = 8;
var KbdKeyState = {
  Released: 0,
  Pressed: 1
};
function unpackKbdEvent(bytes) {
  if (bytes.byteLength < KBD_EVENT_SIZE) {
    return null;
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const key = view.getUint32(0, true);
  const stateRaw = view.getUint32(4, true);
  if (stateRaw !== KbdKeyState.Released && stateRaw !== KbdKeyState.Pressed) {
    return null;
  }
  return { key, state: stateRaw };
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
  /** Most recent decoded pointer position. `null` until the
   * first mouse motion event is injected.
   */
  pointer = null;
  /** Most recent button event, press or release. `null`
   * until the first button event arrives.
   */
  lastButton = null;
  /** Total number of mouse events the kernel has consumed. */
  mouseEventCount = 0;
  /** Total number of keyboard events consumed via the
   * `/dev/input/kbd` path (distinct from the console input
   * path the live terminal uses for scrollback).
   */
  kbdEventCount = 0;
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
    if (devnum === DEV_INPUT_MOUSE_NODE) {
      this.injectMouseEvent(bytes);
      return;
    }
    if (devnum === DEV_INPUT_KBD_NODE) {
      this.injectKbdEvent(bytes);
      return;
    }
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
   * Decode a packed mouse event from the `/dev/input/mouse`
   * device ring and update the tracked pointer state. A
   * motion event updates `pointer`; a button event updates
   * both `pointer` and `lastButton`. Malformed bytes are
   * silently dropped (the packer + unpacker are symmetric,
   * so the only failure mode is a length mismatch caused
   * by a caller bug).
   *
   * When live-terminal mode is on and a fresh pointer
   * position changes anything visible, the kernel re-renders
   * so the future cursor-drawing slice can land without
   * re-plumbing the blit trigger.
   */
  injectMouseEvent(bytes) {
    const evt = unpackMouseEvent(bytes);
    if (!evt) {
      return;
    }
    this.mouseEventCount += 1;
    this.pointer = { x: evt.x, y: evt.y };
    if (evt.kind === MouseEventKind.Button) {
      this.lastButton = evt;
    }
    if (this.liveTerminal) {
      this.renderAndBlit();
    }
  }
  /**
   * Decode a packed keyboard event from the
   * `/dev/input/kbd` device ring. v1 only records the
   * event in a counter for tests; real consumption
   * (focused-window routing, scancode → ASCII) lands with
   * the next slice that wires this path into the live
   * terminal. The existing `console:input` bytes path
   * still delivers typed characters to the scrollback so
   * the browser demo's typing behaviour is unchanged.
   */
  injectKbdEvent(bytes) {
    const evt = unpackKbdEvent(bytes);
    if (!evt) {
      return;
    }
    this.kbdEventCount += 1;
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
      prompt: this.prompt,
      ...this.pointer ? { cursor: this.pointer } : {}
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
  /** Most recent pointer position seen via
   * `/dev/input/mouse`, or `null` if no mouse event has
   * been injected yet. */
  get pointerPosition() {
    return this.pointer === null ? null : { ...this.pointer };
  }
  /** Most recent button event, or `null` if none has
   * been injected. */
  get lastMouseButton() {
    return this.lastButton;
  }
  /** Total mouse events consumed. */
  get mouseEventsObserved() {
    return this.mouseEventCount;
  }
  /** Total keyboard events consumed via `/dev/input/kbd`. */
  get kbdEventsObserved() {
    return this.kbdEventCount;
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
function installWorkerEntry(messaging, options = {}) {
  let scaffold;
  let realKernel;
  let resolveReady;
  const whenReady = new Promise((resolve) => {
    resolveReady = resolve;
  });
  const pidMap = /* @__PURE__ */ new Map();
  const lifecycle = { hasEverSpawned: false };
  messaging.onmessage = (ev) => {
    const msg = ev.data;
    if (msg.kind === "proc:sab") {
      pidMap.set(msg.pid, msg.sab);
      lifecycle.hasEverSpawned = true;
      return;
    }
    if (msg.kind === "proc:exited") {
      pidMap.delete(msg.pid);
      return;
    }
    if (scaffold === void 0) {
      if (msg.kind !== "boot") {
        messaging.postMessage({
          kind: "panic",
          message: `kernel-worker: ${msg.kind} received before boot`
        });
        return;
      }
      if (msg.config.useRealKernel === true) {
        void bootRealKernel(
          messaging,
          msg.config,
          options,
          pidMap,
          lifecycle,
          (s, h) => {
            scaffold = s;
            realKernel = h;
          }
        ).then(() => resolveReady());
        return;
      }
      scaffold = bootMockKernel(messaging, msg.config);
      resolveReady();
      return;
    }
    scaffold.handleMainMessage(msg);
  };
  return {
    get scaffold() {
      return scaffold;
    },
    get realKernel() {
      return realKernel;
    },
    whenReady
  };
}
function bootMockKernel(messaging, config) {
  const liveTerminal = config.liveTerminal === true && config.enableFramebuffer;
  const initialScrollback = liveTerminal ? (config.terminalBanner ?? []).map(
    (text) => ({ text, kind: "banner" })
  ) : void 0;
  const mock = new MockKernel({
    policy: { kind: "faux-shell" },
    emitSplashOnFirstInput: config.enableFramebuffer && !liveTerminal,
    liveTerminal,
    ...initialScrollback ? { initialScrollback } : {},
    panicEmit: (message) => {
      messaging.postMessage({ kind: "panic", message });
    }
  });
  const scaffold = bootKernelWorker({
    kernel: mock,
    config,
    postToMain(out) {
      messaging.postMessage(out);
    }
  });
  mock.bindScaffold(scaffold);
  return scaffold;
}
async function bootRealKernel(messaging, config, options, pidMap, lifecycle, onScaffoldReady) {
  const fetcher = options.fetcher ?? defaultFetcher;
  let bytes;
  try {
    bytes = options.kernelWasmBytes ?? await fetcher("/assets/kernel.wasm");
  } catch (e) {
    const message = `kernel-worker: failed to load /assets/kernel.wasm: ${String(e)}`;
    messaging.postMessage({ kind: "panic", message });
    throw e;
  }
  let registry = options.binaryRegistry;
  if (registry === void 0 && config.bootBinary !== void 0) {
    try {
      registry = await fetchBinaryRegistry(fetcher);
    } catch (e) {
      const message = `kernel-worker: failed to populate binary registry: ${String(e)}`;
      messaging.postMessage({ kind: "panic", message });
      throw e;
    }
  }
  const host = await KernelWasmHost.create(bytes, {
    // Bytes the kernel flushes from `/dev/console` ride the existing
    // ConsoleHost main-thread channel as `console:write` messages,
    // so the boot screen + live terminal don't need to know whether
    // the source was MockKernel or KernelWasmHost.
    onConsoleWrite: (bytes2) => {
      messaging.postMessage({ kind: "console:write", bytes: bytes2 });
    },
    onPanic: (message) => {
      messaging.postMessage({ kind: "panic", message });
    },
    ...registry !== void 0 ? { binaryRegistry: registry } : {},
    kernelWorkerChannel: {
      postMessage: (msg) => {
        messaging.postMessage(msg);
      }
    }
  });
  const scaffold = bootKernelWorker({
    kernel: host,
    config,
    postToMain(out) {
      messaging.postMessage(out);
    }
  });
  onScaffoldReady(scaffold, host);
  messaging.postMessage({
    kind: "kernel:wake-slot",
    sab: host.wakeSlot.buffer
  });
  if (config.bootBinary !== void 0) {
    await runBootBinary(host, config.bootBinary, pidMap, lifecycle);
  }
}
async function defaultFetcher(url) {
  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`HTTP ${res.status} fetching ${url}`);
  }
  return res.arrayBuffer();
}
async function fetchBinaryRegistry(fetcher) {
  const manifestBuf = await fetcher("/manifest.json");
  const manifestJson = new TextDecoder().decode(new Uint8Array(manifestBuf));
  const manifest = JSON.parse(manifestJson);
  const binAssets = manifest.assets.filter(
    (a) => a.startsWith("assets/bin/") && a.endsWith(".wasm")
  );
  const entries = await Promise.all(
    binAssets.map(async (asset) => {
      const stem = asset.slice("assets/bin/".length, -".wasm".length);
      const bytes = await fetcher(`/${asset}`);
      return [`/bin/${stem}`, bytes];
    })
  );
  return new Map(entries);
}
async function runBootBinary(host, bootBinary, pidMap, lifecycle) {
  const bootstrapPid = host.registerProcess(CAPSET_ALL);
  host.installConsoleFd(bootstrapPid, 0);
  host.installConsoleFd(bootstrapPid, 1);
  host.installConsoleFd(bootstrapPid, 2);
  host.markRunning(bootstrapPid);
  const manifest = encodeSpawnManifest({
    path: bootBinary,
    caps: CAPSET_ALL
  });
  const { response } = host.dispatch(
    bootstrapPid,
    {
      opcode: OP_EXT.PROC_SPAWN,
      requestId: 1,
      args: manifest.args,
      heapPtr: 0,
      heapLen: manifest.heap.length
    },
    manifest.heap
  );
  if (response.status !== 0) {
    throw new Error(
      `kernel-worker: PROC_SPAWN(${bootBinary}) failed with status ${response.status}`
    );
  }
  await host.startDispatchLoop({
    pidSource: () => pidMap,
    halted: () => lifecycle.hasEverSpawned && pidMap.size === 0
  });
}
if (typeof DedicatedWorkerGlobalScope !== "undefined" && typeof self !== "undefined" && self instanceof DedicatedWorkerGlobalScope) {
  installWorkerEntry(self);
}
export {
  installWorkerEntry
};
