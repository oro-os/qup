//! Borrowed views and iteration for the `CAPS` compatibility payload.

#![expect(
    clippy::single_char_lifetime_names,
    reason = "the CAPS views use one borrowed-string lifetime throughout the module"
)]

use crate::types::{Opcode, OpcodeClass, PayloadError};

/// One advertised request/ordinary-response pair from a `CAPS` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapsEntry {
    /// The advertised request opcode.
    request: Opcode,
    /// The advertised ordinary success response opcode.
    response: Opcode,
    /// Whether the request/response pair is known to this crate.
    known: bool,
}

impl CapsEntry {
    /// Returns the advertised request opcode.
    #[must_use]
    pub const fn request(self) -> Opcode {
        self.request
    }

    /// Returns the advertised ordinary success response opcode.
    #[must_use]
    pub const fn response(self) -> Opcode {
        self.response
    }

    /// Returns whether the request/response pair is defined by this crate.
    #[must_use]
    pub const fn is_known(self) -> bool {
        self.known
    }
}

/// Borrowed validated view of the `caps:str16` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapsRef<'a> {
    /// The validated raw `caps` string.
    raw: &'a str,
    /// Whether the capability string ends in `!`.
    supports_changed: bool,
}

impl<'a> CapsRef<'a> {
    /// "The string MUST be ASCII."
    /// "If `!` appears, it MUST appear exactly once and it MUST be the last byte of the string."
    pub fn parse(raw: &'a str) -> Result<Self, PayloadError> {
        if !raw.is_ascii() {
            return Err(PayloadError::MalformedCaps);
        }

        let bytes = raw.as_bytes();
        let mut seen = [0u8; 26];
        let mut index = 0usize;

        while let Some(&request_byte) = bytes.get(index) {
            if request_byte == b'!' {
                if index.checked_add(1) != Some(bytes.len()) {
                    return Err(PayloadError::MalformedCaps);
                }
                break;
            }

            let Some(response_index) = index.checked_add(1) else {
                return Err(PayloadError::MalformedCaps);
            };
            let Some(&response_byte) = bytes.get(response_index) else {
                return Err(PayloadError::MalformedCaps);
            };

            let request = Opcode::new(request_byte);
            let response = Opcode::new(response_byte);

            if !matches!(request.class(), OpcodeClass::Request) {
                return Err(PayloadError::MalformedCaps);
            }

            if !matches!(response.class(), OpcodeClass::OrdinaryResponse) {
                return Err(PayloadError::MalformedCaps);
            }

            if let Some(expected) = request.expected_ordinary_response()
                && expected != response
            {
                return Err(PayloadError::MalformedCaps);
            }

            let Some(slot_byte) = request.as_u8().checked_sub(b'A') else {
                return Err(PayloadError::MalformedCaps);
            };
            let slot = usize::from(slot_byte);
            let Some(seen_response) = seen.get_mut(slot) else {
                return Err(PayloadError::MalformedCaps);
            };

            if *seen_response != 0 && *seen_response != response.as_u8() {
                return Err(PayloadError::MalformedCaps);
            }

            if *seen_response == 0 {
                *seen_response = response.as_u8();
            }

            index = response_index.saturating_add(1);
        }

        Ok(Self {
            raw,
            supports_changed: bytes.last() == Some(&b'!'),
        })
    }

    /// Returns the validated raw `caps` string.
    #[must_use]
    pub const fn as_str(self) -> &'a str {
        self.raw
    }

    /// Returns whether the capability string ends with `!`.
    #[must_use]
    pub const fn supports_changed(self) -> bool {
        self.supports_changed
    }

    /// Iterates over the request/response pairs in the payload.
    #[must_use]
    pub const fn iter(self) -> CapsIter<'a> {
        CapsIter {
            bytes: self.raw.as_bytes(),
            index: 0,
        }
    }
}

/// Iterator over advertised request/response pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapsIter<'a> {
    /// The capability-string bytes being iterated.
    bytes: &'a [u8],
    /// The current byte position within `bytes`.
    index: usize,
}

#[expect(
    clippy::copy_iterator,
    reason = "this iterator is a tiny borrowed cursor value and copyability is intentional"
)]
impl Iterator for CapsIter<'_> {
    type Item = CapsEntry;

    fn next(&mut self) -> Option<Self::Item> {
        let &request_byte = self.bytes.get(self.index)?;
        if request_byte == b'!' {
            return None;
        }

        let response_index = self.index.checked_add(1)?;
        let &response_byte = self.bytes.get(response_index)?;
        let request = Opcode::new(request_byte);
        let response = Opcode::new(response_byte);
        self.index = response_index.saturating_add(1);

        Some(CapsEntry {
            request,
            response,
            known: request.expected_ordinary_response().is_some(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::CapsRef;
    use crate::types::{Opcode, PayloadError};

    #[test]
    fn accepts_spec_examples() {
        assert_eq!(
            CapsRef::parse("PkIiCcSsGgWwNkUk")
                .map(|caps| (caps.supports_changed(), caps.iter().count())),
            Ok((false, 8))
        );
        assert_eq!(
            CapsRef::parse("NkUk!").map(|caps| {
                (
                    caps.supports_changed(),
                    caps.iter()
                        .map(|entry| (entry.request(), entry.response()))
                        .collect::<Vec<_>>(),
                )
            }),
            Ok((
                true,
                vec![
                    (Opcode::OBSERVE, Opcode::OK),
                    (Opcode::UNOBSERVE, Opcode::OK)
                ],
            ))
        );
    }

    #[test]
    fn rejects_mismatched_known_pair() {
        assert_eq!(CapsRef::parse("PkPg"), Err(PayloadError::MalformedCaps));
    }

    #[test]
    fn rejects_non_terminal_changed_marker() {
        assert_eq!(CapsRef::parse("!Pk"), Err(PayloadError::MalformedCaps));
        assert_eq!(CapsRef::parse("Pk!Uk"), Err(PayloadError::MalformedCaps));
    }

    #[test]
    fn accepts_unknown_pair_as_a_unit() {
        assert_eq!(
            CapsRef::parse("Zz").map(|caps| caps.iter().next().map(|entry| entry.is_known())),
            Ok(Some(false))
        );
    }

    #[test]
    fn rejects_duplicate_request_with_different_response() {
        assert_eq!(CapsRef::parse("PkPw"), Err(PayloadError::MalformedCaps));
    }
}
