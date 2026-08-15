//! `shell::Session` paired with a real `display_server::Server`
//! over a **fully bidirectional** in-memory loopback.
//!
//! Every byte of every event and every request traverses
//! the real library code paths on both sides:
//!
//!   * Shell → server: `session.drain_outbound()` bytes
//!     are fed straight into `server.dispatch_request`.
//!   * Server → shell: `server.client_mut().emit_*`
//!     helpers build framed events into the client's
//!     pending queue, `server.drain_client_events`
//!     yields a flat byte buffer, and `session.pump`
//!     parses it.
//!
//! Nothing is hand-built. If any layer drifts — wire
//! format, interface tables, typed decoders on either
//! side — one of these tests fails loudly.

use abi::cap::{Cap, CapSet};
use display_proto::{wire::MessageHeader, Interface, ObjectId};
use display_server::{ClientError, Server as DisplayServerState, ServerError};
use shell::Session;
use toolkit::protocol::MemoryConnection;

/// Capability set the desktop shell holds when it
/// connects to the display server. `Cap::Shell` is the
/// gate for binding `pmd_shell_manager`; `Cap::DisplayClient`
/// is what every display client needs by convention.
const SHELL_CAPS: CapSet = CapSet::from_caps(&[Cap::Shell, Cap::DisplayClient]);

fn split_first_message(bytes: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let header = MessageHeader::decode(bytes).ok()?;
    let msg_len = header.length as usize;
    if bytes.len() < msg_len {
        return None;
    }
    Some((bytes[..msg_len].to_vec(), bytes[msg_len..].to_vec()))
}

fn pump_requests_into_server(
    server: &mut DisplayServerState,
    server_client_id: display_server::ClientId,
    mut bytes: Vec<u8>,
) -> usize {
    let mut n = 0;
    while let Some((msg, rest)) = split_first_message(&bytes) {
        server
            .dispatch_request(server_client_id, &msg)
            .unwrap_or_else(|e| panic!("dispatch {n}: {e:?}"));
        n += 1;
        bytes = rest;
    }
    assert!(bytes.is_empty(), "leftover bytes: {bytes:?}");
    n
}

/// Ship whatever the server has enqueued for `client_id`
/// into the paired shell session's pump, returning the
/// session's step summary. Exists as a helper so tests
/// read top-down.
fn pump_events_into_shell(
    server: &mut DisplayServerState,
    server_client_id: display_server::ClientId,
    session: &mut Session<MemoryConnection>,
) -> shell::SessionStep {
    let bytes = server
        .drain_client_events(server_client_id)
        .expect("client exists");
    if bytes.is_empty() {
        return shell::SessionStep::default();
    }
    let (step, consumed) = session.pump(&bytes).expect("pump ok");
    assert_eq!(consumed, bytes.len(), "session consumed the full stream");
    step
}

#[test]
fn shell_start_reaches_display_server_as_get_registry() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    let bytes = session.drain_outbound();
    let dispatched = pump_requests_into_server(&mut server, server_client_id, bytes);
    assert_eq!(dispatched, 1);

    // Server's journal: one get_registry against display.
    let journal = server.client_mut(server_client_id).unwrap().drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].interface, Interface::Display);
    assert_eq!(journal[0].opcode_name, "get_registry");
    // Server auto-installed the registry from the
    // payload.
    let registry_id = session.registry_id().unwrap();
    assert_eq!(
        server.client(server_client_id).unwrap().get(registry_id),
        Some(Interface::Registry)
    );
}

#[test]
fn shell_auto_bind_on_registry_global_reaches_the_server_as_registry_bind() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();

    // 1. Ship the shell's get_registry. The server's
    //    dispatcher auto-installs the registry object on
    //    the new_id the shell allocated.
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());
    server.client_mut(server_client_id).unwrap().drain_journal();

    // 2. Server advertises a compositor global via the
    //    new emit path. NO hand-built bytes: the server
    //    builds the event itself.
    let registry_id = session.registry_id().unwrap();
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_global(registry_id, 1, "pmd_compositor", 1)
        .unwrap();

    // 3. Shell pumps whatever the server enqueued. Its
    //    auto-bind fires, recording the compositor in
    //    known_globals and queueing a registry.bind
    //    request on its own outbound.
    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(step.bound, vec![Interface::Compositor]);

    // 4. Ship the shell's registry.bind back to the server.
    let bind_bytes = session.drain_outbound();
    assert!(!bind_bytes.is_empty());
    pump_requests_into_server(&mut server, server_client_id, bind_bytes);

    // 5. Server's journal picks up exactly one new entry
    //    (the bind). The object table has compositor at
    //    the id the shell chose.
    let journal = server.client_mut(server_client_id).unwrap().drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].interface, Interface::Registry);
    assert_eq!(journal[0].opcode_name, "bind");

    let compositor_id = session.bound(Interface::Compositor).unwrap();
    assert_eq!(
        server.client(server_client_id).unwrap().get(compositor_id),
        Some(Interface::Compositor)
    );
}

