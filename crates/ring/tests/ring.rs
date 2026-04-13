//! Ring-buffer isolation tests (T036).
//!
//! These tests exercise the SAB ring transport against a plain
//! `Vec<u8>` — no browser, no SharedArrayBuffer, no `Atomics.wait`.
//! They validate the invariants every real-SAB user relies on:
//! push/pop ordering, wraparound at the end of the ring, the
//! four magic status values, and the empty/full transitions.
//!
//! Per Principle X ("testability at every layer"), this test is
//! the gate that the next layer up — the kernel's Platform
//! abstraction — can rely on when it uses `Sab`.

use abi::ring::{
    Request, Response, REQ_SLOT_COUNT, SAB_SIZE, SLOT_SIZE, STATUS_IDLE, STATUS_READY,
    STATUS_REQUESTED, STATUS_SERVICING,
};
use ring::Sab;

/// Allocate a zeroed backing buffer for a single test SAB and wrap it.
fn fresh_sab() -> (Vec<u8>, Sab) {
    let mut buf = vec![0u8; SAB_SIZE];
    let sab = Sab::from_slice(&mut buf);
    (buf, sab)
}

fn sample_request(id: u32, opcode: u16) -> Request {
    let mut args = [0u8; 16];
    // Put the id in the args so pop-time reads see the right slot.
    args[0..4].copy_from_slice(&id.to_le_bytes());
    Request {
        opcode,
        flags: 0,
        request_id: id,
        args,
        heap_ptr: id * 100,
        heap_len: id * 10,
    }
}

fn sample_response(id: u32, value: i64) -> Response {
    Response::ok(id, value)
}

// ---- basic push/pop -------------------------------------------------

#[test]
fn push_then_pop_returns_same_request() {
    let (mut _buf, mut sab) = fresh_sab();
    sab.init();

    let req = sample_request(42, 0x002E /* FD_READ */);
    assert!(sab.try_push_request(&req));
    assert_eq!(sab.request_len(), 1);

    let popped = sab.try_pop_request().expect("pop");
    assert_eq!(popped.request_id, 42);
    assert_eq!(popped.opcode, 0x002E);
    assert_eq!(popped.heap_ptr, 4200);
    assert_eq!(popped.heap_len, 420);
    assert_eq!(popped.args[0..4], 42u32.to_le_bytes());

    assert!(sab.try_pop_request().is_none());
    assert_eq!(sab.request_len(), 0);
}

#[test]
fn pop_empty_returns_none() {
    let (mut _buf, mut sab) = fresh_sab();
    sab.init();
    assert!(sab.try_pop_request().is_none());
    assert!(sab.try_pop_response().is_none());
}

#[test]
fn push_full_returns_false() {
    let (mut _buf, mut sab) = fresh_sab();
    sab.init();

    // Fill the ring. `REQ_SLOT_COUNT - 1` slots are usable (one slot
    // is always kept empty to distinguish empty from full).
    for i in 0..(REQ_SLOT_COUNT - 1) {
        let req = sample_request(i as u32, 0x0001);
        assert!(sab.try_push_request(&req), "push {i}");
    }
    // One more must fail.
    let overflow = sample_request(9999, 0x0001);
    assert!(!sab.try_push_request(&overflow));
    assert_eq!(sab.request_len(), REQ_SLOT_COUNT - 1);
}

#[test]
fn push_pop_fifo_order_preserved() {
    let (mut _buf, mut sab) = fresh_sab();
    sab.init();
    for i in 0..10u32 {
        let req = sample_request(i, 0x0001);
        assert!(sab.try_push_request(&req));
    }
    for i in 0..10u32 {
        let popped = sab.try_pop_request().expect("pop");
        assert_eq!(popped.request_id, i, "FIFO order at {i}");
    }
    assert!(sab.try_pop_request().is_none());
}

// ---- wraparound -----------------------------------------------------

#[test]
fn wraparound_across_buffer_end() {
    let (mut _buf, mut sab) = fresh_sab();
    sab.init();

    // Push-pop 1.5 x the slot count to force indices past the end
    // of the underlying ring.
    let rounds = (REQ_SLOT_COUNT as u32) * 3 / 2;
    for i in 0..rounds {
        let req = sample_request(i, 0x0001);
        assert!(sab.try_push_request(&req), "push round {i}");
        let popped = sab.try_pop_request().expect("pop round");
        assert_eq!(popped.request_id, i, "wraparound round {i}");
    }
}

#[test]
fn interleaved_push_pop_respects_bounds() {
    let (mut _buf, mut sab) = fresh_sab();
    sab.init();

    // Push two, pop one, push two, pop one … ad nauseam. Should
    // never overflow because the producer stays ≤ 2 ahead.
    for i in 0..1000u32 {
        let a = sample_request(i * 2, 0x0001);
        let b = sample_request(i * 2 + 1, 0x0002);
        assert!(sab.try_push_request(&a));
        assert!(sab.try_push_request(&b));
        let first = sab.try_pop_request().expect("pop a");
        assert_eq!(first.request_id, i * 2);
    }
    // After 1000 rounds we should still have ~1000 entries queued
    // (one leftover per round).
    let remaining = sab.request_len();
    assert!(remaining > 0);
    assert!(remaining < REQ_SLOT_COUNT);
}

// ---- status value transitions --------------------------------------

#[test]
fn status_constants_are_the_expected_values() {
    assert_eq!(STATUS_IDLE,      0);
    assert_eq!(STATUS_REQUESTED, 1);
    assert_eq!(STATUS_SERVICING, 2);
    assert_eq!(STATUS_READY,     3);
}

