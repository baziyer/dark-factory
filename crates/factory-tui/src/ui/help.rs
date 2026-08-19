//! The bottom tabs/status/essential-controls line and the modal overlays for prompts, pickers,
//! the task menu, confirmations, and the `?` help reference.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, ListItem, ListState, Paragraph};

use crate::model::{
    Board, Connection, Mode, PaneMode, PendingAction, PickerKind, PickerState, PromptKind,
    PromptState, TaskMenuState,
};
use crate::mouse::{HitMap, Target};
use crate::ui::{self, centered_rect, render_tabs};

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

pub fn render_status_line(frame: &mut Frame, area: Rect, board: &Board, hits: &mut HitMap) {
    if area.height == 0 {
        return;
    }
    const TABS_WIDTH: u16 = 18;
    const HELP_LABEL: &str = "[? help]";
    const DETACH_LABEL: &str = "[q detach]";
    const TYPING_ESCAPE: &str = "Ctrl-] board  ";
    let typing = board.pane_mode == PaneMode::Typing && board.has_live_pane();
    let controls = format!(
        "{}{HELP_LABEL} {DETACH_LABEL}",
        if typing { TYPING_ESCAPE } else { "" }
    );
    let controls_width = u16::try_from(controls.len()).unwrap_or(u16::MAX);
    let tabs_width = TABS_WIDTH.min(area.width);
    let remaining = area.width.saturating_sub(tabs_width);
    let controls_width = controls_width.min(remaining);
    let status_width = remaining.saturating_sub(controls_width);
    let tabs_area = Rect::new(area.x, area.y, tabs_width, 1);
    let status_area = Rect::new(area.x.saturating_add(tabs_width), area.y, status_width, 1);
    let controls_area = Rect::new(
        status_area.x.saturating_add(status_width),
        area.y,
        controls_width,
        1,
    );

    render_tabs(frame, tabs_area, board, hits);

    let text_color = if board.status_line_is_error() {
        Color::Red
    } else {
        Color::White
    };
    let mut spans = vec![connection_badge(board), Span::raw(" ")];
    if let Some(cap) = board.live_session_cap {
        let live = board.live_session_count();
        spans.push(Span::styled(
            format!(" {live}/{cap} live "),
            Style::default()
                .fg(Color::Black)
                .bg(if live >= cap as usize {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }),
        ));
        spans.push(Span::raw(" "));
    }
    if let Some(mismatch) = board.version_mismatch() {
        spans.push(Span::styled(
            format!(" {mismatch} "),
            Style::default().fg(Color::Black).bg(Color::Red),
        ));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        board.status_line_text(),
        Style::default().fg(text_color),
    ));
    if board.connection == Connection::Retrying {
        if let Some(detail) = &board.connection_detail {
            spans.push(Span::styled(
                format!(" ({detail})"),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    if let Some(version) = &board.update_available {
        spans.push(Span::styled(
            format!("  update v{version} available: factoryctl update --install"),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), status_area);
    frame.render_widget(
        Paragraph::new(controls.clone()).style(Style::default().fg(Color::Cyan)),
        controls_area,
    );

    let escape_width = if typing { TYPING_ESCAPE.len() } else { 0 };
    let help_start = u16::try_from(escape_width).unwrap_or(u16::MAX);
    let help_width = u16::try_from(HELP_LABEL.len()).unwrap_or(u16::MAX);
    let detach_start = help_start.saturating_add(help_width).saturating_add(1);
    let detach_width = u16::try_from(DETACH_LABEL.len()).unwrap_or(u16::MAX);
    if help_start.saturating_add(help_width) <= controls_area.width {
        hits.add(
            Rect::new(
                controls_area.x.saturating_add(help_start),
                controls_area.y,
                help_width,
                1,
            ),
            Target::Help,
        );
    }
    if detach_start.saturating_add(detach_width) <= controls_area.width {
        hits.add(
            Rect::new(
                controls_area.x.saturating_add(detach_start),
                controls_area.y,
                detach_width,
                1,
            ),
            Target::Detach,
        );
    }
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
    let inner = ui::bordered(
        frame,
        rect,
        ui::block(title).border_style(Style::default().fg(Color::Red)),
    );
    let text = vec![
        Line::from(prompt_line),
        Line::from(""),
        Line::from("y / Enter / x again to confirm — any other key cancels"),
    ];
    frame.render_widget(Paragraph::new(text), inner);
}

fn render_prompt(frame: &mut Frame, area: Rect, board: &Board, prompt: &PromptState) {
    let title = match &prompt.kind {
        PromptKind::NewTask(_) => "new task".to_owned(),
        PromptKind::MessageAgent(agent_id) => format!("message {agent_id}"),
        PromptKind::MessageOrchestrator(agent_id) => format!("message orchestrator {agent_id}"),
        PromptKind::EditTaskTitle(task_id) => format!("edit title — task#{task_id}"),
        PromptKind::ReorderTask(task_id) => format!("reorder — task#{task_id}"),
        PromptKind::EditModel(agent_id) => format!("model — {agent_id}"),
        PromptKind::EditPermission(agent_id) => format!("permission — {agent_id}"),
        PromptKind::Capacity => "live-session capacity".to_owned(),
    };
    let height = u16::try_from(prompt.labels.len()).unwrap_or(1) + 4;
    let rect = centered_rect(area, 60, height);
    frame.render_widget(Clear, rect);
    let inner = ui::bordered(
        frame,
        rect,
        ui::block(title).border_style(Style::default().fg(Color::Cyan)),
    );

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
        PickerKind::Project(projects) => (
            "focus which project?".to_owned(),
            projects
                .iter()
                .map(|id| {
                    let label = board.projects.iter().find(|p| &p.id == id).map_or_else(
                        || id.to_string(),
                        |project| format!("{} — {}", id, project.name),
                    );
                    ListItem::new(label)
                })
                .collect(),
        ),
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
    let block = ui::block(title).border_style(Style::default().fg(Color::Cyan));

    if items.is_empty() {
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        frame.render_widget(Paragraph::new("(nothing to pick)"), inner);
        return;
    }

    let list = ui::styled_list(items, block);
    let mut state = ListState::default();
    state.select(Some(picker.cursor));
    frame.render_stateful_widget(list, rect, &mut state);
}

fn render_task_menu(frame: &mut Frame, area: Rect, board: &Board, menu: &TaskMenuState) {
    let title = board.tasks.get(&menu.task_id).map_or_else(
        || format!("task#{}", menu.task_id),
        |task| format!("task#{} — {}", menu.task_id, task.snapshot.title),
    );
    let items: Vec<ListItem> = menu.items.iter().map(|item| ListItem::new(*item)).collect();
    let rect = centered_rect(area, 40, u16::try_from(menu.items.len()).unwrap_or(6) + 3);
    frame.render_widget(Clear, rect);
    let block = ui::block(title).border_style(Style::default().fg(Color::Cyan));
    let list = ui::styled_list(items, block);
    let mut state = ListState::default();
    state.select(Some(menu.cursor));
    frame.render_stateful_widget(list, rect, &mut state);
}

const HELP_TEXT: &[&str] = &[
    "BUILDING / AGENT — two screens, BOARD / TYPING — two input modes",
    "Enter/Esc  open selected agent / return to BUILDING",
    "j/k, ↑/↓   previous/next agent floor",
    "[ / ]      previous/next agent without leaving AGENT",
    "n          new task (needs a focused project)",
    "m          message the selected agent",
    "o          message the orchestrator (picks by Tab if more than one)",
    "p          focus a project (remembered for next time)",
    "x          stop the selected agent — 2-press confirm",
    "g          jump to the next agent in NEEDS YOU",
    "i/Enter    AGENT: type into the live terminal",
    "Ctrl-]     return terminal input to BOARD mode",
    "z          maximise/restore terminal     PgUp/PgDn scroll",
    "mouse      click tabs/rows/pane; wheel scrolls terminal history",
    "Space      pause/resume agent            t manage active task",
    "I / M      edit instructions.md / memory.md in $EDITOR",
    "q          detach (quits the client only — never stops the factory)",
    "?          toggle this help",
];

fn render_help(frame: &mut Frame, area: Rect) {
    let widest = HELP_TEXT
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(60);
    let rect = centered_rect(
        area,
        u16::try_from(widest + 4).unwrap_or(u16::MAX),
        u16::try_from(HELP_TEXT.len()).unwrap_or(14) + 3,
    );
    frame.render_widget(Clear, rect);
    let inner = ui::bordered(
        frame,
        rect,
        ui::block(" keys ").border_style(Style::default().fg(Color::Cyan)),
    );
    let lines: Vec<Line> = HELP_TEXT.iter().map(|line| Line::from(*line)).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}
