//! Capability enumeration and bitset, matching
//! `data-model.md §5` and the post-batch Principle II clarification
//! in `.specify/memory/constitution.md`.
//!
//! Privilege in PMos is expressed exclusively as kernel-granted
//! capabilities. There is no distinction between "system" and
//! "user" processes except in terms of which caps each holds.
//! The desktop shell additionally acts as the trust root for
//! capability delegation to user-launched apps (per the 2026-04-13
//! constitution amendment).

/// PMos capability identifiers.
///
/// Encoded as `u32` to leave room for 2^32 caps in principle; `CapSet`
/// is a 64-bit bitmask, so v1 is capped at 64 distinct capabilities.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Cap {
    /// May open a connection to `/run/display` (via `display_connect`).
    DisplayClient = 1,
    /// May open `/dev/fb0` and `/dev/input/*`. Held only by the
    /// display server process.
    DisplayServer = 2,
    /// May bind `pmd_shell_manager` and subscribe to window-list
    /// events. Held by the desktop shell and any alternative shell
    /// that replaces it.
    Shell = 3,
    /// May enumerate `/proc` entries for processes other than itself.
    ProcEnumerate = 4,
    /// May deliver signals to processes it did not spawn.
    ProcKillAny = 5,
    /// May use the net syscalls (`sock_*`, high-level fetch/WebSocket).
    Net = 6,
    /// May call `mount` / `umount`.
    Mount = 7,
    /// May call `cap_grant`. In v1 only init holds this by default.
    CapGrant = 8,
    /// May open block devices directly.
    DevBlock = 9,
    /// May bind `pmd_keymap_manager` and switch the system keyboard
    /// layout. Held by the desktop shell (for delegation to the
    /// settings app at launcher spawn time).
    KeymapAdmin = 10,
    /// May query another process's capability set via
    /// `proc_caps_get`. Querying one's own caps never requires
    /// this cap; it's only consulted when the target pid is a
    /// non-child of the sender. Held by init by default (as part
    /// of `CapSet::ALL`) and by `ProcEnumerate`-equipped tools
    /// that need to inspect neighbours' permissions — e.g.
    /// sysmon.
    ProcInspect = 11,
    /// May ask the browser substrate to open the native file picker or
    /// emit a host download. Held by the desktop shell for delegation
    /// to the bundled Files app; ordinary applications do not receive it.
    HostTransfer = 12,
}

impl Cap {
    /// The bit this capability occupies in a `CapSet`.
    #[inline]
    pub const fn bit(self) -> u64 {
        1u64 << (self as u32)
    }

    /// Attempt to decode a `u32` discriminant back into a `Cap`.
    pub const fn from_u32(v: u32) -> Option<Cap> {
        match v {
            1 => Some(Cap::DisplayClient),
            2 => Some(Cap::DisplayServer),
            3 => Some(Cap::Shell),
            4 => Some(Cap::ProcEnumerate),
            5 => Some(Cap::ProcKillAny),
            6 => Some(Cap::Net),
            7 => Some(Cap::Mount),
            8 => Some(Cap::CapGrant),
            9 => Some(Cap::DevBlock),
            10 => Some(Cap::KeymapAdmin),
            11 => Some(Cap::ProcInspect),
            12 => Some(Cap::HostTransfer),
            _ => None,
        }
    }

