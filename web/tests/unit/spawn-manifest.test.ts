import { describe, expect, it } from "vitest";

import {
  encodeSpawnManifest,
  encodeSpawnManifestBlob,
  isValidSpawnManifestBlob,
  SPAWN_V1_HEADER_LEN,
  SPAWN_V1_MAGIC,
  SPAWN_V1_VERSION,
} from "../../src/shared/syscall";

describe("canonical proc_spawn manifest", () => {
  it("encodes every documented field with pipe-safe fd mappings", () => {
    const { args, heap } = encodeSpawnManifest({
      path: "/bin/grep",
      argv: ["grep", "two words"],
      envp: [
        ["PATH", "/bin"],
        ["MODE", "test"],
      ],
      stdinFd: 8,
      stdoutFd: 9,
      stderrFd: 2,
      extraFds: [[11, 7]],
      cwd: "/home/user",
      caps: 0x1234n,
    });

    const request = new DataView(args.buffer);
    expect(request.getUint32(0, true)).toBe(SPAWN_V1_MAGIC);
    expect(request.getUint32(4, true)).toBe(heap.length);
    expect(request.getUint16(8, true)).toBe(SPAWN_V1_VERSION);
    expect(args.slice(10)).toEqual(new Uint8Array(6));

    const header = new DataView(heap.buffer, heap.byteOffset, heap.byteLength);
    expect(header.getUint32(0, true)).toBe(SPAWN_V1_MAGIC);
    expect(header.getUint16(4, true)).toBe(SPAWN_V1_VERSION);
    expect(header.getUint16(6, true)).toBe(0x0003);
    expect(header.getUint32(8, true)).toBe(heap.length);
    expect(header.getUint16(12, true)).toBe(9);
    expect(header.getUint16(14, true)).toBe(10);
    expect(header.getUint16(16, true)).toBe(2);
    expect(header.getUint16(18, true)).toBe(2);
    expect(header.getUint16(20, true)).toBe(1);
    expect(header.getInt32(24, true)).toBe(8);
    expect(header.getInt32(28, true)).toBe(9);
    expect(header.getInt32(32, true)).toBe(2);
    expect(header.getBigUint64(40, true)).toBe(0x1234n);
    expect(isValidSpawnManifestBlob(heap)).toBe(true);

    const text = new TextDecoder();
    expect(text.decode(heap.subarray(SPAWN_V1_HEADER_LEN, SPAWN_V1_HEADER_LEN + 9))).toBe(
      "/bin/grep",
    );
  });

  it("encodes omitted cwd/caps/stdio as inheritance", () => {
    const { heap } = encodeSpawnManifest({ path: "/bin/ls", argv: ["ls"] });
    const header = new DataView(heap.buffer, heap.byteOffset, heap.byteLength);
    expect(header.getUint16(6, true)).toBe(0);
    expect(header.getUint16(14, true)).toBe(0);
    expect(header.getInt32(24, true)).toBe(-1);
    expect(header.getInt32(28, true)).toBe(-1);
    expect(header.getInt32(32, true)).toBe(-1);
    expect(header.getBigUint64(40, true)).toBe(0n);
  });

  it("takes ownership when wrapping a user-memory blob", () => {
    const original = encodeSpawnManifest({ path: "/bin/ls", argv: ["ls"] }).heap;
    const wrapped = encodeSpawnManifestBlob(original);
    original.fill(0);
    expect(wrapped.heap[0]).not.toBe(0);
    expect(isValidSpawnManifestBlob(wrapped.heap)).toBe(true);
  });

  it("rejects relative paths, reserved fd targets, duplicates, and malformed lengths", () => {
    expect(() => encodeSpawnManifest({ path: "bin/ls" })).toThrow(/absolute/);
    expect(() =>
      encodeSpawnManifest({ path: "/bin/ls", extraFds: [[1, 4]] }),
    ).toThrow(/child fd/);
    expect(() =>
      encodeSpawnManifest({
        path: "/bin/ls",
        extraFds: [
          [1, 7],
          [2, 7],
        ],
      }),
    ).toThrow(/duplicate/);

    const valid = encodeSpawnManifest({ path: "/bin/ls" }).heap;
    const truncated = valid.slice(0, valid.length - 1);
    expect(isValidSpawnManifestBlob(truncated)).toBe(false);
    expect(() => encodeSpawnManifestBlob(truncated)).toThrow(/malformed/);
  });
});
