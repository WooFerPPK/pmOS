# /speckit.analyze findings log

Findings from `/speckit.analyze` runs, persisted across sessions so future readers understand why specific T-IDs exist.

Findings are identified U1, U2, U3, … in session-chronological order. Each entry records the finding text (reconstructed where the original was lost), severity, and resolution (T-ID + commit SHA, or direct spec/plan edit).

## Session: prior (date unknown, before 2026-04-13 tasks.md regeneration)

### U1 — text not preserved; resolution unknown

No record of U1's finding text survives in the repo. Either:

- U1 was resolved by a direct spec/plan edit committed before the tasks.md regeneration that introduced T219–T222, or
- The finding sequence for that session started at U2 and U1 never existed.

Future reviewers should either back-fill U1's text if it surfaces in chat logs / Linear / elsewhere, or drop the U-ID sequence and renumber U2–U5 as U1–U4. The cost of leaving a gap in the numbering is low.

### U2 — FR-028 window-close lacked integration-test evidence

**Reconstructed finding** (from T219's `Addresses FR-028 and /speckit.analyze finding U2` annotation): the spec requires that closing a window terminate the owning process (FR-028), but no Playwright integration test asserted the end-to-end behaviour. Without such a test, a regression that left the process running after close could slip through.

**Resolution**: **T219** — `web/tests/integration/window-close.spec.ts`, Playwright test that launches `/usr/bin/edit`, clicks its titlebar close button via synthetic input routed through the input driver's test-harness injection hook, and asserts via a `/proc` snapshot that the owning process has exited within 1 second. Pending in this historical reconstruction; the shipped Files/taskbar implementation and current evidence are recorded below.

### U3 — SC-003 input-latency p95 budget had no defined methodology or executable gate

**Reconstructed finding** (from T220's annotation): SC-003 (input latency p95 ≤ 100 ms) had no defined measurement methodology and no executable CI gate. Principle IX's testability claim ("aggregate empirical gates satisfy this principle's testability requirement") was therefore partially unverified, since T220 itself was the gate and did not exist.

**Resolution**: **T220** — perf harness at `crates/integration-tests/src/bin/input-latency.rs` measuring input-to-pixel p95 and failing the test if the budget is exceeded. Partial in this historical reconstruction — native-Rust synthetic proxy landed in commit `1432f1b` (2026-04-24); the browser-authoritative completion is recorded below.

### U4 — SC-012 external-developer quickstart lacked executable validation

**Reconstructed finding** (from T221's annotation): SC-012 ("a developer who has not worked on the project before can write a new application using only the public documentation") had no automated or repeatable validation. The quickstart documentation could be stale or missing steps that only insiders would know, and nothing would surface the drift until an external contributor stumbled on it.

**Resolution**: **T221** — spin up a fresh container (minimal Ubuntu or Alpine image) with only the documented prerequisites, clone the repo, walk `quickstart.md` end-to-end (`just build`, `just dev`, `just test`, the worked "hello" example from §5, the toolkit-free example from §6). Record every deviation between docs and behaviour. Fix `quickstart.md` and referenced docs until a clean container run succeeds with zero deviations. Resolved on 2026-08-08 by the scoped source-isolated evidence in [`quickstart-validation.md`](./quickstart-validation.md); that record explicitly preserves the container and monolithic-gate coverage boundaries rather than rewriting the historical task wording.

### U5 — FR-040–044 non-goal surface had no automated audit

**Reconstructed finding** (from T222's annotation): the non-goal list (FR-040 no multi-user; FR-041 no accounts; FR-042 no x86 emulation; FR-043 no GPU 3D; FR-044 no raw TCP/IP) had no automated audit. Drift could accumulate silently as contributors added crates or dependencies that violated the non-goals — a service URL in a comment, a WebGL import behind a feature flag, a UID variable inherited from a dependency's example code.

**Resolution**: **T222** — `scripts/non-goal-audit.sh` greps the repo (excluding `target/`, `node_modules/`, `dist/`, `build/`, `.git/`) for forbidden patterns: cloud-service URLs (`s3://`, `gs://`, `azure`, `supabase`, `firebase`); authentication keywords (`login`, `signup`, `oauth`, `jwt`, `session_token`); WebGL / WebGPU imports outside the documented compositor stub; raw TCP/IP imports (`net::TcpStream`, etc.); multi-user APIs (`uid`, `gid`, `getpwnam`, `/etc/passwd`). Produces `docs/non-goal-compliance.md` listing every match with a one-line justification or a "false positive — reason" note. Idempotent, deterministic, re-runnable in CI. Pending in this historical record; current disposition is recorded below.

## Session: 2026-04-24 (agent-management / recovery)

No new findings surfaced beyond pre-existing ones. The analysis report was delivered inline to the user and acted on via immediate docs remediation in the same session:

- **A1** (HIGH, T152 ambiguity) — T152 task text rewritten to cite the existing `host_file_recv` syscall (opcode `0x1500`, `contracts/syscalls.md §3.6`) and delegate ABI wiring to T072 / kernel impl to T153.
- **U1** (MEDIUM, missing finding texts) — this file (`analyze-findings.md`) created to capture U1–U5 so future readers have the full context.
- **C2** (LOW, plan Known Deviations out of date) — `plan.md` Known Deviations block extended with **Deviation #6** covering T079's `common.ts` / `types.ts` naming bifurcation (commit `b0baf01`).
- **G1–G3** (LOW, FR annotations) — T094 now annotates FR-009a (user-visible evidence); T113 annotates FR-020 (acceptance evidence); T176 annotates FR-009 (end-to-end evidence).

Remaining open items from the 2026-04-24 analysis:

- **I1 / C1** — T220 is partial; T128 and T208 are not-started. Principle IX's budgets are therefore not yet gated by executing CI. Track until T128 + T208 + T220-full-stack all land.
- **G4 / U1** — if U1's text surfaces in an external log, back-fill the entry above.
- **I2** — widget list in `plan.md` module tree (`button, label, text input, list, container`) drifts from tasks.md T116 partial-landed note (`Alignment, WindowFrame, Label, Button, TextInput` + deferred List/Container). Sync on the next plan pass.

## Future sessions

Future `/speckit.analyze` runs MUST append a new `## Session: <date>` block to this file with the findings raised and their dispositions, so this log remains the authoritative record of analysis history.

## Session: 2026-08-07 (final release audit)

This session preserves the historical entries above and records their current
disposition against the shipped source and executable gates.

### U2 — resolved by the real Files close workflow

`web/tests/integration/window-close.spec.ts` now launches the shipped Files app,
finds its painted titlebar and focused task entry from framebuffer evidence,
and invokes close through the shipped taskbar shell-manager control. It requires
Files to acknowledge the display-server close event, its live user-Worker count
to drop within one second, and both the titlebar and task entry to disappear. It
does not use the previously proposed Edit/titlebar injection or a synthetic
`/proc` snapshot.

### U3 — resolved by canonical full-stack browser gates

`input-to-pixel-latency.spec.ts` is the authoritative focused-input gate and
`typical-desktop-latency.spec.ts` completes the requested 300 causal
interactions across six real apps. The canonical Chromium/Firefox p95 values
are 3.3/33.7 ms for standard input and 17.8/55.3 ms for the six-app workload;
all taskbar transitions are also below 100 ms. Cold desktop is 2029/2119 ms
and offline warm is 611/1488 ms. Restored warm and input plus all 12 idle-CPU
comparisons pass. The source-bound log SHA-256 is
`956653f04b70ff44c28c028a6334d18bdf9acb9632ab3bb2e2667de4eaa8d713`;
the native 2 us sink remains supplemental only.

### U4 — production workflow and clean-snapshot validation landed

The toolkit example now says exactly that `App::connect` needs a caller-supplied
`Connection` and directs authors to copy the complete sample transport scaffold,
including the cfg-gated allocator declaration. The toolkit-free path is no
longer an in-memory dry run: its WASI binary calls `display_connect`, discovers
the shipped registry globals, maps and paints a collapsed-v1 toplevel, receives
keyboard input, repaints, handles close, and exits through production
`/run/display`. Native real-server isolation covers the same state machine and
`web/tests/integration/toolkit-free-client.spec.ts` defines the separately
isolated Chromium/Firefox Worker gate. T221's 2026-08-08 frozen source-only walk
is now recorded in [`quickstart-validation.md`](./quickstart-validation.md):
dependency setup, clean build, focused static/isolation gates, the complete §5
build/package path, and the §6 production Chromium/Firefox workflow passed. The
record explicitly distinguishes source isolation from a literal new container
and leaves the canonical monolithic release gate to its owning lane.

The definitive 2026-08-11 record now appends an exact-source Ubuntu 24.04.4
walk using Rust 1.97.1, without rewriting the Rust 1.94.1 history above. Every
setup/build/canonical/tutorial/raw-client/clean step exited zero. Evidence is
retained at `/tmp/pmos-ubuntu24-evidence.c5w5tRSc`; transcript SHA-256 is
`2a0a1130131b74c9d11b4c5ef19aa752b3d6667833398a8bb5e0ce28a66666a6`
and canonical-log SHA-256 is
`358a80a829abc7d1a05c98907d2e1ce87d947361593f64037a4fde5e59c2cbb3`.

### U5 — implemented and fully classified

`scripts/non-goal-audit.sh` and `docs/non-goal-compliance.md` exist, and
`just test` runs the deterministic audit through `test-non-goal-audit`. Its
fixture test is blocking; the live scan remains non-blocking but is fully
reviewed. Consecutive live outputs are byte-identical and classify 104 matches
as 81 false positives, 23 legitimate references, and 0 TODO items across six
categories. User-local `.claude` tooling is excluded from the product ledger.

### Completion-marker reconciliation complete

- T208 now cites exact canonical cold, warm, input, interaction, restore, and
  idle figures in `perf-results.md`.
- T113 has native isolation plus recorded Chromium/Firefox production evidence
  in the canonical 63/63 Playwright result.
- T221 retains the scoped 2026-08-08 history and adds the complete Ubuntu
  exact-source validation.
- T238 through T241 are closed with source, isolation, browser, performance,
  and Ubuntu log citations in `tasks.md`.
- T218 remains open. No tag, publication, deployed URL, or deployed-site PASS
  is inferred from local or Ubuntu validation.

The tested executable archive predates this documentation-only reconciliation;
the prose update is not represented as part of that frozen source archive.
