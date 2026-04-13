<!--
SYNC IMPACT REPORT
==================
Version change: 0.0.0 (template) → 1.0.0
Ratification: initial adoption

Modified principles:
  - [PRINCIPLE_1_NAME] → I. Real OS, Not A Simulation
  - [PRINCIPLE_2_NAME] → II. Strict Layering, No Shortcuts
  - [PRINCIPLE_3_NAME] → III. Browser-Only, Zero Backend
  - [PRINCIPLE_4_NAME] → IV. Offline-First And Persistent
  - [PRINCIPLE_5_NAME] → V. Process Isolation Is Mandatory
Added principles (expanded from 5 to 10 slots):
  - VI. Standard Syscall Surface (WASI-Based)
  - VII. Protocol Over API For The Display Server
  - VIII. Bottom-Up Construction
  - IX. Performance Budget
  - X. Testability At Every Layer
Added sections:
  - Architectural Constraints (layer catalogue + forbidden shortcuts)
  - Development Workflow & Quality Gates
Removed sections:
  - Generic template placeholder sections (SECTION_2, SECTION_3)

Templates requiring updates:
  - ✅ .specify/memory/constitution.md (this file)
  - ⚠ .specify/templates/plan-template.md — "Constitution Check" section is a
    generic placeholder; plans MUST fill it with explicit PASS/FAIL lines for
    each of Principles I–X. No template edit required; the /speckit.plan
    workflow is responsible for populating it.
  - ⚠ .specify/templates/spec-template.md — specs MUST state which layers
    they touch and declare any cross-layer interactions; no template edit
    required, this is enforced at spec-review time.
  - ⚠ .specify/templates/tasks-template.md — task categories MUST include
    per-layer isolation tests before integration tests; enforced at task
    review, no template edit required.

Deferred / follow-up TODOs:
  - None. Ratification date set to today.
-->

# PMos Constitution

PMos is a real operating system that happens to run inside a browser tab.
The browser is the hardware. Everything above it is an OS, built like an
OS, tested like an OS, and judged like an OS. This document is the set of
non-negotiable rules that every spec, plan, task, and line of code in this
repository MUST satisfy.

## Core Principles

### I. Real OS, Not A Simulation

PMos is an operating system in the strict, textbook sense. It MUST have a
kernel, isolated processes, a scheduler, a syscall interface, a virtual
filesystem, drivers, inter-process communication, and a display server.
The fact that the underlying "hardware" is a browser does not relax any of
these requirements and does not license shortcuts.

The kernel MUST NOT contain any concept that would be absent from a
textbook OS architecture diagram. In particular, the kernel MUST NOT know
what a "window," a "button," a "DOM node," or an "HTML element" is. If a
proposed kernel feature has no analogue in a conventional OS kernel, it
does not belong in the kernel.

**Rationale**: The value of building this system is lost the moment it
becomes a UI framework in a trench coat. Treating it as a real OS forces
correct abstractions and yields a system whose pieces are independently
meaningful.

### II. Strict Layering, No Shortcuts

The system is built in a fixed stack of layers, from bottom to top:

1. Browser substrate (JS/WASM runtime, storage APIs, workers)
2. Drivers (block, input, network, framebuffer, clock)
3. Kernel (scheduler, VFS, IPC, syscall dispatch, process table)
4. Display server (surfaces, buffers, input routing, compositor)
5. Window toolkit (client library wrapping the display protocol)
6. Desktop shell (launcher, panel, session — a userland program)
7. Applications

Each layer MUST only talk to the layer directly below it, through a
documented interface. Skipping a layer is prohibited. The display server
MUST be the only process that touches the framebuffer device. The desktop
shell MUST be an ordinary userland program whose only elevated capability
is a documented "shell" capability granted by the kernel at startup; it
MUST NOT have any other privilege the toolkit or an app could not also
request.

The desktop shell MAY additionally hold capabilities it delegates to
user-launched applications via the launcher; it is the trust root for
capability delegation to apps the user explicitly launches. This does
not grant the shell any kernel access beyond what those delegated
capabilities already permit.

**Layering test (MUST pass at all times)**: the desktop shell can be
deleted and replaced with a different userland program that holds the
"shell" capability, and every other layer continues to work unchanged
without recompilation of anything below it. A change that breaks this
test is a build-breaking issue.

