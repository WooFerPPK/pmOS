# PMos v1 — Performance Audit Results (T208)

Constitution Principle IX: cold load < 10 s, warm load < 3 s,
input-to-pixel < 100 ms (p95).

This document defines the v1 qualification methodology and retains accepted
measurement artifacts for the canonical "mid-range-from-five-years-ago
laptop" target (roughly: dual-core x86-64 ≥ 2.5 GHz, 8 GB RAM, integrated
graphics, NVMe storage). A configured gate is not reported as a measured PASS
until its canonical release-rerun artifact is available.

## Methodology

- **Cold load** — measured by the `boot-to-desktop.spec.ts`
  Playwright integration test. Wall-clock starts immediately before
  `page.goto` and ends only after the browser has painted the first real
  framebuffer frame and removed the boot splash. The spec enforces the
  strict `< 10 000 ms` budget.
- **Warm load** — measured by the `offline-boot.spec.ts`
  Playwright integration test. It waits for an active, controlling service
  worker, closes the online page and its Workers, stops the origin server,
  then opens a new page from the cache during a real network outage. The
  offline boot must reach the display-server client milestone in
  `< 3 000 ms`; there is no relaxed release tolerance.
- **Keystroke-to-pixel p95** — measured authoritatively by
  `input-to-pixel-latency.spec.ts`. A real Terminal process receives 20
  steady-state browser keyboard events through the input driver, kernel,
  display server, and renderer. Each sample ends at the completed
  `pmos:frame` presentation; the measured p95 must be `< 100 ms`.
- **Window and app interaction** — measured authoritatively by
  `desktop-taskbar-controls.spec.ts`. It times real launcher-open,
  Terminal-launch, minimize, restore, and close interactions from trusted
  browser `pointerdown` to completed `pmos:frame` presentation. The launcher
  sample completes only when the frame also changes a pixel in the opened menu,
  so an unrelated presentation cannot satisfy the measurement. Every sample
  must be `< 100 ms`.
- **Typical restored workload** — `typical-desktop-latency.spec.ts` injects
  300 causal interactions across six real applications, while
  `session-restore.spec.ts` separately measures restored-session readiness and
  20 post-restore input samples. The p95 input budgets remain `< 100 ms` and
  restored warm readiness remains `< 3 000 ms`.
- **Idle CPU** — `idle-cpu-gate.mjs` performs two runs per supported engine.
  Each run takes two settled blank-browser samples, uses the lower valid sample
  as its baseline, and compares shell-only, six-app, and restored-six-app
  phases. Every incremental result must be `<= 2%` of one core.

The browser Playwright workflows are the release authority for perceived
latency because they exercise the complete Worker/WASM/SAB/compositor/canvas
path. The native Rust sink harness is a supplemental microbenchmark only.

## Canonical release record (2026-08-11)

The executable release evidence is bound to the source archive
`/tmp/pmos-source-freeze-final.jAM9XkYd/pmos-source.tar`, SHA-256
`f65d3376abb62da21ad61932544ba97bd9b464c7a016e53c6806696808c2f9e7`.
Its 567-entry source manifest has SHA-256
`3569e63c88850318730de432625ad5e903de99ab0b953d9fedced52af03f2335`.
The canonical evidence directory is
`/tmp/pmos-canonical-final3-evidence.ESBHuyWY`; the complete log is
`/tmp/pmos-canonical-final3-full.log`, SHA-256
`956653f04b70ff44c28c028a6334d18bdf9acb9632ab3bb2e2667de4eaa8d713`.
That log records 34 Vitest files / 767 tests, 63 Playwright tests, 21
idle-gate unit tests, a complete 4-run idle measurement set, and the native
performance harness, all passing.

All values below are milliseconds except idle CPU:

| Engine | Cold desktop | Offline warm | Input cold first | Input p95 | Taskbar launcher / launch / minimize / restore / close |
| --- | ---: | ---: | ---: | ---: | --- |
| Chromium | 2029 | 611 | 3.4 | 3.3 | 9.4 / 9.5 / 2.9 / 5.5 / 6.2 |
| Firefox | 2119 | 1488 | 32.1 | 33.7 | 54.7 / 46.6 / 39.7 / 43.2 / 39.8 |

| Engine | Six-app interactions | Typical p95 | Typical p99 | Restored warm | Restored-input p95 | Restored-input p99 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Chromium | 300 | 17.8 | 40.5 | 504.5 | 3.8 | 10.6 |
| Firefox | 300 | 55.3 | 58.3 | 1105.4 | 36.1 | 36.2 |

The idle gate selected the lower of two valid same-run blank samples. The table
reports baseline and incremental percentage of one core:

