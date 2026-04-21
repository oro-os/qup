//! Tokio-based QUP test node used for exercising client implementations.
//!
//! The server exposes a stable key table with mixed read, write, and observe
//! behavior so host-side tooling can validate protocol handling.

#![allow(
    clippy::missing_docs_in_private_items,
    reason = "the executable is an internal test harness and its private implementation details are intentionally undocumented"
)]
#![expect(
    clippy::missing_const_for_fn,
    reason = "const qualification is not useful for this runtime-focused test binary"
)]
#![expect(
    clippy::std_instead_of_alloc,
    reason = "the tokio-based executable intentionally uses std networking and synchronization types"
)]
#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    io,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicI64, AtomicU16, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::Parser as ClapParser;
use qup_core::{KeyFlags, MessageRef, Opcode, Parser as FrameParser, RequestRef, ValueKind, ValueRef, compute_checksum};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream, tcp::{OwnedReadHalf, OwnedWriteHalf}},
    sync::mpsc,
    time,
};

const CAPS: &str = "PkIiCcSsGgWwNkUk!";
const NODE_ID: &str = "qup-test-node";

const KEY_COUNT: u16 = 14;
const KEY_COUNT_USIZE: usize = 14;

const ERR_UNKNOWN_KEYREF: u8 = 0x01;
const ERR_PERMISSION: u8 = 0x02;
const ERR_TYPE_MISMATCH: u8 = 0x03;

const KEY_VOLTAGE_MA: u16 = 0;
const KEY_UPTIME: u16 = 1;
const KEY_GP1_STRING: u16 = 2;
const KEY_GP2_U64: u16 = 3;
const KEY_GP3_I64: u16 = 4;
const KEY_MIRROR_IN: u16 = 5;
const KEY_MIRROR_OUT: u16 = 6;
const KEY_SMALL_STR10: u16 = 7;
const KEY_SMALL_STR0: u16 = 8;
const KEY_PERM_U64_W: u16 = 11;
const KEY_PERM_U64_RW: u16 = 12;
const KEY_PERM_U64_R: u16 = 13;

