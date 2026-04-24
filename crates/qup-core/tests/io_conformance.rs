//! Integration tests for sync and async transport-facing frame helpers.

#[cfg(test)]
mod tests {
    use core::future::ready;

    use qup_core::io::asynch::{
        AsyncByteRead, AsyncByteWrite, AsyncReadFrameError, read_frame as read_frame_async,
    };
    use qup_core::io::sync::{ByteRead, ByteWrite, ReadFrameError, read_frame as read_frame_sync};
    use qup_core::{FRAME_OVERHEAD, FrameError, Opcode, WireDirection, compute_checksum};

    /// Builds a valid frame for the supplied opcode and payload.
    fn build_frame(opcode: Opcode, payload: &[u8]) -> Vec<u8> {
        let payload_len = ok(u16::try_from(payload.len()));
        let capacity = ok(payload
            .len()
            .checked_add(FRAME_OVERHEAD)
            .ok_or("frame capacity overflow"));

        let mut frame = Vec::with_capacity(capacity);
        frame.push(opcode.as_u8());
        frame.extend_from_slice(&payload_len.to_be_bytes());
        frame.extend_from_slice(payload);
        frame.push(compute_checksum(opcode, payload));
        frame
    }

    /// Extracts a success value from a result.
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

    /// Transport errors injected by the test doubles.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TransportError {
        Injected,
        UnexpectedEof,
    }

    /// A bounded fragmented reader used to exercise the public IO traits.
    struct TestReader<'bytes> {
        bytes: &'bytes [u8],
        cursor: usize,
        max_chunk: usize,
        fail_at: Option<usize>,
    }

    impl<'bytes> TestReader<'bytes> {
        /// Creates a test reader over a fixed byte slice.
        const fn new(bytes: &'bytes [u8], max_chunk: usize, fail_at: Option<usize>) -> Self {
            Self {
                bytes,
                cursor: 0,
                max_chunk,
                fail_at,
            }
        }

        /// Copies exactly enough bytes into `buf` or returns an injected transport error.
        fn copy_exact(&mut self, buf: &mut [u8]) -> Result<(), TransportError> {
            let mut written = 0usize;
            while written < buf.len() {
                if self.fail_at.is_some_and(|limit| self.cursor >= limit) {
                    return Err(TransportError::Injected);
                }

                if self.cursor >= self.bytes.len() {
                    return Err(TransportError::UnexpectedEof);
                }

                let available = self.bytes.len().saturating_sub(self.cursor);
                let remaining = buf.len().saturating_sub(written);
                let to_copy = available.min(self.max_chunk).min(remaining);
                let next_written = written.saturating_add(to_copy);
                let next_cursor = self.cursor.saturating_add(to_copy);

                let Some(target) = buf.get_mut(written..next_written) else {
                    return Err(TransportError::UnexpectedEof);
                };
                let Some(source) = self.bytes.get(self.cursor..next_cursor) else {
                    return Err(TransportError::UnexpectedEof);
                };

                target.copy_from_slice(source);
                self.cursor = next_cursor;
                written = next_written;
            }

            Ok(())
        }
    }

    impl ByteRead for TestReader<'_> {
        type Error = TransportError;

        fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Self::Error> {
            self.copy_exact(buf)
        }
    }

    impl AsyncByteRead for TestReader<'_> {
        type Error = TransportError;
        fn read_exact(
            &mut self,
            buf: &mut [u8],
        ) -> impl Future<Output = Result<(), Self::Error>> {
            ready(self.copy_exact(buf))
        }
    }

    /// A collecting writer used to exercise the public IO traits.
    #[derive(Default)]
    struct TestWriter {
        written: Vec<u8>,
    }

    impl ByteWrite for TestWriter {
        type Error = TransportError;

        fn write_all(&mut self, buf: &[u8]) -> Result<(), Self::Error> {
            self.written.extend_from_slice(buf);
            Ok(())
        }
    }

    impl AsyncByteWrite for TestWriter {
        type Error = TransportError;
        fn write_all(&mut self, buf: &[u8]) -> impl Future<Output = Result<(), Self::Error>> {
            self.written.extend_from_slice(buf);
            ready(Ok(()))
        }
    }

    #[test]
    fn sync_read_frame_reads_zero_length_payload_frame() {
        let frame = build_frame(Opcode::GETCAPS, &[]);
        let mut reader = TestReader::new(&frame, frame.len(), None);
        let mut payload = [];
        let parsed = ok(read_frame_sync(
            &mut reader,
            WireDirection::ClientToNode,
            &mut payload,
        ));

        assert_eq!(parsed.opcode(), Opcode::GETCAPS);
        assert_eq!(parsed.payload(), &[]);
    }

    #[test]
    fn sync_read_frame_accepts_exact_buffer_size() {
        let frame = build_frame(Opcode::GETKEY, &[0x12, 0x34]);
        let mut reader = TestReader::new(&frame, 1, None);
        let mut payload = [0u8; 2];
        let parsed = ok(read_frame_sync(
            &mut reader,
            WireDirection::ClientToNode,
            &mut payload,
        ));

        assert_eq!(parsed.payload(), &[0x12, 0x34]);
    }

    #[test]
    fn sync_read_frame_propagates_header_transport_error() {
        let frame = build_frame(Opcode::GETCAPS, &[]);
        let mut reader = TestReader::new(&frame, frame.len(), Some(0));
        let mut payload = [];

        assert_eq!(
            err(read_frame_sync(
                &mut reader,
                WireDirection::ClientToNode,
                &mut payload,
            )),
            ReadFrameError::Transport(TransportError::Injected)
        );
    }

    #[test]
    fn sync_read_frame_propagates_payload_transport_error() {
        let frame = build_frame(Opcode::GETKEY, &[0x00, 0x01]);
        let mut reader = TestReader::new(&frame, frame.len(), Some(3));
        let mut payload = [0u8; 2];

        assert_eq!(
            err(read_frame_sync(
                &mut reader,
                WireDirection::ClientToNode,
                &mut payload,
            )),
            ReadFrameError::Transport(TransportError::Injected)
        );
    }

    #[test]
    fn sync_read_frame_propagates_checksum_transport_error() {
        let frame = build_frame(Opcode::GETKEY, &[0x00, 0x01]);
        let mut reader = TestReader::new(&frame, frame.len(), Some(5));
        let mut payload = [0u8; 2];

        assert_eq!(
            err(read_frame_sync(
                &mut reader,
                WireDirection::ClientToNode,
                &mut payload,
            )),
            ReadFrameError::Transport(TransportError::Injected)
        );
    }

    #[test]
    fn sync_read_frame_propagates_unexpected_eof() {
        let truncated = [0x53, 0x00, 0x02, 0x00];
        let mut reader = TestReader::new(&truncated, truncated.len(), None);
        let mut payload = [0u8; 2];

        assert_eq!(
            err(read_frame_sync(
                &mut reader,
                WireDirection::ClientToNode,
                &mut payload,
            )),
            ReadFrameError::Transport(TransportError::UnexpectedEof)
        );
    }

    #[test]
    fn sync_read_frame_rejects_unknown_opcode_and_wrong_direction() {
        let unknown = build_frame(Opcode::new(b'Z'), &[]);
        let mut reader = TestReader::new(&unknown, unknown.len(), None);
        let mut payload = [];
        assert!(matches!(
            read_frame_sync(&mut reader, WireDirection::ClientToNode, &mut payload),
            Err(ReadFrameError::Frame(FrameError::UnknownOpcode(opcode))) if opcode == Opcode::new(b'Z')
        ));

        let response = build_frame(Opcode::OK, &[]);
        let mut reader = TestReader::new(&response, response.len(), None);
        let mut payload = [];
        assert!(matches!(
            read_frame_sync(&mut reader, WireDirection::ClientToNode, &mut payload),
            Err(ReadFrameError::Frame(FrameError::InvalidDirection { opcode, .. })) if opcode == Opcode::OK
        ));
    }

    #[test]
    fn sync_byte_write_trait_is_usable() {
        let mut writer = TestWriter::default();
        ok(ByteWrite::write_all(&mut writer, &[0x10, 0x20, 0x30]));
        ok(ByteWrite::write_all(&mut writer, &[0x40]));
        assert_eq!(writer.written, vec![0x10, 0x20, 0x30, 0x40]);
    }

    #[tokio::test]
    async fn async_read_frame_reads_zero_length_payload_frame() {
        let frame = build_frame(Opcode::GETCAPS, &[]);
        let mut reader = TestReader::new(&frame, frame.len(), None);
        let mut payload = [];
        let parsed =
            ok(read_frame_async(&mut reader, WireDirection::ClientToNode, &mut payload).await);

        assert_eq!(parsed.opcode(), Opcode::GETCAPS);
        assert_eq!(parsed.payload(), &[]);
    }

    #[tokio::test]
    async fn async_read_frame_accepts_exact_buffer_size() {
        let frame = build_frame(Opcode::GETKEY, &[0xab, 0xcd]);
        let mut reader = TestReader::new(&frame, 1, None);
        let mut payload = [0u8; 2];
        let parsed =
            ok(read_frame_async(&mut reader, WireDirection::ClientToNode, &mut payload).await);

        assert_eq!(parsed.payload(), &[0xab, 0xcd]);
    }

    #[tokio::test]
    async fn async_read_frame_propagates_header_transport_error() {
        let frame = build_frame(Opcode::GETCAPS, &[]);
        let mut reader = TestReader::new(&frame, frame.len(), Some(0));
        let mut payload = [];

        assert_eq!(
            err(read_frame_async(&mut reader, WireDirection::ClientToNode, &mut payload).await),
            AsyncReadFrameError::Transport(TransportError::Injected)
        );
    }

    #[tokio::test]
    async fn async_read_frame_propagates_payload_transport_error() {
        let frame = build_frame(Opcode::GETKEY, &[0x00, 0x01]);
        let mut reader = TestReader::new(&frame, frame.len(), Some(3));
        let mut payload = [0u8; 2];

        assert_eq!(
            err(read_frame_async(&mut reader, WireDirection::ClientToNode, &mut payload).await),
            AsyncReadFrameError::Transport(TransportError::Injected)
        );
    }

    #[tokio::test]
    async fn async_read_frame_propagates_checksum_transport_error() {
        let frame = build_frame(Opcode::GETKEY, &[0x00, 0x01]);
        let mut reader = TestReader::new(&frame, frame.len(), Some(5));
        let mut payload = [0u8; 2];

        assert_eq!(
            err(read_frame_async(&mut reader, WireDirection::ClientToNode, &mut payload).await),
            AsyncReadFrameError::Transport(TransportError::Injected)
        );
    }

    #[tokio::test]
    async fn async_read_frame_propagates_unexpected_eof() {
        let truncated = [0x53, 0x00, 0x02, 0x00];
        let mut reader = TestReader::new(&truncated, truncated.len(), None);
        let mut payload = [0u8; 2];

        assert_eq!(
            err(read_frame_async(&mut reader, WireDirection::ClientToNode, &mut payload).await),
            AsyncReadFrameError::Transport(TransportError::UnexpectedEof)
        );
    }

    #[tokio::test]
    async fn async_read_frame_rejects_unknown_opcode_and_wrong_direction() {
        let unknown = build_frame(Opcode::new(b'Z'), &[]);
        let mut reader = TestReader::new(&unknown, unknown.len(), None);
        let mut payload = [];
        assert!(matches!(
            read_frame_async(&mut reader, WireDirection::ClientToNode, &mut payload).await,
            Err(AsyncReadFrameError::Frame(FrameError::UnknownOpcode(opcode))) if opcode == Opcode::new(b'Z')
        ));

        let response = build_frame(Opcode::OK, &[]);
        let mut reader = TestReader::new(&response, response.len(), None);
        let mut payload = [];
        assert!(matches!(
            read_frame_async(&mut reader, WireDirection::ClientToNode, &mut payload).await,
            Err(AsyncReadFrameError::Frame(FrameError::InvalidDirection { opcode, .. })) if opcode == Opcode::OK
        ));
    }

    #[tokio::test]
    async fn async_byte_write_trait_is_usable() {
        let mut writer = TestWriter::default();
        ok(AsyncByteWrite::write_all(&mut writer, &[0x10, 0x20, 0x30]).await);
        ok(AsyncByteWrite::write_all(&mut writer, &[0x40]).await);
        assert_eq!(writer.written, vec![0x10, 0x20, 0x30, 0x40]);
    }
}
