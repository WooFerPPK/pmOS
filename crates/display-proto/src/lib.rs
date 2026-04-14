#![no_std]

//! Shared display-protocol types — the pieces of
//! `specs/001-browser-os-v1/contracts/display-protocol.md`
//! that both ends of a connection need to agree on.
//!
//! Both `display-server` (server-side state machine + accept
//! loop) and `toolkit` (client-side library) depend on this
//! crate so there is exactly one implementation of the wire
//! format, the object-ID partitioning, and the interface
//! opcode tables. The moment they diverge, the
//! toolkit-free-client conformance fixture (Principle VII)
//! would fail loudly.
//!
//! The crate is `no_std + alloc` because it has no business
//! pulling in std: it's pure data + byte munging. Both the
//! server (which runs as a wasm32-wasi userland process in
//! production) and the toolkit (which is statically linked
//! into apps) need it to work with just `core + alloc`.

extern crate alloc;

pub mod decode;
pub mod events;
pub mod ids;
pub mod objects;
pub mod requests;
pub mod wire;

pub use decode::DecodeError;
pub use events::{
    BufferRelease, DisplayDeleteId, DisplayError, RegistryGlobal, RegistryGlobalRemove,
    ShellWindowCreated, ShellWindowDestroyed, ShellWindowFocused, ShellWindowTitleChanged,
    ShmFormat,
};
pub use ids::{IdAllocator, IdError, IdKind, ObjectId, ObjectIdAllocationError};
pub use objects::{Direction, Interface, Opcode, OpcodeError};
pub use requests::{
    CompositorCreateSurface, DisplayGetRegistry, RegistryBind, ShellManagerCloseWindow,
    ShellManagerFocusWindow, ShellManagerMinimizeWindow, ShellManagerSubscribeWindows,
    SurfaceAttach, SurfaceDamage,
};
pub use wire::{MessageHeader, WireError, HEADER_SIZE, MAX_MESSAGE_SIZE};
