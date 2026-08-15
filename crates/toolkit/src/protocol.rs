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

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use display_proto::ids::{IdAllocator, IdKind, ObjectId};
use display_proto::objects::{Interface, OpcodeError};
use display_proto::wire::{MessageHeader, WireError, HEADER_SIZE};

/// Additional kernel fd included alongside the display socket in one wait.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WaitFd {
    pub fd: i32,
    pub interest: WaitInterest,
}

impl WaitFd {
    pub const fn readable(fd: i32) -> Self {
        Self {
            fd,
            interest: WaitInterest::Read,
        }
    }

    pub const fn writable(fd: i32) -> Self {
        Self {
            fd,
            interest: WaitInterest::Write,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WaitInterest {
    Read,
    Write,
}

/// Opcode for `pmd_display.get_registry`. Duplicated here as a
/// named constant so the toolkit doesn't have to look up magic
/// numbers at call sites.
const DISPLAY_OPCODE_GET_REGISTRY: u16 = 2;
const DISPLAY_OPCODE_SYNC: u16 = 1;

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

    /// Advance at most one bounded transport-write quantum. In-memory
    /// connections have no separate transport queue; production fd transport
    /// overrides this method.
    fn flush_outbound(&mut self) -> Result<(), i32> {
        Ok(())
    }

    fn outbound_pending(&self) -> bool {
        false
    }

    /// Production fd transports opt into bounded multi-turn pixel uploads.
    /// Native/in-memory fixtures retain immediate commit behavior.
    fn incremental_uploads(&self) -> bool {
        false
    }

    /// Remove and return all pending outbound bytes. Used by
    /// callers that want to forward the buffered bytes to a
    /// real transport (or by tests that want to assert on
    /// what was sent).
    fn drain_outbound(&mut self) -> Vec<u8>;

    /// Pull whatever bytes the peer has made available for
    /// inbound delivery. Returns an empty vec when nothing is
    /// ready. Transports that are strictly outbound (the
    /// default [`MemoryConnection`] used by existing tests
    /// that only drive requests) inherit the default empty
    /// implementation; bidirectional transports override to
    /// surface server events. This method must not advance the
    /// outbound queue: one explicit [`Connection::flush_outbound`]
    /// call is the event loop's complete write quantum for a turn.
    fn recv(&mut self) -> Vec<u8> {
        Vec::new()
    }

    /// Park until inbound transport data is readable or an optional real-duty
    /// deadline expires. In-memory/native test transports keep the default
    /// immediate success; the production WASI fd transport overrides this
    /// with `poll_oneoff`. Waiting selects readiness from the current queue
    /// state but must not perform a hidden write first.
    fn wait(&mut self, _timeout: Option<Duration>) -> Result<(), i32> {
        Ok(())
    }

    /// Park on the display transport plus additional application-owned fds.
    fn wait_with(&mut self, _additional: &[WaitFd], timeout: Option<Duration>) -> Result<(), i32> {
        self.wait(timeout)
    }
}

/// In-memory [`Connection`] for tests. `send` appends to the
/// outbound buffer; `drain_outbound` takes that buffer.
/// `feed_inbound` lets paired-with-server tests stage bytes the
/// server has emitted; `recv` drains those staged bytes.
#[derive(Default)]
pub struct MemoryConnection {
    outbound: Vec<u8>,
    inbound: Vec<u8>,
}

impl MemoryConnection {
    pub fn new() -> Self {
        MemoryConnection::default()
    }

    /// Non-draining view of the buffered outbound bytes. Used
    /// by tests that want to inspect state without consuming
    /// it.
    pub fn outbound(&self) -> &[u8] {
        &self.outbound
    }

    /// Stage a batch of bytes on the inbound side. The next
    /// call to [`Connection::recv`] returns everything that has
    /// been fed so far. Used by tests that pair an `App` with a
    /// real `display_server::Server` and shuttle the server's
    /// outbound bytes back into the client.
    pub fn feed_inbound(&mut self, bytes: &[u8]) {
        self.inbound.extend_from_slice(bytes);
    }
}

impl Connection for MemoryConnection {
    fn send(&mut self, bytes: &[u8]) {
        self.outbound.extend_from_slice(bytes);
        // Isolation fixtures pre-stage registry globals without running a
        // server. Still complete the real protocol ordering handshake: when
        // the client sends display.sync, enqueue the corresponding typed
        // callback.done marker rather than letting App infer completion from
        // an empty in-memory queue.
        let Ok(header) = MessageHeader::decode(bytes) else {
            return;
        };
        if header.object_id != ObjectId::DISPLAY
            || header.opcode != DISPLAY_OPCODE_SYNC
            || header.length as usize > bytes.len()
        {
            return;
        }
        let payload = &bytes[HEADER_SIZE..header.length as usize];
        let Some(raw_id) = payload.get(..4) else {
            return;
        };
        let callback_id = ObjectId::new(u32::from_le_bytes(raw_id.try_into().unwrap()));
        let done_payload = 0u32.to_le_bytes();
        let Ok(done_header) = MessageHeader::try_new(callback_id, 1, done_payload.len(), 0) else {
            return;
        };
        let start = self.inbound.len();
        self.inbound.resize(start + HEADER_SIZE, 0);
        if done_header
            .encode(&mut self.inbound[start..start + HEADER_SIZE])
            .is_err()
        {
            self.inbound.truncate(start);
            return;
        }
        self.inbound.extend_from_slice(&done_payload);
    }

    fn drain_outbound(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbound)
    }

    fn recv(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.inbound)
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

/// A parsed server event that additionally carries the raw
/// payload bytes from the wire.
///
/// Returned by [`Client::push_received_with_payload`] so
/// callers can run one of the typed-event decoders from
/// [`display_proto::events`] on the payload. Unlike
/// [`ClientEvent`], this struct allocates: the payload is
/// copied out of the input buffer so the event can outlive
/// the caller's byte stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientEventWithPayload {
    pub object_id: ObjectId,
    pub interface: Interface,
    pub opcode: u16,
    pub opcode_name: &'static str,
    pub payload: Vec<u8>,
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
    /// A required global was not advertised by the server
    /// during the [`crate::app::App::connect`] bootstrap
    /// handshake. Carried name is the interface wire name
    /// (e.g. `"pmd_compositor"`).
    MissingGlobal(&'static str),
    /// A packed pool-row request had zero/overlapping geometry or its inline
    /// bytes did not exactly match `row_bytes * rows`.
    InvalidWriteRows {
        row_bytes: u32,
        rows: u32,
        stride: u32,
        bytes_len: usize,
    },
    /// A current-surface patch was empty, negative, exceeded the bounded
    /// payload cap, or did not carry exactly `width * height * 4` pixels.
    InvalidSurfacePatch {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        pixels_len: usize,
    },
    /// A buffer pool was asked to continue a surface-bound frame transaction
    /// through a different window.
    BufferPoolSurfaceMismatch {
        expected_surface: ObjectId,
        actual_surface: ObjectId,
    },
    /// A second frame was submitted before the staged upload completed.
    CommitInProgress,
    /// A damage transaction exceeded the toolkit's bounded normalization
    /// input. Keeping this small prevents adversarial overlap patterns from
    /// fragmenting into an unbounded number of internal rectangles.
    TooManyDamageRegions { supplied: usize, max: usize },
    /// Transport-level failure surfaced by an explicit blocking wait.
    Transport(i32),
}

impl From<WireError> for ClientError {
    fn from(e: WireError) -> Self {
        ClientError::Wire(e)
    }
}

/// Per-connection client state.
pub struct Client<C: Connection> {
    conn: C,
    /// Interface metadata remains available after a destroy request so events
    /// the server queued before processing that request can still be decoded.
    /// Entries are removed only when `display.delete_id` arrives.
    objects: BTreeMap<ObjectId, Interface>,
    /// Objects whose destroy request was queued successfully. Retired objects
    /// are valid inbound event targets but can no longer send requests.
    retired_objects: BTreeSet<ObjectId>,
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
            retired_objects: BTreeSet::new(),
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

    pub fn flush_outbound(&mut self) -> Result<(), ClientError> {
        self.conn.flush_outbound().map_err(ClientError::Transport)
    }

    pub fn outbound_pending(&self) -> bool {
        self.conn.outbound_pending()
    }

    /// Park the underlying transport for inbound readiness and an optional
    /// deadline. Callers invoke this only after draining events and paint.
    pub fn wait(&mut self, timeout: Option<Duration>) -> Result<(), ClientError> {
        self.conn.wait(timeout).map_err(ClientError::Transport)
    }

    pub fn wait_with(
        &mut self,
        additional: &[WaitFd],
        timeout: Option<Duration>,
    ) -> Result<(), ClientError> {
        self.conn
            .wait_with(additional, timeout)
            .map_err(ClientError::Transport)
    }

    /// Number of live objects that can still send requests. Tombstones kept
    /// solely for ordered inbound dispatch are excluded.
    pub fn object_count(&self) -> usize {
        self.objects
            .len()
            .saturating_sub(self.retired_objects.len())
    }

    /// Borrow the interface type for a live object, if any. A retired object
    /// deliberately appears absent to request-producing callers even though
    /// its interface metadata remains available to inbound dispatch.
    pub fn get(&self, id: ObjectId) -> Option<Interface> {
        if self.retired_objects.contains(&id) {
            return None;
        }
        self.objects.get(&id).copied()
    }

    /// Whether `id` is awaiting the server's `display.delete_id`
    /// acknowledgement after local destruction.
    pub fn is_retired(&self, id: ObjectId) -> bool {
        self.retired_objects.contains(&id)
    }

    /// Allocate the next client-side ID. Returns
    /// `IdsExhausted` if the partition is full.
    pub fn allocate_id(&mut self) -> Result<ObjectId, ClientError> {
        self.ids.allocate().map_err(|_| ClientError::IdsExhausted)
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

    /// Unconditionally drop an object and any tombstone from the client's
    /// table. Most protocol lifecycle code should use [`Self::retire_object`]
    /// after destroy and [`Self::acknowledge_delete_id`] for the server's final
    /// acknowledgement. This escape hatch remains useful for rolling back
    /// objects that never reached the server.
    pub fn drop_object(&mut self, id: ObjectId) -> bool {
        self.retired_objects.remove(&id);
        self.objects.remove(&id).is_some()
    }

    /// End outbound authority for an object while retaining enough interface
    /// metadata to decode events the server queued before processing destroy.
    /// Returns true only for the live-to-retired transition.
    pub fn retire_object(&mut self, id: ObjectId) -> bool {
        id != ObjectId::DISPLAY && self.objects.contains_key(&id) && self.retired_objects.insert(id)
    }

    /// Apply `pmd_display.delete_id`: remove the event-dispatch tombstone (or
    /// an automatically destroyed live object) from the local object table.
    /// Returns true iff the ID was known locally.
    pub fn acknowledge_delete_id(&mut self, id: ObjectId) -> bool {
        if id == ObjectId::DISPLAY {
            return false;
        }
        self.retired_objects.remove(&id);
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
        if self.retired_objects.contains(&object_id) {
            return Err(ClientError::UnknownObject { id: object_id });
        }
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

    /// Send `pmd_display.sync(new_id callback)`. The server emits one
    /// `pmd_callback.done` only after all earlier requests and events have
    /// been processed, providing an explicit registry-catalog boundary.
    pub fn sync(&mut self) -> Result<ObjectId, ClientError> {
        let callback_id = self.bind_new(Interface::Callback)?;
        let payload = callback_id.raw().to_le_bytes();
        self.send_request(ObjectId::DISPLAY, DISPLAY_OPCODE_SYNC, &payload)?;
        Ok(callback_id)
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
        payload.extend(core::iter::repeat_n(0u8, pad));
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

    /// Request a one-shot callback for the surface's next commit. The callback
    /// is a typed protocol object immediately, so `pmd_callback.done` can be
    /// decoded normally. If request validation fails, roll the local binding
    /// back before returning the error.
    pub fn surface_frame(&mut self, surface_id: ObjectId) -> Result<ObjectId, ClientError> {
        let callback_id = self.bind_new(Interface::Callback)?;
        let payload = callback_id.raw().to_le_bytes();
        if let Err(error) = self.send_request(surface_id, 4 /* frame */, &payload) {
            self.drop_object(callback_id);
            return Err(error);
        }
        Ok(callback_id)
    }

    /// Atomically replace one tightly-packed rectangle in a surface's current
    /// buffer and commit that patch as one protocol request.
    pub fn surface_patch_current(
        &mut self,
        surface_id: ObjectId,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<(), ClientError> {
        let expected = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|area| area.checked_mul(4));
        if x < 0
            || y < 0
            || width == 0
            || height == 0
            || expected != Some(pixels.len() as u64)
            || pixels.len() > display_proto::MAX_SURFACE_PATCH_BYTES
        {
            return Err(ClientError::InvalidSurfacePatch {
                x,
                y,
                width,
                height,
                pixels_len: pixels.len(),
            });
        }
        let payload_len = 16usize
            .checked_add(pixels.len())
            .ok_or(ClientError::Wire(WireError::Overflow))?;
        if payload_len + HEADER_SIZE > display_proto::wire::MAX_MESSAGE_SIZE {
            return Err(ClientError::Wire(WireError::Overflow));
        }
        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(&x.to_le_bytes());
        payload.extend_from_slice(&y.to_le_bytes());
        payload.extend_from_slice(&width.to_le_bytes());
        payload.extend_from_slice(&height.to_le_bytes());
        payload.extend_from_slice(pixels);
        self.send_request(surface_id, 8 /* patch_current */, &payload)
    }

    /// Send `pmd_shm.create_pool(new_id, size)`. Allocates a
    /// fresh pool id, binds it to [`Interface::ShmPool`] in
    /// the client table, and sends the framed request.
    ///
    /// In the v1 native-host test path the SAB fd is not
    /// actually transmitted — the server trusts `size`. Once
    /// the kernel's display-server host is wired to the
    /// ring's aux fd channel, `fd_passing` on the header
    /// will carry the pool's SAB handle.
    pub fn shm_create_pool(
        &mut self,
        shm_id: ObjectId,
        size: u32,
    ) -> Result<ObjectId, ClientError> {
        let pool_id = self.bind_new(Interface::ShmPool)?;
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&pool_id.raw().to_le_bytes());
        payload[4..].copy_from_slice(&size.to_le_bytes());
        self.send_request(shm_id, 1 /* create_pool */, &payload)?;
        Ok(pool_id)
    }

    /// Send `pmd_shm_pool.create_buffer(new_id, offset,
    /// width, height, stride, format)`. Allocates a fresh
    /// buffer id, binds it to [`Interface::Buffer`], and
    /// sends the framed request.
    pub fn shm_pool_create_buffer(
        &mut self,
        pool_id: ObjectId,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) -> Result<ObjectId, ClientError> {
        let buffer_id = self.bind_new(Interface::Buffer)?;
        let mut payload = [0u8; 24];
        payload[0..4].copy_from_slice(&buffer_id.raw().to_le_bytes());
        payload[4..8].copy_from_slice(&offset.to_le_bytes());
        payload[8..12].copy_from_slice(&width.to_le_bytes());
        payload[12..16].copy_from_slice(&height.to_le_bytes());
        payload[16..20].copy_from_slice(&stride.to_le_bytes());
        payload[20..24].copy_from_slice(&format.to_le_bytes());
        self.send_request(pool_id, 1 /* create_buffer */, &payload)?;
        Ok(buffer_id)
    }

    /// Send `pmd_shm_pool.write(offset, bytes)` — v1 affordance
    /// that copies `bytes` into the server-side pool storage at
    /// `offset`. Wayland proper shares pool memory via fd; v1
    /// elides that, so the toolkit calls this for changed pool
    /// regions before commit so the server's compositor sees the
    /// painted pixels without retransmitting unchanged regions.
    pub fn shm_pool_write(
        &mut self,
        pool_id: ObjectId,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), ClientError> {
        let mut payload = Vec::with_capacity(4 + bytes.len());
        payload.extend_from_slice(&offset.to_le_bytes());
        payload.extend_from_slice(bytes);
        self.send_request(pool_id, 4 /* write */, &payload)
    }

    /// Send `pmd_shm_pool.write_rows(offset, row_bytes, rows, stride, bytes)`.
    /// `bytes` stores the rows densely; the server copies each row into pool
    /// backing separated by `stride`. This keeps a narrow damage rectangle in
    /// one bounded request even when framebuffer scanlines are far apart.
    pub fn shm_pool_write_rows(
        &mut self,
        pool_id: ObjectId,
        offset: u32,
        row_bytes: u32,
        rows: u32,
        stride: u32,
        bytes: &[u8],
    ) -> Result<(), ClientError> {
        let expected = u64::from(row_bytes) * u64::from(rows);
        if row_bytes == 0 || rows == 0 || stride < row_bytes || expected != bytes.len() as u64 {
            return Err(ClientError::InvalidWriteRows {
                row_bytes,
                rows,
                stride,
                bytes_len: bytes.len(),
            });
        }
        let payload_len = 16usize
            .checked_add(bytes.len())
            .ok_or(ClientError::Wire(WireError::Overflow))?;
        if payload_len + HEADER_SIZE > display_proto::wire::MAX_MESSAGE_SIZE {
            return Err(ClientError::Wire(WireError::Overflow));
        }
        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(&offset.to_le_bytes());
        payload.extend_from_slice(&row_bytes.to_le_bytes());
        payload.extend_from_slice(&rows.to_le_bytes());
        payload.extend_from_slice(&stride.to_le_bytes());
        payload.extend_from_slice(bytes);
        self.send_request(pool_id, 5 /* write_rows */, &payload)
    }

    /// Resize a live `pmd_shm_pool`. Server-side admission validates both the
    /// per-connection and global byte budgets before changing backing storage.
    pub fn shm_pool_resize(&mut self, pool_id: ObjectId, new_size: u32) -> Result<(), ClientError> {
        self.send_request(pool_id, 2 /* resize */, &new_size.to_le_bytes())
    }

    /// Destroy a pool protocol object. Buffer resources that still reference
    /// its backing remain valid on the server until those buffers are retired.
    /// Locally the pool becomes an inbound-only tombstone until delete_id.
    pub fn shm_pool_destroy(&mut self, pool_id: ObjectId) -> Result<(), ClientError> {
        self.send_request(pool_id, 3 /* destroy */, &[])?;
        self.retire_object(pool_id);
        Ok(())
    }

    /// Destroy a buffer protocol object. A surface that already retained the
    /// buffer may continue displaying it until its next attach/commit. Locally
    /// the buffer becomes an inbound-only tombstone until delete_id.
    pub fn buffer_destroy(&mut self, buffer_id: ObjectId) -> Result<(), ClientError> {
        self.send_request(buffer_id, 1 /* destroy */, &[])?;
        self.retire_object(buffer_id);
        Ok(())
    }

    /// Destroy a roleless surface, retiring it from outbound use while keeping
    /// inbound metadata until delete_id. A surface with a live xdg-toplevel
    /// role must destroy that role first.
    pub fn surface_destroy(&mut self, surface_id: ObjectId) -> Result<(), ClientError> {
        self.send_request(surface_id, 1 /* destroy */, &[])?;
        self.retire_object(surface_id);
        Ok(())
    }

    /// Send `pmd_surface.attach(buffer_id, x, y)`. Does NOT
    /// install any new object — the buffer is already
    /// bound via [`Client::shm_pool_create_buffer`].
    pub fn surface_attach(
        &mut self,
        surface_id: ObjectId,
        buffer_id: ObjectId,
        x: i32,
        y: i32,
    ) -> Result<(), ClientError> {
        let mut payload = [0u8; 12];
        payload[0..4].copy_from_slice(&buffer_id.raw().to_le_bytes());
        payload[4..8].copy_from_slice(&x.to_le_bytes());
        payload[8..12].copy_from_slice(&y.to_le_bytes());
        self.send_request(surface_id, 2 /* attach */, &payload)
    }

    /// Send `pmd_surface.damage(x, y, width, height)`.
    pub fn surface_damage(
        &mut self,
        surface_id: ObjectId,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), ClientError> {
        let mut payload = [0u8; 16];
        payload[0..4].copy_from_slice(&x.to_le_bytes());
        payload[4..8].copy_from_slice(&y.to_le_bytes());
        payload[8..12].copy_from_slice(&width.to_le_bytes());
        payload[12..16].copy_from_slice(&height.to_le_bytes());
        self.send_request(surface_id, 3 /* damage */, &payload)
    }

    /// Send `pmd_xdg_shell.get_toplevel(new_id, surface_id)`.
    /// Allocates a fresh toplevel id bound to
    /// [`Interface::XdgToplevel`] and sends the framed
    /// request. Returns the allocated id so the caller can
    /// follow up with `set_title` / `set_app_id`.
    pub fn xdg_shell_get_toplevel(
        &mut self,
        xdg_shell_id: ObjectId,
        surface_id: ObjectId,
    ) -> Result<ObjectId, ClientError> {
        let toplevel_id = self.bind_new(Interface::XdgToplevel)?;
        let mut payload = [0u8; 8];
        payload[0..4].copy_from_slice(&toplevel_id.raw().to_le_bytes());
        payload[4..8].copy_from_slice(&surface_id.raw().to_le_bytes());
        self.send_request(xdg_shell_id, 1 /* get_toplevel */, &payload)?;
        Ok(toplevel_id)
    }

    /// Send `pmd_xdg_toplevel.set_title(string)`. The
    /// string is encoded with a `u32 length` prefix
    /// followed by the UTF-8 bytes, padded to a 4-byte
    /// boundary — the same shape `read_string` decodes
    /// on the server side.
    pub fn xdg_toplevel_set_title(
        &mut self,
        toplevel_id: ObjectId,
        title: &str,
    ) -> Result<(), ClientError> {
        let payload = encode_wire_string(title);
        self.send_request(toplevel_id, 1 /* set_title */, &payload)
    }

    /// Send `pmd_xdg_toplevel.set_app_id(string)`.
    pub fn xdg_toplevel_set_app_id(
        &mut self,
        toplevel_id: ObjectId,
        app_id: &str,
    ) -> Result<(), ClientError> {
        let payload = encode_wire_string(app_id);
        self.send_request(toplevel_id, 2 /* set_app_id */, &payload)
    }

    /// Send `pmd_xdg_toplevel.ack_configure(u32 serial)`
    /// — ack the server's most recent `configure` event so
    /// the server knows the client has seen the proposed
    /// geometry. The `Window` facade calls this from its
    /// `dispatch` cycle; callers speaking the protocol
    /// directly may also invoke it.
    pub fn xdg_toplevel_ack_configure(
        &mut self,
        toplevel_id: ObjectId,
        serial: u32,
    ) -> Result<(), ClientError> {
        self.send_request(
            toplevel_id,
            4, /* ack_configure */
            &serial.to_le_bytes(),
        )
    }

    /// Send `pmd_xdg_toplevel.set_maximized()` — ask the
    /// server to maximize this toplevel. The server replies
    /// with a `configure(serial, w, h, states | MAXIMIZED)`
    /// event when it accepts.
    pub fn xdg_toplevel_set_maximized(&mut self, toplevel_id: ObjectId) -> Result<(), ClientError> {
        self.send_request(toplevel_id, 5 /* set_maximized */, &[])
    }

    /// Send `pmd_xdg_toplevel.unset_maximized()` — ask the
    /// server to restore this toplevel from a previously-set
    /// maximized state. The server replies with
    /// `configure(serial, w, h, states & !MAXIMIZED)`.
    pub fn xdg_toplevel_unset_maximized(
        &mut self,
        toplevel_id: ObjectId,
    ) -> Result<(), ClientError> {
        self.send_request(toplevel_id, 6 /* unset_maximized */, &[])
    }

    /// Send `pmd_xdg_toplevel.move(u32 serial)` — ask the
    /// server to initiate an interactive move drag. The
    /// `serial` should be the serial of the pointer-button
    /// event that started the drag.
    pub fn xdg_toplevel_move(
        &mut self,
        toplevel_id: ObjectId,
        serial: u32,
    ) -> Result<(), ClientError> {
        self.send_request(toplevel_id, 7 /* move */, &serial.to_le_bytes())
    }

    /// Send `pmd_xdg_toplevel.resize(u32 serial, u32 edges)`
    /// — ask the server to initiate an interactive resize
    /// drag along the given edge(s). `edges` is a bitfield
    /// of [`display_proto::xdg_toplevel_resize_edge`] bits.
    pub fn xdg_toplevel_resize(
        &mut self,
        toplevel_id: ObjectId,
        serial: u32,
        edges: u32,
    ) -> Result<(), ClientError> {
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&serial.to_le_bytes());
        payload[4..].copy_from_slice(&edges.to_le_bytes());
        self.send_request(toplevel_id, 8 /* resize */, &payload)
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
            // Peek length manually before
            // `MessageHeader::decode` so a partial trailing
            // message stays in the re-assembly buffer
            // instead of triggering InvalidLength.
            let len_field = u16::from_le_bytes([remaining[6], remaining[7]]) as usize;
            if len_field < HEADER_SIZE {
                break;
            }
            if remaining.len() < len_field {
                break;
            }
            let header = MessageHeader::decode(remaining)?;
            let msg_len = header.length as usize;
            let interface =
                self.objects
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

    /// Same as [`Client::push_received`] but returns events
    /// with an owned copy of the wire payload, so callers
    /// can run one of the typed decoders from
    /// [`display_proto::events`] on the bytes.
    ///
    /// Returns `(events, consumed)`. The caller drops
    /// `consumed` bytes from its input buffer and calls
    /// again when more arrive.
    pub fn push_received_with_payload(
        &mut self,
        input: &[u8],
    ) -> Result<(Vec<ClientEventWithPayload>, usize), ClientError> {
        let mut cursor = 0usize;
        let mut events = Vec::new();
        while cursor < input.len() {
            let remaining = &input[cursor..];
            if remaining.len() < HEADER_SIZE {
                break;
            }
            // Peek the length field directly. `MessageHeader::decode`
            // errors out with `InvalidLength` when the declared
            // total exceeds the available buffer — but that's
            // exactly the "partial trailing message, wait for
            // more bytes" case for a streamed protocol. Read the
            // length manually so we can break-without-error
            // before the decoder sees it.
            let len_field = u16::from_le_bytes([remaining[6], remaining[7]]) as usize;
            if len_field < HEADER_SIZE {
                // Spec violation — discard the rest as
                // unparseable. Returning `cursor` here will
                // cause the caller to drain everything we've
                // already parsed; the rest stays in the
                // re-assembly buffer. Surfacing the error
                // would kill the connection, which is too
                // brittle for v1.
                break;
            }
            if remaining.len() < len_field {
                // Partial — leave it in the re-assembly
                // buffer for the next push.
                break;
            }
            let header = MessageHeader::decode(remaining)?;
            let msg_len = header.length as usize;
            let interface =
                self.objects
                    .get(&header.object_id)
                    .copied()
                    .ok_or(ClientError::UnknownObject {
                        id: header.object_id,
                    })?;
            let opcode = interface
                .lookup_event(header.opcode)
                .map_err(|e| map_opcode_error(e, Direction::Event))?;
            let payload = remaining[HEADER_SIZE..msg_len].to_vec();
            events.push(ClientEventWithPayload {
                object_id: header.object_id,
                interface,
                opcode: header.opcode,
                opcode_name: opcode.name,
                payload,
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

/// Encode `s` as the wire-string shape the `read_string`
/// decoder in display-proto expects: a little-endian
/// `u32` length prefix followed by the UTF-8 bytes padded
/// to a 4-byte boundary with NULs.
fn encode_wire_string(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let pad = (4 - (bytes.len() % 4)) % 4;
    let mut out = Vec::with_capacity(4 + bytes.len() + pad);
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    out.resize(out.len() + pad, 0);
    out
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
