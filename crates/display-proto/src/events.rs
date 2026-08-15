//! Typed event structs and their decoders + encoders.
//!
//! This is the mirror of [`crate::requests`] for the
//! server → client direction: every event in
//! `display-protocol.md §3-14` that any v1 client might
//! need has a struct here with a `decode(payload) -> Self`
//! method and an `encode(out: &mut Vec<u8>)` method.
//!
//! Having both directions means tests can build event
//! payloads without the display-server library being
//! involved (`toolkit/tests/events.rs` and
//! `integration-tests/tests/*.rs` both lean on this). The
//! real server uses the same encoders to ship events over
//! the wire.
//!
//! Every struct owns its strings (via `String`) rather
//! than borrowing from the decoded payload, so the returned
//! values can outlive the byte buffer they came from. The
//! encoders take `&mut Vec<u8>` and append bytes.
//!
//! v1 event set:
//!
//! * [`DisplayError`]      — `pmd_display.error(object_id, code, message)`
//! * [`DisplayDeleteId`]   — `pmd_display.delete_id(id)`
//! * [`CallbackDone`]      — `pmd_callback.done(callback_data)`
//! * [`RegistryGlobal`]    — `pmd_registry.global(name, interface, version)`
//! * [`RegistryGlobalRemove`] — `pmd_registry.global_remove(name)`
//! * [`BufferRelease`]     — `pmd_buffer.release()` (empty payload)
//! * [`ShmFormat`]         — `pmd_shm.format(format)`

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::decode::{read_i32, read_object_id, read_string, read_u32, DecodeError};
use crate::ids::ObjectId;

/// `pmd_callback.done(callback_data)` — one-shot ordering marker.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CallbackDone {
    pub callback_data: u32,
}

impl CallbackDone {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self {
            callback_data: read_u32(payload, 0)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.callback_data);
    }
}

/// Write an `i32` to `out` in little-endian byte order.
fn write_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Write a `u32` to `out` in little-endian byte order.
fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Write an [`ObjectId`] to `out` in little-endian byte order.
fn write_object_id(out: &mut Vec<u8>, id: ObjectId) {
    write_u32(out, id.raw());
}

/// Append a length-prefixed UTF-8 wire string to `out`:
/// `u32 length` + content bytes + padding to the next
/// 4-byte boundary. Matches `contracts/display-protocol.md §1`
/// and round-trips through [`crate::decode::read_string`].
pub fn write_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    write_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
    let pad = (4 - (bytes.len() % 4)) % 4;
    for _ in 0..pad {
        out.push(0);
    }
}

/// Stable `pmd_display.error.code` values. Both sides of
/// the protocol read these to interpret the numeric code
/// on a `pmd_display.error` event without having to parse
/// the human-readable `message` string.
///
/// v1 only defines one. New variants land here as the
/// server learns to surface more errors.
pub mod error_code {
    /// The bind targeted an interface that requires a
    /// capability the connecting client doesn't hold.
    /// Spec §15: `pmd_shell_manager` requires `Cap::Shell`.
    pub const PERMISSION_DENIED: u32 = 1;
}

/// `pmd_display.error(object_id, code, message)` — spec §3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayError {
    pub object_id: ObjectId,
    pub code: u32,
    pub message: String,
}

impl DisplayError {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let object_id = read_object_id(payload, 0)?;
        let code = read_u32(payload, 4)?;
        let (message, _) = read_string(payload, 8)?;
        Ok(DisplayError {
            object_id,
            code,
            message: message.to_string(),
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_object_id(out, self.object_id);
        write_u32(out, self.code);
        write_string(out, &self.message);
    }
}

/// `pmd_display.delete_id(id)` — spec §3.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DisplayDeleteId {
    pub id: ObjectId,
}

impl DisplayDeleteId {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(DisplayDeleteId {
            id: read_object_id(payload, 0)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_object_id(out, self.id);
    }
}

/// `pmd_registry.global(name, interface, version)` — spec §4.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryGlobal {
    pub name: u32,
    pub interface: String,
    pub version: u32,
}

impl RegistryGlobal {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let name = read_u32(payload, 0)?;
        let (interface, consumed) = read_string(payload, 4)?;
        let version = read_u32(payload, 4 + consumed)?;
        Ok(RegistryGlobal {
            name,
            interface: interface.to_string(),
            version,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.name);
        write_string(out, &self.interface);
        write_u32(out, self.version);
    }
}

/// `pmd_registry.global_remove(name)` — spec §4.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RegistryGlobalRemove {
    pub name: u32,
}

impl RegistryGlobalRemove {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(RegistryGlobalRemove {
            name: read_u32(payload, 0)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.name);
    }
}

