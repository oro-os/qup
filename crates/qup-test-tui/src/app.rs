use std::{
    io,
    time::{Duration, Instant},
};

use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use qup::{KeyInfo, Value};
use tokio::sync::mpsc;

use crate::ui::{ModalClickTarget, ModalFocus, UiGeometry};

#[derive(Debug, Clone)]
pub(crate) struct KeyRow {
    pub(crate) info: KeyInfo,
    pub(crate) value: String,
    pub(crate) last_read: Option<Instant>,
}

#[derive(Debug, Clone)]
pub(crate) enum AppStatus {
    Connecting,
    Connected,
    Disconnected { error: String },
}

#[derive(Debug, Clone)]
pub(crate) struct EditModal {
    pub(crate) keyref: u16,
    pub(crate) key_name: String,
    pub(crate) current_value: String,
    pub(crate) string_input: String,
    pub(crate) number_input: String,
    pub(crate) focus: ModalFocus,
    pub(crate) error: Option<String>,
    pub(crate) submitting: bool,
}

impl EditModal {
    pub(crate) fn from_row(row: &KeyRow) -> Self {
        let number_input = row
            .value
            .parse::<i64>()
            .map(|_value| row.value.clone())
            .unwrap_or_default();

        Self {
            keyref: row.info.keyref,
            key_name: row.info.name.clone(),
            current_value: row.value.clone(),
            string_input: String::new(),
            number_input,
            focus: ModalFocus::StringInput,
            error: None,
            submitting: false,
        }
    }

    pub(crate) fn mark_submitting(&mut self) {
        self.error = None;
        self.submitting = true;
    }

    pub(crate) fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
        self.submitting = false;
    }
}

#[derive(Debug)]
pub(crate) struct App {
    pub(crate) addr: String,
    pub(crate) retry_secs: u64,
    pub(crate) status: AppStatus,
    pub(crate) node_id: Option<String>,
    pub(crate) keys: Vec<KeyRow>,
    pub(crate) modal: Option<EditModal>,
}

impl App {
    pub(crate) fn new(addr: String, retry_secs: u64) -> Self {
        Self {
            addr,
            retry_secs,
            status: AppStatus::Connecting,
            node_id: None,
            keys: Vec::new(),
            modal: None,
        }
    }

    pub(crate) fn apply(&mut self, update: NetworkEvent) {
        match update {
            NetworkEvent::Connecting => {
                self.status = AppStatus::Connecting;
                self.node_id = None;
                self.modal = None;
            }
            NetworkEvent::Connected { node_id, keys } => {
                self.status = AppStatus::Connected;
                self.node_id = Some(node_id);
                self.keys = keys;
                self.modal = None;
            }
            NetworkEvent::Disconnected { error } => {
                self.status = AppStatus::Disconnected { error };
                self.node_id = None;
                self.modal = None;
            }
            NetworkEvent::ValueUpdated {
                keyref,
                value,
                last_read,
            } => {
                self.update_key_value(keyref, value.clone(), last_read);
                if let Some(modal) = self.modal.as_mut() {
                    if modal.keyref == keyref {
                        modal.current_value = value;
                    }
                }
            }
            NetworkEvent::WriteSucceeded {
                keyref,
                value,
                last_read,
            } => {
                self.update_key_value(keyref, value, Some(last_read));
                if self
                    .modal
                    .as_ref()
                    .is_some_and(|modal| modal.keyref == keyref)
                {
                    self.modal = None;
                }
            }
            NetworkEvent::WriteFailed { keyref, error } => {
                if let Some(modal) = self.modal.as_mut() {
                    if modal.keyref == keyref {
                        modal.set_error(error);
                    }
                }
            }
        }
    }

    pub(crate) fn status_line(&self) -> String {
        match &self.status {
            AppStatus::Connecting => {
                format!("connecting to {}", self.addr)
            }
            AppStatus::Connected => {
                let node = self.node_id.as_deref().unwrap_or("<unknown>");
                format!(
                    "connected to {} as {} | {} keys | {} observable | click writable rows, press r to reread, q to quit",
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
                    "disconnected from {}: {} | retrying every {}s | q to quit",
                    self.addr, error, self.retry_secs
                )
            }
        }
    }

    pub(crate) fn open_modal_for_key(&mut self, keyref: u16) {
        let Some(row) = self
            .keys
            .iter()
            .find(|row| row.info.keyref == keyref && row.info.keyflags.is_writable())
        else {
            return;
        };

        self.modal = Some(EditModal::from_row(row));
    }

