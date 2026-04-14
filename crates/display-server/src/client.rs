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
use alloc::vec::Vec;

use display_proto::ids::{IdAllocator, IdKind, ObjectId};
use display_proto::objects::{Interface, OpcodeError};
use display_proto::wire::MessageHeader;

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
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
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
}

/// Per-connection state.
pub struct Client {
    pub id: ClientId,
    pub objects: BTreeMap<ObjectId, Interface>,
    pub server_ids: IdAllocator,
    pub journal: Vec<HandledRequest>,
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

    /// Dispatch a single decoded request. Validates the target
    /// object exists, that the opcode is a known request on
    /// its interface, and records the event in the journal.
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
                    // Check whether this opcode is an event on
                    // the same interface — if so, the caller
                    // violated the direction contract.
                    if interface.lookup_event(opcode).is_ok() {
                        ClientError::WrongDirection { interface, opcode }
                    } else {
                        ClientError::UnknownOpcode { interface, opcode }
                    }
                }
            })?;
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

    /// Test helper: drain the dispatch journal and return it.
    pub fn drain_journal(&mut self) -> Vec<HandledRequest> {
        core::mem::take(&mut self.journal)
    }
}
