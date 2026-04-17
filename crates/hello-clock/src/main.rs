//! End-to-end exercise of the CLOCK_TIME_GET WASI shim.
//!
//! `std::time::Instant::now()` lowers to
//! `wasi_snapshot_preview1::clock_time_get(MONOTONIC, ...)`, and
//! `std::time::SystemTime::now()` lowers to
//! `clock_time_get(REALTIME, ...)`. If either shim is missing or
//! returns garbage, this binary either fails to instantiate
//! (missing import → LinkError), panics (assert), or prints a
//! different message than the test expects.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn main() {
    // Monotonic: two back-to-back reads must be non-decreasing.
    // PMos's `Platform::now_ns` is strictly increasing, but WASI's
    // `Instant` collapses sub-nanosecond ticks, so `>=` is the
    // right relation at the std level.
    let a = Instant::now();
    let b = Instant::now();
    assert!(b >= a, "Instant::now() regressed: {:?} -> {:?}", a, b);
    println!("hello-clock monotonic ok");

    // Wall clock: any time after 2020-01-01 UTC proves the
    // `nowRealtimeNs` host import is wired to the real JS-side
    // clock (`Date.now()` in production, a fixed injected value
    // in tests). The 1.577e9 second bound is a stable "sometime
    // in the 21st century" check that is still valid decades
    // from now.
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock is before Unix epoch");
    assert!(
        epoch.as_secs() > 1_577_836_800,
        "wall clock before 2020: {:?}",
        epoch,
    );
    println!("hello-clock realtime ok");
}