    fn update_key_value(&mut self, keyref: u16, value: String, last_read: Option<Instant>) {
        if let Some(key) = self.keys.iter_mut().find(|key| key.info.keyref == keyref) {
            key.value = value;
            key.last_read = last_read;
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum NetworkEvent {
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
    WriteSucceeded {
        keyref: u16,
        value: String,
        last_read: Instant,
    },
    WriteFailed {
        keyref: u16,
        error: String,
    },
}

#[derive(Debug)]
pub(crate) enum WorkerCommand {
    RefreshAllReadable,
    WriteValue { keyref: u16, value: Value },
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct InputOutcome {
    pub(crate) needs_redraw: bool,
    pub(crate) quit: bool,
}

pub(crate) fn poll_input(timeout: Duration) -> io::Result<Option<CrosstermEvent>> {
    if !event::poll(timeout)? {
        return Ok(None);
    }

    event::read().map(Some)
}

pub(crate) fn handle_event(
    app: &mut App,
    geometry: &UiGeometry,
    event: CrosstermEvent,
    command_tx: &mpsc::UnboundedSender<WorkerCommand>,
) -> InputOutcome {
    match event {
        CrosstermEvent::Key(key) => handle_key_event(app, key, command_tx),
        CrosstermEvent::Mouse(mouse) => handle_mouse_event(app, geometry, mouse, command_tx),
        CrosstermEvent::Resize(_, _) => InputOutcome {
            needs_redraw: true,
            quit: false,
        },
        CrosstermEvent::FocusGained | CrosstermEvent::FocusLost | CrosstermEvent::Paste(_) => {
            InputOutcome::default()
        }
    }
}

fn handle_key_event(
    app: &mut App,
    key: KeyEvent,
    command_tx: &mpsc::UnboundedSender<WorkerCommand>,
) -> InputOutcome {
    if key.kind != KeyEventKind::Press {
        return InputOutcome::default();
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        return InputOutcome {
            needs_redraw: false,
            quit: true,
        };
    }

    if app.modal.is_some() {
        return handle_modal_key_event(app, key, command_tx);
    }

    match key.code {
        KeyCode::Char('q') => InputOutcome {
            needs_redraw: false,
            quit: true,
        },
        KeyCode::Char('r') | KeyCode::Char('R') => {
            let _sent = command_tx.send(WorkerCommand::RefreshAllReadable);
            InputOutcome {
                needs_redraw: true,
                quit: false,
            }
        }
        _ => InputOutcome::default(),
    }
}

fn handle_modal_key_event(
    app: &mut App,
    key: KeyEvent,
    command_tx: &mpsc::UnboundedSender<WorkerCommand>,
) -> InputOutcome {
    let Some(modal) = app.modal.as_mut() else {
        return InputOutcome::default();
    };

    if modal.submitting {
        if matches!(key.code, KeyCode::Esc) {
            app.modal = None;
            return InputOutcome {
                needs_redraw: true,
                quit: false,
            };
        }

        return InputOutcome::default();
    }

    match key.code {
        KeyCode::Esc => {
            app.modal = None;
            InputOutcome {
                needs_redraw: true,
                quit: false,
            }
        }
        KeyCode::Tab | KeyCode::BackTab => {
            modal.focus = modal.focus.toggle();
            InputOutcome {
                needs_redraw: true,
                quit: false,
            }
        }
        KeyCode::Backspace => {
            match modal.focus {
                ModalFocus::StringInput => {
                    modal.string_input.pop();
                }
                ModalFocus::NumberInput => {
                    modal.number_input.pop();
                }
            }
            InputOutcome {
                needs_redraw: true,
                quit: false,
            }
        }
        KeyCode::Enter => {
            submit_focused_modal_value(app, command_tx);
            InputOutcome {
                needs_redraw: true,
                quit: false,
            }
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            match modal.focus {
                ModalFocus::StringInput => {
                    modal.string_input.push(character);
                }
                ModalFocus::NumberInput => {
                    if character.is_ascii_digit()
                        || (character == '-' && modal.number_input.is_empty())
                    {
                        modal.number_input.push(character);
                    }
                }
            }
            InputOutcome {
                needs_redraw: true,
                quit: false,
            }
        }
        _ => InputOutcome::default(),
    }
}

fn handle_mouse_event(
    app: &mut App,
    geometry: &UiGeometry,
    mouse: MouseEvent,
    command_tx: &mpsc::UnboundedSender<WorkerCommand>,
) -> InputOutcome {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return InputOutcome::default();
    }

    if let Some(modal_geometry) = geometry.modal {
        if let Some(target) = modal_geometry.target_at(mouse.column, mouse.row) {
            match target {
                ModalClickTarget::StringInput => {
                    if let Some(modal) = app.modal.as_mut() {
                        modal.focus = ModalFocus::StringInput;
                    }
                }
                ModalClickTarget::StringSet => submit_string_modal(app, command_tx),
                ModalClickTarget::BoolTrue => submit_bool_modal(app, command_tx, true),
                ModalClickTarget::BoolFalse => submit_bool_modal(app, command_tx, false),
                ModalClickTarget::NumberInput => {
                    if let Some(modal) = app.modal.as_mut() {
                        modal.focus = ModalFocus::NumberInput;
                    }
                }
                ModalClickTarget::NumberSet => submit_number_modal(app, command_tx),
            }

            return InputOutcome {
                needs_redraw: true,
                quit: false,
            };
        }

        if !modal_geometry.contains(mouse.column, mouse.row) {
            app.modal = None;
            return InputOutcome {
                needs_redraw: true,
                quit: false,
            };
        }

        return InputOutcome::default();
    }

    let Some(table_geometry) = geometry.table else {
        return InputOutcome::default();
    };
    let Some(row_index) = table_geometry.row_at(mouse.column, mouse.row, app.keys.len()) else {
        return InputOutcome::default();
    };
    let Some(row) = app.keys.get(row_index) else {
        return InputOutcome::default();
    };
    if !row.info.keyflags.is_writable() {
        return InputOutcome::default();
    }

    app.open_modal_for_key(row.info.keyref);
    InputOutcome {
        needs_redraw: true,
        quit: false,
    }
}

fn submit_focused_modal_value(app: &mut App, command_tx: &mpsc::UnboundedSender<WorkerCommand>) {
    let focus = app.modal.as_ref().map(|modal| modal.focus);
    match focus {
        Some(ModalFocus::StringInput) => submit_string_modal(app, command_tx),
        Some(ModalFocus::NumberInput) => submit_number_modal(app, command_tx),
        None => {}
    }
}

fn submit_string_modal(app: &mut App, command_tx: &mpsc::UnboundedSender<WorkerCommand>) {
    let Some(modal) = app.modal.as_mut() else {
        return;
    };

    queue_write_command(
        modal,
        command_tx,
        WorkerCommand::WriteValue {
            keyref: modal.keyref,
            value: Value::Str(modal.string_input.clone()),
        },
    );
}

fn submit_bool_modal(
    app: &mut App,
    command_tx: &mpsc::UnboundedSender<WorkerCommand>,
    value: bool,
) {
    let Some(modal) = app.modal.as_mut() else {
        return;
    };

    queue_write_command(
        modal,
        command_tx,
        WorkerCommand::WriteValue {
            keyref: modal.keyref,
            value: Value::Bool(value),
        },
    );
}

fn submit_number_modal(app: &mut App, command_tx: &mpsc::UnboundedSender<WorkerCommand>) {
    let Some(modal) = app.modal.as_mut() else {
        return;
    };

    match modal.number_input.trim().parse::<i64>() {
        Ok(value) => queue_write_command(
            modal,
            command_tx,
            WorkerCommand::WriteValue {
                keyref: modal.keyref,
                value: Value::I64(value),
            },
        ),
        Err(error) => modal.set_error(format!("invalid number: {error}")),
    }
}

fn queue_write_command(
    modal: &mut EditModal,
    command_tx: &mpsc::UnboundedSender<WorkerCommand>,
    command: WorkerCommand,
) {
    modal.mark_submitting();
    if command_tx.send(command).is_err() {
        modal.set_error("connection manager stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::{EditModal, KeyRow};
    use qup::KeyInfo;
    use qup_core::KeyFlags;

    #[test]
    fn edit_modal_prefills_numeric_values() {
        let row = KeyRow {
            info: KeyInfo {
                keyref: 3,
                keyflags: KeyFlags::new(KeyFlags::WRITABLE).expect("flags should be valid"),
                name: String::from("setpoint"),
            },
            value: String::from("42"),
            last_read: None,
        };

        let modal = EditModal::from_row(&row);
        assert_eq!(modal.number_input, "42");
        assert!(modal.string_input.is_empty());
    }
}
