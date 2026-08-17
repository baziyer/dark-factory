//! The bottom status/help line, and the modal overlays for prompts, pickers, the WORKSHOP task
//! menu, confirmations, and the `?` help reference — rendered centered over everything else.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::model::{
    Board, Connection, Mode, PendingAction, PickerKind, PickerState, PromptKind, PromptState,
    TASK_MENU_ITEMS, TaskMenuState, View,
};
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

fn view_badge(view: View) -> Span<'static> {
    Span::styled(
        format!(" {}:{} ", view.number(), view.label()),
        Style::default().fg(Color::Black).bg(Color::Cyan),
    )
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
        view_badge(board.view),
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
        Mode::Confirm(action) => render_confirm(frame, area, action),
        Mode::Prompt(prompt) => render_prompt(frame, area, board, prompt),
        Mode::Picker(picker) => render_picker(frame, area, board, picker),
        Mode::TaskMenu(state) => render_task_menu(frame, area, board, state),
        Mode::Help => render_help(frame, area),
        Mode::Normal => {}
    }
}

fn render_confirm(frame: &mut Frame, area: Rect, action: &PendingAction) {
    let (title, prompt_line) = match action {
        PendingAction::DeleteTask(task_id) => {
            ("delete task?".to_owned(), format!("delete task#{task_id}?"))
        }
        PendingAction::StopSession { session_id, .. } => (
            "stop agent?".to_owned(),
            format!("stop session {session_id}?"),
        ),
        PendingAction::StopRun { run_id, .. } => {
            ("stop agent?".to_owned(), format!("stop run {run_id}?"))
        }
    };
    let rect = centered_rect(area, 56, 5);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title(title);
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let text = vec![
        Line::from(prompt_line),
        Line::from(""),
        Line::from("y / Enter / x again to confirm — any other key cancels"),
    ];
    frame.render_widget(Paragraph::new(text), inner);
}

fn render_prompt(frame: &mut Frame, area: Rect, board: &Board, prompt: &PromptState) {
    let title = match &prompt.kind {
        PromptKind::NewTask => "new task".to_owned(),
        PromptKind::MessageAgent(agent_id) => format!("message {agent_id}"),
        PromptKind::MessageOrchestrator(agent_id) => format!("message orchestrator {agent_id}"),
        PromptKind::EditTaskTitle(task_id) => format!("edit title — task#{task_id}"),
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

fn render_picker(frame: &mut Frame, area: Rect, board: &Board, picker: &PickerState) {
    let (title, items): (String, Vec<ListItem>) = match &picker.kind {
        PickerKind::AssignAgent(task_id) => {
            let project_id = board.task_project(task_id);
            let ids = project_id
                .as_ref()
                .map(|id| board.agent_ids_in(id))
                .unwrap_or_default();
            (
                format!("assign task#{task_id} to"),
                ids.iter()
                    .map(|id| {
                        let label = board.agents.get(id).map_or_else(
                            || id.to_string(),
                            |agent| format!("{} ({:?})", id, agent.role),
                        );
                        ListItem::new(label)
                    })
                    .collect(),
            )
        }
        PickerKind::Orchestrator(candidates) => (
            "message which orchestrator?".to_owned(),
            candidates
                .iter()
                .map(|id| ListItem::new(id.to_string()))
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

fn render_task_menu(frame: &mut Frame, area: Rect, board: &Board, menu: &TaskMenuState) {
    let title = board.tasks.get(&menu.task_id).map_or_else(
        || format!("task#{}", menu.task_id),
        |task| format!("task#{} — {}", menu.task_id, task.snapshot.title),
    );
    let items: Vec<ListItem> = TASK_MENU_ITEMS
        .iter()
        .map(|item| ListItem::new(*item))
        .collect();
    let rect = centered_rect(
        area,
        40,
        u16::try_from(TASK_MENU_ITEMS.len()).unwrap_or(6) + 3,
    );
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);
    let list = List::new(items)
        .block(block)
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    let mut state = ListState::default();
    state.select(Some(menu.cursor));
    frame.render_stateful_widget(list, rect, &mut state);
}

const HELP_TEXT: &[&str] = &[
    "1-4        switch view (Fortress / Workshop / Terminals / Focus)",
    "Enter/→/l  zoom in       Esc/←/h  zoom out",
    "Tab        cycle agents (Fortress/Terminals/Focus) or panes (Workshop)",
    "j/k, ↑/↓   move",
    "[ / ]      Fortress: cycle selected workshop (reaches a project with no agents yet)",
    "n          new task (needs a focused project)",
    "m          message the selected agent",
    "o          message the orchestrator (picks by Tab if more than one)",
    "x          stop the selected agent — 2-press confirm",
    "g / G      jump to (G: and focus) the next agent needing attention",
    "!          Workshop: toggle attention-only filter",
    "PgUp/PgDn  Focus: scroll terminal scrollback",
    "Ctrl-]     Terminals/Focus: toggle pane input vs. board control",
    "q          detach (quits the client only — never stops the factory)",
    "?          toggle this help",
];

fn render_help(frame: &mut Frame, area: Rect) {
    let rect = centered_rect(area, 66, u16::try_from(HELP_TEXT.len()).unwrap_or(14) + 3);
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" keys ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let lines: Vec<Line> = HELP_TEXT.iter().map(|line| Line::from(*line)).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}
