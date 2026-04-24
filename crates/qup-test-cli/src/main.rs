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
use qup::{Client, ClientError, FrameDirection, KeyInfo, Message, TcpClient, Value};
use qup_core::{KeyFlags, Opcode};

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

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();

    println!("connecting to {}", args.addr);
    let mut client: TcpClient = Client::connect(args.addr.as_str()).await?;
    install_frame_trace(&mut client);

    match args.command {
        Some(Command::Repl) | None => run_repl(&mut client).await?,
        Some(command) => {
            let _keep_running = execute_command(&mut client, command).await?;
        }
    }

    Ok(())
}

fn install_frame_trace(client: &mut TcpClient) {
    client.set_frame_trace(|direction, frame| {
        let label = match direction {
            FrameDirection::Tx => "tx",
            FrameDirection::Rx => "rx",
        };
        eprintln!("{label}\t{}", format_hex_bytes(frame));
    });
}

async fn run_repl(client: &mut TcpClient) -> io::Result<()> {
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
async fn execute_command(client: &mut TcpClient, command: Command) -> io::Result<bool> {
    match command {
        Command::Ping => {
            client.send_empty_request(Opcode::PING).await?;
            let message = read_request_response(client).await?;
            print_message(&message);
        }
        Command::Caps => {
            client.send_empty_request(Opcode::GETCAPS).await?;
            let message = read_request_response(client).await?;
            print_message(&message);
        }
        Command::Identify => {
            client.send_empty_request(Opcode::IDENTIFY).await?;
            let message = read_request_response(client).await?;
            print_message(&message);
        }
        Command::KeyCount => {
            let count = client.key_count().await.map_err(client_error)?;
            println!("{count}");
        }
        Command::ListKeys => {
            for key in client.list_keys().await.map_err(client_error)? {
                print_key_info(&key);
            }
        }
        Command::Key { key } => {
            let keyref = resolve_keyref(client, key.as_str()).await?;
            let key_info = client.key(keyref).await.map_err(client_error)?;
            print_key_info(&key_info);
        }
        Command::Get { key } => {
            let keyref = resolve_keyref(client, key.as_str()).await?;
            client.send_keyref_request(Opcode::GET, keyref).await?;
            let message = read_request_response(client).await?;
            print_command_message(keyref, &message);
        }
        Command::Write { key, kind, value } => {
            let keyref = resolve_keyref(client, key.as_str()).await?;
            let value = parse_input_value(kind, value.as_slice())?;
            client.send_write_request(keyref, &value).await?;
            let message = read_request_response(client).await?;
            print_command_message(keyref, &message);
        }
        Command::Observe { key } => {
            let keyref = resolve_keyref(client, key.as_str()).await?;
            client.send_keyref_request(Opcode::OBSERVE, keyref).await?;
            let message = read_request_response(client).await?;
            print_command_message(keyref, &message);
        }
        Command::Unobserve { key } => {
            let keyref = resolve_keyref(client, key.as_str()).await?;
            client
                .send_keyref_request(Opcode::UNOBSERVE, keyref)
                .await?;
            let message = read_request_response(client).await?;
            print_command_message(keyref, &message);
        }
        Command::Read { timeout_ms } => {
            if let Some(timeout_ms) = timeout_ms {
                match client
                    .next_message_timeout(Duration::from_millis(timeout_ms))
                    .await
                    .map_err(client_error)?
                {
                    Some(message) => print_message(&message),
                    None => println!("timeout"),
                }
            } else {
                let message = client.next_message().await.map_err(client_error)?;
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

async fn read_request_response(client: &mut TcpClient) -> io::Result<Message> {
    loop {
        let message = client.next_message().await.map_err(client_error)?;
        if matches!(message, Message::Changed(_)) {
            print_message(&message);
            continue;
        }
        return Ok(message);
    }
}

async fn resolve_keyref(client: &mut TcpClient, selector: &str) -> io::Result<u16> {
    if let Ok(keyref) = selector.parse::<u16>() {
        return Ok(keyref);
    }

    client
        .resolve_keyref_by_name(selector)
        .await
        .map_err(client_error)
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

fn parse_input_value(kind: ValueInputKind, raw: &[String]) -> io::Result<Value> {
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
            parse_bool(value.as_str()).map(Value::Bool)
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
                .map(Value::I64)
                .map_err(|error| io::Error::new(ErrorKind::InvalidInput, error.to_string()))
        }
        ValueInputKind::Str => Ok(Value::Str(raw.join(" "))),
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

fn print_command_message(keyref: u16, message: &Message) {
    match message {
        Message::Ok => println!("ok\t{keyref}"),
        Message::Value(value) => println!("value\t{keyref}\t{}", format_value(value)),
        Message::Written(value) => {
            println!("written\t{keyref}\t{}", format_value(value));
        }
        Message::Error(code) => println!("error\t{keyref}\t0x{code:02x}"),
        Message::Caps(_)
        | Message::Identified(_)
        | Message::KeytabLen(_)
        | Message::Key { .. }
        | Message::Changed(_) => print_message(message),
    }
}

fn print_message(message: &Message) {
    match message {
        Message::Ok => println!("ok"),
        Message::Caps(caps) => println!("caps\t{}", caps.escape_debug()),
        Message::Identified(node_id) => {
            println!("identified\t{}", node_id.escape_debug());
        }
        Message::KeytabLen(count) => println!("key-count\t{count}"),
        Message::Key { keyflags, name } => {
            println!("key\t{}\t{}", format_flags(*keyflags), name.escape_debug());
        }
        Message::Value(value) => println!("value\t{}", format_value(value)),
        Message::Written(value) => println!("written\t{}", format_value(value)),
        Message::Error(code) => println!("error\t0x{code:02x}"),
        Message::Changed(keyref) => println!("changed\t{keyref}"),
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

fn format_value(value: &Value) -> String {
    match value {
        Value::Bool(value) => format!("bool\t{value}"),
        Value::I64(value) => format!("i64\t{value}"),
        Value::Str(value) => format!("str\t{}", value.escape_debug()),
    }
}

fn client_error(error: ClientError) -> io::Error {
    match error {
        ClientError::Io(error) => error,
        ClientError::Protocol(message) => io::Error::new(ErrorKind::InvalidData, message),
        ClientError::RequestError { request, code } => io::Error::new(
            ErrorKind::InvalidData,
            format!("request {request} failed with error code 0x{code:02x}"),
        ),
        ClientError::UnexpectedMessage { expected, actual } => io::Error::new(
            ErrorKind::InvalidData,
            format!("expected {expected}, got {actual:?}"),
        ),
        ClientError::KeyNotFound(name) => {
            io::Error::new(ErrorKind::NotFound, format!("no key named {name:?}"))
        }
        ClientError::AmbiguousKey(name) => io::Error::new(
            ErrorKind::InvalidInput,
            format!("multiple keys named {name:?}; use a numeric keyref"),
        ),
    }
}
