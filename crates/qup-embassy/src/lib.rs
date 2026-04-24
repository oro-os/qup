//! Embassy-oriented QUP server primitives with typed static keys.
//!
//! # Example
//!
//! ```ignore
//! use qup_embassy::{Key, Perm, QupRead, QupWrite};
//!
//! static CONFIG_VOLTAGE: Key<i64, { Perm::RN }> = Key::new("voltage", 0);
//!
//! async fn run_background_service(stream: impl QupRead + QupWrite) {
//!     let _ = qup_embassy::run!(stream, "demo-node", [&CONFIG_VOLTAGE]);
//! }
//!
//! fn some_operation() {
//!     CONFIG_VOLTAGE.set(251);
//! }
//! ```
//!
//! Current nightly still requires braces around enum const parameters in type
//! position, so the key type is written as `Key<i64, { Perm::RN }>`.
//!
//! The example is marked `ignore` because `run!` currently expands to
//! function-local statics derived from referenced keys, and rustdoc doctests do
//! not currently const-evaluate that pattern.
//!
//! String-valued keys use `heapless::String<N>` as their value type.
//!
#![cfg_attr(not(test), no_std)]
#![allow(incomplete_features)]
#![feature(adt_const_params)]

use core::{
    cell::RefCell,
    fmt,
    marker::ConstParamTy,
    ptr, str,
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

use embassy_futures::select::{Either, select};
use embassy_sync::{
    blocking_mutex::{Mutex as BlockingMutex, raw::CriticalSectionRawMutex},
    signal::Signal,
};
use heapless::String;
use qup_core::{
    AsyncByteRead, AsyncByteWrite, FrameError, KeyFlags, Opcode, PayloadError, ValueKind,
    compute_checksum,
};

const BASE_CAPS: &str = "PkIiCcSsGgWwNkUk";
const OBSERVABLE_CAPS: &str = "PkIiCcSsGgWwNkUk!";
const DISCARD_CHUNK: usize = 64;

const ERROR_UNKNOWN_KEYREF: u8 = 0x01;
const ERROR_NOT_ALLOWED: u8 = 0x02;
const ERROR_TYPE_MISMATCH: u8 = 0x03;

type NotifySignal = Signal<CriticalSectionRawMutex, ()>;
type ValueCell<T> = BlockingMutex<CriticalSectionRawMutex, RefCell<T>>;
type ReadErrorOf<S> = <S as AsyncByteRead>::Error;
type WriteErrorOf<S> = <S as AsyncByteWrite>::Error;

/// Default node identifier used by [`run!`] when no explicit node ID is provided.
pub const DEFAULT_NODE_ID: &str = "qup-embassy";

/// Async QUP byte source used by the embassy server.
///
/// When a server has observable keys, [`Server::run`] races inbound reads against
/// local change notifications. Implementations should therefore make `read_exact`
/// cancellation-safe.
pub trait QupRead: AsyncByteRead {}

impl<T> QupRead for T where T: AsyncByteRead {}

/// Async QUP byte sink used by the embassy server.
pub trait QupWrite: AsyncByteWrite {}

impl<T> QupWrite for T where T: AsyncByteWrite {}

/// Permission flags for a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ConstParamTy)]
pub enum Perm {
    /// The key is neither readable, writable, nor observable.
    None,
    /// The key is readable.
    R,
    /// The key is writable.
    W,
    /// The key is readable and writable.
    RW,
    /// The key is observable.
    N,
    /// The key is readable and observable.
    RN,
    /// The key is writable and observable.
    WN,
    /// The key is readable, writable, and observable.
    RWN,
}

impl Perm {
    /// Returns the protocol `keyflags` bits.
    #[must_use]
    pub const fn flags_bits(self) -> u8 {
        match self {
            Self::None => 0,
            Self::R => KeyFlags::READABLE,
            Self::W => KeyFlags::WRITABLE,
            Self::RW => KeyFlags::READABLE | KeyFlags::WRITABLE,
            Self::N => KeyFlags::OBSERVABLE,
            Self::RN => KeyFlags::READABLE | KeyFlags::OBSERVABLE,
            Self::WN => KeyFlags::WRITABLE | KeyFlags::OBSERVABLE,
            Self::RWN => KeyFlags::READABLE | KeyFlags::WRITABLE | KeyFlags::OBSERVABLE,
        }
    }

    /// Returns whether the readable bit is set.
    #[must_use]
    pub const fn readable(self) -> bool {
        self.flags_bits() & KeyFlags::READABLE != 0
    }

    /// Returns whether the writable bit is set.
    #[must_use]
    pub const fn writable(self) -> bool {
        self.flags_bits() & KeyFlags::WRITABLE != 0
    }

    /// Returns whether the observable bit is set.
    #[must_use]
    pub const fn observable(self) -> bool {
        self.flags_bits() & KeyFlags::OBSERVABLE != 0
    }
}

/// A decoded request value for supported embassy keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireValueRef<'a> {
    /// A `bool` value.
    Bool(bool),
    /// An `i64` value.
    I64(i64),
    /// A borrowed `str16` value.
    Str(&'a str),
    /// A syntactically valid string that exceeded the server's inline buffer.
    OversizedStr,
}

/// Validation error for typed QUP key values.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireValueError {
    /// The wire value kind does not match the key type.
    TypeMismatch,
    /// The value cannot be represented in the protocol's `str16` format.
    ValueTooLarge,
    /// The value contains a NUL byte, which is forbidden by QUP.
    StringContainsNul,
}

impl fmt::Display for WireValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TypeMismatch => f.write_str("type mismatch"),
            Self::ValueTooLarge => f.write_str("value exceeds wire representation"),
            Self::StringContainsNul => f.write_str("string contains NUL byte"),
        }
    }
}

