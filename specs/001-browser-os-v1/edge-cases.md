# PMos v1 — Edge Case Coverage Checklist (T214)

This document walks the 11 edge cases enumerated in `spec.md`
"Edge Cases" and cites either an automated test or a documented
manual procedure for each. Cited tests run as part of the
default `just test` target.

| # | Edge case                              | Coverage                                                                                                                                  |
|---|----------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------|
| 1 | Storage denied / private-mode eviction | `bootstrap.ts` reports OPFS unavailability via the boot-screen check rows (visible in `web/index.html` output). Manual smoke in private-browsing tab. |
| 2 | Process crash                          | `web/tests/integration/process-crash.spec.ts` (T176) asserts the kernel reaps every child cleanly without panicking peers.                 |
| 3 | Closed pipe with surviving writer      | `crates/sh/tests/pipeline.rs` placeholder (T149) gates the future T143 pipe-runner test; current `sh` doesn't yet support `\|` so the runtime case is deferred to that slice. |
| 4 | Many concurrent windows under load     | `crates/integration-tests/tests/two_term_windows.rs` exercises two concurrent toolkit clients sharing the compositor; perf is gated by T220's input-latency harness. |
| 5 | Storage quota exhausted                | Manual: in browser DevTools, simulate quota via `navigator.storage.estimate()` mock; PMos's block-driver returns ENOSPC. No automated harness for v1 — manual procedure in `quickstart.md §9 Troubleshooting`. |
| 6 | Process killed with open file handles  | Covered by `crates/kernel/tests/proc.rs` (kernel-side) — the FdTable's drop-on-exit path closes every fd; OPFS journal preserves on-disk consistency. |
| 7 | Input before focus is routed           | `crates/display-server/tests/keymap.rs` covers the keyboard input routing surface. The "no window holds focus" branch is documented in `display-protocol.md §15` (events are dropped, not leaked). |
| 8 | Window dragged off-screen              | The toolkit's drag interaction stops at the framebuffer's east/south edges (`crates/display-server/tests/compositor.rs`). Manual smoke: drag a window past the right edge with `#real-kernel`'s display-client demo. |
| 9 | Unsupported browser                    | `bootstrap.ts` emits a fallback message via `showFallbackMessage` if the browser lacks SAB / OPFS / Workers; visible at the top of `index.html` instead of an empty canvas. |
| 10 | Unauthorized memory access attempt     | `web/tests/integration/process-isolation.spec.ts` (T175) — Principle V gate. The mem-adversary binary attempts cross-process reads and exits non-zero on every attempt; the kernel reaps without isolation violations. |
| 11 | Corrupt app bundle in /apps            | `crates/pkg/tests/validate.rs` (T206) — 9 malformed-bundle tests reject every bad shape (absolute path, `..`, bad WASM magic, missing fields, unknown caps, missing manifest, missing binary file, empty archive). |

## Manual procedures

The two manual entries above (#5 quota, #8 dragging) have brief
walk-throughs in `quickstart.md §9 Troubleshooting`. Re-walk both
when cutting a release; record the date in the release-notes git
tag annotation.

## Last walk

This checklist was walked end-to-end at **T214 close** (the v1
release boundary). Re-walk on every minor release.
