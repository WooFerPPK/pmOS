//! Per-connection client state.
//!
//! One [`Client`] instance represents a single open connection
//! to the display server (in v1 terms, one `display_connect`
//! syscall from the userland side). It owns:
//!
//! * The client's own object table — a map from
//!   [`ObjectId`](crate::ids::ObjectId) to
//!   [`Interface`](crate::objects::Interface). The table starts
//!   with `ObjectId::DISPLAY` pre-bound to `Interface::Display`
//!   and grows as `pmd_registry.bind`, `pmd_compositor.
//!   create_surface`, etc. carve out new objects.
//! * The server-side allocator for this connection. The client
//!   has its own allocator on its side (we don't simulate it;
//!   the wire protocol carries fresh IDs from the client as
//!   `new_id` arguments).
//! * A journal of "handled requests" — not the real execution
//!   model, but the v1 skeleton's way of making request
//!   dispatch testable without pulling in the full compositor
//!   semantics. Each dispatched request lands as a
//!   [`HandledRequest`] so tests can assert on it.
//!
//! The wire-level decode → dispatch pipeline is minimal:
//!
//! 1. [`Client::dispatch_request`] takes a
//!    [`MessageHeader`](crate::wire::MessageHeader) and a
//!    payload slice.
//! 2. It looks up the target object in the table and
//!    validates the opcode against that interface's request
//!    table.
//! 3. On success it pushes a [`HandledRequest`] into the
//!    journal and returns `Ok(())`.
//! 4. On any validation failure it returns
//!    [`ClientError`]. The caller (the server) is responsible
//!    for turning this into a `pmd_display.error` event on the
//!    wire; v1 just bubbles the error out.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use display_proto::decode::DecodeError;
use display_proto::events::{
    DisplayDeleteId, DisplayError, RegistryGlobal, RegistryGlobalRemove, ShellWindowCreated,
    ShellWindowDestroyed, ShellWindowFocused, ShellWindowTitleChanged,
};
use display_proto::ids::{IdAllocator, IdKind, ObjectId};
use display_proto::objects::{Interface, OpcodeError};
use display_proto::requests::{
    CompositorCreateSurface, DisplayGetRegistry, RegistryBind,
};
use display_proto::wire::{MessageHeader, WireError, HEADER_SIZE};

/// Monotonic per-server identifier for a connected client.
/// Mirrors the kernel's `Pid` in role: stable for the lifetime
/// of the connection, reused after teardown.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(pub u32);

/// A single request that the client's dispatcher recognised
/// and handed to the downstream implementation. The v1
/// skeleton just keeps them around for tests to assert on;
/// later slices will actually carry out the operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandledRequest {
    pub object_id: ObjectId,
    pub interface: Interface,
    pub opcode: u16,
    pub opcode_name: &'static str,
    pub payload_len: usize,
    pub fd_passing: u8,
}

/// Errors surfaced by [`Client::dispatch_request`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// The target object does not exist in this client's table.
    UnknownObject { id: ObjectId },
    /// The target object exists but the opcode is not a known
    /// request on its interface.
    UnknownOpcode { interface: Interface, opcode: u16 },
    /// The opcode exists but as an event, not a request —
    /// someone sent us a server→client message on the
    /// client→server direction.
    WrongDirection { interface: Interface, opcode: u16 },
    /// `pmd_registry.bind` was asked to install an object at
    /// an ID the server side of the partition doesn't own.
    IllegalBindTarget { requested: ObjectId },
    /// An attempt to reuse an existing object ID.
    DuplicateObject { id: ObjectId },
    /// The payload of an auto-installed request did not
    /// decode cleanly. The dispatcher forwards the
    /// underlying [`DecodeError`] so callers can decide
    /// whether to emit a `pmd_display.error` event or just
    /// drop the connection.
    Malformed {
        interface: Interface,
        opcode: u16,
        error: DecodeError,
    },
    /// `pmd_registry.bind` named an interface the server
    /// does not know about. Recorded with the exact name the
    /// client sent so diagnostics and protocol-error events
    /// can quote it back.
    UnknownInterfaceName { name: String },
    /// An emit-path wire-format failure — usually a payload
    /// whose length would overflow the 16-bit `length`
    /// field. Should be unreachable in practice for v1
    /// events, but we propagate it so test assertions have
    /// something specific to match on.
    EncodeFailed(WireError),
}

