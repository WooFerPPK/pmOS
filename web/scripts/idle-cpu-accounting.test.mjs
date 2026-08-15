import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import test from "node:test";

import {
  ProcAccountingError,
  accountStableInterval,
  assertStableTree,
  buildDescendantTree,
  captureProcessGroup,
  captureProcessTree,
  parseCpuList,
  parseProcStat,
  parseProcStatusAffinity,
  passesIncrementalThreshold,
  processIdentity,
  selectDedicatedProcessGroup,
  selectConservativeBaselineIndex,
  serializeSnapshot,
  terminateLinuxProcessGroup,
  verifyDedicatedProcessGroup,
  verifyPinnedAffinity,
} from "./idle-cpu-accounting.mjs";

function statLine({
  pid,
  comm = "browser",
  state = "S",
  ppid = 1,
  pgrp = pid,
  utime = 0,
  stime = 0,
  cutime = 0,
  cstime = 0,
  starttime = 1000,
}) {
  const fields = [
    state,
    String(ppid),
    String(pgrp),
    "0",
    "0",
    "0",
    "0",
    "0",
    "0",
    "0",
    "0",
    String(utime),
    String(stime),
    String(cutime),
    String(cstime),
    "0",
    "0",
    "0",
    "0",
    String(starttime),
    "0",
    "0",
  ];
  return `${pid} (${comm}) ${fields.join(" ")}\n`;
}

function member({
  pid,
  ppid = 1,
  pgrp = 100,
  ticks,
  cutime = 0,
  cstime = 0,
  starttime = pid * 100,
  affinity = "4",
}) {
  const utimeTicks = BigInt(ticks);
  const stimeTicks = 0n;
  const cutimeTicks = BigInt(cutime);
  const cstimeTicks = BigInt(cstime);
  const selfTicks = utimeTicks + stimeTicks;
  const reapedChildrenTicks = cutimeTicks + cstimeTicks;
  return {
    pid,
    ppid,
    pgrp,
    comm: `p${pid}`,
    state: "S",
    utimeTicks,
    stimeTicks,
    cutimeTicks,
    cstimeTicks,
    selfTicks,
    reapedChildrenTicks,
    totalTicks: selfTicks,
    accountedTicks: selfTicks + reapedChildrenTicks,
    starttimeTicks: BigInt(starttime),
    allowedList: affinity,
    cpus: parseCpuList(affinity),
    threads: [{ tid: pid, allowedList: affinity, cpus: parseCpuList(affinity) }],
  };
}

function snapshot(rootPid, ns, members) {
  return { rootPid, capturedMonotonicNs: BigInt(ns), members };
}

test("parseProcStat preserves spaces and closing parentheses in comm", () => {
  const parsed = parseProcStat(
    statLine({
      pid: 42,
      comm: "Browser Worker) pool",
      ppid: 7,
      utime: 120,
      stime: 30,
      cutime: 40,
      cstime: 10,
      starttime: 9999,
    }),
  );
  assert.equal(parsed.pid, 42);
  assert.equal(parsed.comm, "Browser Worker) pool");
  assert.equal(parsed.ppid, 7);
  assert.equal(parsed.pgrp, 42);
  assert.equal(parsed.utimeTicks, 120n);
  assert.equal(parsed.stimeTicks, 30n);
  assert.equal(parsed.cutimeTicks, 40n);
  assert.equal(parsed.cstimeTicks, 10n);
  assert.equal(parsed.selfTicks, 150n);
  assert.equal(parsed.reapedChildrenTicks, 50n);
  assert.equal(parsed.totalTicks, 150n);
  assert.equal(parsed.accountedTicks, 200n);
  assert.equal(parsed.starttimeTicks, 9999n);
});

test("parseProcStat rejects truncated input", () => {
  assert.throws(
    () => parseProcStat("12 (browser) S 1 2 3"),
    (error) =>
      error instanceof ProcAccountingError &&
      error.code === "INVALID_PROC_STAT",
  );
});

test("parseProcStat accepts Linux's lowercase tracing-stop state", () => {
  const parsed = parseProcStat(statLine({ pid: 43, state: "t" }));
  assert.equal(parsed.state, "t");
});

