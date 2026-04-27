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
//!   carries a `size` in bytes but no actual fd passing —
//!   v1 pushes pixel bytes inline as the commit payload in
//!   a future slice. For the toolkit unit tests here the
//!   in-memory `pixels` vec IS the pool backing.
//! * **Frame-callback integration.** Helpers exist
//!   ([`BufferPool::request_frame`] +
//!   [`BufferPool::handle_frame_done`]) to send
//!   `pmd_surface.frame(new_id)` and to mark a recorded
//!   callback id done, but nothing auto-drives the paint
//!   loop on frame-done — that's the next slice.
//! * **Damage tracking.** Every
//!   [`BufferPool::commit_and_swap`] damages the full
//!   buffer region (`0, 0, width, height`). Partial damage
//!   is a future optimisation.
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

use crate::app::App;
use crate::draw::{Canvas, BYTES_PER_PIXEL};
use crate::protocol::{ClientError, Connection};
use crate::window::Window;

/// A double-buffered pixel pool backed by a
/// `pmd_shm_pool`.
///
/// See the module-level doc for the full v1 shape. Fields
/// are private; construct via [`BufferPool::new`] and
/// interact through the public API.
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
    ///
    /// # Errors
    ///
    /// Propagates [`ClientError`] from the underlying
    /// request sends — `UnknownObject` if the app's shm
    /// global isn't bound, `IdsExhausted` if the client's
    /// id allocator is full, `Wire` on encoding overflow.
    pub fn new<C: Connection>(
        app: &mut App<C>,
        width: u32,
        height: u32,
    ) -> Result<Self, ClientError> {
        let stride = width * BYTES_PER_PIXEL as u32;
        let per_buffer_bytes = stride * height;
        let pool_size = per_buffer_bytes
            .checked_mul(2)
            .expect("buffer pool size overflows u32");
        let shm_id = app.shm();
        let client = app.client_mut();
        let pool_id = client.shm_create_pool(shm_id, pool_size)?;
        let buffer_0 = client.shm_pool_create_buffer(
            pool_id,
            0,
            width,
            height,
            stride,
            buffer_format::ARGB8888,
        )?;
        let buffer_1 = client.shm_pool_create_buffer(
            pool_id,
            per_buffer_bytes,
            width,
            height,
            stride,
            buffer_format::ARGB8888,
        )?;
        Ok(BufferPool {
            width,
            height,
            pixels: vec![0u8; pool_size as usize],
            pool_id,
            buffers: [buffer_0, buffer_1],
            back_index: 0,
            attached_in_use: [false; 2],
            pending_frames: Vec::new(),
        })
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

    /// Acquire a [`Canvas`] view onto the back buffer so
    /// the caller can paint. Returns `None` when the back
    /// buffer is currently attached + awaiting release —
    /// the caller should run another dispatch pass to
    /// collect the outstanding `buffer.release` event
    /// before retrying.
    ///
    /// In v1 the "both attached" back-pressure case only
    /// happens in tests, since real apps block on a frame
    /// callback before repainting. The `Option` return is
    /// the forward-compatible signature for a future slice
    /// that adds a real frame loop.
    pub fn acquire_back_canvas(&mut self) -> Option<Canvas<'_>> {
        // The in-use flag was a Wayland-style back-pressure
        // signal saying "the server is still reading from
        // this buffer; wait for buffer.release before
        // overwriting." It deadlocked on the v1 path because:
        //
        // (a) the display-server's `shm_pool.write` dispatch
        //     already copies the painted bytes out into its
        //     own pool storage *before* attach, so the
        //     client's local buffer pixels are no longer
        //     load-bearing once `commit_and_swap` returns;
        //
        // (b) the server now re-composites the WHOLE scene
        //     on every commit (so it keeps reading from
        //     `current_buffer`'s pool storage indefinitely,
        //     not just once per attach).
        //
        // Together these mean the in-use flag never matched
        // the actual ownership: every paint marked the
        // buffer in_use, but the buffer's pixels had already
        // been consumed by shm_pool.write, and the
        // buffer.release events arrived after the server
        // had moved on. After both pool slots got marked
        // in_use the shell's acquire_back_canvas returned
        // None forever and the desktop froze on whatever
        // frame it last managed to paint.
        //
        // Drop the gate entirely: the toolkit always lets
        // the caller paint into the back slot. Even if the
        // server were still reading the buffer's bytes
        // (which it isn't in v1) the worst case is one
        // partly-torn frame, not a permanent hang.
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
    ) -> Result<(), ClientError> {
        let surface_id = window.surface();
        let buffer_id = self.buffers[self.back_index];
        let w = self.width as i32;
        let h = self.height as i32;
        let pool_id = self.pool_id;
        let bytes_per_buf = (self.width as usize) * (self.height as usize) * 4;
        let buf_offset = self.back_index * bytes_per_buf;
        // V1 affordance: pmd_shm.create_pool elides the fd, so
        // the server's pool storage starts empty. Push the
        // painted pixels for this buffer into the server's pool
        // BEFORE the attach so the compositor's blit sees the
        // painted bytes. The toolkit chunks the write at 24 KiB
        // per syscall to fit the SAB ring's 32 KiB heap window
        // (HEAP_SCRATCH_BYTES); each chunk is one
        // `pmd_shm_pool.write(offset, bytes)` request.
        const SHM_WRITE_CHUNK_BYTES: usize = 24 * 1024;
        let pixels_copy: Vec<u8> = self.pixels[buf_offset..buf_offset + bytes_per_buf].to_vec();
        let client = window.app_mut().client_mut();
        let mut written = 0usize;
        while written < pixels_copy.len() {
            let end = core::cmp::min(written + SHM_WRITE_CHUNK_BYTES, pixels_copy.len());
            let chunk = &pixels_copy[written..end];
            client.shm_pool_write(pool_id, (buf_offset + written) as u32, chunk)?;
            written = end;
        }
        client.surface_attach(surface_id, buffer_id, 0, 0)?;
        client.surface_damage(surface_id, 0, 0, w, h)?;
        client.surface_commit(surface_id)?;
        self.attached_in_use[self.back_index] = true;
        self.back_index = 1 - self.back_index;
        Ok(())
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
    /// The allocated id is NOT bound to an interface in
    /// the client's object table — v1 has no
    /// `Interface::Callback` variant, and the `done` event
    /// is matched against the `pending_frames` list by id
    /// rather than interface dispatch. A future slice that
    /// formalises `pmd_callback` will promote this to a
    /// typed object.
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
        let callback_id = client.allocate_id()?;
        let payload = callback_id.raw().to_le_bytes();
        client.send_request(surface_id, 4 /* frame */, &payload)?;
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

    /// Number of frame callbacks currently outstanding.
    pub fn pending_frames(&self) -> usize {
        self.pending_frames.len()
    }

    // ---- internal helpers --------------------------------

    fn per_buffer_bytes(&self) -> usize {
        (self.width * self.height) as usize * BYTES_PER_PIXEL
    }
}
