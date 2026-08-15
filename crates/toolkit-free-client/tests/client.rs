//! Unit tests for the hand-written `FreeClient` helpers.
//!
//! These pin down the wire-format output of each helper
//! method in isolation — a reader can match each assertion
//! back to a row in `contracts/display-protocol.md`.

use display_proto::wire::{MessageHeader, HEADER_SIZE};
use display_proto::ObjectId;
use toolkit_free_client::{
    append_wire_string, conformance_frame, FreeClient, FreeClientError, FRAME_ACCENT_RGBA,
    FRAME_BYTES, FRAME_HEIGHT, FRAME_WIDTH, INITIAL_FRAME_RGBA, INPUT_FRAME_RGBA,
    OP_COMPOSITOR_CREATE_SURFACE, OP_DISPLAY_GET_REGISTRY, OP_REGISTRY_BIND, OP_SEAT_GET_KEYBOARD,
    OP_SHM_CREATE_POOL, OP_SHM_POOL_CREATE_BUFFER, OP_SHM_POOL_WRITE, OP_SURFACE_ATTACH,
    OP_SURFACE_COMMIT, OP_SURFACE_DAMAGE, OP_XDG_SHELL_GET_TOPLEVEL, OP_XDG_TOPLEVEL_ACK_CONFIGURE,
    OP_XDG_TOPLEVEL_SET_APP_ID, OP_XDG_TOPLEVEL_SET_TITLE,
};

fn decode_front(bytes: &[u8]) -> (MessageHeader, &[u8], usize) {
    let header = MessageHeader::decode(bytes).unwrap();
    let end = header.length as usize;
    (header, &bytes[HEADER_SIZE..end], end)
}

#[test]
fn client_starts_with_an_empty_outbound_queue_and_client_id_allocator_at_three() {
    let mut c = FreeClient::new();
    assert_eq!(c.outbound_len(), 0);
    // First allocation returns 3 (1 is pre-bound display,
    // 2 is on the server partition).
    let first = c.allocate_id().unwrap();
    assert_eq!(first.raw(), 3);
}

#[test]
fn allocate_id_stays_on_the_odd_partition() {
    let mut c = FreeClient::new();
    let mut seen = Vec::new();
    for _ in 0..6 {
        seen.push(c.allocate_id().unwrap().raw());
    }
    assert_eq!(seen, vec![3, 5, 7, 9, 11, 13]);
}

#[test]
fn get_registry_emits_display_opcode_2_with_a_4_byte_new_id_payload() {
    let mut c = FreeClient::new();
    let registry_id = c.get_registry().unwrap();
    assert_eq!(registry_id.raw(), 3);

    let bytes = c.drain_outbound();
    let (header, payload, end) = decode_front(&bytes);
    assert_eq!(end, bytes.len(), "only one message on the wire");
    assert_eq!(header.object_id, ObjectId::DISPLAY);
    assert_eq!(header.opcode, OP_DISPLAY_GET_REGISTRY);
    assert_eq!(header.payload_len(), 4);
    // Payload carries the allocated new_id little-endian.
    let new_id = u32::from_le_bytes(payload.try_into().unwrap());
    assert_eq!(new_id, registry_id.raw());
}

#[test]
fn registry_bind_frames_a_u32_string_u32_u32_payload() {
    let mut c = FreeClient::new();
    let registry = c.get_registry().unwrap();
    let compositor = c.registry_bind(registry, 7, "pmd_compositor", 1).unwrap();
    assert_eq!(compositor.raw(), 5);

    let bytes = c.drain_outbound();
    // Skip the display.get_registry header+payload (14 bytes).
    let (_get_registry, _, consumed) = decode_front(&bytes);
    let rest = &bytes[consumed..];
    let (header, payload, _) = decode_front(rest);
    assert_eq!(header.object_id, registry);
    assert_eq!(header.opcode, OP_REGISTRY_BIND);

    // Layout: u32 name (0..4), string "pmd_compositor" with
    // length 14 + padding to 4-byte boundary = u32 length
    // (4..8), 14 bytes of content (8..22), 2 bytes padding
    // (22..24), u32 version (24..28), u32 new_id (28..32).
    assert_eq!(u32::from_le_bytes(payload[0..4].try_into().unwrap()), 7);
    let str_len = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    assert_eq!(str_len, 14);
    assert_eq!(&payload[8..22], b"pmd_compositor");
    assert_eq!(&payload[22..24], &[0, 0], "padding to 4-byte boundary");
    assert_eq!(u32::from_le_bytes(payload[24..28].try_into().unwrap()), 1);
    let new_id = u32::from_le_bytes(payload[28..32].try_into().unwrap());
    assert_eq!(new_id, compositor.raw());
}

