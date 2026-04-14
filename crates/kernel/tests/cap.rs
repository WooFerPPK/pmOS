//! Capability table isolation tests (T066).
//!
//! Runs via `cargo test -p kernel`. Covers the enforcement
//! rules documented on `CapTable` and anchored in the
//! project constitution (Principle II + the 2026-04-13
//! delegation clarification):
//!
//! * install / remove round trip
//! * check returns true / false correctly
//! * list returns the full set
//! * grant requires CAP_GRANT on the caller
//! * grant requires the caller to hold every granted cap
//! * drop_caps: self-revoke succeeds; cross-process revoke
//!   requires CAP_GRANT
//! * Principle II delegation chain: desktop-shell → settings
//!   via KEYMAP_ADMIN works if and only if the shell holds
//!   KEYMAP_ADMIN (it does by default)

#![cfg(feature = "native-platform")]

use abi::cap::{Cap, CapSet, initial};
use kernel::cap::{CapError, CapTable};

// ---- Install / lookup ----------------------------------------------

#[test]
fn install_and_list_round_trip() {
    let mut t = CapTable::new();
    t.install(10, initial::DESKTOP_SHELL);
    let set = t.list(10).unwrap();
    assert!(set.contains(Cap::DisplayClient));
    assert!(set.contains(Cap::Shell));
    assert!(set.contains(Cap::ProcEnumerate));
    assert!(set.contains(Cap::KeymapAdmin));
    assert!(!set.contains(Cap::DisplayServer));
    assert!(!set.contains(Cap::CapGrant));
}

#[test]
fn list_unknown_pid_is_error() {
    let t = CapTable::new();
    assert_eq!(t.list(999).unwrap_err(), CapError::NoSuchPid);
}

#[test]
fn remove_frees_the_pid() {
    let mut t = CapTable::new();
    t.install(10, CapSet::from_caps(&[Cap::DisplayClient]));
    assert_eq!(t.len(), 1);
    let removed = t.remove(10).unwrap();
    assert!(removed.contains(Cap::DisplayClient));
    assert_eq!(t.len(), 0);
    assert!(matches!(t.list(10), Err(CapError::NoSuchPid)));
}

// ---- check ---------------------------------------------------------

#[test]
fn check_returns_true_for_held_caps_and_false_otherwise() {
    let mut t = CapTable::new();
    t.install(10, CapSet::from_caps(&[Cap::DisplayClient, Cap::Net]));
    assert!(t.check(10, Cap::DisplayClient).unwrap());
    assert!(t.check(10, Cap::Net).unwrap());
    assert!(!t.check(10, Cap::DisplayServer).unwrap());
    assert!(!t.check(10, Cap::CapGrant).unwrap());
}

#[test]
fn check_unknown_pid_is_error() {
    let t = CapTable::new();
    assert_eq!(t.check(7, Cap::DisplayClient).unwrap_err(), CapError::NoSuchPid);
}

// ---- grant enforcement --------------------------------------------

#[test]
fn grant_without_cap_grant_is_refused() {
    let mut t = CapTable::new();
    // `shell` holds a lot but NOT CapGrant (only init holds it in v1).
    t.install(1, initial::DESKTOP_SHELL);
    t.install(2, initial::ORDINARY_APP);

    let err = t
        .grant(1, 2, CapSet::from_caps(&[Cap::KeymapAdmin]))
        .unwrap_err();
    assert_eq!(err, CapError::NotPermitted);

    // Target's caps are unchanged.
    let set = t.list(2).unwrap();
    assert!(!set.contains(Cap::KeymapAdmin));
}

#[test]
fn grant_subset_of_granters_own_caps_succeeds() {
    let mut t = CapTable::new();
    // Only init has CapGrant AND the ability to grant any
    // capability (its set is ALL).
    t.install(1, initial::INIT);
    t.install(2, initial::ORDINARY_APP);

    t.grant(1, 2, CapSet::from_caps(&[Cap::Net, Cap::ProcEnumerate]))
        .unwrap();

    let set = t.list(2).unwrap();
    assert!(set.contains(Cap::Net));
    assert!(set.contains(Cap::ProcEnumerate));
    assert!(set.contains(Cap::DisplayClient));
}

