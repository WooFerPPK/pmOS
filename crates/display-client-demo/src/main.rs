//! PMos display-client-demo binary — real IPC client for `display-server`.
//!
//! First binary in the workspace whose only job is to connect to
//! `/run/display` and hand a pixel payload to whatever process
//! owns the server side of the socket. Paired with
//! `/bin/display-server`: init spawns both, display-server binds
//! and sits in an `ipc_accept` poll loop, display-client-demo
//! connects + writes the 16-byte RGBA payload + exits. This slice
//! is the first time two separate user Workers with distinct
//! WASM linear memories actually exchange bytes through a PMos
//! IPC socket.
//!
//! Flow:
//!   1. `display_connect()` (ECONNREFUSED poll) — retry until
//!      display-server has bound `/run/display`.
//!   2. `fd_write(client, PIXELS)`               — hand the 16-byte
//!      RGBA payload to the server.
//!   3. fall off the end of `main`                — std emits
//!      `__wasi_proc_exit(0)`.
//!
//! The connect poll is bounded so the vitest in-process
//! composition helper (`runAllSpawns`, strictly sequential)
//! doesn't spin forever when display-server hasn't bound
//! (display-server runs BEFORE display-client-demo under
//! sequential scheduling, bounded-exits via its own poll loop,
//! and tears its listener down before display-client-demo ever
//! starts). Under production Playwright (concurrent Workers)
//! display-server's bind lands within the first handful of
//! dispatch passes, long before the poll bound.
//!
//! Exit codes:
//!
//!   * 0  = success
//!   * 10 = `display_connect` poll loop exhausted (no server
//!          bound `/run/display` in time)
//!   * 11 = `fd_write` on the client fd failed or short-wrote
//!   * 12 = `display_connect` returned a non-ECONNREFUSED error
//!          (e.g. missing `DisplayClient` cap)

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(
        fd: i32,
        iovs_ptr: *const Ciovec,
        iovs_len: i32,
        nwritten_ptr: *mut u32,
    ) -> i32;
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn display_connect() -> i32;
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct Ciovec {
    buf: *const u8,
    buf_len: u32,
}

/// Four RGBA pixels: red, green, blue, white — shared fixture with
/// `display-server-lite` and the composition tests. The server
/// relays these verbatim to `/dev/fb0`, and both the vitest and
/// Playwright assertions pin this exact byte sequence.
#[cfg(target_arch = "wasm32")]
const PIXELS: [u8; 16] = [
    0xff, 0x00, 0x00, 0xff, // red
    0x00, 0xff, 0x00, 0xff, // green
    0x00, 0x00, 0xff, 0xff, // blue
    0xff, 0xff, 0xff, 0xff, // white
];

#[cfg(target_arch = "wasm32")]
fn main() {
    println!("display-client-demo starting");

    // ECONNREFUSED is positive `abi::errno::ECONNREFUSED = 14`.
    // PMos extension syscalls negate into `-errno` on error.
    const ECONNREFUSED: i32 = 14;
    // EINVAL (positive 28) is what `fd_write` returns while the
    // client socket is still `Connecting` (the server hasn't
    // called `ipc_accept` yet). WASI syscalls surface errno
    // positive on error; PMos extension syscalls negate.
    const EINVAL: i32 = 28;
    // Safety valve: bounded iteration count so the vitest
    // in-process composition test doesn't spin forever when no
    // server exists. Real Playwright lands a connection within
    // the first handful of passes.
    const MAX_POLLS: u32 = 10_000;

    unsafe {
        let mut client: i32 = -1;
        for _ in 0..MAX_POLLS {
            let rc = display_connect();
            if rc >= 0 {
                client = rc;
                break;
            }
            if rc == -ECONNREFUSED {
                continue;
            }
            std::process::exit(12);
        }
        if client < 0 {
            std::process::exit(10);
        }

        let write_iov = Ciovec {
            buf: PIXELS.as_ptr(),
            buf_len: PIXELS.len() as u32,
        };
        let mut nwritten: u32 = 0;
        // Small EINVAL retry loop covers the narrow race where
        // this process's `fd_write` reaches the dispatcher before
        // display-server's `ipc_accept` has promoted the client
        // socket from `Connecting` to `Connected`. In practice
        // Worker wake-up latency keeps dispatch ordering stable
        // (accept lands several passes before this write), but
        // bounding the retry is cheap insurance.
        let mut wrote = false;
        for _ in 0..MAX_POLLS {
            let rc = fd_write(client, &write_iov, 1, &mut nwritten);
            if rc == 0 && nwritten == PIXELS.len() as u32 {
                wrote = true;
                break;
            }
            if rc == EINVAL {
                nwritten = 0;
                continue;
            }
            std::process::exit(11);
        }
        if !wrote {
            std::process::exit(11);
        }
    }

    println!("display-client-demo sent pixels");
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    // Native stub so `cargo test --workspace` + `cargo build
    // --workspace` link the bin target. The WASM target is the
    // only one the slice exercises; everything above is behind
    // `#[cfg(target_arch = "wasm32")]`.
}
