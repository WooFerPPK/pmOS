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
    // Walk the buffer header-by-header. The toolkit's send-
    // side batches consecutive requests into a single
    // fd_write, and the kernel coalesces them into one
    // rx_buf, so by the time `fd_read` returns a chunk it
    // may contain 1-N complete protocol messages. We accept
    // ANY chunk that walks cleanly to the end as protocol-
    // shaped. The legacy 16-byte raw-RGBA payload from
    // `display-client-demo` falls out of this walk on the
    // first header decode (its length field doesn't match
    // any boundary we can recover), routing the chunk to
    // the binary's raw-blit fallback.
    let mut offset = 0usize;
    while offset + HEADER_SIZE <= bytes.len() {
        let header = MessageHeader::decode(&bytes[offset..]).ok()?;
        let msg_len = header.length as usize;
        if msg_len < HEADER_SIZE {
            return None;
        }
        if offset + msg_len > bytes.len() {
            return None;
        }
        offset += msg_len;
    }
    if offset == bytes.len() {
        Some(offset)
    } else {
        None
    }
}

/// Wire size of a packed mouse input event in bytes. Mirrors
/// `web/src/shared/input-proto.ts` MOUSE_EVENT_SIZE so the
/// kernel-side `/dev/input/mouse` ring and the display-server's
/// reader speak the same format.
pub const MOUSE_EVENT_SIZE: usize = 20;

/// Wire size of a packed keyboard input event in bytes.
/// Mirrors `web/src/shared/input-proto.ts` KBD_EVENT_SIZE.
pub const KBD_EVENT_SIZE: usize = 8;

/// Discriminant values for the `kind` field of a packed
/// mouse event. Mirrors `MouseEventKind` in the TS shared
/// proto.
pub mod mouse_event_kind {
    pub const MOTION: u32 = 0;
    pub const BUTTON: u32 = 1;
    pub const WHEEL: u32 = 2;
}

/// Mouse-button-state values for the `state` field of a
/// `mouse_event_kind::BUTTON` packed event.
pub mod mouse_button_state {
    pub const RELEASED: u32 = 0;
    pub const PRESSED: u32 = 1;
}

/// Keyboard-key-state values for the `state` field of a
/// keyboard event.
pub mod kbd_key_state {
    pub const RELEASED: u32 = 0;
    pub const PRESSED: u32 = 1;
}

/// Decoded mouse event ready to inject into [`Server`]
/// via [`Server::inject_pointer_motion`] /
/// [`Server::inject_pointer_button`]. Wheel events carry
/// signed deltas; motion + button events carry pointer-state
/// changes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DecodedMouseEvent {
    Motion { x: i32, y: i32 },
    Button { x: i32, y: i32, button: u32, state: u32 },
    Wheel { x: i32, y: i32, delta_x: i32, delta_y: i32 },
}

/// Decode one packed mouse event off the front of `bytes`,
/// returning the decoded event and the number of bytes
/// consumed. Returns `None` if `bytes.len() <
/// MOUSE_EVENT_SIZE` or the kind discriminant is unknown.
/// Tests for `bytes` that hold multiple events: the caller
/// advances `bytes` by `MOUSE_EVENT_SIZE` between decodes
/// and re-calls.
pub fn decode_mouse_event(bytes: &[u8]) -> Option<(DecodedMouseEvent, usize)> {
    if bytes.len() < MOUSE_EVENT_SIZE {
        return None;
    }
    let kind = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
    let x = i32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let y = i32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let event = match kind {
        mouse_event_kind::MOTION => DecodedMouseEvent::Motion { x, y },
        mouse_event_kind::BUTTON => {
            let button = u32::from_le_bytes(bytes[12..16].try_into().ok()?);
            let state = u32::from_le_bytes(bytes[16..20].try_into().ok()?);
            if state != mouse_button_state::RELEASED && state != mouse_button_state::PRESSED {
                return None;
            }
            DecodedMouseEvent::Button { x, y, button, state }
        }
        mouse_event_kind::WHEEL => {
            let delta_x = i32::from_le_bytes(bytes[12..16].try_into().ok()?);
            let delta_y = i32::from_le_bytes(bytes[16..20].try_into().ok()?);
            DecodedMouseEvent::Wheel { x, y, delta_x, delta_y }
        }
        _ => return None,
    };
    Some((event, MOUSE_EVENT_SIZE))
}

/// Decoded keyboard event ready to inject via
/// [`Server::inject_keyboard_key`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DecodedKbdEvent {
    pub key: u32,
    pub state: u32,
}

/// Decode one packed keyboard event off the front of `bytes`,
/// same shape as [`decode_mouse_event`]. The `state` field
/// is validated against [`kbd_key_state`].
pub fn decode_kbd_event(bytes: &[u8]) -> Option<(DecodedKbdEvent, usize)> {
    if bytes.len() < KBD_EVENT_SIZE {
        return None;
    }
    let key = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
    let state = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if state != kbd_key_state::RELEASED && state != kbd_key_state::PRESSED {
        return None;
    }
    Some((DecodedKbdEvent { key, state }, KBD_EVENT_SIZE))
}

/// Drain every complete mouse event from `bytes` and inject
/// each into `server`. Returns the number of events
/// successfully injected. Stops at the first malformed event
/// (kind out of range, state out of range, etc.) — the
/// caller decides whether to also log/restart the input
/// stream. Trailing partial bytes (less than
/// [`MOUSE_EVENT_SIZE`]) are silently discarded.
pub fn drain_mouse_events_into(bytes: &[u8], server: &mut Server) -> usize {
    let mut consumed = 0usize;
    let mut count = 0usize;
    while consumed + MOUSE_EVENT_SIZE <= bytes.len() {
        let Some((event, used)) = decode_mouse_event(&bytes[consumed..]) else {
            break;
        };
        match event {
            DecodedMouseEvent::Motion { x, y } => {
                server.inject_pointer_motion(x, y);
            }
            DecodedMouseEvent::Button { button, state, .. } => {
                server.inject_pointer_button(button, state);
            }
            DecodedMouseEvent::Wheel { .. } => {
                // v1 doesn't route wheel events to clients yet;
                // the shell's window manager will pick this up
                // in a future slice. Dropping the event keeps
                // the contract that "every input byte is
                // consumed" while not yet propagating wheel.
            }
        }
        consumed += used;
        count += 1;
    }
    count
}

/// Drain every complete keyboard event from `bytes` and
/// inject each into `server`. Same shape as
/// [`drain_mouse_events_into`].
pub fn drain_kbd_events_into(bytes: &[u8], server: &mut Server) -> usize {
    let mut consumed = 0usize;
    let mut count = 0usize;
    while consumed + KBD_EVENT_SIZE <= bytes.len() {
        let Some((event, used)) = decode_kbd_event(&bytes[consumed..]) else {
            break;
        };
        server.inject_keyboard_key(event.key, event.state);
        consumed += used;
        count += 1;
    }
    count
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