#[test]
fn user_wait_slot_is_atomic_and_starts_at_idle() {
    use core::sync::atomic::Ordering;

    let (mut _buf, mut sab) = fresh_sab();
    sab.init();
    let slot = sab.user_wait_slot();
    assert_eq!(slot.load(Ordering::Acquire), STATUS_IDLE);

    // Walk the state machine: IDLE -> REQUESTED -> SERVICING -> READY -> IDLE.
    slot.store(STATUS_REQUESTED, Ordering::Release);
    assert_eq!(slot.load(Ordering::Acquire), STATUS_REQUESTED);

    // Kernel picks it up.
    slot.store(STATUS_SERVICING, Ordering::Release);
    assert_eq!(slot.load(Ordering::Acquire), STATUS_SERVICING);

    // Kernel writes response.
    slot.store(STATUS_READY, Ordering::Release);
    assert_eq!(slot.load(Ordering::Acquire), STATUS_READY);

    // User consumes it.
    slot.store(STATUS_IDLE, Ordering::Release);
    assert_eq!(slot.load(Ordering::Acquire), STATUS_IDLE);
}

#[test]
fn block_counters_can_be_incremented_atomically() {
    use core::sync::atomic::Ordering;

    let (mut _buf, mut sab) = fresh_sab();
    sab.init();
    let user = sab.user_block_count();
    let kernel = sab.kernel_block_count();

    assert_eq!(user.load(Ordering::Acquire), 0);
    assert_eq!(kernel.load(Ordering::Acquire), 0);

    user.fetch_add(1, Ordering::AcqRel);
    user.fetch_add(1, Ordering::AcqRel);
    kernel.fetch_add(5, Ordering::AcqRel);

    assert_eq!(user.load(Ordering::Acquire), 2);
    assert_eq!(kernel.load(Ordering::Acquire), 5);
}

// ---- response ring parity ------------------------------------------

#[test]
fn response_ring_behaves_symmetrically() {
    let (mut _buf, mut sab) = fresh_sab();
    sab.init();

    for i in 0..5u32 {
        assert!(sab.try_push_response(&sample_response(i, i as i64 * 10)));
    }
    for i in 0..5u32 {
        let popped = sab.try_pop_response().expect("pop response");
        assert_eq!(popped.request_id, i);
        assert_eq!(popped.status, 0);
        assert_eq!(popped.value, i as i64 * 10);
    }
    assert!(sab.try_pop_response().is_none());
}

#[test]
fn error_response_round_trip() {
    let (mut _buf, mut sab) = fresh_sab();
    sab.init();

    let err = Response::err(77, abi::errno::EBADF);
    assert!(sab.try_push_response(&err));
    let popped = sab.try_pop_response().expect("pop");
    assert_eq!(popped.request_id, 77);
    assert_eq!(popped.status, -abi::errno::EBADF);
    assert_eq!(popped.value, 0);
}

// ---- binary layout sanity ------------------------------------------

#[test]
fn slot_and_sab_sizes_are_as_documented() {
    assert_eq!(SLOT_SIZE, 32);
    assert_eq!(SAB_SIZE, 0x10000);
}

// ---- multi-threaded producer/consumer under real atomics -----------
//
// This is the closest we get in a native test to exercising the
// atomic ordering: one thread pushes, another pops, we spin until
// done. If the Release/Acquire ordering were wrong, the popped
// bytes would be torn.

#[test]
fn concurrent_producer_consumer() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    const ROUNDS: u32 = 20_000;

    let buf: Arc<Vec<u8>> = Arc::new({
        let mut v = vec![0u8; SAB_SIZE];
        // Initialise by wrapping and calling init() on the owned slice.
        let mut sab = Sab::from_slice(&mut v);
        sab.init();
        v
    });

    // SAFETY: both halves construct an `Sab` from the shared buffer's
    // data pointer; all mutations go through atomic operations inside
    // Sab, and the slot writes are coordinated by the head/tail
    // atomics with Release/Acquire ordering. The Arc keeps the buffer
    // alive for the duration of both threads.
    let ptr = buf.as_ptr() as *mut u8;
    let len = buf.len();
    let pushed_total = Arc::new(AtomicUsize::new(0));
    let popped_total = Arc::new(AtomicUsize::new(0));

    let pushed_total_p = pushed_total.clone();
    let buf_p = buf.clone();
    let producer = thread::spawn(move || {
        let sab = unsafe { Sab::from_raw(ptr, len) };
        let _keep = buf_p; // keep the Arc alive in this thread
        for i in 0..ROUNDS {
            let req = sample_request(i, 0x0001);
            loop {
                if sab.try_push_request(&req) {
                    pushed_total_p.fetch_add(1, Ordering::AcqRel);
                    break;
                }
                std::hint::spin_loop();
            }
        }
    });

    let popped_total_c = popped_total.clone();
    let buf_c = buf.clone();
    let consumer = thread::spawn(move || {
        let sab = unsafe { Sab::from_raw(ptr, len) };
        let _keep = buf_c;
        let mut expected: u32 = 0;
        while popped_total_c.load(Ordering::Acquire) < ROUNDS as usize {
            if let Some(req) = sab.try_pop_request() {
                assert_eq!(req.request_id, expected, "order preserved");
                assert_eq!(req.args[0..4], expected.to_le_bytes(), "tearing");
                expected += 1;
                popped_total_c.fetch_add(1, Ordering::AcqRel);
            } else {
                std::hint::spin_loop();
            }
        }
    });

    producer.join().unwrap();
    consumer.join().unwrap();

    assert_eq!(pushed_total.load(Ordering::Acquire), ROUNDS as usize);
    assert_eq!(popped_total.load(Ordering::Acquire), ROUNDS as usize);
}
