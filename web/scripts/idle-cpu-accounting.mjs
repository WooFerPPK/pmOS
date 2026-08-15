import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

export class ProcAccountingError extends Error {
  constructor(code, message, details = undefined) {
    super(message);
    this.name = "ProcAccountingError";
    this.code = code;
    this.details = details;
  }
}

function parseUnsignedInteger(value, field, source) {
  if (!/^\d+$/.test(value)) {
    throw new ProcAccountingError(
      "INVALID_PROC_STAT",
      `invalid ${field} in /proc stat: ${value}`,
      { source },
    );
  }
  return BigInt(value);
}

function bigintToSafeNumber(value, field, source) {
  const number = Number(value);
  if (!Number.isSafeInteger(number)) {
    throw new ProcAccountingError(
      "INVALID_PROC_STAT",
      `${field} exceeds JavaScript's safe integer range`,
      { source, value: value.toString() },
    );
  }
  return number;
}

/** Parse Linux /proc/<pid>/stat without splitting a parenthesized comm field. */
export function parseProcStat(source) {
  const text = source.trim();
  const open = text.indexOf(" (");
  const close = text.lastIndexOf(") ");
  if (open <= 0 || close <= open + 1) {
    throw new ProcAccountingError(
      "INVALID_PROC_STAT",
      "missing parenthesized comm field in /proc stat",
      { source },
    );
  }

  const pidValue = parseUnsignedInteger(text.slice(0, open), "pid", source);
  const fields = text.slice(close + 2).trim().split(/\s+/);
  if (fields.length < 20 || !/^[RSDZTtXxKWPI]$/.test(fields[0] ?? "")) {
    throw new ProcAccountingError(
      "INVALID_PROC_STAT",
      "truncated or invalid field list in /proc stat",
      { source, fieldCount: fields.length },
    );
  }

  const ppidValue = parseUnsignedInteger(fields[1], "ppid", source);
  const pgrpValue = parseUnsignedInteger(fields[2], "pgrp", source);
  const utimeTicks = parseUnsignedInteger(fields[11], "utime", source);
  const stimeTicks = parseUnsignedInteger(fields[12], "stime", source);
  const cutimeTicks = parseUnsignedInteger(fields[13], "cutime", source);
  const cstimeTicks = parseUnsignedInteger(fields[14], "cstime", source);
  const starttimeTicks = parseUnsignedInteger(fields[19], "starttime", source);
  const selfTicks = utimeTicks + stimeTicks;
  const reapedChildrenTicks = cutimeTicks + cstimeTicks;
  return {
    pid: bigintToSafeNumber(pidValue, "pid", source),
    comm: text.slice(open + 2, close),
    state: fields[0],
    ppid: bigintToSafeNumber(ppidValue, "ppid", source),
    pgrp: bigintToSafeNumber(pgrpValue, "pgrp", source),
    utimeTicks,
    stimeTicks,
    cutimeTicks,
    cstimeTicks,
    starttimeTicks,
    selfTicks,
    reapedChildrenTicks,
    totalTicks: selfTicks,
    accountedTicks: selfTicks + reapedChildrenTicks,
  };
}

/** Expand a Linux Cpus_allowed_list such as "0-3,8,10-11". */
export function parseCpuList(source) {
  const text = source.trim();
  if (text.length === 0) {
    throw new ProcAccountingError("INVALID_AFFINITY", "empty CPU affinity list");
  }
  const cpus = new Set();
  for (const component of text.split(",")) {
    const match = component.match(/^(\d+)(?:-(\d+))?$/);
    if (match === null) {
      throw new ProcAccountingError(
        "INVALID_AFFINITY",
        `invalid CPU affinity component: ${component}`,
        { source },
      );
    }
    const first = Number(match[1]);
    const last = Number(match[2] ?? match[1]);
    if (
      !Number.isSafeInteger(first) ||
      !Number.isSafeInteger(last) ||
      first < 0 ||
      last < first ||
      last - first > 4096
    ) {
      throw new ProcAccountingError(
        "INVALID_AFFINITY",
        `invalid CPU affinity range: ${component}`,
        { source },
      );
    }
    for (let cpu = first; cpu <= last; cpu += 1) cpus.add(cpu);
  }
  return [...cpus].sort((left, right) => left - right);
}

