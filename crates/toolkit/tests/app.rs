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
use toolkit::{App, ClientError, Interface, MessageHeader, ObjectId, WireError, HEADER_SIZE};

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
        if let Some(done) = sync_done_for(bytes) {
            self.inbound.push_back(done);
        }
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbound)
    }

    fn recv(&mut self) -> Vec<u8> {
        self.inbound.pop_front().unwrap_or_default()
    }
}

fn sync_done_for(request: &[u8]) -> Option<Vec<u8>> {
    let header = MessageHeader::decode(request).ok()?;
    if header.object_id != ObjectId::DISPLAY || header.opcode != 1 {
        return None;
    }
    let callback = ObjectId::new(u32::from_le_bytes(
        request.get(HEADER_SIZE..HEADER_SIZE + 4)?.try_into().ok()?,
    ));
    let payload = 0u32.to_le_bytes();
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    MessageHeader::try_new(callback, 1, payload.len(), 0)
        .ok()?
        .encode(&mut out[..HEADER_SIZE])
        .ok()?;
    out[HEADER_SIZE..].copy_from_slice(&payload);
    Some(out)
}

struct FragmentedCatalogConnection {
    inbound: VecDeque<Vec<u8>>,
    waits: usize,
}

struct BackpressuredCatalogConnection {
    outbound: VecDeque<u8>,
    inbound: VecDeque<Vec<u8>>,
    flush_calls: usize,
    write_waits: usize,
    read_waits: usize,
    recv_while_outbound_pending: bool,
}

impl BackpressuredCatalogConnection {
    fn new(catalog: Vec<u8>) -> Self {
        Self {
            outbound: VecDeque::new(),
            inbound: VecDeque::from([catalog]),
            flush_calls: 0,
            write_waits: 0,
            read_waits: 0,
            recv_while_outbound_pending: false,
        }
    }
}

impl Connection for BackpressuredCatalogConnection {
    fn send(&mut self, bytes: &[u8]) {
        self.outbound.extend(bytes.iter().copied());
        if let Some(done) = sync_done_for(bytes) {
            self.inbound.push_back(done);
        }
    }

    fn flush_outbound(&mut self) -> Result<(), i32> {
        self.flush_calls += 1;
        let written = self.outbound.len().min(7);
        self.outbound.drain(..written);
        Ok(())
    }

    fn outbound_pending(&self) -> bool {
        !self.outbound.is_empty()
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        self.outbound.drain(..).collect()
    }

    fn recv(&mut self) -> Vec<u8> {
        self.recv_while_outbound_pending |= self.outbound_pending();
        self.inbound.pop_front().unwrap_or_default()
    }

    fn wait(&mut self, _timeout: Option<std::time::Duration>) -> Result<(), i32> {
        if self.outbound_pending() {
            self.write_waits += 1;
        } else {
            self.read_waits += 1;
        }
        Ok(())
    }
}

impl FragmentedCatalogConnection {
    fn new(catalog: Vec<u8>) -> Self {
        let mut inbound = VecDeque::new();
        for byte in catalog {
            inbound.push_back(vec![byte]);
            inbound.push_back(Vec::new());
        }
        Self { inbound, waits: 0 }
    }
}

impl Connection for FragmentedCatalogConnection {
    fn send(&mut self, bytes: &[u8]) {
        let Some(done) = sync_done_for(bytes) else {
            return;
        };
        let callback_id = MessageHeader::decode(&done).unwrap().object_id;
        let delete_payload = callback_id.raw().to_le_bytes();
        let mut delete = vec![0u8; HEADER_SIZE + delete_payload.len()];
        MessageHeader::try_new(ObjectId::DISPLAY, 2, delete_payload.len(), 0)
            .unwrap()
            .encode(&mut delete[..HEADER_SIZE])
            .unwrap();
        delete[HEADER_SIZE..].copy_from_slice(&delete_payload);

        let remove_payload = 77u32.to_le_bytes();
        let mut remove = vec![0u8; HEADER_SIZE + remove_payload.len()];
        MessageHeader::try_new(REGISTRY_ID, 2, remove_payload.len(), 0)
            .unwrap()
            .encode(&mut remove[..HEADER_SIZE])
            .unwrap();
        remove[HEADER_SIZE..].copy_from_slice(&remove_payload);

        for byte in &done[..done.len() - 1] {
            self.inbound.push_back(vec![*byte]);
            self.inbound.push_back(Vec::new());
        }
        let mut boundary = vec![*done.last().unwrap()];
        // The callback marker shares a read with one complete ordinary event
        // and the prefix of another. App construction must preserve all bytes
        // after done, not merely an incomplete tail.
        boundary.extend_from_slice(&delete);
        boundary.extend_from_slice(&remove[..5]);
        self.inbound.push_back(boundary);
        self.inbound.push_back(remove[5..].to_vec());
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        Vec::new()
    }

