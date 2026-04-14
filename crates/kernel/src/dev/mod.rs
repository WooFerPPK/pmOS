//! Device node dispatch.
//!
//! `devfs` (see `crate::fs::devfs`) exposes a fixed set of
//! character devices at `/dev/{null,zero,random,console,fb0,
//! input_kbd,input_mouse}`. At the VFS layer, each of these is
//! a `NodeType::CharDevice(DevNum)` with no direct read/write
//! support — the VFS's `devfs::read` and `devfs::write` return
//! `NotSupported`, deliberately. Real I/O on device fds comes
//! through this module.
//!
//! When a process opens a file under `/dev/` the kernel's
//! syscall layer (T071+) checks the inode's type; if it's a
//! `CharDevice(devnum)`, the fd is installed as a
//! `FdObject::CharDevice { devnum }` instead of a `Vnode(..)`.
//! Subsequent `fd_read` / `fd_write` calls dispatch here via
//! [`DeviceDispatcher::read`] / [`DeviceDispatcher::write`].
//!
//! Devices split into two categories:
//!
//! * **In-kernel** — `null`, `zero`, `random`, `console`. No
//!   driver call is needed; the kernel services the I/O
//!   itself. `/dev/random` fills with
//!   `Platform::random_bytes` so the sandbox's native-test
//!   path (deterministic xorshift) and the wasm runtime
//!   path (`crypto.getRandomValues`) both work.
//! * **Driver-backed** — `fb0`, `input_kbd`, `input_mouse`.
//!   Each I/O is a `Platform::driver_call` to the
//!   corresponding TypeScript driver module, plus a cap
//!   check (the framebuffer and input devices require
//!   [`Cap::DisplayServer`]).
//!
//! The [`DeviceDispatcher`] type holds per-device state that
//! is easy to express in kernel memory (the console ring
//! buffer, for instance) so the syscall layer has a single
//! place to call into.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use abi::cap::{Cap, CapSet};

use crate::fs::devfs::{
    DEV_CONSOLE, DEV_FB0, DEV_INPUT_KBD, DEV_INPUT_MOUSE, DEV_NULL, DEV_RANDOM, DEV_ZERO,
};
use crate::platform::{self, DevId, DriverError};

/// Errors returned by device dispatch.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DevError {
    /// Unknown devnum.
    UnknownDevice,
    /// The caller's cap set is missing a required cap for
    /// this device (e.g. `DisplayServer` for `/dev/fb0`).
    NotCapable,
    /// The device is open-only and the requested operation
    /// (read on a write-only device or vice versa) doesn't
    /// apply.
    NotSupported,
    /// A driver-backed device reported an error.
    DriverFailed,
    /// Non-blocking read returned no data. Caller converts
    /// to `EAGAIN`.
    WouldBlock,
}

/// Per-kernel device state. One lives on the kernel's top-
/// level struct; every `fd_read` / `fd_write` on a
/// `CharDevice` fd routes through here.
pub struct DeviceDispatcher {
    /// Input ring for the serial-style `/dev/console`. Bytes
    /// are pushed by the console driver (host-side postMessage
    /// from a hidden `<textarea>`, or an explicit test-
    /// harness injection) and popped by `read(/dev/console)`.
    console_input: VecDeque<u8>,
    /// Bytes written to `/dev/console` that the kernel has not
    /// yet handed to the platform's console output sink. This
    /// is a simple line-buffered sink — we flush whole UTF-8
    /// lines to `Platform::driver_call(Console, WRITE_LINE,
    /// ...)` but the write path only needs the buffer for
    /// tests that assert on raw bytes.
    console_output_sink: Vec<u8>,
    /// Input ring for keyboard events from `/dev/input/kbd`.
    /// Populated by the input driver.
    input_kbd: VecDeque<u8>,
    /// Input ring for mouse events from `/dev/input/mouse`.
    input_mouse: VecDeque<u8>,
}

impl DeviceDispatcher {
    pub const fn new() -> Self {
        DeviceDispatcher {
            console_input: VecDeque::new(),
            console_output_sink: Vec::new(),
            input_kbd: VecDeque::new(),
            input_mouse: VecDeque::new(),
        }
    }

