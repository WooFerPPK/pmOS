//! SAB ring-buffer record layouts and magic status values.
//!
//! Mirrors `contracts/driver-kernel.md §1` exactly. This file
//! defines the *record types* used by the transport; the actual
//! producer/consumer logic lives in the sibling `ring` crate so
//! that implementation bugs there cannot silently corrupt the
//! layout constants reviewers audit here.
//!
//! Both the kernel Worker (Rust WASM) and the user-process Workers
//! (also Rust WASM) agree on these layouts; the TypeScript mirror
//! in `web/src/shared/sab-layout.ts` is generated from the values
//! below by `xtask gen-sab-layout` and guarded by a Vitest test.

use core::mem::size_of;

/// Size of the per-process SAB in bytes. Fixed in v1.
pub const SAB_SIZE: usize = 0x10000; // 64 KiB

// ---- Offsets within the SAB, per contracts/driver-kernel.md §1.1 --------

/// Atomic u32 — producer index into the request ring.
pub const OFF_REQ_HEAD:            usize = 0x0000;
/// Atomic u32 — kernel's consumer index.
pub const OFF_REQ_TAIL:            usize = 0x0004;
/// Atomic u32 — kernel's producer index into the response ring.
pub const OFF_RES_HEAD:            usize = 0x0008;
/// Atomic u32 — user's consumer index into the response ring.
pub const OFF_RES_TAIL:            usize = 0x000C;
/// Atomic u32 — user's `Atomics.wait` slot.
pub const OFF_USER_WAIT_SLOT:      usize = 0x0010;
/// Atomic u32 — kernel's `Atomics.wait` slot (shared across processes).
pub const OFF_KERNEL_WAIT_SLOT:    usize = 0x0014;
/// Atomic u32 — diagnostic: number of times the user blocked.
pub const OFF_USER_BLOCK_COUNT:    usize = 0x0018;
/// Atomic u32 — diagnostic: number of times the kernel blocked.
pub const OFF_KERNEL_BLOCK_COUNT:  usize = 0x001C;
/// 32 bytes of flags + reserved header.
pub const OFF_HEADER_FLAGS:        usize = 0x0020;

/// Start offset of the request ring (slot storage).
pub const OFF_REQ_RING:            usize = 0x0040;
/// Size of the request ring in bytes.
pub const REQ_RING_BYTES:          usize = 0x3FC0;

/// Start offset of the response ring.
pub const OFF_RES_RING:            usize = 0x4000;
/// Size of the response ring in bytes.
pub const RES_RING_BYTES:          usize = 0x3FC0;

/// Start offset of the heap scratch region for payloads that don't
/// fit inline in a Request/Response.
pub const OFF_HEAP_SCRATCH:        usize = 0x8000;
/// Size of the heap scratch region in bytes.
pub const HEAP_SCRATCH_BYTES:      usize = 0x8000;

/// Fixed slot stride — each Request/Response record occupies this many
/// bytes in the ring, rounded up to 32 for cache-line friendliness.
pub const SLOT_SIZE: usize = 32;

/// How many request slots fit in the request ring.
pub const REQ_SLOT_COUNT: usize = REQ_RING_BYTES / SLOT_SIZE;
/// How many response slots fit in the response ring.
pub const RES_SLOT_COUNT: usize = RES_RING_BYTES / SLOT_SIZE;

// ---- Magic status values for the user and kernel wait slots ------------
// The slot transitions IDLE -> REQUESTED -> SERVICING -> READY -> IDLE.

/// No request in flight on this channel.
pub const STATUS_IDLE:      u32 = 0;
/// User has posted a request and is parked on Atomics.wait.
pub const STATUS_REQUESTED: u32 = 1;
/// Kernel has taken the request and is servicing it.
pub const STATUS_SERVICING: u32 = 2;
/// Kernel has written the response; user may proceed.
pub const STATUS_READY:     u32 = 3;

// ---- Request / Response records ----------------------------------------

