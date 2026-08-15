import { describe, expect, it } from "vitest";

import {
  boundedConsoleTail,
  ConsoleTranscript,
} from "../../src/console-transcript";

describe("ConsoleTranscript", () => {
  it("retains only the newest configured lines", () => {
    const sink = { textContent: null as string | null };
    const transcript = new ConsoleTranscript(sink, {
      maxBytes: 1024,
      maxLines: 3,
    });

    transcript.append("one\ntwo\n");
    transcript.append("three\nfour\n");

    expect(transcript.text).toBe("two\nthree\nfour\n");
    expect(sink.textContent).toBe(transcript.text);
  });

  it("retains a valid UTF-8 suffix within the byte ceiling", () => {
    const tail = boundedConsoleTail("old:abcdef🙂tail", {
      maxBytes: 8,
      maxLines: 20,
    });

    expect(tail).toBe("🙂tail");
    expect(new TextEncoder().encode(tail).byteLength).toBeLessThanOrEqual(8);
    expect(tail).not.toContain("�");
  });

  it("renders by assignment without reading and appending sink.textContent", () => {
    const writes: string[] = [];
    const sink = {} as { textContent: string | null };
    Object.defineProperty(sink, "textContent", {
      configurable: true,
      get(): never {
        throw new Error("renderer must not read the DOM transcript");
      },
      set(value: string | null): void {
        writes.push(value ?? "");
      },
    });
    const transcript = new ConsoleTranscript(sink, {
      maxBytes: 5,
      maxLines: 10,
    });

    transcript.append("abc");
    transcript.append("def");

    expect(writes).toEqual(["abc", "bcdef"]);
    expect(transcript.text).toBe("bcdef");
  });

  it("stays bounded during a long-running diagnostic stream", () => {
    const sink = { textContent: null as string | null };
    const transcript = new ConsoleTranscript(sink, {
      maxBytes: 128,
      maxLines: 8,
    });
    for (let i = 0; i < 10_000; i += 1) {
      transcript.append(`diagnostic-${i}\n`);
    }

    expect(new TextEncoder().encode(transcript.text).byteLength).toBeLessThanOrEqual(128);
    expect(transcript.text.split("\n").length).toBeLessThanOrEqual(9);
    expect(transcript.text).toContain("diagnostic-9999");
    expect(transcript.text).not.toContain("diagnostic-0\n");
  });
});
