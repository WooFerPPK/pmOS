#![cfg_attr(not(test), no_std)]

//! PMos SAB ring-buffer transport.
//!
//! Logical producer/consumer rings layered over a pre-allocated byte
//! buffer that is, at runtime, a `SharedArrayBuffer` shared between
//! the kernel Worker and a user-process Worker. Every accessor here
//! operates on a `&[u8]` slice, so the same code runs unchanged
//! against:
//!
//! * the real SAB at runtime (via a `SharedArrayBuffer`-backed
//!   slice constructed on the JS side and handed in as a pointer),
//!   and
//! * a plain `Vec<u8>` in host-target `cargo test`, which is how
//!   per-layer isolation tests work without a browser.
//!
//! This crate intentionally does **not** do the blocking
//! `Atomics.wait` / `Atomics.notify` hookup — that is platform-
//! specific (the kernel Worker uses JS `Atomics`, user processes
//! use the WASM `memory.atomic.wait32` instruction, native tests
//! busy-spin). Callers get:
//!
//! * non-blocking [`RequestProducer::try_push`] and
//!   [`RequestConsumer::try_pop`] for the request side,
//! * the same pair for the response side, and
//! * atomic slot getters/setters for the wait slots so callers
//!   can implement their own blocking wrapper.
//!
//! Ordering guarantees: every head/tail update uses `Release` on
//! the writer and `Acquire` on the reader, so a successful pop
//! happens-after the push that produced that slot's contents.

use core::sync::atomic::{AtomicU32, Ordering};

use abi::ring::{
    Request, Response, OFF_KERNEL_BLOCK_COUNT, OFF_KERNEL_WAIT_SLOT, OFF_REQ_HEAD, OFF_REQ_RING,
    OFF_REQ_TAIL, OFF_RES_HEAD, OFF_RES_RING, OFF_RES_TAIL, OFF_USER_BLOCK_COUNT,
    OFF_USER_WAIT_SLOT, REQ_SLOT_COUNT, RES_SLOT_COUNT, SAB_SIZE, SLOT_SIZE,
};

/// Wraps a byte slice whose layout follows `abi::ring`. The slice
/// is assumed to live at least as long as the `Sab` value.
///
/// SAFETY: the caller guarantees that the pointed-to memory is
/// exactly `SAB_SIZE` bytes long, is properly aligned for atomic
/// `u32` accesses at every `OFF_*` offset defined in `abi::ring`,
/// and is not aliased by anything that doesn't also go through
/// atomic operations.
pub struct Sab {
    base: *mut u8,
    len: usize,
}

// The SAB is intentionally shared between threads/Workers; the
// atomic accesses below are what makes this sound.
unsafe impl Send for Sab {}
unsafe impl Sync for Sab {}

impl Sab {
    /// Construct from a raw pointer + length. Caller upholds the
    /// safety contract documented on the struct.
    ///
    /// # Safety
    ///
    /// See [`Sab`] docs.
    pub unsafe fn from_raw(base: *mut u8, len: usize) -> Self {
        debug_assert_eq!(len, SAB_SIZE);
        Sab { base, len }
    }

    /// Construct from a mutable byte slice. The slice must be at
    /// least `SAB_SIZE` bytes long.
    pub fn from_slice(slice: &mut [u8]) -> Self {
        assert!(slice.len() >= SAB_SIZE, "SAB slice is too small");
        Sab {
            base: slice.as_mut_ptr(),
            len: slice.len(),
        }
    }

    /// Zero the header region (req_head, req_tail, res_head, res_tail,
    /// wait slots, block counts). Slot contents are not touched.
    pub fn init(&mut self) {
        for off in [
            OFF_REQ_HEAD,
            OFF_REQ_TAIL,
            OFF_RES_HEAD,
            OFF_RES_TAIL,
            OFF_USER_WAIT_SLOT,
            OFF_KERNEL_WAIT_SLOT,
            OFF_USER_BLOCK_COUNT,
            OFF_KERNEL_BLOCK_COUNT,
        ] {
            self.atomic_u32(off).store(0, Ordering::Release);
        }
    }

    fn atomic_u32(&self, offset: usize) -> &AtomicU32 {
        assert!(offset + 4 <= self.len);
        assert_eq!(offset % 4, 0, "offset not 4-aligned");
        // The base pointer must also be 4-aligned for the cast to
        // `*const AtomicU32` to be sound. Every real allocator we
        // target (glibc, musl, jemalloc, the WASM linear memory) gives
        // ≥8-byte alignment for heap allocations, so this assertion
        // exists to fail loudly on any platform that ever breaks the
        // assumption rather than to silently invoke UB.
        assert_eq!(
            (self.base as usize) % 4,
            0,
            "SAB base pointer must be 4-byte aligned for atomic u32 access",
        );
        // SAFETY: Sab's invariant plus the runtime checks above
        // guarantee the pointer is in-bounds and properly aligned.
        // AtomicU32 has the same layout as u32.
        unsafe { &*(self.base.add(offset) as *const AtomicU32) }
    }