/// `pmd_buffer.release()` — spec §8. No payload.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct BufferRelease;

impl BufferRelease {
    pub fn decode(_payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(BufferRelease)
    }

    pub fn encode(&self, _out: &mut Vec<u8>) {
        // Empty payload.
    }
}

/// `pmd_shm.format(format)` — spec §6. Advertises a
/// supported pixel format (ARGB8888 = 0, XRGB8888 = 1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShmFormat {
    pub format: u32,
}

impl ShmFormat {
    /// ARGB8888 (opaque alpha + premultiplied BGRA order).
    pub const FORMAT_ARGB8888: u32 = 0;
    /// XRGB8888 (no alpha channel).
    pub const FORMAT_XRGB8888: u32 = 1;

    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(ShmFormat {
            format: read_u32(payload, 0)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.format);
    }
}

// ---- pmd_shell_manager events (§15) ---------------------------

/// `pmd_shell_manager.window_created(window_id, title,
/// app_id)` — spec §15. Fired once per window after the
/// shell subscribes via `subscribe_windows`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellWindowCreated {
    pub window_id: u32,
    pub title: String,
    pub app_id: String,
}

impl ShellWindowCreated {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let window_id = read_u32(payload, 0)?;
        let (title, consumed_title) = read_string(payload, 4)?;
        let after_title = 4 + consumed_title;
        let (app_id, _) = read_string(payload, after_title)?;
        Ok(ShellWindowCreated {
            window_id,
            title: title.to_string(),
            app_id: app_id.to_string(),
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.window_id);
        write_string(out, &self.title);
        write_string(out, &self.app_id);
    }
}

/// `pmd_shell_manager.window_destroyed(window_id)` — spec §15.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellWindowDestroyed {
    pub window_id: u32,
}

impl ShellWindowDestroyed {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(ShellWindowDestroyed {
            window_id: read_u32(payload, 0)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.window_id);
    }
}

/// `pmd_shell_manager.window_focused(window_id)` — spec §15.
/// Sent whenever the focused window changes; the shell
/// uses it to update its taskbar / chrome state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellWindowFocused {
    pub window_id: u32,
}

impl ShellWindowFocused {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(ShellWindowFocused {
            window_id: read_u32(payload, 0)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.window_id);
    }
}

/// `pmd_shell_manager.window_title_changed(window_id,
/// new_title)` — spec §15.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellWindowTitleChanged {
    pub window_id: u32,
    pub new_title: String,
}

impl ShellWindowTitleChanged {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let window_id = read_u32(payload, 0)?;
        let (new_title, _) = read_string(payload, 4)?;
        Ok(ShellWindowTitleChanged {
            window_id,
            new_title: new_title.to_string(),
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.window_id);
        write_string(out, &self.new_title);
    }
}

/// Authoritative v2 shell-window state flags. Restore flags are observable so
/// the shell can distinguish a mapped candidate from a visible window and can
/// prove that the candidate committed the placement's effective size before
/// requesting the atomic reveal.
pub mod shell_window_state_flags {
    pub const MAPPED: u32 = 1 << 0;
    pub const MINIMIZED: u32 = 1 << 1;
    pub const MAXIMIZED: u32 = 1 << 2;
    pub const FOCUSED: u32 = 1 << 3;
    pub const HIDDEN_FOR_RESTORE: u32 = 1 << 4;
    /// The active restore placement is causally settled: either the buffer was
    /// already the exact effective size when placed, or a later surface commit
    /// advanced past the placement boundary with that exact size.
    pub const RESTORE_PLACEMENT_APPLIED: u32 = 1 << 5;
    pub const ALL: u32 =
        MAPPED | MINIMIZED | MAXIMIZED | FOCUSED | HIDDEN_FOR_RESTORE | RESTORE_PLACEMENT_APPLIED;
}

/// Full, server-authoritative state used by the v2 `window_created_v2` and
/// `window_state_changed` events. `owner_pid` is the immutable kernel-
/// authenticated peer PID captured when the owning display socket was
/// accepted; no protocol field supplied by the application contributes to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellWindowState {
    /// Client-chosen generation from `subscribe_window_state`.
    pub snapshot_id: u32,
    pub window_id: u32,
    pub owner_pid: u32,
    pub ordinal: u32,
    pub current_x: i32,
    pub current_y: i32,
    pub current_width: u32,
    pub current_height: u32,
    pub normal_x: i32,
    pub normal_y: i32,
    pub normal_width: u32,
    pub normal_height: u32,
    pub flags: u32,
    /// Bottom-to-top rank. Zero is the bottom-most live window.
    pub z_rank: u32,
    pub title: String,
    pub app_id: String,
}

