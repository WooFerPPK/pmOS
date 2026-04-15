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

use abi::cap::{Cap, CapSet};
use display_proto::decode::DecodeError;
use display_proto::events::{
    DisplayDeleteId, DisplayError, RegistryGlobal, RegistryGlobalRemove, ShellWindowCreated,
    ShellWindowDestroyed, ShellWindowFocused, ShellWindowTitleChanged,
};
use display_proto::ids::{IdAllocator, IdKind, ObjectId};
use display_proto::objects::{Interface, OpcodeError};
use display_proto::requests::{
    CompositorCreateSurface, DisplayGetRegistry, RegistryBind, ShmCreatePool,
    ShmPoolCreateBuffer, SurfaceAttach, SurfaceDamage, XdgShellGetToplevel,
    XdgToplevelSetAppId, XdgToplevelSetTitle,
};
use display_proto::wire::{MessageHeader, WireError, HEADER_SIZE};

/// Maximum bytes a single `pmd_shm.create_pool` may request.
/// Protects the server against a runaway client allocating
/// an enormous pool. 64 MiB is comfortably above anything a
/// v1 app needs (a 4K ARGB8888 framebuffer is ~33 MiB) while
/// still being small enough to reject obvious bugs.
pub const MAX_POOL_SIZE: u32 = 64 * 1024 * 1024;

/// A shm pool installed by `pmd_shm.create_pool`. In v1 the
/// server owns the pool's backing storage directly (a plain
/// `Vec<u8>`). When the kernel's display-server host is
/// wired to SharedArrayBuffer, the `storage` field will
/// become a shared-memory view; the rest of the pool's
/// metadata is unchanged.
///
/// Tests simulate a "client SAB write" by calling
/// [`Client::pool_bytes_mut`] and mutating the storage
/// directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pool {
    /// Declared pool size in bytes (matches `storage.len()`).
    pub size: u32,
    /// Pool's backing bytes. Zero-filled at `create_pool`.
    pub storage: Vec<u8>,
}

/// A buffer carved out of a pool by
/// `pmd_shm_pool.create_buffer`. Records the rectangle +
/// format so the compositor can interpret the pool bytes
/// once an `attach` + `commit` sequence promotes the buffer
/// onto a surface.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BufferInfo {
    /// Pool the buffer was carved from.
    pub pool_id: ObjectId,
    pub offset: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
}

impl BufferInfo {
    /// Exclusive upper byte offset the buffer covers within
    /// its pool: `offset + stride * height`. Uses `u64` to
    /// avoid overflow when stride or height are pathological;
    /// the caller compares against the pool's `size` as u64.
    pub fn byte_end(&self) -> u64 {
        (self.offset as u64) + (self.stride as u64) * (self.height as u64)
    }
}

/// A pending or current `surface.attach(buffer_id, x, y)`.
/// Stored in [`Surface::pending_buffer`] between `attach`
/// and `commit`, then promoted to [`Surface::current_buffer`]
/// on `commit`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BufferAttachment {
    pub buffer_id: ObjectId,
    pub x: i32,
    pub y: i32,
}

/// One damage rectangle accumulated in [`Surface::pending_damage`]
/// since the last commit. Matches the wire format of
/// `pmd_surface.damage(x, y, w, h)` 1:1.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Per-surface double-buffered state.
///
/// A surface carries two "sides":
///
/// * `pending_*` — changes the client has sent since the
///   last commit. `attach` overwrites `pending_buffer`;
///   `damage` appends to `pending_damage`.
/// * `current_*` — what the compositor should composite.
///   Promoted atomically from `pending_*` on `commit`.
///
/// `commit_count` increments on every commit (even
/// commits with nothing pending), which is the test-
/// facing way to detect that the commit handler ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Surface {
    pub id: ObjectId,
    pub pending_buffer: Option<BufferAttachment>,
    pub pending_damage: Vec<DamageRect>,
    pub current_buffer: Option<BufferAttachment>,
    pub commit_count: u32,
}

