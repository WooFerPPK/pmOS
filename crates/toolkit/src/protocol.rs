//! Client-side display-protocol layer.
//!
//! The [`Client`] type is the toolkit's mirror of the
//! `display_server::client::Client` state machine: where the
//! server receives requests and emits events, the toolkit
//! client sends requests and parses events. Both sides speak
//! the identical wire format from the shared [`display_proto`]
//! crate so they cannot drift out of sync.
//!
//! The client is transport-agnostic. Production callers wrap a
//! [`Connection`] that round-trips bytes through a real IPC
//! socket (the `display_connect` fd returned by the kernel's
//! `display_connect` extension syscall). Tests use
//! [`MemoryConnection`], an in-memory byte queue, so the state
//! machine can be exercised without any transport glue at all.
//!
//! ## Shape of a client session
//!
//! 1. Construct a [`Client`] around a `Connection`.
//!    `pmd_display` is pre-bound at [`ObjectId::DISPLAY`].
//! 2. Call [`Client::get_registry`] to allocate a fresh
//!    client-side id, bind it to [`Interface::Registry`], and
//!    send `pmd_display.get_registry(new_id)` on the wire.
//! 3. Call [`Client::send_request`] for lower-level sends,
//!    [`Client::bind_new`] to allocate a client-side id for a
//!    newly-bound global or child object.
//! 4. Feed any bytes received from the server through
//!    [`Client::push_received`] — it frames them through
//!    `MessageHeader::decode`, validates the target object
//!    and opcode against the interface's EVENT table, and
//!    returns every parsed event in order.
//!
//! The client does NOT interpret event payloads yet; each
//! [`ClientEvent`] carries enough metadata for higher layers
//! (app, widget) to hand the right payload slice to the right
//! decoder once the typed-payload layer lands.

use std::collections::BTreeMap;

use display_proto::ids::{IdAllocator, IdKind, ObjectId};
use display_proto::objects::{Interface, OpcodeError};
use display_proto::wire::{MessageHeader, WireError, HEADER_SIZE};

/// Opcode for `pmd_display.get_registry`. Duplicated here as a
/// named constant so the toolkit doesn't have to look up magic
/// numbers at call sites.
const DISPLAY_OPCODE_GET_REGISTRY: u16 = 2;

/// Transport-abstraction trait: a thing the toolkit writes
/// outbound bytes into and reads outbound bytes out of.
///
/// Production callers pair a `Connection` impl with an
/// incoming-byte feed loop that calls
/// [`Client::push_received`] with bytes as they arrive from
/// the server. Tests use [`MemoryConnection`] — an in-memory
/// byte queue — so the state machine is unit-testable without
/// any IPC glue at all.
pub trait Connection {
    /// Enqueue a framed outbound message. The caller has
    /// already produced a full `MessageHeader::encode` +
    /// payload sequence; this method is transport-level and
    /// does NOT look at the bytes.
    fn send(&mut self, bytes: &[u8]);

    /// Remove and return all pending outbound bytes. Used by
    /// callers that want to forward the buffered bytes to a
    /// real transport (or by tests that want to assert on
    /// what was sent).
    fn drain_outbound(&mut self) -> Vec<u8>;
}

/// In-memory [`Connection`] for tests. `send` appends to the
/// internal buffer; `drain_outbound` takes the buffer.
#[derive(Default)]
pub struct MemoryConnection {
    outbound: Vec<u8>,
}

impl MemoryConnection {
    pub fn new() -> Self {
        MemoryConnection::default()
    }

    /// Non-draining view of the buffered bytes. Used by tests
    /// that want to inspect state without consuming it.
    pub fn outbound(&self) -> &[u8] {
        &self.outbound
    }
}

impl Connection for MemoryConnection {
    fn send(&mut self, bytes: &[u8]) {
        self.outbound.extend_from_slice(bytes);
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbound)
    }
}

/// A single parsed server event.
///
/// Mirrors `display_server::client::HandledRequest` in shape
/// so the two sides of the protocol converge on the same
/// record structure — one is "request the server saw", one is
/// "event the client saw".
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClientEvent {
    pub object_id: ObjectId,
    pub interface: Interface,
    pub opcode: u16,
    pub opcode_name: &'static str,
    pub payload_len: usize,
    pub fd_passing: u8,
}

/// Errors surfaced by [`Client`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// Wire-format error (truncated input, invalid length,
    /// reserved-byte non-zero, encode overflow, etc.).
    Wire(WireError),
    /// A received or sent message targets an object the
    /// client doesn't have in its table.
    UnknownObject { id: ObjectId },
    /// The opcode is not a known request (outbound) or event
    /// (inbound) on the target object's interface.
    UnknownOpcode { interface: Interface, opcode: u16 },
    /// The opcode exists but in the other direction — a
    /// request opcode arrived as an event, or vice versa.
    WrongDirection { interface: Interface, opcode: u16 },
    /// An attempt to bind a new object at an ID the server
    /// side of the partition would have allocated.
    IllegalBindTarget { requested: ObjectId },
    /// An attempt to bind a second object at an already-used
    /// ID.
    DuplicateObject { id: ObjectId },
    /// Client-side ID allocator is out of odd IDs.
    IdsExhausted,
    /// Partial message at the end of a byte stream — the
    /// caller should buffer more bytes and retry.
    NeedMoreBytes { have: usize, need: usize },
}

