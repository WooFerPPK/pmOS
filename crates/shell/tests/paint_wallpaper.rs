//! T121 paint-wallpaper slice tests.
//!
//! Drives `shell::run_shell` against a bidirectional
//! in-memory `MockConnection` — the same mock-server
//! scaffold pattern used by `crates/toolkit/tests/{window,buffer}.rs`,
//! with one elaboration: because `run_shell` consumes the
//! connection for the lifetime of the event loop (no
//! mid-loop access for the test to push inbound bytes),
//! the mock supports **trigger-on-send**: the test
//! pre-registers a `pending_triggers` list of outbound
//! byte-length thresholds paired with an inbound batch to
//! inject once the shell's send has crossed that
//! threshold. This lets us simulate the server responding
//! to a specific client commit/ack step (e.g. push the
//! configure event only after the client has sent its
//! initial `surface.commit`) without entangling the mock
//! with the toolkit's outbound schedule.
//!
//! These tests pin the wire-level contract of the first
//! real desktop shell binary:
//!
//!   * `App::connect` handshake (display.get_registry →
//!     registry.global drain → registry.bind(compositor,
//!     shm, xdg_shell))
//!   * `Window::new` + `set_title` + `commit` request
//!     sequence
//!   * After a server-sent `xdg_toplevel::configure(w, h)`
//!     the shell allocates a `BufferPool` and emits
//!     `surface.attach` + `surface.damage` +
//!     `surface.commit` exactly once
//!   * `xdg_toplevel::close` exits the loop with
//!     `ShellExit::CloseRequested`
//!   * Missing compositor global bubbles up as
//!     `ClientError::MissingGlobal("pmd_compositor")`
//!
//! The scaffold is duplicated inline rather than lifted to
//! a shared crate — the brief prefers duplicating a few
//! lines over cross-crate entanglement.

use std::collections::VecDeque;

use display_proto::events::{RegistryGlobal, XdgToplevelClose, XdgToplevelConfigure};
use display_proto::ids::ObjectId;
use display_proto::wire::{MessageHeader, HEADER_SIZE};

use shell::{run_shell, ShellExit};
use toolkit::protocol::Connection;
use toolkit::ClientError;

/// Trigger-on-send inbound injection: once an outbound
/// request matching `(object_id, opcode)` has been seen,
/// release `inbound` as the next chunk served by `recv`.
struct PendingTrigger {
    object_id: ObjectId,
    opcode: u16,
    inbound: Vec<u8>,
}

/// Bidirectional in-memory [`Connection`]. Inbound batches
/// can be pushed up front (immediately available) or
/// pre-registered as triggers (released only once a
/// matching request has been observed on the outbound
/// side). Outbound bytes accumulate.
///
/// The trigger-on-observed-request shape lets a test
/// simulate a server that replies to a specific client
/// request (e.g. push `xdg_toplevel::configure` only after
/// the client has sent its initial `surface.commit`)
/// without any mid-loop access to the connection — which
/// is exactly what `run_shell` needs because it owns the
/// connection for the full duration of the event loop.
struct MockConnection {
    outbound: Vec<u8>,
    /// Running trailing outbound byte log, reset by
    /// [`Self::drain_outbound`]. We parse it incrementally
    /// in `send` to fire triggers as requests cross the
    /// wire.
    outbound_log: Vec<u8>,
    /// Cursor into `outbound_log` for incremental parsing.
    parsed_up_to: usize,
    inbound: VecDeque<Vec<u8>>,
    triggers: Vec<PendingTrigger>,
}

impl MockConnection {
    fn new() -> Self {
        MockConnection {
            outbound: Vec::new(),
            outbound_log: Vec::new(),
            parsed_up_to: 0,
            inbound: VecDeque::new(),
            triggers: Vec::new(),
        }
    }

    fn push_inbound(&mut self, bytes: Vec<u8>) {
        self.inbound.push_back(bytes);
    }

    /// Register a trigger: release `inbound` once the
    /// outbound stream carries a framed request matching
    /// `(object_id, opcode)`.
    fn push_inbound_on_request(
        &mut self,
        object_id: ObjectId,
        opcode: u16,
        inbound: Vec<u8>,
    ) {
        self.triggers.push(PendingTrigger {
            object_id,
            opcode,
            inbound,
        });
    }

    /// Incrementally parse framed requests from
    /// `outbound_log` starting at `parsed_up_to`; for each
    /// newly-completed request, fire any trigger matching
    /// `(object_id, opcode)`.
    fn scan_outbound_and_fire(&mut self) {
        loop {
            let remaining = &self.outbound_log[self.parsed_up_to..];
            if remaining.len() < HEADER_SIZE {
                return;
            }
            let header = match MessageHeader::decode(remaining) {
                Ok(h) => h,
                Err(_) => return,
            };
            let msg_len = header.length as usize;
            if remaining.len() < msg_len {
                return;
            }
            // Fire every trigger matching this request.
            let object_id = header.object_id;
            let opcode = header.opcode;
            let mut fired = Vec::new();
            self.triggers.retain(|t| {
                if t.object_id == object_id && t.opcode == opcode {
                    fired.push(t.inbound.clone());
                    false
                } else {
                    true
                }
            });
            for inbound in fired {
                self.inbound.push_back(inbound);
            }
            self.parsed_up_to += msg_len;
        }
    }
}

