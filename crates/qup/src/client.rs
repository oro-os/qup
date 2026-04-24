use std::{
    error::Error,
    fmt,
    io::{self, ErrorKind},
    time::Duration,
};

use qup_core::{
    KeyFlags, MessageRef, Opcode, OrdinaryResponseRef, Parser as FrameParser, ValueKind,
    ValueRef, WireDirection, compute_checksum,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    net::{TcpStream, ToSocketAddrs},
    time,
};

/// Convenience result alias for the host-side client.
pub type Result<T> = std::result::Result<T, ClientError>;

/// Tokio TCP client alias.
pub type TcpClient = Client<TcpStream>;

type FrameTrace = dyn FnMut(FrameDirection, &[u8]) + Send;

/// Direction for traced full frame bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDirection {
    /// A frame transmitted by the client.
    Tx,
    /// A frame received from the node.
    Rx,
}

/// Owned decoded QUP value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A `bool` value.
    Bool(bool),
    /// An `i64` value.
    I64(i64),
    /// An owned string value.
    Str(String),
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::I64(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}

/// Owned decoded QUP message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// `OK`.
    Ok,
    /// `CAPS`.
    Caps(String),
    /// `IDENTIFIED`.
    Identified(String),
    /// `KEYTABLEN`.
    KeytabLen(u16),
    /// `KEY`.
    Key {
        /// The key flags for the returned key.
        keyflags: KeyFlags,
        /// The returned key name.
        name: String,
    },
    /// `VALUE`.
    Value(Value),
    /// `WRITTEN`.
    Written(Value),
    /// `ERROR`.
    Error(u8),
    /// `CHANGED`.
    Changed(u16),
}

/// Owned key table entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// The key reference returned by the node.
    pub keyref: u16,
    /// The key flags returned by `GETKEY`.
    pub keyflags: KeyFlags,
    /// The key name returned by `GETKEY`.
    pub name: String,
}

/// Errors returned by the host-side client.
#[derive(Debug)]
pub enum ClientError {
    /// Underlying transport failure.
    Io(io::Error),
    /// The peer violated framing or payload rules.
    Protocol(String),
    /// A request returned a request-level error response.
    RequestError {
        /// The request that failed.
        request: Opcode,
        /// The returned request-specific error code.
        code: u8,
    },
    /// A response opcode was valid but not the one expected for the operation.
    UnexpectedMessage {
        /// The expected response kind.
        expected: &'static str,
        /// The message that actually arrived.
        actual: Message,
    },
    /// No key with the requested name exists.
    KeyNotFound(String),
    /// Multiple keys with the requested name exist.
    AmbiguousKey(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "transport error: {error}"),
            Self::Protocol(message) => f.write_str(message),
            Self::RequestError { request, code } => {
                write!(f, "request {request} failed with error code 0x{code:02x}")
            }
            Self::UnexpectedMessage { expected, actual } => {
                write!(f, "expected {expected}, got {actual:?}")
            }
            Self::KeyNotFound(name) => write!(f, "no key named {name:?}"),
            Self::AmbiguousKey(name) => {
                write!(f, "multiple keys named {name:?}; use a numeric keyref")
            }
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Protocol(_)
            | Self::RequestError { .. }
            | Self::UnexpectedMessage { .. }
            | Self::KeyNotFound(_)
            | Self::AmbiguousKey(_) => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Reusable Tokio QUP client over any async byte stream.
pub struct Client<S> {
    stream: S,
    parser: FrameParser,
    frame_buf: Vec<u8>,
    payload_buf: Vec<u8>,
    frame_trace: Option<Box<FrameTrace>>,
}

impl<S> Client<S> {
    /// Creates a client around an existing Tokio async stream.
    #[must_use]
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            parser: FrameParser::new(),
            frame_buf: Vec::new(),
            payload_buf: Vec::new(),
            frame_trace: None,
        }
    }

    /// Returns a shared reference to the underlying stream.
    #[must_use]
    pub fn stream(&self) -> &S {
        &self.stream
    }

    /// Returns a mutable reference to the underlying stream.
    #[must_use]
    pub fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// Consumes the client and returns the underlying stream.
    #[must_use]
    pub fn into_inner(self) -> S {
        self.stream
    }

    /// Installs a callback that receives each complete transmitted and received frame.
    pub fn set_frame_trace<F>(&mut self, trace: F)
    where
        F: FnMut(FrameDirection, &[u8]) + Send + 'static,
    {
        self.frame_trace = Some(Box::new(trace));
    }

    /// Removes any installed frame trace callback.
    pub fn clear_frame_trace(&mut self) {
        self.frame_trace = None;
    }
}

