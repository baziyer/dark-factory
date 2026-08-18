//! AGENT: one live terminal with its work, inbox, settings, and delegation context alongside.
//! `pane.rs`'s scrollback methods for why this counts as the design brief's "small custom render
//! path over `vt100::Screen`'s scrollback" without hand-rolling a cell renderer: `vt100` already
//! renders the scrolled-back view correctly once `Parser::set_scrollback` is set, so the "custom"
//! part is entirely the operator-facing scroll-offset plumbing — `PgUp`/`PgDn` here, `pane.rs`'s
//! `scroll_up`/`scroll_down`/`scroll_offset` there — not a reimplementation of the widget).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use tui_term::widget::PseudoTerminal;

use crate::model::{Board, PaneMode};
use crate::pane::PaneMap;
use crate::ui;

pub fn draw(frame: &mut Frame, area: Rect, board: &Board, panes: &mut PaneMap) {
    if board.terminal_maximized {
        render_terminal(frame, area, board, panes);
        return;
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(area);
    render_terminal(frame, columns[0], board, panes);
    render_context(frame, columns[1], board);
}

fn render_terminal(frame: &mut Frame, area: Rect, board: &Board, panes: &mut PaneMap) {
    let Some(session_id) = board.focus_target() else {
        render_placeholder(frame, area, board, "no agent selected");
        return;
    };
    let Some(pane) = panes.get_mut(&session_id) else {
        render_placeholder(frame, area, board, "attaching…");
        return;
    };

    let exited = pane.has_exited();
    let marker = if exited { " [exited]" } else { "" };
    let command = match pane.kind {
        crate::pane::PaneKind::LocalPty => format!(" — {}", pane.command.join(" ")),
        crate::pane::PaneKind::Daemon => String::new(),
    };
    let scroll_offset = pane.scroll_offset();
    let mode_hint = if board.pane_mode == PaneMode::Board {
        "BOARD — Ctrl-] back to pane"
    } else if scroll_offset > 0 {
        "TYPING — PgDn/scroll to return to live"
    } else {
        "TYPING — Ctrl-] for board control"
    };
    let scroll_hint = if scroll_offset > 0 {
        format!("  [scrolled back {scroll_offset}]")
    } else {
        String::new()
    };
    let color = if board.pane_mode == PaneMode::Typing {
        Color::Cyan
    } else {
        Color::Yellow
    };
    let title = format!(
        " {}{command}{marker} — [{mode_hint}]{scroll_hint} ",
        pane.title
    );
    let block = ui::block(title).border_style(Style::default().fg(color));
    let inner = block.inner(area);
    if inner.width > 0 && inner.height > 0 {
        let _ = pane.resize(inner.height, inner.width);
    }

    if let Some(error) = pane.attach_error() {
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                error,
                Style::default().fg(Color::Red),
            ))),
            inner,
        );
        return;
    }

    let parser = pane.lock_parser();
    let screen = parser.screen();
    let pseudo = PseudoTerminal::new(screen).block(block);
    frame.render_widget(pseudo, area);

    // Suppress the hardware cursor while scrolled back — showing the *live* cursor position
    // among historical content would be actively misleading (see this module's doc comment).
    if board.pane_mode == PaneMode::Typing && scroll_offset == 0 && !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        let x = inner.x.saturating_add(col);
        let y = inner.y.saturating_add(row);
        if x < inner.x + inner.width && y < inner.y + inner.height {
            frame.set_cursor_position(Position { x, y });
        }
    }
}

fn render_placeholder(frame: &mut Frame, area: Rect, board: &Board, message: &str) {
    let inner = ui::bordered(frame, area, ui::block(" terminal "));
    let text = if board.focused_project.is_none() {
        "no project focused — press Enter on a BUILDING floor first"
    } else {
        message
    };
    ui::dim(frame, inner, text);
}

fn render_context(frame: &mut Frame, area: Rect, board: &Board) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(25),
            Constraint::Percentage(35),
        ])
        .split(area);
    let Some(agent_id) = board.selected_agent.as_ref() else {
        ui::dim(frame, area, "no agent selected");
        return;
    };
    let Some(agent) = board.agents.get(agent_id) else {
        return;
    };

    let tasks: Vec<Line> = board
        .tasks
        .values()
        .filter(|task| {
            task.snapshot.assigned_agent_id.as_ref() == Some(agent_id)
                && matches!(
                    task.snapshot.status,
                    factory_core::TaskStatus::Queued | factory_core::TaskStatus::Running
                )
        })
        .map(|task| {
            Line::from(format!(
                "{:?}  {}",
                task.snapshot.status, task.snapshot.title
            ))
        })
        .collect();
    let queue = if tasks.is_empty() {
        vec![Line::from("nothing assigned")]
    } else {
        tasks
    };
    frame.render_widget(Paragraph::new(queue).block(ui::block(" queue ")), rows[0]);

    let inbox = board.messages.get(agent_id).map_or_else(
        || "loading…".to_owned(),
        |messages| {
            messages
                .iter()
                .rev()
                .take(4)
                .map(|message| message.body.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        },
    );
    frame.render_widget(
        Paragraph::new(inbox)
            .style(Style::default().fg(Color::DarkGray))
            .block(ui::block(" inbox ")),
        rows[1],
    );

    let pause = if agent.paused {
        "paused (resume via factoryctl)"
    } else {
        "running (pause via factoryctl)"
    };
    let role_context = if agent.role == factory_core::AgentRole::Orchestrator {
        let unassigned = board
            .tasks
            .values()
            .filter(|task| {
                task.snapshot.project_id == agent.project_id
                    && task.snapshot.assigned_agent_id.is_none()
            })
            .count();
        let workers = board
            .agents
            .values()
            .filter(|child| child.parent_agent_id.as_ref() == Some(agent_id))
            .count();
        format!("\nproject queue: {unassigned} unassigned\ndelegation: {workers} direct workers")
    } else {
        String::new()
    };
    let (model, permission, files) = board.agent_details.get(agent_id).map_or_else(
        || {
            (
                "loading…".to_owned(),
                "loading…".to_owned(),
                "instructions.md  memory.md".to_owned(),
            )
        },
        |detail| {
            (
                detail
                    .profile
                    .model
                    .clone()
                    .unwrap_or_else(|| "provider default".to_owned()),
                detail
                    .profile
                    .permission_mode
                    .clone()
                    .unwrap_or_else(|| "provider default".to_owned()),
                format!("{}\n{}", detail.instructions_path, detail.memory_path),
            )
        },
    );
    frame.render_widget(
        Paragraph::new(format!(
            "provider: {:?}\nmodel: {model}\npermission: {permission}\n{pause}\n{files}{role_context}",
            agent.provider
        ))
        .block(ui::block(" settings ")),
        rows[2],
    );
}