test("CPU-list and status parsers expand disjoint ranges", () => {
  assert.deepEqual(parseCpuList("0-2,5,7-8"), [0, 1, 2, 5, 7, 8]);
  assert.deepEqual(
    parseProcStatusAffinity(
      "Name:\tbrowser\nCpus_allowed:\t00000100\nCpus_allowed_list:\t8\n",
    ),
    { allowedList: "8", cpus: [8] },
  );
  assert.throws(() => parseCpuList("3-1"), /invalid CPU affinity range/);
});

test("descendant tree includes only the BrowserServer process family", () => {
  const root = member({ pid: 100, ppid: 50, ticks: 1 });
  const renderer = member({ pid: 101, ppid: 100, ticks: 2 });
  const worker = member({ pid: 102, ppid: 101, ticks: 3 });
  const harness = member({ pid: 50, ppid: 1, ticks: 999 });
  const server = member({ pid: 60, ppid: 50, ticks: 999 });
  const members = buildDescendantTree(
    [harness, server, root, renderer, worker],
    100,
  );
  assert.deepEqual(
    members.map((process) => process.pid),
    [100, 101, 102],
  );
});

test("dedicated process-group verification rejects a foreign group member", () => {
  const dedicated = snapshot(100, 0, [
    member({ pid: 100, ticks: 1, pgrp: 100 }),
    member({ pid: 101, ppid: 100, ticks: 1, pgrp: 100 }),
  ]);
  assert.doesNotThrow(() => verifyDedicatedProcessGroup(dedicated, 100));
  const mixed = snapshot(100, 0, [
    member({ pid: 100, ticks: 1, pgrp: 100 }),
    member({ pid: 101, ppid: 100, ticks: 1, pgrp: 999 }),
  ]);
  assert.throws(
    () => verifyDedicatedProcessGroup(mixed, 100),
    (error) =>
      error instanceof ProcAccountingError &&
      error.code === "PROCESS_GROUP_MISMATCH",
  );
});

test("dedicated process-group selection rejects a live escaped descendant", () => {
  const root = member({ pid: 100, ppid: 50, ticks: 1, pgrp: 100 });
  const child = member({ pid: 101, ppid: 100, ticks: 1, pgrp: 101 });
  assert.throws(
    () => selectDedicatedProcessGroup([root, child], 100),
    (error) =>
      error instanceof ProcAccountingError &&
      error.code === "PROCESS_GROUP_ESCAPE" &&
      error.details.escapedIdentities[0] === processIdentity(child),
  );
});

test("stable accounting reports raw and incremental one-core CPU percent", () => {
  const start = snapshot(100, 0, [
    member({ pid: 100, ticks: 100 }),
    member({ pid: 101, ppid: 100, ticks: 50 }),
  ]);
  const end = snapshot(100, 15_000_000_000, [
    member({ pid: 100, ticks: 120 }),
    member({ pid: 101, ppid: 100, ticks: 60 }),
  ]);
  const result = accountStableInterval({
    start,
    end,
    clockTicks: 100,
    baselinePercent: 0.5,
  });
  assert.equal(result.cpuTicks, 30n);
  assert.equal(result.cpuSeconds, 0.3);
  assert.equal(result.wallSeconds, 15);
  assert.ok(Math.abs(result.rawPercent - 2.0) < 1e-12);
  assert.ok(Math.abs(result.incrementalPercent - 1.5) < 1e-12);
  assert.equal(passesIncrementalThreshold(2.0), true);
  assert.equal(passesIncrementalThreshold(2.000_001), false);
  assert.equal(passesIncrementalThreshold(-0.000_001), true);
  assert.equal(passesIncrementalThreshold(-500), true);
});

test("reaped-child ticks are counted without double-counting stable live descendants", () => {
  const start = snapshot(100, 0, [
    member({ pid: 100, ticks: 100, cutime: 40 }),
    member({ pid: 101, ppid: 100, ticks: 50, cutime: 5 }),
  ]);
  const end = snapshot(100, 1_000_000_000, [
    member({ pid: 100, ticks: 110, cutime: 40 }),
    member({ pid: 101, ppid: 100, ticks: 60, cutime: 30 }),
  ]);
  const result = accountStableInterval({ start, end, clockTicks: 100 });
  assert.equal(result.cpuTicks, 45n);
  assert.deepEqual(
    result.memberDeltas.map(({ selfDeltaTicks, reapedChildrenDeltaTicks }) => ({
      selfDeltaTicks,
      reapedChildrenDeltaTicks,
    })),
    [
      { selfDeltaTicks: 10n, reapedChildrenDeltaTicks: 0n },
      { selfDeltaTicks: 10n, reapedChildrenDeltaTicks: 25n },
    ],
  );
});

