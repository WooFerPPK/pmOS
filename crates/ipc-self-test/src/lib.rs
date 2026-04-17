//! `ipc-self-test` — a no_std wasm32-wasip1 binary that exercises
//! every IPC opcode end-to-end in one `_start` pass.
//!
//! Used by the `user-wasm-runtime.test.ts` composition test to
//! prove that the IPC opcode pipeline works through REAL user
//! wasm (not just direct `KernelWasmHost.dispatch` calls from a
//! Rust test).
//!
//! ## Flow
//!
//! The binary plays both server and client in a single process —
//! self-connection works because the kernel's IPC state machine
//! indexes sockets by `SocketId`, not by pid, so client and
//! listener can live in the same address space without the
//! kernel caring. This avoids the ordering problem that a
//! two-binary (client + server) split would hit under the
//! composition test harness's sequential `runAllSpawns` loop:
//! the binary that runs second would have a listener that no
//! longer has a client waiting for it, because the client
//! already exited.
//!
//! Sequence:
//!
//!   1. `ipc_socket(Stream)` → listener fd
//!   2. `ipc_bind(listener, "/tmp/self")`
//!   3. `ipc_listen(listener, 4)`
//!   4. `ipc_socket(Stream)` → client fd
//!   5. `ipc_connect(client, "/tmp/self")` — client fd is now
//!      enqueued on the listener's backlog
//!   6. `ipc_accept(listener)` → server fd — the kernel pops the
//!      client off the backlog and pairs it with a fresh
//!      server-side socket
//!   7. `fd_write(client, "hello via ipc\n")` — bytes flow into
//!      the kernel-owned ring buffer attached to the
//!      client↔server pair
//!   8. `fd_read(server, buf)` — reads the same bytes back out
//!   9. `fd_write(1, buf[..n])` — echo the received bytes to
//!      `/dev/console` so the vitest's `onConsoleWrite` callback
//!      observes them
//!   10. `proc_exit(0)`
//!
//! The binary exits 0 on success. Any intermediate failure path
//! (bad fd, EAGAIN on accept, ECONNREFUSED on connect, a short
//! read, etc.) proc_exits with a distinctive non-zero code that
//! points at the failed step:
//!
//!   * 10 = `ipc_socket` (listener) failed
//!   * 11 = `ipc_bind` failed
//!   * 12 = `ipc_listen` failed
//!   * 13 = `ipc_socket` (client) failed
//!   * 14 = `ipc_connect` failed
//!   * 15 = `ipc_accept` failed
//!   * 16 = `fd_write` (to socket) failed
//!   * 17 = `fd_read` (from socket) failed or short-read
//!   * 18 = `fd_write` (to stdout) failed
//!
//! These codes are test-facing diagnostics: a green vitest means
//! every step succeeded, and a red vitest's exit code tells the
//! reader which step failed without needing to instrument the
//! kernel or single-step through the wasm.

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(
        fd: i32,
        iovs_ptr: *const Ciovec,
        iovs_len: i32,
        nwritten_ptr: *mut u32,
    ) -> i32;
    fn fd_read(
        fd: i32,
        iovs_ptr: *const Iovec,
        iovs_len: i32,
        nread_ptr: *mut u32,
    ) -> i32;
    fn proc_exit(rval: i32) -> !;
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn ipc_socket(ty: i32) -> i32;
    fn ipc_bind(fd: i32, path_ptr: *const u8, path_len: i32) -> i32;
    fn ipc_listen(fd: i32, backlog: i32) -> i32;
    fn ipc_connect(fd: i32, path_ptr: *const u8, path_len: i32) -> i32;
    fn ipc_accept(listener_fd: i32) -> i32;
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Ciovec {
    buf: *const u8,
    buf_len: u32,
}

/// Iovec with a mutable destination buffer, used for `fd_read`.
/// The layout matches `Ciovec` byte-for-byte (both are a pointer
/// followed by a u32 length); only the mutability of `buf`
/// differs, which is a Rust-side concern the WASM ABI doesn't
/// care about.
#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Iovec {
    buf: *mut u8,
    buf_len: u32,
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn _start() {
    const PATH: &[u8] = b"/tmp/self";
    const STREAM: i32 = 0;

    unsafe {
        // 1. listener socket
        let listener = ipc_socket(STREAM);
        if listener < 0 {
            proc_exit(10);
        }

        // 2. bind
        let rc = ipc_bind(listener, PATH.as_ptr(), PATH.len() as i32);
        if rc != 0 {
            proc_exit(11);
        }

        // 3. listen
        let rc = ipc_listen(listener, 4);
        if rc != 0 {
            proc_exit(12);
        }

        // 4. client socket
        let client = ipc_socket(STREAM);
        if client < 0 {
            proc_exit(13);
        }

        // 5. connect client to listener
        let rc = ipc_connect(client, PATH.as_ptr(), PATH.len() as i32);
        if rc != 0 {
            proc_exit(14);
        }

        // 6. accept on listener — pairs client fd with a fresh
        //    server-side fd.
        let server = ipc_accept(listener);
        if server < 0 {
            proc_exit(15);
        }

        // 7. write via the client fd.
        const MSG: &[u8] = b"hello via ipc\n";
        let write_iov = Ciovec {
            buf: MSG.as_ptr(),
            buf_len: MSG.len() as u32,
        };
        let mut nwritten: u32 = 0;
        let rc = fd_write(client, &write_iov, 1, &mut nwritten);
        if rc != 0 || nwritten != MSG.len() as u32 {
            proc_exit(16);
        }

        // 8. read on the server fd.
        let mut recv_buf = [0u8; 32];
        let read_iov = Iovec {
            buf: recv_buf.as_mut_ptr(),
            buf_len: recv_buf.len() as u32,
        };
        let mut nread: u32 = 0;
        let rc = fd_read(server, &read_iov, 1, &mut nread);
        if rc != 0 || nread as usize != MSG.len() {
            proc_exit(17);
        }

        // 9. echo the received bytes to stdout so the vitest
        //    harness's onConsoleWrite callback observes them.
        let echo_iov = Ciovec {
            buf: recv_buf.as_ptr(),
            buf_len: nread,
        };
        let mut echo_written: u32 = 0;
        let rc = fd_write(1, &echo_iov, 1, &mut echo_written);
        if rc != 0 {
            proc_exit(18);
        }

        // 10. clean exit.
        proc_exit(0);
    }
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { proc_exit(101) }
}
