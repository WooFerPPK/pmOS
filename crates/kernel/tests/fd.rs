//! Fd-table isolation tests (T048).
//!
//! Runs via `cargo test -p kernel`. Gated on native-platform (the
//! kernel's default feature) for the same reason proc.rs is —
//! these tests live in the kernel crate's integration-test
//! harness, which needs std.

#![cfg(feature = "native-platform")]

use kernel::fd::{FdEntry, FdError, FdFlags, FdObject, FdTable, FD_SOFT_LIMIT};
use kernel::vfs::MountId;

/// Build a `Vnode` variant that tests can pass around without
/// having to spell out the struct expression on every line. The
/// tests don't care about the mount id; they only care that
/// distinct `ino` values produce distinct fd-objects that compare
/// correctly.
const fn vn(ino: u64) -> FdObject {
    FdObject::Vnode {
        mount_id: MountId(1),
        ino,
    }
}

// ---- Allocation + lookup --------------------------------------------

#[test]
fn alloc_starts_at_zero_and_is_dense() {
    let mut t = FdTable::new();
    assert_eq!(t.alloc(FdEntry::new(vn(1))).unwrap(), 0);
    assert_eq!(t.alloc(FdEntry::new(vn(2))).unwrap(), 1);
    assert_eq!(t.alloc(FdEntry::new(vn(3))).unwrap(), 2);
    assert_eq!(t.open_count(), 3);
}

#[test]
fn close_then_alloc_returns_lowest_free_slot() {
    let mut t = FdTable::new();
    let a = t.alloc(FdEntry::new(vn(1))).unwrap();
    let b = t.alloc(FdEntry::new(vn(2))).unwrap();
    let c = t.alloc(FdEntry::new(vn(3))).unwrap();
    assert_eq!((a, b, c), (0, 1, 2));

    // Close fd 1, then alloc: should reuse slot 1, not 3.
    t.close(b).unwrap();
    let d = t.alloc(FdEntry::new(vn(4))).unwrap();
    assert_eq!(d, 1);
}

#[test]
fn alloc_past_soft_limit_returns_out_of_fds() {
    let mut t = FdTable::with_limit(3);
    t.alloc(FdEntry::new(vn(1))).unwrap();
    t.alloc(FdEntry::new(vn(2))).unwrap();
    t.alloc(FdEntry::new(vn(3))).unwrap();
    let err = t.alloc(FdEntry::new(vn(4))).unwrap_err();
    assert_eq!(err, FdError::OutOfFds);
}

#[test]
fn default_soft_limit_matches_constant() {
    // Smoke check: the documented default constant is what
    // `new()` actually uses. If this diverges, userland code
    // that picks fd numbers expecting the soft limit will break.
    let t = FdTable::new();
    assert_eq!(t.open_count(), 0);
    // We don't actually allocate 1024+ fds in a unit test, just
    // confirm the constant is what we think it is.
    assert_eq!(FD_SOFT_LIMIT, 1024);
}

// ---- Close semantics -----------------------------------------------

#[test]
fn close_returns_the_removed_entry() {
    let mut t = FdTable::new();
    let fd = t
        .alloc(FdEntry::with_flags(
            FdObject::PipeRead(77),
            FdFlags::NONBLOCK,
        ))
        .unwrap();
    let removed = t.close(fd).unwrap();
    assert_eq!(removed.object, FdObject::PipeRead(77));
    assert!(removed.flags.contains(FdFlags::NONBLOCK));
    assert!(!t.is_open(fd));
}

#[test]
fn close_unopen_fd_is_bad_fd() {
    let mut t = FdTable::new();
    assert_eq!(t.close(0).unwrap_err(), FdError::BadFd);
    assert_eq!(t.close(999).unwrap_err(), FdError::BadFd);
}

#[test]
fn close_twice_is_bad_fd() {
    let mut t = FdTable::new();
    let fd = t.alloc(FdEntry::new(vn(1))).unwrap();
    t.close(fd).unwrap();
    assert_eq!(t.close(fd).unwrap_err(), FdError::BadFd);
}

// ---- dup / install_at -----------------------------------------------

#[test]
fn dup_clears_cloexec_on_the_new_entry() {
    let mut t = FdTable::new();
    let fd = t
        .alloc(FdEntry::with_flags(vn(42), FdFlags::CLOEXEC))
        .unwrap();
    let duped = t.dup(fd).unwrap();
    assert_ne!(duped, fd);

    // Original still has CLOEXEC.
    assert!(t.get(fd).unwrap().flags.contains(FdFlags::CLOEXEC));
    // Duped does NOT.
    assert!(!t.get(duped).unwrap().flags.contains(FdFlags::CLOEXEC));
    // Both still refer to the same underlying object.
    assert_eq!(t.get(duped).unwrap().object, vn(42));
}

#[test]
fn dup_of_unopen_fd_is_bad_fd() {
    let mut t = FdTable::new();
    assert_eq!(t.dup(5).unwrap_err(), FdError::BadFd);
}

