import type { KernelToMain } from "./shared/worker-proto";

export type StorageDegradedMessage = Extract<
  KernelToMain,
  { readonly kind: "storage:degraded" }
>;

export interface StorageRecoveryGateOptions {
  readonly document?: Document;
  readonly onRetry?: () => void;
  readonly onContinueTemporary?: () => void;
}

export interface StorageRecoveryGate {
  readonly blocked: boolean;
}

interface MutableGateState {
  blocked: boolean;
  controller: StorageRecoveryGate;
  detail: HTMLElement;
}

const gates = new WeakMap<Document, MutableGateState>();

function describeReason(reason: StorageDegradedMessage["reason"]): string {
  switch (reason) {
    case "opfs-open-failed":
      return "The browser could not open PMos persistent storage.";
    case "persistent-root-unavailable":
      return "The persistent filesystem could not be installed as the root filesystem.";
    case "persistent-root-invalid":
      return "The existing PMos filesystem image could not be validated or mounted.";
  }
}

/**
 * Mount the mandatory persistent-storage recovery gate.
 *
 * The gate has no automatic dismissal: an ordinary desktop stays inaccessible
 * until Retry reloads the boot path or the user explicitly chooses the lossy
 * temporary session. Repeated degraded signals update the diagnostic without
 * re-blocking a session whose user already made that choice.
 */
export function showStorageRecoveryGate(
  message: StorageDegradedMessage,
  options: StorageRecoveryGateOptions = {},
): StorageRecoveryGate {
  const targetDocument = options.document ?? document;
  const existing = gates.get(targetDocument);
  if (existing !== undefined) {
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
    "font-family: system-ui, -apple-system, BlinkMacSystemFont, \"Segoe UI\", sans-serif",
    "pointer-events: auto",
  ].join("; ");

  const panel = targetDocument.createElement("div");
  panel.style.cssText = [
    "width: min(42rem, 100%)",
    "padding: 2rem",
    "box-sizing: border-box",
    "border: 1px solid #cf6a52",
    "border-radius: 0.75rem",
    "background: #151b24",
    "box-shadow: 0 1.5rem 5rem rgba(0, 0, 0, 0.65)",
  ].join("; ");

  const title = targetDocument.createElement("h1");
  title.id = "pmos-storage-recovery-title";
  title.textContent = "Persistent storage needs attention";
  title.style.cssText = "margin: 0 0 1rem; font-size: 1.65rem";

  const summary = targetDocument.createElement("p");
  summary.textContent =
    "PMos paused the desktop because it cannot currently guarantee that files will survive a reload.";
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
    "font-family: ui-monospace, \"SF Mono\", Menlo, Consolas, monospace",
    "font-size: 0.85rem",
    "overflow-wrap: anywhere",
  ].join("; ");

  const preservation = targetDocument.createElement("p");
  preservation.textContent =
    "Any existing pmos.img was left in place and was not reformatted or overwritten.";
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
    "cursor: pointer",
  ].join("; ");

  const continueTemporary = targetDocument.createElement("button");
  continueTemporary.id = "pmos-storage-continue-temporary";
  continueTemporary.type = "button";
  continueTemporary.textContent =
    "Continue temporary session — files will be lost on reload";
  continueTemporary.style.cssText = [
    "padding: 0.75rem 1rem",
    "border: 1px solid #cf6a52",
    "border-radius: 0.4rem",
    "font: inherit",
    "color: #ffd8cf",
    "background: transparent",
    "cursor: pointer",
  ].join("; ");

  actions.append(retry, continueTemporary);
  panel.append(title, summary, detail, preservation, actions);
  overlay.append(panel);
  targetDocument.body.append(overlay);

  const previousOverflow = targetDocument.body.style.overflow;
  targetDocument.body.style.overflow = "hidden";
  targetDocument.body.dataset["pmosStorageState"] = "degraded-blocked";

  const blockOutsideGate = (event: Event): void => {
    const target = event.target;
    if (target !== null && overlay.contains(target as Node)) return;
    event.preventDefault();
    event.stopImmediatePropagation();
  };
  targetDocument.addEventListener("keydown", blockOutsideGate, true);
  targetDocument.addEventListener("keyup", blockOutsideGate, true);

  const state = {} as MutableGateState;
  const controller: StorageRecoveryGate = {
    get blocked(): boolean {
      return state.blocked;
    },
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
