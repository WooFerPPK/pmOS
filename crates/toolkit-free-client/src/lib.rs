//! Toolkit-free client library — Principle VII conformance
//! fixture.
//!
//! The ONLY thing this crate is allowed to depend on, among
//! the display system, is `display-proto` — the shared wire
//! format. It deliberately does NOT depend on the `toolkit`
//! crate; the whole point of this fixture is to prove that
//! any client that can read `display-proto` can drive the
//! display server without a toolkit in the loop.
//!
//! The code below is intentionally written in the most direct
//! possible style — imperative helpers, no shared state
//! machine, no trait abstractions. A reader who has just
//! read `specs/001-browser-os-v1/contracts/display-protocol.md`
//! should be able to see exactly how each wire message is
//! built from the spec tables without cross-referencing
//! anything else.
//!
//! Principle VII says "Protocol over API for the display
//! server — Wayland-inspired wire protocol, toolkit is a
//! library wrapper". The `tests/conformance.rs` integration
//! test pairs a [`FreeClient`] against a
//! `display_server::Server` over in-memory byte buffers and
//! walks the same display→registry→compositor→surface→commit
//! sequence the toolkit's `tests/loopback.rs` does. If the
//! two sides agree, the claim holds.

#![forbid(unsafe_code)]

use display_proto::ids::{IdAllocator, ObjectId};
use display_proto::wire::{MessageHeader, WireError, HEADER_SIZE};

/// `pmd_display.get_registry` opcode from the spec tables.
pub const OP_DISPLAY_GET_REGISTRY: u16 = 2;

/// `pmd_registry.bind` opcode.
pub const OP_REGISTRY_BIND: u16 = 1;

/// `pmd_compositor.create_surface` opcode.
pub const OP_COMPOSITOR_CREATE_SURFACE: u16 = 1;

/// `pmd_surface.commit` opcode.
pub const OP_SURFACE_COMMIT: u16 = 7;

/// `pmd_surface.attach` opcode.
pub const OP_SURFACE_ATTACH: u16 = 2;

/// `pmd_surface.damage` opcode.
pub const OP_SURFACE_DAMAGE: u16 = 3;

/// Errors the free client returns. A thin newtype over
/// [`WireError`] plus a few extra shapes the spec allows us
/// to produce without depending on `display_server`'s richer
/// `ClientError`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FreeClientError {
    /// Wire-format error forwarded from [`MessageHeader`].
    Wire(WireError),
    /// Client-side ID space exhausted.
    IdsExhausted,
}

impl From<WireError> for FreeClientError {
    fn from(e: WireError) -> Self {
        FreeClientError::Wire(e)
    }
}

/// A minimal hand-written client. Owns an odd-id allocator
/// and an outbound byte buffer; high-level helper methods
/// append framed messages to the buffer.
pub struct FreeClient {
    ids: IdAllocator,
    outbound: Vec<u8>,
}

impl Default for FreeClient {
    fn default() -> Self {
        FreeClient::new()
    }
}

impl FreeClient {
    pub fn new() -> Self {
        FreeClient {
            ids: IdAllocator::for_client(),
            outbound: Vec::new(),
        }
    }

    /// Allocate the next odd id in the client's partition.
    pub fn allocate_id(&mut self) -> Result<ObjectId, FreeClientError> {
        self.ids
            .allocate()
            .map_err(|_| FreeClientError::IdsExhausted)
    }

    /// How many bytes are queued for send.
    pub fn outbound_len(&self) -> usize {
        self.outbound.len()
    }

