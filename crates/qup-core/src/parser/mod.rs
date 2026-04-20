//! Parser entry points and transport traits.
//!
//! > "A node MUST NOT service a request until the entire request frame has been
//! > received and validated."
//!
//! > "A receiver validates a frame in this order:"
//! > 1. "Read `opcode`."
//! > 2. "Read `length`."
//! > 3. "Read exactly `length` payload bytes."
//! > 4. "Read the checksum byte."
//! > 5. "Verify the wrapping sum over the full frame."
//! > 6. "Dispatch the frame by opcode."

#![expect(
    clippy::single_char_lifetime_names,
    reason = "parser entry points use conventional single-letter lifetime names for borrowed buffers"
)]

mod checksum;
mod frame;

pub use checksum::{compute_checksum, frame_sum};

use crate::types::{FrameView, WireDirection};

/// Stateless parser for validated QUP frames.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Parser;

impl Parser {
    /// Creates a new stateless parser value.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// "A receiver validates a frame in this order:"
    /// 1. "Read `opcode`."
    /// 2. "Read `length`."
    /// 3. "Read exactly `length` payload bytes."
    /// 4. "Read the checksum byte."
    /// 5. "Verify the wrapping sum over the full frame."
    /// 6. "Dispatch the frame by opcode."
    pub fn parse_frame(
        self,
        direction: WireDirection,
        frame: &[u8],
    ) -> Result<FrameView<'_>, crate::types::FrameError> {
        frame::parse_frame(direction, frame)
    }

    /// Reads a single frame from an external byte stream into caller-owned storage.
    pub fn read_frame<'a, R>(
        self,
        reader: &mut R,
        direction: WireDirection,
        payload_buf: &'a mut [u8],
    ) -> Result<FrameView<'a>, crate::io::ReadFrameError<R::Error>>
    where
        R: crate::io::ByteRead,
    {
        crate::io::sync::read_frame(reader, direction, payload_buf)
    }

    /// Reads a single frame from an asynchronous byte stream into caller-owned storage.
    pub async fn read_frame_async<'a, R>(
        self,
        reader: &mut R,
        direction: WireDirection,
        payload_buf: &'a mut [u8],
    ) -> Result<FrameView<'a>, crate::io::AsyncReadFrameError<R::Error>>
    where
        R: crate::io::AsyncByteRead,
    {
        crate::io::asynch::read_frame(reader, direction, payload_buf).await
    }
}
