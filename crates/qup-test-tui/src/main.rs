//! ratatui-based live monitor for QUP nodes.
//!
//! The TUI stays open across disconnects, retries the configured address, and
//! repopulates a live key table whenever a connection is re-established.

#![allow(
    clippy::missing_docs_in_private_items,
    reason = "the executable is an internal utility and its private implementation details are intentionally undocumented"
)]
#![allow(
    clippy::std_instead_of_alloc,
    reason = "the TUI intentionally uses std terminal IO and collections"
)]
#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    io,
    time::{Duration, Instant},
};

use clap::Parser as ClapParser;
use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use qup::{Client, KeyInfo, Message, TcpClient, Value};
use qup_core::{KeyFlags, Opcode};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph, Row, Table},
};
use tokio::{sync::mpsc, time};

const DEFAULT_ADDR: &str = "127.0.0.1:3400";
const DEFAULT_RETRY_SECS: u64 = 3;
const INPUT_POLL: Duration = Duration::from_millis(100);
const LAST_READ_TICK: Duration = Duration::from_secs(1);
const NETWORK_POLL: Duration = Duration::from_secs(1);
const HEARTBEAT_IDLE: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, ClapParser)]
#[command(name = "qup-test-tui")]
struct Args {
    #[arg(short = 'a', long = "addr", default_value = DEFAULT_ADDR)]
    addr: String,
    #[arg(long = "retry-secs", default_value_t = DEFAULT_RETRY_SECS)]
    retry_secs: u64,
}

#[derive(Debug, Clone)]
struct KeyRow {
    info: KeyInfo,
    value: String,
    last_read: Option<Instant>,
}

#[derive(Debug, Clone)]
enum AppStatus {
    Connecting,
    Connected,
    Disconnected { error: String },
}

#[derive(Debug)]
struct App {
    addr: String,
    retry_secs: u64,
    status: AppStatus,
    node_id: Option<String>,
    keys: Vec<KeyRow>,
}

impl App {
    fn new(addr: String, retry_secs: u64) -> Self {
        Self {
            addr,
            retry_secs,
            status: AppStatus::Connecting,
            node_id: None,
            keys: Vec::new(),
        }
    }

    fn apply(&mut self, update: NetworkEvent) {
        match update {
            NetworkEvent::Connecting => {
                self.status = AppStatus::Connecting;
                self.node_id = None;
            }
            NetworkEvent::Connected { node_id, keys } => {
                self.status = AppStatus::Connected;
                self.node_id = Some(node_id);
                self.keys = keys;
            }
            NetworkEvent::Disconnected { error } => {
                self.status = AppStatus::Disconnected { error };
                self.node_id = None;
            }
            NetworkEvent::ValueUpdated {
                keyref,
                value,
                last_read,
            } => {
                if let Some(key) = self.keys.iter_mut().find(|key| key.info.keyref == keyref) {
                    key.value = value;
                    key.last_read = last_read;
                }
            }
        }
    }

    fn status_line(&self) -> String {
        match &self.status {
            AppStatus::Connecting => {
                format!("connecting to {}", self.addr)
            }
            AppStatus::Connected => {
                let node = self.node_id.as_deref().unwrap_or("<unknown>");
                format!(
                    "connected to {} as {} | {} keys | {} observable | press r to reread, q to quit",
                    self.addr,
                    node,
                    self.keys.len(),
                    self.keys
                        .iter()
                        .filter(|key| key.info.keyflags.is_observable())
                        .count()
                )
            }
            AppStatus::Disconnected { error } => {
                format!(
                    "disconnected from {}: {} | retrying every {}s | press r to queue reread, q to quit",
                    self.addr, error, self.retry_secs
                )
            }
        }
    }
}

