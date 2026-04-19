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
use kernel::proc::{ExitStatus, ProcState, Signal};
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
    // `FD_FDSTAT_SET_RIGHTS` is in the WASI range (0x0026) but still
    // has no handler — and isn't planned for v1 (WASI's rights system
    // is un-v1-relevant). Same shape as the ext-side test below:
    // decoded as a known WASI opcode, routed to `dispatch_wasi`'s
    // `_ =>` arm, ENOSYS echoed back with the request_id intact.
    //
    // (This probe was `FD_PRESTAT_DIR_NAME` before that handler
    // landed, then `PATH_READLINK`, `PATH_SYMLINK`, `PATH_LINK`,
    // `SOCK_SHUTDOWN`, `FD_PREAD`, `FD_READDIR`. When
    // `FD_FDSTAT_SET_RIGHTS` grows a real handler — if ever — swap
    // this probe to whatever's still unhandled at that point, or
    // delete the test once every WASI opcode has real coverage.)
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "test", 0);
    let mut heap = vec![0u8; 4096];

    let req = Request {
        opcode: op_wasi::FD_FDSTAT_SET_RIGHTS,
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
    // `CAP_GRANT` is in the extension range (0x1302) but still has
    // no handler. Same shape as the WASI case above: decoded as a
    // known extension opcode, routed to `dispatch_ext`'s `_ =>`
    // arm, ENOSYS echoed back with the request_id intact.
    //
    // (This probe was `PROC_SPAWN` before that handler landed, then
    // `PROC_WAIT`, then `PROC_KILL`, then `PROC_CAPS_GET`. CAP_GRANT
    // is a v2-era concern — delegating cap edits to userland is out
    // of scope for v1 — so it's a stable long-term probe target.)
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "test", 0);
    let mut heap = vec![0u8; 4096];

    let req = Request {
        opcode: op_ext::CAP_GRANT,
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

// ---- path_open + WASI fdflags translation ---------------------------
//
// WASI's fdflags encoding (APPEND=0x01, DSYNC=0x02, NONBLOCK=0x04,
// RSYNC=0x08, SYNC=0x10) differs from PMos's internal FdFlags
// (CLOEXEC=0x01, NONBLOCK=0x02, APPEND=0x04). Userland passes WASI
// bits via path_open's args[0..4] window, so the handler must
// translate before storing. These regression tests pin the correct
// mapping: an fd opened with WASI APPEND must end up with PMos
// APPEND (not CLOEXEC, which would mis-drop the fd on proc_spawn);
// WASI NONBLOCK must map to PMos NONBLOCK (not PMos APPEND, which
// would mis-route writes to EOF); WASI's sync bits DSYNC/RSYNC/SYNC
// are accepted and discarded since v1's tmpfs is synchronous —
// setting any of them must not set any PMos bit.

#[test]
fn path_open_with_wasi_append_sets_pmos_append() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "opener", 0);
    k.vfs.create("/o.txt", 0o644).expect("create");
    let path = b"/o.txt";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_OPEN,
        flags: 0,
        request_id: 960,
        args: u32_args(abi::wasi::fdflags::APPEND as u32),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let fd = resp.value as u32;
    let e = k.fds(pid).unwrap().get(fd).unwrap();
    assert!(e.flags.contains(FdFlags::APPEND), "WASI APPEND → PMos APPEND");
    assert!(!e.flags.contains(FdFlags::CLOEXEC), "must not set CLOEXEC");
    assert!(!e.flags.contains(FdFlags::NONBLOCK), "must not set NONBLOCK");
}

#[test]
fn path_open_with_wasi_nonblock_sets_pmos_nonblock() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "opener", 0);
    k.vfs.create("/o.txt", 0o644).expect("create");
    let path = b"/o.txt";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_OPEN,
        flags: 0,
        request_id: 961,
        args: u32_args(abi::wasi::fdflags::NONBLOCK as u32),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let fd = resp.value as u32;
    let e = k.fds(pid).unwrap().get(fd).unwrap();
    assert!(e.flags.contains(FdFlags::NONBLOCK), "WASI NONBLOCK → PMos NONBLOCK");
    assert!(!e.flags.contains(FdFlags::APPEND), "must not set APPEND");
    assert!(!e.flags.contains(FdFlags::CLOEXEC), "must not set CLOEXEC");
}

#[test]
fn path_open_with_wasi_sync_bits_sets_no_pmos_bits() {
    // DSYNC + RSYNC + SYNC are meaningful on platforms with
    // durable-write guarantees; v1's tmpfs writes are already
    // synchronous into in-memory state, so the bits are accepted
    // and discarded — none of them map to a PMos bit.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "opener", 0);
    k.vfs.create("/o.txt", 0o644).expect("create");
    let path = b"/o.txt";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let combined = (abi::wasi::fdflags::DSYNC
        | abi::wasi::fdflags::RSYNC
        | abi::wasi::fdflags::SYNC) as u32;
    let req = Request {
        opcode: op_wasi::PATH_OPEN,
        flags: 0,
        request_id: 962,
        args: u32_args(combined),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let fd = resp.value as u32;
    let e = k.fds(pid).unwrap().get(fd).unwrap();
    assert_eq!(e.flags, FdFlags::EMPTY, "sync-family bits discard to EMPTY");
}

#[test]
fn path_open_with_wasi_append_and_nonblock_sets_both_pmos_bits() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "opener", 0);
    k.vfs.create("/o.txt", 0o644).expect("create");
    let path = b"/o.txt";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let combined =
        (abi::wasi::fdflags::APPEND | abi::wasi::fdflags::NONBLOCK) as u32;
    let req = Request {
        opcode: op_wasi::PATH_OPEN,
        flags: 0,
        request_id: 963,
        args: u32_args(combined),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let fd = resp.value as u32;
    let e = k.fds(pid).unwrap().get(fd).unwrap();
    assert!(e.flags.contains(FdFlags::APPEND));
    assert!(e.flags.contains(FdFlags::NONBLOCK));
    assert!(!e.flags.contains(FdFlags::CLOEXEC));
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

// ---- path_filestat_get ------------------------------------------------
//
// Path-based sibling of `fd_filestat_get`. Writes the same 64-byte
// `filestat_t` wire layout; the only genuinely new surface is the
// path-string input decoding and the ENOENT error path for a
// missing path. `dir_fd` + `lookup_flags` are part of the documented
// wire layout but are ignored in v1 (PMos has no preopens and the
// v1 VFS doesn't follow symlinks either way). Reuses the `filestat_u64`
// helper defined above the fd_filestat_get block.
//
// Five tests cover the reachable branches: tmpfs regular file with
// a known size, tmpfs directory, char-device path (/dev/console),
// missing path, and the root directory "/". A sixth test pins the
// "dir_fd + flags are ignored" contract so a future slice that
// wires them can't silently break the layout.

fn path_filestat_args(dir_fd: u32, flags: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&dir_fd.to_le_bytes());
    args[4..8].copy_from_slice(&flags.to_le_bytes());
    args
}

#[test]
fn path_filestat_get_on_tmpfs_regular_file_returns_filetype_and_size() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "pathstater", 0);
    let bytes: &[u8] = b"hello, path_filestat_get";
    k.vfs.create("/probe.txt", 0o644).expect("create");
    let wrote = k.vfs.write("/probe.txt", 0, bytes).expect("write");
    assert_eq!(wrote, bytes.len());
    let (mount_id, ino) = k.vfs.resolve("/probe.txt").expect("resolve");

    let mut heap = vec![0u8; 128];
    let path = b"/probe.txt";
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_GET,
        flags: 0,
        request_id: 720,
        args: path_filestat_args(0, 0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert_eq!(resp.extra_len, 64);
    assert_eq!(filestat_u64(&heap, 0, 0), mount_id.0 as u64, "dev");
    assert_eq!(filestat_u64(&heap, 0, 8), ino, "ino");
    assert_eq!(heap[16], 4, "filetype = regular_file");
    assert_eq!(&heap[17..24], &[0u8; 7], "filetype padding");
    assert_eq!(filestat_u64(&heap, 0, 24), 1, "nlink");
    assert_eq!(filestat_u64(&heap, 0, 32), bytes.len() as u64, "size");
    // Real vnode timestamps: tmpfs threads Platform::now_realtime_ns
    // through create + write. All three times are non-zero on a freshly-
    // written tmpfs file (create stamps all three, write advances mtime
    // + ctime); exact values come from the wall clock so only the
    // "is nonzero" invariant is asserted here. The native platform's
    // realtime clock reads SystemTime::now(), which is always > 0 in
    // any reasonable test environment.
    assert!(filestat_u64(&heap, 0, 40) > 0, "atim nonzero");
    assert!(filestat_u64(&heap, 0, 48) > 0, "mtim nonzero");
    assert!(filestat_u64(&heap, 0, 56) > 0, "ctim nonzero");
}

#[test]
fn path_filestat_get_on_tmpfs_directory_returns_filetype_directory() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "dirpather", 0);
    k.vfs.mkdir("/adir", 0o755).expect("mkdir");
    let (mount_id, ino) = k.vfs.resolve("/adir").expect("resolve");

    let mut heap = vec![0u8; 128];
    let path = b"/adir";
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_GET,
        flags: 0,
        request_id: 721,
        args: path_filestat_args(0, 0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.extra_len, 64);
    assert_eq!(filestat_u64(&heap, 0, 0), mount_id.0 as u64, "dev");
    assert_eq!(filestat_u64(&heap, 0, 8), ino, "ino");
    assert_eq!(heap[16], 3, "filetype = directory");
    assert_eq!(filestat_u64(&heap, 0, 32), 0, "size (directory)");
}

#[test]
fn path_filestat_get_on_dev_console_returns_filetype_char_device() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "devpather", 0);
    let (mount_id, _ino) = k.vfs.resolve("/dev/console").expect("resolve");

    let mut heap = vec![0u8; 128];
    let path = b"/dev/console";
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_GET,
        flags: 0,
        request_id: 722,
        args: path_filestat_args(0, 0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.extra_len, 64);
    assert_eq!(filestat_u64(&heap, 0, 0), mount_id.0 as u64, "dev");
    assert_eq!(heap[16], 2, "filetype = character_device");
    assert_eq!(filestat_u64(&heap, 0, 24), 1, "nlink");
    assert_eq!(filestat_u64(&heap, 0, 32), 0, "size (char device)");
}

#[test]
fn path_filestat_get_on_missing_path_returns_enoent() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ghost", 0);

    let mut heap = vec![0u8; 128];
    let path = b"/nonexistent/path";
    heap[..path.len()].copy_from_slice(path);
    // Record the path prefix before dispatch so we can assert the
    // handler did NOT overwrite it with filestat bytes on the error
    // path (error responses must leave the heap-out region untouched
    // so callers that reuse the buffer don't observe stale data).
    let before: Vec<u8> = heap[..path.len()].to_vec();

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_GET,
        flags: 0,
        request_id: 723,
        args: path_filestat_args(0, 0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOENT);
    assert_eq!(resp.extra_len, 0);
    assert_eq!(&heap[..path.len()], &before[..], "heap untouched on error");
}

#[test]
fn path_filestat_get_on_root_returns_filetype_directory_and_root_mount_id() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "rooter", 0);
    let (mount_id, ino) = k.vfs.resolve("/").expect("resolve");

    let mut heap = vec![0u8; 128];
    let path = b"/";
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_GET,
        flags: 0,
        request_id: 724,
        args: path_filestat_args(0, 0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(filestat_u64(&heap, 0, 0), mount_id.0 as u64, "dev = root mount id");
    assert_eq!(filestat_u64(&heap, 0, 8), ino, "ino = root ino");
    assert_eq!(heap[16], 3, "filetype = directory");
}

#[test]
fn path_filestat_get_ignores_dir_fd() {
    // v1 has no preopens, so dir_fd is accepted with any value.
    // Non-symlink targets are unaffected by the lookup_flags
    // SYMLINK_FOLLOW bit (follow-vs-nofollow is a no-op on a
    // regular char device). Pass garbage for dir_fd and a flag
    // set whose bit-0 is 1 (follow); the handler still returns
    // the right stat.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ignorer", 0);

    let mut heap = vec![0u8; 128];
    let path = b"/dev/console";
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_GET,
        flags: 0,
        request_id: 725,
        args: path_filestat_args(0xFFFF_FFFF, 0xDEAD_BEEF),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(heap[16], 2, "filetype = character_device");
}

#[test]
fn path_filestat_get_with_symlink_follow_reaches_target_stat() {
    // Build /target (regular file) + /link (symlink to /target),
    // dispatch PATH_FILESTAT_GET with lookup_flags bit 0 set
    // (LOOKUP_SYMLINK_FOLLOW) on "/link", assert the returned
    // filestat's filetype field is regular-file (4), not symlink
    // (7). Post-slice this is the stat-semantics route into
    // Vfs::resolve.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "symfollow", 0);
    k.vfs.create("/target", 0o644).expect("create target");
    k.vfs.symlink("/target", "/link").expect("symlink");

    let mut heap = vec![0u8; 128];
    let path = b"/link";
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_GET,
        flags: 0,
        request_id: 1130,
        args: path_filestat_args(0, 0x1),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(heap[16], 4, "filetype = regular_file (followed to target)");
}

#[test]
fn path_filestat_get_without_symlink_follow_returns_symlink_filetype() {
    // Same /target + /link setup; lookup_flags = 0 is the
    // lstat route — the final symlink component is NOT
    // dereferenced, so the returned filetype is symbolic_link
    // (7). This pins the pre-slice default behaviour: a caller
    // that doesn't opt in to follow keeps lstat semantics.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "lstatter", 0);
    k.vfs.create("/target", 0o644).expect("create target");
    k.vfs.symlink("/target", "/link").expect("symlink");

    let mut heap = vec![0u8; 128];
    let path = b"/link";
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_GET,
        flags: 0,
        request_id: 1131,
        args: path_filestat_args(0, 0x0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(heap[16], 7, "filetype = symbolic_link (no follow)");
}

#[test]
fn path_filestat_get_with_follow_on_symlink_loop_returns_eloop() {
    // /a -> /a self-loop. With LOOKUP_SYMLINK_FOLLOW set, the
    // resolver walks SYMLOOP_MAX hops and returns ELOOP. The
    // error propagates through fs_err_to_errno's new SymLoop
    // arm to abi::errno::ELOOP (32).
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "looper", 0);
    k.vfs.symlink("/a", "/a").expect("self-loop symlink");

    let mut heap = vec![0u8; 128];
    let path = b"/a";
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_GET,
        flags: 0,
        request_id: 1132,
        args: path_filestat_args(0, 0x1),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ELOOP);
}

// ---- path_filestat_set_times -----------------------------------------
//
// Write-side sibling of `path_filestat_get`: lets userland set a
// vnode's atim / mtim via the `utimensat` family of WASI libc calls.
// Unblocked by the real-vnode-timestamps slice (before which every
// fs reported 0 for all three times, so setting them would have
// just ratcheted a zero-initialised field).
//
// Wire layout:
//   args[0..4]    = dir_fd (u32, ignored — v1 has no preopens)
//   args[4..8]    = lookup_flags (u32, ignored — v1 doesn't follow
//                   symlinks either way)
//   args[8..12]   = fstflags (u32; only the low 4 bits meaningful —
//                   SET_ATIM=0x1, SET_ATIM_NOW=0x2, SET_MTIM=0x4,
//                   SET_MTIM_NOW=0x8; the four-bit combinations that
//                   set both explicit + _NOW for the same field are
//                   EINVAL)
//   args[12..16]  = reserved (0)
//   heap[0..8]    = atim (u64 LE ns-since-epoch; ignored unless
//                   SET_ATIM is set)
//   heap[8..16]   = mtim (u64 LE ns-since-epoch; ignored unless
//                   SET_MTIM is set)
//   heap[16..]    = UTF-8 path bytes
//   heap_len      = 16 + path.len()
// Response:
//   value = 0 on success; status = -errno on error.
//
// Semantics: a successful call also bumps ctime to now() (the
// metadata changed even if the caller didn't ask). Zero fstflags
// is a legal no-op success — WASI permits it so callers can use
// the syscall as a permission probe.

/// Pack the 16-byte inline args window for a set_times dispatch.
fn path_filestat_set_times_args(dir_fd: u32, lookup_flags: u32, fstflags: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&dir_fd.to_le_bytes());
    args[4..8].copy_from_slice(&lookup_flags.to_le_bytes());
    args[8..12].copy_from_slice(&fstflags.to_le_bytes());
    args
}

/// Pack the heap prefix + path into a fresh Vec<u8>. Returns the
/// combined buffer; tests copy it into their heap.
fn path_filestat_set_times_heap(atim: u64, mtim: u64, path: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 + path.len());
    buf.extend_from_slice(&atim.to_le_bytes());
    buf.extend_from_slice(&mtim.to_le_bytes());
    buf.extend_from_slice(path);
    buf
}

#[test]
fn path_filestat_set_times_sets_atim_and_mtim_on_tmpfs() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "timer", 0);
    k.vfs.create("/t.txt", 0o644).expect("create");

    let mut heap = vec![0u8; 128];
    let path = b"/t.txt";
    let buf = path_filestat_set_times_heap(777_000_000, 888_000_000, path);
    heap[..buf.len()].copy_from_slice(&buf);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 800,
        args: path_filestat_set_times_args(
            0,
            0,
            (abi::wasi::fstflags::SET_ATIM | abi::wasi::fstflags::SET_MTIM) as u32,
        ),
        heap_ptr: 0,
        heap_len: buf.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);

    // Verify the values landed by stat-ing the file.
    let st = k.vfs.stat("/t.txt").unwrap();
    assert_eq!(st.atime_ns, 777_000_000);
    assert_eq!(st.mtime_ns, 888_000_000);
    // ctime bumps to now() because the metadata changed; it must
    // be non-zero and distinct from the explicit a/mtim (the wall
    // clock is the 2020s in ns, not a three-digit-million value).
    assert!(st.ctime_ns > 888_000_000);
}

#[test]
fn path_filestat_set_times_set_atim_now_stamps_current_wall_clock() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "nower", 0);
    k.vfs.create("/n.txt", 0o644).expect("create");
    let before = k.vfs.stat("/n.txt").unwrap().atime_ns;

    // Pass 0 for atim — the kernel substitutes now() because of
    // SET_ATIM_NOW, ignoring the explicit value.
    let mut heap = vec![0u8; 128];
    let path = b"/n.txt";
    let buf = path_filestat_set_times_heap(0, 0, path);
    heap[..buf.len()].copy_from_slice(&buf);
    std::thread::sleep(std::time::Duration::from_millis(1));

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 801,
        args: path_filestat_set_times_args(
            0,
            0,
            abi::wasi::fstflags::SET_ATIM_NOW as u32,
        ),
        heap_ptr: 0,
        heap_len: buf.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);

    let after = k.vfs.stat("/n.txt").unwrap();
    assert!(after.atime_ns > before, "atim advanced past create-time");
    // mtim must stay at create-time since SET_MTIM wasn't requested.
    // (Create stamped all three to the same now().)
    // Can't assert equality rigidly because SET_ATIM_NOW's "now"
    // and create's "now" read the same clock a millisecond apart;
    // the invariant is mtim did NOT jump with atim.
    assert!(after.mtime_ns < after.atime_ns);
}

#[test]
fn path_filestat_set_times_with_zero_flags_is_noop_success() {
    // WASI permits an empty fstflags — the call validates the path
    // (+ the caller's rights) but doesn't touch the timestamps.
    // Useful as a permission probe from userland.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "prober", 0);
    k.vfs.create("/p.txt", 0o644).expect("create");
    let before = k.vfs.stat("/p.txt").unwrap();

    let mut heap = vec![0u8; 128];
    let path = b"/p.txt";
    let buf = path_filestat_set_times_heap(111, 222, path);
    heap[..buf.len()].copy_from_slice(&buf);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 802,
        args: path_filestat_set_times_args(0, 0, 0),
        heap_ptr: 0,
        heap_len: buf.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);

    let after = k.vfs.stat("/p.txt").unwrap();
    assert_eq!(after.atime_ns, before.atime_ns, "atim untouched");
    assert_eq!(after.mtime_ns, before.mtime_ns, "mtim untouched");
    assert_eq!(after.ctime_ns, before.ctime_ns, "ctime untouched (zero-flags is not a metadata change)");
}

#[test]
fn path_filestat_set_times_with_both_atim_explicit_and_now_returns_einval() {
    // Per WASI: SET_ATIM and SET_ATIM_NOW are mutually exclusive.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "conflicter", 0);
    k.vfs.create("/c.txt", 0o644).expect("create");

    let mut heap = vec![0u8; 128];
    let path = b"/c.txt";
    let buf = path_filestat_set_times_heap(0, 0, path);
    heap[..buf.len()].copy_from_slice(&buf);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 803,
        args: path_filestat_set_times_args(
            0,
            0,
            (abi::wasi::fstflags::SET_ATIM | abi::wasi::fstflags::SET_ATIM_NOW) as u32,
        ),
        heap_ptr: 0,
        heap_len: buf.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_filestat_set_times_with_both_mtim_explicit_and_now_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "conflicter", 0);
    k.vfs.create("/c.txt", 0o644).expect("create");

    let mut heap = vec![0u8; 128];
    let path = b"/c.txt";
    let buf = path_filestat_set_times_heap(0, 0, path);
    heap[..buf.len()].copy_from_slice(&buf);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 804,
        args: path_filestat_set_times_args(
            0,
            0,
            (abi::wasi::fstflags::SET_MTIM | abi::wasi::fstflags::SET_MTIM_NOW) as u32,
        ),
        heap_ptr: 0,
        heap_len: buf.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_filestat_set_times_on_missing_path_returns_enoent() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ghost", 0);

    let mut heap = vec![0u8; 128];
    let path = b"/nope/nope";
    let buf = path_filestat_set_times_heap(1, 2, path);
    heap[..buf.len()].copy_from_slice(&buf);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 805,
        args: path_filestat_set_times_args(
            0,
            0,
            (abi::wasi::fstflags::SET_ATIM | abi::wasi::fstflags::SET_MTIM) as u32,
        ),
        heap_ptr: 0,
        heap_len: buf.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOENT);
}

#[test]
fn path_filestat_set_times_on_dev_console_returns_erofs() {
    // devfs is read-only — set_times on a device node rejects with
    // EROFS just like create/unlink/mkdir do.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "devsetter", 0);

    let mut heap = vec![0u8; 128];
    let path = b"/dev/console";
    let buf = path_filestat_set_times_heap(111, 222, path);
    heap[..buf.len()].copy_from_slice(&buf);

    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 806,
        args: path_filestat_set_times_args(
            0,
            0,
            (abi::wasi::fstflags::SET_ATIM | abi::wasi::fstflags::SET_MTIM) as u32,
        ),
        heap_ptr: 0,
        heap_len: buf.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EROFS);
}

