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

use display_proto::events::{
    KeyboardKey, RegistryGlobal, XdgToplevelClose, XdgToplevelConfigure,
};
use display_proto::ids::ObjectId;
use display_proto::wire::{MessageHeader, HEADER_SIZE};

use term::{run_term_with_options, TermExit, TerminalOptions};
use toolkit::protocol::Connection;

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

    fn push_inbound_on_request(
        &mut self,
        object_id: ObjectId,
        opcode: u16,
        inbound: Vec<u8>,
    ) {
        self.triggers.push(PendingTrigger { object_id, opcode, inbound });
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
        self.scan_outbound_and_fire();
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbound)
    }

    fn recv(&mut self) -> Vec<u8> {
        self.inbound.pop_front().unwrap_or_default()
    }
}

const REGISTRY_ID: ObjectId = ObjectId::new(3);
/// With seat advertised + shell_manager NOT advertised, the
/// id allocator hands out: registry=3, compositor=5, shm=7,
/// xdg_shell=9, seat=11, pointer=13, keyboard=15. Then
/// `Window::new` allocates surface=17, toplevel=19.
const KEYBOARD_ID: ObjectId = ObjectId::new(15);
const SURFACE_ID: ObjectId = ObjectId::new(17);
const TOPLEVEL_ID: ObjectId = ObjectId::new(19);

const SURFACE_OPCODE_COMMIT: u16 = 7;

fn build_global_event(name: u32, interface: &str, version: u32) -> Vec<u8> {
    let event = RegistryGlobal {
        name,
        interface: interface.to_string(),
        version,
    };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header =
        MessageHeader::try_new(REGISTRY_ID, 1 /* global */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
    out
}

fn build_configure_event(serial: u32, width: i32, height: i32) -> Vec<u8> {
    let event = XdgToplevelConfigure { serial, width, height, states: 0 };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header =
        MessageHeader::try_new(TOPLEVEL_ID, 1 /* configure */, payload.len(), 0).unwrap();
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
    let event = KeyboardKey { surface_id: SURFACE_ID, key, state };
    let mut payload = Vec::new();
    event.encode(&mut payload);
    let mut out = vec![0u8; HEADER_SIZE + payload.len()];
    let header =
        MessageHeader::try_new(KEYBOARD_ID, 1 /* key */, payload.len(), 0).unwrap();
    header.encode(&mut out[..HEADER_SIZE]).unwrap();
    out[HEADER_SIZE..].copy_from_slice(&payload);
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
const KEY_PRESSED: u32 = 1;

#[test]
fn run_term_completes_iteration_limit_when_no_close() {
    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        build_configure_event(1, 720, 480),
    );

    let exit = run_term_with_options(conn, 5, quiet_options())
        .expect("run_term must succeed");
    assert_eq!(exit, TermExit::IterationLimit);
}

#[test]
fn run_term_exits_on_close_request() {
    let mut conn = MockConnection::new();
    seed_full_registry(&mut conn);
    let mut configure_then_close = build_configure_event(1, 640, 400);
    configure_then_close.extend(build_close_event());
    conn.push_inbound_on_request(
        SURFACE_ID,
        SURFACE_OPCODE_COMMIT,
        configure_then_close,
    );

    let exit = run_term_with_options(conn, 5, quiet_options())
        .expect("run_term must succeed");
    assert_eq!(exit, TermExit::CloseRequested);
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
    struct Tee {
        inner: MockConnection,
        recorded: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    }
    impl Connection for Tee {
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

    let conn = Tee { inner: conn, recorded: recorded_clone };
    let _ = run_term_with_options(conn, 5, quiet_options())
        .expect("run_term must succeed");

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
    // events. Seat id is 11 in this allocation order.
    let seat_id = ObjectId::new(11);
    let get_keyboard_count = headers
        .iter()
        .filter(|(obj, op, _)| *obj == seat_id && *op == 2 /* get_keyboard */)
        .count();
    assert_eq!(get_keyboard_count, 1, "seat.get_keyboard must fire once");
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
    let exit = run_term_with_options(conn, 16, quiet_options())
        .expect("run_term must succeed");
    assert_eq!(exit, TermExit::ShellExited);
}
