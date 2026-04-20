#![expect(
    clippy::single_char_lifetime_names,
    reason = "borrowed transport helpers use conventional single-letter lifetime names"
)]

use core::future::Future;

use crate::parser::compute_checksum;
use crate::types::{FrameError, FrameHeader, FrameView, Opcode, WireDirection};

/// Minimal transport-independent byte reader for asynchronous QUP parsers.
pub trait AsyncByteRead {
    /// The transport-specific read error.
    type Error;

    /// The future returned by [`AsyncByteRead::read_exact`].
    type ReadExactFuture<'a>: Future<Output = Result<(), Self::Error>>
    where
        Self: 'a;

    /// Fills the provided buffer completely or returns an underlying transport error.
    fn read_exact<'a>(&'a mut self, buf: &'a mut [u8]) -> Self::ReadExactFuture<'a>;
}

/// Minimal transport-independent byte writer for asynchronous QUP encoders or adapters.
pub trait AsyncByteWrite {
    /// The transport-specific write error.
    type Error;

    /// The future returned by [`AsyncByteWrite::write_all`].
    type WriteAllFuture<'a>: Future<Output = Result<(), Self::Error>>
    where
        Self: 'a;

    /// Writes the provided buffer completely or returns an underlying transport error.
    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> Self::WriteAllFuture<'a>;
}

/// Errors returned while reading a frame from an asynchronous transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsyncReadFrameError<E> {
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

impl<E> From<FrameError> for AsyncReadFrameError<E> {
    fn from(value: FrameError) -> Self {
        Self::Frame(value)
    }
}

/// Reads and validates a single frame from an asynchronous byte source.
pub async fn read_frame<'a, R>(
    reader: &mut R,
    direction: WireDirection,
    payload_buf: &'a mut [u8],
) -> Result<FrameView<'a>, AsyncReadFrameError<R::Error>>
where
    R: AsyncByteRead,
{
    let mut header = [0u8; 3];
    reader
        .read_exact(&mut header)
        .await
        .map_err(AsyncReadFrameError::Transport)?;

    let opcode = Opcode::new(header[0]);
    let length = u16::from_be_bytes([header[1], header[2]]);
    let required = usize::from(length);

    if payload_buf.len() < required {
        return Err(AsyncReadFrameError::BufferTooSmall {
            required,
            available: payload_buf.len(),
        });
    }

    let available = payload_buf.len();
    let Some(payload) = payload_buf.get_mut(..required) else {
        return Err(AsyncReadFrameError::BufferTooSmall {
            required,
            available,
        });
    };
    reader
        .read_exact(payload)
        .await
        .map_err(AsyncReadFrameError::Transport)?;

    let mut checksum = [0u8; 1];
    reader
        .read_exact(&mut checksum)
        .await
        .map_err(AsyncReadFrameError::Transport)?;

    let expected = compute_checksum(opcode, payload);
    if checksum[0] != expected {
        let mut sum = opcode.as_u8();
        sum = sum.wrapping_add(header[1]);
        sum = sum.wrapping_add(header[2]);
        for byte in payload.iter() {
            sum = sum.wrapping_add(*byte);
        }
        sum = sum.wrapping_add(checksum[0]);
        return Err(AsyncReadFrameError::Frame(FrameError::ChecksumMismatch {
            sum,
        }));
    }

    opcode
        .validate(direction)
        .map_err(AsyncReadFrameError::Frame)?;

    Ok(FrameView::new(
        FrameHeader::new(opcode, length, checksum[0]),
        payload,
    ))
}

#[cfg(test)]
mod tests {
    use core::future::{Ready, ready};

    use super::{AsyncByteRead, AsyncReadFrameError, read_frame};
    use crate::types::{FrameError, Opcode, WireDirection};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestError {
        UnexpectedEof,
    }

    struct AsyncSliceReader<'a> {
        bytes: &'a [u8],
        cursor: usize,
        max_chunk: usize,
    }

    impl<'a> AsyncSliceReader<'a> {
        fn new(bytes: &'a [u8], max_chunk: usize) -> Self {
            Self {
                bytes,
                cursor: 0,
                max_chunk,
            }
        }
    }

    impl AsyncByteRead for AsyncSliceReader<'_> {
        type Error = TestError;
        type ReadExactFuture<'a>
            = Ready<Result<(), Self::Error>>
        where
            Self: 'a;

        #[expect(
            clippy::arithmetic_side_effects,
            reason = "the test reader uses simple bounded cursor math to model fragmentation"
        )]
        #[expect(
            clippy::indexing_slicing,
            reason = "the test reader slices verified source and destination ranges"
        )]
        fn read_exact<'a>(&'a mut self, buf: &'a mut [u8]) -> Self::ReadExactFuture<'a> {
            let mut written = 0usize;
            while written < buf.len() {
                if self.cursor >= self.bytes.len() {
                    return ready(Err(TestError::UnexpectedEof));
                }

                let available = self.bytes.len() - self.cursor;
                let to_copy = available.min(self.max_chunk).min(buf.len() - written);
                buf[written..written + to_copy]
                    .copy_from_slice(&self.bytes[self.cursor..self.cursor + to_copy]);
                self.cursor += to_copy;
                written += to_copy;
            }

            ready(Ok(()))
        }
    }

    #[tokio::test]
    #[expect(
        clippy::unwrap_used,
        reason = "the unit test intentionally asserts the happy path for frame reads"
    )]
    async fn reads_frame_from_fragmented_transport() {
        let mut reader = AsyncSliceReader::new(&[0x53, 0x00, 0x02, 0x00, 0x01, 0xaa], 1);
        let mut payload = [0u8; 8];
        let frame = read_frame(&mut reader, WireDirection::ClientToNode, &mut payload)
            .await
            .unwrap();

        assert_eq!(frame.opcode(), Opcode::GETKEY);
        assert_eq!(frame.payload(), &[0x00, 0x01]);
    }

    #[tokio::test]
    async fn reports_small_buffer() {
        let mut reader = AsyncSliceReader::new(&[0x53, 0x00, 0x02, 0x00, 0x01, 0xaa], 6);
        let mut payload = [0u8; 1];

        assert!(matches!(
            read_frame(&mut reader, WireDirection::ClientToNode, &mut payload).await,
            Err(AsyncReadFrameError::BufferTooSmall {
                required: 2,
                available: 1,
            })
        ));
    }

    #[tokio::test]
    async fn reports_checksum_failure() {
        let mut reader = AsyncSliceReader::new(&[0x3f, 0x00, 0x00, 0x00], 4);
        let mut payload = [0u8; 0];

        assert!(matches!(
            read_frame(&mut reader, WireDirection::ClientToNode, &mut payload).await,
            Err(AsyncReadFrameError::Frame(
                FrameError::ChecksumMismatch { .. }
            ))
        ));
    }
}
