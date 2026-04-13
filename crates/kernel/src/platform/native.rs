//! Native-target `Platform` implementation used by `cargo test`.
//!
//! Provides an in-process stand-in for every browser capability the
//! kernel would otherwise reach through JS. The goal is that kernel
//! isolation tests run end-to-end on the developer's machine with
//! zero browser involvement — `std::time::Instant` for the clock,
//! a mock driver registry for driver_call, `rand::random` for
//! random bytes, `panic!()` for halt.

use core::panic::PanicInfo;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::{DevId, DriverError, DriverResult, Platform};

/// A recorded driver invocation, kept by the mock driver registry so
/// tests can assert on what the kernel asked drivers to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverCall {
    pub dev: DevId,
    pub op: u32,
    pub args: Vec<u8>,
}

/// Shared mutable state of the native platform. Tests can reset()
/// between runs to get a clean slate.
pub struct NativeState {
    pub start: Instant,
    pub driver_calls: Vec<DriverCall>,
    /// Programmable responses: maps (DevId, op) -> canned reply.
    pub canned_responses: std::collections::HashMap<(u8, u32), u32>,
    pub last_ns: u64,
    pub halted: Option<String>,
    pub panics: Vec<String>,
}

impl Default for NativeState {
    fn default() -> Self {
        NativeState {
            start: Instant::now(),
            driver_calls: Vec::new(),
            canned_responses: std::collections::HashMap::new(),
            last_ns: 0,
            halted: None,
            panics: Vec::new(),
        }
    }
}

/// The singleton NativePlatform instance.
pub struct NativePlatform {
    state: OnceLock<Mutex<NativeState>>,
}

/// Test helper: borrow the state for mutation (installing canned
/// responses, clearing driver-call history, reading recorded panics).
///
/// Only available under `cfg(feature = "native-platform")`.
pub fn with_state<R>(f: impl FnOnce(&mut NativeState) -> R) -> R {
    let state = NATIVE_PLATFORM.get_or_install_state();
    let mut guard = state.lock().expect("NativeState mutex poisoned");
    f(&mut guard)
}

/// Test helper: reset the state back to defaults.
pub fn reset() {
    with_state(|s| *s = NativeState::default());
}

impl NativePlatform {
    pub(crate) const fn new() -> Self {
        NativePlatform {
            state: OnceLock::new(),
        }
    }

    pub(crate) fn get_or_install(&'static self) -> &'static dyn Platform {
        self.get_or_install_state();
        self as &'static dyn Platform
    }

    pub(crate) fn get_or_install_state(&self) -> &Mutex<NativeState> {
        self.state.get_or_init(|| Mutex::new(NativeState::default()))
    }
}

pub static NATIVE_PLATFORM: NativePlatform = NativePlatform::new();

impl Platform for NativePlatform {
    fn now_ns(&self) -> u64 {
        let state = self.get_or_install_state();
        let mut guard = state.lock().unwrap();
        let elapsed = guard.start.elapsed();
        let mut ns = elapsed.as_secs().saturating_mul(1_000_000_000) + u64::from(elapsed.subsec_nanos());
        // Strict monotonicity: even two back-to-back calls within the
        // same nanosecond must produce distinct values.
        if ns <= guard.last_ns {
            ns = guard.last_ns + 1;
        }
        guard.last_ns = ns;
        ns
    }

    fn driver_call(&self, dev: DevId, op: u32, args: &[u8]) -> DriverResult<u32> {
        let state = self.get_or_install_state();
        let mut guard = state.lock().unwrap();
        guard.driver_calls.push(DriverCall {
            dev,
            op,
            args: args.to_vec(),
        });
        match guard.canned_responses.get(&(dev as u8, op)).copied() {
            Some(v) => Ok(v),
            // Default: succeed with result 0.
            None => Ok(0),
        }
    }

    fn random_bytes(&self, out: &mut [u8]) {
        // Tests do not need cryptographic randomness; a deterministic
        // xorshift seeded from the nanosecond clock is fine and is
        // reproducible when callers reset() the platform first.
        let mut seed: u64 = self.now_ns().wrapping_mul(0x9E37_79B9_7F4A_7C15);
        for byte in out.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *byte = (seed & 0xFF) as u8;
        }
    }

    fn halt(&self, reason: &str) -> ! {
        let state = self.get_or_install_state();
        // Record the halt before panicking so tests can inspect it
        // from a catch_unwind harness.
        {
            let mut guard = state.lock().unwrap();
            guard.halted = Some(reason.to_string());
        }
        panic!("NativePlatform::halt(\"{}\")", reason);
    }

    fn on_panic(&self, info: &PanicInfo) {
        let state = self.get_or_install_state();
        let mut guard = state.lock().unwrap();
        guard.panics.push(format!("{info}"));
    }
}
