//! `term::run_term` integration tests.
//!
//! Drives the term event loop against a bidirectional
//! in-memory `MockConnection` patterned after
//! `crates/shell/tests/paint_wallpaper.rs`. Each test
//! pre-seeds the registry advertisement that
//! [`App::connect_with_shell`] expects (compositor + shm +
//! xdg_shell + seat) and then registers trigger-on-send
//! inbound batches that release events once the term has
//! sent specific outbound requests.

use std::collections::VecDeque;
use std::time::Duration;

use display_proto::events::{KeyboardKey, RegistryGlobal, XdgToplevelClose, XdgToplevelConfigure};
use display_proto::ids::ObjectId;
use display_proto::requests::{
    buffer_format, ShmPoolCreateBuffer, SurfaceAttach, SurfacePatchCurrent,
};
use display_proto::wire::{MessageHeader, HEADER_SIZE};

use term::{
    default_font, run_term_with_options, run_term_with_stepwise_runner_and_font,
    StepwiseCommandRunner, StepwiseShellUpdate, TermExit, TerminalOptions,
};
use toolkit::protocol::{Connection, WaitFd};

struct PendingTrigger {
    object_id: ObjectId,
    opcode: u16,
    inbound: Vec<u8>,
}

struct MockConnection {
    outbound: Vec<u8>,
    outbound_log: Vec<u8>,
    parsed_up_to: usize,
    inbound: VecDeque<Vec<u8>>,
    triggers: Vec<PendingTrigger>,
    wait_inbound: VecDeque<Vec<u8>>,
    waits: Option<std::rc::Rc<std::cell::RefCell<Vec<Vec<WaitFd>>>>>,
    incremental: bool,
    blocked: bool,
    wait_snapshots: Option<std::rc::Rc<std::cell::RefCell<Vec<Vec<u8>>>>>,
}

impl MockConnection {
    fn new() -> Self {
        MockConnection {
            outbound: Vec::new(),
            outbound_log: Vec::new(),
            parsed_up_to: 0,
            inbound: VecDeque::new(),
            triggers: Vec::new(),
            wait_inbound: VecDeque::new(),
            waits: None,
            incremental: false,
            blocked: false,
            wait_snapshots: None,
        }
    }

    fn push_inbound(&mut self, bytes: Vec<u8>) {
        self.inbound.push_back(bytes);
    }

    fn push_inbound_on_request(&mut self, object_id: ObjectId, opcode: u16, inbound: Vec<u8>) {
        self.triggers.push(PendingTrigger {
            object_id,
            opcode,
            inbound,
        });
    }

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
        self.blocked |= self.incremental && !bytes.is_empty();
        if let Some(done) = sync_done_for(bytes) {
            self.inbound.push_back(done);
        }
        self.scan_outbound_and_fire();
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        self.blocked = false;
        core::mem::take(&mut self.outbound)
    }

    fn recv(&mut self) -> Vec<u8> {
        self.inbound.pop_front().unwrap_or_default()
    }

    fn wait(&mut self, timeout: Option<Duration>) -> Result<(), i32> {
        self.wait_with(&[], timeout)
    }

    fn wait_with(&mut self, additional: &[WaitFd], _timeout: Option<Duration>) -> Result<(), i32> {
        if let Some(waits) = &self.waits {
            waits.borrow_mut().push(additional.to_vec());
        }
        if let Some(inbound) = self.wait_inbound.pop_front() {
            self.inbound.push_back(inbound);
        }
        if let Some(snapshots) = &self.wait_snapshots {
            snapshots.borrow_mut().push(self.outbound_log.clone());
        }
        Ok(())
    }

    fn flush_outbound(&mut self) -> Result<(), i32> {
        if self.incremental {
            self.outbound.clear();
            self.blocked = false;
        }
        Ok(())
    }

    fn outbound_pending(&self) -> bool {
        self.incremental && self.blocked
    }

    fn incremental_uploads(&self) -> bool {
        self.incremental
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

    fn flush_outbound(&mut self) -> Result<(), i32> {
        self.inner.flush_outbound()
    }

    fn outbound_pending(&self) -> bool {
        self.inner.outbound_pending()
    }

    fn incremental_uploads(&self) -> bool {
        self.inner.incremental_uploads()
    }

    fn wait(&mut self, timeout: Option<Duration>) -> Result<(), i32> {
        self.inner.wait(timeout)
    }

    fn wait_with(&mut self, additional: &[WaitFd], timeout: Option<Duration>) -> Result<(), i32> {
        self.inner.wait_with(additional, timeout)
    }
}