#[test]
fn shell_becomes_ready_after_both_compositor_and_shm_advertised() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    // Shell client connects with Cap::Shell so shell_manager
    // is allowed to bind.
    let server_client_id = server.accept_with_caps(SHELL_CAPS);
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());

    let registry_id = session.registry_id().unwrap();

    // Server advertises compositor first via the emit
    // path, shell auto-binds.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_global(registry_id, 1, "pmd_compositor", 1)
        .unwrap();
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());
    assert!(!session.is_ready(), "shm still missing");

    // Shm next.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_global(registry_id, 2, "pmd_shm", 1)
        .unwrap();
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());
    assert!(!session.is_ready(), "shell_manager still missing");

    // Finally shell_manager.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_global(registry_id, 3, "pmd_shell_manager", 1)
        .unwrap();
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());
    assert!(session.is_ready());

    // Server's object table has all three bindings at
    // the ids the shell chose.
    let compositor_id = session.bound(Interface::Compositor).unwrap();
    let shm_id = session.bound(Interface::Shm).unwrap();
    let sm_id = session.bound(Interface::ShellManager).unwrap();
    let view = server.client(server_client_id).unwrap();
    assert_eq!(view.get(compositor_id), Some(Interface::Compositor));
    assert_eq!(view.get(shm_id), Some(Interface::Shm));
    assert_eq!(view.get(sm_id), Some(Interface::ShellManager));
}

#[test]
fn shell_ignores_uninteresting_globals_but_still_records_them() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());

    let registry_id = session.registry_id().unwrap();
    // pmd_net is a global the v1 shell doesn't auto-bind.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_global(registry_id, 7, "pmd_net", 1)
        .unwrap();
    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(step.discovered, vec![7]);
    assert!(step.bound.is_empty());
    // It's in known_globals with interface == None (v1
    // Interface enum doesn't include pmd_net).
    let entry = session.known_globals().get(&7).unwrap();
    assert_eq!(entry.interface_name, "pmd_net");
    assert_eq!(entry.interface, None);
    // No new outbound bytes.
    assert!(session.drain_outbound().is_empty());
    assert!(!session.is_ready());
}

#[test]
fn shell_receives_multiple_globals_in_one_drain_and_binds_in_order() {
    // Server queues several globals back-to-back via
    // emit_global and then calls drain_client_events
    // once. The shell's push_received_with_payload parses
    // the whole batch in one pump and binds every
    // interesting interface.
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    // Shell-priv connection so the shell_manager bind
    // doesn't get cap-rejected.
    let server_client_id = server.accept_with_caps(SHELL_CAPS);
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());

    let registry_id = session.registry_id().unwrap();
    {
        let server_client = server.client_mut(server_client_id).unwrap();
        server_client
            .emit_global(registry_id, 1, "pmd_compositor", 1)
            .unwrap();
        server_client
            .emit_global(registry_id, 2, "pmd_shm", 1)
            .unwrap();
        server_client
            .emit_global(registry_id, 3, "pmd_shell_manager", 1)
            .unwrap();
        server_client
            .emit_global(registry_id, 4, "pmd_net", 1)
            .unwrap();
    }

    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(step.discovered, vec![1, 2, 3, 4]);
    assert!(step.bound.contains(&Interface::Compositor));
    assert!(step.bound.contains(&Interface::Shm));
    assert!(step.bound.contains(&Interface::ShellManager));
    assert_eq!(step.bound.len(), 3);

    // Shell fired three registry.bind requests — one for
    // each interesting interface. Ship them.
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());
    assert!(session.is_ready());
}

#[test]
fn server_emit_error_reaches_the_shell_as_protocol_error_notice() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());

    // Server emits pmd_display.error on the display
    // object. The shell's pump surfaces it as a
    // ProtocolErrorNotice in the step.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_error(ObjectId::new(3), 42, "malformed request")
        .unwrap();
    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(step.errors.len(), 1);
    assert_eq!(step.errors[0].object_id, ObjectId::new(3));
    assert_eq!(step.errors[0].code, 42);
    assert_eq!(step.errors[0].message, "malformed request");
}

#[test]
fn server_emit_global_remove_flips_known_global_live_to_false() {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept();
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());

    let registry_id = session.registry_id().unwrap();
    {
        let c = server.client_mut(server_client_id).unwrap();
        c.emit_global(registry_id, 5, "pmd_net", 1).unwrap();
        c.emit_global_remove(registry_id, 5).unwrap();
    }
    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(step.discovered, vec![5]);
    assert_eq!(step.removed, vec![5]);

    let entry = session.known_globals().get(&5).unwrap();
    assert!(!entry.live, "remove should flip live to false");
    assert_eq!(entry.interface_name, "pmd_net");
}

