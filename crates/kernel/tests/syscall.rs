//! Syscall dispatcher isolation tests (T071..T073 first landing).
//!
//! Exercise the SAB-ring opcode dispatcher end-to-end on the native
//! host target. The dispatcher sits between [`abi::ring::Request`]
//! / [`abi::ring::Response`] and the Rust method surface on
//! [`kernel::sys::Kernel`]; these tests feed it synthetic requests
//! and assert the responses match what a direct `Kernel` method
//! call would produce.
//!
//! Principle X ties: every opcode that has a handler in
//! `crates/kernel/src/syscall/{wasi,ext}.rs` has at least one
//! positive test and at least one negative test here, and the
//! ring-transport round-trip (`Dispatcher::service_one`) has one
//! dedicated test so the ring crate's producer/consumer pair is
//! exercised through the same path userland will use in
//! production.

#![cfg(feature = "native-platform")]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use abi::cap::{initial, Cap};
use abi::errno;
use abi::ext as op_ext;
use abi::ring::{Request, SAB_SIZE};
use abi::wasi as op_wasi;

use kernel::fd::{FdFlags, FdObject};
use kernel::fs::devfs::{DevFs, DEV_CONSOLE};
use kernel::fs::procfs::ProcFs;
use kernel::fs::tmpfs::TmpFs;
use kernel::proc::{ExitStatus, ProcState};
use kernel::sys::{Kernel, RegisterArgs};
use kernel::syscall::{dispatch, Dispatcher};
use ring::Sab;

// ---- test harness ------------------------------------------------------

/// Build a `Kernel` with the v1 default mount layout. Mirrors the
/// helper in `tests/sys.rs` — keeping them separate is a
/// deliberate choice: isolation tests for the dispatcher should
/// not accidentally share setup helpers with the semantic-level
/// tests, so a change in one file doesn't silently change the
/// other file's assumptions.
fn make_kernel() -> Kernel {
    let mut k = Kernel::new();
    k.vfs.mount("/", Box::new(TmpFs::new())).expect("root mount");
    k.vfs.mount("/dev", Box::new(DevFs::new())).expect("devfs mount");
    k.vfs
        .mount("/proc", Box::new(ProcFs::with_static()))
        .expect("procfs mount");
    k
}

/// Register a process, mark it Ready, then transition it to
/// Running. A process has to be in Running to make syscalls —
/// the dispatcher never observes a process in Ready state during
/// normal execution.
fn make_running_proc(k: &mut Kernel, name: &str, ppid: abi::ext::Pid) -> abi::ext::Pid {
    let pid = k
        .register_process(RegisterArgs {
            name,
            ppid,
            caps: initial::INIT,
            cwd: "/",
        })
        .expect("register");
    k.mark_ready(pid).expect("mark_ready");
    k.procs
        .transition(pid, ProcState::Running)
        .expect("transition to Running");
    pid
}

/// Build an empty inline-args window. Handlers that pack a u32 at
/// offset 0 call `u32_args(value)` to avoid boilerplate.
fn u32_args(value: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[..4].copy_from_slice(&value.to_le_bytes());
    args
}

// ---- opcode routing ----------------------------------------------------

#[test]
fn unknown_opcode_outside_both_ranges_returns_enosys() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "test", 0);
    let mut heap = vec![0u8; 4096];

    let req = Request {
        opcode: 0x4242, // not in WASI (< 0x0080) and not in ext (>= 0x1000, < 0x1501)
        flags: 0,
        request_id: 1,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.request_id, 1);
    assert_eq!(resp.status, -errno::ENOSYS);
    assert_eq!(resp.value, 0);
}

#[test]
fn known_wasi_opcode_without_handler_returns_enosys() {
    // `FD_SEEK` is in the WASI range (0x0031) but still has no
    // handler. Same shape as the ext-side test below: decoded as
    // a known WASI opcode, routed to `dispatch_wasi`'s `_ =>`
    // arm, ENOSYS echoed back with the request_id intact.
    //
    // (This probe was `CLOCK_TIME_GET` before that handler
    // landed. When `FD_SEEK` grows a real handler, swap this
    // probe to whatever's still unhandled at that point, or
    // delete the test once every WASI opcode has real coverage.)
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "test", 0);
    let mut heap = vec![0u8; 4096];

    let req = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 42,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.request_id, 42);
    assert_eq!(resp.status, -errno::ENOSYS);
}

#[test]
fn known_ext_opcode_without_handler_returns_enosys() {
    // `PROC_WAIT` is in the extension range (0x1101) but still has
    // no handler. Same shape as the WASI case above: decoded as a
    // known extension opcode, routed to `dispatch_ext`'s `_ =>`
    // arm, ENOSYS echoed back with the request_id intact.
    //
    // (This probe was `PROC_SPAWN` before that handler landed. When
    // the next extension opcode — `PROC_WAIT`, `IPC_SOCKET`, etc.
    // — gets implemented, swap this probe to whatever's still
    // unhandled at that point, or delete the test once every
    // extension opcode has real coverage.)
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "test", 0);
    let mut heap = vec![0u8; 4096];

    let req = Request {
        opcode: op_ext::PROC_WAIT,
        flags: 0,
        request_id: 7,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.request_id, 7);
    assert_eq!(resp.status, -errno::ENOSYS);
}

// ---- fd_write ---------------------------------------------------------

#[test]
fn fd_write_to_console_returns_bytes_written() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "writer", 0);
    // Install fd 1 as /dev/console so the dispatcher has somewhere
    // to write to.
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();

    let mut heap = vec![0u8; 4096];
    // Write "hi" (no newline) so it stays in the console sink and
    // drain_console_output can return it.
    heap[..2].copy_from_slice(b"hi");

    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 1,
        args: u32_args(1), // fd = 1
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);

    assert_eq!(resp.request_id, 1);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 2);
    assert_eq!(k.devs.drain_console_output(), b"hi");
}

#[test]
fn fd_write_with_bad_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "writer", 0);
    let mut heap = vec![0u8; 4096];
    heap[..3].copy_from_slice(b"hey");

    // No fds installed on the process, so fd 5 is garbage.
    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 2,
        args: u32_args(5),
        heap_ptr: 0,
        heap_len: 3,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.request_id, 2);
    assert_eq!(resp.status, -errno::EBADF);
    assert_eq!(resp.value, 0);
}

#[test]
fn fd_write_with_out_of_range_heap_ptr_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "writer", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];

    // heap_ptr + heap_len > heap.len() → heap_in returns None →
    // dispatcher maps to EINVAL.
    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 3,
        args: u32_args(1),
        heap_ptr: 32,
        heap_len: 64, // 32 + 64 = 96 > heap.len() == 64
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

// ---- fd_read ----------------------------------------------------------

#[test]
fn fd_read_from_console_populates_heap_and_response() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);
    k.install_fd(pid, 0, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    k.devs.inject_console_input(b"abc");

    let mut heap = vec![0u8; 4096];

    // Read up to 8 bytes starting at heap[16..24] — picking a
    // non-zero heap_ptr catches any "assumes heap_ptr == 0"
    // regression.
    let req = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 10,
        args: u32_args(0), // fd = 0
        heap_ptr: 16,
        heap_len: 8,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);

    assert_eq!(resp.request_id, 10);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 3);
    assert_eq!(resp.extra_len, 3);
    assert_eq!(&heap[16..19], b"abc");
    // Bytes outside the populated window are untouched.
    assert_eq!(&heap[19..24], &[0u8; 5]);
}

// ---- fd_close ---------------------------------------------------------

