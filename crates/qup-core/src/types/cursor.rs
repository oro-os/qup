//! Borrowed cursor utilities for decoding payload bodies.

#![expect(
    clippy::single_char_lifetime_names,
    reason = "the payload cursor models one borrowed payload lifetime throughout the module"
)]

use core::convert::TryInto as _;
use core::str;

use crate::types::PayloadError;

/// A borrowed cursor over an opcode payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadCursor<'a> {
    /// The unread tail of the borrowed payload.
    remaining: &'a [u8],
}

impl<'a> PayloadCursor<'a> {
    /// Creates a cursor over the provided payload bytes.
    #[must_use]
    pub const fn new(payload: &'a [u8]) -> Self {
        Self { remaining: payload }
    }

    /// Returns the unread tail of the payload.
    #[must_use]
    pub const fn remaining(self) -> &'a [u8] {
        self.remaining
    }

    /// Returns the number of unread bytes remaining in the payload.
    #[must_use]
    pub const fn remaining_len(self) -> usize {
        self.remaining.len()
    }

    /// Verifies that the payload was consumed exactly.
    pub const fn finish(self) -> Result<(), PayloadError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            Err(PayloadError::TrailingBytes {
                remaining: self.remaining.len(),
            })
        }
    }

    /// Reads a single big-endian byte.
    pub fn read_u8(&mut self) -> Result<u8, PayloadError> {
        Ok(self.take_array::<1>()?[0])
    }

    /// Reads a big-endian `u16`.
    pub fn read_u16(&mut self) -> Result<u16, PayloadError> {
        Ok(u16::from_be_bytes(self.take_array::<2>()?))
    }

    /// Reads a big-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32, PayloadError> {
        Ok(u32::from_be_bytes(self.take_array::<4>()?))
    }

    /// Reads a big-endian `u64`.
    pub fn read_u64(&mut self) -> Result<u64, PayloadError> {
        Ok(u64::from_be_bytes(self.take_array::<8>()?))
    }

    /// Reads a big-endian two's-complement `i64`.
    pub fn read_i64(&mut self) -> Result<i64, PayloadError> {
        Ok(i64::from_be_bytes(self.take_array::<8>()?))
    }

    /// Reads the next `len` raw bytes.
    pub const fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], PayloadError> {
        self.take(len)
    }

    /// Reads a `bytes16` field.
    pub fn read_bytes16(&mut self) -> Result<&'a [u8], PayloadError> {
        let len = usize::from(self.read_u16()?);
        self.take(len)
    }

    /// Reads and validates a `str16` field.
    pub fn read_str16(&mut self) -> Result<&'a str, PayloadError> {
        let bytes = self.read_bytes16()?;

        if bytes.contains(&0x00) {
            return Err(PayloadError::StringContainsNul);
        }

        str::from_utf8(bytes).map_err(PayloadError::InvalidUtf8)
    }

    /// Advances the cursor by `len` bytes and returns that slice.
    const fn take(&mut self, len: usize) -> Result<&'a [u8], PayloadError> {
        if self.remaining.len() < len {
            return Err(PayloadError::InternalLengthExceedsPayload);
        }

        let (head, tail) = self.remaining.split_at(len);
        self.remaining = tail;
        Ok(head)
    }

    /// Advances the cursor by `N` bytes and returns them as an array.
    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], PayloadError> {
        self.take(N)?
            .try_into()
            .map_err(|_error| PayloadError::InternalLengthExceedsPayload)
    }
}

#[cfg(test)]
mod tests {
    use super::PayloadCursor;
    use crate::types::PayloadError;

    #[test]
    fn reads_scalar_values_in_big_endian_order() {
        let mut cursor = PayloadCursor::new(&[0x12, 0x34, 0x01, 0x02, 0x03, 0x04]);

        assert_eq!(cursor.read_u16(), Ok(0x1234));
        assert_eq!(cursor.read_u32(), Ok(0x0102_0304));
        assert_eq!(cursor.finish(), Ok(()));
    }

    #[test]
    fn decodes_valid_str16() {
        let mut cursor = PayloadCursor::new(&[0x00, 0x03, b'l', b'e', b'd']);
        assert_eq!(cursor.read_str16(), Ok("led"));
        assert_eq!(cursor.finish(), Ok(()));
    }

    #[test]
    fn rejects_str16_with_nul() {
        let mut cursor = PayloadCursor::new(&[0x00, 0x03, b'l', 0x00, b'd']);
        assert_eq!(cursor.read_str16(), Err(PayloadError::StringContainsNul));
    }

    #[test]
    fn rejects_internal_length_overrun() {
        let mut cursor = PayloadCursor::new(&[0x00, 0x05, b'l', b'e', b'd']);
        assert_eq!(
            cursor.read_str16(),
            Err(PayloadError::InternalLengthExceedsPayload)
        );
    }
}
