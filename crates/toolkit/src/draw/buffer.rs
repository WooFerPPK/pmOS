//! Double-buffered shm pool + attach/commit loop for
//! top-level windows.
//!
//! [`BufferPool`] bridges the client-side [`Canvas`] paint
//! surface to a server-side [`Window`] through the
//! `pmd_shm.create_pool` → `pmd_shm_pool.create_buffer` →
//! `pmd_surface.attach` → `pmd_surface.damage` →
//! `pmd_surface.commit` request sequence defined in
//! `contracts/display-protocol.md` §§7–9.
//!
//! ## v1 shape
//!
//! The pool hosts **two** ARGB8888 buffers of identical
//! dimensions, stacked back-to-back in the pool's address
//! space: buffer 0 at offset `0`, buffer 1 at offset
//! `width * height * 4`. The caller paints into the "back"
//! buffer via [`BufferPool::acquire_back_canvas`], then
//! attaches + damages + commits via
//! [`BufferPool::commit_and_swap`]. After the server
//! finishes processing the commit it emits
//! `pmd_buffer.release` for the previously-attached buffer;
//! the caller routes that event to
//! [`BufferPool::handle_release`] which flips the in-use
//! flag back to `false` so the next
//! [`BufferPool::acquire_back_canvas`] call returns the
//! reclaimed buffer.
//!
//! ## What this slice does NOT do
//!
//! * **Shared memory.** The `pmd_shm.create_pool` request
//!   carries a `size` in bytes but no actual fd passing.
//!   The toolkit therefore uploads changed 24 KiB regions
//!   with `pmd_shm_pool.write` before each commit. The
//!   in-memory `pixels` vec remains the client backing.
//! * **Frame-callback integration.** Helpers exist
//!   ([`BufferPool::request_frame`] +
//!   [`BufferPool::handle_frame_done`]) to send
//!   `pmd_surface.frame(new_id)` and to mark a recorded
//!   callback id done, but nothing auto-drives the paint
//!   loop on frame-done — that's the next slice.
//! * **Damage tracking.** General app commits damage the full buffer. Persistent
//!   chrome may use [`BufferPool::commit_and_swap_damage`] after initializing
//!   both slots; uploads are then limited to packed rows spanning the clipped
//!   region while the displayed slot remains immutable until commit.
//! * **Format negotiation.** The buffer format is
//!   hardcoded to
//!   [`display_proto::requests::buffer_format::ARGB8888`].
//!
//! ## API shape
//!
//! The `BufferPool` does **not** own the [`Client`]
//! directly. The caller drives protocol requests via
//! [`App`] + [`Window`] and passes mutable references into
//! [`BufferPool::new`] / [`BufferPool::commit_and_swap`].
//! This keeps the pool transport-agnostic — the same
//! struct serves the in-process `MemoryConnection` tests
//! and a real IPC socket in production.

use display_proto::ids::ObjectId;
use display_proto::requests::buffer_format;
use display_proto::wire::WireError;

use crate::app::App;
use crate::draw::{Canvas, Rect, BYTES_PER_PIXEL};
use crate::protocol::{ClientError, Connection};
use crate::window::Window;

pub const SHM_WRITE_CHUNK_BYTES: usize = 24 * 1024;
const MAX_UNCHANGED_LINEAR_CHUNKS_PER_PROGRESS: usize = 8;
/// Maximum caller-supplied rectangles in one damage transaction. Normalizing
/// overlapping rectangles can split them into multiple pieces, so bounding the
/// input also bounds staging memory and per-commit protocol work.
pub const MAX_DAMAGE_REGIONS: usize = 8;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CommitProgress {
    Pending,
    Committed,
}

/// Result of attempting a one-request patch against the surface's current
/// buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CurrentPatch {
    Patched {
        buffer_index: usize,
    },
    /// A staged full commit or transport suffix must drain before retrying.
    Deferred {
        buffer_index: Option<usize>,
    },
    /// This pool has no current slot for the target surface.
    Unavailable,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct ClippedDamage {
    x_start: usize,
    y_start: usize,
    x_end: usize,
    y_end: usize,
}