#[test]
fn fd_close_releases_fd_slot() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "closer", 0);
    k.install_fd(pid, 3, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    assert!(k.fds(pid).unwrap().get(3).is_some());

    let mut heap = vec![0u8; 64];
    let req = Request {
        opcode: op_wasi::FD_CLOSE,
        flags: 0,
        request_id: 4,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);

    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert!(k.fds(pid).unwrap().get(3).is_none());
}

#[test]
fn fd_close_with_bad_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "closer", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::FD_CLOSE,
        flags: 0,
        request_id: 5,
        args: u32_args(99),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

// ---- path_open --------------------------------------------------------

#[test]
fn path_open_against_devfs_console_returns_fresh_fd() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "opener", 0);
    let mut heap = vec![0u8; 128];

    let path = b"/dev/console";
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_OPEN,
        flags: 0,
        request_id: 20,
        args: u32_args(0), // no flags
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);

    assert_eq!(resp.request_id, 20);
    assert_eq!(resp.status, 0);
    let fd = resp.value as u32;
    assert!(k.fds(pid).unwrap().get(fd).is_some());
}

#[test]
fn path_open_bad_path_returns_enoent() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "opener", 0);
    let mut heap = vec![0u8; 128];
    let path = b"/nonexistent/file";
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_OPEN,
        flags: 0,
        request_id: 21,
        args: u32_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOENT);
}

#[test]
fn path_open_with_invalid_utf8_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "opener", 0);
    let mut heap = vec![0u8; 128];
    // Invalid UTF-8 continuation byte sequence.
    heap[..4].copy_from_slice(&[0xFFu8, 0xFE, 0xFD, 0xFC]);

    let req = Request {
        opcode: op_wasi::PATH_OPEN,
        flags: 0,
        request_id: 22,
        args: u32_args(0),
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

// ---- proc_exit --------------------------------------------------------

#[test]
fn proc_exit_moves_caller_to_zombie() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "exiter", 0);
    assert_eq!(k.procs.get(pid).unwrap().state, ProcState::Running);

    let mut heap = vec![0u8; 64];
    let req = Request {
        opcode: op_wasi::PROC_EXIT,
        flags: 0,
        request_id: 30,
        args: u32_args(7), // exit code 7
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);

    assert_eq!(resp.status, 0);
    assert_eq!(k.procs.get(pid).unwrap().state, ProcState::Zombie);
    assert_eq!(
        k.procs.get(pid).unwrap().exit_status,
        Some(ExitStatus::Exited(7)),
    );
}

// ---- proc_self --------------------------------------------------------

#[test]
fn proc_self_returns_caller_pid() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "self", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_ext::PROC_SELF,
        flags: 0,
        request_id: 40,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);

    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, pid as i64);
}

// ---- proc_parent ------------------------------------------------------

#[test]
fn proc_parent_returns_parent_pid() {
    let mut k = make_kernel();
    let parent = make_running_proc(&mut k, "parent", 0);
    // Child has `parent` as its ppid.
    let child = k
        .register_process(RegisterArgs {
            name: "child",
            ppid: parent,
            caps: initial::INIT,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(child).unwrap();
    k.procs.transition(child, ProcState::Running).unwrap();

    let mut heap = vec![0u8; 64];
    let req = Request {
        opcode: op_ext::PROC_PARENT,
        flags: 0,
        request_id: 41,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, child, &req, &mut heap);

    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, parent as i64);
}

// ---- cap_check --------------------------------------------------------

#[test]
fn cap_check_returns_one_for_held_cap() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "capped", 0);
    // `initial::INIT` is `CapSet::ALL`, so every cap variant is
    // held — pick DisplayClient (bit 1) as the probe.
    let mut heap = vec![0u8; 64];
    let req = Request {
        opcode: op_ext::CAP_CHECK,
        flags: 0,
        request_id: 50,
        args: u32_args(Cap::DisplayClient as u32),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);

    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
}

#[test]
fn cap_check_returns_zero_for_absent_cap() {
    let mut k = make_kernel();
    // DESKTOP_SHELL does NOT hold DisplayServer — that's the
    // entire point of the display-server cap split. Pick that as
    // the "not held" probe.
    let pid = k
        .register_process(RegisterArgs {
            name: "shell",
            ppid: 0,
            caps: initial::DESKTOP_SHELL,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(pid).unwrap();
    k.procs.transition(pid, ProcState::Running).unwrap();

    let mut heap = vec![0u8; 64];
    let req = Request {
        opcode: op_ext::CAP_CHECK,
        flags: 0,
        request_id: 51,
        args: u32_args(Cap::DisplayServer as u32),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);

    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
}

#[test]
fn cap_check_with_invalid_cap_id_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "capped", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_ext::CAP_CHECK,
        flags: 0,
        request_id: 52,
        // 0x4242 is not a valid Cap discriminant.
        args: u32_args(0x4242),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

// ---- cap_list ---------------------------------------------------------

#[test]
fn cap_list_returns_full_cap_bitset() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "all", 0);
    let expected_bits = initial::INIT.0 as i64;

    let mut heap = vec![0u8; 64];
    let req = Request {
        opcode: op_ext::CAP_LIST,
        flags: 0,
        request_id: 60,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);

    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, expected_bits);
}

// ---- clock_time_get --------------------------------------------------

#[test]
fn clock_time_get_monotonic_returns_strictly_increasing_nanoseconds() {
    // The `Platform::now_ns` contract is "strictly increasing even
    // across back-to-back calls within the same nanosecond". Fire
    // two dispatches in succession with clock_id = CLOCKID_MONOTONIC
    // and assert the second reports a strictly larger timestamp.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "clock", 0);
    let mut heap = vec![0u8; 64];

    let req1 = Request {
        opcode: op_wasi::CLOCK_TIME_GET,
        flags: 0,
        request_id: 80,
        args: u32_args(abi::wasi::CLOCKID_MONOTONIC),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp1 = dispatch(&mut k, pid, &req1, &mut heap);
    assert_eq!(resp1.status, 0);
    assert_eq!(resp1.request_id, 80);
    let ns1 = resp1.value;

    let req2 = Request {
        request_id: 81,
        ..req1
    };
    let resp2 = dispatch(&mut k, pid, &req2, &mut heap);
    assert_eq!(resp2.status, 0);
    let ns2 = resp2.value;

    assert!(
        ns2 > ns1,
        "clock_time_get(MONOTONIC) must be strictly increasing: ns1={ns1}, ns2={ns2}",
    );
}

#[test]
fn clock_time_get_realtime_returns_wall_clock_nanoseconds() {
    // clock_id = CLOCKID_REALTIME (0) reports the Unix-epoch-ns
    // wall clock via `Platform::now_realtime_ns`. Under NativePlatform
    // that's `SystemTime::now().duration_since(UNIX_EPOCH)` in ns.
    // The assertion pins "value is after 2020-01-01 UTC" (a lower
    // bound that is still true in 2026) to prove the handler really
    // read the wall clock rather than returning the monotonic clock
    // or zero.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "clock", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::CLOCK_TIME_GET,
        flags: 0,
        request_id: 82,
        args: u32_args(abi::wasi::CLOCKID_REALTIME),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.request_id, 82);
    // 2020-01-01 00:00:00 UTC = 1577836800 s since epoch.
    const Y2020_NS: i64 = 1_577_836_800_000_000_000;
    assert!(
        resp.value > Y2020_NS,
        "clock_time_get(REALTIME) returned {}, expected > 2020-01-01 UTC ({})",
        resp.value,
        Y2020_NS,
    );
}