#[test]
fn path_filestat_set_times_with_short_heap_returns_einval() {
    // The heap must carry at least 16 bytes (the atim + mtim prefix)
    // plus the path. A shorter heap means the shim produced a
    // malformed buffer.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "truncated", 0);

    let mut heap = vec![0u8; 128];
    let req = Request {
        opcode: op_wasi::PATH_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 807,
        args: path_filestat_set_times_args(
            0,
            0,
            abi::wasi::fstflags::SET_ATIM as u32,
        ),
        heap_ptr: 0,
        heap_len: 8, // only one u64 fits, no room for both times + path
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

// ---- poll_oneoff ------------------------------------------------------
//
// WASI's multi-subscription poll: the caller hands the kernel N
// `subscription_t` records (48 bytes each) and receives back up to M
// `event_t` records (32 bytes each) describing which subscriptions
// are ready right now. Wire layout used below:
//
//   args[0..4]  = n_subs (u32)
//   args[4..8]  = n_events_cap (u32 — caller-provided max)
//   heap[0..n_subs*48]                        = input subscriptions
//   heap[n_subs*48..n_subs*48 + n_events*32]  = output events
//   heap_len    = n_subs*48 + n_events_cap*32 (caller sizes this)
// Response:
//   value     = n_events actually emitted (u32 widened to i64)
//   extra_len = same, echoed so the shim can read it without
//               re-decoding `value` as BigInt
//
// v1 kernel is single-threaded; the semantic is "non-blocking check".
// A CLOCK subscription is ready iff its target time has been reached
// (absolute) or its relative timeout is zero. An FD_READ subscription
// is ready iff a read() on the fd would not block — for a Vnode that
// is always true (offset < size means data, offset >= size means EOF
// which is also "readable"); for a CharDevice it depends on the
// input ring; for a Socket it means rx_buf non-empty or peer closed.
// An FD_WRITE subscription is ready iff a write() would make progress
// — Vnode + /dev/console always, Socket iff peer has rx capacity.
// Invalid fd / unsupported clock id / bogus tag emit one event per
// bad subscription with `event.error` set (EBADF / EINVAL / ENOTSUP);
// the whole syscall still returns success. Syscall-level EINVAL fires
// only for shape errors: n_subs == 0, heap too short to hold the
// declared subs, heap too short to also hold n_events_cap events.

use abi::wasi::poll as pl;

/// Pack `(n_subs, n_events_cap)` into the 16-byte inline args window.
fn poll_oneoff_args(n_subs: u32, n_events_cap: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&n_subs.to_le_bytes());
    args[4..8].copy_from_slice(&n_events_cap.to_le_bytes());
    args
}

/// Pack a CLOCK subscription into a fresh 48-byte buffer.
fn sub_clock(userdata: u64, clock_id: u32, timeout_ns: u64, flags: u16) -> [u8; 48] {
    let mut s = [0u8; 48];
    s[pl::SUB_OFF_USERDATA..pl::SUB_OFF_USERDATA + 8].copy_from_slice(&userdata.to_le_bytes());
    s[pl::SUB_OFF_TAG] = abi::wasi::eventtype::CLOCK;
    s[pl::SUB_CLOCK_OFF_ID..pl::SUB_CLOCK_OFF_ID + 4].copy_from_slice(&clock_id.to_le_bytes());
    s[pl::SUB_CLOCK_OFF_TIMEOUT..pl::SUB_CLOCK_OFF_TIMEOUT + 8]
        .copy_from_slice(&timeout_ns.to_le_bytes());
    s[pl::SUB_CLOCK_OFF_FLAGS..pl::SUB_CLOCK_OFF_FLAGS + 2].copy_from_slice(&flags.to_le_bytes());
    s
}

/// Pack an FD_READ or FD_WRITE subscription into a fresh 48-byte buffer.
fn sub_fd_rw(userdata: u64, tag: u8, fd: u32) -> [u8; 48] {
    let mut s = [0u8; 48];
    s[pl::SUB_OFF_USERDATA..pl::SUB_OFF_USERDATA + 8].copy_from_slice(&userdata.to_le_bytes());
    s[pl::SUB_OFF_TAG] = tag;
    s[pl::SUB_FDRW_OFF_FD..pl::SUB_FDRW_OFF_FD + 4].copy_from_slice(&fd.to_le_bytes());
    s
}

/// Decode a single 32-byte event from `heap` at `offset`.
fn decode_event(heap: &[u8], offset: usize) -> (u64, u16, u8, u64, u16) {
    let mut u = [0u8; 8];
    u.copy_from_slice(&heap[offset..offset + 8]);
    let userdata = u64::from_le_bytes(u);
    let mut e = [0u8; 2];
    e.copy_from_slice(&heap[offset + pl::EVENT_OFF_ERROR..offset + pl::EVENT_OFF_ERROR + 2]);
    let error = u16::from_le_bytes(e);
    let ty = heap[offset + pl::EVENT_OFF_TYPE];
    let mut n = [0u8; 8];
    n.copy_from_slice(&heap[offset + pl::EVENT_OFF_RW_NBYTES..offset + pl::EVENT_OFF_RW_NBYTES + 8]);
    let nbytes = u64::from_le_bytes(n);
    let mut f = [0u8; 2];
    f.copy_from_slice(&heap[offset + pl::EVENT_OFF_RW_FLAGS..offset + pl::EVENT_OFF_RW_FLAGS + 2]);
    let flags = u16::from_le_bytes(f);
    (userdata, error, ty, nbytes, flags)
}

#[test]
fn poll_oneoff_zero_subscriptions_returns_einval() {
    // WASI requires at least one subscription — a zero-sub call is
    // nonsensical. Reject at the dispatcher layer before allocating
    // anything on the heap.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "empty", 0);
    let mut heap = vec![0u8; 256];

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 820,
        args: poll_oneoff_args(0, 4),
        heap_ptr: 0,
        heap_len: 256,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn poll_oneoff_heap_too_short_for_subs_returns_einval() {
    // Declared 2 subs requires 96 bytes at minimum; a 48-byte heap
    // can't hold them.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "truncsubs", 0);
    let mut heap = vec![0u8; 48];

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 821,
        args: poll_oneoff_args(2, 0),
        heap_ptr: 0,
        heap_len: 48,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn poll_oneoff_heap_too_short_for_events_returns_einval() {
    // Exercise the max(subs_bytes, events_bytes) check: 1 sub at 48
    // bytes is covered by the 80-byte heap, but 3 events cap needs
    // 96 bytes — the caller has to size the heap for the bigger of
    // the two windows. (The usual WASI shim passes n_events_cap ==
    // n_subs, so subs is always the bigger half; this case exists
    // to guard the `max` branch specifically.)
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "truncevs", 0);
    let mut heap = vec![0u8; 100];
    let s = sub_clock(1, abi::wasi::CLOCKID_MONOTONIC, 0, 0);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 822,
        args: poll_oneoff_args(1, 3),
        heap_ptr: 0,
        heap_len: 80, // < 3*32 = 96 events_bytes; kernel rejects
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn poll_oneoff_unknown_subscription_type_emits_einval_event() {
    // Tag 99 is not a valid eventtype. The handler emits one event
    // with error=EINVAL rather than aborting the syscall, because
    // bad-tag is a per-subscription problem (a caller with a mix of
    // good + bad subs still wants their good events).
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "badtag", 0);
    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(0xDEAD_BEEFu64, 99, 0); // tag 99 → unknown
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 823,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    assert_eq!(resp.extra_len, pl::EVENT_SIZE as u32);
    let (ud, err, ty, nb, fl) = decode_event(&heap, 0);
    assert_eq!(ud, 0xDEAD_BEEFu64);
    assert_eq!(err, errno::EINVAL as u16);
    assert_eq!(ty, 99);
    assert_eq!(nb, 0);
    assert_eq!(fl, 0);
}

#[test]
fn poll_oneoff_clock_monotonic_abstime_past_is_ready() {
    // ABSTIME with a timeout of 1 ns is trivially in the past of the
    // Platform monotonic clock (which is far beyond nanoseconds into
    // the test run). Ready → one event, no error.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "pastclk", 0);
    let mut heap = vec![0u8; 128];
    let s = sub_clock(
        42,
        abi::wasi::CLOCKID_MONOTONIC,
        1,
        abi::wasi::subclockflags::ABSTIME,
    );
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 824,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (ud, err, ty, _nb, _fl) = decode_event(&heap, 0);
    assert_eq!(ud, 42);
    assert_eq!(err, 0);
    assert_eq!(ty, abi::wasi::eventtype::CLOCK);
}

#[test]
fn poll_oneoff_clock_monotonic_abstime_future_is_not_ready() {
    // ABSTIME with a timeout in the far future (u64::MAX) never
    // fires in v1's non-blocking model. Zero events.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "futclk", 0);
    let mut heap = vec![0u8; 128];
    let s = sub_clock(
        7,
        abi::wasi::CLOCKID_MONOTONIC,
        u64::MAX,
        abi::wasi::subclockflags::ABSTIME,
    );
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 825,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert_eq!(resp.extra_len, 0);
}

#[test]
fn poll_oneoff_clock_realtime_abstime_past_is_ready() {
    // Same shape as the monotonic case but hitting the realtime
    // clock — 1 ns since the Unix epoch is also firmly in the past.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "realclk", 0);
    let mut heap = vec![0u8; 128];
    let s = sub_clock(
        99,
        abi::wasi::CLOCKID_REALTIME,
        1,
        abi::wasi::subclockflags::ABSTIME,
    );
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 826,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (_ud, err, ty, _nb, _fl) = decode_event(&heap, 0);
    assert_eq!(err, 0);
    assert_eq!(ty, abi::wasi::eventtype::CLOCK);
}

#[test]
fn poll_oneoff_clock_relative_zero_timeout_is_ready() {
    // Relative (no ABSTIME flag) with timeout=0 means "fire
    // immediately". Non-blocking kernel honours that as "ready now".
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "relzero", 0);
    let mut heap = vec![0u8; 128];
    let s = sub_clock(1, abi::wasi::CLOCKID_MONOTONIC, 0, 0);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 827,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
}

#[test]
fn poll_oneoff_clock_relative_nonzero_is_not_ready() {
    // Relative non-zero means "wait N ns from now". v1 is non-
    // blocking — userland spins instead of us blocking — so any
    // non-zero relative timeout is reported as "not ready yet".
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "relnonzero", 0);
    let mut heap = vec![0u8; 128];
    let s = sub_clock(1, abi::wasi::CLOCKID_MONOTONIC, 10_000_000_000, 0);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 828,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
}

#[test]
fn poll_oneoff_clock_invalid_id_emits_einval_event() {
    // Unknown clock id (42): emit a per-subscription event with
    // errno=EINVAL instead of aborting the whole syscall.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "badclkid", 0);
    let mut heap = vec![0u8; 128];
    let s = sub_clock(5, 42, 0, 0);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 829,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (ud, err, ty, _nb, _fl) = decode_event(&heap, 0);
    assert_eq!(ud, 5);
    assert_eq!(err, errno::EINVAL as u16);
    assert_eq!(ty, abi::wasi::eventtype::CLOCK);
}

#[test]
fn poll_oneoff_clock_cputime_id_emits_enotsup_event() {
    // Cputime clock ids are recognised-but-unsupported — the
    // `clock_time_get` handler returns ENOTSUP for them, and
    // poll_oneoff mirrors that by emitting a per-subscription event
    // with errno=ENOTSUP.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "cpuclk", 0);
    let mut heap = vec![0u8; 128];
    let s = sub_clock(9, abi::wasi::CLOCKID_PROCESS_CPUTIME_ID, 0, 0);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 830,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (_ud, err, ty, _nb, _fl) = decode_event(&heap, 0);
    assert_eq!(err, errno::ENOTSUP as u16);
    assert_eq!(ty, abi::wasi::eventtype::CLOCK);
}

#[test]
fn poll_oneoff_fd_read_bad_fd_emits_ebadf_event() {
    // Unopened fd → per-subscription EBADF. Syscall-level success.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "badfd", 0);
    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(0xABCD, abi::wasi::eventtype::FD_READ, 99);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 831,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (ud, err, ty, _nb, _fl) = decode_event(&heap, 0);
    assert_eq!(ud, 0xABCD);
    assert_eq!(err, errno::EBADF as u16);
    assert_eq!(ty, abi::wasi::eventtype::FD_READ);
}

#[test]
fn poll_oneoff_fd_read_vnode_reports_ready_with_bytes_available() {
    // A Vnode fd against a non-empty tmpfs file with offset 0 is
    // always "ready to read" and the handler reports nbytes =
    // remaining-bytes-in-file.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "vnoderd", "/v.txt", b"hello, poll");
    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(1, abi::wasi::eventtype::FD_READ, fd);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 832,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (ud, err, ty, nb, fl) = decode_event(&heap, 0);
    assert_eq!(ud, 1);
    assert_eq!(err, 0);
    assert_eq!(ty, abi::wasi::eventtype::FD_READ);
    assert_eq!(nb, "hello, poll".len() as u64);
    assert_eq!(fl, 0);
}

#[test]
fn poll_oneoff_fd_write_vnode_always_ready() {
    // Vnode FD_WRITE: always ready in v1 (tmpfs + opfs + procfs
    // accept writes without blocking; devfs + procfs reject at the
    // write layer, not at the pollable layer — poll_oneoff's
    // "is it writable" answer for a Vnode is unconditionally yes).
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "vnwr", "/vw.txt", b"");
    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(2, abi::wasi::eventtype::FD_WRITE, fd);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 834,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (_ud, err, ty, _nb, fl) = decode_event(&heap, 0);
    assert_eq!(err, 0);
    assert_eq!(ty, abi::wasi::eventtype::FD_WRITE);
    assert_eq!(fl, 0);
}

#[test]
fn poll_oneoff_fd_read_console_empty_is_not_ready() {
    // /dev/console with an empty input ring is not yet readable.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "con0", 0);
    k.install_fd(pid, 0, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(3, abi::wasi::eventtype::FD_READ, 0);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 835,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
}

#[test]
fn poll_oneoff_fd_read_console_with_input_is_ready() {
    // Inject input bytes into the console ring, then poll FD_READ
    // on the console fd — ready, nbytes = ring length.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "con1", 0);
    k.install_fd(pid, 0, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    k.devs.inject_console_input(b"hi");
    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(4, abi::wasi::eventtype::FD_READ, 0);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 836,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (_ud, err, _ty, nb, _fl) = decode_event(&heap, 0);
    assert_eq!(err, 0);
    assert_eq!(nb, 2);
}

#[test]
fn poll_oneoff_fd_write_console_always_ready() {
    // /dev/console's write sink is line-buffered and never blocks.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "conwr", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(5, abi::wasi::eventtype::FD_WRITE, 1);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 837,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (_ud, err, ty, _nb, _fl) = decode_event(&heap, 0);
    assert_eq!(err, 0);
    assert_eq!(ty, abi::wasi::eventtype::FD_WRITE);
}

#[test]
fn poll_oneoff_fd_read_socket_empty_connected_not_ready() {
    // A connected socket with an empty rx buffer and a still-open
    // peer is not yet readable. Build the pair directly via the
    // IpcTable so the test doesn't depend on the full bind/listen/
    // connect/accept handshake.
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sockrd", 0);

    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 10, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();

    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(6, abi::wasi::eventtype::FD_READ, 10);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 839,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
}

#[test]
fn poll_oneoff_fd_read_socket_with_data_ready() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sockrd2", 0);

    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
        sa.rx_buf.extend(b"data");
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 10, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();

    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(7, abi::wasi::eventtype::FD_READ, 10);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 840,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (_ud, err, _ty, nb, fl) = decode_event(&heap, 0);
    assert_eq!(err, 0);
    assert_eq!(nb, 4);
    assert_eq!(fl, 0);
}

#[test]
fn poll_oneoff_fd_read_socket_peer_closed_hangup_ready() {
    // Peer closed + empty rx_buf is ready-with-hangup, signalling
    // EOF to the caller. nbytes = 0.
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sockrdclosed", 0);

    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
        sb.closed = true;
    }
    k.install_fd(pid, 10, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();

    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(8, abi::wasi::eventtype::FD_READ, 10);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 841,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (_ud, err, _ty, nb, fl) = decode_event(&heap, 0);
    assert_eq!(err, 0);
    assert_eq!(nb, 0);
    assert_eq!(fl, abi::wasi::eventrwflags::FD_READWRITE_HANGUP);
}

#[test]
fn poll_oneoff_fd_write_socket_with_peer_capacity_ready() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sockwr", 0);

    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 10, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();

    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(11, abi::wasi::eventtype::FD_WRITE, 10);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 842,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (_ud, err, ty, _nb, _fl) = decode_event(&heap, 0);
    assert_eq!(err, 0);
    assert_eq!(ty, abi::wasi::eventtype::FD_WRITE);
}

#[test]
fn poll_oneoff_fd_read_on_empty_signal_channel_not_ready() {
    // FD_READ on a SignalChannel with no pending signals is
    // simply not-ready — no event emitted, not a per-sub EINVAL.
    // This lets a caller poll fd 3 alongside other fds without
    // burning CPU on a meaningless error.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sigrd", 0);
    k.install_fd(pid, 5, FdObject::SignalChannel, FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(13, abi::wasi::eventtype::FD_READ, 5);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 843,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
}

#[test]
fn poll_oneoff_fd_read_on_signal_channel_with_pending_signals_ready() {
    // With signals pending in the inbox, FD_READ on a
    // SignalChannel fd reports ready with nbytes = 2 * pending
    // (each signal serialises to 2 bytes).
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    // Self-signal SIGTERM + SIGPIPE to fill two inbox slots.
    k.proc_kill(init, init, Signal::Term).unwrap();
    k.proc_kill(init, init, Signal::Pipe).unwrap();
    k.install_fd(init, 5, FdObject::SignalChannel, FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(17, abi::wasi::eventtype::FD_READ, 5);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 844,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (_ud, err, ty, nb, _fl) = decode_event(&heap, 0);
    assert_eq!(err, 0);
    assert_eq!(ty, abi::wasi::eventtype::FD_READ);
    assert_eq!(nb, 4);
    // Poll does not drain — the signals stay queued for the
    // actual fd_read that follows.
    assert_eq!(k.pending_signals(init).unwrap(), 2);
}

#[test]
fn poll_oneoff_fd_write_on_signal_channel_emits_einval() {
    // SignalChannel is read-only. FD_WRITE on it is ill-posed.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sigwr", 0);
    k.install_fd(pid, 5, FdObject::SignalChannel, FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 128];
    let s = sub_fd_rw(19, abi::wasi::eventtype::FD_WRITE, 5);
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 845,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (_ud, err, _ty, _nb, _fl) = decode_event(&heap, 0);
    assert_eq!(err, errno::EINVAL as u16);
}

#[test]
fn poll_oneoff_userdata_is_echoed_verbatim() {
    // Userdata is 64 bits of opaque caller state; kernel must echo
    // it without modification so the caller can correlate events
    // back to subscriptions without tracking index positions.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "udatareal", 0);
    let mut heap = vec![0u8; 128];
    let s = sub_clock(
        0xFEDC_BA98_7654_3210,
        abi::wasi::CLOCKID_MONOTONIC,
        0,
        0,
    );
    heap[..48].copy_from_slice(&s);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 844,
        args: poll_oneoff_args(1, 1),
        heap_ptr: 0,
        heap_len: 48 + 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 1);
    let (ud, _err, _ty, _nb, _fl) = decode_event(&heap, 0);
    assert_eq!(ud, 0xFEDC_BA98_7654_3210);
}

#[test]
fn poll_oneoff_mixed_subscriptions_emit_only_ready_ones() {
    // Three subscriptions: ready-clock, not-ready-future-clock,
    // ready-clock. Expect 2 events with userdatas 1 and 3 (2 is the
    // never-firing one).
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "mixed", 0);
    let mut heap = vec![0u8; 3 * 48 + 3 * 32 + 32];
    let s1 = sub_clock(1, abi::wasi::CLOCKID_MONOTONIC, 0, 0);
    let s2 = sub_clock(
        2,
        abi::wasi::CLOCKID_MONOTONIC,
        u64::MAX,
        abi::wasi::subclockflags::ABSTIME,
    );
    let s3 = sub_clock(3, abi::wasi::CLOCKID_REALTIME, 0, 0);
    heap[0..48].copy_from_slice(&s1);
    heap[48..96].copy_from_slice(&s2);
    heap[96..144].copy_from_slice(&s3);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 845,
        args: poll_oneoff_args(3, 3),
        heap_ptr: 0,
        heap_len: 3 * 48 + 3 * 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 2);
    // Events are written sequentially starting at heap_ptr + 0,
    // overwriting the subscription window.
    let (ud1, _, _, _, _) = decode_event(&heap, 0);
    let (ud2, _, _, _, _) = decode_event(&heap, 32);
    assert_eq!(ud1, 1);
    assert_eq!(ud2, 3);
}

#[test]
fn poll_oneoff_events_cap_caps_output_count() {
    // Three ready subs but the caller's event cap is 2 — kernel
    // emits 2 events and silently drops the third. No error.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "cap2", 0);
    let mut heap = vec![0u8; 3 * 48 + 2 * 32 + 32];
    let s1 = sub_clock(1, abi::wasi::CLOCKID_MONOTONIC, 0, 0);
    let s2 = sub_clock(2, abi::wasi::CLOCKID_MONOTONIC, 0, 0);
    let s3 = sub_clock(3, abi::wasi::CLOCKID_REALTIME, 0, 0);
    heap[0..48].copy_from_slice(&s1);
    heap[48..96].copy_from_slice(&s2);
    heap[96..144].copy_from_slice(&s3);

    let req = Request {
        opcode: op_wasi::POLL_ONEOFF,
        flags: 0,
        request_id: 846,
        args: poll_oneoff_args(3, 2),
        heap_ptr: 0,
        heap_len: 3 * 48 + 2 * 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 2);
    // extra_len is in bytes (n_events * event_size), mirroring the
    // random_get / fd_read convention: "bytes written to the heap
    // scratch region".
    assert_eq!(resp.extra_len, 2 * pl::EVENT_SIZE as u32);
}

// ---- fd_filestat_set_times -------------------------------------------
//
// Fd-based sibling of `path_filestat_set_times`. Same fstflags +
// Option-value semantics as the path variant; difference is purely in
// how the (mount_id, ino) pair is resolved — from the fd instead of a
// VFS path. Reuses `Vfs::set_times_ino` (the direct-ino mirror of
// `Vfs::set_times`) and the same fstflags decode as
// `handle_path_filestat_set_times`.
//
// Wire layout:
//   args[0..4]    = fd (u32)
//   args[4..8]    = fstflags (u32; only the low 4 bits meaningful —
//                   SET_ATIM / SET_ATIM_NOW / SET_MTIM / SET_MTIM_NOW;
//                   invalid pairs → EINVAL; zero fstflags → no-op
//                   success without touching the fs)
//   heap[0..8]    = atim (u64 LE ns-since-epoch; ignored unless
//                   SET_ATIM is set)
//   heap[8..16]   = mtim (u64 LE ns-since-epoch; ignored unless
//                   SET_MTIM is set)
//   heap_len      = 16
// Response:
//   value = 0 on success; status = -errno on error.
//
// Guards in order:
//   1. Exclusive flag pairs → EINVAL (fires before any fd lookup or
//      heap read so an invalid-flags probe gets a stable errno
//      regardless of whether the fd is open).
//   2. Heap < 16 → EINVAL (malformed shim).
//   3. Unopened fd → EBADF.
//   4. Non-Vnode FdObject → EINVAL (fd_filestat_set_times is only
//      meaningful for regular files + directories; char devices,
//      sockets, pipes, and signal channels carry no time metadata to
//      mutate).
//   5. Filesystem rejection → passed through unchanged (EROFS for
//      devfs/procfs, etc.).

fn fd_filestat_set_times_args(fd: u32, fstflags: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&fd.to_le_bytes());
    args[4..8].copy_from_slice(&fstflags.to_le_bytes());
    args
}

fn fd_filestat_set_times_heap(atim: u64, mtim: u64) -> [u8; 16] {
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&atim.to_le_bytes());
    buf[8..16].copy_from_slice(&mtim.to_le_bytes());
    buf
}

#[test]
fn fd_filestat_set_times_sets_atim_and_mtim_on_tmpfs_fd() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "fd_timer", "/f.txt", b"");
    let mut heap = vec![0u8; 64];
    heap[..16].copy_from_slice(&fd_filestat_set_times_heap(777_000_000, 888_000_000));

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 850,
        args: fd_filestat_set_times_args(
            fd,
            (abi::wasi::fstflags::SET_ATIM | abi::wasi::fstflags::SET_MTIM) as u32,
        ),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let st = k.vfs.stat("/f.txt").unwrap();
    assert_eq!(st.atime_ns, 777_000_000);
    assert_eq!(st.mtime_ns, 888_000_000);
    assert!(st.ctime_ns > 888_000_000);
}

#[test]
fn fd_filestat_set_times_set_atim_now_stamps_wall_clock_via_platform() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "fd_nower", "/fn.txt", b"");
    let before = k.vfs.stat("/fn.txt").unwrap().atime_ns;
    let mut heap = vec![0u8; 64];
    heap[..16].copy_from_slice(&fd_filestat_set_times_heap(0, 0));
    std::thread::sleep(std::time::Duration::from_millis(1));

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 851,
        args: fd_filestat_set_times_args(
            fd,
            abi::wasi::fstflags::SET_ATIM_NOW as u32,
        ),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let after = k.vfs.stat("/fn.txt").unwrap();
    assert!(after.atime_ns > before);
    // mtim unchanged — SET_MTIM not set.
    assert!(after.mtime_ns < after.atime_ns);
}

#[test]
fn fd_filestat_set_times_with_zero_flags_is_noop_success() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "fd_prober", "/fp.txt", b"");
    let before = k.vfs.stat("/fp.txt").unwrap();
    let mut heap = vec![0u8; 64];
    heap[..16].copy_from_slice(&fd_filestat_set_times_heap(111, 222));

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 852,
        args: fd_filestat_set_times_args(fd, 0),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let after = k.vfs.stat("/fp.txt").unwrap();
    assert_eq!(after.atime_ns, before.atime_ns, "atim untouched");
    assert_eq!(after.mtime_ns, before.mtime_ns, "mtim untouched");
    assert_eq!(after.ctime_ns, before.ctime_ns, "ctime untouched");
}

