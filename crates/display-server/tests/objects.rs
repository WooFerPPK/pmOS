//! Object/opcode table tests.

use display_server::objects::{Interface, OpcodeError};

#[test]
fn every_interface_has_a_stable_name() {
    assert_eq!(Interface::Display.name(), "pmd_display");
    assert_eq!(Interface::Registry.name(), "pmd_registry");
    assert_eq!(Interface::Compositor.name(), "pmd_compositor");
    assert_eq!(Interface::Shm.name(), "pmd_shm");
    assert_eq!(Interface::ShmPool.name(), "pmd_shm_pool");
    assert_eq!(Interface::Buffer.name(), "pmd_buffer");
    assert_eq!(Interface::Surface.name(), "pmd_surface");
    assert_eq!(Interface::ShellManager.name(), "pmd_shell_manager");
}

#[test]
fn display_object_has_sync_and_get_registry_requests() {
    let sync = Interface::Display.lookup_request(1).unwrap();
    assert_eq!(sync.name, "sync");
    let reg = Interface::Display.lookup_request(2).unwrap();
    assert_eq!(reg.name, "get_registry");
    // Opcode 3 is not defined.
    assert!(matches!(
        Interface::Display.lookup_request(3).unwrap_err(),
        OpcodeError::UnknownOpcode { .. }
    ));
}

#[test]
fn display_object_has_error_and_delete_id_events() {
    let err = Interface::Display.lookup_event(1).unwrap();
    assert_eq!(err.name, "error");
    let del = Interface::Display.lookup_event(2).unwrap();
    assert_eq!(del.name, "delete_id");
}

#[test]
fn registry_has_bind_request_and_global_event() {
    assert_eq!(
        Interface::Registry.lookup_request(1).unwrap().name,
        "bind"
    );
    assert_eq!(
        Interface::Registry.lookup_event(1).unwrap().name,
        "global"
    );
    assert_eq!(
        Interface::Registry.lookup_event(2).unwrap().name,
        "global_remove"
    );
}

#[test]
fn compositor_has_only_create_surface() {
    assert_eq!(
        Interface::Compositor.lookup_request(1).unwrap().name,
        "create_surface"
    );
    assert!(Interface::Compositor.lookup_request(2).is_err());
    assert!(Interface::Compositor.lookup_event(1).is_err());
}

#[test]
fn shm_pool_has_create_buffer_resize_destroy() {
    assert_eq!(
        Interface::ShmPool.lookup_request(1).unwrap().name,
        "create_buffer"
    );
    assert_eq!(
        Interface::ShmPool.lookup_request(2).unwrap().name,
        "resize"
    );
    assert_eq!(
        Interface::ShmPool.lookup_request(3).unwrap().name,
        "destroy"
    );
}

#[test]
fn surface_has_the_seven_v1_requests() {
    for (op, name) in [
        (1, "destroy"),
        (2, "attach"),
        (3, "damage"),
        (4, "frame"),
        (5, "set_opaque_region"),
        (6, "set_input_region"),
        (7, "commit"),
    ] {
        let o = Interface::Surface.lookup_request(op).unwrap();
        assert_eq!(o.name, name, "opcode {op}");
    }
    // Surfaces emit no events in v1 (configure + close come
    // from xdg_* which aren't wired yet).
    assert!(Interface::Surface.lookup_event(1).is_err());
}

#[test]
fn buffer_has_destroy_request_and_release_event() {
    assert_eq!(
        Interface::Buffer.lookup_request(1).unwrap().name,
        "destroy"
    );
    assert_eq!(
        Interface::Buffer.lookup_event(1).unwrap().name,
        "release"
    );
}

#[test]
fn display_fmt_uses_the_short_name() {
    let s = format!("{}", Interface::Surface);
    assert_eq!(s, "pmd_surface");
}
