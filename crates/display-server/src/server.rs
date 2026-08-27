//! Top-level display server state.
//!
//! [`Server`] owns a map of connected clients keyed by
//! [`ClientId`]. It is the only place in the library that
//! knows about the ensemble of connections; per-connection
//! state lives in [`crate::client::Client`].
//!
//! The server is transport-agnostic: it takes byte buffers on
//! one side and yields byte buffers on the other. The
//! production binary in `src/main.rs` is the integration
//! point that opens `/run/display`, runs
//! `ipc_recv` / `ipc_send` loops, and feeds the byte streams
//! into `Server::dispatch_request` / consumes
//! `Server::take_pending_events`.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;

use crate::client::{
    Client, ClientError, ClientId, ClientLimits, DamageRect as SurfaceDamageRect,
    FrameCallbackCompletion, ServerResourceBudgets,
};
use crate::compositor::{Framebuffer, DEFAULT_HEIGHT, DEFAULT_WIDTH};
use crate::protocol::keymap::{load_bundled, load_default, Keymap, KeymapError, Scancode};
use display_proto::events::key_state;
use display_proto::events::{shell_restore_status, shell_window_state_flags, ShellWindowState};
use display_proto::ids::ObjectId;
use display_proto::objects::Interface;
use display_proto::requests::{
    shell_restore_window_flags, ShellManagerBeginRestore, ShellManagerEndRestore,
    ShellManagerPlaceRestoredWindow, SurfacePatchCurrent, MAX_SHELL_RESTORE_TIMEOUT_MS,
};
use display_proto::wire::{MessageHeader, WireError, HEADER_SIZE};
use preferences::KeyboardLayout;

/// Maximum simultaneously accepted display-protocol connections. This bounds
/// every per-connection queue and object table even when one process opens
/// many sockets.
pub const MAX_SERVER_CLIENTS: usize = 64;
/// At most the active and one replacement desktop shell may overlap. The
/// transport reserves a write attempt for each authenticated shell every turn.
pub const MAX_SERVER_SHELL_CLIENTS: usize = 2;
/// Ordinary clients cannot consume the slots reserved for an active and a
/// replacement desktop shell.
pub const MAX_SERVER_ORDINARY_CLIENTS: usize = MAX_SERVER_CLIENTS - MAX_SERVER_SHELL_CLIENTS;

/// Maximum aggregate shm-pool backing retained by the display server. A normal
/// full-output double buffer is about 6 MiB, so 128 MiB leaves room for roughly
/// twenty such windows without allowing one tab to grow toward host OOM.
pub const MAX_SERVER_POOL_BYTES: u64 = 128 * 1024 * 1024;

/// Maximum live toplevel roles across every connection. Besides bounding the
/// global window maps, this keeps a replacement-shell catch-up snapshot within
/// one client's ordered event queue.
pub const MAX_SERVER_TOPLEVELS: usize = 512;

/// Maximum title/app-id UTF-8 bytes retained across all connections.
pub const MAX_SERVER_TOPLEVEL_METADATA_BYTES: u64 = 64 * 1024;

/// Maximum one-shot frame callbacks converted into ordered `done`/`delete_id`
/// pairs in one production outer turn. With at most 64 clients, the fair
/// one-per-client passes prevent a single callback-heavy client from starving
/// another and cap new lifecycle traffic at 128 events per turn.
pub const MAX_FRAME_CALLBACK_COMPLETIONS_PER_TURN: usize = 64;

/// Lower bound for a shell-requested restore timeout. Zero is not an infinite
/// transaction; it is clamped to one millisecond and therefore fails open.
pub const MIN_SHELL_RESTORE_TIMEOUT_MS: u32 = 1;

/// Exact backing required by the shipped shell's full-output double buffer.
pub const SHELL_FULL_OUTPUT_POOL_BYTES: u64 = DEFAULT_WIDTH as u64 * DEFAULT_HEIGHT as u64 * 4 * 2;
/// Each authenticated shell connection is guaranteed one toplevel and its
/// maximum wire-representable title/app-id metadata.
pub const SHELL_TOPLEVELS_RESERVED_PER_CLIENT: usize = 1;
pub const SHELL_METADATA_BYTES_RESERVED_PER_CLIENT: u64 =
    crate::client::MAX_TOPLEVEL_METADATA_BYTES;
const _: () = assert!(
    MAX_SERVER_POOL_BYTES >= SHELL_FULL_OUTPUT_POOL_BYTES * MAX_SERVER_SHELL_CLIENTS as u64
);
const _: () =
    assert!(MAX_SERVER_TOPLEVELS >= SHELL_TOPLEVELS_RESERVED_PER_CLIENT * MAX_SERVER_SHELL_CLIENTS);
const _: () = assert!(
    MAX_SERVER_TOPLEVEL_METADATA_BYTES
        >= SHELL_METADATA_BYTES_RESERVED_PER_CLIENT * MAX_SERVER_SHELL_CLIENTS as u64
);

/// Worst-case encoded replacement-shell catch-up snapshot: one
/// `window_created` per toplevel plus the single focused-window event. Each
/// created event has a 10-byte header, a 4-byte id, two 4-byte string lengths,
/// and at most six bytes of string padding in addition to retained metadata.
pub const MAX_SERVER_WINDOW_SNAPSHOT_BYTES: usize =
    MAX_SERVER_TOPLEVEL_METADATA_BYTES as usize + MAX_SERVER_TOPLEVELS * 80 + 28;
const _: () =
    assert!(MAX_SERVER_WINDOW_SNAPSHOT_BYTES <= crate::client::MAX_PENDING_EVENT_BYTES - 64 * 1024);
const _: () = assert!(MAX_SERVER_TOPLEVELS + 2 <= crate::client::MAX_PENDING_EVENTS);

/// Server-wide resource ceilings. Tests inject smaller limits to exercise
/// exact N/N+1 boundaries without large allocations.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ServerLimits {
    pub clients: usize,
    pub shell_clients: usize,
    pub pool_bytes: u64,
    pub toplevels: usize,
    pub client_toplevel_metadata_bytes: u64,
    pub toplevel_metadata_bytes: u64,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            clients: MAX_SERVER_CLIENTS,
            shell_clients: MAX_SERVER_SHELL_CLIENTS,
            pool_bytes: MAX_SERVER_POOL_BYTES,
            toplevels: MAX_SERVER_TOPLEVELS,
            client_toplevel_metadata_bytes: crate::client::MAX_CLIENT_TOPLEVEL_METADATA_BYTES,
            toplevel_metadata_bytes: MAX_SERVER_TOPLEVEL_METADATA_BYTES,
        }
    }
}

/// Errors surfaced by [`Server`] operations. Most of them are
/// thin wrappers over [`ClientError`] or [`WireError`] so the
/// caller has a single `?`-friendly error type.
///
/// Not `Copy` because [`ClientError::UnknownInterfaceName`]
/// carries an owned `String`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerError {
    /// Client ID is not in the server's table.
    NoSuchClient { id: ClientId },
    /// Wire-format error from [`MessageHeader::decode`].
    Wire(WireError),
    /// Client-state error from request dispatch.
    Client(ClientError),
    /// Accepting another connection would exceed either the server-wide cap
    /// or the ordinary-client share after shell slots are reserved.
    ClientLimitExceeded { attempted: usize, limit: usize },
    /// Accepting another Cap::Shell connection would exhaust the reserved
    /// per-turn shell transport service.
    ShellClientLimitExceeded { attempted: usize, limit: usize },
}

impl From<WireError> for ServerError {
    fn from(e: WireError) -> Self {
        ServerError::Wire(e)
    }
}
impl From<ClientError> for ServerError {
    fn from(e: ClientError) -> Self {
        ServerError::Client(e)
    }
}

/// Target of a hit-test: which client + surface is at a
/// given screen-space point. Returned by
/// [`Server::hit_test`] and used by the input-routing
/// path to decide which event object should receive an
/// injected event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HitResult {
    pub client_id: ClientId,
    pub surface_id: ObjectId,
    /// The point converted to surface-local coordinates
    /// (i.e. `(screen_x - toplevel.x, screen_y -
    /// toplevel.y)`).
    pub local_x: i32,
    pub local_y: i32,
}

/// Server-global identity for one live toplevel. Unlike
/// [`ObjectId`], this namespace is shared by every client and is
/// therefore safe to expose through `pmd_shell_manager`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub u32);

/// One conservative output-space rectangle that can contain pixels changed by
/// the next composed scene. Rectangles are clipped to the framebuffer before
/// they enter the presentation hint.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OutputDamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Server-owned bound for comparing the composed framebuffer with its last
/// presented shadow. `Full` is the correctness fallback for any mutation the
/// server cannot prove local; `Bounded` contains only validated output-space
/// candidates and may be empty for a proven invisible mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresentationDamage {
    Full,
    Bounded(Vec<OutputDamageRect>),
}

const MAX_PRESENTATION_DAMAGE_RECTS: usize = 8;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct WindowOwner {
    client_id: ClientId,
    toplevel_id: ObjectId,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct SurfaceOutputGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Clone, Debug)]
struct SurfaceCommitDamageCandidate {
    damage: PresentationDamage,
    expected_geometry: Option<SurfaceOutputGeometry>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct RestorePlacement {
    z_rank: u32,
    expected_width: u32,
    expected_height: u32,
    commit_count_at_place: u32,
    settled: bool,
}

/// One bounded shell-owned restore transaction. Windows in `hidden` remain
/// fully protocol-dispatchable but are excluded from composition, hit testing,
/// and automatic first-map focus until completion or fail-open abort.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RestoreTransaction {
    owner: ClientId,
    restore_id: u32,
    deadline_ms: u64,
    hidden: BTreeSet<WindowId>,
    placements: BTreeMap<WindowId, RestorePlacement>,
}

/// What kind of interactive drag is in progress, when one
/// is. `Move` is "follow the pointer with the toplevel's
/// origin"; `Resize { edges }` is "follow the pointer by
/// expanding/contracting along the named edges".
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DragKind {
    Move,
    Resize { edges: u32 },
}

/// State captured when a `pmd_xdg_toplevel.move` /
/// `.resize` request is dispatched. The server consults
/// this on every subsequent pointer-motion event to update
/// the toplevel; the drag ends on the next pointer-button
/// release event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DragState {
    pub client_id: ClientId,
    pub toplevel_id: ObjectId,
    pub kind: DragKind,
    /// Pointer position when the drag started, in screen
    /// space. Used to compute the pointer delta on each
    /// motion event so the toplevel tracks the cursor 1:1.
    pub start_pointer: (i32, i32),
    /// Toplevel origin when the drag started, in screen
    /// space. Move-drag updates `Toplevel.x/y` to
    /// `start_origin + (current_pointer - start_pointer)`.
    pub start_origin: (i32, i32),
}

/// The display server.
pub struct Server {
    next_client_id: u32,
    clients: BTreeMap<ClientId, Client>,
    limits: ServerLimits,
    pool_bytes: u64,
    toplevel_metadata_bytes: u64,
    /// Monotonic global namespace exported to shell clients. Zero is
    /// reserved as "no window" and IDs are never reused within a
    /// server lifetime.
    next_window_id: u32,
    windows: BTreeMap<WindowId, WindowOwner>,
    window_ids: BTreeMap<(ClientId, ObjectId), WindowId>,
    /// Next non-zero toplevel ordinal for each authenticated peer PID. This is
    /// server-wide rather than connection-local because one process may open
    /// multiple display sockets; `(owner_pid, ordinal)` must remain unique.
    next_toplevel_ordinal_by_pid: BTreeMap<u32, u32>,
    /// Explicit bottom-to-top stack of live global window IDs. Authenticated
    /// shell toplevels enter at the bottom so a replacement wallpaper cannot
    /// cover surviving applications; ordinary creation appends at the top.
    /// Focusing or clicking any window moves its ID to the end.
    z_order: Vec<WindowId>,
    /// The composed output framebuffer that every committed
    /// surface blits into. v1 is a single global output at
    /// [`DEFAULT_WIDTH`] × [`DEFAULT_HEIGHT`]; real
    /// multi-output support lands with the kernel-side
    /// `Fb` driver.
    framebuffer: Framebuffer,
    /// Incremented after every full-scene composition pass so the
    /// transport loop can present state changes that did not come
    /// from `surface.commit` (move, minimize, close, disconnect).
    scene_generation: u64,
    /// Incremented whenever logical state requests a scene rebuild, including
    /// while composition is deferred. The transport uses this to stop after
    /// one scene-mutating protocol request without conflating requested work
    /// with a completed framebuffer pass.
    recomposition_serial: u64,
    /// The transport batches every logical mutation handled in one outer
    /// event-loop turn into at most one full-scene rebuild. Direct library
    /// callers retain immediate composition unless they explicitly open a
    /// batch.
    recomposition_deferred: bool,
    recomposition_pending: bool,
    /// Conservative output-space candidates accumulated with every logical
    /// scene mutation since the last successful presentation.
    presentation_damage: PresentationDamage,
    /// Current pointer position in screen space. Updated
    /// by [`Server::inject_pointer_motion`]; consulted by
    /// [`Server::inject_pointer_button`] when the click
    /// needs to be routed to whatever surface the pointer
    /// is currently over.
    pointer_x: i32,
    pointer_y: i32,
    /// Monotonic serial copied into each routed pointer-button event. Clients
    /// echo a press serial in interactive move/resize requests.
    next_pointer_serial: u32,
    /// Currently-focused client + surface for keyboard
    /// input. Set by
    /// [`Server::inject_pointer_button`] on a press
    /// (click-to-focus); cleared when the focused window
    /// is destroyed.
    keyboard_focus: Option<(ClientId, ObjectId)>,
    /// Persisted layout currently applied to physical keyboard input.
    keyboard_layout: KeyboardLayout,
    /// Physical-scancode to codepoint map selected by Settings.
    active_keymap: Keymap,
    /// Stable v1 client-facing logical HID map. Existing applications consume
    /// logical scancodes through their US-HID tables, so the server maps the
    /// selected layout back into this namespace before emitting events.
    logical_keymap: Keymap,
    /// Bit 0 = left shift held, bit 1 = right shift held. Tracking both avoids
    /// clearing Shift when only one of two simultaneously-held keys releases.
    shift_mask: u8,
    /// Bit 0 = left Alt held, bit 1 = right Alt held. This is physical input
    /// state used only to recognize the desktop Alt+F4 gesture before layout
    /// mapping; ordinary modifier transitions still route to the focused app.
    alt_mask: u8,
    /// Physical F4 state and whether its current press was consumed as a
    /// close shortcut. The latch suppresses browser key-repeat duplicates and
    /// the matching release, so applications never receive a release without
    /// the consumed press.
    f4_down: bool,
    f4_close_shortcut_consumed: bool,
    /// Pixels reserved at the bottom of the framebuffer for
    /// the desktop shell's taskbar. `set_maximized`
    /// configures use the framebuffer height MINUS this
    /// value as the work-area height. Defaults to 0;
    /// updated by [`Server::set_taskbar_height_px`].
    taskbar_height_px: u32,
    /// Shell client that most recently reserved the bottom work-area strip.
    /// Disconnect clears only the reservation owned by that client, so a
    /// replacement shell can claim the strip before the old shell exits.
    work_area_owner: Option<ClientId>,
    /// A press routed through the shell-owned work-area reservation. The next
    /// window-control request from that shell completes this command rather
    /// than starting an independent click-to-focus operation.
    pending_shell_command_owner: Option<ClientId>,
    /// A reserved-strip focus changed logical z-order, but presentation is
    /// held until the initiating shell commits its matching taskbar or menu
    /// pixels. Target raise and shell feedback then become one transaction.
    deferred_focus_commit_owner: Option<ClientId>,
    /// Next generic framebuffer presentation-fence serial. Zero is reserved
    /// so browser-side latches can use it as an uninitialised sentinel.
    next_present_fence_serial: u32,
    /// Shell connections that have already issued their one-shot
    /// `pmd_shell_manager.desktop_ready` request.
    present_fence_clients: BTreeSet<ClientId>,
    /// Authenticated desktop-ready requests awaiting a fully settled
    /// presentation boundary in the production transport.
    pending_present_fences: VecDeque<(ClientId, u32)>,
    /// Client-id cursor used to rotate the first callback lifecycle admission
    /// attempt across bounded outer turns.
    frame_callback_client_cursor: u32,
    /// Transport-supplied monotonic time. Native isolation tests advance this
    /// explicitly; the production loop updates it before dispatch and after a
    /// restore-deadline wakeup.
    monotonic_ms: u64,
    restore_transaction: Option<RestoreTransaction>,
    /// Active interactive drag, if any. Set by
    /// `pmd_xdg_toplevel.move` / `.resize` request dispatch;
    /// consulted by `inject_pointer_motion`; cleared by
    /// `inject_pointer_button` on release.
    active_drag: Option<DragState>,
    /// Cross-client auto-layout counter. Each new ordinary application
    /// toplevel is positioned at
    /// (counter, counter) and the counter advances by
    /// AUTO_LAYOUT_STEP. Without this, every client's first
    /// toplevel would land at (0, 0) and stack invisibly
    /// — so spawning two `hello-toplevel` instances looks
    /// like nothing happened on the second click. Capability-authenticated
    /// desktop-shell surfaces stay at the output origin and do not consume a
    /// cascade slot.
    next_toplevel_offset: i32,
}