/// Trait implemented by value types that can be stored in a typed QUP key.
pub trait QupValue: Clone + PartialEq + Send + 'static {
    /// Default value used by [`Key::new`].
    const DEFAULT: Self;
    /// Maximum encoded size of the `value` payload body.
    const MAX_WIRE_LEN: usize;
    /// Maximum raw string byte length this type can decode.
    const MAX_STR_LEN: usize = 0;

    /// Validates a stored value before it is accepted by a key.
    fn validate(value: &Self) -> Result<(), WireValueError> {
        let _ = value;
        Ok(())
    }

    /// Encodes the value into the provided payload buffer.
    fn encode(&self, buffer: &mut [u8]) -> Result<usize, WireValueError>;

    /// Decodes a borrowed wire value into the concrete key type.
    fn decode(value: WireValueRef<'_>) -> Result<Self, WireValueError>;
}

impl QupValue for bool {
    const DEFAULT: Self = false;
    const MAX_WIRE_LEN: usize = 2;

    fn encode(&self, buffer: &mut [u8]) -> Result<usize, WireValueError> {
        write_exact(buffer, &[ValueKind::Bool.as_byte(), u8::from(*self)]);
        Ok(2)
    }

    fn decode(value: WireValueRef<'_>) -> Result<Self, WireValueError> {
        match value {
            WireValueRef::Bool(value) => Ok(value),
            WireValueRef::I64(_) | WireValueRef::Str(_) | WireValueRef::OversizedStr => {
                Err(WireValueError::TypeMismatch)
            }
        }
    }
}

impl QupValue for i64 {
    const DEFAULT: Self = 0;
    const MAX_WIRE_LEN: usize = 9;

    fn encode(&self, buffer: &mut [u8]) -> Result<usize, WireValueError> {
        buffer[0] = ValueKind::I64.as_byte();
        write_exact(&mut buffer[1..], &self.to_be_bytes());
        Ok(9)
    }

    fn decode(value: WireValueRef<'_>) -> Result<Self, WireValueError> {
        match value {
            WireValueRef::I64(value) => Ok(value),
            WireValueRef::Bool(_) | WireValueRef::Str(_) | WireValueRef::OversizedStr => {
                Err(WireValueError::TypeMismatch)
            }
        }
    }
}

impl<const N: usize> QupValue for String<N> {
    const DEFAULT: Self = String::new();
    const MAX_WIRE_LEN: usize = 3 + N;
    const MAX_STR_LEN: usize = N;

    fn validate(value: &Self) -> Result<(), WireValueError> {
        if value.as_bytes().contains(&0x00) {
            return Err(WireValueError::StringContainsNul);
        }

        if value.len() > usize::from(u16::MAX) {
            return Err(WireValueError::ValueTooLarge);
        }

        Ok(())
    }

    fn encode(&self, buffer: &mut [u8]) -> Result<usize, WireValueError> {
        Self::validate(self)?;
        buffer[0] = ValueKind::Str.as_byte();
        let len =
            u16::try_from(self.len()).map_err(|_conversion_error| WireValueError::ValueTooLarge)?;
        write_exact(&mut buffer[1..3], &len.to_be_bytes());
        write_exact(&mut buffer[3..3 + self.len()], self.as_bytes());
        Ok(3 + self.len())
    }

    fn decode(value: WireValueRef<'_>) -> Result<Self, WireValueError> {
        match value {
            WireValueRef::Str(value) => {
                let mut decoded = String::new();
                decoded
                    .push_str(value)
                    .map_err(|_capacity_error| WireValueError::TypeMismatch)?;
                Ok(decoded)
            }
            WireValueRef::OversizedStr => Err(WireValueError::TypeMismatch),
            WireValueRef::Bool(_) | WireValueRef::I64(_) => Err(WireValueError::TypeMismatch),
        }
    }
}

/// A typed static QUP key definition.
pub struct Key<T, const PERM: Perm>
where
    T: QupValue,
{
    name: &'static str,
    keyref: u16,
    value: ValueCell<T>,
    notifier: AtomicPtr<NotifySignal>,
    generation: AtomicU32,
}