// Silence the `MessageHeader` import — only used in the
// `use` line that shipped before this slice repurposed it.
#[allow(dead_code)]
fn _keep_imports_honest(_h: MessageHeader) {}

// ---- Shell-manager + window list end-to-end ---------------------

/// Run the shell session through the full preamble:
///   1. start (display.get_registry)
///   2. server emits registry.global(pmd_shell_manager)
///   3. shell auto-binds shell_manager
///   4. shell sends shell_manager.subscribe_windows
///
/// Returns the booted session, the server, and the
/// server-side client id for the caller to reach into.
///
/// Uses `accept_with_caps(SHELL_CAPS)` because the shell
/// holds `Cap::Shell` — the cap-gate required to bind
/// `pmd_shell_manager`.
fn boot_shell_with_shell_manager() -> (
    Session<MemoryConnection>,
    DisplayServerState,
    display_server::ClientId,
) {
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept_with_caps(SHELL_CAPS);
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());
    server.client_mut(server_client_id).unwrap().drain_journal();

    // Server advertises shell_manager via emit_global.
    let registry_id = session.registry_id().unwrap();
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_global(registry_id, 1, "pmd_shell_manager", 1)
        .unwrap();
    pump_events_into_shell(&mut server, server_client_id, &mut session);

    // Shell auto-bound shell_manager. Ship the bind to
    // the server.
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());

    // Shell sends subscribe_windows.
    session.subscribe_windows().unwrap();
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());

    // Drain the server-side journal so the caller's
    // assertions only see what happens AFTER subscribe.
    server.client_mut(server_client_id).unwrap().drain_journal();

    (session, server, server_client_id)
}

#[test]
fn server_emit_window_created_reaches_the_shell_window_list() {
    let (mut session, mut server, server_client_id) = boot_shell_with_shell_manager();
    let shell_manager_id = session.bound(Interface::ShellManager).unwrap();

    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_window_created(shell_manager_id, 42, "term", "pmos.term")
        .unwrap();
    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(step.windows_created, vec![42]);

    let w = session.windows().get(&42).unwrap();
    assert_eq!(w.title, "term");
    assert_eq!(w.app_id, "pmos.term");
    assert!(!w.focused);
}

#[test]
fn full_window_lifecycle_round_trips_through_the_emit_path() {
    let (mut session, mut server, server_client_id) = boot_shell_with_shell_manager();
    let shell_manager_id = session.bound(Interface::ShellManager).unwrap();

    // 1. Create two windows.
    {
        let c = server.client_mut(server_client_id).unwrap();
        c.emit_window_created(shell_manager_id, 1, "term", "pmos.term")
            .unwrap();
        c.emit_window_created(shell_manager_id, 2, "edit", "pmos.edit")
            .unwrap();
    }
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(session.windows().len(), 2);

    // 2. Focus window 2.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_window_focused(shell_manager_id, 2)
        .unwrap();
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(session.focused_window(), Some(2));
    assert!(session.windows().get(&2).unwrap().focused);

    // 3. Rename window 1.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_window_title_changed(shell_manager_id, 1, "term: bash")
        .unwrap();
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(session.windows().get(&1).unwrap().title, "term: bash");

    // 4. Destroy window 2 (the focused one). Focus
    //    clears.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_window_destroyed(shell_manager_id, 2)
        .unwrap();
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert!(session.windows().get(&2).is_none());
    assert_eq!(session.focused_window(), None);
    assert_eq!(session.windows().len(), 1);
}

#[test]
fn shell_focus_window_request_reaches_the_server_dispatcher() {
    let (mut session, mut server, server_client_id) = boot_shell_with_shell_manager();

    session.focus_window(7).unwrap();
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());

    let journal = server.client_mut(server_client_id).unwrap().drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].interface, Interface::ShellManager);
    assert_eq!(journal[0].opcode_name, "focus_window");
    assert_eq!(journal[0].payload_len, 4);
}

