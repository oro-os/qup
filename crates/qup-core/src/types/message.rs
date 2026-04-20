//! Typed decoded request, response, and control-message payloads.

#![expect(
    clippy::single_char_lifetime_names,
    reason = "decoded borrowed message shapes use conventional single-letter lifetime names"
)]

use crate::types::{CapsRef, FrameView, KeyFlags, Opcode, PayloadCursor, PayloadError, ValueRef};

/// Decoded request payloads.
#[expect(
    variant_size_differences,
    reason = "WRITE must carry its decoded value inline in the borrowed wire model"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestRef<'a> {
    /// `PING` with no payload.
    Ping,
    /// `IDENTIFY` with no payload.
    Identify,
    /// `GETKEYTABLEN` with no payload.
    GetKeytabLen,
    /// `GETKEY` for the referenced key.
    GetKey {
        /// The requested key reference.
        keyref: u16,
    },
    /// `GET` for the referenced key.
    Get {
        /// The requested key reference.
        keyref: u16,
    },
    /// `WRITE` for the referenced key and new value.
    Write {
        /// The key reference to update.
        keyref: u16,
        /// The new value to write.
        value: ValueRef<'a>,
    },
    /// `OBSERVE` for the referenced key.
    Observe {
        /// The key reference to observe.
        keyref: u16,
    },
    /// `UNOBSERVE` for the referenced key.
    Unobserve {
        /// The key reference to stop observing.
        keyref: u16,
    },
}

/// Decoded ordinary success responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdinaryResponseRef<'a> {
    /// `OK` with no payload.
    Ok,
    /// `IDENTIFIED` containing a borrowed node identifier.
    Identified {
        /// The borrowed node identifier.
        nodeid: &'a str,
    },
    /// `KEYTABLEN` containing the connection's key count.
    KeytabLen {
        /// The number of keys in the connection's key table.
        count: u16,
    },
    /// `KEY` containing key flags and a borrowed name.
    Key {
        /// The validated key flags.
        keyflags: KeyFlags,
        /// The borrowed key name.
        name: &'a str,
    },
    /// `VALUE` containing a decoded value.
    Value {
        /// The decoded current value.
        value: ValueRef<'a>,
    },
    /// `WRITTEN` containing the updated value.
    Written {
        /// The decoded value after a successful write.
        value: ValueRef<'a>,
    },
}

/// Decoded request-level error response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorResponse {
    /// The request-specific error code byte.
    code: u8,
}

impl ErrorResponse {
    /// Creates an error response from a raw error code.
    #[must_use]
    pub const fn new(code: u8) -> Self {
        Self { code }
    }

    /// Returns the request-specific error code.
    #[must_use]
    pub const fn code(self) -> u8 {
        self.code
    }
}

/// Decoded QUP message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRef<'a> {
    /// A decoded client-to-node request.
    Request(RequestRef<'a>),
    /// A decoded ordinary node-to-client success response.
    OrdinaryResponse(OrdinaryResponseRef<'a>),
    /// A decoded compatibility request.
    CompatibilityRequest,
    /// A decoded compatibility response.
    CompatibilityResponse {
        /// The validated capability string view.
        caps: CapsRef<'a>,
    },
    /// A decoded request-level error response.
    Error(ErrorResponse),
    /// A decoded changed notification.
    Changed {
        /// The changed key reference.
        keyref: u16,
    },
}

impl<'a> MessageRef<'a> {
    /// Decodes an opcode and borrowed payload into a typed QUP message.
    pub fn decode(opcode: Opcode, payload: &'a [u8]) -> Result<Self, PayloadError> {
        match opcode.as_u8() {
            b'P' => expect_empty(opcode, payload).map(|()| Self::Request(RequestRef::Ping)),
            b'I' => expect_empty(opcode, payload).map(|()| Self::Request(RequestRef::Identify)),
            b'C' => expect_empty(opcode, payload).map(|()| Self::Request(RequestRef::GetKeytabLen)),
            b'S' => decode_keyref_request(opcode, payload, |keyref| RequestRef::GetKey { keyref }),
            b'G' => decode_keyref_request(opcode, payload, |keyref| RequestRef::Get { keyref }),
            b'N' => decode_keyref_request(opcode, payload, |keyref| RequestRef::Observe { keyref }),
            b'U' => {
                decode_keyref_request(opcode, payload, |keyref| RequestRef::Unobserve { keyref })
            }
            b'W' => decode_write(payload),
            b'?' => expect_empty(opcode, payload).map(|()| Self::CompatibilityRequest),
            b'k' => expect_empty(opcode, payload)
                .map(|()| Self::OrdinaryResponse(OrdinaryResponseRef::Ok)),
            b'i' => decode_identified(payload),
            b'c' => decode_keytablen(opcode, payload),
            b's' => decode_key(payload),
            b'g' => decode_value_response(payload, false),
            b'w' => decode_value_response(payload, true),
            b':' => decode_caps(payload),
            b'@' => decode_error(opcode, payload),
            b'!' => decode_changed(opcode, payload),
            _ => Err(PayloadError::UnknownOpcode(opcode)),
        }
    }
}

impl<'a> FrameView<'a> {
    /// Decodes the frame payload according to the frame opcode.
    pub fn decode_message(self) -> Result<MessageRef<'a>, PayloadError> {
        MessageRef::decode(self.opcode(), self.payload())
    }
}

