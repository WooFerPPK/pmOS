import { describe, expect, it, vi } from "vitest";

import { showStorageRecoveryGate } from "../../src/storage-recovery";
import type { StorageDegradedMessage } from "../../src/storage-recovery";

type FakeListener = (event: Event) => void;

class FakeElement {
  id = "";
  type = "";
  textContent: string | null = null;
  readonly dataset: Record<string, string> = {};
  readonly style = { cssText: "", overflow: "" };
  readonly children: FakeElement[] = [];
  private parent: FakeElement | null = null;
  private readonly listeners = new Map<string, FakeListener[]>();

  append(...children: FakeElement[]): void {
    for (const child of children) {
      child.parent = this;
      this.children.push(child);
    }
  }

  addEventListener(type: string, listener: FakeListener): void {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  dispatch(type: string): void {
    const event = makeEvent(this);
    for (const listener of this.listeners.get(type) ?? []) listener(event.event);
  }

  contains(candidate: unknown): boolean {
    if (candidate === this) return true;
    return this.children.some((child) => child.contains(candidate));
  }

  remove(): void {
    if (this.parent === null) return;
    const index = this.parent.children.indexOf(this);
    if (index >= 0) this.parent.children.splice(index, 1);
    this.parent = null;
  }
}

class FakeDocument {
  readonly body = new FakeElement();
  private readonly listeners = new Map<string, FakeListener[]>();

  createElement(_tag: string): FakeElement {
    return new FakeElement();
  }

  getElementById(id: string): FakeElement | null {
    const visit = (element: FakeElement): FakeElement | null => {
      if (element.id === id) return element;
      for (const child of element.children) {
        const found = visit(child);
        if (found !== null) return found;
      }
      return null;
    };
    return visit(this.body);
  }

  addEventListener(
    type: string,
    listener: FakeListener,
    _capture?: boolean,
  ): void {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(
    type: string,
    listener: FakeListener,
    _capture?: boolean,
  ): void {
    const listeners = this.listeners.get(type) ?? [];
    this.listeners.set(
      type,
      listeners.filter((candidate) => candidate !== listener),
    );
  }

  dispatch(type: string, target: FakeElement): ReturnType<typeof makeEvent> {
    const event = makeEvent(target);
    for (const listener of this.listeners.get(type) ?? []) {
      listener(event.event);
      if (event.immediatePropagationStopped) break;
    }
    return event;
  }
}

function makeEvent(target: FakeElement): {
  readonly event: Event;
  readonly defaultPrevented: boolean;
  readonly immediatePropagationStopped: boolean;
} {
  let defaultPrevented = false;
  let immediatePropagationStopped = false;
  const event = {
    target,
    preventDefault: () => {
      defaultPrevented = true;
    },
    stopImmediatePropagation: () => {
      immediatePropagationStopped = true;
    },
  } as unknown as Event;
  return {
    event,
    get defaultPrevented() {
      return defaultPrevented;
    },
    get immediatePropagationStopped() {
      return immediatePropagationStopped;
    },
  };
}

function degradedMessage(): StorageDegradedMessage {
  return {
    kind: "storage:degraded",
    reason: "persistent-root-invalid",
    detail: "The superblock checksum did not validate.",
    existingImagePreserved: true,
  };
}

describe("persistent-storage recovery gate", () => {
  it("blocks the ordinary desktop by default without choosing a lossy fallback", () => {
    const document = new FakeDocument();
    const retry = vi.fn();
    const continueTemporary = vi.fn();

    const gate = showStorageRecoveryGate(degradedMessage(), {
      document: document as unknown as Document,
      onRetry: retry,
      onContinueTemporary: continueTemporary,
    });

    expect(gate.blocked).toBe(true);
    expect(document.body.dataset["pmosStorageState"]).toBe("degraded-blocked");
    expect(document.getElementById("pmos-storage-recovery")).not.toBeNull();
    expect(
      document.getElementById("pmos-storage-continue-temporary")?.textContent,
    ).toBe("Continue temporary session — files will be lost on reload");
    expect(retry).not.toHaveBeenCalled();
    expect(continueTemporary).not.toHaveBeenCalled();

    const key = document.dispatch("keydown", document.body);
    expect(key.defaultPrevented).toBe(true);
    expect(key.immediatePropagationStopped).toBe(true);
  });

  it("keeps Retry blocked and reloads, while explicit continuation unlocks once", () => {
    const document = new FakeDocument();
    const retry = vi.fn();
    const continueTemporary = vi.fn();
    const gate = showStorageRecoveryGate(degradedMessage(), {
      document: document as unknown as Document,
      onRetry: retry,
      onContinueTemporary: continueTemporary,
    });

    document.getElementById("pmos-storage-retry")?.dispatch("click");
    expect(retry).toHaveBeenCalledOnce();
    expect(gate.blocked).toBe(true);

    document
      .getElementById("pmos-storage-continue-temporary")
      ?.dispatch("click");
    expect(continueTemporary).toHaveBeenCalledOnce();
    expect(gate.blocked).toBe(false);
    expect(document.body.dataset["pmosStorageState"]).toBe("temporary");
    expect(document.getElementById("pmos-storage-recovery")).toBeNull();

    const key = document.dispatch("keydown", document.body);
    expect(key.defaultPrevented).toBe(false);
    expect(key.immediatePropagationStopped).toBe(false);
  });
});
