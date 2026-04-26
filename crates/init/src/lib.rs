//! init's library surface — the `/etc/init.conf` parser plus the
//! shell-respawn rate limiter. Lifted out of `main.rs` so the
//! integration tests in `crates/init/tests/` can exercise the
//! parser + the respawn cap on a host target without going
//! through wasm32-wasip1.

pub mod conf;
pub mod respawn;
