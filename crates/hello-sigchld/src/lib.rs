//! Reads one signal record from fd 4 (the auto-installed
//! `FdObject::SignalChannel`) and echoes the two bytes to
//! `/dev/console`. Proves the signal-delivery pipeline end-to-end
//! through a real user wasm binary:
//!
//!   parent `PROC_KILL(child, SIGTERM)` → child's `SignalInbox`
//!   gains one `Signal::Term` → child `fd_read(4, buf)` drains
//!   it as a u16 LE signum → `fd_write(1, buf)` echoes to
//!   `/dev/console` → `onConsoleWrite` observes bytes.
//!
//! fd 4 is installed automatically by `proc_spawn` as
//! the per-process signal channel — no explicit `path_open` is
//! needed. A WASI `poll_oneoff` FD_READ wait on EAGAIN handles the race where a
//! parent's PROC_KILL hasn't yet landed on the child's inbox
//! when the child's first `fd_read` fires. Composition tests
//! stage the signal BEFORE drain, so the first read finds it and
//! returns immediately; the readiness wait is a no-op in that path.
//!
//! The 2-byte u16 LE record plus a trailing newline byte is what
//! the binary writes to stdout — the `/dev/console` driver is
//! line-buffered and only flushes complete lines to
//! `onConsoleWrite`, so without the newline the bytes would stay
//! in the kernel's buffer and never reach the host callback.
//!
//! Exit codes:
//!
//! * 0   = success — drained 2 bytes, echoed + newline
//! * 11  = fd_read returned a non-EAGAIN, non-zero errno
//! * 12  = fd_read returned 0 bytes (unexpected — signal record
//!   is always 2 bytes)
//! * 13  = fd_write to stdout failed or short-wrote
//! * 101 = panic

#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
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
        // fd 4 is the auto-installed SignalChannel (proc_spawn
        // installs it alongside stdio 0/1/2 — see 9fbe708).
        let mut buf = [0u8; 4];
        let read_iov = Iovec {
            buf: buf.as_mut_ptr(),
            buf_len: buf.len() as u32,
        };
        // EAGAIN is errno 6 per `abi::errno`. Keeping the literal
        // here mirrors hello-input-echo's approach.
        const EAGAIN: i32 = 6;
        let mut nread: u32 = 0;
        loop {
            let rc = fd_read(4, &read_iov, 1, &mut nread);
            if rc == 0 && nread > 0 {
                break;
            }
            if rc == 0 {
                // 0-byte success on SignalChannel is unexpected —
                // the kernel returns EAGAIN on empty inboxes and
                // always writes full 2-byte records when signals
                // are pending.
                proc_exit(12);
            }
            if rc == EAGAIN {
                nread = 0;
                if wait_readable(4).is_err() {
                    proc_exit(11);
                }
                continue;
            }
            proc_exit(11);
        }

        // Echo the 2-byte u16 LE signum record back to stdout, plus
        // a trailing newline so the line-buffered console flushes
        // the record to `onConsoleWrite`. Everything after the
        // read `nread` bytes is the newline — keep the write_buf
        // sized to nread + 1 so the flush is deterministic.
        let mut write_buf = [0u8; 5];
        // `buf` holds the u16 LE record at [0..2]. Copy it plus a
        // newline so fd_write delivers a complete line in one shot.
        let copy_len = nread as usize;
        write_buf[..copy_len].copy_from_slice(&buf[..copy_len]);
        write_buf[copy_len] = b'\n';
        let total_len = (copy_len + 1) as u32;
        let write_iov = Ciovec {
            buf: write_buf.as_ptr(),
            buf_len: total_len,
        };
        let mut nwritten: u32 = 0;
        let rc = fd_write(1, &write_iov, 1, &mut nwritten);
        if rc != 0 || nwritten != total_len {
            proc_exit(13);
        }

        proc_exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::readable_subscription;

    #[test]
    fn signal_wait_uses_one_exact_fd_read_subscription() {
        let subscription = readable_subscription(4);
        assert_eq!(subscription[8], 1);
        assert_eq!(
            u32::from_le_bytes(subscription[16..20].try_into().unwrap()),
            4,
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