    /// Human-readable name used by `/proc/<pid>/status` and by
    /// manifest.toml `capabilities.required` matching.
    pub const fn name(self) -> &'static str {
        match self {
            Cap::DisplayClient => "DISPLAY_CLIENT",
            Cap::DisplayServer => "DISPLAY_SERVER",
            Cap::Shell => "SHELL",
            Cap::ProcEnumerate => "PROC_ENUMERATE",
            Cap::ProcKillAny => "PROC_KILL_ANY",
            Cap::Net => "NET",
            Cap::Mount => "MOUNT",
            Cap::CapGrant => "CAP_GRANT",
            Cap::DevBlock => "DEV_BLOCK",
            Cap::KeymapAdmin => "KEYMAP_ADMIN",
            Cap::ProcInspect => "PROC_INSPECT",
            Cap::HostTransfer => "HOST_TRANSFER",
        }
    }

    /// Decode the stable manifest/init configuration spelling of a
    /// capability. Keeping this inverse beside [`Cap::name`] prevents init,
    /// the launcher, and package validation from growing divergent tables.
    pub fn from_name(name: &str) -> Option<Cap> {
        match name {
            "DISPLAY_CLIENT" => Some(Cap::DisplayClient),
            "DISPLAY_SERVER" => Some(Cap::DisplayServer),
            "SHELL" => Some(Cap::Shell),
            "PROC_ENUMERATE" => Some(Cap::ProcEnumerate),
            "PROC_KILL_ANY" => Some(Cap::ProcKillAny),
            "NET" => Some(Cap::Net),
            "MOUNT" => Some(Cap::Mount),
            "CAP_GRANT" => Some(Cap::CapGrant),
            "DEV_BLOCK" => Some(Cap::DevBlock),
            "KEYMAP_ADMIN" => Some(Cap::KeymapAdmin),
            "PROC_INSPECT" => Some(Cap::ProcInspect),
            "HOST_TRANSFER" => Some(Cap::HostTransfer),
            _ => None,
        }
    }
}

/// Bitset of capabilities a process holds. Internally a `u64`, so
/// up to 64 distinct capabilities fit in v1. Adding a 65th cap
/// requires widening the type and is a MAJOR ABI bump.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CapSet(pub u64);

impl CapSet {
    /// Empty set — no capabilities.
    pub const EMPTY: CapSet = CapSet(0);

    /// All possible capabilities (for init at boot).
    pub const ALL: CapSet = CapSet(u64::MAX);

    /// Build a set from a list of caps.
    pub const fn from_caps(caps: &[Cap]) -> CapSet {
        let mut bits: u64 = 0;
        let mut i = 0;
        while i < caps.len() {
            bits |= caps[i].bit();
            i += 1;
        }
        CapSet(bits)
    }

    /// Is `cap` present?
    #[inline]
    pub const fn contains(self, cap: Cap) -> bool {
        self.0 & cap.bit() != 0
    }

    /// Is every cap in `other` also in `self`?
    #[inline]
    pub const fn is_superset_of(self, other: CapSet) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Is every cap in `self` also in `other`?
    #[inline]
    pub const fn is_subset_of(self, other: CapSet) -> bool {
        other.is_superset_of(self)
    }

    /// Add a cap.
    #[inline]
    pub fn insert(&mut self, cap: Cap) {
        self.0 |= cap.bit();
    }

    /// Remove a cap.
    #[inline]
    pub fn remove(&mut self, cap: Cap) {
        self.0 &= !cap.bit();
    }

    /// Bitwise union.
    #[inline]
    pub const fn union(self, other: CapSet) -> CapSet {
        CapSet(self.0 | other.0)
    }

    /// Bitwise intersection.
    #[inline]
    pub const fn intersect(self, other: CapSet) -> CapSet {
        CapSet(self.0 & other.0)
    }
}

/// Default initial cap grants per role. The `data-model.md §5`
/// "Initial grants" table; `init.conf` references these by name.
pub mod initial {
    use super::{Cap, CapSet};

    /// init (PID 1) — every capability.
    pub const INIT: CapSet = CapSet::ALL;

    /// Display server — access to the framebuffer and input devices.
    pub const DISPLAY_SERVER: CapSet = CapSet::from_caps(&[Cap::DisplayServer, Cap::DevBlock]);

    /// Desktop shell — display client + SHELL + process enumeration, plus the
    /// privileged capabilities it delegates to Sysmon, Settings, and Files.
    pub const DESKTOP_SHELL: CapSet = CapSet::from_caps(&[
        Cap::DisplayClient,
        Cap::Shell,
        Cap::ProcEnumerate,
        Cap::ProcKillAny,
        Cap::KeymapAdmin,
        Cap::HostTransfer,
    ]);

    /// Bundled sysmon — process enumeration and termination.
    pub const SYSMON: CapSet =
        CapSet::from_caps(&[Cap::DisplayClient, Cap::ProcEnumerate, Cap::ProcKillAny]);

    /// Bundled settings — display client + KEYMAP_ADMIN (granted by
    /// the launcher from its own cap set at spawn time).
    pub const SETTINGS: CapSet = CapSet::from_caps(&[Cap::DisplayClient, Cap::KeymapAdmin]);

