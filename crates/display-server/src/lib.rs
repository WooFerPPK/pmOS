#![cfg_attr(not(test), no_std)]

//! PMos display server library — wire framing, per-connection
//! state, and the beginnings of the object registry.
//!
//! This crate is the Rust-side implementation of
//! `specs/001-browser-os-v1/contracts/display-protocol.md`. In
//! production the server runs as the `display-server` userland
//! process (see `src/main.rs`) holding the `DisplayServer`
//! capability, which is the only process allowed to open
//! `/dev/fb0` and `/dev/input/*`.
//!
//! The library is deliberately split into small isolated
//! modules so each layer can be unit-tested in isolation —
//! matching the constitution's Principle X gate:
//!
//! * [`wire`] — message framing: [`wire::MessageHeader`] encode
//!   / decode, little-endian integer helpers, length validation.
//! * [`ids`] — [`ids::ObjectId`] newtype + [`ids::IdAllocator`]
//!   with the odd/even client/server partitioning mandated by
//!   `display-protocol.md §2`.
//! * [`objects`] — which object interfaces the v1 server
//!   supports and a compile-time table of their request and
//!   event opcodes.
//! * [`client`] — per-connection [`client::Client`] state
//!   machine: object table, inbound request dispatch, event
//!   journaling.
//! * [`server`] — the top-level [`server::Server`] that owns
//!   multiple clients.
//!
//! Nothing in this crate touches the kernel's IPC subsystem.
//! The binary in `src/main.rs` is the integration point that
//! opens `/run/display` and feeds byte streams into the library;
//! the library itself is transport-agnostic and lives entirely
//! in `no_std + alloc`.

extern crate alloc;

pub mod client;
pub mod ids;
pub mod objects;
pub mod server;
pub mod wire;

pub use client::{Client, ClientError, ClientId};
pub use ids::{IdAllocator, IdKind, IdError, ObjectId, ObjectIdAllocationError};
pub use objects::{Interface, Opcode, OpcodeError};
pub use server::{Server, ServerError};
pub use wire::{MessageHeader, WireError, HEADER_SIZE};