impl Surface {
    /// Build a fresh surface at `id` with no pending /
    /// current state and zero commits.
    pub fn new(id: ObjectId) -> Self {
        Surface {
            id,
            pending_buffer: None,
            pending_damage: Vec::new(),
            current_buffer: None,
            commit_count: 0,
        }
    }
}

/// A narrowed-Wayland `pmd_xdg_toplevel`: a positioned,
/// titled window wrapping a plain [`Surface`].
///
/// v1 stores only the fields the compositor and the
/// shell-manager event path actually read: the backing
/// surface id, title, app_id, and server-assigned origin.
/// Size comes from whatever buffer the client attaches to
/// the surface. A later slice will add a configure
/// handshake that lets the shell propose geometry back to
/// the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toplevel {
    pub id: ObjectId,
    pub surface_id: ObjectId,
    pub title: String,
    pub app_id: String,
    /// Server-assigned screen-space origin. Set at
    /// `get_toplevel` dispatch time via the per-client
    /// auto-layout counter.
    pub x: i32,
    pub y: i32,
}

impl Toplevel {
    pub fn new(id: ObjectId, surface_id: ObjectId, x: i32, y: i32) -> Self {
        Toplevel {
            id,
            surface_id,
            title: String::new(),
            app_id: String::new(),
            x,
            y,
        }
    }
}

/// Horizontal + vertical step for the per-client staircase
/// auto-layout used in v1. Each new toplevel lands 32
/// pixels to the right of and below the previous one.
/// Simple, deterministic, and enough to prove multi-window
/// composition works.
pub const AUTO_LAYOUT_STEP: i32 = 32;

