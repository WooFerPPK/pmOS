#!/usr/bin/env node

import { execFile, spawn } from "node:child_process";
import {
  accessSync,
  constants as fsConstants,
  readFileSync,
} from "node:fs";
import { createServer as createNetServer } from "node:net";
import { cpus as hostCpus, release as hostRelease } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import { chromium, firefox } from "@playwright/test";

import {
  accountStableInterval,
  assertStableTree,
  captureProcessGroup,
  parseCpuList,
  parseProcStat,
  parseProcStatusAffinity,
  passesIncrementalThreshold,
  selectConservativeBaselineIndex,
  serializeSnapshot,
  terminateLinuxProcessGroup,
  verifyDedicatedProcessGroup,
  verifyPinnedAffinity,
} from "./idle-cpu-accounting.mjs";

const execFileAsync = promisify(execFile);
const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const webRoot = resolve(scriptDirectory, "..");
const repositoryRoot = resolve(webRoot, "..");
const distIndex = resolve(repositoryRoot, "dist", "index.html");

const INTERVAL_MS = 15_000;
const SETTLE_MS = 5_000;
const PRESAMPLE_GAP_MS = 500;
const START_GAP_MS = 250;
const INCREMENTAL_LIMIT_PERCENT = 2.0;
const BLANK_BASELINE_LIMIT_PERCENT = 2.0;
const BLANK_SAMPLES_PER_RUN = 2;
const RUNS_PER_ENGINE = 2;
const GLOBAL_TIMEOUT_MS = 12 * 60_000;
const RUN_TIMEOUT_MS = 150_000;
const MEASUREMENT_TIMEOUT_MS = 30_000;
const SERVER_START_TIMEOUT_MS = 30_000;
const BOOT_TIMEOUT_MS = 20_000;
const STATE_TIMEOUT_MS = 12_000;
const TOOL_TIMEOUT_MS = 3_000;
const TERM_GRACE_MS = 3_000;
const KILL_GRACE_MS = 1_000;
const RESERVED_INTEGRATION_PORT = 8081;
const FRAMEBUFFER_WIDTH = 1024;
const FRAMEBUFFER_HEIGHT = 768;
const TASKBAR_ENTRY_SAMPLE_Y = 740;
const TASKBAR_LEFT_MARGIN = 4;
const TASKBAR_LAUNCHER_RESERVED_WIDTH = 86;
const TASKBAR_CLOCK_RESERVED_WIDTH = 68;
const TASKBAR_RIGHT_MARGIN = 4;
const TASKBAR_ENTRY_GAP = 2;
const TASKBAR_ENTRY_WIDTH = 160;
const TASKBAR_ENTRY_HEIGHT = 28;
const TASKBAR_MIN_ENTRY_WIDTH = 112;
const TASKBAR_PALETTES = {
  focused: [
    [0xe9, 0xe9, 0xe9, 0xff],
    [0x3a, 0x3a, 0x3a, 0xff],
  ],
  unfocused: [
    [0xf9, 0xf9, 0xf9, 0xff],
    [0x20, 0x20, 0x20, 0xff],
  ],
};
const LAUNCHER_MARKER = {
  x: 4,
  width: 200,
  bottom: 736,
  palettes: [
    { background: [0xf3, 0xf3, 0xf3], border: [0xd0, 0xd0, 0xd0] },
    { background: [0x20, 0x20, 0x20], border: [0x50, 0x50, 0x50] },
  ],
};
const LAUNCHER_REGION = { x: 4, y: 608, width: 200, height: 128 };
const BAD_CONSOLE_MARKERS = [
  "real kernel panic",
  "user worker crashed pid=",
  "using built-in fallback",
  "[pageerror]",
];
const APPS = [
  {
    name: "Terminal 1",
    exec: "/bin/term",
    launcherY: 624,
    started: (line) => line.includes("term: starting"),
  },
  {
    name: "Files",
    exec: "/bin/files",
    launcherY: 648,
    started: (line) => line.includes("files: starting"),
  },
  {
    name: "Edit",
    exec: "/bin/edit",
    launcherY: 672,
    started: (line) => line.includes("edit: starting"),
  },
  {
    name: "Settings",
    exec: "/bin/settings",
    launcherY: 696,
    started: (line) => line.includes("shell: launched /bin/settings pid="),
  },
  {
    name: "System Monitor",
    exec: "/bin/sysmon",
    launcherY: 720,
    started: (line) => line.includes("sysmon: starting"),
  },
  {
    name: "Terminal 2",
    exec: "/bin/term",
    launcherY: 624,
    started: (line) => line.includes("term: starting"),
  },
];

const SIX_APP_DURABLE_PATTERN =
  /^(?:\[real-kernel\] )?shell: session durable revision=([0-9]+) apps=6 windows=6 bytes=([0-9]+) digest=([0-9a-f]{16})$/;
const SIX_APP_RESTORE_COMPLETED =
  /^(?:\[real-kernel\] )?shell: session restored status=completed apps=6 windows=6$/;
const DESKTOP_READY = /^(?:\[real-kernel\] )?shell: desktop ready$/;

export class IdleCpuGateError extends Error {
  constructor(code, message, details = undefined) {
    super(message);
    this.name = "IdleCpuGateError";
    this.code = code;
    this.details = details;
  }
}

function fail(code, message, details = undefined) {
  throw new IdleCpuGateError(code, message, details);
}

function requireCondition(condition, code, message, details = undefined) {
  if (!condition) fail(code, message, details);
}

function consoleRecords(lines) {
  return lines.flatMap((line) => line.split("\n"));
}

export function parseSixAppDurableRecord(record) {
  const match = record.match(SIX_APP_DURABLE_PATTERN);
  if (match === null) return null;
  const revision = Number(match[1]);
  const bytes = Number(match[2]);
  if (
    !Number.isSafeInteger(revision) ||
    revision <= 0 ||
    !Number.isSafeInteger(bytes) ||
    bytes <= 0
  ) {
    return null;
  }
  return { revision, bytes, digest: match[3] };
}

export function latestSixAppDurableEvidence(lines, afterRecordIndex = -1) {
  if (!Number.isSafeInteger(afterRecordIndex) || afterRecordIndex < -1) {
    return null;
  }
  return consoleRecords(lines)
    .flatMap((record, recordIndex) => {
      const evidence = parseSixAppDurableRecord(record);
      return evidence !== null && recordIndex > afterRecordIndex
        ? [{ ...evidence, record_index: recordIndex }]
        : [];
    })
    .at(-1) ?? null;
}

export function restoredSessionLifecycleEvidence(lines) {
  const records = consoleRecords(lines);
  const restoredIndex = records.findIndex((record) =>
    SIX_APP_RESTORE_COMPLETED.test(record)
  );
  if (restoredIndex < 0) return null;
  const readyOffset = records
    .slice(restoredIndex + 1)
    .findIndex((record) => DESKTOP_READY.test(record));
  if (readyOffset < 0) return null;
  const readyIndex = restoredIndex + readyOffset + 1;
  return {
    restored_record: records[restoredIndex],
    ready_record: records[readyIndex],
    restored_index: restoredIndex,
    ready_index: readyIndex,
  };
}

function jsonSafe(value) {
  if (typeof value === "bigint") return value.toString();
  if (Array.isArray(value)) return value.map(jsonSafe);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [key, jsonSafe(item)]),
    );
  }
  return value;
}