impl<T, const PERM: Perm> Key<T, PERM>
where
    T: QupValue,
{
    /// Creates a new typed key with a type-defined default value.
    #[must_use]
    pub const fn new(name: &'static str, keyref: u16) -> Self {
        assert_wire_string(name);
        Self {
            name,
            keyref,
            value: BlockingMutex::new(RefCell::new(T::DEFAULT)),
            notifier: AtomicPtr::new(ptr::null_mut()),
            generation: AtomicU32::new(0),
        }
    }

    /// Creates a new typed key with an explicit initial value.
    #[must_use]
    pub const fn with_initial(name: &'static str, keyref: u16, value: T) -> Self {
        assert_wire_string(name);
        Self {
            name,
            keyref,
            value: BlockingMutex::new(RefCell::new(value)),
            notifier: AtomicPtr::new(ptr::null_mut()),
            generation: AtomicU32::new(0),
        }
    }

    /// Returns the static key name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Returns the configured key reference.
    #[must_use]
    pub const fn keyref(&self) -> u16 {
        self.keyref
    }

    /// Returns the key permissions.
    #[must_use]
    pub const fn perm(&self) -> Perm {
        PERM
    }

    /// Returns a clone of the current key value.
    #[must_use]
    pub fn get(&self) -> T {
        self.value.lock(|slot| slot.borrow().clone())
    }

    /// Attempts to replace the current key value.
    pub fn try_set(&self, value: T) -> Result<bool, WireValueError> {
        T::validate(&value)?;

        let mut changed = false;
        self.value.lock(|slot| {
            let mut slot = slot.borrow_mut();
            if *slot != value {
                *slot = value.clone();
                changed = true;
            }
        });

        if changed {
            self.notify_changed();
        }

        Ok(changed)
    }

    /// Replaces the current key value and panics if it cannot be represented on the wire.
    pub fn set(&self, value: T) {
        self.try_set(value)
            .unwrap_or_else(|error| panic!("invalid QUP key value: {error}"));
    }

    fn encode_key_payload(&self, buffer: &mut [u8]) -> usize {
        buffer[0] = PERM.flags_bits();
        1 + encode_str16_into(self.name, &mut buffer[1..])
    }

    fn encode_current_value(&self, buffer: &mut [u8]) -> Result<usize, WireValueError> {
        let value = self.get();
        value.encode(buffer)
    }

    fn write_and_encode(
        &self,
        value: WireValueRef<'_>,
        buffer: &mut [u8],
    ) -> Result<usize, WireValueError> {
        let decoded = T::decode(value)?;
        T::validate(&decoded)?;

        let mut changed = false;
        self.value.lock(|slot| {
            let mut slot = slot.borrow_mut();
            if *slot != decoded {
                *slot = decoded.clone();
                changed = true;
            }
        });

        if changed {
            self.notify_changed();
        }

        decoded.encode(buffer)
    }

    fn generation(&self) -> u32 {
        self.generation.load(Ordering::Acquire)
    }

    fn attach_notifier(&'static self, notifier: &'static NotifySignal) {
        if PERM.observable() {
            self.notifier.store(
                (notifier as *const NotifySignal).cast_mut(),
                Ordering::Release,
            );
        }
    }

    fn notify_changed(&self) {
        if !PERM.observable() {
            return;
        }

        self.generation.fetch_add(1, Ordering::AcqRel);

        let notifier = self.notifier.load(Ordering::Acquire);
        if !notifier.is_null() {
            unsafe { (*notifier).signal(()) };
        }
    }
}

/// Protocol or transport failure returned by the embassy QUP server.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug)]
pub enum ServerError<ReadError, WriteError> {
    /// The byte source returned an error.
    Read(ReadError),
    /// The byte sink returned an error.
    Write(WriteError),
    /// The client violated the QUP wire rules.
    Protocol(ProtocolError),
    /// The server registry or stored values were internally inconsistent.
    Internal(&'static str),
}

/// Protocol-level error returned when the incoming request frame is invalid.
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// The frame structure was invalid.
    Frame(FrameError),
    /// The frame payload was invalid.
    Payload(PayloadError),
}

