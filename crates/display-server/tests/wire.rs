//! Wire-format isolation tests.
//!
//! Encodes / decodes `MessageHeader` in both directions and
//! pins down the invariants that `wire.rs` depends on.

use display_server::wire::{MessageHeader, WireError, HEADER_SIZE};
use display_server::ObjectId;

#[test]
fn header_size_is_ten_bytes() {
    assert_eq!(HEADER_SIZE, 10);
}

#[test]
fn encode_decode_round_trip() {
    let header = MessageHeader::new(ObjectId::new(42), 7, 0, 0);
    let mut buf = [0u8; HEADER_SIZE];
    let written = header.encode(&mut buf).unwrap();
    assert_eq!(written, HEADER_SIZE);
    let decoded = MessageHeader::decode(&buf).unwrap();
    assert_eq!(decoded, header);
    assert_eq!(decoded.object_id.raw(), 42);
    assert_eq!(decoded.opcode, 7);
    assert_eq!(decoded.length, HEADER_SIZE as u16);
    assert_eq!(decoded.fd_passing, 0);
    assert_eq!(decoded.payload_len(), 0);
}

#[test]
fn encode_stores_integers_little_endian() {
    let header = MessageHeader::new(ObjectId::new(0x11223344), 0xAABB, 0, 0);
    let mut buf = [0u8; HEADER_SIZE];
    header.encode(&mut buf).unwrap();
    assert_eq!(&buf[0..4], &[0x44, 0x33, 0x22, 0x11]);
    assert_eq!(&buf[4..6], &[0xBB, 0xAA]);
}

#[test]
fn header_with_payload_reports_correct_payload_len() {
    let header = MessageHeader::try_new(ObjectId::DISPLAY, 1, 16, 0).unwrap();
    assert_eq!(header.length, (HEADER_SIZE + 16) as u16);
    assert_eq!(header.payload_len(), 16);
}

#[test]
fn decode_truncated_input_returns_truncated() {
    // Header is 10 bytes. Any input shorter than that fails.
    for len in 0..HEADER_SIZE {
        let buf = vec![0u8; len];
        assert_eq!(
            MessageHeader::decode(&buf).unwrap_err(),
            WireError::Truncated
        );
    }
}

#[test]
fn decode_length_below_header_size_is_invalid() {
    let mut buf = [0u8; HEADER_SIZE];
    let bad_length: u16 = 5;
    buf[6..8].copy_from_slice(&bad_length.to_le_bytes());
    assert_eq!(
        MessageHeader::decode(&buf).unwrap_err(),
        WireError::InvalidLength
    );
}

#[test]
fn decode_length_larger_than_buffer_is_invalid() {
    let mut buf = [0u8; HEADER_SIZE];
    let too_big: u16 = (HEADER_SIZE + 8) as u16;
    buf[6..8].copy_from_slice(&too_big.to_le_bytes());
    assert_eq!(
        MessageHeader::decode(&buf).unwrap_err(),
        WireError::InvalidLength
    );
}

#[test]
fn decode_non_zero_reserved_byte_is_rejected() {
    let mut buf = [0u8; HEADER_SIZE];
    let len: u16 = HEADER_SIZE as u16;
    buf[6..8].copy_from_slice(&len.to_le_bytes());
    buf[9] = 0x01; // reserved byte
    assert_eq!(
        MessageHeader::decode(&buf).unwrap_err(),
        WireError::ReservedSet
    );
}

#[test]
fn encode_into_short_output_fails() {
    let header = MessageHeader::new(ObjectId::DISPLAY, 1, 0, 0);
    let mut buf = [0u8; HEADER_SIZE - 1];
    assert_eq!(
        header.encode(&mut buf).unwrap_err(),
        WireError::OutputTooSmall
    );
}

#[test]
fn try_new_rejects_oversized_messages() {
    let err = MessageHeader::try_new(ObjectId::DISPLAY, 1, u16::MAX as usize, 0).unwrap_err();
    assert_eq!(err, WireError::Overflow);
}

#[test]
fn decode_accepts_a_full_message_with_payload_after_the_header() {
    // A 16-byte payload, total message = 26 bytes.
    let mut buf = vec![0u8; HEADER_SIZE + 16];
    let hdr = MessageHeader::try_new(ObjectId::DISPLAY, 2, 16, 0).unwrap();
    hdr.encode(&mut buf[..HEADER_SIZE]).unwrap();
    // Put some recognizable payload bytes.
    for (i, b) in buf[HEADER_SIZE..].iter_mut().enumerate() {
        *b = i as u8;
    }
    let decoded = MessageHeader::decode(&buf).unwrap();
    assert_eq!(decoded.length as usize, HEADER_SIZE + 16);
    assert_eq!(decoded.payload_len(), 16);
}

#[test]
fn fd_passing_round_trips_through_encode_decode() {
    let header = MessageHeader::new(ObjectId::new(10), 1, 0, 3);
    let mut buf = [0u8; HEADER_SIZE];
    header.encode(&mut buf).unwrap();
    let decoded = MessageHeader::decode(&buf).unwrap();
    assert_eq!(decoded.fd_passing, 3);
}