| Engine / run | Selected blank | Shell | Fresh six apps | Restored six apps |
| --- | ---: | ---: | ---: | ---: |
| Chromium 1 | 0 | 0.0666228534617785 | 0.7998305744488723 | 0.9321895723377167 |
| Chromium 2 | 0 | 0 | 0.6665331803332546 | 0.9322732896896844 |
| Firefox 1 | 0.19976281737196255 | 0.3331537453659822 | 1.2659738615534077 | 1.3980380836573651 |
| Firefox 2 | 0.26653054891120187 | 0.13313307973774807 | 1.1326536098457263 | 1.4661098557825878 |

The gate-end record is complete and passing: 4 runs, 8 blank baselines, 12
comparisons, and 20 total measurements. Every reported incremental value is at
or below the strict 2% limit.

Ubuntu 24.04.4 independently repeated the complete gate against that exact
archive. Its canonical log SHA-256 is
`358a80a829abc7d1a05c98907d2e1ce87d947361593f64037a4fde5e59c2cbb3`;
the complete Ubuntu evidence-manifest SHA-256 is
`b423cc15a85262b4a04d0a336162e12371c34f97eab1f157084220c9b30e855e`.
Ubuntu measurements are retained in that log as an independent reproduction,
not mixed into the local table above.

These documentation-only ledger changes occurred after the frozen archive was
tested. They do not rewrite the archive or claim that the reconciled prose was
part of the validated executable source.

## Historical focused desktop taskbar trace (2026-08-07)

Recorded by the final focused Chromium-and-Firefox run of
`desktop-taskbar-controls.spec.ts` against the fixed taskbar path:

| Engine | Launcher open | Launch transition | Minimize | Restore | Close |
|--------|---------------|-------------------|----------|---------|-------|
| Chromium | 6.8 ms | 4.5 ms | 2.4 ms | 10.3 ms | 12.2 ms |
| Firefox | 46.3 ms | 38.5 ms | 22.4 ms | 59.7 ms | 20.8 ms |

All recorded interactions were below the strict 100 ms budget in both release
engines. The focused run covered `windowing.spec.ts`, `window-close.spec.ts`,
and `desktop-taskbar-controls.spec.ts` in both engines (6/6 passing). It is
retained as history and is superseded for release qualification by the
canonical record above.

## Supplemental native microbenchmark

The canonical run of
`cargo run --locked --release -p integration-tests --bin input-latency`
reported:

```json
{"metric":"input_to_pixel_p50_us","value":2,"budget":100000,"iterations":1000,"pass":true}
{"metric":"input_to_pixel_p95_us","value":2,"budget":100000,"iterations":1000,"pass":true}
{"metric":"input_to_pixel_p99_us","value":2,"budget":100000,"iterations":1000,"pass":true}
{"metric":"input_to_pixel_mean_us","value":2,"budget":100000,"iterations":1000,"pass":true}
```

| Metric                    | Measured | Budget | Margin |
|---------------------------|----------|--------|--------|
| input_to_pixel_p50_us     | 2        | 100000 | 50000× |
| input_to_pixel_p95_us     | 2        | 100000 | 50000× |
| input_to_pixel_p99_us     | 2        | 100000 | 50000× |
| input_to_pixel_mean_us    | 2        | 100000 | 50000× |

This is 1,000 iterations of synthetic input → 64×64 ARGB tile composition
→ native sink, timed with `Instant::now()`. It is useful for detecting a
regression in that narrow Rust path, but its 2 µs p95 is not release evidence
for browser input, Worker scheduling, SAB transport, compositor work, or canvas
presentation.

## Release-gate status

| Budget | Evidence status |
|--------|-----------------|
| Cold load `< 10 s` | **PASS** — 2029 ms Chromium; 2119 ms Firefox. |
| Warm load `< 3 s` | **PASS** — 611 ms Chromium; 1488 ms Firefox. |
| Keystroke-to-pixel p95 `< 100 ms` | **PASS** — 3.3 ms Chromium; 33.7 ms Firefox. |
| Window/app interaction `< 100 ms` | **PASS** — all canonical taskbar samples and six-app p95 values pass in both engines. |
| Restored warm `< 3 s` and restored input p95 `< 100 ms` | **PASS** — 504.5 ms / 3.8 ms Chromium; 1105.4 ms / 36.1 ms Firefox. |
| Incremental idle CPU `<= 2%` of one core | **PASS** — all 12 comparisons pass across four browser runs. |

## Reproducing

```bash
just test-integration   # authoritative full-stack browser gates
just test-perf          # supplemental native 64x64 sink microbenchmark
```

## Notes

- The synthetic perf harness (T220) intentionally does not exercise the full
  WASM/Worker/SAB stack and cannot qualify perceived latency by itself.
- Re-record this file when a release branch cuts; reference the
  numbers in the release notes alongside the layering test
  artifact.
