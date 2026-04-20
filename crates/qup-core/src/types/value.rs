//! Decoding for the protocol `value` type.

#![expect(
    clippy::single_char_lifetime_names,
    reason = "the borrowed value model uses one conventional lifetime parameter"
)]

use crate::types::{PayloadCursor, PayloadError, decode_bool};

/// The discriminator byte used by the `value` payload type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// The `bool` variant.
    Bool,
    /// The `i64` variant.
    I64,
    /// The `str16` variant.
    Str,
}

impl ValueKind {
    /// Wire tag for `bool`.
    pub const BOOL_TAG: u8 = 0x01;
    /// Wire tag for `i64`.
    pub const I64_TAG: u8 = 0x02;
    /// Wire tag for `str16`.
    pub const STR_TAG: u8 = 0x03;

    /// Decodes a wire tag into a [`ValueKind`].
    pub const fn from_byte(byte: u8) -> Result<Self, PayloadError> {
        match byte {
            Self::BOOL_TAG => Ok(Self::Bool),
            Self::I64_TAG => Ok(Self::I64),
            Self::STR_TAG => Ok(Self::Str),
            value => Err(PayloadError::InvalidValueKind(value)),
        }
    }

    /// Returns the wire tag for this kind.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Bool => Self::BOOL_TAG,
            Self::I64 => Self::I64_TAG,
            Self::Str => Self::STR_TAG,
        }
    }
}

/// Borrowed decoded representation of the protocol `value` type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueRef<'a> {
    /// A decoded `bool` value.
    Bool(bool),
    /// A decoded `i64` value.
    I64(i64),
    /// A decoded borrowed `str16` value.
    Str(&'a str),
}

impl<'a> ValueRef<'a> {
    /// Decodes a `value` body from the payload cursor.
    pub fn decode(cursor: &mut PayloadCursor<'a>) -> Result<Self, PayloadError> {
        match ValueKind::from_byte(cursor.read_u8()?)? {
            ValueKind::Bool => Ok(Self::Bool(decode_bool(cursor.read_u8()?)?)),
            ValueKind::I64 => Ok(Self::I64(cursor.read_i64()?)),
            ValueKind::Str => Ok(Self::Str(cursor.read_str16()?)),
        }
    }

    /// Returns the [`ValueKind`] corresponding to this decoded value.
    #[must_use]
    pub const fn kind(self) -> ValueKind {
        match self {
            Self::Bool(_) => ValueKind::Bool,
            Self::I64(_) => ValueKind::I64,
            Self::Str(_) => ValueKind::Str,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ValueKind, ValueRef};
    use crate::types::PayloadCursor;

    #[test]
    fn decodes_bool_value() {
        let mut cursor = PayloadCursor::new(&[ValueKind::BOOL_TAG, 0x01]);
        assert_eq!(ValueRef::decode(&mut cursor), Ok(ValueRef::Bool(true)));
        assert_eq!(cursor.finish(), Ok(()));
    }

    #[test]
    fn decodes_i64_value() {
        let mut cursor = PayloadCursor::new(&[
            ValueKind::I64_TAG,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xfe,
        ]);
        assert_eq!(ValueRef::decode(&mut cursor), Ok(ValueRef::I64(-2)));
        assert_eq!(cursor.finish(), Ok(()));
    }

    #[test]
    fn decodes_string_value() {
        let mut cursor = PayloadCursor::new(&[ValueKind::STR_TAG, 0x00, 0x03, b'l', b'e', b'd']);
        assert_eq!(ValueRef::decode(&mut cursor), Ok(ValueRef::Str("led")));
        assert_eq!(cursor.finish(), Ok(()));
    }

    #[test]
    fn rejects_unknown_value_kind() {
        let mut cursor = PayloadCursor::new(&[0x99]);
        assert!(matches!(
            ValueRef::decode(&mut cursor),
            Err(crate::types::PayloadError::InvalidValueKind(0x99))
        ));
    }
}