#[test]
fn fd_filestat_set_times_with_both_atim_explicit_and_now_returns_einval() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "fd_confl", "/fc.txt", b"");
    let mut heap = vec![0u8; 64];
    heap[..16].copy_from_slice(&fd_filestat_set_times_heap(0, 0));

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 853,
        args: fd_filestat_set_times_args(
            fd,
            (abi::wasi::fstflags::SET_ATIM | abi::wasi::fstflags::SET_ATIM_NOW) as u32,
        ),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_filestat_set_times_with_both_mtim_explicit_and_now_returns_einval() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "fd_confl", "/fm.txt", b"");
    let mut heap = vec![0u8; 64];
    heap[..16].copy_from_slice(&fd_filestat_set_times_heap(0, 0));

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 854,
        args: fd_filestat_set_times_args(
            fd,
            (abi::wasi::fstflags::SET_MTIM | abi::wasi::fstflags::SET_MTIM_NOW) as u32,
        ),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_filestat_set_times_on_unopened_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "fd_ghost", 0);
    let mut heap = vec![0u8; 64];
    heap[..16].copy_from_slice(&fd_filestat_set_times_heap(1, 2));

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 855,
        args: fd_filestat_set_times_args(
            99,
            (abi::wasi::fstflags::SET_ATIM | abi::wasi::fstflags::SET_MTIM) as u32,
        ),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn fd_filestat_set_times_on_char_device_fd_returns_einval() {
    // Non-Vnode FdObject: char device, socket, pipe, signal channel.
    // Guard rejects before any fs call — EINVAL, not EROFS.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "fd_cdev", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];
    heap[..16].copy_from_slice(&fd_filestat_set_times_heap(111, 222));

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 856,
        args: fd_filestat_set_times_args(
            1,
            (abi::wasi::fstflags::SET_ATIM | abi::wasi::fstflags::SET_MTIM) as u32,
        ),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_filestat_set_times_on_procfs_fd_returns_erofs() {
    // procfs is read-only; its set_times returns ReadOnly which maps
    // to EROFS. This exercises the filesystem-rejection passthrough
    // path — guard 5 in the list above.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "fd_proc", 0);
    let (mount_id, ino) = k.vfs.resolve("/proc/version").expect("resolve");
    k.install_fd(
        pid,
        4,
        FdObject::Vnode { mount_id, ino },
        FdFlags::EMPTY,
    )
    .unwrap();
    let mut heap = vec![0u8; 64];
    heap[..16].copy_from_slice(&fd_filestat_set_times_heap(111, 222));

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 857,
        args: fd_filestat_set_times_args(
            4,
            (abi::wasi::fstflags::SET_ATIM | abi::wasi::fstflags::SET_MTIM) as u32,
        ),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EROFS);
}

#[test]
fn fd_filestat_set_times_with_short_heap_returns_einval() {
    // Heap must carry both u64 LE timestamps (16 bytes); a shorter
    // heap means the shim produced a malformed buffer.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "fd_trunc", "/ft.txt", b"");
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_TIMES,
        flags: 0,
        request_id: 858,
        args: fd_filestat_set_times_args(
            fd,
            abi::wasi::fstflags::SET_ATIM as u32,
        ),
        heap_ptr: 0,
        heap_len: 8, // only room for atim, no mtim
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

// ---- fd_renumber -----------------------------------------------------
//
// WASI's dup2-spelling: atomically move an fd from `from` to `to`,
// closing whatever was at `to` if it was already open. Wire layout:
//
//   args[0..4] = from (u32)
//   args[4..8] = to   (u32)
//   heap       = unused
// Response:
//   value = 0 on success; status = -errno on error.
//
// Semantics (follows wasmtime's reading of WASI preview 1 fd_renumber):
//
//   * from == to: no-op success, but still validates that `from`
//     refers to an open fd — calling fd_renumber(99, 99) when 99 is
//     not open returns EBADF, not success. This mirrors POSIX's
//     dup2(invalid, invalid) = EBADF.
//   * from is not open: EBADF; `to` is left unchanged.
//   * from is open, to is closed: entry is moved — `from` becomes
//     closed, `to` holds what `from` held.
//   * from is open, to is also open: `to`'s entry is closed first
//     (releasing any pipe/socket refs via the kernel's
//     release_object path), then `from` is moved into `to`.
//
// The FdEntry is moved verbatim, preserving offset + flags. No
// object-side resources are duplicated (the move doesn't dup a
// pipe/socket reference — the underlying handle is the same).

fn fd_renumber_args(from: u32, to: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&from.to_le_bytes());
    args[4..8].copy_from_slice(&to.to_le_bytes());
    args
}

#[test]
fn fd_renumber_moves_entry_and_closes_source() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renumberer", 0);
    k.install_fd(pid, 3, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_RENUMBER,
        flags: 0,
        request_id: 870,
        args: fd_renumber_args(3, 7),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    // fd 3 is now closed; fd 7 holds the CharDevice entry.
    assert!(k.fds(pid).unwrap().get(3).is_none());
    assert!(matches!(
        k.fds(pid).unwrap().get(7).map(|e| e.object),
        Some(FdObject::CharDevice(n)) if n == DEV_CONSOLE
    ));
}

#[test]
fn fd_renumber_when_from_equals_to_is_noop_success_on_open_fd() {
    // POSIX dup2(fd, fd) = fd; WASI fd_renumber honours the same
    // contract. The fd stays open and the entry is unchanged.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renumself", 0);
    k.install_fd(pid, 5, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let before = *k.fds(pid).unwrap().get(5).unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_RENUMBER,
        flags: 0,
        request_id: 871,
        args: fd_renumber_args(5, 5),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let after = *k.fds(pid).unwrap().get(5).unwrap();
    assert_eq!(before, after);
}

#[test]
fn fd_renumber_closes_prior_to_then_installs() {
    // Both `from` and `to` are open pre-call. Expect `to`'s prior
    // entry to be released (via release_object — matters for pipe
    // / socket refs; here the prior is a CharDevice which is a
    // no-op release) and `from`'s entry to land at `to`.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renumover", 0);
    k.install_fd(pid, 3, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    k.install_fd(pid, 7, FdObject::SignalChannel, FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_RENUMBER,
        flags: 0,
        request_id: 872,
        args: fd_renumber_args(3, 7),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert!(k.fds(pid).unwrap().get(3).is_none(), "from closed");
    assert!(matches!(
        k.fds(pid).unwrap().get(7).map(|e| e.object),
        Some(FdObject::CharDevice(n)) if n == DEV_CONSOLE,
    ));
}

#[test]
fn fd_renumber_from_unopened_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renumghost", 0);
    k.install_fd(pid, 7, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_RENUMBER,
        flags: 0,
        request_id: 873,
        args: fd_renumber_args(99, 7),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
    // `to` is untouched.
    assert!(k.fds(pid).unwrap().get(7).is_some());
}

#[test]
fn fd_renumber_from_equals_to_on_unopened_fd_returns_ebadf() {
    // POSIX dup2(bad, bad) = EBADF. WASI's noop-success-on-equal
    // contract only applies when the fd is actually open.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renumghost2", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_RENUMBER,
        flags: 0,
        request_id: 874,
        args: fd_renumber_args(42, 42),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn fd_renumber_preserves_offset_and_flags() {
    // A Vnode fd with a non-zero offset + NONBLOCK flag round-trips
    // through renumber without losing either.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "renumofs", "/o.txt", b"0123456789");
    {
        let table = k.fds_mut(pid).unwrap();
        let entry = table.get_mut(fd).unwrap();
        entry.offset = 7;
        entry.flags.insert(FdFlags::NONBLOCK);
    }
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_RENUMBER,
        flags: 0,
        request_id: 875,
        args: fd_renumber_args(fd, 42),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let e = k.fds(pid).unwrap().get(42).expect("landed at 42");
    assert_eq!(e.offset, 7);
    assert!(e.flags.contains(FdFlags::NONBLOCK));
}

// ---- fd_readdir ------------------------------------------------------
//
// WASI's directory-listing opcode. Wire layout:
//
//   args[0..4]  = fd (u32)
//   args[4..12] = cookie (u64 LE; 0 = start from the beginning,
//                 otherwise resume after the entry whose d_next
//                 the caller last observed)
//   heap_ptr    = start of the caller's output buffer
//   heap_len    = buffer capacity in bytes
// Response:
//   value     = bytes actually written (0 ≤ value ≤ heap_len)
//   extra_len = mirrored bytes-written, same convention as random_get
//
// Each entry in the output buffer is a 24-byte dirent_t header
// (d_next / d_ino / d_namlen / d_type — see `abi::wasi::dirent`)
// followed immediately by d_namlen bytes of UTF-8 name, with NO
// inter-entry padding. A buffer that fills mid-entry receives a
// truncated final entry; the caller signals "more entries may
// exist" by observing value == heap_len and re-issuing the call
// with the last d_next cookie they successfully decoded.
//
// v1 does NOT inject `.` / `..` entries — WASI doesn't require
// them, and v1 filesystems don't track parent inodes so there's no
// honest way to synthesise `..`. Callers that need them can scan
// the directory and add them userland-side.
//
// Per-branch guards:
//   * Unopened fd → EBADF.
//   * Non-Vnode FdObject (CharDevice / Socket / Pipe / etc.) →
//     EINVAL. Only directories can be readdir'd.
//   * Vnode pointing at a non-directory → ENOTDIR (from
//     Filesystem::readdir via kerr_to_errno).
//   * heap_len == 0 → value=0, extra_len=0 (success with nothing
//     written; caller's probe for buffer sizing).

fn fd_readdir_args(fd: u32, cookie: u64) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&fd.to_le_bytes());
    args[4..12].copy_from_slice(&cookie.to_le_bytes());
    args
}

/// Decode a single dirent header out of `heap` at byte `offset`.
/// Returns (d_next, d_ino, d_namlen, d_type) — name bytes follow
/// at `heap[offset + 24 .. offset + 24 + d_namlen]`.
fn decode_dirent_header(heap: &[u8], offset: usize) -> (u64, u64, u32, u8) {
    use abi::wasi::dirent as de;
    let mut d_next_bytes = [0u8; 8];
    d_next_bytes.copy_from_slice(&heap[offset + de::OFF_D_NEXT..offset + de::OFF_D_NEXT + 8]);
    let d_next = u64::from_le_bytes(d_next_bytes);
    let mut d_ino_bytes = [0u8; 8];
    d_ino_bytes.copy_from_slice(&heap[offset + de::OFF_D_INO..offset + de::OFF_D_INO + 8]);
    let d_ino = u64::from_le_bytes(d_ino_bytes);
    let mut d_namlen_bytes = [0u8; 4];
    d_namlen_bytes
        .copy_from_slice(&heap[offset + de::OFF_D_NAMLEN..offset + de::OFF_D_NAMLEN + 4]);
    let d_namlen = u32::from_le_bytes(d_namlen_bytes);
    let d_type = heap[offset + de::OFF_D_TYPE];
    (d_next, d_ino, d_namlen, d_type)
}

fn make_dir_fd(
    k: &mut Kernel,
    name: &str,
    dir_path: &str,
) -> (abi::ext::Pid, u32) {
    let pid = make_running_proc(k, name, 0);
    k.vfs.mkdir(dir_path, 0o755).expect("mkdir");
    let (mount_id, ino) = k.vfs.resolve(dir_path).expect("resolve");
    k.install_fd(
        pid,
        10,
        FdObject::Vnode { mount_id, ino },
        FdFlags::EMPTY,
    )
    .unwrap();
    (pid, 10)
}

#[test]
fn fd_readdir_on_empty_directory_writes_no_bytes() {
    let mut k = make_kernel();
    let (pid, fd) = make_dir_fd(&mut k, "ereader", "/emptydir");
    let mut heap = vec![0u8; 256];

    let req = Request {
        opcode: op_wasi::FD_READDIR,
        flags: 0,
        request_id: 900,
        args: fd_readdir_args(fd, 0),
        heap_ptr: 0,
        heap_len: 256,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert_eq!(resp.extra_len, 0);
}

#[test]
fn fd_readdir_lists_all_entries_in_a_populated_directory() {
    let mut k = make_kernel();
    let (pid, fd) = make_dir_fd(&mut k, "reader", "/d");
    k.vfs.create("/d/one.txt", 0o644).expect("create one");
    k.vfs.create("/d/two.txt", 0o644).expect("create two");
    k.vfs.mkdir("/d/sub", 0o755).expect("mkdir sub");
    let mut heap = vec![0u8; 512];

    let req = Request {
        opcode: op_wasi::FD_READDIR,
        flags: 0,
        request_id: 901,
        args: fd_readdir_args(fd, 0),
        heap_ptr: 0,
        heap_len: 512,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);

    // Walk the dirent stream: three headers, each 24 bytes + name.
    let total = resp.value as usize;
    assert!(total > 0);
    let mut off = 0usize;
    let mut names = Vec::new();
    while off < total {
        let (d_next, _d_ino, d_namlen, d_type) = decode_dirent_header(&heap, off);
        let name_start = off + 24;
        let name_end = name_start + d_namlen as usize;
        let name = core::str::from_utf8(&heap[name_start..name_end]).unwrap().to_string();
        names.push((name, d_type, d_next));
        off = name_end;
    }
    assert_eq!(names.len(), 3);
    // Cookies strictly increasing (each d_next is the index of the
    // entry AFTER the current one, starting from 1).
    assert_eq!(names[0].2, 1);
    assert_eq!(names[1].2, 2);
    assert_eq!(names[2].2, 3);
    // One of them is the subdirectory, and its type is DIRECTORY.
    let sub = names.iter().find(|(n, _, _)| n == "sub").expect("sub listed");
    assert_eq!(sub.1, 3, "sub is filetype DIRECTORY (3)");
}

#[test]
fn fd_readdir_with_cookie_resumes_from_that_position() {
    // Cookie = 1 should skip the first entry and return the rest.
    let mut k = make_kernel();
    let (pid, fd) = make_dir_fd(&mut k, "resumer", "/r");
    k.vfs.create("/r/alpha", 0o644).expect("create");
    k.vfs.create("/r/beta", 0o644).expect("create");
    k.vfs.create("/r/gamma", 0o644).expect("create");
    let mut heap = vec![0u8; 512];

    let req = Request {
        opcode: op_wasi::FD_READDIR,
        flags: 0,
        request_id: 902,
        args: fd_readdir_args(fd, 1),
        heap_ptr: 0,
        heap_len: 512,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let total = resp.value as usize;
    let mut off = 0usize;
    let mut names = Vec::new();
    while off < total {
        let (_d_next, _d_ino, d_namlen, _d_type) = decode_dirent_header(&heap, off);
        let name_start = off + 24;
        let name_end = name_start + d_namlen as usize;
        let name = core::str::from_utf8(&heap[name_start..name_end]).unwrap().to_string();
        names.push(name);
        off = name_end;
    }
    // 2 entries remaining after skipping 1.
    assert_eq!(names.len(), 2);
}

#[test]
fn fd_readdir_truncates_when_buffer_fills_mid_entry() {
    // Size the buffer so one full entry lands and a partial second
    // entry forces truncation. Caller detects truncation via
    // value == heap_len on re-call.
    let mut k = make_kernel();
    let (pid, fd) = make_dir_fd(&mut k, "trunc", "/t");
    k.vfs.create("/t/aaaaaaaaaaaaaaa.txt", 0o644).expect("create"); // 19 bytes name
    k.vfs.create("/t/bbbbbbbbbbbbbbb.txt", 0o644).expect("create"); // 19 bytes name
    // One entry = 24 bytes header + 19 bytes name = 43 bytes.
    // Buffer of 50 bytes fits one full entry (43) + 7 bytes of
    // the next entry's header (partial).
    let mut heap = vec![0u8; 50];

    let req = Request {
        opcode: op_wasi::FD_READDIR,
        flags: 0,
        request_id: 903,
        args: fd_readdir_args(fd, 0),
        heap_ptr: 0,
        heap_len: 50,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    // At least one full entry written + some of the second.
    assert!(resp.value as usize >= 43);
    assert!(resp.value as usize <= 50);
}

#[test]
fn fd_readdir_on_non_directory_vnode_returns_enotdir() {
    // Open a regular file as a Vnode fd; readdir should return
    // ENOTDIR from tmpfs.readdir.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "nondir", "/notadir.txt", b"");
    let mut heap = vec![0u8; 256];

    let req = Request {
        opcode: op_wasi::FD_READDIR,
        flags: 0,
        request_id: 904,
        args: fd_readdir_args(fd, 0),
        heap_ptr: 0,
        heap_len: 256,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTDIR);
}

#[test]
fn fd_readdir_on_char_device_fd_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "cdev_reader", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 256];

    let req = Request {
        opcode: op_wasi::FD_READDIR,
        flags: 0,
        request_id: 905,
        args: fd_readdir_args(1, 0),
        heap_ptr: 0,
        heap_len: 256,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_readdir_on_unopened_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ghost_reader", 0);
    let mut heap = vec![0u8; 256];

    let req = Request {
        opcode: op_wasi::FD_READDIR,
        flags: 0,
        request_id: 906,
        args: fd_readdir_args(99, 0),
        heap_ptr: 0,
        heap_len: 256,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn fd_readdir_with_zero_sized_buffer_returns_zero_bytes() {
    let mut k = make_kernel();
    let (pid, fd) = make_dir_fd(&mut k, "probe_reader", "/p");
    k.vfs.create("/p/one", 0o644).expect("create");
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::FD_READDIR,
        flags: 0,
        request_id: 907,
        args: fd_readdir_args(fd, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
}

// ---- path_unlink_file / path_rename ---------------------------------
//
// Two filesystem-mutation opcodes that expand the syscall surface
// beyond the "read-mostly" set already covered by the prior eleven
// WASI slices. Both thread through the existing Vfs public API
// (`Vfs::unlink` + `Vfs::rename`) — no new Vfs methods or Filesystem
// trait additions; each in-tree fs (tmpfs, devfs, procfs, opfs)
// already implements the trait-level `unlink` + `rename` methods.
// The slice is purely about new syscall wire layouts + handlers.
//
// PATH_UNLINK_FILE wire layout:
//   args[0..4]  = dir_fd (u32, ignored — v1 has no preopens)
//   heap_ptr    = offset of UTF-8 path bytes
//   heap_len    = length of the path
// Response: value = 0 on success; status = -errno on error.
//
// PATH_RENAME wire layout: two paths in a single heap window need
// in-band length encoding since [`Request`] carries only one heap
// region. The kernel reads old_len from args[8..12] and splits the
// heap into (heap[0..old_len], heap[old_len..heap_len]) as (old,
// new). This keeps the inline args window consistent with other
// path opcodes (dir_fd at offsets 0/4) and fits both path lengths
// in a single heap round-trip.
//
//   args[0..4]   = from_dir_fd (u32, ignored — v1 has no preopens)
//   args[4..8]   = to_dir_fd   (u32, ignored)
//   args[8..12]  = old_len     (u32; index into heap for the split)
//   args[12..16] = reserved    (must be 0; ignored in v1)
//   heap[0..old_len]        = UTF-8 old path
//   heap[old_len..heap_len] = UTF-8 new path
//   heap_len                = old_len + new_len
// Response: value = 0 on success; status = -errno on error.

fn path_unlink_file_args(dir_fd: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&dir_fd.to_le_bytes());
    args
}

fn path_rename_args(from_dir_fd: u32, to_dir_fd: u32, old_len: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&from_dir_fd.to_le_bytes());
    args[4..8].copy_from_slice(&to_dir_fd.to_le_bytes());
    args[8..12].copy_from_slice(&old_len.to_le_bytes());
    args
}

#[test]
fn path_unlink_file_removes_regular_file_from_tmpfs() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "unlinker", 0);
    k.vfs.create("/u.txt", 0o644).expect("create");
    assert!(k.vfs.stat("/u.txt").is_ok(), "file exists pre-call");

    let path = b"/u.txt";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_UNLINK_FILE,
        flags: 0,
        request_id: 880,
        args: path_unlink_file_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(k.vfs.stat("/u.txt").unwrap_err(), kernel::vfs::FsError::NotFound);
}

#[test]
fn path_unlink_file_on_missing_path_returns_enoent() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "unlinker", 0);
    let path = b"/no_such_file";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_UNLINK_FILE,
        flags: 0,
        request_id: 881,
        args: path_unlink_file_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOENT);
}

#[test]
fn path_unlink_file_on_directory_returns_eisdir() {
    // WASI's path_unlink_file is strictly for regular files —
    // unlinking a directory must use path_remove_directory.
    // tmpfs.unlink returns IsADirectory which maps to EISDIR.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "unlinker", 0);
    k.vfs.mkdir("/d", 0o755).expect("mkdir");

    let path = b"/d";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_UNLINK_FILE,
        flags: 0,
        request_id: 882,
        args: path_unlink_file_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EISDIR);
}

#[test]
fn path_unlink_file_on_devfs_returns_erofs() {
    // devfs overrides unlink to return ReadOnly → EROFS. Matches
    // the prior path_filestat_set_times devfs path in terms of
    // wire-level errno; the only write-side mutation that ever
    // reaches devfs is through one of these syscall handlers.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "unlinker", 0);
    let path = b"/dev/console";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_UNLINK_FILE,
        flags: 0,
        request_id: 883,
        args: path_unlink_file_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EROFS);
}

