//! Well-known per-process file descriptors.
//!
//! These descriptors are installed by the kernel for every process created
//! through `proc_spawn`. Keeping the numbers in the shared ABI avoids the
//! signal channel colliding with WASI libc's preopen discovery.

/// Standard input.
pub const STDIN: u32 = 0;
/// Standard output.
pub const STDOUT: u32 = 1;
/// Standard error.
pub const STDERR: u32 = 2;
/// WASI directory preopen for the PMos root filesystem (`/`).
pub const ROOT_PREOPEN: u32 = 3;
/// PMos signal-inbox channel. Each record is one little-endian `u16` signum.
pub const SIGNAL: u32 = 4;
/// First descriptor available to explicit spawn-manifest `extra_fds` maps.
pub const FIRST_DYNAMIC: u32 = SIGNAL + 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_layout_is_stable() {
        assert_eq!(STDIN, 0);
        assert_eq!(STDOUT, 1);
        assert_eq!(STDERR, 2);
        assert_eq!(ROOT_PREOPEN, 3);
        assert_eq!(SIGNAL, 4);
        assert_eq!(FIRST_DYNAMIC, 5);
    }
}