const REGISTRY_ID: ObjectId = ObjectId::new(3);
/// With seat advertised + shell_manager NOT advertised, the
/// id allocator hands out: registry=3, sync callback=5,
/// compositor=7, shm=9, xdg_shell=11, seat=13, pointer=15,
/// keyboard=17. Then `Window::new` allocates surface=19,
/// toplevel=21.
const KEYBOARD_ID: ObjectId = ObjectId::new(17);
const SURFACE_ID: ObjectId = ObjectId::new(19);
const TOPLEVEL_ID: ObjectId = ObjectId::new(21);

const SURFACE_OPCODE_COMMIT: u16 = 7;
const SURFACE_OPCODE_PATCH_CURRENT: u16 = 8;

fn build_global_event(name: u32, interface: &str, version: u32) -> Vec<u8> {
    let event = RegistryGlobal {
        name,
        interface: interface.to_string(),
        version,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header = MessageHeader::try_new(REGISTRY_ID, 1 /* global */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

fn build_configure_event(serial: u32, width: i32, height: i32) -> Vec<u8> {
    build_configure_event_with_states(serial, width, height, 0)
}

fn build_configure_event_with_states(serial: u32, width: i32, height: i32, states: u32) -> Vec<u8> {
    let event = XdgToplevelConfigure {
        serial,
        width,
        height,
        states,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header = MessageHeader::try_new(TOPLEVEL_ID, 1 /* configure */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

fn build_close_event() -> Vec<u8> {
    let _event = XdgToplevelClose;
    let mut out = vec![0u8; HEADER_SIZE];
    let header = MessageHeader::try_new(TOPLEVEL_ID, 2 /* close */, 0, 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out
}

fn build_keyboard_key_event(key: u32, state: u32) -> Vec<u8> {
    let event = KeyboardKey {
        surface_id: SURFACE_ID,
        key,
        state,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header = MessageHeader::try_new(KEYBOARD_ID, 1 /* key */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

fn build_buffer_release_event(buffer_id: ObjectId) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_SIZE];
    MessageHeader::try_new(buffer_id, 1 /* release */, 0, 0)
        .unwrap()
        .encode(&mut out)
        .unwrap();
    out
}

fn seed_full_registry(conn: &mut MockConnection) {
    let mut batch = Vec::new();
    batch.extend(build_global_event(1, "pmd_compositor", 1));
    batch.extend(build_global_event(2, "pmd_shm", 1));
    batch.extend(build_global_event(3, "pmd_xdg_shell", 1));
    batch.extend(build_global_event(4, "pmd_seat", 1));
    conn.push_inbound(batch);
}

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

fn parse_created_buffer_sizes(mut bytes: &[u8]) -> Vec<(u32, u32)> {
    let mut sizes = Vec::new();
    while bytes.len() >= HEADER_SIZE {
        let header = MessageHeader::decode(bytes).expect("valid framed request");
        let msg_len = header.length as usize;
        assert!(bytes.len() >= msg_len, "truncated framed request");
        let payload = &bytes[HEADER_SIZE..msg_len];
        if header.opcode == 1 && payload.len() == 24 {
            if let Ok(request) = ShmPoolCreateBuffer::decode(payload) {
                if request.format == buffer_format::ARGB8888
                    && request.stride == request.width.saturating_mul(4)
                {
                    sizes.push((request.width, request.height));
                }
            }
        }
        bytes = &bytes[msg_len..];
    }
    assert!(bytes.is_empty(), "leftover bytes after parse");
    sizes
}

fn parse_surface_patches(mut bytes: &[u8]) -> Vec<SurfacePatchCurrent<'_>> {
    let mut patches = Vec::new();
    while bytes.len() >= HEADER_SIZE {
        let header = MessageHeader::decode(bytes).expect("valid framed request");
        let msg_len = header.length as usize;
        assert!(bytes.len() >= msg_len, "truncated framed request");
        if header.object_id == SURFACE_ID && header.opcode == SURFACE_OPCODE_PATCH_CURRENT {
            patches.push(
                SurfacePatchCurrent::decode(&bytes[HEADER_SIZE..msg_len])
                    .expect("typed current patch"),
            );
        }
        bytes = &bytes[msg_len..];
    }
    assert!(bytes.is_empty(), "leftover bytes after parse");
    patches
}

fn parse_surface_attaches(mut bytes: &[u8]) -> Vec<SurfaceAttach> {
    let mut attaches = Vec::new();
    while bytes.len() >= HEADER_SIZE {
        let header = MessageHeader::decode(bytes).expect("valid framed request");
        let msg_len = header.length as usize;
        assert!(bytes.len() >= msg_len, "truncated framed request");
        if header.object_id == SURFACE_ID && header.opcode == 2 {
            attaches
                .push(SurfaceAttach::decode(&bytes[HEADER_SIZE..msg_len]).expect("surface attach"));
        }
        bytes = &bytes[msg_len..];
    }
    assert!(bytes.is_empty(), "leftover bytes after parse");
    attaches
}

fn quiet_options() -> TerminalOptions {
    TerminalOptions {
        max_lines: 64,
        banner: vec![],
        prompt: "$ ".to_string(),
    }
}

const ENTER: u32 = 0x28;
const KEY_E: u32 = 0x08;
const KEY_X: u32 = 0x1B;
const KEY_I: u32 = 0x0C;
const KEY_T: u32 = 0x17;
const BACKSPACE: u32 = 0x2A;
const KEY_PRESSED: u32 = 1;

#[derive(Default)]
struct StepwiseRecordingRunner {
    updates: VecDeque<StepwiseShellUpdate>,
    ready: bool,
    commands: Vec<String>,
    input: Vec<Vec<u8>>,
    input_pending: bool,
    terminated: bool,
}

impl StepwiseCommandRunner for StepwiseRecordingRunner {
    fn start_command(&mut self, line: &str) -> Result<(), i32> {
        if !self.ready {
            return Err(abi::errno::EAGAIN);
        }
        self.commands.push(line.to_string());
        Ok(())
    }

    fn send_input(&mut self, bytes: &[u8]) -> Result<(), i32> {
        self.input.push(bytes.to_vec());
        self.input_pending = true;
        Ok(())
    }

    fn flush_input(&mut self) -> Result<(), i32> {
        Ok(())
    }

    fn is_ready(&self) -> bool {
        self.ready
    }

    fn readable_output_fd(&self) -> Option<u32> {
        Some(42)
    }

    fn signal_fd(&self) -> Option<u32> {
        Some(abi::fd::SIGNAL)
    }

    fn writable_input_fd(&self) -> Option<u32> {
        self.input_pending.then_some(43)
    }

    fn drain_output(&mut self) -> StepwiseShellUpdate {
        let update = self.updates.pop_front().unwrap_or_default();
        self.ready |= update.ready;
        update
    }

    fn terminate(&mut self) {
        self.terminated = true;
    }
}

#[test]
fn run_term_completes_iteration_limit_when_no_close() {
    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(1, 720, 480),
    );

    let exit = run_term_with_options(conn, 5, quiet_options()).expect("run_term must succeed");
    assert_eq!(exit, TermExit::IterationLimit);
}

#[test]
fn run_term_exits_on_close_request() {
    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);
    let mut configure_then_close = build_configure_event(1, 640, 400);
    configure_then_close.extend(build_close_event());
    conn.push_inbound_on_request(SURFACE_ID, SURFACE_OPCODE_COMMIT, configure_then_close);

    let exit = run_term_with_options(conn, 5, quiet_options()).expect("run_term must succeed");
    assert_eq!(exit, TermExit::CloseRequested);
}

#[test]
fn stepwise_shell_waits_through_startup_and_close_during_withheld_prompt() {
    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);
    let mut configure_and_early_command = build_configure_event(1, 720, 480);
    configure_and_early_command.extend(build_keyboard_key_event(KEY_E, KEY_PRESSED));
    configure_and_early_command.extend(build_keyboard_key_event(ENTER, KEY_PRESSED));
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        configure_and_early_command,
    );
    // The first Enter arrives before callback readiness and must not clear the
    // editor. The second starts the retained command; then input and close are
    // delivered while the command deliberately withholds its next prompt.
    conn.wait_inbound
        .push_back(build_keyboard_key_event(ENTER, KEY_PRESSED));
    conn.wait_inbound
        .push_back(build_keyboard_key_event(KEY_X, KEY_PRESSED));
    conn.wait_inbound.push_back(build_close_event());
    let waits = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    conn.waits = Some(waits.clone());

    let mut runner = StepwiseRecordingRunner {
        updates: VecDeque::from([
            StepwiseShellUpdate::default(),
            StepwiseShellUpdate {
                ready: true,
                ..StepwiseShellUpdate::default()
            },
            StepwiseShellUpdate {
                output: b"waiting for input\n".to_vec(),
                ..StepwiseShellUpdate::default()
            },
        ]),
        ..StepwiseRecordingRunner::default()
    };
    let exit = run_term_with_stepwise_runner_and_font(
        conn,
        16,
        quiet_options(),
        &mut runner,
        default_font(),
    )
    .expect("stepwise terminal remains responsive");

    assert_eq!(exit, TermExit::CloseRequested);
    assert_eq!(runner.commands, vec!["e"]);
    assert_eq!(runner.input, vec![b"x".to_vec()]);
    assert!(runner.terminated);
    assert!(waits
        .borrow()
        .iter()
        .all(|set| set.contains(&WaitFd::readable(42))));
    assert!(waits
        .borrow()
        .iter()
        .all(|set| set.contains(&WaitFd::readable(abi::fd::SIGNAL as i32))));
    assert!(waits
        .borrow()
        .iter()
        .any(|set| set.contains(&WaitFd::writable(43))));
}

#[test]
fn run_term_emits_term_app_id_and_title_set() {
    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(1, 720, 480),
    );

    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u8>::new()));
    let recorded_clone = recorded.clone();
    let conn = RecordingConnection {
        inner: conn,
        recorded: recorded_clone,
    };
    let _ = run_term_with_options(conn, 5, quiet_options()).expect("run_term must succeed");

    let bytes = recorded.borrow().clone();
    let headers = parse_request_headers(&bytes);

    // set_title (opcode 1 on XdgToplevel) and set_app_id
    // (opcode 4) must each fire exactly once on the toplevel.
    let title_count = headers
        .iter()
        .filter(|(obj, op, _)| *obj == TOPLEVEL_ID && *op == 1)
        .count();
    let app_id_count = headers
        .iter()
        .filter(|(obj, op, _)| *obj == TOPLEVEL_ID && *op == 4)
        .count();
    assert_eq!(title_count, 1, "set_title must fire once");
    assert_eq!(app_id_count, 1, "set_app_id must fire once");

    // seat.get_keyboard must fire so the term receives input
    // events. Seat id is 13 in this allocation order.
    let seat_id = ObjectId::new(13);
    let get_keyboard_count = headers
        .iter()
        .filter(
            |(obj, op, _)| *obj == seat_id && *op == 2, /* get_keyboard */
        )
        .count();
    assert_eq!(get_keyboard_count, 1, "seat.get_keyboard must fire once");
}

