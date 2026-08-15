//! Typed request decoder tests.
//!
//! Each test builds the exact wire payload a spec-conformant
//! client would send for one request, then asserts that the
//! typed struct comes out with the expected field values.

use display_proto::decode::DecodeError;
use display_proto::requests::{
    buffer_format, CompositorCreateSurface, DisplayGetRegistry, RegistryBind, SeatGetKeyboard,
    SeatGetPointer, ShmCreatePool, ShmPoolCreateBuffer, ShmPoolResize, ShmPoolWriteRows,
    SurfaceAttach, SurfaceDamage, SurfaceFrame, SurfacePatchCurrent, XdgShellGetToplevel,
    XdgToplevelSetAppId, XdgToplevelSetTitle,
};
use display_proto::ObjectId;

fn wire_string(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let pad = (4 - (bytes.len() % 4)) % 4;
    let mut out = Vec::with_capacity(4 + bytes.len() + pad);
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    out.extend(core::iter::repeat_n(0u8, pad));
    out
}

#[test]
fn display_get_registry_decodes_a_single_new_id() {
    let payload = 5u32.to_le_bytes();
    let req = DisplayGetRegistry::decode(&payload).unwrap();
    assert_eq!(req.new_id, ObjectId::new(5));
}

#[test]
fn display_get_registry_rejects_short_payload() {
    let err = DisplayGetRegistry::decode(&[0u8; 3]).unwrap_err();
    assert!(matches!(err, DecodeError::Truncated { .. }));
}

#[test]
fn registry_bind_round_trips_name_string_version_new_id() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&7u32.to_le_bytes()); // name
    payload.extend(wire_string("pmd_compositor")); // interface (14 + 2 pad)
    payload.extend_from_slice(&2u32.to_le_bytes()); // version
    payload.extend_from_slice(&11u32.to_le_bytes()); // new_id

    let req = RegistryBind::decode(&payload).unwrap();
    assert_eq!(req.name, 7);
    assert_eq!(req.interface, "pmd_compositor");
    assert_eq!(req.version, 2);
    assert_eq!(req.new_id, ObjectId::new(11));
}

#[test]
fn registry_bind_handles_four_byte_aligned_string_with_no_padding() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes()); // name
    payload.extend(wire_string("pmd_shm")); // 7 bytes + 1 pad
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.extend_from_slice(&5u32.to_le_bytes());
    let req = RegistryBind::decode(&payload).unwrap();
    assert_eq!(req.interface, "pmd_shm");
    assert_eq!(req.new_id, ObjectId::new(5));
}

#[test]
fn registry_bind_rejects_payload_truncated_before_version() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.extend(wire_string("pmd_compositor"));
    // version and new_id omitted
    let err = RegistryBind::decode(&payload).unwrap_err();
    assert!(matches!(err, DecodeError::Truncated { .. }));
}

#[test]
fn compositor_create_surface_decodes_a_single_new_id() {
    let payload = 13u32.to_le_bytes();
    let req = CompositorCreateSurface::decode(&payload).unwrap();
    assert_eq!(req.new_id, ObjectId::new(13));
}

#[test]
fn surface_attach_decodes_u32_i32_i32() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&9u32.to_le_bytes());
    payload.extend_from_slice(&(-5i32).to_le_bytes());
    payload.extend_from_slice(&12i32.to_le_bytes());
    let req = SurfaceAttach::decode(&payload).unwrap();
    assert_eq!(req.buffer_id, ObjectId::new(9));
    assert_eq!(req.x, -5);
    assert_eq!(req.y, 12);
}

#[test]
fn surface_damage_decodes_four_i32s() {
    let mut payload = Vec::new();
    for v in [1i32, 2, 100, 200] {
        payload.extend_from_slice(&v.to_le_bytes());
    }
    let req = SurfaceDamage::decode(&payload).unwrap();
    assert_eq!(req.x, 1);
    assert_eq!(req.y, 2);
    assert_eq!(req.width, 100);
    assert_eq!(req.height, 200);
}

#[test]
fn surface_damage_rejects_truncated_payload() {
    let err = SurfaceDamage::decode(&[0u8; 12]).unwrap_err();
    assert!(matches!(err, DecodeError::Truncated { .. }));
}

#[test]
fn surface_frame_decodes_callback_new_id() {
    let request = SurfaceFrame::decode(&17u32.to_le_bytes()).unwrap();
    assert_eq!(request.new_id, ObjectId::new(17));
}

