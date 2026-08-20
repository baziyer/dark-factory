//! AGENT: durable attempt state, queue, inbox, and settings.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use factory_core::{RunPhase, TaskDetail, TaskStatus};

use crate::model::{AttentionFocus, Board};
use crate::mouse::{HitMap, Target};
use crate::ui;

pub fn draw(frame: &mut Frame, area: Rect, board: &Board, hits: &mut HitMap) {
    let area = if let Some(focus) = &board.attention_focus {
        let panels = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(area.height.min(9)), Constraint::Min(0)])
            .split(area);
        render_attention_card(frame, panels[0], board, focus, hits);
        panels[1]
    } else {
        area
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(area);
    render_attempt(frame, columns[0], board);
    render_context(frame, columns[1], board, hits);
}

fn render_attempt(frame: &mut Frame, area: Rect, board: &Board) {
    let Some(agent_id) = board.selected_agent.as_ref() else {
        let inner = ui::bordered(frame, area, ui::block(" attempt "));
        ui::dim(frame, inner, "no agent selected");
        return;
    };
    let Some(agent) = board.agents.get(agent_id) else {
        return;
    };
    let mut lines = vec![Line::from(format!("agent: {}", agent.id))];
    if let Some(run) = board.latest_run_for(agent_id) {
        lines.extend([
            Line::from(format!("run: {}", run.id)),
            Line::from(format!("phase: {:?}", run.phase)),
            Line::from(format!("task: {}", run.task_id)),
            Line::from(format!("provider: {:?}", run.provider)),
            Line::from(format!(
                "activity: {}",
                run.activity.as_deref().unwrap_or("—")
            )),
            Line::from(format!(
                "wait: {}",
                run.wait_reason.as_deref().unwrap_or("—")
            )),
            Line::from(format!("observer: {:?}", run.observer_health)),
            Line::from(format!(
                "outcome: {}",
                run.outcome
                    .as_ref()
                    .map_or_else(|| "—".to_owned(), |outcome| format!("{outcome:?}"))
            )),
        ]);
        if run.phase == RunPhase::Finalizing {
            lines.push(Line::from(Span::styled(
                "mutation authority revoked; resources are finalizing",
                Style::default().fg(Color::Yellow),
            )));
        }
    } else {
        lines.push(Line::from("no admitted attempt"));
    }
    frame.render_widget(Paragraph::new(lines).block(ui::block(" attempt ")), area);
}