#[test]
fn path_unlink_file_with_invalid_utf8_path_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "unlinker", 0);
    let mut heap = vec![0u8; 64];
    heap[0] = 0xff;
    heap[1] = 0xfe;

    let req = Request {
        opcode: op_wasi::PATH_UNLINK_FILE,
        flags: 0,
        request_id: 884,
        args: path_unlink_file_args(0),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_rename_moves_file_within_same_directory() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renamer", 0);
    k.vfs.create("/a.txt", 0o644).expect("create");
    k.vfs.write("/a.txt", 0, b"hello").expect("write");

    let old_path = b"/a.txt";
    let new_path = b"/b.txt";
    let mut heap = vec![0u8; 128];
    heap[..old_path.len()].copy_from_slice(old_path);
    heap[old_path.len()..old_path.len() + new_path.len()].copy_from_slice(new_path);

    let req = Request {
        opcode: op_wasi::PATH_RENAME,
        flags: 0,
        request_id: 890,
        args: path_rename_args(0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len: (old_path.len() + new_path.len()) as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(
        k.vfs.stat("/a.txt").unwrap_err(),
        kernel::vfs::FsError::NotFound,
    );
    let st = k.vfs.stat("/b.txt").unwrap();
    assert_eq!(st.size, 5);
}

#[test]
fn path_rename_moves_file_across_directories() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renamer", 0);
    k.vfs.mkdir("/src", 0o755).expect("mkdir src");
    k.vfs.mkdir("/dst", 0o755).expect("mkdir dst");
    k.vfs.create("/src/file", 0o644).expect("create");

    let old_path = b"/src/file";
    let new_path = b"/dst/file";
    let mut heap = vec![0u8; 128];
    heap[..old_path.len()].copy_from_slice(old_path);
    heap[old_path.len()..old_path.len() + new_path.len()].copy_from_slice(new_path);

    let req = Request {
        opcode: op_wasi::PATH_RENAME,
        flags: 0,
        request_id: 891,
        args: path_rename_args(0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len: (old_path.len() + new_path.len()) as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert!(k.vfs.stat("/src/file").is_err());
    assert!(k.vfs.stat("/dst/file").is_ok());
}

#[test]
fn path_rename_from_missing_returns_enoent() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renamer", 0);
    let old_path = b"/nope";
    let new_path = b"/also_nope";
    let mut heap = vec![0u8; 128];
    heap[..old_path.len()].copy_from_slice(old_path);
    heap[old_path.len()..old_path.len() + new_path.len()].copy_from_slice(new_path);

    let req = Request {
        opcode: op_wasi::PATH_RENAME,
        flags: 0,
        request_id: 892,
        args: path_rename_args(0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len: (old_path.len() + new_path.len()) as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOENT);
}

#[test]
fn path_rename_on_devfs_returns_erofs() {
    // devfs overrides rename to return ReadOnly → EROFS.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renamer", 0);
    let old_path = b"/dev/null";
    let new_path = b"/dev/nullx";
    let mut heap = vec![0u8; 128];
    heap[..old_path.len()].copy_from_slice(old_path);
    heap[old_path.len()..old_path.len() + new_path.len()].copy_from_slice(new_path);

    let req = Request {
        opcode: op_wasi::PATH_RENAME,
        flags: 0,
        request_id: 893,
        args: path_rename_args(0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len: (old_path.len() + new_path.len()) as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EROFS);
}

#[test]
fn path_rename_cross_mount_returns_enotsup() {
    // Vfs::rename explicitly rejects cross-mount renames with
    // NotSupported so userland uses create+write+unlink instead.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renamer", 0);
    k.vfs.create("/x.txt", 0o644).expect("create");
    let old_path = b"/x.txt";
    let new_path = b"/dev/x.txt";
    let mut heap = vec![0u8; 128];
    heap[..old_path.len()].copy_from_slice(old_path);
    heap[old_path.len()..old_path.len() + new_path.len()].copy_from_slice(new_path);

    let req = Request {
        opcode: op_wasi::PATH_RENAME,
        flags: 0,
        request_id: 894,
        args: path_rename_args(0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len: (old_path.len() + new_path.len()) as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTSUP);
}

#[test]
fn path_rename_with_zero_old_len_returns_einval() {
    // Empty old path is nonsensical.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renamer", 0);
    let new_path = b"/nope";
    let mut heap = vec![0u8; 64];
    heap[..new_path.len()].copy_from_slice(new_path);

    let req = Request {
        opcode: op_wasi::PATH_RENAME,
        flags: 0,
        request_id: 895,
        args: path_rename_args(0, 0, 0),
        heap_ptr: 0,
        heap_len: new_path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_rename_with_old_len_past_heap_returns_einval() {
    // old_len exceeding heap_len is a malformed shim.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renamer", 0);
    let mut heap = vec![0u8; 64];
    heap[..4].copy_from_slice(b"/abc");

    let req = Request {
        opcode: op_wasi::PATH_RENAME,
        flags: 0,
        request_id: 896,
        args: path_rename_args(0, 0, 999),
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_rename_with_zero_new_path_returns_einval() {
    // new_len = 0 (heap_len == old_len) = empty new path.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "renamer", 0);
    let old_path = b"/a.txt";
    let mut heap = vec![0u8; 64];
    heap[..old_path.len()].copy_from_slice(old_path);

    let req = Request {
        opcode: op_wasi::PATH_RENAME,
        flags: 0,
        request_id: 897,
        args: path_rename_args(0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len: old_path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

// ---- path_create_directory / path_remove_directory -----------------
//
// mkdir + rmdir opcodes at 0x0040 + 0x0046. Both thread through the
// existing Vfs API (`Vfs::mkdir` + `Vfs::rmdir`) — no new Vfs methods
// and no Filesystem trait additions; each in-tree fs (tmpfs, devfs,
// procfs, opfs) already implements the trait-level `mkdir` + `rmdir`
// methods. The slice is purely syscall wiring, mirroring the shape
// of path_unlink_file.
//
// Wire layouts match path_unlink_file: args[0..4] = dir_fd (ignored
// in v1, no preopens); heap = UTF-8 path bytes; heap_len = path
// length. PATH_CREATE_DIRECTORY hard-codes mode 0o755 — WASI's
// mkdir signature has no mode argument, so the kernel picks a
// sensible default for the new directory's permission bits.
// Response: value = 0 on success; status = -errno on error.

fn path_mkdir_args(dir_fd: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&dir_fd.to_le_bytes());
    args
}

fn path_rmdir_args(dir_fd: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&dir_fd.to_le_bytes());
    args
}

#[test]
fn path_create_directory_creates_directory_on_tmpfs() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "mkdirer", 0);
    let path = b"/newdir";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_CREATE_DIRECTORY,
        flags: 0,
        request_id: 910,
        args: path_mkdir_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    let stat = k.vfs.stat("/newdir").expect("stat /newdir after mkdir");
    assert_eq!(stat.ty, kernel::vfs::NodeType::Directory);
}

#[test]
fn path_create_directory_on_existing_name_returns_eexist() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "mkdirer", 0);
    k.vfs.create("/already", 0o644).expect("seed file");
    let path = b"/already";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_CREATE_DIRECTORY,
        flags: 0,
        request_id: 911,
        args: path_mkdir_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EEXIST);
}

#[test]
fn path_create_directory_with_missing_parent_returns_enoent() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "mkdirer", 0);
    let path = b"/no/such/parent/child";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_CREATE_DIRECTORY,
        flags: 0,
        request_id: 912,
        args: path_mkdir_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOENT);
}

#[test]
fn path_create_directory_on_devfs_returns_erofs() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "mkdirer", 0);
    let path = b"/dev/newdir";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_CREATE_DIRECTORY,
        flags: 0,
        request_id: 913,
        args: path_mkdir_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EROFS);
}

#[test]
fn path_create_directory_with_invalid_utf8_path_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "mkdirer", 0);
    let mut heap = vec![0u8; 64];
    heap[0] = 0xff;
    heap[1] = 0xfe;

    let req = Request {
        opcode: op_wasi::PATH_CREATE_DIRECTORY,
        flags: 0,
        request_id: 914,
        args: path_mkdir_args(0),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_remove_directory_removes_empty_directory() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "rmdirer", 0);
    k.vfs.mkdir("/emptydir", 0o755).expect("seed dir");
    assert!(k.vfs.stat("/emptydir").is_ok(), "dir exists pre-call");
    let path = b"/emptydir";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_REMOVE_DIRECTORY,
        flags: 0,
        request_id: 915,
        args: path_rmdir_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert_eq!(
        k.vfs.stat("/emptydir").unwrap_err(),
        kernel::vfs::FsError::NotFound,
    );
}

#[test]
fn path_remove_directory_on_non_empty_returns_enotempty() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "rmdirer", 0);
    k.vfs.mkdir("/fulldir", 0o755).expect("seed dir");
    k.vfs.create("/fulldir/child", 0o644).expect("seed child");
    let path = b"/fulldir";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_REMOVE_DIRECTORY,
        flags: 0,
        request_id: 916,
        args: path_rmdir_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTEMPTY);
}

#[test]
fn path_remove_directory_on_regular_file_returns_enotdir() {
    // Calling rmdir on a regular file — tmpfs.rmdir returns
    // NotADirectory which maps to ENOTDIR. WASI callers must use
    // path_unlink_file for regular files instead.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "rmdirer", 0);
    k.vfs.create("/notadir.txt", 0o644).expect("seed file");
    let path = b"/notadir.txt";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_REMOVE_DIRECTORY,
        flags: 0,
        request_id: 917,
        args: path_rmdir_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTDIR);
}

#[test]
fn path_remove_directory_on_missing_path_returns_enoent() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "rmdirer", 0);
    let path = b"/nope";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_REMOVE_DIRECTORY,
        flags: 0,
        request_id: 918,
        args: path_rmdir_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOENT);
}

#[test]
fn path_remove_directory_on_devfs_returns_erofs() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "rmdirer", 0);
    let path = b"/dev/somedir";
    let mut heap = vec![0u8; 64];
    heap[..path.len()].copy_from_slice(path);

    let req = Request {
        opcode: op_wasi::PATH_REMOVE_DIRECTORY,
        flags: 0,
        request_id: 919,
        args: path_rmdir_args(0),
        heap_ptr: 0,
        heap_len: path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EROFS);
}

// ---- fd_fdstat_set_flags --------------------------------------------
//
// Opcode 0x0025. WASI's equivalent of POSIX fcntl(F_SETFL) —
// overwrites the fd's NONBLOCK / APPEND / *SYNC bits. Wire:
//
//   args[0..4] = fd (u32)
//   args[4..8] = new_fdflags (u32; WASI encoding from abi::wasi::fdflags
//                — APPEND=0x01, DSYNC=0x02, NONBLOCK=0x04, RSYNC=0x08,
//                SYNC=0x10)
//
// Translation: the WASI bit values differ from PMos's internal
// FdFlags (CLOEXEC=0x01, NONBLOCK=0x02, APPEND=0x04), so the handler
// decodes the WASI u32 and sets the corresponding PMos bits. Only
// NONBLOCK + APPEND are meaningful in v1; DSYNC/RSYNC/SYNC are
// accepted and discarded (tmpfs has no write buffer to sync). The
// CLOEXEC bit is preserved — F_SETFL semantics: the "fd-level"
// flag is only mutated at spawn time, not by set_flags.

fn fd_fdstat_set_flags_args(fd: u32, wasi_fdflags: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&fd.to_le_bytes());
    args[4..8].copy_from_slice(&wasi_fdflags.to_le_bytes());
    args
}

#[test]
fn fd_fdstat_set_flags_sets_nonblock_on_open_fd() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "flagger", "/f.txt", b"hi");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FDSTAT_SET_FLAGS,
        flags: 0,
        request_id: 920,
        args: fd_fdstat_set_flags_args(fd, abi::wasi::fdflags::NONBLOCK as u32),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let e = k.fds(pid).unwrap().get(fd).unwrap();
    assert!(e.flags.contains(FdFlags::NONBLOCK));
    assert!(!e.flags.contains(FdFlags::APPEND));
}

#[test]
fn fd_fdstat_set_flags_sets_append_on_open_fd() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "flagger", "/f.txt", b"hi");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FDSTAT_SET_FLAGS,
        flags: 0,
        request_id: 921,
        args: fd_fdstat_set_flags_args(fd, abi::wasi::fdflags::APPEND as u32),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let e = k.fds(pid).unwrap().get(fd).unwrap();
    assert!(e.flags.contains(FdFlags::APPEND));
    assert!(!e.flags.contains(FdFlags::NONBLOCK));
}

#[test]
fn fd_fdstat_set_flags_with_zero_clears_previously_set_flags() {
    // Set NONBLOCK + APPEND directly in the FdTable, then call
    // fd_fdstat_set_flags(0) to clear them. F_SETFL-style replace
    // semantics: whatever bits weren't in the argument are cleared.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "flagger", "/f.txt", b"hi");
    {
        let table = k.fds_mut(pid).unwrap();
        let entry = table.get_mut(fd).unwrap();
        entry.flags.insert(FdFlags::NONBLOCK);
        entry.flags.insert(FdFlags::APPEND);
    }
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FDSTAT_SET_FLAGS,
        flags: 0,
        request_id: 922,
        args: fd_fdstat_set_flags_args(fd, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let e = k.fds(pid).unwrap().get(fd).unwrap();
    assert!(!e.flags.contains(FdFlags::NONBLOCK));
    assert!(!e.flags.contains(FdFlags::APPEND));
}

#[test]
fn fd_fdstat_set_flags_preserves_cloexec() {
    // CLOEXEC is a file-descriptor-level flag (POSIX F_SETFD) not a
    // file-status flag (F_SETFL). fd_fdstat_set_flags is the WASI
    // equivalent of F_SETFL, so CLOEXEC must not be disturbed by it.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "flagger", "/f.txt", b"hi");
    {
        let table = k.fds_mut(pid).unwrap();
        let entry = table.get_mut(fd).unwrap();
        entry.flags.insert(FdFlags::CLOEXEC);
    }
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FDSTAT_SET_FLAGS,
        flags: 0,
        request_id: 923,
        args: fd_fdstat_set_flags_args(fd, abi::wasi::fdflags::NONBLOCK as u32),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let e = k.fds(pid).unwrap().get(fd).unwrap();
    assert!(e.flags.contains(FdFlags::CLOEXEC), "CLOEXEC preserved across set_flags");
    assert!(e.flags.contains(FdFlags::NONBLOCK));
}

#[test]
fn fd_fdstat_set_flags_accepts_sync_bits_as_noop() {
    // WASI's DSYNC / RSYNC / SYNC bits are meaningful for platforms
    // with durable-write guarantees. v1's tmpfs is already
    // synchronous (writes land in memory immediately), so these bits
    // are accepted without error and then discarded — they never set
    // any internal flag. Passing them combined with NONBLOCK still
    // sets NONBLOCK correctly.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "flagger", "/f.txt", b"hi");
    let combined = (abi::wasi::fdflags::DSYNC
        | abi::wasi::fdflags::RSYNC
        | abi::wasi::fdflags::SYNC
        | abi::wasi::fdflags::NONBLOCK) as u32;
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FDSTAT_SET_FLAGS,
        flags: 0,
        request_id: 924,
        args: fd_fdstat_set_flags_args(fd, combined),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let e = k.fds(pid).unwrap().get(fd).unwrap();
    assert!(e.flags.contains(FdFlags::NONBLOCK));
    assert!(!e.flags.contains(FdFlags::APPEND));
}

#[test]
fn fd_fdstat_set_flags_with_unopened_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ghost_flagger", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FDSTAT_SET_FLAGS,
        flags: 0,
        request_id: 925,
        args: fd_fdstat_set_flags_args(99, abi::wasi::fdflags::NONBLOCK as u32),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

// ---- fd_filestat_set_size ------------------------------------------
//
// Opcode 0x0028. Truncate / extend a seekable fd to a specific size.
// Wire:
//   args[0..4]  = fd (u32)
//   args[4..12] = new_size (u64 LE)
// Response: value = 0 on success; status = -errno on error.
//
// Vnode fds only — the operation has no meaning on a char device,
// socket, pipe, signal channel, or display connection. The handler
// rejects those with EINVAL (same branch shape as fd_seek / fd_tell).
// Directory targets passthrough to tmpfs.truncate which returns
// IsADirectory → EISDIR. Read-only filesystems (procfs) return
// ReadOnly → EROFS; unsupported filesystems (devfs) return
// NotSupported → ENOTSUP (but devfs has no regular-file vnodes to
// reach in v1, so that branch isn't exercised from the WASI surface).

fn fd_filestat_set_size_args(fd: u32, new_size: u64) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&fd.to_le_bytes());
    args[4..12].copy_from_slice(&new_size.to_le_bytes());
    args
}

#[test]
fn fd_filestat_set_size_truncates_tmpfs_file() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "truncer", "/big.txt", b"0123456789");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_SIZE,
        flags: 0,
        request_id: 930,
        args: fd_filestat_set_size_args(fd, 4),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    let stat = k.vfs.stat("/big.txt").expect("stat");
    assert_eq!(stat.size, 4);
}

#[test]
fn fd_filestat_set_size_extends_tmpfs_file_with_zeros() {
    // Extending past EOF should zero-fill; POSIX + WASI both permit this.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "extender", "/small.txt", b"abc");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_SIZE,
        flags: 0,
        request_id: 931,
        args: fd_filestat_set_size_args(fd, 16),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let stat = k.vfs.stat("/small.txt").expect("stat");
    assert_eq!(stat.size, 16);

    // First three bytes preserved; rest zero.
    let mut buf = [0u8; 16];
    let n = k.vfs.read("/small.txt", 0, &mut buf).expect("read");
    assert_eq!(n, 16);
    assert_eq!(&buf[..3], b"abc");
    assert!(buf[3..].iter().all(|&b| b == 0), "zero fill past old EOF");
}

#[test]
fn fd_filestat_set_size_on_directory_vnode_returns_eisdir() {
    // Open a directory vnode (via make_dir_fd) and try to truncate it.
    // tmpfs.truncate returns IsADirectory → EISDIR.
    let mut k = make_kernel();
    let (pid, fd) = make_dir_fd(&mut k, "dirtruncer", "/d_trunc");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_SIZE,
        flags: 0,
        request_id: 932,
        args: fd_filestat_set_size_args(fd, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EISDIR);
}

#[test]
fn fd_filestat_set_size_on_char_device_fd_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "cdevtruncer", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_SIZE,
        flags: 0,
        request_id: 933,
        args: fd_filestat_set_size_args(1, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_filestat_set_size_on_unopened_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ghosttruncer", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_SIZE,
        flags: 0,
        request_id: 934,
        args: fd_filestat_set_size_args(99, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

// ---- fd_pread / fd_pwrite --------------------------------------------
//
// Opcodes 0x002A + 0x002D. Positional-I/O variants of fd_read /
// fd_write: take an explicit offset from inline args and do NOT
// mutate FdEntry.offset. Wire layout (both shapes identical except
// for the heap direction):
//
//   args[0..4]  = fd (u32)
//   args[4..12] = offset (u64 LE)
//   heap_ptr    = source (for pwrite) or destination (for pread)
//   heap_len    = byte count
// Response:
//   pwrite.value = bytes written
//   pread.value  = bytes read (0 on EOF); extra_len mirrors value
//
// Vnode-only. Non-Vnode FdObject variants reject with EINVAL —
// positional I/O has no meaning on a char device / socket / pipe /
// signal channel / display connection. The handler reaches
// Vfs::read_ino / Vfs::write_ino directly with the explicit offset;
// FdEntry.offset stays untouched, so a pread+pwrite pair does not
// disturb a subsequent fd_read / fd_seek that uses the seekable-fd
// position.

fn fd_pread_args(fd: u32, offset: u64) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&fd.to_le_bytes());
    args[4..12].copy_from_slice(&offset.to_le_bytes());
    args
}

fn fd_pwrite_args(fd: u32, offset: u64) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&fd.to_le_bytes());
    args[4..12].copy_from_slice(&offset.to_le_bytes());
    args
}

#[test]
fn fd_pread_reads_from_explicit_offset_without_advancing_entry_offset() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "preader", "/r.txt", b"0123456789");
    // Set a non-zero entry.offset so we can verify it's preserved.
    k.fds_mut(pid).unwrap().get_mut(fd).unwrap().offset = 3;
    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::FD_PREAD,
        flags: 0,
        request_id: 940,
        args: fd_pread_args(fd, 5),
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 4);
    assert_eq!(resp.extra_len, 4);
    assert_eq!(&heap[..4], b"5678");
    // entry.offset was 3 before the call, must still be 3 after.
    assert_eq!(k.fds(pid).unwrap().get(fd).unwrap().offset, 3);
}

#[test]
fn fd_pread_at_offset_zero_reads_from_start() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "preader", "/r.txt", b"0123456789");
    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::FD_PREAD,
        flags: 0,
        request_id: 941,
        args: fd_pread_args(fd, 0),
        heap_ptr: 0,
        heap_len: 3,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 3);
    assert_eq!(&heap[..3], b"012");
}

#[test]
fn fd_pread_past_eof_returns_zero_bytes() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "preader", "/r.txt", b"0123456789");
    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::FD_PREAD,
        flags: 0,
        request_id: 942,
        args: fd_pread_args(fd, 100),
        heap_ptr: 0,
        heap_len: 8,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
}

#[test]
fn fd_pread_on_char_device_fd_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "cdevpreader", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::FD_PREAD,
        flags: 0,
        request_id: 943,
        args: fd_pread_args(1, 0),
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_pread_on_unopened_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ghostpreader", 0);
    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::FD_PREAD,
        flags: 0,
        request_id: 944,
        args: fd_pread_args(99, 0),
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn fd_pwrite_writes_at_explicit_offset_without_advancing_entry_offset() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "pwriter", "/w.txt", b"0000000000");
    // Non-zero entry.offset survives the pwrite call.
    k.fds_mut(pid).unwrap().get_mut(fd).unwrap().offset = 2;
    let mut heap = vec![0u8; 32];
    heap[..3].copy_from_slice(b"abc");

    let req = Request {
        opcode: op_wasi::FD_PWRITE,
        flags: 0,
        request_id: 945,
        args: fd_pwrite_args(fd, 4),
        heap_ptr: 0,
        heap_len: 3,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 3);
    // Observable effect: reading the file shows "0000abc000".
    let mut buf = [0u8; 10];
    k.vfs.read("/w.txt", 0, &mut buf).unwrap();
    assert_eq!(&buf, b"0000abc000");
    // entry.offset was 2 before the call, must still be 2 after.
    assert_eq!(k.fds(pid).unwrap().get(fd).unwrap().offset, 2);
}

#[test]
fn fd_pwrite_past_eof_extends_file() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "pwriter", "/w.txt", b"abc");
    let mut heap = vec![0u8; 32];
    heap[..2].copy_from_slice(b"hi");

    // Write "hi" at offset 10 — extends the file with zero-fill up
    // through offset 10 (tmpfs.write zero-fills the gap) then the
    // payload.
    let req = Request {
        opcode: op_wasi::FD_PWRITE,
        flags: 0,
        request_id: 946,
        args: fd_pwrite_args(fd, 10),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 2);
    let stat = k.vfs.stat("/w.txt").unwrap();
    assert_eq!(stat.size, 12);
}

#[test]
fn fd_pwrite_on_char_device_fd_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "cdevpwriter", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 32];
    heap[..2].copy_from_slice(b"hi");

    let req = Request {
        opcode: op_wasi::FD_PWRITE,
        flags: 0,
        request_id: 947,
        args: fd_pwrite_args(1, 0),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_pwrite_on_unopened_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ghostpwriter", 0);
    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::FD_PWRITE,
        flags: 0,
        request_id: 948,
        args: fd_pwrite_args(99, 0),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn fd_pwrite_to_procfs_vnode_returns_erofs() {
    // procfs.write returns ReadOnly → EROFS. /proc/version is a
    // regular-file vnode that the handler passes to vfs.write_ino.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "procpwriter", 0);
    let (mount_id, ino) = k.vfs.resolve("/proc/version").expect("resolve");
    k.install_fd(
        pid,
        10,
        FdObject::Vnode { mount_id, ino },
        FdFlags::EMPTY,
    )
    .unwrap();
    let mut heap = vec![0u8; 32];
    heap[..2].copy_from_slice(b"hi");

    let req = Request {
        opcode: op_wasi::FD_PWRITE,
        flags: 0,
        request_id: 949,
        args: fd_pwrite_args(10, 0),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EROFS);
}

#[test]
fn fd_filestat_set_size_on_procfs_returns_erofs() {
    // procfs.truncate returns ReadOnly → EROFS. /proc/version is a
    // regular-file vnode, so it opens as FdObject::Vnode and the
    // handler reaches the filesystem's truncate method rather than
    // stopping at the non-Vnode EINVAL guard.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "proctruncer", 0);
    let (mount_id, ino) = k.vfs.resolve("/proc/version").expect("resolve proc version");
    k.install_fd(
        pid,
        10,
        FdObject::Vnode { mount_id, ino },
        FdFlags::EMPTY,
    )
    .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_FILESTAT_SET_SIZE,
        flags: 0,
        request_id: 935,
        args: fd_filestat_set_size_args(10, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EROFS);
}

// ---- sock_send / sock_recv -----------------------------------------
//
// Opcodes 0x0072 + 0x0071. WASI socket aliases of FD_WRITE / FD_READ
// on Socket fds. Wire:
//
//   SOCK_SEND: args[0..4] = fd (u32)
//              args[4..8] = si_flags (u16 low; ignored in v1)
//              heap_ptr   = source bytes
//              heap_len   = byte count
//   SOCK_RECV: args[0..4] = fd (u32)
//              args[4..8] = ri_flags (u16 low; ignored in v1)
//              heap_ptr   = destination buffer
//              heap_len   = buffer capacity
// Response (sock_send): value = bytes sent
// Response (sock_recv): value + extra_len = bytes received
//
// Socket-only — non-Socket FdObject variants reject with EINVAL
// (PMos's kerr_to_errno has no ENOTSOCK; EINVAL is the honest
// answer, matching the non-Socket branch for every other fd-type-
// specific opcode). An InvalidState IpcError (e.g. sending on an
// unconnected socket) also surfaces as EINVAL via the standard
// IpcError → KernelError mapping. Unopened fds return EBADF.
//
// v1 ignores the si_flags / ri_flags u16 entirely — WASI defines
// them for out-of-band data, MSG_PEEK, MSG_DONTWAIT etc., none of
// which v1's single-threaded kernel exposes. The dispatcher
// accepts whatever low 16 bits are supplied and discards them.

fn sock_send_args(fd: u32, si_flags: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&fd.to_le_bytes());
    args[4..8].copy_from_slice(&si_flags.to_le_bytes());
    args
}

fn sock_recv_args(fd: u32, ri_flags: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&fd.to_le_bytes());
    args[4..8].copy_from_slice(&ri_flags.to_le_bytes());
    args
}

#[test]
fn sock_send_delivers_bytes_to_connected_peer() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sender", 0);

    // Wire a connected socket pair a ↔ b.
    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 10, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();

    let msg = b"hello";
    let mut heap = vec![0u8; 64];
    heap[..msg.len()].copy_from_slice(msg);

    let req = Request {
        opcode: op_wasi::SOCK_SEND,
        flags: 0,
        request_id: 950,
        args: sock_send_args(10, 0),
        heap_ptr: 0,
        heap_len: msg.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, msg.len() as i64);
    let sb = k.ipc.socket_mut(b).unwrap();
    let delivered: Vec<u8> = sb.rx_buf.iter().copied().collect();
    assert_eq!(&delivered[..], msg);
}

#[test]
fn sock_send_ignores_si_flags() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sender", 0);

    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 10, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();

    let mut heap = vec![0u8; 64];
    heap[..2].copy_from_slice(b"hi");

    // Non-zero si_flags (e.g. WASI's siflags bit 0) must not break the
    // call — v1 discards those bits rather than validating them.
    let req = Request {
        opcode: op_wasi::SOCK_SEND,
        flags: 0,
        request_id: 951,
        args: sock_send_args(10, 0x0001),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 2);
}