#[test]
fn initial_work_area_offer_creates_only_normal_sized_buffers() {
    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(1, 1024, 736),
    );

    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u8>::new()));
    let connection = RecordingConnection {
        inner: conn,
        recorded: recorded.clone(),
    };
    let exit = run_term_with_options(connection, 4, quiet_options()).expect("run_term succeeds");

    assert_eq!(exit, TermExit::IterationLimit);
    assert_eq!(
        parse_created_buffer_sizes(&recorded.borrow()),
        vec![(720, 480), (720, 480)],
    );
}

#[test]
fn cold_first_edit_and_alternating_edits_use_one_atomic_current_patch_each() {
    let mut conn = MockConnection::new();
    conn.incremental = true;
    seed_full_registry(&mut conn);
    let pool_id = ObjectId::new(23);
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(1, 720, 480),
    );
    // Arrive after the initial canvas was rasterized but while its first shm
    // chunk is in flight. The edit must survive the full commit's final
    // outbound suffix and patch on the immediate local turn after flush.
    conn.push_inbound_on_request(
        pool_id,
        4, /* write */
        build_keyboard_key_event(KEY_E, KEY_PRESSED),
    );
    for key in [BACKSPACE, KEY_E, BACKSPACE] {
        conn.wait_inbound
            .push_back(build_keyboard_key_event(key, KEY_PRESSED));
    }
    let wait_snapshots = std::rc::Rc::new(std::cell::RefCell::new(Vec::<Vec<u8>>::new()));
    conn.wait_snapshots = Some(wait_snapshots.clone());

    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u8>::new()));
    let connection = RecordingConnection {
        inner: conn,
        recorded: recorded.clone(),
    };
    let exit = run_term_with_options(connection, 70, quiet_options()).expect("term run succeeds");
    assert_eq!(exit, TermExit::IterationLimit);

    let bytes = recorded.borrow();
    let headers = parse_request_headers(&bytes);
    assert_eq!(
        headers
            .iter()
            .filter(|(object, opcode, _)| *object == pool_id && *opcode == 4)
            .count(),
        57,
        "the cold 720x480 slot is uploaded once in bounded 24 KiB turns",
    );
    assert_eq!(
        headers
            .iter()
            .filter(|(object, opcode, _)| *object == pool_id && *opcode == 5)
            .count(),
        0,
        "input edits must not mutate live pool backing through write_rows",
    );
    assert_eq!(
        headers
            .iter()
            .filter(|(object, opcode, _)| *object == SURFACE_ID && *opcode == 2)
            .count(),
        1,
        "edits must not attach the cold alternate buffer",
    );

    let patches = parse_surface_patches(&bytes);
    assert_eq!(patches.len(), 4);
    for patch in patches {
        assert_eq!(
            (patch.x, patch.y, patch.width, patch.height),
            (20, 452, 16, 14)
        );
        assert_eq!(patch.pixels.len(), 896);
        assert!(patch.pixels.len() <= display_proto::MAX_SURFACE_PATCH_BYTES);
    }

    let waits = wait_snapshots.borrow();
    assert!(!waits.is_empty());
    assert_eq!(
        parse_surface_patches(&waits[0]).len(),
        1,
        "after flush drains the full-commit suffix, local InputEdit must patch before READ park",
    );
}

