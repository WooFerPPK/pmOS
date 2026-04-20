//! [`toolkit::App`] isolation tests.
//!
//! Drives the application-level facade against a
//! bidirectional mock connection that pre-seeds
//! `registry.global` events the server would have advertised
//! before `App::connect` runs its bind loop. The three
//! required globals (`pmd_compositor`, `pmd_shm`,
//! `pmd_xdg_shell`) are the v1 contract every app depends on
//! — these tests pin both the happy-path bind and the
//! missing-global rejection path, plus a dispatch round trip
//! proving `App::dispatch` surfaces post-bootstrap server
//! events to callers.

use display_proto::events::RegistryGlobal;
use std::collections::VecDeque;

use toolkit::protocol::Connection;
use toolkit::{App, ClientError, HEADER_SIZE, Interface, MessageHeader, ObjectId};

/// Bidirectional in-memory [`Connection`] for `App` tests.
/// Outbound bytes buffer the same way [`toolkit::
/// MemoryConnection`] does; inbound bytes are pre-queued by
/// the test and drained one batch at a time through
/// [`Connection::recv`].
#[derive(Default)]
struct LoopbackConnection {
    outbound: Vec<u8>,
    inbound: VecDeque<Vec<u8>>,
}

impl LoopbackConnection {
    fn new() -> Self {
        LoopbackConnection::default()
    }

    /// Push a batch of bytes onto the inbound queue. The
    /// next call to [`Connection::recv`] returns this batch.
    fn push_inbound(&mut self, bytes: Vec<u8>) {
        self.inbound.push_back(bytes);
    }
}

impl Connection for LoopbackConnection {
    fn send(&mut self, bytes: &[u8]) {
        self.outbound.extend_from_slice(bytes);
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbound)
    }

    fn recv(&mut self) -> Vec<u8> {
        self.inbound.pop_front().unwrap_or_default()
    }
}

/// Build a single `pmd_registry.global(name, interface,
/// version)` event framed against a given registry object id,
/// ready to push through `LoopbackConnection::push_inbound`.
fn build_global_event(
    registry_id: ObjectId,
    name: u32,
    interface: &str,
    version: u32,
) -> Vec<u8> {
    let event = RegistryGlobal {
        name,
        interface: interface.to_string(),
        version,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header = MessageHeader::try_new(registry_id, 1 /* global */, payload.len(), 0)
        .unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

/// The toolkit's id allocator hands the registry id out
/// first: `get_registry` always lands at raw id 3.
const REGISTRY_ID: ObjectId = ObjectId::new(3);

#[test]
fn app_connect_binds_required_globals() {
    let mut conn = LoopbackConnection::new();
    // Mock server advertises all three required globals
    // synchronously in one recv batch, just as a real server
    // would respond to `display.get_registry` by dumping its
    // current global set.
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    conn.push_inbound(batch);

    let app = App::connect(conn).expect("bootstrap should succeed");

    // Each accessor returns a non-zero id, and the three ids
    // are distinct (the allocator hands out sequential odd
    // client ids 5, 7, 9 after the registry took 3).
    assert_ne!(app.compositor().raw(), 0);
    assert_ne!(app.shm().raw(), 0);
    assert_ne!(app.xdg_shell().raw(), 0);
    assert_ne!(app.compositor(), app.shm());
    assert_ne!(app.shm(), app.xdg_shell());
    assert_ne!(app.compositor(), app.xdg_shell());

    // The underlying client has all four objects bound
    // (registry + three globals) on top of the pre-bound
    // display.
    let client = app.client();
    assert_eq!(client.object_count(), 5);
    assert_eq!(client.get(app.compositor()), Some(Interface::Compositor));
    assert_eq!(client.get(app.shm()), Some(Interface::Shm));
    assert_eq!(client.get(app.xdg_shell()), Some(Interface::XdgShell));
}

#[test]
fn app_connect_fails_when_compositor_missing() {
    let mut conn = LoopbackConnection::new();
    // Mock advertises only shm + xdg_shell — no compositor.
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 1, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_xdg_shell", 1));
    conn.push_inbound(batch);

    let err = match App::connect(conn) {
        Ok(_) => panic!("missing compositor must fail bootstrap"),
        Err(e) => e,
    };
    assert_eq!(err, ClientError::MissingGlobal("pmd_compositor"));
}

#[test]
fn app_dispatch_delivers_events() {
    let mut conn = LoopbackConnection::new();
    // The three globals for bootstrap.
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    conn.push_inbound(batch);

    let mut app = App::connect(conn).expect("bootstrap should succeed");

    // Post-bootstrap: seed a synthetic
    // `registry.global_remove(2)` event through the app's
    // own connection so the next `dispatch` drains it.
    let post_bootstrap = {
        let mut payload = Vec::new();
        payload.extend_from_slice(&2u32.to_le_bytes());
        let mut out = vec![0u8; HEADER_SIZE + payload.len()];
        let header = MessageHeader::try_new(
            REGISTRY_ID,
            2, /* global_remove */
            payload.len(),
            0,
        )
        .unwrap();
        header.encode(&mut out[..HEADER_SIZE]).unwrap();
        out[HEADER_SIZE..].copy_from_slice(&payload);
        out
    };
    // Reach through `client_mut().connection_mut()` to seed
    // the synthetic event into the loopback's inbound queue.
    app.client_mut()
        .connection_mut()
        .push_inbound(post_bootstrap);

    let events = app.dispatch().expect("dispatch must succeed");
    assert_eq!(events.len(), 1);
    let evt = &events[0];
    assert_eq!(evt.object_id, REGISTRY_ID);
    assert_eq!(evt.interface, Interface::Registry);
    assert_eq!(evt.opcode, 2);
    assert_eq!(evt.opcode_name, "global_remove");
    assert_eq!(evt.payload, 2u32.to_le_bytes());

    // A subsequent dispatch with nothing queued returns an
    // empty event list rather than blocking.
    let more = app.dispatch().expect("second dispatch must succeed");
    assert!(more.is_empty());
}