#[test]
fn compositor_create_surface_payload_is_a_single_new_id() {
    let mut c = FreeClient::new();
    let registry = c.get_registry().unwrap();
    let compositor = c.registry_bind(registry, 1, "pmd_compositor", 1).unwrap();
    let surface = c.compositor_create_surface(compositor).unwrap();

    // Third message on the queue is create_surface.
    let bytes = c.drain_outbound();
    let mut cursor = 0;
    for _ in 0..2 {
        let header = MessageHeader::decode(&bytes[cursor..]).unwrap();
        cursor += header.length as usize;
    }
    let (header, payload, _) = decode_front(&bytes[cursor..]);
    assert_eq!(header.object_id, compositor);
    assert_eq!(header.opcode, OP_COMPOSITOR_CREATE_SURFACE);
    assert_eq!(header.payload_len(), 4);
    assert_eq!(
        u32::from_le_bytes(payload.try_into().unwrap()),
        surface.raw()
    );
}

#[test]
fn surface_commit_has_no_payload() {
    let mut c = FreeClient::new();
    let registry = c.get_registry().unwrap();
    let compositor = c.registry_bind(registry, 1, "pmd_compositor", 1).unwrap();
    let surface = c.compositor_create_surface(compositor).unwrap();
    c.surface_commit(surface).unwrap();

    // Walk to the 4th message.
    let bytes = c.drain_outbound();
    let mut cursor = 0;
    for _ in 0..3 {
        let header = MessageHeader::decode(&bytes[cursor..]).unwrap();
        cursor += header.length as usize;
    }
    let header = MessageHeader::decode(&bytes[cursor..]).unwrap();
    assert_eq!(header.object_id, surface);
    assert_eq!(header.opcode, OP_SURFACE_COMMIT);
    assert_eq!(header.payload_len(), 0);
    assert_eq!(header.length as usize, HEADER_SIZE);
}

#[test]
fn surface_attach_payload_is_u32_i32_i32() {
    let mut c = FreeClient::new();
    let surface = c.allocate_id().unwrap(); // pretend it's a surface
    c.surface_attach(surface, ObjectId::new(9), -5, 12).unwrap();
    let bytes = c.drain_outbound();
    let (header, payload, _) = decode_front(&bytes);
    assert_eq!(header.object_id, surface);
    assert_eq!(header.opcode, OP_SURFACE_ATTACH);
    assert_eq!(header.payload_len(), 12);
    assert_eq!(u32::from_le_bytes(payload[0..4].try_into().unwrap()), 9);
    assert_eq!(i32::from_le_bytes(payload[4..8].try_into().unwrap()), -5);
    assert_eq!(i32::from_le_bytes(payload[8..12].try_into().unwrap()), 12);
}

#[test]
fn surface_damage_payload_is_four_i32s() {
    let mut c = FreeClient::new();
    let surface = c.allocate_id().unwrap();
    c.surface_damage(surface, 1, 2, 100, 200).unwrap();
    let bytes = c.drain_outbound();
    let (header, payload, _) = decode_front(&bytes);
    assert_eq!(header.opcode, OP_SURFACE_DAMAGE);
    assert_eq!(header.payload_len(), 16);
    assert_eq!(i32::from_le_bytes(payload[0..4].try_into().unwrap()), 1);
    assert_eq!(i32::from_le_bytes(payload[4..8].try_into().unwrap()), 2);
    assert_eq!(i32::from_le_bytes(payload[8..12].try_into().unwrap()), 100);
    assert_eq!(i32::from_le_bytes(payload[12..16].try_into().unwrap()), 200);
}

#[test]
fn encode_request_on_oversized_payload_returns_wire_error() {
    let mut c = FreeClient::new();
    // The header's `length` field is u16, so a payload that
    // would push the total over u16::MAX is rejected.
    let big = vec![0u8; u16::MAX as usize];
    let err = c.encode_request(ObjectId::DISPLAY, 1, &big).unwrap_err();
    match err {
        FreeClientError::Wire(_) => {}
        FreeClientError::IdsExhausted => {
            panic!("expected Wire, got IdsExhausted")
        }
    }
}

#[test]
fn try_decode_event_recognises_a_full_message_and_reports_consumption() {
    // Build a display.error event: opcode 1, 4-byte payload.
    let mut buf = vec![0u8; HEADER_SIZE + 4];
    let hdr = MessageHeader::try_new(ObjectId::DISPLAY, 1, 4, 0).unwrap();
    hdr.encode(&mut buf[..HEADER_SIZE]).unwrap();
    let outcome = FreeClient::try_decode_event(&buf);
    let (header, consumed) = outcome.unwrap().unwrap();
    assert_eq!(header.opcode, 1);
    assert_eq!(consumed, HEADER_SIZE + 4);
}

#[test]
fn try_decode_event_returns_none_on_partial_input() {
    let short = [0u8; HEADER_SIZE - 1];
    assert!(FreeClient::try_decode_event(&short).is_none());
    // A complete header that claims a bigger payload than we
    // have is also "need more bytes", surfaced as None.
    let mut almost = vec![0u8; HEADER_SIZE];
    let hdr = MessageHeader::try_new(ObjectId::DISPLAY, 1, 100, 0).unwrap();
    hdr.encode(&mut almost).unwrap();
    assert!(FreeClient::try_decode_event(&almost).is_none());
}