#[test]
fn sock_send_on_char_device_fd_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "cdevsender", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];
    heap[..2].copy_from_slice(b"hi");

    let req = Request {
        opcode: op_wasi::SOCK_SEND,
        flags: 0,
        request_id: 952,
        args: sock_send_args(1, 0),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn sock_send_on_unopened_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ghostsender", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SOCK_SEND,
        flags: 0,
        request_id: 953,
        args: sock_send_args(99, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn sock_recv_reads_bytes_from_peer_send() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "receiver", 0);

    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
        sa.rx_buf.extend(b"payload");
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 10, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();

    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::SOCK_RECV,
        flags: 0,
        request_id: 954,
        args: sock_recv_args(10, 0),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 7);
    assert_eq!(resp.extra_len, 7);
    assert_eq!(&heap[..7], b"payload");
}

#[test]
fn sock_recv_on_char_device_fd_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "cdevreceiver", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::SOCK_RECV,
        flags: 0,
        request_id: 955,
        args: sock_recv_args(1, 0),
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn sock_recv_on_unopened_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ghostreceiver", 0);
    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::SOCK_RECV,
        flags: 0,
        request_id: 956,
        args: sock_recv_args(99, 0),
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

// ---- sock_accept ---------------------------------------------------
//
// Opcode 0x0070. WASI alias of the existing IPC_ACCEPT at ext
// 0x1004. Accepts a pending connection on a listening socket and
// returns a new fd for the accepted connection. Wire:
//
//   args[0..4] = listener_fd (u32)
//   args[4..8] = fdflags (u32, WASI encoding — applied to the
//                newly-allocated fd via FdFlags::from_wasi_bits;
//                typically NONBLOCK)
// Response:
//   value = freshly-allocated fd for the accepted connection.
//
// Reuses Kernel::accept_socket (same method IPC_ACCEPT calls). The
// only addition is the fdflags translation applied to the new fd
// after allocation — WASI's sock_accept signature is
// `sock_accept(fd, flags) -> fd` with the flags semantically the
// same as the F_SETFL bits path_open/fd_fdstat_set_flags understand.
//
// Error surface mirrors IPC_ACCEPT: non-Socket FdObject → EINVAL
// (via NotSupportedOnFd); unopened fd → EBADF; listener not in
// Listening state → EINVAL; empty backlog → EAGAIN.

fn sock_accept_args(listener_fd: u32, fdflags: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&listener_fd.to_le_bytes());
    args[4..8].copy_from_slice(&fdflags.to_le_bytes());
    args
}

#[test]
fn sock_accept_returns_fresh_fd_for_pending_backlog_client() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "accepter", 0);

    // Listener with a pending Connecting client on its backlog.
    let listener = k.ipc.create_socket(SocketType::Stream);
    let client = k.ipc.create_socket(SocketType::Stream);
    {
        let ls = k.ipc.socket_mut(listener).unwrap();
        ls.state = SocketState::Listening;
        ls.backlog.push_back(client);
    }
    {
        let cs = k.ipc.socket_mut(client).unwrap();
        cs.state = SocketState::Connecting;
    }
    k.install_fd(pid, 3, FdObject::Socket(listener.0), FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SOCK_ACCEPT,
        flags: 0,
        request_id: 970,
        args: sock_accept_args(3, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let new_fd = resp.value as u32;
    // The new fd must be a Socket variant (server side of the
    // accepted connection).
    let entry = k.fds(pid).unwrap().get(new_fd).unwrap();
    assert!(matches!(entry.object, FdObject::Socket(_)));
}

#[test]
fn sock_accept_applies_wasi_fdflags_to_the_new_fd() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "accepter", 0);

    let listener = k.ipc.create_socket(SocketType::Stream);
    let client = k.ipc.create_socket(SocketType::Stream);
    {
        let ls = k.ipc.socket_mut(listener).unwrap();
        ls.state = SocketState::Listening;
        ls.backlog.push_back(client);
    }
    {
        let cs = k.ipc.socket_mut(client).unwrap();
        cs.state = SocketState::Connecting;
    }
    k.install_fd(pid, 3, FdObject::Socket(listener.0), FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SOCK_ACCEPT,
        flags: 0,
        request_id: 971,
        args: sock_accept_args(3, abi::wasi::fdflags::NONBLOCK as u32),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let new_fd = resp.value as u32;
    let entry = k.fds(pid).unwrap().get(new_fd).unwrap();
    assert!(entry.flags.contains(FdFlags::NONBLOCK));
}

#[test]
fn sock_accept_on_empty_backlog_returns_eagain() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "accepter", 0);

    let listener = k.ipc.create_socket(SocketType::Stream);
    {
        let ls = k.ipc.socket_mut(listener).unwrap();
        ls.state = SocketState::Listening;
        // No backlog entries — empty.
    }
    k.install_fd(pid, 3, FdObject::Socket(listener.0), FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SOCK_ACCEPT,
        flags: 0,
        request_id: 972,
        args: sock_accept_args(3, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EAGAIN);
}

#[test]
fn sock_accept_on_non_listening_socket_returns_einval() {
    use kernel::ipc::SocketType;
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "accepter", 0);

    // Fresh socket — state defaults to Unbound, not Listening.
    let s = k.ipc.create_socket(SocketType::Stream);
    k.install_fd(pid, 3, FdObject::Socket(s.0), FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SOCK_ACCEPT,
        flags: 0,
        request_id: 973,
        args: sock_accept_args(3, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn sock_accept_on_char_device_fd_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "cdev_accepter", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SOCK_ACCEPT,
        flags: 0,
        request_id: 974,
        args: sock_accept_args(1, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn sock_accept_on_unopened_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ghost_accepter", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SOCK_ACCEPT,
        flags: 0,
        request_id: 975,
        args: sock_accept_args(99, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

// ---- sock_shutdown -------------------------------------------------
//
// Opcode 0x0073. Shutdown one or both directions of a socket.
// Wire:
//
//   args[0..4] = fd (u32)
//   args[4..8] = how (u32, low 8 bits = WASI sdflags: RD=0x1, WR=0x2)
//
// v1's IpcTable has no half-close primitive — close_socket tears
// down both directions at once — so the handler accepts only
// `RD | WR` (full close, mapped to close_socket) and rejects the
// half-close combinations with ENOTSUP. Zero `how` rejects with
// EINVAL since shutting down nothing is meaningless; bits beyond
// RD | WR also reject with EINVAL.
//
// Standard guards: non-Socket FdObject → EINVAL; unopened fd →
// EBADF.

fn sock_shutdown_args(fd: u32, how: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&fd.to_le_bytes());
    args[4..8].copy_from_slice(&how.to_le_bytes());
    args
}

#[test]
fn sock_shutdown_rdwr_closes_socket_observable_via_peer_eof() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "shutter", 0);

    // Connect a ↔ b.
    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 16];

    let how = (abi::wasi::sdflags::RD | abi::wasi::sdflags::WR) as u32;
    let req = Request {
        opcode: op_wasi::SOCK_SHUTDOWN,
        flags: 0,
        request_id: 980,
        args: sock_shutdown_args(3, how),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);

    // a is now closed; b's recv_on_socket should return (0, []) EOF.
    let mut rx = [0u8; 8];
    let (n, _fds) = k.ipc.recv_on_socket(b, &mut rx, 0).unwrap();
    assert_eq!(n, 0, "peer observes EOF after sock_shutdown(RDWR)");
}

#[test]
fn sock_shutdown_rd_alone_marks_read_side_shutdown() {
    // Half-close is now first-class: RD sets shutdown_read = true
    // (shutdown_write left false), and the fd's .closed flag stays
    // false — the fd remains open for a future fd_close.
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "shutter", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    k.ipc.socket_mut(a).unwrap().state = SocketState::Connected;
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SOCK_SHUTDOWN,
        flags: 0,
        request_id: 981,
        args: sock_shutdown_args(3, abi::wasi::sdflags::RD as u32),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let sock = k.ipc.socket_mut(a).unwrap();
    assert!(sock.shutdown_read);
    assert!(!sock.shutdown_write);
    assert!(!sock.closed, "shutdown_socket does NOT set .closed");
}

#[test]
fn sock_shutdown_wr_alone_marks_write_side_shutdown() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "shutter", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    k.ipc.socket_mut(a).unwrap().state = SocketState::Connected;
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SOCK_SHUTDOWN,
        flags: 0,
        request_id: 982,
        args: sock_shutdown_args(3, abi::wasi::sdflags::WR as u32),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let sock = k.ipc.socket_mut(a).unwrap();
    assert!(!sock.shutdown_read);
    assert!(sock.shutdown_write);
    assert!(!sock.closed);
}

#[test]
fn sock_shutdown_rdwr_sets_both_flags_without_closing() {
    // RD | WR now flips both shutdown flags but leaves the socket's
    // `closed` flag false — that's what distinguishes
    // `shutdown_socket` from `close_socket`. A subsequent fd_close
    // is still required to reap the kernel-side Socket entry.
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "shutter", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    k.ipc.socket_mut(a).unwrap().state = SocketState::Connected;
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 16];

    let how = (abi::wasi::sdflags::RD | abi::wasi::sdflags::WR) as u32;
    let req = Request {
        opcode: op_wasi::SOCK_SHUTDOWN,
        flags: 0,
        request_id: 1050,
        args: sock_shutdown_args(3, how),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let sock = k.ipc.socket_mut(a).unwrap();
    assert!(sock.shutdown_read);
    assert!(sock.shutdown_write);
    assert!(!sock.closed);
}

#[test]
fn sock_shutdown_read_makes_recv_return_eof() {
    // Even with bytes in rx_buf, a read-shut socket's recv returns
    // (0, []) — matches POSIX shutdown(SHUT_RD), which discards any
    // pending incoming data.
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "shutter", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    // Push some bytes into a's rx buffer, then shutdown RD on a.
    k.ipc.send_on_socket(b, b"payload", Vec::new()).unwrap();
    assert_eq!(k.ipc.socket_mut(a).unwrap().rx_len(), 7);
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SOCK_SHUTDOWN,
        flags: 0,
        request_id: 1051,
        args: sock_shutdown_args(3, abi::wasi::sdflags::RD as u32),
        heap_ptr: 0,
        heap_len: 0,
    };
    assert_eq!(dispatch(&mut k, pid, &req, &mut heap).status, 0);

    // Now recv on a should return (0, []) EOF — rx_buf bytes are
    // discarded by the shutdown.
    let mut rx = [0u8; 16];
    let (n, _fds) = k.ipc.recv_on_socket(a, &mut rx, 0).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn sock_shutdown_read_makes_peer_send_return_pipe_broken() {
    // After A.shutdown(RD), B's subsequent send to A returns
    // PipeBroken (→ EINVAL via NotSupportedOnFd → EINVAL in the
    // current mapping) because A has refused further reads.
    use kernel::ipc::{IpcError, SocketState, SocketType};
    let mut k = make_kernel();
    let _pid = make_running_proc(&mut k, "shutter", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.ipc.shutdown_socket(a, true, false).unwrap();

    let send_err = k.ipc.send_on_socket(b, b"data", Vec::new()).unwrap_err();
    assert_eq!(send_err, IpcError::PipeBroken);
}

#[test]
fn sock_shutdown_write_makes_send_return_pipe_broken() {
    // After A.shutdown(WR), A's own send returns PipeBroken.
    use kernel::ipc::{IpcError, SocketState, SocketType};
    let mut k = make_kernel();
    let _pid = make_running_proc(&mut k, "shutter", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.ipc.shutdown_socket(a, false, true).unwrap();

    let send_err = k.ipc.send_on_socket(a, b"data", Vec::new()).unwrap_err();
    assert_eq!(send_err, IpcError::PipeBroken);
}

#[test]
fn sock_shutdown_write_makes_peer_recv_observe_eof_when_rx_drains() {
    // After A.shutdown(WR), B's recv sees EOF (0, []) once its
    // rx_buf drains. Before the drain, still-buffered bytes are
    // readable normally — only new reads past the buffer return EOF.
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let _pid = make_running_proc(&mut k, "shutter", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    // Send some bytes to B's rx_buf first.
    k.ipc.send_on_socket(a, b"xy", Vec::new()).unwrap();
    k.ipc.shutdown_socket(a, false, true).unwrap();

    // B drains its rx_buf.
    let mut rx = [0u8; 4];
    let (n, _) = k.ipc.recv_on_socket(b, &mut rx, 0).unwrap();
    assert_eq!(n, 2);
    assert_eq!(&rx[..2], b"xy");

    // Next recv sees EOF (A's shutdown_write counts as closed for
    // recv's EOF check).
    let (n2, _) = k.ipc.recv_on_socket(b, &mut rx, 0).unwrap();
    assert_eq!(n2, 0);
}

#[test]
fn sock_shutdown_with_zero_how_returns_einval() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "shutter", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    k.ipc.socket_mut(a).unwrap().state = SocketState::Connected;
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::SOCK_SHUTDOWN,
        flags: 0,
        request_id: 983,
        args: sock_shutdown_args(3, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn sock_shutdown_with_reserved_bits_returns_einval() {
    // Any bits beyond RD | WR (0x3) are not defined by WASI and
    // the handler rejects them rather than silently discarding —
    // v1 is strict about input validation for unused bitfields.
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "shutter", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    k.ipc.socket_mut(a).unwrap().state = SocketState::Connected;
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    let mut heap = vec![0u8; 16];

    // Bit 0x80 is undefined; RD | WR = 0x3, so 0x83 has the
    // full-close pair set but also an undefined bit.
    let req = Request {
        opcode: op_wasi::SOCK_SHUTDOWN,
        flags: 0,
        request_id: 984,
        args: sock_shutdown_args(3, 0x83),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn sock_shutdown_on_char_device_fd_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "cdev_shutter", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let how = (abi::wasi::sdflags::RD | abi::wasi::sdflags::WR) as u32;
    let req = Request {
        opcode: op_wasi::SOCK_SHUTDOWN,
        flags: 0,
        request_id: 985,
        args: sock_shutdown_args(1, how),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn sock_shutdown_on_unopened_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "ghost_shutter", 0);
    let mut heap = vec![0u8; 16];

    let how = (abi::wasi::sdflags::RD | abi::wasi::sdflags::WR) as u32;
    let req = Request {
        opcode: op_wasi::SOCK_SHUTDOWN,
        flags: 0,
        request_id: 986,
        args: sock_shutdown_args(99, how),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

// ---- path_link -----------------------------------------------------
//
// Opcode 0x0043. Create a new directory entry (hardlink) pointing at
// an existing inode. Wire:
//
//   args[0..4]   = old_fd       (u32; ignored in v1 — no preopens)
//   args[4..8]   = old_flags    (u32; lookup flags — ignored in v1,
//                                 symlinks aren't followed by the path
//                                 resolver regardless)
//   args[8..12]  = new_fd       (u32; ignored in v1)
//   args[12..16] = old_len      (u32; split point in heap window —
//                                 heap[0..old_len] is the old / source
//                                 UTF-8 path, heap[old_len..heap_len]
//                                 is the new / hardlink-target path)
//
// Mirrors PATH_RENAME's two-heap-strings packing but with old_len at
// [12..16] because path_link has three integer-shaped args in the
// WASI signature before the path-length fields.
//
// Semantics: new name appears as an alias of the same inode — writes
// through one name are visible through the other, and the source's
// nlink increments. Vfs::link detects cross-mount and returns
// NotSupported → ENOTSUP; within-mount calls dispatch to
// Filesystem::link on the owning mount. tmpfs overrides with the real
// impl; devfs/procfs inherit the trait default (ReadOnly → EROFS,
// matching Filesystem::set_times' default).

fn path_link_args(old_fd: u32, old_flags: u32, new_fd: u32, old_len: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&old_fd.to_le_bytes());
    args[4..8].copy_from_slice(&old_flags.to_le_bytes());
    args[8..12].copy_from_slice(&new_fd.to_le_bytes());
    args[12..16].copy_from_slice(&old_len.to_le_bytes());
    args
}

fn path_link_heap(old_path: &[u8], new_path: &[u8]) -> Vec<u8> {
    let mut heap = vec![0u8; old_path.len() + new_path.len()];
    heap[..old_path.len()].copy_from_slice(old_path);
    heap[old_path.len()..].copy_from_slice(new_path);
    heap
}

#[test]
fn path_link_creates_alias_pointing_at_same_ino() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    let src_ino = k.vfs.create("/src.txt", 0o644).expect("create");
    k.vfs.write("/src.txt", 0, b"hello").expect("write");

    let old_path = b"/src.txt";
    let new_path = b"/hlnk.txt";
    let mut heap = path_link_heap(old_path, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 990,
        args: path_link_args(0, 0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);

    let src_stat = k.vfs.stat("/src.txt").unwrap();
    let new_stat = k.vfs.stat("/hlnk.txt").unwrap();
    assert_eq!(src_stat.ino, new_stat.ino);
    assert_eq!(src_stat.ino, src_ino);
    assert_eq!(src_stat.size, 5);
    assert_eq!(new_stat.size, 5);
}

#[test]
fn path_link_both_names_share_file_content() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    k.vfs.create("/original", 0o644).expect("create");
    k.vfs.write("/original", 0, b"ABC").expect("write");

    let old_path = b"/original";
    let new_path = b"/alias";
    let mut heap = path_link_heap(old_path, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 991,
        args: path_link_args(0, 0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    assert_eq!(dispatch(&mut k, pid, &req, &mut heap).status, 0);

    // Append via the new name; read via the old name — shared content.
    k.vfs.write("/alias", 3, b"DEF").expect("write alias");
    let mut buf = [0u8; 6];
    let n = k.vfs.read("/original", 0, &mut buf).unwrap();
    assert_eq!(n, 6);
    assert_eq!(&buf, b"ABCDEF");
}

#[test]
fn path_link_increments_nlink() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    k.vfs.create("/f", 0o644).expect("create");
    assert_eq!(k.vfs.stat("/f").unwrap().nlink, 1);

    let old_path = b"/f";
    let new_path = b"/g";
    let mut heap = path_link_heap(old_path, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 992,
        args: path_link_args(0, 0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    assert_eq!(dispatch(&mut k, pid, &req, &mut heap).status, 0);
    assert_eq!(k.vfs.stat("/f").unwrap().nlink, 2);
    assert_eq!(k.vfs.stat("/g").unwrap().nlink, 2);
}

#[test]
fn path_link_from_missing_returns_enoent() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    let old_path = b"/nope";
    let new_path = b"/also_nope";
    let mut heap = path_link_heap(old_path, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 993,
        args: path_link_args(0, 0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOENT);
}

#[test]
fn path_link_to_existing_target_returns_eexist() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    k.vfs.create("/a", 0o644).expect("create a");
    k.vfs.create("/b", 0o644).expect("create b");

    let old_path = b"/a";
    let new_path = b"/b";
    let mut heap = path_link_heap(old_path, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 994,
        args: path_link_args(0, 0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EEXIST);
}

#[test]
fn path_link_cross_mount_returns_enotsup() {
    // Vfs::link rejects cross-mount links with NotSupported → ENOTSUP,
    // mirroring Vfs::rename's cross-mount guard. A hardlink can't span
    // filesystems because inode numbers are per-mount.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    k.vfs.create("/x.txt", 0o644).expect("create");
    let old_path = b"/x.txt";
    let new_path = b"/dev/x.txt";
    let mut heap = path_link_heap(old_path, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 995,
        args: path_link_args(0, 0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTSUP);
}

#[test]
fn path_link_on_devfs_returns_erofs() {
    // Within-devfs link: devfs inherits the trait default (ReadOnly →
    // EROFS), same as Filesystem::set_times does for devfs.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    let old_path = b"/dev/null";
    let new_path = b"/dev/nullx";
    let mut heap = path_link_heap(old_path, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 996,
        args: path_link_args(0, 0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EROFS);
}

#[test]
fn path_link_with_invalid_utf8_old_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    let mut heap = vec![0u8; 64];
    // Invalid UTF-8 in the old-path region, valid (but unused) new path.
    heap[0] = 0xff;
    heap[1] = 0xfe;
    heap[2..2 + b"/b".len()].copy_from_slice(b"/b");

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 997,
        args: path_link_args(0, 0, 0, 2),
        heap_ptr: 0,
        heap_len: (2 + b"/b".len()) as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_link_with_invalid_utf8_new_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    let mut heap = vec![0u8; 64];
    let old_path = b"/a";
    heap[..old_path.len()].copy_from_slice(old_path);
    heap[old_path.len()] = 0xff;
    heap[old_path.len() + 1] = 0xfe;

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 998,
        args: path_link_args(0, 0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len: (old_path.len() + 2) as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_link_with_zero_old_len_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    let new_path = b"/nowhere";
    let mut heap = vec![0u8; 32];
    heap[..new_path.len()].copy_from_slice(new_path);

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 999,
        args: path_link_args(0, 0, 0, 0),
        heap_ptr: 0,
        heap_len: new_path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_link_with_old_len_past_heap_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    let mut heap = vec![0u8; 32];
    heap[..4].copy_from_slice(b"/abc");

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 1000,
        args: path_link_args(0, 0, 0, 999),
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

// ---- path_symlink -------------------------------------------------
//
// Opcode 0x0048. Create a symlink at a new path, holding a target
// string. Wire:
//
//   args[0..4] = old_len (u32; split point in heap — heap[0..old_len]
//                is the target string that the symlink holds,
//                heap[old_len..heap_len] is the new path to create)
//   heap[0..old_len]        = UTF-8 target string (arbitrary; may
//                             not exist — dangling symlinks are fine)
//   heap[old_len..heap_len] = UTF-8 new path (the path of the symlink
//                             the caller is creating)
//
// Simpler packing than PATH_LINK because WASI path_symlink has only
// one integer-shaped arg (new_fd) in its signature before the path
// lengths, and v1 ignores it anyway.
//
// Semantics: creates a NodeType::SymLink vnode whose "content" is the
// target string rather than regular-file bytes. Vfs::resolve does NOT
// follow symlinks in v1 — `stat()` on the symlink path returns the
// symlink's own metadata (ty = SymLink, size = target byte length);
// opening the symlink path via path_open still returns the symlink
// itself; walking a path that crosses a symlink component yields
// whatever the filesystem returns without dereferencing. A future
// slice can teach Vfs::resolve to follow symlinks for callers that
// want it.

fn path_symlink_args(old_len: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&old_len.to_le_bytes());
    args
}

fn path_symlink_heap(target: &[u8], new_path: &[u8]) -> Vec<u8> {
    let mut heap = vec![0u8; target.len() + new_path.len()];
    heap[..target.len()].copy_from_slice(target);
    heap[target.len()..].copy_from_slice(new_path);
    heap
}

#[test]
fn path_symlink_creates_symlink_stats_as_filetype_symlink() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "symlinker", 0);

    let target = b"/some/target";
    let new_path = b"/mylink";
    let mut heap = path_symlink_heap(target, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_SYMLINK,
        flags: 0,
        request_id: 1010,
        args: path_symlink_args(target.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);

    let st = k.vfs.stat("/mylink").expect("symlink stats");
    assert!(st.ty.is_symlink(), "stat reports NodeType::SymLink");
    assert_eq!(st.size, target.len() as u64, "size = target byte length");
}

#[test]
fn path_symlink_dangling_target_is_allowed() {
    // A symlink's target need not exist — dangling links are first-
    // class in POSIX/WASI.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "symlinker", 0);

    let target = b"/no/such/path";
    let new_path = b"/dangling";
    let mut heap = path_symlink_heap(target, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_SYMLINK,
        flags: 0,
        request_id: 1011,
        args: path_symlink_args(target.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    assert_eq!(dispatch(&mut k, pid, &req, &mut heap).status, 0);
    assert!(k.vfs.stat("/dangling").unwrap().ty.is_symlink());
}

#[test]
fn path_symlink_with_existing_link_path_returns_eexist() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "symlinker", 0);
    k.vfs.create("/exists", 0o644).expect("create");

    let target = b"/target";
    let new_path = b"/exists";
    let mut heap = path_symlink_heap(target, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_SYMLINK,
        flags: 0,
        request_id: 1012,
        args: path_symlink_args(target.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EEXIST);
}

#[test]
fn path_symlink_with_missing_parent_returns_enoent() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "symlinker", 0);

    let target = b"/target";
    let new_path = b"/no/such/dir/link";
    let mut heap = path_symlink_heap(target, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_SYMLINK,
        flags: 0,
        request_id: 1013,
        args: path_symlink_args(target.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOENT);
}

#[test]
fn path_symlink_on_devfs_returns_enotsup() {
    // devfs inherits the trait default (NotSupported → ENOTSUP).
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "symlinker", 0);

    let target = b"/target";
    let new_path = b"/dev/mylink";
    let mut heap = path_symlink_heap(target, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_SYMLINK,
        flags: 0,
        request_id: 1014,
        args: path_symlink_args(target.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTSUP);
}

#[test]
fn path_symlink_with_invalid_utf8_target_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "symlinker", 0);
    let mut heap = vec![0u8; 64];
    // Target bytes are invalid UTF-8; the new-path half is valid.
    heap[0] = 0xff;
    heap[1] = 0xfe;
    let new_path = b"/link";
    heap[2..2 + new_path.len()].copy_from_slice(new_path);

    let req = Request {
        opcode: op_wasi::PATH_SYMLINK,
        flags: 0,
        request_id: 1015,
        args: path_symlink_args(2),
        heap_ptr: 0,
        heap_len: (2 + new_path.len()) as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_symlink_with_invalid_utf8_newpath_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "symlinker", 0);
    let target = b"/t";
    let mut heap = vec![0u8; 64];
    heap[..target.len()].copy_from_slice(target);
    heap[target.len()] = 0xff;
    heap[target.len() + 1] = 0xfe;

    let req = Request {
        opcode: op_wasi::PATH_SYMLINK,
        flags: 0,
        request_id: 1016,
        args: path_symlink_args(target.len() as u32),
        heap_ptr: 0,
        heap_len: (target.len() + 2) as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_symlink_with_zero_old_len_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "symlinker", 0);
    let new_path = b"/anywhere";
    let mut heap = vec![0u8; 32];
    heap[..new_path.len()].copy_from_slice(new_path);

    let req = Request {
        opcode: op_wasi::PATH_SYMLINK,
        flags: 0,
        request_id: 1017,
        args: path_symlink_args(0),
        heap_ptr: 0,
        heap_len: new_path.len() as u32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_symlink_with_old_len_past_heap_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "symlinker", 0);
    let mut heap = vec![0u8; 32];
    heap[..4].copy_from_slice(b"/xxx");

    let req = Request {
        opcode: op_wasi::PATH_SYMLINK,
        flags: 0,
        request_id: 1018,
        args: path_symlink_args(999),
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

// ---- IPC_PIPE extension opcode -------------------------------------
//
// 0x1007. Create a pipe pair and install both ends in the caller's
// fd table. Wire:
//
//   args[0..16]            = all zero (no inline args)
//   heap[0..8]             = output buffer for (read_fd, write_fd)
//                             — kernel writes two u32s little-endian
//   heap_len               = 8 (fixed; a shorter heap_len rejects
//                             with EINVAL)
// Response:
//   value                  = 0
//   extra_len              = 8 (bytes-written convention — mirrors
//                             fd_read's surface)
//
// After a successful call:
//   * read_fd is an FdObject::PipeRead handle.
//   * write_fd is an FdObject::PipeWrite handle.
//   * Bytes written to write_fd are readable via read_fd via the
//     fd_read / fd_write pipe arms landed in fbddb91.
//   * Closing read_fd and then writing to write_fd returns EPIPE
//     (via the ca5bb8a PipeBroken → EPIPE mapping).
//
// Mirrors POSIX `pipe(2)` and WASI's `fd_pipe` style: the caller
// owns both ends, can dup-inherit either into a child via
// PROC_SPAWN, and closes them independently.

#[test]
fn ipc_pipe_allocates_two_fresh_fds_and_installs_read_and_write_pair() {
    use kernel::fd::FdObject;
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "piper", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_ext::IPC_PIPE,
        flags: 0,
        request_id: 1100,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 8,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    assert_eq!(resp.extra_len, 8);

    let read_fd = u32::from_le_bytes(heap[0..4].try_into().unwrap());
    let write_fd = u32::from_le_bytes(heap[4..8].try_into().unwrap());
    assert_ne!(read_fd, write_fd, "two distinct fds");

    // Verify the installed FdObject variants.
    let table = k.fds(pid).unwrap();
    let read_entry = table.get(read_fd).expect("read_fd installed");
    let write_entry = table.get(write_fd).expect("write_fd installed");
    assert!(matches!(read_entry.object, FdObject::PipeRead(_)));
    assert!(matches!(write_entry.object, FdObject::PipeWrite(_)));

    // Both fds reference the same underlying pipe id.
    let read_pipe_id = match read_entry.object {
        FdObject::PipeRead(id) => id,
        _ => unreachable!(),
    };
    let write_pipe_id = match write_entry.object {
        FdObject::PipeWrite(id) => id,
        _ => unreachable!(),
    };
    assert_eq!(read_pipe_id, write_pipe_id, "both ends share the same pipe");
}

#[test]
fn ipc_pipe_round_trip_write_then_read_via_fd_syscalls() {
    // End-to-end: IPC_PIPE to create, FD_WRITE via the write fd,
    // FD_READ via the read fd. Confirms the fbddb91 pipe fd arms
    // and this slice compose cleanly.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "piper", 0);
    let mut heap = vec![0u8; 64];

    // Create the pipe pair.
    let create = Request {
        opcode: op_ext::IPC_PIPE,
        flags: 0,
        request_id: 1101,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 8,
    };
    let resp = dispatch(&mut k, pid, &create, &mut heap);
    assert_eq!(resp.status, 0);
    let read_fd = u32::from_le_bytes(heap[0..4].try_into().unwrap());
    let write_fd = u32::from_le_bytes(heap[4..8].try_into().unwrap());

    // Write "ping" through write_fd.
    heap[..4].copy_from_slice(b"ping");
    let write = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 1102,
        args: u32_args(write_fd),
        heap_ptr: 0,
        heap_len: 4,
    };
    let w = dispatch(&mut k, pid, &write, &mut heap);
    assert_eq!(w.status, 0);
    assert_eq!(w.value, 4);

    // Clear heap, then read through read_fd.
    for b in heap[..16].iter_mut() {
        *b = 0;
    }
    let read = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 1103,
        args: u32_args(read_fd),
        heap_ptr: 0,
        heap_len: 16,
    };
    let r = dispatch(&mut k, pid, &read, &mut heap);
    assert_eq!(r.status, 0);
    assert_eq!(r.value, 4);
    assert_eq!(&heap[..4], b"ping");
}

#[test]
fn ipc_pipe_read_fd_close_then_write_returns_epipe() {
    // Drop the read end via fd_close; subsequent write surfaces
    // EPIPE via the PipeBroken → EPIPE mapping.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "piper", 0);
    let mut heap = vec![0u8; 64];

    let create = Request {
        opcode: op_ext::IPC_PIPE,
        flags: 0,
        request_id: 1104,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 8,
    };
    assert_eq!(dispatch(&mut k, pid, &create, &mut heap).status, 0);
    let read_fd = u32::from_le_bytes(heap[0..4].try_into().unwrap());
    let write_fd = u32::from_le_bytes(heap[4..8].try_into().unwrap());

    // Close the read end.
    let close = Request {
        opcode: op_wasi::FD_CLOSE,
        flags: 0,
        request_id: 1105,
        args: u32_args(read_fd),
        heap_ptr: 0,
        heap_len: 0,
    };
    assert_eq!(dispatch(&mut k, pid, &close, &mut heap).status, 0);

    // Write now yields EPIPE.
    heap[..2].copy_from_slice(b"no");
    let write = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 1106,
        args: u32_args(write_fd),
        heap_ptr: 0,
        heap_len: 2,
    };
    let w = dispatch(&mut k, pid, &write, &mut heap);
    assert_eq!(w.status, -errno::EPIPE);
}

#[test]
fn ipc_pipe_write_fd_close_then_read_returns_zero_eof() {
    // Drop the write end; the read end returns (0, []) EOF once
    // the buffer drains.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "piper", 0);
    let mut heap = vec![0u8; 64];

    let create = Request {
        opcode: op_ext::IPC_PIPE,
        flags: 0,
        request_id: 1107,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 8,
    };
    assert_eq!(dispatch(&mut k, pid, &create, &mut heap).status, 0);
    let read_fd = u32::from_le_bytes(heap[0..4].try_into().unwrap());
    let write_fd = u32::from_le_bytes(heap[4..8].try_into().unwrap());

    // Close the write end.
    let close = Request {
        opcode: op_wasi::FD_CLOSE,
        flags: 0,
        request_id: 1108,
        args: u32_args(write_fd),
        heap_ptr: 0,
        heap_len: 0,
    };
    assert_eq!(dispatch(&mut k, pid, &close, &mut heap).status, 0);

    // Read on an empty pipe with no writer → EOF (0, []).
    let read = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 1109,
        args: u32_args(read_fd),
        heap_ptr: 0,
        heap_len: 16,
    };
    let r = dispatch(&mut k, pid, &read, &mut heap);
    assert_eq!(r.status, 0);
    assert_eq!(r.value, 0);
}

#[test]
fn ipc_pipe_with_short_heap_returns_einval() {
    // heap_len < 8 can't hold the (read_fd, write_fd) pair; reject
    // with EINVAL before any fd allocation happens so a caller with
    // a malformed shim doesn't leak half-installed pipes.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "piper", 0);
    let mut heap = vec![0u8; 4]; // too short

    let req = Request {
        opcode: op_ext::IPC_PIPE,
        flags: 0,
        request_id: 1110,
        args: [0u8; 16],
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);

    // No fds were installed — fd table open count is still zero.
    let table = k.fds(pid).unwrap();
    assert_eq!(table.open_count(), 0);
}

// ---- fd_read / fd_write on pipe fds --------------------------------
//
// Pre-slice, Kernel::fd_read on a PipeRead fd and Kernel::fd_write on
// a PipeWrite fd both returned NotSupportedOnFd → EINVAL
// unconditionally, even though the ipc::pipe module had been
// providing try_read / try_write primitives since slice 0. Post-
// slice, those two variants thread through to `ipc::Pipe::try_read`
// / `ipc::Pipe::try_write` directly; the reverse-direction cases
// (read on PipeWrite, write on PipeRead) still return EINVAL.
//
// Conversion of PipeReadResult / PipeWriteResult to syscall returns:
//
//   PipeReadResult::Read(n)       → Ok(n)
//   PipeReadResult::Eof           → Ok(0)                     (EOF convention)
//   PipeReadResult::WouldBlock    → Err(KernelError::WouldBlock)  → EAGAIN
//   PipeWriteResult::Wrote(n)     → Ok(n)
//   PipeWriteResult::Broken       → Err(KernelError::PipeBroken)  → EPIPE
//   PipeWriteResult::WouldBlock   → Err(KernelError::WouldBlock)  → EAGAIN

#[test]
fn fd_read_on_pipe_read_returns_bytes_written_by_other_side() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);
    let pipe_id = k.ipc.create_pipe();
    // Preload the pipe with content via the IpcTable helper.
    k.ipc.pipe_mut(pipe_id).unwrap().try_write(b"hello");
    k.install_fd(pid, 3, FdObject::PipeRead(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 1080,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 5);
    assert_eq!(&heap[..5], b"hello");
}

#[test]
fn fd_read_on_pipe_read_returns_zero_when_writer_closed_and_buffer_empty() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);
    let pipe_id = k.ipc.create_pipe();
    // Drop the writer side; the buffer is empty and writer_closed.
    k.ipc.drop_pipe_writer(pipe_id).unwrap();
    k.install_fd(pid, 3, FdObject::PipeRead(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 1081,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0, "POSIX EOF convention");
}

#[test]
fn fd_read_on_empty_pipe_with_writer_open_returns_eagain() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);
    let pipe_id = k.ipc.create_pipe();
    k.install_fd(pid, 3, FdObject::PipeRead(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 1082,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EAGAIN);
}

#[test]
fn fd_write_on_pipe_write_buffers_bytes_for_reader() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "writer", 0);
    let pipe_id = k.ipc.create_pipe();
    k.install_fd(pid, 3, FdObject::PipeWrite(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];
    heap[..5].copy_from_slice(b"world");

    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 1083,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 5,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 5);

    // The reader side now has those bytes in its buffer.
    let mut buf = [0u8; 8];
    let res = k.ipc.pipe_mut(pipe_id).unwrap().try_read(&mut buf);
    assert_eq!(res, kernel::ipc::PipeReadResult::Read(5));
    assert_eq!(&buf[..5], b"world");
}

