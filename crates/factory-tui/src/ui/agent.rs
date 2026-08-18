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
    let Some(agent_id) = board.selected_agent.as_ref() else {
        ui::dim(frame, area, "no agent selected");
        return;
    };
    let Some(agent) = board.agents.get(agent_id) else {
        return;
    };
    let orchestrator = agent.role == factory_core::AgentRole::Orchestrator;
    let constraints = if orchestrator {
        vec![
            Constraint::Percentage(25),
            Constraint::Percentage(20),
            Constraint::Percentage(25),
            Constraint::Percentage(30),
        ]
    } else {
        vec![
            Constraint::Percentage(40),
            Constraint::Percentage(25),
            Constraint::Percentage(35),
        ]
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

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
            "provider: {:?}\nmodel: {model}\npermission: {permission}\n{pause}\n{files}",
            agent.provider
        ))
        .block(ui::block(" settings ")),
        rows[2],
    );
    if orchestrator {
        frame.render_widget(
            Paragraph::new(orchestrator_context_lines(board, &agent.project_id).join("\n"))
                .block(ui::block(" project queue + delegation ")),
            rows[3],
        );
    }
}

fn orchestrator_context_lines(board: &Board, project_id: &factory_core::ProjectId) -> Vec<String> {
    let mut lines = vec!["project queue:".to_owned()];
    let mut tasks: Vec<_> = board
        .tasks
        .values()
        .filter(|task| {
            &task.snapshot.project_id == project_id
                && matches!(
                    task.snapshot.status,
                    factory_core::TaskStatus::Queued | factory_core::TaskStatus::Running
                )
        })
        .collect();
    tasks.sort_by_key(|task| (task.snapshot.created_at_ms, task.snapshot.id.clone()));
    for task in tasks {
        let owner = task
            .snapshot
            .assigned_agent_id
            .as_ref()
            .map_or("unassigned", factory_core::AgentId::as_str);
        lines.push(format!("  {owner}: {}", task.snapshot.title));
    }
    lines.push("delegation:".to_owned());
    let mut agents: Vec<_> = board
        .agents
        .values()
        .filter(|agent| &agent.project_id == project_id)
        .collect();
    agents.sort_by_key(|agent| (agent.created_at_ms, agent.id.clone()));
    for agent in agents {
        if let Some(parent) = &agent.parent_agent_id {
            lines.push(format!("  {parent} ═> {}", agent.id));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{agent, project, task};
    use factory_core::{AgentRole, ProjectId, TaskStatus};

    #[test]
    fn orchestrator_context_lists_unassigned_worker_queues_and_nested_edges() {
        let mut board = Board::new(false, 0, crate::theme::PLAIN);
        let mut worker = agent("worker", "proj", AgentRole::Worker, None);
        worker.parent_agent_id = Some(factory_core::AgentId::try_from("orch").unwrap());
        let mut child = agent("child", "proj", AgentRole::Worker, None);
        child.parent_agent_id = Some(factory_core::AgentId::try_from("worker").unwrap());
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![
                agent("orch", "proj", AgentRole::Orchestrator, None),
                worker,
                child,
            ],
            vec![
                task("free", "proj", TaskStatus::Queued, None, 0),
                task("owned", "proj", TaskStatus::Running, Some("worker"), 1),
                task("done", "proj", TaskStatus::Succeeded, Some("child"), 2),
            ],
            Vec::new(),
            Vec::new(),
        );
        let text =
            orchestrator_context_lines(&board, &ProjectId::try_from("proj").unwrap()).join("\n");
        assert!(text.contains("unassigned: free"));
        assert!(text.contains("worker: owned"));
        assert!(!text.contains("done"));
        assert!(text.contains("orch ═> worker"));
        assert!(text.contains("worker ═> child"));
    }
}
