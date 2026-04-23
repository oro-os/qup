//! Generic TCP client for interacting with QUP nodes.
//!
//! The client can run one-shot protocol commands or drop into a small REPL so
//! the same TCP connection can be reused for sequences like `observe` followed
//! by `read`.

#![allow(
    clippy::missing_docs_in_private_items,
    reason = "the executable is an internal utility and its private implementation details are intentionally undocumented"
)]
#![allow(
    clippy::print_stdout,
    reason = "the CLI is expected to print query results and REPL output to the terminal"
)]
#![allow(
    clippy::std_instead_of_alloc,
    reason = "the CLI intentionally uses std networking and console IO facilities"
)]
#![forbid(unsafe_code)]

use std::{
    io::{self, ErrorKind, Write as _},
    time::Duration,
};

use clap::{CommandFactory as _, Parser as ClapParser, Subcommand, ValueEnum};
use qup_core::{
    KeyFlags, MessageRef, Opcode, OrdinaryResponseRef, Parser as FrameParser, ValueKind,
    ValueRef, WireDirection, compute_checksum,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
    time,
};

#[derive(Debug, ClapParser)]
#[command(name = "qup-test-cli")]
struct Args {
    #[arg(short = 'a', long = "addr", default_value = "127.0.0.1:3400")]
    addr: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, ClapParser)]
struct LineArgs {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Subcommand)]
enum Command {
    Ping,
    #[command(alias = "getcaps")]
    Caps,
    Identify,
    #[command(alias = "getkeytablen")]
    KeyCount,
    ListKeys,
    #[command(alias = "getkey")]
    Key {
        key: String,
    },
    Get {
        key: String,
    },
    Write {
        key: String,
        kind: ValueInputKind,
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        value: Vec<String>,
    },
    Observe {
        key: String,
    },
    Unobserve {
        key: String,
    },
    Read {
        #[arg(long = "timeout-ms")]
        timeout_ms: Option<u64>,
    },
    Repl,
    #[command(alias = "quit")]
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ValueInputKind {
    Bool,
    I64,
    Str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnedValue {
    Bool(bool),
    I64(i64),
    Str(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnedMessage {
    Ok,
    Caps(String),
    Identified(String),
    KeytabLen(u16),
    Key {
        keyflags: KeyFlags,
        name: String,
    },
    Value(OwnedValue),
    Written(OwnedValue),
    Error(u8),
    Changed(u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyInfo {
    keyref: u16,
    keyflags: KeyFlags,
    name: String,
}

struct QupClient {
    stream: TcpStream,
    parser: FrameParser,
    frame_buf: Vec<u8>,
    payload_buf: Vec<u8>,
}

impl QupClient {
    async fn connect(addr: &str) -> io::Result<Self> {
        Ok(Self {
            stream: TcpStream::connect(addr).await?,
            parser: FrameParser::new(),
            frame_buf: Vec::new(),
            payload_buf: Vec::new(),
        })
    }

    async fn send_empty_request(&mut self, opcode: Opcode) -> io::Result<()> {
        self.send_frame(opcode, &[]).await
    }

    async fn send_keyref_request(&mut self, opcode: Opcode, keyref: u16) -> io::Result<()> {
        self.send_frame(opcode, &keyref.to_be_bytes()).await
    }

    async fn send_write(&mut self, keyref: u16, value: &OwnedValue) -> io::Result<()> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&keyref.to_be_bytes());
        push_value(&mut payload, value)?;
        self.send_frame(Opcode::WRITE, payload.as_slice()).await
    }

    async fn send_frame(&mut self, opcode: Opcode, payload: &[u8]) -> io::Result<()> {
        let frame = build_frame(opcode, payload)?;
        eprintln!("tx\t{}", format_hex_bytes(frame.as_slice()));
        self.stream.write_all(frame.as_slice()).await
    }

    async fn read_message(&mut self) -> io::Result<OwnedMessage> {
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

        eprintln!("rx\t{}", format_hex_bytes(self.frame_buf.as_slice()));

        let frame = self
            .parser
            .parse_frame(WireDirection::NodeToClient, self.frame_buf.as_slice())
            .map_err(frame_error)?;
        let message = frame
            .decode_message()
            .map_err(|payload_error_value| payload_error(&payload_error_value))?;
        owned_message(message)
    }

    async fn read_message_timeout(&mut self, duration: Duration) -> io::Result<Option<OwnedMessage>> {
        match time::timeout(duration, self.read_message()).await {
            Ok(message) => message.map(Some),
            Err(_elapsed) => Ok(None),
        }
    }

    async fn read_request_response(&mut self) -> io::Result<OwnedMessage> {
        loop {
            let message = self.read_message().await?;
            if matches!(message, OwnedMessage::Changed(_)) {
                print_message(&message);
                continue;
            }
            return Ok(message);
        }
    }

    async fn fetch_key_count(&mut self) -> io::Result<u16> {
        self.send_empty_request(Opcode::GETKEYTABLEN).await?;
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "protocol validation intentionally folds any non-KEYTABLEN frame into one error path"
        )]
        match self.read_request_response().await? {
            OwnedMessage::KeytabLen(count) => Ok(count),
            other => Err(unexpected_message("KEYTABLEN", &other)),
        }
    }

    async fn fetch_key_info(&mut self, keyref: u16) -> io::Result<KeyInfo> {
        self.send_keyref_request(Opcode::GETKEY, keyref).await?;
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "protocol validation intentionally folds any non-KEY frame into one error path"
        )]
        match self.read_request_response().await? {
            OwnedMessage::Key { keyflags, name } => Ok(KeyInfo {
                keyref,
                keyflags,
                name,
            }),
            other => Err(unexpected_message("KEY", &other)),
        }
    }