test("reaped-child ticks account for a descendant absent from both endpoints", () => {
  const start = snapshot(100, 0, [
    member({ pid: 100, ticks: 100, cutime: 40 }),
  ]);
  const end = snapshot(100, 1_000_000_000, [
    member({ pid: 100, ticks: 105, cutime: 65 }),
  ]);
  const result = accountStableInterval({ start, end, clockTicks: 100 });
  assert.equal(result.cpuTicks, 30n);
  assert.equal(result.memberDeltas[0].selfDeltaTicks, 5n);
  assert.equal(result.memberDeltas[0].reapedChildrenDeltaTicks, 25n);
});

test("baseline selection rejects hot blanks and chooses the lower valid sample", () => {
  assert.equal(selectConservativeBaselineIndex([1.8, 0.25], 2.0), 1);
  assert.equal(selectConservativeBaselineIndex([0.25, 1.8], 2.0), 0);
  assert.equal(selectConservativeBaselineIndex([0.25, 0.25], 2.0), 0);
  assert.throws(
    () => selectConservativeBaselineIndex([0.25], 2.0),
    (error) =>
      error instanceof ProcAccountingError && error.code === "INVALID_BASELINE",
  );
  assert.throws(
    () => selectConservativeBaselineIndex([0.25, 0.5]),
    (error) =>
      error instanceof ProcAccountingError && error.code === "INVALID_BASELINE",
  );
  assert.throws(
    () => selectConservativeBaselineIndex([0.25, 8.0], 2.0),
    (error) =>
      error instanceof ProcAccountingError &&
      error.code === "INVALID_BASELINE" &&
      error.details.ceilingViolations[0].index === 1,
  );
  assert.throws(
    () => selectConservativeBaselineIndex([0.25, 2.0], 2.0),
    (error) =>
      error instanceof ProcAccountingError && error.code === "INVALID_BASELINE",
  );
  assert.throws(
    () => selectConservativeBaselineIndex([0.25, Number.NaN], 2.0),
    (error) =>
      error instanceof ProcAccountingError && error.code === "INVALID_BASELINE",
  );
});

test(
  "Linux /proc accounts a spawned, CPU-burning, waited child after it leaves the tree",
  { skip: process.platform !== "linux", timeout: 15_000 },
  () => {
    const start = captureProcessTree(process.pid);
    const child = spawnSync(
      process.execPath,
      [
        "-e",
        "const start = process.cpuUsage(); while (true) { const used = process.cpuUsage(start); if (used.user + used.system >= 200000) break; }",
      ],
      { stdio: "ignore", timeout: 10_000 },
    );
    assert.equal(child.error, undefined);
    assert.equal(child.signal, null);
    assert.equal(child.status, 0);
    const end = captureProcessTree(process.pid);
    assert.doesNotThrow(() => assertStableTree(start, end));

    const beforeRoot = start.members.find((entry) => entry.pid === process.pid);
    const afterRoot = end.members.find((entry) => entry.pid === process.pid);
    const reapedDelta =
      afterRoot.reapedChildrenTicks - beforeRoot.reapedChildrenTicks;
    assert.ok(reapedDelta > 0n, "waited child must advance cutime/cstime");

    const result = accountStableInterval({ start, end, clockTicks: 100 });
    const rootDelta = result.memberDeltas.find((entry) => entry.pid === process.pid);
    assert.equal(rootDelta.reapedChildrenDeltaTicks, reapedDelta);
    assert.ok(result.cpuTicks >= reapedDelta);
  },
);

