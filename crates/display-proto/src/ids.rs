//! Object ID types and the odd/even allocator.
//!
//! Per `display-protocol.md §2`, each client has its own
//! independent object ID namespace rooted at `ObjectId::DISPLAY`
//! (ID 1 — the implicit `pmd_display` object). IDs are
//! partitioned so odd IDs are allocated by the client and even
//! IDs are allocated by the server:
//!
//! > A client allocates IDs using odd/even partitioning: odd
//! > IDs are allocated by the client; even IDs are allocated by
//! > the server. Both sides MUST respect each other's partition.
//!
//! [`IdAllocator`] enforces this partitioning and is used by
//! both the server (for server-allocated IDs in a given client
//! connection: `pmd_output`, `pmd_seat`, etc.) and by tests
//! that simulate a client connection.

/// An ID into a client's object space.
///
/// `ObjectId::DISPLAY` (1) is always the root `pmd_display`
/// object, pre-bound on every connection. `ObjectId::NULL` (0)
/// is a sentinel meaning "no object" — it is used in protocol
/// messages where an argument is optional (e.g.
/// `pmd_surface.attach(buffer_id=0, ...)` detaches).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId(pub u32);

impl ObjectId {
    /// Sentinel "no object".
    pub const NULL: ObjectId = ObjectId(0);
    /// The implicit root `pmd_display` object.
    pub const DISPLAY: ObjectId = ObjectId(1);

    pub const fn new(raw: u32) -> Self {
        ObjectId(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// Which side of the odd/even partition does this ID belong
    /// to?
    pub const fn kind(self) -> IdKind {
        if self.0 == 0 {
            IdKind::Null
        } else if self.0 & 1 == 1 {
            IdKind::Client
        } else {
            IdKind::Server
        }
    }
}

/// Which side owns an ID. The null ID is its own variant so
/// callers can match exhaustively.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IdKind {
    Null,
    Client,
    Server,
}

/// Errors returned when allocating an ID.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum IdError {
    /// 32-bit space for this allocator's partition was exhausted.
    /// Client allocators run out of odd IDs (1, 3, 5, …); server
    /// allocators run out of even IDs (2, 4, 6, …). In practice
    /// neither ever happens in v1 — the v1 window count is in
    /// the thousands at most — but we check anyway.
    Exhausted,
}

/// Alias for the error type returned by
/// [`IdAllocator::allocate`]. Kept as a separate name so the
/// library's public re-export in `lib.rs` reads naturally
/// (`ObjectIdAllocationError`).
pub type ObjectIdAllocationError = IdError;

/// Monotonic allocator constrained to one side of the odd/even
/// partition.
///
/// * A client-side allocator hands out 1, 3, 5, 7, …
/// * A server-side allocator hands out 2, 4, 6, 8, …
///
/// (The server starts its sequence at 2 because ObjectId 0 is
/// the null sentinel and ObjectId 1 is the implicit display
/// object.)
#[derive(Debug, Clone)]
pub struct IdAllocator {
    kind: IdKind,
    next: u32,
}

impl IdAllocator {
    /// Build a fresh client-side allocator. First allocation
    /// returns `ObjectId(3)` — ID 1 is the pre-bound display
    /// object, so `1` is never handed out again.
    pub const fn for_client() -> Self {
        IdAllocator {
            kind: IdKind::Client,
            next: 3,
        }
    }

    /// Build a fresh server-side allocator. First allocation
    /// returns `ObjectId(2)`.
    pub const fn for_server() -> Self {
        IdAllocator {
            kind: IdKind::Server,
            next: 2,
        }
    }

    /// Which side this allocator runs on.
    pub const fn kind(&self) -> IdKind {
        self.kind
    }

    /// Peek at the next ID that `allocate` will return.
    pub const fn peek(&self) -> ObjectId {
        ObjectId(self.next)
    }

    /// Allocate a fresh ID.
    pub fn allocate(&mut self) -> Result<ObjectId, IdError> {
        if self.next == 0 {
            return Err(IdError::Exhausted);
        }
        let id = ObjectId(self.next);
        // Advance by 2 to stay within the partition; wrap
        // around to 0 is the exhaustion signal on the NEXT call.
        self.next = self.next.checked_add(2).unwrap_or(0);
        Ok(id)
    }
}
