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
//! * [`DisplaySync`] / [`DisplayGetRegistry`] — bind ordering callback and registry objects.
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

use crate::decode::{
    read_i32, read_object_id, read_string, read_string_byte_len, read_u32, DecodeError,
};
use crate::ids::ObjectId;

/// `pmd_display.sync(new_id callback)` — spec §3 row 1.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DisplaySync {
    pub new_id: ObjectId,
}

impl DisplaySync {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self {
            new_id: read_object_id(payload, 0)?,
        })
    }
}

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

/// `pmd_shm_pool.resize(new_size)` — replace the pool's declared backing
/// length. The display server validates resource budgets and existing buffer
/// extents before changing storage.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShmPoolResize {
    pub new_size: u32,
}

impl ShmPoolResize {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self {
            new_size: read_u32(payload, 0)?,
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

/// Maximum inline pixel bytes accepted by one
/// `pmd_surface.patch_current` request. This matches one toolkit upload
/// quantum and keeps both decode allocation and server-side copying bounded.
pub const MAX_SURFACE_PATCH_BYTES: usize = 24 * 1024;

/// `pmd_shm_pool.write(offset, bytes)` — v1 affordance to
/// fill the server-side pool storage with pixel bytes the
/// client just painted. Wayland proper shares the pool
/// memory via fd; v1 elides that and uses an explicit
/// in-protocol write so the client and server stay in
/// sync without any out-of-band fd plumbing.
///
/// Wire layout: `(offset: u32 LE) || bytes`. The server
/// validates `offset + bytes.len() <= pool.size` and writes
/// `bytes` into `pool.storage[offset..offset+bytes.len()]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShmPoolWrite {
    pub offset: u32,
    pub bytes: alloc::vec::Vec<u8>,
}

impl ShmPoolWrite {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let offset = read_u32(payload, 0)?;
        let bytes = payload.get(4..).unwrap_or(&[]).to_vec();
        Ok(ShmPoolWrite { offset, bytes })
    }
}

/// `pmd_shm_pool.write_rows(offset, row_bytes, rows, stride, bytes)` copies a
/// densely packed set of rows into a strided pool region. It is the bounded
/// damage counterpart to [`ShmPoolWrite`]: persistent surfaces can transport a
/// narrow rectangle in one message instead of issuing one write for every
/// destination scanline.
///
/// Wire layout: four `u32` fields followed by exactly `row_bytes * rows`
/// inline bytes. The server validates non-zero geometry, `stride >= row_bytes`,
/// checked destination arithmetic, and the final pool extent before mutating
/// any backing bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShmPoolWriteRows {
    pub offset: u32,
    pub row_bytes: u32,
    pub rows: u32,
    pub stride: u32,
    pub bytes: alloc::vec::Vec<u8>,
}

impl ShmPoolWriteRows {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let offset = read_u32(payload, 0)?;
        let row_bytes = read_u32(payload, 4)?;
        let rows = read_u32(payload, 8)?;
        let stride = read_u32(payload, 12)?;
        let bytes = payload.get(16..).unwrap_or(&[]);
        let expected = u64::from(row_bytes) * u64::from(rows);
        if bytes.len() as u64 != expected {
            return Err(DecodeError::PayloadLengthMismatch {
                expected,
                actual: bytes.len(),
            });
        }
        Ok(Self {
            offset,
            row_bytes,
            rows,
            stride,
            bytes: bytes.to_vec(),
        })
    }
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

/// `pmd_surface.frame(new_id callback)` — spec §9 row 4.
///
/// The callback is a real one-shot [`crate::Interface::Callback`] object.
/// Request dispatch only binds it and associates it with the surface's next
/// commit; completion is tied to a later framebuffer presentation boundary.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SurfaceFrame {
    pub new_id: ObjectId,
}