    async fn fetch_key_table(&mut self) -> io::Result<Vec<KeyInfo>> {
        let count = self.fetch_key_count().await?;
        let mut keys = Vec::with_capacity(usize::from(count));
        for keyref in 0..count {
            keys.push(self.fetch_key_info(keyref).await?);
        }
        Ok(keys)
    }

    async fn resolve_keyref(&mut self, selector: &str) -> io::Result<u16> {
        if let Ok(keyref) = selector.parse::<u16>() {
            return Ok(keyref);
        }

        let mut matches = self
            .fetch_key_table()
            .await?
            .into_iter()
            .filter(|key| key.name == selector);

        let Some(first) = matches.next() else {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                format!("no key named {selector:?}"),
            ));
        };

        if matches.next().is_some() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("multiple keys named {selector:?}; use a numeric keyref"),
            ));
        }

        Ok(first.keyref)
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();

    println!("connecting to {}", args.addr);
    let mut client = QupClient::connect(args.addr.as_str()).await?;

    match args.command {
        Some(Command::Repl) | None => run_repl(&mut client).await?,
        Some(command) => {
            let _keep_running = execute_command(&mut client, command).await?;
        }
    }

    Ok(())
}

async fn run_repl(client: &mut QupClient) -> io::Result<()> {
    println!("type 'help' for commands, 'exit' to quit");
    let stdin = io::stdin();
    let mut line = String::new();

    loop {
        let mut stdout = io::stdout();
        write!(stdout, "qup> ")?;
        stdout.flush()?;

        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            println!();
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let command = match parse_repl_command(trimmed) {
            Ok(command) => command,
            Err(error) => {
                if error.kind() != ErrorKind::Interrupted {
                    println!("{error}");
                }
                continue;
            }
        };

        if !execute_command(client, command).await? {
            break;
        }
    }

    Ok(())
}

#[expect(
    clippy::cognitive_complexity,
    reason = "the command dispatcher is a flat protocol verb table and is easier to maintain as one match"
)]
async fn execute_command(client: &mut QupClient, command: Command) -> io::Result<bool> {
    match command {
        Command::Ping => {
            client.send_empty_request(Opcode::PING).await?;
            let message = client.read_request_response().await?;
            print_message(&message);
        }
        Command::Caps => {
            client.send_empty_request(Opcode::GETCAPS).await?;
            let message = client.read_request_response().await?;
            print_message(&message);
        }
        Command::Identify => {
            client.send_empty_request(Opcode::IDENTIFY).await?;
            let message = client.read_request_response().await?;
            print_message(&message);
        }
        Command::KeyCount => {
            let count = client.fetch_key_count().await?;
            println!("{count}");
        }
        Command::ListKeys => {
            for key in client.fetch_key_table().await? {
                print_key_info(&key);
            }
        }
        Command::Key { key } => {
            let keyref = client.resolve_keyref(key.as_str()).await?;
            let key_info = client.fetch_key_info(keyref).await?;
            print_key_info(&key_info);
        }
        Command::Get { key } => {
            let keyref = client.resolve_keyref(key.as_str()).await?;
            client.send_keyref_request(Opcode::GET, keyref).await?;
            let message = client.read_request_response().await?;
            print_command_message(keyref, &message);
        }
        Command::Write { key, kind, value } => {
            let keyref = client.resolve_keyref(key.as_str()).await?;
            let value = parse_input_value(kind, value.as_slice())?;
            client.send_write(keyref, &value).await?;
            let message = client.read_request_response().await?;
            print_command_message(keyref, &message);
        }
        Command::Observe { key } => {
            let keyref = client.resolve_keyref(key.as_str()).await?;
            client.send_keyref_request(Opcode::OBSERVE, keyref).await?;
            let message = client.read_request_response().await?;
            print_command_message(keyref, &message);
        }
        Command::Unobserve { key } => {
            let keyref = client.resolve_keyref(key.as_str()).await?;
            client.send_keyref_request(Opcode::UNOBSERVE, keyref).await?;
            let message = client.read_request_response().await?;
            print_command_message(keyref, &message);
        }
        Command::Read { timeout_ms } => {
            if let Some(timeout_ms) = timeout_ms {
                match client
                    .read_message_timeout(Duration::from_millis(timeout_ms))
                    .await?
                {
                    Some(message) => print_message(&message),
                    None => println!("timeout"),
                }
            } else {
                let message = client.read_message().await?;
                print_message(&message);
            }
        }
        Command::Repl => {
            println!("already in repl");
        }
        Command::Exit => return Ok(false),
    }

    Ok(true)
}

