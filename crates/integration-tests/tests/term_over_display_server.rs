//! `term::Session` paired with a real `display_server::Server`
//! over a fully bidirectional in-memory loopback.
//!
//! Unlike `shell_over_display_server`, this suite exercises
//! the *ordinary app* path: a client that holds only
//! `Cap::DisplayClient`, binds `pmd_compositor` and `pmd_shm`,
//! and creates a single surface. No `pmd_shell_manager` — an
//! ordinary app is not allowed to touch the window-list API
//! (that's the desktop shell's privilege; see the
//! `ordinary_client_without_cap_shell_cannot_bind_shell_manager`
//! test in `shell_over_display_server`).
//!
//! Every byte crosses the real library boundaries on both
//! sides: `session.drain_outbound()` bytes are fed straight
//! into `server.dispatch_request`, and server-emitted events
//! from `drain_client_events` are fed back into
//! `session.pump`.

use abi::cap::{Cap, CapSet};
use display_proto::{wire::MessageHeader, Interface};
use display_server::Server as DisplayServerState;
use term::{Key, KeyFeedResult, Session, Terminal, TerminalOptions};
use toolkit::protocol::MemoryConnection;

/// Capability set an ordinary app holds on connect. A term
/// window is just an app — it does NOT hold `Cap::Shell`.
const APP_CAPS: CapSet = CapSet::from_caps(&[Cap::DisplayClient]);

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

fn pump_events_into_term(
    server: &mut DisplayServerState,
    server_client_id: display_server::ClientId,
    session: &mut Session<MemoryConnection>,
) -> term::SessionStep {
    let bytes = server
        .drain_client_events(server_client_id)
        .expect("client exists");
    if bytes.is_empty() {
        return term::SessionStep::default();
    }
    let (step, consumed) = session.pump(&bytes).expect("pump ok");
    assert_eq!(consumed, bytes.len(), "session consumed the full stream");
    step
}

fn make_session() -> Session<MemoryConnection> {
    let terminal = Terminal::new(TerminalOptions {
        max_lines: 128,
        banner: vec!["pmos term".to_string()],
        prompt: "> ".to_string(),
    });
    Session::new(MemoryConnection::new(), terminal)
}

/// Walk the session through the full preamble:
///   1. `session.start()` (get_registry).
///   2. Server advertises compositor + shm via emit_global.
///   3. Session auto-binds both.
///   4. Session calls `create_surface()`.
/// Returns the booted session, the server, and the
/// server-side client id so the caller can reach into the
/// server's client view for further assertions.
fn boot_term_session() -> (
    Session<MemoryConnection>,
    DisplayServerState,
    display_server::ClientId,
) {
    let mut session = make_session();
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept_with_caps(APP_CAPS);
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );
    server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();

    let registry_id = session.registry_id().unwrap();
    {
        let c = server.client_mut(server_client_id).unwrap();
        c.emit_global(registry_id, 1, "pmd_compositor", 1).unwrap();
        c.emit_global(registry_id, 2, "pmd_shm", 1).unwrap();
    }
    pump_events_into_term(&mut server, server_client_id, &mut session);
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    session.create_surface().unwrap();
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();

    (session, server, server_client_id)
}

#[test]
fn term_session_start_reaches_display_server_as_get_registry() {
    let mut session = make_session();
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept_with_caps(APP_CAPS);
    let bytes = session.drain_outbound();
    let dispatched = pump_requests_into_server(&mut server, server_client_id, bytes);
    assert_eq!(dispatched, 1);

    let journal = server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].interface, Interface::Display);
    assert_eq!(journal[0].opcode_name, "get_registry");

    let registry_id = session.registry_id().unwrap();
    assert_eq!(
        server.client(server_client_id).unwrap().get(registry_id),
        Some(Interface::Registry)
    );
}

#[test]
fn term_auto_binds_compositor_and_shm_but_not_shell_manager() {
    let mut session = make_session();
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept_with_caps(APP_CAPS);
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    let registry_id = session.registry_id().unwrap();
    // Server advertises all three — shell_manager should be
    // discovered but NOT bound.
    {
        let c = server.client_mut(server_client_id).unwrap();
        c.emit_global(registry_id, 1, "pmd_compositor", 1).unwrap();
        c.emit_global(registry_id, 2, "pmd_shm", 1).unwrap();
        c.emit_global(registry_id, 3, "pmd_shell_manager", 1).unwrap();
    }
    let step = pump_events_into_term(&mut server, server_client_id, &mut session);

    assert_eq!(step.discovered, vec![1, 2, 3]);
    assert!(step.bound.contains(&Interface::Compositor));
    assert!(step.bound.contains(&Interface::Shm));
    assert!(
        !step.bound.contains(&Interface::ShellManager),
        "a terminal app must NOT bind shell_manager"
    );
    assert_eq!(step.bound.len(), 2);

    // Ship the binds to the server; both should succeed
    // because they're not cap-gated.
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    assert!(session.interfaces_ready());
    assert!(!session.is_ready(), "surface not yet created");
    assert_eq!(session.bound(Interface::ShellManager), None);
}