impl ShellWindowState {
    pub const FIXED_PAYLOAD_BYTES: usize = 56;

    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let (title, consumed_title) = read_string(payload, Self::FIXED_PAYLOAD_BYTES)?;
        let app_offset = Self::FIXED_PAYLOAD_BYTES + consumed_title;
        let (app_id, consumed_app_id) = read_string(payload, app_offset)?;
        let expected = app_offset.saturating_add(consumed_app_id);
        if expected != payload.len() {
            return Err(DecodeError::PayloadLengthMismatch {
                expected: expected as u64,
                actual: payload.len(),
            });
        }
        Ok(Self {
            snapshot_id: read_u32(payload, 0)?,
            window_id: read_u32(payload, 4)?,
            owner_pid: read_u32(payload, 8)?,
            ordinal: read_u32(payload, 12)?,
            current_x: read_i32(payload, 16)?,
            current_y: read_i32(payload, 20)?,
            current_width: read_u32(payload, 24)?,
            current_height: read_u32(payload, 28)?,
            normal_x: read_i32(payload, 32)?,
            normal_y: read_i32(payload, 36)?,
            normal_width: read_u32(payload, 40)?,
            normal_height: read_u32(payload, 44)?,
            flags: read_u32(payload, 48)?,
            z_rank: read_u32(payload, 52)?,
            title: title.to_string(),
            app_id: app_id.to_string(),
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.snapshot_id);
        write_u32(out, self.window_id);
        write_u32(out, self.owner_pid);
        write_u32(out, self.ordinal);
        write_i32(out, self.current_x);
        write_i32(out, self.current_y);
        write_u32(out, self.current_width);
        write_u32(out, self.current_height);
        write_i32(out, self.normal_x);
        write_i32(out, self.normal_y);
        write_u32(out, self.normal_width);
        write_u32(out, self.normal_height);
        write_u32(out, self.flags);
        write_u32(out, self.z_rank);
        write_string(out, &self.title);
        write_string(out, &self.app_id);
    }
}

/// Empty catch-up terminator for `subscribe_window_state`. Live events queued
/// after it are therefore unambiguously newer than the initial snapshot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellWindowSnapshotDone {
    pub snapshot_id: u32,
}

impl ShellWindowSnapshotDone {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        if payload.len() != 4 {
            return Err(DecodeError::PayloadLengthMismatch {
                expected: 4,
                actual: payload.len(),
            });
        }
        Ok(Self {
            snapshot_id: read_u32(payload, 0)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.snapshot_id);
    }
}

/// Completion status carried by [`ShellRestoreFinished`].
pub mod shell_restore_status {
    pub const COMPLETED: u32 = 0;
    pub const ABORTED: u32 = 1;
    pub const TIMED_OUT: u32 = 2;
    pub const BUSY: u32 = 3;
}

/// Restore transaction completion. `placed` is the number of windows whose
/// persisted placement was accepted before the transaction finished.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellRestoreFinished {
    pub restore_id: u32,
    pub status: u32,
    pub placed: u32,
}

impl ShellRestoreFinished {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        if payload.len() != 12 {
            return Err(DecodeError::PayloadLengthMismatch {
                expected: 12,
                actual: payload.len(),
            });
        }
        Ok(Self {
            restore_id: read_u32(payload, 0)?,
            status: read_u32(payload, 4)?,
            placed: read_u32(payload, 8)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.restore_id);
        write_u32(out, self.status);
        write_u32(out, self.placed);
    }
}

// ---- pmd_pointer / pmd_keyboard events ------------------------

/// Pointer button press/release state. Matches the
/// on-the-wire u32 encoding of `pmd_pointer.button.state`.
pub mod pointer_button_state {
    /// The button was pressed.
    pub const PRESSED: u32 = 1;
    /// The button was released.
    pub const RELEASED: u32 = 0;
}

/// Key press/release state. Matches
/// `pmd_keyboard.key.state` on the wire.
pub mod key_state {
    pub const PRESSED: u32 = 1;
    pub const RELEASED: u32 = 0;
}

/// `pmd_pointer.motion(surface_id, x, y)` — the pointer
/// moved over a surface. Coordinates are surface-local
/// (i.e. relative to the toplevel's origin if the surface
/// is wrapped in one). `surface_id` tells the client
/// which of its surfaces the event applies to; there is
/// no enter/leave state machine in v1.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PointerMotion {
    pub surface_id: ObjectId,
    pub x: i32,
    pub y: i32,
}

