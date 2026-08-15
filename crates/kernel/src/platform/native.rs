//! Native-target `Platform` implementation used by `cargo test`.
//!
//! Provides an in-process stand-in for every browser capability the
//! kernel would otherwise reach through JS. The goal is that kernel
//! isolation tests run end-to-end on the developer's machine with
//! zero browser involvement — `std::time::Instant` for the clock,
//! a mock driver registry for driver_call, `rand::random` for
//! random bytes, `panic!()` for halt.

use core::panic::PanicInfo;
use std::cell::RefCell;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use abi::ext::Pid;

use super::{DevId, DriverResult, Platform};

/// A recorded driver invocation, kept by the mock driver registry so
/// tests can assert on what the kernel asked drivers to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverCall {
    pub dev: DevId,
    pub op: u32,
    pub args: Vec<u8>,
}

/// A recorded `spawn_process` request. NativePlatform appends one of
/// these every time the kernel asks for a Worker spawn; tests read
/// them out via [`with_state`] to assert on what was requested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnCall {
    pub pid: Pid,
    pub path: String,
    pub executable: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostDownloadCall {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Mutable state of the native platform for one native test thread.
/// Tests can reset() between runs to get a clean slate.
pub struct NativeState {
    pub start: Instant,
    pub driver_calls: Vec<DriverCall>,
    /// Programmable responses: maps (DevId, op) -> canned reply.
    pub canned_responses: std::collections::HashMap<(u8, u32), u32>,
    pub last_ns: u64,
    pub halted: Option<String>,
    pub panics: Vec<String>,
    /// Recorded `spawn_process` requests, in order.
    pub spawn_calls: Vec<SpawnCall>,
    /// Recorded `terminate_process` requests, in order.
    pub terminate_calls: Vec<Pid>,
    pub host_picker_calls: u32,
    pub host_download_calls: Vec<HostDownloadCall>,
    /// If `Some`, the next call to `spawn_process` returns this
    /// error and records nothing. Tests set this to exercise the
    /// rollback path in the `PROC_SPAWN` opcode handler.
    pub next_spawn_error: Option<super::DriverError>,
    pub next_host_picker_error: Option<super::DriverError>,
    pub next_host_download_error: Option<super::DriverError>,
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
            spawn_calls: Vec::new(),
            terminate_calls: Vec::new(),
            host_picker_calls: 0,
            host_download_calls: Vec::new(),
            next_spawn_error: None,
            next_host_picker_error: None,
            next_host_download_error: None,
        }
    }
}

// Rust's test runner executes tests concurrently on separate threads. Keeping
// fixture controls in thread-local storage prevents one test's reset or
// one-shot error injection from changing another test's platform state. This
// also matches the browser runtime's execution model: one kernel per Worker.
std::thread_local! {
    static NATIVE_STATE: RefCell<NativeState> = RefCell::new(NativeState::default());
}

/// The singleton NativePlatform instance.
pub struct NativePlatform;

/// Test helper: borrow the state for mutation (installing canned
/// responses, clearing driver-call history, reading recorded panics).
///
/// Only available under `cfg(feature = "native-platform")`.
pub fn with_state<R>(f: impl FnOnce(&mut NativeState) -> R) -> R {
    NATIVE_STATE.with(|state| {
        let mut state = state.borrow_mut();
        f(&mut state)
    })
}

/// Test helper: reset the state back to defaults.
pub fn reset() {
    with_state(|s| *s = NativeState::default());
}

impl NativePlatform {
    pub(crate) const fn new() -> Self {
        NativePlatform
    }

    pub(crate) fn get_or_install(&'static self) -> &'static dyn Platform {
        self as &'static dyn Platform
    }
}

pub static NATIVE_PLATFORM: NativePlatform = NativePlatform::new();

impl Platform for NativePlatform {
    fn now_ns(&self) -> u64 {
        with_state(|state| {
            let elapsed = state.start.elapsed();
            let mut ns =
                elapsed.as_secs().saturating_mul(1_000_000_000) + u64::from(elapsed.subsec_nanos());
            // Strict monotonicity: even two back-to-back calls within the
            // same nanosecond must produce distinct values.
            if ns <= state.last_ns {
                ns = state.last_ns + 1;
            }
            state.last_ns = ns;
            ns
        })
    }