impl From<WireError> for ClientError {
    fn from(e: WireError) -> Self {
        ClientError::EncodeFailed(e)
    }
}

/// Per-connection state.
pub struct Client {
    pub id: ClientId,
    pub objects: BTreeMap<ObjectId, Interface>,
    pub server_ids: IdAllocator,
    pub journal: Vec<HandledRequest>,
    /// FIFO of framed event bytes the server has
    /// enqueued for this client. Each entry is ONE
    /// complete message (header + payload). The caller
    /// drains them via [`Client::drain_pending_events`]
    /// and ships the bytes over the transport (socket,
    /// in-memory buffer, etc.). The queue is empty
    /// between drains.
    pub pending_events: Vec<Vec<u8>>,
}

impl Client {
    /// Build a fresh client with `pmd_display` pre-bound to
    /// `ObjectId::DISPLAY` and a server-side ID allocator
    /// ready to hand out even IDs.
    pub fn new(id: ClientId) -> Self {
        let mut objects = BTreeMap::new();
        objects.insert(ObjectId::DISPLAY, Interface::Display);
        Client {
            id,
            objects,
            server_ids: IdAllocator::for_server(),
            journal: Vec::new(),
            pending_events: Vec::new(),
        }
    }

    /// Number of currently-bound objects in this client's table.
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Borrow the interface type for an object, if any.
    pub fn get(&self, id: ObjectId) -> Option<Interface> {
        self.objects.get(&id).copied()
    }

    /// Install a client-allocated object at `id` with the given
    /// interface. Used by the dispatcher when a `new_id`
    /// argument arrives in a request (e.g.
    /// `pmd_compositor.create_surface(new_id)`). The caller
    /// must have already extracted the new_id from the payload.
    ///
    /// `id` must be on the client side of the partition AND
    /// not already bound.
    pub fn install_client_object(
        &mut self,
        id: ObjectId,
        interface: Interface,
    ) -> Result<(), ClientError> {
        if id.kind() != IdKind::Client {
            return Err(ClientError::IllegalBindTarget { requested: id });
        }
        if self.objects.contains_key(&id) {
            return Err(ClientError::DuplicateObject { id });
        }
        self.objects.insert(id, interface);
        Ok(())
    }

    /// Allocate a fresh server-side ID and install an object
    /// at it. Returns the chosen ID. Used when the server
    /// creates an object the client didn't ask for — e.g. the
    /// default `pmd_output` that's advertised via `registry.
    /// global` once the client binds the registry.
    pub fn install_server_object(
        &mut self,
        interface: Interface,
    ) -> Result<ObjectId, ClientError> {
        let id = self
            .server_ids
            .allocate()
            .map_err(|_| ClientError::DuplicateObject {
                id: self.server_ids.peek(),
            })?;
        self.objects.insert(id, interface);
        Ok(id)
    }

    /// Drop an object from the table. Used by `destroy`
    /// requests and by the server-side `delete_id` event flow.
    /// Returns true iff an object was actually removed.
    pub fn drop_object(&mut self, id: ObjectId) -> bool {
        self.objects.remove(&id).is_some()
    }

