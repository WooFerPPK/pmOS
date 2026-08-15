//! Bounded transport-side buffering for one display connection.
//!
//! Protocol events first enter the per-client queue in [`crate::Client`]. The
//! production binary drains those framed events into an [`OutboundQueue`] while
//! the kernel socket applies backpressure. Keeping this second queue bounded is
//! essential: a client that never reads must not make the display server retain
//! bytes indefinitely.

use alloc::vec::Vec;

/// Maximum event bytes retained after the protocol queue has been drained but
/// before the peer socket accepts them.
pub const MAX_CONN_OUTBOUND_BYTES: usize = 256 * 1024;

/// An append would exceed the connection's transport-side byte budget.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct OutboundQueueFull {
    pub queued: usize,
    pub incoming: usize,
    pub max: usize,
}

/// Byte queue used by the production connection loop during partial writes.
///
/// `append` is all-or-nothing. Production treats [`OutboundQueueFull`] as a
/// slow/non-reading peer and disconnects it; a successful partial write merely
/// consumes the written prefix and retries the remainder on a later loop turn.
pub struct OutboundQueue {
    bytes: Vec<u8>,
    consumed: usize,
    max_bytes: usize,
}

impl OutboundQueue {
    /// Construct a queue with the production byte ceiling.
    pub fn new() -> Self {
        Self::with_limit(MAX_CONN_OUTBOUND_BYTES)
    }

    /// Construct a queue with an explicit ceiling. This keeps adversarial
    /// isolation tests small without weakening the production constant.
    pub fn with_limit(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            consumed: 0,
            max_bytes,
        }
    }

    /// Append one complete event batch without ever exceeding the configured
    /// logical or backing-allocation ceiling.
    pub fn append(&mut self, incoming: &[u8]) -> Result<(), OutboundQueueFull> {
        let queued = self.len();
        let attempted = queued.saturating_add(incoming.len());
        if attempted > self.max_bytes {
            return Err(OutboundQueueFull {
                queued,
                incoming: incoming.len(),
                max: self.max_bytes,
            });
        }
        if incoming.is_empty() {
            return Ok(());
        }
        self.compact();
        self.bytes.extend_from_slice(incoming);
        Ok(())
    }

    /// Bytes currently awaiting a socket write.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[self.consumed..]
    }

    /// Account for a successful socket write. Values larger than the queued
    /// length consume the whole queue defensively.
    pub fn consume(&mut self, written: usize) {
        self.consumed = self.consumed.saturating_add(written).min(self.bytes.len());
        if self.consumed == self.bytes.len() {
            self.bytes.clear();
            self.consumed = 0;
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len().saturating_sub(self.consumed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Additional bytes that can be appended without crossing the logical
    /// transport ceiling. The production scheduler uses this to leave complete
    /// protocol events in the client queue until a partial socket write frees
    /// enough room, rather than disconnecting a responsive backpressured peer.
    pub fn remaining_capacity(&self) -> usize {
        self.max_bytes.saturating_sub(self.len())
    }

    fn compact(&mut self) {
        if self.consumed == 0 {
            return;
        }
        self.bytes.drain(..self.consumed);
        self.consumed = 0;
    }
}

impl Default for OutboundQueue {
    fn default() -> Self {
        Self::new()
    }
}
