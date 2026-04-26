//! Load-average tracking — `/proc/loadavg`'s 1/5/15-minute moving
//! averages of the kernel's running + ready process count.
//!
//! Linux's classic CALC_LOAD algorithm: at a fixed 5-second
//! sampling interval, multiply each running average by an
//! exp(-5/tau) decay factor and fold in the current sample
//! weighted by the complementary `(1 - decay)` factor. The three
//! decay factors target tau values of 1, 5, and 15 minutes:
//!
//! ```text
//! EXP_1  = exp(-5/60)  ≈ 0.92013
//! EXP_5  = exp(-5/300) ≈ 0.98349
//! EXP_15 = exp(-5/900) ≈ 0.99463
//! ```
//!
//! PMos uses Linux's fixed-point representation (FIXED_1 = 1<<11
//! = 2048) so the no_std kernel does not need `f64::exp` or any
//! libm dependency. Values are stored as `load * 2048` u64 and
//! scaled back to floats only at format time.
//!
//! ## Sampling discipline
//!
//! The kernel is event-driven: there is no preemptive scheduler
//! tick to hang a "sample every 5 seconds" timer off. The
//! averages are updated lazily — every call to [`LoadAverages::tick`]
//! checks whether at least one 5-second interval has elapsed
//! since the last sample, and if so applies one EMA step per
//! elapsed interval using the current runnable count. A long
//! gap between ticks (e.g. user idled the tab for 10 minutes
//! between syscalls) catches up by applying many decay steps in
//! one go, all using the *current* runnable count — the past is
//! lost, but the model converges to the "true" average within a
//! few sample intervals once activity resumes.
//!
//! The "runnable count" the kernel feeds in is `ProcState::Running`
//! plus `ProcState::Ready` — running processes plus those queued
//! waiting for CPU. This matches the Linux semantics of "how
//! many processes are competing for the CPU" rather than the
//! narrower "how many are actively executing right now."
//!
//! ## When ticks happen
//!
//! Every read of `/proc/loadavg` calls `tick` first so the
//! returned bytes reflect the current activity, even if the
//! kernel has been quiescent for the whole sample window. The
//! [`LiveProcFsSource::loadavg`](crate::wasm_entry) hook in
//! `wasm_entry.rs` is the only production caller. Native tests
//! drive `tick` directly to verify decay arithmetic without a
//! real kernel.

/// Linux fixed-point scale: `load_value * FIXED_1` is what we
/// store in u64 fields. Exact value matches the kernel's
/// `FSHIFT = 11` definition.
pub const FIXED_1: u64 = 1 << 11;

/// Decay constant for the 1-minute average — exp(-5/60), scaled
/// to FIXED_1. 1884/2048 = 0.9199...; matches Linux's
/// `EXP_1 = 1884`.
pub const EXP_1: u64 = 1884;
/// Decay constant for the 5-minute average — exp(-5/300).
pub const EXP_5: u64 = 2014;
/// Decay constant for the 15-minute average — exp(-5/900).
pub const EXP_15: u64 = 2037;

/// Sampling interval in nanoseconds. Linux uses 5 seconds; we
/// match. The interval is the unit of the decay constants
/// above; changing it without recomputing EXP_* would skew the
/// time-window semantics.
pub const SAMPLE_INTERVAL_NS: u64 = 5_000_000_000;

/// 1/5/15-minute moving averages of the runnable-process count.
///
/// All three internal counters are stored as `load * FIXED_1`
/// (Linux's classic CALC_LOAD fixed-point format) so the kernel
/// can compute decay arithmetic without a floating-point
/// dependency. Use [`LoadAverages::current_fixed`] to read the
/// raw fixed-point values, or [`LoadAverages::format_three`] to
/// get the `"x.xx y.yy z.zz"` text the procfs source serves.
#[derive(Copy, Clone, Debug)]
pub struct LoadAverages {
    load_1m: u64,
    load_5m: u64,
    load_15m: u64,
    /// Monotonic-clock timestamp (ns) at which the last sample
    /// fired. Updated each time at least one full
    /// [`SAMPLE_INTERVAL_NS`] has elapsed; the residual elapsed
    /// time below the interval threshold is discarded so the
    /// sample boundary stays aligned to real time.
    last_sample_ns: u64,
    /// Whether `tick` has ever been called. The first tick seeds
    /// `last_sample_ns` from the caller's `now_ns` instead of
    /// applying a decay step against an undefined elapsed time.
    seeded: bool,
}

