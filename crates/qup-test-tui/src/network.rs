use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use qup::{Client, KeyInfo, Message, TcpClient, Value};
use qup_core::Opcode;
use tokio::{sync::mpsc, time};

use crate::app::{KeyRow, NetworkEvent, WorkerCommand};
use crate::ui::format_value;

pub(crate) const NETWORK_POLL: Duration = Duration::from_secs(1);
pub(crate) const HEARTBEAT_IDLE: Duration = Duration::from_secs(3);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
enum WriteCommandError {
    Request(String),
    Connection(String),
}

pub(crate) async fn connection_manager(
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
        }

        if row.info.keyflags.is_readable() {
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
            WorkerCommand::WriteValue { keyref, value } => {
                match write_value_with_changes(client, keyref, &value).await {
                    Ok((written, changed)) => {
                        enqueue_changed_keys(pending_changed, changed.as_slice());
                        tx.send(NetworkEvent::WriteSucceeded {
                            keyref,
                            value: format_value(&written),
                            last_read: Instant::now(),
                        })
                        .map_err(|_closed| String::from("viewer closed"))?;
                    }
                    Err(WriteCommandError::Request(error)) => {
                        tx.send(NetworkEvent::WriteFailed { keyref, error })
                            .map_err(|_closed| String::from("viewer closed"))?;
                    }
                    Err(WriteCommandError::Connection(error)) => return Err(error),
                }
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

async fn write_value_with_changes(
    client: &mut TcpClient,
    keyref: u16,
    value: &Value,
) -> Result<(Value, Vec<u16>), WriteCommandError> {
    client
        .send_write_request(keyref, value)
        .await
        .map_err(|error| WriteCommandError::Connection(format!("send failed: {error}")))?;
    wait_for_written_with_changes(client).await
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

async fn wait_for_written_with_changes(
    client: &mut TcpClient,
) -> Result<(Value, Vec<u16>), WriteCommandError> {
    let started = Instant::now();
    let mut changed = Vec::new();

    loop {
        let remaining = REQUEST_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(WriteCommandError::Connection(String::from(
                "timed out waiting for WRITTEN",
            )));
        }

        match client.next_message_timeout(remaining).await {
            Ok(Some(Message::Written(value))) => return Ok((value, changed)),
            Ok(Some(Message::Changed(keyref))) => enqueue_changed_key(&mut changed, keyref),
            Ok(Some(Message::Error(code))) => {
                return Err(WriteCommandError::Request(format!(
                    "request failed with error code 0x{code:02x}"
                )));
            }
            Ok(Some(other)) => {
                return Err(WriteCommandError::Connection(format!(
                    "unexpected response: {other:?}"
                )));
            }
            Ok(None) => {
                return Err(WriteCommandError::Connection(String::from(
                    "timed out waiting for response",
                )));
            }
            Err(error) => return Err(WriteCommandError::Connection(error.to_string())),
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

pub(crate) fn initial_display_value(key: &KeyInfo) -> String {
    if !key.keyflags.is_readable() {
        return unreadable_text(key);
    }

    if key.keyflags.is_observable() {
        return String::from("<waiting for update>");
    }

    String::from("<loading>")
}

pub(crate) fn unreadable_text(key: &KeyInfo) -> String {
    if key.keyflags.is_observable() {
        return String::from("<observable; unreadable>");
    }

    String::from("<unreadable>")
}

#[cfg(test)]
mod tests {
    use super::{initial_display_value, unreadable_text};
    use qup::KeyInfo;
    use qup_core::KeyFlags;

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
}
