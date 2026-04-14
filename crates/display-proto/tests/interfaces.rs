//! `Interface::from_name` round-trip tests.

use display_proto::Interface;

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
    ] {
        let round = Interface::from_name(iface.name()).unwrap();
        assert_eq!(round, iface, "round trip broke on {iface:?}");
    }
}

#[test]
fn from_name_returns_none_for_unknown_names() {
    assert!(Interface::from_name("pmd_nope").is_none());
    assert!(Interface::from_name("").is_none());
    assert!(Interface::from_name("surface").is_none());
}