#[test]
fn clock_time_get_process_cputime_returns_enotsup() {
    // CLOCKID_PROCESS_CPUTIME_ID (2) is a WASI-defined clock PMos
    // does not implement in v1. The handler returns ENOTSUP so a
    // userland libc that probes for cpu-time support sees a clean
    // "no" rather than a phoney zero or a bogus monotonic value.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "clock", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::CLOCK_TIME_GET,
        flags: 0,
        request_id: 83,
        args: u32_args(abi::wasi::CLOCKID_PROCESS_CPUTIME_ID),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTSUP);
    assert_eq!(resp.request_id, 83);
    assert_eq!(resp.value, 0);
}

#[test]
fn clock_time_get_thread_cputime_returns_enotsup() {
    // CLOCKID_THREAD_CPUTIME_ID (3) — same rationale as the
    // process-cputime test above.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "clock", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::CLOCK_TIME_GET,
        flags: 0,
        request_id: 84,
        args: u32_args(abi::wasi::CLOCKID_THREAD_CPUTIME_ID),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTSUP);
    assert_eq!(resp.request_id, 84);
    assert_eq!(resp.value, 0);
}

#[test]
fn clock_time_get_unknown_clock_returns_einval() {
    // Any clock_id outside the four WASI-defined values (0..=3)
    // returns EINVAL. Matches POSIX's `clock_gettime(unknown_id, ...)`
    // behavior and distinguishes "I know about this clock but don't
    // implement it" (ENOTSUP) from "this isn't a clock id I recognise"
    // (EINVAL).
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "clock", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::CLOCK_TIME_GET,
        flags: 0,
        request_id: 85,
        args: u32_args(99),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
    assert_eq!(resp.request_id, 85);
    assert_eq!(resp.value, 0);
}

// ---- clock_res_get ---------------------------------------------------

#[test]
fn clock_res_get_monotonic_returns_nanosecond_resolution() {
    // The WASI `clock_res_get(MONOTONIC, ...)` contract: report the
    // finest resolution the monotonic clock can resolve. PMos's
    // `Platform::now_ns` is nanosecond-granular already (both
    // `SystemTime::now()` on native and `performance.now()` * 1_000_000
    // on the browser floor), so 1 ns is the honest answer. Userland
    // libc takes that as "every call can return a distinct value
    // even at the fastest measurable tick".
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "clock", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::CLOCK_RES_GET,
        flags: 0,
        request_id: 90,
        args: u32_args(abi::wasi::CLOCKID_MONOTONIC),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.request_id, 90);
    assert_eq!(resp.value, 1);
}

#[test]
fn clock_res_get_realtime_returns_nanosecond_resolution() {
    // Realtime resolution mirrors monotonic — both clocks sit on
    // top of the same nanosecond-granular Platform clock, so they
    // share the 1 ns resolution answer. The split only matters for
    // values + monotonicity guarantees, not for precision.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "clock", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::CLOCK_RES_GET,
        flags: 0,
        request_id: 91,
        args: u32_args(abi::wasi::CLOCKID_REALTIME),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.request_id, 91);
    assert_eq!(resp.value, 1);
}

#[test]
fn clock_res_get_process_cputime_returns_enotsup() {
    // Same ENOTSUP split as the time handler — "known clock id,
    // not implemented here" returns ENOTSUP rather than a bogus
    // value. A libc probing for cpu-time support sees a clean "no"
    // and falls back to monotonic.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "clock", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::CLOCK_RES_GET,
        flags: 0,
        request_id: 92,
        args: u32_args(abi::wasi::CLOCKID_PROCESS_CPUTIME_ID),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTSUP);
    assert_eq!(resp.request_id, 92);
    assert_eq!(resp.value, 0);
}

#[test]
fn clock_res_get_thread_cputime_returns_enotsup() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "clock", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::CLOCK_RES_GET,
        flags: 0,
        request_id: 93,
        args: u32_args(abi::wasi::CLOCKID_THREAD_CPUTIME_ID),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTSUP);
    assert_eq!(resp.request_id, 93);
    assert_eq!(resp.value, 0);
}

#[test]
fn clock_res_get_unknown_clock_returns_einval() {
    // Same ENOTSUP/EINVAL split as clock_time_get: unknown clock id
    // is EINVAL (distinct from "known but unsupported" = ENOTSUP).
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "clock", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::CLOCK_RES_GET,
        flags: 0,
        request_id: 94,
        args: u32_args(99),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
    assert_eq!(resp.request_id, 94);
    assert_eq!(resp.value, 0);
}

// ---- random_get ------------------------------------------------------

#[test]
fn random_get_fills_heap_and_echoes_length() {
    // Ask for 16 bytes of random data at a non-zero heap offset so
    // any "assumes heap_ptr == 0" regression trips here.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "rand", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::RANDOM_GET,
        flags: 0,
        request_id: 82,
        args: [0u8; 16],
        heap_ptr: 8,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);

    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 16);
    assert_eq!(resp.extra_len, 16);

    // The 16 bytes in the target window should not ALL be zero —
    // the xorshift prng NativePlatform ships is deterministic but
    // non-trivial, and an all-zero return would mean
    // `random_bytes` was never called. Soft assertion.
    let window = &heap[8..24];
    assert!(
        window.iter().any(|&b| b != 0),
        "random_get left the heap window untouched",
    );
    // Bytes outside the window are still zero.
    assert!(heap[..8].iter().all(|&b| b == 0));
    assert!(heap[24..].iter().all(|&b| b == 0));
}

#[test]
fn random_get_with_zero_length_is_a_noop_success() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "rand", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::RANDOM_GET,
        flags: 0,
        request_id: 83,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert_eq!(resp.extra_len, 0);
    // Heap untouched.
    assert!(heap.iter().all(|&b| b == 0));
}

// ---- sched_yield -----------------------------------------------------

#[test]
fn sched_yield_returns_ok() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "yielder", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SCHED_YIELD,
        flags: 0,
        request_id: 84,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.request_id, 84);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
}

// ---- args / environ ---------------------------------------------------
//
// v1 answer for all four: empty. A Rust std binary's init probe sees
// (0, 0) on the *_SIZES_GET side and writes nothing on the *_GET side,
// which is exactly what `std::env::args()` / `std::env::vars()`
// degrade into.

#[test]
fn args_sizes_get_returns_zero_argc_and_zero_buf_size() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "argsy", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::ARGS_SIZES_GET,
        flags: 0,
        request_id: 85,
        args: [0u8; 16],
        heap_ptr: 16,
        heap_len: 8,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert_eq!(resp.extra_len, 8);
    // Two zero u32s written at offset 16.
    assert_eq!(&heap[16..24], &[0u8; 8]);
    // Nothing else clobbered.
    assert!(heap[..16].iter().all(|&b| b == 0));
    assert!(heap[24..].iter().all(|&b| b == 0));
}

#[test]
fn args_get_is_a_noop_success() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "argsy", 0);
    let mut heap = vec![0u8; 16];
    let req = Request {
        opcode: op_wasi::ARGS_GET,
        flags: 0,
        request_id: 86,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert!(heap.iter().all(|&b| b == 0));
}

#[test]
fn environ_sizes_get_returns_zero_envc_and_zero_buf_size() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "environey", 0);
    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::ENVIRON_SIZES_GET,
        flags: 0,
        request_id: 87,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 8,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert_eq!(resp.extra_len, 8);
    assert_eq!(&heap[..8], &[0u8; 8]);
}

#[test]
fn environ_get_is_a_noop_success() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "environey", 0);
    let mut heap = vec![0u8; 16];
    let req = Request {
        opcode: op_wasi::ENVIRON_GET,
        flags: 0,
        request_id: 88,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
}

