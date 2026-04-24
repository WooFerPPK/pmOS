//! `input-latency` — T220 Principle IX input-to-pixel perf gate.
//!
//! # What cycle this harness measures
//!
//! Each iteration times a **native-Rust synthetic proxy** of a
//! single input-to-pixel cycle:
//!   1. push a synthetic `InputEvent` struct into an in-process
//!      `VecDeque` (stands in for the kernel input queue),
//!   2. pop it on the "handler" side,
//!   3. format a 64x64 ARGB8888 tile (4 KiB) that represents the
//!      pixels the compositor would blit for that frame,
//!   4. write the tile to `std::io::sink()` so the allocation and
//!      write-call cost is real, not DCE'd away.
//!
//! `Instant::now()` wraps the whole cycle; the elapsed
//! microseconds are collected, sorted, and summarised as p50 /
//! p95 / p99 / mean.
//!
//! # Budget
//!
//! Principle IX caps input-to-pixel at **p95 <= 100 ms**
//! (`100_000` us). The binary exits 0 iff measured p95 is within
//! budget, 1 otherwise. Pass/fail is pinned to p95 only; p50, p99
//! and mean are reported for observability but do not gate CI.
//!
//! # Honest scope — what this synthetic proxy approximates
//!
//! The proxy covers the in-process subset of the real path:
//! queue hand-off + pixel formatting + sink write. It is a
//! preliminary gate so CI has Principle-IX evidence today, not a
//! faithful reproduction of the browser-stack latency.
//!
//! # Explicitly NOT measured (deferred to a Playwright-based
//! end-to-end harness in a later slice):
//!
//!   * browser paint / compositor latency (rAF + GPU present),
//!   * `OffscreenCanvas.putImageData` copy cost,
//!   * WebWorker `postMessage` / structured-clone overhead,
//!   * SharedArrayBuffer `Atomics.wait` wake latency,
//!   * OPFS / service-worker scheduling jitter,
//!   * real kernel syscall dispatch through WASI + PMos ext.
//!
//! When the end-to-end harness lands, this binary stays as the
//! native-Rust lower bound; the two numbers bracket the budget.

use std::collections::VecDeque;
use std::io::Write;
use std::process::ExitCode;
use std::time::Instant;

use integration_tests::percentile_us;

const DEFAULT_ITERATIONS: u32 = 1000;
const BUDGET_US: u64 = 100_000;
const TILE_EDGE: usize = 64;
const TILE_BYTES: usize = TILE_EDGE * TILE_EDGE * 4;

#[derive(Clone, Copy)]
struct InputEvent {
    kind: u8,
    code: u32,
    ts_us: u64,
}

fn run_cycle(seq: u32, queue: &mut VecDeque<InputEvent>, sink: &mut dyn Write) {
    queue.push_back(InputEvent {
        kind: 1,
        code: seq,
        ts_us: seq as u64,
    });
    let ev = queue.pop_front().expect("queue drained unexpectedly");
    let mut tile = vec![0u8; TILE_BYTES];
    for (i, px) in tile.chunks_exact_mut(4).enumerate() {
        px[0] = (ev.code as u8).wrapping_add(i as u8);
        px[1] = (ev.ts_us as u8).wrapping_add(i as u8);
        px[2] = ev.kind;
        px[3] = 0xff;
    }
    sink.write_all(&tile).expect("sink write failed");
}

fn parse_iterations(args: &[String]) -> Result<u32, String> {
    let mut i = 0;
    let mut iterations = DEFAULT_ITERATIONS;
    while i < args.len() {
        match args[i].as_str() {
            "--iterations" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| "missing value for --iterations".to_string())?;
                let n: u32 = v
                    .parse()
                    .map_err(|_| format!("--iterations: not a u32: {v}"))?;
                if n == 0 {
                    return Err("--iterations must be > 0".to_string());
                }
                iterations = n;
                i += 2;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(iterations)
}

fn emit(metric: &str, value: u64, iterations: u32) {
    let pass = value <= BUDGET_US;
    println!(
        "{{\"metric\":\"{metric}\",\"value\":{value},\"budget\":{BUDGET_US},\"iterations\":{iterations},\"pass\":{pass}}}"
    );
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let iterations = match parse_iterations(&argv) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("input-latency: {e}");
            eprintln!("usage: input-latency [--iterations N]  (N: positive u32, default 1000)");
            return ExitCode::from(2);
        }
    };

    let mut samples: Vec<u64> = Vec::with_capacity(iterations as usize);
    let mut queue: VecDeque<InputEvent> = VecDeque::with_capacity(4);
    let mut sink = std::io::sink();

    for seq in 0..iterations {
        let t0 = Instant::now();
        run_cycle(seq, &mut queue, &mut sink);
        samples.push(t0.elapsed().as_micros() as u64);
    }

    let p50 = percentile_us(&samples, 50);
    let p95 = percentile_us(&samples, 95);
    let p99 = percentile_us(&samples, 99);
    let mean: u64 = samples.iter().sum::<u64>() / samples.len() as u64;

    emit("input_to_pixel_p50_us", p50, iterations);
    emit("input_to_pixel_p95_us", p95, iterations);
    emit("input_to_pixel_p99_us", p99, iterations);
    emit("input_to_pixel_mean_us", mean, iterations);

    if p95 <= BUDGET_US {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