export function parseProcStatusAffinity(source) {
  const match = source.match(/^Cpus_allowed_list:\s*(\S+)\s*$/m);
  if (match === null) {
    throw new ProcAccountingError(
      "INVALID_PROC_STATUS",
      "Cpus_allowed_list is absent from /proc status",
    );
  }
  return { allowedList: match[1], cpus: parseCpuList(match[1]) };
}

export function processIdentity(process) {
  return `${process.pid}:${process.starttimeTicks.toString()}`;
}

/** Build the root process and all descendants from one /proc table snapshot. */
export function buildDescendantTree(records, rootPid) {
  const byPid = new Map();
  const children = new Map();
  for (const record of records) {
    if (byPid.has(record.pid)) {
      throw new ProcAccountingError(
        "DUPLICATE_PID",
        `duplicate PID ${record.pid} in /proc snapshot`,
      );
    }
    byPid.set(record.pid, record);
    const siblings = children.get(record.ppid) ?? [];
    siblings.push(record.pid);
    children.set(record.ppid, siblings);
  }
  if (!byPid.has(rootPid)) {
    throw new ProcAccountingError(
      "ROOT_MISSING",
      `browser root PID ${rootPid} is absent from /proc snapshot`,
    );
  }

  const pending = [rootPid];
  const visited = new Set();
  const members = [];
  while (pending.length > 0) {
    const pid = pending.shift();
    if (visited.has(pid)) continue;
    visited.add(pid);
    const record = byPid.get(pid);
    if (record === undefined) continue;
    members.push(record);
    for (const child of children.get(pid) ?? []) pending.push(child);
  }
  members.sort((left, right) => left.pid - right.pid);
  return members;
}

function isTransientProcError(error) {
  return (
    error !== null &&
    typeof error === "object" &&
    (error.code === "ENOENT" || error.code === "ESRCH")
  );
}

function readThreadAffinities(procRoot, pid) {
  const taskRoot = join(procRoot, String(pid), "task");
  const tids = readdirSync(taskRoot, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && /^\d+$/.test(entry.name))
    .map((entry) => Number(entry.name))
    .sort((left, right) => left - right);
  const threads = [];
  for (const tid of tids) {
    try {
      const status = readFileSync(
        join(taskRoot, String(tid), "status"),
        "utf8",
      );
      threads.push({ tid, ...parseProcStatusAffinity(status) });
    } catch (error) {
      if (!isTransientProcError(error)) throw error;
    }
  }
  if (threads.length === 0) {
    throw new ProcAccountingError(
      "THREADS_MISSING",
      `PID ${pid} has no stable task entries`,
    );
  }
  return threads;
}

export function readLinuxProcessTable(procRoot = "/proc") {
  const records = [];
  for (const entry of readdirSync(procRoot, { withFileTypes: true })) {
    if (!entry.isDirectory() || !/^\d+$/.test(entry.name)) continue;
    try {
      records.push(
        parseProcStat(readFileSync(join(procRoot, entry.name, "stat"), "utf8")),
      );
    } catch (error) {
      if (!isTransientProcError(error)) throw error;
    }
  }
  return records;
}

function captureRecords(candidates, rootPid, procRoot, missingCode, missingMessage) {
  const members = [];
  for (const process of candidates) {
    try {
      const affinity = parseProcStatusAffinity(
        readFileSync(join(procRoot, String(process.pid), "status"), "utf8"),
      );
      members.push({
        ...process,
        ...affinity,
        threads: readThreadAffinities(procRoot, process.pid),
      });
    } catch (error) {
      if (!isTransientProcError(error)) throw error;
      if (process.pid === rootPid) {
        throw new ProcAccountingError(missingCode, missingMessage);
      }
    }
  }
  return {
    rootPid,
    capturedMonotonicNs: process.hrtime.bigint(),
    members,
  };
}

export function captureProcessTree(rootPid, procRoot = "/proc") {
  const candidates = buildDescendantTree(
    readLinuxProcessTable(procRoot),
    rootPid,
  );
  return captureRecords(
    candidates,
    rootPid,
    procRoot,
    "ROOT_MISSING",
    `browser root PID ${rootPid} disappeared during /proc capture`,
  );
}

export function readLinuxProcessGroup(processGroupId, procRoot = "/proc") {
  if (!Number.isSafeInteger(processGroupId) || processGroupId <= 1) {
    throw new ProcAccountingError(
      "INVALID_PROCESS_GROUP",
      `invalid process group ${processGroupId}`,
    );
  }
  return readLinuxProcessTable(procRoot)
    .filter((record) => record.pgrp === processGroupId)
    .sort((left, right) => left.pid - right.pid);
}