function serializeError(error) {
  if (error instanceof Error) {
    return jsonSafe({
      name: error.name,
      message: error.message,
      code: error.code,
      details: error.details,
      stack: error.stack,
      cause: error.cause instanceof Error ? serializeError(error.cause) : error.cause,
    });
  }
  return { name: "NonError", message: String(error) };
}

function emit(type, fields = {}) {
  process.stdout.write(
    `${JSON.stringify(jsonSafe({ type, recorded_at: new Date().toISOString(), ...fields }))}\n`,
  );
}

function createDeadline(parentSignal, timeoutMs, label) {
  const controller = new AbortController();
  const deadline = performance.now() + timeoutMs;
  const abortFromParent = () => controller.abort(parentSignal.reason);
  if (parentSignal.aborted) abortFromParent();
  else parentSignal.addEventListener("abort", abortFromParent, { once: true });
  const timer = setTimeout(() => {
    controller.abort(
      new IdleCpuGateError(
        "PHASE_TIMEOUT",
        `${label} exceeded ${timeoutMs} ms`,
        { label, timeout_ms: timeoutMs },
      ),
    );
  }, timeoutMs);
  return {
    signal: controller.signal,
    deadline,
    dispose() {
      clearTimeout(timer);
      parentSignal.removeEventListener("abort", abortFromParent);
    },
  };
}

function throwIfAborted(context) {
  if (context.signal.aborted) {
    throw context.signal.reason instanceof Error
      ? context.signal.reason
      : new IdleCpuGateError("ABORTED", "idle-CPU gate was aborted");
  }
  if (performance.now() >= context.deadline) {
    fail("PHASE_TIMEOUT", "idle-CPU gate phase deadline expired");
  }
}

function remainingTimeout(context, maximum) {
  throwIfAborted(context);
  return Math.max(1, Math.min(maximum, context.deadline - performance.now()));
}

/** Race any external operation against both the AbortSignal and hard deadline. */
export function awaitWithinContext(operation, context, label) {
  throwIfAborted(context);
  const timeoutMs = context.deadline - performance.now();
  if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) {
    fail("PHASE_TIMEOUT", `${label} reached its deadline before starting`, {
      label,
      timeout_ms: timeoutMs,
    });
  }
  return new Promise((resolvePromise, rejectPromise) => {
    let timer;
    let settled = false;
    const finish = (callback, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      context.signal.removeEventListener("abort", onAbort);
      callback(value);
    };
    const onAbort = () => {
      finish(
        rejectPromise,
        context.signal.reason instanceof Error
          ? context.signal.reason
          : new IdleCpuGateError("ABORTED", `${label} was aborted`),
      );
    };
    timer = setTimeout(() => {
      finish(
        rejectPromise,
        new IdleCpuGateError(
          "PHASE_TIMEOUT",
          `${label} exceeded its hard deadline`,
          { label, timeout_ms: timeoutMs },
        ),
      );
    }, Math.max(1, Math.ceil(timeoutMs)));
    context.signal.addEventListener("abort", onAbort, { once: true });
    Promise.resolve()
      .then(() => {
        throwIfAborted(context);
        return operation();
      })
      .then(
        (value) => finish(resolvePromise, value),
        (error) => finish(rejectPromise, error),
      );
    if (context.signal.aborted) onAbort();
  });
}

async function delay(ms, context) {
  throwIfAborted(context);
  if (performance.now() + ms > context.deadline) {
    fail("PHASE_TIMEOUT", `insufficient phase budget for ${ms} ms delay`);
  }
  await new Promise((resolvePromise, rejectPromise) => {
    let timer;
    const finish = (callback, value) => {
      clearTimeout(timer);
      context.signal.removeEventListener("abort", onAbort);
      callback(value);
    };
    const onAbort = () => {
      finish(
        rejectPromise,
        context.signal.reason instanceof Error
          ? context.signal.reason
          : new IdleCpuGateError("ABORTED", "idle-CPU gate was aborted"),
      );
    };
    timer = setTimeout(() => finish(resolvePromise), ms);
    context.signal.addEventListener("abort", onAbort, { once: true });
    if (context.signal.aborted) onAbort();
  });
  throwIfAborted(context);
}

async function pollUntil(predicate, { context, timeoutMs, intervalMs = 50, label }) {
  const deadline = Math.min(context.deadline, performance.now() + timeoutMs);
  let lastValue;
  while (performance.now() < deadline) {
    throwIfAborted(context);
    lastValue = await awaitWithinContext(
      predicate,
      { ...context, deadline },
      `${label} predicate`,
    );
    if (lastValue) return lastValue;
    await delay(Math.min(intervalMs, Math.max(1, deadline - performance.now())), {
      ...context,
      deadline,
    });
  }
  fail("STATE_TIMEOUT", `${label} did not become true within ${timeoutMs} ms`, {
    label,
    last_value: lastValue,
  });
}

function appendTail(current, chunk, maximum = 65_536) {
  const joined = current + chunk.toString("utf8");
  return joined.length > maximum ? joined.slice(-maximum) : joined;
}

function assertExecutable(path) {
  accessSync(path, fsConstants.X_OK);
}

function matchesPalette(actual, palettes) {
  return palettes.some((expected) =>
    expected.every((channel, index) => actual[index] === channel),
  );
}

async function reserveUnusedPort() {
  for (;;) {
    const port = await new Promise((resolvePromise, rejectPromise) => {
      const listener = createNetServer();
      listener.once("error", rejectPromise);
      listener.listen({ host: "127.0.0.1", port: 0, exclusive: true }, () => {
        const address = listener.address();
        const selected = typeof address === "object" && address !== null
          ? address.port
          : 0;
        listener.close((error) => {
          if (error) rejectPromise(error);
          else resolvePromise(selected);
        });
      });
    });
    if (port > 0 && port !== RESERVED_INTEGRATION_PORT) return port;
  }
}

async function startStaticServer(context) {
  accessSync(distIndex, fsConstants.R_OK);
  const port = await awaitWithinContext(
    reserveUnusedPort,
    context,
    "unused-port reservation",
  );
  const cargo = process.env.CARGO || "cargo";
  const args = [
    "run",
    "--locked",
    "--quiet",
    "-p",
    "xtask",
    "--",
    "dev-server",
    "--dir=dist",
    `--port=${port}`,
  ];
  const child = spawn(cargo, args, {
    cwd: repositoryRoot,
    detached: true,
    shell: false,
    stdio: ["ignore", "pipe", "pipe"],
  });
  requireCondition(
    Number.isSafeInteger(child.pid) && child.pid > 1,
    "SERVER_PID",
    "static server spawn did not expose a safe PID",
  );
  let processGroupId;
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => {
    stdout = appendTail(stdout, chunk);
  });
  child.stderr.on("data", (chunk) => {
    stderr = appendTail(stderr, chunk);
  });
  child.once("error", (error) => {
    stderr = appendTail(stderr, `\nspawn error: ${error.message}`);
  });
  const baseURL = `http://127.0.0.1:${port}`;
  try {
    processGroupId = verifyProcessGroupLeader(child.pid, "static server");
    await pollUntil(
      async () => {
        if (child.exitCode !== null || child.signalCode !== null) {
          fail("SERVER_EXITED", "static server exited before readiness", {
            exit_code: child.exitCode,
            signal: child.signalCode,
            stdout,
            stderr,
          });
        }
        try {
          const response = await fetch(`${baseURL}/index.html`, {
            signal: AbortSignal.timeout(1_000),
          });
          return response.status === 200 &&
            response.headers.get("cross-origin-opener-policy") === "same-origin" &&
            response.headers.get("cross-origin-embedder-policy") === "require-corp";
        } catch {
          return false;
        }
      },
      {
        context,
        timeoutMs: SERVER_START_TIMEOUT_MS,
        intervalMs: 100,
        label: "isolated static server readiness",
      },
    );
    emit("server_ready", {
      pid: child.pid,
      process_group_id: processGroupId,
      port,
      base_url: baseURL,
      command: { executable: cargo, args },
    });
    return {
      child,
      processGroupId,
      port,
      baseURL,
      logs: () => ({ stdout, stderr }),
    };
  } catch (error) {
    if (Number.isSafeInteger(processGroupId) && processGroupId > 1) {
      try {
        await terminateOwnedProcessGroup(
          processGroupId,
          "failed static server start",
        );
      } catch (cleanupError) {
        emit("cleanup_error", {
          label: "failed static server start",
          error: serializeError(cleanupError),
          logs: { stdout, stderr },
        });
      }
    } else if (Number.isSafeInteger(child.pid) && child.pid > 1) {
      try {
        child.kill("SIGKILL");
      } catch (cleanupError) {
        emit("cleanup_error", {
          label: "failed static server group verification",
          error: serializeError(cleanupError),
          logs: { stdout, stderr },
        });
      }
    }
    throw error;
  }
}