#[derive(Debug, Clone)]
enum NetworkEvent {
    Connecting,
    Connected {
        node_id: String,
        keys: Vec<KeyRow>,
    },
    Disconnected {
        error: String,
    },
    ValueUpdated {
        keyref: u16,
        value: String,
        last_read: Option<Instant>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerCommand {
    RefreshAllReadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiAction {
    None,
    Quit,
    RefreshAllReadable,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let worker = tokio::spawn(connection_manager(
        args.addr.clone(),
        Duration::from_secs(args.retry_secs),
        tx,
        command_rx,
    ));

    let mut terminal = Tui::new()?;
    let mut app = App::new(args.addr, args.retry_secs);
    let mut last_clock_redraw = Instant::now();
    let mut needs_redraw = true;

    loop {
        while let Ok(update) = rx.try_recv() {
            app.apply(update);
            needs_redraw = true;
        }

        let now = Instant::now();
        if now.duration_since(last_clock_redraw) >= LAST_READ_TICK {
            last_clock_redraw = now;
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|frame| draw(frame, &app, now))?;
            needs_redraw = false;
        }

        match poll_input(INPUT_POLL)? {
            UiAction::None => {}
            UiAction::Quit => break,
            UiAction::RefreshAllReadable => {
                let _sent = command_tx.send(WorkerCommand::RefreshAllReadable);
                needs_redraw = true;
            }
        }
    }

    worker.abort();
    let _ = worker.await;

    Ok(())
}

async fn connection_manager(
    addr: String,
    retry_delay: Duration,
    tx: mpsc::UnboundedSender<NetworkEvent>,
    mut command_rx: mpsc::UnboundedReceiver<WorkerCommand>,
) {
    loop {
        if tx.send(NetworkEvent::Connecting).is_err() {
            return;
        }

        let result = run_connection(addr.as_str(), &tx, &mut command_rx).await;
        let error = match result {
            Ok(()) => String::from("connection closed"),
            Err(error) => error,
        };

        if tx.send(NetworkEvent::Disconnected { error }).is_err() {
            return;
        }

        time::sleep(retry_delay).await;
    }
}

async fn run_connection(
    addr: &str,
    tx: &mpsc::UnboundedSender<NetworkEvent>,
    command_rx: &mut mpsc::UnboundedReceiver<WorkerCommand>,
) -> Result<(), String> {
    let mut client: TcpClient = Client::connect(addr)
        .await
        .map_err(|error| format!("connect failed: {error}"))?;

    let node_id = client
        .identify()
        .await
        .map_err(|error| format!("identify failed: {error}"))?;
    let keys = client
        .list_keys()
        .await
        .map_err(|error| format!("key enumeration failed: {error}"))?;

    let mut keys_by_ref = BTreeMap::new();
    let mut rows = Vec::with_capacity(keys.len());
    let mut pending_changed = Vec::new();
    for key in keys {
        keys_by_ref.insert(key.keyref, key.clone());
        rows.push(KeyRow {
            value: initial_display_value(&key),
            info: key,
            last_read: None,
        });
    }

    for row in &mut rows {
        if row.info.keyflags.is_observable() {
            let changed = observe_with_changes(&mut client, row.info.keyref)
                .await
                .map_err(|error| format!("observe {} failed: {error}", row.info.name))?;
            enqueue_changed_keys(&mut pending_changed, changed.as_slice());
        } else if row.info.keyflags.is_readable() {
            let (value, changed) = read_value_with_changes(&mut client, row.info.keyref)
                .await
                .map_err(|error| format!("initial read {} failed: {error}", row.info.name))?;
            row.value = format_value(&value);
            row.last_read = Some(Instant::now());
            enqueue_changed_keys(&mut pending_changed, changed.as_slice());
        }
    }

    tx.send(NetworkEvent::Connected {
        node_id,
        keys: rows,
    })
    .map_err(|_closed| String::from("viewer closed"))?;

    let mut last_traffic = Instant::now();
    drain_changed_keys(&mut client, &keys_by_ref, tx, &mut pending_changed).await?;

    loop {
        process_commands(
            &mut client,
            &keys_by_ref,
            tx,
            command_rx,
            &mut pending_changed,
        )
        .await?;
        drain_changed_keys(&mut client, &keys_by_ref, tx, &mut pending_changed).await?;

        match client.next_message_timeout(NETWORK_POLL).await {
            Ok(Some(Message::Changed(keyref))) => {
                enqueue_changed_key(&mut pending_changed, keyref);
                last_traffic = Instant::now();
            }
            Ok(Some(
                Message::Ok
                | Message::Caps(_)
                | Message::Identified(_)
                | Message::KeytabLen(_)
                | Message::Key { .. }
                | Message::Value(_)
                | Message::Written(_)
                | Message::Error(_),
            )) => return Err(String::from("unexpected unsolicited response from node")),
            Ok(None) => {
                if last_traffic.elapsed() >= HEARTBEAT_IDLE {
                    let changed = ping_with_changes(&mut client)
                        .await
                        .map_err(|error| format!("heartbeat failed: {error}"))?;
                    enqueue_changed_keys(&mut pending_changed, changed.as_slice());
                    last_traffic = Instant::now();
                }
            }
            Err(error) => return Err(format!("connection lost: {error}")),
        }
    }
}

async fn process_commands(
    client: &mut TcpClient,
    keys_by_ref: &BTreeMap<u16, KeyInfo>,
    tx: &mpsc::UnboundedSender<NetworkEvent>,
    command_rx: &mut mpsc::UnboundedReceiver<WorkerCommand>,
    pending_changed: &mut Vec<u16>,
) -> Result<(), String> {
    while let Ok(command) = command_rx.try_recv() {
        match command {
            WorkerCommand::RefreshAllReadable => {
                reread_all_readable_keys(client, keys_by_ref, tx, pending_changed).await?;
            }
        }
    }

    Ok(())
}

async fn reread_all_readable_keys(
    client: &mut TcpClient,
    keys_by_ref: &BTreeMap<u16, KeyInfo>,
    tx: &mpsc::UnboundedSender<NetworkEvent>,
    pending_changed: &mut Vec<u16>,
) -> Result<(), String> {
    for key in keys_by_ref.values() {
        if !key.keyflags.is_readable() {
            continue;
        }

        let (value, changed) = read_value_with_changes(client, key.keyref)
            .await
            .map_err(|error| format!("refresh {} failed: {error}", key.name))?;
        enqueue_changed_keys(pending_changed, changed.as_slice());
        tx.send(NetworkEvent::ValueUpdated {
            keyref: key.keyref,
            value: format_value(&value),
            last_read: Some(Instant::now()),
        })
        .map_err(|_closed| String::from("viewer closed"))?;
    }

    Ok(())
}

async fn drain_changed_keys(
    client: &mut TcpClient,
    keys_by_ref: &BTreeMap<u16, KeyInfo>,
    tx: &mpsc::UnboundedSender<NetworkEvent>,
    pending_changed: &mut Vec<u16>,
) -> Result<(), String> {
    while let Some(keyref) = pending_changed.pop() {
        let Some(key) = keys_by_ref.get(&keyref) else {
            continue;
        };

        if key.keyflags.is_readable() {
            let (value, nested) = read_value_with_changes(client, keyref)
                .await
                .map_err(|error| format!("refresh {} failed: {error}", key.name))?;
            enqueue_changed_keys(pending_changed, nested.as_slice());
            tx.send(NetworkEvent::ValueUpdated {
                keyref,
                value: format_value(&value),
                last_read: Some(Instant::now()),
            })
            .map_err(|_closed| String::from("viewer closed"))?;
        } else {
            tx.send(NetworkEvent::ValueUpdated {
                keyref,
                value: String::from("<changed; unreadable>"),
                last_read: None,
            })
            .map_err(|_closed| String::from("viewer closed"))?;
        }
    }

    Ok(())
}

async fn observe_with_changes(client: &mut TcpClient, keyref: u16) -> Result<Vec<u16>, String> {
    client
        .send_keyref_request(Opcode::OBSERVE, keyref)
        .await
        .map_err(|error| format!("send failed: {error}"))?;
    wait_for_ok_with_changes(client).await
}

async fn read_value_with_changes(
    client: &mut TcpClient,
    keyref: u16,
) -> Result<(Value, Vec<u16>), String> {
    client
        .send_keyref_request(Opcode::GET, keyref)
        .await
        .map_err(|error| format!("send failed: {error}"))?;
    wait_for_value_with_changes(client).await
}

async fn ping_with_changes(client: &mut TcpClient) -> Result<Vec<u16>, String> {
    client
        .send_empty_request(Opcode::PING)
        .await
        .map_err(|error| format!("send failed: {error}"))?;
    wait_for_ok_with_changes(client).await
}

async fn wait_for_ok_with_changes(client: &mut TcpClient) -> Result<Vec<u16>, String> {
    let started = Instant::now();
    let mut changed = Vec::new();

    loop {
        let remaining = REQUEST_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(String::from("timed out waiting for OK"));
        }

        match client.next_message_timeout(remaining).await {
            Ok(Some(Message::Ok)) => return Ok(changed),
            Ok(Some(Message::Changed(keyref))) => enqueue_changed_key(&mut changed, keyref),
            Ok(Some(Message::Error(code))) => {
                return Err(format!("request failed with error code 0x{code:02x}"));
            }
            Ok(Some(other)) => return Err(format!("unexpected response: {other:?}")),
            Ok(None) => return Err(String::from("timed out waiting for response")),
            Err(error) => return Err(error.to_string()),
        }
    }
}

async fn wait_for_value_with_changes(client: &mut TcpClient) -> Result<(Value, Vec<u16>), String> {
    let started = Instant::now();
    let mut changed = Vec::new();

    loop {
        let remaining = REQUEST_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(String::from("timed out waiting for VALUE"));
        }

        match client.next_message_timeout(remaining).await {
            Ok(Some(Message::Value(value))) => return Ok((value, changed)),
            Ok(Some(Message::Changed(keyref))) => enqueue_changed_key(&mut changed, keyref),
            Ok(Some(Message::Error(code))) => {
                return Err(format!("request failed with error code 0x{code:02x}"));
            }
            Ok(Some(other)) => return Err(format!("unexpected response: {other:?}")),
            Ok(None) => return Err(String::from("timed out waiting for response")),
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn enqueue_changed_keys(pending_changed: &mut Vec<u16>, keyrefs: &[u16]) {
    for &keyref in keyrefs {
        enqueue_changed_key(pending_changed, keyref);
    }
}

fn enqueue_changed_key(pending_changed: &mut Vec<u16>, keyref: u16) {
    if !pending_changed.contains(&keyref) {
        pending_changed.push(keyref);
    }
}

fn initial_display_value(key: &KeyInfo) -> String {
    if !key.keyflags.is_readable() {
        return unreadable_text(key);
    }

    if key.keyflags.is_observable() {
        return String::from("<waiting for update>");
    }

    String::from("<loading>")
}

fn unreadable_text(key: &KeyInfo) -> String {
    if key.keyflags.is_observable() {
        return String::from("<observable; unreadable>");
    }

    String::from("<unreadable>")
}

fn poll_input(timeout: Duration) -> io::Result<UiAction> {
    if !event::poll(timeout)? {
        return Ok(UiAction::None);
    }

    match event::read()? {
        CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char('q') => Ok(UiAction::Quit),
            KeyCode::Char('r') | KeyCode::Char('R') => Ok(UiAction::RefreshAllReadable),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Ok(UiAction::Quit)
            }
            _ => Ok(UiAction::None),
        },
        _ => Ok(UiAction::None),
    }
}

fn draw(frame: &mut Frame<'_>, app: &App, now: Instant) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(frame.area());

    let status = Paragraph::new(app.status_line())
        .block(Block::default().title("Connection").borders(Borders::ALL));
    frame.render_widget(status, layout[0]);

    let header = Row::new(["Key", "Flags", "Name", "Value", "Last Read"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = app.keys.iter().map(|key| {
        Row::new([
            key.info.keyref.to_string(),
            format_flags(key.info.keyflags),
            key.info.name.clone(),
            key.value.clone(),
            format_last_read(key.last_read, now),
        ])
    });
    let table = Table::default()
        .header(header)
        .rows(rows)
        .block(Block::default().title("Keys").borders(Borders::ALL))
        .column_spacing(1)
        .widths([
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(22),
            Constraint::Min(18),
            Constraint::Length(10),
        ]);
    frame.render_widget(table, layout[1]);
}

fn format_flags(flags: KeyFlags) -> String {
    let readable = if flags.is_readable() { 'r' } else { '-' };
    let writable = if flags.is_writable() { 'w' } else { '-' };
    let observable = if flags.is_observable() { 'o' } else { '-' };
    format!("{readable}{writable}{observable}")
}

fn format_last_read(last_read: Option<Instant>, now: Instant) -> String {
    let Some(last_read) = last_read else {
        return String::from("never");
    };

    let elapsed = now.saturating_duration_since(last_read).as_secs();
    if elapsed < 60 {
        return format!("{elapsed}s");
    }

    let minutes = elapsed / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }

    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h");
    }

    format!("{}d", hours / 24)
}

fn format_value(value: &Value) -> String {
    match value {
        Value::Bool(value) => value.to_string(),
        Value::I64(value) => value.to_string(),
        Value::Str(value) => value.escape_debug().to_string(),
    }
}

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

struct Tui {
    terminal: TuiTerminal,
}

impl Tui {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;

        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        terminal.hide_cursor()?;