fn subtract_damage(rect: ClippedDamage, blocker: ClippedDamage) -> Vec<ClippedDamage> {
    let overlap_x_start = rect.x_start.max(blocker.x_start);
    let overlap_y_start = rect.y_start.max(blocker.y_start);
    let overlap_x_end = rect.x_end.min(blocker.x_end);
    let overlap_y_end = rect.y_end.min(blocker.y_end);
    if overlap_x_start >= overlap_x_end || overlap_y_start >= overlap_y_end {
        return vec![rect];
    }

    let mut pieces = Vec::with_capacity(4);
    if rect.y_start < overlap_y_start {
        pieces.push(ClippedDamage {
            x_start: rect.x_start,
            y_start: rect.y_start,
            x_end: rect.x_end,
            y_end: overlap_y_start,
        });
    }
    if overlap_y_end < rect.y_end {
        pieces.push(ClippedDamage {
            x_start: rect.x_start,
            y_start: overlap_y_end,
            x_end: rect.x_end,
            y_end: rect.y_end,
        });
    }
    if rect.x_start < overlap_x_start {
        pieces.push(ClippedDamage {
            x_start: rect.x_start,
            y_start: overlap_y_start,
            x_end: overlap_x_start,
            y_end: overlap_y_end,
        });
    }
    if overlap_x_end < rect.x_end {
        pieces.push(ClippedDamage {
            x_start: overlap_x_end,
            y_start: overlap_y_start,
            x_end: rect.x_end,
            y_end: overlap_y_end,
        });
    }
    pieces
}

fn normalize_damage_rects(damages: &[Rect], width: u32, height: u32) -> Vec<ClippedDamage> {
    let width = i64::from(width);
    let height = i64::from(height);
    let mut normalized: Vec<ClippedDamage> = Vec::new();
    for damage in damages {
        let x_start = i64::from(damage.x).clamp(0, width);
        let y_start = i64::from(damage.y).clamp(0, height);
        let x_end = (i64::from(damage.x) + i64::from(damage.width)).clamp(0, width);
        let y_end = (i64::from(damage.y) + i64::from(damage.height)).clamp(0, height);
        if x_start >= x_end || y_start >= y_end {
            continue;
        }
        let mut pieces = vec![ClippedDamage {
            x_start: x_start as usize,
            y_start: y_start as usize,
            x_end: x_end as usize,
            y_end: y_end as usize,
        }];
        for existing in normalized.iter().copied() {
            pieces = pieces
                .into_iter()
                .flat_map(|piece| subtract_damage(piece, existing))
                .collect();
            if pieces.is_empty() {
                break;
            }
        }
        normalized.extend(pieces);
    }
    normalized
}

enum UploadCursor {
    Linear {
        written: usize,
    },
    Rows {
        damage_index: usize,
        tile_x: usize,
        tile_y: usize,
    },
}

struct PendingCommit {
    surface_id: ObjectId,
    buffer_index: usize,
    swap: bool,
    damages: Vec<ClippedDamage>,
    upload: UploadCursor,
}

/// A double-buffered pixel pool backed by a
/// `pmd_shm_pool`.
///
/// See the module-level doc for the full v1 shape. Fields
/// are private; construct via [`BufferPool::new`] and
/// interact through the public API.
///
/// A staged frame must be progressed through its originating surface. After
/// the first frame completes, the pool remains bound to that surface; create
/// or replace the pool before using a different window.
pub struct BufferPool {
    width: u32,
    height: u32,
    /// In-memory pixel store for BOTH buffers, laid out
    /// back-to-back: buffer 0 occupies
    /// `[0 .. width*height*4]`, buffer 1 occupies
    /// `[width*height*4 .. 2*width*height*4]`.
    ///
    /// The server reads from the shm pool via the protocol
    /// in production; in v1 the pixel transfer to the
    /// server is elided (`pmd_shm.create_pool` carries
    /// only a `size`, not the fd), so this vec is the
    /// canonical backing store the toolkit operates on.
    pixels: Vec<u8>,
    /// Last bytes successfully uploaded into the server's
    /// pool storage. Keeping a shadow avoids transporting
    /// every unchanged pixel on each commit while retaining
    /// the explicit v1 `pmd_shm_pool.write` protocol.
    uploaded_pixels: Vec<u8>,
    pool_id: ObjectId,
    buffers: [ObjectId; 2],
    /// Index of the "back" buffer — the one
    /// [`BufferPool::acquire_back_canvas`] will hand to
    /// the caller. Flipped by
    /// [`BufferPool::commit_and_swap`] after each commit.
    back_index: usize,
    /// Per-buffer in-use flag. A buffer is "in use" from
    /// the moment its id lands in
    /// `pmd_surface.attach(buffer_id, ...)` until the
    /// server emits `pmd_buffer.release` and the caller
    /// routes it through [`BufferPool::handle_release`].
    attached_in_use: [bool; 2],
    /// Callback ids currently outstanding on
    /// `pmd_surface.frame(new_id)`. Callers that invoke
    /// [`BufferPool::request_frame`] push the allocated id
    /// here; [`BufferPool::handle_frame_done`] pops
    /// matching ids. Recorded so a future slice can
    /// auto-drive paint on frame-done without reshaping
    /// the public API.
    pending_frames: Vec<ObjectId>,
    pending_commit: Option<PendingCommit>,
    /// Slot installed by the most recently completed attach/commit
    /// transaction. `back_index` always remains the alternate paint target.
    current_index: Option<usize>,
    /// Surface that received `current_index`; prevents a pool from mirroring a
    /// patch sent through an unrelated [`Window`].
    current_surface: Option<ObjectId>,
}

