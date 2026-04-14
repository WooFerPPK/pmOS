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