export function selectDedicatedProcessGroup(records, processGroupId) {
  const descendants = buildDescendantTree(records, processGroupId);
  const escaped = descendants.filter(
    (record) => record.pgrp !== processGroupId,
  );
  if (escaped.length > 0) {
    throw new ProcAccountingError(
      "PROCESS_GROUP_ESCAPE",
      `BrowserServer descendants escaped process group ${processGroupId}`,
      {
        processGroupId,
        escapedIdentities: escaped.map(processIdentity),
        escaped: escaped.map((record) => ({
          pid: record.pid,
          ppid: record.ppid,
          pgrp: record.pgrp,
          starttimeTicks: record.starttimeTicks.toString(),
        })),
      },
    );
  }
  return records
    .filter((record) => record.pgrp === processGroupId)
    .sort((left, right) => left.pid - right.pid);
}

/** Capture a dedicated BrowserServer process group, including reparented members. */
export function captureProcessGroup(processGroupId, procRoot = "/proc") {
  const candidates = selectDedicatedProcessGroup(
    readLinuxProcessTable(procRoot),
    processGroupId,
  );
  if (!candidates.some((record) => record.pid === processGroupId)) {
    throw new ProcAccountingError(
      "GROUP_LEADER_MISSING",
      `process-group leader PID ${processGroupId} is absent from /proc`,
    );
  }
  return captureRecords(
    candidates,
    processGroupId,
    procRoot,
    "GROUP_LEADER_MISSING",
    `process-group leader PID ${processGroupId} disappeared during /proc capture`,
  );
}

export function verifyDedicatedProcessGroup(snapshot, processGroupId) {
  const leader = snapshot.members.find((member) => member.pid === processGroupId);
  const violations = snapshot.members
    .filter((member) => member.pgrp !== processGroupId)
    .map((member) => ({ pid: member.pid, pgrp: member.pgrp }));
  if (leader === undefined || leader.pgrp !== processGroupId || violations.length > 0) {
    throw new ProcAccountingError(
      "PROCESS_GROUP_MISMATCH",
      `process scope is not the dedicated group led by PID ${processGroupId}`,
      {
        leader: leader === undefined
          ? null
          : { pid: leader.pid, pgrp: leader.pgrp },
        violations,
      },
    );
  }
}

function signalLinuxProcessGroup(processGroupId, signal) {
  try {
    process.kill(-processGroupId, signal);
    return true;
  } catch (error) {
    if (isTransientProcError(error)) return false;
    throw error;
  }
}

function waitMilliseconds(milliseconds) {
  return new Promise((resolvePromise) => {
    setTimeout(resolvePromise, milliseconds);
  });
}

/**
 * Terminate every member of a dedicated Linux process group. The second scan
 * deliberately occurs after SIGTERM so helpers forked by shutdown handlers are
 * still discovered even when the original group leader has already exited.
 */