function verifyProcessGroupLeader(rootPid, label) {
  const root = parseProcStat(readFileSync(`/proc/${rootPid}/stat`, "utf8"));
  const harness = parseProcStat(readFileSync("/proc/self/stat", "utf8"));
  requireCondition(
    root.pid === rootPid && root.pgrp === rootPid,
    "PROCESS_GROUP_LEADER",
    `${label} PID ${rootPid} is not a dedicated process-group leader`,
    { root_pid: rootPid, observed_process_group_id: root.pgrp },
  );
  requireCondition(
    harness.pgrp !== rootPid,
    "UNSAFE_PROCESS_GROUP",
    `${label} shares the harness process group`,
    { root_pid: rootPid, harness_pid: harness.pid, process_group_id: root.pgrp },
  );
  return root.pgrp;
}

async function terminateOwnedProcessGroup(processGroupId, label) {
  return terminateLinuxProcessGroup(processGroupId, label, {
    termGraceMs: TERM_GRACE_MS,
    killGraceMs: KILL_GRACE_MS,
    onEvent: (fields) => emit("cleanup", fields),
  });
}

async function pinBrowserProcessGroup(processGroupId, cpu, context) {
  const before = captureProcessGroup(processGroupId);
  verifyDedicatedProcessGroup(before, processGroupId);
  for (const member of before.members) {
    await awaitWithinContext(
      () => execFileAsync(
        "/usr/bin/taskset",
        ["--all-tasks", "--pid", "--cpu-list", String(cpu), String(member.pid)],
        {
          encoding: "utf8",
          timeout: Math.ceil(remainingTimeout(context, TOOL_TIMEOUT_MS)),
          signal: context.signal,
        },
      ),
      context,
      `pin browser PID ${member.pid}`,
    );
  }
  const after = captureProcessGroup(processGroupId);
  verifyDedicatedProcessGroup(after, processGroupId);
  verifyPinnedAffinity(after, cpu);
  return { before, after };
}

function measurementJson(result) {
  return {
    cpu_ticks: result.cpuTicks.toString(),
    cpu_seconds: result.cpuSeconds,
    wall_seconds: result.wallSeconds,
    raw_percent_one_core: result.rawPercent,
    baseline_percent_one_core: result.baselinePercent,
    incremental_percent_one_core: result.incrementalPercent,
    member_deltas: result.memberDeltas.map((member) => ({
      identity: member.identity,
      pid: member.pid,
      self_delta_ticks: member.selfDeltaTicks.toString(),
      reaped_children_delta_ticks: member.reapedChildrenDeltaTicks.toString(),
      delta_ticks: member.deltaTicks.toString(),
    })),
  };
}

async function measurePhase({
  processGroupId,
  cpu,
  clockTicks,
  engine,
  run,
  phase,
  baselinePercent,
  comparison = true,
  context,
}) {
  const phaseDeadline = createDeadline(
    context.signal,
    Math.min(MEASUREMENT_TIMEOUT_MS, context.deadline - performance.now()),
    `${engine} run ${run} ${phase} measurement`,
  );
  context = phaseDeadline;
  try {
    await delay(SETTLE_MS, context);
    const pinned = await pinBrowserProcessGroup(processGroupId, cpu, context);
    await delay(PRESAMPLE_GAP_MS, context);
    const preSampleOne = captureProcessGroup(processGroupId);
    verifyDedicatedProcessGroup(preSampleOne, processGroupId);
    verifyPinnedAffinity(preSampleOne, cpu);
    await delay(PRESAMPLE_GAP_MS, context);
    const preSampleTwo = captureProcessGroup(processGroupId);
    verifyDedicatedProcessGroup(preSampleTwo, processGroupId);
    verifyPinnedAffinity(preSampleTwo, cpu);
    assertStableTree(preSampleOne, preSampleTwo, `${phase} stable pre-samples`);
    await delay(START_GAP_MS, context);
    const start = captureProcessGroup(processGroupId);
    verifyDedicatedProcessGroup(start, processGroupId);
    verifyPinnedAffinity(start, cpu);
    assertStableTree(preSampleTwo, start, `${phase} pre-sample to interval start`);
    emit("measurement_start", {
      engine,
      run,
      phase,
      requested_interval_ms: INTERVAL_MS,
      phase_timeout_ms: MEASUREMENT_TIMEOUT_MS,
      process_group_id: processGroupId,
      pinned_cpu: cpu,
      pin_before: serializeSnapshot(pinned.before),
      pin_after: serializeSnapshot(pinned.after),
      stable_pre_samples: [
        serializeSnapshot(preSampleOne),
        serializeSnapshot(preSampleTwo),
      ],
      start: serializeSnapshot(start),
    });

    // Deliberately no page operation or synthetic input occurs in this interval.
    await delay(INTERVAL_MS, context);
    const end = captureProcessGroup(processGroupId);
    verifyDedicatedProcessGroup(end, processGroupId);
    verifyPinnedAffinity(end, cpu);
    const result = accountStableInterval({
      start,
      end,
      clockTicks,
      baselinePercent,
      label: `${engine} run ${run} ${phase}`,
    });
    const pass = comparison
      ? passesIncrementalThreshold(result.incrementalPercent, INCREMENTAL_LIMIT_PERCENT)
      : Number.isFinite(result.rawPercent) &&
        result.rawPercent >= 0 &&
        result.rawPercent < BLANK_BASELINE_LIMIT_PERCENT;
    emit("measurement", {
      engine,
      run,
      phase,
      requested_interval_ms: INTERVAL_MS,
      threshold_percent_one_core: comparison
        ? INCREMENTAL_LIMIT_PERCENT
        : null,
      blank_environment_ceiling_percent_one_core: comparison
        ? null
        : BLANK_BASELINE_LIMIT_PERCENT,
      comparison,
      ...measurementJson(result),
      end: serializeSnapshot(end),
      pass,
    });
    return { result, pass, comparison };
  } catch (error) {
    emit("measurement_error", {
      engine,
      run,
      phase,
      error: serializeError(error),
      pass: false,
    });
    throw error;
  } finally {
    phaseDeadline.dispose();
  }
}