        Ok(Self { terminal })
    }

    fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<()> {
        self.terminal.draw(render).map(|_completed| ())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        format_flags, format_last_read, format_value, initial_display_value, unreadable_text,
    };
    use qup::{KeyInfo, Value};
    use qup_core::KeyFlags;

    #[test]
    fn format_flags_matches_cli_convention() {
        let flags = KeyFlags::new(KeyFlags::READABLE | KeyFlags::OBSERVABLE)
            .expect("test flags should be valid");
        assert_eq!(format_flags(flags), "r-o");
    }

    #[test]
    fn format_value_uses_plain_strings() {
        assert_eq!(format_value(&Value::Bool(true)), "true");
        assert_eq!(format_value(&Value::I64(42)), "42");
        assert_eq!(format_value(&Value::Str(String::from("a\nb"))), "a\\nb");
    }

    #[test]
    fn unreadable_text_marks_observable_keys() {
        let observable = KeyInfo {
            keyref: 0,
            keyflags: KeyFlags::new(KeyFlags::OBSERVABLE).expect("flags should be valid"),
            name: String::from("foo"),
        };
        let hidden = KeyInfo {
            keyref: 1,
            keyflags: KeyFlags::new(0).expect("flags should be valid"),
            name: String::from("bar"),
        };

        assert_eq!(unreadable_text(&observable), "<observable; unreadable>");
        assert_eq!(unreadable_text(&hidden), "<unreadable>");
    }

    #[test]
    fn initial_display_for_observable_readable_key_waits_for_update() {
        let key = KeyInfo {
            keyref: 2,
            keyflags: KeyFlags::new(KeyFlags::READABLE | KeyFlags::OBSERVABLE)
                .expect("flags should be valid"),
            name: String::from("voltage"),
        };

        assert_eq!(initial_display_value(&key), "<waiting for update>");
    }

    #[test]
    fn last_read_is_humanized() {
        let now = Instant::now();
        assert_eq!(format_last_read(None, now), "never");
        assert_eq!(
            format_last_read(now.checked_sub(Duration::from_secs(2)), now),
            "2s"
        );
        assert_eq!(
            format_last_read(now.checked_sub(Duration::from_secs(5 * 60 + 3)), now),
            "5m"
        );
    }
}