fn parse_repl_command(line: &str) -> io::Result<Command> {
    let Some(tokens) = shlex::split(line) else {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "unmatched quote in input",
        ));
    };

    if tokens.len() == 1 && tokens.first().is_some_and(|token| token == "help") {
        print_repl_help()?;
        return Err(io::Error::new(ErrorKind::Interrupted, ""));
    }

    let mut argv = Vec::with_capacity(tokens.len().saturating_add(1));
    argv.push(String::from("qup-test-cli"));
    argv.extend(tokens);

    LineArgs::try_parse_from(argv)
        .map(|args| args.command)
        .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))
}

fn print_repl_help() -> io::Result<()> {
    let mut command = LineArgs::command();
    command.write_long_help(&mut io::stdout())?;
    println!();
    Ok(())
}

fn parse_input_value(kind: ValueInputKind, raw: &[String]) -> io::Result<OwnedValue> {
    if raw.is_empty() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "missing value bytes",
        ));
    }

    match kind {
        ValueInputKind::Bool => {
            if raw.len() != 1 {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "bool values take exactly one token",
                ));
            }
            let value = raw
                .first()
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "missing bool token"))?;
            parse_bool(value.as_str()).map(OwnedValue::Bool)
        }
        ValueInputKind::I64 => {
            if raw.len() != 1 {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "i64 values take exactly one token",
                ));
            }
            raw.first()
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "missing i64 token"))?
                .parse::<i64>()
                .map(OwnedValue::I64)
                .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))
        }
        ValueInputKind::Str => Ok(OwnedValue::Str(raw.join(" "))),
    }
}

fn parse_bool(raw: &str) -> io::Result<bool> {
    match raw {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid bool literal {raw:?}; use true, false, 1, or 0"),
        )),
    }
}

fn print_key_info(key: &KeyInfo) {
    println!(
        "{}\t{}\t{}",
        key.keyref,
        format_flags(key.keyflags),
        key.name.escape_debug()
    );
}

fn print_command_message(keyref: u16, message: &OwnedMessage) {
    match message {
        OwnedMessage::Ok => println!("ok\t{keyref}"),
        OwnedMessage::Value(value) => println!("value\t{keyref}\t{}", format_value(value)),
        OwnedMessage::Written(value) => {
            println!("written\t{keyref}\t{}", format_value(value));
        }
        OwnedMessage::Error(code) => println!("error\t{keyref}\t0x{code:02x}"),
        OwnedMessage::Caps(_)
        | OwnedMessage::Identified(_)
        | OwnedMessage::KeytabLen(_)
        | OwnedMessage::Key { .. }
        | OwnedMessage::Changed(_) => print_message(message),
    }
}

fn print_message(message: &OwnedMessage) {
    match message {
        OwnedMessage::Ok => println!("ok"),
        OwnedMessage::Caps(caps) => println!("caps\t{}", caps.escape_debug()),
        OwnedMessage::Identified(node_id) => {
            println!("identified\t{}", node_id.escape_debug());
        }
        OwnedMessage::KeytabLen(count) => println!("key-count\t{count}"),
        OwnedMessage::Key { keyflags, name } => {
            println!("key\t{}\t{}", format_flags(*keyflags), name.escape_debug());
        }
        OwnedMessage::Value(value) => println!("value\t{}", format_value(value)),
        OwnedMessage::Written(value) => println!("written\t{}", format_value(value)),
        OwnedMessage::Error(code) => println!("error\t0x{code:02x}"),
        OwnedMessage::Changed(keyref) => println!("changed\t{keyref}"),
    }
}

