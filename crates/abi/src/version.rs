//! ABI versioning.
//!
//! Bumped per `contracts/syscalls.md §5`. Kernel and userland check
//! this at process spawn; mismatched processes fail with `ENOABIVER`.

/// The current PMos ABI version.
///
/// - v1.0 — initial release
/// - **v1.1** — adds `fs_watch` (0x1402, §3.7) and `host_file_recv`
///   (0x1500, §3.6). Additive; v1.0 programs continue to run unchanged.
/// - **v1.2** — adds `ipc_peer_caps` (0x1008, §3.1), the generic
///   kernel-authenticated peer-credential query used by privileged
///   IPC services such as the display server.
/// - **v1.3** — adds `host_file_pick` (0x1501), the write-only
///   `host_file_send` stream (0x1502), `fs_chmod` (0x1403), and
///   `HOST_TRANSFER`.
/// - **v1.4** — adds the fd-scoped, kernel-authenticated
///   `ipc_peer_pid` query (0x1009, §3.1).
pub const ABI_VERSION: (u16, u16) = (1, 4);

/// Major component.
pub const ABI_MAJOR: u16 = ABI_VERSION.0;
/// Minor component.
pub const ABI_MINOR: u16 = ABI_VERSION.1;

/// Packed 32-bit representation for wire encoding: `(major << 16) | minor`.
pub const fn packed() -> u32 {
    ((ABI_MAJOR as u32) << 16) | (ABI_MINOR as u32)
}

/// Check whether a program compiled against `requested` ABI is
/// compatible with the kernel's current ABI.
///
/// Compatibility rules:
/// * Major must match exactly.
/// * Requested minor must be `<=` current minor (older programs run
///   on newer kernels, not the other way round).
#[inline]
pub const fn is_compatible(requested_major: u16, requested_minor: u16) -> bool {
    requested_major == ABI_MAJOR && requested_minor <= ABI_MINOR
}

/// Same as [`is_compatible`] but takes a packed `(major, minor)`
/// tuple as produced by [`packed`].
#[inline]
pub const fn is_compatible_packed(requested_packed: u32) -> bool {
    let major = (requested_packed >> 16) as u16;
    let minor = (requested_packed & 0xFFFF) as u16;
    is_compatible(major, minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_is_one_four() {
        assert_eq!(ABI_VERSION, (1, 4));
        assert_eq!(ABI_MAJOR, 1);
        assert_eq!(ABI_MINOR, 4);
    }

    #[test]
    fn packed_roundtrip() {
        let p = packed();
        assert_eq!(p, 0x0001_0004);
        assert!(is_compatible_packed(p));
    }

    #[test]
    fn old_minor_still_compatible() {
        // Older v1 programs on a v1.4 kernel: OK.
        assert!(is_compatible(1, 0));
        assert!(is_compatible(1, 1));
        assert!(is_compatible(1, 2));
        assert!(is_compatible(1, 3));
    }

    #[test]
    fn newer_minor_is_incompatible() {
        // v1.5 program on v1.4 kernel: NOT OK (kernel doesn't know
        // the new opcodes yet).
        assert!(!is_compatible(1, 5));
    }

    #[test]
    fn different_major_is_incompatible() {
        assert!(!is_compatible(0, 1));
        assert!(!is_compatible(2, 0));
        assert!(!is_compatible(2, 1));
    }
}