impl Client<TcpStream> {
    /// Connects a Tokio TCP stream and wraps it in a QUP client.
    pub async fn connect<A>(addr: A) -> io::Result<Self>
    where
        A: ToSocketAddrs,
    {
        Ok(Self::new(TcpStream::connect(addr).await?))
    }
}

impl<S> Client<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Sends a raw request frame with the provided opcode and payload.
    pub async fn send_request(&mut self, opcode: Opcode, payload: &[u8]) -> io::Result<()> {
        self.send_frame(opcode, payload).await
    }

    /// Sends a request with an empty payload.
    pub async fn send_empty_request(&mut self, opcode: Opcode) -> io::Result<()> {
        self.send_frame(opcode, &[]).await
    }

    /// Sends a request whose payload is a single `keyref`.
    pub async fn send_keyref_request(&mut self, opcode: Opcode, keyref: u16) -> io::Result<()> {
        self.send_frame(opcode, &keyref.to_be_bytes()).await
    }

    /// Sends a `WRITE` request for the provided key and value.
    pub async fn send_write_request(&mut self, keyref: u16, value: &Value) -> io::Result<()> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&keyref.to_be_bytes());
        push_value(&mut payload, value)?;
        self.send_frame(Opcode::WRITE, payload.as_slice()).await
    }

    /// Reads the next decoded message from the stream.
    pub async fn next_message(&mut self) -> Result<Message> {
        let mut header = [0u8; 3];
        self.stream.read_exact(&mut header).await?;

        let payload_len = usize::from(u16::from_be_bytes([header[1], header[2]]));
        self.payload_buf.resize(payload_len, 0);
        self.stream.read_exact(self.payload_buf.as_mut_slice()).await?;

        let mut checksum = [0u8; 1];
        self.stream.read_exact(&mut checksum).await?;

        self.frame_buf.clear();
        self.frame_buf.extend_from_slice(&header);
        self.frame_buf.extend_from_slice(self.payload_buf.as_slice());
        self.frame_buf.extend_from_slice(&checksum);
        Self::trace_frame(&mut self.frame_trace, FrameDirection::Rx, self.frame_buf.as_slice());

        let frame = self
            .parser
            .parse_frame(WireDirection::NodeToClient, self.frame_buf.as_slice())
            .map_err(frame_error)?;
        let message = frame
            .decode_message()
            .map_err(|error| payload_error(&error))?;
        owned_message(message)
    }

    /// Reads the next decoded message with a Tokio timeout.
    pub async fn next_message_timeout(&mut self, duration: Duration) -> Result<Option<Message>> {
        match time::timeout(duration, self.next_message()).await {
            Ok(message) => message.map(Some),
            Err(_elapsed) => Ok(None),
        }
    }

    /// Sends `PING` and expects `OK`.
    pub async fn ping(&mut self) -> Result<()> {
        self.send_empty_request(Opcode::PING).await?;
        self.expect_ok(Opcode::PING).await
    }

    /// Sends `GETCAPS` and returns the capability string.
    pub async fn caps(&mut self) -> Result<String> {
        self.send_empty_request(Opcode::GETCAPS).await?;
        match self.read_request_response().await? {
            Message::Caps(caps) => Ok(caps),
            actual => Err(unexpected_message(Opcode::GETCAPS, "CAPS", actual)),
        }
    }

    /// Sends `IDENTIFY` and returns the node identifier.
    pub async fn identify(&mut self) -> Result<String> {
        self.send_empty_request(Opcode::IDENTIFY).await?;
        match self.read_request_response().await? {
            Message::Identified(node_id) => Ok(node_id),
            actual => Err(unexpected_message(Opcode::IDENTIFY, "IDENTIFIED", actual)),
        }
    }

    /// Sends `GETKEYTABLEN` and returns the reported key count.
    pub async fn key_count(&mut self) -> Result<u16> {
        self.send_empty_request(Opcode::GETKEYTABLEN).await?;
        match self.read_request_response().await? {
            Message::KeytabLen(count) => Ok(count),
            actual => Err(unexpected_message(Opcode::GETKEYTABLEN, "KEYTABLEN", actual)),
        }
    }

    /// Sends `GETKEY` for a specific key reference.
    pub async fn key(&mut self, keyref: u16) -> Result<KeyInfo> {
        self.send_keyref_request(Opcode::GETKEY, keyref).await?;
        match self.read_request_response().await? {
            Message::Key { keyflags, name } => Ok(KeyInfo {
                keyref,
                keyflags,
                name,
            }),
            actual => Err(unexpected_message(Opcode::GETKEY, "KEY", actual)),
        }
    }

    /// Fetches the full key table by iterating `GETKEY` across `GETKEYTABLEN`.
    pub async fn list_keys(&mut self) -> Result<Vec<KeyInfo>> {
        let count = self.key_count().await?;
        let mut keys = Vec::with_capacity(usize::from(count));
        for keyref in 0..count {
            keys.push(self.key(keyref).await?);
        }
        Ok(keys)
    }

    /// Resolves a key reference by exact key name.
    pub async fn resolve_keyref_by_name(&mut self, name: &str) -> Result<u16> {
        let mut matches = self
            .list_keys()
            .await?
            .into_iter()
            .filter(|key| key.name == name);

        let Some(first) = matches.next() else {
            return Err(ClientError::KeyNotFound(name.to_owned()));
        };

        if matches.next().is_some() {
            return Err(ClientError::AmbiguousKey(name.to_owned()));
        }

        Ok(first.keyref)
    }

    /// Resolves and returns key metadata by exact key name.
    pub async fn key_by_name(&mut self, name: &str) -> Result<KeyInfo> {
        let keyref = self.resolve_keyref_by_name(name).await?;
        self.key(keyref).await
    }

    /// Sends `GET` and returns the decoded current value.
    pub async fn get(&mut self, keyref: u16) -> Result<Value> {
        self.send_keyref_request(Opcode::GET, keyref).await?;
        match self.read_request_response().await? {
            Message::Value(value) => Ok(value),
            actual => Err(unexpected_message(Opcode::GET, "VALUE", actual)),
        }
    }

    /// Resolves a key by name and reads its current value.
    pub async fn get_by_name(&mut self, name: &str) -> Result<Value> {
        let keyref = self.resolve_keyref_by_name(name).await?;
        self.get(keyref).await
    }

    /// Sends `WRITE` and returns the `WRITTEN` value.
    pub async fn write(&mut self, keyref: u16, value: &Value) -> Result<Value> {
        self.send_write_request(keyref, value).await?;
        match self.read_request_response().await? {
            Message::Written(value) => Ok(value),
            actual => Err(unexpected_message(Opcode::WRITE, "WRITTEN", actual)),
        }
    }

    /// Resolves a key by name and writes a new value.
    pub async fn write_by_name(&mut self, name: &str, value: &Value) -> Result<Value> {
        let keyref = self.resolve_keyref_by_name(name).await?;
        self.write(keyref, value).await
    }

    /// Sends `OBSERVE` and expects `OK`.
    pub async fn observe(&mut self, keyref: u16) -> Result<()> {
        self.send_keyref_request(Opcode::OBSERVE, keyref).await?;
        self.expect_ok(Opcode::OBSERVE).await
    }

    /// Resolves a key by name and sends `OBSERVE`.
    pub async fn observe_by_name(&mut self, name: &str) -> Result<()> {
        let keyref = self.resolve_keyref_by_name(name).await?;
        self.observe(keyref).await
    }

    /// Sends `UNOBSERVE` and expects `OK`.
    pub async fn unobserve(&mut self, keyref: u16) -> Result<()> {
        self.send_keyref_request(Opcode::UNOBSERVE, keyref).await?;
        self.expect_ok(Opcode::UNOBSERVE).await
    }

    /// Resolves a key by name and sends `UNOBSERVE`.
    pub async fn unobserve_by_name(&mut self, name: &str) -> Result<()> {
        let keyref = self.resolve_keyref_by_name(name).await?;
        self.unobserve(keyref).await
    }

    async fn expect_ok(&mut self, request: Opcode) -> Result<()> {
        match self.read_request_response().await? {
            Message::Ok => Ok(()),
            actual => Err(unexpected_message(request, "OK", actual)),
        }
    }

    async fn send_frame(&mut self, opcode: Opcode, payload: &[u8]) -> io::Result<()> {
        let frame = build_frame(opcode, payload)?;
        Self::trace_frame(&mut self.frame_trace, FrameDirection::Tx, frame.as_slice());
        self.stream.write_all(frame.as_slice()).await
    }

    async fn read_request_response(&mut self) -> Result<Message> {
        loop {
            let message = self.next_message().await?;
            if matches!(message, Message::Changed(_)) {
                continue;
            }
            return Ok(message);
        }
    }

    fn trace_frame(
        frame_trace: &mut Option<Box<FrameTrace>>,
        direction: FrameDirection,
        frame: &[u8],
    ) {
        if let Some(trace) = frame_trace.as_mut() {
            trace(direction, frame);
        }
    }
}