    fn recv(&mut self) -> Vec<u8> {
        self.inbound.pop_front().unwrap_or_default()
    }

    fn wait(&mut self, _timeout: Option<std::time::Duration>) -> Result<(), i32> {
        self.waits += 1;
        Ok(())
    }
}

/// Build a single `pmd_registry.global(name, interface,
/// version)` event framed against a given registry object id,
/// ready to push through `LoopbackConnection::push_inbound`.
fn build_global_event(registry_id: ObjectId, name: u32, interface: &str, version: u32) -> Vec<u8> {
    let event = RegistryGlobal {
        name,
        interface: interface.to_string(),
        version,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header = MessageHeader::try_new(registry_id, 1 /* global */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

fn build_event(object_id: ObjectId, opcode: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    MessageHeader::try_new(object_id, opcode, payload.len(), 0)
        .unwrap()
        .encode(&mut out[..HEADER_SIZE])
        .unwrap();
    out[HEADER_SIZE..].copy_from_slice(payload);
    out
}

fn connected_app() -> App<LoopbackConnection> {
    let mut conn = LoopbackConnection::new();
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    conn.push_inbound(batch);
    App::connect(conn).unwrap()
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
fn fragmented_catalog_preserves_complete_and_partial_events_following_sync_done() {
    let mut catalog = Vec::new();
    catalog.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    catalog.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    catalog.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    catalog.extend(build_global_event(REGISTRY_ID, 4, "pmd_seat", 1));
    catalog.extend(build_global_event(REGISTRY_ID, 5, "pmd_shell_manager", 1));

    let mut app = App::connect_with_shell(FragmentedCatalogConnection::new(catalog)).unwrap();
    assert!(app.seat().is_some());
    assert!(app.pointer().is_some());
    assert!(app.keyboard().is_some());
    assert!(app.shell_manager().is_some());
    assert!(app.client().connection().waits > 0);

    // callback.done shared its final read with a complete display.delete_id
    // and five bytes of the following registry.global_remove header. App
    // retained both in exact order across construction.
    let events = app.dispatch().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].object_id, ObjectId::DISPLAY);
    assert_eq!(events[0].opcode, 2);
    assert_eq!(events[1].object_id, REGISTRY_ID);
    assert_eq!(events[1].opcode, 2);
    assert_eq!(events[1].payload, 77u32.to_le_bytes());
}

#[test]
fn registry_catalog_explicitly_flushes_backpressured_requests_before_recv() {
    let mut catalog = Vec::new();
    catalog.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    catalog.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    catalog.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));

    let app = App::connect(BackpressuredCatalogConnection::new(catalog))
        .expect("bounded flush/wait handshake must reach callback.done");
    let conn = app.client().connection();
    assert!(conn.flush_calls > 1, "seven-byte writes force a suffix");
    assert!(conn.write_waits > 0, "a retained suffix waits for FD_WRITE");
    assert_eq!(conn.read_waits, 0, "catalog bytes were already available");
    assert!(
        !conn.recv_while_outbound_pending,
        "registry receive must not race ahead of queued get_registry/sync bytes",
    );
}