impl PointerMotion {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(PointerMotion {
            surface_id: read_object_id(payload, 0)?,
            x: read_i32(payload, 4)?,
            y: read_i32(payload, 8)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_object_id(out, self.surface_id);
        write_i32(out, self.x);
        write_i32(out, self.y);
    }
}

/// `pmd_pointer.button(serial, surface_id, x, y, button, state)`.
/// `button` is a linux-input-style code (1 = left,
/// 2 = right, 3 = middle in v1). `state` is one of
/// [`pointer_button_state`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PointerButton {
    /// Monotonic display-server input serial. Interactive move/resize requests
    /// echo the serial from the press that initiated the operation.
    pub serial: u32,
    pub surface_id: ObjectId,
    pub x: i32,
    pub y: i32,
    pub button: u32,
    pub state: u32,
}

impl PointerButton {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(PointerButton {
            serial: read_u32(payload, 0)?,
            surface_id: read_object_id(payload, 4)?,
            x: read_i32(payload, 8)?,
            y: read_i32(payload, 12)?,
            button: read_u32(payload, 16)?,
            state: read_u32(payload, 20)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.serial);
        write_object_id(out, self.surface_id);
        write_i32(out, self.x);
        write_i32(out, self.y);
        write_u32(out, self.button);
        write_u32(out, self.state);
    }
}

/// `pmd_keyboard.key(surface_id, key, state)`. `key` is a logical
/// USB-HID-style scancode: the display server maps the physical input through
/// the active keyboard layout while preserving this stable v1 namespace.
/// `state` is one of [`key_state`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KeyboardKey {
    pub surface_id: ObjectId,
    pub key: u32,
    pub state: u32,
}

impl KeyboardKey {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(KeyboardKey {
            surface_id: read_object_id(payload, 0)?,
            key: read_u32(payload, 4)?,
            state: read_u32(payload, 8)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_object_id(out, self.surface_id);
        write_u32(out, self.key);
        write_u32(out, self.state);
    }
}

// ---- pmd_xdg_toplevel events (§11 + §12 merged) ---------------

/// Bitfield values for [`XdgToplevelConfigure::states`].
/// Mirrors Wayland's `xdg_toplevel.state` enum but encoded
/// as a u32 bitfield rather than a variable-length array,
/// since the v1 collapsed `pmd_xdg_toplevel` sends a fixed
/// 16-byte payload. Only bits that v1 actively uses are
/// allocated; future slices can grow the bitfield without
/// breaking decode (zero is the "no states" default).
pub mod xdg_toplevel_state {
    /// The window is currently maximized — the toolkit
    /// should re-lay-out to fill the work area.
    pub const MAXIMIZED: u32 = 1 << 0;
    /// The window is currently fullscreen.
    pub const FULLSCREEN: u32 = 1 << 1;
    /// The window is being resized — the toolkit should
    /// throttle expensive recomputation.
    pub const RESIZING: u32 = 1 << 2;
    /// The window has keyboard focus.
    pub const ACTIVATED: u32 = 1 << 3;
}

/// `pmd_xdg_toplevel.configure(u32 serial, i32 width,
/// i32 height, u32 states)` — the server is proposing a size
/// + state for the window.
///
/// Spec §11 + §12: the `serial` value (from `pmd_xdg_surface`)
/// and the suggested `width` / `height` + window-state bitfield
/// (from `pmd_xdg_toplevel`) are merged into one event in the
/// v1 collapsed `pmd_xdg_toplevel`. The client must reply with
/// [`crate::requests::XdgToplevelAckConfigure`] carrying
/// the same `serial`.
///
/// `width == 0` and `height == 0` mean "the server defers to
/// the client's preferred size" — v1 always sends an
/// explicit size, but the sentinel is kept for forward
/// compatibility with the spec.
///
/// `states` is a bitfield of [`xdg_toplevel_state`] bits;
/// `0` means "no special state". Older test fixtures that
/// pre-date the states extension can pass `0` to get the
/// previous configure semantics.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct XdgToplevelConfigure {
    pub serial: u32,
    pub width: i32,
    pub height: i32,
    pub states: u32,
}

impl XdgToplevelConfigure {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(XdgToplevelConfigure {
            serial: read_u32(payload, 0)?,
            width: read_i32(payload, 4)?,
            height: read_i32(payload, 8)?,
            states: read_u32(payload, 12)?,
        })
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        write_u32(out, self.serial);
        write_i32(out, self.width);
        write_i32(out, self.height);
        write_u32(out, self.states);
    }
}

/// `pmd_xdg_toplevel.close` — the server / user asked
/// this window to close. The client decides what to do:
/// prompt for unsaved-work confirmation, save state, etc.,
/// then destroy the toplevel. No payload.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct XdgToplevelClose;

impl XdgToplevelClose {
    pub fn decode(_payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(XdgToplevelClose)
    }

    pub fn encode(&self, _out: &mut Vec<u8>) {
        // Empty payload.
    }
}