export async function terminateLinuxProcessGroup(
  processGroupId,
  label,
  {
    procRoot = "/proc",
    termGraceMs = 3_000,
    killGraceMs = 1_000,
    onEvent = () => {},
  } = {},
) {
  if (!Number.isSafeInteger(processGroupId) || processGroupId <= 1) {
    throw new ProcAccountingError(
      "UNSAFE_CLEANUP_GROUP",
      `refusing to clean up invalid ${label} process group`,
      { processGroupId },
    );
  }
  if (
    !Number.isFinite(termGraceMs) ||
    termGraceMs < 0 ||
    !Number.isFinite(killGraceMs) ||
    killGraceMs < 0
  ) {
    throw new ProcAccountingError(
      "INVALID_CLEANUP_GRACE",
      `invalid cleanup grace for ${label}`,
      { termGraceMs, killGraceMs },
    );
  }

  const self = parseProcStat(readFileSync(join(procRoot, "self", "stat"), "utf8"));
  if (self.pgrp === processGroupId) {
    throw new ProcAccountingError(
      "UNSAFE_CLEANUP_GROUP",
      `refusing to signal the harness process group for ${label}`,
      { processGroupId, harnessPid: self.pid },
    );
  }

  const initial = readLinuxProcessGroup(processGroupId, procRoot);
  if (initial.length === 0) {
    const result = {
      label,
      process_group_id: processGroupId,
      already_exited: true,
      initial_identities: [],
      after_term_identities: [],
      remaining_identities: [],
      sigterm_sent: false,
      sigkill_sent: false,
      pass: true,
    };
    onEvent(result);
    return result;
  }

  const initialIdentities = initial.map(processIdentity);
  const sigtermSent = signalLinuxProcessGroup(processGroupId, "SIGTERM");
  await waitMilliseconds(termGraceMs);
  const afterTerm = readLinuxProcessGroup(processGroupId, procRoot);
  const sigkillSent = afterTerm.length > 0
    ? signalLinuxProcessGroup(processGroupId, "SIGKILL")
    : false;
  await waitMilliseconds(killGraceMs);
  const remaining = readLinuxProcessGroup(processGroupId, procRoot);
  const result = {
    label,
    process_group_id: processGroupId,
    already_exited: false,
    initial_identities: initialIdentities,
    after_term_identities: afterTerm.map(processIdentity),
    remaining_identities: remaining.map(processIdentity),
    sigterm_sent: sigtermSent,
    sigkill_sent: sigkillSent,
    pass: remaining.length === 0,
  };
  onEvent(result);
  if (remaining.length > 0) {
    throw new ProcAccountingError(
      "CLEANUP_FAILED",
      `${label} process group survived SIGKILL`,
      result,
    );
  }
  return result;
}

export function diffTreeMembership(start, end) {
  if (start.rootPid !== end.rootPid) {
    throw new ProcAccountingError(
      "ROOT_CHANGED",
      `process-tree root changed from ${start.rootPid} to ${end.rootPid}`,
    );
  }
  const before = new Map(
    start.members.map((member) => [processIdentity(member), member]),
  );
  const after = new Map(
    end.members.map((member) => [processIdentity(member), member]),
  );
  const removed = [...before.keys()].filter((identity) => !after.has(identity));
  const added = [...after.keys()].filter((identity) => !before.has(identity));
  return { added, removed, stable: added.length === 0 && removed.length === 0 };
}

export function assertStableTree(start, end, label = "process tree") {
  const churn = diffTreeMembership(start, end);
  if (!churn.stable) {
    throw new ProcAccountingError(
      "PROCESS_TREE_CHURN",
      `${label} changed during a stable interval`,
      churn,
    );
  }
  return churn;
}

export function verifyPinnedAffinity(snapshot, cpu) {
  const violations = [];
  for (const member of snapshot.members) {
    if (member.cpus.length !== 1 || member.cpus[0] !== cpu) {
      violations.push({ pid: member.pid, allowedList: member.allowedList });
    }
    for (const thread of member.threads) {
      if (thread.cpus.length !== 1 || thread.cpus[0] !== cpu) {
        violations.push({
          pid: member.pid,
          tid: thread.tid,
          allowedList: thread.allowedList,
        });
      }
    }
  }
  if (violations.length > 0) {
    throw new ProcAccountingError(
      "AFFINITY_MISMATCH",
      `browser process tree is not pinned to CPU ${cpu}`,
      { violations },
    );
  }
}