#[derive(Debug, ClapParser)]
#[command(name = "qup-test-node")]
struct Args {
    #[arg(short = 'b', long = "bind", default_value = "127.0.0.1")]
    bind: String,
    #[arg(short = 'p', long = "port", default_value_t = 3400)]
    port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeySpec {
    name: &'static str,
    flags: u8,
}

const KEY_SPECS: [KeySpec; KEY_COUNT_USIZE] = [
    KeySpec {
        name: "voltage_ma",
        flags: KeyFlags::READABLE | KeyFlags::OBSERVABLE,
    },
    KeySpec {
        name: "uptime",
        flags: KeyFlags::READABLE,
    },
    KeySpec {
        name: "gp1_string",
        flags: KeyFlags::READABLE | KeyFlags::WRITABLE | KeyFlags::OBSERVABLE,
    },
    KeySpec {
        name: "gp2_u64",
        flags: KeyFlags::READABLE | KeyFlags::WRITABLE | KeyFlags::OBSERVABLE,
    },
    KeySpec {
        name: "gp3_i64",
        flags: KeyFlags::READABLE | KeyFlags::WRITABLE | KeyFlags::OBSERVABLE,
    },
    KeySpec {
        name: "mirror_in",
        flags: KeyFlags::WRITABLE,
    },
    KeySpec {
        name: "mirror_out",
        flags: KeyFlags::READABLE | KeyFlags::OBSERVABLE,
    },
    KeySpec {
        name: "small_str10",
        flags: KeyFlags::READABLE | KeyFlags::WRITABLE | KeyFlags::OBSERVABLE,
    },
    KeySpec {
        name: "small_str0",
        flags: KeyFlags::READABLE | KeyFlags::WRITABLE | KeyFlags::OBSERVABLE,
    },
    KeySpec {
        name: "dead",
        flags: 0,
    },
    KeySpec {
        name: "dead2",
        flags: 0,
    },
    KeySpec {
        name: "perm_u64_w",
        flags: KeyFlags::WRITABLE,
    },
    KeySpec {
        name: "perm_u64_rw",
        flags: KeyFlags::READABLE | KeyFlags::WRITABLE,
    },
    KeySpec {
        name: "perm_u64_r",
        flags: KeyFlags::READABLE,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum WireValue {
    I64(i64),
    Str(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteSuccess {
    value: WireValue,
    changed_keyref: Option<u16>,
}

impl WriteSuccess {
    fn new(value: WireValue, changed_keyref: Option<u16>) -> Self {
        Self {
            value,
            changed_keyref,
        }
    }
}

#[derive(Debug)]
struct ConnectionHandle {
    sender: mpsc::UnboundedSender<Vec<u8>>,
    observed_mask: AtomicU16,
}

impl ConnectionHandle {
    const fn new(sender: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        Self {
            sender,
            observed_mask: AtomicU16::new(0),
        }
    }

    fn send(&self, frame: Vec<u8>) -> bool {
        self.sender.send(frame).is_ok()
    }

    fn observe(&self, keyref: u16) {
        if let Some(mask) = key_mask(keyref) {
            self.observed_mask.fetch_or(mask, Ordering::SeqCst);
        }
    }

    fn unobserve(&self, keyref: u16) {
        if let Some(mask) = key_mask(keyref) {
            self.observed_mask.fetch_and(!mask, Ordering::SeqCst);
        }
    }

    fn is_observing(&self, keyref: u16) -> bool {
        key_mask(keyref).is_some_and(|mask| {
            self.observed_mask.load(Ordering::SeqCst) & mask != 0
        })
    }
}

#[derive(Debug, Default)]
struct ConnectionRegistry {
    next_id: AtomicU64,
    connections: Mutex<BTreeMap<u64, Arc<ConnectionHandle>>>,
}

impl ConnectionRegistry {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            connections: Mutex::new(BTreeMap::new()),
        }
    }

    fn register(
        &self,
        sender: mpsc::UnboundedSender<Vec<u8>>,
    ) -> (u64, Arc<ConnectionHandle>) {
        let connection_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let connection = Arc::new(ConnectionHandle::new(sender));
        recover_lock(&self.connections).insert(connection_id, Arc::clone(&connection));
        (connection_id, connection)
    }

    fn unregister(&self, connection_id: u64) {
        recover_lock(&self.connections).remove(&connection_id);
    }

    fn broadcast_changed(&self, keyref: u16) {
        let frame = encode_changed(keyref);
        let connections = recover_lock(&self.connections)
            .values()
            .cloned()
            .collect::<Vec<_>>();

        for connection in connections {
            if connection.is_observing(keyref) {
                let _sent = connection.send(frame.clone());
            }
        }
    }
}

#[derive(Debug)]
struct SharedState {
    registry: Arc<ConnectionRegistry>,
    started_at: Instant,
    rng_seed: AtomicU64,
    voltage_ma: AtomicI64,
    gp1_string: Mutex<String>,
    gp2_u64: AtomicU64,
    gp3_i64: AtomicI64,
    mirror_in: Mutex<String>,
    mirror_pending: Mutex<String>,
    mirror_generation: AtomicU64,
    mirror_out: Mutex<String>,
    small_str10: Mutex<String>,
    small_str0: Mutex<String>,
    perm_u64_w: AtomicU64,
    perm_u64_rw: AtomicU64,
}

impl SharedState {
    fn new(registry: Arc<ConnectionRegistry>) -> Arc<Self> {
        Arc::new(Self {
            registry,
            started_at: Instant::now(),
            rng_seed: AtomicU64::new(initial_seed()),
            voltage_ma: AtomicI64::new(250),
            gp1_string: Mutex::new(String::new()),
            gp2_u64: AtomicU64::new(0),
            gp3_i64: AtomicI64::new(0),
            mirror_in: Mutex::new(String::new()),
            mirror_pending: Mutex::new(String::new()),
            mirror_generation: AtomicU64::new(0),
            mirror_out: Mutex::new(String::new()),
            small_str10: Mutex::new(String::new()),
            small_str0: Mutex::new(String::new()),
            perm_u64_w: AtomicU64::new(0),
            perm_u64_rw: AtomicU64::new(0),
        })
    }

    fn start_background_tasks(self: &Arc<Self>) {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            state.run_voltage_loop().await;
        });
    }

    async fn run_voltage_loop(self: Arc<Self>) {
        let mut interval = time::interval(Duration::from_millis(250));
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        #[expect(
            clippy::infinite_loop,
            reason = "the test node keeps publishing background voltage updates until process exit"
        )]
        loop {
            interval.tick().await;
            self.update_voltage();
        }
    }

    fn update_voltage(&self) {
        let next_value = 250i64.saturating_add(self.next_voltage_offset());
        let previous = self.voltage_ma.swap(next_value, Ordering::SeqCst);
        if previous != next_value {
            self.registry.broadcast_changed(KEY_VOLTAGE_MA);
        }
    }

    #[expect(
        clippy::integer_division_remainder_used,
        reason = "simple bounded jitter is sufficient for the test node's fake measurement stream"
    )]
    fn next_voltage_offset(&self) -> i64 {
        let seed = self
            .rng_seed
            .fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::SeqCst);
        let mixed = mix_u64(seed);
        let bucket = mixed % 41;
        i64::try_from(bucket).unwrap_or(0).saturating_sub(20)
    }

    fn get_value(&self, keyref: u16) -> Result<WireValue, u8> {
        let Some(spec) = key_spec(keyref) else {
            return Err(ERR_UNKNOWN_KEYREF);
        };

        if spec.flags & KeyFlags::READABLE == 0 {
            return Err(ERR_PERMISSION);
        }

        match keyref {
            KEY_VOLTAGE_MA => Ok(WireValue::I64(self.voltage_ma.load(Ordering::SeqCst))),
            KEY_UPTIME => Ok(WireValue::I64(self.uptime_seconds())),
            KEY_GP1_STRING => Ok(WireValue::Str(recover_lock(&self.gp1_string).clone())),
            KEY_GP2_U64 => Ok(WireValue::I64(load_u64_wire(&self.gp2_u64))),
            KEY_GP3_I64 => Ok(WireValue::I64(self.gp3_i64.load(Ordering::SeqCst))),
            KEY_MIRROR_OUT => Ok(WireValue::Str(recover_lock(&self.mirror_out).clone())),
            KEY_SMALL_STR10 => Ok(WireValue::Str(recover_lock(&self.small_str10).clone())),
            KEY_SMALL_STR0 => Ok(WireValue::Str(recover_lock(&self.small_str0).clone())),
            KEY_PERM_U64_RW => Ok(WireValue::I64(load_u64_wire(&self.perm_u64_rw))),
            KEY_PERM_U64_R => Ok(WireValue::I64(1337)),
            _ => Err(ERR_PERMISSION),
        }
    }

    fn write_value(self: &Arc<Self>, keyref: u16, value: ValueRef<'_>) -> Result<WriteSuccess, u8> {
        let Some(spec) = key_spec(keyref) else {
            return Err(ERR_UNKNOWN_KEYREF);
        };

        if spec.flags & KeyFlags::WRITABLE == 0 {
            return Err(ERR_PERMISSION);
        }

        match keyref {
            KEY_GP1_STRING => {
                let value = expect_str(value)?;
                let mut stored = recover_lock(&self.gp1_string);
                let changed = stored.as_str() != value;
                stored.clear();
                stored.push_str(value);
                Ok(WriteSuccess::new(
                    WireValue::Str(stored.clone()),
                    observable_change(keyref, changed),
                ))
            }
            KEY_GP2_U64 => {
                let value = expect_u64(value)?;
                let previous = self.gp2_u64.swap(value, Ordering::SeqCst);
                Ok(WriteSuccess::new(
                    WireValue::I64(u64_to_wire(value)),
                    observable_change(keyref, previous != value),
                ))
            }
            KEY_GP3_I64 => {
                let value = expect_i64(value)?;
                let previous = self.gp3_i64.swap(value, Ordering::SeqCst);
                Ok(WriteSuccess::new(
                    WireValue::I64(value),
                    observable_change(keyref, previous != value),
                ))
            }
            KEY_MIRROR_IN => {
                let value = expect_str(value)?;
                {
                    let mut stored = recover_lock(&self.mirror_in);
                    stored.clear();
                    stored.push_str(value);
                }
                {
                    let mut pending = recover_lock(&self.mirror_pending);
                    pending.clear();
                    pending.push_str(value);
                }
                let generation = self
                    .mirror_generation
                    .fetch_add(1, Ordering::SeqCst)
                    .saturating_add(1);
                self.schedule_mirror_update(generation);
                Ok(WriteSuccess::new(WireValue::Str(value.to_owned()), None))
            }
            KEY_SMALL_STR10 => {
                let value = expect_str(value)?;
                if value.len() > 10 {
                    return Err(ERR_TYPE_MISMATCH);
                }
                let mut stored = recover_lock(&self.small_str10);
                let changed = stored.as_str() != value;
                stored.clear();
                stored.push_str(value);
                Ok(WriteSuccess::new(
                    WireValue::Str(stored.clone()),
                    observable_change(keyref, changed),
                ))
            }
            KEY_SMALL_STR0 => {
                expect_str(value)?;
                Err(ERR_TYPE_MISMATCH)
            }
            KEY_PERM_U64_W => {
                let value = expect_u64(value)?;
                self.perm_u64_w.store(value, Ordering::SeqCst);
                Ok(WriteSuccess::new(WireValue::I64(u64_to_wire(value)), None))
            }
            KEY_PERM_U64_RW => {
                let value = expect_u64(value)?;
                self.perm_u64_rw.store(value, Ordering::SeqCst);
                Ok(WriteSuccess::new(WireValue::I64(u64_to_wire(value)), None))
            }
            _ => Err(ERR_PERMISSION),
        }
    }

    fn can_observe(keyref: u16) -> Result<(), u8> {
        let Some(spec) = key_spec(keyref) else {
            return Err(ERR_UNKNOWN_KEYREF);
        };

        if spec.flags & KeyFlags::OBSERVABLE == 0 {
            Err(ERR_PERMISSION)
        } else {
            Ok(())
        }
    }

    fn validate_keyref(keyref: u16) -> Result<(), u8> {
        if key_spec(keyref).is_some() {
            Ok(())
        } else {
            Err(ERR_UNKNOWN_KEYREF)
        }
    }

    fn uptime_seconds(&self) -> i64 {
        i64::try_from(self.started_at.elapsed().as_secs()).unwrap_or(i64::MAX)
    }

    fn schedule_mirror_update(self: &Arc<Self>, generation: u64) {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            time::sleep(Duration::from_millis(50)).await;
            state.flush_mirror_update(generation);
        });
    }

    fn flush_mirror_update(&self, generation: u64) {
        if self.mirror_generation.load(Ordering::SeqCst) != generation {
            return;
        }

        let next_value = recover_lock(&self.mirror_pending).clone();
        let changed = {
            let mut current = recover_lock(&self.mirror_out);
            let changed = current.as_str() != next_value;
            if changed {
                current.clear();
                current.push_str(&next_value);
            }
            changed
        };

        if changed {
            self.registry.broadcast_changed(KEY_MIRROR_OUT);
        }
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();
    let listener = TcpListener::bind((args.bind.as_str(), args.port)).await?;
    let registry = Arc::new(ConnectionRegistry::new());
    let state = SharedState::new(registry);
    state.start_background_tasks();

    loop {
        let (stream, _) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            run_connection(stream, state).await;
        });
    }
}

