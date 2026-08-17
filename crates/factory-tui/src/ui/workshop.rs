//! WORKSHOP: one project's tasks (queue order), its agent hierarchy (orchestrator → workers →
//! sub-agents, indented), and a detail pane for whichever item is selected. `Tab` toggles which
//! list has the cursor; `!` filters both lists to attention-worthy rows only.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, ListState, Paragraph, Wrap};

use factory_core::{AgentRole, ProjectId, TaskStatus};

use crate::model::state::{self, AgentState};
use crate::model::{Board, WorkshopPane, provider_letter};
use crate::ui::{self, pad, truncate};
use factory_core::attention::Attention;

/// Same width as the pre-Track-6c board's unit-list sparkline — see `model::state::ActivitySeries`.
const SPARKLINE_WIDTH: usize = 8;

fn agent_state_style(state: AgentState) -> Style {
    match state {
        AgentState::Idle => Style::default().fg(Color::DarkGray),
        AgentState::Working => Style::default().fg(Color::Green),
        AgentState::Waiting => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        AgentState::Stopped => Style::default().fg(Color::Blue),
        AgentState::Failed => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

/// `theme.complete` is the only status glyph that varies by theme (`--theme plain`'s whole point
/// is no non-ASCII glyph escapes it — see `theme.rs`'s `glyph_tables_are_complete_and_ascii_for_
/// plain`); the rest are already plain ASCII in both themes, so they stay literal here.
fn task_glyph(status: TaskStatus, theme: &crate::theme::Theme) -> char {
    match status {
        TaskStatus::Queued => '#',
        TaskStatus::Running => '>',
        TaskStatus::Blocked => '?',
        TaskStatus::Succeeded => theme.complete,
        TaskStatus::Failed => '!',
        TaskStatus::Cancelled => 'x',
    }
}

fn task_color(status: TaskStatus) -> Color {
    match status {
        TaskStatus::Queued => Color::Gray,
        TaskStatus::Running => Color::Green,
        TaskStatus::Blocked => Color::Yellow,
        TaskStatus::Succeeded => Color::Blue,
        TaskStatus::Failed => Color::Red,
        TaskStatus::Cancelled => Color::DarkGray,
    }
}

pub fn draw(frame: &mut Frame, area: Rect, board: &Board) {
    let Some(project_id) = board.focused_project.clone() else {
        render_no_project(frame, area);
        return;
    };
    let project_name = board
        .projects
        .iter()
        .find(|p| p.id == project_id)
        .map_or_else(|| project_id.to_string(), |p| p.name.clone());

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);
    let (top, detail_area) = (rows[0], rows[1]);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(top);

    render_tasks(frame, cols[0], board, &project_id, &project_name);
    render_agents(frame, cols[1], board, &project_id);
    render_detail(frame, detail_area, board, &project_id);
}

fn render_no_project(frame: &mut Frame, area: Rect) {
    let inner = ui::bordered(frame, area, ui::block(" workshop "));
    ui::dim(
        frame,
        inner,
        "no project focused — press Enter on an agent in FORTRESS to zoom in",
    );
}

fn render_tasks(
    frame: &mut Frame,
    area: Rect,
    board: &Board,
    project_id: &ProjectId,
    project_name: &str,
) {
    let tasks = board.visible_tasks(project_id);
    let mut items: Vec<ListItem> = tasks
        .iter()
        .map(|task| {
            let status = task.snapshot.status;
            let assignee = task
                .snapshot
                .assigned_agent_id
                .as_ref()
                .map_or_else(|| "-".to_owned(), |id| id.as_str().to_owned());
            let line = Line::from(vec![
                Span::styled(
                    format!("{} ", task_glyph(status, &board.theme)),
                    Style::default().fg(task_color(status)),
                ),
                Span::raw(pad(
                    &format!("#{}", truncate(task.snapshot.id.as_str(), 9)),
                    11,
                )),
                // A literal separator space, not folded into `pad`'s width: a title exactly at
                // (or past) the pad width would otherwise glue directly onto the arrow that
                // follows (`pad` only pads when it has room to).
                Span::raw(pad(&task.snapshot.title, 18)),
                Span::raw(" "),
                Span::styled(
                    format!("\u{2192} {assignee}"),
                    Style::default().fg(Color::Gray),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();
    if items.is_empty() {
        let hint = if board.attention_filter && !board.tasks_in(project_id).is_empty() {
            "no tasks need attention — press ! to clear the filter"
        } else {
            "no tasks yet — press n to add one"
        };
        items.push(ListItem::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let focused = board.workshop_focus == WorkshopPane::Tasks;
    let filter_badge = if board.attention_filter { " [!]" } else { "" };
    let title = format!(" {project_name} — tasks{filter_badge} ");
    let block = ui::block(title).border_style(panel_style(focused));
    let list = ui::styled_list(items, block);

    let mut state = ListState::default();
    if !tasks.is_empty() {
        let cursor = board
            .selected_task
            .as_ref()
            .and_then(|id| tasks.iter().position(|t| &t.snapshot.id == id))
            .unwrap_or(0);
        state.select(Some(cursor));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn wait_or_activity_text(
    board: &Board,
    agent: &factory_core::AgentSnapshot,
) -> Option<(String, bool)> {
    if let Some(session) = board.session_for(agent) {
        if let Some(reason) = &session.wait_reason {
            return Some((reason.clone(), false));
        }
        return None;
    }
    let run = board.latest_run_for(&agent.id)?;
    run.wait_reason
        .clone()
        .or_else(|| run.activity.clone())
        .map(|text| (text, true))
}

fn render_agents(frame: &mut Frame, area: Rect, board: &Board, project_id: &ProjectId) {
    let tree = board.visible_agent_tree(project_id);
    let mut items: Vec<ListItem> = tree
        .iter()
        .map(|(agent_id, depth)| {
            let indent = "  ".repeat(usize::from(*depth));
            let Some(agent) = board.agents.get(agent_id) else {
                return ListItem::new(Line::from(format!("{indent}{agent_id}")));
            };
            let rated_state = board.agent_state(agent);
            let glyph = if agent.role == AgentRole::Orchestrator {
                board.theme.orchestrator
            } else if agent.parent_agent_id.is_some() {
                board.theme.subagent
            } else {
                provider_letter(agent.provider)
            };
            let counts = board
                .activity
                .get(agent_id)
                .map(state::ActivitySeries::counts)
                .unwrap_or_default();
            let spark = state::braille_sparkline(&counts, SPARKLINE_WIDTH);
            let mut spans = vec![
                Span::raw(indent),
                Span::styled(format!("{glyph} "), agent_state_style(rated_state.value)),
                // Same guaranteed separator as the tasks list above: a 12-char-or-longer agent
                // id must not glue onto the state label that follows.
                Span::raw(pad(agent_id.as_str(), 12)),
                Span::raw(" "),
                Span::styled(
                    pad(rated_state.value.label(), 9),
                    agent_state_style(rated_state.value),
                ),
                Span::styled(format!("{spark} "), Style::default().fg(Color::Cyan)),
            ];
            if let Some((text, inferred)) = wait_or_activity_text(board, agent) {
                let prefix = if inferred { "~" } else { "" };
                spans.push(Span::styled(
                    format!("{prefix}{}", truncate(&text, 24)),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    if items.is_empty() {
        let hint = if board.attention_filter && !board.agent_tree(project_id).is_empty() {
            "no agents need attention — press ! to clear the filter"
        } else {
            "no agents in this project yet — create one with factoryctl agent add"
        };
        items.push(ListItem::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let focused = board.workshop_focus == WorkshopPane::Agents;
    let filter_badge = if board.attention_filter { " [!]" } else { "" };
    let block = ui::block(format!(" agents{filter_badge} ")).border_style(panel_style(focused));
    let list = ui::styled_list(items, block);

    let mut state = ListState::default();
    if !tree.is_empty() {
        let cursor = board
            .selected_agent
            .as_ref()
            .and_then(|id| tree.iter().position(|(row_id, _)| row_id == id))
            .unwrap_or(0);
        state.select(Some(cursor));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn panel_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn render_detail(frame: &mut Frame, area: Rect, board: &Board, project_id: &ProjectId) {
    let inner = ui::bordered(frame, area, ui::block(" detail "));
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let lines = match board.workshop_focus {
        WorkshopPane::Tasks => task_detail_lines(board, project_id),
        WorkshopPane::Agents => agent_detail_lines(board),
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn task_detail_lines(board: &Board, project_id: &ProjectId) -> Vec<Line<'static>> {
    let Some(task) = board
        .selected_task
        .as_ref()
        .and_then(|id| board.tasks.get(id))
        .filter(|task| &task.snapshot.project_id == project_id)
    else {
        return vec![Line::from("(no task selected)")];
    };
    let attention = factory_core::attention::task_attention(task.snapshot.status);
    let mut lines = vec![
        Line::from(Span::styled(
            task.snapshot.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "status: {:?}   assignee: {}",
            task.snapshot.status,
            task.snapshot
                .assigned_agent_id
                .as_ref()
                .map_or_else(|| "-".to_owned(), |id| id.to_string())
        )),
        Line::from(""),
    ];
    if task.body.is_empty() {
        if board.is_task_detail_pending(&task.snapshot.id) {
            lines.push(Line::from(Span::styled(
                "(loading…)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            lines.push(Line::from("(no body)"));
        }
    } else {
        lines.push(Line::from(task.body.clone()));
    }
    if let Some(result) = &task.result {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "result:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(result.clone()));
    }
    if attention == Attention::NeedsInput {
        lines.push(Line::from(""));
        let reason = task.blocked_reason.as_deref().map_or_else(
            || {
                if board.is_task_detail_pending(&task.snapshot.id) {
                    "blocked — (loading reason…)".to_owned()
                } else {
                    "blocked — no reason given".to_owned()
                }
            },
            |reason| format!("blocked — {reason}"),
        );
        lines.push(Line::from(Span::styled(
            reason,
            Style::default().fg(Color::Yellow),
        )));
    }
    lines
}

fn agent_detail_lines(board: &Board) -> Vec<Line<'static>> {
    let Some(agent) = board
        .selected_agent
        .as_ref()
        .and_then(|id| board.agents.get(id))
    else {
        return vec![Line::from("(no agent selected)")];
    };
    let mut lines = vec![
        Line::from(Span::styled(
            agent.id.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "role: {:?}   provider: {:?}",
            agent.role, agent.provider
        )),
    ];
    if let Some(worktree) = &agent.worktree {
        lines.push(Line::from(format!("worktree: {worktree}")));
    }
    if let Some(session) = board.session_for(agent) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "session (observed via hooks):",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!("state: {:?}", session.state)));
        if let Some(event) = session.last_hook_event {
            lines.push(Line::from(format!("last hook: {event:?}")));
        }
        if let Some(reason) = &session.wait_reason {
            lines.push(Line::from(format!("wait reason: {reason}")));
        }
        lines.push(Line::from(format!("worktree: {}", session.worktree)));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "~no session yet — state inferred from run history",
            Style::default().fg(Color::DarkGray),
        )));
        if let Some(run) = board.latest_run_for(&agent.id) {
            lines.push(Line::from(format!("~latest run: {:?}", run.status)));
            if let Some(activity) = &run.activity {
                lines.push(Line::from(format!("~activity: {activity}")));
            }
        }
    }
    lines
}