#[test]
fn input_enabled_ordinary_app_binds_seat_without_shell_manager() {
    let mut conn = LoopbackConnection::new();
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    batch.extend(build_global_event(REGISTRY_ID, 4, "pmd_seat", 1));
    conn.push_inbound(batch);

    let mut app = App::connect_with_shell(conn).expect("ordinary input bootstrap should succeed");

    assert!(app.seat().is_some());
    assert!(app.pointer().is_some());
    assert!(app.keyboard().is_some());
    assert_eq!(app.shell_manager(), None);
    assert_eq!(app.client().get(app.seat().unwrap()), Some(Interface::Seat));
    assert_eq!(
        app.client().get(app.pointer().unwrap()),
        Some(Interface::Pointer),
    );
    assert_eq!(
        app.client().get(app.keyboard().unwrap()),
        Some(Interface::Keyboard),
    );
    assert_eq!(
        app.shell_manager_desktop_ready(),
        Err(ClientError::MissingGlobal("pmd_shell_manager")),
    );
}

#[test]
fn shell_manager_window_controls_emit_global_id_requests() {
    let mut conn = LoopbackConnection::new();
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    batch.extend(build_global_event(REGISTRY_ID, 5, "pmd_shell_manager", 1));
    conn.push_inbound(batch);

    let mut app = App::connect_with_shell(conn).expect("shell bootstrap should succeed");
    let shell_manager = app.shell_manager().expect("shell manager bound");
    let _ = app.client_mut().connection_mut().drain_outbound();

    app.shell_manager_focus_window(41).unwrap();
    app.shell_manager_minimize_window(42).unwrap();
    app.shell_manager_unminimize_window(43).unwrap();
    app.shell_manager_close_window(44).unwrap();
    app.shell_manager_set_work_area_bottom(32).unwrap();
    app.shell_manager_toggle_maximized_window(45).unwrap();
    app.shell_manager_desktop_ready().unwrap();

    let mut bytes = app.client_mut().connection_mut().drain_outbound();
    for (opcode, value) in [(2, 41u32), (4, 42), (5, 43), (3, 44), (6, 32), (7, 45)] {
        let header = MessageHeader::decode(&bytes).expect("framed shell-manager request");
        assert_eq!(header.object_id, shell_manager);
        assert_eq!(header.opcode, opcode);
        let length = header.length as usize;
        assert_eq!(&bytes[HEADER_SIZE..length], &value.to_le_bytes());
        bytes.drain(..length);
    }
    let ready = MessageHeader::decode(&bytes).expect("framed desktop-ready request");
    assert_eq!(ready.object_id, shell_manager);
    assert_eq!(ready.opcode, 8);
    assert_eq!(ready.length as usize, HEADER_SIZE);
    bytes.drain(..HEADER_SIZE);
    assert!(bytes.is_empty());
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
        let header =
            MessageHeader::try_new(REGISTRY_ID, 2 /* global_remove */, payload.len(), 0).unwrap();
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

#[test]
fn callback_done_retires_until_following_delete_id_while_remaining_observable() {
    let mut app = connected_app();
    let callback = app.client_mut().bind_new(Interface::Callback).unwrap();
    app.client_mut()
        .connection_mut()
        .push_inbound(build_event(callback, 1, &55u32.to_le_bytes()));

    let done = app.dispatch().unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!((done[0].object_id, done[0].opcode), (callback, 1));
    assert_eq!(done[0].payload, 55u32.to_le_bytes());
    assert!(app.client().is_retired(callback));
    assert_eq!(app.client().get(callback), None);

    app.client_mut().connection_mut().push_inbound(build_event(
        ObjectId::DISPLAY,
        2,
        &callback.raw().to_le_bytes(),
    ));
    let deleted = app.dispatch().unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(
        (deleted[0].object_id, deleted[0].opcode),
        (ObjectId::DISPLAY, 2)
    );
    assert!(!app.client().is_retired(callback));
    assert_eq!(app.client().get(callback), None);
}

#[test]
fn malformed_callback_done_does_not_retire_the_callback() {
    let mut app = connected_app();
    let callback = app.client_mut().bind_new(Interface::Callback).unwrap();
    app.client_mut()
        .connection_mut()
        .push_inbound(build_event(callback, 1, &[1, 2, 3, 4, 5]));

    assert_eq!(
        app.dispatch(),
        Err(ClientError::Wire(WireError::InvalidLength))
    );
    assert_eq!(app.client().get(callback), Some(Interface::Callback));
    assert!(!app.client().is_retired(callback));
}
