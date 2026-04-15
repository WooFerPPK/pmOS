//! Typed request structs and their decoders.
//!
//! Each struct in this module matches exactly one row of
//! the request tables in
//! `specs/001-browser-os-v1/contracts/display-protocol.md`.
//! A `decode` method consumes a payload slice (the bytes
//! AFTER `MessageHeader`) and returns the typed struct on
//! success.
//!
//! The initial v1 set covers the requests the server
//! actually has to decode to keep its object table in sync:
//!
//! * [`DisplayGetRegistry`] — binds a new registry.
//! * [`RegistryBind`] — binds a new global (compositor, shm,
//!   ...). Carries the interface NAME string so the server
//!   can map it to a Rust-level [`crate::Interface`] via
//!   [`crate::Interface::from_name`].
//! * [`CompositorCreateSurface`] — creates a new surface.
//! * [`SurfaceAttach`] / [`SurfaceDamage`] — decoded for
//!   tests; no object-table side effects.
//!
//! Requests that don't carry a `new_id` argument (commit,
//! destroy, sync, ...) don't need a typed struct here yet
//! — the server's dispatch path ignores their payloads and
//! just journals the header. Those land when the real
//! compositor needs them.

use crate::decode::{read_i32, read_object_id, read_string, read_u32, DecodeError};
use crate::ids::ObjectId;

/// `pmd_display.get_registry(new_id)` — spec §3 row 2.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DisplayGetRegistry {
    pub new_id: ObjectId,
}

impl DisplayGetRegistry {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(DisplayGetRegistry {
            new_id: read_object_id(payload, 0)?,
        })
    }
}

/// `pmd_registry.bind(name, interface, version, new_id)` —
/// spec §4 row 1.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RegistryBind<'a> {
    pub name: u32,
    pub interface: &'a str,
    pub version: u32,
    pub new_id: ObjectId,
}

impl<'a> RegistryBind<'a> {
    pub fn decode(payload: &'a [u8]) -> Result<Self, DecodeError> {
        let name = read_u32(payload, 0)?;
        let (interface, consumed) = read_string(payload, 4)?;
        let after_string = 4 + consumed;
        let version = read_u32(payload, after_string)?;
        let new_id = read_object_id(payload, after_string + 4)?;
        Ok(RegistryBind {
            name,
            interface,
            version,
            new_id,
        })
    }
}

/// `pmd_compositor.create_surface(new_id)` — spec §5 row 1.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CompositorCreateSurface {
    pub new_id: ObjectId,
}

impl CompositorCreateSurface {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(CompositorCreateSurface {
            new_id: read_object_id(payload, 0)?,
        })
    }
}

/// `pmd_shm.create_pool(new_id, fd, size)` — spec §6 row 1.
///
/// The `fd` argument is carried out-of-band in the message
/// header's `fd_passing` count — the payload itself is just
/// `new_id` + `size`. In v1 on the native-host test path no
/// real fd is attached; the server trusts `size` to describe
/// the pool's logical byte length. When SharedArrayBuffer
/// transport lands, the client will set `fd_passing = 1` and
/// the kernel-side display-server host will pull the SAB
/// handle off the ring's aux channel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShmCreatePool {
    pub new_id: ObjectId,
    pub size: u32,
}

impl ShmCreatePool {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(ShmCreatePool {
            new_id: read_object_id(payload, 0)?,
            size: read_u32(payload, 4)?,
        })
    }
}

/// `pmd_shm_pool.create_buffer(new_id, offset, width,
/// height, stride, format)` — spec §7 row 1.
///
/// `format` is one of the v1 pixel formats: `ARGB8888 = 0`
/// or `XRGB8888 = 1`. Decoded as a raw `u32` here; the
/// display server's compositor path is responsible for
/// validating that the value is in range.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShmPoolCreateBuffer {
    pub new_id: ObjectId,
    pub offset: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
}

impl ShmPoolCreateBuffer {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(ShmPoolCreateBuffer {
            new_id: read_object_id(payload, 0)?,
            offset: read_u32(payload, 4)?,
            width: read_u32(payload, 8)?,
            height: read_u32(payload, 12)?,
            stride: read_u32(payload, 16)?,
            format: read_u32(payload, 20)?,
        })
    }
}

/// Known v1 pixel formats for [`ShmPoolCreateBuffer::format`].
/// Kept as a plain u32 constant set so the decoder doesn't
/// have to validate — the compositor's attach/commit path
/// owns format validation.
pub mod buffer_format {
    pub const ARGB8888: u32 = 0;
    pub const XRGB8888: u32 = 1;
}