/// A static embassy QUP server over a fixed key registry.
pub struct Server<const N: usize, const MAX_STR: usize, const MAX_RESPONSE: usize> {
    node_id: &'static str,
    key_count: u16,
    has_observable_keys: bool,
    keys: [&'static dyn ErasedKey; N],
    notifier: NotifySignal,
}

impl<const N: usize, const MAX_STR: usize, const MAX_RESPONSE: usize>
    Server<N, MAX_STR, MAX_RESPONSE>
{
    /// Creates a new static embassy QUP server.
    #[must_use]
    pub const fn new(
        node_id: &'static str,
        key_count: u16,
        has_observable_keys: bool,
        keys: [&'static dyn ErasedKey; N],
    ) -> Self {
        assert_wire_string(node_id);
        Self {
            node_id,
            key_count,
            has_observable_keys,
            keys,
            notifier: Signal::new(),
        }
    }

    /// Runs the server until the transport returns an error or a protocol violation is detected.
    pub async fn run<S>(
        &'static self,
        stream: &mut S,
    ) -> Result<(), ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
    where
        S: QupRead + QupWrite,
    {
        self.attach_notifiers();
        self.debug_assert_unique_keyrefs();

        let mut observed = [false; N];
        let mut seen_generation = [0u32; N];
        let mut string_scratch = [0u8; MAX_STR];
        let mut response_payload = [0u8; MAX_RESPONSE];

        loop {
            if self.has_observable_keys {
                match select(
                    self.read_request(stream, &mut string_scratch),
                    self.notifier.wait(),
                )
                .await
                {
                    Either::First(request) => {
                        self.handle_request(
                            stream,
                            request?,
                            &mut observed,
                            &mut seen_generation,
                            &mut response_payload,
                        )
                        .await?;
                        self.flush_notifications(
                            stream,
                            &observed,
                            &mut seen_generation,
                            &mut response_payload,
                        )
                        .await?;
                    }
                    Either::Second(()) => {
                        self.flush_notifications(
                            stream,
                            &observed,
                            &mut seen_generation,
                            &mut response_payload,
                        )
                        .await?;
                    }
                }
            } else {
                let request = self.read_request(stream, &mut string_scratch).await?;
                self.handle_request(
                    stream,
                    request,
                    &mut observed,
                    &mut seen_generation,
                    &mut response_payload,
                )
                .await?;
            }
        }
    }

    async fn handle_request<S>(
        &self,
        stream: &mut S,
        request: IncomingRequest<'_>,
        observed: &mut [bool; N],
        seen_generation: &mut [u32; N],
        response_payload: &mut [u8; MAX_RESPONSE],
    ) -> Result<(), ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
    where
        S: QupRead + QupWrite,
    {
        match request {
            IncomingRequest::Ping => {
                self.send_frame(stream, Opcode::OK, &[]).await?;
            }
            IncomingRequest::Identify => {
                let len = encode_str16_into(self.node_id, response_payload);
                self.send_frame(stream, Opcode::IDENTIFIED, &response_payload[..len])
                    .await?;
            }
            IncomingRequest::GetCaps => {
                let len = encode_str16_into(self.caps(), response_payload);
                self.send_frame(stream, Opcode::CAPS, &response_payload[..len])
                    .await?;
            }
            IncomingRequest::GetKeytabLen => {
                write_exact(response_payload, &self.key_count.to_be_bytes());
                self.send_frame(stream, Opcode::KEYTABLEN, &response_payload[..2])
                    .await?;
            }
            IncomingRequest::GetKey { keyref } => {
                if keyref >= self.key_count {
                    self.send_error(stream, ERROR_UNKNOWN_KEYREF).await?;
                } else if let Some(index) = self.find_slot(keyref) {
                    let len = self.keys[index].encode_key_payload(response_payload);
                    self.send_frame(stream, Opcode::KEY, &response_payload[..len])
                        .await?;
                } else {
                    response_payload[0] = 0;
                    let len = 1 + encode_str16_into("", &mut response_payload[1..]);
                    self.send_frame(stream, Opcode::KEY, &response_payload[..len])
                        .await?;
                }
            }
            IncomingRequest::Get { keyref } => {
                if keyref >= self.key_count {
                    self.send_error(stream, ERROR_UNKNOWN_KEYREF).await?;
                } else if let Some(index) = self.find_slot(keyref) {
                    if !self.keys[index].readable() {
                        self.send_error(stream, ERROR_NOT_ALLOWED).await?;
                    } else {
                        let len = self.keys[index]
                            .encode_current_value(response_payload)
                            .map_err(|_error| {
                                ServerError::Internal("stored value could not be encoded")
                            })?;
                        self.send_frame(stream, Opcode::VALUE, &response_payload[..len])
                            .await?;
                    }
                } else {
                    self.send_error(stream, ERROR_NOT_ALLOWED).await?;
                }
            }
            IncomingRequest::Write { keyref, value } => {
                if keyref >= self.key_count {
                    self.send_error(stream, ERROR_UNKNOWN_KEYREF).await?;
                } else if let Some(index) = self.find_slot(keyref) {
                    if !self.keys[index].writable() {
                        self.send_error(stream, ERROR_NOT_ALLOWED).await?;
                    } else {
                        match self.keys[index].write_and_encode(value, response_payload) {
                            Ok(len) => {
                                self.send_frame(stream, Opcode::WRITTEN, &response_payload[..len])
                                    .await?;
                            }
                            Err(
                                WireValueError::TypeMismatch
                                | WireValueError::ValueTooLarge
                                | WireValueError::StringContainsNul,
                            ) => {
                                self.send_error(stream, ERROR_TYPE_MISMATCH).await?;
                            }
                        }
                    }
                } else {
                    self.send_error(stream, ERROR_NOT_ALLOWED).await?;
                }
            }
            IncomingRequest::Observe { keyref } => {
                if keyref >= self.key_count {
                    self.send_error(stream, ERROR_UNKNOWN_KEYREF).await?;
                } else if let Some(index) = self.find_slot(keyref) {
                    if !self.keys[index].observable() {
                        self.send_error(stream, ERROR_NOT_ALLOWED).await?;
                    } else {
                        observed[index] = true;
                        seen_generation[index] = self.keys[index].generation();
                        self.send_frame(stream, Opcode::OK, &[]).await?;
                    }
                } else {
                    self.send_error(stream, ERROR_NOT_ALLOWED).await?;
                }
            }
            IncomingRequest::Unobserve { keyref } => {
                if keyref >= self.key_count {
                    self.send_error(stream, ERROR_UNKNOWN_KEYREF).await?;
                } else {
                    if let Some(index) = self.find_slot(keyref) {
                        observed[index] = false;
                        seen_generation[index] = self.keys[index].generation();
                    }
                    self.send_frame(stream, Opcode::OK, &[]).await?;
                }
            }
        }

        Ok(())
    }

    async fn flush_notifications<S>(
        &self,
        stream: &mut S,
        observed: &[bool; N],
        seen_generation: &mut [u32; N],
        response_payload: &mut [u8; MAX_RESPONSE],
    ) -> Result<(), ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
    where
        S: QupRead + QupWrite,
    {
        if !self.has_observable_keys {
            return Ok(());
        }

        for (index, key) in self.keys.iter().enumerate() {
            if !observed[index] || !key.observable() {
                continue;
            }

            let generation = key.generation();
            if generation == seen_generation[index] {
                continue;
            }

            seen_generation[index] = generation;
            write_exact(response_payload, &key.keyref().to_be_bytes());
            self.send_frame(stream, Opcode::CHANGED, &response_payload[..2])
                .await?;
        }

        Ok(())
    }

    async fn send_error<S>(
        &self,
        stream: &mut S,
        code: u8,
    ) -> Result<(), ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
    where
        S: QupRead + QupWrite,
    {
        self.send_frame(stream, Opcode::ERROR, &[code]).await
    }

    async fn send_frame<S>(
        &self,
        stream: &mut S,
        opcode: Opcode,
        payload: &[u8],
    ) -> Result<(), ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
    where
        S: QupRead + QupWrite,
    {
        let payload_len = u16::try_from(payload.len()).map_err(|_conversion_error| {
            ServerError::Internal("response payload exceeded u16 wire length")
        })?;
        let header = [
            opcode.as_u8(),
            payload_len.to_be_bytes()[0],
            payload_len.to_be_bytes()[1],
        ];
        let checksum = [compute_checksum(opcode, payload)];

        stream
            .write_all(&header)
            .await
            .map_err(ServerError::Write)?;
        stream
            .write_all(payload)
            .await
            .map_err(ServerError::Write)?;
        stream
            .write_all(&checksum)
            .await
            .map_err(ServerError::Write)?;
        Ok(())
    }

    async fn read_request<'a, S>(
        &self,
        stream: &mut S,
        string_scratch: &'a mut [u8; MAX_STR],
    ) -> Result<IncomingRequest<'a>, ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
    where
        S: QupRead + QupWrite,
    {
        let mut header = [0u8; 3];
        stream
            .read_exact(&mut header)
            .await
            .map_err(ServerError::Read)?;

        let opcode = Opcode::new(header[0]);
        opcode
            .validate(qup_core::WireDirection::ClientToNode)
            .map_err(|error| ServerError::Protocol(ProtocolError::Frame(error)))?;

        let payload_len = u16::from_be_bytes([header[1], header[2]]);
        let mut sum = FrameSum::new(opcode, payload_len);

        match opcode {
            Opcode::PING => {
                self.read_empty_request(
                    stream,
                    opcode,
                    payload_len,
                    &mut sum,
                    IncomingRequest::Ping,
                )
                .await
            }
            Opcode::IDENTIFY => {
                self.read_empty_request(
                    stream,
                    opcode,
                    payload_len,
                    &mut sum,
                    IncomingRequest::Identify,
                )
                .await
            }
            Opcode::GETKEYTABLEN => {
                self.read_empty_request(
                    stream,
                    opcode,
                    payload_len,
                    &mut sum,
                    IncomingRequest::GetKeytabLen,
                )
                .await
            }
            Opcode::GETCAPS => {
                self.read_empty_request(
                    stream,
                    opcode,
                    payload_len,
                    &mut sum,
                    IncomingRequest::GetCaps,
                )
                .await
            }
            Opcode::GETKEY => {
                self.read_keyref_request(stream, opcode, payload_len, &mut sum, |keyref| {
                    IncomingRequest::GetKey { keyref }
                })
                .await
            }
            Opcode::GET => {
                self.read_keyref_request(stream, opcode, payload_len, &mut sum, |keyref| {
                    IncomingRequest::Get { keyref }
                })
                .await
            }
            Opcode::OBSERVE => {
                self.read_keyref_request(stream, opcode, payload_len, &mut sum, |keyref| {
                    IncomingRequest::Observe { keyref }
                })
                .await
            }
            Opcode::UNOBSERVE => {
                self.read_keyref_request(stream, opcode, payload_len, &mut sum, |keyref| {
                    IncomingRequest::Unobserve { keyref }
                })
                .await
            }
            Opcode::WRITE => {
                self.read_write_request(stream, payload_len, &mut sum, string_scratch)
                    .await
            }
            _ => Err(ServerError::Protocol(ProtocolError::Frame(
                FrameError::InvalidDirection {
                    opcode,
                    direction: qup_core::WireDirection::ClientToNode,
                },
            ))),
        }
    }

    async fn read_empty_request<S>(
        &self,
        stream: &mut S,
        opcode: Opcode,
        payload_len: u16,
        sum: &mut FrameSum,
        request: IncomingRequest<'static>,
    ) -> Result<IncomingRequest<'static>, ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
    where
        S: QupRead + QupWrite,
    {
        if payload_len != 0 {
            let mut discard = [0u8; DISCARD_CHUNK];
            discard_exact(stream, usize::from(payload_len), &mut discard, sum).await?;
            finish_checksum(stream, sum).await?;
            return Err(ServerError::Protocol(ProtocolError::Payload(
                PayloadError::InvalidLength {
                    opcode,
                    expected: 0,
                    actual: usize::from(payload_len),
                },
            )));
        }

        finish_checksum(stream, sum).await?;
        Ok(request)
    }

    async fn read_keyref_request<S>(
        &self,
        stream: &mut S,
        opcode: Opcode,
        payload_len: u16,
        sum: &mut FrameSum,
        constructor: fn(u16) -> IncomingRequest<'static>,
    ) -> Result<IncomingRequest<'static>, ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
    where
        S: QupRead + QupWrite,
    {
        if payload_len != 2 {
            let mut discard = [0u8; DISCARD_CHUNK];
            discard_exact(stream, usize::from(payload_len), &mut discard, sum).await?;
            finish_checksum(stream, sum).await?;
            return Err(ServerError::Protocol(ProtocolError::Payload(
                PayloadError::InvalidLength {
                    opcode,
                    expected: 2,
                    actual: usize::from(payload_len),
                },
            )));
        }

        let mut keyref = [0u8; 2];
        read_exact_tracked(stream, &mut keyref, sum).await?;
        finish_checksum(stream, sum).await?;
        Ok(constructor(u16::from_be_bytes(keyref)))
    }

    async fn read_write_request<'a, S>(
        &self,
        stream: &mut S,
        payload_len: u16,
        sum: &mut FrameSum,
        string_scratch: &'a mut [u8; MAX_STR],
    ) -> Result<IncomingRequest<'a>, ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
    where
        S: QupRead + QupWrite,
    {
        if payload_len < 3 {
            let mut discard = [0u8; DISCARD_CHUNK];
            discard_exact(stream, usize::from(payload_len), &mut discard, sum).await?;
            finish_checksum(stream, sum).await?;
            return Err(ServerError::Protocol(ProtocolError::Payload(
                PayloadError::InvalidLength {
                    opcode: Opcode::WRITE,
                    expected: 3,
                    actual: usize::from(payload_len),
                },
            )));
        }

        let mut keyref_bytes = [0u8; 2];
        read_exact_tracked(stream, &mut keyref_bytes, sum).await?;
        let keyref = u16::from_be_bytes(keyref_bytes);

        let mut kind = [0u8; 1];
        read_exact_tracked(stream, &mut kind, sum).await?;

        match ValueKind::from_byte(kind[0]) {
            Ok(ValueKind::Bool) => {
                let remaining = usize::from(payload_len) - 3;
                if remaining != 1 {
                    let mut discard = [0u8; DISCARD_CHUNK];
                    discard_exact(stream, remaining, &mut discard, sum).await?;
                    finish_checksum(stream, sum).await?;
                    return Err(ServerError::Protocol(ProtocolError::Payload(
                        if remaining < 1 {
                            PayloadError::InternalLengthExceedsPayload
                        } else {
                            PayloadError::TrailingBytes {
                                remaining: remaining - 1,
                            }
                        },
                    )));
                }

                let mut value = [0u8; 1];
                read_exact_tracked(stream, &mut value, sum).await?;
                finish_checksum(stream, sum).await?;

                let decoded = match value[0] {
                    0x00 => false,
                    0x01 => true,
                    invalid => {
                        return Err(ServerError::Protocol(ProtocolError::Payload(
                            PayloadError::InvalidBool(invalid),
                        )));
                    }
                };
                Ok(IncomingRequest::Write {
                    keyref,
                    value: WireValueRef::Bool(decoded),
                })
            }
            Ok(ValueKind::I64) => {
                let remaining = usize::from(payload_len) - 3;
                if remaining != 8 {
                    let mut discard = [0u8; DISCARD_CHUNK];
                    discard_exact(stream, remaining, &mut discard, sum).await?;
                    finish_checksum(stream, sum).await?;
                    return Err(ServerError::Protocol(ProtocolError::Payload(
                        if remaining < 8 {
                            PayloadError::InternalLengthExceedsPayload
                        } else {
                            PayloadError::TrailingBytes {
                                remaining: remaining - 8,
                            }
                        },
                    )));
                }

                let mut value = [0u8; 8];
                read_exact_tracked(stream, &mut value, sum).await?;
                finish_checksum(stream, sum).await?;
                Ok(IncomingRequest::Write {
                    keyref,
                    value: WireValueRef::I64(i64::from_be_bytes(value)),
                })
            }
            Ok(ValueKind::Str) => {
                let remaining = usize::from(payload_len) - 3;
                if remaining < 2 {
                    let mut discard = [0u8; DISCARD_CHUNK];
                    discard_exact(stream, remaining, &mut discard, sum).await?;
                    finish_checksum(stream, sum).await?;
                    return Err(ServerError::Protocol(ProtocolError::Payload(
                        PayloadError::InternalLengthExceedsPayload,
                    )));
                }

                let mut len_bytes = [0u8; 2];
                read_exact_tracked(stream, &mut len_bytes, sum).await?;
                let string_len = usize::from(u16::from_be_bytes(len_bytes));
                let remaining_string_bytes = remaining - 2;

                if remaining_string_bytes != string_len {
                    let mut discard = [0u8; DISCARD_CHUNK];
                    discard_exact(stream, remaining_string_bytes, &mut discard, sum).await?;
                    finish_checksum(stream, sum).await?;
                    return Err(ServerError::Protocol(ProtocolError::Payload(
                        if remaining_string_bytes < string_len {
                            PayloadError::InternalLengthExceedsPayload
                        } else {
                            PayloadError::TrailingBytes {
                                remaining: remaining_string_bytes - string_len,
                            }
                        },
                    )));
                }

                if string_len <= MAX_STR {
                    read_exact_tracked(stream, &mut string_scratch[..string_len], sum).await?;
                    finish_checksum(stream, sum).await?;
                    let value = decode_wire_string(&string_scratch[..string_len])
                        .map_err(|error| ServerError::Protocol(ProtocolError::Payload(error)))?;
                    Ok(IncomingRequest::Write {
                        keyref,
                        value: WireValueRef::Str(value),
                    })
                } else {
                    let mut validator = Utf8StreamValidator::new();
                    let mut discard = [0u8; DISCARD_CHUNK];
                    let mut remaining_bytes = string_len;
                    let mut payload_error = None;

                    while remaining_bytes != 0 {
                        let chunk_len = remaining_bytes.min(DISCARD_CHUNK);
                        let chunk = &mut discard[..chunk_len];
                        read_exact_tracked(stream, chunk, sum).await?;
                        if payload_error.is_none() {
                            if let Err(error) = validator.feed(chunk) {
                                payload_error = Some(error);
                            }
                        }
                        remaining_bytes -= chunk_len;
                    }

                    finish_checksum(stream, sum).await?;
                    if let Some(error) = payload_error.or_else(|| validator.finish().err()) {
                        return Err(ServerError::Protocol(ProtocolError::Payload(error)));
                    }

                    Ok(IncomingRequest::Write {
                        keyref,
                        value: WireValueRef::OversizedStr,
                    })
                }
            }
            Err(error) => {
                let mut discard = [0u8; DISCARD_CHUNK];
                discard_exact(stream, usize::from(payload_len) - 3, &mut discard, sum).await?;
                finish_checksum(stream, sum).await?;
                Err(ServerError::Protocol(ProtocolError::Payload(error)))
            }
        }
    }