async fn run_connection(stream: TcpStream, state: Arc<SharedState>) {
    let (reader, writer) = stream.into_split();
    let (sender, receiver) = mpsc::unbounded_channel();
    let (connection_id, connection) = state.registry.register(sender);
    let writer_task = tokio::spawn(write_frames(writer, receiver));

    read_frames(reader, Arc::clone(&state), Arc::clone(&connection)).await;
    state.registry.unregister(connection_id);
    drop(connection);

    match writer_task.await {
        Ok(Ok(()) | Err(_)) | Err(_) => {}
    }
}

async fn write_frames(
    mut writer: OwnedWriteHalf,
    mut receiver: mpsc::UnboundedReceiver<Vec<u8>>,
) -> io::Result<()> {
    while let Some(frame) = receiver.recv().await {
        writer.write_all(frame.as_slice()).await?;
    }

    Ok(())
}

async fn read_frames(
    mut reader: OwnedReadHalf,
    state: Arc<SharedState>,
    connection: Arc<ConnectionHandle>,
) {
    let parser = FrameParser::new();
    let mut frame_buf = Vec::new();
    let mut payload_buf = Vec::new();

    loop {
        let Ok(Some(message)) = read_next_message(&mut reader, parser, &mut frame_buf, &mut payload_buf)
            .await
        else {
            break;
        };

        if !dispatch_message(&state, &connection, message) {
            break;
        }
    }
}