impl LoadAverages {
    pub const fn new() -> Self {
        LoadAverages {
            load_1m: 0,
            load_5m: 0,
            load_15m: 0,
            last_sample_ns: 0,
            seeded: false,
        }
    }

    /// Apply one or more EMA steps if at least one full sample
    /// interval has elapsed since the last sample. `runnable` is
    /// the count of processes currently in `Running` or `Ready`
    /// state; the kernel folds this into all three averages
    /// using the appropriate decay constants.
    ///
    /// The first ever call seeds `last_sample_ns` from `now_ns`
    /// without applying any decay (no past to average). Time
    /// going backwards (a non-monotonic clock — should never
    /// happen with `Platform::now_ns`, but defence in depth) is
    /// treated as "no time elapsed" and the call is a no-op.
    pub fn tick(&mut self, now_ns: u64, runnable: u32) {
        self.tick_with_interval(now_ns, runnable, SAMPLE_INTERVAL_NS)
    }

    /// Variant of [`tick`] with a configurable sample interval —
    /// only useful for tests so the EMA arithmetic can be
    /// exercised over compressed timescales without waiting 5
    /// seconds between updates. Production code calls [`tick`]
    /// which uses the Linux-compatible 5-second interval.
    pub fn tick_with_interval(&mut self, now_ns: u64, runnable: u32, interval_ns: u64) {
        if !self.seeded {
            self.last_sample_ns = now_ns;
            self.seeded = true;
            return;
        }
        if now_ns < self.last_sample_ns {
            return;
        }
        let elapsed = now_ns - self.last_sample_ns;
        if elapsed < interval_ns {
            return;
        }
        let steps_u64 = elapsed / interval_ns;
        // Cap step count so a 100-year idle gap doesn't produce a
        // CPU-bound catch-up loop. After ~30 steps every load is
        // numerically zero (EXP_1^30 ≈ 0.08, EXP_15^30 ≈ 0.85);
        // capping at 64 is a comfortable bound that includes the
        // "load_15m has effectively converged to the new sample"
        // threshold for any realistic activity pattern.
        let steps: u32 = if steps_u64 > 64 { 64 } else { steps_u64 as u32 };
        let n = u64::from(runnable) * FIXED_1;
        for _ in 0..steps {
            self.load_1m = decay_step(self.load_1m, n, EXP_1);
            self.load_5m = decay_step(self.load_5m, n, EXP_5);
            self.load_15m = decay_step(self.load_15m, n, EXP_15);
        }
        self.last_sample_ns = self
            .last_sample_ns
            .saturating_add(interval_ns.saturating_mul(steps_u64));
    }

    /// Raw fixed-point values for the three averages. Mainly for
    /// unit tests that want to assert exact post-step values.
    pub fn current_fixed(&self) -> (u64, u64, u64) {
        (self.load_1m, self.load_5m, self.load_15m)
    }

    /// Human-readable `"<1m> <5m> <15m>"` with two decimal
    /// places per value. The caller (`/proc/loadavg`) appends
    /// the `<running>/<total> <last_pid>\n` suffix.
    pub fn format_three(&self) -> alloc::string::String {
        use alloc::format;
        format!(
            "{} {} {}",
            format_fixed(self.load_1m),
            format_fixed(self.load_5m),
            format_fixed(self.load_15m)
        )
    }

    /// Whether the seeded-on-first-tick flag has been set.
    /// Tests can read this to confirm seeding semantics.
    pub fn seeded(&self) -> bool {
        self.seeded
    }
}

impl Default for LoadAverages {
    fn default() -> Self {
        LoadAverages::new()
    }
}