impl From<WireError> for ClientError {
    fn from(e: WireError) -> Self {
        ClientError::Wire(e)
    }
}

/// Per-connection client state.
pub struct Client<C: Connection> {
    conn: C,
    objects: BTreeMap<ObjectId, Interface>,
    ids: IdAllocator,
}

impl<C: Connection> Client<C> {
    /// Build a client wrapping `conn`, with `pmd_display`
    /// pre-bound at `ObjectId::DISPLAY` and a fresh
    /// client-side ID allocator ready to hand out odd IDs
    /// starting at 3.
    pub fn new(conn: C) -> Self {
        let mut objects = BTreeMap::new();
        objects.insert(ObjectId::DISPLAY, Interface::Display);
        Client {
            conn,
            objects,
            ids: IdAllocator::for_client(),
        }
    }

    /// Borrow the underlying connection.
    pub fn connection(&self) -> &C {
        &self.conn
    }

    /// Mutably borrow the underlying connection.
    pub fn connection_mut(&mut self) -> &mut C {
        &mut self.conn
    }

    /// Drain outbound bytes directly from the connection.
    pub fn drain_outbound(&mut self) -> Vec<u8> {
        self.conn.drain_outbound()
    }

    /// Number of currently-bound objects in this client's table.
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Borrow the interface type for an object, if any.
    pub fn get(&self, id: ObjectId) -> Option<Interface> {
        self.objects.get(&id).copied()
    }

    /// Allocate the next client-side ID. Returns
    /// `IdsExhausted` if the partition is full.
    pub fn allocate_id(&mut self) -> Result<ObjectId, ClientError> {
        self.ids
            .allocate()
            .map_err(|_| ClientError::IdsExhausted)
    }

    /// Allocate a fresh client-side ID AND bind it to the
    /// given interface in the client's object table. This is
    /// the one-stop helper for the `new_id` argument pattern
    /// every request-that-creates-an-object uses.
    pub fn bind_new(&mut self, interface: Interface) -> Result<ObjectId, ClientError> {
        let id = self.allocate_id()?;
        if self.objects.contains_key(&id) {
            return Err(ClientError::DuplicateObject { id });
        }
        self.objects.insert(id, interface);
        Ok(id)
    }

    /// Bind an already-allocated server-side ID into the
    /// client's table. Used when the server advertises an
    /// object the client didn't ask for (e.g. `pmd_output`
    /// advertised through `pmd_registry.global`).
    pub fn bind_server_id(
        &mut self,
        id: ObjectId,
        interface: Interface,
    ) -> Result<(), ClientError> {
        if id.kind() != IdKind::Server {
            return Err(ClientError::IllegalBindTarget { requested: id });
        }
        if self.objects.contains_key(&id) {
            return Err(ClientError::DuplicateObject { id });
        }
        self.objects.insert(id, interface);
        Ok(())
    }

    /// Drop an object from the client's table. Used by
    /// `destroy` flows and on `pmd_display.delete_id` events.
    /// Returns true iff an object was actually removed.
    pub fn drop_object(&mut self, id: ObjectId) -> bool {
        self.objects.remove(&id).is_some()
    }