async function framebufferPixel(page, point, context) {
  return awaitWithinContext(() => page.locator("#pmos-fb").evaluate((canvas, sample) => {
    const context = canvas.getContext("2d");
    if (context === null) throw new Error("framebuffer 2d context missing");
    return Array.from(context.getImageData(sample.x, sample.y, 1, 1).data);
  }, point), context, "framebuffer pixel read");
}

async function framebufferFingerprint(page, region, context) {
  return awaitWithinContext(() => page.locator("#pmos-fb").evaluate((canvas, sample) => {
    const context = canvas.getContext("2d");
    if (context === null) throw new Error("framebuffer 2d context missing");
    const bytes = context.getImageData(
      sample.x,
      sample.y,
      sample.width,
      sample.height,
    ).data;
    let hash = 0x811c9dc5;
    for (const byte of bytes) hash = Math.imul(hash ^ byte, 0x01000193);
    return hash >>> 0;
  }, region), context, "framebuffer fingerprint read");
}

async function framebufferRegionPixels(page, region, context) {
  return awaitWithinContext(() => page.locator("#pmos-fb").evaluate((canvas, sample) => {
    const drawing = canvas.getContext("2d");
    if (drawing === null) throw new Error("framebuffer 2d context missing");
    const bytes = drawing.getImageData(
      sample.x,
      sample.y,
      sample.width,
      sample.height,
    ).data;
    const pixels = [];
    for (let offset = 0; offset < bytes.length; offset += 4) {
      pixels.push(Array.from(bytes.slice(offset, offset + 4)));
    }
    return pixels;
  }, region), context, "framebuffer region pixel read");
}

async function launcherMenuIsOpen(page, context) {
  return awaitWithinContext(() => page.locator("#pmos-fb").evaluate((canvas, marker) => {
    const context = canvas.getContext("2d");
    if (context === null) return false;
    const left = context.getImageData(marker.x, 0, 1, marker.bottom).data;
    const innerLeft = context.getImageData(marker.x + 6, 0, 1, marker.bottom).data;
    const innerRight = context.getImageData(
      marker.x + marker.width - 10,
      0,
      1,
      marker.bottom,
    ).data;
    const rgbAt = (column, y) => {
      const offset = y * 4;
      return Array.from(column.slice(offset, offset + 3));
    };
    const matches = (actual, expected) =>
      actual.every((channel, index) => channel === expected[index]);
    for (let top = marker.bottom - 32; top >= 0; top -= 24) {
      for (const palette of marker.palettes) {
        if (
          matches(rgbAt(left, top), palette.border) &&
          matches(rgbAt(left, marker.bottom - 1), palette.border) &&
          matches(rgbAt(innerLeft, top + 2), palette.background) &&
          matches(rgbAt(innerRight, top + 2), palette.background)
        ) {
          return true;
        }
      }
    }
    return false;
  }, LAUNCHER_MARKER), context, "launcher-state read");
}

async function clickFramebuffer(page, x, y, context) {
  const box = await awaitWithinContext(
    () => page.locator("#pmos-fb").boundingBox(),
    context,
    "framebuffer bounds read",
  );
  requireCondition(box !== null, "FRAMEBUFFER_MISSING", "framebuffer has no layout box");
  await awaitWithinContext(
    () => page.mouse.click(
      box.x + (x / FRAMEBUFFER_WIDTH) * box.width,
      box.y + (y / FRAMEBUFFER_HEIGHT) * box.height,
    ),
    context,
    "framebuffer mouse click",
  );
}

function taskbarEntryWidth(entryCount) {
  const available = FRAMEBUFFER_WIDTH - TASKBAR_LEFT_MARGIN -
    TASKBAR_LAUNCHER_RESERVED_WIDTH - TASKBAR_CLOCK_RESERVED_WIDTH -
    TASKBAR_RIGHT_MARGIN;
  const gaps = TASKBAR_ENTRY_GAP * Math.max(0, entryCount - 1);
  return Math.max(
    TASKBAR_MIN_ENTRY_WIDTH,
    Math.min(TASKBAR_ENTRY_WIDTH, Math.floor((available - gaps) / entryCount)),
  );
}

function taskbarEntryPoint(index, entryCount) {
  const width = taskbarEntryWidth(entryCount);
  return {
    x: TASKBAR_LEFT_MARGIN + TASKBAR_LAUNCHER_RESERVED_WIDTH +
      index * (width + TASKBAR_ENTRY_GAP) + 30,
    y: TASKBAR_ENTRY_SAMPLE_Y,
  };
}

export function taskbarPixelsMatchLayout(pixels, entryCount, focusedIndex) {
  if (entryCount === 0) {
    return pixels.length === TASKBAR_ENTRY_WIDTH * TASKBAR_ENTRY_HEIGHT &&
      TASKBAR_PALETTES.unfocused.some((palette) =>
      pixels.every((pixel) =>
        palette.every((channel, index) => pixel[index] === channel)
      )
    );
  }
  if (pixels.length !== entryCount) return false;
  return pixels.every((pixel, index) =>
    matchesPalette(
      pixel,
      index === focusedIndex
        ? TASKBAR_PALETTES.focused
        : TASKBAR_PALETTES.unfocused,
    )
  );
}

async function assertTaskbarLayout(page, entryCount, focusedIndex, context, label) {
  return pollUntil(
    async () => {
      if (entryCount === 0) {
        const empty = await framebufferRegionPixels(
          page,
          {
            x: 90,
            y: 738,
            width: TASKBAR_ENTRY_WIDTH,
            height: TASKBAR_ENTRY_HEIGHT,
          },
          context,
        );
        return taskbarPixelsMatchLayout(empty, entryCount, focusedIndex);
      }
      const pixels = await Promise.all(
        Array.from({ length: entryCount }, (_, index) =>
          framebufferPixel(page, taskbarEntryPoint(index, entryCount), context)),
      );
      return taskbarPixelsMatchLayout(pixels, entryCount, focusedIndex);
    },
    { context, timeoutMs: STATE_TIMEOUT_MS, label },
  );
}

async function assertRestoredSixAppTaskbar(page, context) {
  return pollUntil(
    async () => {
      const pixels = await Promise.all(
        Array.from({ length: 6 }, (_, index) =>
          framebufferPixel(page, taskbarEntryPoint(index, 6), context)),
      );
      const palettes = pixels.map((pixel) => {
        if (matchesPalette(pixel, TASKBAR_PALETTES.focused)) return "focused";
        if (matchesPalette(pixel, TASKBAR_PALETTES.unfocused)) return "unfocused";
        return "other";
      });
      const focusedIndexes = palettes.flatMap((palette, index) =>
        palette === "focused" ? [index] : []
      );
      return focusedIndexes.length === 1 &&
          palettes.filter((palette) => palette === "unfocused").length === 5
        ? { focused_index: focusedIndexes[0], palettes }
        : false;
    },
    {
      context,
      timeoutMs: STATE_TIMEOUT_MS,
      label: "restored six-app taskbar",
    },
  );
}

async function waitForLine(lines, start, predicate, label, context) {
  return pollUntil(
    () => lines.slice(start).find(predicate) || false,
    { context, timeoutMs: STATE_TIMEOUT_MS, label },
  );
}