    /// Dispatch a single decoded request. Validates the
    /// target object exists, validates the opcode is a
    /// known request on its interface, auto-installs any
    /// `new_id` object the request's payload carries, and
    /// records the event in the journal.
    ///
    /// Auto-installed opcodes (v1):
    ///
    /// * `pmd_display.get_registry(new_id)` — new object at
    ///   `new_id` is [`Interface::Registry`].
    /// * `pmd_registry.bind(name, interface, version, new_id)`
    ///   — the interface-name string is mapped through
    ///   [`Interface::from_name`] and installed at `new_id`.
    ///   An unknown name yields [`ClientError::UnknownInterfaceName`].
    /// * `pmd_compositor.create_surface(new_id)` — new
    ///   object at `new_id` is [`Interface::Surface`].
    ///
    /// Other requests fall through to the journal without
    /// touching the object table.
    pub fn dispatch_request(
        &mut self,
        header: MessageHeader,
        payload: &[u8],
    ) -> Result<(), ClientError> {
        let interface = self
            .objects
            .get(&header.object_id)
            .copied()
            .ok_or(ClientError::UnknownObject { id: header.object_id })?;
        let opcode = interface
            .lookup_request(header.opcode)
            .map_err(|e| match e {
                OpcodeError::UnknownOpcode { interface, opcode } => {
                    if interface.lookup_event(opcode).is_ok() {
                        ClientError::WrongDirection { interface, opcode }
                    } else {
                        ClientError::UnknownOpcode { interface, opcode }
                    }
                }
            })?;

        self.auto_install(interface, header.opcode, payload)?;

        self.journal.push(HandledRequest {
            object_id: header.object_id,
            interface,
            opcode: header.opcode,
            opcode_name: opcode.name,
            payload_len: payload.len(),
            fd_passing: header.fd_passing,
        });
        Ok(())
    }

    /// Decode the payload of a request that creates a new
    /// object and install the new object in the client's
    /// table at the client-allocated id. The full list of
    /// opcodes that auto-install is documented on
    /// [`Client::dispatch_request`].
    fn auto_install(
        &mut self,
        interface: Interface,
        opcode: u16,
        payload: &[u8],
    ) -> Result<(), ClientError> {
        match (interface, opcode) {
            (Interface::Display, 2 /* get_registry */) => {
                let req = DisplayGetRegistry::decode(payload).map_err(|e| {
                    ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    }
                })?;
                self.install_client_object(req.new_id, Interface::Registry)?;
                Ok(())
            }
            (Interface::Registry, 1 /* bind */) => {
                let req = RegistryBind::decode(payload).map_err(|e| {
                    ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    }
                })?;
                let target = Interface::from_name(req.interface).ok_or_else(|| {
                    ClientError::UnknownInterfaceName {
                        name: req.interface.into(),
                    }
                })?;
                self.install_client_object(req.new_id, target)?;
                Ok(())
            }
            (Interface::Compositor, 1 /* create_surface */) => {
                let req = CompositorCreateSurface::decode(payload).map_err(|e| {
                    ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    }
                })?;
                self.install_client_object(req.new_id, Interface::Surface)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Test helper: drain the dispatch journal and return it.
    pub fn drain_journal(&mut self) -> Vec<HandledRequest> {
        core::mem::take(&mut self.journal)
    }

    // ---- Event emit path ---------------------------------------
    //
    // Server code calls these to ship events to this client.
    // Each helper builds a framed message (header + payload),
    // pushes it onto `pending_events`, and trusts the caller
    // to `drain_pending_events()` periodically and feed the
    // bytes into the transport.

