//! PMos window toolkit.
//!
//! Library statically linked into apps. Wraps the display
//! server wire protocol with windows, widgets, layout,
//! drawing, and an event loop integrated with frame
//! callbacks.
//!
//! Protocol layer: [`protocol`]. Mirrors the server-side
//! state machine in `display-server` (the two speak the
//! identical wire format from the shared [`display_proto`]
//! crate) but from the client's point of view — it sends
//! requests and parses events instead of receiving requests
//! and emitting events. The client's object table starts
//! with `pmd_display` pre-bound at `ObjectId::DISPLAY` and
//! grows via `bind_new` as the client allocates ids for
//! newly-bound globals and child objects.
//!
//! Higher layers (windows, widgets, drawing) are scheduled
//! for Phase 2 T114..T120 and currently exist as one-line
//! stubs — the protocol slice is the foundation.

pub mod app;
pub mod decorated_window;
pub mod draw;
pub mod layout;
pub mod protocol;
pub mod theme;
pub mod wasi;
pub mod widget;
pub mod window;
pub mod window_frame_patch;

pub use app::App;
pub use decorated_window::{DecoratedPointerOutcome, DecoratedWindow};
pub use draw::{
    BufferPool, CommitProgress, CurrentPatch, MAX_DAMAGE_REGIONS, SHM_WRITE_CHUNK_BYTES,
};
pub use protocol::{
    Client, ClientError, ClientEvent, ClientEventWithPayload, Connection, MemoryConnection, WaitFd,
    WaitInterest,
};
pub use theme::{watch_theme, Theme, ThemeWatcher};
#[cfg(target_arch = "wasm32")]
pub use wasi::wait_fd;
#[cfg(target_arch = "wasm32")]
pub use wasi::{FdConnection, FsWatch, PathWatch};
pub use window::Window;
pub use window_frame_patch::{WindowFramePatch, WindowFramePatchProgress};

// Re-export the shared protocol types so toolkit callers get
// a single namespace (`toolkit::Interface`, `toolkit::ObjectId`,
// etc.) without having to also depend on `display-proto`
// directly.
pub use display_proto::{
    Direction, IdAllocator, IdError, IdKind, Interface, MessageHeader, ObjectId, Opcode,
    OpcodeError, WireError, HEADER_SIZE,
};
