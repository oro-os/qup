//! Opcode classification and validation helpers.

use core::fmt;

use crate::types::FrameError;

/// Direction of bytes on the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireDirection {
    /// Bytes sent from a client toward a node.
    ClientToNode,
    /// Bytes sent from a node toward a client.
    NodeToClient,
}

impl fmt::Display for WireDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientToNode => f.write_str("client-to-node traffic"),
            Self::NodeToClient => f.write_str("node-to-client traffic"),
        }
    }
}

/// Classification for a single opcode byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpcodeClass {
    /// A client-to-node request opcode in `A..=Z`.
    Request,
    /// A node-to-client ordinary success response in `a..=z`.
    OrdinaryResponse,
    /// The request-level error response opcode `@`.
    ErrorResponse,
    /// The node-initiated changed notification opcode `!`.
    ChangedNotification,
    /// The compatibility request opcode `?`.
    CompatibilityRequest,
    /// The compatibility response opcode `:`.
    CompatibilityResponse,
    /// Any byte that is reserved by the specification.
    Reserved,
}

/// A raw opcode byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Opcode(pub u8);

impl Opcode {
    /// Request opcode for `PING`.
    pub const PING: Self = Self(b'P');
    /// Request opcode for `IDENTIFY`.
    pub const IDENTIFY: Self = Self(b'I');
    /// Request opcode for `GETKEYTABLEN`.
    pub const GETKEYTABLEN: Self = Self(b'C');
    /// Request opcode for `GETKEY`.
    pub const GETKEY: Self = Self(b'S');
    /// Request opcode for `GET`.
    pub const GET: Self = Self(b'G');
    /// Request opcode for `WRITE`.
    pub const WRITE: Self = Self(b'W');
    /// Request opcode for `OBSERVE`.
    pub const OBSERVE: Self = Self(b'N');
    /// Request opcode for `UNOBSERVE`.
    pub const UNOBSERVE: Self = Self(b'U');
    /// Compatibility request opcode for `GETCAPS`.
    pub const GETCAPS: Self = Self(b'?');

    /// Ordinary success response opcode for `OK`.
    pub const OK: Self = Self(b'k');
    /// Ordinary success response opcode for `IDENTIFIED`.
    pub const IDENTIFIED: Self = Self(b'i');
    /// Ordinary success response opcode for `KEYTABLEN`.
    pub const KEYTABLEN: Self = Self(b'c');
    /// Ordinary success response opcode for `KEY`.
    pub const KEY: Self = Self(b's');
    /// Ordinary success response opcode for `VALUE`.
    pub const VALUE: Self = Self(b'g');
    /// Ordinary success response opcode for `WRITTEN`.
    pub const WRITTEN: Self = Self(b'w');
    /// Compatibility response opcode for `CAPS`.
    pub const CAPS: Self = Self(b':');
    /// Request-level error response opcode `@`.
    pub const ERROR: Self = Self(b'@');
    /// Node-initiated changed notification opcode `!`.
    pub const CHANGED: Self = Self(b'!');