#[test]
fn grant_of_cap_not_held_by_granter_is_refused() {
    let mut t = CapTable::new();
    // Forge a granter that holds CapGrant but NOT KeymapAdmin.
    // This synthetic "evil init" attempts to grant KeymapAdmin
    // without having it. The rule forbids privilege escalation
    // even for CapGrant holders.
    let forged = CapSet::from_caps(&[Cap::CapGrant, Cap::DisplayClient]);
    t.install(1, forged);
    t.install(2, initial::ORDINARY_APP);

    let err = t
        .grant(1, 2, CapSet::from_caps(&[Cap::KeymapAdmin]))
        .unwrap_err();
    assert_eq!(err, CapError::NotASubset);

    // Target is unchanged.
    assert!(!t.list(2).unwrap().contains(Cap::KeymapAdmin));
}

#[test]
fn grant_merges_with_existing_caps() {
    let mut t = CapTable::new();
    t.install(1, initial::INIT);
    t.install(2, CapSet::from_caps(&[Cap::DisplayClient]));

    t.grant(1, 2, CapSet::from_caps(&[Cap::Net])).unwrap();
    t.grant(1, 2, CapSet::from_caps(&[Cap::KeymapAdmin])).unwrap();

    let set = t.list(2).unwrap();
    assert!(set.contains(Cap::DisplayClient));
    assert!(set.contains(Cap::Net));
    assert!(set.contains(Cap::KeymapAdmin));
}

#[test]
fn grant_unknown_pid_is_no_such_pid() {
    let mut t = CapTable::new();
    t.install(1, initial::INIT);

    let err = t.grant(1, 999, CapSet::from_caps(&[Cap::Net])).unwrap_err();
    assert_eq!(err, CapError::NoSuchPid);
}

#[test]
fn grant_from_missing_granter_is_no_such_pid() {
    let mut t = CapTable::new();
    t.install(2, initial::ORDINARY_APP);

    let err = t.grant(1, 2, CapSet::from_caps(&[Cap::Net])).unwrap_err();
    assert_eq!(err, CapError::NoSuchPid);
}

// ---- drop_caps -----------------------------------------------------

#[test]
fn self_drop_caps_succeeds() {
    let mut t = CapTable::new();
    t.install(10, CapSet::from_caps(&[Cap::DisplayClient, Cap::Net, Cap::KeymapAdmin]));
    t.drop_caps(10, 10, CapSet::from_caps(&[Cap::Net])).unwrap();

    let set = t.list(10).unwrap();
    assert!(set.contains(Cap::DisplayClient));
    assert!(!set.contains(Cap::Net));
    assert!(set.contains(Cap::KeymapAdmin));
}

#[test]
fn cross_process_drop_without_cap_grant_is_refused() {
    let mut t = CapTable::new();
    t.install(10, initial::DESKTOP_SHELL); // no CapGrant
    t.install(20, initial::ORDINARY_APP);

    let err = t
        .drop_caps(10, 20, CapSet::from_caps(&[Cap::DisplayClient]))
        .unwrap_err();
    assert_eq!(err, CapError::NotPermitted);
}

#[test]
fn cross_process_drop_with_cap_grant_succeeds() {
    let mut t = CapTable::new();
    t.install(1, initial::INIT); // has CapGrant
    t.install(20, initial::ORDINARY_APP);

    t.drop_caps(1, 20, CapSet::from_caps(&[Cap::DisplayClient]))
        .unwrap();
    assert!(!t.list(20).unwrap().contains(Cap::DisplayClient));
}

