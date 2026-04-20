//! Borrowed frame header and body views.

#![expect(
    clippy::single_char_lifetime_names,
    reason = "frame views carry one borrowed payload lifetime throughout the module"
)]

use crate::types::Opcode;

/// The number of framing bytes outside the payload.
pub const FRAME_OVERHEAD: usize = 4;

/// Borrowed frame header fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// The validated frame opcode.
    opcode: Opcode,
    /// The validated payload length.
    payload_len: u16,
    /// The checksum byte carried by the frame.
    checksum: u8,
}

impl FrameHeader {
    /// Creates a header from decoded frame fields.
    #[must_use]
    pub const fn new(opcode: Opcode, payload_len: u16, checksum: u8) -> Self {
        Self {
            opcode,
            payload_len,
            checksum,
        }
    }

    /// Returns the frame opcode.
    #[must_use]
    pub const fn opcode(self) -> Opcode {
        self.opcode
    }

    /// Returns the declared payload length.
    #[must_use]
    pub const fn payload_len(self) -> u16 {
        self.payload_len
    }

    /// Returns the checksum byte carried by the frame.
    #[must_use]
    pub const fn checksum(self) -> u8 {
        self.checksum
    }

    /// Returns the total frame length, including header and checksum.
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "the frame length adds a fixed four-byte overhead to a u16 payload length"
    )]
    #[expect(
        clippy::as_conversions,
        reason = "a u16 payload length always fits in usize on supported targets"
    )]
    #[must_use]
    pub const fn frame_len(self) -> usize {
        self.payload_len as usize + FRAME_OVERHEAD
    }
}

/// A borrowed view of a validated QUP frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameView<'a> {
    /// The validated header fields.
    header: FrameHeader,
    /// The borrowed payload body.
    payload: &'a [u8],
}

impl<'a> FrameView<'a> {
    /// Creates a borrowed view from a validated header and payload slice.
    #[must_use]
    pub const fn new(header: FrameHeader, payload: &'a [u8]) -> Self {
        Self { header, payload }
    }

    /// Returns the validated header.
    #[must_use]
    pub const fn header(self) -> FrameHeader {
        self.header
    }

    /// Returns the frame opcode.
    #[must_use]
    pub const fn opcode(self) -> Opcode {
        self.header.opcode()
    }

    /// Returns the checksum byte.
    #[must_use]
    pub const fn checksum(self) -> u8 {
        self.header.checksum()
    }

    /// Returns the borrowed payload bytes.
    #[must_use]
    pub const fn payload(self) -> &'a [u8] {
        self.payload
    }

    /// Returns the declared payload length.
    #[must_use]
    pub const fn payload_len(self) -> u16 {
        self.header.payload_len()
    }

    /// Returns the total frame length, including header and checksum.
    #[must_use]
    pub const fn frame_len(self) -> usize {
        self.header.frame_len()
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameHeader, FrameView};
    use crate::types::Opcode;

    #[test]
    fn frame_view_reports_lengths() {
        let payload = [0x00, 0x01];
        let header = FrameHeader::new(Opcode::GET, 2, 0xaa);
        let frame = FrameView::new(header, &payload);

        assert_eq!(frame.opcode(), Opcode::GET);
        assert_eq!(frame.payload_len(), 2);
        assert_eq!(frame.frame_len(), 6);
        assert_eq!(frame.checksum(), 0xaa);
        assert_eq!(frame.payload(), &payload);
    }
}
