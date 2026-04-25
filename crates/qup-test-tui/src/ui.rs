use std::{io, time::Instant};

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use qup::Value;
use qup_core::KeyFlags;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table},
};

use crate::app::{App, EditModal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalFocus {
    StringInput,
    NumberInput,
}

impl ModalFocus {
    pub(crate) fn toggle(self) -> Self {
        match self {
            Self::StringInput => Self::NumberInput,
            Self::NumberInput => Self::StringInput,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct UiGeometry {
    pub(crate) table: Option<TableGeometry>,
    pub(crate) modal: Option<ModalGeometry>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TableGeometry {
    body: Rect,
    visible_rows: usize,
}

impl TableGeometry {
    pub(crate) fn row_at(self, column: u16, row: u16, total_rows: usize) -> Option<usize> {
        if !contains_point(self.body, column, row) {
            return None;
        }

        let row_index = usize::from(row.saturating_sub(self.body.y));
        let visible_rows = self.visible_rows.min(total_rows);
        (row_index < visible_rows).then_some(row_index)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ModalGeometry {
    outer: Rect,
    string_input: Rect,
    string_set_button: Rect,
    bool_true_button: Rect,
    bool_false_button: Rect,
    number_input: Rect,
    number_set_button: Rect,
}

impl ModalGeometry {
    pub(crate) fn target_at(self, column: u16, row: u16) -> Option<ModalClickTarget> {
        if contains_point(self.string_input, column, row) {
            return Some(ModalClickTarget::StringInput);
        }
        if contains_point(self.string_set_button, column, row) {
            return Some(ModalClickTarget::StringSet);
        }
        if contains_point(self.bool_true_button, column, row) {
            return Some(ModalClickTarget::BoolTrue);
        }
        if contains_point(self.bool_false_button, column, row) {
            return Some(ModalClickTarget::BoolFalse);
        }
        if contains_point(self.number_input, column, row) {
            return Some(ModalClickTarget::NumberInput);
        }
        if contains_point(self.number_set_button, column, row) {
            return Some(ModalClickTarget::NumberSet);
        }
        None
    }

    pub(crate) fn contains(self, column: u16, row: u16) -> bool {
        contains_point(self.outer, column, row)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalClickTarget {
    StringInput,
    StringSet,
    BoolTrue,
    BoolFalse,
    NumberInput,
    NumberSet,
}

pub(crate) fn draw(frame: &mut Frame<'_>, app: &App, now: Instant, geometry: &mut UiGeometry) {
    *geometry = UiGeometry::default();

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(frame.area());

    let status = Paragraph::new(app.status_line())
        .block(Block::default().title("Connection").borders(Borders::ALL));
    frame.render_widget(status, layout[0]);

    let table_area = layout[1];
    geometry.table = build_table_geometry(table_area);
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
    frame.render_widget(table, table_area);

    if let Some(modal) = app.modal.as_ref() {
        geometry.modal = Some(draw_modal(frame, modal));
    }
}

fn build_table_geometry(table_area: Rect) -> Option<TableGeometry> {
    let inner = table_area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let body_height = inner.height.saturating_sub(1);
    if body_height == 0 {
        return None;
    }

    Some(TableGeometry {
        body: Rect::new(inner.x, inner.y.saturating_add(1), inner.width, body_height),
        visible_rows: usize::from(body_height),
    })
}

fn draw_modal(frame: &mut Frame<'_>, modal: &EditModal) -> ModalGeometry {
    let outer = centered_rect(frame.area(), 72, 15);
    frame.render_widget(Clear, outer);

    let block = Block::default()
        .title(format!("Set Value: {}", modal.key_name))
        .borders(Borders::ALL);
    frame.render_widget(block, outer);

    let inner = outer.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(2),
        ])
        .split(inner);

    let current = Paragraph::new(format!("current: {}", modal.current_value));
    frame.render_widget(current, sections[0]);

    let string_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(10)])
        .split(sections[1]);
    let string_input = Paragraph::new(modal.string_input.as_str()).block(
        Block::default()
            .title("String")
            .borders(Borders::ALL)
            .border_style(input_border_style(
                modal.focus == ModalFocus::StringInput,
                modal.submitting,
            )),
    );
    frame.render_widget(string_input, string_row[0]);
    let string_button = Paragraph::new("set")
        .style(button_text_style(modal.submitting))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(button_border_style()),
        );
    frame.render_widget(string_button, string_row[1]);

    let bool_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Min(1),
        ])
        .split(sections[2]);
    let bool_true = Paragraph::new("true")
        .style(button_text_style(modal.submitting))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Bool")
                .border_style(button_border_style()),
        );
    let bool_false = Paragraph::new("false")
        .style(button_text_style(modal.submitting))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(button_border_style()),
        );
    frame.render_widget(bool_true, bool_row[0]);
    frame.render_widget(bool_false, bool_row[1]);

    let number_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(10)])
        .split(sections[3]);
    let number_input = Paragraph::new(modal.number_input.as_str()).block(
        Block::default()
            .title("Number")
            .borders(Borders::ALL)
            .border_style(input_border_style(
                modal.focus == ModalFocus::NumberInput,
                modal.submitting,
            )),
    );
    frame.render_widget(number_input, number_row[0]);
    let number_button = Paragraph::new("set")
        .style(button_text_style(modal.submitting))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(button_border_style()),
        );
    frame.render_widget(number_button, number_row[1]);

    let footer_text = match (&modal.error, modal.submitting) {
        (Some(error), _) => error.clone(),
        (None, true) => String::from("writing..."),
        (None, false) => {
            String::from("click a button or press Enter in the focused input; Esc closes")
        }
    };
    let footer_style = if modal.error.is_some() {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };
    let footer = Paragraph::new(footer_text).style(footer_style);
    frame.render_widget(footer, sections[4]);

    ModalGeometry {
        outer,
        string_input: string_row[0],
        string_set_button: string_row[1],
        bool_true_button: bool_row[0],
        bool_false_button: bool_row[1],
        number_input: number_row[0],
        number_set_button: number_row[1],
    }
}

