use std::string::FromUtf8Error;

use abi::ext::host_file;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFileNotification {
    pub token: u32,
    pub size: u64,
    pub name: String,
    pub mime: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum NotificationError {
    InvalidLength,
    InvalidMagic,
    UnsupportedVersion(u16),
    InvalidUtf8,
}

impl From<FromUtf8Error> for NotificationError {
    fn from(_: FromUtf8Error) -> Self {
        Self::InvalidUtf8
    }
}

/// Incremental decoder for the kernel-owned `/run/host-files` byte stream.
/// Socket reads may split one frame or coalesce several, so callers retain one
/// decoder for the lifetime of their subscription.
#[derive(Debug, Default)]
pub struct HostNotificationDecoder {
    pending: Vec<u8>,
}

impl HostNotificationDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<HostFileNotification>, NotificationError> {
        self.pending.extend_from_slice(bytes);
        let mut decoded = Vec::new();
        loop {
            if self.pending.len() < 4 {
                break;
            }
            let frame_len = u32::from_le_bytes(self.pending[0..4].try_into().unwrap()) as usize;
            let max_len = host_file::NOTIFICATION_HEADER_LEN
                + host_file::MAX_NAME_BYTES
                + host_file::MAX_MIME_BYTES;
            if !(host_file::NOTIFICATION_HEADER_LEN..=max_len).contains(&frame_len) {
                self.pending.clear();
                return Err(NotificationError::InvalidLength);
            }
            if self.pending.len() < frame_len {
                break;
            }
            let frame = self.pending.drain(..frame_len).collect::<Vec<_>>();
            decoded.push(decode_frame(&frame)?);
        }
        Ok(decoded)
    }
}

fn decode_frame(frame: &[u8]) -> Result<HostFileNotification, NotificationError> {
    if frame.len() < host_file::NOTIFICATION_HEADER_LEN {
        return Err(NotificationError::InvalidLength);
    }
    let magic = u32::from_le_bytes(frame[4..8].try_into().unwrap());
    if magic != host_file::NOTIFICATION_MAGIC {
        return Err(NotificationError::InvalidMagic);
    }
    let version = u16::from_le_bytes(frame[8..10].try_into().unwrap());
    if version != host_file::NOTIFICATION_VERSION {
        return Err(NotificationError::UnsupportedVersion(version));
    }
    let flags = u16::from_le_bytes(frame[10..12].try_into().unwrap());
    if flags != 0 {
        return Err(NotificationError::InvalidLength);
    }
    let token = u32::from_le_bytes(frame[12..16].try_into().unwrap());
    let size = u64::from_le_bytes(frame[16..24].try_into().unwrap());
    let name_len = u16::from_le_bytes(frame[24..26].try_into().unwrap()) as usize;
    let mime_len = u16::from_le_bytes(frame[26..28].try_into().unwrap()) as usize;
    if name_len > host_file::MAX_NAME_BYTES || mime_len > host_file::MAX_MIME_BYTES {
        return Err(NotificationError::InvalidLength);
    }
    let expected = host_file::NOTIFICATION_HEADER_LEN + name_len + mime_len;
    if frame.len() != expected {
        return Err(NotificationError::InvalidLength);
    }
    let name_start = host_file::NOTIFICATION_HEADER_LEN;
    let mime_start = name_start + name_len;
    let name = String::from_utf8(frame[name_start..mime_start].to_vec())?;
    let mime = String::from_utf8(frame[mime_start..expected].to_vec())?;
    Ok(HostFileNotification {
        token,
        size,
        name,
        mime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(token: u32, size: u64, name: &str, mime: &str) -> Vec<u8> {
        let len = host_file::NOTIFICATION_HEADER_LEN + name.len() + mime.len();
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&host_file::NOTIFICATION_MAGIC.to_le_bytes());
        out.extend_from_slice(&host_file::NOTIFICATION_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&token.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(mime.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(mime.as_bytes());
        out
    }

    #[test]
    fn fragmented_frame_decodes_only_after_completion() {
        let bytes = frame(7, 3, "a.txt", "text/plain");
        let mut decoder = HostNotificationDecoder::default();
        assert!(decoder.push(&bytes[..9]).unwrap().is_empty());
        assert!(decoder.push(&bytes[9..27]).unwrap().is_empty());
        assert_eq!(
            decoder.push(&bytes[27..]).unwrap(),
            vec![HostFileNotification {
                token: 7,
                size: 3,
                name: "a.txt".to_string(),
                mime: "text/plain".to_string(),
            }]
        );
    }

    #[test]
    fn coalesced_frames_decode_in_order() {
        let mut bytes = frame(1, 4, "one", "x/a");
        bytes.extend(frame(2, 8, "two", "x/b"));
        let mut decoder = HostNotificationDecoder::default();
        let decoded = decoder.push(&bytes).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].token, 1);
        assert_eq!(decoded[1].token, 2);
    }

    #[test]
    fn malformed_length_is_rejected_and_buffer_cleared() {
        let mut decoder = HostNotificationDecoder::default();
        assert_eq!(
            decoder.push(&1u32.to_le_bytes()),
            Err(NotificationError::InvalidLength)
        );
        assert_eq!(decoder.push(&frame(3, 0, "ok", "")).unwrap().len(), 1);
    }

    #[test]
    fn unknown_version_and_invalid_utf8_are_rejected() {
        let mut versioned = frame(1, 0, "a", "b");
        versioned[8..10].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(
            HostNotificationDecoder::default().push(&versioned),
            Err(NotificationError::UnsupportedVersion(2))
        );

        let mut bad_utf8 = frame(1, 0, "a", "b");
        bad_utf8[28] = 0xff;
        assert_eq!(
            HostNotificationDecoder::default().push(&bad_utf8),
            Err(NotificationError::InvalidUtf8)
        );
    }
}
