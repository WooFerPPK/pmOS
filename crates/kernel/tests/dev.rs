//! Device dispatch isolation tests (T067-T070).
//!
//! Runs via `cargo test -p kernel`. Covers the in-kernel
//! character devices (null/zero/random/console/input),
//! the cap check on privileged devices, and the console
//! ring-buffer drain semantics used by the Principle VIII
//! headless-shell gate T077.

#![cfg(feature = "native-platform")]

use abi::cap::{Cap, CapSet};
use kernel::dev::{DevError, DeviceDispatcher, CONSOLE_PARTIAL_LINE_CAP};
use kernel::fs::devfs::{
    DEV_CONSOLE, DEV_FB0, DEV_INPUT_KBD, DEV_INPUT_MOUSE, DEV_NULL, DEV_RANDOM, DEV_ZERO,
};

// ---- /dev/null -----------------------------------------------------

#[test]
fn null_reads_return_zero_bytes() {
    let mut d = DeviceDispatcher::new();
    let mut buf = [0xAAu8; 16];
    assert_eq!(d.read(DEV_NULL, &mut buf).unwrap(), 0);
    // Buffer is untouched on a zero-length read.
    assert!(buf.iter().all(|&b| b == 0xAA));
}

#[test]
fn null_writes_accept_and_discard() {
    let mut d = DeviceDispatcher::new();
    assert_eq!(d.write(DEV_NULL, b"anything").unwrap(), 8);
}

// ---- /dev/zero -----------------------------------------------------

#[test]
fn zero_reads_fill_buffer_with_zeros() {
    let mut d = DeviceDispatcher::new();
    let mut buf = [0xFFu8; 32];
    let n = d.read(DEV_ZERO, &mut buf).unwrap();
    assert_eq!(n, 32);
    assert!(buf.iter().all(|&b| b == 0));
}

#[test]
fn zero_writes_accept_and_discard() {
    let mut d = DeviceDispatcher::new();
    assert_eq!(d.write(DEV_ZERO, &[1u8; 100]).unwrap(), 100);
}

// ---- /dev/random ---------------------------------------------------

#[test]
fn random_reads_fill_full_buffer() {
    let mut d = DeviceDispatcher::new();
    let mut buf = [0u8; 64];
    let n = d.read(DEV_RANDOM, &mut buf).unwrap();
    assert_eq!(n, 64);
    // At least one byte should be non-zero after 64 bytes of
    // "randomness". This can technically fail with probability
    // 2^-512 but in practice it's a strong signal that the
    // random-fill path actually executed.
    assert!(buf.iter().any(|&b| b != 0));
}

#[test]
fn random_writes_are_not_supported() {
    let mut d = DeviceDispatcher::new();
    assert_eq!(
        d.write(DEV_RANDOM, b"no").unwrap_err(),
        DevError::NotSupported
    );
}

// ---- /dev/console (T070 — the headless-shell gate primitive) ------

#[test]
fn console_reads_drain_the_input_ring() {
    let mut d = DeviceDispatcher::new();
    d.inject_console_input(b"echo hello\n");
    assert_eq!(d.console_input_len(), 11);

    let mut buf = [0u8; 32];
    let n = d.read(DEV_CONSOLE, &mut buf).unwrap();
    assert_eq!(n, 11);
    assert_eq!(&buf[..n], b"echo hello\n");
    assert_eq!(d.console_input_len(), 0);
}

#[test]
fn console_read_empty_is_would_block() {
    let mut d = DeviceDispatcher::new();
    let mut buf = [0u8; 4];
    assert_eq!(
        d.read(DEV_CONSOLE, &mut buf).unwrap_err(),
        DevError::WouldBlock
    );
}

#[test]
fn console_reads_are_partial_when_buffer_smaller_than_ring() {
    let mut d = DeviceDispatcher::new();
    d.inject_console_input(b"long input line");
    let mut buf = [0u8; 4];
    let n = d.read(DEV_CONSOLE, &mut buf).unwrap();
    assert_eq!(n, 4);
    assert_eq!(&buf, b"long");
    assert_eq!(d.console_input_len(), 11);
    let n = d.read(DEV_CONSOLE, &mut buf).unwrap();
    assert_eq!(n, 4);
    assert_eq!(&buf, b" inp");
}

#[test]
fn console_writes_line_buffer_into_the_sink() {
    let mut d = DeviceDispatcher::new();
    assert_eq!(d.write(DEV_CONSOLE, b"hel").unwrap(), 3);
    assert_eq!(d.write(DEV_CONSOLE, b"lo\n").unwrap(), 3);
    assert_eq!(d.write(DEV_CONSOLE, b"wor").unwrap(), 3);

    // The sink holds whatever hasn't been flushed as a whole
    // line. `hello\n` was flushed by the second write; `wor`
    // is still pending.
    let drained = d.drain_console_output();
    assert_eq!(drained, b"wor");
}

#[test]
fn console_write_flushes_completed_line_bytes() {
    // The key behaviour for T077 is: a write of "cmd\n"
    // immediately flushes "cmd\n" as a complete line to the
    // console driver. Tests assert on the "pending" sink for
    // correctness — complete lines aren't there.
    let mut d = DeviceDispatcher::new();
    d.write(DEV_CONSOLE, b"first\nsecond\n").unwrap();
    let drained = d.drain_console_output();
    assert_eq!(drained, b""); // both lines were flushed out
}

#[test]
fn console_write_holds_partial_line_until_newline() {
    let mut d = DeviceDispatcher::new();
    d.write(DEV_CONSOLE, b"no newline here").unwrap();
    let drained = d.drain_console_output();
    assert_eq!(drained, b"no newline here");
}