// ---- Principle II delegation chain ---------------------------------
//
// The constitution's Principle II (post-2026-04-13 amendment)
// says the desktop shell is the trust root for capability
// delegation to user-launched applications. The tests below
// encode the invariants that make this work at runtime:
//
//   1. The shell holds KEYMAP_ADMIN so it can delegate it.
//   2. The settings app is granted KEYMAP_ADMIN at spawn
//      time by the launcher (which is the shell), which
//      requires the shell to ALSO hold CapGrant OR have the
//      caps passed through by the parent chain. In v1 only
//      init holds CapGrant; the intended path is for init
//      to spawn settings with the right caps already set.
//      But the test still verifies that the subset rule
//      holds when it WOULD happen via grant.

#[test]
fn shell_can_delegate_keymap_admin_if_it_also_had_cap_grant() {
    // Synthetic scenario: the shell holds DESKTOP_SHELL caps
    // PLUS CapGrant. This is not the default, but the rule
    // is that IF a process holds CapGrant and IF its cap set
    // includes KEYMAP_ADMIN, it can grant KEYMAP_ADMIN to a
    // child. Verifies the subset-of-parent invariant works
    // for delegation.
    let mut t = CapTable::new();
    let shell_with_grant = initial::DESKTOP_SHELL.union(CapSet::from_caps(&[Cap::CapGrant]));
    t.install(1, shell_with_grant);
    // Settings starts with just DisplayClient (as if the
    // launcher hadn't yet applied X-PMos-Caps).
    t.install(2, CapSet::from_caps(&[Cap::DisplayClient]));

    t.grant(1, 2, CapSet::from_caps(&[Cap::KeymapAdmin])).unwrap();

    let settings = t.list(2).unwrap();
    assert!(settings.contains(Cap::DisplayClient));
    assert!(settings.contains(Cap::KeymapAdmin));
}

#[test]
fn subset_invariant_blocks_shell_from_delegating_unheld_caps() {
    // Same synthetic shell-with-CapGrant, but now it tries to
    // grant DisplayServer — which a desktop shell does NOT
    // hold. The subset rule rejects it.
    let mut t = CapTable::new();
    let shell_with_grant = initial::DESKTOP_SHELL.union(CapSet::from_caps(&[Cap::CapGrant]));
    t.install(1, shell_with_grant);
    t.install(2, initial::ORDINARY_APP);

    let err = t
        .grant(1, 2, CapSet::from_caps(&[Cap::DisplayServer]))
        .unwrap_err();
    assert_eq!(err, CapError::NotASubset);
    assert!(!t.list(2).unwrap().contains(Cap::DisplayServer));
}

#[test]
fn settings_initial_grants_are_a_subset_of_desktop_shell() {
    // Structural invariant from data-model.md §5: if the
    // settings app's initial cap set is NOT a subset of
    // the desktop shell's, the launcher (which gets its
    // caps from the shell in the intended integration)
    // will not be able to apply X-PMos-Caps at spawn time,
    // and the keymap picker will silently fail at runtime.
    assert!(initial::SETTINGS.is_subset_of(initial::DESKTOP_SHELL));
}

#[test]
fn init_holds_every_capability_at_boot() {
    assert!(initial::INIT.contains(Cap::DisplayClient));
    assert!(initial::INIT.contains(Cap::DisplayServer));
    assert!(initial::INIT.contains(Cap::Shell));
    assert!(initial::INIT.contains(Cap::ProcEnumerate));
    assert!(initial::INIT.contains(Cap::ProcKillAny));
    assert!(initial::INIT.contains(Cap::Net));
    assert!(initial::INIT.contains(Cap::Mount));
    assert!(initial::INIT.contains(Cap::CapGrant));
    assert!(initial::INIT.contains(Cap::DevBlock));
    assert!(initial::INIT.contains(Cap::KeymapAdmin));
}

#[test]
fn ordinary_app_only_has_display_client() {
    let set = initial::ORDINARY_APP;
    assert!(set.contains(Cap::DisplayClient));
    assert!(!set.contains(Cap::DisplayServer));
    assert!(!set.contains(Cap::Shell));
    assert!(!set.contains(Cap::Net));
    assert!(!set.contains(Cap::CapGrant));
    assert!(!set.contains(Cap::KeymapAdmin));
}
