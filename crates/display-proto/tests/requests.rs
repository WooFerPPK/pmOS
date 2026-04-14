//! Typed request decoder tests.
//!
//! Each test builds the exact wire payload a spec-conformant
//! client would send for one request, then asserts that the
//! typed struct comes out with the expected field values.

use display_proto::decode::DecodeError;
use display_proto::requests::{
    buffer_format, CompositorCreateSurface, DisplayGetRegistry, RegistryBind,
    ShmCreatePool, ShmPoolCreateBuffer, SurfaceAttach, SurfaceDamage,
};
use display_proto::ObjectId;

fn wire_string(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let pad = (4 - (bytes.len() % 4)) % 4;
    let mut out = Vec::with_capacity(4 + bytes.len() + pad);
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    out.extend(core::iter::repeat(0u8).take(pad));
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