    /// Wraps a raw opcode byte.
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the raw opcode byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self.0
    }

    /// "The opcode byte defines the direction and class of a frame."
    #[must_use]
    pub const fn class(self) -> OpcodeClass {
        match self.0 {
            b'A'..=b'Z' => OpcodeClass::Request,
            b'a'..=b'z' => OpcodeClass::OrdinaryResponse,
            b'@' => OpcodeClass::ErrorResponse,
            b'!' => OpcodeClass::ChangedNotification,
            b'?' => OpcodeClass::CompatibilityRequest,
            b':' => OpcodeClass::CompatibilityResponse,
            _ => OpcodeClass::Reserved,
        }
    }

    /// Returns whether the opcode byte falls outside the defined opcode classes.
    #[must_use]
    pub const fn is_reserved(self) -> bool {
        matches!(self.class(), OpcodeClass::Reserved)
    }

    /// Returns whether the opcode belongs to any non-reserved ASCII class.
    #[must_use]
    pub const fn is_ok(self) -> bool {
        !self.is_reserved()
    }

    /// Returns whether the opcode is explicitly defined by this crate's QUP model.
    #[must_use]
    pub const fn is_defined(self) -> bool {
        matches!(
            self.0,
            b'P' | b'I'
                | b'C'
                | b'S'
                | b'G'
                | b'W'
                | b'N'
                | b'U'
                | b'?'
                | b'k'
                | b'i'
                | b'c'
                | b's'
                | b'g'
                | b'w'
                | b':'
                | b'@'
                | b'!'
        )
    }

    /// Returns the ordinary success response defined for this request, if any.
    #[must_use]
    pub const fn expected_ordinary_response(self) -> Option<Self> {
        match self.0 {
            b'P' | b'N' | b'U' => Some(Self::OK),
            b'I' => Some(Self::IDENTIFIED),
            b'C' => Some(Self::KEYTABLEN),
            b'S' => Some(Self::KEY),
            b'G' => Some(Self::VALUE),
            b'W' => Some(Self::WRITTEN),
            _ => None,
        }
    }

    /// Returns the required wire direction for the opcode.
    #[must_use]
    pub const fn required_direction(self) -> Option<WireDirection> {
        match self.class() {
            OpcodeClass::Request | OpcodeClass::CompatibilityRequest => {
                Some(WireDirection::ClientToNode)
            }
            OpcodeClass::OrdinaryResponse
            | OpcodeClass::ErrorResponse
            | OpcodeClass::ChangedNotification
            | OpcodeClass::CompatibilityResponse => Some(WireDirection::NodeToClient),
            OpcodeClass::Reserved => None,
        }
    }

    /// Validates that the opcode is defined and legal in the given direction.
    pub const fn validate(self, direction: WireDirection) -> Result<(), FrameError> {
        if self.is_reserved() {
            return Err(FrameError::ReservedOpcode(self));
        }

        if !self.is_defined() {
            return Err(FrameError::UnknownOpcode(self));
        }

        match self.required_direction() {
            Some(WireDirection::ClientToNode) => match direction {
                WireDirection::ClientToNode => Ok(()),
                WireDirection::NodeToClient => Err(FrameError::InvalidDirection {
                    opcode: self,
                    direction,
                }),
            },
            Some(WireDirection::NodeToClient) => match direction {
                WireDirection::NodeToClient => Ok(()),
                WireDirection::ClientToNode => Err(FrameError::InvalidDirection {
                    opcode: self,
                    direction,
                }),
            },
            None => Err(FrameError::ReservedOpcode(self)),
        }
    }
}

impl fmt::Display for Opcode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            byte if byte.is_ascii_graphic() || byte == b' ' => {
                write!(f, "'{}'", char::from(byte))
            }
            _ => write!(f, "0x{:02x}", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Opcode, OpcodeClass, WireDirection};

    #[test]
    fn classifies_defined_opcode_families() {
        assert_eq!(Opcode::PING.class(), OpcodeClass::Request);
        assert_eq!(Opcode::OK.class(), OpcodeClass::OrdinaryResponse);
        assert_eq!(Opcode::ERROR.class(), OpcodeClass::ErrorResponse);
        assert_eq!(Opcode::CHANGED.class(), OpcodeClass::ChangedNotification);
        assert_eq!(Opcode::GETCAPS.class(), OpcodeClass::CompatibilityRequest);
        assert_eq!(Opcode::CAPS.class(), OpcodeClass::CompatibilityResponse);
        assert_eq!(Opcode::new(0x00).class(), OpcodeClass::Reserved);
    }

    #[test]
    fn rejects_reserved_opcode() {
        assert!(matches!(
            Opcode::new(0x01).validate(WireDirection::ClientToNode),
            Err(crate::types::FrameError::ReservedOpcode(_))
        ));
    }

    #[test]
    fn rejects_unknown_but_classified_opcode() {
        assert!(matches!(
            Opcode::new(b'Z').validate(WireDirection::ClientToNode),
            Err(crate::types::FrameError::UnknownOpcode(_))
        ));
        assert!(matches!(
            Opcode::new(b'z').validate(WireDirection::NodeToClient),
            Err(crate::types::FrameError::UnknownOpcode(_))
        ));
    }

    #[test]
    fn validates_direction_rules() {
        assert_eq!(Opcode::PING.validate(WireDirection::ClientToNode), Ok(()));
        assert!(matches!(
            Opcode::PING.validate(WireDirection::NodeToClient),
            Err(crate::types::FrameError::InvalidDirection { .. })
        ));
        assert_eq!(Opcode::OK.validate(WireDirection::NodeToClient), Ok(()));
        assert!(matches!(
            Opcode::OK.validate(WireDirection::ClientToNode),
            Err(crate::types::FrameError::InvalidDirection { .. })
        ));
        assert_eq!(
            Opcode::GETCAPS.validate(WireDirection::ClientToNode),
            Ok(())
        );
        assert_eq!(Opcode::CAPS.validate(WireDirection::NodeToClient), Ok(()));
    }
}
