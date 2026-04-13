//! PMos native-Rust integration test harness.
//!
//! This crate hosts full-stack tests that need timing precision
//! the Playwright JS layer cannot provide. The perf/input-latency
//! harness (T220, Principle IX gate) lives here under src/bin/ once
//! T220 populates it.
//!
//! Host-target only. Never compiled for wasm.