async function openLauncher(page, context) {
  requireCondition(
    !(await launcherMenuIsOpen(page, context)),
    "LAUNCHER_STATE",
    "launcher must be closed before an app launch",
  );
  const closedFingerprint = await framebufferFingerprint(page, LAUNCHER_REGION, context);
  const canvas = page.locator("#pmos-fb");
  const sequenceBefore = Number(
    (await awaitWithinContext(
      () => canvas.getAttribute("data-pmos-frame-sequence"),
      context,
      "launcher frame-sequence read",
    )) || "0",
  );
  await clickFramebuffer(page, 40, 752, context);
  await pollUntil(
    async () => {
      const sequence = Number(
        (await canvas.getAttribute("data-pmos-frame-sequence")) || "0",
      );
      return sequence > sequenceBefore && await launcherMenuIsOpen(page, context);
    },
    {
      context,
      timeoutMs: STATE_TIMEOUT_MS,
      label: "causal launcher-open presentation",
    },
  );
  return closedFingerprint;
}

async function launchApp(page, lines, app, index, context) {
  const logStart = lines.length;
  const closedFingerprint = await openLauncher(page, context);
  const canvas = page.locator("#pmos-fb");
  const sequenceBefore = Number(
    (await awaitWithinContext(
      () => canvas.getAttribute("data-pmos-frame-sequence"),
      context,
      `${app.name} frame-sequence read`,
    )) || "0",
  );
  await clickFramebuffer(page, 100, app.launcherY, context);
  await pollUntil(
    async () => {
      const sequence = Number(
        (await canvas.getAttribute("data-pmos-frame-sequence")) || "0",
      );
      return sequence > sequenceBefore && !(await launcherMenuIsOpen(page, context));
    },
    {
      context,
      timeoutMs: STATE_TIMEOUT_MS,
      label: `${app.name} causal launcher-close presentation`,
    },
  );
  const launchLine = await waitForLine(
    lines,
    logStart,
    (line) => line.includes(`shell: launched ${app.exec} pid=`),
    `${app.name} launch PID`,
    context,
  );
  await waitForLine(lines, logStart, app.started, `${app.name} startup`, context);
  await assertTaskbarLayout(
    page,
    index + 1,
    index,
    context,
    `${app.name} mapped taskbar entry`,
  );
  const pidMatch = launchLine.match(/ pid=(\d+)$/);
  requireCondition(pidMatch !== null, "LAUNCH_PID", "launch line has no terminal PID", {
    launch_line: launchLine,
    closed_fingerprint: closedFingerprint,
  });
  const pid = Number(pidMatch[1]);
  requireCondition(Number.isSafeInteger(pid) && pid > 0, "LAUNCH_PID", "invalid app PID", {
    launch_line: launchLine,
  });
  return { ...app, pid, launchLine };
}

async function workerCount(page, context) {
  return Number(
    (await awaitWithinContext(
      () => page.locator("body").getAttribute("data-pmos-live-workers"),
      context,
      "live-worker count read",
    )) || "0",
  );
}

function assertHealthyConsole(lines) {
  const bad = lines.filter((line) =>
    BAD_CONSOLE_MARKERS.some((marker) => line.includes(marker)),
  );
  requireCondition(bad.length === 0, "PMOS_FAILURE_LOG", "PMos emitted a failure marker", {
    lines: bad,
  });
}

async function assertStableWorkerCount(page, expected, context, label) {
  await pollUntil(
    async () => (await workerCount(page, context)) === expected,
    { context, timeoutMs: STATE_TIMEOUT_MS, label },
  );
  await delay(250, context);
  const second = await workerCount(page, context);
  requireCondition(second === expected, "WORKER_COUNT_CHANGED", `${label} was not stable`, {
    expected,
    observed: second,
  });
}

async function assertShellOnly(page, lines, shellWorkers, context) {
  const launches = lines.filter((line) => line.includes("shell: launched /bin/"));
  requireCondition(launches.length === 0, "SHELL_NOT_EMPTY", "shell-only state launched apps", {
    launches,
  });
  await assertStableWorkerCount(page, shellWorkers, context, "shell-only worker count");
  await assertTaskbarLayout(page, 0, -1, context, "shell-only empty taskbar");
  assertHealthyConsole(lines);
  return { worker_count: shellWorkers, launch_count: 0, taskbar_entries: 0 };
}

function appLaunchLines(lines) {
  return lines.filter((line) =>
    /shell: launched \/bin\/(term|files|edit|settings|sysmon) pid=\d+$/.test(line),
  );
}

async function assertSixApps(page, lines, launched, shellWorkers, context) {
  requireCondition(launched.length === APPS.length, "APP_COUNT", "did not launch six apps");
  const pids = launched.map((app) => app.pid);
  requireCondition(new Set(pids).size === APPS.length, "APP_PID_COLLISION", "app PIDs are not unique", {
    pids,
  });
  const launchLines = appLaunchLines(lines);
  requireCondition(launchLines.length === APPS.length, "APP_COUNT", "launch log count is not exactly six", {
    launch_lines: launchLines,
  });
  const loggedLaunches = launchLines.map((line) => {
    const match = line.match(/shell: launched (\/bin\/(?:term|files|edit|settings|sysmon)) pid=(\d+)$/);
    requireCondition(match !== null, "APP_LAUNCH_LOG", "malformed app launch evidence", {
      line,
    });
    return `${match[1]}:${match[2]}`;
  });
  requireCondition(
    new Set(loggedLaunches).size === APPS.length &&
      launched.every((app) => loggedLaunches.includes(`${app.exec}:${app.pid}`)),
    "APP_LAUNCH_LOG",
    "six launch log identities do not exactly match the six mapped apps",
    { logged_launches: loggedLaunches },
  );
  await assertStableWorkerCount(
    page,
    shellWorkers + 8,
    context,
    "six apps plus two Terminal shell children",
  );
  await assertTaskbarLayout(page, 6, 5, context, "six-app taskbar");
  const observations = [];
  for (const app of launched) {
    const row = await waitForLine(
      lines,
      0,
      (line) =>
        line.includes(`sysmon: observed pid=${app.pid} name=${app.exec} `) ||
        line.includes(`sysmon: updated pid=${app.pid} name=${app.exec} `),
      `System Monitor observation for ${app.name} PID ${app.pid}`,
      context,
    );
    const metrics = row.match(/ vm_kib=(\d+) fds=(\d+)$/);
    requireCondition(
      metrics !== null && Number(metrics[1]) > 0 && Number(metrics[2]) > 0,
      "SYSMON_METRICS",
      `System Monitor metrics are incomplete for PID ${app.pid}`,
      { row },
    );
    observations.push({ name: app.name, exec: app.exec, pid: app.pid, row });
    const exited = lines.some((line) =>
      line.includes(`sysmon: process exited pid=${app.pid} name=${app.exec}`),
    );
    requireCondition(!exited, "APP_EXITED", `${app.name} exited before measurement ended`, {
      pid: app.pid,
    });
  }
  assertHealthyConsole(lines);
  return {
    worker_count: shellWorkers + 8,
    taskbar_entries: 6,
    app_pids: pids,
    observations,
  };
}

function countLineOccurrences(lines, needle) {
  return consoleRecords(lines).filter((line) => line.includes(needle)).length;
}

