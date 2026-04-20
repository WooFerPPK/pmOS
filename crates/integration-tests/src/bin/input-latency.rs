//! `input-latency` — T220 placeholder.
//!
//! Eventually this will exercise the full input-to-echo path
//! (DOM `keydown` → `InputDriver` → `kernel_inject_input_kbd` →
//! user wasm `fd_read` → `fd_write` → `onConsoleWrite` callback)
//! with sub-millisecond timing so the Principle IX < 100 ms
//! input-latency budget (plan.md) has native-Rust evidence the
//! Playwright JS layer can't provide.
//!
//! Today the harness is a stub: it prints one explanatory line
//! and exits 0 so `just test-perf` (and therefore `just test`)
//! can complete cleanly on CI while T220 is still not-started.
//!
//! When the real harness lands, replace the body with the
//! Platform-abstraction-driven end-to-end measurement loop
//! sketched in `crates/integration-tests/src/lib.rs` and wire
//! a real pass/fail threshold keyed to the 100 ms budget.

fn main() {
    println!("input-latency: T220 placeholder — no measurement wired yet (see crates/integration-tests/src/bin/input-latency.rs)");
}
