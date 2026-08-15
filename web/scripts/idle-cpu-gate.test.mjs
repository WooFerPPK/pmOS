import assert from "node:assert/strict";
import test from "node:test";

import {
  IdleCpuGateError,
  awaitWithinContext,
  latestSixAppDurableEvidence,
  parseSixAppDurableRecord,
  restoredSessionLifecycleEvidence,
} from "./idle-cpu-gate.mjs";

test("six-app durability evidence is exact, bounded, and selects the latest record", () => {
  const first =
    "[real-kernel] shell: session durable revision=7 apps=6 windows=6 bytes=321 digest=0123456789abcdef";
  const second =
    "shell: session durable revision=8 apps=6 windows=6 bytes=322 digest=fedcba9876543210";
  assert.deepEqual(parseSixAppDurableRecord(first), {
    revision: 7,
    bytes: 321,
    digest: "0123456789abcdef",
  });
  assert.deepEqual(latestSixAppDurableEvidence([first, `noise\n${second}`]), {
    revision: 8,
    bytes: 322,
    digest: "fedcba9876543210",
    record_index: 2,
  });
  for (const invalid of [
    "shell: session durable revision=0 apps=6 windows=6 bytes=321 digest=0123456789abcdef",
    "shell: session durable revision=7 apps=5 windows=6 bytes=321 digest=0123456789abcdef",
    "shell: session durable revision=7 apps=6 windows=5 bytes=321 digest=0123456789abcdef",
    "shell: session durable revision=7 apps=6 windows=6 bytes=0 digest=0123456789abcdef",
    "shell: session durable revision=7 apps=6 windows=6 bytes=321 digest=0123456789abcdeg",
    "shell: session durable revision=7 apps=6 windows=6 bytes=321 digest=0123456789abcdef trailing",
  ]) {
    assert.equal(parseSixAppDurableRecord(invalid), null, invalid);
  }
});

test("restored lifecycle and durability evidence are exact and causally ordered", () => {
  const before =
    "shell: session durable revision=1 apps=6 windows=6 bytes=300 digest=1111111111111111";
  const restored =
    "[real-kernel] shell: session restored status=completed apps=6 windows=6";
  const durable =
    "shell: session durable revision=2 apps=6 windows=6 bytes=301 digest=2222222222222222";
  const ready = "[real-kernel] shell: desktop ready";
  const lines = [before, restored, durable, ready];
  const lifecycle = restoredSessionLifecycleEvidence(lines);
  assert.deepEqual(lifecycle, {
    restored_record: restored,
    ready_record: ready,
    restored_index: 1,
    ready_index: 3,
  });
  assert.deepEqual(
    latestSixAppDurableEvidence(lines, lifecycle.restored_index),
    {
      revision: 2,
      bytes: 301,
      digest: "2222222222222222",
      record_index: 2,
    },
  );
  assert.equal(
    latestSixAppDurableEvidence([before, restored, ready], 1),
    null,
    "a pre-restore durable record must not settle the restored scene",
  );
  assert.equal(restoredSessionLifecycleEvidence([ready, restored]), null);
  assert.equal(
    restoredSessionLifecycleEvidence([
      "shell: session restored status=deadline apps=6 windows=6",
      ready,
    ]),
    null,
  );
  assert.equal(
    restoredSessionLifecycleEvidence([
      `${restored} trailing`,
      ready,
    ]),
    null,
  );
});

test("hard deadline rejects a never-resolving external operation", async () => {
  const controller = new AbortController();
  const started = performance.now();
  await assert.rejects(
    awaitWithinContext(
      () => new Promise(() => {}),
      { signal: controller.signal, deadline: performance.now() + 40 },
      "never-resolving fixture",
    ),
    (error) =>
      error instanceof IdleCpuGateError && error.code === "PHASE_TIMEOUT",
  );
  assert.ok(
    performance.now() - started < 1_000,
    "hard deadline must not wait for the underlying operation",
  );
});

test("abort reason interrupts an external operation before its deadline", async () => {
  const controller = new AbortController();
  const reason = new IdleCpuGateError("TEST_ABORT", "test abort");
  const pending = awaitWithinContext(
    () => new Promise(() => {}),
    { signal: controller.signal, deadline: performance.now() + 5_000 },
    "abort fixture",
  );
  controller.abort(reason);
  await assert.rejects(pending, (error) => error === reason);
});