#[test]
fn surface_frame_rejects_short_payload() {
    let error = SurfaceFrame::decode(&[0u8; 3]).unwrap_err();
    assert_eq!(
        error,
        DecodeError::PayloadLengthMismatch {
            expected: 4,
            actual: 3,
        }
    );
}

#[test]
fn surface_frame_rejects_trailing_payload() {
    let error = SurfaceFrame::decode(&[0u8; 5]).unwrap_err();
    assert_eq!(
        error,
        DecodeError::PayloadLengthMismatch {
            expected: 4,
            actual: 5,
        }
    );
}

#[test]
fn surface_patch_current_decodes_exact_packed_pixels() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&2i32.to_le_bytes());
    payload.extend_from_slice(&3i32.to_le_bytes());
    payload.extend_from_slice(&2u32.to_le_bytes());
    payload.extend_from_slice(&1u32.to_le_bytes());
    payload.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);

    let request = SurfacePatchCurrent::decode(&payload).unwrap();
    assert_eq!(request.x, 2);
    assert_eq!(request.y, 3);
    assert_eq!(request.width, 2);
    assert_eq!(request.height, 1);
    assert_eq!(request.pixels, [1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn surface_patch_current_rejects_truncated_mismatched_and_overflowing_lengths() {
    assert!(matches!(
        SurfacePatchCurrent::decode(&[0; 15]),
        Err(DecodeError::Truncated { .. })
    ));

    let mut mismatched = Vec::new();
    mismatched.extend_from_slice(&0i32.to_le_bytes());
    mismatched.extend_from_slice(&0i32.to_le_bytes());
    mismatched.extend_from_slice(&2u32.to_le_bytes());
    mismatched.extend_from_slice(&1u32.to_le_bytes());
    mismatched.extend_from_slice(&[0; 7]);
    assert_eq!(
        SurfacePatchCurrent::decode(&mismatched).unwrap_err(),
        DecodeError::PayloadLengthMismatch {
            expected: 8,
            actual: 7,
        }
    );

    let mut overflowing = Vec::new();
    overflowing.extend_from_slice(&0i32.to_le_bytes());
    overflowing.extend_from_slice(&0i32.to_le_bytes());
    overflowing.extend_from_slice(&u32::MAX.to_le_bytes());
    overflowing.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        SurfacePatchCurrent::decode(&overflowing).unwrap_err(),
        DecodeError::PayloadLengthOverflow
    );
}

#[test]
fn shm_create_pool_decodes_new_id_and_size() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&17u32.to_le_bytes()); // new_id
    payload.extend_from_slice(&(64 * 1024u32).to_le_bytes()); // size
    let req = ShmCreatePool::decode(&payload).unwrap();
    assert_eq!(req.new_id, ObjectId::new(17));
    assert_eq!(req.size, 64 * 1024);
}

#[test]
fn shm_create_pool_rejects_truncated_payload() {
    let err = ShmCreatePool::decode(&[0u8; 5]).unwrap_err();
    assert!(matches!(err, DecodeError::Truncated { .. }));
}

#[test]
fn shm_pool_create_buffer_decodes_all_six_fields() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&23u32.to_le_bytes()); // new_id
    payload.extend_from_slice(&0u32.to_le_bytes()); // offset
    payload.extend_from_slice(&320u32.to_le_bytes()); // width
    payload.extend_from_slice(&240u32.to_le_bytes()); // height
    payload.extend_from_slice(&(320u32 * 4).to_le_bytes()); // stride
    payload.extend_from_slice(&buffer_format::ARGB8888.to_le_bytes()); // format
    let req = ShmPoolCreateBuffer::decode(&payload).unwrap();
    assert_eq!(req.new_id, ObjectId::new(23));
    assert_eq!(req.offset, 0);
    assert_eq!(req.width, 320);
    assert_eq!(req.height, 240);
    assert_eq!(req.stride, 320 * 4);
    assert_eq!(req.format, buffer_format::ARGB8888);
}

#[test]
fn shm_pool_create_buffer_round_trips_xrgb8888_format() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&5u32.to_le_bytes());
    for v in [0u32, 16, 16, 64, 2] {
        payload.extend_from_slice(&v.to_le_bytes());
    }
    let req = ShmPoolCreateBuffer::decode(&payload).unwrap();
    assert_eq!(req.format, 2);
    assert_ne!(req.format, buffer_format::ARGB8888);
    assert_ne!(req.format, buffer_format::XRGB8888);
}

#[test]
fn shm_pool_create_buffer_rejects_truncated_payload() {
    let err = ShmPoolCreateBuffer::decode(&[0u8; 16]).unwrap_err();
    assert!(matches!(err, DecodeError::Truncated { .. }));
}

