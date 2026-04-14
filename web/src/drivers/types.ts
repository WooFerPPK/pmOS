// Shared driver interface + result types.
//
// Every TS-side driver module (console, block, fb, input, net)
// implements this interface. The kernel-worker scaffold in
// `../kernel-worker.ts` registers one driver per `DevId` and
// routes `driver_call` requests from the Rust kernel into the
// matching driver.
//
// Drivers run in the kernel Worker's thread. They do NOT have
// direct access to the DOM — they post messages to the main
// thread via the `DriverHost` handed to them at `init` time,
// which owns the Worker<->main channel.

/** A numeric device identifier. Matches `abi::platform::DevId`. */
export type DevId = number;

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
  /** Push input bytes into the kernel's per-device input ring. */
  pushInputToKernel(devnum: DevId, bytes: Uint8Array): void;
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
  readonly devId: DevId;
  readonly name: string;

  init(host: DriverHost): void;
  call(op: DriverOp, payload: Uint8Array): DriverResult;
  onHostMessage?(msg: unknown): void;
}