#[test]
fn fd_write_on_pipe_write_returns_epipe_when_reader_closed() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "writer", 0);
    let pipe_id = k.ipc.create_pipe();
    // Drop the reader side first — subsequent writes are broken.
    k.ipc.drop_pipe_reader(pipe_id).unwrap();
    k.install_fd(pid, 3, FdObject::PipeWrite(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];
    heap[..2].copy_from_slice(b"hi");

    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 1084,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EPIPE);
}

#[test]
fn fd_write_on_pipe_write_returns_eagain_when_buffer_full() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "writer", 0);
    let pipe_id = k.ipc.create_pipe();
    // Fill the pipe's 64 KiB buffer via direct IpcTable calls.
    let chunk = vec![0u8; 64 * 1024];
    let res = k.ipc.pipe_mut(pipe_id).unwrap().try_write(&chunk);
    assert_eq!(res, kernel::ipc::PipeWriteResult::Wrote(64 * 1024));
    k.install_fd(pid, 3, FdObject::PipeWrite(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];
    heap[..1].copy_from_slice(b"x");

    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 1085,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 1,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EAGAIN);
}

#[test]
fn fd_read_on_pipe_write_fd_returns_einval() {
    // Reading from the write end is still a wrong-direction op;
    // NotSupportedOnFd → EINVAL, distinct from the read-end paths.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "misreader", 0);
    let pipe_id = k.ipc.create_pipe();
    k.install_fd(pid, 3, FdObject::PipeWrite(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 1086,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 8,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_write_on_pipe_read_fd_returns_einval() {
    // Writing to the read end is still a wrong-direction op.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "miswriter", 0);
    let pipe_id = k.ipc.create_pipe();
    k.install_fd(pid, 3, FdObject::PipeRead(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];
    heap[..2].copy_from_slice(b"no");

    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 1087,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn pipe_round_trip_via_fd_read_and_fd_write_syscalls() {
    // End-to-end: one process holds both ends, writes via fd, reads
    // back via the paired fd.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "round_tripper", 0);
    let pipe_id = k.ipc.create_pipe();
    k.install_fd(pid, 3, FdObject::PipeRead(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    k.install_fd(pid, 4, FdObject::PipeWrite(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];

    // Write "pipe!" via fd 4.
    heap[..5].copy_from_slice(b"pipe!");
    let write_req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 1088,
        args: u32_args(4),
        heap_ptr: 0,
        heap_len: 5,
    };
    let w = dispatch(&mut k, pid, &write_req, &mut heap);
    assert_eq!(w.status, 0);
    assert_eq!(w.value, 5);

    // Read via fd 3.
    for b in heap[..5].iter_mut() {
        *b = 0;
    }
    let read_req = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 1089,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 16,
    };
    let r = dispatch(&mut k, pid, &read_req, &mut heap);
    assert_eq!(r.status, 0);
    assert_eq!(r.value, 5);
    assert_eq!(&heap[..5], b"pipe!");
}

// ---- PipeBroken → EPIPE surface mapping ----------------------------
//
// Pre-slice, IpcError::PipeBroken mapped to KernelError::NotSupportedOnFd
// → EINVAL, which is POSIX-imprecise (POSIX specifies EPIPE for
// writes into a broken pipe / socket, distinct from the generic
// EINVAL). Post-slice, KernelError has its own PipeBroken variant
// mapped to EPIPE. Three canonical scenarios exercise the path
// through the syscall dispatcher: fd_write on a Socket whose peer
// has fully closed; fd_write on a Socket whose own write side has
// been shut down; sock_send on a Socket whose peer has shut down
// its read side.

#[test]
fn fd_write_on_socket_after_peer_close_returns_epipe() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sender", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    // Peer b closes, leaving a.peer pointing at a closed socket.
    k.ipc.close_socket(b).unwrap();

    let mut heap = vec![0u8; 16];
    heap[..2].copy_from_slice(b"hi");
    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 1070,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EPIPE);
}

#[test]
fn fd_write_on_write_shutdown_socket_returns_epipe() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sender", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    // Half-close the write side.
    k.ipc.shutdown_socket(a, false, true).unwrap();

    let mut heap = vec![0u8; 16];
    heap[..2].copy_from_slice(b"hi");
    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 1071,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EPIPE);
}

#[test]
fn sock_send_on_socket_with_peer_read_shutdown_returns_epipe() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sender", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    // Peer b shuts down its read side; a's send must EPIPE.
    k.ipc.shutdown_socket(b, true, false).unwrap();

    let mut heap = vec![0u8; 16];
    heap[..2].copy_from_slice(b"hi");
    let req = Request {
        opcode: op_wasi::SOCK_SEND,
        flags: 0,
        request_id: 1072,
        args: {
            let mut a = [0u8; 16];
            a[0..4].copy_from_slice(&3u32.to_le_bytes());
            a
        },
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EPIPE);
}

// ---- PipeBroken → SIGPIPE delivery ---------------------------------
//
// POSIX `write(2)` on a broken pipe or socket delivers SIGPIPE to the
// writer alongside the EPIPE errno. PMos v1 has no signal handlers
// yet, so SIGPIPE is posted to the caller's signal inbox and can be
// observed via `Kernel::pending_signals` / `Kernel::drain_signals`.
// The three paths that surface PipeBroken at the syscall layer — a
// pipe write after the reader has closed, an fd_write on a socket
// whose peer closed or whose own write-side shut down, and
// sock_send on a socket whose peer's read-side shut down — each
// post SIGPIPE. Successful writes do NOT touch the inbox.

#[test]
fn fd_write_on_broken_pipe_posts_sigpipe_alongside_epipe() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "writer", 0);
    let pipe_id = k.ipc.create_pipe();
    k.ipc.drop_pipe_reader(pipe_id).unwrap();
    k.install_fd(pid, 3, FdObject::PipeWrite(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];
    heap[..2].copy_from_slice(b"hi");

    assert_eq!(k.pending_signals(pid).unwrap(), 0);
    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 2100,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EPIPE);
    assert_eq!(k.pending_signals(pid).unwrap(), 1);
    assert_eq!(k.drain_signals(pid).unwrap(), alloc::vec![Signal::Pipe]);
}

#[test]
fn fd_write_on_broken_socket_posts_sigpipe_alongside_epipe() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sender", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    k.ipc.close_socket(b).unwrap();

    let mut heap = vec![0u8; 16];
    heap[..2].copy_from_slice(b"hi");
    assert_eq!(k.pending_signals(pid).unwrap(), 0);
    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 2101,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EPIPE);
    assert_eq!(k.drain_signals(pid).unwrap(), alloc::vec![Signal::Pipe]);
}

#[test]
fn sock_send_on_broken_socket_posts_sigpipe_alongside_epipe() {
    use kernel::ipc::{SocketState, SocketType};
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sender", 0);
    let a = k.ipc.create_socket(SocketType::Stream);
    let b = k.ipc.create_socket(SocketType::Stream);
    {
        let sa = k.ipc.socket_mut(a).unwrap();
        sa.state = SocketState::Connected;
        sa.peer = Some(b);
    }
    {
        let sb = k.ipc.socket_mut(b).unwrap();
        sb.state = SocketState::Connected;
        sb.peer = Some(a);
    }
    k.install_fd(pid, 3, FdObject::Socket(a.0), FdFlags::EMPTY).unwrap();
    // Peer shuts down its read side; a's send must EPIPE + SIGPIPE.
    k.ipc.shutdown_socket(b, true, false).unwrap();

    let mut heap = vec![0u8; 16];
    heap[..2].copy_from_slice(b"hi");
    assert_eq!(k.pending_signals(pid).unwrap(), 0);
    let req = Request {
        opcode: op_wasi::SOCK_SEND,
        flags: 0,
        request_id: 2102,
        args: {
            let mut a = [0u8; 16];
            a[0..4].copy_from_slice(&3u32.to_le_bytes());
            a
        },
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EPIPE);
    assert_eq!(k.drain_signals(pid).unwrap(), alloc::vec![Signal::Pipe]);
}

#[test]
fn successful_fd_write_on_pipe_does_not_queue_sigpipe() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "writer", 0);
    let pipe_id = k.ipc.create_pipe();
    // Both ends open; the write succeeds.
    k.install_fd(pid, 3, FdObject::PipeWrite(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];
    heap[..2].copy_from_slice(b"ok");

    let req = Request {
        opcode: op_wasi::FD_WRITE,
        flags: 0,
        request_id: 2103,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 2);
    assert_eq!(k.pending_signals(pid).unwrap(), 0);
}

#[test]
fn repeated_broken_writes_coalesce_to_single_sigpipe_entry() {
    // SignalInbox::post dedupes on signal identity, so even ten
    // consecutive broken writes post the caller's inbox only once.
    // Observable state after the loop: exactly one SIGPIPE pending.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "writer", 0);
    let pipe_id = k.ipc.create_pipe();
    k.ipc.drop_pipe_reader(pipe_id).unwrap();
    k.install_fd(pid, 3, FdObject::PipeWrite(pipe_id.0), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];
    heap[..1].copy_from_slice(b"x");

    for i in 0..10 {
        let req = Request {
            opcode: op_wasi::FD_WRITE,
            flags: 0,
            request_id: 2200 + i,
            args: u32_args(3),
            heap_ptr: 0,
            heap_len: 1,
        };
        let resp = dispatch(&mut k, pid, &req, &mut heap);
        assert_eq!(resp.status, -errno::EPIPE);
    }
    assert_eq!(k.drain_signals(pid).unwrap(), alloc::vec![Signal::Pipe]);
}

// ---- fd_read on SignalChannel --------------------------------------
//
// v1 wire format: each pending signal serialises as a u16 LE signum
// byte pair. An empty inbox surfaces WouldBlock → EAGAIN so callers
// polling fd 3 for signal delivery behave like any other readable fd
// with no data. A buffer smaller than 2 bytes returns 0 without
// consuming anything — there's no room for even one record. The
// drain-order matches POSIX FIFO with dedup coalescing.

#[test]
fn fd_read_on_signal_channel_with_empty_inbox_returns_eagain() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sigreader", 0);
    k.install_fd(pid, 3, FdObject::SignalChannel, FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 2300,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 8,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EAGAIN);
}

#[test]
fn fd_read_on_signal_channel_drains_pending_signals_as_u16_pairs() {
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    // Self-signal two catchable signals so both land in the inbox.
    k.proc_kill(init, init, Signal::Term).unwrap();
    k.proc_kill(init, init, Signal::Interrupt).unwrap();
    k.install_fd(init, 3, FdObject::SignalChannel, FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 2301,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 8,
    };
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 4);
    // Two signals, FIFO order: Term (15) first, then Interrupt (2).
    assert_eq!(
        u16::from_le_bytes([heap[0], heap[1]]),
        Signal::Term.number(),
    );
    assert_eq!(
        u16::from_le_bytes([heap[2], heap[3]]),
        Signal::Interrupt.number(),
    );
    // Inbox is empty post-read.
    assert_eq!(k.pending_signals(init).unwrap(), 0);
}

#[test]
fn fd_read_on_signal_channel_with_one_byte_buffer_returns_zero_preserving_queue() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sigreader", 0);
    k.proc_kill(pid, pid, Signal::Term).unwrap();
    k.install_fd(pid, 3, FdObject::SignalChannel, FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 2302,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 1, // Too small for one 2-byte record.
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    // Signal still queued — the kernel did not drop it.
    assert_eq!(k.pending_signals(pid).unwrap(), 1);
}

#[test]
fn fd_read_on_signal_channel_with_partial_buffer_drains_only_what_fits() {
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    // Three distinct catchable signals.
    k.proc_kill(init, init, Signal::Term).unwrap();
    k.proc_kill(init, init, Signal::Interrupt).unwrap();
    k.proc_kill(init, init, Signal::Pipe).unwrap();
    k.install_fd(init, 3, FdObject::SignalChannel, FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    // Buffer for exactly one record (2 bytes). One signal gets
    // drained; the other two stay queued.
    let req = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 2303,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 2,
    };
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 2);
    assert_eq!(
        u16::from_le_bytes([heap[0], heap[1]]),
        Signal::Term.number(),
    );
    assert_eq!(k.pending_signals(init).unwrap(), 2);

    // Second read drains the rest.
    heap.iter_mut().for_each(|b| *b = 0);
    let req2 = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 2304,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp2 = dispatch(&mut k, init, &req2, &mut heap);
    assert_eq!(resp2.status, 0);
    assert_eq!(resp2.value, 4);
    assert_eq!(
        u16::from_le_bytes([heap[0], heap[1]]),
        Signal::Interrupt.number(),
    );
    assert_eq!(
        u16::from_le_bytes([heap[2], heap[3]]),
        Signal::Pipe.number(),
    );
    assert_eq!(k.pending_signals(init).unwrap(), 0);
}