/// Verifies that an opcode-specific payload is empty.
const fn expect_empty(opcode: Opcode, payload: &[u8]) -> Result<(), PayloadError> {
    if payload.is_empty() {
        Ok(())
    } else {
        Err(PayloadError::InvalidLength {
            opcode,
            expected: 0,
            actual: payload.len(),
        })
    }
}

/// Verifies that an opcode-specific payload has a fixed length.
const fn expect_len(opcode: Opcode, payload: &[u8], expected: usize) -> Result<(), PayloadError> {
    if payload.len() == expected {
        Ok(())
    } else {
        Err(PayloadError::InvalidLength {
            opcode,
            expected,
            actual: payload.len(),
        })
    }
}

/// Decodes any request whose payload is a single `keyref`.
fn decode_keyref_request<'a>(
    opcode: Opcode,
    payload: &'a [u8],
    constructor: fn(u16) -> RequestRef<'a>,
) -> Result<MessageRef<'a>, PayloadError> {
    expect_len(opcode, payload, 2)?;
    let mut cursor = PayloadCursor::new(payload);
    Ok(MessageRef::Request(constructor(cursor.read_u16()?)))
}

/// Decodes the `WRITE` request payload.
fn decode_write(payload: &[u8]) -> Result<MessageRef<'_>, PayloadError> {
    let mut cursor = PayloadCursor::new(payload);
    let keyref = cursor.read_u16()?;
    let value = ValueRef::decode(&mut cursor)?;
    cursor.finish()?;
    Ok(MessageRef::Request(RequestRef::Write { keyref, value }))
}

/// Decodes the `IDENTIFIED` response payload.
fn decode_identified(payload: &[u8]) -> Result<MessageRef<'_>, PayloadError> {
    let mut cursor = PayloadCursor::new(payload);
    let nodeid = cursor.read_str16()?;
    cursor.finish()?;
    Ok(MessageRef::OrdinaryResponse(
        OrdinaryResponseRef::Identified { nodeid },
    ))
}

/// Decodes the `KEYTABLEN` response payload.
fn decode_keytablen(opcode: Opcode, payload: &[u8]) -> Result<MessageRef<'_>, PayloadError> {
    expect_len(opcode, payload, 2)?;
    let mut cursor = PayloadCursor::new(payload);
    Ok(MessageRef::OrdinaryResponse(
        OrdinaryResponseRef::KeytabLen {
            count: cursor.read_u16()?,
        },
    ))
}

/// Decodes the `KEY` response payload.
fn decode_key(payload: &[u8]) -> Result<MessageRef<'_>, PayloadError> {
    let mut cursor = PayloadCursor::new(payload);
    let keyflags = KeyFlags::new(cursor.read_u8()?)?;
    let name = cursor.read_str16()?;
    cursor.finish()?;
    Ok(MessageRef::OrdinaryResponse(OrdinaryResponseRef::Key {
        keyflags,
        name,
    }))
}