/// Which capability, if any, must a connecting client
/// hold to bind a given interface via `pmd_registry.bind`?
/// `None` means "no restriction; any client may bind".
///
/// v1 only restricts `pmd_shell_manager` (spec §15: must
/// hold `Cap::Shell`). Other privileged interfaces land
/// here as they're added.
pub const fn interface_required_cap(interface: Interface) -> Option<Cap> {
    match interface {
        Interface::ShellManager => Some(Cap::Shell),
        Interface::Display
        | Interface::Registry
        | Interface::Compositor
        | Interface::Shm
        | Interface::ShmPool
        | Interface::Buffer
        | Interface::Surface
        | Interface::XdgShell
        | Interface::XdgToplevel => None,
    }
}

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
    /// `pmd_registry.bind` tried to bind an interface that
    /// requires a capability the connecting client doesn't
    /// hold. v1: `pmd_shell_manager` requires `Cap::Shell`.
    /// The bind is rejected and the new object is NOT
    /// installed. `new_id` is the client-side id the
    /// rejected bind would have allocated; the server
    /// emits a `pmd_display.error` event on it so the
    /// client can drop its local stale entry.
    PermissionDenied {
        interface: Interface,
        required: Cap,
        new_id: ObjectId,
    },
    /// An emit-path wire-format failure — usually a payload
    /// whose length would overflow the 16-bit `length`
    /// field. Should be unreachable in practice for v1
    /// events, but we propagate it so test assertions have
    /// something specific to match on.
    EncodeFailed(WireError),
    /// A `pmd_shm.create_pool(new_id, size)` asked for a
    /// pool larger than [`MAX_POOL_SIZE`]. The pool is
    /// rejected and neither the storage nor the object is
    /// installed.
    PoolTooLarge { requested: u32, max: u32 },
    /// A `pmd_shm_pool.create_buffer(...)` specified a
    /// region that doesn't fit inside its parent pool.
    /// `byte_end` is the exclusive upper offset the request
    /// would have used; `pool_size` is what the pool was
    /// created with.
    BufferOutOfPool {
        pool_id: ObjectId,
        pool_size: u32,
        byte_end: u64,
    },
    /// A `pmd_shm_pool.create_buffer(...)` targetted a
    /// `ShmPool` object that exists in the object table but
    /// has no corresponding entry in the per-client pool
    /// storage map. Indicates server-side state corruption —
    /// should be unreachable in practice.
    UnknownPool { pool_id: ObjectId },
    /// A `pmd_surface.attach(buffer_id, ...)` named a
    /// buffer id the client's buffer table doesn't know
    /// about (or which has been destroyed). `NULL` is NOT
    /// an error — it detaches — so this is only raised
    /// for non-null unknown ids.
    UnknownBuffer { buffer_id: ObjectId },
    /// An attach / damage / commit targetted a `Surface`
    /// object that exists in the object table but has no
    /// corresponding entry in the per-client surfaces
    /// map. Should be unreachable in practice.
    UnknownSurface { surface_id: ObjectId },
    /// `pmd_xdg_shell.get_toplevel` named a surface id
    /// that either doesn't exist in the client's table OR
    /// isn't of type `Surface`.
    ToplevelSurfaceNotFound { surface_id: ObjectId },
    /// `pmd_xdg_shell.get_toplevel` was called a second
    /// time for the same surface. v1 allows at most one
    /// toplevel per surface (the real xdg-shell rule).
    SurfaceAlreadyHasToplevel {
        surface_id: ObjectId,
        existing_toplevel: ObjectId,
    },
    /// A `set_title` / `set_app_id` / `destroy` request
    /// targetted an `XdgToplevel` object that exists in
    /// the object table but has no corresponding entry
    /// in the per-client toplevels map.
    UnknownToplevel { toplevel_id: ObjectId },
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
    /// Capability set this client connected with. Set
    /// once at accept time and never widened. Used by
    /// the bind dispatch path to gate privileged
    /// interfaces (`pmd_shell_manager` requires
    /// `Cap::Shell`).
    pub capabilities: CapSet,
    /// Backing storage for every `pmd_shm_pool` object the
    /// client has allocated. Keyed by the pool's object id.
    /// Parallel to the entries in `objects` that carry
    /// `Interface::ShmPool`.
    pub pools: BTreeMap<ObjectId, Pool>,
    /// Metadata for every `pmd_buffer` object the client
    /// has allocated, keyed by the buffer's object id.
    /// Parallel to the entries in `objects` that carry
    /// `Interface::Buffer`.
    pub buffers: BTreeMap<ObjectId, BufferInfo>,
    /// Per-surface double-buffered state. An entry is
    /// created for every `compositor.create_surface` auto-
    /// install and torn down when the surface is destroyed
    /// (follow-up slice: `surface.destroy` → `delete_id`).
    pub surfaces: BTreeMap<ObjectId, Surface>,
    /// Per-toplevel window state, keyed by the toplevel's
    /// object id. Populated by `pmd_xdg_shell.get_toplevel`
    /// auto-install and mutated by `set_title` / `set_app_id`.
    pub toplevels: BTreeMap<ObjectId, Toplevel>,
    /// Reverse index: surface id → toplevel id. Makes the
    /// compositor's "does this surface have a window?"
    /// lookup O(log N) during commit dispatch.
    pub toplevel_by_surface: BTreeMap<ObjectId, ObjectId>,
    /// Counter for the per-client staircase auto-layout.
    /// Advanced by [`AUTO_LAYOUT_STEP`] each time a new
    /// toplevel is installed.
    pub next_toplevel_offset: i32,
}

impl Client {
    /// Build a fresh client with no capabilities. Used by
    /// `Server::accept` for unprivileged connections.
    pub fn new(id: ClientId) -> Self {
        Client::new_with_caps(id, CapSet::EMPTY)
    }

    /// Build a fresh client with the given capability set.
    /// Used by `Server::accept_with_caps` when the
    /// connecting process advertises the caps it holds
    /// (typically derived from the kernel's per-pid cap
    /// table at `display_connect` time).
    pub fn new_with_caps(id: ClientId, capabilities: CapSet) -> Self {
        let mut objects = BTreeMap::new();
        objects.insert(ObjectId::DISPLAY, Interface::Display);
        Client {
            id,
            objects,
            server_ids: IdAllocator::for_server(),
            journal: Vec::new(),
            pending_events: Vec::new(),
            capabilities,
            pools: BTreeMap::new(),
            buffers: BTreeMap::new(),
            surfaces: BTreeMap::new(),
            toplevels: BTreeMap::new(),
            toplevel_by_surface: BTreeMap::new(),
            next_toplevel_offset: 0,
        }
    }

