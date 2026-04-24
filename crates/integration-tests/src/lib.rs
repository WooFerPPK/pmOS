//! PMos native-Rust integration test harness.
//!
//! This crate hosts full-stack tests that need timing precision
//! the Playwright JS layer cannot provide. The perf/input-latency
//! harness (T220, Principle IX gate) lives under src/bin/ and is
//! exercised by tests/input_latency.rs.
//!
//! Host-target only. Never compiled for wasm.

/// Sorted-vec percentile helper shared between the input-latency
/// binary and its unit test. The caller is responsible for passing
/// a non-empty slice; with `pct` in 0..=99 the returned sample is
/// `samples_sorted[(N * pct) / 100]`, and for `pct == 50` the
/// formula reduces to `samples_sorted[N / 2]` as documented in
/// T220.
///
/// This is deliberately the simplest honest thing: a sort plus
/// an integer-divided index. It matches the ranking reported on
/// the stdout JSON lines so test and binary never disagree.
#[must_use]
pub fn percentile_us(samples: &[u64], pct: u32) -> u64 {
    assert!(!samples.is_empty(), "percentile_us: empty sample slice");
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let idx = if pct >= 100 {
        n - 1
    } else if pct == 50 {
        n / 2
    } else {
        (n * pct as usize) / 100
    };
    sorted[idx.min(n - 1)]
}

#[cfg(test)]
mod tests {
    use super::percentile_us;

    #[test]
    fn percentile_us_matches_documented_formula() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile_us(&v, 50), 51);
        assert_eq!(percentile_us(&v, 95), 96);
        assert_eq!(percentile_us(&v, 99), 100);
    }
}
