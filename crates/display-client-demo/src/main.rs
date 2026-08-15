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
//! * 0  = success
//! * 10 = `display_connect` poll loop exhausted (no server
//!   bound `/run/display` in time)
//! * 11 = `fd_write` on the client fd failed or short-wrote
//! * 12 = `display_connect` returned a non-ECONNREFUSED error
//!   (e.g. missing `DisplayClient` cap)

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
    fn poll_oneoff(
        subscriptions: *const u8,
        events: *mut u8,
        nsubscriptions: u32,
        nevents: *mut u32,
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
const SUBSCRIPTION_SIZE: usize = 48;
#[cfg(target_arch = "wasm32")]
const EVENT_SIZE: usize = 32;

#[cfg(target_arch = "wasm32")]
unsafe fn wait_clock(timeout_ns: u64) -> Result<(), i32> {
    let mut subscription = [0u8; SUBSCRIPTION_SIZE];
    subscription[8] = 0;
    subscription[16..20].copy_from_slice(&1u32.to_le_bytes());
    subscription[24..32].copy_from_slice(&timeout_ns.to_le_bytes());
    wait_subscription(&subscription, false)
}

#[cfg(target_arch = "wasm32")]
unsafe fn wait_writable(fd: i32) -> Result<(), i32> {
    let mut subscription = [0u8; SUBSCRIPTION_SIZE];
    subscription[8] = 2;
    subscription[16..20].copy_from_slice(&(fd as u32).to_le_bytes());
    wait_subscription(&subscription, true)
}

#[cfg(target_arch = "wasm32")]
unsafe fn wait_subscription(
    subscription: &[u8; SUBSCRIPTION_SIZE],
    check_hangup: bool,
) -> Result<(), i32> {
    let mut event = [0u8; EVENT_SIZE];
    let mut nevents = 0u32;
    let errno = poll_oneoff(subscription.as_ptr(), event.as_mut_ptr(), 1, &mut nevents);
    if errno != 0 {
        return Err(errno);
    }
    if nevents != 1 {
        return Err(29 /* EIO */);
    }
    let event_errno = u16::from_le_bytes([event[8], event[9]]) as i32;
    if event_errno != 0 {
        return Err(event_errno);
    }
    if check_hangup && u16::from_le_bytes([event[24], event[25]]) & 1 != 0 {
        return Err(64 /* EPIPE */);
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn main() {
    println!("display-client-demo starting");

    // ECONNREFUSED is positive `abi::errno::ECONNREFUSED = 14`.
    // PMos extension syscalls negate into `-errno` on error.
    const ECONNREFUSED: i32 = 14;
    // WASI fd I/O returns EAGAIN while an asynchronously connected
    // socket is still waiting for the server's `ipc_accept`.
    const CONNECT_RETRIES: u32 = 500;
    const CONNECT_RETRY_NS: u64 = 10_000_000;
    const EAGAIN: i32 = 6;

    unsafe {
        let mut client: i32 = -1;
        for _ in 0..CONNECT_RETRIES {
            let rc = display_connect();
            if rc >= 0 {
                client = rc;
                break;
            }
            if rc == -ECONNREFUSED {
                if wait_clock(CONNECT_RETRY_NS).is_err() {
                    std::process::exit(10);
                }
                continue;
            }
            std::process::exit(12);
        }
        if client < 0 {
            std::process::exit(10);
        }

        let mut offset = 0usize;
        while offset < PIXELS.len() {
            let remaining = &PIXELS[offset..];
            let write_iov = Ciovec {
                buf: remaining.as_ptr(),
                buf_len: remaining.len() as u32,
            };
            let mut nwritten = 0u32;
            let rc = fd_write(client, &write_iov, 1, &mut nwritten);
            if rc == 0 && nwritten > 0 {
                offset += nwritten as usize;
                continue;
            }
            if rc == EAGAIN || (rc == 0 && nwritten == 0) {
                if wait_writable(client).is_err() {
                    std::process::exit(11);
                }
                continue;
            }
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
