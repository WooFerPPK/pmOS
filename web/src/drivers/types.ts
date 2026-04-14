// Shared driver interface + result types.
//
// Every TS-side driver module (console, block, fb, input, net)
// implements this interface. The kernel-worker scaffold in
// `../kernel-worker.ts` registers one driver per `DriverId` and
// routes `driver_call` requests from the Rust kernel into the
// matching driver.
//
// Drivers run in the kernel Worker's thread. They do NOT have
// direct access to the DOM — they post messages to the main
// thread via the `DriverHost` handed to them at `init` time,
// which owns the Worker<->main channel.
//
// Two numeric namespaces appear in this layer; they are distinct
// and should not be conflated:
//
//   * **DriverId** — driver CLASS identifier. Mirrors
//     `kernel::platform::DevId` on the Rust side. The kernel's
//     `Platform::driver_call(dev, op, args)` routes by this.
//     Values are small and stable: 0 = Framebuffer, 1 = InputKbd,
//     2 = InputMouse, 3 = Block, 4 = Net, 5 = Console. One
//     driver instance per class.
//
//   * **Devnum** — per-device NODE identifier. Mirrors the u32
//     inside `NodeType::CharDevice(devnum)` on the Rust side,
//     defined by `kernel::fs::devfs::DEV_*`. The kernel's
//     internal `DeviceDispatcher::read/write` pattern-matches
//     on this. Driver code uses it when pushing input bytes
//     via `DriverHost::pushInputToKernel`, because the kernel
//     has one input ring per devnum.

/** Driver-class identifier. See module header. */
export type DriverId = number;

/** Per-device-node identifier. See module header. */
export type Devnum = number;

/** A device-specific opcode number. */
export type DriverOp = number;

/** Maps to `abi::platform::DriverError` variants. */
export const DriverErrorCode = {
  /** Driver isn't wired up yet or its backing resource is gone. */
  NotReady: 1,
  /** Transport error: bad payload, invalid opcode, etc. */
  Transport: 2,
  /** The driver reports a POSIX errno to the kernel. */
  Errno: 3,
} as const;

export type DriverErrorCode = (typeof DriverErrorCode)[keyof typeof DriverErrorCode];

/**
 * Non-error result of a driver call. `value` is a device-specific
 * integer (typically "bytes written" for write ops); `output` is
 * optional binary output that the kernel copies into its own
 * buffer.
 */
export interface DriverOk {
  readonly ok: true;
  readonly value: number;
  readonly output?: Uint8Array;
}

/** Error result. */
export interface DriverErr {
  readonly ok: false;
  readonly error: DriverErrorCode;
}

export type DriverResult = DriverOk | DriverErr;

/**
 * The services a driver can request from its host (the kernel
 * Worker scaffold). Every driver is handed one at `init` time.
 */
export interface DriverHost {
  /** Post a message to the main thread. */
  postToMain(msg: unknown): void;
  /**
   * Push input bytes into the kernel's per-device-NODE input
   * ring. `devnum` is the devnum namespace (e.g.
   * DEV_CONSOLE = 4, DEV_INPUT_KBD = 20), NOT a `DriverId`.
   */
  pushInputToKernel(devnum: Devnum, bytes: Uint8Array): void;
}

/**
 * The interface every driver implements.
 *
 * * `init` is called once at scaffold boot time. The driver must
 *   retain `host` for the lifetime of the scaffold.
 * * `call` is invoked by the kernel (via `Platform::driver_call`)
 *   whenever a device node is written to or receives an ioctl.
 *   It runs synchronously on the kernel Worker's thread.
 * * `onHostMessage` is optional; the scaffold calls it when a
 *   main-thread message is addressed to this driver. Drivers
 *   that only emit output and never consume input (e.g. fb0 in
 *   v1) can omit it.
 */
export interface Driver {
  /** Driver-class id — the scaffold's map key. */
  readonly driverId: DriverId;
  readonly name: string;

  init(host: DriverHost): void;
  call(op: DriverOp, payload: Uint8Array): DriverResult;
  onHostMessage?(msg: unknown): void;
}
