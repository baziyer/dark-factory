//! The bottom status/help line, and the modal overlays for prompts, pickers, and delete
//! confirmation (rendered centered over everything else - see `centered_rect` in `ui/mod.rs`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::model::{Board, Connection, Mode, PickerKind};
use crate::ui::centered_rect;

fn connection_badge(board: &Board) -> Span<'static> {
    match board.connection {
        Connection::Connecting => Span::styled(
            " CONNECTING ",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ),
        Connection::Live => {
            Span::styled(" LIVE ", Style::default().fg(Color::Black).bg(Color::Green))
        }
        Connection::Retrying => Span::styled(
            " RETRYING ",
            Style::default().fg(Color::Black).bg(Color::Red),
        ),
    }
}

pub fn render_status_line(frame: &mut Frame, area: Rect, board: &Board) {
    if area.height == 0 {
        return;
    }
    let text_color = if board.status_line_is_error() {
        Color::Red
    } else {
        Color::White
    };
    let mut spans = vec![
        connection_badge(board),
        Span::raw(" "),
        Span::styled(board.status_line_text(), Style::default().fg(text_color)),
    ];
    if board.connection == Connection::Retrying {
        if let Some(detail) = &board.connection_detail {
            spans.push(Span::styled(
                format!(" ({detail})"),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn render_overlay(frame: &mut Frame, area: Rect, board: &Board) {
    match &board.mode {
        Mode::ConfirmDelete(task_id) => render_confirm(frame, area, task_id.as_str()),
        Mode::Prompt(prompt) => render_prompt(frame, area, board, prompt),
        Mode::Picker(picker) => render_picker(frame, area, board, picker),
        Mode::Normal | Mode::Zoomed(_) => {}
    }
}

fn render_confirm(frame: &mut Frame, area: Rect, task_id: &str) {
    let rect = centered_rect(area, 50, 5);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title("delete task?");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let text = vec![
        Line::from(format!("delete task#{task_id}?")),
        Line::from(""),
        Line::from("press x again to confirm - any other key cancels"),
    ];
    frame.render_widget(Paragraph::new(text), inner);
}

fn render_prompt(frame: &mut Frame, area: Rect, board: &Board, prompt: &crate::model::PromptState) {
    let title = match &prompt.kind {
        crate::model::PromptKind::NewTask => "new task".to_owned(),
        crate::model::PromptKind::MessageAgent(agent_id) => format!("message {agent_id}"),
    };
    let height = u16::try_from(prompt.labels.len()).unwrap_or(1) + 4;
    let rect = centered_rect(area, 60, height);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let mut lines: Vec<Line> = Vec::new();
    for (index, label) in prompt.labels.iter().enumerate() {
        let value = prompt
            .values
            .get(index)
            .map(String::as_str)
            .unwrap_or_default();
        let cursor = if index == prompt.field {
            "\u{2588}"
        } else {
            ""
        };
        let style = if index == prompt.field {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(Span::styled(
            format!("{label}: {value}{cursor}"),
            style,
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        board.help_text(),
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_picker(frame: &mut Frame, area: Rect, board: &Board, picker: &crate::model::PickerState) {
    let (title, items): (String, Vec<ListItem>) = match &picker.kind {
        PickerKind::Project => (
            "select a project".to_owned(),
            board
                .projects
                .iter()
                .map(|project| ListItem::new(format!("{} ({})", project.name, project.id)))
                .collect(),
        ),
        PickerKind::AssignAgent(task_id) => (
            format!("assign task#{task_id} to"),
            board
                .units()
                .iter()
                .map(|unit| {
                    let state = board.state_of(&unit.id);
                    ListItem::new(format!("{} ({})", unit.id, state.label()))
                })
                .collect(),
        ),
    };

    let rect = centered_rect(area, 50, (items.len() as u16 + 3).clamp(4, area.height));
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);

    if items.is_empty() {
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        frame.render_widget(Paragraph::new("(nothing to pick)"), inner);
        return;
    }

    let list = List::new(items)
        .block(block)
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    let mut state = ListState::default();
    state.select(Some(picker.cursor));
    frame.render_stateful_widget(list, rect, &mut state);
}
