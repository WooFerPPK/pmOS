# PMos v1 System Monitor Contract

This contract defines the bundled `/usr/bin/sysmon` application. System
Monitor is an ordinary userland toolkit client; it has no privileged browser,
kernel, display-server, or framebuffer access.

## 1. Process data

System Monitor MUST obtain process data through the VFS interfaces already
defined by the procfs and syscall contracts:

- enumerate numeric entries below `/proc`;
- read `/proc/<pid>/status` for PID, parent PID, name, state, `VmSize`, and
  `VmPeak`;
- read PMos's optional `FDCount` status field to display the exact number of
  open descriptors without materialising every descriptor target;
- when `FDCount` is absent (for example in a compatible synthetic or foreign
  proc tree), enumerate `/proc/<pid>/fd` as the compatibility fallback.

The live PMos procfs source MUST emit `FDCount:\tN` from the kernel-owned fd
table in every `/proc/<pid>/status` snapshot. `/proc/<pid>/fd` remains
available for descriptor inspection; the status field is a bounded summary,
not a replacement for that directory.

The displayed table MUST refresh from a monotonic clock at least once per
second. Arbitrary event-loop iteration counts are not a clock and MUST NOT be
used as the refresh interval.

A process can exit between directory enumeration and a per-process read.
System Monitor MUST skip that transient row and surface a warning that the
snapshot was partial. If `/proc` itself cannot be enumerated, it MUST retain
the last good snapshot and show the read error; it MUST NOT replace the table
with a misleading empty snapshot.

## 2. Interaction

The ordinary display-protocol keyboard and pointer events provide:

- row selection, with selection identity tracked by PID rather than row index;
- arrow, page, home/end, and scrollbar navigation;
- explicit and automatic refresh;
- window close handling;
- a terminate action followed by an Enter/Escape confirmation.

Selection MUST be revalidated after every refresh. A reordered table MUST NOT
cause a confirmation for one PID to signal a different PID.

## 3. Termination capability boundary

Termination uses only the documented `pmos_ext.proc_kill(pid, SIGKILL)`
extension. At startup the process MUST query `PROC_KILL_ANY` with
`pmos_ext.cap_check`:

- when present, Terminate is enabled, refuses the System Monitor's own PID,
  and calls `proc_kill` only after explicit confirmation;
- when absent or when the capability query fails, the app remains useful and
  visibly read-only; it MUST NOT attempt `proc_kill`;
- a `proc_kill` error MUST be shown in the status area and MUST NOT be reported
  as a successful exit.

The launcher remains subject to `package-manifest.md` capability-delegation
rules: it cannot give System Monitor a capability it does not itself hold.
That spawn-policy boundary does not justify adding a new syscall or bypassing
the kernel capability check.

## 4. Repaint and acceptance gates

System Monitor processes input on every display dispatch, but repaints only
after configuration, input/state changes, or the one-second snapshot. It MUST
not continuously upload unchanged framebuffer buffers.

Isolation tests cover proc parsing, memory/fd projection, transient errors,
selection/scrolling, refresh timing, confirmation, self-protection, and
read-only fallback. The Chromium/Firefox browser acceptance gate launches four
independent graphical peers followed by System Monitor, binds all five exact
launcher PIDs to distinct rows with positive memory and fd metrics, and selects
one exact peer by its PID-sorted row. After explicit confirmation, that PID's
host termination, Worker removal, and task-entry removal MUST converge within
one shared second. System Monitor MUST subsequently report that exact row's
exit, while the other four original processes remain alive and accept
app-specific keyboard or pointer input without replacement or relaunch.