fn unexpected_message(request: Opcode, expected: &'static str, actual: Message) -> ClientError {
    match actual {
        Message::Error(code) => ClientError::RequestError { request, code },
        actual => ClientError::UnexpectedMessage { expected, actual },
    }
}

fn build_frame(opcode: Opcode, payload: &[u8]) -> io::Result<Vec<u8>> {
    let payload_len = u16::try_from(payload.len())
        .map_err(|_conversion_error| io::Error::new(ErrorKind::InvalidInput, "payload exceeds u16 wire length"))?;
    let mut frame = Vec::with_capacity(payload.len().saturating_add(4));
    frame.push(opcode.as_u8());
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(payload);
    frame.push(compute_checksum(opcode, payload));
    Ok(frame)
}

fn owned_message(message: MessageRef<'_>) -> Result<Message> {
    match message {
        MessageRef::OrdinaryResponse(ordinary) => Ok(owned_ordinary_message(ordinary)),
        MessageRef::CompatibilityResponse { caps } => Ok(Message::Caps(caps.as_str().to_owned())),
        MessageRef::Error(error) => Ok(Message::Error(error.code())),
        MessageRef::Changed { keyref } => Ok(Message::Changed(keyref)),
        MessageRef::Request(_) | MessageRef::CompatibilityRequest => {
            Err(protocol_error("node sent a client-direction message"))
        }
    }
}

