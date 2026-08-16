//! The units (agents) and jobs (task queue) panels: single-key-navigable lists with a `>`
//! cursor, DF-style. Units show a per-agent braille activity sparkline (see `model.rs`'s
//! `ActivitySeries`); jobs show status and current assignee.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

use factory_core::{AgentRole, TaskStatus};

use crate::model::{ActivitySeries, Board, Panel, braille_sparkline};
use crate::ui::{floor::state_color, pad, truncate};

const SPARKLINE_WIDTH: usize = 10;

fn panel_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

pub fn render_units(frame: &mut Frame, area: Rect, board: &Board) {
    let units = board.units();
    let items: Vec<ListItem> = units
        .iter()
        .map(|unit| {
            let state = board.state_of(&unit.id);
            let glyph = if unit.role == AgentRole::Orchestrator {
                'Ω'
            } else {
                state.glyph()
            };
            let counts = board
                .activity
                .get(&unit.id)
                .map(ActivitySeries::counts)
                .unwrap_or_default();
            let spark = braille_sparkline(&counts, SPARKLINE_WIDTH);
            let line = Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(state_color(state))),
                Span::raw(pad(unit.id.as_str(), 10)),
                Span::styled(
                    pad(state.label(), 9),
                    Style::default().fg(state_color(state)),
                ),
                Span::styled(spark, Style::default().fg(Color::Cyan)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let focused = board.focus == Panel::Units;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(panel_border_style(focused))
        .title("units");
    let list = List::new(items)
        .block(block)
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));

    let mut state = ListState::default();
    if !units.is_empty() {
        state.select(Some(board.units_selected.min(units.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn task_glyph(status: TaskStatus) -> char {
    match status {
        TaskStatus::Queued => '#',
        TaskStatus::Running => '>',
        TaskStatus::Blocked => '?',
        TaskStatus::Succeeded => '\u{2713}',
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

pub fn render_jobs(frame: &mut Frame, area: Rect, board: &Board) {
    let jobs = board.jobs();
    let items: Vec<ListItem> = jobs
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
                    format!("{} ", task_glyph(status)),
                    Style::default().fg(task_color(status)),
                ),
                Span::raw(pad(
                    &format!("#{}", truncate(task.snapshot.id.as_str(), 9)),
                    11,
                )),
                Span::raw(pad(&task.snapshot.title, 16)),
                Span::styled(
                    format!("\u{2192} {assignee}"),
                    Style::default().fg(Color::Gray),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let focused = board.focus == Panel::Jobs;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(panel_border_style(focused))
        .title("jobs");
    let list = List::new(items)
        .block(block)
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));

    let mut state = ListState::default();
    if !jobs.is_empty() {
        state.select(Some(board.jobs_selected.min(jobs.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
}
