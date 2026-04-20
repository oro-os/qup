//! Scalar decoding helpers shared across payload parsers.

use crate::types::PayloadError;

/// Decodes the protocol `bool` representation.
pub const fn decode_bool(byte: u8) -> Result<bool, PayloadError> {
    match byte {
        0x00 => Ok(false),
        0x01 => Ok(true),
        value => Err(PayloadError::InvalidBool(value)),
    }
}

/// `keyflags` is one byte with bits `0..=2` defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyFlags(u8);

impl KeyFlags {
    /// The readable bit.
    pub const READABLE: u8 = 0b001;
    /// The writable bit.
    pub const WRITABLE: u8 = 0b010;
    /// The observable bit.
    pub const OBSERVABLE: u8 = 0b100;
    /// The mask of every currently defined keyflag bit.
    const VALID_MASK: u8 = Self::READABLE | Self::WRITABLE | Self::OBSERVABLE;

    /// Creates a validated `KeyFlags` value from raw bits.
    pub const fn new(bits: u8) -> Result<Self, PayloadError> {
        if bits & !Self::VALID_MASK == 0 {
            Ok(Self(bits))
        } else {
            Err(PayloadError::InvalidKeyFlags(bits))
        }
    }

    /// Returns the raw flag bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns whether the readable bit is set.
    #[must_use]
    pub const fn is_readable(self) -> bool {
        self.0 & Self::READABLE != 0
    }

    /// Returns whether the writable bit is set.
    #[must_use]
    pub const fn is_writable(self) -> bool {
        self.0 & Self::WRITABLE != 0
    }

    /// Returns whether the observable bit is set.
    #[must_use]
    pub const fn is_observable(self) -> bool {
        self.0 & Self::OBSERVABLE != 0
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyFlags, decode_bool};
    use crate::types::PayloadError;

    #[test]
    fn bool_encoding_is_strict() {
        assert_eq!(decode_bool(0x00), Ok(false));
        assert_eq!(decode_bool(0x01), Ok(true));
        assert_eq!(decode_bool(0x02), Err(PayloadError::InvalidBool(0x02)));
    }

    #[test]
    fn keyflags_only_allow_defined_bits() {
        assert_eq!(
            KeyFlags::new(0b111).map(|flags| (
                flags.is_readable(),
                flags.is_writable(),
                flags.is_observable(),
            )),
            Ok((true, true, true))
        );
        assert_eq!(
            KeyFlags::new(0b1000),
            Err(PayloadError::InvalidKeyFlags(0b1000))
        );
    }
}