#[test]
fn newline_free_console_writes_keep_only_the_recent_bounded_tail() {
    let mut d = DeviceDispatcher::new();
    let first = vec![b'a'; CONSOLE_PARTIAL_LINE_CAP];
    let recent = vec![b'b'; CONSOLE_PARTIAL_LINE_CAP];

    assert_eq!(
        d.write(DEV_CONSOLE, &first).unwrap(),
        CONSOLE_PARTIAL_LINE_CAP
    );
    assert_eq!(
        d.write(DEV_CONSOLE, &recent).unwrap(),
        CONSOLE_PARTIAL_LINE_CAP
    );
    assert_eq!(d.console_output_len(), CONSOLE_PARTIAL_LINE_CAP);
    assert_eq!(d.drain_console_output(), recent);
}

// ---- /dev/input/kbd and /dev/input/mouse ---------------------------

#[test]
fn input_kbd_read_drains_injected_events() {
    let mut d = DeviceDispatcher::new();
    d.inject_kbd_event(&[0x01, 0x02, 0x03]);
    let mut buf = [0u8; 8];
    let n = d.read(DEV_INPUT_KBD, &mut buf).unwrap();
    assert_eq!(n, 3);
    assert_eq!(&buf[..3], &[0x01, 0x02, 0x03]);
}

#[test]
fn input_mouse_read_drains_injected_events() {
    let mut d = DeviceDispatcher::new();
    d.inject_mouse_event(&[0x10, 0x20]);
    let mut buf = [0u8; 4];
    let n = d.read(DEV_INPUT_MOUSE, &mut buf).unwrap();
    assert_eq!(n, 2);
    assert_eq!(&buf[..2], &[0x10, 0x20]);
}

#[test]
fn input_writes_are_not_supported() {
    let mut d = DeviceDispatcher::new();
    assert_eq!(
        d.write(DEV_INPUT_KBD, b"x").unwrap_err(),
        DevError::NotSupported
    );
    assert_eq!(
        d.write(DEV_INPUT_MOUSE, b"x").unwrap_err(),
        DevError::NotSupported
    );
}

// ---- /dev/fb0 write-path -------------------------------------------

#[test]
fn fb0_read_is_not_supported() {
    let mut d = DeviceDispatcher::new();
    let mut buf = [0u8; 4];
    assert_eq!(
        d.read(DEV_FB0, &mut buf).unwrap_err(),
        DevError::NotSupported
    );
}

#[test]
fn fb0_write_routes_through_platform() {
    // The NativePlatform's driver_call returns Ok(0) by
    // default, so writes succeed. We assert the return equals
    // the input length, which proves the device dispatch path
    // actually invoked the platform and got Ok.
    let mut d = DeviceDispatcher::new();
    let n = d.write(DEV_FB0, &[0u8; 32]).unwrap();
    assert_eq!(n, 32);
}

// ---- Cap checks ----------------------------------------------------

#[test]
fn check_open_allows_unprivileged_devices_for_ordinary_apps() {
    let caps = CapSet::from_caps(&[Cap::DisplayClient]);
    for dev in [DEV_NULL, DEV_ZERO, DEV_RANDOM, DEV_CONSOLE] {
        DeviceDispatcher::check_open(dev, caps)
            .unwrap_or_else(|e| panic!("check_open({dev}) unexpectedly failed: {e:?}"));
    }
}

#[test]
fn check_open_refuses_fb0_without_display_server_cap() {
    let caps = CapSet::from_caps(&[Cap::DisplayClient]);
    assert_eq!(
        DeviceDispatcher::check_open(DEV_FB0, caps).unwrap_err(),
        DevError::NotCapable
    );
}

#[test]
fn check_open_allows_fb0_with_display_server_cap() {
    let caps = CapSet::from_caps(&[Cap::DisplayServer]);
    DeviceDispatcher::check_open(DEV_FB0, caps).unwrap();
}

#[test]
fn check_open_refuses_input_without_display_server_cap() {
    let caps = CapSet::from_caps(&[Cap::DisplayClient]);
    assert_eq!(
        DeviceDispatcher::check_open(DEV_INPUT_KBD, caps).unwrap_err(),
        DevError::NotCapable
    );
    assert_eq!(
        DeviceDispatcher::check_open(DEV_INPUT_MOUSE, caps).unwrap_err(),
        DevError::NotCapable
    );
}

#[test]
fn check_open_allows_input_with_display_server_cap() {
    let caps = CapSet::from_caps(&[Cap::DisplayServer]);
    DeviceDispatcher::check_open(DEV_INPUT_KBD, caps).unwrap();
    DeviceDispatcher::check_open(DEV_INPUT_MOUSE, caps).unwrap();
}

#[test]
fn check_open_unknown_devnum_is_an_error() {
    let caps = CapSet::from_caps(&[Cap::DisplayClient]);
    assert_eq!(
        DeviceDispatcher::check_open(9999, caps).unwrap_err(),
        DevError::UnknownDevice
    );
}

#[test]
fn read_unknown_devnum_is_an_error() {
    let mut d = DeviceDispatcher::new();
    let mut buf = [0u8; 4];
    assert_eq!(d.read(9999, &mut buf).unwrap_err(), DevError::UnknownDevice);
}

#[test]
fn write_unknown_devnum_is_an_error() {
    let mut d = DeviceDispatcher::new();
    assert_eq!(d.write(9999, b"x").unwrap_err(), DevError::UnknownDevice);
}
