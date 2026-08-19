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

fn compact_activity(activity: &str, width: usize) -> String {
    if ui::display_width(activity) <= width {
        return activity.to_owned();
    }
    let words = activity.split_whitespace().collect::<Vec<_>>();
    if let [.., age, "ago"] = words.as_slice() {
        let suffix = format!("{age} ago");
        let suffix_width = ui::display_width(&suffix);
        if suffix_width <= width {
            let prefix_width = width.saturating_sub(suffix_width).saturating_sub(1);
            if prefix_width == 0 {
                return suffix;
            }
            let prefix_source = activity
                .strip_suffix(&suffix)
                .expect("suffix came from the same activity label")
                .trim_end();
            return format!(
                "{} {suffix}",
                ui::truncate_width(prefix_source, prefix_width)
            );
        }
    }
    ui::truncate_width(activity, width)
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
            let glyph = if agent.role == factory_core::AgentRole::Orchestrator {
                board.theme.orchestrator
            } else {
                provider_letter(agent.provider)
            };
            let spark = board
                .activity
                .get(&agent_id)
                .map(|series| state::braille_sparkline(&series.counts(), 8))
                .unwrap_or_else(|| "        ".to_owned());
            let assigned = board.active_tasks_for_agent(&agent_id);
            let current = assigned
                .iter()
                .find(|task| task.snapshot.status == factory_core::TaskStatus::Running)
                .or_else(|| assigned.first())
                .map(|task| task.snapshot.title.as_str());
            let queue = (!assigned.is_empty()).then(|| format!("queue {}", assigned.len()));
            let activity = board.activity_label(agent);
            let route = if agent.parent_agent_id.is_some() {
                "══ "
            } else {
                ""
            };
            let route_prefix = format!("{selected} {}{route}", "  ".repeat(usize::from(depth)));
            let detailed_identity =
                format!("{glyph} {:<18}", ui::truncate_middle(agent_id.as_str(), 18));
            let detailed_state = format!(" {:?} {:<8} {spark}", agent.provider, state.label());
            let compact_identity =
                format!("{glyph} {:<10}", ui::truncate_middle(agent_id.as_str(), 10));
            let compact_state = format!(" {}", state.label());
            let queue_width = queue
                .as_deref()
                .map_or(0, |value| 1 + ui::display_width(value));
            let task_preferred_width =
                current.map_or(0, |title| 6 + ui::display_width(title).min(12));
            let activity_preferred_width = 1 + ui::display_width(&activity).min(18);
            let preferred_tail = queue_width + task_preferred_width + activity_preferred_width;
            let detailed_width = ui::display_width(&route_prefix)
                + ui::display_width(&detailed_identity)
                + ui::display_width(&detailed_state);
            let compact = detailed_width + preferred_tail > usize::from(inner.width);
            let (identity, state_text) = if compact {
                (compact_identity, compact_state)
            } else {
                (detailed_identity, detailed_state)
            };

            let mut spans = vec![
                Span::raw(route_prefix),
                Span::styled(identity, state_style(state)),
                Span::raw(state_text),
            ];
            let prefix_width = spans.iter().map(Span::width).sum::<usize>();
            let mut remaining = usize::from(inner.width).saturating_sub(prefix_width);
            if let Some(queue) = queue {
                let text = format!(" {queue}");
                remaining = remaining.saturating_sub(ui::display_width(&text));
                spans.push(Span::raw(text));
            }

            let activity_reserve = (1 + ui::display_width(&activity).min(7)).min(remaining);
            if let Some(title) = current {
                let task_capacity = remaining.saturating_sub(activity_reserve);
                if task_capacity >= 6 {
                    let title_width = task_capacity.saturating_sub(6).min(24);
                    let text = format!(" task {}", ui::truncate_width(title, title_width));
                    remaining = remaining.saturating_sub(ui::display_width(&text));
                    spans.push(Span::raw(text));
                }
            }
            if remaining > 1 {
                spans.push(Span::raw(format!(
                    " {}",
                    compact_activity(&activity, remaining - 1)
                )));
            }
            lines.push(Line::from(spans));
            hits.add_row(inner, row, Target::Agent(agent_id));
            row += 1;
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_needs_you(frame: &mut Frame, area: Rect, board: &Board, hits: &mut HitMap) {
    if let Some(focus) = board
        .attention_focus
        .as_ref()
        .filter(|focus| !focus.resolved)
    {
        crate::ui::agent::render_attention_card(frame, area, board, focus, hits);
        return;
    }
    let inner = ui::bordered(
        frame,
        area,
        ui::block(" NEEDS YOU — priority, then oldest "),
    );
    let items = board.decision_items();
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
                    factory_core::status::display_text(&item.reason.summary),
                    item.project_id
                )),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{agent, attention, project, run, session, task};
    use factory_core::{
        AgentRole, RunStatus, SessionId, SessionState, TaskStatus, status::AttentionReasonKind,
    };
    use ratatui::{Terminal, backend::TestBackend};

    fn rendered_nested_building(width: u16, title: &str) -> (String, ratatui::buffer::Buffer) {
        let mut board = Board::new(false, 120_000, crate::theme::PLAIN);
        let mut child = agent(
            "nested-worker",
            "proj",
            AgentRole::Worker,
            Some("orchestrator"),
        );
        child.current_session_id = Some(SessionId::try_from("sess-1").unwrap());
        let mut live = session("sess-1", "nested-worker", "proj", SessionState::Working);
        live.activity = Some("tool: Read".to_owned());
        live.last_hook_at_ms = Some(110_000);
        let mut current = task(
            "task",
            "proj",
            TaskStatus::Running,
            Some("nested-worker"),
            0,
        );
        current.snapshot.title = title.to_owned();
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![
                agent("orchestrator", "proj", AgentRole::Orchestrator, None),
                child,
            ],
            vec![current],
            vec![run("nested-worker", "proj", RunStatus::Running, 0)],
            vec![live],
        );
        board.selected_agent = Some(factory_core::AgentId::try_from("nested-worker").unwrap());
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &board, &mut HitMap::default()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text = buffer.content.iter().map(|cell| cell.symbol()).collect();
        (text, buffer)
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
            AttentionReasonKind::BudgetExhausted,
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
        assert!(text.contains("budget exhausted"));
    }

    #[test]
    fn selected_needs_you_row_keeps_building_visible_and_renders_decision_card() {
        let mut board = Board::new(false, 0, crate::theme::PLAIN);
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![agent("alice", "proj", AgentRole::Worker, None)],
            vec![task(
                "blocked",
                "proj",
                TaskStatus::Blocked,
                Some("alice"),
                0,
            )],
            Vec::new(),
            Vec::new(),
        );
        let item = attention(
            AttentionReasonKind::BudgetExhausted,
            Some("alice"),
            Some("blocked"),
            None,
            0,
        );
        board.attention = vec![item.clone()];
        board.attention_focus = Some(crate::model::AttentionFocus {
            item,
            resolved: false,
        });
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
        assert!(text.contains("ACTION — NEEDS YOU"));
        assert!(text.contains("Reset budget"));
        assert!(text.contains("task: blocked"));
    }

    #[test]
    fn building_degrades_deliberately_without_losing_queue_task_or_activity_age() {
        for width in [80, 102, 120, 124] {
            let (text, buffer) = rendered_nested_building(
                width,
                "界面 activity task with a deliberately long title",
            );
            assert!(
                text.contains("queue 1"),
                "queue missing at width {width}: {text}"
            );
            assert!(
                text.contains("task "),
                "task label missing at width {width}: {text}"
            );
            assert!(
                text.contains("task 界"),
                "Unicode task title missing at width {width}: {text}"
            );
            assert!(
                text.contains("10s ago"),
                "activity age missing at width {width}: {text}"
            );
            if width >= 120 {
                assert!(
                    text.contains("tool"),
                    "activity description missing at width {width}: {text}"
                );
            }
            let selected = buffer
                .content
                .iter()
                .find(|cell| cell.symbol() == ">")
                .expect("selected nested agent marker");
            assert_eq!(
                selected.fg,
                Color::Reset,
                "selection marker inherited state color"
            );
        }
    }
}