impl SurfaceFrame {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        if payload.len() != 4 {
            return Err(DecodeError::PayloadLengthMismatch {
                expected: 4,
                actual: payload.len(),
            });
        }
        Ok(Self {
            new_id: read_object_id(payload, 0)?,
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

/// `pmd_surface.patch_current(x, y, width, height, pixels)` — spec §9 row 8.
///
/// `pixels` contains tightly packed four-byte ARGB8888/XRGB8888 rows. The
/// decoder proves that the payload contains exactly `width * height * 4`
/// bytes after the fixed geometry header. Surface bounds, current-buffer
/// state, pixel format, and the inline byte ceiling are server-side semantic
/// checks because they require the target object's live state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SurfacePatchCurrent<'a> {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [u8],
}

impl<'a> SurfacePatchCurrent<'a> {
    pub fn decode(payload: &'a [u8]) -> Result<Self, DecodeError> {
        let x = read_i32(payload, 0)?;
        let y = read_i32(payload, 4)?;
        let width = read_u32(payload, 8)?;
        let height = read_u32(payload, 12)?;
        let expected = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(DecodeError::PayloadLengthOverflow)?;
        let pixels = payload.get(16..).unwrap_or(&[]);
        if pixels.len() as u64 != expected {
            return Err(DecodeError::PayloadLengthMismatch {
                expected,
                actual: pixels.len(),
            });
        }
        Ok(Self {
            x,
            y,
            width,
            height,
            pixels,
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

/// `pmd_shell_manager.unminimize_window(window_id)` — spec §15.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellManagerUnminimizeWindow {
    pub window_id: u32,
}

impl ShellManagerUnminimizeWindow {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(ShellManagerUnminimizeWindow {
            window_id: read_u32(payload, 0)?,
        })
    }
}

/// `pmd_shell_manager.set_work_area_bottom(height_px)` — reserve a bottom
/// output strip for shell chrome such as the taskbar. The display server uses
/// the remaining work area for subsequent application configures.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellManagerSetWorkAreaBottom {
    pub height_px: u32,
}

impl ShellManagerSetWorkAreaBottom {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self {
            height_px: read_u32(payload, 0)?,
        })
    }
}

/// `pmd_shell_manager.toggle_maximized_window(window_id)` — toggle the exact
/// server-global toplevel between its normal geometry and the shell-published
/// work area. The display server resolves `window_id`; client-local object IDs
/// are never accepted as a fallback.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellManagerToggleMaximizedWindow {
    pub window_id: u32,
}

impl ShellManagerToggleMaximizedWindow {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self {
            window_id: read_u32(payload, 0)?,
        })
    }
}

/// `pmd_shell_manager.desktop_ready()` — report that this authenticated shell
/// has published its initial desktop state. The display server turns the
/// one-shot request into a generic framebuffer presentation fence only after
/// all preceding display requests and resulting composition work are settled.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct ShellManagerDesktopReady;

impl ShellManagerDesktopReady {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        if !payload.is_empty() {
            return Err(DecodeError::PayloadLengthMismatch {
                expected: 0,
                actual: payload.len(),
            });
        }
        Ok(Self)
    }
}

/// Maximum timeout accepted by [`ShellManagerBeginRestore`]. The display
/// server clamps shorter/longer caller values into `1..=2500` milliseconds so
/// a crashed shell can never leave application windows hidden indefinitely.
pub const MAX_SHELL_RESTORE_TIMEOUT_MS: u32 = 2_500;

/// `pmd_shell_manager.subscribe_window_state()` — opt in to the v2,
/// authoritative full-window-state stream. Opcode 1 remains the byte-stable
/// v1 lifecycle stream for existing shells.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellManagerSubscribeWindowState {
    pub snapshot_id: u32,
}

impl ShellManagerSubscribeWindowState {
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
}

/// `pmd_shell_manager.begin_restore(timeout_ms)` — begin a privileged atomic
/// restore transaction. A following `pmd_display.sync` is the protocol barrier
/// proving that newly spawned clients will be admitted into the hidden restore
/// set.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellManagerBeginRestore {
    pub restore_id: u32,
    pub timeout_ms: u32,
}

impl ShellManagerBeginRestore {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        if payload.len() != 8 {
            return Err(DecodeError::PayloadLengthMismatch {
                expected: 8,
                actual: payload.len(),
            });
        }
        Ok(Self {
            restore_id: read_u32(payload, 0)?,
            timeout_ms: read_u32(payload, 4)?,
        })
    }
}

/// Flags accepted by [`ShellManagerPlaceRestoredWindow`]. Unknown bits are
/// rejected by the display server before any window state changes.
pub mod shell_restore_window_flags {
    pub const MINIMIZED: u32 = 1 << 0;
    pub const MAXIMIZED: u32 = 1 << 1;
    pub const ALL: u32 = MINIMIZED | MAXIMIZED;
}

/// `pmd_shell_manager.place_restored_window(...)` — associate one hidden,
/// newly created window with its persisted normal geometry and stack rank.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellManagerPlaceRestoredWindow {
    pub restore_id: u32,
    pub window_id: u32,
    pub normal_x: i32,
    pub normal_y: i32,
    pub normal_width: u32,
    pub normal_height: u32,
    pub z_rank: u32,
    pub flags: u32,
}

impl ShellManagerPlaceRestoredWindow {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        if payload.len() != 32 {
            return Err(DecodeError::PayloadLengthMismatch {
                expected: 32,
                actual: payload.len(),
            });
        }
        Ok(Self {
            restore_id: read_u32(payload, 0)?,
            window_id: read_u32(payload, 4)?,
            normal_x: read_i32(payload, 8)?,
            normal_y: read_i32(payload, 12)?,
            normal_width: read_u32(payload, 16)?,
            normal_height: read_u32(payload, 20)?,
            z_rank: read_u32(payload, 24)?,
            flags: read_u32(payload, 28)?,
        })
    }
}

/// `pmd_shell_manager.end_restore(focus_window_id)` — atomically reveal and
/// order all restored windows. Zero asks the server to focus the restoring
/// shell's mapped toplevel, with a highest-visible fallback only when the shell
/// has no focusable window.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShellManagerEndRestore {
    pub restore_id: u32,
    pub focus_window_id: u32,
}

