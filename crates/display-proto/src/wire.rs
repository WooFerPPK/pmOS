//! Message framing for the display protocol.
//!
//! Every message — request (client → server) or event (server →
//! client) — is a 10-byte header followed by a payload. The
//! canonical specification is
//! `specs/001-browser-os-v1/contracts/display-protocol.md §1`
//! and the layout is:
//!
//! ```text
//! offset   field         width
//!   0      object_id     u32   target object in the client's id space
//!   4      opcode        u16   interface-specific operation id
//!   6      length        u16   total message length in bytes (header + payload)
//!   8      fd_passing    u8    count of fds on the ipc_send side channel
//!   9      _reserved     u8    MUST be 0
//! ```
//!
//! All integers are little-endian. The `length` field is
//! **inclusive of the header**, so `length == HEADER_SIZE`
//! means "header only, zero-byte payload" and `length <
//! HEADER_SIZE` is a malformed message.
//!
//! The protocol contract and this codec both define a 10-byte header,
//! so the payload length is `length - HEADER_SIZE`.
//!
//! The [`MessageHeader::decode`] / [`MessageHeader::encode`]
//! functions are the only public framing API. They operate on
//! raw `&[u8]` / `&mut [u8]` so callers can plug them into any
//! transport.

use super::ids::ObjectId;

/// Fixed size of a message header in bytes.
pub const HEADER_SIZE: usize = 10;

/// Maximum total length of a single message, bounded by the
/// 16-bit `length` field.
pub const MAX_MESSAGE_SIZE: usize = u16::MAX as usize;

/// Errors returned by wire-format helpers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// The input buffer is shorter than one header.
    Truncated,
    /// `length` field is below [`HEADER_SIZE`] or larger than
    /// the available bytes in the input buffer.
    InvalidLength,
    /// The reserved byte was non-zero.
    ReservedSet,
    /// An attempt to encode a header whose `length` would
    /// exceed [`MAX_MESSAGE_SIZE`].
    Overflow,
    /// The output buffer for [`MessageHeader::encode`] is smaller
    /// than [`HEADER_SIZE`].
    OutputTooSmall,
}

/// A decoded message header. See the module docs for the wire
/// layout.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MessageHeader {
    pub object_id: ObjectId,
    pub opcode: u16,
    /// Total message length in bytes, including this header.
    pub length: u16,
    pub fd_passing: u8,
}

impl MessageHeader {
    /// Construct a header. The caller is responsible for making
    /// sure `payload_len + HEADER_SIZE <= u16::MAX`; use
    /// [`MessageHeader::try_new`] for the checked version.
    pub const fn new(object_id: ObjectId, opcode: u16, payload_len: u16, fd_passing: u8) -> Self {
        let length = payload_len.saturating_add(HEADER_SIZE as u16);
        MessageHeader {
            object_id,
            opcode,
            length,
            fd_passing,
        }
    }

    /// Checked constructor: fails with [`WireError::Overflow`]
    /// if `payload_len` plus the header would exceed the
    /// 16-bit `length` field.
    pub const fn try_new(
        object_id: ObjectId,
        opcode: u16,
        payload_len: usize,
        fd_passing: u8,
    ) -> Result<Self, WireError> {
        if payload_len + HEADER_SIZE > MAX_MESSAGE_SIZE {
            return Err(WireError::Overflow);
        }
        Ok(MessageHeader {
            object_id,
            opcode,
            length: (payload_len + HEADER_SIZE) as u16,
            fd_passing,
        })
    }

    /// Length of this message's payload in bytes.
    #[inline]
    pub const fn payload_len(&self) -> usize {
        (self.length as usize).saturating_sub(HEADER_SIZE)
    }

    /// Parse a header from the start of `buf`. Returns the
    /// header on success; does NOT advance the slice.
    pub fn decode(buf: &[u8]) -> Result<Self, WireError> {
        if buf.len() < HEADER_SIZE {
            return Err(WireError::Truncated);
        }
        let object_raw = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let opcode = u16::from_le_bytes([buf[4], buf[5]]);
        let length = u16::from_le_bytes([buf[6], buf[7]]);
        let fd_passing = buf[8];
        let reserved = buf[9];

        if reserved != 0 {
            return Err(WireError::ReservedSet);
        }
        if (length as usize) < HEADER_SIZE || (length as usize) > buf.len() {
            return Err(WireError::InvalidLength);
        }

        Ok(MessageHeader {
            object_id: ObjectId::new(object_raw),
            opcode,
            length,
            fd_passing,
        })
    }

    /// Write this header to `out`. Returns `HEADER_SIZE` on
    /// success.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        if out.len() < HEADER_SIZE {
            return Err(WireError::OutputTooSmall);
        }
        let id = self.object_id.raw().to_le_bytes();
        let op = self.opcode.to_le_bytes();
        let len = self.length.to_le_bytes();
        out[0] = id[0];
        out[1] = id[1];
        out[2] = id[2];
        out[3] = id[3];
        out[4] = op[0];
        out[5] = op[1];
        out[6] = len[0];
        out[7] = len[1];
        out[8] = self.fd_passing;
        out[9] = 0;
        Ok(HEADER_SIZE)
    }
}