    /// Drain the outbound byte queue. The caller typically
    /// forwards these to a real transport OR feeds them to a
    /// paired server dispatcher in tests.
    pub fn drain_outbound(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outbound)
    }

    /// Append a framed request (header + payload) to the
    /// outbound queue. This is the low-level building block
    /// every helper below goes through — tests can use it
    /// directly to craft malformed requests for error-path
    /// coverage.
    pub fn encode_request(
        &mut self,
        object_id: ObjectId,
        opcode: u16,
        payload: &[u8],
    ) -> Result<(), FreeClientError> {
        let header = MessageHeader::try_new(object_id, opcode, payload.len(), 0)?;
        let start = self.outbound.len();
        self.outbound.resize(start + HEADER_SIZE, 0);
        header.encode(&mut self.outbound[start..start + HEADER_SIZE])?;
        self.outbound.extend_from_slice(payload);
        Ok(())
    }

    // ---- High-level helpers ------------------------------------
    //
    // Each helper reads like the spec's request table row: it
    // lists the opcode number, the argument layout, and emits
    // the framed bytes. No shared state: a reader can map each
    // method back to the spec in seconds.

    /// `pmd_display.get_registry(new_id)` — spec §3 row 2.
    /// Payload: a single 4-byte little-endian `new_id`.
    /// Returns the allocated registry id.
    pub fn get_registry(&mut self) -> Result<ObjectId, FreeClientError> {
        let registry_id = self.allocate_id()?;
        let payload = registry_id.raw().to_le_bytes();
        self.encode_request(
            ObjectId::DISPLAY,
            OP_DISPLAY_GET_REGISTRY,
            &payload,
        )?;
        Ok(registry_id)
    }

    /// `pmd_registry.bind(name, interface, version, new_id)` —
    /// spec §4 row 1. Payload: u32 name, length-prefixed
    /// string, u32 version, u32 new_id. Returns the allocated
    /// new_id.
    pub fn registry_bind(
        &mut self,
        registry_id: ObjectId,
        global_name: u32,
        interface_name: &str,
        version: u32,
    ) -> Result<ObjectId, FreeClientError> {
        let new_id = self.allocate_id()?;
        let mut payload = Vec::new();
        payload.extend_from_slice(&global_name.to_le_bytes());
        append_wire_string(&mut payload, interface_name);
        payload.extend_from_slice(&version.to_le_bytes());
        payload.extend_from_slice(&new_id.raw().to_le_bytes());
        self.encode_request(registry_id, OP_REGISTRY_BIND, &payload)?;
        Ok(new_id)
    }

    /// `pmd_compositor.create_surface(new_id)` — spec §5.
    pub fn compositor_create_surface(
        &mut self,
        compositor_id: ObjectId,
    ) -> Result<ObjectId, FreeClientError> {
        let new_id = self.allocate_id()?;
        self.encode_request(
            compositor_id,
            OP_COMPOSITOR_CREATE_SURFACE,
            &new_id.raw().to_le_bytes(),
        )?;
        Ok(new_id)
    }

    /// `pmd_surface.attach(buffer_id, x, y)` — spec §9 row 2.
    /// Payload: u32 buffer_id, i32 x, i32 y.
    pub fn surface_attach(
        &mut self,
        surface_id: ObjectId,
        buffer_id: ObjectId,
        x: i32,
        y: i32,
    ) -> Result<(), FreeClientError> {
        let mut payload = [0u8; 12];
        payload[0..4].copy_from_slice(&buffer_id.raw().to_le_bytes());
        payload[4..8].copy_from_slice(&x.to_le_bytes());
        payload[8..12].copy_from_slice(&y.to_le_bytes());
        self.encode_request(surface_id, OP_SURFACE_ATTACH, &payload)
    }

    /// `pmd_surface.damage(x, y, w, h)` — spec §9 row 3.
    pub fn surface_damage(
        &mut self,
        surface_id: ObjectId,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> Result<(), FreeClientError> {
        let mut payload = [0u8; 16];
        payload[0..4].copy_from_slice(&x.to_le_bytes());
        payload[4..8].copy_from_slice(&y.to_le_bytes());
        payload[8..12].copy_from_slice(&w.to_le_bytes());
        payload[12..16].copy_from_slice(&h.to_le_bytes());
        self.encode_request(surface_id, OP_SURFACE_DAMAGE, &payload)
    }

    /// `pmd_surface.commit()` — spec §9 row 7. No payload.
    pub fn surface_commit(
        &mut self,
        surface_id: ObjectId,
    ) -> Result<(), FreeClientError> {
        self.encode_request(surface_id, OP_SURFACE_COMMIT, &[])
    }

    /// Parse the next framed message from the front of
    /// `bytes`. Returns the header and the number of bytes
    /// consumed, or `None` if `bytes` doesn't contain a
    /// complete message yet. Wire-format errors surface as
    /// `Some(Err(...))`.
    ///
    /// A buffer shorter than the header, OR a buffer whose
    /// header is structurally valid but claims more bytes
    /// than are available, is "need more bytes" → `None`.
    /// Only structurally-invalid headers (length below
    /// `HEADER_SIZE`, reserved byte set) produce an error.
    pub fn try_decode_event(
        bytes: &[u8],
    ) -> Option<Result<(MessageHeader, usize), WireError>> {
        if bytes.len() < HEADER_SIZE {
            return None;
        }
        // Peek the length field (bytes 6..8) without running
        // the full decode: short buffers should read as
        // "need more bytes" (None), not as InvalidLength
        // errors.
        let claimed_length =
            u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
        if claimed_length < HEADER_SIZE {
            return Some(Err(WireError::InvalidLength));
        }
        if bytes.len() < claimed_length {
            return None;
        }
        match MessageHeader::decode(bytes) {
            Ok(header) => Some(Ok((header, header.length as usize))),
            Err(e) => Some(Err(e)),
        }
    }
}

/// Append a length-prefixed, 4-byte-aligned UTF-8 string to
/// `out` as the spec's §1 framing describes.
///
/// Layout: `u32 byte_length`, `byte_length` bytes of UTF-8,
/// then padding to the next 4-byte boundary with zeros.
pub fn append_wire_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len() as u32;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    let pad = (4 - (bytes.len() % 4)) % 4;
    for _ in 0..pad {
        out.push(0);
    }
}