async fn read_next_message<'frame>(
    reader: &mut OwnedReadHalf,
    parser: FrameParser,
    frame_buf: &'frame mut Vec<u8>,
    payload_buf: &'frame mut Vec<u8>,
) -> Result<Option<MessageRef<'frame>>, ()> {
    let mut header = [0u8; 3];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(_) => return Err(()),
    }

    let payload_len = usize::from(u16::from_be_bytes([header[1], header[2]]));
    payload_buf.resize(payload_len, 0);

    if reader.read_exact(payload_buf.as_mut_slice()).await.is_err() {
        return Err(());
    }

    let mut checksum = [0u8; 1];
    if reader.read_exact(&mut checksum).await.is_err() {
        return Err(());
    }

    frame_buf.clear();
    frame_buf.extend_from_slice(&header);
    frame_buf.extend_from_slice(payload_buf.as_slice());
    frame_buf.extend_from_slice(&checksum);

    let frame = parser
        .parse_frame(qup_core::WireDirection::ClientToNode, frame_buf.as_slice())
        .map_err(|_frame_error| ())?;

    frame.decode_message().map(Some).map_err(|_payload_error| ())
}

fn dispatch_message(
    state: &Arc<SharedState>,
    connection: &Arc<ConnectionHandle>,
    message: MessageRef<'_>,
) -> bool {
    match message {
        MessageRef::CompatibilityRequest => connection.send(encode_caps()),
        MessageRef::Request(request) => dispatch_request(state, connection, request),
        MessageRef::OrdinaryResponse(_)
        | MessageRef::CompatibilityResponse { .. }
        | MessageRef::Error(_)
        | MessageRef::Changed { .. } => false,
    }
}