    /// True iff this client holds `cap` in its connection-
    /// time capability set.
    pub fn has_cap(&self, cap: Cap) -> bool {
        self.capabilities.contains(cap)
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
    /// * `pmd_shm.create_pool(new_id, size)` — new object
    ///   at `new_id` is [`Interface::ShmPool`]. The
    ///   accompanying fd is signalled via the header's
    ///   `fd_passing` count; v1 trusts `size` for the
    ///   logical byte length.
    /// * `pmd_shm_pool.create_buffer(new_id, ...)` — new
    ///   object at `new_id` is [`Interface::Buffer`]. Format
    ///   validation happens in the compositor, not here.
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

        if let Err(e) = self.auto_install(header.object_id, interface, header.opcode, payload) {
            // Cap-rejected binds get surfaced to the client
            // as a `pmd_display.error` event so the
            // client can drop its local stale state. The
            // emit can fail (e.g. payload-too-big), but
            // the canonical signal is still the Err we
            // bubble up to the caller — so we ignore the
            // emit's Result.
            if let ClientError::PermissionDenied {
                interface: target,
                required,
                new_id,
            } = &e
            {
                let msg = alloc::format!(
                    "permission denied: {} requires Cap::{:?}",
                    target.name(),
                    required,
                );
                let _ = self.emit_error(
                    *new_id,
                    display_proto::events::error_code::PERMISSION_DENIED,
                    &msg,
                );
            }
            return Err(e);
        }

        // Surface state transitions (attach / damage /
        // commit) don't install new objects but DO mutate
        // per-surface state on the client. Handle them
        // after auto_install so `Surface` existence
        // is already guaranteed by `create_surface`.
        self.apply_surface_state(header.object_id, interface, header.opcode, payload)?;

        // `pmd_xdg_toplevel.set_title` / `set_app_id`
        // similarly mutate per-toplevel state without
        // installing new objects.
        self.apply_toplevel_state(header.object_id, interface, header.opcode, payload)?;

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

    /// Apply a `pmd_surface.*` state transition. Called
    /// from [`Client::dispatch_request`] for the three
    /// opcodes that modify surface state without installing
    /// new objects: `attach` (2), `damage` (3), and
    /// `commit` (7). Returns an error on validation
    /// failures (unknown buffer id, unknown surface); other
    /// opcodes fall through as no-ops.
    /// Apply a `pmd_xdg_toplevel.*` state transition. Called
    /// from [`Client::dispatch_request`] for the two opcodes
    /// that mutate per-toplevel state: `set_title` (1) and
    /// `set_app_id` (2). Other opcodes fall through as
    /// no-ops.
    fn apply_toplevel_state(
        &mut self,
        toplevel_id: ObjectId,
        interface: Interface,
        opcode: u16,
        payload: &[u8],
    ) -> Result<(), ClientError> {
        if interface != Interface::XdgToplevel {
            return Ok(());
        }
        match opcode {
            1 /* set_title */ => {
                let req = XdgToplevelSetTitle::decode(payload).map_err(|e| {
                    ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    }
                })?;
                let toplevel = self
                    .toplevels
                    .get_mut(&toplevel_id)
                    .ok_or(ClientError::UnknownToplevel { toplevel_id })?;
                toplevel.title = req.title;
                Ok(())
            }
            2 /* set_app_id */ => {
                let req = XdgToplevelSetAppId::decode(payload).map_err(|e| {
                    ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    }
                })?;
                let toplevel = self
                    .toplevels
                    .get_mut(&toplevel_id)
                    .ok_or(ClientError::UnknownToplevel { toplevel_id })?;
                toplevel.app_id = req.app_id;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn apply_surface_state(
        &mut self,
        surface_id: ObjectId,
        interface: Interface,
        opcode: u16,
        payload: &[u8],
    ) -> Result<(), ClientError> {
        if interface != Interface::Surface {
            return Ok(());
        }
        match opcode {
            2 /* attach */ => {
                let req = SurfaceAttach::decode(payload).map_err(|e| {
                    ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    }
                })?;
                // Null buffer_id detaches. Non-null ids
                // must refer to an existing buffer in
                // this client's table.
                if !req.buffer_id.is_null()
                    && !self.buffers.contains_key(&req.buffer_id)
                {
                    return Err(ClientError::UnknownBuffer {
                        buffer_id: req.buffer_id,
                    });
                }
                let surface = self
                    .surfaces
                    .get_mut(&surface_id)
                    .ok_or(ClientError::UnknownSurface { surface_id })?;
                surface.pending_buffer = if req.buffer_id.is_null() {
                    None
                } else {
                    Some(BufferAttachment {
                        buffer_id: req.buffer_id,
                        x: req.x,
                        y: req.y,
                    })
                };
                Ok(())
            }
            3 /* damage */ => {
                let req = SurfaceDamage::decode(payload).map_err(|e| {
                    ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    }
                })?;
                let surface = self
                    .surfaces
                    .get_mut(&surface_id)
                    .ok_or(ClientError::UnknownSurface { surface_id })?;
                surface.pending_damage.push(DamageRect {
                    x: req.x,
                    y: req.y,
                    width: req.width,
                    height: req.height,
                });
                Ok(())
            }
            7 /* commit */ => {
                let surface = self
                    .surfaces
                    .get_mut(&surface_id)
                    .ok_or(ClientError::UnknownSurface { surface_id })?;
                // Promote pending → current atomically.
                // A commit with no pending attach keeps
                // the current buffer; explicit detach
                // happens via `attach(null)` followed by
                // commit, which reaches this branch with
                // `pending_buffer = None` and clears the
                // current buffer below.
                //
                // Wayland semantics detail: a commit with
                // no prior attach leaves the surface's
                // current buffer unchanged. We encode
                // that by only promoting when there's a
                // pending attach OR an explicit detach.
                if surface.pending_buffer.is_some() {
                    surface.current_buffer = surface.pending_buffer.take();
                }
                surface.pending_damage.clear();
                surface.commit_count = surface.commit_count.saturating_add(1);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Decode the payload of a request that creates a new
    /// object and install the new object in the client's
    /// table at the client-allocated id. The full list of
    /// opcodes that auto-install is documented on
    /// [`Client::dispatch_request`].
    fn auto_install(
        &mut self,
        target_id: ObjectId,
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
                // Capability gate: privileged interfaces
                // (v1: ShellManager → Cap::Shell) reject
                // the bind for clients that don't hold
                // the required cap. The new object is NOT
                // installed.
                if let Some(required) = interface_required_cap(target) {
                    if !self.capabilities.contains(required) {
                        return Err(ClientError::PermissionDenied {
                            interface: target,
                            required,
                            new_id: req.new_id,
                        });
                    }
                }
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
                self.surfaces.insert(req.new_id, Surface::new(req.new_id));
                Ok(())
            }
            (Interface::Shm, 1 /* create_pool */) => {
                let req = ShmCreatePool::decode(payload).map_err(|e| {
                    ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    }
                })?;
                if req.size > MAX_POOL_SIZE {
                    return Err(ClientError::PoolTooLarge {
                        requested: req.size,
                        max: MAX_POOL_SIZE,
                    });
                }
                self.install_client_object(req.new_id, Interface::ShmPool)?;
                let storage = alloc::vec![0u8; req.size as usize];
                self.pools.insert(
                    req.new_id,
                    Pool {
                        size: req.size,
                        storage,
                    },
                );
                Ok(())
            }
            (Interface::ShmPool, 1 /* create_buffer */) => {
                let req = ShmPoolCreateBuffer::decode(payload).map_err(|e| {
                    ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    }
                })?;
                let pool = self
                    .pools
                    .get(&target_id)
                    .ok_or(ClientError::UnknownPool { pool_id: target_id })?;
                let info = BufferInfo {
                    pool_id: target_id,
                    offset: req.offset,
                    width: req.width,
                    height: req.height,
                    stride: req.stride,
                    format: req.format,
                };
                let byte_end = info.byte_end();
                if byte_end > pool.size as u64 {
                    return Err(ClientError::BufferOutOfPool {
                        pool_id: target_id,
                        pool_size: pool.size,
                        byte_end,
                    });
                }
                self.install_client_object(req.new_id, Interface::Buffer)?;
                self.buffers.insert(req.new_id, info);
                Ok(())
            }
            (Interface::XdgShell, 1 /* get_toplevel */) => {
                let req = XdgShellGetToplevel::decode(payload).map_err(|e| {
                    ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    }
                })?;
                // The referenced surface must already
                // exist and be of type `Surface`.
                if self.objects.get(&req.surface_id).copied()
                    != Some(Interface::Surface)
                {
                    return Err(ClientError::ToplevelSurfaceNotFound {
                        surface_id: req.surface_id,
                    });
                }
                if let Some(existing) =
                    self.toplevel_by_surface.get(&req.surface_id).copied()
                {
                    return Err(ClientError::SurfaceAlreadyHasToplevel {
                        surface_id: req.surface_id,
                        existing_toplevel: existing,
                    });
                }
                let offset = self.next_toplevel_offset;
                self.next_toplevel_offset =
                    self.next_toplevel_offset.saturating_add(AUTO_LAYOUT_STEP);
                self.install_client_object(req.new_id, Interface::XdgToplevel)?;
                self.toplevels.insert(
                    req.new_id,
                    Toplevel::new(req.new_id, req.surface_id, offset, offset),
                );
                self.toplevel_by_surface
                    .insert(req.surface_id, req.new_id);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Borrow a pool by object id. Returns `None` if the
    /// id is not a pool in this client's table.
    pub fn pool(&self, pool_id: ObjectId) -> Option<&Pool> {
        self.pools.get(&pool_id)
    }

    /// Borrow the raw pool bytes for a given pool id.
    /// Equivalent to `self.pool(id).map(|p| p.storage.as_slice())`.
    pub fn pool_bytes(&self, pool_id: ObjectId) -> Option<&[u8]> {
        self.pools.get(&pool_id).map(|p| p.storage.as_slice())
    }

    /// Mutable access to a pool's backing storage. Exists
    /// for tests (and, transitionally, for the
    /// in-process demo compositor) to simulate a client
    /// SAB write before the real shared-memory transport
    /// lands.
    pub fn pool_bytes_mut(&mut self, pool_id: ObjectId) -> Option<&mut [u8]> {
        self.pools.get_mut(&pool_id).map(|p| p.storage.as_mut_slice())
    }

    /// Borrow a buffer's metadata by its object id.
    pub fn buffer_info(&self, buffer_id: ObjectId) -> Option<&BufferInfo> {
        self.buffers.get(&buffer_id)
    }

    /// Borrow a surface's pending/current state by its
    /// object id. Returns `None` if no surface exists at
    /// that id.
    pub fn surface(&self, surface_id: ObjectId) -> Option<&Surface> {
        self.surfaces.get(&surface_id)
    }

    /// Borrow a toplevel by object id.
    pub fn toplevel(&self, toplevel_id: ObjectId) -> Option<&Toplevel> {
        self.toplevels.get(&toplevel_id)
    }

    /// Reverse lookup: given a surface id, return the
    /// toplevel that wraps it (if any).
    pub fn toplevel_for_surface(&self, surface_id: ObjectId) -> Option<&Toplevel> {
        let toplevel_id = self.toplevel_by_surface.get(&surface_id).copied()?;
        self.toplevels.get(&toplevel_id)
    }

    /// Borrow the pool-backing bytes for a buffer —
    /// `&pool.storage[offset .. offset + stride * height]`.
    /// Returns `None` if the buffer is unknown or its
    /// parent pool has been destroyed. The returned slice
    /// includes any stride padding; the caller is expected
    /// to know the buffer's width and format.
    pub fn buffer_bytes(&self, buffer_id: ObjectId) -> Option<&[u8]> {
        let info = self.buffers.get(&buffer_id)?;
        let pool = self.pools.get(&info.pool_id)?;
        let start = info.offset as usize;
        let end = info.byte_end() as usize;
        pool.storage.get(start..end)
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