test(
  "Linux process-group cleanup kills a helper forked after SIGTERM and leader exit",
  { skip: process.platform !== "linux", timeout: 15_000 },
  async () => {
    const root = spawn(
      process.execPath,
      [
        "-e",
        `
          const { spawn } = require("node:child_process");
          let handled = false;
          process.on("SIGTERM", () => {
            if (handled) return;
            handled = true;
            const helper = spawn(process.execPath, ["-e", "setInterval(() => {}, 1000)"], {
              detached: false,
              stdio: "ignore",
            });
            process.stdout.write("helper:" + helper.pid + "\\n");
            setTimeout(() => process.exit(0), 25);
          });
          process.stdout.write("ready:" + process.pid + "\\n");
          setInterval(() => {}, 1000);
        `,
      ],
      { detached: true, stdio: ["ignore", "pipe", "ignore"] },
    );
    let output = "";
    root.stdout.on("data", (chunk) => {
      output += chunk.toString("utf8");
    });
    try {
      await new Promise((resolvePromise, rejectPromise) => {
        const timer = setTimeout(
          () => rejectPromise(new Error("detached cleanup fixture did not start")),
          5_000,
        );
        const inspect = () => {
          if (!output.includes(`ready:${root.pid}\n`)) return;
          clearTimeout(timer);
          root.stdout.removeListener("data", inspect);
          resolvePromise();
        };
        root.stdout.on("data", inspect);
        inspect();
      });
      const before = captureProcessGroup(root.pid);
      verifyDedicatedProcessGroup(before, root.pid);
      const result = await terminateLinuxProcessGroup(root.pid, "cleanup fixture", {
        termGraceMs: 250,
        killGraceMs: 250,
      });
      const helperMatch = output.match(/helper:(\d+)/);
      assert.notEqual(helperMatch, null, "SIGTERM handler must publish its helper PID");
      const helperPid = Number(helperMatch[1]);
      assert.ok(
        result.after_term_identities.some((identity) =>
          identity.startsWith(`${helperPid}:`)),
        "post-TERM scan must discover the late helper",
      );
      assert.equal(result.sigkill_sent, true);
      assert.deepEqual(result.remaining_identities, []);
      assert.equal(result.pass, true);
    } finally {
      try {
        process.kill(-root.pid, "SIGKILL");
      } catch (error) {
        if (error?.code !== "ESRCH") throw error;
      }
    }
  },
);

test("PID reuse and process-tree churn fail a measurement", () => {
  const before = snapshot(100, 0, [member({ pid: 100, ticks: 5 })]);
  const reused = snapshot(100, 1_000_000_000, [
    member({ pid: 100, ticks: 1, starttime: 9999 }),
  ]);
  assert.throws(
    () => assertStableTree(before, reused),
    (error) =>
      error instanceof ProcAccountingError &&
      error.code === "PROCESS_TREE_CHURN" &&
      error.details.added.length === 1 &&
      error.details.removed.length === 1,
  );
});

test("negative per-process CPU deltas fail closed", () => {
  const before = snapshot(100, 0, [member({ pid: 100, ticks: 10 })]);
  const after = snapshot(100, 1_000_000_000, [member({ pid: 100, ticks: 9 })]);
  assert.throws(
    () =>
      accountStableInterval({ start: before, end: after, clockTicks: 100 }),
    (error) =>
      error instanceof ProcAccountingError &&
      error.code === "NEGATIVE_CPU_DELTA",
  );

  const reapedBefore = snapshot(100, 0, [
    member({ pid: 100, ticks: 10, cutime: 5 }),
  ]);
  const reapedAfter = snapshot(100, 1_000_000_000, [
    member({ pid: 100, ticks: 10, cutime: 4 }),
  ]);
  assert.throws(
    () => accountStableInterval({
      start: reapedBefore,
      end: reapedAfter,
      clockTicks: 100,
    }),
    (error) =>
      error instanceof ProcAccountingError &&
      error.code === "NEGATIVE_CPU_DELTA",
  );
});

test("affinity verification checks every process and thread", () => {
  const pinned = snapshot(100, 0, [member({ pid: 100, ticks: 1 })]);
  assert.doesNotThrow(() => verifyPinnedAffinity(pinned, 4));
  const broad = snapshot(100, 0, [
    member({ pid: 100, ticks: 1, affinity: "4-5" }),
  ]);
  assert.throws(
    () => verifyPinnedAffinity(broad, 4),
    (error) =>
      error instanceof ProcAccountingError && error.code === "AFFINITY_MISMATCH",
  );
});

test("serialized snapshots preserve process identities and exact tick strings", () => {
  const sample = snapshot(100, 123, [member({ pid: 100, ticks: 55 })]);
  const serialized = serializeSnapshot(sample);
  assert.equal(serialized.captured_monotonic_ns, "123");
  assert.equal(serialized.members[0].process_group_id, 100);
  assert.equal(serialized.members[0].total_ticks, "55");
  assert.equal(serialized.members[0].accounted_ticks, "55");
  assert.equal(processIdentity(sample.members[0]), "100:10000");
});
