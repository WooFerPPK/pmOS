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
    use shell::{run_desktop_shell, Taskbar, DEFAULT_LAUNCHER_SLOTS};
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
        fn sched_yield() -> i32;
        fn proc_exit(rval: i32) -> !;
    }

    #[link(wasm_import_module = "pmos_ext")]
    extern "C" {
        /// Spawn a child process. Returns a positive child
        /// pid on success, negative errno on failure.
        fn proc_spawn(path_ptr: *const u8, path_len: u32, caps: u64) -> i32;
        /// Reap a zombie child. With WNOHANG (options=1) returns
        /// -EAGAIN if no zombie matches the target. With
        /// target=-1 (WAIT_ANY) reaps any child.
        fn proc_wait(target_pid: i32, options: i32, status_out_ptr: i32) -> i32;
    }

    /// Drain every zombie child the shell can see. Called on a
    /// schedule from the desktop event loop so that spawned
    /// apps that exit (closed by the user, crashed, exited on
    /// their own) don't accumulate zombie process-table
    /// entries forever. WNOHANG = 1; -EAGAIN means "no zombies
    /// right now"; positive return is the reaped pid.
    pub fn shell_reap_zombies() {
        let mut status_out: i64 = 0;
        let status_ptr = &mut status_out as *mut i64 as i32;
        loop {
            let rc = unsafe { proc_wait(-1, 1, status_ptr) };
            if rc <= 0 {
                // Either -EAGAIN (no zombies), -ECHILD (no
                // children at all), or some other error. Stop
                // reaping until next tick.
                return;
            }
            // Successfully reaped pid `rc`; loop again to drain
            // any other zombies in one tick.
        }
    }

    /// Closure-friendly wrapper the shell library calls when
    /// the launcher dispatches a row. Maps each known exec
    /// path to a cap set the kernel will accept (the parent
    /// shell holds DESKTOP_SHELL caps; ORDINARY_APP is a
    /// strict subset for plain apps; the privileged settings
    /// app gets KEYMAP_ADMIN delegated).
    pub fn shell_spawn(path: &str) -> i32 {
        // Default to ORDINARY_APP (DisplayClient only). The
        // settings binary needs KEYMAP_ADMIN delegated since
        // its purpose is to switch the system keymap.
        let caps = if path == "/bin/settings" {
            abi::cap::initial::SETTINGS.0
        } else if path == "/bin/sysmon" {
            abi::cap::initial::SYSMON.0
        } else {
            abi::cap::initial::ORDINARY_APP.0
        };
        unsafe { proc_spawn(path.as_ptr(), path.len() as u32, caps) }
    }

    /// EAGAIN errno code (matches abi::errno::EAGAIN).
    const EAGAIN: i32 = 6;
    /// EINVAL errno code (positive form per WASI). Returned
    /// by `fd_write` while the socket is still `Connecting`
    /// — the server hasn't called `ipc_accept` yet.
    const EINVAL: i32 = 28;
    /// Max busy-poll iterations FdConnection::recv waits
    /// for bytes before returning empty. Tuned so a fresh
    /// connect → bind → reply round-trip lands within the
    /// poll budget on a reasonably loaded substrate.
    const RECV_MAX_POLLS: u32 = 50_000;

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
        /// True after the toolkit's connect handshake
        /// completes. Before that, `recv()` busy-polls the
        /// EAGAIN response so the toolkit's "drain until
        /// empty" sees the server's globals advertisement
        /// (which the kernel may not have routed yet on the
        /// very first recv call). After connect, `recv()`
        /// is single-shot non-blocking — the shell's main
        /// loop is responsible for spinning.
        connected: core::cell::Cell<bool>,
    }

    impl FdConnection {
        pub fn new(fd: i32) -> Self {
            FdConnection { fd, connected: core::cell::Cell::new(false) }
        }
    }

    impl Connection for FdConnection {
        fn send(&mut self, bytes: &[u8]) {
            if bytes.is_empty() {
                return;
            }
            // Drive the full byte slice to completion. The
            // kernel's `send_on_socket` returns whatever fits
            // in the peer's rx_buf RIGHT NOW (cap is 64 KiB);
            // a single write of >64 KiB OR a stream of writes
            // faster than the peer drains will see partial
            // sends with `nwritten < bytes.len()`. The toolkit's
            // protocol layer assumes its messages land
            // atomically, so this loop must keep going until
            // every byte is on the wire.
            //
            // EAGAIN: rx_buf full → busy-poll with sched_yield
            // until the peer drains.
            // EINVAL: socket still in `Connecting` (race with
            // server's ipc_accept) → busy-poll same as EAGAIN.
            // Real I/O error: drop the rest. Even though that
            // corrupts the protocol stream, there's no recovery
            // path from here — the toolkit's request encoder
            // doesn't track checkpoints, so a partial send of
            // a chunked shm_pool.write would desynchronise the
            // server's parser. The previous "give up after 50K
            // spins" semantic was the entire wedge-cause: a
            // brief rx_buf saturation under load turned into
            // dropped bytes turned into a stuck protocol
            // stream. Spin forever now; the substrate's
            // cooperative scheduler will eventually run the
            // peer.
            let mut sent = 0usize;
            while sent < bytes.len() {
                let remaining = &bytes[sent..];
                let iov = Ciovec {
                    buf: remaining.as_ptr(),
                    buf_len: remaining.len() as u32,
                };
                let mut nwritten: u32 = 0;
                let rc = unsafe { fd_write(self.fd, &iov, 1, &mut nwritten) };
                if rc == 0 && nwritten > 0 {
                    sent += nwritten as usize;
                    continue;
                }
                if rc == 0 || rc == EAGAIN || rc == EINVAL {
                    unsafe { let _ = sched_yield(); }
                    continue;
                }
                // Real I/O error (peer closed, etc) — give up.
                return;
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
            if !self.connected.get() {
                // Pre-handshake: busy-poll because the
                // toolkit's App::connect breaks on the first
                // empty chunk and we need to give the server
                // a chance to advertise its globals before
                // we report "nothing here". The first
                // non-empty chunk transitions us into
                // post-handshake mode automatically.
                for _ in 0..RECV_MAX_POLLS {
                    let rc = unsafe { fd_read(self.fd, &iov, 1, &mut nread) };
                    if rc == 0 && nread > 0 {
                        self.connected.set(true);
                        return buf[..nread as usize].to_vec();
                    }
                    if rc == 0 || rc == EAGAIN {
                        nread = 0;
                        unsafe { let _ = sched_yield(); }
                        continue;
                    }
                    return alloc::vec::Vec::new();
                }
                return alloc::vec::Vec::new();
            }
            // Post-handshake: single non-blocking read. The
            // shell's main event loop calls recv() in a
            // tight loop and handles the empty case itself;
            // busy-polling here multiplies the per-iteration
            // cost by RECV_MAX_POLLS (50 000) — each poll
            // round-trips through the SAB ring → kernel-
            // worker → response, so a no-op recv burns tens
            // of seconds of wall time and starves the rest
            // of the desktop. The watchdog (8s of zero
            // console output) trips on this every time.
            let rc = unsafe { fd_read(self.fd, &iov, 1, &mut nread) };
            if rc == 0 && nread > 0 {
                return buf[..nread as usize].to_vec();
            }
            alloc::vec::Vec::new()
        }
    }

    /// ECONNREFUSED: the kernel reports it as positive errno 14
    /// for ext syscalls (negated by the shim → -14 on the wire).
    /// Returned when the listener at `/run/display` doesn't exist
    /// yet (display-server hasn't called `display_bind`).
    const ECONNREFUSED: i32 = 14;
    /// Bounded retry budget for the connect-poll loop. With each
    /// iteration calling sched_yield + a short syscall, this gives
    /// the kernel-worker plenty of room to schedule display-server's
    /// startup (display_bind + path_open + the loop entry) before
    /// the shell gives up.
    const CONNECT_MAX_POLLS: u32 = 50_000;

    pub fn run() {
        println!("shell: starting");
        // Retry on ECONNREFUSED while display-server is starting up
        // — the shell and display-server are spawned almost
        // simultaneously by init-desktop, and the shell's
        // display_connect can race the server's display_bind. Same
        // retry pattern as crates/display-client-demo/src/main.rs.
        let mut fd: i32 = -1;
        for _ in 0..CONNECT_MAX_POLLS {
            let rc = unsafe { display_connect() };
            if rc >= 0 {
                fd = rc;
                break;
            }
            if rc == -ECONNREFUSED {
                unsafe {
                    let _ = sched_yield();
                }
                continue;
            }
            // Other errors are fatal — log via proc_exit's exit
            // code (the negated errno).
            unsafe { proc_exit(-rc) };
        }
        if fd < 0 {
            // Connect-poll exhausted without ever seeing the
            // server bind. Exit with ECONNREFUSED so init-desktop
            // logs the cause.
            unsafe { proc_exit(ECONNREFUSED) };
        }
        let conn = FdConnection::new(fd);
        let taskbar = Taskbar::new(0, 0);
        println!("shell: connected to /run/display");
        match run_desktop_shell(
            conn,
            u32::MAX,
            taskbar,
            DEFAULT_LAUNCHER_SLOTS,
            shell_spawn,
            shell_reap_zombies,
        ) {
            Ok(_) => unsafe { proc_exit(0) },
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