    /// Copy `slot_bytes` bytes into the ring slot at (ring_off + slot_ix * SLOT_SIZE).
    fn write_slot(&self, ring_off: usize, slot_ix: usize, bytes: &[u8]) {
        assert_eq!(bytes.len(), SLOT_SIZE);
        let at = ring_off + slot_ix * SLOT_SIZE;
        assert!(at + SLOT_SIZE <= self.len);
        // SAFETY: in-bounds, writer/reader are coordinated by the
        // head/tail atomics with Release/Acquire.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(at), SLOT_SIZE);
        }
    }

    /// Read `SLOT_SIZE` bytes out of the ring slot at (ring_off + slot_ix * SLOT_SIZE).
    fn read_slot(&self, ring_off: usize, slot_ix: usize, out: &mut [u8]) {
        assert_eq!(out.len(), SLOT_SIZE);
        let at = ring_off + slot_ix * SLOT_SIZE;
        assert!(at + SLOT_SIZE <= self.len);
        // SAFETY: in-bounds, coordinated as above.
        unsafe {
            core::ptr::copy_nonoverlapping(self.base.add(at), out.as_mut_ptr(), SLOT_SIZE);
        }
    }

    // --- wait slot accessors (callers implement blocking) --------------

    pub fn user_wait_slot(&self) -> &AtomicU32 {
        self.atomic_u32(OFF_USER_WAIT_SLOT)
    }

    pub fn kernel_wait_slot(&self) -> &AtomicU32 {
        self.atomic_u32(OFF_KERNEL_WAIT_SLOT)
    }

    pub fn user_block_count(&self) -> &AtomicU32 {
        self.atomic_u32(OFF_USER_BLOCK_COUNT)
    }

    pub fn kernel_block_count(&self) -> &AtomicU32 {
        self.atomic_u32(OFF_KERNEL_BLOCK_COUNT)
    }

    // --- head/tail accessors -------------------------------------------

    fn req_head(&self) -> &AtomicU32 {
        self.atomic_u32(OFF_REQ_HEAD)
    }
    fn req_tail(&self) -> &AtomicU32 {
        self.atomic_u32(OFF_REQ_TAIL)
    }
    fn res_head(&self) -> &AtomicU32 {
        self.atomic_u32(OFF_RES_HEAD)
    }
    fn res_tail(&self) -> &AtomicU32 {
        self.atomic_u32(OFF_RES_TAIL)
    }

    // --- request ring --------------------------------------------------

    /// Non-blocking push of a `Request` into the request ring.
    /// Returns `false` if the ring is full.
    ///
    /// Used by user processes (producer) and, symmetrically, by the
    /// mock client in kernel tests.
    pub fn try_push_request(&self, req: &Request) -> bool {
        let head = self.req_head().load(Ordering::Relaxed);
        let tail = self.req_tail().load(Ordering::Acquire);

        // Full when head + 1 would equal tail (mod slot count).
        let next_head = (head + 1) % REQ_SLOT_COUNT as u32;
        if next_head == tail {
            return false;
        }
        let slot_ix = (head as usize) % REQ_SLOT_COUNT;
        self.write_slot(OFF_REQ_RING, slot_ix, &req_to_bytes(req));
        // Release ordering publishes the slot contents before the
        // head update is observable.
        self.req_head().store(next_head, Ordering::Release);
        true
    }

    /// Non-blocking pop of a `Request` from the request ring.
    /// Returns `None` if the ring is empty.
    ///
    /// Used by the kernel (consumer) and by the native-test harness.
    pub fn try_pop_request(&self) -> Option<Request> {
        let head = self.req_head().load(Ordering::Acquire);
        let tail = self.req_tail().load(Ordering::Relaxed);
        if head == tail {
            return None;
        }
        let slot_ix = (tail as usize) % REQ_SLOT_COUNT;
        let mut buf = [0u8; SLOT_SIZE];
        self.read_slot(OFF_REQ_RING, slot_ix, &mut buf);
        let next_tail = (tail + 1) % REQ_SLOT_COUNT as u32;
        self.req_tail().store(next_tail, Ordering::Release);
        Some(bytes_to_req(&buf))
    }

    /// Returns the number of requests currently queued.
    pub fn request_len(&self) -> usize {
        let head = self.req_head().load(Ordering::Acquire);
        let tail = self.req_tail().load(Ordering::Acquire);
        ring_distance(head, tail, REQ_SLOT_COUNT)
    }

    // --- response ring -------------------------------------------------

    pub fn try_push_response(&self, res: &Response) -> bool {
        let head = self.res_head().load(Ordering::Relaxed);
        let tail = self.res_tail().load(Ordering::Acquire);
        let next_head = (head + 1) % RES_SLOT_COUNT as u32;
        if next_head == tail {
            return false;
        }
        let slot_ix = (head as usize) % RES_SLOT_COUNT;
        self.write_slot(OFF_RES_RING, slot_ix, &res_to_bytes(res));
        self.res_head().store(next_head, Ordering::Release);
        true
    }

    pub fn try_pop_response(&self) -> Option<Response> {
        let head = self.res_head().load(Ordering::Acquire);
        let tail = self.res_tail().load(Ordering::Relaxed);
        if head == tail {
            return None;
        }
        let slot_ix = (tail as usize) % RES_SLOT_COUNT;
        let mut buf = [0u8; SLOT_SIZE];
        self.read_slot(OFF_RES_RING, slot_ix, &mut buf);
        let next_tail = (tail + 1) % RES_SLOT_COUNT as u32;
        self.res_tail().store(next_tail, Ordering::Release);
        Some(bytes_to_res(&buf))
    }

    pub fn response_len(&self) -> usize {
        let head = self.res_head().load(Ordering::Acquire);
        let tail = self.res_tail().load(Ordering::Acquire);
        ring_distance(head, tail, RES_SLOT_COUNT)
    }
}