impl BufferPool {
    /// Create a double-buffered pool on the given [`App`]'s
    /// `pmd_shm` global, sized `width × height` pixels.
    ///
    /// Allocates the pool object via
    /// `pmd_shm.create_pool(new_id, size)` with
    /// `size = 2 * width * height * 4` and then allocates
    /// two buffer objects via
    /// `pmd_shm_pool.create_buffer(new_id, offset, width,
    /// height, stride, format)` at offsets `0` and
    /// `width * height * 4`. Returns the fully-initialised
    /// pool, ready for the caller to
    /// [`BufferPool::acquire_back_canvas`] and paint.
    /// Representable geometry with a zero width or height creates a zero-byte
    /// pool, and commits against that pool normalize to a no-op. Width must
    /// still admit the protocol's `width * 4` stride when height is zero.
    ///
    /// # Errors
    ///
    /// Returns `Wire(Overflow)` before allocating an object id or sending a
    /// request when the stride, one-buffer byte count, or double-buffered pool
    /// size cannot be represented by the protocol's `u32` fields. Otherwise
    /// propagates [`ClientError`] from the underlying request sends —
    /// `UnknownObject` if the app's shm global isn't bound and `IdsExhausted`
    /// if the client's id allocator is full.
    pub fn new<C: Connection>(
        app: &mut App<C>,
        width: u32,
        height: u32,
    ) -> Result<Self, ClientError> {
        let stride = width
            .checked_mul(BYTES_PER_PIXEL as u32)
            .ok_or(ClientError::Wire(WireError::Overflow))?;
        let per_buffer_bytes = stride
            .checked_mul(height)
            .ok_or(ClientError::Wire(WireError::Overflow))?;
        let pool_size = per_buffer_bytes
            .checked_mul(2)
            .ok_or(ClientError::Wire(WireError::Overflow))?;
        let shm_id = app.shm();
        let client = app.client_mut();
        let pool_id = client.shm_create_pool(shm_id, pool_size)?;
        let buffer_0 = match client.shm_pool_create_buffer(
            pool_id,
            0,
            width,
            height,
            stride,
            buffer_format::ARGB8888,
        ) {
            Ok(buffer) => buffer,
            Err(error) => {
                let _ = client.shm_pool_destroy(pool_id);
                return Err(error);
            }
        };
        let buffer_1 = match client.shm_pool_create_buffer(
            pool_id,
            per_buffer_bytes,
            width,
            height,
            stride,
            buffer_format::ARGB8888,
        ) {
            Ok(buffer) => buffer,
            Err(error) => {
                let _ = client.buffer_destroy(buffer_0);
                let _ = client.shm_pool_destroy(pool_id);
                return Err(error);
            }
        };
        let pixels = vec![0u8; pool_size as usize];
        Ok(BufferPool {
            width,
            height,
            uploaded_pixels: pixels.clone(),
            pixels,
            pool_id,
            buffers: [buffer_0, buffer_1],
            back_index: 0,
            attached_in_use: [false; 2],
            pending_frames: Vec::new(),
            pending_commit: None,
            current_index: None,
            current_surface: None,
        })
    }

