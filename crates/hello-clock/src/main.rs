//! End-to-end exercise of the CLOCK_TIME_GET + CLOCK_RES_GET WASI
//! shims.
//!
//! `std::time::Instant::now()` lowers to
//! `wasi_snapshot_preview1::clock_time_get(MONOTONIC, ...)`, and
//! `std::time::SystemTime::now()` lowers to
//! `clock_time_get(REALTIME, ...)`. If either shim is missing or
//! returns garbage, this binary either fails to instantiate
//! (missing import → LinkError), panics (assert), or prints a
//! different message than the test expects.
//!
//! The third probe exercises `clock_res_get` directly via a
//! `wasi_snapshot_preview1` extern block — std doesn't expose
//! `clock_getres` at the std level, so the binary does the FFI
//! itself. The probe is gated to `target_arch = "wasm32"` so the
//! native-target build (driven by `cargo build --workspace`)
//! still links cleanly.

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

    // Clock resolution: PMos's nanosecond-granular Platform clock
    // reports 1 ns for both MONOTONIC + REALTIME. A regression
    // that broke the CLOCK_RES_GET shim or the kernel handler
    // surfaces here as a LinkError at instantiation (shim
    // missing), a nonzero errno (handler broken), or a wrong
    // resolution value (a bug in the handler's match arms or in
    // the shim's bigint round-trip).
    #[cfg(target_arch = "wasm32")]
    {
        const CLOCKID_MONOTONIC: i32 = 1;
        const CLOCKID_REALTIME: i32 = 0;

        #[link(wasm_import_module = "wasi_snapshot_preview1")]
        extern "C" {
            fn clock_res_get(clock_id: i32, resolution_out: *mut u64) -> i32;
        }

        let mut res: u64 = 0;
        let rc = unsafe { clock_res_get(CLOCKID_MONOTONIC, &mut res as *mut u64) };
        assert_eq!(rc, 0, "clock_res_get(MONOTONIC) errno: {}", rc);
        assert_eq!(
            res, 1,
            "clock_res_get(MONOTONIC) resolution: {}, expected 1 ns",
            res,
        );

        let mut res_rt: u64 = 0;
        let rc_rt = unsafe { clock_res_get(CLOCKID_REALTIME, &mut res_rt as *mut u64) };
        assert_eq!(rc_rt, 0, "clock_res_get(REALTIME) errno: {}", rc_rt);
        assert_eq!(
            res_rt, 1,
            "clock_res_get(REALTIME) resolution: {}, expected 1 ns",
            res_rt,
        );
    }
    println!("hello-clock res ok");
}
