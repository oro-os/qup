//! Error types for frame and payload validation.

use core::fmt;
use core::str::Utf8Error;

use crate::types::{Opcode, WireDirection};

/// Frame-level violations of the wire format.
#[expect(
    variant_size_differences,
    reason = "length mismatch keeps both declared and observed lengths for diagnostics"
)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// "Every frame is self-delimiting."
    Truncated,
    /// The declared payload length does not match the supplied frame bytes.
    LengthMismatch {
        /// The payload length declared in the frame header.
        declared: u16,
        /// The payload length implied by the supplied frame bytes.
        actual: usize,
    },
    /// "Any reserved or directionally invalid opcode is a protocol error."
    ReservedOpcode(Opcode),
    /// The opcode has the right ASCII class but is not defined by this specification.
    UnknownOpcode(Opcode),
    /// "A client MUST NOT send `a` through `z`, `@`, `!`, or `:`."
    /// "A node MUST NOT send `A` through `Z` or `?`."
    InvalidDirection {
        /// The opcode that appeared on the wire.
        opcode: Opcode,
        /// The direction in which the opcode was observed.
        direction: WireDirection,
    },
    /// The full-frame checksum did not validate.
    ChecksumMismatch {
        /// The wrapping sum across the full frame bytes.
        sum: u8,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated frame"),
            Self::LengthMismatch { declared, actual } => {
                write!(
                    f,
                    "frame length mismatch: declared {declared}, actual {actual}"
                )
            }
            Self::ReservedOpcode(opcode) => write!(f, "reserved opcode 0x{:02x}", opcode.as_u8()),
            Self::UnknownOpcode(opcode) => write!(f, "unknown opcode 0x{:02x}", opcode.as_u8()),
            Self::InvalidDirection { opcode, direction } => {
                write!(
                    f,
                    "opcode 0x{:02x} is invalid for {direction}",
                    opcode.as_u8()
                )
            }
            Self::ChecksumMismatch { sum } => {
                write!(f, "checksum mismatch: wrapping sum was 0x{sum:02x}")
            }
        }
    }
}

/// Payload-level violations defined by the normative specification.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadError {
    /// The payload size is not valid for the opcode-specific body.
    InvalidLength {
        /// The opcode whose payload shape is being decoded.
        opcode: Opcode,
        /// The exact payload length expected by the decoder.
        expected: usize,
        /// The payload length actually supplied to the decoder.
        actual: usize,
    },
    /// "If any internal length prefix exceeds the remaining payload bytes in the containing frame, the payload is malformed."
    InternalLengthExceedsPayload,
    /// The payload contained extra bytes after a complete decode.
    TrailingBytes {
        /// The number of bytes left unread after decoding.
        remaining: usize,
    },
    /// "Strings MUST be valid UTF-8."
    InvalidUtf8(#[cfg_attr(feature = "defmt", defmt(Debug2Format))] Utf8Error),
    /// "Strings MUST NOT contain `0x00` anywhere."
    StringContainsNul,
    /// "`bool` values are encoded as `0x00` or `0x01`; any other value is a protocol error."
    InvalidBool(u8),
    /// `keyflags` contains bits outside the defined range.
    InvalidKeyFlags(u8),
    /// `value.kind` is not defined by the protocol.
    InvalidValueKind(u8),
    /// The opcode is outside the set this decoder understands.
    UnknownOpcode(Opcode),
    /// The `CAPS` string violates the protocol rules.
    MalformedCaps,
}

impl fmt::Display for PayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength {
                opcode,
                expected,
                actual,
            } => write!(
                f,
                "invalid payload length for opcode 0x{:02x}: expected {expected}, actual {actual}",
                opcode.as_u8()
            ),
            Self::InternalLengthExceedsPayload => {
                f.write_str("internal length prefix exceeds remaining payload bytes")
            }
            Self::TrailingBytes { remaining } => {
                write!(f, "payload has {remaining} trailing byte(s)")
            }
            Self::InvalidUtf8(_) => f.write_str("invalid UTF-8 in str16"),
            Self::StringContainsNul => f.write_str("str16 contains NUL byte"),
            Self::InvalidBool(value) => write!(f, "invalid bool encoding 0x{value:02x}"),
            Self::InvalidKeyFlags(value) => write!(f, "invalid keyflags 0x{value:02x}"),
            Self::InvalidValueKind(value) => write!(f, "invalid value kind 0x{value:02x}"),
            Self::UnknownOpcode(opcode) => write!(f, "unknown opcode 0x{:02x}", opcode.as_u8()),
            Self::MalformedCaps => f.write_str("malformed CAPS string"),
        }
    }
}
