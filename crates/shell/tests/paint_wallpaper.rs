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
use std::io::{self, Cursor, Read};
use std::path::Path;

use display_proto::events::{
    PointerButton, RegistryGlobal, ShellWindowSnapshotDone, XdgToplevelClose, XdgToplevelConfigure,
};
use display_proto::ids::ObjectId;
use display_proto::requests::{ShmPoolWriteRows, SurfaceAttach, SurfaceDamage};
use display_proto::wire::{MessageHeader, HEADER_SIZE};

use shell::launcher::{DesktopEntryScan, DesktopEntryScanBatch, DesktopEntryStore, LauncherError};
use shell::{
    run_desktop_shell_with_preferences, run_desktop_shell_with_runtimes_and_events,
    run_desktop_shell_with_runtimes_events_and_session, run_shell, DesktopEventSource,
    DesktopPreferenceRuntime, Launcher, LauncherClock, LauncherRuntime, LauncherSlot, MemoryStore,
    PreferenceClock, PreferenceSource, SessionFile, SessionFilesystem, SessionRuntime, ShellExit,
    Taskbar, WallpaperChoice, WallpaperSource, SESSION_SNAPSHOT_ID,
};
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
    fn push_inbound_on_request(&mut self, object_id: ObjectId, opcode: u16, inbound: Vec<u8>) {
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
        if let Some(done) = sync_done_for(bytes) {
            self.inbound.push_back(done);
        }
        self.scan_outbound_and_fire();
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
    let callback_id = ObjectId::new(u32::from_le_bytes(
        request.get(HEADER_SIZE..HEADER_SIZE + 4)?.try_into().ok()?,
    ));
    let payload = 0u32.to_le_bytes();
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    MessageHeader::try_new(callback_id, 1, payload.len(), 0)
        .ok()?
        .encode(&mut out[..HEADER_SIZE])
        .ok()?;
    out[HEADER_SIZE..].copy_from_slice(&payload);
    Some(out)
}

/// The toolkit's id allocator hands out the registry first:
/// `get_registry` always lands at client-side id 3 (odd
/// partition, starts at 3).
const REGISTRY_ID: ObjectId = ObjectId::new(3);

/// `display.sync` reserves callback=5 after registry=3. Required-global binds
/// then allocate compositor=7, shm=9, xdg_shell=11; `Window::new` allocates
/// surface=13 and toplevel=15.
const TOPLEVEL_ID: ObjectId = ObjectId::new(15);

/// Build a single framed `pmd_registry.global` event.
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

