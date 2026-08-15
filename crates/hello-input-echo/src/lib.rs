//! Reads one keyboard event from `/dev/input_kbd` and echoes the
//! bytes to `/dev/console`. Proves the input path end-to-end:
//!
//!   TS `KernelWasmHost.injectInput(Devnum.InputKbd, bytes)` →
//!   `kernel_inject_input_kbd` → `DeviceDispatcher::inject_kbd_event` →
//!   `/dev/input_kbd` input ring → user wasm `fd_read` →
//!   `fd_write(1, ...)` → `onConsoleWrite`.
//!
//! The binary parks in WASI `poll_oneoff` on EAGAIN so it survives the race where
//! input arrives after the process starts — the browser keydown path
//! injects bytes asynchronously, and the user Worker may run `fd_read`
//! before the first keypress reaches the kernel. Existing in-process
//! composition tests inject bytes BEFORE spawning, so their first
//! `fd_read` finds the ring non-empty and returns immediately; the
//! readiness wait is a no-op in that path.
//!
//! Exit codes:
//!
//!   * 0  = success — read N bytes, echoed all of them
//!   * 10 = path_open("/dev/input_kbd") failed (missing cap?)
//!   * 11 = fd_read returned a non-EAGAIN, non-zero errno
//!   * 13 = fd_write to stdout failed or short-wrote
//!   * 101 = panic

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn path_open(
        dirfd: i32,
        dirflags: i32,
        path_ptr: *const u8,
        path_len: i32,
        oflags: i32,
        fs_rights_base: i64,
        fs_rights_inheriting: i64,
        fdflags: i32,
        fd_out_ptr: *mut u32,
    ) -> i32;
    fn fd_read(fd: i32, iovs_ptr: *const Iovec, iovs_len: i32, nread_ptr: *mut u32) -> i32;
    fn fd_write(fd: i32, iovs_ptr: *const Ciovec, iovs_len: i32, nwritten_ptr: *mut u32) -> i32;
    fn poll_oneoff(
        subscriptions: *const u8,
        events: *mut u8,
        nsubscriptions: u32,
        nevents: *mut u32,
    ) -> i32;
    fn proc_exit(rval: i32) -> !;
}

#[cfg(any(target_arch = "wasm32", test))]
const SUBSCRIPTION_SIZE: usize = 48;
#[cfg(target_arch = "wasm32")]
const EVENT_SIZE: usize = 32;

#[cfg(any(target_arch = "wasm32", test))]
fn readable_subscription(fd: i32) -> [u8; SUBSCRIPTION_SIZE] {
    let mut subscription = [0u8; SUBSCRIPTION_SIZE];
    subscription[8] = 1;
    subscription[16..20].copy_from_slice(&(fd as u32).to_le_bytes());
    subscription
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

#[cfg(target_arch = "wasm32")]
unsafe fn wait_readable(fd: i32) -> Result<(), i32> {
    const EIO: i32 = 29;
    const EPIPE: i32 = 64;
    let subscription = readable_subscription(fd);
    let mut event = [0u8; EVENT_SIZE];
    let mut nevents = 0u32;
    let errno = poll_oneoff(subscription.as_ptr(), event.as_mut_ptr(), 1, &mut nevents);
    if errno != 0 {
        return Err(errno);
    }
    if nevents != 1 {
        return Err(EIO);
    }
    let event_errno = u16::from_le_bytes([event[8], event[9]]) as i32;
    if event_errno != 0 {
        return Err(event_errno);
    }
    let flags = u16::from_le_bytes([event[24], event[25]]);
    if flags & 1 != 0 {
        return Err(EPIPE);
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub extern "C" fn _start() {
    unsafe {
        // v1 devfs is flat — the kbd node lives at /dev/input_kbd,
        // not /dev/input/kbd. See crates/kernel/src/fs/devfs.rs.
        const KBD_PATH: &[u8] = b"/dev/input_kbd";
        let mut kbd_fd: u32 = 0;
        let rc = path_open(
            0,
            0,
            KBD_PATH.as_ptr(),
            KBD_PATH.len() as i32,
            0,
            0,
            0,
            0,
            &mut kbd_fd,
        );
        if rc != 0 {
            proc_exit(10);
        }

        let mut buf = [0u8; 64];
        let read_iov = Iovec {
            buf: buf.as_mut_ptr(),
            buf_len: buf.len() as u32,
        };
        // EAGAIN is errno 6 per `abi::errno` — keeping the literal here
        // avoids pulling in an extra crate for a single constant in a
        // no_std cdylib. The kernel returns EAGAIN on an empty input
        // ring; poll_oneoff parks the Worker until bytes land.
        const EAGAIN: i32 = 6;
        let mut nread: u32 = 0;
        loop {
            let rc = fd_read(kbd_fd as i32, &read_iov, 1, &mut nread);
            if rc == 0 && nread > 0 {
                break;
            }
            if rc == 0 || rc == EAGAIN {
                nread = 0;
                if wait_readable(kbd_fd as i32).is_err() {
                    proc_exit(11);
                }
                continue;
            }
            proc_exit(11);
        }

        // Echo the exact bytes we read back to stdout.
        let write_iov = Ciovec {
            buf: buf.as_ptr(),
            buf_len: nread,
        };
        let mut nwritten: u32 = 0;
        let rc = fd_write(1, &write_iov, 1, &mut nwritten);
        if rc != 0 || nwritten != nread {
            proc_exit(13);
        }

        proc_exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::readable_subscription;

    #[test]
    fn keyboard_wait_uses_one_exact_fd_read_subscription() {
        let subscription = readable_subscription(27);
        assert_eq!(subscription[8], 1);
        assert_eq!(
            u32::from_le_bytes(subscription[16..20].try_into().unwrap()),
            27,
        );
        assert!(subscription[..8].iter().all(|byte| *byte == 0));
        assert!(subscription[20..].iter().all(|byte| *byte == 0));
    }
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { proc_exit(101) }
}