    /// Low-level emit: validate that `object_id` is in the
    /// client's table, that `opcode` is a known EVENT on
    /// that interface, and enqueue a framed message
    /// carrying `payload`. Returns the byte length of the
    /// emitted message on success.
    ///
    /// Callers typically use one of the typed helpers
    /// below ([`Client::emit_global`],
    /// [`Client::emit_error`], etc.) which build the
    /// payload for them; this method is the building block
    /// for any v1 event the typed helpers don't cover yet.
    pub fn emit_raw(
        &mut self,
        object_id: ObjectId,
        opcode: u16,
        payload: &[u8],
    ) -> Result<usize, ClientError> {
        let interface = self
            .objects
            .get(&object_id)
            .copied()
            .ok_or(ClientError::UnknownObject { id: object_id })?;
        interface
            .lookup_event(opcode)
            .map_err(|e| match e {
                OpcodeError::UnknownOpcode { interface, opcode } => {
                    if interface.lookup_request(opcode).is_ok() {
                        ClientError::WrongDirection { interface, opcode }
                    } else {
                        ClientError::UnknownOpcode { interface, opcode }
                    }
                }
            })?;
        let header = MessageHeader::try_new(object_id, opcode, payload.len(), 0)?;
        let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());
        buf.resize(HEADER_SIZE, 0);
        header.encode(&mut buf[..HEADER_SIZE])?;
        buf.extend_from_slice(payload);
        let total = buf.len();
        self.pending_events.push(buf);
        Ok(total)
    }

    /// Emit `pmd_display.error(object_id, code, message)` —
    /// the protocol's generic "this went wrong" event.
    pub fn emit_error(
        &mut self,
        target: ObjectId,
        code: u32,
        message: &str,
    ) -> Result<usize, ClientError> {
        let event = DisplayError {
            object_id: target,
            code,
            message: message.to_string(),
        };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(ObjectId::DISPLAY, 1 /* error */, &payload)
    }

    /// Emit `pmd_display.delete_id(id)` — acknowledge to
    /// the client that an object it destroyed is fully
    /// torn down on the server.
    pub fn emit_delete_id(&mut self, id: ObjectId) -> Result<usize, ClientError> {
        let event = DisplayDeleteId { id };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(ObjectId::DISPLAY, 2 /* delete_id */, &payload)
    }

    /// Emit `pmd_registry.global(name, interface, version)`
    /// against `registry_id`. The caller is responsible
    /// for having installed a registry object at that id
    /// first (typically via auto-install in
    /// `dispatch_request` for `display.get_registry`).
    pub fn emit_global(
        &mut self,
        registry_id: ObjectId,
        name: u32,
        interface_name: &str,
        version: u32,
    ) -> Result<usize, ClientError> {
        let event = RegistryGlobal {
            name,
            interface: interface_name.to_string(),
            version,
        };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(registry_id, 1 /* global */, &payload)
    }

    /// Emit `pmd_registry.global_remove(name)` against
    /// `registry_id`.
    pub fn emit_global_remove(
        &mut self,
        registry_id: ObjectId,
        name: u32,
    ) -> Result<usize, ClientError> {
        let event = RegistryGlobalRemove { name };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(registry_id, 2 /* global_remove */, &payload)
    }

    /// Emit `pmd_shell_manager.window_created(window_id,
    /// title, app_id)` — spec §15. The caller has already
    /// installed a shell-manager object at
    /// `shell_manager_id` (typically via the `registry.bind`
    /// auto-install path).
    pub fn emit_window_created(
        &mut self,
        shell_manager_id: ObjectId,
        window_id: u32,
        title: &str,
        app_id: &str,
    ) -> Result<usize, ClientError> {
        let event = ShellWindowCreated {
            window_id,
            title: title.to_string(),
            app_id: app_id.to_string(),
        };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(shell_manager_id, 1 /* window_created */, &payload)
    }

    /// Emit `pmd_shell_manager.window_destroyed(window_id)` —
    /// spec §15.
    pub fn emit_window_destroyed(
        &mut self,
        shell_manager_id: ObjectId,
        window_id: u32,
    ) -> Result<usize, ClientError> {
        let event = ShellWindowDestroyed { window_id };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(shell_manager_id, 2 /* window_destroyed */, &payload)
    }

    /// Emit `pmd_shell_manager.window_focused(window_id)` —
    /// spec §15.
    pub fn emit_window_focused(
        &mut self,
        shell_manager_id: ObjectId,
        window_id: u32,
    ) -> Result<usize, ClientError> {
        let event = ShellWindowFocused { window_id };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(shell_manager_id, 3 /* window_focused */, &payload)
    }

    /// Emit `pmd_shell_manager.window_title_changed(
    /// window_id, new_title)` — spec §15.
    pub fn emit_window_title_changed(
        &mut self,
        shell_manager_id: ObjectId,
        window_id: u32,
        new_title: &str,
    ) -> Result<usize, ClientError> {
        let event = ShellWindowTitleChanged {
            window_id,
            new_title: new_title.to_string(),
        };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(shell_manager_id, 4 /* window_title_changed */, &payload)
    }

    /// Drain the pending-events queue and return one
    /// flattened `Vec<u8>` containing every enqueued
    /// message back-to-back. The client's queue is empty
    /// after this call.
    ///
    /// Returns an empty `Vec<u8>` if no events are
    /// queued.
    pub fn drain_pending_events(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for msg in self.pending_events.drain(..) {
            out.extend_from_slice(&msg);
        }
        out
    }

    /// Number of events currently queued, without
    /// draining. Diagnostic.
    pub fn pending_events_len(&self) -> usize {
        self.pending_events.len()
    }
}
