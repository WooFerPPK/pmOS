// Main-thread wrapper for the kernel Worker's console channel.
//
// `bootstrap.ts` (and any future main-thread caller that wants
// to surface console I/O to the user) constructs a `ConsoleHost`
// over a Worker, subscribes to `onOutput` for incoming bytes,
// and calls `sendInput` / `sendLine` to ship user-typed bytes
// back to the kernel. Lifecycle events (`ready`, `panic`) land
// on `onLifecycle` subscribers.
//
// Input sent before the `ready` message is queued; the host
// flushes it to the Worker as soon as boot completes, so
// bootstrap code can safely wire up a keyboard listener
// immediately after constructing the host without racing the
// Worker's boot.
//
// Tests supply a `WorkerLike` fake instead of a real Worker so
// the boot / message flow can be driven deterministically.

import type {
  BootConfig,
  KernelToMain,
  MainToKernel,
} from "./shared/worker-proto";

/**
 * Subset of the DOM Worker API `ConsoleHost` needs. A real
 * `Worker` satisfies it structurally; tests pass in a fake.
 */
export interface WorkerLike {
  postMessage(msg: MainToKernel): void;
  addEventListener(
    type: "message",
    handler: (ev: { data: KernelToMain }) => void,
  ): void;
  terminate(): void;
}

/** Handler for `console:write` events from the kernel. */
export type ConsoleOutputHandler = (bytes: Uint8Array) => void;

/** Lifecycle event shape. */
export type ConsoleLifecycleEvent =
  | { readonly kind: "ready" }
  | { readonly kind: "panic"; readonly message: string };

/** Handler for lifecycle events. */
export type ConsoleLifecycleHandler = (event: ConsoleLifecycleEvent) => void;

export interface ConsoleHostOptions {
  readonly worker: WorkerLike;
  readonly bootConfig: BootConfig;
}

export class ConsoleHost {
  private readonly worker: WorkerLike;
  private readonly outputHandlers: ConsoleOutputHandler[] = [];
  private readonly lifecycleHandlers: ConsoleLifecycleHandler[] = [];
  private isReady = false;
  private terminated = false;
  private queuedInput: Uint8Array[] = [];

  constructor(options: ConsoleHostOptions) {
    this.worker = options.worker;
    this.worker.addEventListener("message", (ev) => {
      this.handleMessage(ev.data);
    });
    this.worker.postMessage({ kind: "boot", config: options.bootConfig });
  }

  /** True once the Worker has posted `ready`. */
  get ready(): boolean {
    return this.isReady;
  }

  /** Send raw bytes as console input. */
  sendInput(bytes: Uint8Array): void {
    if (this.terminated) {
      return;
    }
    // Copy defensively: callers often recycle keystroke buffers.
    const copy = new Uint8Array(bytes.byteLength);
    copy.set(bytes);
    if (!this.isReady) {
      this.queuedInput.push(copy);
      return;
    }
    this.worker.postMessage({ kind: "console:input", bytes: copy });
  }

  /** Convenience: encode a UTF-8 string and send it as input. */
  sendLine(line: string): void {
    const bytes = new TextEncoder().encode(line);
    this.sendInput(bytes);
  }

  /** Subscribe to output bytes. Handlers are called in registration order. */
  onOutput(handler: ConsoleOutputHandler): void {
    this.outputHandlers.push(handler);
  }

  /** Subscribe to lifecycle events. */
  onLifecycle(handler: ConsoleLifecycleHandler): void {
    this.lifecycleHandlers.push(handler);
  }

  /** Post a shutdown message and terminate the worker. */
  shutdown(): void {
    if (this.terminated) {
      return;
    }
    this.terminated = true;
    this.worker.postMessage({ kind: "shutdown" });
    this.worker.terminate();
    this.isReady = false;
  }

  private handleMessage(msg: KernelToMain): void {
    switch (msg.kind) {
      case "ready": {
        this.isReady = true;
        // Flush any input queued before ready.
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
}