    /// Send a raw request. Validates that `object_id` exists
    /// in the client's table AND that `opcode` is a known
    /// REQUEST on its interface. The payload bytes are
    /// appended after the encoded header and written out
    /// through the connection in one `send` call.
    pub fn send_request(
        &mut self,
        object_id: ObjectId,
        opcode: u16,
        payload: &[u8],
    ) -> Result<(), ClientError> {
        let interface = self
            .objects
            .get(&object_id)
            .copied()
            .ok_or(ClientError::UnknownObject { id: object_id })?;
        interface
            .lookup_request(opcode)
            .map_err(|e| map_opcode_error(e, Direction::Request))?;
        let header = MessageHeader::try_new(object_id, opcode, payload.len(), 0)
            .map_err(ClientError::Wire)?;
        let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());
        buf.resize(HEADER_SIZE, 0);
        header
            .encode(&mut buf[..HEADER_SIZE])
            .map_err(ClientError::Wire)?;
        buf.extend_from_slice(payload);
        self.conn.send(&buf);
        Ok(())
    }

    /// Send `pmd_display.get_registry(new_id)`. Allocates the
    /// fresh registry id, binds it to [`Interface::Registry`]
    /// in the client table, and sends the framed request in
    /// one call. Returns the allocated id so the caller can
    /// later send `registry.bind` through it.
    pub fn get_registry(&mut self) -> Result<ObjectId, ClientError> {
        let registry_id = self.bind_new(Interface::Registry)?;
        let payload = registry_id.raw().to_le_bytes();
        self.send_request(ObjectId::DISPLAY, DISPLAY_OPCODE_GET_REGISTRY, &payload)?;
        Ok(registry_id)
    }

    /// Send `pmd_registry.bind(name, interface, version, new_id)`.
    /// Allocates a fresh client-side id, binds it to
    /// `target` in the client's table, and sends the framed
    /// request. The `interface` string encoded on the wire
    /// is `target.name()`, so the server's
    /// `Interface::from_name` lookup round-trips correctly.
    pub fn registry_bind(
        &mut self,
        registry_id: ObjectId,
        global_name: u32,
        target: Interface,
        version: u32,
    ) -> Result<ObjectId, ClientError> {
        let new_id = self.bind_new(target)?;
        let name_str = target.name();
        let bytes = name_str.as_bytes();
        let pad = (4 - (bytes.len() % 4)) % 4;
        let mut payload = Vec::with_capacity(4 + 4 + bytes.len() + pad + 4 + 4);
        payload.extend_from_slice(&global_name.to_le_bytes());
        payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        payload.extend_from_slice(bytes);
        payload.extend(core::iter::repeat(0u8).take(pad));
        payload.extend_from_slice(&version.to_le_bytes());
        payload.extend_from_slice(&new_id.raw().to_le_bytes());
        self.send_request(registry_id, 1 /* bind */, &payload)?;
        Ok(new_id)
    }

    /// Send `pmd_compositor.create_surface(new_id)`. Allocates
    /// the fresh surface id, binds it to
    /// [`Interface::Surface`], and sends the framed request.
    pub fn compositor_create_surface(
        &mut self,
        compositor_id: ObjectId,
    ) -> Result<ObjectId, ClientError> {
        let surface_id = self.bind_new(Interface::Surface)?;
        self.send_request(
            compositor_id,
            1, /* create_surface */
            &surface_id.raw().to_le_bytes(),
        )?;
        Ok(surface_id)
    }

    /// Send `pmd_surface.commit()` — no payload.
    pub fn surface_commit(&mut self, surface_id: ObjectId) -> Result<(), ClientError> {
        self.send_request(surface_id, 7 /* commit */, &[])
    }

    /// Parse as many complete server-bound events out of the
    /// input byte stream as possible. Stops at the first
    /// partial message (returning the events parsed so far
    /// plus [`ClientError::NeedMoreBytes`] via the second
    /// return slot) or at a framing error.
    ///
    /// The caller is responsible for buffering incomplete
    /// bytes between calls — this function does not retain
    /// any state about unparsed input.
    ///
    /// Returns `(events, consumed)` on success: `events` is
    /// the decoded event list, `consumed` is how many bytes
    /// of `input` were eaten. The caller may then drop those
    /// bytes from its own buffer and call again when more
    /// arrive.
    pub fn push_received(
        &mut self,
        input: &[u8],
    ) -> Result<(Vec<ClientEvent>, usize), ClientError> {
        let mut cursor = 0usize;
        let mut events = Vec::new();
        while cursor < input.len() {
            let remaining = &input[cursor..];
            if remaining.len() < HEADER_SIZE {
                break;
            }
            let header = MessageHeader::decode(remaining)?;
            let msg_len = header.length as usize;
            if remaining.len() < msg_len {
                break;
            }
            let interface = self
                .objects
                .get(&header.object_id)
                .copied()
                .ok_or(ClientError::UnknownObject {
                    id: header.object_id,
                })?;
            let opcode = interface
                .lookup_event(header.opcode)
                .map_err(|e| map_opcode_error(e, Direction::Event))?;
            events.push(ClientEvent {
                object_id: header.object_id,
                interface,
                opcode: header.opcode,
                opcode_name: opcode.name,
                payload_len: header.payload_len(),
                fd_passing: header.fd_passing,
            });
            cursor += msg_len;
        }
        Ok((events, cursor))
    }
}

/// Which direction the caller EXPECTED an opcode to have —
/// used to phrase `UnknownOpcode` vs. `WrongDirection` errors
/// correctly.
#[derive(Copy, Clone)]
enum Direction {
    Request,
    Event,
}

fn map_opcode_error(err: OpcodeError, expected: Direction) -> ClientError {
    match err {
        OpcodeError::UnknownOpcode { interface, opcode } => {
            // Cross-check: maybe the opcode exists in the
            // other direction, in which case the error is
            // really "wrong direction", not "unknown opcode".
            let other_has_it = match expected {
                Direction::Request => interface.lookup_event(opcode).is_ok(),
                Direction::Event => interface.lookup_request(opcode).is_ok(),
            };
            if other_has_it {
                ClientError::WrongDirection { interface, opcode }
            } else {
                ClientError::UnknownOpcode { interface, opcode }
            }
        }
    }
}