impl ShellManagerEndRestore {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        if payload.len() != 8 {
            return Err(DecodeError::PayloadLengthMismatch {
                expected: 8,
                actual: payload.len(),
            });
        }
        Ok(Self {
            restore_id: read_u32(payload, 0)?,
            focus_window_id: read_u32(payload, 4)?,
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
    /// Declared UTF-8 content bytes without allocating the owned title.
    pub fn title_byte_len(payload: &[u8]) -> Result<usize, DecodeError> {
        read_string_byte_len(payload, 0)
    }

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
    /// Declared UTF-8 content bytes without allocating the owned app id.
    pub fn app_id_byte_len(payload: &[u8]) -> Result<usize, DecodeError> {
        read_string_byte_len(payload, 0)
    }

    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        let (app_id, _consumed) = read_string(payload, 0)?;
        Ok(XdgToplevelSetAppId {
            app_id: app_id.into(),
        })
    }
}

/// `pmd_xdg_toplevel.ack_configure(u32 serial)` — ack a
/// previously-received `configure` event.
///
/// Spec §11 places `ack_configure` on `pmd_xdg_surface`;
/// the v1 codebase collapses `pmd_xdg_surface` into
/// `pmd_xdg_toplevel` (see `client.rs`'s single
/// `xdg_shell.get_toplevel(new_id, surface)` that skips
/// the intermediate `get_xdg_surface`), so the ack lands
/// as a request on the toplevel. The `serial` value is
/// the one the server sent in the preceding `configure`
/// event.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct XdgToplevelAckConfigure {
    pub serial: u32,
}

impl XdgToplevelAckConfigure {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(XdgToplevelAckConfigure {
            serial: read_u32(payload, 0)?,
        })
    }
}

/// `pmd_xdg_toplevel.set_maximized()` — ask the server to
/// maximize this toplevel. Empty payload — the server
/// answers (eventually) with `configure(serial, w, h,
/// states | MAXIMIZED)`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct XdgToplevelSetMaximized;

impl XdgToplevelSetMaximized {
    pub fn decode(_payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(XdgToplevelSetMaximized)
    }
}

/// `pmd_xdg_toplevel.unset_maximized()` — ask the server to
/// restore this toplevel from a previously-set maximized
/// state. Empty payload — the server answers with
/// `configure(serial, w, h, states & !MAXIMIZED)`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct XdgToplevelUnsetMaximized;

impl XdgToplevelUnsetMaximized {
    pub fn decode(_payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(XdgToplevelUnsetMaximized)
    }
}

/// Edge bits for [`XdgToplevelResize::edges`]. A resize-drag
/// can be along a single edge or along two edges meeting at
/// a corner — e.g. `TOP | LEFT` for the top-left corner.
/// Mirrors Wayland's `xdg_toplevel.resize_edge` enum but
/// encoded as a u32 bitfield rather than a fixed enum, since
/// the v1 collapsed `pmd_xdg_toplevel` is byte-stable around
/// bit fields. Zero is a degenerate "no edge" value the
/// server should reject.
pub mod xdg_toplevel_resize_edge {
    pub const TOP: u32 = 1 << 0;
    pub const BOTTOM: u32 = 1 << 1;
    pub const LEFT: u32 = 1 << 2;
    pub const RIGHT: u32 = 1 << 3;
    pub const TOP_LEFT: u32 = TOP | LEFT;
    pub const TOP_RIGHT: u32 = TOP | RIGHT;
    pub const BOTTOM_LEFT: u32 = BOTTOM | LEFT;
    pub const BOTTOM_RIGHT: u32 = BOTTOM | RIGHT;
}

/// `pmd_xdg_toplevel.move(u32 serial)` — ask the server to
/// initiate an interactive move drag. The client passes the
/// serial of the pointer-button event that started the drag
/// so the server can reject mismatched/stale requests. The
/// server takes over pointer events for the duration of the
/// drag and emits `configure` events as the toplevel moves.
///
/// Spec note: Wayland's `xdg_toplevel.move` carries a `seat`
/// argument too; v1 has only one seat (`pmd_seat`) so the
/// argument is implicit and the wire payload only carries
/// the serial.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct XdgToplevelMove {
    pub serial: u32,
}

impl XdgToplevelMove {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(XdgToplevelMove {
            serial: read_u32(payload, 0)?,
        })
    }
}

/// `pmd_xdg_toplevel.resize(u32 serial, u32 edges)` — ask
/// the server to initiate an interactive resize drag along
/// the given edge(s). `edges` is a bitfield of
/// [`xdg_toplevel_resize_edge`] bits. The server emits
/// `configure` events with new sizes as the drag proceeds.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct XdgToplevelResize {
    pub serial: u32,
    pub edges: u32,
}

impl XdgToplevelResize {
    pub fn decode(payload: &[u8]) -> Result<Self, DecodeError> {
        Ok(XdgToplevelResize {
            serial: read_u32(payload, 0)?,
            edges: read_u32(payload, 4)?,
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