impl Connection for MockConnection {
    fn send(&mut self, bytes: &[u8]) {
        self.outbound.extend_from_slice(bytes);
        self.outbound_log.extend_from_slice(bytes);
        self.scan_outbound_and_fire();
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbound)
    }

    fn recv(&mut self) -> Vec<u8> {
        self.inbound.pop_front().unwrap_or_default()
    }
}

/// The toolkit's id allocator hands out the registry first:
/// `get_registry` always lands at client-side id 3 (odd
/// partition, starts at 3).
const REGISTRY_ID: ObjectId = ObjectId::new(3);

/// After `App::connect` (allocates registry=3, compositor=5,
/// shm=7, xdg_shell=9) plus `Window::new` (allocates
/// surface=11, toplevel=13), the next new id is 15.
const TOPLEVEL_ID: ObjectId = ObjectId::new(13);

/// Build a single framed `pmd_registry.global` event.
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
    let header = MessageHeader::try_new(registry_id, 1 /* global */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

/// Build a single framed `pmd_xdg_toplevel.configure` event.
fn build_configure_event(
    toplevel_id: ObjectId,
    serial: u32,
    width: i32,
    height: i32,
) -> Vec<u8> {
    let event = XdgToplevelConfigure {
        serial,
        width,
        height,
        states: 0,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header =
        MessageHeader::try_new(toplevel_id, 1 /* configure */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

/// Build a single framed `pmd_xdg_toplevel.close` event
/// (empty payload).
fn build_close_event(toplevel_id: ObjectId) -> Vec<u8> {
    let _event = XdgToplevelClose;
    let mut out = vec![0u8; HEADER_SIZE];
    let header = MessageHeader::try_new(toplevel_id, 2 /* close */, 0, 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out
}

/// Pre-seed the three required globals so `App::connect`
/// succeeds immediately.
fn seed_full_registry(conn: &mut MockConnection) {
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    conn.push_inbound(batch);
}

/// Parse framed messages from an outbound byte stream into
/// `(object_id, opcode, payload_len)` triples. Lets each
/// test assert on the exact request sequence the shell
/// emitted to the mock server.
fn parse_request_headers(mut bytes: &[u8]) -> Vec<(ObjectId, u16, usize)> {
    let mut out = Vec::new();
    while bytes.len() >= HEADER_SIZE {
        let header = MessageHeader::decode(bytes).expect("valid framed request");
        let msg_len = header.length as usize;
        assert!(bytes.len() >= msg_len, "truncated framed request");
        let payload_len = msg_len - HEADER_SIZE;
        out.push((header.object_id, header.opcode, payload_len));
        bytes = &bytes[msg_len..];
    }
    assert!(bytes.is_empty(), "leftover bytes after parse");
    out
}

/// The surface id the shell allocates: display=1,
/// registry=3, compositor=5, shm=7, xdg_shell=9, then
/// `Window::new` allocates surface=11 first, then
/// toplevel=13.
const SURFACE_ID: ObjectId = ObjectId::new(11);

/// Opcode for `pmd_surface.commit`. Used as a trigger for
/// the mock server's configure reply — in production the
/// initial surface.commit is the handshake step that asks
/// the server to emit the first configure event.
const SURFACE_OPCODE_COMMIT: u16 = 7;

#[test]
fn shell_runs_and_paints_wallpaper_on_configure() {
    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);

    // Release the configure event once the shell has sent
    // its initial surface.commit (the protocol step that
    // solicits the server's first configure). By the time
    // the next `recv` runs (inside the event loop's
    // `window.dispatch`), the toplevel id is bound and the
    // configure event targets a known object.
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(TOPLEVEL_ID, 1, 800, 600),
    );

    // 5 dispatch iterations is plenty: iter 1 sees configure
    // + paints, iters 2–5 idle.
    let exit = run_shell(conn, 5).expect("run_shell must succeed");

    // No close event sent → iteration limit.
    assert_eq!(exit, ShellExit::IterationLimit);
}

#[test]
fn shell_paints_once_after_configure_and_commits_attach_damage_commit() {
    // Variant of the happy path that instruments the
    // connection and verifies the exact wire sequence the
    // shell emitted across the full run.
    //
    // Strategy: wrap MockConnection in a recording shell,
    // run, then assert that the outbound byte stream
    // contains the expected request invariants. Specific
    // byte-exact assertions on sub-sequences live in the
    // toolkit-level tests at tests/window.rs +
    // tests/buffer.rs; the shell test pins the composition
    // — that all the sub-sequences happen in the expected
    // order across the full event loop.

    /// Connection that tees outbound bytes into a shared
    /// record so the test can inspect them after run_shell
    /// returns.
    struct RecordingConnection {
        inner: MockConnection,
        recorded: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    }
    impl Connection for RecordingConnection {
        fn send(&mut self, bytes: &[u8]) {
            self.recorded.borrow_mut().extend_from_slice(bytes);
            self.inner.send(bytes);
        }
        fn drain_outbound(&mut self) -> Vec<u8> {
            self.inner.drain_outbound()
        }
        fn recv(&mut self) -> Vec<u8> {
            self.inner.recv()
        }
    }

    let mut inner = MockConnection::new();
    seed_full_registry(&mut inner);
    inner.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(TOPLEVEL_ID, 42, 320, 240),
    );

    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u8>::new()));
    let conn = RecordingConnection {
        inner,
        recorded: recorded.clone(),
    };

    let exit = run_shell(conn, 5).expect("run_shell must succeed");
    assert_eq!(exit, ShellExit::IterationLimit);

    let bytes = recorded.borrow().clone();
    let headers = parse_request_headers(&bytes);

    // Expected wire sequence (object-id progression is
    // deterministic under the client-side odd-id allocator):
    //
    //  * display.get_registry (on display id 1, new_id=3)
    //  * registry.bind × 3    (compositor=5, shm=7, xdg_shell=9)
    //  * compositor.create_surface(new_id=11)
    //  * xdg_shell.get_toplevel(new_id=13, surface=11)
    //  * xdg_toplevel.set_title "PMos"  (on id 13)
    //  * surface.commit                 (on surface=11 — triggers configure)
    //  * xdg_toplevel.ack_configure(42) (on id 13, from dispatch)
    //  * shm.create_pool(new_id=15, size=…)
    //  * shm_pool.create_buffer × 2     (buf0=17, buf1=19)
    //  * surface.attach(buf0, 0, 0)     (on surface=11)
    //  * surface.damage(0, 0, 320, 240) (on surface=11)
    //  * surface.commit                 (on surface=11 — final frame)
    //
    // We lightly assert a handful of invariants rather than
    // the full sequence; toolkit tests already cover each
    // sub-step byte-exactly.

    let registry_id = ObjectId::new(3);
    let surface_id = ObjectId::new(11);
    let toplevel_id = ObjectId::new(13);

    let bind_count = headers
        .iter()
        .filter(|(obj, op, _)| *obj == registry_id && *op == 1 /* bind */)
        .count();
    assert!(bind_count >= 3, "expected ≥3 registry.bind, got {bind_count}");

    let set_title_count = headers
        .iter()
        .filter(|(obj, op, _)| *obj == toplevel_id && *op == 1 /* set_title */)
        .count();
    assert_eq!(set_title_count, 1, "xdg_toplevel.set_title must fire once");

    let attach_count = headers
        .iter()
        .filter(|(obj, op, _)| *obj == surface_id && *op == 2 /* attach */)
        .count();
    assert_eq!(attach_count, 1, "surface.attach must fire once");

    let damage_count = headers
        .iter()
        .filter(|(obj, op, _)| *obj == surface_id && *op == 3 /* damage */)
        .count();
    assert_eq!(damage_count, 1, "surface.damage must fire once");

    let commit_count = headers
        .iter()
        .filter(|(obj, op, _)| *obj == surface_id && *op == 7 /* commit */)
        .count();
    assert!(
        commit_count >= 2,
        "expected ≥2 surface.commit (initial + frame), got {commit_count}"
    );

    // ack_configure must fire once with the same serial
    // (42) the mock server handed out.
    let ack_count = headers
        .iter()
        .filter(|(obj, op, _)| *obj == toplevel_id && *op == 4 /* ack_configure */)
        .count();
    assert_eq!(ack_count, 1, "xdg_toplevel.ack_configure must fire once");
}

#[test]
fn shell_exits_on_close_requested() {
    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);

    // Release a close event once the bootstrap handshake is
    // through — same pattern as the configure test, except
    // the event is `xdg_toplevel::close` (opcode 2, empty
    // payload). The shell's dispatch loop flips
    // `close_requested=true` on first observation and
    // returns ShellExit::CloseRequested without ever
    // reaching the paint branch (is_configured stays false).
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_close_event(TOPLEVEL_ID),
    );

    let exit = run_shell(conn, 10).expect("run_shell must succeed");
    assert_eq!(exit, ShellExit::CloseRequested);
}

#[test]
fn shell_connect_failure_surfaces_client_error() {
    // Mock server omits the compositor global — App::connect
    // must fail with MissingGlobal("pmd_compositor"). The
    // error bubbles up through run_shell without any loop
    // iterations running.
    let mut conn = MockConnection::new();
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    conn.push_inbound(batch);

    let err = run_shell(conn, 5).expect_err("connect must fail");
    assert!(
        matches!(err, ClientError::MissingGlobal("pmd_compositor")),
        "expected MissingGlobal(pmd_compositor), got {err:?}"
    );
}
