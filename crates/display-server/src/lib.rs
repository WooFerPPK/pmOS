#![cfg_attr(not(test), no_std)]

//! PMos display server library — per-connection state machine
//! and multi-client dispatcher.
//!
//! The wire format, the object-ID partitioning, and the
//! interface opcode tables all live in the sibling
//! [`display_proto`] crate so exactly one implementation is
//! shared between the server (this crate) and the toolkit
//! client library. This crate re-exports everything that
//! `display_server::wire`, `display_server::ids`, and
//! `display_server::objects` used to hand out directly, so
//! existing callers keep compiling.
//!
//! Two modules are unique to the server:
//!
//! * [`client`] — per-connection [`client::Client`] state
//!   machine: object table, inbound request dispatch,
//!   event journaling.
//! * [`server`] — the top-level [`server::Server`] that owns
//!   multiple clients.
//!
//! Nothing in this crate touches the kernel's IPC subsystem.
//! The binary in `src/main.rs` is the integration point that
//! opens `/run/display` and feeds byte streams into the
//! library; the library itself is transport-agnostic.

extern crate alloc;

pub mod client;
pub mod compositor;
pub mod server;

// Re-export the shared protocol types so existing callers
// (`display_server::wire::MessageHeader`, etc.) keep working.
pub use display_proto::{ids, objects, wire};

pub use client::{
    interface_required_cap, BufferAttachment, BufferInfo, Client, ClientError, ClientId,
    DamageRect, Pool, Surface, Toplevel, AUTO_LAYOUT_STEP, MAX_POOL_SIZE,
};
pub use compositor::{Framebuffer, BYTES_PER_PIXEL, DEFAULT_HEIGHT, DEFAULT_WIDTH};
pub use display_proto::{
    IdAllocator, IdError, IdKind, Interface, MessageHeader, ObjectId,
    ObjectIdAllocationError, Opcode, OpcodeError, WireError, HEADER_SIZE,
};
pub use server::{HitResult, Server, ServerError};