// ---- fd_fdstat_get ----------------------------------------------------
//
// The handler branches on the FdObject variant. Three tests:
// stdio (CharDevice → 2), socket (Socket → 6), bad fd (→ EBADF).

#[test]
fn fd_fdstat_get_on_stdio_fd_returns_character_device() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "fdstater", 0);
    // Stdout is a CharDevice(DEV_CONSOLE).
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::FD_FDSTAT_GET,
        flags: 0,
        request_id: 89,
        args: u32_args(1),
        heap_ptr: 8,
        heap_len: 24,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert_eq!(resp.extra_len, 24);
    // filetype (byte 0) == CHARACTER_DEVICE (2).
    assert_eq!(heap[8], 2);
    // _pad + fs_flags both zero.
    assert_eq!(&heap[9..12], &[0u8; 3]);
    // offset 4 pad zero.
    assert_eq!(&heap[12..16], &[0u8; 4]);
    // fs_rights_base == u64::MAX.
    assert_eq!(&heap[16..24], &u64::MAX.to_le_bytes());
    // fs_rights_inh == u64::MAX.
    assert_eq!(&heap[24..32], &u64::MAX.to_le_bytes());
}

#[test]
fn fd_fdstat_get_on_invalid_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "fdstater", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::FD_FDSTAT_GET,
        flags: 0,
        request_id: 90,
        args: u32_args(99), // fd 99 is not open
        heap_ptr: 0,
        heap_len: 24,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
    // Heap must not have been written.
    assert!(heap.iter().all(|&b| b == 0));
}

// ---- fd_filestat_get --------------------------------------------------
//
// Writes a 64-byte `filestat_t` (dev/ino/filetype/nlink/size/atim/
// mtim/ctim — little-endian u64s with the filetype crammed into
// the first byte of the 8-byte nlink-alignment window, per WASI
// preview 1's C ABI) into the caller's heap-scratch out window.
//
// For `Vnode` fds the handler queries `Vfs::stat_ino` so a directory
// vnode reports filetype=3 (DIRECTORY), not the "regular file"
// default `fd_fdstat_get` hardcodes for every Vnode. For non-Vnode
// fds dev/ino/size/times are synthesised: filetype from the
// FdObject variant, size=0, nlink=1, dev=0, ino=<opaque id>, times=0.
//
// Seven tests: one per FdObject variant the handler can see
// (CharDevice → 2, Socket → 6, Vnode regular → 4 with size, Vnode
// directory → 3, PipeRead → 0, SignalChannel → 0) plus the EBADF
// bad-fd path.

/// Decode the 8-byte little-endian u64 at `off` in the 64-byte
/// filestat window. Shared by every fd_filestat_get test.
fn filestat_u64(heap: &[u8], base: usize, off: usize) -> u64 {
    u64::from_le_bytes([
        heap[base + off],
        heap[base + off + 1],
        heap[base + off + 2],
        heap[base + off + 3],
        heap[base + off + 4],
        heap[base + off + 5],
        heap[base + off + 6],
        heap[base + off + 7],
    ])
}

#[test]
fn fd_filestat_get_on_char_device_fd_returns_filetype_char_device() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "stater", 0);
    // Stdout is a CharDevice(DEV_CONSOLE); filetype=2 (character
    // device). dev=0, ino=DEV_CONSOLE, size=0, nlink=1, times=0.
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 128];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_GET,
        flags: 0,
        request_id: 700,
        args: u32_args(1),
        heap_ptr: 8,
        heap_len: 64,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert_eq!(resp.extra_len, 64);

    let base = 8;
    assert_eq!(filestat_u64(&heap, base, 0), 0, "dev");
    assert_eq!(filestat_u64(&heap, base, 8), DEV_CONSOLE as u64, "ino");
    assert_eq!(heap[base + 16], 2, "filetype = character_device");
    assert_eq!(&heap[base + 17..base + 24], &[0u8; 7], "filetype padding");
    assert_eq!(filestat_u64(&heap, base, 24), 1, "nlink");
    assert_eq!(filestat_u64(&heap, base, 32), 0, "size");
    assert_eq!(filestat_u64(&heap, base, 40), 0, "atim");
    assert_eq!(filestat_u64(&heap, base, 48), 0, "mtim");
    assert_eq!(filestat_u64(&heap, base, 56), 0, "ctim");
}

#[test]
fn fd_filestat_get_on_socket_fd_returns_filetype_socket_stream() {
    use kernel::ipc::SocketType;
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sockstater", 0);
    let fd = k.ipc_socket(pid, SocketType::Stream).expect("ipc_socket");
    let mut heap = vec![0u8; 128];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_GET,
        flags: 0,
        request_id: 701,
        args: u32_args(fd),
        heap_ptr: 0,
        heap_len: 64,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.extra_len, 64);
    assert_eq!(heap[16], 6, "filetype = socket_stream");
    assert_eq!(filestat_u64(&heap, 0, 24), 1, "nlink");
    assert_eq!(filestat_u64(&heap, 0, 32), 0, "size");
}

#[test]
fn fd_filestat_get_on_regular_file_vnode_returns_filetype_and_size() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "filestater", 0);
    // Pre-create a tmpfs regular file with a known size.
    let bytes: &[u8] = b"hello, fd_filestat_get";
    k.vfs.create("/probe.txt", 0o644).expect("create");
    let wrote = k.vfs.write("/probe.txt", 0, bytes).expect("write");
    assert_eq!(wrote, bytes.len());
    let (mount_id, ino) = k.vfs.resolve("/probe.txt").expect("resolve");
    k.install_fd(
        pid,
        10,
        FdObject::Vnode { mount_id, ino },
        FdFlags::EMPTY,
    )
    .unwrap();
    let mut heap = vec![0u8; 128];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_GET,
        flags: 0,
        request_id: 702,
        args: u32_args(10),
        heap_ptr: 0,
        heap_len: 64,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.extra_len, 64);
    assert_eq!(filestat_u64(&heap, 0, 0), mount_id.0 as u64, "dev");
    assert_eq!(filestat_u64(&heap, 0, 8), ino, "ino");
    assert_eq!(heap[16], 4, "filetype = regular_file");
    assert_eq!(filestat_u64(&heap, 0, 24), 1, "nlink");
    assert_eq!(filestat_u64(&heap, 0, 32), bytes.len() as u64, "size");
}

#[test]
fn fd_filestat_get_on_directory_vnode_returns_filetype_directory() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "dirstater", 0);
    // Pre-create a tmpfs directory. `Vfs::mkdir` returns the ino;
    // resolving the path afterwards gives us the (mount_id, ino)
    // pair the Vnode fd needs.
    k.vfs.mkdir("/adir", 0o755).expect("mkdir");
    let (mount_id, ino) = k.vfs.resolve("/adir").expect("resolve");
    k.install_fd(
        pid,
        11,
        FdObject::Vnode { mount_id, ino },
        FdFlags::EMPTY,
    )
    .unwrap();
    let mut heap = vec![0u8; 128];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_GET,
        flags: 0,
        request_id: 703,
        args: u32_args(11),
        heap_ptr: 0,
        heap_len: 64,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(heap[16], 3, "filetype = directory");
    assert_eq!(filestat_u64(&heap, 0, 32), 0, "size (directory)");
}

#[test]
fn fd_filestat_get_on_pipe_read_fd_returns_filetype_unknown() {
    // Pipes have no WASI filetype; WASI returns UNKNOWN (0) for
    // FIFOs/pipes. `nlink=1, size=0, times=0` are synthesised.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "piper", 0);
    k.install_fd(pid, 3, FdObject::PipeRead(7), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 128];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_GET,
        flags: 0,
        request_id: 704,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 64,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(heap[16], 0, "filetype = unknown");
    assert_eq!(filestat_u64(&heap, 0, 8), 7, "ino = opaque pipe id");
}