#[test]
fn full_repaint_waits_for_old_back_buffer_release_without_local_spin() {
    let mut conn = MockConnection::new();
    conn.incremental = true;
    seed_full_registry(&mut conn);
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(1, 720, 480),
    );
    conn.wait_inbound
        .push_back(build_keyboard_key_event(ENTER, KEY_PRESSED));
    conn.wait_inbound
        .push_back(build_keyboard_key_event(ENTER, KEY_PRESSED));
    let wait_snapshots = std::rc::Rc::new(std::cell::RefCell::new(Vec::<Vec<u8>>::new()));
    conn.wait_snapshots = Some(wait_snapshots.clone());

    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u8>::new()));
    let connection = RecordingConnection {
        inner: conn,
        recorded: recorded.clone(),
    };
    assert_eq!(
        run_term_with_options(connection, 120, quiet_options()).expect("term run succeeds"),
        TermExit::IterationLimit,
    );

    let bytes = recorded.borrow();
    let headers = parse_request_headers(&bytes);
    assert_eq!(
        headers
            .iter()
            .filter(|(object, opcode, _)| *object == SURFACE_ID && *opcode == 2)
            .count(),
        2,
        "the third full frame must not reuse still-current buffer 0",
    );
    let waits = wait_snapshots.borrow();
    assert!(
        waits.len() >= 3,
        "blocked full paint must park for display READ"
    );
    let second_frame_bytes = waits[1].len();
    assert!(waits[2..]
        .iter()
        .all(|snapshot| snapshot.len() == second_frame_bytes));
}

