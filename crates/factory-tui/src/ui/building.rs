//! BUILDING: project-grouped agent floors plus the oldest-first operator attention list.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::model::state::{self, AgentState};
use crate::model::{AttentionTarget, Board, provider_letter};
use crate::ui;

pub fn draw(frame: &mut Frame, area: Rect, board: &Board) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(area);
    render_floors(frame, columns[0], board);
    render_needs_you(frame, columns[1], board);
}

fn state_style(state: AgentState) -> Style {
    match state {
        AgentState::Working => Style::default().fg(Color::Green),
        AgentState::Waiting => Style::default().fg(Color::Yellow),
        AgentState::Failed => Style::default().fg(Color::Red),
        AgentState::Stopped => Style::default().fg(Color::Blue),
        AgentState::Idle => Style::default().fg(Color::DarkGray),
    }
}

fn render_floors(frame: &mut Frame, area: Rect, board: &Board) {
    let inner = ui::bordered(frame, area, ui::block(" BUILDING "));
    if board.projects.is_empty() {
        ui::dim(frame, inner, "no projects — factoryctl project add");
        return;
    }
    let mut lines = Vec::new();
    for project in board.projects_sorted() {
        lines.push(Line::from(Span::styled(
            format!(" {} ", project.name),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for (agent_id, depth) in board.agent_tree(&project.id) {
            let Some(agent) = board.agents.get(&agent_id) else {
                continue;
            };
            let state = board.agent_state(agent).value;
            let selected = if board.selected_agent.as_ref() == Some(&agent_id) {
                ">"
            } else {
                " "
            };
            let glyph = if agent.role == factory_core::AgentRole::Orchestrator {
                board.theme.orchestrator
            } else {
                provider_letter(agent.provider)
            };
            let spark = board
                .activity
                .get(&agent_id)
                .map(|series| {
                    state::braille_sparkline(&series.counts(), state::ACTIVITY_VISIBLE_BUCKETS)
                })
                .unwrap_or_else(|| " ".repeat(state::ACTIVITY_VISIBLE_BUCKETS));
            let assigned: Vec<_> = board
                .tasks
                .values()
                .filter(|task| {
                    task.snapshot.assigned_agent_id.as_ref() == Some(&agent_id)
                        && matches!(
                            task.snapshot.status,
                            factory_core::TaskStatus::Queued | factory_core::TaskStatus::Running
                        )
                })
                .collect();
            let current = assigned
                .iter()
                .find(|task| task.snapshot.status == factory_core::TaskStatus::Running)
                .or_else(|| assigned.first())
                .map_or("idle", |task| task.snapshot.title.as_str());
            let route = if agent.parent_agent_id.is_some() {
                "══ "
            } else {
                ""
            };
            lines.push(Line::from(vec![
                Span::raw(format!(
                    "{selected} {}{route}",
                    "  ".repeat(usize::from(depth))
                )),
                Span::styled(
                    format!("{glyph} {:<18}", ui::truncate_middle(agent_id.as_str(), 18)),
                    state_style(state),
                ),
                Span::raw(format!(
                    " {:?} {:<8} {spark} q:{}  {}",
                    agent.provider,
                    state.label(),
                    assigned.len(),
                    ui::truncate(current, 28)
                )),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_needs_you(frame: &mut Frame, area: Rect, board: &Board) {
    let inner = ui::bordered(frame, area, ui::block(" NEEDS YOU — oldest first "));
    let items = board.attention_items();
    if items.is_empty() {
        ui::dim(frame, inner, "nothing needs you");
        return;
    }
    let lines = items
        .into_iter()
        .map(|item| {
            let inferred = if item.inferred { "~" } else { "" };
            let (selected, label) = match item.target {
                AttentionTarget::Agent(id) => (
                    board.selected_agent.as_ref() == Some(&id) && board.selected_task.is_none(),
                    format!("agent {id}"),
                ),
                AttentionTarget::Task(id) => (
                    board.selected_task.as_ref() == Some(&id),
                    board.tasks.get(&id).map_or_else(
                        || format!("task#{id}"),
                        |task| format!("task {}", task.snapshot.title),
                    ),
                ),
            };
            Line::from(vec![
                Span::raw(if selected { "> " } else { "  " }),
                Span::styled(
                    format!("{inferred}{:?} ", item.attention),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(format!("{} :: {label}", item.project_id)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{agent, project, run, task};
    use factory_core::{AgentRole, RunStatus, TaskStatus};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn building_renders_project_floor_queue_and_needs_you() {
        let mut board = Board::new(false, 0, crate::theme::PLAIN);
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![agent("alice", "proj", AgentRole::Worker, None)],
            vec![task("task", "proj", TaskStatus::Queued, Some("alice"), 0)],
            vec![run("alice", "proj", RunStatus::Failed, 0)],
            Vec::new(),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &board))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("BUILDING"));
        assert!(text.contains("NEEDS YOU"));
        assert!(text.contains("alice"));
        assert!(text.contains("q:1"));
    }
}
