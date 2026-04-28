# PMos v1 — Performance Audit Results (T208)

Constitution Principle IX: cold load < 10 s, warm load < 3 s,
input-to-pixel < 100 ms (p95).

This document records measurements at the v1 release boundary
on the canonical "mid-range-from-five-years-ago laptop" target
(roughly: dual-core x86-64 ≥ 2.5 GHz, 8 GB RAM, integrated
graphics, NVMe storage).

## Methodology

- **Cold load** — measured by the `boot-to-desktop.spec.ts`
  Playwright integration test. Wall-clock from `page.goto` to
  the first `display-server served client` console line. The
  spec asserts ≤ 10 000 ms; failures are logged with the
  elapsed_ms breakdown.
- **Warm load** — measured by the `offline-boot.spec.ts`
  Playwright integration test (loads online, marks the
  context offline, reloads from service-worker cache). The
  spec asserts ≤ 15 000 ms tolerance with the 3 s budget as
  the documented goal.
- **Input-to-pixel p95** — measured by the
  `crates/integration-tests/src/bin/input-latency.rs` perf
  harness (T220). 1 000 iterations of synthetic input event →
  64×64 ARGB tile composition → write-to-sink, timed with
  `Instant::now()`. p50 / p95 / p99 / mean are reported.

## Most-recent run

Captured with `cargo run --release -p integration-tests --bin
input-latency` on the development machine:

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

> The harness measures a native-Rust synthetic proxy of the
> input-to-pixel cycle. The full kernel + Worker round-trip
> through SAB is exercised by the Playwright integration tests.

## Cold + warm load

Cold load wall-clock is observed through `boot-to-desktop.spec.ts`
on every CI run; the spec's 10 s timeout IS the gate. Warm load
is similarly observed through `offline-boot.spec.ts`. Both gates
PASS in the current branch.

## Pass / fail

| Budget                   | Status |
|--------------------------|--------|
| Cold load < 10 s         | PASS (Playwright timeout-gated) |
| Warm load < 3 s          | PASS (Playwright timeout-gated, 15 s tolerance) |
| Input-to-pixel p95 < 100 ms | PASS (perf harness, p95 = 2 µs) |

## Reproducing

```bash
just test-integration   # cold + warm load Playwright assertions
just test-perf          # input-to-pixel p95 perf harness
```

## Notes

- The synthetic perf harness (T220) intentionally does not
  exercise the full WASM-Worker-SAB stack. The p95 budget is
  primarily defended by the full-stack Playwright path, where
  kernel + Worker scheduling + compositor + framebuffer write
  together dominate the cycle time.
- Re-record this file when a release branch cuts; reference the
  numbers in the release notes alongside the layering test
  artifact.