/// Decodes either the `VALUE` or `WRITTEN` response payload.
fn decode_value_response(payload: &[u8], written: bool) -> Result<MessageRef<'_>, PayloadError> {
    let mut cursor = PayloadCursor::new(payload);
    let value = ValueRef::decode(&mut cursor)?;
    cursor.finish()?;

    Ok(MessageRef::OrdinaryResponse(if written {
        OrdinaryResponseRef::Written { value }
    } else {
        OrdinaryResponseRef::Value { value }
    }))
}

/// Decodes the `CAPS` response payload.
fn decode_caps(payload: &[u8]) -> Result<MessageRef<'_>, PayloadError> {
    let mut cursor = PayloadCursor::new(payload);
    let caps = CapsRef::parse(cursor.read_str16()?)?;
    cursor.finish()?;
    Ok(MessageRef::CompatibilityResponse { caps })
}

/// Decodes the `ERROR` response payload.
fn decode_error(opcode: Opcode, payload: &[u8]) -> Result<MessageRef<'_>, PayloadError> {
    expect_len(opcode, payload, 1)?;
    let code = payload
        .first()
        .copied()
        .ok_or(PayloadError::InvalidLength {
            opcode,
            expected: 1,
            actual: payload.len(),
        })?;
    Ok(MessageRef::Error(ErrorResponse::new(code)))
}

/// Decodes the `CHANGED` notification payload.
fn decode_changed(opcode: Opcode, payload: &[u8]) -> Result<MessageRef<'_>, PayloadError> {
    expect_len(opcode, payload, 2)?;
    let mut cursor = PayloadCursor::new(payload);
    Ok(MessageRef::Changed {
        keyref: cursor.read_u16()?,
    })
}

#[cfg(test)]
mod tests {
    use super::{ErrorResponse, MessageRef, OrdinaryResponseRef, RequestRef};
    use crate::types::{KeyFlags, Opcode, ValueRef};

    #[test]
    fn decodes_request_examples() {
        assert_eq!(
            MessageRef::decode(Opcode::GETCAPS, &[]),
            Ok(MessageRef::CompatibilityRequest)
        );
        assert_eq!(
            MessageRef::decode(Opcode::GETKEY, &[0x00, 0x01]),
            Ok(MessageRef::Request(RequestRef::GetKey { keyref: 1 }))
        );
    }

    #[test]
    fn decodes_key_response_example() {
        assert_eq!(
            MessageRef::decode(Opcode::KEY, &[0x07, 0x00, 0x03, b'l', b'e', b'd']),
            KeyFlags::new(0x07).map(|keyflags| {
                MessageRef::OrdinaryResponse(OrdinaryResponseRef::Key {
                    keyflags,
                    name: "led",
                })
            })
        );
    }

    #[test]
    fn decodes_caps_and_changed_examples() {
        assert!(matches!(
            MessageRef::decode(Opcode::CAPS, &[0x00, 0x05, b'N', b'k', b'U', b'k', b'!']),
            Ok(MessageRef::CompatibilityResponse { .. })
        ));

        assert_eq!(
            MessageRef::decode(Opcode::CHANGED, &[0x00, 0x01]),
            Ok(MessageRef::Changed { keyref: 1 })
        );
    }

    #[test]
    fn decodes_written_and_error_responses() {
        assert_eq!(
            MessageRef::decode(Opcode::WRITTEN, &[0x01, 0x01]),
            Ok(MessageRef::OrdinaryResponse(OrdinaryResponseRef::Written {
                value: ValueRef::Bool(true),
            }))
        );

        assert_eq!(
            MessageRef::decode(Opcode::ERROR, &[0x02]),
            Ok(MessageRef::Error(ErrorResponse::new(0x02)))
        );
    }

    #[test]
    fn rejects_extra_bytes_for_empty_messages() {
        assert!(matches!(
            MessageRef::decode(Opcode::PING, &[0x00]),
            Err(crate::types::PayloadError::InvalidLength { .. })
        ));
    }
}