    /// Create a new pool in `slot` and explicitly retire the previous pool's
    /// protocol objects. Creation happens first so a failed replacement leaves
    /// the currently displayed pool untouched. The server retains destroyed
    /// backing while the surface still references it and reclaims it after the
    /// replacement's first attach/commit.
    pub fn replace<C: Connection>(
        slot: &mut Option<Self>,
        app: &mut App<C>,
        width: u32,
        height: u32,
    ) -> Result<(), ClientError> {
        let replacement = Self::new(app, width, height)?;
        if let Some(retired) = slot.replace(replacement) {
            retired.destroy(app)?;
        }
        Ok(())
    }

    /// Send destroy requests for both buffers followed by their pool. The
    /// method consumes `self` so callers cannot accidentally keep painting into
    /// locally retired storage. Every request is attempted; the first error is
    /// returned after best-effort cleanup of the remaining objects.
    pub fn destroy<C: Connection>(self, app: &mut App<C>) -> Result<(), ClientError> {
        let mut first_error = None;
        let client = app.client_mut();
        for buffer_id in self.buffers {
            if let Err(error) = client.buffer_destroy(buffer_id) {
                first_error.get_or_insert(error);
            }
        }
        if let Err(error) = client.shm_pool_destroy(self.pool_id) {
            first_error.get_or_insert(error);
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Pool width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Pool height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Byte stride of a single row in a buffer.
    pub fn stride(&self) -> u32 {
        self.width * BYTES_PER_PIXEL as u32
    }

    /// Total pool size in bytes — `2 * width * height * 4`.
    pub fn size(&self) -> u32 {
        self.stride() * self.height * 2
    }

    /// The `pmd_shm_pool` object id. Escape hatch for
    /// downstream users needing to send `resize` /
    /// `destroy`.
    pub fn pool_id(&self) -> ObjectId {
        self.pool_id
    }

    /// The `pmd_buffer` id at the given pool index (0 or
    /// 1). Panics on any other index.
    pub fn buffer_id(&self, index: usize) -> ObjectId {
        self.buffers[index]
    }

    /// Current back buffer index (0 or 1). Exposed for
    /// tests and diagnostic callers; paint flows via
    /// [`BufferPool::acquire_back_canvas`] do not need it.
    pub fn back_index(&self) -> usize {
        self.back_index
    }

    /// Pool slot currently attached to the surface, if this pool has
    /// completed at least one attach/commit transaction.
    pub fn current_index(&self) -> Option<usize> {
        self.current_index
    }

    /// True iff the buffer at `index` is currently
    /// attached to a surface and awaiting
    /// `pmd_buffer.release`.
    pub fn is_in_use(&self, index: usize) -> bool {
        self.attached_in_use[index]
    }

    /// Byte offset of buffer `index` inside the shm pool.
    pub fn buffer_offset(&self, index: usize) -> usize {
        match index {
            0 => 0,
            1 => (self.width * self.height) as usize * BYTES_PER_PIXEL,
            other => panic!("buffer index {other} out of range (0 or 1)"),
        }
    }

    /// Borrow the back-buffer byte slice for inspection —
    /// useful in tests. Length is
    /// `width * height * BYTES_PER_PIXEL`.
    pub fn back_buffer(&self) -> &[u8] {
        let start = self.buffer_offset(self.back_index);
        let end = start + self.per_buffer_bytes();
        &self.pixels[start..end]
    }

    /// Acquire a [`Canvas`] view onto the alternate back buffer so the caller
    /// can paint. Returns `None` while a local commit is still staged. The
    /// `attached_in_use` flags remain release diagnostics; FIFO commit ordering
    /// makes the selected alternate writable before a lagging release event is
    /// observed locally. Live current-buffer changes use the atomic patch path.
    pub fn acquire_back_canvas(&mut self) -> Option<Canvas<'_>> {
        if self.pending_commit.is_some() {
            return None;
        }
        // Server-side current backing is immutable to ordinary pool writes.
        // `back_index` names the alternate selected by the prior FIFO commit;
        // even if its release event has not reached the client yet, the server
        // processes that promotion before any later upload on this stream.
        // Callers must never retarget this path at `current_index`; updates to
        // current pixels go through the single-request patch protocol.
        let offset = self.buffer_offset(self.back_index);
        let len = self.per_buffer_bytes();
        let slice = &mut self.pixels[offset..offset + len];
        Some(Canvas::from_slice(slice, self.width, self.height))
    }

    /// Attach the back buffer to `window`'s surface, damage
    /// the full buffer region, and commit. Flips the
    /// `back_index` and marks the just-attached buffer as
    /// in-use, pending `buffer.release` from the server.
    ///
    /// This is the "publish the newly-painted frame" call.
    /// The caller typically invokes
    /// [`BufferPool::acquire_back_canvas`] → paint → this.
    ///
    /// Takes `&mut Window` rather than a separate
    /// `(&mut App, &Window)` pair so the caller can stack
    /// Window on top of App without fighting the borrow
    /// checker — Window already owns the mutable reference
    /// to its App, and the pool only needs the surface id
    /// from the window plus a mutable handle to the
    /// client.
    ///
    /// # Errors
    ///
    /// Propagates [`ClientError`] from the three inner
    /// request sends (`surface.attach`, `surface.damage`,
    /// `surface.commit`).
    pub fn commit_and_swap<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
    ) -> Result<CommitProgress, ClientError> {
        self.commit(window, true)
    }