#[test]
fn shm_pool_resize_decodes_new_size() {
    let request = ShmPoolResize::decode(&1234u32.to_le_bytes()).unwrap();
    assert_eq!(request.new_size, 1234);
    assert!(matches!(
        ShmPoolResize::decode(&[0u8; 3]).unwrap_err(),
        DecodeError::Truncated { .. }
    ));
}

#[test]
fn shm_pool_write_rows_requires_exact_packed_payload() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&32u32.to_le_bytes());
    payload.extend_from_slice(&3u32.to_le_bytes());
    payload.extend_from_slice(&2u32.to_le_bytes());
    payload.extend_from_slice(&8u32.to_le_bytes());
    payload.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    let request = ShmPoolWriteRows::decode(&payload).unwrap();
    assert_eq!(request.offset, 32);
    assert_eq!(request.row_bytes, 3);
    assert_eq!(request.rows, 2);
    assert_eq!(request.stride, 8);
    assert_eq!(request.bytes, [1, 2, 3, 4, 5, 6]);

    payload.pop();
    assert_eq!(
        ShmPoolWriteRows::decode(&payload).unwrap_err(),
        DecodeError::PayloadLengthMismatch {
            expected: 6,
            actual: 5,
        }
    );
}

#[test]
fn xdg_shell_get_toplevel_decodes_new_id_and_surface_id() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&19u32.to_le_bytes()); // new_id
    payload.extend_from_slice(&7u32.to_le_bytes()); // surface_id
    let req = XdgShellGetToplevel::decode(&payload).unwrap();
    assert_eq!(req.new_id, ObjectId::new(19));
    assert_eq!(req.surface_id, ObjectId::new(7));
}

#[test]
fn xdg_shell_get_toplevel_rejects_truncated_payload() {
    let err = XdgShellGetToplevel::decode(&[0u8; 5]).unwrap_err();
    assert!(matches!(err, DecodeError::Truncated { .. }));
}

#[test]
fn xdg_toplevel_set_title_round_trips_a_string() {
    // Wire layout: u32 length + bytes + 4-byte padding.
    let mut payload = Vec::new();
    payload.extend(wire_string("pmos.term"));
    let req = XdgToplevelSetTitle::decode(&payload).unwrap();
    assert_eq!(req.title, "pmos.term");
}

#[test]
fn xdg_toplevel_string_lengths_are_available_before_utf8_allocation() {
    let mut payload = Vec::new();
    payload.extend_from_slice(&5u32.to_le_bytes());
    payload.extend_from_slice(&[0xff; 5]);
    payload.extend_from_slice(&[0; 3]);

    assert_eq!(XdgToplevelSetTitle::title_byte_len(&payload), Ok(5));
    assert_eq!(XdgToplevelSetAppId::app_id_byte_len(&payload), Ok(5));
    assert!(matches!(
        XdgToplevelSetTitle::decode(&payload),
        Err(DecodeError::InvalidUtf8 { .. })
    ));
}

#[test]
fn xdg_toplevel_set_title_handles_empty_string() {
    let mut payload = Vec::new();
    payload.extend(wire_string(""));
    let req = XdgToplevelSetTitle::decode(&payload).unwrap();
    assert_eq!(req.title, "");
}

#[test]
fn xdg_toplevel_set_app_id_round_trips_a_string() {
    let mut payload = Vec::new();
    payload.extend(wire_string("pmos.files"));
    let req = XdgToplevelSetAppId::decode(&payload).unwrap();
    assert_eq!(req.app_id, "pmos.files");
}

#[test]
fn xdg_toplevel_set_title_rejects_truncated_payload() {
    let err = XdgToplevelSetTitle::decode(&[0u8; 2]).unwrap_err();
    assert!(matches!(err, DecodeError::Truncated { .. }));
}

#[test]
fn seat_get_pointer_decodes_a_single_new_id() {
    let payload = 17u32.to_le_bytes();
    let req = SeatGetPointer::decode(&payload).unwrap();
    assert_eq!(req.new_id, ObjectId::new(17));
}

#[test]
fn seat_get_keyboard_decodes_a_single_new_id() {
    let payload = 21u32.to_le_bytes();
    let req = SeatGetKeyboard::decode(&payload).unwrap();
    assert_eq!(req.new_id, ObjectId::new(21));
}

#[test]
fn seat_get_pointer_rejects_truncated_payload() {
    let err = SeatGetPointer::decode(&[0u8; 3]).unwrap_err();
    assert!(matches!(err, DecodeError::Truncated { .. }));
}
