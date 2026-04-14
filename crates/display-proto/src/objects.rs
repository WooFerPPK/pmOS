//! Which object interfaces the v1 server supports and their
//! request/event opcode tables.
//!
//! The v1 skeleton implements a narrow slice of the
//! `display-protocol.md` interface set — enough for the
//! toolkit-free-client conformance fixture (T109+) to bind a
//! registry, bind a compositor, create a surface, attach a
//! buffer, commit. The interfaces not listed here
//! (`pmd_seat`, `pmd_xdg_*`, `pmd_shell_manager`, etc.) remain
//! `todo!()` at dispatch time until their slices land.
//!
//! **This module does NOT implement the semantics of each
//! opcode** — it only defines:
//!
//! * which interfaces exist
//! * which opcodes are legal on each interface
//! * the direction (request vs. event) of each opcode
//!
//! The request-handling state machine lives in
//! [`crate::client`]; the decoding of opcode payloads into
//! typed Rust structs will land in a follow-up slice.

use core::fmt;

/// The (extensible-in-v2, fixed-in-v1) set of interfaces a
/// client can bind through `pmd_registry`. `Display` is
/// pre-bound on every connection and is not listed here
/// because `pmd_registry.bind` doesn't bind it — it's object 1
/// by convention.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Interface {
    /// Implicit object 1.
    Display,
    /// `pmd_registry` — advertises globals.
    Registry,
    /// `pmd_compositor` — creates surfaces.
    Compositor,
    /// `pmd_shm` — binds a shared-memory pool.
    Shm,
    /// `pmd_shm_pool` — a single SAB-backed pool.
    ShmPool,
    /// `pmd_buffer` — a sub-rectangle of a pool.
    Buffer,
    /// `pmd_surface` — the basic drawable.
    Surface,
}

impl Interface {
    /// Short human-readable name for diagnostics and
    /// `pmd_registry.global` events.
    pub const fn name(self) -> &'static str {
        match self {
            Interface::Display => "pmd_display",
            Interface::Registry => "pmd_registry",
            Interface::Compositor => "pmd_compositor",
            Interface::Shm => "pmd_shm",
            Interface::ShmPool => "pmd_shm_pool",
            Interface::Buffer => "pmd_buffer",
            Interface::Surface => "pmd_surface",
        }
    }
}

impl fmt::Display for Interface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Numeric opcode paired with its direction and a human name.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Opcode {
    /// Wire opcode number, little-endian u16 on the wire.
    pub number: u16,
    /// Direction of travel.
    pub direction: Direction,
    /// Short name from the spec tables.
    pub name: &'static str,
}

/// A direction tag.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Client → server.
    Request,
    /// Server → client.
    Event,
}

/// Errors from [`Interface::lookup_request`] /
/// [`Interface::lookup_event`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OpcodeError {
    /// The interface doesn't define this opcode as a request
    /// (or as an event, depending on which lookup was called).
    UnknownOpcode { interface: Interface, opcode: u16 },
}

impl Interface {
    /// Look up the named request on this interface, or return
    /// `UnknownOpcode` if none exists.
    pub const fn lookup_request(self, opcode: u16) -> Result<Opcode, OpcodeError> {
        let table = self.request_table();
        let mut i = 0;
        while i < table.len() {
            let entry = &table[i];
            if entry.number == opcode {
                return Ok(*entry);
            }
            i += 1;
        }
        Err(OpcodeError::UnknownOpcode {
            interface: self,
            opcode,
        })
    }

    /// Look up the named event on this interface.
    pub const fn lookup_event(self, opcode: u16) -> Result<Opcode, OpcodeError> {
        let table = self.event_table();
        let mut i = 0;
        while i < table.len() {
            let entry = &table[i];
            if entry.number == opcode {
                return Ok(*entry);
            }
            i += 1;
        }
        Err(OpcodeError::UnknownOpcode {
            interface: self,
            opcode,
        })
    }

    const fn request_table(self) -> &'static [Opcode] {
        match self {
            Interface::Display => DISPLAY_REQUESTS,
            Interface::Registry => REGISTRY_REQUESTS,
            Interface::Compositor => COMPOSITOR_REQUESTS,
            Interface::Shm => SHM_REQUESTS,
            Interface::ShmPool => SHM_POOL_REQUESTS,
            Interface::Buffer => BUFFER_REQUESTS,
            Interface::Surface => SURFACE_REQUESTS,
        }
    }

    const fn event_table(self) -> &'static [Opcode] {
        match self {
            Interface::Display => DISPLAY_EVENTS,
            Interface::Registry => REGISTRY_EVENTS,
            Interface::Compositor => COMPOSITOR_EVENTS,
            Interface::Shm => SHM_EVENTS,
            Interface::ShmPool => SHM_POOL_EVENTS,
            Interface::Buffer => BUFFER_EVENTS,
            Interface::Surface => SURFACE_EVENTS,
        }
    }
}

// ---- Opcode tables -------------------------------------------------
//
// These correspond one-for-one with the tables in
// `specs/001-browser-os-v1/contracts/display-protocol.md`.
// Each table is a sorted-by-opcode slice; tests in
// `tests/opcodes.rs` assert the sortedness so binary-search
// can replace linear search later without a correctness risk.

const DISPLAY_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "sync" },
    Opcode { number: 2, direction: Direction::Request, name: "get_registry" },
];

const DISPLAY_EVENTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Event, name: "error" },
    Opcode { number: 2, direction: Direction::Event, name: "delete_id" },
];

const REGISTRY_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "bind" },
];

const REGISTRY_EVENTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Event, name: "global" },
    Opcode { number: 2, direction: Direction::Event, name: "global_remove" },
];

const COMPOSITOR_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "create_surface" },
];

const COMPOSITOR_EVENTS: &[Opcode] = &[];

const SHM_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "create_pool" },
];

const SHM_EVENTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Event, name: "format" },
];

const SHM_POOL_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "create_buffer" },
    Opcode { number: 2, direction: Direction::Request, name: "resize" },
    Opcode { number: 3, direction: Direction::Request, name: "destroy" },
];

const SHM_POOL_EVENTS: &[Opcode] = &[];

const BUFFER_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "destroy" },
];

const BUFFER_EVENTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Event, name: "release" },
];

const SURFACE_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "destroy" },
    Opcode { number: 2, direction: Direction::Request, name: "attach" },
    Opcode { number: 3, direction: Direction::Request, name: "damage" },
    Opcode { number: 4, direction: Direction::Request, name: "frame" },
    Opcode { number: 5, direction: Direction::Request, name: "set_opaque_region" },
    Opcode { number: 6, direction: Direction::Request, name: "set_input_region" },
    Opcode { number: 7, direction: Direction::Request, name: "commit" },
];

const SURFACE_EVENTS: &[Opcode] = &[];