    /// Inject raw bytes into the `/dev/console` input ring.
    /// Called by the host JS when the hidden `<textarea>`
    /// receives a line, and by the T077 headless-shell test
    /// harness to feed commands to `/bin/sh`.
    pub fn inject_console_input(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.console_input.push_back(b);
        }
    }

    /// Inject a single keyboard event record into
    /// `/dev/input/kbd`. Exposed for the display-server test
    /// harness.
    pub fn inject_kbd_event(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.input_kbd.push_back(b);
        }
    }

    /// Inject a single mouse event record into
    /// `/dev/input/mouse`.
    pub fn inject_mouse_event(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.input_mouse.push_back(b);
        }
    }

    /// Test helper: drain and return everything a process has
    /// written to `/dev/console` so far. The normal kernel
    /// flushes this into `Platform::driver_call(Console, ...)`;
    /// the native-test Platform is a no-op on driver_call so
    /// this buffer is the only way tests can observe the
    /// bytes.
    pub fn drain_console_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.console_output_sink)
    }

    /// Number of bytes currently available on the `/dev/console`
    /// input ring.
    pub fn console_input_len(&self) -> usize {
        self.console_input.len()
    }

    // --- Cap checks ---------------------------------------------------

    /// Check whether `caps` is allowed to open `devnum`. Called
    /// by the syscall layer at `path_open` time, before any
    /// read/write can happen.
    pub fn check_open(devnum: u32, caps: CapSet) -> Result<(), DevError> {
        match devnum {
            DEV_NULL | DEV_ZERO | DEV_RANDOM | DEV_CONSOLE => Ok(()),
            DEV_FB0 => {
                if caps.contains(Cap::DisplayServer) {
                    Ok(())
                } else {
                    Err(DevError::NotCapable)
                }
            }
            DEV_INPUT_KBD | DEV_INPUT_MOUSE => {
                if caps.contains(Cap::DisplayServer) {
                    Ok(())
                } else {
                    Err(DevError::NotCapable)
                }
            }
            _ => Err(DevError::UnknownDevice),
        }
    }

    // --- Read path ----------------------------------------------------

    /// Read up to `buf.len()` bytes from the device identified
    /// by `devnum`. Returns the number of bytes written to
    /// `buf`. Semantics are device-specific:
    ///
    /// * `null`   — always returns 0 (EOF).
    /// * `zero`   — fills `buf` with zeros.
    /// * `random` — fills `buf` with `Platform::random_bytes`.
    /// * `console`— drains up to `buf.len()` bytes from the
    ///              input ring. Returns `WouldBlock` if empty
    ///              (the syscall layer blocks the process on
    ///              a non-zero-len request against an empty
    ///              ring, or returns `EAGAIN` under NONBLOCK).
    /// * `fb0`    — write-only; returns `NotSupported`.
    /// * `input_kbd` / `input_mouse` — drains the ring like
    ///              console.
    pub fn read(&mut self, devnum: u32, buf: &mut [u8]) -> Result<usize, DevError> {
        match devnum {
            DEV_NULL => Ok(0),
            DEV_ZERO => {
                buf.fill(0);
                Ok(buf.len())
            }
            DEV_RANDOM => {
                platform::current().random_bytes(buf);
                Ok(buf.len())
            }
            DEV_CONSOLE => drain_ring(&mut self.console_input, buf),
            DEV_INPUT_KBD => drain_ring(&mut self.input_kbd, buf),
            DEV_INPUT_MOUSE => drain_ring(&mut self.input_mouse, buf),
            DEV_FB0 => Err(DevError::NotSupported),
            _ => Err(DevError::UnknownDevice),
        }
    }

    // --- Write path ---------------------------------------------------

    /// Write `buf` to the device. Returns the number of bytes
    /// accepted.
    ///
    /// * `null`   — discards, returns `buf.len()`.
    /// * `zero`   — writes are accepted and discarded (like
    ///              `/dev/null`).
    /// * `random` — read-only; returns `NotSupported`.
    /// * `console`— appends to `console_output_sink`, flushing
    ///              whole lines to `Platform::driver_call`.
    /// * `fb0`    — forwards to the framebuffer driver.
    /// * `input_*`— read-only; returns `NotSupported`.
    pub fn write(&mut self, devnum: u32, buf: &[u8]) -> Result<usize, DevError> {
        match devnum {
            DEV_NULL | DEV_ZERO => Ok(buf.len()),
            DEV_RANDOM => Err(DevError::NotSupported),
            DEV_CONSOLE => self.console_write(buf),
            DEV_FB0 => self.framebuffer_write(buf),
            DEV_INPUT_KBD | DEV_INPUT_MOUSE => Err(DevError::NotSupported),
            _ => Err(DevError::UnknownDevice),
        }
    }

    // --- Private helpers ---------------------------------------------

    fn console_write(&mut self, buf: &[u8]) -> Result<usize, DevError> {
        // Append to the in-kernel buffer.
        self.console_output_sink.extend_from_slice(buf);

        // Walk the buffer looking for complete lines. Each
        // whole line gets forwarded to the console driver as
        // a single driver_call; the trailing partial line
        // stays in the buffer until a newline arrives.
        let mut drained = 0usize;
        while let Some(nl) = self.console_output_sink[drained..].iter().position(|&b| b == b'\n') {
            let end = drained + nl + 1;
            let line = &self.console_output_sink[drained..end];
            // Fire and forget: the native Platform's driver_call
            // records the call without side effects; the wasm
            // Platform posts to the host console driver.
            let _ = platform::current().driver_call(DevId::Console, DEV_CONSOLE, line);
            drained = end;
        }
        if drained > 0 {
            self.console_output_sink.drain(..drained);
        }
        Ok(buf.len())
    }

    fn framebuffer_write(&mut self, buf: &[u8]) -> Result<usize, DevError> {
        // Forwards the whole buffer to the framebuffer driver.
        // Real production paths use a SAB ring (see
        // contracts/driver-kernel.md §3); this cold path is
        // for low-frequency ioctls like SET_MODE. The display
        // server's hot commit path (T100+) bypasses this and
        // talks to the FB driver via shared-memory rings.
        match platform::current().driver_call(DevId::Framebuffer, DEV_FB0, buf) {
            Ok(_) => Ok(buf.len()),
            Err(DriverError::NotReady) => Err(DevError::DriverFailed),
            Err(DriverError::Errno(_)) => Err(DevError::DriverFailed),
            Err(DriverError::Transport) => Err(DevError::DriverFailed),
        }
    }
}

impl Default for DeviceDispatcher {
    fn default() -> Self {
        DeviceDispatcher::new()
    }
}

/// Drain up to `buf.len()` bytes from `ring` into `buf`.
/// Returns the number of bytes copied. `Err(WouldBlock)` if
/// the ring is empty and the caller expected data.
fn drain_ring(ring: &mut VecDeque<u8>, buf: &mut [u8]) -> Result<usize, DevError> {
    if ring.is_empty() {
        if buf.is_empty() {
            return Ok(0);
        }
        return Err(DevError::WouldBlock);
    }
    let take = core::cmp::min(buf.len(), ring.len());
    for i in 0..take {
        buf[i] = ring.pop_front().unwrap();
    }
    Ok(take)
}
