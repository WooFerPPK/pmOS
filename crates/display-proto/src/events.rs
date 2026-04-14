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
//! * [`RegistryGlobal`]    — `pmd_registry.global(name, interface, version)`
//! * [`RegistryGlobalRemove`] — `pmd_registry.global_remove(name)`
//! * [`BufferRelease`]     — `pmd_buffer.release()` (empty payload)
//! * [`ShmFormat`]         — `pmd_shm.format(format)`

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::decode::{read_object_id, read_string, read_u32, DecodeError};
use crate::ids::ObjectId;

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
