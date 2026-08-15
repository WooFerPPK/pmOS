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
//! * A bounded diagnostics journal of handled-request metadata.
//!   It retains no request payload bytes and evicts its oldest
//!   entry at [`MAX_JOURNAL_ENTRIES`], keeping request dispatch
//!   observable in isolation tests without unbounded production
//!   memory growth.
//!
//! The wire-level decode → dispatch pipeline is minimal:
//!
//! 1. [`Client::dispatch_request`] takes a
//!    [`MessageHeader`](crate::wire::MessageHeader) and a
//!    payload slice.
//! 2. It looks up the target object in the table and
//!    validates the opcode against that interface's request
//!    table.
//! 3. On success it records bounded [`HandledRequest`] metadata
//!    and returns `Ok(())`.
//! 4. On any validation failure it returns
//!    [`ClientError`]. The caller (the server) is responsible
//!    for turning this into a `pmd_display.error` event on the
//!    wire; v1 just bubbles the error out.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use abi::cap::{Cap, CapSet};
use display_proto::decode::DecodeError;
use display_proto::events::{
    CallbackDone, DisplayDeleteId, DisplayError, RegistryGlobal, RegistryGlobalRemove,
    ShellRestoreFinished, ShellWindowCreated, ShellWindowDestroyed, ShellWindowFocused,
    ShellWindowSnapshotDone, ShellWindowState, ShellWindowTitleChanged,
};
use display_proto::events::{KeyboardKey, PointerButton, PointerMotion};
use display_proto::ids::{IdAllocator, IdKind, ObjectId};
use display_proto::objects::{Interface, OpcodeError};
use display_proto::requests::{
    buffer_format, CompositorCreateSurface, DisplayGetRegistry, DisplaySync, RegistryBind,
    SeatGetKeyboard, SeatGetPointer, ShellManagerBeginRestore, ShellManagerEndRestore,
    ShellManagerPlaceRestoredWindow, ShellManagerSubscribeWindowState, ShmCreatePool,
    ShmPoolCreateBuffer, ShmPoolResize, ShmPoolWriteRows, SurfaceAttach, SurfaceDamage,
    SurfaceFrame, SurfacePatchCurrent, XdgShellGetToplevel, XdgToplevelSetAppId,
    XdgToplevelSetTitle, MAX_SURFACE_PATCH_BYTES,
};
use display_proto::wire::{MessageHeader, WireError, HEADER_SIZE};

/// Maximum bytes a single `pmd_shm.create_pool` may request.
/// Protects the server against a runaway client allocating
/// an enormous pool. 64 MiB is more than ten times the normal
/// 1024x736 double-buffered v1 pool while still being small enough
/// to reject obvious bugs.
pub const MAX_POOL_SIZE: u32 = 64 * 1024 * 1024;

/// Per-connection resource ceilings. The default desktop uses roughly a dozen
/// objects and one 1024x736 double-buffered pool (~6 MiB); these limits leave
/// substantial headroom while making every client-owned collection finite.
pub const MAX_CLIENT_OBJECTS: usize = 512;
pub const MAX_CLIENT_POOLS: usize = 64;
pub const MAX_CLIENT_POOL_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_CLIENT_BUFFERS: usize = 256;
pub const MAX_CLIENT_SURFACES: usize = 64;
pub const MAX_CLIENT_TOPLEVELS: usize = 64;
/// Maximum combined UTF-8 title + app-id bytes retained by one toplevel. This
/// keeps every shell lifecycle event wire-representable and, together with the
/// production request and shell-write quanta, prevents ordinary title churn
/// from producing events faster than a responsive shell can drain them.
pub const MAX_TOPLEVEL_METADATA_BYTES: u64 = 880;
/// Aggregate UTF-8 bytes retained in all live toplevel titles and app ids for
/// one connection. Shipped apps use tens of bytes per window; 64 KiB leaves
/// ample headroom while bounding repeated replacement and multi-window abuse.
pub const MAX_CLIENT_TOPLEVEL_METADATA_BYTES: u64 = 64 * 1024;
pub const MAX_SURFACE_DAMAGE_RECTS: usize = 64;
pub const MAX_PENDING_EVENTS: usize = 1024;
pub const MAX_PENDING_EVENT_BYTES: usize = 256 * 1024;
const ERROR_CODE_NO_MEMORY: u32 = 4;
const FRAME_CALLBACK_COMPLETION_EVENTS: usize = 2;
const FRAME_CALLBACK_COMPLETION_BYTES: usize = 2 * (HEADER_SIZE + 4);
const FRAME_CALLBACK_CANCELLATION_EVENTS: usize = 1;
const FRAME_CALLBACK_CANCELLATION_BYTES: usize = HEADER_SIZE + 4;

/// Resource category reported when a request is rejected by admission control.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClientResource {
    Objects,
    Pools,
    PoolBytes,
    ServerPoolBytes,
    Buffers,
    Surfaces,
    Toplevels,
    ServerToplevels,
    ToplevelMetadataBytesPerWindow,
    ToplevelMetadataBytes,
    ServerToplevelMetadataBytes,
    PendingEvents,
    PendingEventBytes,
}

/// Limits applied to one display-protocol connection.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClientLimits {
    pub objects: usize,
    pub pools: usize,
    pub pool_bytes: u64,
    pub buffers: usize,
    pub surfaces: usize,
    pub toplevels: usize,
    pub toplevel_metadata_bytes: u64,
    pub damage_rects_per_surface: usize,
    pub pending_events: usize,
    pub pending_event_bytes: usize,
}