fn owned_ordinary_message(message: OrdinaryResponseRef<'_>) -> Message {
    match message {
        OrdinaryResponseRef::Ok => Message::Ok,
        OrdinaryResponseRef::Identified { nodeid } => Message::Identified(nodeid.to_owned()),
        OrdinaryResponseRef::KeytabLen { count } => Message::KeytabLen(count),
        OrdinaryResponseRef::Key { keyflags, name } => Message::Key {
            keyflags,
            name: name.to_owned(),
        },
        OrdinaryResponseRef::Value { value } => Message::Value(owned_value(value)),
        OrdinaryResponseRef::Written { value } => Message::Written(owned_value(value)),
    }
}

fn owned_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Bool(value) => Value::Bool(value),
        ValueRef::I64(value) => Value::I64(value),
        ValueRef::Str(value) => Value::Str(value.to_owned()),
    }
}

fn push_value(payload: &mut Vec<u8>, value: &Value) -> io::Result<()> {
    match value {
        Value::Bool(value) => {
            payload.push(ValueKind::Bool.as_byte());
            payload.push(u8::from(*value));
        }
        Value::I64(value) => {
            payload.push(ValueKind::I64.as_byte());
            payload.extend_from_slice(&value.to_be_bytes());
        }
        Value::Str(value) => {
            payload.push(ValueKind::Str.as_byte());
            push_str16(payload, value)?;
        }
    }

    Ok(())
}

fn push_str16(payload: &mut Vec<u8>, value: &str) -> io::Result<()> {
    if value.as_bytes().contains(&0x00) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "strings must not contain NUL bytes",
        ));
    }

    let length = u16::try_from(value.len())
        .map_err(|_conversion_error| io::Error::new(ErrorKind::InvalidInput, "string exceeds u16 wire length"))?;
    payload.extend_from_slice(&length.to_be_bytes());
    payload.extend_from_slice(value.as_bytes());
    Ok(())
}

fn protocol_error(message: impl Into<String>) -> ClientError {
    ClientError::Protocol(message.into())
}

fn frame_error(error: qup_core::FrameError) -> ClientError {
    protocol_error(format!("frame validation failed: {error}"))
}