#[test]
fn append_wire_string_pads_to_four_byte_boundary() {
    // Empty string: 4-byte length only, no padding.
    let mut out = Vec::new();
    append_wire_string(&mut out, "");
    assert_eq!(out, vec![0, 0, 0, 0]);

    // Three bytes of content pad to four.
    let mut out = Vec::new();
    append_wire_string(&mut out, "abc");
    assert_eq!(out, vec![3, 0, 0, 0, b'a', b'b', b'c', 0]);

    // Four bytes of content need no padding.
    let mut out = Vec::new();
    append_wire_string(&mut out, "abcd");
    assert_eq!(out, vec![4, 0, 0, 0, b'a', b'b', b'c', b'd']);

    // Eight bytes of content need no padding either.
    let mut out = Vec::new();
    append_wire_string(&mut out, "hellohi!");
    assert_eq!(
        out,
        vec![8, 0, 0, 0, b'h', b'e', b'l', b'l', b'o', b'h', b'i', b'!']
    );
}

#[test]
fn raw_window_pool_role_and_keyboard_helpers_emit_the_shipped_v1_opcodes() {
    let mut client = FreeClient::new();
    let shm = client.allocate_id().unwrap();
    let pool = client.shm_create_pool(shm, 512_000).unwrap();
    let buffer = client
        .shm_pool_create_buffer(pool, 0, 320, 200, 1_280, 0)
        .unwrap();
    client.shm_pool_write(pool, 0, &[1, 2, 3, 4]).unwrap();
    let xdg_shell = client.allocate_id().unwrap();
    let surface = client.allocate_id().unwrap();
    let toplevel = client.xdg_shell_get_toplevel(xdg_shell, surface).unwrap();
    client
        .xdg_toplevel_set_title(toplevel, "Raw Protocol Window")
        .unwrap();
    client
        .xdg_toplevel_set_app_id(toplevel, "pmos.toolkit-free-client")
        .unwrap();
    client.xdg_toplevel_ack_configure(toplevel, 41).unwrap();
    let seat = client.allocate_id().unwrap();
    let keyboard = client.seat_get_keyboard(seat).unwrap();

    let bytes = client.drain_outbound();
    let mut cursor = 0usize;
    let mut messages = Vec::new();
    while cursor < bytes.len() {
        let (header, payload, consumed) = decode_front(&bytes[cursor..]);
        messages.push((header.object_id, header.opcode, payload.to_vec()));
        cursor += consumed;
    }
    assert_eq!(messages[0].0, shm);
    assert_eq!(messages[0].1, OP_SHM_CREATE_POOL);
    assert_eq!(messages[1].0, pool);
    assert_eq!(messages[1].1, OP_SHM_POOL_CREATE_BUFFER);
    assert_eq!(messages[2].1, OP_SHM_POOL_WRITE);
    assert_eq!(messages[3].0, xdg_shell);
    assert_eq!(messages[3].1, OP_XDG_SHELL_GET_TOPLEVEL);
    assert_eq!(messages[4].1, OP_XDG_TOPLEVEL_SET_TITLE);
    assert_eq!(messages[5].1, OP_XDG_TOPLEVEL_SET_APP_ID);
    assert_eq!(messages[6].1, OP_XDG_TOPLEVEL_ACK_CONFIGURE);
    assert_eq!(messages[7].0, seat);
    assert_eq!(messages[7].1, OP_SEAT_GET_KEYBOARD);
    assert_eq!(
        u32::from_le_bytes(messages[7].2.as_slice().try_into().unwrap()),
        keyboard.raw()
    );
    assert_ne!(buffer, toplevel);
}

#[test]
fn conformance_frames_are_bounded_distinct_and_have_a_large_exact_colour_field() {
    let initial = conformance_frame(false);
    let input = conformance_frame(true);
    assert_eq!(initial.len(), FRAME_BYTES);
    assert_eq!(input.len(), FRAME_BYTES);
    assert_eq!(
        FRAME_BYTES,
        FRAME_WIDTH as usize * FRAME_HEIGHT as usize * 4
    );
    assert_ne!(initial, input);

    let initial_fill = initial
        .chunks_exact(4)
        .filter(|pixel| *pixel == INITIAL_FRAME_RGBA)
        .count();
    let input_fill = input
        .chunks_exact(4)
        .filter(|pixel| *pixel == INPUT_FRAME_RGBA)
        .count();
    let accents = initial
        .chunks_exact(4)
        .filter(|pixel| *pixel == FRAME_ACCENT_RGBA)
        .count();
    assert!(initial_fill > 45_000);
    assert_eq!(initial_fill, input_fill);
    assert!(accents > 5_000);
}
