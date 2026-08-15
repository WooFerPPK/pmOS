# PMos Agent Guidelines

This file is the Codex-facing companion to `CLAUDE.md`. It captures the
repository rules an automated coding agent should follow when changing PMos.
If this file conflicts with `.specify/memory/constitution.md`, the
constitution wins.

## Project identity

PMos is a real operating system that runs entirely inside a browser tab.
The browser is the hardware substrate; everything above it should be designed,
layered, and tested like an OS. Do not treat this project as a web app or UI
framework.

There is no backend, no accounts, no telemetry, and no hosted per-user data.
The deployable artifact is a directory of static files.

## First checks for substantial work

- Read `.specify/memory/constitution.md` before proposing or implementing a
  substantial architectural change.
- Check the relevant files under `specs/001-browser-os-v1/`, especially
  contracts in `specs/001-browser-os-v1/contracts/`, before changing syscall,
  display protocol, package, init, or driver behavior.
- Keep the layer touched by the change explicit. Avoid broad refactors unless
  the task asks for them or the current change cannot be completed cleanly.
- Do not overwrite unrelated local changes. This repo may have generated files
  and work-in-progress artifacts in the working tree.

## Non-negotiable architecture

The authoritative layer order is:

```text
browser substrate -> drivers -> kernel -> display server -> toolkit -> desktop shell -> applications
```

Rules that matter most in day-to-day edits:

- The kernel must not know about windows, buttons, DOM nodes, HTML elements,
  canvas objects, or shell UI concepts.
- Each layer talks only to the layer directly below it through documented
  interfaces.
- The display server is reached through the Wayland-inspired wire protocol over
  IPC at `/run/display`; apps and toolkit code must not call display server
  internals directly.
- The display server is the only process that should touch the framebuffer
  device.
- The desktop shell is an ordinary userland program with documented
  capabilities. It must be replaceable without recompiling or changing lower
  layers.
- Userland processes must run in isolated WASM instances/workers with separate
  linear memory. IPC goes through kernel-mediated primitives unless a
  kernel-tracked shared buffer object has been explicitly granted.
- The syscall surface is WASI preview 1 plus documented PMos extensions only.
  New custom syscalls require justification and contract updates.
- The root filesystem is OPFS-based. Do not introduce IndexedDB as the FS
  backend without an explicit spec/plan change.
- No runtime network fetch path belongs below applications.

## Current implementation shape

The codebase now includes real implementation, not just plans. Key areas:

```text
crates/
  abi/                  shared syscall numbering and layouts
  ring/                 SharedArrayBuffer ring transport pieces
  kernel/               kernel, wasm32-unknown-unknown
  init/                 PID 1 userland
  display-server/       display server
  display-server-lite/  lighter display fixture/runtime
  display-proto/        display wire protocol definitions
  toolkit/              client-side toolkit library
  shell sh term files edit settings sysmon preferences
  sample-app/ toolkit-free-client/
  hello-*               focused userland/syscall/demo fixtures
  integration-tests/    native Rust integration/perf harnesses
  xtask/                dist assembly, packaging, dev server helpers
web/
  src/bootstrap.ts
  src/kernel-worker*.ts
  src/user-worker*.ts
  src/drivers/
  src/shared/
  src/sw.ts
specs/001-browser-os-v1/
  spec.md plan.md tasks.md contracts/
```

## Commands

Use the `Justfile` targets when possible:

- `just build` builds Rust, TypeScript, and assembles `dist/`.
- `just dev` builds and serves `dist/` with COOP/COEP headers on port 8080.
- `just test` runs the full gate.
- `just test-kernel` runs kernel isolation tests.
- `just test-display-server` runs display server isolation tests.
- `just test-toolkit` runs toolkit tests against a mock display server.
- `just test-drivers` runs Vitest for TypeScript drivers.
- `just test-integration` builds and runs Playwright integration tests.
- `just test-perf` runs the native Rust input-latency perf harness.

When a change is narrow, run the smallest relevant test first, then broaden if
the touched surface justifies it. Integration tests do not replace isolation
tests for the layer being changed.

## Coding conventions

- Rust uses latest stable. Kernel code is `no_std + alloc`; most other Rust
  crates use `std`.
- Avoid `unsafe` in the kernel except inside the established platform/ring
  abstraction boundaries.
- TypeScript is strict, framework-free, and should avoid `any`. Treat the JS
  layer as firmware, not application UI.
- Prefer existing local abstractions, contracts, and test fixtures over new
  helper layers.
- Keep comments sparse. Add comments for non-obvious reasoning, not to restate
  the code.
- Preserve static-host deployment assumptions. Do not add backend requirements,
  telemetry, account flows, or server-only tooling to runtime behavior.

## Planning and review expectations

Every implementation plan or PR-level summary should include a Constitution
Check with PASS/FAIL for Principles I-X and a Principle IX performance impact
note. A known deviation must be recorded explicitly in the relevant plan.

Review for:

- Layering violations, especially shell/display/toolkit shortcuts.
- Kernel imports or concepts that belong to UI, DOM, canvas, or applications.
- New network fetch paths below applications.
- Process isolation regressions.
- Missing isolation tests for the layer changed.
- Performance regressions against the budgets: cold load < 10 s, warm load
  < 3 s, and perceived input/window/app-launch latency < 100 ms.

## Accessibility scope

For v1, accessibility is a documented non-goal. Do not advertise accessibility
support or add tests that assert accessibility semantics unless the v2 amendment
or a current task explicitly brings accessibility into scope.

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

When the user types `/graphify`, use the installed graphify skill or instructions before doing anything else.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- Dirty graphify-out/ files are expected after hooks or incremental updates; dirty graph files are not a reason to skip graphify. Only skip graphify if the task is about stale or incorrect graph output, or the user explicitly says not to use it.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