/// One CALC_LOAD step: `load = (load * exp + n * (FIXED_1 - exp)) / FIXED_1`.
/// All inputs are in FIXED_1 scale; the result is the new load
/// in the same scale. Saturating math throughout — a bounded
/// runnable count and bounded decay factor cannot realistically
/// overflow u64 (peak intermediate ≈ FIXED_1 * (FIXED_1 + max_n)
/// which fits easily in u64 for any plausible system size), but
/// `saturating_*` makes the arithmetic robust against future
/// changes to the FIXED_1 / EXP_* constants.
#[inline]
fn decay_step(load: u64, n: u64, exp: u64) -> u64 {
    let decayed = load.saturating_mul(exp);
    let added = n.saturating_mul(FIXED_1.saturating_sub(exp));
    let sum = decayed.saturating_add(added);
    sum / FIXED_1
}

/// Format one fixed-point load as `"X.YY"` with two decimal
/// places. Rounding: `centi = round(load * 100 / FIXED_1)`,
/// computed via integer math so a 0.005 boundary lands on the
/// next centi — e.g. 0.495 → "0.50" not "0.49".
fn format_fixed(load: u64) -> alloc::string::String {
    use alloc::format;
    // Add half a unit (FIXED_1 / 2 / 100) for round-to-nearest;
    // FIXED_1 = 2048 so `+ 1024 / 100` would lose precision —
    // multiply through first.
    let centi = load.saturating_mul(100).saturating_add(FIXED_1 / 2) / FIXED_1;
    let whole = centi / 100;
    let frac = centi % 100;
    format!("{}.{:02}", whole, frac)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(seconds: u64) -> u64 {
        seconds * SAMPLE_INTERVAL_NS / 5
    }

    #[test]
    fn fresh_load_averages_are_all_zero() {
        let la = LoadAverages::new();
        assert_eq!(la.current_fixed(), (0, 0, 0));
        assert_eq!(la.format_three(), "0.00 0.00 0.00");
        assert!(!la.seeded());
    }

    #[test]
    fn first_tick_seeds_without_applying_decay() {
        let mut la = LoadAverages::new();
        la.tick(ns(100), 5);
        // No decay step happened — the 5-runnable sample is NOT
        // in the average yet because there was no prior sample
        // boundary to fold from.
        assert_eq!(la.current_fixed(), (0, 0, 0));
        assert!(la.seeded());
    }

    #[test]
    fn second_tick_after_one_interval_folds_in_one_sample() {
        let mut la = LoadAverages::new();
        la.tick(ns(0), 0);
        la.tick(ns(5), 2);
        // load_1m = 0 * 1884 / 2048 + 2 * 2048 * (2048 - 1884) / 2048
        //         = 0 + (4096 * 164) / 2048
        //         = 671744 / 2048
        //         = 328
        let (l1, l5, l15) = la.current_fixed();
        assert_eq!(l1, 328);
        // load_5m = 2 * 2048 * (2048 - 2014) / 2048 = 4096 * 34 / 2048 = 68
        assert_eq!(l5, 68);
        // load_15m = 2 * 2048 * (2048 - 2037) / 2048 = 22
        assert_eq!(l15, 22);
    }

    #[test]
    fn tick_within_sample_interval_is_a_noop() {
        let mut la = LoadAverages::new();
        la.tick(ns(0), 0);
        la.tick(ns(2), 100); // < 5s elapsed
        assert_eq!(la.current_fixed(), (0, 0, 0));
    }

    #[test]
    fn many_ticks_at_constant_load_converge_toward_the_load() {
        let mut la = LoadAverages::new();
        la.tick(ns(0), 0);
        for i in 1..=200 {
            la.tick(ns(5 * i), 4);
        }
        // After 200 sample steps with runnable=4, the three
        // averages converge at different rates per their decay
        // constants. 200 steps = 1000 seconds:
        //   1m  EMA: decay^200 ≈ 1e-7 → load ≈ target (< 0.1%).
        //   5m  EMA: 0.9834^200 ≈ 0.035 → load ≈ 0.965 * target.
        //   15m EMA: 0.9946^200 ≈ 0.339 → load ≈ 0.661 * target.
        let (l1, l5, l15) = la.current_fixed();
        let target = 4 * FIXED_1; // 8192
        assert!(l1.abs_diff(target) < target / 100, "1m off: {}", l1);
        // 5m: within 5% (decay^200 ≈ 0.035).
        assert!(l5.abs_diff(target) < target / 20, "5m off: {}", l5);
        // 15m: within 40% (decay^200 ≈ 0.34, so still ~34% off).
        assert!(l15.abs_diff(target) < target * 2 / 5, "15m off: {}", l15);
    }

    #[test]
    fn load_decays_to_zero_when_runnable_drops() {
        let mut la = LoadAverages::new();
        la.tick(ns(0), 0);
        // Climb to 4 runnable.
        for i in 1..=200 {
            la.tick(ns(5 * i), 4);
        }
        // Now drop runnable to 0 and let the 1m EMA decay.
        for i in 201..=240 {
            la.tick(ns(5 * i), 0);
        }
        let (l1, _, _) = la.current_fixed();
        // After 40 5-second intervals = 200 seconds = 3.3 1m
        // windows of zero load, the 1m EMA should be very near
        // zero. EXP_1^40 ≈ 0.038; starting from 4.0 → ~0.15.
        assert!(l1 < FIXED_1 / 2, "1m did not decay: {} (expect < 1024)", l1);
    }

    #[test]
    fn long_idle_gap_caps_at_64_steps() {
        let mut la = LoadAverages::new();
        la.tick(ns(0), 0);
        // 10 hours of idle elapsed before the next tick. The
        // step cap should keep this O(64) rather than
        // O(36000/5) = O(7200) iterations.
        la.tick(ns(36000), 1);
        // After 64 steps with runnable=1, the 15m EMA is at
        // about 1 - EXP_15^64 = 1 - 0.9946^64 ≈ 0.293.
        // 0.293 * 2048 ≈ 600. Wide range to be tolerant.
        let (l1, l5, l15) = la.current_fixed();
        assert!(l1 > FIXED_1 - 100); // 1m basically saturated.
        assert!(l5 > FIXED_1 / 2);   // 5m well past halfway.
        assert!(l15 > 200 && l15 < 1000); // 15m partway up.
    }

    #[test]
    fn time_going_backwards_is_a_noop() {
        let mut la = LoadAverages::new();
        la.tick(ns(100), 0);
        la.tick(ns(50), 99); // earlier than seed
        assert_eq!(la.current_fixed(), (0, 0, 0));
    }

    #[test]
    fn format_three_emits_two_decimals() {
        let mut la = LoadAverages::new();
        la.tick(ns(0), 0);
        la.tick(ns(5), 2);
        // l1 = 328 (above) → 328 * 100 / 2048 = 32000/2048 = 16
        // → "0.16" with rounding nudge.
        let s = la.format_three();
        let parts: alloc::vec::Vec<&str> = s.split(' ').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "0.16");
        assert_eq!(parts[1], "0.03"); // 68 * 100 / 2048 ≈ 3.32 → 0.03
        assert_eq!(parts[2], "0.01"); // 22 * 100 / 2048 ≈ 1.07 → 0.01
    }

    #[test]
    fn format_fixed_handles_integer_loads() {
        assert_eq!(format_fixed(0), "0.00");
        assert_eq!(format_fixed(FIXED_1), "1.00");
        assert_eq!(format_fixed(2 * FIXED_1), "2.00");
        assert_eq!(format_fixed(FIXED_1 / 2), "0.50");
        assert_eq!(format_fixed(FIXED_1 / 4), "0.25");
    }

    #[test]
    fn format_fixed_rounds_half_up() {
        // 0.005 boundary in fixed-point = FIXED_1 * 0.005 = 10.24.
        // load_fp = 10 (just below) → 0.00 (rounds down).
        assert_eq!(format_fixed(10), "0.00");
        // load_fp = 11 (just above) → 0.01 (rounds up).
        assert_eq!(format_fixed(11), "0.01");
    }

    #[test]
    fn tick_with_interval_test_seam() {
        // Use a 1ns interval so we can drive many EMA steps from
        // tightly-spaced timestamps in tests.
        let mut la = LoadAverages::new();
        la.tick_with_interval(0, 0, 1);
        for i in 1..=200 {
            la.tick_with_interval(i, 4, 1);
        }
        // The same convergence-to-4 behaviour applies because
        // the EXP_* constants are still tied to "5s per step";
        // the test seam decouples wall-clock from step count.
        let (l1, _, _) = la.current_fixed();
        assert!(l1.abs_diff(4 * FIXED_1) < FIXED_1 / 50);
    }
}