impl Server {
    pub fn new() -> Self {
        Server::with_framebuffer_size(DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }

    /// Build a server whose composed framebuffer is
    /// `width × height`. Tests use this to keep the
    /// allocated pixel buffer small and to produce
    /// deterministic clipping boundaries.
    pub fn with_framebuffer_size(width: u32, height: u32) -> Self {
        Self::with_limits(width, height, ServerLimits::default())
    }

    /// Build a server with injectable global resource ceilings.
    pub fn with_limits(width: u32, height: u32, limits: ServerLimits) -> Self {
        let active_keymap = load_default();
        let logical_keymap = load_default();
        Server {
            next_client_id: 1,
            clients: BTreeMap::new(),
            limits,
            pool_bytes: 0,
            toplevel_metadata_bytes: 0,
            next_window_id: 1,
            windows: BTreeMap::new(),
            window_ids: BTreeMap::new(),
            next_toplevel_ordinal_by_pid: BTreeMap::new(),
            z_order: Vec::new(),
            framebuffer: Framebuffer::new(width, height),
            scene_generation: 0,
            recomposition_serial: 0,
            recomposition_deferred: false,
            recomposition_pending: false,
            presentation_damage: PresentationDamage::Bounded(Vec::new()),
            pointer_x: 0,
            pointer_y: 0,
            next_pointer_serial: 1,
            keyboard_focus: None,
            keyboard_layout: KeyboardLayout::default(),
            active_keymap,
            logical_keymap,
            shift_mask: 0,
            alt_mask: 0,
            f4_down: false,
            f4_close_shortcut_consumed: false,
            taskbar_height_px: 0,
            work_area_owner: None,
            pending_shell_command_owner: None,
            deferred_focus_commit_owner: None,
            next_present_fence_serial: 1,
            present_fence_clients: BTreeSet::new(),
            pending_present_fences: VecDeque::new(),
            frame_callback_client_cursor: 1,
            monotonic_ms: 0,
            restore_transaction: None,
            active_drag: None,
            next_toplevel_offset: 0,
        }
    }

    /// Borrow the composed output framebuffer. Every
    /// committed surface's current buffer has been blitted
    /// into this; a future `Fb` driver bridge will read
    /// the pixels here on each frame tick.
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }

    /// Mutable access to the framebuffer. Useful in tests
    /// that want to pre-fill with a distinctive background
    /// colour before asserting a blit landed in the
    /// expected rectangle.
    pub fn framebuffer_mut(&mut self) -> &mut Framebuffer {
        &mut self.framebuffer
    }

    /// Monotonic change counter for the composed scene.
    pub fn scene_generation(&self) -> u64 {
        self.scene_generation
    }

    /// Monotonic count of logical requests for a complete-scene rebuild.
    pub fn recomposition_serial(&self) -> u64 {
        self.recomposition_serial
    }

    /// Conservative candidates for comparing the current composed output to
    /// the last successfully presented shadow. The production transport clears
    /// this only after its complete framebuffer command sequence succeeds.
    pub fn presentation_damage(&self) -> &PresentationDamage {
        &self.presentation_damage
    }

    /// Acknowledge that the current composed output crossed a successful
    /// presentation boundary. Later mutations start a fresh bounded set.
    pub fn clear_presentation_damage(&mut self) {
        self.presentation_damage = PresentationDamage::Bounded(Vec::new());
    }

    /// Defer full-scene rebuilds until [`Server::finish_recomposition_batch`].
    /// The display transport opens one batch per input-first outer turn so a
    /// burst of motion, protocol commits, or disconnects cannot trigger an
    /// unbounded number of complete framebuffer traversals.
    pub fn begin_recomposition_batch(&mut self) {
        debug_assert!(!self.recomposition_deferred);
        self.recomposition_deferred = true;
    }

    /// Close the current batch and materialize all accumulated scene changes
    /// in one composition pass. Returns whether a pass ran.
    pub fn finish_recomposition_batch(&mut self) -> bool {
        debug_assert!(self.recomposition_deferred);
        self.recomposition_deferred = false;
        if !core::mem::take(&mut self.recomposition_pending) {
            return false;
        }
        self.recomposite_scene();
        true
    }

    fn request_recomposition(&mut self) {
        self.request_recomposition_with_damage(PresentationDamage::Full);
    }

    fn request_recomposition_with_damage(&mut self, damage: PresentationDamage) {
        self.accumulate_presentation_damage(damage);
        self.recomposition_serial = self.recomposition_serial.wrapping_add(1);
        if self.recomposition_deferred {
            self.recomposition_pending = true;
        } else {
            self.recomposite_scene();
        }
    }

    fn accumulate_presentation_damage(&mut self, damage: PresentationDamage) {
        if matches!(self.presentation_damage, PresentationDamage::Full) {
            return;
        }
        let PresentationDamage::Bounded(mut added) = damage else {
            self.presentation_damage = PresentationDamage::Full;
            return;
        };
        let PresentationDamage::Bounded(current) = &mut self.presentation_damage else {
            unreachable!();
        };
        for rect in added.drain(..) {
            Self::merge_output_damage_rect(current, rect);
        }
    }

    fn merge_output_damage_rect(rects: &mut Vec<OutputDamageRect>, mut added: OutputDamageRect) {
        if added.width == 0 || added.height == 0 {
            return;
        }
        let mut index = 0;
        while index < rects.len() {
            if Self::output_rects_overlap_or_touch(rects[index], added) {
                added = Self::union_output_rects(rects.swap_remove(index), added);
                index = 0;
            } else {
                index += 1;
            }
        }
        rects.push(added);
        if rects.len() > MAX_PRESENTATION_DAMAGE_RECTS {
            let mut bounds = rects[0];
            for rect in rects.iter().skip(1).copied() {
                bounds = Self::union_output_rects(bounds, rect);
            }
            rects.clear();
            rects.push(bounds);
        }
    }

    fn output_rects_overlap_or_touch(left: OutputDamageRect, right: OutputDamageRect) -> bool {
        let left_right = u64::from(left.x) + u64::from(left.width);
        let left_bottom = u64::from(left.y) + u64::from(left.height);
        let right_right = u64::from(right.x) + u64::from(right.width);
        let right_bottom = u64::from(right.y) + u64::from(right.height);
        u64::from(left.x) <= right_right
            && u64::from(right.x) <= left_right
            && u64::from(left.y) <= right_bottom
            && u64::from(right.y) <= left_bottom
    }

    fn union_output_rects(left: OutputDamageRect, right: OutputDamageRect) -> OutputDamageRect {
        let x = left.x.min(right.x);
        let y = left.y.min(right.y);
        let right_edge = (u64::from(left.x) + u64::from(left.width))
            .max(u64::from(right.x) + u64::from(right.width));
        let bottom_edge = (u64::from(left.y) + u64::from(left.height))
            .max(u64::from(right.y) + u64::from(right.height));
        OutputDamageRect {
            x,
            y,
            width: u32::try_from(right_edge - u64::from(x)).unwrap_or(u32::MAX),
            height: u32::try_from(bottom_edge - u64::from(y)).unwrap_or(u32::MAX),
        }
    }

    fn surface_output_geometry_for_attachment(
        &self,
        client_id: ClientId,
        surface_id: ObjectId,
        attachment: Option<crate::client::BufferAttachment>,
    ) -> Option<SurfaceOutputGeometry> {
        let client = self.clients.get(&client_id)?;
        let surface = client.surfaces.get(&surface_id)?;
        let attachment = attachment?;
        let buffer = client.buffers.get(&attachment.buffer_id)?;
        if let Some(toplevel_id) = client.toplevel_by_surface.get(&surface_id) {
            let toplevel = client.toplevels.get(toplevel_id)?;
            if toplevel.minimized || toplevel.hidden_for_restore {
                return None;
            }
            return Some(SurfaceOutputGeometry {
                x: toplevel.x.saturating_add(attachment.x),
                y: toplevel.y.saturating_add(attachment.y),
                width: buffer.width,
                height: buffer.height,
            });
        }
        if surface.had_toplevel_role {
            return None;
        }
        Some(SurfaceOutputGeometry {
            x: attachment.x,
            y: attachment.y,
            width: buffer.width,
            height: buffer.height,
        })
    }