/// Build a single framed `pmd_xdg_toplevel.configure` event.
fn build_configure_event(toplevel_id: ObjectId, serial: u32, width: i32, height: i32) -> Vec<u8> {
    let event = XdgToplevelConfigure {
        serial,
        width,
        height,
        states: 0,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header = MessageHeader::try_new(toplevel_id, 1 /* configure */, payload.len(), 0).unwrap();
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

fn build_pointer_button_event(
    pointer_id: ObjectId,
    surface_id: ObjectId,
    serial: u32,
    x: i32,
    y: i32,
) -> Vec<u8> {
    let event = PointerButton {
        serial,
        surface_id,
        x,
        y,
        button: 1,
        state: display_proto::events::pointer_button_state::PRESSED,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    MessageHeader::try_new(pointer_id, 2 /* button */, payload.len(), 0)
        .unwrap()
        .encode(&mut out[..HEADER_SIZE])
        .unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
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

/// Registry advertised to the capability-authenticated desktop shell.
fn seed_desktop_registry(conn: &mut MockConnection) {
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    batch.extend(build_global_event(REGISTRY_ID, 4, "pmd_seat", 1));
    batch.extend(build_global_event(REGISTRY_ID, 5, "pmd_shell_manager", 1));
    conn.push_inbound(batch);
}

fn seed_desktop_registry_v2(conn: &mut MockConnection) {
    let mut batch = Vec::new();
    batch.extend(build_global_event(REGISTRY_ID, 1, "pmd_compositor", 1));
    batch.extend(build_global_event(REGISTRY_ID, 2, "pmd_shm", 1));
    batch.extend(build_global_event(REGISTRY_ID, 3, "pmd_xdg_shell", 1));
    batch.extend(build_global_event(REGISTRY_ID, 4, "pmd_seat", 1));
    batch.extend(build_global_event(REGISTRY_ID, 5, "pmd_shell_manager", 2));
    conn.push_inbound(batch);
}

fn build_snapshot_done_event(shell_manager: ObjectId) -> Vec<u8> {
    let event = ShellWindowSnapshotDone {
        snapshot_id: SESSION_SNAPSHOT_ID,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    MessageHeader::try_new(shell_manager, 7, payload.len(), 0)
        .unwrap()
        .encode(&mut out[..HEADER_SIZE])
        .unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
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

fn attached_buffers(mut bytes: &[u8], surface_id: ObjectId) -> Vec<ObjectId> {
    let mut buffers = Vec::new();
    while bytes.len() >= HEADER_SIZE {
        let header = MessageHeader::decode(bytes).expect("valid framed request");
        let msg_len = header.length as usize;
        assert!(bytes.len() >= msg_len, "truncated framed request");
        if header.object_id == surface_id && header.opcode == 2 {
            let attach = SurfaceAttach::decode(&bytes[HEADER_SIZE..msg_len])
                .expect("surface.attach payload");
            buffers.push(attach.buffer_id);
        }
        bytes = &bytes[msg_len..];
    }
    assert!(bytes.is_empty(), "leftover bytes after parse");
    buffers
}

fn surface_damages(mut bytes: &[u8], surface_id: ObjectId) -> Vec<SurfaceDamage> {
    let mut damages = Vec::new();
    while bytes.len() >= HEADER_SIZE {
        let header = MessageHeader::decode(bytes).expect("valid framed request");
        let msg_len = header.length as usize;
        assert!(bytes.len() >= msg_len, "truncated framed request");
        if header.object_id == surface_id && header.opcode == 3 {
            damages.push(
                SurfaceDamage::decode(&bytes[HEADER_SIZE..msg_len])
                    .expect("surface.damage payload"),
            );
        }
        bytes = &bytes[msg_len..];
    }
    assert!(bytes.is_empty(), "leftover bytes after parse");
    damages
}

/// The surface id the shell allocates: display=1,
/// registry=3, sync callback=5, compositor=7, shm=9, xdg_shell=11, then
/// `Window::new` allocates surface=13 first, then toplevel=15.
const SURFACE_ID: ObjectId = ObjectId::new(13);

/// Opcode for `pmd_surface.commit`. Used as a trigger for
/// the mock server's configure reply — in production the
/// initial surface.commit is the handshake step that asks
/// the server to emit the first configure event.
const SURFACE_OPCODE_COMMIT: u16 = 7;

fn never_spawn(_path: &str) -> i32 {
    -1
}

fn one_pixel_wallpaper_png() -> Vec<u8> {
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, 1, 1);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("encode header")
            .write_image_data(&[0x21, 0x43, 0x65])
            .expect("encode pixel");
    }
    encoded
}

struct StepWallpaperSource(Option<Vec<u8>>);

impl WallpaperSource for StepWallpaperSource {
    fn read(&mut self, _choice: WallpaperChoice) -> io::Result<Option<Vec<u8>>> {
        panic!("production-shaped wallpaper source must use stepwise open")
    }

    fn open(&mut self, _choice: WallpaperChoice) -> io::Result<Option<Box<dyn Read>>> {
        Ok(self
            .0
            .take()
            .map(|bytes| Box::new(Cursor::new(bytes)) as Box<dyn Read>))
    }
}

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
    //  * display.sync callback=5
    //  * registry.bind × 3    (compositor=7, shm=9, xdg_shell=11)
    //  * compositor.create_surface(new_id=13)
    //  * xdg_shell.get_toplevel(new_id=15, surface=13)
    //  * xdg_toplevel.set_title "PMos"  (on id 15)
    //  * surface.commit                 (on surface=13 — triggers configure)
    //  * xdg_toplevel.ack_configure(42) (on id 15, from dispatch)
    //  * shm.create_pool(new_id=17, size=…)
    //  * shm_pool.create_buffer × 2     (buf0=19, buf1=21)
    //  * surface.attach(buf0, 0, 0)     (on surface=13)
    //  * surface.damage(0, 0, 320, 240) (on surface=13)
    //  * surface.commit                 (on surface=13 — final frame)
    //
    // We lightly assert a handful of invariants rather than
    // the full sequence; toolkit tests already cover each
    // sub-step byte-exactly.

    let registry_id = ObjectId::new(3);
    let surface_id = ObjectId::new(13);
    let toplevel_id = ObjectId::new(15);

    let bind_count = headers
        .iter()
        .filter(
            |(obj, op, _)| *obj == registry_id && *op == 1, /* bind */
        )
        .count();
    assert!(
        bind_count >= 3,
        "expected ≥3 registry.bind, got {bind_count}"
    );

    let set_title_count = headers
        .iter()
        .filter(
            |(obj, op, _)| *obj == toplevel_id && *op == 1, /* set_title */
        )
        .count();
    assert_eq!(set_title_count, 1, "xdg_toplevel.set_title must fire once");

    let attach_count = headers
        .iter()
        .filter(
            |(obj, op, _)| *obj == surface_id && *op == 2, /* attach */
        )
        .count();
    assert_eq!(attach_count, 1, "surface.attach must fire once");

    let damage_count = headers
        .iter()
        .filter(
            |(obj, op, _)| *obj == surface_id && *op == 3, /* damage */
        )
        .count();
    assert_eq!(damage_count, 1, "surface.damage must fire once");

    let commit_count = headers
        .iter()
        .filter(
            |(obj, op, _)| *obj == surface_id && *op == 7, /* commit */
        )
        .count();
    assert!(
        commit_count >= 2,
        "expected ≥2 surface.commit (initial + frame), got {commit_count}"
    );

    // ack_configure must fire once with the same serial
    // (42) the mock server handed out.
    let ack_count = headers
        .iter()
        .filter(
            |(obj, op, _)| *obj == toplevel_id && *op == 4, /* ack_configure */
        )
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
fn desktop_dispatches_close_before_draining_ready_filesystem_work() {
    struct Preference;
    impl PreferenceSource for Preference {
        fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }
    struct Clock;
    impl PreferenceClock for Clock {
        fn monotonic_ms(&mut self) -> u64 {
            0
        }
        fn unix_seconds(&mut self) -> i64 {
            0
        }
    }
    impl LauncherClock for Clock {
        fn elapsed(&mut self) -> std::time::Duration {
            std::time::Duration::ZERO
        }
    }
    struct ReadyFilesystemWork;
    impl DesktopEventSource for ReadyFilesystemWork {
        fn drain(&mut self) -> shell::DesktopWake {
            panic!("close dispatch must return before filesystem work")
        }
        fn event_driven(&self) -> bool {
            true
        }
    }

    let mut connection = MockConnection::new();
    seed_desktop_registry(&mut connection);
    let surface = ObjectId::new(21);
    let toplevel = ObjectId::new(23);
    connection.push_inbound_on_request(surface, SURFACE_OPCODE_COMMIT, build_close_event(toplevel));
    let preferences = DesktopPreferenceRuntime::new(Preference, Clock);
    let launcher = LauncherRuntime::new(Launcher::new(Box::new(MemoryStore::new())), Clock);
    let exit = run_desktop_shell_with_runtimes_and_events(
        connection,
        1,
        Taskbar::new(0, 0),
        launcher,
        never_spawn,
        || {},
        preferences,
        ReadyFilesystemWork,
    )
    .expect("desktop close");
    assert_eq!(exit, ShellExit::CloseRequested);
}

#[test]
fn event_driven_catalog_progress_bypasses_wait_and_reports_first_publication_once() {
    struct TwoEntryStore;
    struct TwoEntryScan {
        step: u8,
    }

    impl DesktopEntryStore for TwoEntryStore {
        fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError> {
            Ok(Vec::new())
        }

        fn begin_scan(&mut self) -> Result<Option<Box<dyn DesktopEntryScan>>, LauncherError> {
            Ok(Some(Box::new(TwoEntryScan { step: 0 })))
        }
    }

    impl DesktopEntryScan for TwoEntryScan {
        fn step(&mut self) -> Result<DesktopEntryScanBatch, LauncherError> {
            let (id, name, exec, complete) = match self.step {
                0 => ("terminal", "Terminal", "/bin/term", false),
                1 => ("edit", "Edit", "/bin/edit", true),
                _ => panic!("scan stepped after its complete batch"),
            };
            self.step += 1;
            Ok(DesktopEntryScanBatch {
                entries: vec![(
                    id.to_string(),
                    format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n"),
                )],
                complete,
            })
        }
    }

    struct Preference;
    impl PreferenceSource for Preference {
        fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    #[derive(Clone, Copy)]
    struct Clock;
    impl PreferenceClock for Clock {
        fn monotonic_ms(&mut self) -> u64 {
            0
        }

        fn unix_seconds(&mut self) -> i64 {
            0
        }
    }
    impl LauncherClock for Clock {
        fn elapsed(&mut self) -> std::time::Duration {
            std::time::Duration::ZERO
        }
    }

    struct WaitRecordingConnection {
        inner: MockConnection,
        waits: std::rc::Rc<std::cell::Cell<usize>>,
    }
    impl Connection for WaitRecordingConnection {
        fn send(&mut self, bytes: &[u8]) {
            self.inner.send(bytes);
        }

        fn drain_outbound(&mut self) -> Vec<u8> {
            self.inner.drain_outbound()
        }

        fn recv(&mut self) -> Vec<u8> {
            self.inner.recv()
        }

        fn wait_with(
            &mut self,
            _additional: &[toolkit::WaitFd],
            _timeout: Option<std::time::Duration>,
        ) -> Result<(), i32> {
            self.waits.set(self.waits.get() + 1);
            Ok(())
        }
    }

    struct RecordingEvents {
        waits: std::rc::Rc<std::cell::Cell<usize>>,
        publications: std::rc::Rc<std::cell::RefCell<Vec<(usize, usize)>>>,
        request_follow_up_scan: bool,
    }
    impl DesktopEventSource for RecordingEvents {
        fn drain(&mut self) -> shell::DesktopWake {
            shell::DesktopWake {
                launcher: core::mem::take(&mut self.request_follow_up_scan),
                ..shell::DesktopWake::default()
            }
        }

        fn event_driven(&self) -> bool {
            true
        }

        fn catalog_published(&mut self, entry_count: usize) {
            self.publications
                .borrow_mut()
                .push((entry_count, self.waits.get()));
            self.request_follow_up_scan = true;
        }
    }

    let mut inner = MockConnection::new();
    seed_desktop_registry(&mut inner);
    let waits = std::rc::Rc::new(std::cell::Cell::new(0));
    let publications = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let connection = WaitRecordingConnection {
        inner,
        waits: waits.clone(),
    };
    let launcher = LauncherRuntime::new(Launcher::new_stepwise(Box::new(TwoEntryStore)), Clock);
    let preferences = DesktopPreferenceRuntime::new(Preference, Clock);
    let events = RecordingEvents {
        waits: waits.clone(),
        publications: publications.clone(),
        request_follow_up_scan: false,
    };

    let exit = run_desktop_shell_with_runtimes_and_events(
        connection,
        9,
        Taskbar::new(0, 0),
        launcher,
        never_spawn,
        || {},
        preferences,
        events,
    )
    .expect("event-driven desktop loop");

    assert_eq!(exit, ShellExit::IterationLimit);
    assert_eq!(
        publications.borrow().as_slice(),
        &[(2, 0)],
        "the first real snapshot publishes once before any park; the follow-up scan is silent",
    );
    assert!(
        waits.get() > 0,
        "the loop may park only after the bounded catalog work completes"
    );
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

#[test]
fn shell_with_taskbar_runs_to_iteration_limit_without_panicking() {
    // T121 follow-up: run_shell_with_taskbar wraps the
    // wallpaper paint with a Taskbar paint pass. This test
    // pins that the additional paint doesn't break the
    // happy-path event loop: the shell still emits its
    // expected surface.commit sequence and exits via the
    // iteration cap (no close event).
    use shell::{run_shell_with_taskbar, Taskbar};

    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(TOPLEVEL_ID, 1, 800, 600),
    );

    // Pre-populate a taskbar with one entry — exercises the
    // entry-paint path inside the loop.
    let mut taskbar = Taskbar::new(0, 0);
    taskbar.add_window(7, "Term", "pmos.term");
    let exit = run_shell_with_taskbar(conn, 5, taskbar).expect("must succeed");
    assert_eq!(exit, ShellExit::IterationLimit);
}

#[test]
fn production_initial_wallpaper_settles_before_two_buffer_seed_and_typed_ready() {
    struct IncrementalConnection {
        inner: MockConnection,
        recorded: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        uploads_this_dispatch: usize,
        max_uploads: std::rc::Rc<std::cell::Cell<usize>>,
        outbound_pending: std::rc::Rc<std::cell::Cell<bool>>,
        surface: ObjectId,
        turn: std::rc::Rc<std::cell::Cell<usize>>,
        upload_turns: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
        commit_turns: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
        wait_turns: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
    }

    impl Connection for IncrementalConnection {
        fn send(&mut self, bytes: &[u8]) {
            if bytes.len() > 1_024 {
                self.uploads_this_dispatch += 1;
                self.max_uploads
                    .set(self.max_uploads.get().max(self.uploads_this_dispatch));
                self.upload_turns.borrow_mut().push(self.turn.get());
            }
            if MessageHeader::decode(bytes).is_ok_and(|header| {
                header.object_id == self.surface && header.opcode == SURFACE_OPCODE_COMMIT
            }) {
                self.commit_turns.borrow_mut().push(self.turn.get());
            }
            self.recorded.borrow_mut().extend_from_slice(bytes);
            self.inner.send(bytes);
            self.outbound_pending.set(true);
        }

        fn flush_outbound(&mut self) -> Result<(), i32> {
            self.outbound_pending.set(false);
            Ok(())
        }

        fn outbound_pending(&self) -> bool {
            self.outbound_pending.get()
        }

        fn incremental_uploads(&self) -> bool {
            true
        }

        fn drain_outbound(&mut self) -> Vec<u8> {
            self.outbound_pending.set(false);
            self.inner.drain_outbound()
        }

        fn recv(&mut self) -> Vec<u8> {
            self.uploads_this_dispatch = 0;
            self.inner.recv()
        }

        fn wait_with(
            &mut self,
            _additional: &[toolkit::WaitFd],
            _timeout: Option<std::time::Duration>,
        ) -> Result<(), i32> {
            self.wait_turns.borrow_mut().push(self.turn.get());
            Ok(())
        }
    }

    struct EmptyCatalogStore;
    struct EmptyCatalogScan {
        complete: bool,
    }

    impl DesktopEntryStore for EmptyCatalogStore {
        fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError> {
            Ok(Vec::new())
        }

        fn begin_scan(&mut self) -> Result<Option<Box<dyn DesktopEntryScan>>, LauncherError> {
            Ok(Some(Box::new(EmptyCatalogScan { complete: false })))
        }
    }

    impl DesktopEntryScan for EmptyCatalogScan {
        fn step(&mut self) -> Result<DesktopEntryScanBatch, LauncherError> {
            assert!(!self.complete, "scan stepped after its complete batch");
            self.complete = true;
            Ok(DesktopEntryScanBatch {
                entries: Vec::new(),
                complete: true,
            })
        }
    }

    struct StableSource;
    impl PreferenceSource for StableSource {
        fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
            Ok(Some(b"[theme]\nname = \"light\"\n".to_vec()))
        }
    }

    struct FrozenClock;
    impl PreferenceClock for FrozenClock {
        fn monotonic_ms(&mut self) -> u64 {
            0
        }

        fn unix_seconds(&mut self) -> i64 {
            1_768_478_400
        }
    }
    impl LauncherClock for FrozenClock {
        fn elapsed(&mut self) -> std::time::Duration {
            std::time::Duration::ZERO
        }
    }

    type SharedReadySnapshots =
        std::rc::Rc<std::cell::RefCell<Vec<(usize, bool, usize, usize, usize)>>>;

    struct RecordingEvents {
        recorded: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        outbound_pending: std::rc::Rc<std::cell::Cell<bool>>,
        surface: ObjectId,
        shell_manager: ObjectId,
        catalog_snapshots: std::rc::Rc<std::cell::RefCell<Vec<(usize, usize)>>>,
        ready_snapshots: SharedReadySnapshots,
        turn: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl DesktopEventSource for RecordingEvents {
        fn drain(&mut self) -> shell::DesktopWake {
            self.turn.set(self.turn.get() + 1);
            shell::DesktopWake::default()
        }

        fn event_driven(&self) -> bool {
            true
        }

        fn catalog_published(&mut self, entry_count: usize) {
            self.catalog_snapshots.borrow_mut().push((
                entry_count,
                attached_buffers(&self.recorded.borrow(), self.surface).len(),
            ));
        }

        fn desktop_ready(&mut self) {
            let recorded = self.recorded.borrow();
            let requests = parse_request_headers(&recorded);
            let commits = requests
                .iter()
                .filter(|(object_id, opcode, _)| {
                    *object_id == self.surface && *opcode == SURFACE_OPCODE_COMMIT
                })
                .count();
            let ready_requests = requests
                .iter()
                .filter(|(object_id, opcode, payload_len)| {
                    *object_id == self.shell_manager && *opcode == 8 && *payload_len == 0
                })
                .count();
            self.ready_snapshots.borrow_mut().push((
                attached_buffers(&recorded, self.surface).len(),
                self.outbound_pending.get(),
                commits,
                ready_requests,
                self.turn.get(),
            ));
        }
    }

    let mut inner = MockConnection::new();
    seed_desktop_registry(&mut inner);
    let surface = ObjectId::new(21);
    let toplevel = ObjectId::new(23);
    let shell_manager = ObjectId::new(19);
    inner.push_inbound_on_request(
        surface,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(toplevel, 1, 128, 128),
    );
    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let max_uploads = std::rc::Rc::new(std::cell::Cell::new(0));
    let outbound_pending = std::rc::Rc::new(std::cell::Cell::new(false));
    let turn = std::rc::Rc::new(std::cell::Cell::new(0));
    let upload_turns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let commit_turns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let wait_turns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let connection = IncrementalConnection {
        inner,
        recorded: recorded.clone(),
        uploads_this_dispatch: 0,
        max_uploads: max_uploads.clone(),
        outbound_pending: outbound_pending.clone(),
        surface,
        turn: turn.clone(),
        upload_turns: upload_turns.clone(),
        commit_turns: commit_turns.clone(),
        wait_turns: wait_turns.clone(),
    };
    let preferences = DesktopPreferenceRuntime::new(StableSource, FrozenClock);
    let launcher = LauncherRuntime::new(
        Launcher::new_stepwise(Box::new(EmptyCatalogStore)),
        FrozenClock,
    );
    let catalog_snapshots = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let ready_snapshots = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let events = RecordingEvents {
        recorded: recorded.clone(),
        outbound_pending: outbound_pending.clone(),
        surface,
        shell_manager,
        catalog_snapshots: catalog_snapshots.clone(),
        ready_snapshots: ready_snapshots.clone(),
        turn: turn.clone(),
    };

    let exit = shell::paint::run_desktop_shell_with_runtimes_events_and_wallpaper_source(
        connection,
        24,
        Taskbar::new(0, 0),
        launcher,
        never_spawn,
        || {},
        preferences,
        events,
        StepWallpaperSource(Some(one_pixel_wallpaper_png())),
    )
    .expect("incremental desktop loop");
    assert_eq!(exit, ShellExit::IterationLimit);
    assert_eq!(max_uploads.get(), 1, "one 24 KiB upload per dispatch turn");
    let attached = attached_buffers(&recorded.borrow(), surface);
    assert_eq!(attached.len(), 2, "both local/server slots are seeded");
    assert_ne!(attached[0], attached[1]);
    assert_eq!(
        catalog_snapshots.borrow().as_slice(),
        &[(0, 0)],
        "a successfully published empty catalog is ready, but publication alone is insufficient",
    );
    let ready_snapshots = ready_snapshots.borrow();
    assert_eq!(ready_snapshots.len(), 1, "desktop-ready is one-shot");
    let (ready_buffers, ready_outbound, ready_commits, ready_requests_at_callback, ready_turn) =
        ready_snapshots[0];
    assert_eq!(
        (
            ready_buffers,
            ready_outbound,
            ready_commits,
            ready_requests_at_callback,
        ),
        (2, true, 3, 1),
        "readiness follows both final buffers; only its typed fence is then queued",
    );
    const INITIAL_WALLPAPER_TERMINAL_TURN: usize = 4;
    assert!(
        upload_turns
            .borrow()
            .iter()
            .all(|turn| *turn >= INITIAL_WALLPAPER_TERMINAL_TURN),
        "no framebuffer bytes may be uploaded while the initial wallpaper is pending",
    );
    let commit_turns = commit_turns.borrow();
    assert_eq!(
        commit_turns.len(),
        3,
        "one configure solicitation plus exactly two final framebuffer commits",
    );
    assert_eq!(commit_turns[0], 0, "the pre-loop configure solicitation");
    assert!(
        commit_turns[1..]
            .iter()
            .all(|turn| *turn >= INITIAL_WALLPAPER_TERMINAL_TURN),
        "no framebuffer commit may land while the initial wallpaper is pending",
    );
    let wait_turns = wait_turns.borrow();
    assert!(
        !wait_turns.is_empty() && wait_turns.iter().all(|turn| *turn >= ready_turn),
        "bounded wallpaper/upload work must finish before the settled loop parks instead of spinning",
    );
    let requests = parse_request_headers(&recorded.borrow());
    let commits = requests
        .iter()
        .filter(|(object_id, opcode, _)| *object_id == surface && *opcode == SURFACE_OPCODE_COMMIT)
        .count();
    assert_eq!(
        commits, 3,
        "typed readiness must not manufacture an unchanged surface commit",
    );
    let ready_requests = requests
        .iter()
        .filter(|(object_id, opcode, payload_len)| {
            *object_id == shell_manager && *opcode == 8 && *payload_len == 0
        })
        .count();
    assert_eq!(ready_requests, 1, "desktop-ready fence request is one-shot");
    let last_commit = requests
        .iter()
        .rposition(|(object_id, opcode, _)| {
            *object_id == surface && *opcode == SURFACE_OPCODE_COMMIT
        })
        .expect("initial desktop committed");
    let ready_request = requests
        .iter()
        .position(|(object_id, opcode, payload_len)| {
            *object_id == shell_manager && *opcode == 8 && *payload_len == 0
        })
        .expect("typed desktop-ready request");
    assert!(
        last_commit < ready_request,
        "the capability-authenticated fence must follow every initial desktop commit",
    );
    assert!(
        !outbound_pending.get(),
        "the typed readiness request is flushed on a later loop turn",
    );

    // A terminal decode failure releases the same gate and paints the safe
    // color fallback once; it must not leave a configured desktop blank.
    let mut fallback_inner = MockConnection::new();
    seed_desktop_registry(&mut fallback_inner);
    fallback_inner.push_inbound_on_request(
        surface,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(toplevel, 2, 128, 128),
    );
    let fallback_recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let fallback_max_uploads = std::rc::Rc::new(std::cell::Cell::new(0));
    let fallback_outbound = std::rc::Rc::new(std::cell::Cell::new(false));
    let fallback_turn = std::rc::Rc::new(std::cell::Cell::new(0));
    let fallback_upload_turns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let fallback_commit_turns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let fallback_wait_turns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let fallback_ready = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let fallback_catalog = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let fallback_connection = IncrementalConnection {
        inner: fallback_inner,
        recorded: fallback_recorded.clone(),
        uploads_this_dispatch: 0,
        max_uploads: fallback_max_uploads,
        outbound_pending: fallback_outbound.clone(),
        surface,
        turn: fallback_turn.clone(),
        upload_turns: fallback_upload_turns.clone(),
        commit_turns: fallback_commit_turns.clone(),
        wait_turns: fallback_wait_turns,
    };
    let fallback_events = RecordingEvents {
        recorded: fallback_recorded.clone(),
        outbound_pending: fallback_outbound,
        surface,
        shell_manager,
        catalog_snapshots: fallback_catalog,
        ready_snapshots: fallback_ready.clone(),
        turn: fallback_turn,
    };
    let fallback_exit = shell::paint::run_desktop_shell_with_runtimes_events_and_wallpaper_source(
        fallback_connection,
        20,
        Taskbar::new(0, 0),
        LauncherRuntime::new(
            Launcher::new_stepwise(Box::new(EmptyCatalogStore)),
            FrozenClock,
        ),
        never_spawn,
        || {},
        DesktopPreferenceRuntime::new(StableSource, FrozenClock),
        fallback_events,
        StepWallpaperSource(Some(b"malformed png".to_vec())),
    )
    .expect("malformed initial wallpaper falls back");
    assert_eq!(fallback_exit, ShellExit::IterationLimit);
    assert_eq!(
        attached_buffers(&fallback_recorded.borrow(), surface).len(),
        2,
        "terminal malformed input still seeds exactly two fallback buffers",
    );
    assert_eq!(
        fallback_ready.borrow().len(),
        1,
        "fallback desktop reaches the one-shot ready fence",
    );
    assert!(
        fallback_upload_turns.borrow().iter().all(|turn| *turn >= 3),
        "malformed input is terminal before fallback pixels upload",
    );
    let fallback_commit_turns = fallback_commit_turns.borrow();
    assert_eq!(fallback_commit_turns.len(), 3);
    assert!(fallback_commit_turns[1..].iter().all(|turn| *turn >= 3));
}

#[test]
fn production_completed_wallpaper_parks_until_delayed_configure_before_buffer_seed() {
    struct DelayedConfigureConnection {
        inner: MockConnection,
        recorded: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        outbound_pending: bool,
        surface: ObjectId,
        toplevel: ObjectId,
        turn: std::rc::Rc<std::cell::Cell<usize>>,
        upload_turns: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
        commit_turns: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
        wait_turns: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
        upload_counts_at_wait: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
        configure_release_turn: std::rc::Rc<std::cell::Cell<Option<usize>>>,
    }

    impl Connection for DelayedConfigureConnection {
        fn send(&mut self, bytes: &[u8]) {
            if bytes.len() > 1_024 {
                self.upload_turns.borrow_mut().push(self.turn.get());
            }
            if MessageHeader::decode(bytes).is_ok_and(|header| {
                header.object_id == self.surface && header.opcode == SURFACE_OPCODE_COMMIT
            }) {
                self.commit_turns.borrow_mut().push(self.turn.get());
            }
            self.recorded.borrow_mut().extend_from_slice(bytes);
            self.inner.send(bytes);
            self.outbound_pending = true;
        }

        fn flush_outbound(&mut self) -> Result<(), i32> {
            self.outbound_pending = false;
            Ok(())
        }

        fn outbound_pending(&self) -> bool {
            self.outbound_pending
        }

        fn incremental_uploads(&self) -> bool {
            true
        }

        fn drain_outbound(&mut self) -> Vec<u8> {
            self.outbound_pending = false;
            self.inner.drain_outbound()
        }

        fn recv(&mut self) -> Vec<u8> {
            self.inner.recv()
        }

        fn wait_with(
            &mut self,
            _additional: &[toolkit::WaitFd],
            _timeout: Option<std::time::Duration>,
        ) -> Result<(), i32> {
            let turn = self.turn.get();
            self.wait_turns.borrow_mut().push(turn);
            self.upload_counts_at_wait
                .borrow_mut()
                .push(self.upload_turns.borrow().len());
            if self.configure_release_turn.get().is_none() && self.wait_turns.borrow().len() == 3 {
                self.configure_release_turn.set(Some(turn));
                self.inner
                    .push_inbound(build_configure_event(self.toplevel, 1, 128, 128));
            }
            Ok(())
        }
    }

    struct EmptyCatalogStore;
    struct EmptyCatalogScan(bool);

    impl DesktopEntryStore for EmptyCatalogStore {
        fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError> {
            Ok(Vec::new())
        }

        fn begin_scan(&mut self) -> Result<Option<Box<dyn DesktopEntryScan>>, LauncherError> {
            Ok(Some(Box::new(EmptyCatalogScan(false))))
        }
    }

    impl DesktopEntryScan for EmptyCatalogScan {
        fn step(&mut self) -> Result<DesktopEntryScanBatch, LauncherError> {
            assert!(!self.0, "empty catalog stepped after completion");
            self.0 = true;
            Ok(DesktopEntryScanBatch {
                entries: Vec::new(),
                complete: true,
            })
        }
    }

    struct StableSource;
    impl PreferenceSource for StableSource {
        fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
            Ok(Some(b"[theme]\nname = \"light\"\n".to_vec()))
        }
    }

    struct FrozenClock;
    impl PreferenceClock for FrozenClock {
        fn monotonic_ms(&mut self) -> u64 {
            0
        }

        fn unix_seconds(&mut self) -> i64 {
            1_768_478_400
        }
    }
    impl LauncherClock for FrozenClock {
        fn elapsed(&mut self) -> std::time::Duration {
            std::time::Duration::ZERO
        }
    }

    struct DelayedConfigureEvents {
        turn: std::rc::Rc<std::cell::Cell<usize>>,
        catalog_count: std::rc::Rc<std::cell::Cell<usize>>,
        ready_turns: std::rc::Rc<std::cell::RefCell<Vec<usize>>>,
    }

    impl DesktopEventSource for DelayedConfigureEvents {
        fn drain(&mut self) -> shell::DesktopWake {
            self.turn.set(self.turn.get() + 1);
            shell::DesktopWake::default()
        }

        fn event_driven(&self) -> bool {
            true
        }

        fn catalog_published(&mut self, _entry_count: usize) {
            self.catalog_count.set(self.catalog_count.get() + 1);
        }

        fn desktop_ready(&mut self) {
            self.ready_turns.borrow_mut().push(self.turn.get());
        }
    }

    let mut inner = MockConnection::new();
    seed_desktop_registry(&mut inner);
    let shell_manager = ObjectId::new(19);
    let surface = ObjectId::new(21);
    let toplevel = ObjectId::new(23);
    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let turn = std::rc::Rc::new(std::cell::Cell::new(0));
    let upload_turns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let commit_turns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let wait_turns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let upload_counts_at_wait = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let configure_release_turn = std::rc::Rc::new(std::cell::Cell::new(None));
    let ready_turns = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let catalog_count = std::rc::Rc::new(std::cell::Cell::new(0));
    let connection = DelayedConfigureConnection {
        inner,
        recorded: recorded.clone(),
        outbound_pending: false,
        surface,
        toplevel,
        turn: turn.clone(),
        upload_turns: upload_turns.clone(),
        commit_turns: commit_turns.clone(),
        wait_turns: wait_turns.clone(),
        upload_counts_at_wait: upload_counts_at_wait.clone(),
        configure_release_turn: configure_release_turn.clone(),
    };
    let events = DelayedConfigureEvents {
        turn,
        catalog_count: catalog_count.clone(),
        ready_turns: ready_turns.clone(),
    };

    let exit = shell::paint::run_desktop_shell_with_runtimes_events_and_wallpaper_source(
        connection,
        32,
        Taskbar::new(0, 0),
        LauncherRuntime::new(
            Launcher::new_stepwise(Box::new(EmptyCatalogStore)),
            FrozenClock,
        ),
        never_spawn,
        || {},
        DesktopPreferenceRuntime::new(StableSource, FrozenClock),
        events,
        StepWallpaperSource(Some(one_pixel_wallpaper_png())),
    )
    .expect("desktop waits for its delayed initial configure");
    assert_eq!(exit, ShellExit::IterationLimit);
    assert_eq!(
        catalog_count.get(),
        1,
        "catalog work continues before configure"
    );

    let wait_turns = wait_turns.borrow();
    assert!(
        wait_turns.len() >= 3,
        "a completed wallpaper with no configure must park rather than spin"
    );
    assert!(
        wait_turns[..3]
            .windows(2)
            .all(|turns| turns[1] == turns[0] + 1),
        "the empty delayed-configure turns each reach the blocking wait"
    );
    assert_eq!(
        &upload_counts_at_wait.borrow()[..3],
        &[0, 0, 0],
        "no framebuffer upload precedes the delayed configure"
    );
    let configure_release_turn = configure_release_turn
        .get()
        .expect("third parked turn releases configure");
    assert!(
        upload_turns
            .borrow()
            .iter()
            .all(|turn| *turn > configure_release_turn),
        "framebuffer writes begin only after configure is dispatched"
    );

    let attached = attached_buffers(&recorded.borrow(), surface);
    assert_eq!(attached.len(), 2, "only the two final buffers are seeded");
    assert_ne!(attached[0], attached[1]);
    let commit_turns = commit_turns.borrow();
    assert_eq!(
        commit_turns.len(),
        3,
        "one configure solicitation precedes exactly two final buffer commits"
    );
    assert_eq!(commit_turns[0], 0);
    assert!(
        commit_turns[1..]
            .iter()
            .all(|turn| *turn > configure_release_turn),
        "no framebuffer commit precedes configure"
    );
    assert_eq!(ready_turns.borrow().len(), 1, "desktop-ready is one-shot");

    let requests = parse_request_headers(&recorded.borrow());
    assert_eq!(
        requests
            .iter()
            .filter(|(object_id, opcode, _)| {
                *object_id == surface && *opcode == SURFACE_OPCODE_COMMIT
            })
            .count(),
        3
    );
    assert_eq!(
        requests
            .iter()
            .filter(|(object_id, opcode, payload_len)| {
                *object_id == shell_manager && *opcode == 8 && *payload_len == 0
            })
            .count(),
        1
    );
}

#[test]
fn production_session_loop_subscribes_v2_and_ready_does_not_wait_for_capture_durability() {
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

    struct EmptyStore;
    struct EmptyScan(bool);

    impl DesktopEntryStore for EmptyStore {
        fn list_entries(&mut self) -> Result<Vec<(String, String)>, LauncherError> {
            Ok(Vec::new())
        }

        fn begin_scan(&mut self) -> Result<Option<Box<dyn DesktopEntryScan>>, LauncherError> {
            Ok(Some(Box::new(EmptyScan(false))))
        }
    }

    impl DesktopEntryScan for EmptyScan {
        fn step(&mut self) -> Result<DesktopEntryScanBatch, LauncherError> {
            assert!(!self.0, "empty session catalog stepped after completion");
            self.0 = true;
            Ok(DesktopEntryScanBatch {
                entries: Vec::new(),
                complete: true,
            })
        }
    }

    struct StableSource;
    impl PreferenceSource for StableSource {
        fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
            Ok(Some(b"[theme]\nname = \"light\"\n".to_vec()))
        }
    }

    struct FrozenClock;
    impl PreferenceClock for FrozenClock {
        fn monotonic_ms(&mut self) -> u64 {
            0
        }

        fn unix_seconds(&mut self) -> i64 {
            1_768_478_400
        }
    }
    impl LauncherClock for FrozenClock {
        fn elapsed(&mut self) -> std::time::Duration {
            std::time::Duration::ZERO
        }
    }

    struct MissingSessionFilesystem;
    impl SessionFilesystem for MissingSessionFilesystem {
        fn open_read(&mut self, _path: &Path) -> io::Result<Box<dyn SessionFile>> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }

        fn create_new(&mut self, _path: &Path) -> io::Result<Box<dyn SessionFile>> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }

        fn open_sync(&mut self, _path: &Path) -> io::Result<Box<dyn SessionFile>> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }

        fn close(&mut self, _file: Box<dyn SessionFile>) -> io::Result<()> {
            Ok(())
        }

        fn create_dir(&mut self, _path: &Path) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }

        fn rename(&mut self, _from: &Path, _to: &Path) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }

        fn remove_file(&mut self, _path: &Path) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        }
    }

    struct ReadyEvents(std::rc::Rc<std::cell::Cell<usize>>);
    impl DesktopEventSource for ReadyEvents {
        fn event_driven(&self) -> bool {
            true
        }

        fn desktop_ready(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    let mut inner = MockConnection::new();
    seed_desktop_registry_v2(&mut inner);
    let shell_manager = ObjectId::new(19);
    let surface = ObjectId::new(21);
    let toplevel = ObjectId::new(23);
    inner.push_inbound_on_request(shell_manager, 9, build_snapshot_done_event(shell_manager));
    inner.push_inbound_on_request(
        surface,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(toplevel, 1, 128, 128),
    );

    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let ready_count = std::rc::Rc::new(std::cell::Cell::new(0));
    let connection = RecordingConnection {
        inner,
        recorded: recorded.clone(),
    };
    let launcher = LauncherRuntime::new(Launcher::new_stepwise(Box::new(EmptyStore)), FrozenClock);
    let preferences = DesktopPreferenceRuntime::new(StableSource, FrozenClock);
    let session =
        SessionRuntime::with_filesystem("/session-v1", Box::new(MissingSessionFilesystem));

    let exit = run_desktop_shell_with_runtimes_events_and_session(
        connection,
        16,
        Taskbar::new(0, 0),
        launcher,
        never_spawn,
        || {},
        preferences,
        ReadyEvents(ready_count.clone()),
        session,
    )
    .expect("session-enabled production loop");
    assert_eq!(exit, ShellExit::IterationLimit);
    assert_eq!(ready_count.get(), 1, "desktop readiness remains one-shot");

    let requests = parse_request_headers(&recorded.borrow());
    let subscribe = requests
        .iter()
        .position(|(object_id, opcode, payload_len)| {
            *object_id == shell_manager && *opcode == 9 && *payload_len == 4
        })
        .expect("v2 authoritative-state subscription");
    let ready = requests
        .iter()
        .position(|(object_id, opcode, payload_len)| {
            *object_id == shell_manager && *opcode == 8 && *payload_len == 0
        })
        .expect("typed desktop-ready request");
    assert!(
        subscribe < ready,
        "snapshot catch-up causally precedes readiness"
    );
    assert!(
        requests
            .iter()
            .all(|(object_id, opcode, _)| *object_id != shell_manager || *opcode != 10),
        "a missing durable snapshot never opens a restore transaction",
    );
}

#[test]
fn production_launcher_open_and_close_use_narrow_damage_on_alternating_slots() {
    struct InputAfterSeedConnection {
        inner: MockConnection,
        recorded: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        surface: ObjectId,
        pointer: ObjectId,
        attach_count: usize,
    }

    impl Connection for InputAfterSeedConnection {
        fn send(&mut self, bytes: &[u8]) {
            self.recorded.borrow_mut().extend_from_slice(bytes);
            self.inner.send(bytes);
            let Ok(header) = MessageHeader::decode(bytes) else {
                return;
            };
            if header.object_id == self.surface && header.opcode == 2
            /* attach */
            {
                self.attach_count += 1;
                if matches!(self.attach_count, 2 | 3) {
                    self.inner.push_inbound(build_pointer_button_event(
                        self.pointer,
                        self.surface,
                        self.attach_count as u32,
                        8,
                        744,
                    ));
                }
            }
        }

        fn drain_outbound(&mut self) -> Vec<u8> {
            self.inner.drain_outbound()
        }

        fn recv(&mut self) -> Vec<u8> {
            self.inner.recv()
        }
    }

    struct StableSource;
    impl PreferenceSource for StableSource {
        fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    struct FrozenClock;
    impl PreferenceClock for FrozenClock {
        fn monotonic_ms(&mut self) -> u64 {
            0
        }

        fn unix_seconds(&mut self) -> i64 {
            1_768_478_400
        }
    }

    const FIVE_SLOTS: &[LauncherSlot<'static>] = &[
        LauncherSlot {
            label: "Term",
            exec: "/bin/term",
        },
        LauncherSlot {
            label: "Files",
            exec: "/bin/files",
        },
        LauncherSlot {
            label: "Edit",
            exec: "/bin/edit",
        },
        LauncherSlot {
            label: "Settings",
            exec: "/bin/settings",
        },
        LauncherSlot {
            label: "Sysmon",
            exec: "/bin/sysmon",
        },
    ];

    let mut inner = MockConnection::new();
    seed_desktop_registry(&mut inner);
    let surface = ObjectId::new(21);
    let toplevel = ObjectId::new(23);
    let pointer = ObjectId::new(15);
    let pool = ObjectId::new(25);
    inner.push_inbound_on_request(
        surface,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(toplevel, 1, 1024, 768),
    );
    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let connection = InputAfterSeedConnection {
        inner,
        recorded: recorded.clone(),
        surface,
        pointer,
        attach_count: 0,
    };
    let preferences = DesktopPreferenceRuntime::new(StableSource, FrozenClock);

    let exit = run_desktop_shell_with_preferences(
        connection,
        5,
        Taskbar::new(0, 0),
        FIVE_SLOTS,
        never_spawn,
        || {},
        preferences,
    )
    .expect("production-shaped launcher interaction");
    assert_eq!(exit, ShellExit::IterationLimit);

    let recorded = recorded.borrow();
    let attached = attached_buffers(&recorded, surface);
    assert_eq!(attached.len(), 4, "two seed frames plus open and close");
    assert_eq!(attached[0], attached[2]);
    assert_eq!(attached[1], attached[3]);
    assert_ne!(attached[0], attached[1]);

    let damages = surface_damages(&recorded, surface);
    assert_eq!(damages.len(), 4);
    for damage in &damages[..2] {
        assert_eq!(
            (damage.x, damage.y, damage.width, damage.height),
            (0, 0, 1024, 768),
        );
    }
    for damage in &damages[2..] {
        assert_eq!(
            (damage.x, damage.y, damage.width, damage.height),
            (4, 608, 200, 158),
            "open and close both cover menu history plus the launcher button",
        );
    }

    let mut bytes = &recorded[..];
    let mut row_writes = Vec::new();
    while bytes.len() >= HEADER_SIZE {
        let header = MessageHeader::decode(bytes).expect("valid framed request");
        let msg_len = header.length as usize;
        if header.object_id == pool && header.opcode == 5 {
            row_writes.push(
                ShmPoolWriteRows::decode(&bytes[HEADER_SIZE..msg_len])
                    .expect("packed launcher upload"),
            );
        }
        bytes = &bytes[msg_len..];
    }
    assert_eq!(
        row_writes.len(),
        6,
        "open uploads six chunks; close reuses the already-closed alternate slot",
    );
    assert!(row_writes
        .iter()
        .all(|write| write.bytes.len() <= toolkit::SHM_WRITE_CHUNK_BYTES));
}

#[test]
fn production_six_app_focus_changes_damage_only_old_and_new_entries_on_alternating_slots() {
    struct FocusAfterSeedConnection {
        inner: MockConnection,
        recorded: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        surface: ObjectId,
        pointer: ObjectId,
        attach_count: usize,
        uploads_this_dispatch: usize,
        max_uploads: std::rc::Rc<std::cell::Cell<usize>>,
        outbound_pending: bool,
    }

    impl Connection for FocusAfterSeedConnection {
        fn send(&mut self, bytes: &[u8]) {
            if bytes.len() > 1_024 {
                self.uploads_this_dispatch += 1;
                self.max_uploads
                    .set(self.max_uploads.get().max(self.uploads_this_dispatch));
            }
            self.recorded.borrow_mut().extend_from_slice(bytes);
            self.inner.send(bytes);
            self.outbound_pending = true;
            let Ok(header) = MessageHeader::decode(bytes) else {
                return;
            };
            if header.object_id == self.surface && header.opcode == 2
            /* attach */
            {
                self.attach_count += 1;
                let click_x = match self.attach_count {
                    // The typical-desktop workload has seven entries: the
                    // shell plus six launched apps. Its two Term windows are
                    // indexes 1 and 6, with fitted 121px entry widths.
                    2 => Some(836),
                    3 => Some(221),
                    _ => None,
                };
                if let Some(x) = click_x {
                    self.inner.push_inbound(build_pointer_button_event(
                        self.pointer,
                        self.surface,
                        self.attach_count as u32,
                        x,
                        744,
                    ));
                }
            }
        }

        fn flush_outbound(&mut self) -> Result<(), i32> {
            self.outbound_pending = false;
            Ok(())
        }

        fn outbound_pending(&self) -> bool {
            self.outbound_pending
        }

        fn incremental_uploads(&self) -> bool {
            true
        }

        fn drain_outbound(&mut self) -> Vec<u8> {
            self.outbound_pending = false;
            self.inner.drain_outbound()
        }

        fn recv(&mut self) -> Vec<u8> {
            self.uploads_this_dispatch = 0;
            self.inner.recv()
        }
    }

    struct StableSource;
    impl PreferenceSource for StableSource {
        fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    struct FrozenClock;
    impl PreferenceClock for FrozenClock {
        fn monotonic_ms(&mut self) -> u64 {
            0
        }

        fn unix_seconds(&mut self) -> i64 {
            1_768_478_400
        }
    }

    let mut inner = MockConnection::new();
    seed_desktop_registry(&mut inner);
    let surface = ObjectId::new(21);
    let toplevel = ObjectId::new(23);
    let pointer = ObjectId::new(15);
    let pool = ObjectId::new(25);
    inner.push_inbound_on_request(
        surface,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(toplevel, 1, 1024, 768),
    );
    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let max_uploads = std::rc::Rc::new(std::cell::Cell::new(0));
    let connection = FocusAfterSeedConnection {
        inner,
        recorded: recorded.clone(),
        surface,
        pointer,
        attach_count: 0,
        uploads_this_dispatch: 0,
        max_uploads: max_uploads.clone(),
        outbound_pending: false,
    };
    let preferences = DesktopPreferenceRuntime::new(StableSource, FrozenClock);
    let mut taskbar = Taskbar::new(0, 0);
    for (window_id, title, app_id) in [
        (1, "PMos", "pmos.shell"),
        (2, "Terminal 1", "pmos.term"),
        (3, "Files", "pmos.files"),
        (4, "Edit", "pmos.edit"),
        (5, "Settings", "pmos.settings"),
        (6, "System Monitor", "pmos.sysmon"),
        (7, "Terminal 2", "pmos.term"),
    ] {
        taskbar.add_window(window_id, title, app_id);
    }
    taskbar.set_focused_window(2);
    let exit = run_desktop_shell_with_preferences(
        connection,
        300,
        taskbar,
        &[],
        never_spawn,
        || {},
        preferences,
    )
    .expect("production-shaped focus interaction");
    assert_eq!(exit, ShellExit::IterationLimit);
    assert_eq!(max_uploads.get(), 1, "one bounded upload per dispatch turn");

    let recorded = recorded.borrow();
    let attached = attached_buffers(&recorded, surface);
    assert_eq!(attached.len(), 4, "two seed frames plus two focus frames");
    assert_eq!(attached[0], attached[2]);
    assert_eq!(attached[1], attached[3]);
    assert_ne!(attached[0], attached[1]);

    let damages = surface_damages(&recorded, surface);
    assert_eq!(damages.len(), 6);
    for damage in &damages[..2] {
        assert_eq!(
            (damage.x, damage.y, damage.width, damage.height),
            (0, 0, 1024, 768),
        );
    }
    let expected_entry_pairs = [
        [(213, 738, 121, 28), (828, 738, 121, 28)],
        [(828, 738, 121, 28), (213, 738, 121, 28)],
    ];
    for (pair, expected) in damages[2..].chunks_exact(2).zip(expected_entry_pairs) {
        assert_eq!(
            pair.iter()
                .map(|damage| (damage.x, damage.y, damage.width, damage.height))
                .collect::<Vec<_>>(),
            expected,
        );
    }

    let mut bytes = &recorded[..];
    let mut row_writes = Vec::new();
    while bytes.len() >= HEADER_SIZE {
        let header = MessageHeader::decode(bytes).expect("valid framed request");
        let msg_len = header.length as usize;
        if header.object_id == pool && header.opcode == 5 {
            row_writes.push(
                ShmPoolWriteRows::decode(&bytes[HEADER_SIZE..msg_len])
                    .expect("packed focus upload"),
            );
        }
        bytes = &bytes[msg_len..];
    }
    assert_eq!(
        row_writes.len(),
        2,
        "the old/new focus pair takes exactly two upload progress turns; the alternating back slot already holds the return state",
    );
    assert!(row_writes
        .iter()
        .all(|write| write.bytes.len() <= toolkit::SHM_WRITE_CHUNK_BYTES));
}

#[test]
fn desktop_repaints_when_persisted_preferences_change() {
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

    struct Source {
        values: VecDeque<Vec<u8>>,
    }
    impl PreferenceSource for Source {
        fn read(&mut self) -> io::Result<Option<Vec<u8>>> {
            Ok(self
                .values
                .pop_front()
                .or_else(|| Some(b"[theme]\nname = \"dark\"\n".to_vec())))
        }
    }

    struct StepClock {
        monotonic: VecDeque<u64>,
    }
    impl PreferenceClock for StepClock {
        fn monotonic_ms(&mut self) -> u64 {
            self.monotonic.pop_front().unwrap_or(100)
        }
        fn unix_seconds(&mut self) -> i64 {
            1_768_478_400
        }
    }

    let mut inner = MockConnection::new();
    seed_desktop_registry(&mut inner);
    // With every optional object bound, Window allocates surface=21 and
    // toplevel=23 after registry/callback/compositor/shm/xdg/seat/pointer/
    // keyboard/shell-manager consume the preceding odd ids.
    let surface = ObjectId::new(21);
    let toplevel = ObjectId::new(23);
    inner.push_inbound_on_request(
        surface,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(toplevel, 1, 64, 48),
    );

    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let connection = RecordingConnection {
        inner,
        recorded: recorded.clone(),
    };
    let source = Source {
        values: VecDeque::from([
            b"[theme]\nname = \"light\"\n[wallpaper]\nname = \"blue.png\"\n".to_vec(),
            b"[theme]\nname = \"dark\"\n[wallpaper]\nname = \"green.png\"\n".to_vec(),
        ]),
    };
    let clock = StepClock {
        // Runtime construction consumes the first 0. Iteration one stays at
        // zero and performs the configure paint; iteration two reaches the
        // preference boundary and must schedule a second frame.
        monotonic: VecDeque::from([0, 0, 100, 100]),
    };
    let runtime = DesktopPreferenceRuntime::new(source, clock).with_clock_check_every_iterations(1);

    let exit = run_desktop_shell_with_preferences(
        connection,
        3,
        Taskbar::new(0, 0),
        &[],
        never_spawn,
        || {},
        runtime,
    )
    .expect("desktop loop must accept live preference updates");
    assert_eq!(exit, ShellExit::IterationLimit);

    let recorded = recorded.borrow();
    let commits = parse_request_headers(&recorded)
        .into_iter()
        .filter(|(object_id, opcode, _)| *object_id == surface && *opcode == SURFACE_OPCODE_COMMIT)
        .count();
    assert_eq!(
        commits, 5,
        "initial map plus two-buffer configured and live-preference frames"
    );
    let attached = attached_buffers(&recorded, surface);
    assert_eq!(attached.len(), 4, "each full state must seed both buffers");
    assert_ne!(attached[0], attached[1]);
    assert_eq!(attached[0], attached[2]);
    assert_eq!(attached[1], attached[3]);
}
