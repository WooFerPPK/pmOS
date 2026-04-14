//! Payload primitive decoder tests.
//!
//! Pin down each primitive in `display_proto::decode` so the
//! server-side auto-install path and the toolkit's own
//! decoders can share the same behaviour. If any of these
//! tests change, both sides of the protocol need a coordinated
//! update.

use display_proto::decode::{read_i32, read_object_id, read_string, read_u32, DecodeError};
use display_proto::ObjectId;

#[test]
fn read_u32_decodes_little_endian_at_offset_zero() {
    let buf = [0x78, 0x56, 0x34, 0x12];
    assert_eq!(read_u32(&buf, 0).unwrap(), 0x1234_5678);
}

#[test]
fn read_u32_decodes_at_non_zero_offset() {
    let buf = [0u8, 0, 0x78, 0x56, 0x34, 0x12];
    assert_eq!(read_u32(&buf, 2).unwrap(), 0x1234_5678);
}

#[test]
fn read_u32_truncated_returns_error() {
    let buf = [0u8; 3];
    let err = read_u32(&buf, 0).unwrap_err();
    assert_eq!(
        err,
        DecodeError::Truncated {
            offset: 0,
            need: 4,
            have: 3
        }
    );
}

#[test]
fn read_i32_preserves_sign() {
    let minus_one = (-1i32).to_le_bytes();
    assert_eq!(read_i32(&minus_one, 0).unwrap(), -1);
    let minus_large = (i32::MIN).to_le_bytes();
    assert_eq!(read_i32(&minus_large, 0).unwrap(), i32::MIN);
}

#[test]
fn read_object_id_round_trips() {
    let id = ObjectId::new(42);
    let bytes = 42u32.to_le_bytes();
    assert_eq!(read_object_id(&bytes, 0).unwrap(), id);
}

#[test]
fn read_string_decodes_empty_string_with_no_padding() {
    let buf = [0, 0, 0, 0];
    let (s, consumed) = read_string(&buf, 0).unwrap();
    assert_eq!(s, "");
    assert_eq!(consumed, 4);
}

#[test]
fn read_string_decodes_three_byte_string_with_one_pad_byte() {
    // len=3, "abc", pad=1 → 8 bytes total.
    let buf = [3, 0, 0, 0, b'a', b'b', b'c', 0];
    let (s, consumed) = read_string(&buf, 0).unwrap();
    assert_eq!(s, "abc");
    assert_eq!(consumed, 8);
}

#[test]
fn read_string_decodes_four_byte_string_with_no_padding() {
    let buf = [4, 0, 0, 0, b'a', b'b', b'c', b'd'];
    let (s, consumed) = read_string(&buf, 0).unwrap();
    assert_eq!(s, "abcd");
    assert_eq!(consumed, 8);
}

#[test]
fn read_string_at_non_zero_offset() {
    let mut buf = vec![0u8; 4];
    buf.extend_from_slice(&3u32.to_le_bytes());
    buf.extend_from_slice(b"foo");
    buf.push(0); // padding
    let (s, consumed) = read_string(&buf, 4).unwrap();
    assert_eq!(s, "foo");
    assert_eq!(consumed, 8);
}

#[test]
fn read_string_with_length_overrunning_buffer_fails() {
    // length=10 but only 4 bytes of content available.
    let mut buf = vec![];
    buf.extend_from_slice(&10u32.to_le_bytes());
    buf.extend_from_slice(b"abcd");
    let err = read_string(&buf, 0).unwrap_err();
    assert_eq!(
        err,
        DecodeError::StringOverrun {
            offset: 0,
            claimed: 10
        }
    );
}

#[test]
fn read_string_with_non_utf8_content_fails() {
    // 2-byte string containing an invalid lone continuation.
    let mut buf = vec![];
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.push(0xFF);
    buf.push(0xFE);
    buf.push(0);
    buf.push(0);
    let err = read_string(&buf, 0).unwrap_err();
    assert_eq!(err, DecodeError::InvalidUtf8 { offset: 0 });
}