fn render_context(frame: &mut Frame, area: Rect, board: &Board, hits: &mut HitMap) {
    let Some(agent_id) = board.selected_agent.as_ref() else {
        return;
    };
    let Some(agent) = board.agents.get(agent_id) else {
        return;
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(42),
            Constraint::Percentage(23),
            Constraint::Percentage(35),
        ])
        .split(area);

    let task_area = ui::block("").inner(rows[0]);
    let mut queue: Vec<Line> = board
        .active_tasks_for_agent(agent_id)
        .iter()
        .enumerate()
        .map(|(row, task)| {
            hits.add_row(task_area, row, Target::Task(task.snapshot.id.clone()));
            Line::from(format!(
                "{} {:?} p={} {}",
                if board.selected_task.as_ref() == Some(&task.snapshot.id) {
                    ">"
                } else {
                    " "
                },
                task.snapshot.status,
                task.snapshot.priority,
                task.snapshot.title
            ))
        })
        .collect();
    if queue.is_empty() {
        queue.push(Line::from("nothing assigned"));
    }
    for task in board.task_history_for_agent(agent_id) {
        if matches!(
            task.snapshot.status,
            TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            hits.add_row(
                task_area,
                queue.len(),
                Target::Task(task.snapshot.id.clone()),
            );
        }
        queue.push(Line::from(format!(
            "  {:?} p={} {}",
            task.snapshot.status, task.snapshot.priority, task.snapshot.title
        )));
    }
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

    let settings = board.agent_details.get(agent_id).map_or_else(
        || vec![Line::from("loading…")],
        |detail| {
            let run = board.latest_run_for(agent_id);
            vec![
                Line::from(format!(
                    "model: {} → {}",
                    detail
                        .profile
                        .model
                        .as_deref()
                        .unwrap_or("provider default"),
                    run.and_then(|run| run.runtime_model.as_deref())
                        .unwrap_or("unreported")
                )),
                Line::from(format!(
                    "effort: {} → {}",
                    detail
                        .profile
                        .reasoning_effort
                        .as_deref()
                        .unwrap_or("provider default"),
                    run.and_then(|run| run.runtime_reasoning_effort.as_deref())
                        .unwrap_or("unreported")
                )),
                Line::from(format!(
                    "access: {} → {}",
                    detail
                        .profile
                        .permission_mode
                        .as_deref()
                        .unwrap_or("provider default"),
                    run.and_then(|run| run.runtime_permission_mode.as_deref())
                        .unwrap_or("unreported")
                )),
                Line::from(format!(
                    "memory: {} {}/{}",
                    memory_health_state(detail.memory_health.state),
                    detail.memory_health.bytes,
                    detail.memory_health.max_bytes
                )),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(settings).block(ui::block(" settings ")),
        rows[2],
    );

    if agent.role == factory_core::AgentRole::Orchestrator {
        // The durable queue above already includes orchestrator-owned work. Project backlog remains
        // visible in BUILDING, avoiding a second orchestration-specific state projection here.
    }
}

fn memory_health_state(state: factory_core::local::GuidanceHealthState) -> &'static str {
    match state {
        factory_core::local::GuidanceHealthState::Ok => "ok",
        factory_core::local::GuidanceHealthState::NearLimit => "near_limit",
        factory_core::local::GuidanceHealthState::Oversized => "oversized",
        factory_core::local::GuidanceHealthState::InvalidUtf8 => "invalid_utf8",
        factory_core::local::GuidanceHealthState::PathError => "path_error",
    }
}

pub(crate) fn render_attention_card(
    frame: &mut Frame,
    area: Rect,
    board: &Board,
    focus: &AttentionFocus,
    hits: &mut HitMap,
) {
    let item = &focus.item;
    let pending = board.attention_is_pending(item);
    let title = if focus.resolved {
        " ACTION — STALE "
    } else if pending {
        " ACTION — PENDING "
    } else {
        " ACTION — NEEDS YOU "
    };
    let width = usize::from(area.width.saturating_sub(2));
    let decision = item.decision();
    let mut lines = vec![
        Line::from(Span::styled(
            item.reason.kind.label(),
            Style::default().fg(if focus.resolved {
                Color::DarkGray
            } else {
                Color::Yellow
            }),
        )),
        Line::from(ui::truncate(&item.reason.summary, width)),
        Line::from(format!(
            "project: {}  agent: {}",
            item.project_id,
            item.agent_id
                .as_ref()
                .map_or("—", factory_core::AgentId::as_str)
        )),
        Line::from(format!(
            "task: {}  run: {}",
            item.task_id
                .as_ref()
                .map_or("—", factory_core::TaskId::as_str),
            item.run_id
                .as_ref()
                .map_or("—", factory_core::RunId::as_str)
        )),
        Line::from(format!(
            "age: {}",
            factory_core::status::age_text(board.now_ms, item.since_ms)
        )),
    ];
    if !focus.resolved {
        for (index, choice) in decision.choices.iter().enumerate() {
            hits.add_row(
                ui::block("").inner(area),
                5 + index,
                Target::AttentionChoice(item.clone(), index),
            );
            lines.push(Line::from(ui::truncate(
                &format!("{}. {} — {}", index + 1, choice.label, choice.consequence),
                width,
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines).block(ui::block(title)), area);
}

fn _task_owner(_task: &TaskDetail) {}
