//! Shell-respawn rate limiter — keeps init from busy-spawning a
//! crashing shell. Per the contract (init-conf.md §3.2): "respawn
//! the shell binary once per 1 second." That bound exists so a
//! shell that segfaults on every startup can't burn CPU + log
//! buffer space at thousands of respawns per second.
//!
//! The limiter is time-source agnostic so unit tests can drive
//! the wall clock from a fake source.

/// Decision returned by [`RespawnLimiter::should_respawn`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RespawnDecision {
    /// Spawn the shell now.
    Spawn,
    /// Wait until at least `wait_ns` more nanoseconds have elapsed
    /// before the next respawn is permitted.
    Wait { wait_ns: u64 },
}

/// 1-second minimum between respawns, matching the contract.
pub const MIN_INTERVAL_NS: u64 = 1_000_000_000;

/// Time-source–agnostic 1-second-window respawn limiter.
///
/// `last_respawn_ns` tracks the monotonic timestamp of the most
/// recent permitted respawn. The first call always returns
/// `Spawn`; every subsequent call returns `Wait` until at least
/// `MIN_INTERVAL_NS` has elapsed since `last_respawn_ns`.
#[derive(Debug, Clone)]
pub struct RespawnLimiter {
    last_respawn_ns: Option<u64>,
}

impl RespawnLimiter {
    pub const fn new() -> Self {
        RespawnLimiter {
            last_respawn_ns: None,
        }
    }

    /// Ask whether a respawn is permitted at `now_ns`. On
    /// `Spawn`, the caller MUST call [`record_spawn`] after the
    /// spawn succeeds so the next decision sees the right
    /// timestamp.
    pub fn should_respawn(&self, now_ns: u64) -> RespawnDecision {
        match self.last_respawn_ns {
            None => RespawnDecision::Spawn,
            Some(last) => {
                let elapsed = now_ns.saturating_sub(last);
                if elapsed >= MIN_INTERVAL_NS {
                    RespawnDecision::Spawn
                } else {
                    RespawnDecision::Wait {
                        wait_ns: MIN_INTERVAL_NS - elapsed,
                    }
                }
            }
        }
    }

    /// Mark a successful respawn at `now_ns`. Subsequent calls to
    /// `should_respawn` will return `Wait` until 1 second later.
    pub fn record_spawn(&mut self, now_ns: u64) {
        self.last_respawn_ns = Some(now_ns);
    }
}

impl Default for RespawnLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_limiter_permits_the_first_spawn() {
        let r = RespawnLimiter::new();
        assert_eq!(r.should_respawn(0), RespawnDecision::Spawn);
    }

    #[test]
    fn second_call_within_interval_returns_wait_with_remaining_ns() {
        let mut r = RespawnLimiter::new();
        r.record_spawn(1_000_000_000);
        // 0.5 seconds later — half the interval remaining.
        let d = r.should_respawn(1_500_000_000);
        assert_eq!(
            d,
            RespawnDecision::Wait {
                wait_ns: 500_000_000
            }
        );
    }

    #[test]
    fn second_call_after_full_interval_permits_spawn() {
        let mut r = RespawnLimiter::new();
        r.record_spawn(0);
        let d = r.should_respawn(MIN_INTERVAL_NS);
        assert_eq!(d, RespawnDecision::Spawn);
    }

    #[test]
    fn spawn_record_advances_the_window() {
        let mut r = RespawnLimiter::new();
        r.record_spawn(0);
        r.record_spawn(MIN_INTERVAL_NS);
        // After the second spawn at t=1s, t=1.4s should still be Wait.
        let d = r.should_respawn(MIN_INTERVAL_NS + 400_000_000);
        assert_eq!(
            d,
            RespawnDecision::Wait {
                wait_ns: 600_000_000
            }
        );
    }

    #[test]
    fn time_going_backwards_still_returns_wait() {
        let mut r = RespawnLimiter::new();
        r.record_spawn(1_000_000_000);
        // Saturating subtraction keeps elapsed = 0; wait ≈ full interval.
        let d = r.should_respawn(500_000_000);
        assert_eq!(
            d,
            RespawnDecision::Wait {
                wait_ns: MIN_INTERVAL_NS,
            }
        );
    }

    #[test]
    fn one_thousand_attempts_within_a_second_grant_exactly_one() {
        let mut r = RespawnLimiter::new();
        let mut spawns = 0;
        for i in 0..1000u64 {
            // 1ms apart — total 999ms, all inside the 1-second window.
            let now = i * 1_000_000;
            if r.should_respawn(now) == RespawnDecision::Spawn {
                spawns += 1;
                r.record_spawn(now);
            }
        }
        assert_eq!(spawns, 1);
    }

    #[test]
    fn ten_attempts_each_one_second_apart_grant_all_ten() {
        let mut r = RespawnLimiter::new();
        let mut spawns = 0;
        for i in 0..10u64 {
            let now = i * MIN_INTERVAL_NS;
            if r.should_respawn(now) == RespawnDecision::Spawn {
                spawns += 1;
                r.record_spawn(now);
            }
        }
        assert_eq!(spawns, 10);
    }
}