Drift from layering is the single most dangerous failure mode for this
project and MUST be treated as such in review.

### III. Browser-Only, Zero Backend

The entire system runs in the user's browser. The repository MUST NOT
contain or depend on a server component, a per-user data store on
infrastructure we operate, a remote code execution service, an account
system, or telemetry of any kind. The deployment target is a static file
host (CDN) serving immutable assets.

After first load, the only network traffic permitted is traffic that the
user's own programs initiate through the OS's network stack. No layer
below applications may "phone home" for any reason.

**Rationale**: This guarantee is the product. Every user runs a private
OS we cannot see into and cannot take down. Any feature that requires a
backend MUST be rejected or redesigned to run client-side.

### IV. Offline-First And Persistent

After first load, the system MUST work with no network connection,
indefinitely. A service worker MUST cache all OS assets so the system
boots offline on subsequent loads. The user's filesystem MUST persist
across sessions in browser-local storage (IndexedDB or equivalent), and
closing the tab and reopening it MUST restore the filesystem and session
state.

A feature that only works online, or that loses user data on tab close,
is a regression against this principle and MUST be rejected.

### V. Process Isolation Is Mandatory

Every userland process MUST run in its own isolated execution context
with its own linear memory. A process MUST NOT be able to read or write
another process's memory directly, and this MUST be enforced by the
execution substrate (e.g., distinct WASM instances or workers), not by
convention or code review.

All inter-process communication MUST go through kernel-mediated IPC
primitives. Shared mutable memory between processes is prohibited except
through explicitly granted, kernel-tracked shared-buffer objects (e.g.,
display server surface buffers), which are themselves a kernel-managed
resource.

### VI. Standard Syscall Surface (WASI-Based)

The kernel MUST expose a POSIX-style syscall interface based on WASI.
Extensions beyond WASI are permitted only for capabilities WASI genuinely
does not cover: IPC primitives, display server access, device nodes, and
capability management. Every custom syscall MUST be justified in the
plan that introduces it, MUST be documented in the syscall reference, and
MUST be reviewed against the question "could this be expressed with
existing WASI calls?"

**Goal**: programs written for PMos look like normal POSIX-ish programs,
and existing WASI-compatible code MUST be portable to PMos with minimal
changes.

### VII. Protocol Over API For The Display Server

The display server MUST be reached through a wire protocol over IPC, not
through a shared library API. Clients submit buffers and receive events
over that protocol. The protocol takes its architectural cues from
Wayland: surfaces, buffers, commits, frame callbacks, input focus, and
window roles are first-class protocol concepts.

The window toolkit is a client-side library that wraps the protocol for
app authors' convenience, but the protocol — not the toolkit — is the
source of truth. A hand-written app that speaks the wire protocol
directly, with no toolkit linked in, MUST work. A toolkit feature that
cannot be expressed as protocol messages is not a toolkit feature.

### VIII. Bottom-Up Construction

Development MUST proceed from the kernel outward. No layer may be
started until the layer below it is demonstrably working and tested in
isolation.

Concrete gates:

- The kernel MUST be runnable and testable with no graphics whatsoever.
  A headless shell driven over a serial-style character device MUST work
  before any display server code is written.
- The display server MUST be runnable with a single hardcoded test
  client and a mock framebuffer before any toolkit code is written.
- The toolkit MUST be usable by a hand-written test app before any
  desktop shell code is written.
- The desktop shell MUST be usable with at least one real userland app
  before end-user application work begins.

### IX. Performance Budget

PMos MUST remain interactive on a mid-range laptop from five years ago.
The following budgets are hard limits, not aspirations:

- Cold load over a fast connection to a logged-in desktop: **< 10 s**.
- Warm load (cached) to a logged-in desktop: **< 3 s**.
- Window drag, keystroke-to-screen, and app launch perceived latency:
  **< 100 ms**.
- Idle CPU with a logged-in desktop and no user activity: negligible
  (no busy loops, no unnecessary wakeups).

A design choice that would blow any of these budgets MUST be revisited,
not accepted. Plans MUST state the expected impact on each budget;
tasks that measurably regress a budget MUST be rejected or accompanied
by an approved deviation.

