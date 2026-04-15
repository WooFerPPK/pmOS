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

/// The set of interfaces a client can bind through
/// `pmd_registry`. `Display` is pre-bound on every
/// connection and is not listed here because
/// `pmd_registry.bind` doesn't bind it — it's object 1
/// by convention.
///
/// **Spec §15: `pmd_shell_manager` is a privileged
/// interface.** Only clients holding the `Shell`
/// capability are allowed to bind it; ordinary apps
/// that try will get `pmd_display.error(code =
/// PERMISSION_DENIED)`. The capability check happens at
/// the server's `registry.bind` dispatch path; this
/// enum just records that the interface exists.
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
    /// `pmd_shell_manager` — desktop-shell window list /
    /// focus / close API. Spec §15.
    ShellManager,
    /// `pmd_xdg_shell` — window-shell global. Clients bind
    /// it to promote a plain [`Interface::Surface`] to a
    /// positioned, titled toplevel window. Narrowed subset
    /// of the Wayland xdg-shell protocol.
    XdgShell,
    /// `pmd_xdg_toplevel` — one window. Carries title +
    /// app_id + server-assigned geometry.
    XdgToplevel,
    /// `pmd_seat` — input device collection global.
    /// Clients bind and then `get_pointer` / `get_keyboard`
    /// to receive event objects.
    Seat,
    /// `pmd_pointer` — per-client pointer event object,
    /// created via `pmd_seat.get_pointer(new_id)`.
    Pointer,
    /// `pmd_keyboard` — per-client keyboard event object,
    /// created via `pmd_seat.get_keyboard(new_id)`.
    Keyboard,
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
            Interface::ShellManager => "pmd_shell_manager",
            Interface::XdgShell => "pmd_xdg_shell",
            Interface::XdgToplevel => "pmd_xdg_toplevel",
            Interface::Seat => "pmd_seat",
            Interface::Pointer => "pmd_pointer",
            Interface::Keyboard => "pmd_keyboard",
        }
    }

    /// Reverse of [`Interface::name`] — look up an interface
    /// by its wire-level string name. Returns `None` for
    /// unknown names.
    ///
    /// Used by the server's `registry.bind` handler to map
    /// the client's interface-name argument onto a Rust
    /// variant so the new object can be installed at the
    /// right interface in the object table.
    pub fn from_name(name: &str) -> Option<Interface> {
        match name {
            "pmd_display" => Some(Interface::Display),
            "pmd_registry" => Some(Interface::Registry),
            "pmd_compositor" => Some(Interface::Compositor),
            "pmd_shm" => Some(Interface::Shm),
            "pmd_shm_pool" => Some(Interface::ShmPool),
            "pmd_buffer" => Some(Interface::Buffer),
            "pmd_surface" => Some(Interface::Surface),
            "pmd_shell_manager" => Some(Interface::ShellManager),
            "pmd_xdg_shell" => Some(Interface::XdgShell),
            "pmd_xdg_toplevel" => Some(Interface::XdgToplevel),
            "pmd_seat" => Some(Interface::Seat),
            "pmd_pointer" => Some(Interface::Pointer),
            "pmd_keyboard" => Some(Interface::Keyboard),
            _ => None,
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
            Interface::ShellManager => SHELL_MANAGER_REQUESTS,
            Interface::XdgShell => XDG_SHELL_REQUESTS,
            Interface::XdgToplevel => XDG_TOPLEVEL_REQUESTS,
            Interface::Seat => SEAT_REQUESTS,
            Interface::Pointer => POINTER_REQUESTS,
            Interface::Keyboard => KEYBOARD_REQUESTS,
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
            Interface::ShellManager => SHELL_MANAGER_EVENTS,
            Interface::XdgShell => XDG_SHELL_EVENTS,
            Interface::XdgToplevel => XDG_TOPLEVEL_EVENTS,
            Interface::Seat => SEAT_EVENTS,
            Interface::Pointer => POINTER_EVENTS,
            Interface::Keyboard => KEYBOARD_EVENTS,
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

// pmd_shell_manager — spec §15. Privileged interface
// (requires Cap::Shell) that lets the desktop shell observe
// + control the open-window list.
const SHELL_MANAGER_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "subscribe_windows" },
    Opcode { number: 2, direction: Direction::Request, name: "focus_window" },
    Opcode { number: 3, direction: Direction::Request, name: "close_window" },
    Opcode { number: 4, direction: Direction::Request, name: "minimize_window" },
];

const SHELL_MANAGER_EVENTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Event, name: "window_created" },
    Opcode { number: 2, direction: Direction::Event, name: "window_destroyed" },
    Opcode { number: 3, direction: Direction::Event, name: "window_focused" },
    Opcode { number: 4, direction: Direction::Event, name: "window_title_changed" },
];

// pmd_xdg_shell — narrowed Wayland xdg-shell. A single
// request in v1: promote a surface to a toplevel. Events
// come later (configure handshake, close request, etc.).
const XDG_SHELL_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "get_toplevel" },
];

const XDG_SHELL_EVENTS: &[Opcode] = &[];

// pmd_xdg_toplevel — per-window object. Clients set the
// title + app_id; the server assigns geometry via its
// auto-layout policy. `destroy` tears the window down.
const XDG_TOPLEVEL_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "set_title" },
    Opcode { number: 2, direction: Direction::Request, name: "set_app_id" },
    Opcode { number: 3, direction: Direction::Request, name: "destroy" },
];

const XDG_TOPLEVEL_EVENTS: &[Opcode] = &[];

// pmd_seat — narrowed Wayland wl_seat. Clients bind the
// global and then derive per-capability objects via
// `get_pointer` / `get_keyboard`.
const SEAT_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "get_pointer" },
    Opcode { number: 2, direction: Direction::Request, name: "get_keyboard" },
];

const SEAT_EVENTS: &[Opcode] = &[];

// pmd_pointer — per-client pointer event object.
// Events carry surface-local coordinates + the target
// surface id so the client knows which of its surfaces
// the event applies to. No enter/leave state machine in
// v1 — every event is self-contained.
const POINTER_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "release" },
];

const POINTER_EVENTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Event, name: "motion" },
    Opcode { number: 2, direction: Direction::Event, name: "button" },
];

// pmd_keyboard — per-client keyboard event object.
const KEYBOARD_REQUESTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Request, name: "release" },
];

const KEYBOARD_EVENTS: &[Opcode] = &[
    Opcode { number: 1, direction: Direction::Event, name: "key" },
];
