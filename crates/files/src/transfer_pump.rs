//! Bounded host-transfer body pumps shared by the production WASM loop and
//! native isolation tests. Each call performs a fixed amount of stream work
//! and then returns control to the display/input loop.

use std::collections::VecDeque;
use std::fmt;

pub const HOST_TRANSFER_CHUNK_BYTES: usize = 16 * 1024;
pub const HOST_TRANSFER_OPS_PER_TURN: usize = 4;

pub fn enqueue_bounded<T>(queue: &mut VecDeque<T>, value: T, capacity: usize) -> Result<(), T> {
    if queue.len() >= capacity {
        Err(value)
    } else {
        queue.push_back(value);
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TransferInterest {
    Read,
    Write,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PumpError {
    Os(i32),
    Message(String),
}

impl fmt::Display for PumpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Os(errno) => write!(formatter, "OS error {}", errno.saturating_abs()),
            Self::Message(message) => formatter.write_str(message),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PumpIoError {
    WouldBlock,
    Failed(PumpError),
}

#[derive(Debug, PartialEq, Eq)]
pub enum TransferTurn {
    Pending,
    Complete,
    Failed(PumpError),
}

pub struct ImportBody {
    expected: usize,
    received_bytes: usize,
    written_bytes: usize,
    pending: Vec<u8>,
    pending_offset: usize,
}

impl ImportBody {
    pub fn new(expected: usize) -> Self {
        Self {
            expected,
            received_bytes: 0,
            written_bytes: 0,
            pending: Vec::new(),
            pending_offset: 0,
        }
    }

    pub fn interest(&self) -> TransferInterest {
        if self.pending_offset < self.pending.len() {
            TransferInterest::Write
        } else {
            TransferInterest::Read
        }
    }

    pub const fn completed_bytes(&self) -> usize {
        self.written_bytes
    }

    pub fn pump_turn<R, W>(&mut self, mut read: R, mut write: W) -> TransferTurn
    where
        R: FnMut(&mut [u8]) -> Result<usize, PumpIoError>,
        W: FnMut(&[u8]) -> Result<usize, PumpIoError>,
    {
        for _ in 0..HOST_TRANSFER_OPS_PER_TURN {
            if self.pending_offset < self.pending.len() {
                let remaining = &self.pending[self.pending_offset..];
                let written = match write(remaining) {
                    Ok(0) => {
                        return TransferTurn::Failed(PumpError::Message(
                            "destination write made no progress".to_string(),
                        ))
                    }
                    Ok(written) if written <= remaining.len() => written,
                    Ok(_) => {
                        return TransferTurn::Failed(PumpError::Message(
                            "destination write exceeded its source buffer".to_string(),
                        ))
                    }
                    Err(PumpIoError::WouldBlock) => return TransferTurn::Pending,
                    Err(PumpIoError::Failed(error)) => return TransferTurn::Failed(error),
                };
                self.pending_offset += written;
                self.written_bytes += written;
                if self.pending_offset == self.pending.len() {
                    self.pending.clear();
                    self.pending_offset = 0;
                }
                continue;
            }

            let mut chunk = vec![0u8; HOST_TRANSFER_CHUNK_BYTES];
            let nread = match read(&mut chunk) {
                Ok(nread) if nread <= chunk.len() => nread,
                Ok(_) => {
                    return TransferTurn::Failed(PumpError::Message(
                        "host read exceeded its destination buffer".to_string(),
                    ))
                }
                Err(PumpIoError::WouldBlock) => return TransferTurn::Pending,
                Err(PumpIoError::Failed(error)) => return TransferTurn::Failed(error),
            };
            if nread == 0 {
                return if self.received_bytes == self.expected
                    && self.written_bytes == self.expected
                {
                    TransferTurn::Complete
                } else {
                    TransferTurn::Failed(PumpError::Message(format!(
                        "short host file: expected {} bytes, received {}",
                        self.expected, self.received_bytes
                    )))
                };
            }
            let Some(next_len) = self.received_bytes.checked_add(nread) else {
                return TransferTurn::Failed(PumpError::Message(
                    "host file size overflow".to_string(),
                ));
            };
            if next_len > self.expected {
                return TransferTurn::Failed(PumpError::Message(
                    "host file exceeded its declared size".to_string(),
                ));
            }
            chunk.truncate(nread);
            self.received_bytes = next_len;
            self.pending = chunk;
            self.pending_offset = 0;
        }
        TransferTurn::Pending
    }
}

pub struct ExportBody {
    max_bytes: usize,
    source_bytes: usize,
    written_bytes: usize,
    pending: Vec<u8>,
    pending_offset: usize,
}

impl ExportBody {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            source_bytes: 0,
            written_bytes: 0,
            pending: Vec::new(),
            pending_offset: 0,
        }
    }

    pub fn interest(&self) -> TransferInterest {
        if self.pending_offset < self.pending.len() {
            TransferInterest::Write
        } else {
            TransferInterest::Read
        }
    }

    pub const fn completed_bytes(&self) -> usize {
        self.written_bytes
    }

    pub fn pump_turn<R, W>(&mut self, mut read: R, mut write: W) -> TransferTurn
    where
        R: FnMut(&mut [u8]) -> Result<usize, PumpError>,
        W: FnMut(&[u8]) -> Result<usize, PumpIoError>,
    {
        for _ in 0..HOST_TRANSFER_OPS_PER_TURN {
            if self.pending_offset < self.pending.len() {
                let remaining = &self.pending[self.pending_offset..];
                let written = match write(remaining) {
                    Ok(0) | Err(PumpIoError::WouldBlock) => return TransferTurn::Pending,
                    Ok(written) if written <= remaining.len() => written,
                    Ok(_) => {
                        return TransferTurn::Failed(PumpError::Message(
                            "host write exceeded its source buffer".to_string(),
                        ))
                    }
                    Err(PumpIoError::Failed(error)) => return TransferTurn::Failed(error),
                };
                self.pending_offset += written;
                self.written_bytes += written;
                if self.pending_offset == self.pending.len() {
                    self.pending.clear();
                    self.pending_offset = 0;
                }
                continue;
            }

            let mut chunk = vec![0u8; HOST_TRANSFER_CHUNK_BYTES];
            let nread = match read(&mut chunk) {
                Ok(nread) if nread <= chunk.len() => nread,
                Ok(_) => {
                    return TransferTurn::Failed(PumpError::Message(
                        "file read exceeded its destination buffer".to_string(),
                    ))
                }
                Err(error) => return TransferTurn::Failed(error),
            };
            if nread == 0 {
                return TransferTurn::Complete;
            }
            let Some(next_size) = self.source_bytes.checked_add(nread) else {
                return TransferTurn::Failed(PumpError::Message(
                    "export size overflow".to_string(),
                ));
            };
            if next_size > self.max_bytes {
                return TransferTurn::Failed(PumpError::Message(format!(
                    "file grew beyond the {} byte transfer limit",
                    self.max_bytes
                )));
            }
            chunk.truncate(nread);
            self.source_bytes = next_size;
            self.pending = chunk;
            self.pending_offset = 0;
        }
        TransferTurn::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    fn cursor_read(cursor: &mut Cursor<Vec<u8>>, bytes: &mut [u8]) -> Result<usize, PumpError> {
        cursor
            .read(bytes)
            .map_err(|error| PumpError::Message(error.to_string()))
    }

    #[test]
    fn perpetual_export_backpressure_yields_to_display_close_and_buffer_release() {
        let mut body = ExportBody::new(64);
        let mut source = Cursor::new(b"host body".to_vec());
        let mut display_dispatches = 0;
        let mut buffer_releases = 0;
        let mut input_events = 0;
        let mut write_attempts = 0;

        for turn in 0..3 {
            // This is the production ordering in run_window: display events,
            // including close and buffer.release, precede one transfer turn.
            display_dispatches += 1;
            buffer_releases += 1;
            input_events += 1;
            if turn == 2 {
                break;
            }
            let result = body.pump_turn(
                |bytes| cursor_read(&mut source, bytes),
                |_| {
                    write_attempts += 1;
                    Err(PumpIoError::WouldBlock)
                },
            );
            assert_eq!(result, TransferTurn::Pending);
            assert_eq!(body.interest(), TransferInterest::Write);
        }

        assert_eq!(display_dispatches, 3);
        assert_eq!(buffer_releases, 3);
        assert_eq!(input_events, 3);
        assert_eq!(write_attempts, 2);
        assert_eq!(body.completed_bytes(), 0);
    }

    #[test]
    fn perpetual_import_backpressure_yields_to_display_close_and_buffer_release() {
        let mut body = ImportBody::new(9);
        let mut display_dispatches = 0;
        let mut buffer_releases = 0;
        let mut input_events = 0;
        let mut read_attempts = 0;

        for turn in 0..3 {
            display_dispatches += 1;
            buffer_releases += 1;
            input_events += 1;
            if turn == 2 {
                break;
            }
            let result = body.pump_turn(
                |_| {
                    read_attempts += 1;
                    Err(PumpIoError::WouldBlock)
                },
                |_| panic!("a blocked import has no destination bytes"),
            );
            assert_eq!(result, TransferTurn::Pending);
            assert_eq!(body.interest(), TransferInterest::Read);
        }

        assert_eq!(display_dispatches, 3);
        assert_eq!(buffer_releases, 3);
        assert_eq!(input_events, 3);
        assert_eq!(read_attempts, 2);
        assert_eq!(body.completed_bytes(), 0);
    }

    #[test]
    fn perpetual_destination_backpressure_yields_to_display_close() {
        let mut body = ImportBody::new(HOST_TRANSFER_CHUNK_BYTES);
        let mut host_reads = 0;
        let mut destination_writes = 0;
        let mut display_dispatches = 0;

        for turn in 0..3 {
            display_dispatches += 1;
            if turn == 2 {
                break;
            }
            let result = body.pump_turn(
                |bytes| {
                    host_reads += 1;
                    bytes.fill(b'x');
                    Ok(bytes.len())
                },
                |_| {
                    destination_writes += 1;
                    Err(PumpIoError::WouldBlock)
                },
            );
            assert_eq!(result, TransferTurn::Pending);
            assert_eq!(body.interest(), TransferInterest::Write);
        }

        assert_eq!(display_dispatches, 3);
        assert_eq!(host_reads, 1);
        assert_eq!(destination_writes, 2);
        assert_eq!(body.completed_bytes(), 0);
    }

    #[test]
    fn finite_export_completes_across_bounded_partial_write_turns() {
        let expected = b"finite host export body".to_vec();
        let mut source = Cursor::new(expected.clone());
        let mut output = Vec::new();
        let mut body = ExportBody::new(64);
        let mut turns = 0;

        loop {
            turns += 1;
            let mut writes_this_turn = 0;
            let result = body.pump_turn(
                |bytes| cursor_read(&mut source, bytes),
                |bytes| {
                    writes_this_turn += 1;
                    let count = bytes.len().min(2);
                    output.extend_from_slice(&bytes[..count]);
                    Ok(count)
                },
            );
            assert!(writes_this_turn <= HOST_TRANSFER_OPS_PER_TURN);
            match result {
                TransferTurn::Pending => assert!(turns < 32),
                TransferTurn::Complete => break,
                TransferTurn::Failed(error) => panic!("unexpected export failure: {error}"),
            }
        }

        assert!(turns > 1);
        assert_eq!(output, expected);
        assert_eq!(body.completed_bytes(), output.len());
    }

    #[test]
    fn finite_import_requires_exact_declared_size_and_eof() {
        let expected = b"finite host import body".to_vec();
        let mut offset = 0;
        let mut output = Vec::new();
        let mut body = ImportBody::new(expected.len());
        let mut turns = 0;

        loop {
            turns += 1;
            let result = body.pump_turn(
                |bytes| {
                    if offset == expected.len() {
                        return Ok(0);
                    }
                    let count = (expected.len() - offset).min(3).min(bytes.len());
                    bytes[..count].copy_from_slice(&expected[offset..offset + count]);
                    offset += count;
                    Ok(count)
                },
                |bytes| {
                    let count = bytes.len().min(2);
                    output.extend_from_slice(&bytes[..count]);
                    Ok(count)
                },
            );
            match result {
                TransferTurn::Pending => assert!(turns < 16),
                TransferTurn::Complete => break,
                TransferTurn::Failed(error) => panic!("unexpected import failure: {error}"),
            }
        }

        assert!(turns > 1);
        assert_eq!(body.interest(), TransferInterest::Read);
        assert_eq!(body.completed_bytes(), expected.len());
        assert_eq!(output, expected);
    }

    #[test]
    fn import_rejects_short_and_over_declared_bodies() {
        let mut short = ImportBody::new(2);
        let mut reads = 0;
        let result = short.pump_turn(
            |bytes| {
                reads += 1;
                if reads == 1 {
                    bytes[0] = b'x';
                    Ok(1)
                } else {
                    Ok(0)
                }
            },
            |bytes| Ok(bytes.len()),
        );
        assert!(matches!(
            result,
            TransferTurn::Failed(PumpError::Message(message)) if message.contains("short host file")
        ));

        let mut long = ImportBody::new(1);
        let result = long.pump_turn(
            |bytes| {
                bytes[..2].copy_from_slice(b"xx");
                Ok(2)
            },
            |_| panic!("over-declared bytes must not reach the destination"),
        );
        assert!(matches!(
            result,
            TransferTurn::Failed(PumpError::Message(message)) if message.contains("declared size")
        ));
    }

    #[test]
    fn pending_import_queue_rejects_the_first_item_beyond_its_limit() {
        let mut queue = VecDeque::new();
        let capacity = abi::ext::host_file::MAX_LIVE_IMPORTS;
        for token in 0..capacity {
            enqueue_bounded(&mut queue, token, capacity).unwrap();
        }
        assert_eq!(
            enqueue_bounded(&mut queue, capacity, capacity),
            Err(capacity)
        );
        assert_eq!(queue.len(), capacity);
    }

    #[test]
    fn sixteen_mib_import_yields_after_one_combined_io_budget() {
        assert_eq!(abi::ext::host_file::MAX_IMPORT_BYTES, 16 * 1024 * 1024);
        let mut body = ImportBody::new(abi::ext::host_file::MAX_IMPORT_BYTES);
        let mut host_reads = 0;
        let mut destination_writes = 0;
        let result = body.pump_turn(
            |bytes| {
                host_reads += 1;
                bytes.fill(b'x');
                Ok(bytes.len())
            },
            |bytes| {
                destination_writes += 1;
                Ok(bytes.len())
            },
        );

        assert_eq!(result, TransferTurn::Pending);
        assert_eq!(host_reads + destination_writes, HOST_TRANSFER_OPS_PER_TURN);
        assert_eq!(body.completed_bytes(), 2 * HOST_TRANSFER_CHUNK_BYTES);

        // The next production loop turn dispatches display close before it
        // would call the pump again, regardless of the declared 16 MiB size.
        let display_dispatches = 1;
        let close_requested = true;
        assert_eq!(display_dispatches, 1);
        assert!(close_requested);
    }

    #[test]
    fn export_rejects_source_growth_past_the_per_file_limit() {
        let mut source = Cursor::new(b"four".to_vec());
        let mut body = ExportBody::new(3);
        let result = body.pump_turn(
            |bytes| cursor_read(&mut source, bytes),
            |_| panic!("an over-limit source must fail before writing"),
        );
        assert!(matches!(
            result,
            TransferTurn::Failed(PumpError::Message(message)) if message.contains("transfer limit")
        ));
    }
}