#[test]
fn fd_filestat_get_on_signal_channel_fd_returns_filetype_unknown() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "signaler", 0);
    k.install_fd(pid, 5, FdObject::SignalChannel, FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 128];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_GET,
        flags: 0,
        request_id: 705,
        args: u32_args(5),
        heap_ptr: 0,
        heap_len: 64,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(heap[16], 0, "filetype = unknown");
    assert_eq!(filestat_u64(&heap, 0, 0), 0, "dev");
    assert_eq!(filestat_u64(&heap, 0, 8), 0, "ino");
}

#[test]
fn fd_filestat_get_on_invalid_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "stater", 0);
    let mut heap = vec![0u8; 128];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_GET,
        flags: 0,
        request_id: 706,
        args: u32_args(99), // fd 99 is not open
        heap_ptr: 0,
        heap_len: 64,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
    // Heap must not have been written.
    assert!(heap.iter().all(|&b| b == 0));
}

// ---- fd_prestat_get ---------------------------------------------------

#[test]
fn fd_prestat_get_always_returns_ebadf() {
    // PMos has no preopen dirs; the WASI preopen-discovery loop
    // hits EBADF at the first probe and stops. The handler ignores
    // the fd entirely — every fd gets the same EBADF.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "prestat", 0);
    let mut heap = vec![0u8; 16];

    for fd in [3u32, 4, 7, 42] {
        let req = Request {
            opcode: op_wasi::FD_PRESTAT_GET,
            flags: 0,
            request_id: 91,
            args: u32_args(fd),
            heap_ptr: 0,
            heap_len: 0,
        };
        let resp = dispatch(&mut k, pid, &req, &mut heap);
        assert_eq!(resp.status, -errno::EBADF, "fd={fd}");
    }
}

// ---- IPC sockets ------------------------------------------------------
//
// Covers the 5 new generic IPC opcodes (`IPC_SOCKET`, `IPC_BIND`,
// `IPC_LISTEN`, `IPC_CONNECT`, `IPC_ACCEPT`) plus the bigger end-to-
// end round trip that glues them together: server binds + listens,
// client connects, server accepts, both ends exchange bytes through
// the existing `FD_READ` / `FD_WRITE` opcodes (which already route
// `FdObject::Socket` to the right `ipc.send_on_socket` /
// `recv_on_socket` paths).
//
// The full round-trip test is the one that actually matters. The
// per-opcode tests guard the argument-decoding + error-mapping
// behaviour of each handler in isolation.

/// Build a 16-byte args window with a single u32 at offset 0 and
/// another at offset 4. Used by `ipc_listen` (fd + backlog) and
/// anything else that packs two u32s.
fn u32_u32_args(a: u32, b: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&a.to_le_bytes());
    args[4..8].copy_from_slice(&b.to_le_bytes());
    args
}

#[test]
fn ipc_socket_allocates_a_fresh_socket_fd() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "socketeer", 0);
    let mut heap = vec![0u8; 64];

    // Type 0 = Stream.
    let req = Request {
        opcode: op_ext::IPC_SOCKET,
        flags: 0,
        request_id: 90,
        args: u32_args(0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let fd = resp.value as u32;
    assert!(k.fds(pid).unwrap().get(fd).is_some());
}

#[test]
fn ipc_socket_rejects_invalid_type_with_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "socketeer", 0);
    let mut heap = vec![0u8; 64];

    // Type 99 isn't a valid SocketType discriminant.
    let req = Request {
        opcode: op_ext::IPC_SOCKET,
        flags: 0,
        request_id: 91,
        args: u32_args(99),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn ipc_bind_and_listen_transition_a_fresh_socket() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "server", 0);
    let mut heap = vec![0u8; 128];

    // Create socket.
    let sock_req = Request {
        opcode: op_ext::IPC_SOCKET,
        flags: 0,
        request_id: 100,
        args: u32_args(0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let fd = dispatch(&mut k, pid, &sock_req, &mut heap).value as u32;

    // Bind to "/tmp/sock".
    let path = b"/tmp/sock";
    heap[..path.len()].copy_from_slice(path);
    let bind_req = Request {
        opcode: op_ext::IPC_BIND,
        flags: 0,
        request_id: 101,
        args: u32_args(fd),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let bind_resp = dispatch(&mut k, pid, &bind_req, &mut heap);
    assert_eq!(bind_resp.status, 0);

    // Listen with backlog 4.
    let listen_req = Request {
        opcode: op_ext::IPC_LISTEN,
        flags: 0,
        request_id: 102,
        args: u32_u32_args(fd, 4),
        heap_ptr: 0,
        heap_len: 0,
    };
    let listen_resp = dispatch(&mut k, pid, &listen_req, &mut heap);
    assert_eq!(listen_resp.status, 0);
}

#[test]
fn ipc_bind_on_already_bound_path_returns_eaddrinuse() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "server", 0);
    let mut heap = vec![0u8; 128];

    // First server binds /tmp/x successfully.
    let fd1 = dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::IPC_SOCKET,
            flags: 0,
            request_id: 110,
            args: u32_args(0),
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    )
    .value as u32;
    let path = b"/tmp/x";
    heap[..path.len()].copy_from_slice(path);
    assert_eq!(
        dispatch(
            &mut k,
            pid,
            &Request {
                opcode: op_ext::IPC_BIND,
                flags: 0,
                request_id: 111,
                args: u32_args(fd1),
                heap_ptr: 0,
                heap_len: path.len() as u32,
            },
            &mut heap,
        )
        .status,
        0,
    );

    // Second socket tries to bind the same path — EADDRINUSE.
    let fd2 = dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::IPC_SOCKET,
            flags: 0,
            request_id: 112,
            args: u32_args(0),
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    )
    .value as u32;
    heap[..path.len()].copy_from_slice(path);
    let resp = dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::IPC_BIND,
            flags: 0,
            request_id: 113,
            args: u32_args(fd2),
            heap_ptr: 0,
            heap_len: path.len() as u32,
        },
        &mut heap,
    );
    assert_eq!(resp.status, -errno::EADDRINUSE);
}

#[test]
fn ipc_connect_to_nonexistent_path_returns_econnrefused() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "client", 0);
    let mut heap = vec![0u8; 128];

    let fd = dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::IPC_SOCKET,
            flags: 0,
            request_id: 120,
            args: u32_args(0),
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    )
    .value as u32;

    let path = b"/nope/no-one-here";
    heap[..path.len()].copy_from_slice(path);
    let resp = dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::IPC_CONNECT,
            flags: 0,
            request_id: 121,
            args: u32_args(fd),
            heap_ptr: 0,
            heap_len: path.len() as u32,
        },
        &mut heap,
    );
    assert_eq!(resp.status, -errno::ECONNREFUSED);
}

#[test]
fn ipc_accept_on_empty_listener_returns_eagain() {
    // Listener exists and is in `Listening` state, but no client
    // has connected. accept_socket on `IpcTable` returns
    // `IpcError::WouldBlock` which `kerr_to_errno` maps to EAGAIN.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "server", 0);
    let mut heap = vec![0u8; 128];

    let listener_fd = dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::IPC_SOCKET,
            flags: 0,
            request_id: 130,
            args: u32_args(0),
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    )
    .value as u32;
    let path = b"/tmp/lonely";
    heap[..path.len()].copy_from_slice(path);
    dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::IPC_BIND,
            flags: 0,
            request_id: 131,
            args: u32_args(listener_fd),
            heap_ptr: 0,
            heap_len: path.len() as u32,
        },
        &mut heap,
    );
    dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::IPC_LISTEN,
            flags: 0,
            request_id: 132,
            args: u32_u32_args(listener_fd, 4),
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    );

    let resp = dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::IPC_ACCEPT,
            flags: 0,
            request_id: 133,
            args: u32_args(listener_fd),
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    );
    assert_eq!(resp.status, -errno::EAGAIN);
}

