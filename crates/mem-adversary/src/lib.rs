//! Adversarial test program — Principle V acceptance gate (T172).
//!
//! The adversary runs every probe a malicious user-wasm could use
//! to read foreign memory through a non-IPC channel and asserts
//! that EVERY attempt is REJECTED by the kernel. The composition
//! contract:
//!
//!   * Spawned with `ORDINARY_APP` caps (or any non-omnipotent set —
//!     the program checks its own caps and skips probes whose
//!     expected-failure outcome doesn't apply when the caller
//!     happens to hold the gating cap).
//!   * Provided console stdio (fd 0/1/2 → /dev/console).
//!
//! On full success the binary prints `mem-adversary OK\n` to
//! `/dev/console` and exits 0. If ANY probe succeeds when it
//! should have failed, the binary prints `mem-adversary BREACH
//! probe <n>\n` and exits with the probe index — the test harness
//! (Playwright `process-isolation.spec.ts`) reads the exit code
//! and the console output to identify which probe failed.
//!
//! Probe catalogue:
//!
//!   1. `cap_check(invalid_id=99)` → expect EINVAL.
//!   2. `proc_kill(fake_pid=99999, SIGTERM)` → expect ESRCH (when
//!      caller holds PROC_KILL_ANY) or ENOTCAPABLE (when not).
//!   3. `proc_caps_get(fake_pid=99999)` → expect ESRCH; the out
//!      ptr must NOT be filled with foreign-process caps.
//!   4. `fd_read(fake_fd=99, buf)` → expect EBADF; `buf` must
//!      remain zero (the kernel must not leak bytes).
//!   5. `fd_close(fake_fd=99)` → expect EBADF.
//!   6. `path_open("/no/such/file")` → expect ENOENT.
//!   7. `path_open("/proc/99999/status")` → expect ENOENT (a
//!      probe for a stranger's pid existence MUST NOT succeed).
//!   8. `proc_spawn(path, caps=u64::MAX)` → expect ENOTCAPABLE
//!      (when caller's caps != ALL); the kernel must reject the
//!      cap-superset request rather than silently down-cast.
//!
//! Probes that depend on the absence of `cap_grant` and `mount`
//! WASI imports (the user-wasm runtime simply does not expose
//! those host functions, which makes the call ABI-unreachable
//! from user wasm) are not in this catalogue — Principle V
//! holds for them by construction, not by capability check.
//!
//! Exit codes:
//!
//! * 0   = every applicable probe was correctly rejected
//! * 1..=8 = probe N succeeded when it should have failed
//! * 13  = `fd_write` to /dev/console failed (test-harness
//!   failure, not an isolation breach)
//! * 101 = panic

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
mod imports {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        pub fn fd_write(
            fd: i32,
            iovs_ptr: *const super::Ciovec,
            iovs_len: i32,
            nwritten_ptr: *mut u32,
        ) -> i32;
        pub fn fd_read(
            fd: i32,
            iovs_ptr: *const super::Iovec,
            iovs_len: i32,
            nread_ptr: *mut u32,
        ) -> i32;
        pub fn fd_close(fd: i32) -> i32;
        pub fn path_open(
            dirfd: i32,
            dirflags: i32,
            path_ptr: *const u8,
            path_len: i32,
            oflags: i32,
            fs_rights_base: u64,
            fs_rights_inh: u64,
            fdflags: i32,
            ret_fd_ptr: *mut i32,
        ) -> i32;
        pub fn proc_exit(rval: i32) -> !;
    }

    #[link(wasm_import_module = "pmos_ext")]
    extern "C" {
        pub fn cap_check(cap_id: i32) -> i32;
        pub fn cap_list(out_ptr: *mut u64) -> i32;
        pub fn proc_kill(target_pid: i32, signum: i32) -> i32;
        pub fn proc_caps_get(target_pid: i32, out_ptr: *mut u64) -> i32;
        pub fn proc_spawn(path_ptr: *const u8, path_len: u32, caps: u64) -> i32;
    }
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Ciovec {
    buf: *const u8,
    buf_len: u32,
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Iovec {
    buf: *mut u8,
    buf_len: u32,
}

// ---- Errno constants (mirror abi::errno) ----------------------------

#[cfg(target_arch = "wasm32")]
const EBADF: i32 = 8;
#[cfg(target_arch = "wasm32")]
const EINVAL: i32 = 28;
#[cfg(target_arch = "wasm32")]
const ENOENT: i32 = 44;
#[cfg(target_arch = "wasm32")]
const ESRCH: i32 = 71;
#[cfg(target_arch = "wasm32")]
const ENOTCAPABLE: i32 = 76;

// ---- Cap bits (mirror abi::cap::Cap) --------------------------------

#[cfg(target_arch = "wasm32")]
const CAP_PROC_KILL_ANY: u32 = 1;

#[cfg(target_arch = "wasm32")]
fn write_console(bytes: &[u8]) -> bool {
    unsafe {
        let iov = Ciovec {
            buf: bytes.as_ptr(),
            buf_len: bytes.len() as u32,
        };
        let mut nwritten: u32 = 0;
        let rc = imports::fd_write(1, &iov, 1, &mut nwritten);
        rc == 0 && nwritten == bytes.len() as u32
    }
}

#[cfg(target_arch = "wasm32")]
fn write_line(line: &[u8]) -> bool {
    if !write_console(line) {
        return false;
    }
    write_console(b"\n")
}

/// Run a probe and verify its return code matches the expected
/// negative errno. Returns `true` if the probe was correctly
/// rejected; `false` if the kernel allowed the operation through.
#[cfg(target_arch = "wasm32")]
fn check_rejected(name: &[u8], actual: i32, expected_negative_errno: i32) -> bool {
    if actual == expected_negative_errno {
        let _ = write_console(b"PASS ");
        let _ = write_line(name);
        true
    } else {
        let _ = write_console(b"BREACH ");
        let _ = write_line(name);
        false
    }
}

#[cfg(target_arch = "wasm32")]
fn caller_caps() -> u64 {
    unsafe {
        let mut caps: u64 = 0;
        let _ = imports::cap_list(&mut caps as *mut u64);
        caps
    }
}

#[cfg(target_arch = "wasm32")]
fn caller_holds_cap(bit: u32, caps: u64) -> bool {
    (caps >> bit) & 1 == 1
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn _start() {
    let _ = write_line(b"mem-adversary START");

    let mut breach_index: i32 = 0;
    let caps = caller_caps();

    unsafe {
        // Probe 1: cap_check with an invalid cap id rejects with
        // EINVAL. The kernel must not happily return "yes" for
        // an unknown cap (defence in depth against a userland
        // probing for unknown caps post-kernel-upgrade).
        let rc = imports::cap_check(99);
        if !check_rejected(b"cap_check_invalid_id", rc, -EINVAL) {
            breach_index = 1;
        }

        // Probe 2: proc_kill on a fake pid (99999). When the
        // caller holds PROC_KILL_ANY the kernel checks the cap
        // first then ESRCH; without it, ENOTCAPABLE wins.
        if breach_index == 0 {
            let rc = imports::proc_kill(99999, 15); // SIGTERM
            let expected = if caller_holds_cap(CAP_PROC_KILL_ANY, caps) {
                -ESRCH
            } else {
                -ENOTCAPABLE
            };
            if !check_rejected(b"proc_kill_fake_pid", rc, expected) {
                breach_index = 2;
            }
        }

        // Probe 3: proc_caps_get(unknown_pid) returns ESRCH and
        // does NOT fill the out pointer with foreign caps —
        // defence in depth, the kernel's own contract is that
        // out is only written on success.
        if breach_index == 0 {
            let mut out: u64 = 0xdead_beef_dead_beef;
            let rc = imports::proc_caps_get(99999, &mut out as *mut u64);
            if !check_rejected(b"proc_caps_get_fake_pid", rc, -ESRCH) {
                breach_index = 3;
            }
            if breach_index == 0 && out != 0xdead_beef_dead_beef {
                let _ = write_line(b"BREACH proc_caps_get_fake_pid wrote out on error");
                breach_index = 3;
            }
        }

        // Probe 4: fd_read on an unowned fd returns EBADF and
        // does not write any bytes into the caller's buffer.
        // WASI shims return POSITIVE errno (matching the WASI
        // standard), so we compare against +EBADF, not -EBADF.
        if breach_index == 0 {
            let mut buf = [0u8; 16];
            let iov = Iovec {
                buf: buf.as_mut_ptr(),
                buf_len: buf.len() as u32,
            };
            let mut nread: u32 = 0;
            let rc = imports::fd_read(99, &iov, 1, &mut nread);
            if !check_rejected(b"fd_read_unowned_fd", rc, EBADF) {
                breach_index = 4;
            }
            if breach_index == 0 && buf.iter().any(|&b| b != 0) {
                let _ = write_line(b"BREACH fd_read_unowned_fd leaked bytes");
                breach_index = 4;
            }
        }

        // Probe 5: fd_close on an unowned fd returns EBADF.
        if breach_index == 0 {
            let rc = imports::fd_close(99);
            if !check_rejected(b"fd_close_unowned_fd", rc, EBADF) {
                breach_index = 5;
            }
        }

        // Probe 6: path_open on a nonexistent absolute path returns ENOENT.
        if breach_index == 0 {
            let path = b"/no/such/file";
            let mut fd: i32 = -1;
            let rc = imports::path_open(
                3, // dirfd: any — absolute paths bypass dirfd
                0,
                path.as_ptr(),
                path.len() as i32,
                0,
                0,
                0,
                0,
                &mut fd as *mut i32,
            );
            if !check_rejected(b"path_open_nonexistent_path", rc, ENOENT) {
                breach_index = 6;
            }
        }

        // Probe 7: path_open on /proc/<unknown-pid>/status returns
        // ENOENT — a probe for a stranger's pid existence MUST
        // NOT succeed via this side channel.
        if breach_index == 0 {
            let path = b"/proc/99999/status";
            let mut fd: i32 = -1;
            let rc = imports::path_open(
                3,
                0,
                path.as_ptr(),
                path.len() as i32,
                0,
                0,
                0,
                0,
                &mut fd as *mut i32,
            );
            if !check_rejected(b"path_open_unknown_pid_status", rc, ENOENT) {
                breach_index = 7;
            }
        }

        // Probe 8: proc_spawn with a cap superset of self. When the
        // caller doesn't already hold every cap, asking for u64::MAX
        // is necessarily a superset; the kernel must reject with
        // ENOTCAPABLE rather than silently down-cast (defence
        // against a crafted manifest that elevates the child).
        if breach_index == 0 {
            if caps != u64::MAX {
                let path = b"/usr/bin/sh";
                let rc = imports::proc_spawn(path.as_ptr(), path.len() as u32, u64::MAX);
                if !check_rejected(b"proc_spawn_cap_superset", rc, -ENOTCAPABLE) {
                    breach_index = 8;
                }
            } else {
                let _ = write_line(b"SKIP proc_spawn_cap_superset (caller holds CAPSET_ALL)");
            }
        }

        if breach_index == 0 {
            let _ = write_line(b"mem-adversary OK");
            imports::proc_exit(0);
        } else {
            let _ = write_console(b"mem-adversary BREACH probe ");
            // breach_index is 1..=8 — single ASCII digit.
            let _ = write_console(&[b'0' + breach_index as u8]);
            let _ = write_console(b"\n");
            imports::proc_exit(breach_index);
        }
    }
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { imports::proc_exit(101) }
}
