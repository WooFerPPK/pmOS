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
pub mod protocol;
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

/// Best-effort check: does `bytes` parse as a complete protocol
/// message? Returns `Some(total_len)` if a header decodes AND the
/// header's `length` field equals `bytes.len()`. Anything else
/// (truncated header, short buffer, mismatched length, malformed
/// flags) returns `None` so a caller can fall back to a non-
/// protocol byte path. Used by the binary's main loop to route
/// the legacy demo client's 16-byte raw RGBA payload through a
/// raw-blit code path while sending real protocol messages
/// through `Server::dispatch_request`.
pub fn detect_protocol_message(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < HEADER_SIZE {
        return None;
    }
    let header = MessageHeader::decode(bytes).ok()?;
    let total = header.length as usize;
    if total == bytes.len() {
        Some(total)
    } else {
        None
    }
}

#[cfg(test)]
mod detect_tests {
    use super::*;

    fn encode_header(object_id: ObjectId, opcode: u16, total_len: u16) -> Vec<u8> {
        let header = MessageHeader {
            object_id,
            opcode,
            length: total_len,
            fd_passing: 0,
        };
        let mut buf = vec![0u8; HEADER_SIZE];
        header.encode(&mut buf).unwrap();
        buf
    }

    #[test]
    fn empty_input_is_not_a_protocol_message() {
        assert_eq!(detect_protocol_message(&[]), None);
    }

    #[test]
    fn a_buffer_shorter_than_a_header_is_not_a_protocol_message() {
        assert_eq!(detect_protocol_message(&[0u8; 4]), None);
    }

    #[test]
    fn a_well_formed_header_with_matching_length_is_a_protocol_message() {
        let bytes = encode_header(ObjectId::DISPLAY, 1, HEADER_SIZE as u16);
        assert_eq!(detect_protocol_message(&bytes), Some(HEADER_SIZE));
    }

    #[test]
    fn a_header_with_length_below_header_size_returns_none() {
        // length=5 (below HEADER_SIZE=10) is rejected by
        // MessageHeader::decode itself.
        let mut bytes = vec![0u8; HEADER_SIZE];
        // object_id u32 LE
        bytes[0..4].copy_from_slice(&1u32.to_le_bytes());
        // opcode u16 LE
        bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
        // length u16 LE = 5 (below HEADER_SIZE)
        bytes[6..8].copy_from_slice(&5u16.to_le_bytes());
        // fd_passing + reserved = 0
        assert_eq!(detect_protocol_message(&bytes), None);
    }

    #[test]
    fn a_header_with_length_field_smaller_than_bytes_returns_none() {
        // 16 bytes received, but header claims length=10. The
        // detect helper rejects this: the caller is sending one
        // message at a time, so length must equal bytes.len().
        let mut bytes = encode_header(ObjectId::DISPLAY, 1, HEADER_SIZE as u16);
        bytes.extend_from_slice(&[0xff; 6]); // 16 total
        assert_eq!(detect_protocol_message(&bytes), None);
    }

    #[test]
    fn a_header_with_length_field_larger_than_bytes_returns_none() {
        // bytes.len()=10 (just the header), but length field
        // claims 20 (truncated payload).
        let bytes = encode_header(ObjectId::DISPLAY, 1, 20);
        assert_eq!(detect_protocol_message(&bytes), None);
    }

    #[test]
    fn the_demo_clients_16_byte_rgba_payload_almost_never_decodes_as_a_header() {
        // Realistic demo payload: 4 RGBA pixels, all 0x33.
        // The "length" field (bytes[6..8]) reads as 0x3333 =
        // 13107, far past the 16-byte buffer size, so detect
        // returns None and the caller falls back to raw-blit.
        let bytes = [0x33u8; 16];
        assert_eq!(detect_protocol_message(&bytes), None);
    }
}