async function assertRestoredSixApps(
  page,
  lines,
  shellWorkers,
  lifecycle,
  durable,
  context,
) {
  const starts = {
    terminal: countLineOccurrences(lines, "term: starting"),
    files: countLineOccurrences(lines, "files: starting"),
    edit: countLineOccurrences(lines, "edit: starting"),
    sysmon: countLineOccurrences(lines, "sysmon: starting"),
  };
  requireCondition(
    starts.terminal === 2 &&
      starts.files === 1 &&
      starts.edit === 1 &&
      starts.sysmon === 1,
    "RESTORED_APP_STARTS",
    "restored process startup multiplicities are not the durable six-app scene",
    { starts },
  );
  const launchLines = consoleRecords(lines).filter((line) =>
    line.includes("shell: launched "),
  );
  requireCondition(
    launchLines.length === 0,
    "RESTORE_REPLAYED_LAUNCH_POLICY",
    "restored apps emitted interactive launcher acknowledgements",
    { launch_lines: launchLines },
  );
  await assertStableWorkerCount(
    page,
    shellWorkers + 8,
    context,
    "restored six apps plus two Terminal shell children",
  );
  const taskbar = await assertRestoredSixAppTaskbar(page, context);
  assertHealthyConsole(lines);
  return {
    worker_count: shellWorkers + 8,
    taskbar_entries: 6,
    focused_index: taskbar.focused_index,
    taskbar_palettes: taskbar.palettes,
    starts,
    durable_revision: durable.revision,
    durable_bytes: durable.bytes,
    durable_digest: durable.digest,
    durable_record_index: durable.record_index,
    restored_record: lifecycle.restored_record,
    ready_record: lifecycle.ready_record,
  };
}