export function accountStableInterval({
  start,
  end,
  clockTicks,
  baselinePercent = 0,
  label = "measurement",
}) {
  if (!Number.isFinite(clockTicks) || clockTicks <= 0) {
    throw new ProcAccountingError(
      "INVALID_CLOCK_TICKS",
      `invalid CLK_TCK value: ${clockTicks}`,
    );
  }
  const wallSeconds =
    Number(end.capturedMonotonicNs - start.capturedMonotonicNs) / 1e9;
  if (!Number.isFinite(wallSeconds) || wallSeconds <= 0) {
    throw new ProcAccountingError(
      "INVALID_WALL_TIME",
      `invalid measured wall time: ${wallSeconds}`,
    );
  }
  assertStableTree(start, end, label);

  const startByIdentity = new Map(
    start.members.map((member) => [processIdentity(member), member]),
  );
  let cpuTicks = 0n;
  const memberDeltas = [];
  for (const member of end.members) {
    const identity = processIdentity(member);
    const before = startByIdentity.get(identity);
    const selfDeltaTicks = member.selfTicks - before.selfTicks;
    const reapedChildrenDeltaTicks =
      member.reapedChildrenTicks - before.reapedChildrenTicks;
    if (selfDeltaTicks < 0n || reapedChildrenDeltaTicks < 0n) {
      throw new ProcAccountingError(
        "NEGATIVE_CPU_DELTA",
        `CPU ticks moved backwards for ${identity}`,
        {
          startSelfTicks: before.selfTicks.toString(),
          endSelfTicks: member.selfTicks.toString(),
          startReapedChildrenTicks: before.reapedChildrenTicks.toString(),
          endReapedChildrenTicks: member.reapedChildrenTicks.toString(),
        },
      );
    }
    // Linux rolls a child's CPU into cutime/cstime only after that child is
    // waited for. Stable live descendants therefore remain represented solely
    // by their own self delta, while short-lived, fully reaped descendants are
    // recovered from the waiting stable member's child-time delta.
    const deltaTicks = selfDeltaTicks + reapedChildrenDeltaTicks;
    cpuTicks += deltaTicks;
    memberDeltas.push({
      identity,
      pid: member.pid,
      selfDeltaTicks,
      reapedChildrenDeltaTicks,
      deltaTicks,
    });
  }

  const cpuSeconds = Number(cpuTicks) / clockTicks;
  const rawPercent = (cpuSeconds / wallSeconds) * 100;
  const incrementalPercent = rawPercent - baselinePercent;
  return {
    cpuTicks,
    cpuSeconds,
    wallSeconds,
    rawPercent,
    baselinePercent,
    incrementalPercent,
    memberDeltas,
  };
}

export function passesIncrementalThreshold(
  incrementalPercent,
  threshold = 2.0,
) {
  return (
    Number.isFinite(incrementalPercent) &&
    Number.isFinite(threshold) &&
    threshold >= 0 &&
    incrementalPercent <= threshold
  );
}

export function selectConservativeBaselineIndex(
  rawPercents,
  baselineCeilingPercent,
) {
  if (!Array.isArray(rawPercents) || rawPercents.length < 2) {
    throw new ProcAccountingError(
      "INVALID_BASELINE",
      "at least two blank CPU samples are required",
      { rawPercents },
    );
  }
  if (
    !Number.isFinite(baselineCeilingPercent) ||
    baselineCeilingPercent <= 0
  ) {
    throw new ProcAccountingError(
      "INVALID_BASELINE",
      `invalid blank CPU ceiling: ${baselineCeilingPercent}`,
      { rawPercents, baselineCeilingPercent },
    );
  }
  let selected = 0;
  const ceilingViolations = [];
  for (let index = 0; index < rawPercents.length; index += 1) {
    const value = rawPercents[index];
    if (!Number.isFinite(value) || value < 0) {
      throw new ProcAccountingError(
        "INVALID_BASELINE",
        `invalid blank CPU sample: ${value}`,
        { rawPercents },
      );
    }
    if (value >= baselineCeilingPercent) {
      ceilingViolations.push({ index, rawPercent: value });
    }
    if (value < rawPercents[selected]) selected = index;
  }
  if (ceilingViolations.length > 0) {
    throw new ProcAccountingError(
      "INVALID_BASELINE",
      `blank CPU samples must each stay below ${baselineCeilingPercent}% of one core`,
      { rawPercents, baselineCeilingPercent, ceilingViolations },
    );
  }
  return selected;
}

export function serializeSnapshot(snapshot) {
  return {
    root_pid: snapshot.rootPid,
    captured_monotonic_ns: snapshot.capturedMonotonicNs.toString(),
    members: snapshot.members.map((member) => ({
      pid: member.pid,
      ppid: member.ppid,
      process_group_id: member.pgrp,
      comm: member.comm,
      state: member.state,
      starttime_ticks: member.starttimeTicks.toString(),
      utime_ticks: member.utimeTicks.toString(),
      stime_ticks: member.stimeTicks.toString(),
      cutime_ticks: member.cutimeTicks.toString(),
      cstime_ticks: member.cstimeTicks.toString(),
      self_ticks: member.selfTicks.toString(),
      reaped_children_ticks: member.reapedChildrenTicks.toString(),
      total_ticks: member.totalTicks.toString(),
      accounted_ticks: member.accountedTicks.toString(),
      affinity: member.allowedList,
      threads: member.threads.map((thread) => ({
        tid: thread.tid,
        affinity: thread.allowedList,
      })),
    })),
  };
}
