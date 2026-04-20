//! Parsing from a complete borrowed frame slice.

use core::convert::TryInto as _;

use crate::parser::frame_sum;
use crate::types::{FRAME_OVERHEAD, FrameError, FrameHeader, FrameView, Opcode, WireDirection};

/// Parses and validates a complete frame slice for the given wire direction.
pub fn parse_frame(direction: WireDirection, frame: &[u8]) -> Result<FrameView<'_>, FrameError> {
    if frame.len() < FRAME_OVERHEAD {
        return Err(FrameError::Truncated);
    }

    let header: [u8; 3] = frame
        .get(..3)
        .ok_or(FrameError::Truncated)?
        .try_into()
        .map_err(|_error| FrameError::Truncated)?;
    let [opcode_byte, length_hi, length_lo] = header;
    let opcode = Opcode::new(opcode_byte);
    let length = u16::from_be_bytes([length_hi, length_lo]);
    let payload_len = frame.len().saturating_sub(FRAME_OVERHEAD);

    if payload_len != usize::from(length) {
        return Err(FrameError::LengthMismatch {
            declared: length,
            actual: payload_len,
        });
    }

    let sum = frame_sum(frame);
    if sum != 0 {
        return Err(FrameError::ChecksumMismatch { sum });
    }

    opcode.validate(direction)?;

    let Some((checksum, without_checksum)) = frame.split_last() else {
        return Err(FrameError::Truncated);
    };
    let Some(payload) = without_checksum.get(3..) else {
        return Err(FrameError::Truncated);
    };
    Ok(FrameView::new(
        FrameHeader::new(opcode, length, *checksum),
        payload,
    ))
}

#[cfg(test)]
mod tests {
    use super::parse_frame;
    use crate::types::{FrameError, Opcode, WireDirection};

    #[test]
    fn parses_spec_examples() {
        assert_eq!(
            parse_frame(WireDirection::ClientToNode, &[0x3f, 0x00, 0x00, 0xc1])
                .map(|frame| (frame.opcode(), frame.payload().to_vec())),
            Ok((Opcode::GETCAPS, Vec::new()))
        );

        assert_eq!(
            parse_frame(
                WireDirection::NodeToClient,
                &[0x73, 0x00, 0x06, 0x07, 0x00, 0x03, b'l', b'e', b'd', 0x48],
            )
            .map(|frame| frame.opcode()),
            Ok(Opcode::KEY)
        );
    }

    #[test]
    fn rejects_bad_checksum() {
        assert!(matches!(
            parse_frame(WireDirection::ClientToNode, &[0x3f, 0x00, 0x00, 0x00]),
            Err(FrameError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn rejects_length_mismatch() {
        assert!(matches!(
            parse_frame(WireDirection::ClientToNode, &[0x3f, 0x00, 0x01, 0xc1]),
            Err(FrameError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn rejects_wrong_direction() {
        assert!(matches!(
            parse_frame(WireDirection::ClientToNode, &[0x6b, 0x00, 0x00, 0x95]),
            Err(FrameError::InvalidDirection { .. })
        ));
    }
}