#[test]
fn ordinary_client_without_cap_shell_cannot_bind_shell_manager() {
    // A client that connects with only Cap::DisplayClient
    // (i.e. an ordinary app, not the desktop shell) tries
    // to bind pmd_shell_manager. The server's bind
    // dispatch path rejects with PermissionDenied and
    // the new object is NOT installed in the client's
    // table. This is the empirical Principle II + spec §15
    // gate.
    let mut session = Session::new(MemoryConnection::new());
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    // Ordinary app: only DisplayClient, no Shell.
    let ordinary_caps = CapSet::from_caps(&[Cap::DisplayClient]);
    let server_client_id = server.accept_with_caps(ordinary_caps);
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());

    // Server advertises shell_manager via emit_global.
    let registry_id = session.registry_id().unwrap();
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_global(registry_id, 1, "pmd_shell_manager", 1)
        .unwrap();

    // Shell pumps the global, auto-binds, queues a
    // registry.bind request. (The client doesn't know
    // it'll be rejected — the cap check happens server-
    // side, on dispatch.)
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    let bind_bytes = session.drain_outbound();
    assert!(!bind_bytes.is_empty());

    // The shell THINKS it bound shell_manager because
    // the auto-bind fires on registry.global before the
    // server replies. Snapshot the id now so we can
    // assert the shell drops it after pumping the error.
    let stale_sm_id = session.bound(Interface::ShellManager).unwrap();

    // The server rejects the bind with PermissionDenied.
    let mut remaining = bind_bytes;
    let (msg, rest) = split_first_message(&remaining).unwrap();
    let err = server.dispatch_request(server_client_id, &msg).unwrap_err();
    match err {
        ServerError::Client(ClientError::PermissionDenied {
            interface,
            required,
            new_id,
        }) => {
            assert_eq!(interface, Interface::ShellManager);
            assert_eq!(required, Cap::Shell);
            assert_eq!(new_id, stale_sm_id);
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
    remaining = rest;
    assert!(remaining.is_empty(), "only one bind expected");

    // The server's object table for this client does NOT
    // contain a shell_manager binding at the new_id the
    // session chose.
    let server_view = server.client(server_client_id).unwrap();
    assert_eq!(server_view.get(stale_sm_id), None);

    // The server enqueued a pmd_display.error event for
    // the failed bind. Drain it through the shell.
    let step = pump_events_into_shell(&mut server, server_client_id, &mut session);
    // The shell observes the error.
    assert_eq!(step.errors.len(), 1);
    assert_eq!(step.errors[0].object_id, stale_sm_id);
    assert_eq!(
        step.errors[0].code,
        display_proto::error_code::PERMISSION_DENIED
    );
    assert!(step.errors[0].message.contains("pmd_shell_manager"));

    // AND the shell drops its local stale binding —
    // the asymmetry from the previous slice is gone.
    assert_eq!(session.bound(Interface::ShellManager), None);
    // The known_globals entry's bound_id is also cleared.
    let entry = session
        .known_globals()
        .values()
        .find(|e| e.interface == Some(Interface::ShellManager))
        .expect("shell_manager global was discovered");
    assert_eq!(entry.bound_id, None);
}

#[test]
fn shell_window_controls_dispatch_with_distinct_opcodes() {
    let (mut session, mut server, server_client_id) = boot_shell_with_shell_manager();

    session.close_window(3).unwrap();
    session.minimize_window(5).unwrap();
    session.toggle_maximized_window(7).unwrap();
    pump_requests_into_server(&mut server, server_client_id, session.drain_outbound());

    let journal = server.client_mut(server_client_id).unwrap().drain_journal();
    assert_eq!(journal.len(), 3);
    assert_eq!(journal[0].opcode_name, "close_window");
    assert_eq!(journal[1].opcode_name, "minimize_window");
    assert_eq!(journal[2].opcode_name, "toggle_maximized_window");
}

/// T177 verification: window_created events delivered after the
/// shell has subscribed land in the session's window list. The
/// catch-up replay-on-subscribe path is exercised inside
/// `boot_shell_with_shell_manager` (it sends subscribe_windows and
/// the server's `subscribe_windows_for` snapshots the existing
/// toplevels into the wire). This test pins the post-subscribe
/// streaming half of the contract — the same wire shape the replay
/// uses to deliver pre-existing windows.
#[test]
fn shell_session_window_list_reflects_window_created_event() {
    let (mut session, mut server, server_client_id) = boot_shell_with_shell_manager();
    let shell_manager_id = session.bound(Interface::ShellManager).unwrap();

    // Sanity: window list starts empty.
    assert!(session.windows().is_empty());

    // Server emits a window_created for window_id=42.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_window_created(shell_manager_id, 42, "term", "pmos.term")
        .unwrap();
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(session.windows().len(), 1);
    assert!(session.windows().contains_key(&42));

    // A second window_created adds to the list.
    server
        .client_mut(server_client_id)
        .unwrap()
        .emit_window_created(shell_manager_id, 7, "edit", "pmos.edit")
        .unwrap();
    pump_events_into_shell(&mut server, server_client_id, &mut session);
    assert_eq!(session.windows().len(), 2);
    assert!(session.windows().contains_key(&7));
}