#[test]
fn fd_read_on_signal_channel_coalesces_repeated_same_signal() {
    // Ten SIGTERMs posted via proc_kill dedup to one entry via
    // SignalInbox::post. The fd_read path reads exactly one
    // 2-byte record, not ten.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "sigreader", 0);
    for _ in 0..10 {
        k.proc_kill(pid, pid, Signal::Term).unwrap();
    }
    k.install_fd(pid, 3, FdObject::SignalChannel, FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 2305,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 2);
    assert_eq!(
        u16::from_le_bytes([heap[0], heap[1]]),
        Signal::Term.number(),
    );
}

// ---- fd_prestat_dir_name --------------------------------------------
//
// Opcode 0x002C. WASI preopen-name lookup; companion to
// fd_prestat_get. v1 has no preopens, so the honest answer for every
// fd is EBADF (matching fd_prestat_get's semantic). Userland's
// libc-style preopen-discovery loops iterate fd 3/4/5 calling both
// fd_prestat_get and fd_prestat_dir_name until both return EBADF;
// pre-slice, v1 returned EBADF from get and ENOSYS from dir_name,
// which broke the loop. Post-slice, both agree on EBADF and the
// loop terminates cleanly.

#[test]
fn fd_prestat_dir_name_returns_ebadf_for_any_fd() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "libc_probe", 0);
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::FD_PRESTAT_DIR_NAME,
        flags: 0,
        request_id: 1030,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 64,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn fd_prestat_dir_name_returns_ebadf_for_unopened_fd() {
    // Same EBADF whether the fd is unallocated or allocated — v1 has
    // no preopens at all.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "libc_probe", 0);
    k.install_fd(pid, 3, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 64];

    let req = Request {
        opcode: op_wasi::FD_PRESTAT_DIR_NAME,
        flags: 0,
        request_id: 1031,
        args: u32_args(3),
        heap_ptr: 0,
        heap_len: 64,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn fd_prestat_dir_name_and_fd_prestat_get_agree_on_ebadf() {
    // Pin the consistency invariant: whatever fd_prestat_get returns
    // for a given fd, fd_prestat_dir_name returns the same (both
    // EBADF in v1, matching the libc preopen-discovery contract).
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "libc_probe", 0);
    let mut heap = vec![0u8; 64];

    let get_req = Request {
        opcode: op_wasi::FD_PRESTAT_GET,
        flags: 0,
        request_id: 1032,
        args: u32_args(4),
        heap_ptr: 0,
        heap_len: 0,
    };
    let get_resp = dispatch(&mut k, pid, &get_req, &mut heap);

    let name_req = Request {
        opcode: op_wasi::FD_PRESTAT_DIR_NAME,
        flags: 0,
        request_id: 1033,
        args: u32_args(4),
        heap_ptr: 0,
        heap_len: 64,
    };
    let name_resp = dispatch(&mut k, pid, &name_req, &mut heap);

    assert_eq!(get_resp.status, name_resp.status);
    assert_eq!(get_resp.status, -errno::EBADF);
}

// ---- path_readlink -------------------------------------------------
//
// Opcode 0x0045. Read a symlink's target bytes into a caller-supplied
// output buffer. Wire:
//
//   args[0..4] = dir_fd (u32; ignored in v1 — no preopens)
//   args[4..8] = path_len (u32; how many bytes at heap[0..] are the
//                UTF-8 input path; the rest of the heap is the
//                output buffer)
//   heap[0..path_len]        = UTF-8 input path (the symlink to read)
//   heap[path_len..heap_len] = output buffer capacity for the kernel
//                              to write the target bytes into
// Response:
//   value     = bytes actually written (0..=buf_cap)
//   extra_len = mirrors value (bytes-written convention)
//
// Truncates if the target exceeds the output buffer's capacity —
// matches POSIX readlink(2)'s documented behaviour. The kernel does
// NOT null-terminate; the caller uses the returned byte count.
//
// Semantics: looks up the ino at `path`, confirms it's a SymLink,
// copies the target bytes into the output buffer. Non-symlink targets
// (regular file, directory, char device) → EINVAL. Filesystems that
// don't know what a symlink is (devfs, procfs, opfs) inherit the
// trait default NotSupported → ENOTSUP.

fn path_readlink_args(dir_fd: u32, path_len: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&dir_fd.to_le_bytes());
    args[4..8].copy_from_slice(&path_len.to_le_bytes());
    args
}

/// Build a heap buffer of exactly `heap_size` bytes with the path
/// copied into the leading `path.len()` bytes. The rest is zero-
/// filled; the kernel writes target bytes back at heap[0..n] which
/// may overlap the path region (the kernel snapshots the path first).
fn path_readlink_heap(path: &[u8], heap_size: usize) -> Vec<u8> {
    let mut heap = vec![0u8; heap_size];
    let copy_len = core::cmp::min(path.len(), heap_size);
    heap[..copy_len].copy_from_slice(&path[..copy_len]);
    heap
}

#[test]
fn path_readlink_returns_target_bytes_for_a_symlink() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);
    // Create a symlink via the VFS directly (the path_symlink handler
    // is exercised by its own tests).
    k.vfs.symlink("/actual/target", "/lnk").expect("symlink");

    let path = b"/lnk";
    let mut heap = path_readlink_heap(path, 64);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_READLINK,
        flags: 0,
        request_id: 1020,
        args: path_readlink_args(0, path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let expected = b"/actual/target";
    assert_eq!(resp.value as usize, expected.len());
    assert_eq!(resp.extra_len as usize, expected.len());
    // Kernel writes target bytes at heap[0..n] (overwriting the path
    // region; path was snapshotted before the write).
    let n = resp.value as usize;
    assert_eq!(&heap[..n], expected);
}

#[test]
fn path_readlink_truncates_when_buffer_is_smaller_than_target() {
    // POSIX readlink(2) silently truncates when the buffer is too
    // small; the caller sees only value = heap_len and has no way to
    // distinguish from an exact-fit target. heap_len doubles as the
    // output buffer capacity.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);
    k.vfs.symlink("/a/long/target/string", "/lnk").expect("symlink");

    let path = b"/lnk";
    // heap_size = path length means the kernel has exactly path.len()
    // bytes of output space after snapshotting the path.
    let mut heap = path_readlink_heap(path, path.len());
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_READLINK,
        flags: 0,
        request_id: 1021,
        args: path_readlink_args(0, path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value as usize, path.len());
    assert_eq!(&heap[..path.len()], b"/a/l");
}

#[test]
fn path_readlink_on_regular_file_returns_einval() {
    // tmpfs.readlink returns InvalidArgument for non-SymLink nodes.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);
    k.vfs.create("/regular", 0o644).expect("create");

    let path = b"/regular";
    let mut heap = path_readlink_heap(path, 32);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_READLINK,
        flags: 0,
        request_id: 1022,
        args: path_readlink_args(0, path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_readlink_on_missing_path_returns_enoent() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);
    let path = b"/nope";
    let mut heap = path_readlink_heap(path, 32);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_READLINK,
        flags: 0,
        request_id: 1023,
        args: path_readlink_args(0, path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOENT);
}

#[test]
fn path_readlink_on_devfs_returns_enotsup() {
    // devfs inherits the NotSupported default — it doesn't know what
    // a symlink is.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);

    let path = b"/dev/console";
    let mut heap = path_readlink_heap(path, 32);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_READLINK,
        flags: 0,
        request_id: 1024,
        args: path_readlink_args(0, path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTSUP);
}

#[test]
fn path_readlink_with_zero_path_len_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);
    let mut heap = vec![0u8; 32];

    let req = Request {
        opcode: op_wasi::PATH_READLINK,
        flags: 0,
        request_id: 1025,
        args: path_readlink_args(0, 0),
        heap_ptr: 0,
        heap_len: 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_readlink_with_path_len_past_heap_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::PATH_READLINK,
        flags: 0,
        request_id: 1026,
        args: path_readlink_args(0, 999),
        heap_ptr: 0,
        heap_len: 16,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_readlink_with_invalid_utf8_path_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "reader", 0);
    let mut heap = vec![0u8; 32];
    heap[0] = 0xff;
    heap[1] = 0xfe;

    let req = Request {
        opcode: op_wasi::PATH_READLINK,
        flags: 0,
        request_id: 1027,
        args: path_readlink_args(0, 2),
        heap_ptr: 0,
        heap_len: 32,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn path_link_then_unlink_source_keeps_content_via_alias() {
    // With nlink tracking, unlinking one name of a hardlinked pair
    // must leave the content reachable via the surviving name — the
    // entire point of nlink. Validates tmpfs's unlink no longer drops
    // the inode while another reference exists.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "linker", 0);
    k.vfs.create("/primary", 0o644).expect("create");
    k.vfs.write("/primary", 0, b"zz").expect("write");

    let old_path = b"/primary";
    let new_path = b"/alias";
    let mut heap = path_link_heap(old_path, new_path);
    let heap_len = heap.len() as u32;

    let req = Request {
        opcode: op_wasi::PATH_LINK,
        flags: 0,
        request_id: 1001,
        args: path_link_args(0, 0, 0, old_path.len() as u32),
        heap_ptr: 0,
        heap_len,
    };
    assert_eq!(dispatch(&mut k, pid, &req, &mut heap).status, 0);

    k.vfs.unlink("/primary").expect("unlink primary");
    assert!(k.vfs.stat("/primary").is_err(), "primary name gone");

    let st = k.vfs.stat("/alias").expect("alias still resolves");
    assert_eq!(st.nlink, 1, "nlink decremented on unlink");
    let mut buf = [0u8; 2];
    let n = k.vfs.read("/alias", 0, &mut buf).unwrap();
    assert_eq!(n, 2);
    assert_eq!(&buf, b"zz");
}

// ---- fd_seek ----------------------------------------------------------
//
// WASI's seek combines three distinct file-position operations into a
// single opcode whose meaning depends on `whence` (Set=0, Cur=1, End=2).
// Per-fd, not per-vnode: two fds pointing at the same vnode have
// independent positions. v1 rejects fd_seek on every non-Vnode FdObject
// with EINVAL — whence has no meaning on a char device, socket, pipe,
// or signal channel. Seeking past EOF is allowed (POSIX/WASI both
// permit it; v1's tmpfs read returns 0 bytes from such an offset).
//
// Wire layout used below:
//   args[0..4]  = fd (u32)
//   args[4..8]  = whence (u32; only the low byte is meaningful)
//   args[8..16] = offset (i64; bit pattern of u64)
// Response:
//   value       = new absolute offset (u64 widened to i64; bit-exact)

/// Pack `(fd, whence, offset)` into the 16-byte inline args window.
fn fd_seek_args(fd: u32, whence: u32, offset: i64) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&fd.to_le_bytes());
    args[4..8].copy_from_slice(&whence.to_le_bytes());
    args[8..16].copy_from_slice(&offset.to_le_bytes());
    args
}

/// Set up a Vnode fd against a fresh tmpfs file containing `bytes`,
/// returning the (pid, fd) pair. The test bodies are short enough
/// that this helper is the bulk of the setup boilerplate.
fn make_proc_with_file_fd(
    k: &mut Kernel,
    name: &str,
    path: &str,
    bytes: &[u8],
) -> (abi::ext::Pid, u32) {
    let pid = make_running_proc(k, name, 0);
    k.vfs.create(path, 0o644).expect("create");
    let wrote = k.vfs.write(path, 0, bytes).expect("write");
    assert_eq!(wrote, bytes.len());
    let (mount_id, ino) = k.vfs.resolve(path).expect("resolve");
    k.install_fd(
        pid,
        10,
        FdObject::Vnode { mount_id, ino },
        FdFlags::EMPTY,
    )
    .unwrap();
    (pid, 10)
}

#[test]
fn fd_seek_set_advances_to_absolute_offset() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "seeker", "/s.txt", b"abcdefghij");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 730,
        args: fd_seek_args(fd, abi::wasi::Whence::Set as u32, 5),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.request_id, 730);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 5);
    assert_eq!(k.fds(pid).unwrap().offset(fd).unwrap(), 5);
}

#[test]
fn fd_seek_set_with_negative_offset_returns_einval() {
    // SeekSet asks for an absolute position; negative is meaningless.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "seeker", "/s.txt", b"abcdef");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 731,
        args: fd_seek_args(fd, abi::wasi::Whence::Set as u32, -1),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
    // Offset must be unchanged after a rejected seek.
    assert_eq!(k.fds(pid).unwrap().offset(fd).unwrap(), 0);
}

#[test]
fn fd_seek_cur_with_zero_returns_current_position() {
    // The `fd_tell` idiom: Cur 0 reports the current offset without
    // changing it.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "teller", "/s.txt", b"abcdefghij");
    // Pre-seed the offset so the test isn't checking 0 == 0.
    k.fds_mut(pid).unwrap().set_offset(fd, 4).unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 732,
        args: fd_seek_args(fd, abi::wasi::Whence::Cur as u32, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 4);
    assert_eq!(k.fds(pid).unwrap().offset(fd).unwrap(), 4);
}

#[test]
fn fd_seek_cur_with_negative_offset_back_past_zero_returns_einval() {
    // SeekCur from offset 2 with delta -5 would underflow to -3;
    // checked_add_signed catches it and the handler maps to EINVAL.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "rewinder", "/s.txt", b"abcdefghij");
    k.fds_mut(pid).unwrap().set_offset(fd, 2).unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 733,
        args: fd_seek_args(fd, abi::wasi::Whence::Cur as u32, -5),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
    assert_eq!(k.fds(pid).unwrap().offset(fd).unwrap(), 2);
}

#[test]
fn fd_seek_end_with_zero_returns_file_size() {
    // SeekEnd 0 lands at the end-of-file position, which is the file
    // size for a regular file.
    let bytes: &[u8] = b"abcdefghij";
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "ender", "/s.txt", bytes);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 734,
        args: fd_seek_args(fd, abi::wasi::Whence::End as u32, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, bytes.len() as i64);
    assert_eq!(k.fds(pid).unwrap().offset(fd).unwrap(), bytes.len() as u64);
}

#[test]
fn fd_seek_end_with_negative_offset_seeks_into_file() {
    // SeekEnd -3 on a 10-byte file lands at offset 7 (size + offset).
    let bytes: &[u8] = b"abcdefghij";
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "tail", "/s.txt", bytes);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 735,
        args: fd_seek_args(fd, abi::wasi::Whence::End as u32, -3),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, (bytes.len() - 3) as i64);
    assert_eq!(
        k.fds(pid).unwrap().offset(fd).unwrap(),
        (bytes.len() - 3) as u64,
    );
}

#[test]
fn fd_seek_with_invalid_whence_returns_einval() {
    // Whence values outside {Set=0, Cur=1, End=2} are EINVAL — the
    // handler is the single line of defence for the wire format.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "bogus", "/s.txt", b"abc");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 736,
        args: fd_seek_args(fd, 99, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
    assert_eq!(k.fds(pid).unwrap().offset(fd).unwrap(), 0);
}

#[test]
fn fd_seek_on_invalid_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "stranger", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 737,
        args: fd_seek_args(99, abi::wasi::Whence::Set as u32, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn fd_seek_on_char_device_fd_returns_einval() {
    // CharDevice (and every other non-Vnode FdObject) has no seek
    // semantics — whence on a console fd is meaningless, EINVAL.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "consoler", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 738,
        args: fd_seek_args(1, abi::wasi::Whence::Set as u32, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_seek_then_fd_read_reads_from_new_position() {
    // The integration test that pins the actual reason fd_seek
    // exists: a subsequent fd_read on the same fd starts from the
    // seek'd offset, not from zero.
    let mut k = make_kernel();
    let bytes: &[u8] = b"abcdefghij";
    let (pid, fd) = make_proc_with_file_fd(&mut k, "after_seek", "/s.txt", bytes);
    let mut heap = vec![0u8; 64];

    // Seek to offset 4.
    let seek = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 739,
        args: fd_seek_args(fd, abi::wasi::Whence::Set as u32, 4),
        heap_ptr: 0,
        heap_len: 0,
    };
    assert_eq!(dispatch(&mut k, pid, &seek, &mut heap).status, 0);

    // Read 4 bytes — should get "efgh", not "abcd".
    let read = Request {
        opcode: op_wasi::FD_READ,
        flags: 0,
        request_id: 740,
        args: u32_args(fd),
        heap_ptr: 0,
        heap_len: 4,
    };
    let resp = dispatch(&mut k, pid, &read, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 4);
    assert_eq!(&heap[..4], b"efgh");
}

// ---- fd_tell ----------------------------------------------------------
//
// The read-only sibling of `fd_seek`: report the current absolute file
// position of a seekable fd without changing it. Same per-fd semantics
// as fd_seek (two fds at the same vnode have independent positions),
// same non-Vnode rejection (EINVAL on every non-Vnode FdObject — a
// char device / socket / pipe has no meaningful position), same EBADF
// on an unopened fd. WASI libc lowers `ftell()` through this opcode;
// Rust's `File::stream_position` lowers through the equivalent
// `fd_seek(fd, 0, Cur, *)` fold. Either path reads the same
// `FdEntry.offset` field this handler reports.
//
// Wire layout used below:
//   args[0..4]  = fd (u32)
// Response:
//   value       = current absolute offset (u64 widened to i64; bit-exact)

#[test]
fn fd_tell_on_fresh_file_returns_initial_offset_zero() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "teller", "/t.txt", b"abcdefghij");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_TELL,
        flags: 0,
        request_id: 750,
        args: u32_args(fd),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.request_id, 750);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    // The read-only contract: fd_tell must not mutate the offset.
    assert_eq!(k.fds(pid).unwrap().offset(fd).unwrap(), 0);
}

#[test]
fn fd_tell_after_fd_seek_returns_the_sought_position() {
    // The integration test that pins the actual reason fd_tell exists
    // as a distinct opcode: fd_tell sees whatever fd_seek just wrote.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "after_seek", "/t.txt", b"abcdefghij");
    let mut heap = vec![0u8; 16];

    // Seek to offset 5 first.
    let seek = Request {
        opcode: op_wasi::FD_SEEK,
        flags: 0,
        request_id: 750,
        args: fd_seek_args(fd, abi::wasi::Whence::Set as u32, 5),
        heap_ptr: 0,
        heap_len: 0,
    };
    assert_eq!(dispatch(&mut k, pid, &seek, &mut heap).status, 0);

    // fd_tell must report the seek'd position.
    let tell = Request {
        opcode: op_wasi::FD_TELL,
        flags: 0,
        request_id: 751,
        args: u32_args(fd),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &tell, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 5);
    assert_eq!(k.fds(pid).unwrap().offset(fd).unwrap(), 5);
}

#[test]
fn fd_tell_on_char_device_fd_returns_einval() {
    // CharDevice (and every other non-Vnode FdObject) has no position
    // semantics — fd_tell on a console fd is meaningless, EINVAL.
    // Shares this rejection branch with fd_seek.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "consoler", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_TELL,
        flags: 0,
        request_id: 752,
        args: u32_args(1),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_tell_on_invalid_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "stranger", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_TELL,
        flags: 0,
        request_id: 753,
        args: u32_args(99),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

// ---- fd-state opcodes (fd_advise / fd_allocate / fd_sync / fd_datasync) ----
//
// Four related "fd-state" opcodes bundled in one block. In v1's
// tmpfs-backed VFS there is no advisor, no write-buffer to flush, and
// no preallocation primitive — the semantics collapse to:
//
//   fd_advise   = no-op success on a Vnode (the advice is taken, then
//                 discarded — POSIX and WASI both permit this; the
//                 opcode is purely a hint).
//   fd_sync     = no-op success on a Vnode (nothing to flush; v1
//                 writes are synchronous into the vfs state).
//   fd_datasync = no-op success on a Vnode (same reason).
//   fd_allocate = ENOTSUP on every fd (v1 tmpfs doesn't honour
//                 preallocation; returning success would lie about
//                 reserved space, so the honest answer is ENOTSUP).
//
// All four share the same guards: EBADF on an unopened fd; EINVAL on
// every non-Vnode FdObject (the state these opcodes touch only has
// meaning for seekable regular files). Handlers model exactly on
// `handle_fd_tell`'s shape — one `args_u32` read for the fd, one
// lookup, one match on the FdObject variant, done.

// Pack the 40-byte fd_advise args window. The opcode's WASI signature
// is `(fd, offset: u64, len: u64, advice: u8) -> errno`; the PMos
// wire layout is u32-aligned-friendly: fd at 0..4, offset at 4..12,
// len at 12..20, advice at 20..24 (low byte). The test helper packs
// into 16 bytes for the inline args window; offset and len carry
// zero in the v1 no-op semantics so the trailing bytes stay zero.
fn fd_advise_args(fd: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[..4].copy_from_slice(&fd.to_le_bytes());
    args
}

#[test]
fn fd_advise_on_vnode_fd_returns_success() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "adviser", "/a.txt", b"abcdefghij");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_ADVISE,
        flags: 0,
        request_id: 760,
        args: fd_advise_args(fd),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.request_id, 760);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
    // The advise is a hint — the fd's offset must be untouched.
    assert_eq!(k.fds(pid).unwrap().offset(fd).unwrap(), 0);
}

#[test]
fn fd_advise_on_char_device_fd_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "adviser", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_ADVISE,
        flags: 0,
        request_id: 761,
        args: fd_advise_args(1),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_advise_on_invalid_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "adviser", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_ADVISE,
        flags: 0,
        request_id: 762,
        args: fd_advise_args(99),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn fd_allocate_on_vnode_fd_returns_enotsup() {
    // v1 tmpfs has no preallocation primitive. A success response
    // would lie about reserved space; ENOTSUP is the honest answer.
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "allocator", "/a.txt", b"abc");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_ALLOCATE,
        flags: 0,
        request_id: 763,
        args: fd_advise_args(fd),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTSUP);
    // The fd's offset must be untouched (the refusal is a no-op).
    assert_eq!(k.fds(pid).unwrap().offset(fd).unwrap(), 0);
}

#[test]
fn fd_allocate_on_char_device_fd_returns_einval() {
    // Non-Vnode rejection fires *before* the ENOTSUP semantic;
    // char-device has no preallocation meaning at all.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "allocator", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_ALLOCATE,
        flags: 0,
        request_id: 764,
        args: fd_advise_args(1),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_allocate_on_invalid_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "allocator", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_ALLOCATE,
        flags: 0,
        request_id: 765,
        args: fd_advise_args(99),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn fd_sync_on_vnode_fd_returns_success() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "syncer", "/s.txt", b"abcdefghij");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SYNC,
        flags: 0,
        request_id: 766,
        args: u32_args(fd),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
}

#[test]
fn fd_sync_on_char_device_fd_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "syncer", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SYNC,
        flags: 0,
        request_id: 767,
        args: u32_args(1),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_sync_on_invalid_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "syncer", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_SYNC,
        flags: 0,
        request_id: 768,
        args: u32_args(99),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
}

#[test]
fn fd_datasync_on_vnode_fd_returns_success() {
    let mut k = make_kernel();
    let (pid, fd) = make_proc_with_file_fd(&mut k, "datasyncer", "/d.txt", b"abc");
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_DATASYNC,
        flags: 0,
        request_id: 769,
        args: u32_args(fd),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value, 0);
}

#[test]
fn fd_datasync_on_char_device_fd_returns_einval() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "datasyncer", 0);
    k.install_fd(pid, 1, FdObject::CharDevice(DEV_CONSOLE), FdFlags::EMPTY)
        .unwrap();
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_DATASYNC,
        flags: 0,
        request_id: 770,
        args: u32_args(1),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn fd_datasync_on_invalid_fd_returns_ebadf() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "datasyncer", 0);
    let mut heap = vec![0u8; 16];

    let req = Request {
        opcode: op_wasi::FD_DATASYNC,
        flags: 0,
        request_id: 771,
        args: u32_args(99),
        heap_ptr: 0,
        heap_len: 0,
    };
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EBADF);
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