impl Default for ClientLimits {
    fn default() -> Self {
        Self {
            objects: MAX_CLIENT_OBJECTS,
            pools: MAX_CLIENT_POOLS,
            pool_bytes: MAX_CLIENT_POOL_BYTES,
            buffers: MAX_CLIENT_BUFFERS,
            surfaces: MAX_CLIENT_SURFACES,
            toplevels: MAX_CLIENT_TOPLEVELS,
            toplevel_metadata_bytes: MAX_CLIENT_TOPLEVEL_METADATA_BYTES,
            damage_rects_per_surface: MAX_SURFACE_DAMAGE_RECTS,
            pending_events: MAX_PENDING_EVENTS,
            pending_event_bytes: MAX_PENDING_EVENT_BYTES,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServerResourceBudgets {
    pub pool_bytes: u64,
    pub pool_byte_limit: u64,
    pub toplevels: usize,
    pub toplevel_limit: usize,
    pub toplevel_metadata_bytes: u64,
    pub toplevel_metadata_byte_limit: u64,
}

/// Maximum number of handled-request metadata records retained
/// per client for diagnostics and isolation tests. The journal
/// never stores request payload bytes, and evicts the oldest
/// record before inserting beyond this bound.
pub const MAX_JOURNAL_ENTRIES: usize = 256;

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
    /// Distinguishes "no attach since the last commit" from an
    /// explicit `attach(NULL)`. Both have `pending_buffer == None`,
    /// but only the latter must detach the current buffer on commit.
    pub pending_attach: bool,
    pub pending_damage: Vec<DamageRect>,
    pub current_buffer: Option<BufferAttachment>,
    pub commit_count: u32,
    /// One-shot callbacks requested since the last commit. A frame request
    /// binds the callback but does not dirty the scene; the next successful
    /// commit moves this FIFO to the client's presentation wait queue.
    pub pending_frame_callbacks: Vec<ObjectId>,
    /// Once an xdg toplevel role has been assigned, destroying that
    /// role unmaps the surface. It must not fall back into the
    /// roleless-surface composition pass and leave the old pixels
    /// visible.
    pub had_toplevel_role: bool,
}

impl Surface {
    /// Build a fresh surface at `id` with no pending /
    /// current state and zero commits.
    pub fn new(id: ObjectId) -> Self {
        Surface {
            id,
            pending_buffer: None,
            pending_attach: false,
            pending_damage: Vec::new(),
            current_buffer: None,
            commit_count: 0,
            pending_frame_callbacks: Vec::new(),
            had_toplevel_role: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReadyFrameBatch {
    callback_data: Option<u32>,
    callbacks: VecDeque<(ObjectId, ObjectId)>,
    surface_delete_id: Option<ObjectId>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum FrameCallbackCompletion {
    Empty,
    Blocked,
    Completed,
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
    /// Stable non-zero creation ordinal for the authenticated owner PID. The
    /// top-level Server overwrites the fixture-local value from its per-PID
    /// allocator so multiple display connections cannot collide. Session
    /// restore never trusts an app-supplied protocol token.
    pub ordinal: u32,
    /// Server-assigned screen-space origin. Set at
    /// `get_toplevel` dispatch time via the per-client
    /// auto-layout counter.
    pub x: i32,
    pub y: i32,
    /// Last non-maximized geometry. Width and height become authoritative once
    /// a normal-state buffer is committed; origin tracks stable move releases.
    pub normal_x: i32,
    pub normal_y: i32,
    pub normal_width: u32,
    pub normal_height: u32,
    /// `true` after the shell has called
    /// `pmd_shell_manager.minimize_window(global_window_id)` (T131).
    /// While minimized, the compositor must skip this
    /// toplevel's surface in its blit pass — the surface is
    /// "unmapped" but NOT destroyed; a subsequent restore
    /// (today: re-emit a configure / focus event from the
    /// shell) clears the flag and the surface re-enters the
    /// blit pass with its existing state intact.
    pub minimized: bool,
    /// `true` after the server has emitted an initial
    /// `xdg_toplevel.configure` event on this toplevel.
    /// Used by `Server::composite_surface_commit` to fire
    /// the initial configure exactly once after the first
    /// commit — Wayland clients block on receiving a
    /// configure before painting their first real frame.
    pub initial_configure_sent: bool,
    /// Set once the toplevel first has a committed current
    /// buffer. The display server uses the transition to give a
    /// newly mapped application conventional initial focus exactly
    /// once; detach/reattach does not steal focus again.
    pub mapped_once: bool,
    /// Most-recent maximize state — set by
    /// `pmd_xdg_toplevel.set_maximized()` and cleared by
    /// `unset_maximized()` (T132). The server answers the
    /// transition with a work-area-sized configure and the
    /// corresponding MAXIMIZED state bit.
    pub maximized: bool,
    /// Server-assigned origin to restore after leaving the maximized state.
    /// Maximizing moves the toplevel to the work-area origin so the configure
    /// dimensions cannot extend through reserved shell chrome.
    pub restore_origin: Option<(i32, i32)>,
    /// A restore transaction may allow a window to configure and commit while
    /// withholding it from composition, hit-testing, and automatic focus.
    pub hidden_for_restore: bool,
}

impl Toplevel {
    pub fn new(id: ObjectId, surface_id: ObjectId, x: i32, y: i32) -> Self {
        Toplevel {
            id,
            surface_id,
            title: String::new(),
            app_id: String::new(),
            ordinal: 0,
            x,
            y,
            normal_x: x,
            normal_y: y,
            normal_width: 0,
            normal_height: 0,
            minimized: false,
            maximized: false,
            restore_origin: None,
            initial_configure_sent: false,
            mapped_once: false,
            hidden_for_restore: false,
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
        | Interface::XdgToplevel
        | Interface::Seat
        | Interface::Pointer
        | Interface::Keyboard
        | Interface::Callback => None,
    }
}

/// Monotonic per-server identifier for a connected client.
/// Mirrors the kernel's `Pid` in role: stable for the lifetime
/// of the connection, reused after teardown.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(pub u32);

/// Metadata for one request that the client dispatcher recognised
/// and handed to the downstream implementation. Records are kept
/// only in the bounded diagnostics journal; protocol state changes
/// are applied independently of this record.
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
    /// A request or event would exceed a per-connection resource ceiling. The
    /// attempted value is the post-operation count or byte total. No state or
    /// backing storage is installed on this error path.
    ResourceLimitExceeded {
        resource: ClientResource,
        attempted: u64,
        limit: u64,
    },
    /// The event queue previously crossed a hard ceiling and this client is
    /// awaiting connection teardown. No further events are accepted.
    EventQueueOverflowed,
    /// A `pmd_shm.create_pool(new_id, size)` asked for a
    /// pool larger than [`MAX_POOL_SIZE`]. The pool is
    /// rejected and neither the storage nor the object is
    /// installed.
    PoolTooLarge { requested: u32, max: u32 },
    /// The allocator could not reserve backing storage after all logical
    /// resource ceilings had admitted the request. No object or pool entry is
    /// installed on this path.
    PoolAllocationFailed { requested: u32 },
    /// Shrinking a pool would invalidate a buffer that still retains its
    /// backing. The request is rejected atomically.
    PoolResizeWouldTruncateBuffer {
        pool_id: ObjectId,
        requested: u32,
        required: u64,
    },
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
    /// `pmd_shm_pool.write_rows` carried zero-sized or overlapping row
    /// geometry, or arithmetic that could not describe one bounded region.
    /// The backing pool is unchanged on this error path.
    InvalidPoolWriteRows {
        row_bytes: u32,
        rows: u32,
        stride: u32,
    },
    /// An inline pool write would mutate bytes retained by a current surface
    /// without that surface's commit transaction.
    PoolWriteIntersectsCurrentBuffer {
        pool_id: ObjectId,
        surface_id: ObjectId,
        buffer_id: ObjectId,
    },
    /// A current attachment lacks complete retained metadata, so a pool write
    /// cannot prove that its destination is disjoint from visible content.
    PoolWriteInvalidCurrentBacking {
        surface_id: ObjectId,
        buffer_id: ObjectId,
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
    /// `pmd_surface.patch_current` requires an already committed buffer.
    SurfacePatchNoCurrentBuffer { surface_id: ObjectId },
    /// Direct current-buffer patching cannot leapfrog an earlier staged
    /// attach or damage transaction on the same surface.
    SurfacePatchHasPendingState { surface_id: ObjectId },
    /// The patch rectangle was empty, negative, or outside the current
    /// buffer's pixel dimensions.
    SurfacePatchInvalidGeometry {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        buffer_width: u32,
        buffer_height: u32,
    },
    /// One inline patch exceeded the fixed protocol work quantum.
    SurfacePatchTooLarge { bytes: u64, max: usize },
    /// The current buffer uses a format `patch_current` cannot copy.
    SurfacePatchUnsupportedFormat { buffer_id: ObjectId, format: u32 },
    /// Current buffer metadata or retained pool backing cannot describe every
    /// requested destination row safely.
    SurfacePatchInvalidBacking {
        buffer_id: ObjectId,
        pool_id: ObjectId,
    },
    /// The destination bytes alias another visible surface. Mutating the pool
    /// would otherwise bypass that surface's own commit boundary.
    SurfacePatchAliasedCurrentBuffer {
        surface_id: ObjectId,
        other_surface_id: ObjectId,
    },
    /// Another surface has a current attachment whose retained metadata is
    /// incomplete, so non-aliasing cannot be proved safely.
    SurfacePatchInvalidAliasBacking {
        other_surface_id: ObjectId,
        buffer_id: ObjectId,
    },
    /// A surface commit would make two current attachments read overlapping
    /// retained pool bytes, defeating independent commit/release boundaries.
    SurfaceCommitAliasedBuffer {
        surface_id: ObjectId,
        buffer_id: ObjectId,
        conflicting_surface_id: ObjectId,
        conflicting_buffer_id: ObjectId,
    },
    /// A pending attachment or an existing current attachment lacks the
    /// retained metadata needed to prove exclusive current backing.
    SurfaceCommitInvalidBacking {
        surface_id: ObjectId,
        buffer_id: ObjectId,
    },
    /// A surface cannot be destroyed while its xdg-toplevel role is live.
    /// Destroy the role first so the server-global window registry can retire
    /// it coherently.
    SurfaceHasToplevel {
        surface_id: ObjectId,
        toplevel_id: ObjectId,
    },
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
    /// `pmd_seat.get_pointer` was called when this client
    /// already has a bound pointer object.
    PointerAlreadyBound { existing: ObjectId },
    /// `pmd_seat.get_keyboard` was called when this
    /// client already has a bound keyboard object.
    KeyboardAlreadyBound { existing: ObjectId },
}

impl From<WireError> for ClientError {
    fn from(e: WireError) -> Self {
        ClientError::EncodeFailed(e)
    }
}

/// Per-connection state.
pub struct Client {
    pub id: ClientId,
    /// Immutable process identity returned by the kernel for the peer end of
    /// this accepted socket. Zero exists only for native fixtures that do not
    /// model kernel credentials; production accepts fail closed before here.
    pub peer_pid: u32,
    pub objects: BTreeMap<ObjectId, Interface>,
    /// IDs whose protocol objects were destroyed but whose backing is still
    /// retained by another live resource. They cannot be reused until final
    /// reclamation emits `pmd_display.delete_id`.
    retired_object_ids: BTreeSet<ObjectId>,
    pub server_ids: IdAllocator,
    journal: VecDeque<HandledRequest>,
    /// FIFO of framed event bytes the server has
    /// enqueued for this client. Each entry is ONE
    /// complete message (header + payload). The caller
    /// drains them via [`Client::drain_pending_events`]
    /// and ships the bytes over the transport (socket,
    /// in-memory buffer, etc.). The queue is empty
    /// between drains.
    pending_events: Vec<Vec<u8>>,
    pending_event_bytes: usize,
    event_queue_overflowed: bool,
    limits: ClientLimits,
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
    pool_bytes: u64,
    /// Metadata for every `pmd_buffer` object the client
    /// has allocated, keyed by the buffer's object id.
    /// Parallel to the entries in `objects` that carry
    /// `Interface::Buffer`.
    pub buffers: BTreeMap<ObjectId, BufferInfo>,
    /// Per-surface double-buffered state. An entry is
    /// created for every `compositor.create_surface` auto-
    /// install and torn down when the surface is destroyed.
    /// Its id remains charged as a tombstone until the bounded
    /// lifecycle queue admits the trailing `display.delete_id`.
    pub surfaces: BTreeMap<ObjectId, Surface>,
    /// Commit-ordered callbacks that have not yet crossed a successful
    /// framebuffer presentation boundary. Entries carry their owning surface
    /// so surface destruction can cancel them without disturbing other work.
    awaiting_present_callbacks: VecDeque<(ObjectId, ObjectId)>,
    /// Presentation-qualified callbacks, grouped by the monotonic timestamp
    /// of the boundary that made them eligible. Batches let production promote
    /// all clients in O(clients) and emit a bounded callback quantum per turn.
    ready_frame_callbacks: VecDeque<ReadyFrameBatch>,
    /// Per-toplevel window state, keyed by the toplevel's
    /// object id. Populated by `pmd_xdg_shell.get_toplevel`
    /// auto-install and mutated by `set_title` / `set_app_id`.
    pub toplevels: BTreeMap<ObjectId, Toplevel>,
    /// Exact UTF-8 bytes retained by every live title and app id.
    toplevel_metadata_bytes: u64,
    /// Reverse index: surface id → toplevel id. Makes the
    /// compositor's "does this surface have a window?"
    /// lookup O(log N) during commit dispatch.
    pub toplevel_by_surface: BTreeMap<ObjectId, ObjectId>,
    /// Counter for the per-client staircase auto-layout.
    /// Advanced by [`AUTO_LAYOUT_STEP`] each time a new
    /// toplevel is installed.
    pub next_toplevel_offset: i32,
    /// Next non-zero fixture-local toplevel ordinal. Production Server state
    /// replaces it from the server-wide per-authenticated-PID allocator.
    pub next_toplevel_ordinal: u32,
    /// Object id of this client's `pmd_pointer`, populated
    /// by `pmd_seat.get_pointer` auto-install. `None` if
    /// the client hasn't asked for one.
    pub pointer_id: Option<ObjectId>,
    /// Object id of this client's `pmd_keyboard`, same
    /// semantics as [`Client::pointer_id`].
    pub keyboard_id: Option<ObjectId>,
    /// Monotonically increasing counter used for the
    /// `xdg_toplevel.configure` event's `serial` field. The
    /// client must echo each value back through
    /// `ack_configure(serial)`. Starts at 1; 0 is reserved
    /// as a sentinel "no configure has been sent yet".
    pub next_configure_serial: u32,
    /// Object id of this client's bound `pmd_shell_manager`,
    /// if any. Set by the registry.bind auto-install path
    /// when the target interface is `Interface::ShellManager`.
    /// The display server's broadcast machinery walks every
    /// client whose `shell_manager_id` is `Some` AND whose
    /// `shell_manager_subscribed` is true, emitting
    /// window_* events on the shell_manager object.
    pub shell_manager_id: Option<ObjectId>,
    /// True iff this client has sent
    /// `pmd_shell_manager.subscribe_windows`. Window-list
    /// events are only broadcast to subscribed clients.
    pub shell_manager_subscribed: bool,
    /// True after the v2 `subscribe_window_state` request. Legacy opcode-1
    /// subscribers retain the original event layouts indefinitely.
    pub shell_manager_state_snapshot_id: Option<u32>,
}

impl Client {
    /// Build a fresh client with no capabilities. Used by
    /// `Server::accept` for unprivileged connections.
    pub fn new(id: ClientId) -> Self {
        Client::new_with_caps(id, CapSet::EMPTY)
    }

    /// Build a fresh client with the given capability set.
    /// Used by `Server::accept_with_caps` when the
    /// connecting process's caps have been authenticated by
    /// the kernel's fd-scoped peer-credential query. Protocol
    /// claims must never be passed here as authority.
    pub fn new_with_caps(id: ClientId, capabilities: CapSet) -> Self {
        Client::new_with_credentials_and_limits(id, capabilities, 0, ClientLimits::default())
    }

    /// Build a client from kernel-authenticated connection credentials.
    pub fn new_with_credentials(id: ClientId, capabilities: CapSet, peer_pid: u32) -> Self {
        Client::new_with_credentials_and_limits(id, capabilities, peer_pid, ClientLimits::default())
    }

    /// Build a client with explicit limits. Production uses
    /// [`Client::new_with_caps`]; this constructor allows small deterministic
    /// ceilings in adversarial isolation tests.
    pub fn new_with_caps_and_limits(
        id: ClientId,
        capabilities: CapSet,
        limits: ClientLimits,
    ) -> Self {
        Client::new_with_credentials_and_limits(id, capabilities, 0, limits)
    }

    pub fn new_with_credentials_and_limits(
        id: ClientId,
        capabilities: CapSet,
        peer_pid: u32,
        mut limits: ClientLimits,
    ) -> Self {
        // Object 1 (pmd_display) is implicit and always present.
        limits.objects = limits.objects.max(1);
        let mut objects = BTreeMap::new();
        objects.insert(ObjectId::DISPLAY, Interface::Display);
        Client {
            id,
            peer_pid,
            objects,
            retired_object_ids: BTreeSet::new(),
            server_ids: IdAllocator::for_server(),
            journal: VecDeque::new(),
            pending_events: Vec::new(),
            pending_event_bytes: 0,
            event_queue_overflowed: false,
            limits,
            capabilities,
            pools: BTreeMap::new(),
            pool_bytes: 0,
            buffers: BTreeMap::new(),
            surfaces: BTreeMap::new(),
            awaiting_present_callbacks: VecDeque::new(),
            ready_frame_callbacks: VecDeque::new(),
            toplevels: BTreeMap::new(),
            toplevel_metadata_bytes: 0,
            toplevel_by_surface: BTreeMap::new(),
            next_toplevel_offset: 0,
            next_toplevel_ordinal: 1,
            pointer_id: None,
            keyboard_id: None,
            next_configure_serial: 1,
            shell_manager_id: None,
            shell_manager_subscribed: false,
            shell_manager_state_snapshot_id: None,
        }
    }

    /// True iff this client holds `cap` in its connection-
    /// time capability set.
    pub fn has_cap(&self, cap: Cap) -> bool {
        self.capabilities.contains(cap)
    }

    /// Number of currently-bound objects in this client's table.
    pub fn object_count(&self) -> usize {
        self.objects
            .len()
            .saturating_add(self.retired_object_ids.len())
    }

    /// Aggregate retained UTF-8 bytes across live titles and app ids.
    pub fn toplevel_metadata_bytes_len(&self) -> u64 {
        self.toplevel_metadata_bytes
    }

    /// Release metadata belonging to a server-retired toplevel.
    pub(crate) fn release_toplevel_metadata(&mut self, toplevel: &Toplevel) {
        self.toplevel_metadata_bytes = self
            .toplevel_metadata_bytes
            .saturating_sub((toplevel.title.len() + toplevel.app_id.len()) as u64);
    }

    /// Borrow the interface type for an object, if any.
    pub fn get(&self, id: ObjectId) -> Option<Interface> {
        self.objects.get(&id).copied()
    }

    /// Callbacks bound by `surface.frame` but not yet attached to a commit.
    pub fn pending_frame_callback_count(&self) -> usize {
        self.surfaces
            .values()
            .map(|surface| surface.pending_frame_callbacks.len())
            .sum()
    }

    /// Callbacks attached to committed surface state but not yet qualified by
    /// a successful framebuffer presentation.
    pub fn awaiting_present_frame_callback_count(&self) -> usize {
        self.awaiting_present_callbacks.len()
    }

    /// Presentation-qualified callbacks still waiting for bounded event-queue
    /// admission.
    pub fn presented_frame_callback_count(&self) -> usize {
        self.ready_frame_callbacks
            .iter()
            .filter(|batch| batch.callback_data.is_some())
            .map(|batch| batch.callbacks.len())
            .sum()
    }

    /// Destroy-cancelled callbacks plus trailing surface acknowledgements that
    /// are waiting for bounded event-queue admission.
    pub fn cancelled_frame_callback_lifecycle_count(&self) -> usize {
        self.ready_frame_callbacks
            .iter()
            .filter(|batch| batch.callback_data.is_none())
            .map(|batch| batch.callbacks.len() + usize::from(batch.surface_delete_id.is_some()))
            .sum()
    }

    /// Promote every commit-qualified callback at one successful presentation
    /// boundary without walking the callbacks themselves. The complete FIFO is
    /// moved into one timestamped batch in O(1).
    pub(crate) fn mark_frame_callbacks_presented(&mut self, callback_data: u32) {
        if self.awaiting_present_callbacks.is_empty() {
            return;
        }
        self.ready_frame_callbacks.push_back(ReadyFrameBatch {
            callback_data: Some(callback_data),
            callbacks: core::mem::take(&mut self.awaiting_present_callbacks),
            surface_delete_id: None,
        });
    }

    fn queue_surface_frame_callback_cancellation(
        &mut self,
        surface_id: ObjectId,
        callbacks: Vec<ObjectId>,
    ) {
        self.ready_frame_callbacks.push_back(ReadyFrameBatch {
            callback_data: None,
            callbacks: callbacks
                .into_iter()
                .map(|callback_id| (surface_id, callback_id))
                .collect(),
            surface_delete_id: Some(surface_id),
        });
    }

    pub(crate) fn has_ready_frame_callback_lifecycle(&self) -> bool {
        self.ready_frame_callbacks
            .iter()
            .any(|batch| !batch.callbacks.is_empty() || batch.surface_delete_id.is_some())
    }

    fn can_enqueue_frame_lifecycle(&self, events: usize, bytes: usize) -> bool {
        !self.event_queue_overflowed
            && self.pending_events.len().saturating_add(events) <= self.limits.pending_events
            && self.pending_event_bytes.saturating_add(bytes) <= self.limits.pending_event_bytes
    }

    /// True only when the oldest callback lifecycle item fits as one complete
    /// ordered unit. A blocked item is retried after existing events drain.
    pub(crate) fn ready_frame_callback_lifecycle_can_progress(&self) -> bool {
        let Some(batch) = self.ready_frame_callbacks.front() else {
            return false;
        };
        if !batch.callbacks.is_empty() && batch.callback_data.is_some() {
            self.can_enqueue_frame_lifecycle(
                FRAME_CALLBACK_COMPLETION_EVENTS,
                FRAME_CALLBACK_COMPLETION_BYTES,
            )
        } else if !batch.callbacks.is_empty() || batch.surface_delete_id.is_some() {
            self.can_enqueue_frame_lifecycle(
                FRAME_CALLBACK_CANCELLATION_EVENTS,
                FRAME_CALLBACK_CANCELLATION_BYTES,
            )
        } else {
            false
        }
    }

    /// Queue one ready callback lifecycle item. Presentation-qualified items
    /// emit `done`, retire the callback object, then emit `delete_id` after an
    /// exact two-event preflight. Destroy-cancelled callbacks emit only
    /// `delete_id`; the surface acknowledgement trails its cancellation batch.
    pub(crate) fn try_complete_ready_frame_callback_lifecycle(
        &mut self,
    ) -> FrameCallbackCompletion {
        while self
            .ready_frame_callbacks
            .front()
            .is_some_and(|batch| batch.callbacks.is_empty() && batch.surface_delete_id.is_none())
        {
            self.ready_frame_callbacks.pop_front();
        }
        let Some(batch) = self.ready_frame_callbacks.front() else {
            return FrameCallbackCompletion::Empty;
        };
        if !self.ready_frame_callback_lifecycle_can_progress() {
            return FrameCallbackCompletion::Blocked;
        }

        if let Some((_, callback_id)) = batch.callbacks.front().copied() {
            let callback_data = batch.callback_data;
            debug_assert_eq!(self.objects.get(&callback_id), Some(&Interface::Callback));
            match callback_data {
                Some(callback_data) => {
                    self.emit_callback_done(callback_id, callback_data)
                        .expect("preflighted callback.done must fit");
                    self.drop_object(callback_id);
                    self.emit_delete_id(callback_id)
                        .expect("preflighted callback delete_id must fit");
                }
                None => {
                    self.drop_object(callback_id);
                    self.emit_delete_id(callback_id)
                        .expect("preflighted cancelled callback delete_id must fit");
                }
            }
            self.ready_frame_callbacks
                .front_mut()
                .expect("front batch exists")
                .callbacks
                .pop_front();
        } else {
            let surface_id = self
                .ready_frame_callbacks
                .front_mut()
                .expect("front batch exists")
                .surface_delete_id
                .take()
                .expect("empty callback batch retains surface delete");
            self.emit_delete_id(surface_id)
                .expect("preflighted surface delete_id must fit");
            self.retired_object_ids.remove(&surface_id);
        }
        while self
            .ready_frame_callbacks
            .front()
            .is_some_and(|batch| batch.callbacks.is_empty() && batch.surface_delete_id.is_none())
        {
            self.ready_frame_callbacks.pop_front();
        }
        FrameCallbackCompletion::Completed
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
        self.ensure_client_object_installable(id)?;
        self.objects.insert(id, interface);
        Ok(())
    }

    /// Allocate a fresh server-side ID and install an object
    /// at it. Returns the chosen ID. Used when the server
    /// creates an object the client didn't ask for — e.g. the
    /// default `pmd_output` that's advertised via `registry.
    /// global` once the client binds the registry.
    pub fn install_server_object(&mut self, interface: Interface) -> Result<ObjectId, ClientError> {
        self.ensure_capacity(
            ClientResource::Objects,
            self.object_count(),
            1,
            self.limits.objects,
        )?;
        let id = self
            .server_ids
            .allocate()
            .map_err(|_| ClientError::DuplicateObject {
                id: self.server_ids.peek(),
            })?;
        self.objects.insert(id, interface);
        Ok(id)
    }

    fn ensure_client_object_installable(&self, id: ObjectId) -> Result<(), ClientError> {
        if id.kind() != IdKind::Client {
            return Err(ClientError::IllegalBindTarget { requested: id });
        }
        if self.objects.contains_key(&id) || self.retired_object_ids.contains(&id) {
            return Err(ClientError::DuplicateObject { id });
        }
        self.ensure_capacity(
            ClientResource::Objects,
            self.object_count(),
            1,
            self.limits.objects,
        )
    }

    fn ensure_capacity(
        &self,
        resource: ClientResource,
        current: usize,
        additional: usize,
        limit: usize,
    ) -> Result<(), ClientError> {
        let attempted = current.saturating_add(additional);
        if attempted > limit {
            return Err(ClientError::ResourceLimitExceeded {
                resource,
                attempted: attempted as u64,
                limit: limit as u64,
            });
        }
        Ok(())
    }

    fn ensure_pool_byte_capacity(&self, additional: u64) -> Result<(), ClientError> {
        let attempted = self.pool_bytes.saturating_add(additional);
        if attempted > self.limits.pool_bytes {
            return Err(ClientError::ResourceLimitExceeded {
                resource: ClientResource::PoolBytes,
                attempted,
                limit: self.limits.pool_bytes,
            });
        }
        Ok(())
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
        self.dispatch_request_with_server_budgets(
            header,
            payload,
            ServerResourceBudgets {
                pool_bytes: 0,
                pool_byte_limit: u64::MAX,
                toplevels: self.toplevels.len(),
                toplevel_limit: usize::MAX,
                toplevel_metadata_bytes: self.toplevel_metadata_bytes,
                toplevel_metadata_byte_limit: u64::MAX,
            },
        )
    }

    /// Dispatch with a server-wide shm-pool budget in addition to this
    /// connection's own limits. `server_pool_bytes` includes this client's
    /// existing pools; admission therefore compares the requested positive
    /// delta against the complete live-server total before allocating.
    pub fn dispatch_request_with_pool_budget(
        &mut self,
        header: MessageHeader,
        payload: &[u8],
        server_pool_bytes: u64,
        server_pool_byte_limit: u64,
    ) -> Result<(), ClientError> {
        self.dispatch_request_with_server_budgets(
            header,
            payload,
            ServerResourceBudgets {
                pool_bytes: server_pool_bytes,
                pool_byte_limit: server_pool_byte_limit,
                toplevels: self.toplevels.len(),
                toplevel_limit: usize::MAX,
                toplevel_metadata_bytes: self.toplevel_metadata_bytes,
                toplevel_metadata_byte_limit: u64::MAX,
            },
        )
    }

    /// Dispatch with server-global retained backing and toplevel-metadata
    /// budgets. Admission examines the wire string length before allocating an
    /// owned title or app id.
    pub(crate) fn dispatch_request_with_server_budgets(
        &mut self,
        header: MessageHeader,
        payload: &[u8],
        server: ServerResourceBudgets,
    ) -> Result<(), ClientError> {
        let interface =
            self.objects
                .get(&header.object_id)
                .copied()
                .ok_or(ClientError::UnknownObject {
                    id: header.object_id,
                })?;
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

        if interface == Interface::ShellManager && header.opcode == 8 {
            display_proto::requests::ShellManagerDesktopReady::decode(payload).map_err(
                |error| ClientError::Malformed {
                    interface,
                    opcode: header.opcode,
                    error,
                },
            )?;
        }
        if interface == Interface::ShellManager {
            let decoded = match header.opcode {
                9 => ShellManagerSubscribeWindowState::decode(payload).map(|_| ()),
                10 => ShellManagerBeginRestore::decode(payload).map(|_| ()),
                11 => ShellManagerPlaceRestoredWindow::decode(payload).map(|_| ()),
                12 => ShellManagerEndRestore::decode(payload).map(|_| ()),
                _ => Ok(()),
            };
            decoded.map_err(|error| ClientError::Malformed {
                interface,
                opcode: header.opcode,
                error,
            })?;
        }

        if interface == Interface::XdgShell && header.opcode == 1 {
            let attempted = server.toplevels.saturating_add(1);
            if attempted > server.toplevel_limit {
                let error = ClientError::ResourceLimitExceeded {
                    resource: ClientResource::ServerToplevels,
                    attempted: attempted as u64,
                    limit: server.toplevel_limit as u64,
                };
                self.report_dispatch_error(header.object_id, &error);
                return Err(error);
            }
        }

        let toplevel_metadata_bytes_after = match self.admit_toplevel_metadata_change(
            header.object_id,
            interface,
            header.opcode,
            payload,
            server.toplevel_metadata_bytes,
            server.toplevel_metadata_byte_limit,
        ) {
            Ok(change) => change,
            Err(error) => {
                self.report_dispatch_error(header.object_id, &error);
                return Err(error);
            }
        };

        let mutation = self
            .auto_install(
                header.object_id,
                interface,
                header.opcode,
                payload,
                server.pool_bytes,
                server.pool_byte_limit,
            )
            .and_then(|()| {
                self.apply_resource_lifecycle(
                    header.object_id,
                    interface,
                    header.opcode,
                    payload,
                    server.pool_bytes,
                    server.pool_byte_limit,
                )
            });
        if let Err(e) = mutation {
            self.report_dispatch_error(header.object_id, &e);
            return Err(e);
        }

        // Surface state transitions (attach / damage /
        // commit) don't install new objects but DO mutate
        // per-surface state on the client. Handle them
        // after auto_install so `Surface` existence
        // is already guaranteed by `create_surface`.
        self.apply_surface_state(header.object_id, interface, header.opcode, payload)?;
        self.reap_retired_resources();

        // `pmd_xdg_toplevel.set_title` / `set_app_id`
        // similarly mutate per-toplevel state without
        // installing new objects.
        self.apply_toplevel_state(header.object_id, interface, header.opcode, payload)?;
        if let Some(bytes_after) = toplevel_metadata_bytes_after {
            self.toplevel_metadata_bytes = bytes_after;
        }

        if self.journal.len() == MAX_JOURNAL_ENTRIES {
            self.journal.pop_front();
        }
        self.journal.push_back(HandledRequest {
            object_id: header.object_id,
            interface,
            opcode: header.opcode,
            opcode_name: opcode.name,
            payload_len: payload.len(),
            fd_passing: header.fd_passing,
        });
        if interface == Interface::Display && header.opcode == 1 {
            let request = DisplaySync::decode(payload).map_err(|error| ClientError::Malformed {
                interface,
                opcode: header.opcode,
                error,
            })?;
            self.emit_callback_done(request.new_id, 0)?;
            self.drop_object(request.new_id);
            self.emit_delete_id(request.new_id)?;
        }
        Ok(())
    }

    fn report_dispatch_error(&mut self, object_id: ObjectId, error: &ClientError) {
        match error {
            ClientError::PermissionDenied {
                interface: target,
                required,
                new_id,
            } => {
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
            ClientError::ResourceLimitExceeded {
                resource,
                attempted,
                limit,
            } => {
                let msg = alloc::format!(
                    "resource limit exceeded: {resource:?} attempted={attempted} limit={limit}"
                );
                let _ = self.emit_error(object_id, ERROR_CODE_NO_MEMORY, &msg);
            }
            ClientError::PoolTooLarge { requested, max } => {
                let msg = alloc::format!(
                    "resource limit exceeded: PoolBytes requested={requested} max={max}"
                );
                let _ = self.emit_error(object_id, ERROR_CODE_NO_MEMORY, &msg);
            }
            ClientError::PoolAllocationFailed { requested } => {
                let msg =
                    alloc::format!("resource allocation failed: PoolBytes requested={requested}");
                let _ = self.emit_error(object_id, ERROR_CODE_NO_MEMORY, &msg);
            }
            _ => {}
        }
    }

    fn admit_toplevel_metadata_change(
        &self,
        toplevel_id: ObjectId,
        interface: Interface,
        opcode: u16,
        payload: &[u8],
        server_bytes: u64,
        server_byte_limit: u64,
    ) -> Result<Option<u64>, ClientError> {
        if interface != Interface::XdgToplevel || !matches!(opcode, 1 | 2) {
            return Ok(None);
        }
        let new_bytes = match opcode {
            1 => XdgToplevelSetTitle::title_byte_len(payload),
            2 => XdgToplevelSetAppId::app_id_byte_len(payload),
            _ => unreachable!(),
        }
        .map_err(|error| ClientError::Malformed {
            interface,
            opcode,
            error,
        })? as u64;
        let toplevel = self
            .toplevels
            .get(&toplevel_id)
            .ok_or(ClientError::UnknownToplevel { toplevel_id })?;
        let old_bytes = match opcode {
            1 => toplevel.title.len(),
            2 => toplevel.app_id.len(),
            _ => unreachable!(),
        } as u64;
        let unchanged_field_bytes = match opcode {
            1 => toplevel.app_id.len(),
            2 => toplevel.title.len(),
            _ => unreachable!(),
        } as u64;
        let toplevel_after = unchanged_field_bytes.saturating_add(new_bytes);
        if toplevel_after > MAX_TOPLEVEL_METADATA_BYTES {
            return Err(ClientError::ResourceLimitExceeded {
                resource: ClientResource::ToplevelMetadataBytesPerWindow,
                attempted: toplevel_after,
                limit: MAX_TOPLEVEL_METADATA_BYTES,
            });
        }

        let client_after = self
            .toplevel_metadata_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        if client_after > self.limits.toplevel_metadata_bytes {
            return Err(ClientError::ResourceLimitExceeded {
                resource: ClientResource::ToplevelMetadataBytes,
                attempted: client_after,
                limit: self.limits.toplevel_metadata_bytes,
            });
        }

        let server_after = server_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        if server_after > server_byte_limit {
            return Err(ClientError::ResourceLimitExceeded {
                resource: ClientResource::ServerToplevelMetadataBytes,
                attempted: server_after,
                limit: server_byte_limit,
            });
        }
        Ok(Some(client_after))
    }

    /// Apply requests that mutate or retire resource objects. Destroy removes
    /// the protocol object immediately, but backing referenced by a surface is
    /// retained until the reference moves away. This mirrors Wayland's
    /// object-lifetime rule without letting stale IDs be attached again.
    fn apply_resource_lifecycle(
        &mut self,
        target_id: ObjectId,
        interface: Interface,
        opcode: u16,
        payload: &[u8],
        server_pool_bytes: u64,
        server_pool_byte_limit: u64,
    ) -> Result<(), ClientError> {
        match (interface, opcode) {
            (Interface::ShmPool, 2 /* resize */) => {
                let req =
                    ShmPoolResize::decode(payload).map_err(|error| ClientError::Malformed {
                        interface,
                        opcode,
                        error,
                    })?;
                if req.new_size > MAX_POOL_SIZE {
                    return Err(ClientError::PoolTooLarge {
                        requested: req.new_size,
                        max: MAX_POOL_SIZE,
                    });
                }
                let old_size = self
                    .pools
                    .get(&target_id)
                    .ok_or(ClientError::UnknownPool { pool_id: target_id })?
                    .size;
                let required = self
                    .buffers
                    .values()
                    .filter(|buffer| buffer.pool_id == target_id)
                    .map(BufferInfo::byte_end)
                    .max()
                    .unwrap_or(0);
                if u64::from(req.new_size) < required {
                    return Err(ClientError::PoolResizeWouldTruncateBuffer {
                        pool_id: target_id,
                        requested: req.new_size,
                        required,
                    });
                }
                let additional = req.new_size.saturating_sub(old_size) as u64;
                self.ensure_pool_byte_capacity(additional)?;
                let server_attempted = server_pool_bytes.saturating_add(additional);
                if server_attempted > server_pool_byte_limit {
                    return Err(ClientError::ResourceLimitExceeded {
                        resource: ClientResource::ServerPoolBytes,
                        attempted: server_attempted,
                        limit: server_pool_byte_limit,
                    });
                }

                let pool = self
                    .pools
                    .get_mut(&target_id)
                    .ok_or(ClientError::UnknownPool { pool_id: target_id })?;
                let new_len = req.new_size as usize;
                if new_len > pool.storage.len() {
                    pool.storage
                        .try_reserve_exact(new_len - pool.storage.len())
                        .map_err(|_| ClientError::PoolAllocationFailed {
                            requested: req.new_size,
                        })?;
                }
                pool.storage.resize(new_len, 0);
                pool.size = req.new_size;
                self.pool_bytes = self
                    .pool_bytes
                    .saturating_sub(u64::from(old_size))
                    .saturating_add(u64::from(req.new_size));
                Ok(())
            }
            (Interface::ShmPool, 3 /* destroy */) => {
                self.objects.remove(&target_id);
                self.retired_object_ids.insert(target_id);
                self.reap_retired_resources();
                Ok(())
            }
            (Interface::Buffer, 1 /* destroy */) => {
                self.objects.remove(&target_id);
                self.retired_object_ids.insert(target_id);
                self.reap_retired_resources();
                Ok(())
            }
            (Interface::Surface, 1 /* destroy */) => {
                if let Some(&toplevel_id) = self.toplevel_by_surface.get(&target_id) {
                    return Err(ClientError::SurfaceHasToplevel {
                        surface_id: target_id,
                        toplevel_id,
                    });
                }
                self.objects.remove(&target_id);
                self.retired_object_ids.insert(target_id);
                let removed_surface = self.surfaces.remove(&target_id);
                let released_buffer = removed_surface
                    .as_ref()
                    .and_then(|surface| surface.current_buffer)
                    .map(|attachment| attachment.buffer_id);
                let pending_callbacks = removed_surface
                    .map(|surface| surface.pending_frame_callbacks)
                    .unwrap_or_default();
                let cancelled_callbacks =
                    self.take_surface_frame_callbacks(target_id, pending_callbacks);
                self.queue_surface_frame_callback_cancellation(target_id, cancelled_callbacks);
                self.emit_buffer_release_if_unused(released_buffer);
                self.reap_retired_resources();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Remove callbacks that have not crossed a presentation boundary from
    /// `surface_id`. Older ready batches are irrevocably done-eligible and stay
    /// in the lifecycle FIFO ahead of this cancellation batch.
    fn take_surface_frame_callbacks(
        &mut self,
        surface_id: ObjectId,
        pending_callbacks: Vec<ObjectId>,
    ) -> Vec<ObjectId> {
        let mut cancelled = Vec::new();
        self.awaiting_present_callbacks
            .retain(|(owner, callback_id)| {
                if *owner == surface_id {
                    cancelled.push(*callback_id);
                    false
                } else {
                    true
                }
            });
        cancelled.extend(pending_callbacks);
        cancelled
    }

    /// Detach all backing from a surface that has just lost its toplevel role.
    /// Server owns the global window registry, so it calls this only after that
    /// role has been retired.
    pub(crate) fn detach_surface_resources(&mut self, surface_id: ObjectId) {
        let released_buffer = if let Some(surface) = self.surfaces.get_mut(&surface_id) {
            let released_buffer = surface
                .current_buffer
                .map(|attachment| attachment.buffer_id);
            surface.pending_buffer = None;
            surface.pending_attach = false;
            surface.pending_damage.clear();
            surface.current_buffer = None;
            released_buffer
        } else {
            None
        };
        self.emit_buffer_release_if_unused(released_buffer);
        self.reap_retired_resources();
    }

    fn emit_buffer_release_if_unused(&mut self, buffer_id: Option<ObjectId>) {
        let Some(buffer_id) = buffer_id else {
            return;
        };
        let still_current = self.surfaces.values().any(|surface| {
            surface
                .current_buffer
                .is_some_and(|attachment| attachment.buffer_id == buffer_id)
        });
        if !still_current {
            // A client-destroyed buffer is no longer an event target. Its
            // retained backing is reclaimed by reap and acknowledged only with
            // delete_id after the final current reference moves away.
            if self.objects.get(&buffer_id).copied() == Some(Interface::Buffer) {
                let _ = self.emit_buffer_release(buffer_id);
            }
        }
    }

    fn ensure_pool_write_rows_not_current(
        &self,
        pool_id: ObjectId,
        write_offset: u64,
        row_bytes: u64,
        rows: u64,
        stride: u64,
    ) -> Result<(), ClientError> {
        if row_bytes == 0 || rows == 0 {
            return Ok(());
        }
        for (surface_id, surface) in self.surfaces.iter() {
            let Some(attachment) = surface.current_buffer else {
                continue;
            };
            let buffer = self.buffers.get(&attachment.buffer_id).ok_or(
                ClientError::PoolWriteInvalidCurrentBacking {
                    surface_id: *surface_id,
                    buffer_id: attachment.buffer_id,
                },
            )?;
            let current_pool = self.pools.get(&buffer.pool_id).ok_or(
                ClientError::PoolWriteInvalidCurrentBacking {
                    surface_id: *surface_id,
                    buffer_id: attachment.buffer_id,
                },
            )?;
            let current_start = u64::from(buffer.offset);
            let current_end = buffer.byte_end();
            if current_end > u64::from(current_pool.size)
                || current_end > current_pool.storage.len() as u64
            {
                return Err(ClientError::PoolWriteInvalidCurrentBacking {
                    surface_id: *surface_id,
                    buffer_id: attachment.buffer_id,
                });
            }
            if buffer.pool_id != pool_id {
                continue;
            }

            // Written rows are ordered and non-overlapping. Jump directly to
            // the first row whose exclusive end can cross current_start, then
            // one interval comparison proves exact byte overlap in O(1).
            let mut row = if write_offset.saturating_add(row_bytes) > current_start {
                0
            } else {
                current_start.saturating_sub(write_offset) / stride
            };
            let candidate_start = write_offset.saturating_add(row.saturating_mul(stride));
            if candidate_start.saturating_add(row_bytes) <= current_start {
                row = row.saturating_add(1);
            }
            let intersects = if row < rows {
                let candidate_start = write_offset + row * stride;
                candidate_start < current_end
                    && current_start < candidate_start.saturating_add(row_bytes)
            } else {
                false
            };
            if intersects {
                return Err(ClientError::PoolWriteIntersectsCurrentBuffer {
                    pool_id,
                    surface_id: *surface_id,
                    buffer_id: attachment.buffer_id,
                });
            }
        }
        Ok(())
    }

    fn retained_buffer_extent_for_commit(
        &self,
        surface_id: ObjectId,
        buffer_id: ObjectId,
    ) -> Result<(ObjectId, u64, u64), ClientError> {
        let buffer =
            self.buffers
                .get(&buffer_id)
                .ok_or(ClientError::SurfaceCommitInvalidBacking {
                    surface_id,
                    buffer_id,
                })?;
        let pool =
            self.pools
                .get(&buffer.pool_id)
                .ok_or(ClientError::SurfaceCommitInvalidBacking {
                    surface_id,
                    buffer_id,
                })?;
        let start = u64::from(buffer.offset);
        let end = buffer.byte_end();
        if end > u64::from(pool.size) || end > pool.storage.len() as u64 {
            return Err(ClientError::SurfaceCommitInvalidBacking {
                surface_id,
                buffer_id,
            });
        }
        Ok((buffer.pool_id, start, end))
    }

    fn validate_surface_commit_exclusivity(&self, surface_id: ObjectId) -> Result<(), ClientError> {
        let surface = self
            .surfaces
            .get(&surface_id)
            .ok_or(ClientError::UnknownSurface { surface_id })?;
        if !surface.pending_attach {
            return Ok(());
        }
        let Some(candidate) = surface.pending_buffer else {
            return Ok(());
        };
        let candidate_extent =
            self.retained_buffer_extent_for_commit(surface_id, candidate.buffer_id)?;

        let conflicts = |other_extent: (ObjectId, u64, u64)| {
            candidate_extent.0 == other_extent.0
                && candidate_extent.1 < other_extent.2
                && other_extent.1 < candidate_extent.2
        };

        if let Some(old) = surface.current_buffer {
            if old.buffer_id != candidate.buffer_id {
                let old_extent =
                    self.retained_buffer_extent_for_commit(surface_id, old.buffer_id)?;
                if conflicts(old_extent) {
                    return Err(ClientError::SurfaceCommitAliasedBuffer {
                        surface_id,
                        buffer_id: candidate.buffer_id,
                        conflicting_surface_id: surface_id,
                        conflicting_buffer_id: old.buffer_id,
                    });
                }
            }
        }

        for (other_surface_id, other_surface) in self.surfaces.iter() {
            if *other_surface_id == surface_id {
                continue;
            }
            let Some(other) = other_surface.current_buffer else {
                continue;
            };
            let other_extent =
                self.retained_buffer_extent_for_commit(*other_surface_id, other.buffer_id)?;
            if conflicts(other_extent) {
                return Err(ClientError::SurfaceCommitAliasedBuffer {
                    surface_id,
                    buffer_id: candidate.buffer_id,
                    conflicting_surface_id: *other_surface_id,
                    conflicting_buffer_id: other.buffer_id,
                });
            }
        }
        Ok(())
    }

    /// Reclaim destroyed buffer objects once no surface retains them, then
    /// reclaim destroyed pools once no buffer metadata retains their backing.
    fn reap_retired_resources(&mut self) {
        let retired_buffers: Vec<ObjectId> = self
            .buffers
            .keys()
            .copied()
            .filter(|buffer_id| {
                self.objects.get(buffer_id).copied() != Some(Interface::Buffer)
                    && !self.surfaces.values().any(|surface| {
                        surface
                            .pending_buffer
                            .map(|attachment| attachment.buffer_id == *buffer_id)
                            .unwrap_or(false)
                            || surface
                                .current_buffer
                                .map(|attachment| attachment.buffer_id == *buffer_id)
                                .unwrap_or(false)
                    })
            })
            .collect();
        for buffer_id in retired_buffers {
            self.buffers.remove(&buffer_id);
            self.retired_object_ids.remove(&buffer_id);
            let _ = self.emit_delete_id(buffer_id);
        }

        let retired_pools: Vec<ObjectId> = self
            .pools
            .keys()
            .copied()
            .filter(|pool_id| {
                self.objects.get(pool_id).copied() != Some(Interface::ShmPool)
                    && !self
                        .buffers
                        .values()
                        .any(|buffer| buffer.pool_id == *pool_id)
            })
            .collect();
        for pool_id in retired_pools {
            if let Some(pool) = self.pools.remove(&pool_id) {
                self.pool_bytes = self.pool_bytes.saturating_sub(u64::from(pool.size));
            }
            self.retired_object_ids.remove(&pool_id);
            let _ = self.emit_delete_id(pool_id);
        }
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
            5 /* set_maximized */ => {
                let toplevel = self
                    .toplevels
                    .get_mut(&toplevel_id)
                    .ok_or(ClientError::UnknownToplevel { toplevel_id })?;
                toplevel.maximized = true;
                Ok(())
            }
            6 /* unset_maximized */ => {
                let toplevel = self
                    .toplevels
                    .get_mut(&toplevel_id)
                    .ok_or(ClientError::UnknownToplevel { toplevel_id })?;
                toplevel.maximized = false;
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
                    && self.objects.get(&req.buffer_id).copied() != Some(Interface::Buffer)
                {
                    return Err(ClientError::UnknownBuffer {
                        buffer_id: req.buffer_id,
                    });
                }
                let surface = self
                    .surfaces
                    .get_mut(&surface_id)
                    .ok_or(ClientError::UnknownSurface { surface_id })?;
                surface.pending_attach = true;
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
                let damage = DamageRect {
                    x: req.x,
                    y: req.y,
                    width: req.width,
                    height: req.height,
                };
                let max_damage = self.limits.damage_rects_per_surface;
                let surface = self
                    .surfaces
                    .get_mut(&surface_id)
                    .ok_or(ClientError::UnknownSurface { surface_id })?;
                if max_damage == 0 {
                    return Ok(());
                }
                if surface.pending_damage.len() < max_damage {
                    surface.pending_damage.push(damage);
                } else {
                    coalesce_damage(&mut surface.pending_damage, damage);
                }
                Ok(())
            }
            7 /* commit */ => {
                self.validate_surface_commit_exclusivity(surface_id)?;
                let (released_buffer, frame_callbacks) = {
                    let surface = self
                        .surfaces
                        .get_mut(&surface_id)
                        .ok_or(ClientError::UnknownSurface { surface_id })?;
                    let old_buffer = surface
                        .current_buffer
                        .map(|attachment| attachment.buffer_id);
                    // Promote pending → current atomically. A commit with no
                    // pending attach keeps the current buffer; attach(NULL)
                    // explicitly clears it.
                    if surface.pending_attach {
                        surface.current_buffer = surface.pending_buffer.take();
                        surface.pending_attach = false;
                    }
                    let current_buffer = surface
                        .current_buffer
                        .map(|attachment| attachment.buffer_id);
                    surface.pending_damage.clear();
                    surface.commit_count = surface.commit_count.saturating_add(1);
                    (
                        (old_buffer != current_buffer).then_some(old_buffer).flatten(),
                        core::mem::take(&mut surface.pending_frame_callbacks),
                    )
                };
                self.awaiting_present_callbacks.extend(
                    frame_callbacks
                        .into_iter()
                        .map(|callback_id| (surface_id, callback_id)),
                );
                // Release only live backing the complete client no longer
                // reads. Client-destroyed IDs are not event targets and flow
                // directly through retained-resource reaping to delete_id.
                self.emit_buffer_release_if_unused(released_buffer);
                Ok(())
            }
            8 /* patch_current */ => self.apply_surface_patch_current(surface_id, payload),
            _ => Ok(()),
        }
    }

    fn apply_surface_patch_current(
        &mut self,
        surface_id: ObjectId,
        payload: &[u8],
    ) -> Result<(), ClientError> {
        let inline_bytes = payload.len().saturating_sub(16);
        if inline_bytes > MAX_SURFACE_PATCH_BYTES {
            return Err(ClientError::SurfacePatchTooLarge {
                bytes: inline_bytes as u64,
                max: MAX_SURFACE_PATCH_BYTES,
            });
        }
        let request =
            SurfacePatchCurrent::decode(payload).map_err(|error| ClientError::Malformed {
                interface: Interface::Surface,
                opcode: 8,
                error,
            })?;
        let patch_bytes = request.pixels.len() as u64;
        if patch_bytes > MAX_SURFACE_PATCH_BYTES as u64 {
            return Err(ClientError::SurfacePatchTooLarge {
                bytes: patch_bytes,
                max: MAX_SURFACE_PATCH_BYTES,
            });
        }

        let surface = self
            .surfaces
            .get(&surface_id)
            .ok_or(ClientError::UnknownSurface { surface_id })?;
        if surface.pending_attach
            || surface.pending_buffer.is_some()
            || !surface.pending_damage.is_empty()
        {
            return Err(ClientError::SurfacePatchHasPendingState { surface_id });
        }
        let attachment = surface
            .current_buffer
            .ok_or(ClientError::SurfacePatchNoCurrentBuffer { surface_id })?;
        let buffer =
            *self
                .buffers
                .get(&attachment.buffer_id)
                .ok_or(ClientError::UnknownBuffer {
                    buffer_id: attachment.buffer_id,
                })?;
        if !matches!(
            buffer.format,
            buffer_format::ARGB8888 | buffer_format::XRGB8888
        ) {
            return Err(ClientError::SurfacePatchUnsupportedFormat {
                buffer_id: attachment.buffer_id,
                format: buffer.format,
            });
        }

        let pool = self
            .pools
            .get(&buffer.pool_id)
            .ok_or(ClientError::UnknownPool {
                pool_id: buffer.pool_id,
            })?;
        let full_row_bytes = u64::from(buffer.width) * 4;
        let buffer_byte_end = buffer.byte_end();
        if u64::from(buffer.stride) < full_row_bytes
            || buffer_byte_end > u64::from(pool.size)
            || buffer_byte_end > pool.storage.len() as u64
        {
            return Err(ClientError::SurfacePatchInvalidBacking {
                buffer_id: attachment.buffer_id,
                pool_id: buffer.pool_id,
            });
        }

        let x = u32::try_from(request.x).ok();
        let y = u32::try_from(request.y).ok();
        let in_bounds = x
            .zip(y)
            .filter(|_| request.width != 0 && request.height != 0)
            .and_then(|(x, y)| {
                let x_end = u64::from(x).checked_add(u64::from(request.width))?;
                let y_end = u64::from(y).checked_add(u64::from(request.height))?;
                (x_end <= u64::from(buffer.width) && y_end <= u64::from(buffer.height))
                    .then_some((x, y))
            });
        let Some((x, y)) = in_bounds else {
            return Err(ClientError::SurfacePatchInvalidGeometry {
                x: request.x,
                y: request.y,
                width: request.width,
                height: request.height,
                buffer_width: buffer.width,
                buffer_height: buffer.height,
            });
        };

        let row_bytes = u64::from(request.width) * 4;
        let x_bytes = u64::from(x) * 4;
        let destination_row = |row: u32| {
            u64::from(buffer.offset)
                .checked_add(u64::from(y + row) * u64::from(buffer.stride))
                .and_then(|start| start.checked_add(x_bytes))
                .and_then(|start| start.checked_add(row_bytes).map(|end| (start, end)))
        };

        // Fixed positive stride makes row destinations monotonic. Proving the
        // first start and final end are in backing proves every intermediate
        // row too, while keeping adversarial validation O(1) in patch height.
        let Some((patch_start, _)) = destination_row(0) else {
            return Err(ClientError::SurfacePatchInvalidBacking {
                buffer_id: attachment.buffer_id,
                pool_id: buffer.pool_id,
            });
        };
        let Some((_, patch_end)) = destination_row(request.height - 1) else {
            return Err(ClientError::SurfacePatchInvalidBacking {
                buffer_id: attachment.buffer_id,
                pool_id: buffer.pool_id,
            });
        };
        if patch_end > u64::from(pool.size)
            || usize::try_from(patch_start)
                .ok()
                .zip(usize::try_from(patch_end).ok())
                .and_then(|(start, end)| pool.storage.get(start..end))
                .is_none()
        {
            return Err(ClientError::SurfacePatchInvalidBacking {
                buffer_id: attachment.buffer_id,
                pool_id: buffer.pool_id,
            });
        }

        // Pool/buffer aliases are legal elsewhere in v1. Directly modifying a
        // byte visible through another current surface would bypass that
        // surface's commit boundary, so reject the transaction conservatively
        // against the other buffer's complete retained extent.
        for (other_surface_id, other_surface) in self.surfaces.iter() {
            if *other_surface_id == surface_id {
                continue;
            }
            let Some(other_attachment) = other_surface.current_buffer else {
                continue;
            };
            let other_buffer = self.buffers.get(&other_attachment.buffer_id).ok_or(
                ClientError::SurfacePatchInvalidAliasBacking {
                    other_surface_id: *other_surface_id,
                    buffer_id: other_attachment.buffer_id,
                },
            )?;
            let other_pool = self.pools.get(&other_buffer.pool_id).ok_or(
                ClientError::SurfacePatchInvalidAliasBacking {
                    other_surface_id: *other_surface_id,
                    buffer_id: other_attachment.buffer_id,
                },
            )?;
            if other_buffer.byte_end() > u64::from(other_pool.size)
                || other_buffer.byte_end() > other_pool.storage.len() as u64
            {
                return Err(ClientError::SurfacePatchInvalidAliasBacking {
                    other_surface_id: *other_surface_id,
                    buffer_id: other_attachment.buffer_id,
                });
            }
            if other_buffer.pool_id != buffer.pool_id {
                continue;
            }
            let other_start = u64::from(other_buffer.offset);
            let other_end = other_buffer.byte_end();
            if patch_start < other_end && other_start < patch_end {
                return Err(ClientError::SurfacePatchAliasedCurrentBuffer {
                    surface_id,
                    other_surface_id: *other_surface_id,
                });
            }
        }

        let row_bytes = row_bytes as usize;
        let Client {
            pools, surfaces, ..
        } = self;
        let pool = pools
            .get_mut(&buffer.pool_id)
            .ok_or(ClientError::UnknownPool {
                pool_id: buffer.pool_id,
            })?;
        let surface = surfaces
            .get_mut(&surface_id)
            .ok_or(ClientError::UnknownSurface { surface_id })?;
        for row in 0..request.height {
            let destination_start = buffer.offset as usize
                + (y + row) as usize * buffer.stride as usize
                + x as usize * 4;
            let destination_end = destination_start + row_bytes;
            let source_start = row as usize * row_bytes;
            let source_end = source_start + row_bytes;
            pool.storage[destination_start..destination_end]
                .copy_from_slice(&request.pixels[source_start..source_end]);
        }
        surface.commit_count = surface.commit_count.saturating_add(1);
        Ok(())
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
        server_pool_bytes: u64,
        server_pool_byte_limit: u64,
    ) -> Result<(), ClientError> {
        match (interface, opcode) {
            (Interface::Display, 1 /* sync */) => {
                let req = DisplaySync::decode(payload).map_err(|e| ClientError::Malformed {
                    interface,
                    opcode,
                    error: e,
                })?;
                self.install_client_object(req.new_id, Interface::Callback)?;
                Ok(())
            }
            (Interface::Display, 2 /* get_registry */) => {
                let req =
                    DisplayGetRegistry::decode(payload).map_err(|e| ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    })?;
                self.install_client_object(req.new_id, Interface::Registry)?;
                Ok(())
            }
            (Interface::Registry, 1 /* bind */) => {
                let req = RegistryBind::decode(payload).map_err(|e| ClientError::Malformed {
                    interface,
                    opcode,
                    error: e,
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
                if target == Interface::ShellManager {
                    self.shell_manager_id = Some(req.new_id);
                }
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
                self.ensure_capacity(
                    ClientResource::Surfaces,
                    self.surfaces.len(),
                    1,
                    self.limits.surfaces,
                )?;
                self.ensure_client_object_installable(req.new_id)?;
                self.install_client_object(req.new_id, Interface::Surface)?;
                self.surfaces.insert(req.new_id, Surface::new(req.new_id));
                Ok(())
            }
            (Interface::Surface, 4 /* frame */) => {
                let request =
                    SurfaceFrame::decode(payload).map_err(|error| ClientError::Malformed {
                        interface,
                        opcode,
                        error,
                    })?;
                if !self.surfaces.contains_key(&target_id) {
                    return Err(ClientError::UnknownSurface {
                        surface_id: target_id,
                    });
                }
                self.install_client_object(request.new_id, Interface::Callback)?;
                self.surfaces
                    .get_mut(&target_id)
                    .expect("validated surface exists")
                    .pending_frame_callbacks
                    .push(request.new_id);
                Ok(())
            }
            (Interface::Shm, 1 /* create_pool */) => {
                let req = ShmCreatePool::decode(payload).map_err(|e| ClientError::Malformed {
                    interface,
                    opcode,
                    error: e,
                })?;
                if req.size > MAX_POOL_SIZE {
                    return Err(ClientError::PoolTooLarge {
                        requested: req.size,
                        max: MAX_POOL_SIZE,
                    });
                }
                self.ensure_capacity(
                    ClientResource::Pools,
                    self.pools.len(),
                    1,
                    self.limits.pools,
                )?;
                self.ensure_pool_byte_capacity(req.size as u64)?;
                let server_attempted = server_pool_bytes.saturating_add(req.size as u64);
                if server_attempted > server_pool_byte_limit {
                    return Err(ClientError::ResourceLimitExceeded {
                        resource: ClientResource::ServerPoolBytes,
                        attempted: server_attempted,
                        limit: server_pool_byte_limit,
                    });
                }
                self.ensure_client_object_installable(req.new_id)?;
                let mut storage = Vec::new();
                storage.try_reserve_exact(req.size as usize).map_err(|_| {
                    ClientError::PoolAllocationFailed {
                        requested: req.size,
                    }
                })?;
                storage.resize(req.size as usize, 0);
                self.install_client_object(req.new_id, Interface::ShmPool)?;
                self.pools.insert(
                    req.new_id,
                    Pool {
                        size: req.size,
                        storage,
                    },
                );
                self.pool_bytes = self.pool_bytes.saturating_add(req.size as u64);
                Ok(())
            }
            (Interface::ShmPool, 1 /* create_buffer */) => {
                let req =
                    ShmPoolCreateBuffer::decode(payload).map_err(|e| ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
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
                self.ensure_capacity(
                    ClientResource::Buffers,
                    self.buffers.len(),
                    1,
                    self.limits.buffers,
                )?;
                self.ensure_client_object_installable(req.new_id)?;
                self.install_client_object(req.new_id, Interface::Buffer)?;
                self.buffers.insert(req.new_id, info);
                Ok(())
            }
            (Interface::ShmPool, 4 /* write */) => {
                use display_proto::requests::ShmPoolWrite;
                let req = ShmPoolWrite::decode(payload).map_err(|e| ClientError::Malformed {
                    interface,
                    opcode,
                    error: e,
                })?;
                let pool = self
                    .pools
                    .get(&target_id)
                    .ok_or(ClientError::UnknownPool { pool_id: target_id })?;
                let offset = req.offset as usize;
                let end = offset.saturating_add(req.bytes.len());
                if end > pool.storage.len() {
                    return Err(ClientError::BufferOutOfPool {
                        pool_id: target_id,
                        pool_size: pool.size,
                        byte_end: end as u64,
                    });
                }
                self.ensure_pool_write_rows_not_current(
                    target_id,
                    offset as u64,
                    req.bytes.len() as u64,
                    1,
                    req.bytes.len() as u64,
                )?;
                let pool = self
                    .pools
                    .get_mut(&target_id)
                    .ok_or(ClientError::UnknownPool { pool_id: target_id })?;
                pool.storage[offset..end].copy_from_slice(&req.bytes);
                Ok(())
            }
            (Interface::ShmPool, 5 /* write_rows */) => {
                let req =
                    ShmPoolWriteRows::decode(payload).map_err(|error| ClientError::Malformed {
                        interface,
                        opcode,
                        error,
                    })?;
                if req.row_bytes == 0 || req.rows == 0 || req.stride < req.row_bytes {
                    return Err(ClientError::InvalidPoolWriteRows {
                        row_bytes: req.row_bytes,
                        rows: req.rows,
                        stride: req.stride,
                    });
                }
                let byte_end = u64::from(req.offset)
                    .checked_add(u64::from(req.rows - 1) * u64::from(req.stride))
                    .and_then(|last_row| last_row.checked_add(u64::from(req.row_bytes)))
                    .ok_or(ClientError::InvalidPoolWriteRows {
                        row_bytes: req.row_bytes,
                        rows: req.rows,
                        stride: req.stride,
                    })?;
                let pool = self
                    .pools
                    .get(&target_id)
                    .ok_or(ClientError::UnknownPool { pool_id: target_id })?;
                if byte_end > u64::from(pool.size) {
                    return Err(ClientError::BufferOutOfPool {
                        pool_id: target_id,
                        pool_size: pool.size,
                        byte_end,
                    });
                }

                self.ensure_pool_write_rows_not_current(
                    target_id,
                    u64::from(req.offset),
                    u64::from(req.row_bytes),
                    u64::from(req.rows),
                    u64::from(req.stride),
                )?;

                // All decoding, geometry, and extent validation is complete
                // before the first row is touched, so rejection is atomic.
                let pool = self
                    .pools
                    .get_mut(&target_id)
                    .ok_or(ClientError::UnknownPool { pool_id: target_id })?;
                let row_bytes = req.row_bytes as usize;
                let stride = req.stride as usize;
                let offset = req.offset as usize;
                for row in 0..req.rows as usize {
                    let source_start = row * row_bytes;
                    let destination_start = offset + row * stride;
                    pool.storage[destination_start..destination_start + row_bytes]
                        .copy_from_slice(&req.bytes[source_start..source_start + row_bytes]);
                }
                Ok(())
            }
            (Interface::Seat, 1 /* get_pointer */) => {
                let req = SeatGetPointer::decode(payload).map_err(|e| ClientError::Malformed {
                    interface,
                    opcode,
                    error: e,
                })?;
                if let Some(existing) = self.pointer_id {
                    return Err(ClientError::PointerAlreadyBound { existing });
                }
                self.install_client_object(req.new_id, Interface::Pointer)?;
                self.pointer_id = Some(req.new_id);
                Ok(())
            }
            (Interface::Seat, 2 /* get_keyboard */) => {
                let req = SeatGetKeyboard::decode(payload).map_err(|e| ClientError::Malformed {
                    interface,
                    opcode,
                    error: e,
                })?;
                if let Some(existing) = self.keyboard_id {
                    return Err(ClientError::KeyboardAlreadyBound { existing });
                }
                self.install_client_object(req.new_id, Interface::Keyboard)?;
                self.keyboard_id = Some(req.new_id);
                Ok(())
            }
            (Interface::XdgShell, 1 /* get_toplevel */) => {
                let req =
                    XdgShellGetToplevel::decode(payload).map_err(|e| ClientError::Malformed {
                        interface,
                        opcode,
                        error: e,
                    })?;
                // The referenced surface must already
                // exist and be of type `Surface`.
                if self.objects.get(&req.surface_id).copied() != Some(Interface::Surface) {
                    return Err(ClientError::ToplevelSurfaceNotFound {
                        surface_id: req.surface_id,
                    });
                }
                if let Some(existing) = self.toplevel_by_surface.get(&req.surface_id).copied() {
                    return Err(ClientError::SurfaceAlreadyHasToplevel {
                        surface_id: req.surface_id,
                        existing_toplevel: existing,
                    });
                }
                self.ensure_capacity(
                    ClientResource::Toplevels,
                    self.toplevels.len(),
                    1,
                    self.limits.toplevels,
                )?;
                self.ensure_client_object_installable(req.new_id)?;
                let offset = self.next_toplevel_offset;
                self.install_client_object(req.new_id, Interface::XdgToplevel)?;
                let ordinal = self.next_toplevel_ordinal;
                self.next_toplevel_ordinal = self
                    .next_toplevel_ordinal
                    .checked_add(1)
                    .expect("per-client toplevel ordinal exhausted");
                let mut toplevel = Toplevel::new(req.new_id, req.surface_id, offset, offset);
                toplevel.ordinal = ordinal;
                self.toplevels.insert(req.new_id, toplevel);
                if let Some(surface) = self.surfaces.get_mut(&req.surface_id) {
                    surface.had_toplevel_role = true;
                }
                self.toplevel_by_surface.insert(req.surface_id, req.new_id);
                self.next_toplevel_offset =
                    self.next_toplevel_offset.saturating_add(AUTO_LAYOUT_STEP);
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
        self.pools
            .get_mut(&pool_id)
            .map(|p| p.storage.as_mut_slice())
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

    /// Aggregate bytes retained by this client's shm pools.
    pub fn pool_bytes_len(&self) -> u64 {
        self.pool_bytes
    }

    /// Test helper: drain the dispatch journal and return it.
    pub fn drain_journal(&mut self) -> Vec<HandledRequest> {
        core::mem::take(&mut self.journal).into_iter().collect()
    }

    /// Number of bounded diagnostic records currently retained.
    pub fn journal_len(&self) -> usize {
        self.journal.len()
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
        if self.event_queue_overflowed {
            return Err(ClientError::EventQueueOverflowed);
        }
        let interface = self
            .objects
            .get(&object_id)
            .copied()
            .ok_or(ClientError::UnknownObject { id: object_id })?;
        interface.lookup_event(opcode).map_err(|e| match e {
            OpcodeError::UnknownOpcode { interface, opcode } => {
                if interface.lookup_request(opcode).is_ok() {
                    ClientError::WrongDirection { interface, opcode }
                } else {
                    ClientError::UnknownOpcode { interface, opcode }
                }
            }
        })?;
        let header = MessageHeader::try_new(object_id, opcode, payload.len(), 0)?;
        let total = HEADER_SIZE.saturating_add(payload.len());
        // Window titles are state, not an unbounded history stream. Preserve
        // the FIFO position of the first not-yet-transported update for a
        // window while replacing its payload with the latest value. This
        // prevents ordinary title churn from poisoning a subscribed shell's
        // bounded event queue, and the shell still observes the exact final
        // state before any subsequently queued event.
        let replace_title = (interface == Interface::ShellManager && opcode == 4)
            .then(|| payload.get(..4))
            .flatten()
            .and_then(|window_id| {
                self.pending_events.iter().position(|message| {
                    let Ok(pending_header) = MessageHeader::decode(message) else {
                        return false;
                    };
                    pending_header.object_id == object_id
                        && pending_header.opcode == opcode
                        && message.get(HEADER_SIZE..HEADER_SIZE + 4) == Some(window_id)
                })
            })
            .map(|index| (index, opcode));
        // V2 state changes replace an unsent creation snapshot for the same
        // window when one exists, otherwise the oldest unsent state update.
        // This keeps creation ordering intact while bounding metadata, commit,
        // and focus churn to one queued authoritative state per window.
        let replace_state = (interface == Interface::ShellManager && opcode == 6)
            .then(|| payload.get(..8))
            .flatten()
            .and_then(|state_key| {
                self.pending_events.iter().position(|message| {
                    let Ok(pending_header) = MessageHeader::decode(message) else {
                        return false;
                    };
                    pending_header.object_id == object_id
                        && matches!(pending_header.opcode, 5 | 6)
                        && message.get(HEADER_SIZE..HEADER_SIZE + 8) == Some(state_key)
                })
            })
            .and_then(|index| {
                MessageHeader::decode(&self.pending_events[index])
                    .ok()
                    .map(|header| (index, header.opcode))
            });
        if let Some((index, replacement_opcode)) = replace_title.or(replace_state) {
            let old_len = self.pending_events[index].len();
            let attempted = self
                .pending_event_bytes
                .saturating_sub(old_len)
                .saturating_add(total);
            if attempted > self.limits.pending_event_bytes {
                self.event_queue_overflowed = true;
                return Err(ClientError::ResourceLimitExceeded {
                    resource: ClientResource::PendingEventBytes,
                    attempted: attempted as u64,
                    limit: self.limits.pending_event_bytes as u64,
                });
            }
            let replacement_header =
                MessageHeader::try_new(object_id, replacement_opcode, payload.len(), 0)?;
            let mut buf = Vec::with_capacity(total);
            buf.resize(HEADER_SIZE, 0);
            replacement_header.encode(&mut buf[..HEADER_SIZE])?;
            buf.extend_from_slice(payload);
            self.pending_events[index] = buf;
            self.pending_event_bytes = attempted;
            return Ok(total);
        }
        if let Err(error) = self.ensure_capacity(
            ClientResource::PendingEvents,
            self.pending_events.len(),
            1,
            self.limits.pending_events,
        ) {
            self.event_queue_overflowed = true;
            return Err(error);
        }
        if let Err(error) = self.ensure_capacity(
            ClientResource::PendingEventBytes,
            self.pending_event_bytes,
            total,
            self.limits.pending_event_bytes,
        ) {
            self.event_queue_overflowed = true;
            return Err(error);
        }
        let mut buf = Vec::with_capacity(total);
        buf.resize(HEADER_SIZE, 0);
        header.encode(&mut buf[..HEADER_SIZE])?;
        buf.extend_from_slice(payload);
        debug_assert_eq!(buf.len(), total);
        self.pending_events.push(buf);
        self.pending_event_bytes += total;
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

    /// Emit the one-shot completion marker for display.sync/frame ordering.
    pub fn emit_callback_done(
        &mut self,
        callback_id: ObjectId,
        callback_data: u32,
    ) -> Result<usize, ClientError> {
        let event = CallbackDone { callback_data };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(callback_id, 1 /* done */, &payload)
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

    /// Emit `pmd_xdg_toplevel.configure(serial, width,
    /// height, states)` — spec §11/§12 collapsed. The
    /// `serial` is the value the client must echo back
    /// via `ack_configure`; `states` is a bitfield of
    /// [`display_proto::xdg_toplevel_state`] bits — pass
    /// `0` for "no special state".
    pub fn emit_xdg_toplevel_configure(
        &mut self,
        toplevel_id: ObjectId,
        serial: u32,
        width: i32,
        height: i32,
        states: u32,
    ) -> Result<usize, ClientError> {
        use display_proto::events::XdgToplevelConfigure;
        let event = XdgToplevelConfigure {
            serial,
            width,
            height,
            states,
        };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(toplevel_id, 1 /* configure */, &payload)
    }

    /// Allocate the next `serial` for `emit_xdg_toplevel_configure`.
    /// Increments the per-client counter; never returns 0.
    pub fn next_configure_serial(&mut self) -> u32 {
        let s = self.next_configure_serial;
        self.next_configure_serial = self.next_configure_serial.saturating_add(1).max(1);
        s
    }

    /// Emit `pmd_buffer.release(buffer_id)` after that buffer has ceased to be
    /// current on every surface owned by this client.
    pub fn emit_buffer_release(&mut self, buffer_id: ObjectId) -> Result<usize, ClientError> {
        self.emit_raw(buffer_id, 1 /* release */, &[])
    }

    /// Emit `pmd_xdg_toplevel.close` — spec §12. No payload.
    pub fn emit_xdg_toplevel_close(&mut self, toplevel_id: ObjectId) -> Result<usize, ClientError> {
        self.emit_raw(toplevel_id, 2 /* close */, &[])
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
        self.emit_raw(
            shell_manager_id,
            4, /* window_title_changed */
            &payload,
        )
    }

    /// Emit one authoritative v2 creation snapshot.
    pub fn emit_window_created_v2(
        &mut self,
        shell_manager_id: ObjectId,
        state: &ShellWindowState,
    ) -> Result<usize, ClientError> {
        let mut payload = Vec::new();
        state.encode(&mut payload);
        self.emit_raw(shell_manager_id, 5 /* window_created_v2 */, &payload)
    }

    /// Emit a coalescible authoritative v2 state update.
    pub fn emit_window_state_changed(
        &mut self,
        shell_manager_id: ObjectId,
        state: &ShellWindowState,
    ) -> Result<usize, ClientError> {
        let mut payload = Vec::new();
        state.encode(&mut payload);
        self.emit_raw(
            shell_manager_id,
            6, /* window_state_changed */
            &payload,
        )
    }

    pub fn emit_window_snapshot_done(
        &mut self,
        shell_manager_id: ObjectId,
        snapshot_id: u32,
    ) -> Result<usize, ClientError> {
        let mut payload = Vec::new();
        ShellWindowSnapshotDone { snapshot_id }.encode(&mut payload);
        self.emit_raw(
            shell_manager_id,
            7, /* window_snapshot_done */
            &payload,
        )
    }

    pub fn emit_restore_finished(
        &mut self,
        shell_manager_id: ObjectId,
        restore_id: u32,
        status: u32,
        placed: u32,
    ) -> Result<usize, ClientError> {
        let event = ShellRestoreFinished {
            restore_id,
            status,
            placed,
        };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(shell_manager_id, 8 /* restore_finished */, &payload)
    }

    /// Emit `pmd_pointer.motion(surface_id, x, y)` on
    /// this client's pointer object, if one has been
    /// allocated via `pmd_seat.get_pointer`.
    pub fn emit_pointer_motion(
        &mut self,
        surface_id: ObjectId,
        x: i32,
        y: i32,
    ) -> Result<usize, ClientError> {
        let pointer_id = self
            .pointer_id
            .ok_or(ClientError::UnknownObject { id: ObjectId::NULL })?;
        let event = PointerMotion { surface_id, x, y };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(pointer_id, 1 /* motion */, &payload)
    }

    /// Emit `pmd_pointer.button(surface_id, x, y, button, state)`.
    pub fn emit_pointer_button(
        &mut self,
        serial: u32,
        surface_id: ObjectId,
        x: i32,
        y: i32,
        button: u32,
        state: u32,
    ) -> Result<usize, ClientError> {
        let pointer_id = self
            .pointer_id
            .ok_or(ClientError::UnknownObject { id: ObjectId::NULL })?;
        let event = PointerButton {
            serial,
            surface_id,
            x,
            y,
            button,
            state,
        };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(pointer_id, 2 /* button */, &payload)
    }

    /// Emit `pmd_keyboard.key(surface_id, key, state)`.
    pub fn emit_keyboard_key(
        &mut self,
        surface_id: ObjectId,
        key: u32,
        state: u32,
    ) -> Result<usize, ClientError> {
        let keyboard_id = self
            .keyboard_id
            .ok_or(ClientError::UnknownObject { id: ObjectId::NULL })?;
        let event = KeyboardKey {
            surface_id,
            key,
            state,
        };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        self.emit_raw(keyboard_id, 1 /* key */, &payload)
    }

    /// Drain the pending-events queue and return one
    /// flattened `Vec<u8>` containing every enqueued
    /// message back-to-back. The client's queue is empty
    /// after this call.
    ///
    /// Returns an empty `Vec<u8>` if no events are
    /// queued.
    pub fn drain_pending_events(&mut self) -> Vec<u8> {
        self.drain_pending_events_bounded(usize::MAX)
    }

    /// Drain a prefix of complete events whose aggregate encoded size does not
    /// exceed `max_bytes`. Framing is never split, and the undrained suffix
    /// retains its exact order for a later transport turn.
    pub fn drain_pending_events_bounded(&mut self, max_bytes: usize) -> Vec<u8> {
        let mut drained_bytes = 0usize;
        let mut drained_events = 0usize;
        for message in &self.pending_events {
            let next = drained_bytes.saturating_add(message.len());
            if next > max_bytes {
                break;
            }
            drained_bytes = next;
            drained_events += 1;
        }
        let mut out = Vec::with_capacity(drained_bytes);
        for msg in self.pending_events.drain(..drained_events) {
            out.extend_from_slice(&msg);
        }
        debug_assert_eq!(out.len(), drained_bytes);
        self.pending_event_bytes = self.pending_event_bytes.saturating_sub(drained_bytes);
        out
    }

    /// Number of events currently queued, without
    /// draining. Diagnostic.
    pub fn pending_events_len(&self) -> usize {
        self.pending_events.len()
    }

    /// Bytes currently retained in the protocol event queue.
    pub fn pending_event_bytes(&self) -> usize {
        self.pending_event_bytes
    }

    /// Encoded length of the next complete ordered event, if any. Transport
    /// scheduling uses this to distinguish immediately drainable local work
    /// from work blocked on peer socket capacity.
    pub fn next_pending_event_bytes(&self) -> Option<usize> {
        self.pending_events.first().map(Vec::len)
    }

    /// Once true, production must disconnect this client after the current
    /// dispatch pass. Draining does not clear the flag because overflow is a
    /// connection-fatal protocol condition, not transient backpressure.
    pub fn event_queue_overflowed(&self) -> bool {
        self.event_queue_overflowed
    }
}

fn coalesce_damage(rects: &mut Vec<DamageRect>, added: DamageRect) {
    let mut left = added.x.min(added.x.saturating_add(added.width));
    let mut top = added.y.min(added.y.saturating_add(added.height));
    let mut right = added.x.max(added.x.saturating_add(added.width));
    let mut bottom = added.y.max(added.y.saturating_add(added.height));
    for rect in rects.iter() {
        left = left.min(rect.x.min(rect.x.saturating_add(rect.width)));
        top = top.min(rect.y.min(rect.y.saturating_add(rect.height)));
        right = right.max(rect.x.max(rect.x.saturating_add(rect.width)));
        bottom = bottom.max(rect.y.max(rect.y.saturating_add(rect.height)));
    }
    rects.clear();
    rects.push(DamageRect {
        x: left,
        y: top,
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
    });
}
