//! ABI versioning.
//!
//! Bumped per `contracts/syscalls.md §5`. Kernel and userland check
//! this at process spawn; mismatched processes fail with `ENOABIVER`.

/// The current PMos ABI version.
///
/// - v1.0 — initial release
/// - **v1.1** — adds `fs_watch` (0x1402, §3.7) and `host_file_recv`
///   (0x1500, §3.6). Additive; v1.0 programs continue to run unchanged.
pub const ABI_VERSION: (u16, u16) = (1, 1);

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
    fn current_is_one_one() {
        assert_eq!(ABI_VERSION, (1, 1));
        assert_eq!(ABI_MAJOR, 1);
        assert_eq!(ABI_MINOR, 1);
    }

    #[test]
    fn packed_roundtrip() {
        let p = packed();
        assert_eq!(p, 0x0001_0001);
        assert!(is_compatible_packed(p));
    }

    #[test]
    fn old_minor_still_compatible() {
        // v1.0 program on v1.1 kernel: OK.
        assert!(is_compatible(1, 0));
    }

    #[test]
    fn newer_minor_is_incompatible() {
        // v1.2 program on v1.1 kernel: NOT OK (kernel doesn't know
        // the new opcodes yet).
        assert!(!is_compatible(1, 2));
    }

    #[test]
    fn different_major_is_incompatible() {
        assert!(!is_compatible(0, 1));
        assert!(!is_compatible(2, 0));
        assert!(!is_compatible(2, 1));
    }
}
