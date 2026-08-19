//! BUILDING: project-grouped agent floors plus the oldest-first operator attention list.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::model::state::{self, AgentState};
use crate::model::{Board, provider_letter};
use crate::mouse::{HitMap, Target};
use crate::ui;

pub fn draw(frame: &mut Frame, area: Rect, board: &Board, hits: &mut HitMap) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(area);
    render_floors(frame, columns[0], board, hits);
    render_needs_you(frame, columns[1], board, hits);
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

fn render_floors(frame: &mut Frame, area: Rect, board: &Board, hits: &mut HitMap) {
    let inner = ui::bordered(frame, area, ui::block(" BUILDING · activity last 40s "));
    if board.projects.is_empty() {
        ui::dim(frame, inner, "no projects — factoryctl project add");
        return;
    }
    let mut lines = Vec::new();
    let mut row = 0;
    for project in board.projects_sorted() {
        lines.push(Line::from(Span::styled(
            format!(" {} ", project.name),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        row += 1;
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
            let compact = inner.width < 70;
            let glyph = if agent.role == factory_core::AgentRole::Orchestrator {
                board.theme.orchestrator
            } else {
                provider_letter(agent.provider)
            };
            let spark = if compact {
                String::new()
            } else {
                board
                    .activity
                    .get(&agent_id)
                    .map(|series| state::braille_sparkline(&series.counts(), 8))
                    .unwrap_or_else(|| "        ".to_owned())
            };
            let assigned = board.active_tasks_for_agent(&agent_id);
            let current = assigned
                .iter()
                .find(|task| task.snapshot.status == factory_core::TaskStatus::Running)
                .or_else(|| assigned.first())
                .map(|task| ui::truncate(&task.snapshot.title, 24));
            let queue = (!assigned.is_empty()).then(|| format!("queue {}", assigned.len()));
            let activity = board.activity_label(agent);
            let route = if agent.parent_agent_id.is_some() {
                "══ "
            } else {
                ""
            };
            let prefix = if compact {
                format!(
                    "{selected} {}{route}{glyph} {:<10} {}",
                    "  ".repeat(usize::from(depth)),
                    ui::truncate_middle(agent_id.as_str(), 10),
                    state.label(),
                )
            } else {
                format!(
                    "{selected} {}{route}{glyph} {:<18} {:?} {:<8} {spark}",
                    "  ".repeat(usize::from(depth)),
                    ui::truncate_middle(agent_id.as_str(), 18),
                    agent.provider,
                    state.label(),
                )
            };
            let priority = match (queue, current) {
                (Some(queue), Some(title)) => format!(" {queue} task {title}"),
                (Some(queue), None) => format!(" {queue}"),
                (None, Some(title)) => format!(" task {title}"),
                (None, None) => String::new(),
            };
            let activity_width = usize::from(inner.width)
                .saturating_sub(prefix.chars().count())
                .saturating_sub(priority.chars().count())
                .saturating_sub(1);
            lines.push(Line::from(vec![
                Span::styled(prefix, state_style(state)),
                Span::raw(priority),
                Span::raw(format!(" {}", ui::truncate(&activity, activity_width))),
            ]));
            hits.add_row(inner, row, Target::Agent(agent_id));
            row += 1;
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_needs_you(frame: &mut Frame, area: Rect, board: &Board, hits: &mut HitMap) {
    let inner = ui::bordered(
        frame,
        area,
        ui::block(" NEEDS YOU — priority, then oldest "),
    );
    let items = board.attention_items();
    if items.is_empty() {
        ui::dim(frame, inner, "nothing needs you");
        return;
    }
    let lines = items
        .into_iter()
        .enumerate()
        .map(|(row, item)| {
            hits.add_row(inner, row, Target::Attention(item.clone()));
            let selected = board.attention_focus.as_ref().is_some_and(|focus| {
                !focus.resolved && crate::model::same_attention_source(&focus.item, &item)
            });
            let subject = item.agent_id.as_ref().map_or_else(
                || {
                    item.task_id
                        .as_ref()
                        .map_or_else(|| item.project_id.to_string(), |id| format!("task#{id}"))
                },
                |id| format!("agent {id}"),
            );
            Line::from(vec![
                Span::raw(if selected { "> " } else { "  " }),
                Span::styled(
                    format!("{}: ", item.reason.kind.label()),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(format!(
                    "{} :: {}/{subject}",
                    item.reason.summary, item.project_id
                )),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{agent, attention, project, run, task};
    use factory_core::{AgentRole, RunStatus, TaskStatus, status::AttentionReasonKind};
    use ratatui::{Terminal, backend::TestBackend};

    fn rendered_building_text(width: u16) -> String {
        let mut board = Board::new(false, 0, crate::theme::PLAIN);
        let mut current = task("task", "proj", TaskStatus::Running, Some("alice"), 0);
        current.snapshot.title = "current task".to_owned();
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![agent("alice", "proj", AgentRole::Worker, None)],
            vec![current],
            vec![run("alice", "proj", RunStatus::Running, 0)],
            Vec::new(),
        );
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &board, &mut HitMap::default()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

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
        board.attention = vec![attention(
            AttentionReasonKind::Inferred,
            Some("alice"),
            Some("task"),
            None,
            0,
        )];
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &board, &mut HitMap::default()))
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
        assert!(text.contains("queue 1"));
        assert!(text.contains("task task"));
        assert!(
            text.contains("no rece"),
            "activity should remain visible after priority text"
        );
        assert!(text.contains("inference"));
        assert!(text.contains("lifecycle state"));
    }

    #[test]
    fn building_keeps_queue_and_current_task_visible_at_narrow_widths() {
        for width in [120, 80] {
            let text = rendered_building_text(width);
            assert!(
                text.contains("queue 1"),
                "queue missing at width {width}: {text}"
            );
            assert!(
                text.contains("task current task"),
                "current task missing at width {width}: {text}"
            );
        }
    }
}