/// `pmd_surface.attach(buffer_id, x, y)` — spec §9 row 2.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SurfaceAttach {
    pub buffer_id: ObjectId,
    pub x: i32,
    pub y: i32,
}

impl SurfaceAttach {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(SurfaceAttach {
            buffer_id: read_object_id(payload, 0)?,
            x: read_i32(payload, 4)?,
            y: read_i32(payload, 8)?,
        })
    }
}

/// `pmd_surface.damage(x, y, w, h)` — spec §9 row 3.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SurfaceDamage {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl SurfaceDamage {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(SurfaceDamage {
            x: read_i32(payload, 0)?,
            y: read_i32(payload, 4)?,
            width: read_i32(payload, 8)?,
            height: read_i32(payload, 12)?,
        })
    }
}

// ---- pmd_shell_manager (§15) ----------------------------------

/// `pmd_shell_manager.subscribe_windows()` — spec §15. No
/// payload. Requesting this turns on the
/// `window_created` / `window_destroyed` / `window_focused`
/// / `window_title_changed` event stream for the calling
/// shell client.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct ShellManagerSubscribeWindows;

impl ShellManagerSubscribeWindows {
    pub fn decode(_payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(ShellManagerSubscribeWindows)
    }
}

/// `pmd_shell_manager.focus_window(window_id)` — spec §15.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellManagerFocusWindow {
    pub window_id: u32,
}

impl ShellManagerFocusWindow {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(ShellManagerFocusWindow {
            window_id: read_u32(payload, 0)?,
        })
    }
}

/// `pmd_shell_manager.close_window(window_id)` — spec §15.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellManagerCloseWindow {
    pub window_id: u32,
}

impl ShellManagerCloseWindow {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(ShellManagerCloseWindow {
            window_id: read_u32(payload, 0)?,
        })
    }
}

/// `pmd_shell_manager.minimize_window(window_id)` — spec §15.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellManagerMinimizeWindow {
    pub window_id: u32,
}

impl ShellManagerMinimizeWindow {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(ShellManagerMinimizeWindow {
            window_id: read_u32(payload, 0)?,
        })
    }
}

// ---- pmd_xdg_shell (narrowed Wayland xdg-shell) ---------------

/// `pmd_xdg_shell.get_toplevel(new_id toplevel, object_id
/// surface)` — promote an existing `pmd_surface` to a
/// positioned, titled toplevel window. The server assigns
/// a screen-space origin to the new toplevel via its
/// per-client auto-layout policy at dispatch time.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct XdgShellGetToplevel {
    pub new_id: ObjectId,
    pub surface_id: ObjectId,
}

impl XdgShellGetToplevel {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(XdgShellGetToplevel {
            new_id: read_object_id(payload, 0)?,
            surface_id: read_object_id(payload, 4)?,
        })
    }
}

/// `pmd_xdg_toplevel.set_title(string title)` — update the
/// toplevel's human-readable title. The server stores it on
/// the per-client toplevel record and (when a shell client
/// is subscribed) emits a `window_title_changed` event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XdgToplevelSetTitle {
    pub title: alloc::string::String,
}

impl XdgToplevelSetTitle {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let (title, _consumed) = read_string(payload, 0)?;
        Ok(XdgToplevelSetTitle {
            title: title.into(),
        })
    }
}

/// `pmd_xdg_toplevel.set_app_id(string app_id)` — update
/// the toplevel's app identifier (e.g. `"pmos.term"`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XdgToplevelSetAppId {
    pub app_id: alloc::string::String,
}

impl XdgToplevelSetAppId {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let (app_id, _consumed) = read_string(payload, 0)?;
        Ok(XdgToplevelSetAppId {
            app_id: app_id.into(),
        })
    }
}

// ---- pmd_seat (narrowed Wayland wl_seat) ----------------------

/// `pmd_seat.get_pointer(new_id pointer)` — carve a new
/// `pmd_pointer` object out of the seat. Events on the
/// new id deliver pointer motion and button presses.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SeatGetPointer {
    pub new_id: ObjectId,
}

impl SeatGetPointer {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(SeatGetPointer {
            new_id: read_object_id(payload, 0)?,
        })
    }
}

/// `pmd_seat.get_keyboard(new_id keyboard)` — carve a new
/// `pmd_keyboard` object out of the seat.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SeatGetKeyboard {
    pub new_id: ObjectId,
}

impl SeatGetKeyboard {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(SeatGetKeyboard {
            new_id: read_object_id(payload, 0)?,
        })
    }
}
