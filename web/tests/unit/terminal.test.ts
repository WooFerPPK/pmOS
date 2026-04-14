// Unit tests for the canvas terminal's state machine.
//
// These test only the state side — feedKey, appendOutput,
// clear, snapshot, etc. — because the render path depends
// on CanvasRenderingContext2D which is not available under
// the default Vitest environment.

import { describe, expect, it } from "vitest";
import { Terminal } from "../../src/terminal";

describe("Terminal state machine", () => {
  it("starts empty when no banner is provided", () => {
    const t = new Terminal({ maxLines: 10 });
    expect(t.isEmpty()).toBe(true);
    expect(t.snapshot().lines).toHaveLength(0);
    expect(t.input).toBe("");
  });

  it("banner lines populate scrollback at construction time", () => {
    const t = new Terminal({
      maxLines: 10,
      banner: ["line one", "line two"],
    });
    const snap = t.snapshot();
    expect(snap.lines).toHaveLength(2);
    expect(snap.lines[0]?.text).toBe("line one");
    expect(snap.lines[1]?.text).toBe("line two");
    expect(snap.lines.every((l) => l.kind === "output")).toBe(true);
  });

  it("throws when maxLines <= 0", () => {
    expect(() => new Terminal({ maxLines: 0 })).toThrow();
    expect(() => new Terminal({ maxLines: -5 })).toThrow();
  });

  it("feedKey(printable) appends to the input buffer without committing", () => {
    const t = new Terminal({ maxLines: 10 });
    expect(t.feedKey("a")).toBeNull();
    expect(t.feedKey("b")).toBeNull();
    expect(t.feedKey("c")).toBeNull();
    expect(t.input).toBe("abc");
    expect(t.snapshot().lines).toHaveLength(0);
  });

  it("feedKey(Backspace) removes the last character", () => {
    const t = new Terminal({ maxLines: 10 });
    t.feedKey("h");
    t.feedKey("i");
    t.feedKey("!");
    expect(t.feedKey("Backspace")).toBeNull();
    expect(t.input).toBe("hi");
    t.feedKey("Backspace");
    t.feedKey("Backspace");
    expect(t.input).toBe("");
    // Extra Backspace on an empty buffer is harmless.
    expect(t.feedKey("Backspace")).toBeNull();
    expect(t.input).toBe("");
  });

  it("feedKey(Enter) commits the buffer and returns bytes with a trailing newline", () => {
    const t = new Terminal({ maxLines: 10 });
    t.feedKey("e");
    t.feedKey("c");
    t.feedKey("h");
    t.feedKey("o");
    t.feedKey(" ");
    t.feedKey("h");
    t.feedKey("i");
    const committed = t.feedKey("Enter");
    expect(committed).not.toBeNull();
    if (committed) {
      expect(new TextDecoder().decode(committed)).toBe("echo hi\n");
    }
    expect(t.input).toBe("");
    // The committed line shows up in scrollback with the prompt prefix.
    const snap = t.snapshot();
    expect(snap.lines).toHaveLength(1);
    expect(snap.lines[0]?.text).toBe("> echo hi");
    expect(snap.lines[0]?.kind).toBe("input");
  });

  it("feedKey(Enter) on an empty buffer still commits a blank line", () => {
    const t = new Terminal({ maxLines: 10 });
    const committed = t.feedKey("Enter");
    expect(committed).not.toBeNull();
    if (committed) {
      expect(new TextDecoder().decode(committed)).toBe("\n");
    }
    const snap = t.snapshot();
    expect(snap.lines).toHaveLength(1);
    expect(snap.lines[0]?.text).toBe("> ");
  });

  it("feedKey ignores Shift, Alt, arrow keys, and other non-printable names", () => {
    const t = new Terminal({ maxLines: 10 });
    for (const k of ["Shift", "Alt", "Control", "Meta", "ArrowUp", "ArrowDown", "F1", "Escape", "Tab"]) {
      expect(t.feedKey(k)).toBeNull();
    }
    expect(t.input).toBe("");
  });

  it("appendOutput splits incoming bytes on newlines into separate scrollback lines", () => {
    const t = new Terminal({ maxLines: 10 });
    t.appendOutput(new TextEncoder().encode("first\nsecond\n"));
    const snap = t.snapshot();
    expect(snap.lines).toHaveLength(2);
    expect(snap.lines[0]?.text).toBe("first");
    expect(snap.lines[1]?.text).toBe("second");
    expect(snap.lines.every((l) => l.kind === "output")).toBe(true);
  });

  it("appendOutput buffers partial lines until the next newline arrives", () => {
    const t = new Terminal({ maxLines: 10 });
    t.appendOutput(new TextEncoder().encode("par"));
    // No line yet — the chunk is buffered.
    expect(t.snapshot().lines).toHaveLength(0);
    t.appendOutput(new TextEncoder().encode("tial\n"));
    const snap = t.snapshot();
    expect(snap.lines).toHaveLength(1);
    expect(snap.lines[0]?.text).toBe("partial");
  });

  it("interleaved input and output preserves both line kinds in order", () => {
    const t = new Terminal({ maxLines: 20 });
    t.appendOutput(new TextEncoder().encode("welcome\n"));
    t.feedKey("e");
    t.feedKey("c");
    t.feedKey("h");
    t.feedKey("o");
    t.feedKey(" ");
    t.feedKey("h");
    t.feedKey("i");
    t.feedKey("Enter");
    t.appendOutput(new TextEncoder().encode("hi\n"));
    const snap = t.snapshot();
    expect(snap.lines).toHaveLength(3);
    expect(snap.lines[0]).toEqual({ text: "welcome", kind: "output" });
    expect(snap.lines[1]).toEqual({ text: "> echo hi", kind: "input" });
    expect(snap.lines[2]).toEqual({ text: "hi", kind: "output" });
  });

  it("scrollback is bounded by maxLines and older lines fall off", () => {
    const t = new Terminal({ maxLines: 3 });
    t.appendOutput(new TextEncoder().encode("a\nb\nc\nd\ne\n"));
    const snap = t.snapshot();
    expect(snap.lines).toHaveLength(3);
    // The first two lines ("a", "b") rolled off.
    expect(snap.lines.map((l) => l.text)).toEqual(["c", "d", "e"]);
  });

  it("clear() wipes scrollback, pending output, and the input buffer", () => {
    const t = new Terminal({ maxLines: 10, banner: ["boot"] });
    t.appendOutput(new TextEncoder().encode("partial"));
    t.feedKey("x");
    t.clear();
    expect(t.snapshot().lines).toHaveLength(0);
    expect(t.input).toBe("");
    // A subsequent newline for the previously-pending
    // content does NOT resurrect it.
    t.appendOutput(new TextEncoder().encode("\n"));
    const snap = t.snapshot();
    expect(snap.lines).toHaveLength(1);
    expect(snap.lines[0]?.text).toBe("");
  });

  it("appendOutput handles multi-byte UTF-8 across chunks", () => {
    const t = new Terminal({ maxLines: 5 });
    // "é" is 0xC3 0xA9 in UTF-8 — split the two bytes
    // across two appendOutput calls to exercise the
    // streaming decoder.
    t.appendOutput(new Uint8Array([0xc3]));
    t.appendOutput(new Uint8Array([0xa9, 0x0a]));
    const snap = t.snapshot();
    expect(snap.lines).toHaveLength(1);
    expect(snap.lines[0]?.text).toBe("é");
  });
});