#[test]
fn install_at_closes_existing_entry_and_returns_it() {
    let mut t = FdTable::new();
    let original = FdEntry::new(vn(1));
    let fd = t.alloc(original).unwrap();

    let replacement = FdEntry::new(FdObject::PipeRead(99));
    let closed = t.install_at(fd, replacement).unwrap();
    assert_eq!(closed, Some(original));

    let got = t.get(fd).unwrap();
    assert_eq!(got.object, FdObject::PipeRead(99));
}

#[test]
fn install_at_to_unopen_slot_grows_table_and_returns_none() {
    let mut t = FdTable::new();
    let closed = t.install_at(3, FdEntry::new(vn(3))).unwrap();
    assert_eq!(closed, None);
    assert!(t.get(3).is_some());
    // Slots 0..3 exist as None (they were filled in by the grow).
    for i in 0..3 {
        assert!(!t.is_open(i));
    }
}

#[test]
fn install_at_past_soft_limit_is_out_of_fds() {
    let mut t = FdTable::with_limit(2);
    let err = t.install_at(5, FdEntry::new(vn(1))).unwrap_err();
    assert_eq!(err, FdError::OutOfFds);
}

// ---- Offset -----------------------------------------------------------

#[test]
fn offset_round_trip() {
    let mut t = FdTable::new();
    let fd = t.alloc(FdEntry::new(vn(1))).unwrap();
    assert_eq!(t.offset(fd).unwrap(), 0);

    t.set_offset(fd, 12345).unwrap();
    assert_eq!(t.offset(fd).unwrap(), 12345);

    assert_eq!(t.offset(fd + 1).unwrap_err(), FdError::BadFd);
    assert_eq!(t.set_offset(fd + 1, 0).unwrap_err(), FdError::BadFd);
}

// ---- CLOEXEC semantics (used by proc_spawn) --------------------------

#[test]
fn drop_cloexec_removes_only_cloexec_entries() {
    let mut t = FdTable::new();
    let plain = t.alloc(FdEntry::new(vn(1))).unwrap();
    let cloexec = t
        .alloc(FdEntry::with_flags(vn(2), FdFlags::CLOEXEC))
        .unwrap();
    let nonblock = t
        .alloc(FdEntry::with_flags(vn(3), FdFlags::NONBLOCK))
        .unwrap();

    let dropped = t.drop_cloexec();
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].0, cloexec);
    assert_eq!(dropped[0].1.object, vn(2));

    // Non-cloexec entries survive.
    assert!(t.is_open(plain));
    assert!(t.is_open(nonblock));
    assert!(!t.is_open(cloexec));
}

#[test]
fn drain_all_empties_and_returns_every_entry() {
    let mut t = FdTable::new();
    let a = t.alloc(FdEntry::new(vn(1))).unwrap();
    let b = t.alloc(FdEntry::new(FdObject::PipeWrite(2))).unwrap();
    let c = t
        .alloc(FdEntry::with_flags(FdObject::Socket(3), FdFlags::CLOEXEC))
        .unwrap();

    let drained = t.drain_all();
    assert_eq!(drained.len(), 3);
    assert_eq!(t.open_count(), 0);

    let fds: Vec<u32> = drained.iter().map(|(fd, _)| *fd).collect();
    assert_eq!(fds, vec![a, b, c]);
}

// ---- FdFlags bitset mechanics ---------------------------------------

#[test]
fn fd_flags_insert_contains_remove() {
    let mut f = FdFlags::EMPTY;
    assert!(!f.contains(FdFlags::CLOEXEC));

    f.insert(FdFlags::CLOEXEC);
    assert!(f.contains(FdFlags::CLOEXEC));
    assert!(!f.contains(FdFlags::NONBLOCK));

    f.insert(FdFlags::NONBLOCK);
    assert!(f.contains(FdFlags::CLOEXEC));
    assert!(f.contains(FdFlags::NONBLOCK));

    f.remove(FdFlags::CLOEXEC);
    assert!(!f.contains(FdFlags::CLOEXEC));
    assert!(f.contains(FdFlags::NONBLOCK));
}

// ---- Iterator view --------------------------------------------------

#[test]
fn iter_yields_only_open_fds_in_ascending_order() {
    let mut t = FdTable::new();
    t.alloc(FdEntry::new(vn(10))).unwrap(); // fd 0
    t.alloc(FdEntry::new(vn(20))).unwrap(); // fd 1
    t.alloc(FdEntry::new(vn(30))).unwrap(); // fd 2
    t.close(1).unwrap();

    let seen: Vec<(u32, FdObject)> = t.iter().map(|(fd, e)| (fd, e.object)).collect();
    assert_eq!(seen, vec![(0, vn(10)), (2, vn(30))]);
}

// ---- Object variants round-trip correctly -------------------------

#[test]
fn every_fd_object_variant_round_trips() {
    let mut t = FdTable::new();
    let variants = [
        vn(1),
        FdObject::CharDevice(7),
        FdObject::PipeRead(2),
        FdObject::PipeWrite(3),
        FdObject::Socket(4),
        FdObject::DisplayConn(5),
        FdObject::SignalChannel,
    ];
    for (i, v) in variants.iter().enumerate() {
        let fd = t.alloc(FdEntry::new(*v)).unwrap();
        assert_eq!(fd as usize, i);
        assert_eq!(t.get(fd).unwrap().object, *v);
    }
    assert_eq!(t.open_count(), variants.len());
}
