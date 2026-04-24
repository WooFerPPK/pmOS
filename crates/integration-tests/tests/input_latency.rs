//! T220 isolation tests for the `input-latency` perf harness.
//!
//! Three layers of coverage:
//!   1. percentile helper matches the documented sorted-index
//!      formula on a known input,
//!   2. the binary runs end-to-end under budget and emits the
//!      four JSON-per-line metrics specified by T220,
//!   3. the binary rejects bad CLI input with exit status 2.
//!
//! Kept native-only (no wasm) so this runs inside
//! `cargo test --workspace` on every developer push and in CI.

use std::process::Command;

use integration_tests::percentile_us;

#[test]
fn percentile_computation_is_correct() {
    let v: Vec<u64> = (1..=100).collect();
    // Formula: samples_sorted[N/2], samples_sorted[(N*95)/100],
    // samples_sorted[(N*99)/100]. For N=100 that's indices 50, 95,
    // 99 which in 1..=100 are values 51, 96, 100.
    assert_eq!(percentile_us(&v, 50), 51);
    assert_eq!(percentile_us(&v, 95), 96);
    assert_eq!(percentile_us(&v, 99), 100);
}

#[test]
fn binary_runs_and_exits_zero_under_budget() {
    let out = Command::new(env!("CARGO_BIN_EXE_input-latency"))
        .args(["--iterations", "100"])
        .output()
        .expect("spawn input-latency binary");
    assert!(
        out.status.success(),
        "expected exit 0; got {:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    for metric in [
        "input_to_pixel_p50_us",
        "input_to_pixel_p95_us",
        "input_to_pixel_p99_us",
        "input_to_pixel_mean_us",
    ] {
        assert!(
            stdout.contains(metric),
            "stdout missing metric {metric}: {stdout}"
        );
    }
    assert_eq!(stdout.lines().count(), 4, "expected 4 JSON lines: {stdout}");
}

#[test]
fn binary_rejects_bad_iterations_arg() {
    let out = Command::new(env!("CARGO_BIN_EXE_input-latency"))
        .args(["--iterations", "notanumber"])
        .output()
        .expect("spawn input-latency binary");
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 for bad arg; stderr={}",
        String::from_utf8_lossy(&out.stderr),
    );
}