fn ring_distance(head: u32, tail: u32, capacity: usize) -> usize {
    let cap = capacity as u32;
    if head >= tail {
        (head - tail) as usize
    } else {
        (cap - tail + head) as usize
    }
}

// ---- Request <-> byte conversion -------------------------------------
//
// These helpers exist because `core::ptr::copy_nonoverlapping` between
// the slot byte array and the `Request` struct requires no padding
// surprises. The struct is `#[repr(C)]` with explicit fields, so the
// layout is deterministic — but doing the copy through a byte buffer
// keeps the code endian-safe under the assumption that both sides of
// the SAB are little-endian (which wasm32 is, unconditionally).

fn req_to_bytes(req: &Request) -> [u8; SLOT_SIZE] {
    let mut b = [0u8; SLOT_SIZE];
    b[0..2].copy_from_slice(&req.opcode.to_le_bytes());
    b[2..4].copy_from_slice(&req.flags.to_le_bytes());
    b[4..8].copy_from_slice(&req.request_id.to_le_bytes());
    b[8..24].copy_from_slice(&req.args);
    b[24..28].copy_from_slice(&req.heap_ptr.to_le_bytes());
    b[28..32].copy_from_slice(&req.heap_len.to_le_bytes());
    b
}

fn bytes_to_req(b: &[u8; SLOT_SIZE]) -> Request {
    let mut args = [0u8; 16];
    args.copy_from_slice(&b[8..24]);
    Request {
        opcode: u16::from_le_bytes([b[0], b[1]]),
        flags: u16::from_le_bytes([b[2], b[3]]),
        request_id: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        args,
        heap_ptr: u32::from_le_bytes([b[24], b[25], b[26], b[27]]),
        heap_len: u32::from_le_bytes([b[28], b[29], b[30], b[31]]),
    }
}

fn res_to_bytes(res: &Response) -> [u8; SLOT_SIZE] {
    let mut b = [0u8; SLOT_SIZE];
    b[0..4].copy_from_slice(&res.request_id.to_le_bytes());
    b[4..8].copy_from_slice(&res.status.to_le_bytes());
    b[8..16].copy_from_slice(&res.value.to_le_bytes());
    b[16..20].copy_from_slice(&res.extra_len.to_le_bytes());
    b[20..32].copy_from_slice(&res._pad);
    b
}

fn bytes_to_res(b: &[u8; SLOT_SIZE]) -> Response {
    let mut pad = [0u8; 12];
    pad.copy_from_slice(&b[20..32]);
    Response {
        request_id: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        status: i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        value: i64::from_le_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]),
        extra_len: u32::from_le_bytes([b[16], b[17], b[18], b[19]]),
        _pad: pad,
    }
}

/// Re-export the status constants for convenience.
pub mod status {
    pub use abi::ring::{STATUS_IDLE, STATUS_READY, STATUS_REQUESTED, STATUS_SERVICING};
}

// Re-export the status consts directly too so callers can write
// `ring::STATUS_REQUESTED` without the nested module.
pub use abi::ring::{
    SAB_SIZE as SAB_SIZE_BYTES, STATUS_IDLE, STATUS_READY, STATUS_REQUESTED, STATUS_SERVICING,
};

#[cfg(test)]
mod self_check {
    use super::*;

    #[test]
    fn re_exports_match_abi() {
        assert_eq!(SAB_SIZE_BYTES, abi::ring::SAB_SIZE);
        assert_eq!(STATUS_REQUESTED, 1);
        assert_eq!(status::STATUS_READY, 3);
    }
}