fn dispatch_request(
    state: &Arc<SharedState>,
    connection: &Arc<ConnectionHandle>,
    request: RequestRef<'_>,
) -> bool {
    match request {
        RequestRef::Ping => connection.send(encode_ok()),
        RequestRef::Identify => connection.send(encode_identified()),
        RequestRef::GetKeytabLen => connection.send(encode_keytab_len()),
        RequestRef::GetKey { keyref } => key_spec(keyref).map_or_else(
            || connection.send(encode_error(ERR_UNKNOWN_KEYREF)),
            |spec| connection.send(encode_key(spec)),
        ),
        RequestRef::Get { keyref } => match state.get_value(keyref) {
            Ok(value) => connection.send(encode_value_response(Opcode::VALUE, &value)),
            Err(code) => connection.send(encode_error(code)),
        },
        RequestRef::Write { keyref, value } => match state.write_value(keyref, value) {
            Ok(success) => {
                if !connection.send(encode_value_response(Opcode::WRITTEN, &success.value)) {
                    return false;
                }
                if let Some(changed_keyref) = success.changed_keyref {
                    state.registry.broadcast_changed(changed_keyref);
                }
                true
            }
            Err(code) => connection.send(encode_error(code)),
        },
        RequestRef::Observe { keyref } => match SharedState::can_observe(keyref) {
            Ok(()) => {
                if !connection.send(encode_ok()) {
                    return false;
                }
                connection.observe(keyref);
                true
            }
            Err(code) => connection.send(encode_error(code)),
        },
        RequestRef::Unobserve { keyref } => match SharedState::validate_keyref(keyref) {
            Ok(()) => {
                connection.unobserve(keyref);
                connection.send(encode_ok())
            }
            Err(code) => connection.send(encode_error(code)),
        },
    }
}

fn recover_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn key_mask(keyref: u16) -> Option<u16> {
    1u16.checked_shl(u32::from(keyref))
}

fn observable_change(keyref: u16, changed: bool) -> Option<u16> {
    (changed
        && key_spec(keyref)
            .is_some_and(|spec| spec.flags & KeyFlags::OBSERVABLE != 0))
        .then_some(keyref)
}

fn key_spec(keyref: u16) -> Option<KeySpec> {
    KEY_SPECS.get(usize::from(keyref)).copied()
}