#[test]
fn old_buffer_release_after_replacement_unlocks_the_next_full_frame() {
    let mut conn = MockConnection::new();
    conn.incremental = true;
    seed_full_registry(&mut conn);
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(1, 720, 480),
    );
    conn.wait_inbound
        .push_back(build_keyboard_key_event(ENTER, KEY_PRESSED));
    let mut old_release_and_next_command = build_buffer_release_event(ObjectId::new(25));
    old_release_and_next_command.extend(build_keyboard_key_event(ENTER, KEY_PRESSED));
    conn.wait_inbound.push_back(old_release_and_next_command);

    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u8>::new()));
    let connection = RecordingConnection {
        inner: conn,
        recorded: recorded.clone(),
    };
    assert_eq!(
        run_term_with_options(connection, 140, quiet_options()).expect("term run succeeds"),
        TermExit::IterationLimit,
    );

    assert_eq!(
        parse_surface_attaches(&recorded.borrow())
            .into_iter()
            .map(|attach| attach.buffer_id)
            .collect::<Vec<_>>(),
        vec![ObjectId::new(25), ObjectId::new(27), ObjectId::new(25)],
        "only replacement releases the old slot for the next safe full repaint",
    );
}

#[test]
fn maximize_and_restore_replace_buffers_at_state_appropriate_sizes() {
    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(1, 1024, 736),
    );
    conn.push_inbound_on_request(
        ObjectId::new(23),
        1,
        build_configure_event_with_states(
            2,
            1024,
            736,
            display_proto::xdg_toplevel_state::MAXIMIZED,
        ),
    );
    conn.push_inbound_on_request(ObjectId::new(29), 1, build_configure_event(3, 0, 0));

    let recorded = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u8>::new()));
    let connection = RecordingConnection {
        inner: conn,
        recorded: recorded.clone(),
    };
    let exit = run_term_with_options(connection, 8, quiet_options()).expect("run_term succeeds");

    assert_eq!(exit, TermExit::IterationLimit);
    assert_eq!(
        parse_created_buffer_sizes(&recorded.borrow()),
        vec![
            (720, 480),
            (720, 480),
            (1024, 736),
            (1024, 736),
            (720, 480),
            (720, 480),
        ],
    );
}

#[test]
fn run_term_typing_exit_then_enter_runs_shell_exit_builtin() {
    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);
    let mut after_commit = build_configure_event(1, 720, 480);
    // Stage the typing AFTER configure: e, x, i, t, Enter.
    // Each key gets a press event + release event so the term
    // sees the press path. We only need press events for the
    // term's keystroke loop, but include releases too since
    // production drivers send them.
    for sc in [KEY_E, KEY_X, KEY_I, KEY_T, ENTER] {
        after_commit.extend(build_keyboard_key_event(sc, KEY_PRESSED));
    }
    conn.push_inbound_on_request(SURFACE_ID, SURFACE_OPCODE_COMMIT, after_commit);

    // 16 dispatch iterations: iter 1 sees configure +
    // key events + paints. Subsequent iters drain. After
    // Enter commits "exit", the embedded shell flips its
    // exit flag and run_term returns ShellExited.
    let exit = run_term_with_options(conn, 16, quiet_options()).expect("run_term must succeed");
    assert_eq!(exit, TermExit::ShellExited);
}
