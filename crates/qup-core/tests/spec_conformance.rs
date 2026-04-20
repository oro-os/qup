//! Spec-driven integration tests for QUP core framing and decoding.

#[cfg(test)]
mod tests {
    use qup_core::{
        CapsRef, FRAME_OVERHEAD, FrameError, KeyFlags, MessageRef, Opcode, OrdinaryResponseRef,
        Parser, PayloadCursor, PayloadError, RequestRef, ValueKind, ValueRef, WireDirection,
        compute_checksum, frame_sum,
    };

    /// Builds a frame with a valid checksum for the supplied opcode and payload.
    #[expect(
        clippy::arithmetic_side_effects,
        reason = "the helper computes exact frame capacity from a bounded test payload"
    )]
    fn build_frame(opcode: Opcode, payload: &[u8]) -> Vec<u8> {
        let payload_len = ok(u16::try_from(payload.len()));
        let mut frame = Vec::with_capacity(payload.len() + FRAME_OVERHEAD);
        frame.push(opcode.as_u8());
        frame.extend_from_slice(&payload_len.to_be_bytes());
        frame.extend_from_slice(payload);
        frame.push(compute_checksum(opcode, payload));
        frame
    }

    /// Extracts a successful value from a result.
    ///
    /// # Panics
    ///
    /// Panics when the result is `Err` because the surrounding test expected success.
    #[expect(
        clippy::panic,
        reason = "integration test helpers fail immediately when a success path regresses"
    )]
    fn ok<T, E: core::fmt::Debug>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("expected Ok(..), got Err({error:?})"),
        }
    }

    /// Extracts an error value from a result.
    ///
    /// # Panics
    ///
    /// Panics when the result is `Ok` because the surrounding test expected failure.
    #[expect(
        clippy::panic,
        reason = "integration test helpers fail immediately when an error path stops failing"
    )]
    fn err<T, E: core::fmt::Debug>(result: Result<T, E>) -> E {
        match result {
            Ok(_) => panic!("expected Err(..), got Ok(..)"),
            Err(error) => error,
        }
    }

    /// Returns a validated `CapsRef` from a decoded message.
    ///
    /// # Panics
    ///
    /// Panics when the message is not a compatibility response.
    #[expect(
        clippy::panic,
        reason = "integration tests need explicit unexpected-variant failures"
    )]
    fn expect_caps(message: MessageRef<'_>) -> CapsRef<'_> {
        match message {
            MessageRef::CompatibilityResponse { caps } => caps,
            other @ (MessageRef::Request(_)
            | MessageRef::OrdinaryResponse(_)
            | MessageRef::CompatibilityRequest
            | MessageRef::Error(_)
            | MessageRef::Changed { .. }) => {
                panic!("expected compatibility response, got {other:?}")
            }
        }
    }

    /// Returns a decoded request-level error response from a decoded message.
    ///
    /// # Panics
    ///
    /// Panics when the message is not an error response.
    #[expect(
        clippy::panic,
        reason = "integration tests need explicit unexpected-variant failures"
    )]
    fn expect_error(message: MessageRef<'_>) -> qup_core::ErrorResponse {
        match message {
            MessageRef::Error(error) => error,
            other @ (MessageRef::Request(_)
            | MessageRef::OrdinaryResponse(_)
            | MessageRef::CompatibilityRequest
            | MessageRef::CompatibilityResponse { .. }
            | MessageRef::Changed { .. }) => {
                panic!("expected error response, got {other:?}")
            }
        }
    }

    /// Creates validated key flags for equality assertions.
    fn key_flags(bits: u8) -> KeyFlags {
        ok(KeyFlags::new(bits))
    }

    #[test]
    fn frame_overhead_matches_wire_layout() {
        assert_eq!(FRAME_OVERHEAD, 4);
    }

    #[test]
    fn documented_examples_have_zero_wrapping_sum() {
        let examples: &[&[u8]] = &[
            &[0x3f, 0x00, 0x00, 0xc1],
            &[0x3a, 0x00, 0x02, 0x00, 0x00, 0xc4],
            &[
                0x3a, 0x00, 0x07, 0x00, 0x05, 0x4e, 0x6b, 0x55, 0x6b, 0x21, 0x20,
            ],
            &[0x50, 0x00, 0x00, 0xb0],
            &[0x6b, 0x00, 0x00, 0x95],
            &[0x53, 0x00, 0x02, 0x00, 0x01, 0xaa],
            &[0x73, 0x00, 0x06, 0x07, 0x00, 0x03, 0x6c, 0x65, 0x64, 0x48],
            &[0x40, 0x00, 0x01, 0x02, 0xbd],
            &[0x21, 0x00, 0x02, 0x00, 0x01, 0xdc],
        ];

        for example in examples {
            assert_eq!(frame_sum(example), 0);
        }
    }

    #[test]
    fn documented_examples_parse_and_decode() {
        let parser = Parser::new();

        let frame = ok(parser.parse_frame(WireDirection::ClientToNode, &[0x3f, 0x00, 0x00, 0xc1]));
        assert_eq!(ok(frame.decode_message()), MessageRef::CompatibilityRequest);

        let frame = ok(parser.parse_frame(
            WireDirection::NodeToClient,
            &[0x3a, 0x00, 0x02, 0x00, 0x00, 0xc4],
        ));
        let caps = expect_caps(ok(frame.decode_message()));
        assert_eq!(caps.as_str(), "");
        assert!(!caps.supports_changed());

        let frame = ok(parser.parse_frame(
            WireDirection::NodeToClient,
            &[
                0x3a, 0x00, 0x07, 0x00, 0x05, 0x4e, 0x6b, 0x55, 0x6b, 0x21, 0x20,
            ],
        ));
        let caps = expect_caps(ok(frame.decode_message()));
        assert_eq!(caps.as_str(), "NkUk!");
        assert!(caps.supports_changed());

        let frame = ok(parser.parse_frame(WireDirection::ClientToNode, &[0x50, 0x00, 0x00, 0xb0]));
        assert_eq!(
            ok(frame.decode_message()),
            MessageRef::Request(RequestRef::Ping)
        );

        let frame = ok(parser.parse_frame(WireDirection::NodeToClient, &[0x6b, 0x00, 0x00, 0x95]));
        assert_eq!(
            ok(frame.decode_message()),
            MessageRef::OrdinaryResponse(OrdinaryResponseRef::Ok)
        );

        let frame = ok(parser.parse_frame(
            WireDirection::ClientToNode,
            &[0x53, 0x00, 0x02, 0x00, 0x01, 0xaa],
        ));
        assert_eq!(
            ok(frame.decode_message()),
            MessageRef::Request(RequestRef::GetKey { keyref: 1 })
        );

        let frame = ok(parser.parse_frame(
            WireDirection::NodeToClient,
            &[0x73, 0x00, 0x06, 0x07, 0x00, 0x03, 0x6c, 0x65, 0x64, 0x48],
        ));
        assert_eq!(
            ok(frame.decode_message()),
            MessageRef::OrdinaryResponse(OrdinaryResponseRef::Key {
                keyflags: key_flags(0x07),
                name: "led",
            })
        );

        let frame =
            ok(parser.parse_frame(WireDirection::NodeToClient, &[0x40, 0x00, 0x01, 0x02, 0xbd]));
        assert_eq!(expect_error(ok(frame.decode_message())).code(), 0x02);

        let frame = ok(parser.parse_frame(
            WireDirection::NodeToClient,
            &[0x21, 0x00, 0x02, 0x00, 0x01, 0xdc],
        ));
        assert_eq!(
            ok(frame.decode_message()),
            MessageRef::Changed { keyref: 1 }
        );
    }

    #[test]
    fn frame_parser_rejects_every_truncated_prefix() {
        let frame = [0x3f, 0x00, 0x00, 0xc1];
        let parser = Parser::new();

        for prefix_len in 0..FRAME_OVERHEAD {
            let prefix = frame.get(..prefix_len).unwrap_or(&[]);
            assert_eq!(
                err(parser.parse_frame(WireDirection::ClientToNode, prefix)),
                FrameError::Truncated
            );
        }
    }

    #[test]
    fn frame_parser_accepts_maximum_payload_length() {
        let payload = vec![0x5a; usize::from(u16::MAX)];
        let frame = build_frame(Opcode::GET, &payload);
        let parsed = ok(Parser::new().parse_frame(WireDirection::ClientToNode, &frame));

        assert_eq!(parsed.opcode(), Opcode::GET);
        assert_eq!(parsed.payload_len(), u16::MAX);
        assert_eq!(parsed.frame_len(), payload.len() + FRAME_OVERHEAD);
        assert_eq!(parsed.payload().len(), payload.len());
        assert!(parsed.payload().iter().all(|byte| *byte == 0x5a));
    }

    #[test]
    fn frame_parser_rejects_unknown_and_reserved_opcodes() {
        let unknown = build_frame(Opcode::new(b'Z'), &[]);
        assert!(matches!(
            Parser::new().parse_frame(WireDirection::ClientToNode, &unknown),
            Err(FrameError::UnknownOpcode(opcode)) if opcode == Opcode::new(b'Z')
        ));

        let reserved = build_frame(Opcode::new(0x00), &[]);
        assert!(matches!(
            Parser::new().parse_frame(WireDirection::ClientToNode, &reserved),
            Err(FrameError::ReservedOpcode(opcode)) if opcode == Opcode::new(0x00)
        ));
    }

    #[test]
    fn caps_accept_empty_changed_only_and_duplicate_identical_pairs() {
        let caps = ok(CapsRef::parse(""));
        assert_eq!(caps.as_str(), "");
        assert!(!caps.supports_changed());
        assert_eq!(caps.iter().count(), 0);

        let caps = ok(CapsRef::parse("!"));
        assert_eq!(caps.as_str(), "!");
        assert!(caps.supports_changed());
        assert_eq!(caps.iter().count(), 0);

        let caps = ok(CapsRef::parse("PkPk"));
        let pairs: Vec<_> = caps
            .iter()
            .map(|entry| (entry.request(), entry.response()))
            .collect();
        assert_eq!(
            pairs,
            vec![(Opcode::PING, Opcode::OK), (Opcode::PING, Opcode::OK)]
        );
    }

    #[test]
    fn caps_reject_forbidden_bytes_and_malformed_shapes() {
        for malformed in ["P", "P?", "P:", "P@", "Pk!!", "aA", "\u{00e9}", "ZzZy"] {
            assert_eq!(err(CapsRef::parse(malformed)), PayloadError::MalformedCaps);
        }
    }

    #[test]
    fn payload_cursor_handles_empty_and_multibyte_strings() {
        let mut cursor = PayloadCursor::new(&[0x00, 0x00]);
        assert_eq!(ok(cursor.read_str16()), "");
        ok(cursor.finish());

        let mut cursor = PayloadCursor::new(&[0x00, 0x02, 0xc3, 0xa9]);
        assert_eq!(ok(cursor.read_str16()), "\u{00e9}");
        ok(cursor.finish());
    }

    #[test]
    fn payload_cursor_reads_bytes16_and_signed_i64() {
        let mut cursor = PayloadCursor::new(&[
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, 0x00, 0x03, 0x10, 0x20, 0x30,
        ]);

        assert_eq!(ok(cursor.read_i64()), -2);
        assert_eq!(ok(cursor.read_bytes16()), &[0x10, 0x20, 0x30]);
        ok(cursor.finish());
    }

    #[test]
    fn payload_cursor_reports_invalid_utf8_and_trailing_bytes() {
        let mut cursor = PayloadCursor::new(&[0x00, 0x02, 0xc3, 0x28]);
        assert!(matches!(
            cursor.read_str16(),
            Err(PayloadError::InvalidUtf8(_))
        ));

        let mut cursor = PayloadCursor::new(&[0x12, 0x34, 0xaa]);
        assert_eq!(ok(cursor.read_u16()), 0x1234);
        assert_eq!(
            err(cursor.finish()),
            PayloadError::TrailingBytes { remaining: 1 }
        );
    }

    #[test]
    fn value_decoder_accepts_empty_and_multibyte_strings() {
        let mut cursor = PayloadCursor::new(&[ValueKind::STR_TAG, 0x00, 0x00]);
        assert_eq!(ok(ValueRef::decode(&mut cursor)), ValueRef::Str(""));
        ok(cursor.finish());

        let mut cursor = PayloadCursor::new(&[ValueKind::STR_TAG, 0x00, 0x02, 0xc3, 0xa9]);
        let value = ok(ValueRef::decode(&mut cursor));
        assert_eq!(value, ValueRef::Str("\u{00e9}"));
        assert_eq!(value.kind(), ValueKind::Str);
        ok(cursor.finish());
    }

    #[test]
    fn value_decoder_rejects_invalid_bool_and_truncated_bodies() {
        let mut cursor = PayloadCursor::new(&[ValueKind::BOOL_TAG, 0x02]);
        assert_eq!(
            err(ValueRef::decode(&mut cursor)),
            PayloadError::InvalidBool(0x02)
        );

        let mut cursor = PayloadCursor::new(&[ValueKind::BOOL_TAG]);
        assert_eq!(
            err(ValueRef::decode(&mut cursor)),
            PayloadError::InternalLengthExceedsPayload
        );

        let mut cursor =
            PayloadCursor::new(&[ValueKind::I64_TAG, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(
            err(ValueRef::decode(&mut cursor)),
            PayloadError::InternalLengthExceedsPayload
        );
    }

    #[test]
    fn keyflags_zero_bits_are_valid() {
        let flags = key_flags(0x00);
        assert_eq!(flags.bits(), 0x00);
        assert!(!flags.is_readable());
        assert!(!flags.is_writable());
        assert!(!flags.is_observable());
    }

    #[test]
    fn opcode_metadata_matches_spec_pairs_and_directions() {
        assert_eq!(Opcode::PING.expected_ordinary_response(), Some(Opcode::OK));
        assert_eq!(
            Opcode::IDENTIFY.expected_ordinary_response(),
            Some(Opcode::IDENTIFIED)
        );
        assert_eq!(
            Opcode::GETKEYTABLEN.expected_ordinary_response(),
            Some(Opcode::KEYTABLEN)
        );
        assert_eq!(
            Opcode::GETKEY.expected_ordinary_response(),
            Some(Opcode::KEY)
        );
        assert_eq!(
            Opcode::GET.expected_ordinary_response(),
            Some(Opcode::VALUE)
        );
        assert_eq!(
            Opcode::WRITE.expected_ordinary_response(),
            Some(Opcode::WRITTEN)
        );
        assert_eq!(
            Opcode::OBSERVE.expected_ordinary_response(),
            Some(Opcode::OK)
        );
        assert_eq!(
            Opcode::UNOBSERVE.expected_ordinary_response(),
            Some(Opcode::OK)
        );
        assert_eq!(Opcode::GETCAPS.expected_ordinary_response(), None);
        assert_eq!(Opcode::OK.expected_ordinary_response(), None);

        assert_eq!(
            Opcode::GET.required_direction(),
            Some(WireDirection::ClientToNode)
        );
        assert_eq!(
            Opcode::GETCAPS.required_direction(),
            Some(WireDirection::ClientToNode)
        );
        assert_eq!(
            Opcode::VALUE.required_direction(),
            Some(WireDirection::NodeToClient)
        );
        assert_eq!(
            Opcode::CAPS.required_direction(),
            Some(WireDirection::NodeToClient)
        );
        assert_eq!(Opcode::new(0x00).required_direction(), None);
    }

    #[test]
    fn message_decoder_covers_all_request_variants() {
        assert_eq!(
            ok(MessageRef::decode(Opcode::PING, &[])),
            MessageRef::Request(RequestRef::Ping)
        );
        assert_eq!(
            ok(MessageRef::decode(Opcode::IDENTIFY, &[])),
            MessageRef::Request(RequestRef::Identify)
        );
        assert_eq!(
            ok(MessageRef::decode(Opcode::GETKEYTABLEN, &[])),
            MessageRef::Request(RequestRef::GetKeytabLen)
        );
        assert_eq!(
            ok(MessageRef::decode(Opcode::GETKEY, &[0x12, 0x34])),
            MessageRef::Request(RequestRef::GetKey { keyref: 0x1234 })
        );
        assert_eq!(
            ok(MessageRef::decode(Opcode::GET, &[0xab, 0xcd])),
            MessageRef::Request(RequestRef::Get { keyref: 0xabcd })
        );
        assert_eq!(
            ok(MessageRef::decode(
                Opcode::WRITE,
                &[0x00, 0x02, ValueKind::BOOL_TAG, 0x00],
            )),
            MessageRef::Request(RequestRef::Write {
                keyref: 2,
                value: ValueRef::Bool(false),
            })
        );
        assert_eq!(
            ok(MessageRef::decode(Opcode::OBSERVE, &[0x00, 0x09])),
            MessageRef::Request(RequestRef::Observe { keyref: 9 })
        );
        assert_eq!(
            ok(MessageRef::decode(Opcode::UNOBSERVE, &[0x00, 0x09])),
            MessageRef::Request(RequestRef::Unobserve { keyref: 9 })
        );
        assert_eq!(
            ok(MessageRef::decode(Opcode::GETCAPS, &[])),
            MessageRef::CompatibilityRequest
        );
    }

    #[test]
    fn message_decoder_covers_all_response_variants() {
        assert_eq!(
            ok(MessageRef::decode(Opcode::OK, &[])),
            MessageRef::OrdinaryResponse(OrdinaryResponseRef::Ok)
        );
        assert_eq!(
            ok(MessageRef::decode(Opcode::IDENTIFIED, &[0x00, 0x00])),
            MessageRef::OrdinaryResponse(OrdinaryResponseRef::Identified { nodeid: "" })
        );
        assert_eq!(
            ok(MessageRef::decode(Opcode::KEYTABLEN, &[0x12, 0x34])),
            MessageRef::OrdinaryResponse(OrdinaryResponseRef::KeytabLen { count: 0x1234 })
        );
        assert_eq!(
            ok(MessageRef::decode(Opcode::KEY, &[0x00, 0x00, 0x00])),
            MessageRef::OrdinaryResponse(OrdinaryResponseRef::Key {
                keyflags: key_flags(0x00),
                name: "",
            })
        );
        assert_eq!(
            ok(MessageRef::decode(
                Opcode::VALUE,
                &[
                    ValueKind::I64_TAG,
                    0xff,
                    0xff,
                    0xff,
                    0xff,
                    0xff,
                    0xff,
                    0xff,
                    0xfe,
                ],
            )),
            MessageRef::OrdinaryResponse(OrdinaryResponseRef::Value {
                value: ValueRef::I64(-2),
            })
        );
        assert_eq!(
            ok(MessageRef::decode(
                Opcode::WRITTEN,
                &[ValueKind::STR_TAG, 0x00, 0x00],
            )),
            MessageRef::OrdinaryResponse(OrdinaryResponseRef::Written {
                value: ValueRef::Str(""),
            })
        );

        let caps = expect_caps(ok(MessageRef::decode(Opcode::CAPS, &[0x00, 0x01, b'!'])));
        assert_eq!(caps.as_str(), "!");
        assert!(caps.supports_changed());

        assert_eq!(
            expect_error(ok(MessageRef::decode(Opcode::ERROR, &[0xff]))).code(),
            0xff
        );

        assert_eq!(
            ok(MessageRef::decode(Opcode::CHANGED, &[0x00, 0x00])),
            MessageRef::Changed { keyref: 0 }
        );
    }

    #[test]
    fn message_decoder_rejects_fixed_length_and_semantic_malformed_payloads() {
        assert_eq!(
            err(MessageRef::decode(Opcode::PING, &[0x00])),
            PayloadError::InvalidLength {
                opcode: Opcode::PING,
                expected: 0,
                actual: 1,
            }
        );
        assert_eq!(
            err(MessageRef::decode(Opcode::GET, &[0x00])),
            PayloadError::InvalidLength {
                opcode: Opcode::GET,
                expected: 2,
                actual: 1,
            }
        );
        assert_eq!(
            err(MessageRef::decode(Opcode::KEYTABLEN, &[0x00])),
            PayloadError::InvalidLength {
                opcode: Opcode::KEYTABLEN,
                expected: 2,
                actual: 1,
            }
        );
        assert_eq!(
            err(MessageRef::decode(Opcode::ERROR, &[])),
            PayloadError::InvalidLength {
                opcode: Opcode::ERROR,
                expected: 1,
                actual: 0,
            }
        );
        assert_eq!(
            err(MessageRef::decode(Opcode::KEY, &[0x08, 0x00, 0x00])),
            PayloadError::InvalidKeyFlags(0x08)
        );
        assert_eq!(
            err(MessageRef::decode(
                Opcode::VALUE,
                &[ValueKind::BOOL_TAG, 0x01, 0xff],
            )),
            PayloadError::TrailingBytes { remaining: 1 }
        );
        assert_eq!(
            err(MessageRef::decode(Opcode::CAPS, &[0x00, 0x02, b'P', b'?'])),
            PayloadError::MalformedCaps
        );
        assert_eq!(
            err(MessageRef::decode(Opcode::IDENTIFIED, &[0x00, 0x00, 0xff])),
            PayloadError::TrailingBytes { remaining: 1 }
        );
        assert_eq!(
            err(MessageRef::decode(
                Opcode::KEY,
                &[0x01, 0x00, 0x03, b'l', b'e'],
            )),
            PayloadError::InternalLengthExceedsPayload
        );
    }
}