const fn expect_i64(value: ValueRef<'_>) -> Result<i64, u8> {
    match value {
        ValueRef::I64(value) => Ok(value),
        ValueRef::Bool(_) | ValueRef::Str(_) => Err(ERR_TYPE_MISMATCH),
    }
}

fn expect_u64(value: ValueRef<'_>) -> Result<u64, u8> {
    let value = expect_i64(value)?;
    if value < 0 {
        return Err(ERR_TYPE_MISMATCH);
    }

    u64::try_from(value).map_err(|_conversion_error| ERR_TYPE_MISMATCH)
}

const fn expect_str(value: ValueRef<'_>) -> Result<&str, u8> {
    match value {
        ValueRef::Str(value) => Ok(value),
        ValueRef::Bool(_) | ValueRef::I64(_) => Err(ERR_TYPE_MISMATCH),
    }
}

#[expect(
    clippy::panic,
    reason = "response payload sizes are derived from protocol-bounded values and metadata"
)]
fn build_frame(opcode: Opcode, payload: &[u8]) -> Vec<u8> {
    let length = u16::try_from(payload.len())
        .unwrap_or_else(|_conversion_error| panic!("QUP payloads are bounded by the wire format"));
    let mut frame = Vec::with_capacity(payload.len().saturating_add(4));
    frame.push(opcode.as_u8());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    frame.push(compute_checksum(opcode, payload));
    frame
}

fn encode_ok() -> Vec<u8> {
    build_frame(Opcode::OK, &[])
}

fn encode_identified() -> Vec<u8> {
    let mut payload = Vec::new();
    push_str16(&mut payload, NODE_ID);
    build_frame(Opcode::IDENTIFIED, payload.as_slice())
}

fn encode_keytab_len() -> Vec<u8> {
    build_frame(Opcode::KEYTABLEN, &KEY_COUNT.to_be_bytes())
}

fn encode_key(spec: KeySpec) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(spec.flags);
    push_str16(&mut payload, spec.name);
    build_frame(Opcode::KEY, payload.as_slice())
}

fn encode_value_response(opcode: Opcode, value: &WireValue) -> Vec<u8> {
    let mut payload = Vec::new();
    push_value(&mut payload, value);
    build_frame(opcode, payload.as_slice())
}

fn encode_error(code: u8) -> Vec<u8> {
    build_frame(Opcode::ERROR, &[code])
}

fn encode_caps() -> Vec<u8> {
    let mut payload = Vec::new();
    push_str16(&mut payload, CAPS);
    build_frame(Opcode::CAPS, payload.as_slice())
}

fn encode_changed(keyref: u16) -> Vec<u8> {
    build_frame(Opcode::CHANGED, &keyref.to_be_bytes())
}

fn load_u64_wire(value: &AtomicU64) -> i64 {
    u64_to_wire(value.load(Ordering::SeqCst))
}

fn push_value(payload: &mut Vec<u8>, value: &WireValue) {
    match value {
        WireValue::I64(value) => {
            payload.push(ValueKind::I64.as_byte());
            payload.extend_from_slice(&value.to_be_bytes());
        }
        WireValue::Str(value) => {
            payload.push(ValueKind::Str.as_byte());
            push_str16(payload, value);
        }
    }
}

#[expect(
    clippy::panic,
    reason = "encoded strings originate from protocol-bounded values or static metadata"
)]
fn push_str16(payload: &mut Vec<u8>, value: &str) {
    let length = u16::try_from(value.len())
        .unwrap_or_else(|_conversion_error| panic!("QUP strings are bounded by the wire format"));
    payload.extend_from_slice(&length.to_be_bytes());
    payload.extend_from_slice(value.as_bytes());
}

fn u64_to_wire(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn initial_seed() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(
        0x5eed_1234_5678_9abc,
        |duration| duration.as_secs() ^ u64::from(duration.subsec_nanos()),
    )
}

const fn mix_u64(value: u64) -> u64 {
    let first = value ^ value.wrapping_shr(30);
    let second = first.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let third = second ^ second.wrapping_shr(27);
    let fourth = third.wrapping_mul(0x94d0_49bb_1331_11eb);
    fourth ^ fourth.wrapping_shr(31)
}