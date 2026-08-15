//! Typed event encode/decode round-trip tests.
//!
//! Pins down the wire format for every event struct in
//! `display_proto::events`. Each test builds a typed
//! event, encodes it to bytes, decodes the bytes back,
//! and asserts the decoded struct is byte-identical to
//! the original — the strongest form of "encoder and
//! decoder agree" coverage.

use display_proto::events::{key_state, pointer_button_state, write_string};
use display_proto::{
    BufferRelease, DecodeError, DisplayDeleteId, DisplayError, KeyboardKey, ObjectId,
    PointerButton, PointerMotion, RegistryGlobal, RegistryGlobalRemove, ShmFormat,
};

// ---- DisplayError -------------------------------------------------

#[test]
fn display_error_round_trips() {
    let original = DisplayError {
        object_id: ObjectId::new(7),
        code: 42,
        message: "invalid request".to_string(),
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    let decoded = DisplayError::decode(&buf).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn display_error_encodes_to_stable_byte_layout() {
    let err = DisplayError {
        object_id: ObjectId::new(3),
        code: 1,
        message: "err".to_string(),
    };
    let mut buf = Vec::new();
    err.encode(&mut buf);
    // Layout: u32 object_id (0..4), u32 code (4..8),
    // u32 string length (8..12), content "err" (12..15),
    // one pad byte (15..16).
    assert_eq!(buf.len(), 16);
    assert_eq!(&buf[0..4], &[3, 0, 0, 0]);
    assert_eq!(&buf[4..8], &[1, 0, 0, 0]);
    assert_eq!(&buf[8..12], &[3, 0, 0, 0]);
    assert_eq!(&buf[12..15], b"err");
    assert_eq!(buf[15], 0);
}

#[test]
fn display_error_rejects_truncated_payload() {
    let err = DisplayError::decode(&[0u8; 7]).unwrap_err();
    assert!(matches!(err, DecodeError::Truncated { .. }));
}

// ---- DisplayDeleteId ---------------------------------------------

#[test]
fn display_delete_id_round_trips() {
    let original = DisplayDeleteId {
        id: ObjectId::new(99),
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    assert_eq!(buf.len(), 4);
    let decoded = DisplayDeleteId::decode(&buf).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn display_delete_id_rejects_short_payload() {
    assert!(DisplayDeleteId::decode(&[0u8; 3]).is_err());
}

// ---- RegistryGlobal ----------------------------------------------

#[test]
fn registry_global_round_trips_with_padded_string() {
    let original = RegistryGlobal {
        name: 7,
        interface: "pmd_compositor".to_string(),
        version: 2,
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    // Layout: u32 name (4) + u32 strlen (4) + 14 content
    // + 2 pad + u32 version (4). = 28 bytes total.
    assert_eq!(buf.len(), 28);
    let decoded = RegistryGlobal::decode(&buf).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn registry_global_round_trips_with_4_byte_aligned_string() {
    let original = RegistryGlobal {
        name: 1,
        interface: "pmd_shm".to_string(),
        version: 1,
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    // u32 name + u32 strlen + 7 content + 1 pad + u32 version
    // = 4 + 4 + 8 + 4 = 20.
    assert_eq!(buf.len(), 20);
    let decoded = RegistryGlobal::decode(&buf).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn registry_global_round_trips_with_empty_string() {
    let original = RegistryGlobal {
        name: 0,
        interface: String::new(),
        version: 0,
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    let decoded = RegistryGlobal::decode(&buf).unwrap();
    assert_eq!(decoded, original);
}

// ---- RegistryGlobalRemove ----------------------------------------

#[test]
fn registry_global_remove_round_trips() {
    let original = RegistryGlobalRemove { name: 5 };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    assert_eq!(buf, vec![5, 0, 0, 0]);
    let decoded = RegistryGlobalRemove::decode(&buf).unwrap();
    assert_eq!(decoded, original);
}

// ---- BufferRelease ------------------------------------------------

#[test]
fn buffer_release_has_zero_byte_payload() {
    let release = BufferRelease;
    let mut buf = Vec::new();
    release.encode(&mut buf);
    assert_eq!(buf.len(), 0);
    assert_eq!(BufferRelease::decode(&buf).unwrap(), release);
    // Decode from a leftover buffer also works (the
    // decoder just ignores payload bytes).
    assert_eq!(BufferRelease::decode(&[1, 2, 3]).unwrap(), release);
}

// ---- ShmFormat ----------------------------------------------------

#[test]
fn shm_format_encodes_u32_little_endian() {
    let fmt = ShmFormat {
        format: ShmFormat::FORMAT_ARGB8888,
    };
    let mut buf = Vec::new();
    fmt.encode(&mut buf);
    assert_eq!(buf, vec![0, 0, 0, 0]);

    let xrgb = ShmFormat {
        format: ShmFormat::FORMAT_XRGB8888,
    };
    buf.clear();
    xrgb.encode(&mut buf);
    assert_eq!(buf, vec![1, 0, 0, 0]);
}

#[test]
fn shm_format_round_trips_both_formats() {
    for v in [ShmFormat::FORMAT_ARGB8888, ShmFormat::FORMAT_XRGB8888] {
        let fmt = ShmFormat { format: v };
        let mut buf = Vec::new();
        fmt.encode(&mut buf);
        assert_eq!(ShmFormat::decode(&buf).unwrap(), fmt);
    }
}

// ---- write_string helper used by the server encoders -------------

#[test]
fn write_string_matches_read_string_for_round_trip() {
    use display_proto::decode::read_string;
    for s in [
        "",
        "a",
        "ab",
        "abc",
        "abcd",
        "hello world",
        "pmd_compositor",
    ] {
        let mut out = Vec::new();
        write_string(&mut out, s);
        let (got, consumed) = read_string(&out, 0).unwrap();
        assert_eq!(got, s);
        assert_eq!(consumed, out.len(), "consumed == encoded length for {s:?}");
    }
}

#[test]
fn write_string_padding_matches_four_byte_boundary() {
    // 0 bytes → 4 bytes (len only, no content, no pad)
    let mut out = Vec::new();
    write_string(&mut out, "");
    assert_eq!(out.len(), 4);

    // 1 byte → 4 len + 1 content + 3 pad = 8
    out.clear();
    write_string(&mut out, "a");
    assert_eq!(out.len(), 8);

    // 3 bytes → 4 len + 3 content + 1 pad = 8
    out.clear();
    write_string(&mut out, "abc");
    assert_eq!(out.len(), 8);

    // 4 bytes → 4 len + 4 content + 0 pad = 8
    out.clear();
    write_string(&mut out, "abcd");
    assert_eq!(out.len(), 8);

    // 5 bytes → 4 len + 5 content + 3 pad = 12
    out.clear();
    write_string(&mut out, "abcde");
    assert_eq!(out.len(), 12);
}

// ---- pointer / keyboard events ------------------------------------

#[test]
fn pointer_motion_round_trips() {
    let original = PointerMotion {
        surface_id: ObjectId::new(9),
        x: 42,
        y: -7,
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    let decoded = PointerMotion::decode(&buf).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn pointer_motion_payload_is_exactly_twelve_bytes() {
    let mut buf = Vec::new();
    let event = PointerMotion {
        surface_id: ObjectId::new(1),
        x: 0,
        y: 0,
    };
    event.encode(&mut buf);
    assert_eq!(buf.len(), 12);
}

#[test]
fn pointer_button_round_trips_with_press_state() {
    let original = PointerButton {
        serial: 0x1020_3040,
        surface_id: ObjectId::new(11),
        x: 100,
        y: 50,
        button: 1,
        state: pointer_button_state::PRESSED,
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    let decoded = PointerButton::decode(&buf).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(decoded.state, 1);
}

#[test]
fn pointer_button_round_trips_with_release_state() {
    let original = PointerButton {
        serial: 77,
        surface_id: ObjectId::new(11),
        x: 0,
        y: 0,
        button: 2,
        state: pointer_button_state::RELEASED,
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    let decoded = PointerButton::decode(&buf).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(decoded.state, 0);
}

#[test]
fn pointer_button_payload_is_exactly_twenty_four_bytes() {
    let mut buf = Vec::new();
    PointerButton {
        serial: 1,
        surface_id: ObjectId::new(1),
        x: 0,
        y: 0,
        button: 0,
        state: 0,
    }
    .encode(&mut buf);
    assert_eq!(buf.len(), 24);
}

#[test]
fn keyboard_key_round_trips() {
    let original = KeyboardKey {
        surface_id: ObjectId::new(13),
        key: 0x1e, /* 'a' scancode */
        state: key_state::PRESSED,
    };
    let mut buf = Vec::new();
    original.encode(&mut buf);
    let decoded = KeyboardKey::decode(&buf).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn keyboard_key_payload_is_exactly_twelve_bytes() {
    let mut buf = Vec::new();
    KeyboardKey {
        surface_id: ObjectId::new(1),
        key: 0,
        state: 0,
    }
    .encode(&mut buf);
    assert_eq!(buf.len(), 12);
}

#[test]
fn pointer_motion_decode_rejects_truncated_payload() {
    let err = PointerMotion::decode(&[0u8; 8]).unwrap_err();
    assert!(matches!(err, DecodeError::Truncated { .. }));
}

#[test]
fn pointer_button_decode_rejects_truncated_payload() {
    let err = PointerButton::decode(&[0u8; 16]).unwrap_err();
    assert!(matches!(err, DecodeError::Truncated { .. }));
}