    /// Publish the painted back buffer without advancing to the alternate
    /// slot. This is appropriate for initial/fixture publication where the
    /// slot is not already visible. Live multi-request repaints must use an
    /// initialized alternate slot plus [`Self::commit_and_swap_damage`], so an
    /// unrelated compositor pass cannot observe partially uploaded pixels.
    pub fn commit_in_place<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
    ) -> Result<CommitProgress, ClientError> {
        self.commit(window, false)
    }

    /// Publish an in-place repaint whose modified pixels are contained by
    /// `damage`. The rectangle is clipped to the buffer. Only protocol upload
    /// chunks intersecting the clipped region are compared and transported,
    /// while the surface receives the exact clipped damage rectangle.
    ///
    /// An empty or wholly out-of-bounds rectangle is a no-op. Callers must not
    /// modify pixels outside `damage` before invoking this method. This method
    /// does not make a multi-request upload to an already attached slot
    /// transactional; live surfaces should prefer [`Self::commit_and_swap_damage`].
    pub fn commit_in_place_damage<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
        damage: Rect,
    ) -> Result<CommitProgress, ClientError> {
        self.commit_region(window, false, damage, true)
    }

    /// Publish a damage-bounded repaint from the unattached back buffer, then
    /// advance to the alternate slot. Packed-row uploads stay proportional to
    /// the changed rectangle while the currently displayed buffer remains
    /// immutable until the final attach + commit transaction.
    pub fn commit_and_swap_damage<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
        damage: Rect,
    ) -> Result<CommitProgress, ClientError> {
        self.commit_region(window, true, damage, true)
    }

    /// Publish a repaint described by multiple damage rectangles from the
    /// unattached back buffer, then advance to the alternate slot. Rectangles
    /// are clipped to the buffer and normalized into non-overlapping regions
    /// before upload, so overlapping caller damage never transports the same
    /// pixels twice. Every [`Self::progress_commit`] call still produces at
    /// most one [`SHM_WRITE_CHUNK_BYTES`]-bounded upload request; after all
    /// regions are staged, the client queues each `surface.damage` followed by
    /// one `surface.commit` transaction.
    pub fn commit_and_swap_damage_regions<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
        damages: &[Rect],
    ) -> Result<CommitProgress, ClientError> {
        self.commit_regions(window, true, damages, true)
    }

    /// Send one atomic `pmd_surface.patch_current` request and mirror the
    /// packed pixels into this pool's local and uploaded shadows.
    ///
    /// This does not touch `back_index`, attach a buffer, or stage a shm-pool
    /// write. A normal staged commit or outbound transport suffix defers the
    /// patch so `current` has one unambiguous protocol meaning at dispatch.
    pub fn patch_current<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
        damage: Rect,
        packed_pixels: &[u8],
    ) -> Result<CurrentPatch, ClientError> {
        if self.pending_commit.is_some() || window.outbound_pending() {
            return Ok(CurrentPatch::Deferred {
                buffer_index: self.current_index,
            });
        }
        let Some(buffer_index) = self.current_index else {
            return Ok(CurrentPatch::Unavailable);
        };
        let surface_id = window.surface();
        if self.current_surface != Some(surface_id) {
            return Ok(CurrentPatch::Unavailable);
        }

        let x = usize::try_from(damage.x).ok();
        let y = usize::try_from(damage.y).ok();
        let valid_extent = x
            .zip(y)
            .and_then(|(x, y)| {
                x.checked_add(damage.width as usize)
                    .zip(y.checked_add(damage.height as usize))
            })
            .is_some_and(|(right, bottom)| {
                right <= self.width as usize && bottom <= self.height as usize
            });
        let expected = (damage.width as usize)
            .checked_mul(damage.height as usize)
            .and_then(|area| area.checked_mul(BYTES_PER_PIXEL));
        if damage.is_empty()
            || !valid_extent
            || expected != Some(packed_pixels.len())
            || packed_pixels.len() > display_proto::MAX_SURFACE_PATCH_BYTES
        {
            return Err(ClientError::InvalidSurfacePatch {
                x: damage.x,
                y: damage.y,
                width: damage.width,
                height: damage.height,
                pixels_len: packed_pixels.len(),
            });
        }

        window.app_mut().client_mut().surface_patch_current(
            surface_id,
            damage.x,
            damage.y,
            damage.width,
            damage.height,
            packed_pixels,
        )?;

        let x = x.expect("validated non-negative patch x");
        let y = y.expect("validated non-negative patch y");
        let row_bytes = damage.width as usize * BYTES_PER_PIXEL;
        let stride = self.width as usize * BYTES_PER_PIXEL;
        let buffer_offset = self.buffer_offset(buffer_index);
        for row in 0..damage.height as usize {
            let source_start = row * row_bytes;
            let destination_start = buffer_offset + (y + row) * stride + x * BYTES_PER_PIXEL;
            let source = &packed_pixels[source_start..source_start + row_bytes];
            self.pixels[destination_start..destination_start + row_bytes].copy_from_slice(source);
            self.uploaded_pixels[destination_start..destination_start + row_bytes]
                .copy_from_slice(source);
        }
        Ok(CurrentPatch::Patched { buffer_index })
    }

    /// Whether a frame upload has more bounded work to perform before its
    /// attach/damage/commit transaction can be queued.
    pub fn commit_pending(&self) -> bool {
        self.pending_commit.is_some()
    }

    /// Advance a staged commit by at most one 24 KiB upload request. Linear
    /// uploads may skip up to eight consecutive unchanged chunks before
    /// yielding, bounding comparison work to 192 KiB per call while avoiding
    /// one event-loop turn per unchanged chunk.
    ///
    /// A production fd transport may retain that request while its socket is
    /// backpressured. In that case no later chunk is produced until the
    /// connection reports its outbound queue empty, keeping both CPU work and
    /// retained bytes bounded per event-loop turn.
    ///
    /// Returns [`ClientError::BufferPoolSurfaceMismatch`] before doing work if
    /// `window` is not the surface that originated the staged transaction.
    pub fn progress_commit<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
    ) -> Result<CommitProgress, ClientError> {
        let Some(pending_surface) = self
            .pending_commit
            .as_ref()
            .map(|pending| pending.surface_id)
        else {
            return Ok(CommitProgress::Committed);
        };
        let actual_surface = window.surface();
        if actual_surface != pending_surface {
            return Err(ClientError::BufferPoolSurfaceMismatch {
                expected_surface: pending_surface,
                actual_surface,
            });
        }
        if window.outbound_pending() {
            return Ok(CommitProgress::Pending);
        }

        let mut pending = self.pending_commit.take().expect("checked above");
        let bytes_per_buf = self.per_buffer_bytes();
        let buf_offset = pending.buffer_index * bytes_per_buf;
        let stride = self.width as usize * BYTES_PER_PIXEL;
        let upload_complete = match pending.upload {
            UploadCursor::Linear { mut written } => {
                let mut unchanged_chunks = 0usize;
                loop {
                    let end = core::cmp::min(written + SHM_WRITE_CHUNK_BYTES, bytes_per_buf);
                    let start = buf_offset + written;
                    let absolute_end = buf_offset + end;
                    if self.pixels[start..absolute_end] != self.uploaded_pixels[start..absolute_end]
                    {
                        let payload = self.pixels[start..absolute_end].to_vec();
                        pending.upload = UploadCursor::Linear { written };
                        if let Err(error) = window.app_mut().client_mut().shm_pool_write(
                            self.pool_id,
                            start as u32,
                            &payload,
                        ) {
                            self.pending_commit = Some(pending);
                            return Err(error);
                        }
                        self.uploaded_pixels[start..absolute_end].copy_from_slice(&payload);
                        pending.upload = UploadCursor::Linear { written: end };
                        break end == bytes_per_buf;
                    }

                    written = end;
                    pending.upload = UploadCursor::Linear { written };
                    if end == bytes_per_buf {
                        break true;
                    }
                    unchanged_chunks += 1;
                    if unchanged_chunks == MAX_UNCHANGED_LINEAR_CHUNKS_PER_PROGRESS {
                        break false;
                    }
                }
            }
            UploadCursor::Rows {
                damage_index,
                tile_x,
                tile_y,
            } => {
                let damage = pending.damages[damage_index];
                let remaining_row_bytes = (damage.x_end - tile_x) * BYTES_PER_PIXEL;
                let tile_row_bytes = remaining_row_bytes.min(SHM_WRITE_CHUNK_BYTES);
                let tile_pixels = tile_row_bytes / BYTES_PER_PIXEL;
                let rows_per_request = (SHM_WRITE_CHUNK_BYTES / tile_row_bytes).max(1);
                let row_count = rows_per_request.min(damage.y_end - tile_y);
                let mut payload = Vec::with_capacity(tile_row_bytes * row_count);
                let mut changed = false;
                for row in tile_y..tile_y + row_count {
                    let start = buf_offset + row * stride + tile_x * BYTES_PER_PIXEL;
                    let end = start + tile_row_bytes;
                    payload.extend_from_slice(&self.pixels[start..end]);
                    changed |= self.pixels[start..end] != self.uploaded_pixels[start..end];
                }
                if changed {
                    let pool_offset = buf_offset + tile_y * stride + tile_x * BYTES_PER_PIXEL;
                    if let Err(error) = window.app_mut().client_mut().shm_pool_write_rows(
                        self.pool_id,
                        pool_offset as u32,
                        tile_row_bytes as u32,
                        row_count as u32,
                        stride as u32,
                        &payload,
                    ) {
                        self.pending_commit = Some(pending);
                        return Err(error);
                    }
                    for (payload_row, row) in (tile_y..tile_y + row_count).enumerate() {
                        let start = buf_offset + row * stride + tile_x * BYTES_PER_PIXEL;
                        let end = start + tile_row_bytes;
                        let payload_start = payload_row * tile_row_bytes;
                        self.uploaded_pixels[start..end].copy_from_slice(
                            &payload[payload_start..payload_start + tile_row_bytes],
                        );
                    }
                }

                let next_y = tile_y + row_count;
                if next_y < damage.y_end {
                    pending.upload = UploadCursor::Rows {
                        damage_index,
                        tile_x,
                        tile_y: next_y,
                    };
                    false
                } else {
                    let next_x = tile_x + tile_pixels;
                    if next_x < damage.x_end {
                        pending.upload = UploadCursor::Rows {
                            damage_index,
                            tile_x: next_x,
                            tile_y: damage.y_start,
                        };
                        false
                    } else if let Some(next_damage) = pending.damages.get(damage_index + 1) {
                        pending.upload = UploadCursor::Rows {
                            damage_index: damage_index + 1,
                            tile_x: next_damage.x_start,
                            tile_y: next_damage.y_start,
                        };
                        false
                    } else {
                        true
                    }
                }
            }
        };

        if !upload_complete {
            self.pending_commit = Some(pending);
            return Ok(CommitProgress::Pending);
        }

        let surface_id = pending.surface_id;
        let buffer_id = self.buffers[pending.buffer_index];
        let client = window.app_mut().client_mut();
        client.surface_attach(surface_id, buffer_id, 0, 0)?;
        for damage in &pending.damages {
            client.surface_damage(
                surface_id,
                damage.x_start as i32,
                damage.y_start as i32,
                (damage.x_end - damage.x_start) as i32,
                (damage.y_end - damage.y_start) as i32,
            )?;
        }
        client.surface_commit(surface_id)?;
        self.attached_in_use[pending.buffer_index] = true;
        self.current_index = Some(pending.buffer_index);
        self.current_surface = Some(surface_id);
        if pending.swap {
            self.back_index = 1 - pending.buffer_index;
        }
        Ok(CommitProgress::Committed)
    }

    fn commit<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
        swap: bool,
    ) -> Result<CommitProgress, ClientError> {
        self.commit_region(
            window,
            swap,
            Rect::new(0, 0, self.width, self.height),
            false,
        )
    }

    fn commit_region<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
        swap: bool,
        damage: Rect,
        packed_rows: bool,
    ) -> Result<CommitProgress, ClientError> {
        self.commit_regions(window, swap, core::slice::from_ref(&damage), packed_rows)
    }

    fn commit_regions<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
        swap: bool,
        damages: &[Rect],
        packed_rows: bool,
    ) -> Result<CommitProgress, ClientError> {
        if self.pending_commit.is_some() {
            return Err(ClientError::CommitInProgress);
        }
        let surface_id = window.surface();
        if let Some(expected_surface) = self.current_surface {
            if expected_surface != surface_id {
                return Err(ClientError::BufferPoolSurfaceMismatch {
                    expected_surface,
                    actual_surface: surface_id,
                });
            }
        }
        if damages.len() > MAX_DAMAGE_REGIONS {
            return Err(ClientError::TooManyDamageRegions {
                supplied: damages.len(),
                max: MAX_DAMAGE_REGIONS,
            });
        }
        let damages = normalize_damage_rects(damages, self.width, self.height);
        if damages.is_empty() {
            return Ok(CommitProgress::Committed);
        }
        let upload = if packed_rows {
            let first = damages[0];
            UploadCursor::Rows {
                damage_index: 0,
                tile_x: first.x_start,
                tile_y: first.y_start,
            }
        } else {
            UploadCursor::Linear { written: 0 }
        };
        self.pending_commit = Some(PendingCommit {
            surface_id,
            buffer_index: self.back_index,
            swap,
            damages,
            upload,
        });

        let incremental = window
            .app_mut()
            .client_mut()
            .connection()
            .incremental_uploads();
        let mut progress = self.progress_commit(window)?;
        while !incremental && progress == CommitProgress::Pending {
            progress = self.progress_commit(window)?;
        }
        Ok(progress)
    }

    /// Handle a `pmd_buffer.release` event for one of this
    /// pool's buffers. Returns `true` iff `buffer_id`
    /// belonged to this pool and a flag flipped from
    /// in-use to free; `false` for unrelated ids so the
    /// caller can route the event elsewhere.
    pub fn handle_release(&mut self, buffer_id: ObjectId) -> bool {
        for (i, id) in self.buffers.iter().enumerate() {
            if *id == buffer_id {
                let was_in_use = self.attached_in_use[i];
                self.attached_in_use[i] = false;
                return was_in_use;
            }
        }
        false
    }

    /// Request a one-shot frame callback on `window`'s
    /// surface. Allocates a fresh client-side id, sends
    /// `pmd_surface.frame(new_id)`, and records the id in
    /// the pool's `pending_frames` list.
    ///
    /// Returns the allocated callback id so the caller can
    /// track it across multiple in-flight frames.
    ///
    /// Takes `&mut Window` matching
    /// [`BufferPool::commit_and_swap`] for consistency (and
    /// the same borrow-checker reasoning).
    pub fn request_frame<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
    ) -> Result<ObjectId, ClientError> {
        let surface_id = window.surface();
        let client = window.app_mut().client_mut();
        let callback_id = client.surface_frame(surface_id)?;
        self.pending_frames.push(callback_id);
        Ok(callback_id)
    }

    /// Mark a previously-allocated callback id as done,
    /// removing it from the `pending_frames` list.
    /// Returns `true` iff the id was in the list.
    ///
    /// Callers observe `pmd_callback.done` by scanning
    /// dispatch output for an event targeting a known
    /// pending id; a future slice moves this matching
    /// logic inside the pool.
    pub fn handle_frame_done(&mut self, callback_id: ObjectId) -> bool {
        if let Some(pos) = self.pending_frames.iter().position(|id| *id == callback_id) {
            self.pending_frames.remove(pos);
            true
        } else {
            false
        }
    }

    /// Remove a frame callback that the server cancelled with
    /// `pmd_display.delete_id` before sending `pmd_callback.done` (for example,
    /// because its surface was destroyed). This does not imply presentation.
    /// After a normal done path it returns false because [`Self::handle_frame_done`]
    /// already removed the callback.
    pub fn handle_frame_deleted(&mut self, callback_id: ObjectId) -> bool {
        if let Some(pos) = self.pending_frames.iter().position(|id| *id == callback_id) {
            self.pending_frames.remove(pos);
            true
        } else {
            false
        }
    }

    /// Number of frame callbacks currently outstanding.
    pub fn pending_frames(&self) -> usize {
        self.pending_frames.len()
    }

    // ---- internal helpers --------------------------------

    fn per_buffer_bytes(&self) -> usize {
        (self.width * self.height) as usize * BYTES_PER_PIXEL
    }
}