async function executeBrowserRun({
  engineName,
  browserType,
  run,
  baseURL,
  cpu,
  clockTicks,
  globalContext,
}) {
  const runDeadline = createDeadline(
    globalContext.signal,
    Math.min(RUN_TIMEOUT_MS, globalContext.deadline - performance.now()),
    `${engineName} run ${run}`,
  );
  const context = runDeadline;
  let browserServer;
  let browserProcess;
  let browserRootPid;
  let browserProcessGroupId;
  let cleanupPromise;
  const beginBrowserCleanup = () => {
    if (browserProcessGroupId === undefined) return undefined;
    cleanupPromise ??= terminateOwnedProcessGroup(
      browserProcessGroupId,
      `${engineName} run ${run} browser`,
    );
    return cleanupPromise;
  };
  const cleanupOnAbort = () => {
    const pending = beginBrowserCleanup();
    if (pending !== undefined) pending.catch(() => {});
  };
  context.signal.addEventListener("abort", cleanupOnAbort);
  const measurements = [];
  const lines = [];
  let baselineEvidence;
  let outcome;
  try {
    throwIfAborted(context);
    const launchPromise = browserType.launchServer({
        headless: true,
        timeout: remainingTimeout(context, BOOT_TIMEOUT_MS),
    });
    try {
      browserServer = await awaitWithinContext(
        () => launchPromise,
        context,
        `${engineName} BrowserServer launch`,
      );
    } catch (error) {
      launchPromise.then(async (lateServer) => {
        const lateProcess = lateServer.process();
        const latePid = lateProcess?.pid;
        if (!Number.isSafeInteger(latePid) || latePid <= 1) return;
        try {
          const lateGroup = verifyProcessGroupLeader(
            latePid,
            `${engineName} abandoned BrowserServer`,
          );
          await terminateOwnedProcessGroup(
            lateGroup,
            `${engineName} abandoned BrowserServer`,
          );
        } catch (cleanupError) {
          try {
            lateProcess.kill("SIGKILL");
          } catch {
            // The late process may already have exited; the original failure wins.
          }
          emit("cleanup_error", {
            label: `${engineName} abandoned BrowserServer`,
            error: serializeError(cleanupError),
          });
        }
      }).catch(() => {});
      throw error;
    }
    browserProcess = browserServer.process();
    browserRootPid = browserProcess?.pid;
    requireCondition(
      Number.isSafeInteger(browserRootPid) && browserRootPid > 1,
      "BROWSER_PID",
      "BrowserServer.process() did not expose a safe root PID",
    );
    browserProcessGroupId = verifyProcessGroupLeader(
      browserRootPid,
      `${engineName} BrowserServer`,
    );
    if (context.signal.aborted) cleanupOnAbort();
    const browser = await awaitWithinContext(
      () => browserType.connect(browserServer.wsEndpoint(), {
        timeout: remainingTimeout(context, BOOT_TIMEOUT_MS),
      }),
      context,
      `${engineName} BrowserServer connection`,
    );
    const browserVersion = browser.version();
    const browserContext = await awaitWithinContext(
      () => browser.newContext({
        viewport: { width: 1280, height: 900 },
        serviceWorkers: "allow",
      }),
      context,
      `${engineName} browser-context creation`,
    );
    const page = await awaitWithinContext(
      () => browserContext.newPage(),
      context,
      `${engineName} page creation`,
    );
    page.on("console", (message) => lines.push(message.text()));
    page.on("pageerror", (error) => lines.push(`[pageerror] ${error.message}`));
    emit("browser_run_start", {
      engine: engineName,
      run,
      browser_version: browserVersion,
      browser_root_pid: browserRootPid,
      browser_process_group_id: browserProcessGroupId,
      ws_endpoint_protocol: new URL(browserServer.wsEndpoint()).protocol,
      pinned_cpu: cpu,
    });

    await awaitWithinContext(
      () => page.goto("about:blank", {
        waitUntil: "load",
        timeout: remainingTimeout(context, BOOT_TIMEOUT_MS),
      }),
      context,
      `${engineName} about:blank navigation`,
    );
    requireCondition(page.url() === "about:blank", "BLANK_STATE", "blank baseline page changed URL");
    const blankMeasurements = [];
    for (let sample = 1; sample <= BLANK_SAMPLES_PER_RUN; sample += 1) {
      const phase = `blank_${sample}`;
      const blank = await measurePhase({
        processGroupId: browserProcessGroupId,
        cpu,
        clockTicks,
        engine: engineName,
        run,
        phase,
        baselinePercent: 0,
        comparison: false,
        context,
      });
      const measurement = { phase, ...blank };
      blankMeasurements.push(measurement);
      measurements.push(measurement);
      requireCondition(
        page.url() === "about:blank",
        "BLANK_STATE",
        `blank baseline navigated during ${phase}`,
      );
    }
    const rawBlankPercents = blankMeasurements.map(
      (sample) => sample.result.rawPercent,
    );
    const blankSpreadPercent = Math.max(...rawBlankPercents) -
      Math.min(...rawBlankPercents);
    const baselineCandidatesPass = rawBlankPercents.every((value) =>
      Number.isFinite(value) &&
      value >= 0 &&
      value < BLANK_BASELINE_LIMIT_PERCENT
    );
    emit("baseline_candidates", {
      engine: engineName,
      run,
      raw_percent_one_core_pair: rawBlankPercents,
      spread_percent_one_core: blankSpreadPercent,
      environment_ceiling_percent_one_core: BLANK_BASELINE_LIMIT_PERCENT,
      strict_environment_predicate:
        "every blank rawPercent is finite and 0 <= rawPercent < environment ceiling",
      pass: baselineCandidatesPass,
    });
    const selectedBlankIndex = selectConservativeBaselineIndex(
      rawBlankPercents,
      BLANK_BASELINE_LIMIT_PERCENT,
    );
    const selectedBlank = blankMeasurements[selectedBlankIndex];
    baselineEvidence = {
      selected_phase: selectedBlank.phase,
      selected_raw_percent_one_core: selectedBlank.result.rawPercent,
      raw_percent_one_core_pair: rawBlankPercents,
      spread_percent_one_core: blankSpreadPercent,
      environment_ceiling_percent_one_core: BLANK_BASELINE_LIMIT_PERCENT,
      samples: blankMeasurements.map((sample) => ({
        phase: sample.phase,
        ...measurementJson(sample.result),
      })),
    };
    emit("baseline_selection", {
      engine: engineName,
      run,
      selection:
        "lower raw one-core percent after both settled blanks pass the environment ceiling",
      ...baselineEvidence,
      pass: true,
    });

    await awaitWithinContext(
      () => page.goto(`${baseURL}/index.html`, {
        waitUntil: "load",
        timeout: remainingTimeout(context, BOOT_TIMEOUT_MS),
      }),
      context,
      `${engineName} PMos navigation`,
    );
    await pollUntil(
      async () => (await page.locator("#pmos-boot-splash").count()) === 0,
      { context, timeoutMs: BOOT_TIMEOUT_MS, label: "PMos boot splash removal" },
    );
    await waitForLine(
      lines,
      0,
      (line) => line.includes("shell: connected to /run/display"),
      "desktop shell display connection",
      context,
    );
    await waitForLine(
      lines,
      0,
      (line) => line.includes("shell: loaded 5 applications from /usr/share/applications"),
      "five-entry launcher catalog",
      context,
    );
    const shellWorkers = await workerCount(page, context);
    requireCondition(
      Number.isSafeInteger(shellWorkers) && shellWorkers > 0,
      "SHELL_WORKERS",
      "shell-only live-worker count is invalid",
      { observed: shellWorkers },
    );
    const shellBefore = await assertShellOnly(page, lines, shellWorkers, context);
    emit("state", { engine: engineName, run, phase: "shell", moment: "before", ...shellBefore });
    const shell = await measurePhase({
      processGroupId: browserProcessGroupId,
      cpu,
      clockTicks,
      engine: engineName,
      run,
      phase: "shell",
      baselinePercent: selectedBlank.result.rawPercent,
      context,
    });
    measurements.push({ phase: "shell", ...shell });
    const shellAfter = await assertShellOnly(page, lines, shellWorkers, context);
    emit("state", { engine: engineName, run, phase: "shell", moment: "after", ...shellAfter });

    const launched = [];
    for (let index = 0; index < APPS.length; index += 1) {
      launched.push(await launchApp(page, lines, APPS[index], index, context));
    }
    const appsBefore = await assertSixApps(page, lines, launched, shellWorkers, context);
    emit("state", { engine: engineName, run, phase: "six_apps", moment: "before", ...appsBefore });
    const sixApps = await measurePhase({
      processGroupId: browserProcessGroupId,
      cpu,
      clockTicks,
      engine: engineName,
      run,
      phase: "six_apps",
      baselinePercent: selectedBlank.result.rawPercent,
      context,
    });
    measurements.push({ phase: "six_apps", ...sixApps });
    const appsAfter = await assertSixApps(page, lines, launched, shellWorkers, context);
    emit("state", { engine: engineName, run, phase: "six_apps", moment: "after", ...appsAfter });

    const preCloseDurable = await pollUntil(
      () => latestSixAppDurableEvidence(lines) || false,
      {
        context,
        timeoutMs: STATE_TIMEOUT_MS,
        label: "pre-close durable six-app session",
      },
    );
    emit("session_transition", {
      engine: engineName,
      run,
      moment: "pre_close_durable",
      ...preCloseDurable,
    });
    await awaitWithinContext(
      () => page.close(),
      context,
      `${engineName} browser-owned PMos page close`,
    );

    const restoredLines = [];
    const restoredPage = await awaitWithinContext(
      () => browserContext.newPage(),
      context,
      `${engineName} restored page creation`,
    );
    restoredPage.on("console", (message) => restoredLines.push(message.text()));
    restoredPage.on("pageerror", (error) =>
      restoredLines.push(`[pageerror] ${error.message}`));
    await awaitWithinContext(
      () => restoredPage.goto(`${baseURL}/index.html`, {
        waitUntil: "load",
        timeout: remainingTimeout(context, BOOT_TIMEOUT_MS),
      }),
      context,
      `${engineName} restored PMos navigation`,
    );
    const restoredLifecycle = await pollUntil(
      () => restoredSessionLifecycleEvidence(restoredLines) || false,
      {
        context,
        timeoutMs: STATE_TIMEOUT_MS,
        label: "ordered completed six-app restore and desktop readiness",
      },
    );
    await pollUntil(
      async () => (await restoredPage.locator("#pmos-boot-splash").count()) === 0,
      {
        context,
        timeoutMs: BOOT_TIMEOUT_MS,
        label: "restored PMos boot splash removal",
      },
    );
    const postRestoreDurable = await pollUntil(
      () => latestSixAppDurableEvidence(
        restoredLines,
        restoredLifecycle.restored_index,
      ) || false,
      {
        context,
        timeoutMs: STATE_TIMEOUT_MS,
        label: "post-restore durable six-app rewrite",
      },
    );
    emit("session_transition", {
      engine: engineName,
      run,
      moment: "restored_ready_and_durable",
      ...restoredLifecycle,
      durable_revision: postRestoreDurable.revision,
      durable_bytes: postRestoreDurable.bytes,
      durable_digest: postRestoreDurable.digest,
      durable_record_index: postRestoreDurable.record_index,
    });
    const restoredBefore = await assertRestoredSixApps(
      restoredPage,
      restoredLines,
      shellWorkers,
      restoredLifecycle,
      postRestoreDurable,
      context,
    );
    emit("state", {
      engine: engineName,
      run,
      phase: "restored_six_apps",
      moment: "before",
      ...restoredBefore,
    });
    const restoredSixApps = await measurePhase({
      processGroupId: browserProcessGroupId,
      cpu,
      clockTicks,
      engine: engineName,
      run,
      phase: "restored_six_apps",
      baselinePercent: selectedBlank.result.rawPercent,
      context,
    });
    measurements.push({ phase: "restored_six_apps", ...restoredSixApps });
    const restoredAfter = await assertRestoredSixApps(
      restoredPage,
      restoredLines,
      shellWorkers,
      restoredLifecycle,
      postRestoreDurable,
      context,
    );
    emit("state", {
      engine: engineName,
      run,
      phase: "restored_six_apps",
      moment: "after",
      ...restoredAfter,
    });

    const pass = measurements.every((measurement) => measurement.pass);
    emit("browser_run_end", {
      engine: engineName,
      run,
      browser_version: browserVersion,
      browser_root_pid: browserRootPid,
      browser_process_group_id: browserProcessGroupId,
      baseline: baselineEvidence,
      measurements: measurements.map(({
        phase,
        result,
        pass: phasePass,
        comparison,
      }) => ({
        phase,
        comparison,
        ...measurementJson(result),
        pass: phasePass,
      })),
      pass,
    });
    outcome = { pass, measurements, baselineEvidence };
  } catch (error) {
    emit("browser_run_error", {
      engine: engineName,
      run,
      browser_root_pid: browserRootPid,
      browser_process_group_id: browserProcessGroupId,
      error: serializeError(error),
      console_tail: lines.slice(-100),
      pass: false,
    });
    outcome = { pass: false, measurements, baselineEvidence, error };
  } finally {
    context.signal.removeEventListener("abort", cleanupOnAbort);
    if (browserProcessGroupId !== undefined) {
      try {
        await beginBrowserCleanup();
      } catch (error) {
        outcome = outcome || { pass: false, measurements };
        outcome.pass = false;
        outcome.cleanupError = error;
        emit("cleanup_error", {
          label: `${engineName} run ${run} browser`,
          error: serializeError(error),
        });
      }
    } else if (browserProcess !== undefined) {
      try {
        browserProcess.kill("SIGKILL");
      } catch (error) {
        outcome = outcome || { pass: false, measurements };
        outcome.pass = false;
        outcome.cleanupError = error;
        emit("cleanup_error", {
          label: `${engineName} run ${run} unverified browser PID`,
          error: serializeError(error),
        });
      }
    }
    runDeadline.dispose();
  }
  return outcome;
}

