#![expect(
    clippy::single_char_lifetime_names,
    reason = "borrowed transport helpers use conventional single-letter lifetime names"
)]

use crate::parser::compute_checksum;
use crate::types::{FrameError, FrameHeader, FrameView, Opcode, WireDirection};

/// Minimal transport-independent byte reader for synchronous QUP parsers.
pub trait ByteRead {
    /// The transport-specific read error.
    type Error;

    /// Fills the provided buffer completely or returns an underlying transport error.
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Self::Error>;
}

/// Minimal transport-independent byte writer for synchronous QUP encoders or adapters.
pub trait ByteWrite {
    /// The transport-specific write error.
    type Error;

    /// Writes the provided buffer completely or returns an underlying transport error.
    fn write_all(&mut self, buf: &[u8]) -> Result<(), Self::Error>;
}

/// Errors returned while reading a frame from a synchronous transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadFrameError<E> {
    /// The transport returned an underlying read error.
    Transport(E),
    /// The caller-provided payload buffer is smaller than the declared frame payload.
    BufferTooSmall {
        /// The payload size declared in the frame header.
        required: usize,
        /// The size of the caller-provided payload buffer.
        available: usize,
    },
    /// The frame bytes were read successfully but violated the QUP framing rules.
    Frame(FrameError),
}

impl<E> From<FrameError> for ReadFrameError<E> {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

/// Reads and validates a single frame from a synchronous byte source.
pub fn read_frame<'a, R>(
    reader: &mut R,
    direction: WireDirection,
    payload_buf: &'a mut [u8],
) -> Result<FrameView<'a>, ReadFrameError<R::Error>>
where
    R: ByteRead,
{
    let mut header = [0u8; 3];
    reader
        .read_exact(&mut header)
        .map_err(ReadFrameError::Transport)?;

    let opcode = Opcode::new(header[0]);
    let length = u16::from_be_bytes([header[1], header[2]]);
    let required = usize::from(length);

    if payload_buf.len() < required {
        return Err(ReadFrameError::BufferTooSmall {
            required,
            available: payload_buf.len(),
        });
    }

    let available = payload_buf.len();
    let Some(payload) = payload_buf.get_mut(..required) else {
        return Err(ReadFrameError::BufferTooSmall {
            required,
            available,
        });
    };
    reader
        .read_exact(payload)
        .map_err(ReadFrameError::Transport)?;

    let mut checksum = [0u8; 1];
    reader
        .read_exact(&mut checksum)
        .map_err(ReadFrameError::Transport)?;

    let expected = compute_checksum(opcode, payload);
    if checksum[0] != expected {
        let mut sum = opcode.as_u8();
        sum = sum.wrapping_add(header[1]);
        sum = sum.wrapping_add(header[2]);
        for byte in payload.iter() {
            sum = sum.wrapping_add(*byte);
        }
        sum = sum.wrapping_add(checksum[0]);
        return Err(ReadFrameError::Frame(FrameError::ChecksumMismatch { sum }));
    }

    opcode.validate(direction).map_err(ReadFrameError::Frame)?;

    Ok(FrameView::new(
        FrameHeader::new(opcode, length, checksum[0]),
        payload,
    ))
}

#[cfg(test)]
mod tests {
    use super::{ByteRead, ReadFrameError, read_frame};
    use crate::types::{FrameError, Opcode, WireDirection};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestError {
        UnexpectedEof,
    }

    struct SliceReader<'a> {
        bytes: &'a [u8],
        cursor: usize,
        max_chunk: usize,
    }

    impl<'a> SliceReader<'a> {
        fn new(bytes: &'a [u8], max_chunk: usize) -> Self {
            Self {
                bytes,
                cursor: 0,
                max_chunk,
            }
        }
    }

    impl ByteRead for SliceReader<'_> {
        type Error = TestError;

        #[expect(
            clippy::arithmetic_side_effects,
            reason = "the test reader uses simple bounded cursor math to model fragmentation"
        )]
        #[expect(
            clippy::indexing_slicing,
            reason = "the test reader slices verified source and destination ranges"
        )]
        fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Self::Error> {
            let mut written = 0usize;
            while written < buf.len() {
                if self.cursor >= self.bytes.len() {
                    return Err(TestError::UnexpectedEof);
                }

                let available = self.bytes.len() - self.cursor;
                let to_copy = available.min(self.max_chunk).min(buf.len() - written);
                buf[written..written + to_copy]
                    .copy_from_slice(&self.bytes[self.cursor..self.cursor + to_copy]);
                self.cursor += to_copy;
                written += to_copy;
            }

            Ok(())
        }
    }

    #[test]
    #[expect(
        clippy::unwrap_used,
        reason = "the unit test intentionally asserts the happy path for frame reads"
    )]
    fn reads_frame_from_fragmented_transport() {
        let mut reader = SliceReader::new(&[0x53, 0x00, 0x02, 0x00, 0x01, 0xaa], 1);
        let mut payload = [0u8; 8];
        let frame = read_frame(&mut reader, WireDirection::ClientToNode, &mut payload).unwrap();

        assert_eq!(frame.opcode(), Opcode::GETKEY);
        assert_eq!(frame.payload(), &[0x00, 0x01]);
    }

    #[test]
    fn reports_small_buffer() {
        let mut reader = SliceReader::new(&[0x53, 0x00, 0x02, 0x00, 0x01, 0xaa], 6);
        let mut payload = [0u8; 1];

        assert!(matches!(
            read_frame(&mut reader, WireDirection::ClientToNode, &mut payload),
            Err(ReadFrameError::BufferTooSmall {
                required: 2,
                available: 1,
            })
        ));
    }

    #[test]
    fn reports_checksum_failure() {
        let mut reader = SliceReader::new(&[0x3f, 0x00, 0x00, 0x00], 4);
        let mut payload = [0u8; 0];

        assert!(matches!(
            read_frame(&mut reader, WireDirection::ClientToNode, &mut payload),
            Err(ReadFrameError::Frame(FrameError::ChecksumMismatch { .. }))
        ));
    }
}