Aggregate empirical gates (notably T220 for input latency and
T128/T208 for cold/warm load and asset size) satisfy this principle's
testability requirement; per-task budget annotations are not required
when an aggregate gate covers the relevant budget.

### X. Testability At Every Layer

Each layer MUST ship with tests that exercise it in isolation from the
layers above it:

- Kernel tests MUST run without a display server.
- Display server tests MUST run with a mock client and a mock
  framebuffer, with no real toolkit or shell.
- Toolkit tests MUST run against a mock display server.
- Integration tests cover the full stack and are additional, not a
  substitute for isolation tests.

A change that breaks a layer's isolation tests MUST be rejected before
it is allowed to touch integration tests. "It passes integration" is
not a defence against a broken isolation test.

## Architectural Constraints

**Layer catalogue (authoritative)**: browser substrate → drivers →
kernel → display server → toolkit → desktop shell → applications. Any
new component MUST be placed in exactly one of these layers in its
introducing plan.

**Forbidden shortcuts** (non-exhaustive; each is a layering violation):

- Kernel code that imports DOM or canvas APIs.
- Display server code reached by direct function call from an app
  instead of by protocol message.
- Toolkit code that bypasses the display server and renders to the
  framebuffer itself.
- Desktop shell code that holds any capability not also grantable to a
  non-shell userland program.
- Any userland process reading another process's memory through a
  non-IPC channel.
- Any code path that requires a network fetch at runtime to function.

**Capabilities**: privilege in PMos is expressed as kernel-granted
capabilities, not as "the shell is special." The shell capability MUST
be a documented, named capability, and the kernel MUST be able to
grant it to an alternative program at boot.

## Development Workflow & Quality Gates

**Spec gate**: every feature spec MUST declare which layer(s) it
touches and MUST state explicitly that it does not introduce a
cross-layer shortcut. Specs that cannot make this statement MUST be
rewritten.

**Plan gate (Constitution Check)**: every implementation plan MUST
include a Constitution Check section with an explicit PASS/FAIL line
for each of Principles I–X, plus a note on the performance budget
impact (Principle IX). A FAIL line MUST either be removed by
redesigning the plan or MUST be recorded as a deviation in the plan's
Complexity Tracking table with written justification and an owner.

**Task gate**: task lists MUST schedule per-layer isolation tests
(Principle X) before integration tests for the same functionality. A
task that adds behaviour without a corresponding isolation test in the
same layer is not ready to merge.

**Review gate**: code review MUST verify, at minimum, that (a) the
layering test of Principle II still passes, (b) no new kernel code
contains UI concepts, (c) no new network fetch paths have been added
below the applications layer, and (d) isolation tests still pass for
every layer the change touches.

**Deviation log**: any accepted violation of a principle MUST be
recorded in the plan that introduced it, under an explicit "Known
Deviations" heading, with the principle number, the reason, and the
intended remediation. Unrecorded deviations are build-breaking issues.

## Governance

This constitution supersedes all other practices, style guides, and
conventions in this repository. In any conflict, this document wins.

**Amendment procedure**: amendments are proposed as pull requests that
modify this file and include a Sync Impact Report (see the HTML comment
at the top of this file) describing the version bump, the principles
affected, and the downstream templates that need updating. Amendments
MUST be reviewed against the same principles they modify: an amendment
that weakens a principle MUST state, in writing, what architectural
goal is being traded away and why.

**Versioning policy**: this constitution follows semantic versioning.

- **MAJOR**: a principle is removed, redefined in a backward-
  incompatible way, or the layer catalogue is restructured.
- **MINOR**: a new principle or new normative section is added, or an
  existing principle is materially expanded.
- **PATCH**: wording clarifications, typo fixes, non-semantic edits.

**Compliance review**: every plan's Constitution Check is a compliance
review. In addition, a full audit of the layering test (Principle II)
and the performance budgets (Principle IX) MUST be run at each release
candidate. Drift from layering is treated as a build-breaking issue,
not as tech debt.

**Runtime guidance**: day-to-day development guidance that is not
constitutional (coding style, dependency choices, directory naming)
lives in `README.md` and in per-layer docs. Those documents MUST NOT
contradict this constitution; where they do, this constitution wins and
the other document MUST be corrected.

**Version**: 1.0.0 | **Ratified**: 2026-04-13 | **Last Amended**: 2026-04-13
