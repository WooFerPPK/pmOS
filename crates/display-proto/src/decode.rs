//! Payload primitive decoders.
//!
//! Payloads on this protocol are little-endian throughout.
//! Integers are u32 / i32 / u16 / u8. Strings are
//! length-prefixed UTF-8 padded to 4-byte boundaries per
//! `contracts/display-protocol.md §1`. Object IDs are wire-
//! level u32 values.
//!
//! The helpers here are the lowest layer: they read a single
//! field at a given byte offset and return it or a
//! [`DecodeError`]. Typed per-request struct decoders live in
//! [`crate::requests`] and compose these primitives.

use core::str;

use crate::ids::ObjectId;

/// Errors produced by the payload decoders.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The payload ended before `need` bytes at `offset`
    /// could be read.
    Truncated {
        offset: usize,
        need: usize,
        have: usize,
    },
    /// The length prefix on a wire string claimed more bytes
    /// than the payload actually contains.
    StringOverrun { offset: usize, claimed: usize },
    /// A wire string was not valid UTF-8.
    InvalidUtf8 { offset: usize },
}

/// Read a little-endian `u32` at `offset`.
pub fn read_u32(payload: &[u8], offset: usize) -> Result<u32, DecodeError> {
    if offset + 4 > payload.len() {
        return Err(DecodeError::Truncated {
            offset,
            need: 4,
            have: payload.len().saturating_sub(offset),
        });
    }
    Ok(u32::from_le_bytes([
        payload[offset],
        payload[offset + 1],
        payload[offset + 2],
        payload[offset + 3],
    ]))
}

/// Read a little-endian `i32` at `offset`. Wire
/// representation is the same as a u32 at the bit level.
pub fn read_i32(payload: &[u8], offset: usize) -> Result<i32, DecodeError> {
    Ok(read_u32(payload, offset)? as i32)
}

/// Read an [`ObjectId`] (wire representation: u32) at
/// `offset`.
pub fn read_object_id(payload: &[u8], offset: usize) -> Result<ObjectId, DecodeError> {
    Ok(ObjectId::new(read_u32(payload, offset)?))
}

/// Read a length-prefixed UTF-8 string at `offset`. Returns
/// the string and the number of payload bytes consumed
/// (length field + content + padding).
///
/// Layout: `u32 byte_length`, `byte_length` bytes of UTF-8,
/// then zero padding to the next 4-byte boundary.
pub fn read_string(payload: &[u8], offset: usize) -> Result<(&str, usize), DecodeError> {
    let length = read_u32(payload, offset)? as usize;
    let content_start = offset + 4;
    let content_end = content_start + length;
    if content_end > payload.len() {
        return Err(DecodeError::StringOverrun {
            offset,
            claimed: length,
        });
    }
    let content = &payload[content_start..content_end];
    let s = str::from_utf8(content).map_err(|_| DecodeError::InvalidUtf8 { offset })?;
    // Advance over trailing zero padding.
    let pad = (4 - (length % 4)) % 4;
    let consumed = 4 + length + pad;
    Ok((s, consumed))
}