#[test]
fn term_create_surface_reaches_server_as_compositor_create_surface() {
    let mut session = make_session();
    session.start().unwrap();

    let mut server = DisplayServerState::new();
    let server_client_id = server.accept_with_caps(APP_CAPS);
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    // Advertise compositor + shm so the session auto-binds.
    let registry_id = session.registry_id().unwrap();
    {
        let c = server.client_mut(server_client_id).unwrap();
        c.emit_global(registry_id, 1, "pmd_compositor", 1).unwrap();
        c.emit_global(registry_id, 2, "pmd_shm", 1).unwrap();
    }
    pump_events_into_term(&mut server, server_client_id, &mut session);
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );
    server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();

    // Create the surface.
    let surface_id = session.create_surface().unwrap();
    assert_eq!(session.surface_id(), Some(surface_id));
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    // Server's journal shows one new entry: create_surface
    // against the compositor object.
    let journal = server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].interface, Interface::Compositor);
    assert_eq!(journal[0].opcode_name, "create_surface");

    // Server's object table has the surface id the session
    // allocated.
    assert_eq!(
        server.client(server_client_id).unwrap().get(surface_id),
        Some(Interface::Surface)
    );

    // Now `is_ready` flips to true because interfaces are
    // bound AND a surface exists.
    assert!(session.is_ready());
}

#[test]
fn term_create_surface_twice_returns_surface_already_created() {
    let (mut session, _server, _id) = boot_term_session();
    let err = session.create_surface().unwrap_err();
    assert!(matches!(err, term::SessionError::SurfaceAlreadyCreated));
}

#[test]
fn term_create_surface_before_compositor_returns_compositor_not_bound() {
    let mut session = make_session();
    session.start().unwrap();
    // Compositor global has NOT been advertised yet — the
    // session has no compositor binding to point at.
    let err = session.create_surface().unwrap_err();
    assert!(matches!(err, term::SessionError::CompositorNotBound));
}

#[test]
fn term_commit_before_create_surface_returns_no_surface() {
    let mut session = make_session();
    session.start().unwrap();
    let err = session.commit().unwrap_err();
    assert!(matches!(err, term::SessionError::NoSurface));
}

#[test]
fn term_commit_after_create_surface_reaches_server_as_surface_commit() {
    let (mut session, mut server, server_client_id) = boot_term_session();
    let surface_id = session.surface_id().unwrap();

    session.commit().unwrap();
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    let journal = server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].interface, Interface::Surface);
    assert_eq!(journal[0].opcode_name, "commit");

    // The surface id the journal points at matches the
    // one the session allocated.
    let view = server.client(server_client_id).unwrap();
    assert_eq!(view.get(surface_id), Some(Interface::Surface));
}

#[test]
fn term_feed_key_drives_embedded_shell_without_touching_server() {
    let (mut session, mut server, server_client_id) = boot_term_session();

    // Typing `echo hi` and pressing Enter commits through
    // the embedded sh::Shell. No display-protocol traffic
    // is generated (the commit comes from surface.commit,
    // which the caller drives explicitly).
    for ch in "echo hi".chars() {
        session.feed_key(Key::Char(ch));
    }
    let result = session.feed_key(Key::Enter);
    let KeyFeedResult::Committed { output, line, .. } = result else {
        panic!("expected Committed, got {result:?}");
    };
    assert_eq!(line, "echo hi");
    assert_eq!(output.stdout, b"hi\n");

    // Nothing was sent on the wire — the server's journal
    // is still empty.
    let journal = server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();
    assert!(journal.is_empty());

    // Scrollback shows the banner, the committed input, and
    // the echo output.
    let snap = session.terminal().snapshot();
    assert!(snap.lines.iter().any(|l| l.text == "pmos term"));
    assert!(snap.lines.iter().any(|l| l.text == "> echo hi"));
    assert!(snap.lines.iter().any(|l| l.text == "hi"));
}

#[test]
fn term_full_cycle_eval_plus_commit() {
    // Type a command, evaluate it, then commit the surface.
    // Verifies the embedded shell and the display-protocol
    // path are both functional in one session.
    let (mut session, mut server, server_client_id) = boot_term_session();

    for ch in "help".chars() {
        session.feed_key(Key::Char(ch));
    }
    let _ = session.feed_key(Key::Enter);
    session.commit().unwrap();
    pump_requests_into_server(
        &mut server,
        server_client_id,
        session.drain_outbound(),
    );

    let journal = server
        .client_mut(server_client_id)
        .unwrap()
        .drain_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].opcode_name, "commit");

    let snap = session.terminal().snapshot();
    // `help` output includes a "builtins:" header plus
    // each builtin indented with two spaces.
    assert!(snap.lines.iter().any(|l| l.text == "builtins:"));
    assert!(snap.lines.iter().any(|l| l.text == "  echo"));
    assert!(snap.lines.iter().any(|l| l.text == "  exit"));
}

#[test]
fn term_start_twice_returns_already_started() {
    let mut session = make_session();
    session.start().unwrap();
    let err = session.start().unwrap_err();
    assert!(matches!(err, term::SessionError::AlreadyStarted));
}

#[test]
fn term_pump_before_start_returns_not_started() {
    let mut session = make_session();
    let err = session.pump(&[]).unwrap_err();
    assert!(matches!(err, term::SessionError::NotStarted));
}

// Silence the `MessageHeader` import — referenced via
// `display_proto::wire::MessageHeader` in `split_first_message`.
#[allow(dead_code)]
fn _keep_imports_honest(_h: MessageHeader) {}
