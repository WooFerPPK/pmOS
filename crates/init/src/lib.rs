//! init's library surface — the `/etc/init.conf` parser plus the
//! shell-respawn rate limiter. Lifted out of `main.rs` so the
//! integration tests in `crates/init/tests/` can exercise the
//! parser + the respawn cap on a host target without going
//! through wasm32-wasip1.

pub mod conf;
pub mod respawn;
pub mod spawn;

/// Least-privilege grants used by the two PID-1 binaries. Keeping these in
/// the host-testable library prevents a future boot-path edit from silently
/// regressing to `u64::MAX` for every child.
pub mod grants {
    pub const ORDINARY_APP: u64 = abi::cap::initial::ORDINARY_APP.0;
    pub const DISPLAY_SERVER: u64 = abi::cap::initial::DISPLAY_SERVER.0;
    pub const DESKTOP_SHELL: u64 = abi::cap::initial::DESKTOP_SHELL.0;
}