    fn now_realtime_ns(&self) -> u64 {
        // Under native tests the wall clock comes straight from
        // `SystemTime::now()`. No strict-monotonicity fudge: the
        // WASI `CLOCK_REALTIME` contract permits the clock to jump
        // (NTP adjustment, manual clock change, leap-second
        // smearing) and a caller who needs monotonicity is expected
        // to use `CLOCK_MONOTONIC` instead. If the host clock is
        // somehow before the Unix epoch (developer set the clock to
        // 1969 to test something), the `duration_since` call errors
        // out; we saturate to 0 in that case rather than panic.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| {
                d.as_secs()
                    .saturating_mul(1_000_000_000)
                    .saturating_add(u64::from(d.subsec_nanos()))
            })
            .unwrap_or(0)
    }

    fn driver_call(&self, dev: DevId, op: u32, args: &[u8]) -> DriverResult<u32> {
        with_state(|state| {
            state.driver_calls.push(DriverCall {
                dev,
                op,
                args: args.to_vec(),
            });
            match state.canned_responses.get(&(dev as u8, op)).copied() {
                Some(v) => Ok(v),
                // Default: succeed with result 0.
                None => Ok(0),
            }
        })
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
        // Record the halt before panicking so tests can inspect it
        // from a catch_unwind harness.
        with_state(|state| state.halted = Some(reason.to_string()));
        panic!("NativePlatform::halt(\"{}\")", reason);
    }

    fn on_panic(&self, info: &PanicInfo) {
        with_state(|state| state.panics.push(format!("{info}")));
    }

    fn spawn_process(&self, pid: Pid, path: &str, executable: Option<&[u8]>) -> DriverResult<()> {
        with_state(|state| {
            if let Some(err) = state.next_spawn_error.take() {
                return Err(err);
            }
            state.spawn_calls.push(SpawnCall {
                pid,
                path: path.to_string(),
                executable: executable.map(Vec::from),
            });
            Ok(())
        })
    }

    fn terminate_process(&self, pid: Pid) -> DriverResult<()> {
        with_state(|state| state.terminate_calls.push(pid));
        Ok(())
    }

    fn request_host_file_picker(&self) -> DriverResult<()> {
        with_state(|state| {
            if let Some(error) = state.next_host_picker_error.take() {
                return Err(error);
            }
            state.host_picker_calls = state.host_picker_calls.saturating_add(1);
            Ok(())
        })
    }

    fn download_host_file(&self, name: &str, mime: &str, bytes: &[u8]) -> DriverResult<()> {
        with_state(|state| {
            if let Some(error) = state.next_host_download_error.take() {
                return Err(error);
            }
            state.host_download_calls.push(HostDownloadCall {
                name: name.to_string(),
                mime: mime.to_string(),
                bytes: bytes.to_vec(),
            });
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    #[test]
    fn reset_and_error_injection_are_isolated_between_test_threads() {
        let error_installed = Arc::new(Barrier::new(2));
        let peer_spawned = Arc::new(Barrier::new(2));

        let error_thread = {
            let error_installed = Arc::clone(&error_installed);
            let peer_spawned = Arc::clone(&peer_spawned);
            thread::spawn(move || {
                reset();
                with_state(|state| {
                    state.next_spawn_error = Some(super::super::DriverError::NotReady);
                });
                error_installed.wait();
                peer_spawned.wait();

                let result = NATIVE_PLATFORM.spawn_process(41, "/bin/fail", None);
                let calls = with_state(|state| state.spawn_calls.clone());
                (result, calls)
            })
        };

        let success_thread = thread::spawn(move || {
            error_installed.wait();
            reset();
            let result = NATIVE_PLATFORM.spawn_process(42, "/bin/succeed", None);
            let calls = with_state(|state| state.spawn_calls.clone());
            peer_spawned.wait();
            (result, calls)
        });

        let (error_result, error_calls) = error_thread.join().expect("error thread panicked");
        let (success_result, success_calls) =
            success_thread.join().expect("success thread panicked");

        assert_eq!(error_result, Err(super::super::DriverError::NotReady));
        assert!(error_calls.is_empty());
        assert_eq!(success_result, Ok(()));
        assert_eq!(
            success_calls,
            vec![SpawnCall {
                pid: 42,
                path: "/bin/succeed".to_string(),
                executable: None,
            }]
        );
    }
}