#[test]
fn fd_write_on_pending_client_after_listener_drop_returns_econnrefused() {
    // End-to-end through the dispatcher: server binds + listens;
    // client connects (pending on backlog); server closes the
    // listener fd WITHOUT accepting; client's subsequent fd_write
    // surfaces ECONNREFUSED instead of the pre-drain EINVAL.
    //
    // This is the kernel-correctness guarantee display-client-demo
    // now relies on: a non-EINVAL errno means the retry loop should
    // exit rather than spin to MAX_POLLS waiting for an accept that
    // will never come.
    let mut k = make_kernel();
    let server = make_running_proc(&mut k, "server", 0);
    let client = make_running_proc(&mut k, "client", 0);
    let mut heap = vec![0u8; 128];
    let path = b"/tmp/drop";

    // Server: socket + bind + listen.
    let srv_listener = dispatch(
        &mut k,
        server,
        &Request {
            opcode: op_ext::IPC_SOCKET,
            flags: 0,
            request_id: 300,
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
                request_id: 301,
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
                request_id: 302,
                args: u32_u32_args(srv_listener, 4),
                heap_ptr: 0,
                heap_len: 0,
            },
            &mut heap,
        )
        .status,
        0,
    );

    // Client: socket + connect (parks on backlog).
    let cli_fd = dispatch(
        &mut k,
        client,
        &Request {
            opcode: op_ext::IPC_SOCKET,
            flags: 0,
            request_id: 303,
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
                request_id: 304,
                args: u32_args(cli_fd),
                heap_ptr: 0,
                heap_len: path.len() as u32,
            },
            &mut heap,
        )
        .status,
        0,
    );

    // Pre-drop sanity: fd_write on the pending client returns
    // EINVAL (the accept-race signal display-client-demo retries
    // on). If this assertion drifts, the retry loop semantic has
    // broken even before the listener drops.
    heap[..2].copy_from_slice(b"hi");
    let pre_drop = dispatch(
        &mut k,
        client,
        &Request {
            opcode: op_wasi::FD_WRITE,
            flags: 0,
            request_id: 305,
            args: u32_args(cli_fd),
            heap_ptr: 0,
            heap_len: 2,
        },
        &mut heap,
    );
    assert_eq!(pre_drop.status, -errno::EINVAL);

    // Server closes the listener fd WITHOUT accepting. The close
    // path drains the backlog so the still-pending client
    // transitions to Closed.
    assert_eq!(
        dispatch(
            &mut k,
            server,
            &Request {
                opcode: op_wasi::FD_CLOSE,
                flags: 0,
                request_id: 306,
                args: u32_args(srv_listener),
                heap_ptr: 0,
                heap_len: 0,
            },
            &mut heap,
        )
        .status,
        0,
    );

    // Post-drop: fd_write now reports ECONNREFUSED.
    let post_drop = dispatch(
        &mut k,
        client,
        &Request {
            opcode: op_wasi::FD_WRITE,
            flags: 0,
            request_id: 307,
            args: u32_args(cli_fd),
            heap_ptr: 0,
            heap_len: 2,
        },
        &mut heap,
    );
    assert_eq!(post_drop.status, -errno::ECONNREFUSED);
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
    // Signal channel auto-installed at fd 3.
    assert_eq!(
        child_fds.get(3).unwrap().object,
        FdObject::SignalChannel,
    );

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

// ---- proc_wait -------------------------------------------------------
//
// PROC_WAIT (0x1101). v1 userland surface over Kernel::proc_wait.
// Wire layout:
//   args[0..4] = target_pid (i32). 0 or -1 → any child; > 0 → specific
//                 pid; < -1 → EINVAL (process-group wait unsupported).
//                 == caller's own pid → ECHILD (can't wait on self).
//   args[4..8] = options (u32). WNOHANG = 0x1. v1 is always non-
//                blocking — dispatcher can't park processes yet — so
//                the bit is decoded for forward-compat but all waits
//                return -EAGAIN on WouldBlock regardless.
//   heap_len   = 0 (no child pid read-back) or 4 (write child pid to
//                heap[0..4] on a successful reap).
// Response:
//   status     = 0 on reap; -errno on error.
//   value (i64) on success = packed status:
//                 bits  0..32 = exit code (i32).
//                 bits 32..40 = signum (u8, 0 if not Signaled).
//                 bits 40..48 = flags: 0x01 Exited, 0x02 Signaled,
//                                       0x04 Crashed.
//   extra_len  = 4 if the caller requested the child pid (heap_len
//                >= 4), 0 otherwise.
// Errors (negated errno in .status):
//   ECHILD     = no children matching the target, or target ==
//                sender pid.
//   EAGAIN     = live children exist but none are zombies (also
//                returned when WNOHANG is set; v1 never blocks).
//   EINVAL     = malformed target_pid (< -1) or the options u32 has
//                unknown bits set beyond WNOHANG.

fn proc_wait_args(target_pid: i32, options: u32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&target_pid.to_le_bytes());
    args[4..8].copy_from_slice(&options.to_le_bytes());
    args
}

/// Register a child under `parent` (using a plain register — NOT
/// `proc_spawn`, because that needs a backing fs path + a Platform
/// spawn_process call). Leaves the child in Ready state; tests
/// transition it to Running + proc_exit as needed.
fn register_child(k: &mut Kernel, parent: abi::ext::Pid, name: &str) -> abi::ext::Pid {
    let child = k
        .register_process(RegisterArgs {
            name,
            ppid: parent,
            caps: initial::ORDINARY_APP,
            cwd: "/",
        })
        .expect("register child");
    k.mark_ready(child).expect("mark_ready child");
    child
}

#[test]
fn proc_wait_any_reaps_zombie_child_returns_packed_status() {
    // Set up init (pid 1-ish) + a child that runs then exits(42).
    // proc_wait with target=-1 + options=0 reaps the zombie and
    // returns the packed exit status.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "shortlived");
    k.procs
        .transition(child, kernel::proc::ProcState::Running)
        .unwrap();
    k.proc_exit(child, ExitStatus::Exited(42)).unwrap();

    let req = Request {
        opcode: op_ext::PROC_WAIT,
        flags: 0,
        request_id: 1200,
        args: proc_wait_args(-1, 0),
        heap_ptr: 0,
        heap_len: 4,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    // Packed: exit code 42 at low 32; flags Exited=0x01 at bits 40..48.
    let packed = resp.value as u64;
    let exit_code = (packed & 0xffff_ffff) as i32;
    let signum = ((packed >> 32) & 0xff) as u8;
    let flags = ((packed >> 40) & 0xff) as u8;
    assert_eq!(exit_code, 42);
    assert_eq!(signum, 0);
    assert_eq!(flags, 0x01);
    // Child pid written to heap[0..4] as u32 LE.
    assert_eq!(resp.extra_len, 4);
    let reaped_pid = u32::from_le_bytes([heap[0], heap[1], heap[2], heap[3]]);
    assert_eq!(reaped_pid as i32, child);
    // And the child is actually reaped.
    assert!(!k.procs.is_alive(child));
}

#[test]
fn proc_wait_specific_pid_reaps_only_named_child() {
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child_a = register_child(&mut k, init, "a");
    let child_b = register_child(&mut k, init, "b");
    k.procs
        .transition(child_a, kernel::proc::ProcState::Running)
        .unwrap();
    k.procs
        .transition(child_b, kernel::proc::ProcState::Running)
        .unwrap();
    k.proc_exit(child_b, ExitStatus::Exited(7)).unwrap();
    // child_a still running; child_b is a zombie.

    let req = Request {
        opcode: op_ext::PROC_WAIT,
        flags: 0,
        request_id: 1201,
        args: proc_wait_args(child_b, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let exit_code = (resp.value as u64 & 0xffff_ffff) as i32;
    assert_eq!(exit_code, 7);
    // heap_len was 0, so no pid written back.
    assert_eq!(resp.extra_len, 0);
}

#[test]
fn proc_wait_wnohang_with_live_child_returns_eagain() {
    // A live child that hasn't exited yet. With WNOHANG set, the
    // handler returns -EAGAIN instead of blocking.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let _child = register_child(&mut k, init, "running");
    // Child remains in Ready state (alive).

    let req = Request {
        opcode: op_ext::PROC_WAIT,
        flags: 0,
        request_id: 1202,
        args: proc_wait_args(-1, abi::ext::wait_opts::WNOHANG),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, -errno::EAGAIN);
}

#[test]
fn proc_wait_on_unknown_pid_returns_echild() {
    // Wait target is a pid that either doesn't exist or isn't a
    // child of the caller. Must distinguish from EAGAIN: no live
    // children at all matching the target → ECHILD.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "lonely", 0);
    // No children registered. Wait for any → ECHILD.

    let req = Request {
        opcode: op_ext::PROC_WAIT,
        flags: 0,
        request_id: 1203,
        args: proc_wait_args(-1, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, -errno::ECHILD);
}

#[test]
fn proc_wait_on_self_returns_echild() {
    // Specific target == caller's own pid: can't wait on yourself.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "selfie", 0);

    let req = Request {
        opcode: op_ext::PROC_WAIT,
        flags: 0,
        request_id: 1204,
        args: proc_wait_args(pid, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ECHILD);
}

#[test]
fn proc_wait_invalid_target_below_minus_one_returns_einval() {
    // target_pid < -1 is process-group wait (POSIX waitpid(-gid, ...)).
    // v1 doesn't implement process groups, so reject with EINVAL
    // rather than silently treating it as Any.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "grpwait", 0);

    let req = Request {
        opcode: op_ext::PROC_WAIT,
        flags: 0,
        request_id: 1205,
        args: proc_wait_args(-5, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn proc_wait_on_signaled_child_returns_packed_signaled_status() {
    // Child terminated by signal 9 (SIGKILL). Packed status has
    // flags = 0x02 Signaled and signum = 9.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "killed");
    k.procs
        .transition(child, kernel::proc::ProcState::Running)
        .unwrap();
    k.proc_exit(child, ExitStatus::Signaled(9)).unwrap();

    let req = Request {
        opcode: op_ext::PROC_WAIT,
        flags: 0,
        request_id: 1206,
        args: proc_wait_args(child, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    let packed = resp.value as u64;
    let signum = ((packed >> 32) & 0xff) as u8;
    let flags = ((packed >> 40) & 0xff) as u8;
    assert_eq!(signum, 9);
    assert_eq!(flags, 0x02);
}

// ---- proc_kill -------------------------------------------------------
//
// PROC_KILL (0x1102). Send a POSIX-style signal to a pid. v1 knows
// three signal numbers:
//
//   * SIGKILL=9  → terminal, target zombifies with Signaled(9).
//   * SIGTERM=15 → catchable, queued on target's SignalInbox.
//   * SIGINT=2   → catchable, queued on target's SignalInbox.
//
// Any other signum returns -EINVAL. Cap rules: sender must be the
// target's parent OR the sender itself OR hold
// `Cap::ProcKillAny`; otherwise -ENOTCAPABLE. Non-existent target
// returns -ESRCH.
//
// Wire layout:
//   args[0..4] = target_pid (i32).
//   args[4..6] = signum (u16; low 16 bits of the POSIX signal
//                number).
// Response: value = 0 on success.

fn proc_kill_args(target_pid: i32, signum: u16) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&target_pid.to_le_bytes());
    args[4..6].copy_from_slice(&signum.to_le_bytes());
    args
}

#[test]
fn proc_kill_sigkill_zombifies_child() {
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "doomed");
    k.procs
        .transition(child, kernel::proc::ProcState::Running)
        .unwrap();

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1210,
        args: proc_kill_args(child, 9),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(
        k.procs.get(child).unwrap().state,
        kernel::proc::ProcState::Zombie
    );
    assert_eq!(
        k.procs.get(child).unwrap().exit_status,
        Some(ExitStatus::Signaled(9))
    );
}

#[test]
fn proc_kill_sigterm_queues_on_inbox_and_leaves_target_running() {
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "target");

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1211,
        args: proc_kill_args(child, 15),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(k.pending_signals(child).unwrap(), 1);
    // Still Ready (Term is catchable and doesn't zombify).
    assert_eq!(
        k.procs.get(child).unwrap().state,
        kernel::proc::ProcState::Ready
    );
}

#[test]
fn proc_kill_sigint_queues_on_inbox() {
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "int");

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1212,
        args: proc_kill_args(child, 2),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(k.pending_signals(child).unwrap(), 1);
}

#[test]
fn proc_kill_unknown_signum_returns_einval() {
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "child");

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1213,
        args: proc_kill_args(child, 77),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
}

#[test]
fn proc_kill_nonexistent_target_returns_esrch() {
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1214,
        args: proc_kill_args(9999, 15),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, -errno::ESRCH);
}

#[test]
fn proc_kill_non_child_without_proc_kill_any_returns_enotcapable() {
    // a and b are both children of init → peers of each other, not
    // in a parent-child relationship. Neither holds ProcKillAny, so
    // a killing b returns ENOTCAPABLE.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let a = register_child(&mut k, init, "a");
    let b = register_child(&mut k, init, "b");
    k.procs
        .transition(a, kernel::proc::ProcState::Running)
        .unwrap();
    k.procs
        .transition(b, kernel::proc::ProcState::Running)
        .unwrap();

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1215,
        args: proc_kill_args(b, 15),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, a, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTCAPABLE);
    // b unchanged.
    assert_eq!(
        k.procs.get(b).unwrap().state,
        kernel::proc::ProcState::Running
    );
}

#[test]
fn proc_kill_self_sigint_is_allowed_and_queues() {
    // Self-kill with a catchable signal: POSIX kill(getpid(), SIG)
    // works even without ProcKillAny. Post-slice the kernel's cap
    // check allows sender == target.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "self-signaller", 0);

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1216,
        args: proc_kill_args(pid, 2),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(k.pending_signals(pid).unwrap(), 1);
}

#[test]
fn proc_kill_dead_target_returns_esrch() {
    // Reaped (Dead) target: ESRCH. proc_kill's NoSuchPid arm fires
    // for both unknown pids and pids in the terminal Dead state.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "short");
    k.procs
        .transition(child, kernel::proc::ProcState::Running)
        .unwrap();
    k.proc_exit(child, ExitStatus::Exited(0)).unwrap();
    // Reap the zombie so the process is fully Dead.
    k.reap(child).unwrap();

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1217,
        args: proc_kill_args(child, 15),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, -errno::ESRCH);
}

// ---- proc_raise ------------------------------------------------------
//
// PROC_RAISE (0x0061). POSIX raise(3): deliver a signal to the
// calling process. v1 knows three signal numbers (same set
// PROC_KILL knows):
//
//   * SIGKILL=9  → terminal, caller zombifies with Signaled(9).
//   * SIGTERM=15 → catchable, queued on caller's SignalInbox.
//   * SIGINT=2   → catchable, queued on caller's SignalInbox.
//
// Any other signum returns -EINVAL. Because the target is always
// the caller, the self-signal cap rule (sender == target) is the
// only one exercised — raise(3) never reports ENOTCAPABLE even
// from a pid that holds no signalling caps at all.
//
// Wire layout:
//   args[0..2] = signum (u16).
// Response: value = 0 on success.

fn proc_raise_args(signum: u16) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..2].copy_from_slice(&signum.to_le_bytes());
    args
}

#[test]
fn proc_raise_sigterm_queues_on_own_inbox() {
    // Catchable signal to self: POSIX raise(SIGTERM). The caller's
    // own SignalInbox accumulates the signal; state stays Running
    // through the call since Term is catchable.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "self-raiser", 0);

    let req = Request {
        opcode: op_wasi::PROC_RAISE,
        flags: 0,
        request_id: 1300,
        args: proc_raise_args(15),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(k.pending_signals(pid).unwrap(), 1);
    assert_eq!(
        k.procs.get(pid).unwrap().state,
        kernel::proc::ProcState::Running
    );
}

#[test]
fn proc_raise_sigint_queues_on_own_inbox() {
    // Same path as SIGTERM — the catchable-signal branch also
    // carries SIGINT. Included so a future slice that diverges the
    // two (e.g. SIGINT wakes a parked syscall, SIGTERM does not)
    // would fail fast here instead of inside the signal-delivery
    // code.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "self-raiser-int", 0);

    let req = Request {
        opcode: op_wasi::PROC_RAISE,
        flags: 0,
        request_id: 1301,
        args: proc_raise_args(2),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(k.pending_signals(pid).unwrap(), 1);
}

#[test]
fn proc_raise_sigkill_zombifies_caller() {
    // Terminal signal to self: POSIX raise(SIGKILL). Caller
    // transitions straight to Zombie with Signaled(9) exit status.
    // The response still posts back (0) — the dispatcher writes it
    // before the caller's Worker is torn down.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "self-killer", 0);

    let req = Request {
        opcode: op_wasi::PROC_RAISE,
        flags: 0,
        request_id: 1302,
        args: proc_raise_args(9),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(
        k.procs.get(pid).unwrap().state,
        kernel::proc::ProcState::Zombie
    );
    assert_eq!(
        k.procs.get(pid).unwrap().exit_status,
        Some(ExitStatus::Signaled(9))
    );
}

#[test]
fn proc_raise_unknown_signum_returns_einval() {
    // Signum outside the accepted set → EINVAL before the kernel
    // is touched. Mirrors PROC_KILL's unknown-signum probe so the
    // two handlers stay symmetric on input validation. Post-
    // SIGCHLD slice the accepted set is {2, 9, 13, 15, 17}.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "self-bad-sig", 0);

    let req = Request {
        opcode: op_wasi::PROC_RAISE,
        flags: 0,
        request_id: 1303,
        args: proc_raise_args(77),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::EINVAL);
    // No signal was queued and state is unchanged.
    assert_eq!(k.pending_signals(pid).unwrap(), 0);
    assert_eq!(
        k.procs.get(pid).unwrap().state,
        kernel::proc::ProcState::Running
    );
}

#[test]
fn proc_raise_sigpipe_queues_on_own_inbox() {
    // PROC_RAISE(13) → SIGPIPE queued on the caller's own inbox.
    // Proves the dispatcher accepts signum 13 post-SIGCHLD slice.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "self-pipe", 0);

    let req = Request {
        opcode: op_wasi::PROC_RAISE,
        flags: 0,
        request_id: 1310,
        args: proc_raise_args(13),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(k.drain_signals(pid).unwrap(), alloc::vec![Signal::Pipe]);
}

#[test]
fn proc_raise_sigchld_queues_on_own_inbox() {
    // PROC_RAISE(17) → SIGCHLD on self. Unusual in real POSIX
    // (usually kernel-generated) but accepted for symmetry with
    // the PROC_KILL dispatcher.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "self-chld", 0);

    let req = Request {
        opcode: op_wasi::PROC_RAISE,
        flags: 0,
        request_id: 1311,
        args: proc_raise_args(17),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(k.drain_signals(pid).unwrap(), alloc::vec![Signal::Child]);
}

#[test]
fn proc_kill_sigpipe_queues_on_child_inbox() {
    // PROC_KILL(child, 13) → SIGPIPE on child's inbox; child
    // state unchanged (catchable).
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "target");

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1410,
        args: proc_kill_args(child, 13),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(
        k.drain_signals(child).unwrap(),
        alloc::vec![Signal::Pipe]
    );
}

#[test]
fn proc_kill_sigchld_queues_on_child_inbox() {
    // PROC_KILL(child, 17) → SIGCHLD on child's inbox. Normally
    // SIGCHLD is kernel-generated (delivered to parent on child
    // exit) but the dispatcher accepts userland-synthesised
    // SIGCHLD for test + tool flexibility.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "target");

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1411,
        args: proc_kill_args(child, 17),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(
        k.drain_signals(child).unwrap(),
        alloc::vec![Signal::Child]
    );
}

// ---- proc_kill(pid, 0) existence probe ---------------------------
//
// POSIX: kill(pid, 0) doesn't deliver a signal — it checks whether
// `pid` exists and whether the caller would be permitted to signal
// it. Returns 0 on success, ESRCH if pid doesn't exist / is reaped,
// ENOTCAPABLE on permission denied. Shares the precondition surface
// with the signum != 0 path, so the dispatcher routes signum 0 to
// Kernel::proc_check_signal instead of Kernel::proc_kill.

#[test]
fn proc_kill_signum_zero_on_live_child_returns_ok_and_queues_nothing() {
    // Parent -> live child with signum 0: permission check + target
    // existence both pass -> 0 return. No signal delivered.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "target");

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1420,
        args: proc_kill_args(child, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(k.pending_signals(child).unwrap(), 0);
}

#[test]
fn proc_kill_signum_zero_on_self_returns_ok() {
    // Self-probe: sender == target always permitted. No signal
    // delivered either way.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "self", 0);

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1421,
        args: proc_kill_args(pid, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(k.pending_signals(pid).unwrap(), 0);
}

#[test]
fn proc_kill_signum_zero_on_nonexistent_target_returns_esrch() {
    // POSIX kill(bogus_pid, 0) returns ESRCH — the existence-check
    // role of the probe.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1422,
        args: proc_kill_args(9999, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, -errno::ESRCH);
}

#[test]
fn proc_kill_signum_zero_on_non_child_without_proc_kill_any_returns_enotcapable() {
    // Two ordinary apps that aren't parent/child of each other;
    // neither holds ProcKillAny. Signum 0 still gates on the
    // permission check and returns ENOTCAPABLE.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    // Manually register two children with minimal caps (no
    // ProcKillAny) and re-home their ppid so they are NOT in a
    // parent-child relationship to each other.
    let a = k
        .register_process(RegisterArgs {
            name: "a",
            ppid: init,
            caps: abi::cap::initial::ORDINARY_APP,
            cwd: "/",
        })
        .expect("register a");
    k.mark_ready(a).unwrap();
    k.procs
        .transition(a, kernel::proc::ProcState::Running)
        .unwrap();
    let b = k
        .register_process(RegisterArgs {
            name: "b",
            ppid: init,
            caps: abi::cap::initial::ORDINARY_APP,
            cwd: "/",
        })
        .expect("register b");
    k.mark_ready(b).unwrap();
    k.procs
        .transition(b, kernel::proc::ProcState::Running)
        .unwrap();

    // a probes b — neither is a parent of the other, and
    // ORDINARY_APP doesn't carry ProcKillAny. Must ENOTCAPABLE.
    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1423,
        args: proc_kill_args(b, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, a, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTCAPABLE);
}

#[test]
fn proc_kill_signum_zero_on_reaped_target_returns_esrch() {
    // A zombified + reaped target is as good as non-existent.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "doomed");
    k.proc_exit(child, ExitStatus::Exited(0)).unwrap();
    k.reap(child).unwrap();

    let req = Request {
        opcode: op_ext::PROC_KILL,
        flags: 0,
        request_id: 1424,
        args: proc_kill_args(child, 0),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, -errno::ESRCH);
}

// ---- proc_caps_get ---------------------------------------------------
//
// PROC_CAPS_GET (0x1105). Query a process's cap set. Sender may
// query its own caps freely. Querying another pid requires the
// sender to be the target's parent OR to hold Cap::ProcInspect —
// otherwise ENOTCAPABLE. Non-existent / reaped target → ESRCH.
//
// Wire layout:
//   args[0..4] = target_pid (i32).
// Response: value = CapSet as i64 on success; negative errno on
// failure.

fn proc_caps_get_args(target_pid: i32) -> [u8; 16] {
    let mut args = [0u8; 16];
    args[0..4].copy_from_slice(&target_pid.to_le_bytes());
    args
}

#[test]
fn proc_caps_get_self_returns_own_capset() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "selfy", 0);
    let own = k.caps.list(pid).unwrap();

    let req = Request {
        opcode: op_ext::PROC_CAPS_GET,
        flags: 0,
        request_id: 1220,
        args: proc_caps_get_args(pid),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value as u64, own.0);
}

#[test]
fn proc_caps_get_child_from_parent_returns_child_capset() {
    // init (parent) queries a child's caps — allowed without
    // ProcInspect because is_parent = true.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let child = register_child(&mut k, init, "kid");
    let child_caps = k.caps.list(child).unwrap();

    let req = Request {
        opcode: op_ext::PROC_CAPS_GET,
        flags: 0,
        request_id: 1221,
        args: proc_caps_get_args(child),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, init, &req, &mut heap);
    assert_eq!(resp.status, 0);
    assert_eq!(resp.value as u64, child_caps.0);
}

#[test]
fn proc_caps_get_on_unknown_pid_returns_esrch() {
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "pryer", 0);

    let req = Request {
        opcode: op_ext::PROC_CAPS_GET,
        flags: 0,
        request_id: 1222,
        args: proc_caps_get_args(9999),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, pid, &req, &mut heap);
    assert_eq!(resp.status, -errno::ESRCH);
}

#[test]
fn proc_caps_get_on_non_child_without_proc_inspect_returns_enotcapable() {
    // Two siblings a and b under init. Neither is the other's
    // parent. `a` has ORDINARY_APP caps — no ProcInspect. `a`
    // trying to read `b`'s caps → ENOTCAPABLE.
    let mut k = make_kernel();
    let init = make_running_proc(&mut k, "init", 0);
    let a = register_child(&mut k, init, "a");
    let b = register_child(&mut k, init, "b");
    k.procs
        .transition(a, kernel::proc::ProcState::Running)
        .unwrap();
    k.procs
        .transition(b, kernel::proc::ProcState::Running)
        .unwrap();

    let req = Request {
        opcode: op_ext::PROC_CAPS_GET,
        flags: 0,
        request_id: 1223,
        args: proc_caps_get_args(b),
        heap_ptr: 0,
        heap_len: 0,
    };
    let mut heap = vec![0u8; 16];
    let resp = dispatch(&mut k, a, &req, &mut heap);
    assert_eq!(resp.status, -errno::ENOTCAPABLE);
}