    /// Bundled Files — display client plus the host picker/download bridge.
    pub const FILES: CapSet = CapSet::from_caps(&[Cap::DisplayClient, Cap::HostTransfer]);

    /// Default for any other userland program — display client only.
    pub const ORDINARY_APP: CapSet = CapSet::from_caps(&[Cap::DisplayClient]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_contains_nothing() {
        assert!(!CapSet::EMPTY.contains(Cap::DisplayClient));
        assert!(!CapSet::EMPTY.contains(Cap::KeymapAdmin));
    }

    #[test]
    fn all_contains_everything() {
        assert!(CapSet::ALL.contains(Cap::DisplayClient));
        assert!(CapSet::ALL.contains(Cap::KeymapAdmin));
        assert!(CapSet::ALL.contains(Cap::HostTransfer));
        assert!(CapSet::ALL.contains(Cap::CapGrant));
    }

    #[test]
    fn desktop_shell_has_keymap_admin_for_delegation() {
        // Post-2026-04-13 constitution amendment: the desktop shell
        // holds KEYMAP_ADMIN not because it changes the keymap
        // itself but so it can delegate the cap to the settings
        // app at launcher spawn time.
        assert!(initial::DESKTOP_SHELL.contains(Cap::KeymapAdmin));
        assert!(initial::DESKTOP_SHELL.contains(Cap::Shell));
        assert!(initial::DESKTOP_SHELL.contains(Cap::ProcKillAny));
        assert!(!initial::DESKTOP_SHELL.contains(Cap::DisplayServer));
    }

    #[test]
    fn settings_is_subset_of_desktop_shell_for_delegation_to_work() {
        // The launcher can only grant caps that are a subset of its
        // own set. So when the desktop shell's launcher spawns
        // settings, settings' desired cap set must be a subset of
        // the shell's cap set. If this ever regresses, the keymap
        // picker silently stops working at runtime.
        assert!(initial::SETTINGS.is_subset_of(initial::DESKTOP_SHELL));
    }

    #[test]
    fn files_is_subset_of_desktop_shell_for_delegation_to_work() {
        assert!(initial::FILES.is_subset_of(initial::DESKTOP_SHELL));
        assert!(initial::FILES.contains(Cap::HostTransfer));
        assert!(!initial::ORDINARY_APP.contains(Cap::HostTransfer));
    }

    #[test]
    fn sysmon_is_subset_of_desktop_shell_for_delegation_to_work() {
        assert!(initial::SYSMON.is_subset_of(initial::DESKTOP_SHELL));
    }

    #[test]
    fn from_u32_roundtrip() {
        for v in 1..=12 {
            let cap = Cap::from_u32(v).expect("known cap");
            assert_eq!(cap as u32, v);
        }
        assert!(Cap::from_u32(0).is_none());
        assert!(Cap::from_u32(13).is_none());
    }

    #[test]
    fn cap_names_match_data_model() {
        // These strings appear in manifest.toml, /proc, and the
        // launcher's cap-declaration parser. Drift between this
        // table and data-model.md is an ABI break.
        assert_eq!(Cap::DisplayClient.name(), "DISPLAY_CLIENT");
        assert_eq!(Cap::DisplayServer.name(), "DISPLAY_SERVER");
        assert_eq!(Cap::Shell.name(), "SHELL");
        assert_eq!(Cap::ProcEnumerate.name(), "PROC_ENUMERATE");
        assert_eq!(Cap::ProcKillAny.name(), "PROC_KILL_ANY");
        assert_eq!(Cap::Net.name(), "NET");
        assert_eq!(Cap::Mount.name(), "MOUNT");
        assert_eq!(Cap::CapGrant.name(), "CAP_GRANT");
        assert_eq!(Cap::DevBlock.name(), "DEV_BLOCK");
        assert_eq!(Cap::KeymapAdmin.name(), "KEYMAP_ADMIN");
        assert_eq!(Cap::HostTransfer.name(), "HOST_TRANSFER");
        assert_eq!(Cap::ProcInspect.name(), "PROC_INSPECT");
        for value in 1..=12 {
            let cap = Cap::from_u32(value).unwrap();
            assert_eq!(Cap::from_name(cap.name()), Some(cap));
        }
        assert_eq!(Cap::from_name("UNKNOWN"), None);
    }
}