    fn surface_output_geometry(
        &self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> Option<SurfaceOutputGeometry> {
        let attachment = self
            .clients
            .get(&client_id)?
            .surfaces
            .get(&surface_id)?
            .current_buffer;
        self.surface_output_geometry_for_attachment(client_id, surface_id, attachment)
    }

    fn clip_output_damage(
        &self,
        x: i64,
        y: i64,
        width: u32,
        height: u32,
    ) -> Option<OutputDamageRect> {
        let output_width = i64::from(self.framebuffer.width());
        let output_height = i64::from(self.framebuffer.height());
        let left = x.clamp(0, output_width);
        let top = y.clamp(0, output_height);
        let right = x.saturating_add(i64::from(width)).clamp(0, output_width);
        let bottom = y.saturating_add(i64::from(height)).clamp(0, output_height);
        if right <= left || bottom <= top {
            return None;
        }
        Some(OutputDamageRect {
            x: left as u32,
            y: top as u32,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
        })
    }

    fn validated_surface_local_damage(
        &self,
        geometry: SurfaceOutputGeometry,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<Option<OutputDamageRect>, ()> {
        let x = u32::try_from(x).map_err(|_| ())?;
        let y = u32::try_from(y).map_err(|_| ())?;
        if width == 0 || height == 0 {
            return Err(());
        }
        let right = x.checked_add(width).ok_or(())?;
        let bottom = y.checked_add(height).ok_or(())?;
        if right > geometry.width || bottom > geometry.height {
            return Err(());
        }
        Ok(self.clip_output_damage(
            i64::from(geometry.x) + i64::from(x),
            i64::from(geometry.y) + i64::from(y),
            width,
            height,
        ))
    }

    fn surface_commit_damage_candidate(
        &self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> SurfaceCommitDamageCandidate {
        let full = || SurfaceCommitDamageCandidate {
            damage: PresentationDamage::Full,
            expected_geometry: None,
        };
        let Some(client) = self.clients.get(&client_id) else {
            return full();
        };
        let Some(surface) = client.surfaces.get(&surface_id) else {
            return full();
        };
        if surface.pending_damage.is_empty() || surface.pending_damage_unprovable {
            return full();
        }
        let before = self.surface_output_geometry_for_attachment(
            client_id,
            surface_id,
            surface.current_buffer,
        );
        let next_attachment = if surface.pending_attach {
            surface.pending_buffer
        } else {
            surface.current_buffer
        };
        let expected =
            self.surface_output_geometry_for_attachment(client_id, surface_id, next_attachment);
        let Some(geometry) = before.filter(|before| Some(*before) == expected) else {
            return full();
        };
        let mut rects = Vec::new();
        for SurfaceDamageRect {
            x,
            y,
            width,
            height,
        } in surface.pending_damage.iter().copied()
        {
            let Ok(width) = u32::try_from(width) else {
                return full();
            };
            let Ok(height) = u32::try_from(height) else {
                return full();
            };
            match self.validated_surface_local_damage(geometry, x, y, width, height) {
                Ok(Some(rect)) => Self::merge_output_damage_rect(&mut rects, rect),
                Ok(None) => {}
                Err(()) => return full(),
            }
        }
        SurfaceCommitDamageCandidate {
            damage: PresentationDamage::Bounded(rects),
            expected_geometry: Some(geometry),
        }
    }

    fn surface_patch_presentation_damage(
        &self,
        client_id: ClientId,
        surface_id: ObjectId,
        patch: &SurfacePatchCurrent<'_>,
    ) -> PresentationDamage {
        let Some(geometry) = self.surface_output_geometry(client_id, surface_id) else {
            return PresentationDamage::Full;
        };
        match self.validated_surface_local_damage(
            geometry,
            patch.x,
            patch.y,
            patch.width,
            patch.height,
        ) {
            Ok(rect) => PresentationDamage::Bounded(rect.into_iter().collect()),
            Err(()) => PresentationDamage::Full,
        }
    }

    fn raised_window_presentation_damage(&self, window_id: WindowId) -> PresentationDamage {
        let Some(owner) = self.windows.get(&window_id) else {
            return PresentationDamage::Full;
        };
        let Some(surface_id) = self
            .clients
            .get(&owner.client_id)
            .and_then(|client| client.toplevels.get(&owner.toplevel_id))
            .map(|toplevel| toplevel.surface_id)
        else {
            return PresentationDamage::Full;
        };
        let Some(geometry) = self.surface_output_geometry(owner.client_id, surface_id) else {
            return PresentationDamage::Full;
        };
        // Reordering this validated mapped surface cannot affect pixels outside
        // its own footprint: every other surface retains the same relative
        // order there. Occlusion can only make this conservative rectangle
        // contain unchanged pixels.
        PresentationDamage::Bounded(
            self.clip_output_damage(
                i64::from(geometry.x),
                i64::from(geometry.y),
                geometry.width,
                geometry.height,
            )
            .into_iter()
            .collect(),
        )
    }

    /// Whether the transport must wait before publishing the current logical
    /// scene. A shell-owned focus transaction waits for its matching surface
    /// commit, and a deferred recomposition batch waits until its accumulated
    /// logical mutations have been materialized into the framebuffer.
    pub fn presentation_deferred(&self) -> bool {
        self.deferred_focus_commit_owner.is_some() || self.recomposition_pending
    }

    /// Number of authenticated shell readiness fences waiting for the
    /// transport to finish all earlier presentation work.
    pub fn pending_present_fence_count(&self) -> usize {
        self.pending_present_fences.len()
    }

    /// Qualify every commit-associated frame callback at one successfully
    /// completed framebuffer presentation boundary. Each client moves its
    /// complete awaiting FIFO into a timestamped batch in O(1), so a deferred
    /// presentation cannot turn into an unbounded one-turn callback walk.
    pub fn mark_frame_callbacks_presented(&mut self, callback_data: u32) {
        for client in self.clients.values_mut() {
            client.mark_frame_callbacks_presented(callback_data);
        }
    }

    /// Drain ready presentation and cancellation lifecycle work against one
    /// caller-owned outer-turn budget. Each pass attempts at most one item per
    /// client; capacity-blocked clients retain their oldest item while other
    /// clients progress. The rotating start preserves fairness across turns.
    pub fn drain_ready_frame_callback_lifecycle(&mut self, remaining: &mut usize) -> usize {
        if *remaining == 0 || self.clients.is_empty() {
            return 0;
        }
        let client_ids = self.clients.keys().copied().collect::<Vec<_>>();
        let mut start = client_ids
            .iter()
            .position(|id| id.0 >= self.frame_callback_client_cursor)
            .unwrap_or(0);
        let mut completed = 0usize;
        let mut last_completed = None;
        loop {
            let mut pass_progress = false;
            for offset in 0..client_ids.len() {
                if *remaining == 0 {
                    break;
                }
                let index = (start + offset) % client_ids.len();
                let client_id = client_ids[index];
                let client = self
                    .clients
                    .get_mut(&client_id)
                    .expect("callback client id came from live map");
                if client.try_complete_ready_frame_callback_lifecycle()
                    == FrameCallbackCompletion::Completed
                {
                    completed += 1;
                    *remaining -= 1;
                    pass_progress = true;
                    last_completed = Some(index);
                }
            }
            if !pass_progress || *remaining == 0 {
                break;
            }
            start = (start + 1) % client_ids.len();
        }
        if let Some(index) = last_completed {
            self.frame_callback_client_cursor = client_ids[(index + 1) % client_ids.len()].0;
        }
        completed
    }

    /// Presentation-site spelling for the shared lifecycle drain. The caller
    /// first invokes [`Self::mark_frame_callbacks_presented`] and passes the
    /// same outer-turn budget used by cancellation drains.
    pub fn complete_presented_frame_callbacks(&mut self, remaining: &mut usize) -> usize {
        self.drain_ready_frame_callback_lifecycle(remaining)
    }

    pub fn presented_frame_callback_count(&self) -> usize {
        self.clients
            .values()
            .map(Client::presented_frame_callback_count)
            .sum()
    }

    pub fn cancelled_frame_callback_lifecycle_count(&self) -> usize {
        self.clients
            .values()
            .map(Client::cancelled_frame_callback_lifecycle_count)
            .sum()
    }

    pub fn has_ready_frame_callback_lifecycle(&self) -> bool {
        self.clients
            .values()
            .any(Client::has_ready_frame_callback_lifecycle)
    }

    /// True only when a bounded completion turn can enqueue at least one exact
    /// lifecycle pair immediately. A capacity-blocked callback relies on the
    /// existing event/outbound FD_WRITE readiness instead of forcing a spin.
    pub fn ready_frame_callback_lifecycle_can_progress(&self) -> bool {
        self.clients
            .values()
            .any(Client::ready_frame_callback_lifecycle_can_progress)
    }

    /// Remove the oldest presentation-fence serial once the transport has
    /// proved that no earlier scene/frame work remains. The serial is generic
    /// framebuffer ordering metadata; desktop policy stays in the privileged
    /// shell-manager request that created it.
    pub fn take_pending_present_fence(&mut self) -> Option<u32> {
        while let Some((client_id, serial)) = self.pending_present_fences.pop_front() {
            if self
                .clients
                .get(&client_id)
                .is_some_and(|client| client.capabilities.contains(abi::cap::Cap::Shell))
            {
                return Some(serial);
            }
        }
        None
    }

    /// Advance the server's transport-independent monotonic clock and fail an
    /// expired restore transaction open. Returns true when expiry revealed or
    /// otherwise mutated scene state.
    pub fn advance_monotonic_time(&mut self, now_ms: u64) -> bool {
        self.monotonic_ms = self.monotonic_ms.max(now_ms);
        let expired = self
            .restore_transaction
            .as_ref()
            .is_some_and(|restore| restore.deadline_ms <= self.monotonic_ms);
        if expired {
            self.abort_restore(shell_restore_status::TIMED_OUT, true)
        } else {
            false
        }
    }

    /// Remaining time until the active restore must fail open. The production
    /// transport uses this as its exact `poll_oneoff` clock deadline, avoiding
    /// timers, periodic wakes, and idle spin.
    pub fn restore_poll_timeout_ms(&self) -> Option<u64> {
        self.restore_transaction
            .as_ref()
            .map(|restore| restore.deadline_ms.saturating_sub(self.monotonic_ms))
    }

    pub fn restore_transaction_owner(&self) -> Option<ClientId> {
        self.restore_transaction
            .as_ref()
            .map(|restore| restore.owner)
    }

    fn begin_restore(&mut self, client_id: ClientId, request: ShellManagerBeginRestore) -> bool {
        let authenticated_shell = self.clients.get(&client_id).is_some_and(|client| {
            client.capabilities.contains(abi::cap::Cap::Shell) && client.shell_manager_id.is_some()
        });
        if !authenticated_shell || request.restore_id == 0 {
            return false;
        }
        if self.restore_transaction.is_some() {
            if let Some(client) = self.clients.get_mut(&client_id) {
                if let Some(shell_manager_id) = client.shell_manager_id {
                    let _ = client.emit_restore_finished(
                        shell_manager_id,
                        request.restore_id,
                        shell_restore_status::BUSY,
                        0,
                    );
                }
            }
            return false;
        }
        let timeout_ms = request
            .timeout_ms
            .clamp(MIN_SHELL_RESTORE_TIMEOUT_MS, MAX_SHELL_RESTORE_TIMEOUT_MS);
        self.restore_transaction = Some(RestoreTransaction {
            owner: client_id,
            restore_id: request.restore_id,
            deadline_ms: self.monotonic_ms.saturating_add(u64::from(timeout_ms)),
            hidden: BTreeSet::new(),
            placements: BTreeMap::new(),
        });
        true
    }

    fn place_restored_window(
        &mut self,
        client_id: ClientId,
        request: ShellManagerPlaceRestoredWindow,
    ) -> bool {
        let window_id = WindowId(request.window_id);
        let valid_transaction = self.restore_transaction.as_ref().is_some_and(|restore| {
            restore.owner == client_id
                && restore.restore_id == request.restore_id
                && restore.hidden.contains(&window_id)
        });
        if !valid_transaction
            || request.restore_id == 0
            || request.normal_width == 0
            || request.normal_height == 0
            || request.flags & !shell_restore_window_flags::ALL != 0
        {
            return false;
        }
        let Some(owner) = self.windows.get(&window_id).copied() else {
            return false;
        };
        if self
            .clients
            .get(&owner.client_id)
            .is_none_or(|client| client.capabilities.contains(abi::cap::Cap::Shell))
        {
            return false;
        }

        let work_width = self.work_area_width().max(1);
        let work_height = self.work_area_height().max(1);
        let normal_width = request.normal_width.min(work_width).max(1);
        let normal_height = request.normal_height.min(work_height).max(1);
        let max_x = work_width.saturating_sub(normal_width) as i32;
        let max_y = work_height.saturating_sub(normal_height) as i32;
        let normal_x = request.normal_x.clamp(0, max_x);
        let normal_y = request.normal_y.clamp(0, max_y);
        let minimized = request.flags & shell_restore_window_flags::MINIMIZED != 0;
        let maximized = request.flags & shell_restore_window_flags::MAXIMIZED != 0;
        let expected_dimensions = if maximized {
            (work_width, work_height)
        } else {
            (normal_width, normal_height)
        };
        let Some((commit_count_at_place, current_dimensions)) =
            self.clients.get(&owner.client_id).and_then(|client| {
                let toplevel = client.toplevels.get(&owner.toplevel_id)?;
                let surface = client.surfaces.get(&toplevel.surface_id)?;
                let dimensions = surface.current_buffer.and_then(|attachment| {
                    client
                        .buffers
                        .get(&attachment.buffer_id)
                        .map(|buffer| (buffer.width, buffer.height))
                });
                Some((surface.commit_count, dimensions))
            })
        else {
            return false;
        };
        {
            let Some(client) = self.clients.get_mut(&owner.client_id) else {
                return false;
            };
            let Some(toplevel) = client.toplevels.get_mut(&owner.toplevel_id) else {
                return false;
            };
            toplevel.normal_x = normal_x;
            toplevel.normal_y = normal_y;
            toplevel.normal_width = normal_width;
            toplevel.normal_height = normal_height;
            toplevel.minimized = minimized;
            // Restore always establishes the saved normal baseline first.
            // Defer the logical maximize transition until that configure has
            // been emitted so the baseline cannot inherit MAXIMIZED.
            toplevel.maximized = false;
            toplevel.initial_configure_sent = true;
            toplevel.restore_origin = None;
            toplevel.x = normal_x;
            toplevel.y = normal_y;
        }
        self.emit_toplevel_configure(
            owner.client_id,
            owner.toplevel_id,
            normal_width as i32,
            normal_height as i32,
        );
        if maximized {
            let Some(toplevel) = self
                .clients
                .get_mut(&owner.client_id)
                .and_then(|client| client.toplevels.get_mut(&owner.toplevel_id))
            else {
                return false;
            };
            toplevel.maximized = true;
            toplevel.restore_origin = Some((normal_x, normal_y));
            toplevel.x = 0;
            toplevel.y = 0;
            self.emit_toplevel_configure(
                owner.client_id,
                owner.toplevel_id,
                work_width as i32,
                work_height as i32,
            );
        }
        if let Some(restore) = self.restore_transaction.as_mut() {
            restore.placements.insert(
                window_id,
                RestorePlacement {
                    z_rank: request.z_rank,
                    expected_width: expected_dimensions.0,
                    expected_height: expected_dimensions.1,
                    commit_count_at_place,
                    settled: current_dimensions == Some(expected_dimensions),
                },
            );
        }
        self.broadcast_window_state_changed(window_id);
        true
    }

    fn end_restore(&mut self, client_id: ClientId, request: ShellManagerEndRestore) -> bool {
        let matches = self.restore_transaction.as_ref().is_some_and(|restore| {
            restore.owner == client_id && restore.restore_id == request.restore_id
        });
        if !matches || request.restore_id == 0 {
            return false;
        }
        let placements_are_settled = self.restore_transaction.as_ref().is_some_and(|restore| {
            restore
                .placements
                .values()
                .all(|placement| placement.settled)
        });
        if !placements_are_settled {
            return false;
        }
        let ranks_are_contiguous = self.restore_transaction.as_ref().is_some_and(|restore| {
            let mut ranks = restore
                .placements
                .values()
                .map(|placement| placement.z_rank)
                .collect::<Vec<_>>();
            ranks.sort_unstable();
            ranks
                .iter()
                .enumerate()
                .all(|(index, rank)| *rank == index as u32)
        });
        if !ranks_are_contiguous {
            self.abort_restore(shell_restore_status::ABORTED, true);
            return false;
        }
        let restore = self
            .restore_transaction
            .take()
            .expect("restore checked above");
        let ranks_before = self
            .z_order
            .iter()
            .enumerate()
            .map(|(rank, window_id)| (*window_id, rank))
            .collect::<BTreeMap<_, _>>();
        let focus_before = self
            .keyboard_focus
            .and_then(|(owner, surface)| self.window_id_for_surface(owner, surface));

        let mut ranked = restore
            .placements
            .iter()
            .map(|(window_id, placement)| (placement.z_rank, *window_id))
            .collect::<Vec<_>>();
        ranked.sort_unstable_by_key(|(rank, window_id)| (*rank, *window_id));
        self.z_order
            .retain(|window_id| !restore.placements.contains_key(window_id));
        self.z_order
            .extend(ranked.iter().map(|(_, window_id)| *window_id));

        for window_id in &restore.hidden {
            if let Some(owner) = self.windows.get(window_id).copied() {
                if let Some(toplevel) = self
                    .clients
                    .get_mut(&owner.client_id)
                    .and_then(|client| client.toplevels.get_mut(&owner.toplevel_id))
                {
                    toplevel.hidden_for_restore = false;
                }
            }
        }

        let requested_focus = (request.focus_window_id != 0)
            .then_some(WindowId(request.focus_window_id))
            .filter(|window_id| restore.placements.contains_key(window_id))
            .filter(|window_id| self.window_is_focusable(*window_id));
        let fallback_focus = self
            .focusable_window_for_client(restore.owner)
            .or_else(|| {
                self.z_order.iter().rev().copied().find(|window_id| {
                    restore.hidden.contains(window_id) && self.window_is_focusable(*window_id)
                })
            })
            .or_else(|| {
                self.z_order
                    .iter()
                    .rev()
                    .copied()
                    .find(|window_id| self.window_is_focusable(*window_id))
            });
        let focus = requested_focus.or(fallback_focus);
        let next_focus = focus.and_then(|window_id| {
            let owner = self.windows.get(&window_id)?;
            let surface_id = self
                .clients
                .get(&owner.client_id)?
                .toplevels
                .get(&owner.toplevel_id)?
                .surface_id;
            Some((owner.client_id, surface_id))
        });
        self.transition_keyboard_focus(next_focus);
        if let Some(window_id) = focus {
            self.move_window_to_top(window_id);
        }
        self.request_recomposition();
        let mut changed_windows = restore.hidden.clone();
        for (rank, window_id) in self.z_order.iter().copied().enumerate() {
            if ranks_before.get(&window_id).copied() != Some(rank) {
                changed_windows.insert(window_id);
            }
        }
        if let Some(window_id) = focus_before {
            changed_windows.insert(window_id);
        }
        if let Some(window_id) = focus {
            changed_windows.insert(window_id);
        }
        for window_id in changed_windows {
            self.broadcast_window_state_changed(window_id);
        }
        self.broadcast_window_focused(focus.map_or(0, |window_id| window_id.0));
        self.emit_restore_finished(
            restore.owner,
            restore.restore_id,
            shell_restore_status::COMPLETED,
            restore.placements.len() as u32,
        );
        true
    }

    fn abort_restore(&mut self, status: u32, notify_owner: bool) -> bool {
        let Some(restore) = self.restore_transaction.take() else {
            return false;
        };
        for window_id in &restore.hidden {
            if let Some(owner) = self.windows.get(window_id).copied() {
                if let Some(toplevel) = self
                    .clients
                    .get_mut(&owner.client_id)
                    .and_then(|client| client.toplevels.get_mut(&owner.toplevel_id))
                {
                    toplevel.hidden_for_restore = false;
                }
            }
        }
        if self.keyboard_focus.is_none() {
            if let Some(window_id) = self.focusable_window_for_client(restore.owner).or_else(|| {
                self.z_order
                    .iter()
                    .rev()
                    .copied()
                    .find(|window_id| self.window_is_focusable(*window_id))
            }) {
                let owner = self.windows.get(&window_id).copied();
                let next_focus = owner.and_then(|owner| {
                    let surface_id = self
                        .clients
                        .get(&owner.client_id)?
                        .toplevels
                        .get(&owner.toplevel_id)?
                        .surface_id;
                    Some((owner.client_id, surface_id))
                });
                self.transition_keyboard_focus(next_focus);
            }
        }
        self.request_recomposition();
        for window_id in &restore.hidden {
            self.broadcast_window_state_changed(*window_id);
        }
        let focused = self.keyboard_focus.and_then(|(owner, surface)| {
            self.window_id_for_surface(owner, surface)
                .map(|window_id| window_id.0)
        });
        self.broadcast_window_focused(focused.unwrap_or(0));
        if notify_owner {
            self.emit_restore_finished(
                restore.owner,
                restore.restore_id,
                status,
                restore.placements.len() as u32,
            );
        }
        true
    }

    fn emit_restore_finished(
        &mut self,
        owner: ClientId,
        restore_id: u32,
        status: u32,
        placed: u32,
    ) {
        if let Some(client) = self.clients.get_mut(&owner) {
            if let Some(shell_manager_id) = client.shell_manager_id {
                let _ = client.emit_restore_finished(shell_manager_id, restore_id, status, placed);
            }
        }
    }

    fn window_is_focusable(&self, window_id: WindowId) -> bool {
        let Some(owner) = self.windows.get(&window_id) else {
            return false;
        };
        let Some(client) = self.clients.get(&owner.client_id) else {
            return false;
        };
        let Some(toplevel) = client.toplevels.get(&owner.toplevel_id) else {
            return false;
        };
        !toplevel.minimized
            && !toplevel.hidden_for_restore
            && client
                .surfaces
                .get(&toplevel.surface_id)
                .is_some_and(|surface| surface.current_buffer.is_some())
    }

    fn focusable_window_for_client(&self, client_id: ClientId) -> Option<WindowId> {
        let focused = self
            .keyboard_focus
            .filter(|(owner, _)| *owner == client_id)
            .and_then(|(owner, surface)| self.window_id_for_surface(owner, surface))
            .filter(|window_id| self.window_is_focusable(*window_id));
        focused.or_else(|| {
            self.z_order.iter().rev().copied().find(|window_id| {
                self.windows
                    .get(window_id)
                    .is_some_and(|owner| owner.client_id == client_id)
                    && self.window_is_focusable(*window_id)
            })
        })
    }

    /// Current global window stack in bottom-to-top order.
    pub fn window_z_order(&self) -> &[WindowId] {
        &self.z_order
    }

    /// Current pointer position in screen space.
    pub fn pointer_position(&self) -> (i32, i32) {
        (self.pointer_x, self.pointer_y)
    }

    /// Currently-focused client + surface for keyboard
    /// input, if any.
    pub fn keyboard_focus(&self) -> Option<(ClientId, ObjectId)> {
        self.keyboard_focus
    }

    /// Keyboard layout currently applied to subsequently-routed key events.
    pub const fn keyboard_layout(&self) -> KeyboardLayout {
        self.keyboard_layout
    }

    /// Atomically replace the live keyboard map. The embedded bytes are fully
    /// parsed before any server state changes, preserving the prior map if a
    /// bundled asset is malformed.
    pub fn set_keyboard_layout(&mut self, layout: KeyboardLayout) -> Result<bool, KeymapError> {
        if layout == self.keyboard_layout {
            return Ok(false);
        }
        let keymap = load_bundled(layout)?;
        self.active_keymap = keymap;
        self.keyboard_layout = layout;
        Ok(true)
    }

    /// Hit-test a screen-space point against the toplevels
    /// in every connected client. Returns the topmost
    /// surface whose rectangle contains `(x, y)`, or
    /// `None` if the point doesn't land on any window.
    ///
    /// Z-order comes from the explicit server-global stack.
    /// The stack is walked top-to-bottom, so focus raises and
    /// cross-client creation order are reflected exactly.
    ///
    /// The hit rectangle is `(top.x, top.y, top.x + w,
    /// top.y + h)` where `(w, h)` comes from the
    /// surface's current buffer geometry. A surface that
    /// hasn't committed a buffer yet is invisible to the
    /// hit-test (no rectangle → no hit).
    pub fn hit_test(&self, x: i32, y: i32) -> Option<HitResult> {
        for window_id in self.z_order.iter().rev() {
            let Some(owner) = self.windows.get(window_id) else {
                continue;
            };
            let Some(client) = self.clients.get(&owner.client_id) else {
                continue;
            };
            let Some(toplevel) = client.toplevels.get(&owner.toplevel_id) else {
                continue;
            };
            if toplevel.minimized || toplevel.hidden_for_restore {
                continue;
            }
            let Some(surface) = client.surfaces.get(&toplevel.surface_id) else {
                continue;
            };
            let Some(attachment) = surface.current_buffer else {
                continue;
            };
            let Some(info) = client.buffers.get(&attachment.buffer_id) else {
                continue;
            };
            let rect_x = toplevel.x.saturating_add(attachment.x);
            let rect_y = toplevel.y.saturating_add(attachment.y);
            let rect_w = info.width as i32;
            let rect_h = info.height as i32;
            if x >= rect_x
                && x < rect_x.saturating_add(rect_w)
                && y >= rect_y
                && y < rect_y.saturating_add(rect_h)
            {
                return Some(HitResult {
                    client_id: owner.client_id,
                    surface_id: toplevel.surface_id,
                    local_x: x - rect_x,
                    local_y: y - rect_y,
                });
            }
        }
        None
    }

    /// Inject a pointer motion event in screen space.
    /// Updates the server's pointer position, hit-tests
    /// against the toplevels, and (if the hit client has
    /// a bound `pmd_pointer`) emits a `motion` event on
    /// its pointer object carrying surface-local
    /// coordinates. Returns the hit result, or `None`
    /// if the pointer didn't land on any window.
    pub fn inject_pointer_motion(&mut self, x: i32, y: i32) -> Option<HitResult> {
        self.pointer_x = x;
        self.pointer_y = y;
        // T133 server: if a drag is in progress, update the
        // toplevel + emit a configure for resize-drags. Move
        // drags just translate the origin — no configure
        // needed since the size doesn't change.
        if let Some(drag) = self.active_drag {
            self.advance_drag(drag, x, y);
            // During a drag we don't route motion events to
            // app surfaces; the server is exclusively
            // tracking the cursor for the drag.
            return None;
        }
        let hit = self.hit_test(x, y)?;
        let client = self.clients.get_mut(&hit.client_id)?;
        if client.pointer_id.is_some() {
            let _ = client.emit_pointer_motion(hit.surface_id, hit.local_x, hit.local_y);
        }
        Some(hit)
    }

    /// Advance an in-progress drag by translating /
    /// resizing the toplevel by the pointer delta. Called
    /// from `inject_pointer_motion`.
    fn advance_drag(&mut self, drag: DragState, pointer_x: i32, pointer_y: i32) {
        use display_proto::xdg_toplevel_resize_edge as edge;
        let dx = pointer_x - drag.start_pointer.0;
        let dy = pointer_y - drag.start_pointer.1;
        let mut resize_dimensions = None;
        let moved = {
            let Some(client) = self.clients.get_mut(&drag.client_id) else {
                return;
            };
            match drag.kind {
                DragKind::Move => {
                    if let Some(toplevel) = client.toplevels.get_mut(&drag.toplevel_id) {
                        let new_x = drag.start_origin.0.saturating_add(dx);
                        let new_y = drag.start_origin.1.saturating_add(dy);
                        let changed = (toplevel.x, toplevel.y) != (new_x, new_y);
                        toplevel.x = new_x;
                        toplevel.y = new_y;
                        changed
                    } else {
                        false
                    }
                }
                DragKind::Resize { edges } => {
                    // V1 resize semantics: the surface buffer's
                    // width/height come from the client side
                    // (client decides the buffer geometry on
                    // attach). The server emits a configure
                    // proposing the new size; the client
                    // respects it on its next attach.
                    let surface_id = match client.toplevels.get(&drag.toplevel_id) {
                        Some(t) => t.surface_id,
                        None => return,
                    };
                    let (start_w, start_h) = client
                        .surfaces
                        .get(&surface_id)
                        .and_then(|s| s.current_buffer)
                        .and_then(|attachment| client.buffers.get(&attachment.buffer_id))
                        .map(|info| (info.width as i32, info.height as i32))
                        .unwrap_or((320, 240));
                    let mut new_w = start_w;
                    let mut new_h = start_h;
                    if edges & edge::RIGHT != 0 {
                        new_w = (start_w + dx).max(1);
                    } else if edges & edge::LEFT != 0 {
                        new_w = (start_w - dx).max(1);
                    }
                    if edges & edge::BOTTOM != 0 {
                        new_h = (start_h + dy).max(1);
                    } else if edges & edge::TOP != 0 {
                        new_h = (start_h - dy).max(1);
                    }
                    resize_dimensions = Some((new_w, new_h));
                    false
                }
            }
        };
        if let Some((width, height)) = resize_dimensions {
            self.emit_toplevel_configure(drag.client_id, drag.toplevel_id, width, height);
        }
        if moved {
            self.request_recomposition();
        }
    }

    /// Inject a pointer button event at the current
    /// pointer position. Emits a `button` event on the
    /// target client's pointer object (if any), and on a
    /// press, sets keyboard focus to the hit surface. Presses in the
    /// shell-owned bottom work-area reservation are delivered without the
    /// generic focus/raise step: that strip contains shell commands whose
    /// handlers select the actual target window.
    /// Returns the hit result, or `None` if the click
    /// didn't land on any window.
    pub fn inject_pointer_button(&mut self, button: u32, state: u32) -> Option<HitResult> {
        // T133 server: a release during a drag terminates
        // the drag and emits a final configure (resize) with
        // the RESIZING bit cleared. Move drags don't need a
        // final configure since the size never changed.
        if state == display_proto::events::pointer_button_state::RELEASED {
            if let Some(drag) = self.active_drag.take() {
                if let DragKind::Resize { .. } = drag.kind {
                    self.emit_resize_final_configure(drag);
                }
                self.settle_drag_geometry(drag);
                if let Some(window_id) = self
                    .window_ids
                    .get(&(drag.client_id, drag.toplevel_id))
                    .copied()
                {
                    self.broadcast_window_state_changed(window_id);
                }
                return None;
            }
        }
        let hit = self.hit_test(self.pointer_x, self.pointer_y)?;
        let serial = self.next_pointer_serial;
        self.next_pointer_serial = self.next_pointer_serial.wrapping_add(1).max(1);
        let is_shell_command = self.work_area_owner == Some(hit.client_id)
            && self.pointer_y >= self.work_area_height() as i32;
        if state == display_proto::events::pointer_button_state::PRESSED {
            if is_shell_command {
                self.pending_shell_command_owner = Some(hit.client_id);
            } else {
                self.pending_shell_command_owner = None;
                if self.deferred_focus_commit_owner.take().is_some() {
                    // A new independent press supersedes an incomplete shell
                    // transaction. Materialize its logical z-order before
                    // routing the new click so a failed shell cannot withhold
                    // presentation indefinitely.
                    self.request_recomposition();
                }
            }
        }
        if state == display_proto::events::pointer_button_state::PRESSED && !is_shell_command {
            let prev_focus = self.keyboard_focus;
            let new_focus = (hit.client_id, hit.surface_id);
            self.transition_keyboard_focus(Some(new_focus));
            let focused_window_id = self.window_id_for_surface(hit.client_id, hit.surface_id);
            if focused_window_id
                .map(|window_id| self.raise_window(window_id))
                .unwrap_or(false)
            {
                self.request_recomposition();
            }
            // Only broadcast window_focused on a real focus
            // change. Re-clicking the already-focused window
            // re-emitted the same broadcast every time, which
            // forced every subscribed shell to repaint —
            // turning idle clicks into 3 MiB chunked
            // shm_pool.write traffic. Suppress those.
            let focus_changed = prev_focus != Some(new_focus);
            if focus_changed {
                if let Some(wid) = focused_window_id {
                    self.broadcast_window_focused(wid.0);
                }
            }
        }
        let client = self.clients.get_mut(&hit.client_id)?;
        if client.pointer_id.is_some() {
            let _ = client.emit_pointer_button(
                serial,
                hit.surface_id,
                hit.local_x,
                hit.local_y,
                button,
                state,
            );
        }
        Some(hit)
    }

    /// Emit the final `configure` event after a resize drag
    /// ends, with the `RESIZING` state bit cleared. The
    /// proposed size carries the surface's current buffer
    /// dimensions — the client may have mid-drag committed
    /// a buffer matching one of the in-flight resizing
    /// configures, so this is the size the server "settles
    /// on".
    fn emit_resize_final_configure(&mut self, drag: DragState) {
        let (width, height) = self
            .clients
            .get(&drag.client_id)
            .and_then(|client| {
                let surface_id = client.toplevels.get(&drag.toplevel_id)?.surface_id;
                client
                    .surfaces
                    .get(&surface_id)
                    .and_then(|surface| surface.current_buffer)
                    .and_then(|attachment| client.buffers.get(&attachment.buffer_id))
                    .map(|buffer| (buffer.width as i32, buffer.height as i32))
            })
            .unwrap_or((0, 0));
        self.emit_toplevel_configure(drag.client_id, drag.toplevel_id, width, height);
    }

    fn settle_drag_geometry(&mut self, drag: DragState) {
        let Some(client) = self.clients.get_mut(&drag.client_id) else {
            return;
        };
        let Some(toplevel) = client.toplevels.get(&drag.toplevel_id) else {
            return;
        };
        if toplevel.maximized {
            return;
        }
        let surface_id = toplevel.surface_id;
        let (x, y) = (toplevel.x, toplevel.y);
        let dimensions = client
            .surfaces
            .get(&surface_id)
            .and_then(|surface| surface.current_buffer)
            .and_then(|attachment| client.buffers.get(&attachment.buffer_id))
            .map(|buffer| (buffer.width, buffer.height));
        if let Some(toplevel) = client.toplevels.get_mut(&drag.toplevel_id) {
            toplevel.normal_x = x;
            toplevel.normal_y = y;
            if let Some((width, height)) = dimensions {
                toplevel.normal_width = width;
                toplevel.normal_height = height;
            }
        }
    }

    /// Inject a physical keyboard transition. Ordinary keys route to the
    /// currently-focused client's `pmd_keyboard`. A first F4 press while
    /// either Alt key is held is instead consumed only when the authoritative
    /// focus names a visible ordinary toplevel and the active authenticated
    /// shell-manager can accept a target-specific `close_shortcut` event.
    /// Repeated presses and the matching release remain consumed for that
    /// physical hold. Returns the focused target involved, or `None` when no
    /// window has keyboard focus.
    pub fn inject_keyboard_key(&mut self, key: u32, state: u32) -> Option<(ClientId, ObjectId)> {
        self.track_keyboard_modifier(key, state);
        let focused = self.keyboard_focus;
        if u8::try_from(key)
            .ok()
            .and_then(|raw| Scancode::try_from(raw).ok())
            == Some(Scancode::F4)
        {
            match state {
                key_state::PRESSED if !self.f4_down => {
                    self.f4_down = true;
                    self.f4_close_shortcut_consumed =
                        self.alt_mask != 0 && self.try_emit_close_shortcut();
                    if self.f4_close_shortcut_consumed {
                        return focused;
                    }
                }
                key_state::PRESSED if self.f4_close_shortcut_consumed => return focused,
                key_state::RELEASED => {
                    let consumed = self.f4_close_shortcut_consumed;
                    self.f4_down = false;
                    self.f4_close_shortcut_consumed = false;
                    if consumed {
                        return focused;
                    }
                }
                _ => {}
            }
        }

        let (client_id, surface_id) = focused?;
        let key = self.map_keyboard_key(key);
        let client = self.clients.get_mut(&client_id)?;
        if client.keyboard_id.is_some() {
            let _ = client.emit_keyboard_key(surface_id, key, state);
        }
        Some((client_id, surface_id))
    }

    fn map_keyboard_key(&self, key: u32) -> u32 {
        if self.keyboard_layout == KeyboardLayout::UsQwerty {
            return key;
        }
        self.active_keymap
            .map_to_logical_scancode(key, self.shift_mask != 0, &self.logical_keymap)
    }

    fn track_keyboard_modifier(&mut self, key: u32, state: u32) {
        let modifier = match u8::try_from(key)
            .ok()
            .and_then(|raw| Scancode::try_from(raw).ok())
        {
            Some(Scancode::ShiftLeft) => Some((&mut self.shift_mask, 1)),
            Some(Scancode::ShiftRight) => Some((&mut self.shift_mask, 2)),
            Some(Scancode::AltLeft) => Some((&mut self.alt_mask, 1)),
            Some(Scancode::AltRight) => Some((&mut self.alt_mask, 2)),
            _ => None,
        };
        let Some((mask, bit)) = modifier else {
            return;
        };
        match state {
            key_state::PRESSED => *mask |= bit,
            key_state::RELEASED => *mask &= !bit,
            _ => {}
        }
    }

    /// Emit one privileged shortcut event only after both endpoints have been
    /// resolved from server-owned state. The work-area owner is the active
    /// replaceable desktop shell; binding that object was capability-gated
    /// from kernel-authenticated peer credentials.
    fn try_emit_close_shortcut(&mut self) -> bool {
        use abi::cap::Cap;

        let Some(window_id) = self
            .keyboard_focus
            .and_then(|(client_id, surface_id)| {
                let client = self.clients.get(&client_id)?;
                if client.capabilities.contains(Cap::Shell) {
                    return None;
                }
                self.window_id_for_surface(client_id, surface_id)
            })
            .filter(|window_id| self.window_is_focusable(*window_id))
        else {
            return false;
        };
        let Some(shell_client_id) = self.work_area_owner else {
            return false;
        };
        let Some(shell_manager_id) = self.clients.get(&shell_client_id).and_then(|client| {
            (client.capabilities.contains(Cap::Shell)
                && client
                    .shell_manager_version
                    .is_some_and(|version| version >= 2))
            .then_some(client.shell_manager_id)
            .flatten()
        }) else {
            return false;
        };
        self.clients
            .get_mut(&shell_client_id)
            .is_some_and(|client| {
                client
                    .emit_close_shortcut(shell_manager_id, window_id.0)
                    .is_ok()
            })
    }

    /// Explicitly set keyboard focus. Used by tests and
    /// by the desktop shell's click-to-focus path.
    pub fn set_keyboard_focus(&mut self, focus: Option<(ClientId, ObjectId)>) {
        self.transition_keyboard_focus(focus);
        let raised = focus
            .and_then(|(client_id, surface_id)| self.window_id_for_surface(client_id, surface_id))
            .map(|window_id| self.raise_window(window_id))
            .unwrap_or(false);
        if raised {
            self.request_recomposition();
        }
    }

    /// Change the keyboard-focus target and publish the activation transition
    /// to both affected client-owned toplevels. Activation is part of the
    /// xdg-toplevel configure state, so a shell focus event alone is
    /// insufficient for ordinary applications to repaint focused chrome.
    fn transition_keyboard_focus(&mut self, focus: Option<(ClientId, ObjectId)>) -> bool {
        let previous = self.keyboard_focus;
        if previous == focus {
            return false;
        }
        self.keyboard_focus = focus;
        if let Some(previous) = previous {
            self.emit_current_toplevel_configure(previous);
        }
        if let Some(focus) = focus {
            self.emit_current_toplevel_configure(focus);
        }
        true
    }

    /// Select the topmost remaining mapped, visible toplevel. Destructive and
    /// minimize paths use an exclusion set while the outgoing window is still
    /// registered so deactivation can be emitted before removal.
    fn fallback_keyboard_focus(
        &self,
        excluded: &BTreeSet<WindowId>,
    ) -> Option<(ClientId, ObjectId)> {
        self.z_order.iter().rev().find_map(|window_id| {
            if excluded.contains(window_id) || !self.window_is_focusable(*window_id) {
                return None;
            }
            let owner = self.windows.get(window_id)?;
            let surface_id = self
                .clients
                .get(&owner.client_id)?
                .toplevels
                .get(&owner.toplevel_id)?
                .surface_id;
            Some((owner.client_id, surface_id))
        })
    }

    /// Compose the persistent and transient configure state for one
    /// toplevel. Keeping this in one place prevents a later resize or
    /// maximize configure from accidentally clearing ACTIVATED.
    fn composed_toplevel_states(&self, client_id: ClientId, toplevel_id: ObjectId) -> u32 {
        use display_proto::xdg_toplevel_state;

        let Some(toplevel) = self
            .clients
            .get(&client_id)
            .and_then(|client| client.toplevels.get(&toplevel_id))
        else {
            return 0;
        };
        let mut states = 0;
        if toplevel.maximized {
            states |= xdg_toplevel_state::MAXIMIZED;
        }
        if self.keyboard_focus == Some((client_id, toplevel.surface_id)) {
            states |= xdg_toplevel_state::ACTIVATED;
        }
        if self.active_drag.is_some_and(|drag| {
            drag.client_id == client_id
                && drag.toplevel_id == toplevel_id
                && matches!(drag.kind, DragKind::Resize { .. })
        }) {
            states |= xdg_toplevel_state::RESIZING;
        }
        states
    }

    /// Emit one configure using the authoritative state composer. Geometry
    /// producers supply only dimensions; persistent/transient state cannot be
    /// lost by bypassing focus, maximize, or resize bits at individual sites.
    fn emit_toplevel_configure(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
        width: i32,
        height: i32,
    ) -> bool {
        let states = self.composed_toplevel_states(client_id, toplevel_id);
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };
        let serial = client.next_configure_serial();
        client
            .emit_xdg_toplevel_configure(toplevel_id, serial, width, height, states)
            .is_ok()
    }

    /// Emit a bounded focus-transition configure for a live mapped surface.
    /// Focus changes touch at most the previous and next targets; no global
    /// client/window scan is needed.
    fn emit_current_toplevel_configure(&mut self, target: (ClientId, ObjectId)) {
        let (client_id, surface_id) = target;
        let Some((toplevel_id, width, height)) = self.clients.get(&client_id).and_then(|client| {
            let toplevel_id = client.toplevel_by_surface.get(&surface_id).copied()?;
            let toplevel = client.toplevels.get(&toplevel_id)?;
            if !toplevel.mapped_once || !toplevel.initial_configure_sent {
                return None;
            }
            let dimensions = if toplevel.maximized {
                Some((self.work_area_width(), self.work_area_height()))
            } else {
                client
                    .surfaces
                    .get(&surface_id)
                    .and_then(|surface| surface.current_buffer)
                    .and_then(|attachment| client.buffers.get(&attachment.buffer_id))
                    .map(|buffer| (buffer.width, buffer.height))
                    .or_else(|| {
                        (toplevel.normal_width > 0 && toplevel.normal_height > 0)
                            .then_some((toplevel.normal_width, toplevel.normal_height))
                    })
            }?;
            Some((toplevel_id, dimensions.0, dimensions.1))
        }) else {
            return;
        };
        self.emit_toplevel_configure(
            client_id,
            toplevel_id,
            width.min(i32::MAX as u32) as i32,
            height.min(i32::MAX as u32) as i32,
        );
    }

    /// Accept a new client connection. Returns the
    /// allocated [`ClientId`]. The client starts with
    /// `pmd_display` bound at `ObjectId::DISPLAY` and
    /// **no capabilities** — privileged interfaces like
    /// `pmd_shell_manager` will refuse to bind for this
    /// client. Use [`Server::accept_with_caps`] when the
    /// connecting process holds capabilities the bind
    /// dispatcher should honour.
    pub fn accept(&mut self) -> ClientId {
        self.try_accept()
            .expect("display server client limit exceeded")
    }

    /// Fallible production accept path. Admission happens before allocating a
    /// `Client`, so rejected sockets retain no server-side protocol state.
    pub fn try_accept(&mut self) -> Result<ClientId, ServerError> {
        self.try_accept_with_caps(abi::cap::CapSet::EMPTY)
    }

    /// Accept a new client connection with a given
    /// capability set. The caps are stored on the
    /// per-client `Client::capabilities` field and
    /// consulted by the `pmd_registry.bind` auto-install
    /// path: binding `pmd_shell_manager` requires
    /// `Cap::Shell`, future privileged interfaces add
    /// their own entries to
    /// [`crate::client::interface_required_cap`].
    pub fn accept_with_caps(&mut self, caps: abi::cap::CapSet) -> ClientId {
        self.try_accept_with_caps(caps)
            .expect("display server client limit exceeded")
    }

    /// Fallible capability-authenticated accept used by the production
    /// transport.
    pub fn try_accept_with_caps(
        &mut self,
        caps: abi::cap::CapSet,
    ) -> Result<ClientId, ServerError> {
        self.try_accept_with_credentials(caps, 0)
    }

    /// Accept using immutable credentials queried from the accepted kernel
    /// socket. Production callers must pass a non-zero peer PID; the zero value
    /// is retained only by the older native fixture helpers above.
    pub fn try_accept_with_credentials(
        &mut self,
        caps: abi::cap::CapSet,
        peer_pid: u32,
    ) -> Result<ClientId, ServerError> {
        let shell_clients = self
            .clients
            .values()
            .filter(|client| client.capabilities.contains(abi::cap::Cap::Shell))
            .count();
        if caps.contains(abi::cap::Cap::Shell) {
            let attempted = shell_clients.saturating_add(1);
            if attempted > self.limits.shell_clients {
                return Err(ServerError::ShellClientLimitExceeded {
                    attempted,
                    limit: self.limits.shell_clients,
                });
            }
        } else {
            let ordinary_clients = self.clients.len().saturating_sub(shell_clients);
            let ordinary_limit = self
                .limits
                .clients
                .saturating_sub(self.limits.shell_clients);
            let attempted = ordinary_clients.saturating_add(1);
            if attempted > ordinary_limit {
                return Err(ServerError::ClientLimitExceeded {
                    attempted,
                    limit: ordinary_limit,
                });
            }
        }
        let attempted = self.clients.len().saturating_add(1);
        if attempted > self.limits.clients {
            return Err(ServerError::ClientLimitExceeded {
                attempted,
                limit: self.limits.clients,
            });
        }
        let id = ClientId(self.next_client_id);
        self.next_client_id = self.next_client_id.saturating_add(1);
        let client_limits = ClientLimits {
            toplevel_metadata_bytes: self.limits.client_toplevel_metadata_bytes,
            ..ClientLimits::default()
        };
        self.clients.insert(
            id,
            Client::new_with_credentials_and_limits(id, caps, peer_pid, client_limits),
        );
        Ok(id)
    }

    /// Drop a client, e.g. on connection close. Returns the
    /// removed client so the caller can inspect its final
    /// state (mostly for tests).
    ///
    /// Side-effect: every toplevel the dropped client owned
    /// triggers a `pmd_shell_manager.window_destroyed`
    /// broadcast so subscribed shells can prune the
    /// associated taskbar entries before the connection's
    /// last events are flushed.
    pub fn disconnect(&mut self, id: ClientId) -> Option<Client> {
        let owned_restore = self
            .restore_transaction
            .as_ref()
            .is_some_and(|restore| restore.owner == id);
        let had_composed_surface = self
            .clients
            .get(&id)
            .map(|client| {
                client
                    .surfaces
                    .values()
                    .any(|surface| surface.current_buffer.is_some())
            })
            .unwrap_or(false);
        let dying_windows: alloc::vec::Vec<(ObjectId, WindowId)> = self
            .window_ids
            .iter()
            .filter_map(|(&(client_id, toplevel_id), &window_id)| {
                (client_id == id).then_some((toplevel_id, window_id))
            })
            .collect();
        let had_windows = !dying_windows.is_empty();
        let dying_window_ids = dying_windows
            .iter()
            .map(|(_, window_id)| *window_id)
            .collect::<BTreeSet<_>>();
        let focus_changed = self.keyboard_focus.map(|(client_id, _)| client_id) == Some(id);
        let fallback_focus = focus_changed
            .then(|| self.fallback_keyboard_focus(&dying_window_ids))
            .flatten();
        if focus_changed {
            self.transition_keyboard_focus(fallback_focus);
        }
        let removed = self.clients.remove(&id);
        if let Some(client) = removed.as_ref() {
            self.pool_bytes = self.pool_bytes.saturating_sub(client.pool_bytes_len());
            self.toplevel_metadata_bytes = self
                .toplevel_metadata_bytes
                .saturating_sub(client.toplevel_metadata_bytes_len());
        }
        if self.active_drag.map(|drag| drag.client_id) == Some(id) {
            self.active_drag = None;
        }
        if self.work_area_owner == Some(id) {
            self.work_area_owner = None;
            self.taskbar_height_px = 0;
        }
        if self.pending_shell_command_owner == Some(id) {
            self.pending_shell_command_owner = None;
        }
        if self.deferred_focus_commit_owner == Some(id) {
            self.deferred_focus_commit_owner = None;
        }
        self.present_fence_clients.remove(&id);
        self.pending_present_fences
            .retain(|(client_id, _)| *client_id != id);
        let shifted_windows = self.remove_from_z_order(&dying_window_ids);
        for (toplevel_id, window_id) in dying_windows {
            self.window_ids.remove(&(id, toplevel_id));
            self.windows.remove(&window_id);
            self.broadcast_window_destroyed(window_id.0);
            if let Some(restore) = self.restore_transaction.as_mut() {
                restore.hidden.remove(&window_id);
                restore.placements.remove(&window_id);
            }
        }
        for window_id in shifted_windows {
            self.broadcast_window_state_changed(window_id);
        }
        if focus_changed {
            let focused_window = fallback_focus
                .and_then(|(client_id, surface_id)| {
                    self.window_id_for_surface(client_id, surface_id)
                })
                .map_or(0, |window_id| window_id.0);
            self.broadcast_window_focused(focused_window);
        }
        let restore_aborted =
            owned_restore && self.abort_restore(shell_restore_status::ABORTED, false);
        if !restore_aborted && removed.is_some() && (had_composed_surface || had_windows) {
            self.request_recomposition();
        }
        removed
    }

    /// Look up the shell-visible global ID for a client's local
    /// toplevel object. Protocol clients learn IDs through events;
    /// this accessor is for server integrations and isolation tests.
    pub fn window_id(&self, client_id: ClientId, toplevel_id: ObjectId) -> Option<u32> {
        self.window_ids
            .get(&(client_id, toplevel_id))
            .map(|id| id.0)
    }

    /// Resolve a shell-visible ID back to its exact owning client and
    /// client-local toplevel object.
    pub fn window_owner(&self, window_id: u32) -> Option<(ClientId, ObjectId)> {
        let owner = self.windows.get(&WindowId(window_id))?;
        Some((owner.client_id, owner.toplevel_id))
    }

    /// Number of currently-connected clients.
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Aggregate live/retained shm-pool backing across every connection.
    pub fn pool_bytes_len(&self) -> u64 {
        self.pool_bytes
    }

    /// Aggregate UTF-8 bytes retained in live toplevel titles and app ids.
    pub fn toplevel_metadata_bytes_len(&self) -> u64 {
        self.toplevel_metadata_bytes
    }

    /// Immutably borrow a client.
    pub fn client(&self, id: ClientId) -> Option<&Client> {
        self.clients.get(&id)
    }

    /// Mutably borrow a client.
    pub fn client_mut(&mut self, id: ClientId) -> Option<&mut Client> {
        self.clients.get_mut(&id)
    }

    /// Whether the per-client protocol event queue crossed a hard ceiling.
    /// The transport owner must close this connection; the queue deliberately
    /// does not resume after a drain because continuing would silently lose an
    /// ordered protocol event.
    pub fn client_event_queue_overflowed(&self, id: ClientId) -> bool {
        self.clients
            .get(&id)
            .is_some_and(Client::event_queue_overflowed)
    }

    /// Drain any events the server has enqueued for a
    /// client. Returns a single flat `Vec<u8>` containing
    /// every pending event back-to-back, or `None` if the
    /// client id is unknown.
    ///
    /// The caller typically ships the returned bytes over
    /// the transport the client is connected on; see
    /// `integration-tests/tests/shell_over_display_server.rs`
    /// for a full loopback example.
    pub fn drain_client_events(&mut self, client_id: ClientId) -> Option<Vec<u8>> {
        let client = self.clients.get_mut(&client_id)?;
        Some(client.drain_pending_events())
    }

    /// Encoded event bytes currently awaiting transport for `client_id`.
    pub fn client_pending_event_bytes(&self, client_id: ClientId) -> Option<usize> {
        self.clients
            .get(&client_id)
            .map(Client::pending_event_bytes)
    }

    /// Encoded size of the next complete ordered event for `client_id`.
    pub fn client_next_pending_event_bytes(&self, client_id: ClientId) -> Option<usize> {
        self.clients
            .get(&client_id)
            .and_then(Client::next_pending_event_bytes)
    }

    /// Drain a framing-preserving event prefix under a transport-turn byte
    /// budget. The remainder stays queued in exact protocol order.
    pub fn drain_client_events_bounded(
        &mut self,
        client_id: ClientId,
        max_bytes: usize,
    ) -> Option<Vec<u8>> {
        let client = self.clients.get_mut(&client_id)?;
        Some(client.drain_pending_events_bounded(max_bytes))
    }

    /// Feed one wire-format request (header + payload) through
    /// the dispatcher. The input buffer must contain a full
    /// message starting at offset 0; callers should frame
    /// their byte stream with [`MessageHeader::decode`] first
    /// to know how many bytes to pass.
    ///
    /// If the dispatched request is a `pmd_surface.commit`,
    /// the server's minimal compositor runs after the
    /// client-side state transition: the newly-current
    /// buffer's pixels are blitted into [`Self::framebuffer`]
    /// at the attachment's origin. This is how a
    /// client-side pool write becomes a "pixel on the
    /// screen" in the v1 skeleton.
    pub fn dispatch_request(
        &mut self,
        client_id: ClientId,
        bytes: &[u8],
    ) -> Result<(), ServerError> {
        let header = MessageHeader::decode(bytes)?;
        let payload_end = header.length as usize;
        let payload = &bytes[HEADER_SIZE..payload_end];

        // Peek at the target object's interface BEFORE
        // dispatch so we can tell whether this request is
        // a surface commit. After dispatch the client's
        // state has already been mutated.
        let pre_interface = self
            .clients
            .get(&client_id)
            .and_then(|c| c.get(header.object_id));
        let surface_commit_damage = (pre_interface == Some(Interface::Surface)
            && header.opcode == 7)
            .then(|| self.surface_commit_damage_candidate(client_id, header.object_id));
        let surface_patch = (pre_interface == Some(Interface::Surface) && header.opcode == 8)
            .then(|| SurfacePatchCurrent::decode(payload).ok())
            .flatten();
        let surface_geometry_before = if pre_interface == Some(Interface::Surface) {
            self.clients.get(&client_id).and_then(|client| {
                let attachment = client.surfaces.get(&header.object_id)?.current_buffer?;
                let buffer = client.buffers.get(&attachment.buffer_id)?;
                Some((buffer.width, buffer.height))
            })
        } else {
            None
        };
        let reserved_shell_command = pre_interface == Some(Interface::ShellManager)
            && matches!(header.opcode, 2 | 3 | 4 | 5 | 7)
            && self.pending_shell_command_owner == Some(client_id);
        let restore_holds_toplevel_owner_state = pre_interface == Some(Interface::XdgToplevel)
            && matches!(header.opcode, 5..=9)
            && self
                .clients
                .get(&client_id)
                .and_then(|client| client.toplevels.get(&header.object_id))
                .is_some_and(|toplevel| toplevel.hidden_for_restore);
        if restore_holds_toplevel_owner_state {
            return Ok(());
        }

        let pool_bytes_before = self
            .clients
            .get(&client_id)
            .ok_or(ServerError::NoSuchClient { id: client_id })?
            .pool_bytes_len();
        let toplevel_metadata_bytes_before = self
            .clients
            .get(&client_id)
            .ok_or(ServerError::NoSuchClient { id: client_id })?
            .toplevel_metadata_bytes_len();
        let is_shell = self
            .clients
            .get(&client_id)
            .is_some_and(|client| client.capabilities.contains(abi::cap::Cap::Shell));
        let reserved_shell_pool_bytes =
            SHELL_FULL_OUTPUT_POOL_BYTES.saturating_mul(self.limits.shell_clients as u64);
        let reserved_shell_toplevels =
            SHELL_TOPLEVELS_RESERVED_PER_CLIENT.saturating_mul(self.limits.shell_clients);
        let reserved_shell_metadata_bytes = SHELL_METADATA_BYTES_RESERVED_PER_CLIENT
            .saturating_mul(self.limits.shell_clients as u64);
        let client = self
            .clients
            .get_mut(&client_id)
            .ok_or(ServerError::NoSuchClient { id: client_id })?;
        client.dispatch_request_with_server_budgets(
            header,
            payload,
            ServerResourceBudgets {
                pool_bytes: self.pool_bytes,
                pool_byte_limit: if is_shell {
                    self.limits.pool_bytes
                } else {
                    self.limits
                        .pool_bytes
                        .saturating_sub(reserved_shell_pool_bytes)
                },
                toplevels: self.windows.len(),
                toplevel_limit: if is_shell {
                    self.limits.toplevels
                } else {
                    self.limits
                        .toplevels
                        .saturating_sub(reserved_shell_toplevels)
                },
                toplevel_metadata_bytes: self.toplevel_metadata_bytes,
                toplevel_metadata_byte_limit: if is_shell {
                    self.limits.toplevel_metadata_bytes
                } else {
                    self.limits
                        .toplevel_metadata_bytes
                        .saturating_sub(reserved_shell_metadata_bytes)
                },
            },
        )?;
        let pool_bytes_after = client.pool_bytes_len();
        let toplevel_metadata_bytes_after = client.toplevel_metadata_bytes_len();
        self.pool_bytes = self
            .pool_bytes
            .saturating_sub(pool_bytes_before)
            .saturating_add(pool_bytes_after);
        self.toplevel_metadata_bytes = self
            .toplevel_metadata_bytes
            .saturating_sub(toplevel_metadata_bytes_before)
            .saturating_add(toplevel_metadata_bytes_after);
        if reserved_shell_command {
            self.pending_shell_command_owner = None;
        }

        if is_shell
            && pre_interface == Some(Interface::ShellManager)
            && header.opcode == 8
            && display_proto::requests::ShellManagerDesktopReady::decode(payload).is_ok()
            && self.present_fence_clients.insert(client_id)
        {
            let serial = self.next_present_fence_serial;
            self.next_present_fence_serial = self.next_present_fence_serial.wrapping_add(1).max(1);
            self.pending_present_fences.push_back((client_id, serial));
        }

        if pre_interface == Some(Interface::Surface) && header.opcode == 7
        /* commit */
        {
            let geometry_changed = self.update_normal_geometry_after_commit(
                client_id,
                header.object_id,
                surface_geometry_before,
            );
            let placement_settled =
                self.settle_restore_placement_after_commit(client_id, header.object_id);
            let newly_focused = self.focus_newly_mapped_toplevel(client_id, header.object_id);
            let commit_damage = surface_commit_damage
                .filter(|candidate| {
                    candidate.expected_geometry.is_some()
                        && candidate.expected_geometry
                            == self.surface_output_geometry(client_id, header.object_id)
                })
                .map(|candidate| candidate.damage)
                .unwrap_or(PresentationDamage::Full);
            self.composite_surface_commit(client_id, header.object_id, commit_damage);
            if self.deferred_focus_commit_owner == Some(client_id) {
                self.deferred_focus_commit_owner = None;
            }
            if let Some(window_id) = newly_focused {
                self.broadcast_window_focused(window_id.0);
            }
            if let Some(window_id) = geometry_changed.or(placement_settled) {
                let dragging_same_window = self
                    .active_drag
                    .and_then(|drag| self.window_ids.get(&(drag.client_id, drag.toplevel_id)))
                    .is_some_and(|dragged| *dragged == window_id);
                if !dragging_same_window {
                    self.broadcast_window_state_changed(window_id);
                }
            }
            self.maybe_emit_initial_configure(client_id, header.object_id);
        }

        if pre_interface == Some(Interface::Surface) && header.opcode == 8
        /* patch_current */
        {
            let damage = surface_patch
                .as_ref()
                .map(|patch| {
                    self.surface_patch_presentation_damage(client_id, header.object_id, patch)
                })
                .unwrap_or(PresentationDamage::Full);
            self.request_recomposition_with_damage(damage);
        }

        if pre_interface == Some(Interface::Surface) && header.opcode == 1
        /* destroy */
        {
            if self.keyboard_focus == Some((client_id, header.object_id)) {
                // A surface carrying a toplevel role cannot reach this hook:
                // client dispatch rejects it with SurfaceHasToplevel. Keep
                // fallback semantics complete for an explicitly-focused
                // roleless surface (primarily fixture/debug use); there is no
                // old toplevel configure to emit, but the surviving mapped
                // target still receives ACTIVATED.
                let fallback_focus = self.fallback_keyboard_focus(&BTreeSet::new());
                self.transition_keyboard_focus(fallback_focus);
                let focused_window = fallback_focus
                    .and_then(|(owner, surface)| self.window_id_for_surface(owner, surface))
                    .map_or(0, |window_id| window_id.0);
                self.broadcast_window_focused(focused_window);
            }
            self.request_recomposition();
        }

        // Shell-manager window IDs live in the server-global window
        // registry, never in a client's local ObjectId namespace.
        if pre_interface == Some(Interface::ShellManager) && header.opcode == 4
        /* minimize_window */
        {
            if let Ok(req) = display_proto::requests::ShellManagerMinimizeWindow::decode(payload) {
                self.set_window_minimized(req.window_id, true);
            }
        }

        if pre_interface == Some(Interface::ShellManager) && header.opcode == 5
        /* unminimize_window */
        {
            if let Ok(req) = display_proto::requests::ShellManagerUnminimizeWindow::decode(payload)
            {
                // Resolve only through the server-global window registry.
                // Restoring from the taskbar also activates and raises the
                // window, matching the existing focus-window behavior.
                self.focus_window_by_id(req.window_id, None);
            }
        }

        if pre_interface == Some(Interface::ShellManager) && header.opcode == 6
        /* set_work_area_bottom */
        {
            if let Ok(req) = display_proto::requests::ShellManagerSetWorkAreaBottom::decode(payload)
            {
                self.set_work_area_bottom_for(client_id, req.height_px);
            }
        }

        if pre_interface == Some(Interface::ShellManager) && header.opcode == 7
        /* toggle_maximized_window */
        {
            if let Ok(req) =
                display_proto::requests::ShellManagerToggleMaximizedWindow::decode(payload)
            {
                self.toggle_window_maximized(req.window_id);
            }
        }

        if pre_interface == Some(Interface::XdgToplevel) && header.opcode == 3
        /* destroy */
        {
            self.destroy_toplevel(client_id, header.object_id);
            return Ok(());
        }

        // T132 server emit: set_maximized / unset_maximized
        // immediately get a configure response back so the
        // toolkit's is_maximized accessor reflects the new
        // state without an external poke. set_maximized
        // sizes to the work area (framebuffer minus taskbar);
        // unset_maximized echoes the server's idea of the
        // pre-max size (cached at set_maximized time).
        if pre_interface == Some(Interface::XdgToplevel) {
            match header.opcode {
                5 /* set_maximized */ => {
                    self.emit_configure_for_max_state(client_id, header.object_id, true)?;
                    if let Some(window_id) =
                        self.window_ids.get(&(client_id, header.object_id)).copied()
                    {
                        self.broadcast_window_state_changed(window_id);
                    }
                }
                6 /* unset_maximized */ => {
                    self.emit_configure_for_max_state(client_id, header.object_id, false)?;
                    if let Some(window_id) =
                        self.window_ids.get(&(client_id, header.object_id)).copied()
                    {
                        self.broadcast_window_state_changed(window_id);
                    }
                }
                7 /* move */ | 8 /* resize */ => {
                    self.start_drag_from_request(client_id, header.object_id, header.opcode, payload);
                }
                9 /* set_minimized */ => {
                    if let Some(window_id) = self
                        .window_ids
                        .get(&(client_id, header.object_id))
                        .copied()
                    {
                        self.set_window_minimized(window_id.0, true);
                    }
                }
                _ => {}
            }
        }

        // pmd_shell_manager hooks. Subscribe is a one-shot —
        // once a client has called subscribe_windows, any
        // subsequent toplevel mutation triggers a broadcast.
        // The catch-up snapshot fires inline so the client's
        // taskbar populates with everything that's already
        // open before the subscription landed.
        if pre_interface == Some(Interface::ShellManager) {
            match header.opcode {
                1 /* subscribe_windows */ => {
                    self.subscribe_windows_for(client_id);
                }
                2 /* focus_window */ => {
                    if let Ok(req) =
                        display_proto::requests::ShellManagerFocusWindow::decode(payload)
                    {
                        self.focus_window_by_id(
                            req.window_id,
                            reserved_shell_command.then_some(client_id),
                        );
                    }
                }
                3 /* close_window */ => {
                    if let Ok(req) =
                        display_proto::requests::ShellManagerCloseWindow::decode(payload)
                    {
                        self.close_window_by_id(req.window_id);
                    }
                }
                9 /* subscribe_window_state */ => {
                    if let Ok(req) =
                        display_proto::requests::ShellManagerSubscribeWindowState::decode(payload)
                    {
                        self.subscribe_window_state_for(client_id, req.snapshot_id);
                    }
                }
                10 /* begin_restore */ => {
                    if let Ok(req) = ShellManagerBeginRestore::decode(payload) {
                        self.begin_restore(client_id, req);
                    }
                }
                11 /* place_restored_window */ => {
                    if let Ok(req) = ShellManagerPlaceRestoredWindow::decode(payload) {
                        self.place_restored_window(client_id, req);
                    }
                }
                12 /* end_restore */ => {
                    if let Ok(req) = ShellManagerEndRestore::decode(payload) {
                        self.end_restore(client_id, req);
                    }
                }
                _ => {}
            }
        }

        // Broadcast hooks for toplevel lifecycle events.
        // `set_title` / `set_app_id` were applied to the
        // client's per-toplevel state by the underlying
        // `apply_toplevel_state` call inside dispatch_request;
        // we now have to fan out a window_title_changed event
        // to every subscribed client.
        if pre_interface == Some(Interface::XdgToplevel) {
            match header.opcode {
                1 /* set_title */ => {
                    self.broadcast_window_title_changed(client_id, header.object_id);
                }
                2 /* set_app_id */ => {
                    // app_id changes don't have a dedicated
                    // event; the title-changed event covers the
                    // taskbar's repaint trigger since both
                    // affect the visible label.
                    self.broadcast_window_title_changed(client_id, header.object_id);
                }
                _ => {}
            }
        }

        // get_toplevel installed a new toplevel; reposition
        // it using the server-global staircase so two
        // separate clients' first toplevels don't both land
        // at (0, 0). Then broadcast window_created.
        if pre_interface == Some(Interface::XdgShell) && header.opcode == 1
        /* get_toplevel */
        {
            if let Ok(req) = display_proto::requests::XdgShellGetToplevel::decode(payload) {
                self.position_new_toplevel(client_id, req.new_id);
                self.assign_toplevel_ordinal(client_id, req.new_id);
                self.broadcast_window_created(client_id, req.new_id);
                // Assigning a role changes pixels only when the
                // surface was already mapped through the roleless
                // base plane. Avoid publishing a blank intermediate
                // frame for the common create-role-before-first-commit
                // sequence; early input must not race that placeholder.
                let was_visible = self
                    .clients
                    .get(&client_id)
                    .and_then(|client| {
                        let toplevel = client.toplevels.get(&req.new_id)?;
                        client.surfaces.get(&toplevel.surface_id)
                    })
                    .and_then(|surface| surface.current_buffer)
                    .is_some();
                if was_visible {
                    let newly_focused = self.focus_newly_mapped_toplevel(client_id, req.surface_id);
                    self.request_recomposition();
                    if let Some(window_id) = newly_focused {
                        self.broadcast_window_focused(window_id.0);
                    }
                }
            }
        }

        // surface.commit on a surface that has a toplevel
        // means the window has produced its first paintable
        // frame; many shells repaint the taskbar after this
        // because the window's title may have just been set.
        // We treat the FIRST commit as the canonical
        // window_created broadcast trigger if the window was
        // never broadcast before — get_toplevel fires before
        // the title is set, so a delayed-broadcast pattern
        // would wait for the first commit. For v1 we keep the
        // simpler get_toplevel-time broadcast above and do
        // nothing extra on commit.

        Ok(())
    }

    /// Override a just-installed toplevel's auto-layout
    /// position with the server-global counter so each new
    /// window across ANY client lands at a different spot.
    /// The per-client `Client::next_toplevel_offset` already
    /// stepped, but its counter starts at 0 for every fresh
    /// client; without this two separate processes would
    /// have their first toplevel both land at (0, 0) and
    /// stack invisibly.
    fn position_new_toplevel(
        &mut self,
        client_id: ClientId,
        toplevel_id: display_proto::ids::ObjectId,
    ) {
        use crate::client::AUTO_LAYOUT_STEP;
        use abi::cap::Cap;
        let is_shell = self
            .clients
            .get(&client_id)
            .is_some_and(|client| client.capabilities.contains(Cap::Shell));
        let pos = if is_shell {
            // A replacement desktop shell owns the full-output wallpaper and
            // bottom chrome planes. It must never inherit the application
            // cascade offset, and it must not consume an app cascade slot.
            0
        } else {
            let pos = self.next_toplevel_offset;
            self.next_toplevel_offset = self.next_toplevel_offset.saturating_add(AUTO_LAYOUT_STEP);
            pos
        };
        let hide_for_restore = !is_shell && self.restore_transaction.is_some();
        if let Some(client) = self.clients.get_mut(&client_id) {
            if let Some(toplevel) = client.toplevels.get_mut(&toplevel_id) {
                toplevel.x = pos;
                toplevel.y = pos;
                toplevel.normal_x = pos;
                toplevel.normal_y = pos;
                toplevel.hidden_for_restore = hide_for_restore;
            }
        }
    }

    fn assign_toplevel_ordinal(&mut self, client_id: ClientId, toplevel_id: ObjectId) {
        let Some(peer_pid) = self.clients.get(&client_id).map(|client| client.peer_pid) else {
            return;
        };
        // Zero is confined to native fixtures that do not model kernel peer
        // credentials. Their Client-local non-zero ordinal remains sufficient
        // for existing isolation tests; production never enters this branch.
        if peer_pid == 0 {
            return;
        }
        let next = self
            .next_toplevel_ordinal_by_pid
            .entry(peer_pid)
            .or_insert(1);
        let ordinal = *next;
        *next = next
            .checked_add(1)
            .expect("per-process toplevel ordinal exhausted");
        if let Some(toplevel) = self
            .clients
            .get_mut(&client_id)
            .and_then(|client| client.toplevels.get_mut(&toplevel_id))
        {
            toplevel.ordinal = ordinal;
        }
    }

    fn register_window(
        &mut self,
        client_id: ClientId,
        toplevel_id: ObjectId,
    ) -> (WindowId, Vec<WindowId>) {
        if let Some(&window_id) = self.window_ids.get(&(client_id, toplevel_id)) {
            return (window_id, Vec::new());
        }
        let window_id = WindowId(self.next_window_id);
        self.next_window_id = self
            .next_window_id
            .checked_add(1)
            .expect("server-global window id exhausted");
        self.windows.insert(
            window_id,
            WindowOwner {
                client_id,
                toplevel_id,
            },
        );
        self.window_ids.insert((client_id, toplevel_id), window_id);
        let is_shell = self
            .clients
            .get(&client_id)
            .is_some_and(|client| client.capabilities.contains(abi::cap::Cap::Shell));
        let shifted_windows = if is_shell {
            let shifted = self.z_order.clone();
            self.z_order.insert(0, window_id);
            shifted
        } else {
            self.z_order.push(window_id);
            Vec::new()
        };
        if self
            .clients
            .get(&client_id)
            .and_then(|client| client.toplevels.get(&toplevel_id))
            .is_some_and(|toplevel| toplevel.hidden_for_restore)
        {
            if let Some(restore) = self.restore_transaction.as_mut() {
                restore.hidden.insert(window_id);
            }
        }
        (window_id, shifted_windows)
    }

    fn window_id_for_surface(&self, client_id: ClientId, surface_id: ObjectId) -> Option<WindowId> {
        let toplevel_id = self
            .clients
            .get(&client_id)?
            .toplevel_by_surface
            .get(&surface_id)
            .copied()?;
        self.window_ids.get(&(client_id, toplevel_id)).copied()
    }

    /// Record normal geometry only when a commit changes the mapped state or
    /// the non-maximized buffer rectangle. Damage-only commits therefore do
    /// not produce shell state traffic.
    fn update_normal_geometry_after_commit(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
        geometry_before: Option<(u32, u32)>,
    ) -> Option<WindowId> {
        let client = self.clients.get_mut(&client_id)?;
        let toplevel_id = client.toplevel_by_surface.get(&surface_id).copied()?;
        let surface = client.surfaces.get(&surface_id)?;
        let attachment = surface.current_buffer;
        let dimensions = attachment.and_then(|attachment| {
            client
                .buffers
                .get(&attachment.buffer_id)
                .map(|buffer| (buffer.width, buffer.height))
        });
        let toplevel = client.toplevels.get_mut(&toplevel_id)?;
        let mut changed = geometry_before != dimensions;
        if !toplevel.maximized {
            if let Some((width, height)) = dimensions {
                let geometry = (toplevel.x, toplevel.y, width, height);
                let normal = (
                    toplevel.normal_x,
                    toplevel.normal_y,
                    toplevel.normal_width,
                    toplevel.normal_height,
                );
                changed |= geometry != normal;
                toplevel.normal_x = toplevel.x;
                toplevel.normal_y = toplevel.y;
                toplevel.normal_width = width;
                toplevel.normal_height = height;
            }
        }
        changed
            .then(|| self.window_ids.get(&(client_id, toplevel_id)).copied())
            .flatten()
    }

    /// Mark a hidden restore placement settled only when a commit strictly
    /// follows the placement boundary and promotes a buffer with the exact
    /// effective size. This gives the shell a causal readiness signal rather
    /// than letting an already-mapped default buffer masquerade as restored.
    fn settle_restore_placement_after_commit(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> Option<WindowId> {
        let window_id = self.window_id_for_surface(client_id, surface_id)?;
        let (commit_count_at_place, expected_dimensions, was_settled) = {
            let placement = self
                .restore_transaction
                .as_ref()?
                .placements
                .get(&window_id)?;
            (
                placement.commit_count_at_place,
                (placement.expected_width, placement.expected_height),
                placement.settled,
            )
        };
        let (commit_count, current_dimensions) = {
            let client = self.clients.get(&client_id)?;
            let surface = client.surfaces.get(&surface_id)?;
            let dimensions = surface.current_buffer.and_then(|attachment| {
                client
                    .buffers
                    .get(&attachment.buffer_id)
                    .map(|buffer| (buffer.width, buffer.height))
            });
            (surface.commit_count, dimensions)
        };
        if commit_count <= commit_count_at_place || current_dimensions != Some(expected_dimensions)
        {
            if commit_count <= commit_count_at_place || !was_settled {
                return None;
            }
            let placement = self
                .restore_transaction
                .as_mut()?
                .placements
                .get_mut(&window_id)?;
            placement.settled = false;
            return Some(window_id);
        }
        let placement = self
            .restore_transaction
            .as_mut()?
            .placements
            .get_mut(&window_id)?;
        if placement.settled {
            return None;
        }
        placement.settled = true;
        Some(window_id)
    }

    /// Give a toplevel focus on its first transition to mapped.
    /// Returns the focused global ID so the caller can broadcast
    /// after its composition pass. Hidden first maps are marked but
    /// do not steal focus.
    fn focus_newly_mapped_toplevel(
        &mut self,
        client_id: ClientId,
        surface_id: ObjectId,
    ) -> Option<WindowId> {
        let (toplevel_id, minimized, hidden, is_shell) = {
            let client = self.clients.get_mut(&client_id)?;
            let is_shell = client.capabilities.contains(abi::cap::Cap::Shell);
            let toplevel_id = client.toplevel_by_surface.get(&surface_id).copied()?;
            let surface = client.surfaces.get(&surface_id)?;
            surface.current_buffer?;
            let toplevel = client.toplevels.get_mut(&toplevel_id)?;
            if toplevel.mapped_once {
                return None;
            }
            toplevel.mapped_once = true;
            (
                toplevel_id,
                toplevel.minimized,
                toplevel.hidden_for_restore,
                is_shell,
            )
        };
        // The shell deliberately delays its first full-output buffer while
        // bounded wallpaper and session-restore work advances. A surviving
        // ordinary mapped window keeps its stack position even if focus was
        // temporarily cleared before a replacement shell maps. With no such
        // window, the shell-only cold path retains conventional initial focus.
        let mapped_focusable_ordinary_survives = is_shell
            && self.windows.values().any(|owner| {
                let Some(client) = self.clients.get(&owner.client_id) else {
                    return false;
                };
                if client.capabilities.contains(abi::cap::Cap::Shell) {
                    return false;
                }
                let Some(toplevel) = client.toplevels.get(&owner.toplevel_id) else {
                    return false;
                };
                !toplevel.minimized
                    && !toplevel.hidden_for_restore
                    && client
                        .surfaces
                        .get(&toplevel.surface_id)
                        .is_some_and(|surface| surface.current_buffer.is_some())
            });
        if minimized || hidden || mapped_focusable_ordinary_survives {
            return None;
        }
        let window_id = self.window_ids.get(&(client_id, toplevel_id)).copied()?;
        self.transition_keyboard_focus(Some((client_id, surface_id)));
        self.raise_window(window_id);
        Some(window_id)
    }

    /// Move a live window to the top of the explicit stack.
    /// Returns true only when the visible ordering changed. Every v2 shell
    /// subscriber receives fresh authoritative state for the complete changed
    /// rank interval, so session capture never has to infer displaced ranks
    /// from the separate focus event.
    fn raise_window(&mut self, window_id: WindowId) -> bool {
        let changed = self.move_window_to_top(window_id);
        if changed.is_empty() {
            return false;
        }
        for changed_window in changed {
            self.broadcast_window_state_changed(changed_window);
        }
        true
    }

    /// Apply the stack mutation without publishing intermediate ranks. Restore
    /// end uses this while assembling one atomic final order, then emits the
    /// complete final authoritative state set exactly once.
    fn move_window_to_top(&mut self, window_id: WindowId) -> Vec<WindowId> {
        let Some(index) = self.z_order.iter().position(|id| *id == window_id) else {
            return Vec::new();
        };
        if index + 1 == self.z_order.len() {
            return Vec::new();
        }
        let changed = self.z_order[index..].to_vec();
        self.z_order.remove(index);
        self.z_order.push(window_id);
        changed
    }

    /// Remove a set of windows and return every surviving window whose
    /// authoritative rank shifted. Callers publish destruction first and then
    /// one fresh v2 state per returned window, avoiding both stale rank caches
    /// and quadratic intermediate broadcasts when a whole client exits.
    fn remove_from_z_order(&mut self, removed: &BTreeSet<WindowId>) -> Vec<WindowId> {
        let Some(first_removed) = self
            .z_order
            .iter()
            .position(|window_id| removed.contains(window_id))
        else {
            return Vec::new();
        };
        self.z_order
            .retain(|window_id| !removed.contains(window_id));
        self.z_order[first_removed.min(self.z_order.len())..].to_vec()
    }

    fn destroy_toplevel(&mut self, client_id: ClientId, toplevel_id: ObjectId) {
        let window_id = self.window_ids.get(&(client_id, toplevel_id)).copied();
        let focused_surface = self
            .clients
            .get(&client_id)
            .and_then(|client| client.toplevels.get(&toplevel_id))
            .map(|toplevel| toplevel.surface_id)
            .filter(|surface_id| self.keyboard_focus == Some((client_id, *surface_id)));
        let fallback_focus = focused_surface.and_then(|_| {
            self.fallback_keyboard_focus(&window_id.into_iter().collect::<BTreeSet<_>>())
        });
        if focused_surface.is_some() {
            self.transition_keyboard_focus(fallback_focus);
        }
        let removed_surface = self.clients.get_mut(&client_id).and_then(|client| {
            let pool_bytes_before = client.pool_bytes_len();
            let metadata_bytes_before = client.toplevel_metadata_bytes_len();
            let toplevel = client.toplevels.remove(&toplevel_id)?;
            client.release_toplevel_metadata(&toplevel);
            client.toplevel_by_surface.remove(&toplevel.surface_id);
            client.objects.remove(&toplevel_id);
            let _ = client.emit_delete_id(toplevel_id);
            client.detach_surface_resources(toplevel.surface_id);
            Some((
                toplevel.surface_id,
                pool_bytes_before,
                client.pool_bytes_len(),
                metadata_bytes_before,
                client.toplevel_metadata_bytes_len(),
            ))
        });
        let Some((
            _surface_id,
            pool_bytes_before,
            pool_bytes_after,
            metadata_bytes_before,
            metadata_bytes_after,
        )) = removed_surface
        else {
            return;
        };
        self.pool_bytes = self
            .pool_bytes
            .saturating_sub(pool_bytes_before)
            .saturating_add(pool_bytes_after);
        self.toplevel_metadata_bytes = self
            .toplevel_metadata_bytes
            .saturating_sub(metadata_bytes_before)
            .saturating_add(metadata_bytes_after);
        if self
            .active_drag
            .map(|drag| (drag.client_id, drag.toplevel_id))
            == Some((client_id, toplevel_id))
        {
            self.active_drag = None;
        }
        if let Some(window_id) = self.window_ids.remove(&(client_id, toplevel_id)) {
            self.windows.remove(&window_id);
            let shifted_windows = self.remove_from_z_order(&BTreeSet::from([window_id]));
            if let Some(restore) = self.restore_transaction.as_mut() {
                restore.hidden.remove(&window_id);
                restore.placements.remove(&window_id);
            }
            self.broadcast_window_destroyed(window_id.0);
            for shifted_window in shifted_windows {
                self.broadcast_window_state_changed(shifted_window);
            }
        }
        if focused_surface.is_some() {
            let focused_window = fallback_focus
                .and_then(|(owner, surface)| self.window_id_for_surface(owner, surface))
                .map_or(0, |window_id| window_id.0);
            self.broadcast_window_focused(focused_window);
        }
        self.request_recomposition();
    }

    /// Mark `client_id` as subscribed to shell_manager
    /// window-list events and emit a catch-up snapshot of
    /// every currently-open toplevel through the client's
    /// shell_manager object. No-op if the client has no
    /// bound shell_manager.
    fn subscribe_windows_for(&mut self, client_id: ClientId) {
        let Some(shell_manager_id) = self
            .clients
            .get(&client_id)
            .and_then(|c| c.shell_manager_id)
        else {
            return;
        };
        // Snapshot in stable global-ID order before mutating the
        // subscriber. Focus comparison includes ClientId because
        // surface ObjectIds are client-local and may collide.
        let snapshot: alloc::vec::Vec<(u32, alloc::string::String, alloc::string::String, bool)> =
            self.z_order
                .iter()
                .filter_map(|window_id| {
                    let owner = self.windows.get(window_id)?;
                    let toplevel = self
                        .clients
                        .get(&owner.client_id)?
                        .toplevels
                        .get(&owner.toplevel_id)?;
                    let focused =
                        self.keyboard_focus == Some((owner.client_id, toplevel.surface_id));
                    Some((
                        window_id.0,
                        toplevel.title.clone(),
                        toplevel.app_id.clone(),
                        focused,
                    ))
                })
                .collect();
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        client.shell_manager_subscribed = true;
        for (window_id, title, app_id, focused) in &snapshot {
            let _ = client.emit_window_created(shell_manager_id, *window_id, title, app_id);
            if *focused {
                let _ = client.emit_window_focused(shell_manager_id, *window_id);
            }
        }
    }

    fn subscribe_window_state_for(&mut self, client_id: ClientId, snapshot_id: u32) {
        if snapshot_id == 0 {
            return;
        }
        let Some(shell_manager_id) = self
            .clients
            .get(&client_id)
            .and_then(|client| client.shell_manager_id)
        else {
            return;
        };
        let snapshot = self
            .z_order
            .iter()
            .filter_map(|window_id| self.window_state(*window_id, snapshot_id))
            .collect::<Vec<_>>();
        let focused = self.keyboard_focus.and_then(|(owner, surface)| {
            self.window_id_for_surface(owner, surface)
                .map(|window_id| window_id.0)
        });
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        client.shell_manager_state_snapshot_id = Some(snapshot_id);
        for state in &snapshot {
            let _ = client.emit_window_created_v2(shell_manager_id, state);
        }
        if let Some(window_id) = focused {
            let _ = client.emit_window_focused(shell_manager_id, window_id);
        }
        let _ = client.emit_window_snapshot_done(shell_manager_id, snapshot_id);
    }

    /// Broadcast `window_created` to every subscribed client.
    /// Called from the post-dispatch hook for `get_toplevel`;
    /// at that point the title is empty (set_title comes
    /// later) but the taskbar already wants to show an entry.
    /// A subsequent `set_title` triggers a `window_title_changed`
    /// broadcast that updates the existing entry in place.
    fn broadcast_window_created(
        &mut self,
        owning_client_id: ClientId,
        toplevel_id: display_proto::ids::ObjectId,
    ) {
        let Some(owning) = self.clients.get(&owning_client_id) else {
            return;
        };
        let Some(toplevel) = owning.toplevels.get(&toplevel_id) else {
            return;
        };
        let title = toplevel.title.clone();
        let app_id = toplevel.app_id.clone();
        let (window_id, shifted_windows) = self.register_window(owning_client_id, toplevel_id);
        let state = self.window_state(window_id, 0);
        for client in self.clients.values_mut() {
            let Some(sm_id) = client.shell_manager_id else {
                continue;
            };
            if client.shell_manager_subscribed {
                let _ = client.emit_window_created(sm_id, window_id.0, &title, &app_id);
            }
            if let (Some(snapshot_id), Some(state)) =
                (client.shell_manager_state_snapshot_id, state.as_ref())
            {
                let mut state = state.clone();
                state.snapshot_id = snapshot_id;
                let _ = client.emit_window_created_v2(sm_id, &state);
            }
        }
        for shifted_window in shifted_windows {
            self.broadcast_window_state_changed(shifted_window);
        }
    }

    /// Broadcast `window_destroyed` to every subscribed
    /// client. Called when a toplevel is removed from a
    /// client's table (e.g. on disconnect or via
    /// `xdg_toplevel.destroy`).
    pub fn broadcast_window_destroyed(&mut self, window_id: u32) {
        for client in self.clients.values_mut() {
            if !client.shell_manager_subscribed && client.shell_manager_state_snapshot_id.is_none()
            {
                continue;
            }
            let Some(sm_id) = client.shell_manager_id else {
                continue;
            };
            let _ = client.emit_window_destroyed(sm_id, window_id);
        }
    }

    /// Broadcast `window_title_changed` to every subscribed
    /// client. Called from the post-dispatch hooks for
    /// `set_title` / `set_app_id`.
    fn broadcast_window_title_changed(
        &mut self,
        owning_client_id: ClientId,
        toplevel_id: display_proto::ids::ObjectId,
    ) {
        let Some(owning) = self.clients.get(&owning_client_id) else {
            return;
        };
        let Some(toplevel) = owning.toplevels.get(&toplevel_id) else {
            return;
        };
        let new_title = toplevel.title.clone();
        let Some(window_id) = self
            .window_ids
            .get(&(owning_client_id, toplevel_id))
            .copied()
        else {
            return;
        };
        for client in self.clients.values_mut() {
            let Some(sm_id) = client.shell_manager_id else {
                continue;
            };
            if client.shell_manager_subscribed {
                let _ = client.emit_window_title_changed(sm_id, window_id.0, &new_title);
            }
        }
        self.broadcast_window_state_changed(window_id);
    }

    fn window_state(&self, window_id: WindowId, snapshot_id: u32) -> Option<ShellWindowState> {
        let z_rank = self.z_order.iter().position(|id| *id == window_id)? as u32;
        let owner = self.windows.get(&window_id)?;
        let client = self.clients.get(&owner.client_id)?;
        let toplevel = client.toplevels.get(&owner.toplevel_id)?;
        let attachment = client
            .surfaces
            .get(&toplevel.surface_id)
            .and_then(|surface| surface.current_buffer);
        let (current_x, current_y, current_width, current_height) = attachment
            .and_then(|attachment| {
                client.buffers.get(&attachment.buffer_id).map(|buffer| {
                    (
                        toplevel.x.saturating_add(attachment.x),
                        toplevel.y.saturating_add(attachment.y),
                        buffer.width,
                        buffer.height,
                    )
                })
            })
            .unwrap_or((toplevel.x, toplevel.y, 0, 0));
        let mut flags = 0;
        if attachment.is_some() {
            flags |= shell_window_state_flags::MAPPED;
        }
        if toplevel.minimized {
            flags |= shell_window_state_flags::MINIMIZED;
        }
        if toplevel.maximized {
            flags |= shell_window_state_flags::MAXIMIZED;
        }
        if self.keyboard_focus == Some((owner.client_id, toplevel.surface_id)) {
            flags |= shell_window_state_flags::FOCUSED;
        }
        if toplevel.hidden_for_restore {
            flags |= shell_window_state_flags::HIDDEN_FOR_RESTORE;
        }
        if self
            .restore_transaction
            .as_ref()
            .and_then(|restore| restore.placements.get(&window_id))
            .is_some_and(|placement| placement.settled)
        {
            flags |= shell_window_state_flags::RESTORE_PLACEMENT_APPLIED;
        }
        if client.capabilities.contains(abi::cap::Cap::Shell) {
            flags |= shell_window_state_flags::SHELL_OWNED;
        }
        Some(ShellWindowState {
            snapshot_id,
            window_id: window_id.0,
            owner_pid: client.peer_pid,
            ordinal: toplevel.ordinal,
            current_x,
            current_y,
            current_width,
            current_height,
            normal_x: toplevel.normal_x,
            normal_y: toplevel.normal_y,
            normal_width: toplevel.normal_width,
            normal_height: toplevel.normal_height,
            flags,
            z_rank,
            title: toplevel.title.clone(),
            app_id: toplevel.app_id.clone(),
        })
    }

    fn broadcast_window_state_changed(&mut self, window_id: WindowId) {
        let Some(state) = self.window_state(window_id, 0) else {
            return;
        };
        for client in self.clients.values_mut() {
            let (Some(shell_manager_id), Some(snapshot_id)) = (
                client.shell_manager_id,
                client.shell_manager_state_snapshot_id,
            ) else {
                continue;
            };
            let mut state = state.clone();
            state.snapshot_id = snapshot_id;
            let _ = client.emit_window_state_changed(shell_manager_id, &state);
        }
    }

    /// Broadcast `window_focused(window_id)` to every
    /// subscribed client. Called from the click-to-focus
    /// path inside `inject_pointer_button` and from the
    /// shell_manager.focus_window dispatch hook.
    fn broadcast_window_focused(&mut self, window_id: u32) {
        for client in self.clients.values_mut() {
            if !client.shell_manager_subscribed && client.shell_manager_state_snapshot_id.is_none()
            {
                continue;
            }
            let Some(sm_id) = client.shell_manager_id else {
                continue;
            };
            let _ = client.emit_window_focused(sm_id, window_id);
        }
    }

    /// Implementation of `pmd_shell_manager.focus_window`.
    /// Resolve the server-global ID to its exact owning client and
    /// make that toplevel's surface the keyboard focus target.
    fn focus_window_by_id(&mut self, window_id: u32, defer_until_commit_by: Option<ClientId>) {
        let global_id = WindowId(window_id);
        let owner = self.windows.get(&global_id).copied();
        if let Some(owner) = owner {
            if self
                .clients
                .get(&owner.client_id)
                .and_then(|client| client.toplevels.get(&owner.toplevel_id))
                .is_some_and(|toplevel| toplevel.hidden_for_restore)
            {
                return;
            }
            let Some((surface_id, restored)) = self
                .clients
                .get_mut(&owner.client_id)
                .and_then(|client| client.toplevels.get_mut(&owner.toplevel_id))
                .map(|toplevel| {
                    let restored = toplevel.minimized;
                    toplevel.minimized = false;
                    (toplevel.surface_id, restored)
                })
            else {
                return;
            };
            let cid = owner.client_id;
            self.transition_keyboard_focus(Some((cid, surface_id)));
            let raise_damage = self.raised_window_presentation_damage(global_id);
            let raised = self.raise_window(global_id);
            if restored || raised {
                let damage = if restored {
                    PresentationDamage::Full
                } else {
                    raise_damage
                };
                if let Some(shell_client_id) = defer_until_commit_by {
                    self.deferred_focus_commit_owner = Some(shell_client_id);
                    self.accumulate_presentation_damage(damage);
                } else {
                    self.deferred_focus_commit_owner = None;
                    self.request_recomposition_with_damage(damage);
                }
            }
            self.broadcast_window_focused(window_id);
            self.broadcast_window_state_changed(global_id);
        }
    }

    /// Implementation of `pmd_shell_manager.close_window`.
    /// Activates the target before sending `xdg_toplevel.close` so a client
    /// that vetoes the request (for example, to show an unsaved-changes
    /// prompt) remains visible and owns keyboard focus. The client's
    /// subsequent drop of the toplevel triggers a `window_destroyed`
    /// broadcast; a veto leaves the registered window intact.
    fn close_window_by_id(&mut self, window_id: u32) {
        self.focus_window_by_id(window_id, None);
        let Some(owner) = self.windows.get(&WindowId(window_id)).copied() else {
            return;
        };
        if let Some(client) = self.clients.get_mut(&owner.client_id) {
            let _ = client.emit_xdg_toplevel_close(owner.toplevel_id);
        }
    }

    /// Toggle maximization for the exact toplevel named by the shell-visible
    /// server-global ID. This deliberately never scans client-local object
    /// tables: two clients may use the same toplevel `ObjectId` without the
    /// shell affecting the wrong owner. The operation also restores, raises,
    /// and activates the target as one user-visible action.
    fn toggle_window_maximized(&mut self, window_id: u32) -> bool {
        let global_id = WindowId(window_id);
        let Some(owner) = self.windows.get(&global_id).copied() else {
            return false;
        };
        if self
            .clients
            .get(&owner.client_id)
            .and_then(|client| client.toplevels.get(&owner.toplevel_id))
            .is_some_and(|toplevel| toplevel.hidden_for_restore)
        {
            return false;
        }
        let Some((surface_id, maximize, restored)) = self
            .clients
            .get_mut(&owner.client_id)
            .and_then(|client| client.toplevels.get_mut(&owner.toplevel_id))
            .map(|toplevel| {
                let restored = toplevel.minimized;
                let maximize = !toplevel.maximized;
                toplevel.minimized = false;
                toplevel.maximized = maximize;
                (toplevel.surface_id, maximize, restored)
            })
        else {
            return false;
        };

        self.transition_keyboard_focus(Some((owner.client_id, surface_id)));
        let raised = self.raise_window(global_id);
        let generation = self.scene_generation;
        if self
            .emit_configure_for_max_state(owner.client_id, owner.toplevel_id, maximize)
            .is_err()
        {
            return false;
        }
        // The configure helper recomposes when geometry moved. If the target
        // was only unminimized or raised, present that scene transition here.
        if (restored || raised) && self.scene_generation == generation {
            self.request_recomposition();
        }
        self.broadcast_window_focused(window_id);
        self.broadcast_window_state_changed(global_id);
        true
    }

    /// Server-driven configure emission for the
    /// `set_maximized` / `unset_maximized` request handlers.
    /// On `set_maximized = true`, snapshots the current origin, moves the
    /// toplevel to the work-area origin, and emits a work-area-sized configure
    /// with the MAXIMIZED bit. On `set_maximized = false`, restores the
    /// server-assigned origin and lets the client resume its preferred size.
    fn emit_configure_for_max_state(
        &mut self,
        client_id: ClientId,
        toplevel_id: display_proto::ids::ObjectId,
        maximize: bool,
    ) -> Result<(), ServerError> {
        let work_area_w = self.work_area_width();
        let work_area_h = self.work_area_height();
        let (moved, width, height) = {
            let client = self
                .clients
                .get_mut(&client_id)
                .ok_or(ServerError::NoSuchClient { id: client_id })?;
            let current_dimensions = client
                .toplevels
                .get(&toplevel_id)
                .and_then(|toplevel| client.surfaces.get(&toplevel.surface_id))
                .and_then(|surface| surface.current_buffer)
                .and_then(|attachment| client.buffers.get(&attachment.buffer_id))
                .map(|buffer| (buffer.width, buffer.height));
            // The per-client dispatcher already updated `maximized`; the
            // server owns screen-space placement and therefore snapshots and
            // restores the origin here.
            let moved = {
                let toplevel = client
                    .toplevels
                    .get_mut(&toplevel_id)
                    .ok_or(ServerError::NoSuchClient { id: client_id })?;
                if maximize {
                    if toplevel.restore_origin.is_none() {
                        toplevel.restore_origin = Some((toplevel.x, toplevel.y));
                        toplevel.normal_x = toplevel.x;
                        toplevel.normal_y = toplevel.y;
                        if let Some((width, height)) = current_dimensions {
                            toplevel.normal_width = width;
                            toplevel.normal_height = height;
                        }
                    }
                    let moved = (toplevel.x, toplevel.y) != (0, 0);
                    toplevel.x = 0;
                    toplevel.y = 0;
                    moved
                } else if let Some((x, y)) = toplevel.restore_origin.take() {
                    let moved = (toplevel.x, toplevel.y) != (x, y);
                    toplevel.x = toplevel.normal_x;
                    toplevel.y = toplevel.normal_y;
                    moved
                } else {
                    false
                }
            };
            let (width, height) = if maximize {
                (work_area_w as i32, work_area_h as i32)
            } else {
                let toplevel = client
                    .toplevels
                    .get(&toplevel_id)
                    .ok_or(ServerError::NoSuchClient { id: client_id })?;
                (toplevel.normal_width as i32, toplevel.normal_height as i32)
            };
            (moved, width, height)
        };
        self.emit_toplevel_configure(client_id, toplevel_id, width, height);
        if moved {
            self.request_recomposition();
        }
        Ok(())
    }

    /// Width of the v1 "work area" — the screen region apps
    /// can fill when maximized. For now this equals the full
    /// framebuffer width; a future T130 slice that lands the
    /// taskbar will subtract the taskbar's width / height.
    pub fn work_area_width(&self) -> u32 {
        self.framebuffer.width()
    }

    /// Height of the v1 "work area" — framebuffer height
    /// minus the taskbar's reserved strip. `taskbar_height_px`
    /// defaults to 0 and is set by the shell via
    /// [`Server::set_taskbar_height_px`] when it claims the
    /// bottom strip.
    pub fn work_area_height(&self) -> u32 {
        self.framebuffer
            .height()
            .saturating_sub(self.taskbar_height_px)
    }

    /// Tell the server how tall the taskbar is, in pixels.
    /// The shell calls this after it lays out its taskbar
    /// surface so subsequent `set_maximized` configures use
    /// a work-area height that excludes the taskbar strip.
    /// Pass `0` to clear the reservation.
    pub fn set_taskbar_height_px(&mut self, px: u32) {
        self.taskbar_height_px = px.min(self.framebuffer.height());
        self.work_area_owner = None;
    }

    fn set_work_area_bottom_for(&mut self, client_id: ClientId, px: u32) {
        self.taskbar_height_px = px.min(self.framebuffer.height());
        self.work_area_owner = Some(client_id);
    }

    /// Begin an interactive drag from a `pmd_xdg_toplevel.move`
    /// (opcode 7) or `.resize` (opcode 8) request. Captures
    /// the current pointer position + toplevel origin/size
    /// into [`Server::active_drag`]; subsequent
    /// `inject_pointer_motion` calls update the toplevel
    /// origin (move) or emit a resize configure (resize).
    /// The drag ends on the next `inject_pointer_button(release)`.
    fn start_drag_from_request(
        &mut self,
        client_id: ClientId,
        toplevel_id: display_proto::ids::ObjectId,
        opcode: u16,
        payload: &[u8],
    ) {
        let kind = if opcode == 7 {
            DragKind::Move
        } else {
            // resize payload: u32 serial + u32 edges
            if payload.len() < 8 {
                return;
            }
            let edges = u32::from_le_bytes(payload[4..8].try_into().unwrap());
            DragKind::Resize { edges }
        };
        let (origin_x, origin_y) = match self
            .clients
            .get(&client_id)
            .and_then(|c| c.toplevels.get(&toplevel_id))
        {
            Some(t) => (t.x, t.y),
            None => return,
        };
        self.active_drag = Some(DragState {
            client_id,
            toplevel_id,
            kind,
            start_pointer: self.pointer_position(),
            start_origin: (origin_x, origin_y),
        });
    }

    /// True if an interactive move/resize drag is in
    /// progress (one of the toolkit's clients sent
    /// `xdg_toplevel.move` / `.resize` and the pointer
    /// hasn't been released yet). Exposed for tests + the
    /// shell's "is the user dragging a window?" cursor
    /// logic.
    pub fn is_dragging(&self) -> bool {
        self.active_drag.is_some()
    }

    /// End any in-progress drag, returning the drag state
    /// for tests that want to inspect what was happening.
    /// Called automatically by `inject_pointer_button` on
    /// release; exposed for tests + restore paths.
    pub fn end_drag(&mut self) -> Option<DragState> {
        self.active_drag.take()
    }

    /// If `surface_id`'s toplevel has not yet received an
    /// `xdg_toplevel.configure`, emit one now sized to the
    /// work area and flag the toplevel as configured. The
    /// initial configure is the Wayland-style handshake
    /// that tells the client "you may now paint real
    /// frames"; the toolkit's `Window::dispatch` blocks
    /// the paint loop on `is_configured = true`. Called
    /// from the surface-commit dispatch hook so the very
    /// first commit synthesises the configure (clients
    /// commit an empty buffer or a placeholder buffer
    /// before they know the size).
    pub fn maybe_emit_initial_configure(&mut self, client_id: ClientId, surface_id: ObjectId) {
        use abi::cap::Cap;

        let output_w = self.framebuffer.width() as i32;
        let output_h = self.framebuffer.height() as i32;
        let work_area_w = self.work_area_width() as i32;
        let work_area_h = self.work_area_height() as i32;
        let Some(toplevel_id) = self
            .clients
            .get(&client_id)
            .and_then(|client| client.toplevel_by_surface.get(&surface_id).copied())
        else {
            return;
        };
        let Some((already_sent, is_shell, origin)) = self.clients.get(&client_id).map(|client| {
            let toplevel = client.toplevels.get(&toplevel_id);
            (
                toplevel.is_none_or(|toplevel| toplevel.initial_configure_sent),
                client.capabilities.contains(Cap::Shell),
                toplevel
                    .map(|toplevel| (toplevel.x.max(0), toplevel.y.max(0)))
                    .unwrap_or((0, 0)),
            )
        }) else {
            return;
        };
        if already_sent {
            return;
        }
        let (configured_w, configured_h) = if is_shell {
            // The desktop shell paints the wallpaper and reserved chrome over
            // the complete output. Its own surface is not constrained by the
            // work area it publishes for ordinary application windows.
            (output_w, output_h)
        } else {
            (
                work_area_w.saturating_sub(origin.0).max(1),
                work_area_h.saturating_sub(origin.1).max(1),
            )
        };
        self.emit_toplevel_configure(client_id, toplevel_id, configured_w, configured_h);
        if let Some(toplevel) = self
            .clients
            .get_mut(&client_id)
            .and_then(|client| client.toplevels.get_mut(&toplevel_id))
        {
            toplevel.initial_configure_sent = true;
        }
    }

    /// Emit the v1 globals catalog onto `registry_id` for
    /// `client_id`. Called by the post-dispatch hook in
    /// [`Server::dispatch_request`] after a successful
    /// `pmd_display.get_registry`. Advertises the four
    /// universal globals (compositor, shm, xdg_shell, seat)
    /// plus pmd_shell_manager — gated on the client holding
    /// `Cap::Shell` since binding shell_manager requires
    /// that cap (see [`crate::client::interface_required_cap`]).
    /// The numeric `name` values are the registry handles
    /// the client echoes back through `registry.bind`.
    pub fn advertise_globals_to(&mut self, client_id: ClientId, registry_id: ObjectId) {
        use abi::cap::Cap;
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        let _ = client.emit_global(
            registry_id,
            1,
            "pmd_compositor",
            Interface::Compositor.supported_version(),
        );
        let _ = client.emit_global(
            registry_id,
            2,
            "pmd_shm",
            Interface::Shm.supported_version(),
        );
        let _ = client.emit_global(
            registry_id,
            3,
            "pmd_xdg_shell",
            Interface::XdgShell.supported_version(),
        );
        let _ = client.emit_global(
            registry_id,
            4,
            "pmd_seat",
            Interface::Seat.supported_version(),
        );
        if client.capabilities.contains(Cap::Shell) {
            let _ = client.emit_global(
                registry_id,
                5,
                "pmd_shell_manager",
                Interface::ShellManager.supported_version(),
            );
        }
    }

    /// Flip the minimized state of the exact toplevel named by a
    /// server-global shell-manager window ID.
    pub fn set_window_minimized(&mut self, window_id: u32, minimized: bool) -> bool {
        let Some(owner) = self.windows.get(&WindowId(window_id)).copied() else {
            return false;
        };
        if self
            .clients
            .get(&owner.client_id)
            .and_then(|client| client.toplevels.get(&owner.toplevel_id))
            .is_some_and(|toplevel| toplevel.hidden_for_restore)
        {
            return false;
        }
        let Some((surface_id, changed)) = self
            .clients
            .get_mut(&owner.client_id)
            .and_then(|client| client.toplevels.get_mut(&owner.toplevel_id))
            .map(|toplevel| {
                let changed = toplevel.minimized != minimized;
                toplevel.minimized = minimized;
                (toplevel.surface_id, changed)
            })
        else {
            return false;
        };
        if changed {
            let cleared_focus =
                minimized && self.keyboard_focus == Some((owner.client_id, surface_id));
            if minimized
                && self
                    .active_drag
                    .map(|drag| (drag.client_id, drag.toplevel_id))
                    == Some((owner.client_id, owner.toplevel_id))
            {
                self.active_drag = None;
            }
            let fallback_focus = if cleared_focus {
                self.fallback_keyboard_focus(&BTreeSet::new())
            } else {
                None
            };
            if cleared_focus {
                self.transition_keyboard_focus(fallback_focus);
            }
            self.request_recomposition();
            self.broadcast_window_state_changed(WindowId(window_id));
            if cleared_focus {
                let focused_window = fallback_focus
                    .and_then(|(client_id, surface_id)| {
                        self.window_id_for_surface(client_id, surface_id)
                    })
                    .map_or(0, |window_id| window_id.0);
                self.broadcast_window_focused(focused_window);
            }
        }
        true
    }

    /// Server-driven restore for a previously-minimized
    /// toplevel. Counterpart to the cross-client minimize
    /// path — there's no `pmd_shell_manager.restore_window`
    /// request today (the spec lets the shell drive restore
    /// via `focus_window`, which in v1 is a separate slice),
    /// so this method exists for callers wiring restore
    /// through other channels (test harnesses, future
    /// taskbar click routing).
    pub fn restore_window(&mut self, window_id: u32) -> bool {
        self.set_window_minimized(window_id, false)
    }

    /// Schedule a full-scene rebuild after a successful surface commit. Buffer
    /// release ownership stays in the client state transition so state and
    /// event ordering are settled before recomposition.
    fn composite_surface_commit(
        &mut self,
        _client_id: ClientId,
        _surface_id: display_proto::ids::ObjectId,
        damage: PresentationDamage,
    ) {
        // Re-composite the whole scene: clear the backbuffer,
        // draw roleless base-plane surfaces, then draw every
        // live toplevel in the explicit global z-order.
        // Without this loop a single-surface blit on a later
        // surface.commit would not restore the OTHER
        // toplevels' pixels and the shell's wallpaper paint
        // would erase every app window.
        self.request_recomposition_with_damage(damage);
    }

    /// Clear and reconstruct the complete scene. Roleless
    /// surfaces form a base plane; toplevel surfaces then blit
    /// in explicit global bottom-to-top z-order at the toplevel's
    /// (x + attach.x, y + attach.y) origin; non-toplevel
    /// surfaces blit at the attach offset (matches the
    /// pre-multi-window behaviour the integration tests
    /// expect). Toplevels that are minimized are skipped.
    ///
    /// Called after every `surface.commit` so the
    /// framebuffer always reflects the full scene — without
    /// this loop a single-surface blit would leave the
    /// other windows' pixels intact only as long as nobody
    /// painted over them, and the shell's full-screen
    /// wallpaper paint would erase every app window on
    /// every shell repaint.
    fn recomposite_scene(&mut self) {
        let Server {
            clients,
            framebuffer,
            windows,
            z_order,
            scene_generation,
            ..
        } = self;
        framebuffer.clear_for_composition();

        // Roleless surfaces form the base plane. A surface that
        // previously held a toplevel role stays unmapped after that
        // role is destroyed; otherwise it would be redrawn at its
        // raw attach offset and leave a ghost window behind.
        for client in clients.values() {
            for (surface_id, surface) in client.surfaces.iter() {
                if surface.had_toplevel_role || client.toplevel_by_surface.contains_key(surface_id)
                {
                    continue;
                }
                let Some(attachment) = surface.current_buffer else {
                    continue;
                };
                Self::blit_client_surface(
                    framebuffer,
                    client,
                    *surface_id,
                    attachment.x,
                    attachment.y,
                );
            }
        }

        // Toplevels are then composed in the explicit global
        // bottom-to-top stack. Neither client IDs nor ObjectIds
        // are treated as ordering signals.
        for window_id in z_order.iter() {
            let Some(owner) = windows.get(window_id) else {
                continue;
            };
            let Some(client) = clients.get(&owner.client_id) else {
                continue;
            };
            let Some(toplevel) = client.toplevels.get(&owner.toplevel_id) else {
                continue;
            };
            if toplevel.minimized || toplevel.hidden_for_restore {
                continue;
            }
            let Some(surface) = client.surfaces.get(&toplevel.surface_id) else {
                continue;
            };
            let Some(attachment) = surface.current_buffer else {
                continue;
            };
            Self::blit_client_surface(
                framebuffer,
                client,
                toplevel.surface_id,
                toplevel.x.saturating_add(attachment.x),
                toplevel.y.saturating_add(attachment.y),
            );
        }
        *scene_generation = scene_generation.wrapping_add(1);
    }

    fn blit_client_surface(
        framebuffer: &mut Framebuffer,
        client: &Client,
        surface_id: ObjectId,
        origin_x: i32,
        origin_y: i32,
    ) {
        let Some(surface) = client.surfaces.get(&surface_id) else {
            return;
        };
        let Some(attachment) = surface.current_buffer else {
            return;
        };
        let Some(info) = client.buffers.get(&attachment.buffer_id) else {
            return;
        };
        let Some(pool) = client.pools.get(&info.pool_id) else {
            return;
        };
        let start = info.offset as usize;
        let end = info.byte_end() as usize;
        let Some(src_bytes) = pool.storage.get(start..end) else {
            return;
        };
        framebuffer.blit_buffer(info, src_bytes, origin_x, origin_y);
    }
}

impl Default for Server {
    fn default() -> Self {
        Server::new()
    }
}