    fn caps(&self) -> &'static str {
        if self.has_observable_keys {
            OBSERVABLE_CAPS
        } else {
            BASE_CAPS
        }
    }

    fn attach_notifiers(&'static self) {
        for key in &self.keys {
            key.attach_notifier(&self.notifier);
        }
    }

    fn find_slot(&self, keyref: u16) -> Option<usize> {
        self.keys.iter().position(|key| key.keyref() == keyref)
    }

    fn debug_assert_unique_keyrefs(&self) {
        for (index, key) in self.keys.iter().enumerate() {
            for other in self.keys.iter().skip(index + 1) {
                debug_assert!(
                    key.keyref() != other.keyref(),
                    "duplicate keyref {} in embassy server registry",
                    key.keyref()
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncomingRequest<'a> {
    Ping,
    Identify,
    GetCaps,
    GetKeytabLen,
    GetKey {
        keyref: u16,
    },
    Get {
        keyref: u16,
    },
    Write {
        keyref: u16,
        value: WireValueRef<'a>,
    },
    Observe {
        keyref: u16,
    },
    Unobserve {
        keyref: u16,
    },
}

#[doc(hidden)]
pub trait ErasedKey: Sync {
    fn keyref(&self) -> u16;
    fn readable(&self) -> bool;
    fn writable(&self) -> bool;
    fn observable(&self) -> bool;
    fn generation(&self) -> u32;
    fn encode_key_payload(&self, buffer: &mut [u8]) -> usize;
    fn encode_current_value(&self, buffer: &mut [u8]) -> Result<usize, WireValueError>;
    fn write_and_encode(
        &self,
        value: WireValueRef<'_>,
        buffer: &mut [u8],
    ) -> Result<usize, WireValueError>;
    fn attach_notifier(&'static self, notifier: &'static NotifySignal);
}

impl<T, const PERM: Perm> ErasedKey for Key<T, PERM>
where
    T: QupValue,
{
    fn keyref(&self) -> u16 {
        self.keyref
    }

    fn readable(&self) -> bool {
        PERM.readable()
    }

    fn writable(&self) -> bool {
        PERM.writable()
    }

    fn observable(&self) -> bool {
        PERM.observable()
    }

    fn generation(&self) -> u32 {
        self.generation()
    }

    fn encode_key_payload(&self, buffer: &mut [u8]) -> usize {
        self.encode_key_payload(buffer)
    }

    fn encode_current_value(&self, buffer: &mut [u8]) -> Result<usize, WireValueError> {
        self.encode_current_value(buffer)
    }

    fn write_and_encode(
        &self,
        value: WireValueRef<'_>,
        buffer: &mut [u8],
    ) -> Result<usize, WireValueError> {
        self.write_and_encode(value, buffer)
    }

    fn attach_notifier(&'static self, notifier: &'static NotifySignal) {
        self.attach_notifier(notifier);
    }
}

#[doc(hidden)]
pub mod __private {
    use super::{BASE_CAPS, DEFAULT_NODE_ID, Key, OBSERVABLE_CAPS, Perm, QupValue};

    pub use super::ErasedKey;

    pub const fn default_node_id() -> &'static str {
        DEFAULT_NODE_ID
    }

    pub const fn str16_wire_len(value: &str) -> usize {
        2 + value.len()
    }

    pub const fn caps_wire_len(has_observable: bool) -> usize {
        str16_wire_len(if has_observable {
            OBSERVABLE_CAPS
        } else {
            BASE_CAPS
        })
    }

    pub const fn max_usize(values: &[usize]) -> usize {
        let mut index = 0usize;
        let mut max = 0usize;
        while index < values.len() {
            if values[index] > max {
                max = values[index];
            }
            index += 1;
        }
        max
    }

    pub const fn max_u16(values: &[u16]) -> u16 {
        let mut index = 0usize;
        let mut max = 0u16;
        while index < values.len() {
            if values[index] > max {
                max = values[index];
            }
            index += 1;
        }
        max
    }

    pub const fn any(values: &[bool]) -> bool {
        let mut index = 0usize;
        while index < values.len() {
            if values[index] {
                return true;
            }
            index += 1;
        }
        false
    }

    pub const fn key_wire_len<T, const PERM: Perm>(_: &Key<T, PERM>) -> usize
    where
        T: QupValue,
    {
        T::MAX_WIRE_LEN
    }

    pub const fn key_max_string_len<T, const PERM: Perm>(_: &Key<T, PERM>) -> usize
    where
        T: QupValue,
    {
        T::MAX_STR_LEN
    }

    pub const fn key_key_payload_len<T, const PERM: Perm>(key: &Key<T, PERM>) -> usize
    where
        T: QupValue,
    {
        1 + str16_wire_len(key.name())
    }

    pub const fn key_keyref<T, const PERM: Perm>(key: &Key<T, PERM>) -> u16
    where
        T: QupValue,
    {
        key.keyref()
    }

    pub const fn key_observable<T, const PERM: Perm>(_: &Key<T, PERM>) -> bool
    where
        T: QupValue,
    {
        PERM.observable()
    }

    pub const fn erased_key<T, const PERM: Perm>(
        key: &'static Key<T, PERM>,
    ) -> &'static dyn ErasedKey
    where
        T: QupValue,
    {
        key
    }
}

/// Runs a static embassy QUP server over a fixed key registry.
#[macro_export]
macro_rules! run {
    ($stream:expr, [$($key:expr),* $(,)?]) => {
        $crate::run!($stream, $crate::__private::default_node_id(), [$($key),*])
    };
    ($stream:expr, $node_id:expr, [$($key:expr),* $(,)?]) => {{
        const __QUP_MAX_VALUE: usize = $crate::__private::max_usize(&[
            0usize,
            $($crate::__private::key_wire_len($key)),*
        ]);
        const __QUP_MAX_STR: usize = $crate::__private::max_usize(&[
            0usize,
            $($crate::__private::key_max_string_len($key)),*
        ]);
        const __QUP_MAX_KEY: usize = $crate::__private::max_usize(&[
            0usize,
            $($crate::__private::key_key_payload_len($key)),*
        ]);
        const __QUP_COUNT: u16 = $crate::__private::max_u16(&[
            0u16,
            $($crate::__private::key_keyref($key).wrapping_add(1)),*
        ]);
        const __QUP_HAS_OBSERVABLE: bool = $crate::__private::any(&[
            false,
            $($crate::__private::key_observable($key)),*
        ]);
        const __QUP_MAX_RESPONSE: usize = $crate::__private::max_usize(&[
            __QUP_MAX_VALUE,
            __QUP_MAX_KEY,
            $crate::__private::str16_wire_len($node_id),
            $crate::__private::caps_wire_len(__QUP_HAS_OBSERVABLE),
            2usize,
            1usize,
        ]);

        static __QUP_SERVER: $crate::Server<
            { $crate::__qup_count_exprs!($($key),*) },
            __QUP_MAX_STR,
            __QUP_MAX_RESPONSE,
        > = $crate::Server::new(
            $node_id,
            __QUP_COUNT,
            __QUP_HAS_OBSERVABLE,
            [$( $crate::__private::erased_key($key) ),*],
        );

        let mut __qup_stream = $stream;
        __QUP_SERVER.run(&mut __qup_stream).await
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __qup_count_exprs {
    () => { 0usize };
    ($head:expr $(, $tail:expr)*) => { 1usize + $crate::__qup_count_exprs!($($tail),*) };
}

const fn assert_wire_string(value: &str) {
    assert!(
        value.len() <= u16::MAX as usize,
        "QUP strings must fit in str16"
    );
    let bytes = value.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        assert!(
            bytes[index] != 0x00,
            "QUP strings must not contain NUL bytes"
        );
        index += 1;
    }
}

fn decode_wire_string(bytes: &[u8]) -> Result<&str, PayloadError> {
    if bytes.contains(&0x00) {
        return Err(PayloadError::StringContainsNul);
    }

    str::from_utf8(bytes).map_err(PayloadError::InvalidUtf8)
}

fn encode_str16_into(value: &str, buffer: &mut [u8]) -> usize {
    let len = u16::try_from(value.len()).expect("QUP str16 exceeded u16 length");
    write_exact(buffer, &len.to_be_bytes());
    write_exact(&mut buffer[2..2 + value.len()], value.as_bytes());
    2 + value.len()
}

fn write_exact(dst: &mut [u8], src: &[u8]) {
    dst[..src.len()].copy_from_slice(src);
}

async fn read_exact_tracked<S>(
    stream: &mut S,
    buffer: &mut [u8],
    sum: &mut FrameSum,
) -> Result<(), ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
where
    S: QupRead + QupWrite,
{
    stream.read_exact(buffer).await.map_err(ServerError::Read)?;
    sum.add_slice(buffer);
    Ok(())
}

async fn discard_exact<S>(
    stream: &mut S,
    len: usize,
    scratch: &mut [u8; DISCARD_CHUNK],
    sum: &mut FrameSum,
) -> Result<(), ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
where
    S: QupRead + QupWrite,
{
    let mut remaining = len;
    while remaining != 0 {
        let chunk_len = remaining.min(DISCARD_CHUNK);
        let chunk = &mut scratch[..chunk_len];
        stream.read_exact(chunk).await.map_err(ServerError::Read)?;
        sum.add_slice(chunk);
        remaining -= chunk_len;
    }

    Ok(())
}

async fn finish_checksum<S>(
    stream: &mut S,
    sum: &mut FrameSum,
) -> Result<(), ServerError<ReadErrorOf<S>, WriteErrorOf<S>>>
where
    S: QupRead + QupWrite,
{
    let mut checksum = [0u8; 1];
    stream
        .read_exact(&mut checksum)
        .await
        .map_err(ServerError::Read)?;
    sum.add_byte(checksum[0]);
    sum.finish()
        .map_err(|error| ServerError::Protocol(ProtocolError::Frame(error)))
}

#[derive(Clone, Copy)]
struct FrameSum(u8);

impl FrameSum {
    fn new(opcode: Opcode, payload_len: u16) -> Self {
        let [hi, lo] = payload_len.to_be_bytes();
        Self(opcode.as_u8().wrapping_add(hi).wrapping_add(lo))
    }

    fn add_byte(&mut self, byte: u8) {
        self.0 = self.0.wrapping_add(byte);
    }

    fn add_slice(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.add_byte(*byte);
        }
    }

    fn finish(self) -> Result<(), FrameError> {
        if self.0 == 0 {
            Ok(())
        } else {
            Err(FrameError::ChecksumMismatch { sum: self.0 })
        }
    }
}

struct Utf8StreamValidator {
    tail: [u8; 4],
    tail_len: usize,
}

impl Utf8StreamValidator {
    fn new() -> Self {
        Self {
            tail: [0u8; 4],
            tail_len: 0,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<(), PayloadError> {
        if chunk.contains(&0x00) {
            return Err(PayloadError::StringContainsNul);
        }

        let total = self.tail_len + chunk.len();
        let mut buffer = [0u8; DISCARD_CHUNK + 4];
        buffer[..self.tail_len].copy_from_slice(&self.tail[..self.tail_len]);
        buffer[self.tail_len..total].copy_from_slice(chunk);

        match str::from_utf8(&buffer[..total]) {
            Ok(_) => {
                self.tail_len = 0;
                Ok(())
            }
            Err(error) => {
                if error.error_len().is_some() {
                    return Err(PayloadError::InvalidUtf8(error));
                }

                let valid_up_to = error.valid_up_to();
                let leftover = total - valid_up_to;
                self.tail[..leftover].copy_from_slice(&buffer[valid_up_to..total]);
                self.tail_len = leftover;
                Ok(())
            }
        }
    }

    fn finish(self) -> Result<(), PayloadError> {
        if self.tail_len == 0 {
            return Ok(());
        }

        match str::from_utf8(&self.tail[..self.tail_len]) {
            Ok(_) => Ok(()),
            Err(error) => Err(PayloadError::InvalidUtf8(error)),
        }
    }
}