#[test]
fn ipc_accept_on_non_socket_fd_returns_einval() {
    // A process with fd 0 wired to /dev/console tries to accept on
    // fd 0. The fd exists but it's not a Socket object, so the
    // handler returns EINVAL (via `KernelError::NotSupportedOnFd`
    // -> `kerr_to_errno`).
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "confused", 0);
    k.install_fd(pid, 0, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];

    let resp = dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::IPC_ACCEPT,
            flags: 0,
            request_id: 140,
            args: u32_args(0),
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    );
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn ipc_round_trip_server_accepts_client_and_they_exchange_bytes() {
    // The big end-to-end test. Two processes in one kernel:
    //   server: socket + bind("/tmp/rt") + listen + accept
    //   client: socket + connect("/tmp/rt")
    //   server reads what the client wrote via FD_READ on the
    //     accepted fd.
    //   client reads what the server wrote via FD_READ on the
    //     connected fd.
    //
    // This exercises every IPC opcode + proves the socket fd
    // objects route correctly through the existing fd_read /
    // fd_write paths.
    let mut k = make_kernel();
    let server = make_running_proc(&mut k, "server", 0);
    let client = make_running_proc(&mut k, "client", 0);
    let mut heap = vec![0u8; 256];

    let path = b"/tmp/rt";

    // --- server side: socket + bind + listen ------------------

    let srv_listener = dispatch(
        &mut k,
        server,
        &Request {
            opcode: op_ext::IPC_SOCKET,
            flags: 0,
            request_id: 200,
            args: u32_args(0),
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    )
    .value as u32;

    heap[..path.len()].copy_from_slice(path);
    assert_eq!(
        dispatch(
            &mut k,
            server,
            &Request {
                opcode: op_ext::IPC_BIND,
                flags: 0,
                request_id: 201,
                args: u32_args(srv_listener),
                heap_ptr: 0,
                heap_len: path.len() as u32,
            },
            &mut heap,
        )
        .status,
        0,
    );
    assert_eq!(
        dispatch(
            &mut k,
            server,
            &Request {
                opcode: op_ext::IPC_LISTEN,
                flags: 0,
                request_id: 202,
                args: u32_u32_args(srv_listener, 4),
                heap_ptr: 0,
                heap_len: 0,
            },
            &mut heap,
        )
        .status,
        0,
    );

    // --- client side: socket + connect ------------------------

    let cli_fd = dispatch(
        &mut k,
        client,
        &Request {
            opcode: op_ext::IPC_SOCKET,
            flags: 0,
            request_id: 203,
            args: u32_args(0),
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    )
    .value as u32;

    heap[..path.len()].copy_from_slice(path);
    assert_eq!(
        dispatch(
            &mut k,
            client,
            &Request {
                opcode: op_ext::IPC_CONNECT,
                flags: 0,
                request_id: 204,
                args: u32_args(cli_fd),
                heap_ptr: 0,
                heap_len: path.len() as u32,
            },
            &mut heap,
        )
        .status,
        0,
    );

    // --- server side: accept ---------------------------------

    let srv_conn = dispatch(
        &mut k,
        server,
        &Request {
            opcode: op_ext::IPC_ACCEPT,
            flags: 0,
            request_id: 205,
            args: u32_args(srv_listener),
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    )
    .value as u32;

    // --- client writes "hi" via FD_WRITE, server reads it ----

    heap[..2].copy_from_slice(b"hi");
    let write_resp = dispatch(
        &mut k,
        client,
        &Request {
            opcode: op_wasi::FD_WRITE,
            flags: 0,
            request_id: 210,
            args: u32_args(cli_fd),
            heap_ptr: 0,
            heap_len: 2,
        },
        &mut heap,
    );
    assert_eq!(write_resp.status, 0);
    assert_eq!(write_resp.value, 2);

    // Zero the heap so FD_READ's output is unambiguous.
    for b in heap.iter_mut() {
        *b = 0;
    }
    let read_resp = dispatch(
        &mut k,
        server,
        &Request {
            opcode: op_wasi::FD_READ,
            flags: 0,
            request_id: 211,
            args: u32_args(srv_conn),
            heap_ptr: 0,
            heap_len: 16,
        },
        &mut heap,
    );
    assert_eq!(read_resp.status, 0);
    assert_eq!(read_resp.value, 2);
    assert_eq!(&heap[..2], b"hi");
}

// ---- display_connect --------------------------------------------------

#[test]
fn display_connect_with_server_listening_returns_connected_fd() {
    // Set up a display server using the semantic `Kernel::display_bind`
    // method (which installs a listener at /run/display with
    // DisplayServer cap). Then a DisplayClient-cap process calls
    // DISPLAY_CONNECT via the opcode and should get back a connected
    // fd.
    let mut k = make_kernel();
    let srv = k
        .register_process(RegisterArgs {
            name: "srv",
            ppid: 0,
            caps: abi::cap::initial::DISPLAY_SERVER,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(srv).unwrap();
    k.procs.transition(srv, ProcState::Running).unwrap();
    k.display_bind(srv).expect("display_bind");

    let client = k
        .register_process(RegisterArgs {
            name: "cli",
            ppid: 0,
            caps: abi::cap::initial::DESKTOP_SHELL,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(client).unwrap();
    k.procs.transition(client, ProcState::Running).unwrap();

    let mut heap = vec![0u8; 32];
    let resp = dispatch(
        &mut k,
        client,
        &Request {
            opcode: op_ext::DISPLAY_CONNECT,
            flags: 0,
            request_id: 220,
            args: [0u8; 16],
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    );
    assert_eq!(resp.status, 0);
    let fd = resp.value as u32;
    assert!(k.fds(client).unwrap().get(fd).is_some());
}

#[test]
fn display_bind_installs_listener_at_run_display_for_display_server_cap() {
    // Symmetric to the existing DISPLAY_CONNECT happy-path
    // test: a process with DISPLAY_SERVER caps dispatches
    // DISPLAY_BIND and gets back a listener fd. The kernel's
    // `Kernel::display_bind` already creates the socket,
    // binds `/run/display`, and transitions to Listening — the
    // opcode handler is a trivial adapter.
    //
    // A subsequent DISPLAY_CONNECT call from a different
    // DISPLAY_CLIENT process should then succeed (proving the
    // listener really is bound at the well-known path).
    let mut k = make_kernel();
    let srv = k
        .register_process(RegisterArgs {
            name: "srv",
            ppid: 0,
            caps: abi::cap::initial::DISPLAY_SERVER,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(srv).unwrap();
    k.procs.transition(srv, ProcState::Running).unwrap();

    let mut heap = vec![0u8; 32];
    let resp = dispatch(
        &mut k,
        srv,
        &Request {
            opcode: op_ext::DISPLAY_BIND,
            flags: 0,
            request_id: 230,
            args: [0u8; 16],
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    );
    assert_eq!(resp.status, 0);
    let listener_fd = resp.value as u32;
    assert!(k.fds(srv).unwrap().get(listener_fd).is_some());

    // A DISPLAY_CLIENT-capable process can now connect to the
    // freshly-bound listener.
    let cli = k
        .register_process(RegisterArgs {
            name: "cli",
            ppid: 0,
            caps: abi::cap::initial::DESKTOP_SHELL,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(cli).unwrap();
    k.procs.transition(cli, ProcState::Running).unwrap();

    let connect_resp = dispatch(
        &mut k,
        cli,
        &Request {
            opcode: op_ext::DISPLAY_CONNECT,
            flags: 0,
            request_id: 231,
            args: [0u8; 16],
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    );
    assert_eq!(connect_resp.status, 0);
}

#[test]
fn display_bind_without_display_server_cap_returns_enotcapable() {
    // A process with DISPLAY_CLIENT but NOT DISPLAY_SERVER
    // can't claim ownership of /run/display. The cap check in
    // `Kernel::display_bind` fires first, producing
    // `KernelError::NotCapable` → `ENOTCAPABLE` via
    // `kerr_to_errno`.
    let mut k = make_kernel();
    let pid = k
        .register_process(RegisterArgs {
            name: "cli-only",
            ppid: 0,
            caps: abi::cap::initial::DESKTOP_SHELL,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(pid).unwrap();
    k.procs.transition(pid, ProcState::Running).unwrap();

    let mut heap = vec![0u8; 32];
    let resp = dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::DISPLAY_BIND,
            flags: 0,
            request_id: 232,
            args: [0u8; 16],
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    );
    assert_eq!(resp.status, -errno::ENOTCAPABLE);
}

#[test]
fn display_bind_second_caller_returns_eaddrinuse() {
    // Only one display server can exist at a time — a second
    // DISPLAY_BIND on /run/display while the first is still
    // live fails with EADDRINUSE. Guards against a future bug
    // where a misbehaving (or compromised) display-server
    // replacement could accidentally mask the real one.
    let mut k = make_kernel();
    let srv_a = k
        .register_process(RegisterArgs {
            name: "srv-a",
            ppid: 0,
            caps: abi::cap::initial::DISPLAY_SERVER,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(srv_a).unwrap();
    k.procs.transition(srv_a, ProcState::Running).unwrap();

    let mut heap = vec![0u8; 32];
    assert_eq!(
        dispatch(
            &mut k,
            srv_a,
            &Request {
                opcode: op_ext::DISPLAY_BIND,
                flags: 0,
                request_id: 233,
                args: [0u8; 16],
                heap_ptr: 0,
                heap_len: 0,
            },
            &mut heap,
        )
        .status,
        0,
    );

    let srv_b = k
        .register_process(RegisterArgs {
            name: "srv-b",
            ppid: 0,
            caps: abi::cap::initial::DISPLAY_SERVER,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(srv_b).unwrap();
    k.procs.transition(srv_b, ProcState::Running).unwrap();

    let resp = dispatch(
        &mut k,
        srv_b,
        &Request {
            opcode: op_ext::DISPLAY_BIND,
            flags: 0,
            request_id: 234,
            args: [0u8; 16],
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    );
    assert_eq!(resp.status, -errno::EADDRINUSE);
}

#[test]
fn display_connect_without_display_client_cap_returns_enotcapable() {
    // An ordinary app without DisplayClient cap can't connect.
    // `Kernel::display_connect` returns NotCapable, which
    // `kerr_to_errno` maps to ENOTCAPABLE (76).
    let mut k = make_kernel();
    // Even simpler: register with an empty cap set.
    let pid = k
        .register_process(RegisterArgs {
            name: "capless",
            ppid: 0,
            caps: abi::cap::CapSet::EMPTY,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(pid).unwrap();
    k.procs.transition(pid, ProcState::Running).unwrap();

    let mut heap = vec![0u8; 32];
    let resp = dispatch(
        &mut k,
        pid,
        &Request {
            opcode: op_ext::DISPLAY_CONNECT,
            flags: 0,
            request_id: 221,
            args: [0u8; 16],
            heap_ptr: 0,
            heap_len: 0,
        },
        &mut heap,
    );
    assert_eq!(resp.status, -errno::ENOTCAPABLE);
}

// ---- proc_spawn -------------------------------------------------------
//
// Layout recap:
//   args[0..4]  = path_len
//   args[4..12] = caps bitset (u64 LE)
//   heap[heap_ptr .. heap_ptr + path_len] = UTF-8 path
//
// Happy path: parent holds CAP_ALL, child gets CAP_ALL (subset rule
// trivially satisfied), parent has fd 0/1/2 wired to /dev/console so
// the stdio inheritance check passes, NativePlatform records the
// spawn_process call. Result: a new pid that didn't exist before the
// call, and a matching SpawnCall in `NativeState::spawn_calls`.

/// Build a 16-byte args window for `PROC_SPAWN` with `path_len` at
/// offset 0 and `caps_bits` packed as u64 LE at offset 4. This
/// helper exists so the spawn-specific byte layout lives in one
/// place and a rewording of the fields doesn't scatter through
/// every test case.
fn proc_spawn_args(path_len: u32, caps_bits: u64) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&path_len.to_le_bytes());
    args[4..12].copy_from_slice(&caps_bits.to_le_bytes());
    args
}

fn install_default_stdio(k: &mut Kernel, pid: abi::ext::Pid) {
    k.install_fd(pid, 0, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    k.install_fd(pid, 2, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
}

#[test]
fn proc_spawn_creates_child_and_records_platform_spawn_call() {
    // Start from a clean NativePlatform so prior tests' spawn_calls
    // don't leak into the assertion below.
    kernel::platform::native::reset();

    let mut k = make_kernel();
    let parent = make_running_proc(&mut k, "parent", 0);
    install_default_stdio(&mut k, parent);

    let path = "/usr/bin/hello";
    let mut heap = vec![0u8; 4096];
    heap[..path.len()].copy_from_slice(path.as_bytes());

    let req = Request {
        opcode: op_ext::PROC_SPAWN,
        flags: 0,
        request_id: 70,
        args: proc_spawn_args(path.len() as u32, abi::cap::initial::INIT.0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, parent, &req, &mut heap);

    assert_eq!(resp.status, 0);
    assert!(resp.value > parent as i64, "new pid must be positive and fresh");
    let new_pid = resp.value as i32;
    assert!(k.procs.is_alive(new_pid));
    assert_eq!(k.procs.get(new_pid).unwrap().ppid, parent);
    // Child inherited the three stdio fd objects.
    let child_fds = k.fds(new_pid).unwrap();
    assert!(child_fds.get(0).is_some());
    assert!(child_fds.get(1).is_some());
    assert!(child_fds.get(2).is_some());

    // Platform was asked to spawn a Worker for the new pid.
    kernel::platform::native::with_state(|s| {
        assert_eq!(s.spawn_calls.len(), 1);
        let call = &s.spawn_calls[0];
        assert_eq!(call.pid, new_pid);
        assert_eq!(call.path, "/usr/bin/hello");
    });
}

#[test]
fn proc_spawn_rolls_back_when_platform_refuses() {
    // Programme NativePlatform to fail the next spawn_process call.
    // The PROC_SPAWN handler should roll the new pid all the way back
    // so no child process is leaked: the pid does not exist after
    // the call, and the kernel returns -EIO.
    kernel::platform::native::reset();
    kernel::platform::native::with_state(|s| {
        s.next_spawn_error = Some(kernel::platform::DriverError::NotReady);
    });

    let mut k = make_kernel();
    let parent = make_running_proc(&mut k, "parent", 0);
    install_default_stdio(&mut k, parent);

    let path = "/usr/bin/nope";
    let mut heap = vec![0u8; 4096];
    heap[..path.len()].copy_from_slice(path.as_bytes());

    // Remember which pids exist pre-call so we can assert that NO new
    // pid survives the rollback.
    let before_alive: Vec<_> = (0..20)
        .filter(|p| k.procs.is_alive(*p as i32))
        .collect();

    let req = Request {
        opcode: op_ext::PROC_SPAWN,
        flags: 0,
        request_id: 71,
        args: proc_spawn_args(path.len() as u32, abi::cap::initial::INIT.0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, parent, &req, &mut heap);

    assert_eq!(resp.status, -errno::EIO);
    // No new pid in the process table: the set of alive pids is
    // the same as before the call.
    let after_alive: Vec<_> = (0..20)
        .filter(|p| k.procs.is_alive(*p as i32))
        .collect();
    assert_eq!(before_alive, after_alive);
    // Platform recorded no successful spawn (the error consumed the
    // `next_spawn_error` slot without appending).
    kernel::platform::native::with_state(|s| {
        assert_eq!(s.spawn_calls.len(), 0);
    });
}

#[test]
fn proc_spawn_with_missing_stdio_returns_einval() {
    // A parent that has no fd 0 can't inherit stdio into a child.
    // The opcode handler refuses rather than fabricating a sentinel
    // (see the note in the handler's docstring).
    kernel::platform::native::reset();

    let mut k = make_kernel();
    let parent = make_running_proc(&mut k, "stdio-less", 0);
    // Install fd 1 and fd 2 but NOT fd 0.
    k.install_fd(parent, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    k.install_fd(parent, 2, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();

    let path = "/usr/bin/anything";
    let mut heap = vec![0u8; 4096];
    heap[..path.len()].copy_from_slice(path.as_bytes());

    let req = Request {
        opcode: op_ext::PROC_SPAWN,
        flags: 0,
        request_id: 72,
        args: proc_spawn_args(path.len() as u32, abi::cap::initial::INIT.0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, parent, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
    // No spawn call was made.
    kernel::platform::native::with_state(|s| {
        assert_eq!(s.spawn_calls.len(), 0);
    });
}

#[test]
fn proc_spawn_rejects_cap_superset() {
    // A parent that does NOT hold `Cap::CapGrant` cannot spawn a
    // child with it. The `Kernel::proc_spawn` subset check fires
    // and `kerr_to_errno` maps `NotCapable` to ENOTCAPABLE.
    kernel::platform::native::reset();

    let mut k = make_kernel();
    // Ordinary-app parent: only DisplayClient. Initial::ORDINARY_APP
    // is `CapSet::from_caps(&[Cap::DisplayClient])`.
    let parent = k
        .register_process(RegisterArgs {
            name: "ordinary",
            ppid: 0,
            caps: abi::cap::initial::ORDINARY_APP,
            cwd: "/",
        })
        .unwrap();
    k.mark_ready(parent).unwrap();
    k.procs
        .transition(parent, ProcState::Running)
        .unwrap();
    install_default_stdio(&mut k, parent);

    let path = "/usr/bin/escalator";
    let mut heap = vec![0u8; 4096];
    heap[..path.len()].copy_from_slice(path.as_bytes());

    // Request CapSet::ALL for the child — parent doesn't hold it.
    let req = Request {
        opcode: op_ext::PROC_SPAWN,
        flags: 0,
        request_id: 73,
        args: proc_spawn_args(path.len() as u32, abi::cap::CapSet::ALL.0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, parent, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTCAPABLE);
}

#[test]
fn proc_spawn_rejects_invalid_utf8_path() {
    kernel::platform::native::reset();

    let mut k = make_kernel();
    let parent = make_running_proc(&mut k, "parent", 0);
    install_default_stdio(&mut k, parent);

    let mut heap = vec![0u8; 4096];
    heap[..4].copy_from_slice(&[0xff, 0xfe, 0xfd, 0xfc]);

    let req = Request {
        opcode: op_ext::PROC_SPAWN,
        flags: 0,
        request_id: 74,
        args: proc_spawn_args(4, abi::cap::initial::INIT.0),
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, parent, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

// ---- request_id echo is universal -------------------------------------

#[test]
fn request_id_is_echoed_on_every_response_shape() {
    // OK response, error response, ENOSYS — all must echo
    // request_id. Drift here would break userland's match-response-
    // to-request logic, so it's worth one tight assertion.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "echo", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();

    let mut heap = vec![0u8; 64];
    heap[..1].copy_from_slice(b"z");

    // OK path: fd_write, request_id = 100.
    let req_ok = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 100,
        args: u32_args(1),
        heap_ptr: 0,
        heap_len: 1,
    };
    let resp_ok = dispatch(&mut k, pid, &req_ok, &mut heap);
    assert_eq!(resp_ok.request_id, 100);
    assert_eq!(resp_ok.status, 0);

    // Error path: fd_write with bad fd, request_id = 101.
    let req_err = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 101,
        args: u32_args(99),
        heap_ptr: 0,
        heap_len: 1,
    };
    let resp_err = dispatch(&mut k, pid, &req_err, &mut heap);
    assert_eq!(resp_err.request_id, 101);
    assert_eq!(resp_err.status, -errno::EBADF);

    // ENOSYS path: unknown opcode, request_id = 102.
    let req_nosys = Request {
        opcode: 0x0990, // in neither range
        flags: 0,
        request_id: 102,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp_nosys = dispatch(&mut k, pid, &req_nosys, &mut heap);
    assert_eq!(resp_nosys.request_id, 102);
    assert_eq!(resp_nosys.status, -errno::ENOSYS);
}

// ---- service_one end-to-end through a real Sab ------------------------

#[test]
fn service_one_pops_request_services_and_pushes_response() {
    // Full round trip: push a Request into a real SAB-backed
    // ring, call Dispatcher::service_one, pop the Response off the
    // other ring, assert. This is the one test that proves the
    // dispatcher actually wires the ring crate's producer/consumer
    // pair — every other test calls `dispatch` directly.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ringed", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();

    // Backing storage for the SAB. In production this is a real
    // SharedArrayBuffer; here it's a Vec that outlives the Sab.
    let mut sab_mem: Vec<u8> = vec![0u8; SAB_SIZE];
    let mut sab = Sab::from_slice(&mut sab_mem);
    sab.init();

    // Separate heap Vec — mirrors the layout in production where
    // the heap lives at offset 0x8000 inside the same SAB, but the
    // dispatcher takes it as a separate parameter so tests can use
    // an independent allocation.
    let mut heap = vec![0u8; 4096];
    heap[..5].copy_from_slice(b"ringy");

    // Userland pushes a FD_WRITE request.
    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 200,
        args: u32_args(1),
        heap_ptr: 0,
        heap_len: 5,
    };
    assert!(sab.try_push_request(&req), "push request");
    assert_eq!(sab.request_len(), 1);

    // Kernel side: dispatcher services one.
    let mut disp = Dispatcher::new(&mut k, pid);
    assert!(disp.service_one(&sab, &mut heap), "serviced one");
    assert_eq!(sab.request_len(), 0, "request consumed");

    // Userland side: pops the response.
    let resp = sab.try_pop_response().expect("response available");
    assert_eq!(resp.request_id, 200);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 5);
    // And the bytes really did land on /dev/console.
    assert_eq!(k.devs.drain_console_output(), b"ringy");
}

#[test]
fn service_one_returns_false_when_ring_empty() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "idle", 0);
    let mut sab_mem: Vec<u8> = vec![0u8; SAB_SIZE];
    let mut sab = Sab::from_slice(&mut sab_mem);
    sab.init();
    let mut heap = vec![0u8; 64];

    let mut disp = Dispatcher::new(&mut k, pid);
    assert!(!disp.service_one(&sab, &mut heap));
}