fn format_flags(flags: KeyFlags) -> String {
    let readable = if flags.is_readable() { 'r' } else { '-' };
    let writable = if flags.is_writable() { 'w' } else { '-' };
    let observable = if flags.is_observable() { 'o' } else { '-' };
    format!("{readable}{writable}{observable}")
}

fn format_hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_value(value: &OwnedValue) -> String {
    match value {
        OwnedValue::Bool(value) => format!("bool\t{value}"),
        OwnedValue::I64(value) => format!("i64\t{value}"),
        OwnedValue::Str(value) => format!("str\t{}", value.escape_debug()),
    }
}

fn build_frame(opcode: Opcode, payload: &[u8]) -> io::Result<Vec<u8>> {
    let payload_len = u16::try_from(payload.len()).map_err(|_conversion_error| {
        io::Error::new(ErrorKind::InvalidInput, "payload exceeds u16 wire length")
    })?;
    let mut frame = Vec::with_capacity(payload.len().saturating_add(4));
    frame.push(opcode.as_u8());
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(payload);
    frame.push(compute_checksum(opcode, payload));
    Ok(frame)
}

fn owned_message(message: MessageRef<'_>) -> io::Result<OwnedMessage> {
    match message {
        MessageRef::OrdinaryResponse(ordinary) => Ok(owned_ordinary_message(ordinary)),
        MessageRef::CompatibilityResponse { caps } => {
            Ok(OwnedMessage::Caps(caps.as_str().to_owned()))
        }
        MessageRef::Error(error) => Ok(OwnedMessage::Error(error.code())),
        MessageRef::Changed { keyref } => Ok(OwnedMessage::Changed(keyref)),
        MessageRef::Request(_) | MessageRef::CompatibilityRequest => {
            Err(protocol_error("node sent a client-direction message"))
        }
    }
}

fn owned_ordinary_message(message: OrdinaryResponseRef<'_>) -> OwnedMessage {
    match message {
        OrdinaryResponseRef::Ok => OwnedMessage::Ok,
        OrdinaryResponseRef::Identified { nodeid } => OwnedMessage::Identified(nodeid.to_owned()),
        OrdinaryResponseRef::KeytabLen { count } => OwnedMessage::KeytabLen(count),
        OrdinaryResponseRef::Key { keyflags, name } => OwnedMessage::Key {
            keyflags,
            name: name.to_owned(),
        },
        OrdinaryResponseRef::Value { value } => OwnedMessage::Value(owned_value(value)),
        OrdinaryResponseRef::Written { value } => OwnedMessage::Written(owned_value(value)),
    }
}

fn owned_value(value: ValueRef<'_>) -> OwnedValue {
    match value {
        ValueRef::Bool(value) => OwnedValue::Bool(value),
        ValueRef::I64(value) => OwnedValue::I64(value),
        ValueRef::Str(value) => OwnedValue::Str(value.to_owned()),
    }
}

fn push_value(payload: &mut Vec<u8>, value: &OwnedValue) -> io::Result<()> {
    match value {
        OwnedValue::Bool(value) => {
            payload.push(ValueKind::Bool.as_byte());
            payload.push(u8::from(*value));
        }
        OwnedValue::I64(value) => {
            payload.push(ValueKind::I64.as_byte());
            payload.extend_from_slice(&value.to_be_bytes());
        }
        OwnedValue::Str(value) => {
            payload.push(ValueKind::Str.as_byte());
            push_str16(payload, value)?;
        }
    }

    Ok(())
}

fn push_str16(payload: &mut Vec<u8>, value: &str) -> io::Result<()> {
    let length = u16::try_from(value.len()).map_err(|_conversion_error| {
        io::Error::new(ErrorKind::InvalidInput, "string exceeds u16 wire length")
    })?;
    payload.extend_from_slice(&length.to_be_bytes());
    payload.extend_from_slice(value.as_bytes());
    Ok(())
}

fn unexpected_message(expected: &str, message: &OwnedMessage) -> io::Error {
    protocol_error(format!("expected {expected}, got {message:?}"))
}

fn protocol_error(message: impl Into<String>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.into())
}

fn frame_error(error: qup_core::FrameError) -> io::Error {
    protocol_error(format!("frame validation failed: {error}"))
}

fn payload_error(error: &qup_core::PayloadError) -> io::Error {
    protocol_error(format!("payload validation failed: {error}"))
}