fn payload_error(error: &qup_core::PayloadError) -> ClientError {
    protocol_error(format!("payload validation failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{Client, FrameDirection, Result, Value, build_frame};
    use qup_core::{KeyFlags, MessageRef, Opcode, Parser, RequestRef, ValueKind, WireDirection};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream, duplex};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestRequest {
        Ping,
        GetKeytabLen,
        GetKey { keyref: u16 },
        Get { keyref: u16 },
    }

    #[tokio::test]
    async fn ping_round_trip() -> Result<()> {
        let (client_stream, mut server_stream) = duplex(64);
        let server = tokio::spawn(async move {
            let request = read_request(&mut server_stream).await;
            assert_eq!(request, TestRequest::Ping);
            write_frame(&mut server_stream, Opcode::OK, &[]).await;
        });

        let mut client = Client::new(client_stream);
        let trace = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let trace_sink = trace.clone();
        client.set_frame_trace(move |direction, frame| {
            trace_sink
                .lock()
                .expect("trace mutex should not be poisoned")
                .push((direction, frame.to_vec()));
        });
        client.ping().await?;

        server.await.expect("server task should complete");
        let frames = trace.lock().expect("trace mutex should not be poisoned");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, FrameDirection::Tx);
        assert_eq!(frames[1].0, FrameDirection::Rx);
        assert_eq!(frames[0].1, build_frame(Opcode::PING, &[]).expect("ping frame should build"));
        assert_eq!(frames[1].1, build_frame(Opcode::OK, &[]).expect("ok frame should build"));
        Ok(())
    }

    #[tokio::test]
    async fn get_by_name_fetches_key_table_then_value() -> Result<()> {
        let (client_stream, mut server_stream) = duplex(256);
        let server = tokio::spawn(async move {
            assert_eq!(read_request(&mut server_stream).await, TestRequest::GetKeytabLen);
            write_frame(&mut server_stream, Opcode::KEYTABLEN, &[0x00, 0x01]).await;

            assert_eq!(
                read_request(&mut server_stream).await,
                TestRequest::GetKey { keyref: 0 }
            );
            let mut key_payload = Vec::new();
            key_payload.push(
                KeyFlags::new(KeyFlags::READABLE)
                    .expect("valid keyflags")
                    .bits(),
            );
            key_payload.extend_from_slice(&[0x00, 0x07]);
            key_payload.extend_from_slice(b"voltage");
            write_frame(&mut server_stream, Opcode::KEY, key_payload.as_slice()).await;

            assert_eq!(
                read_request(&mut server_stream).await,
                TestRequest::Get { keyref: 0 }
            );
            write_frame(
                &mut server_stream,
                Opcode::VALUE,
                &[ValueKind::I64.as_byte(), 0, 0, 0, 0, 0, 0, 0, 0xfd],
            )
            .await;
        });

        let mut client = Client::new(client_stream);
        let value = client.get_by_name("voltage").await?;
        assert_eq!(value, Value::I64(253));

        server.await.expect("server task should complete");
        Ok(())
    }

    async fn read_request(stream: &mut DuplexStream) -> TestRequest {
        let mut header = [0u8; 3];
        stream
            .read_exact(&mut header)
            .await
            .expect("request header should be readable");
        let payload_len = usize::from(u16::from_be_bytes([header[1], header[2]]));
        let mut payload = vec![0u8; payload_len];
        stream
            .read_exact(payload.as_mut_slice())
            .await
            .expect("request payload should be readable");
        let mut checksum = [0u8; 1];
        stream
            .read_exact(&mut checksum)
            .await
            .expect("request checksum should be readable");

        let mut frame = Vec::with_capacity(payload_len + 4);
        frame.extend_from_slice(&header);
        frame.extend_from_slice(payload.as_slice());
        frame.extend_from_slice(&checksum);

        let parser = Parser::new();
        match parser
            .parse_frame(WireDirection::ClientToNode, frame.as_slice())
            .expect("request frame should validate")
            .decode_message()
            .expect("request payload should decode")
        {
            MessageRef::Request(RequestRef::Ping) => TestRequest::Ping,
            MessageRef::Request(RequestRef::GetKeytabLen) => TestRequest::GetKeytabLen,
            MessageRef::Request(RequestRef::GetKey { keyref }) => TestRequest::GetKey { keyref },
            MessageRef::Request(RequestRef::Get { keyref }) => TestRequest::Get { keyref },
            other => panic!("unexpected request in test server: {other:?}"),
        }
    }

    async fn write_frame(stream: &mut DuplexStream, opcode: Opcode, payload: &[u8]) {
        let frame = build_frame(opcode, payload).expect("response frame should build");
        stream
            .write_all(frame.as_slice())
            .await
            .expect("response frame should be writable");
    }
}