pub(crate) fn centered_rect(area: Rect, desired_width: u16, desired_height: u16) -> Rect {
    let width = desired_width.min(area.width.saturating_sub(2)).max(20);
    let height = desired_height.min(area.height.saturating_sub(2)).max(8);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(width),
            Constraint::Fill(1),
        ])
        .split(area);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(height),
            Constraint::Fill(1),
        ])
        .split(horizontal[1]);
    vertical[1]
}

fn contains_point(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn input_border_style(focused: bool, disabled: bool) -> Style {
    if disabled {
        Style::default().fg(Color::DarkGray)
    } else if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}

fn button_border_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn button_text_style(disabled: bool) -> Style {
    if disabled {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

pub(crate) fn format_flags(flags: KeyFlags) -> String {
    let readable = if flags.is_readable() { 'r' } else { '-' };
    let writable = if flags.is_writable() { 'w' } else { '-' };
    let observable = if flags.is_observable() { 'o' } else { '-' };
    format!("{readable}{writable}{observable}")
}

pub(crate) fn format_last_read(last_read: Option<Instant>, now: Instant) -> String {
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

pub(crate) fn format_value(value: &Value) -> String {
    match value {
        Value::Bool(value) => value.to_string(),
        Value::I64(value) => value.to_string(),
        Value::Str(value) => value.escape_debug().to_string(),
    }
}

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

pub(crate) struct Tui {
    terminal: TuiTerminal,
}

impl Tui {
    pub(crate) fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        terminal.hide_cursor()?;

        Ok(Self { terminal })
    }

    pub(crate) fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> io::Result<()> {
        self.terminal.draw(render).map(|_completed| ())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        build_table_geometry, centered_rect, format_flags, format_last_read, format_value,
    };
    use qup::Value;
    use qup_core::KeyFlags;
    use ratatui::layout::Rect;

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

    #[test]
    fn centered_rect_stays_inside_area() {
        let area = Rect::new(0, 0, 40, 10);
        let centered = centered_rect(area, 72, 15);
        assert!(centered.width <= area.width);
        assert!(centered.height <= area.height);
    }

    #[test]
    fn table_geometry_exposes_body_rows() {
        let table = build_table_geometry(Rect::new(0, 0, 20, 8)).expect("table body exists");
        assert_eq!(table.visible_rows, 5);
        assert_eq!(table.row_at(1, 2, 10), Some(0));
    }
}
