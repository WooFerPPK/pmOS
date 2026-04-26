//! PMos desktop shell — binary entry point.
//!
//! Production main: open `/run/display` via the kernel's
//! `pmos_ext.display_connect` extension syscall, wrap the
//! returned fd in an [`FdConnection`] adapter, and call
//! [`shell::run_shell_with_taskbar`] with a freshly-allocated
//! [`shell::Taskbar`]. The toolkit's protocol layer drives
//! the event loop; the FdConnection bridges its `send` /
//! `recv` calls to `fd_write` / `fd_read` syscalls.
//!
//! The native-host build (used by `cargo test -p shell`)
//! falls back to a no-op `MemoryConnection` so the binary
//! links in the workspace's host target. The fd-syscall path
//! is gated on `target_arch = "wasm32"` since the WASI ABI
//! shims aren't reachable from the host `cargo` target.

use shell::{run_shell, run_shell_with_taskbar, ShellExit, Taskbar};
use toolkit::{ClientError, MemoryConnection};

// Anchor references to the library's entry points so
// `just build` doesn't DCE the symbols out of the WASM
// binary even though `fn main` only invokes the production
// path on wasm32. Keeping the host-side function pointers
// alive catches any downstream link breakage early.
#[used]
static _KEEP_RUN_SHELL: fn(MemoryConnection, u32) -> Result<ShellExit, ClientError> =
    run_shell::<MemoryConnection>;
#[used]
static _KEEP_RUN_SHELL_WITH_TASKBAR: fn(
    MemoryConnection,
    u32,
    Taskbar,
) -> Result<ShellExit, ClientError> = run_shell_with_taskbar::<MemoryConnection>;

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    use shell::{run_shell_with_taskbar, ShellExit, Taskbar};
    use toolkit::protocol::Connection;

    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn fd_read(
            fd: i32,
            iovs_ptr: *const Iovec,
            iovs_len: i32,
            nread_ptr: *mut u32,
        ) -> i32;
        fn fd_write(
            fd: i32,
            iovs_ptr: *const Ciovec,
            iovs_len: i32,
            nwritten_ptr: *mut u32,
        ) -> i32;
        fn proc_exit(rval: i32) -> !;
    }

    #[link(wasm_import_module = "pmos_ext")]
    extern "C" {
        /// Connect to `/run/display`. Returns a non-negative
        /// fd on success or a negative errno on failure
        /// (matches the pmos_ext signed-i32 convention).
        fn display_connect() -> i32;
    }

    #[repr(C)]
    struct Ciovec {
        buf: *const u8,
        buf_len: u32,
    }

    #[repr(C)]
    struct Iovec {
        buf: *mut u8,
        buf_len: u32,
    }

    /// Connection adapter that wraps a wasi fd. `send`
    /// pushes bytes via `fd_write`; `drain_outbound` returns
    /// the bytes the toolkit layer queued via `send` since
    /// the last drain (the toolkit then forwards them to
    /// the wire — but here `send` already wrote them
    /// directly, so `drain_outbound` is a noop). `recv`
    /// reads up to a 4 KiB chunk via `fd_read`; an empty
    /// vec means "no bytes available" but in v1 `fd_read`
    /// is BLOCKING so the call doesn't return until the
    /// server emits at least one byte.
    pub struct FdConnection {
        fd: i32,
    }

    impl FdConnection {
        pub fn new(fd: i32) -> Self {
            FdConnection { fd }
        }
    }

    impl Connection for FdConnection {
        fn send(&mut self, bytes: &[u8]) {
            if bytes.is_empty() {
                return;
            }
            let iov = Ciovec {
                buf: bytes.as_ptr(),
                buf_len: bytes.len() as u32,
            };
            let mut nwritten: u32 = 0;
            unsafe {
                let _ = fd_write(self.fd, &iov, 1, &mut nwritten);
            }
        }

        fn drain_outbound(&mut self) -> alloc::vec::Vec<u8> {
            // FdConnection::send writes synchronously, so
            // there's nothing buffered to drain on this
            // side. The toolkit's outbound queue is
            // separately drained by Client::drain_outbound;
            // FdConnection's job is just to flush each
            // chunk through the fd as `send` is called.
            alloc::vec::Vec::new()
        }

        fn recv(&mut self) -> alloc::vec::Vec<u8> {
            let mut buf = [0u8; 4096];
            let iov = Iovec {
                buf: buf.as_mut_ptr(),
                buf_len: buf.len() as u32,
            };
            let mut nread: u32 = 0;
            let rc = unsafe { fd_read(self.fd, &iov, 1, &mut nread) };
            if rc != 0 || nread == 0 {
                return alloc::vec::Vec::new();
            }
            buf[..nread as usize].to_vec()
        }
    }

    pub fn run() {
        let fd = unsafe { display_connect() };
        if fd < 0 {
            // display_connect failed — bail with the negated
            // errno so the kernel logs the failure cause.
            unsafe { proc_exit(-fd) };
        }
        let conn = FdConnection::new(fd);
        let taskbar = Taskbar::new(0, 0);
        match run_shell_with_taskbar(conn, u32::MAX, taskbar) {
            Ok(ShellExit::CloseRequested) => unsafe { proc_exit(0) },
            Ok(ShellExit::IterationLimit) => unsafe { proc_exit(0) },
            Err(_) => unsafe { proc_exit(1) },
        }
    }
}

#[cfg(target_arch = "wasm32")]
extern crate alloc;

fn main() {
    #[cfg(target_arch = "wasm32")]
    wasm_main::run();
    #[cfg(not(target_arch = "wasm32"))]
    println!("shell (host build): use `cargo build --target wasm32-wasip1 -p shell` for the production binary");
}
