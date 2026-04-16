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
    // CLOCK_TIME_GET is in the WASI range (0x0011) but has no
    // handler in the first T073 landing. The dispatcher's _ arm
    // in `dispatch_wasi` should catch it with ENOSYS.
    let mut k = make_kernel();
    let pid = make_running_proc(&mut k, "test", 0);
    let mut heap = vec![0u8; 4096];

    let req = Request {
        opcode: op_wasi::CLOCK_TIME_GET,
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
