//! Checksum helpers for QUP frame construction and validation.

use crate::types::Opcode;

/// Computes the wrapping sum across a full frame byte slice.
#[must_use]
pub fn frame_sum(frame: &[u8]) -> u8 {
    frame.iter().fold(0u8, |acc, byte| acc.wrapping_add(*byte))
}

/// Computes the checksum byte that makes the full frame sum equal zero modulo `256`.
#[expect(
    clippy::as_conversions,
    reason = "QUP frames encode payload length as u16 and callers must stay within that wire limit"
)]
#[must_use]
pub fn compute_checksum(opcode: Opcode, payload: &[u8]) -> u8 {
    let length = payload.len() as u16;
    let [length_hi, length_lo] = length.to_be_bytes();

    let mut acc = 0u8;
    acc = acc.wrapping_add(opcode.as_u8());
    acc = acc.wrapping_add(length_hi);
    acc = acc.wrapping_add(length_lo);

    for byte in payload {
        acc = acc.wrapping_add(*byte);
    }

    0u8.wrapping_sub(acc)
}

#[cfg(test)]
mod tests {
    use super::{compute_checksum, frame_sum};
    use crate::types::Opcode;

    #[test]
    fn matches_spec_examples() {
        assert_eq!(compute_checksum(Opcode::GETCAPS, &[]), 0xc1);
        assert_eq!(compute_checksum(Opcode::PING, &[]), 0xb0);
        assert_eq!(
            compute_checksum(Opcode::KEY, &[0x07, 0x00, 0x03, b'l', b'e', b'd']),
            0x48
        );
    }

    #[test]
    fn full_frame_sum_validates_to_zero() {
        let frame = [0x3f, 0x00, 0x00, 0xc1];
        assert_eq!(frame_sum(&frame), 0);
    }
}
