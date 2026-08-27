//! `Interface::from_name` round-trip tests.

use display_proto::{Direction, Interface};

#[test]
fn from_name_is_the_inverse_of_name_for_every_v1_interface() {
    for iface in [
        Interface::Display,
        Interface::Registry,
        Interface::Compositor,
        Interface::Shm,
        Interface::ShmPool,
        Interface::Buffer,
        Interface::Surface,
        Interface::ShellManager,
    ] {
        let round = Interface::from_name(iface.name()).unwrap();
        assert_eq!(round, iface, "round trip broke on {iface:?}");
    }
}

#[test]
fn shell_manager_is_named_pmd_shell_manager_per_spec_section_15() {
    assert_eq!(Interface::ShellManager.name(), "pmd_shell_manager");
    assert_eq!(
        Interface::from_name("pmd_shell_manager"),
        Some(Interface::ShellManager)
    );
}

#[test]
fn supported_versions_match_the_registry_catalogue() {
    assert_eq!(Interface::ShellManager.supported_version(), 2);
    for interface in [
        Interface::Compositor,
        Interface::Shm,
        Interface::XdgShell,
        Interface::Seat,
    ] {
        assert_eq!(interface.supported_version(), 1, "{interface:?}");
    }
}

#[test]
fn from_name_returns_none_for_unknown_names() {
    assert!(Interface::from_name("pmd_nope").is_none());
    assert!(Interface::from_name("").is_none());
    assert!(Interface::from_name("surface").is_none());
}

#[test]
fn xdg_toplevel_opcode_nine_is_owner_set_minimized_request() {
    let opcode = Interface::XdgToplevel.lookup_request(9).unwrap();
    assert_eq!(opcode.number, 9);
    assert_eq!(opcode.direction, Direction::Request);
    assert_eq!(opcode.name, "set_minimized");
}