async function readHostConfiguration(context) {
  requireCondition(process.platform === "linux", "UNSUPPORTED_HOST", "idle CPU gate requires Linux /proc");
  accessSync("/proc/self/stat", fsConstants.R_OK);
  assertExecutable("/usr/bin/taskset");
  assertExecutable("/usr/bin/getconf");
  const harnessAffinity = parseProcStatusAffinity(
    readFileSync("/proc/self/status", "utf8"),
  );
  requireCondition(harnessAffinity.cpus.length > 0, "INVALID_AFFINITY", "harness has no allowed CPU");
  const { stdout } = await awaitWithinContext(
    () => execFileAsync("/usr/bin/getconf", ["CLK_TCK"], {
      encoding: "utf8",
      timeout: Math.ceil(remainingTimeout(context, TOOL_TIMEOUT_MS)),
      signal: context.signal,
    }),
    context,
    "CLK_TCK lookup",
  );
  const clockTicks = Number(stdout.trim());
  requireCondition(Number.isFinite(clockTicks) && clockTicks > 0, "INVALID_CLOCK_TICKS", "getconf returned invalid CLK_TCK", {
    stdout,
  });
  const packageJson = JSON.parse(
    readFileSync(resolve(webRoot, "node_modules", "@playwright", "test", "package.json"), "utf8"),
  );
  return {
    cpu: harnessAffinity.cpus[0],
    clockTicks,
    metadata: {
      platform: process.platform,
      architecture: process.arch,
      kernel_release: hostRelease(),
      logical_cpu_count: hostCpus().length,
      harness_pid: process.pid,
      harness_affinity: harnessAffinity.allowedList,
      pinned_cpu: harnessAffinity.cpus[0],
      clock_ticks_per_second: clockTicks,
      node_version: process.version,
      playwright_version: packageJson.version,
    },
  };
}

async function main() {
  const globalController = new AbortController();
  const globalTimer = setTimeout(() => {
    globalController.abort(
      new IdleCpuGateError("GLOBAL_TIMEOUT", `gate exceeded ${GLOBAL_TIMEOUT_MS} ms`),
    );
  }, GLOBAL_TIMEOUT_MS);
  const globalContext = {
    signal: globalController.signal,
    deadline: performance.now() + GLOBAL_TIMEOUT_MS,
  };
  let staticServer;
  let passed = true;
  const runs = [];
  emit("gate_start", {
    policy: {
      engines: ["chromium", "firefox"],
      runs_per_engine: RUNS_PER_ENGINE,
      phases: ["blank_1", "blank_2", "shell", "six_apps", "restored_six_apps"],
      blank_samples_per_run: BLANK_SAMPLES_PER_RUN,
      interval_ms: INTERVAL_MS,
      blank_environment_ceiling_percent_one_core:
        BLANK_BASELINE_LIMIT_PERCENT,
      incremental_limit_percent_one_core: INCREMENTAL_LIMIT_PERCENT,
      comparison:
        "phase raw one-core percent minus the lower of two valid same-run settled blank raw percents",
      strict_blank_predicate:
        `every blank rawPercent is finite and 0 <= rawPercent < ${BLANK_BASELINE_LIMIT_PERCENT} before either sample may be used`,
      strict_upper_predicate:
        `incrementalPercent <= ${INCREMENTAL_LIMIT_PERCENT}`,
      negative_incremental_policy:
        "finite negative incremental values pass because PMos may be quieter than about:blank",
      accounting_scope:
        "the dedicated Linux process group led by BrowserServer.process().pid; every live descendant must remain in that group",
      cleanup_policy:
        "SIGTERM the entire group, rescan by pgrp after the leader may exit, then SIGKILL the entire group",
    },
  });
  try {
    const host = await readHostConfiguration(globalContext);
    emit("host", host.metadata);
    staticServer = await startStaticServer(globalContext);
    for (const [engineName, browserType] of [
      ["chromium", chromium],
      ["firefox", firefox],
    ]) {
      for (let run = 1; run <= RUNS_PER_ENGINE; run += 1) {
        throwIfAborted(globalContext);
        const result = await executeBrowserRun({
          engineName,
          browserType,
          run,
          baseURL: staticServer.baseURL,
          cpu: host.cpu,
          clockTicks: host.clockTicks,
          globalContext,
        });
        runs.push({ engine: engineName, run, ...result });
        if (!result.pass) passed = false;
      }
    }
  } catch (error) {
    passed = false;
    emit("gate_error", { error: serializeError(error), pass: false });
  } finally {
    if (staticServer?.processGroupId !== undefined) {
      try {
        await terminateOwnedProcessGroup(
          staticServer.processGroupId,
          "static server",
        );
      } catch (error) {
        passed = false;
        emit("cleanup_error", {
          label: "static server",
          error: serializeError(error),
          logs: staticServer.logs(),
        });
      }
    }
    clearTimeout(globalTimer);
    const baselines = runs.flatMap((runResult) =>
      runResult.measurements.filter((measurement) => !measurement.comparison),
    );
    const comparisons = runs.flatMap((runResult) =>
      runResult.measurements.filter((measurement) => measurement.comparison),
    );
    const phaseSetsComplete = runs.every((runResult) => {
      const baselinePhases = runResult.measurements
        .filter((measurement) => !measurement.comparison)
        .map((measurement) => measurement.phase)
        .sort()
        .join(",");
      const comparisonPhases = runResult.measurements
        .filter((measurement) => measurement.comparison)
        .map((measurement) => measurement.phase)
        .sort()
        .join(",");
      return baselinePhases === "blank_1,blank_2" &&
        comparisonPhases === "restored_six_apps,shell,six_apps";
    });
    const complete = runs.length === 4 &&
      baselines.length === 8 &&
      comparisons.length === 12 &&
      phaseSetsComplete;
    passed = passed && complete && comparisons.every((measurement) => measurement.pass);
    emit("gate_end", {
      run_count: runs.length,
      baseline_count: baselines.length,
      comparison_count: comparisons.length,
      measurement_count: baselines.length + comparisons.length,
      phase_sets_complete: phaseSetsComplete,
      expected_run_count: 4,
      expected_baseline_count: 8,
      expected_comparison_count: 12,
      expected_measurement_count: 20,
      complete,
      pass: passed,
    });
    process.exitCode = passed ? 0 : 1;
  }
}

const invokedScript = process.argv[1] === undefined
  ? ""
  : resolve(process.argv[1]);
if (invokedScript === fileURLToPath(import.meta.url)) {
  await main();
}