/// A syscall request as it sits in the ring.
///
/// Size: 32 bytes, matching `SLOT_SIZE`.
/// Layout is `#[repr(C)]` with explicit padding — this struct is
/// shared between the kernel and userland via the SAB.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Opcode — WASI if `< ext::FIRST`, extension if `>= ext::FIRST`.
    pub opcode: u16,
    /// Flags — reserved in v1, MUST be zero.
    pub flags: u16,
    /// Monotonically-increasing per-process id; echoed in the
    /// corresponding `Response`.
    pub request_id: u32,
    /// Inline argument bytes. Layout is opcode-specific. When the
    /// arguments don't fit in 16 bytes, `heap_ptr` and `heap_len`
    /// point into the heap scratch region instead.
    pub args: [u8; 16],
    /// Offset into the heap scratch region for overflow payload.
    pub heap_ptr: u32,
    /// Length of the overflow payload at `heap_ptr`.
    pub heap_len: u32,
}

const _: () = {
    // Compile-time assertion that Request fits in a slot.
    assert!(size_of::<Request>() == SLOT_SIZE);
};

impl Request {
    /// All-zero request, used as the "empty slot" marker.
    pub const EMPTY: Request = Request {
        opcode: 0,
        flags: 0,
        request_id: 0,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
}

/// A syscall response as it sits in the ring.
///
/// Size: 32 bytes, matching `SLOT_SIZE`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// Echoes the request's `request_id`.
    pub request_id: u32,
    /// 0 on success, negative errno on failure.
    pub status: i32,
    /// Primary return value (e.g. bytes read, new fd).
    pub value: i64,
    /// Length of any extra payload in the heap scratch region.
    pub extra_len: u32,
    /// Padding to `SLOT_SIZE`.
    pub _pad: [u8; 12],
}

const _: () = {
    assert!(size_of::<Response>() == SLOT_SIZE);
};

impl Response {
    /// Successful response with no return value and no extra payload.
    pub const OK: Response = Response {
        request_id: 0,
        status: 0,
        value: 0,
        extra_len: 0,
        _pad: [0u8; 12],
    };

    /// Successful response echoing a request id and a return value.
    pub const fn ok(request_id: u32, value: i64) -> Response {
        Response {
            request_id,
            status: 0,
            value,
            extra_len: 0,
            _pad: [0u8; 12],
        }
    }

    /// Error response. `errno` is the positive errno constant from
    /// `crate::errno`; the response stores it negated.
    pub const fn err(request_id: u32, errno: i32) -> Response {
        Response {
            request_id,
            status: -errno,
            value: 0,
            extra_len: 0,
            _pad: [0u8; 12],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_sizes_match() {
        assert_eq!(size_of::<Request>(),  SLOT_SIZE);
        assert_eq!(size_of::<Response>(), SLOT_SIZE);
    }

    #[test]
    fn ring_sizes_divide_cleanly_by_slot() {
        assert_eq!(REQ_RING_BYTES % SLOT_SIZE, 0);
        assert_eq!(RES_RING_BYTES % SLOT_SIZE, 0);
        assert_eq!(REQ_SLOT_COUNT, 510);
        assert_eq!(RES_SLOT_COUNT, 510);
    }

    #[test]
    fn layout_covers_full_sab() {
        // OFF_HEAP_SCRATCH + HEAP_SCRATCH_BYTES should equal SAB_SIZE.
        assert_eq!(OFF_HEAP_SCRATCH + HEAP_SCRATCH_BYTES, SAB_SIZE);
        // Every labelled region must be inside the SAB.
        assert!(OFF_REQ_RING + REQ_RING_BYTES <= OFF_RES_RING);
        assert!(OFF_RES_RING + RES_RING_BYTES <= OFF_HEAP_SCRATCH);
    }

    #[test]
    fn status_values_are_distinct() {
        let all = [STATUS_IDLE, STATUS_REQUESTED, STATUS_SERVICING, STATUS_READY];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j]);
            }
        }
    }

    #[test]
    fn empty_request_is_zero() {
        let r = Request::EMPTY;
        assert_eq!(r.opcode, 0);
        assert_eq!(r.request_id, 0);
        assert_eq!(r.heap_ptr, 0);
        assert_eq!(r.heap_len, 0);
    }

    #[test]
    fn response_ok_and_err() {
        let ok = Response::ok(42, 100);
        assert_eq!(ok.request_id, 42);
        assert_eq!(ok.status, 0);
        assert_eq!(ok.value, 100);

        let err = Response::err(42, crate::errno::EBADF);
        assert_eq!(err.request_id, 42);
        assert_eq!(err.status, -crate::errno::EBADF);
        assert_eq!(err.value, 0);
    }
}
