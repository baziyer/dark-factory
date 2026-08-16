//! The Dwarf-Fortress-style ASCII floor: one glyph per agent at its "workstation", colored by
//! [`AgentState`]. The orchestrator (if any) gets a distinct glyph and always renders first (the
//! "god" desk, per the design brief).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use factory_core::{AgentRole, TaskStatus};

use crate::model::{AgentState, Board};
use crate::ui::pad;

const CELL_WIDTH: usize = 20;

pub(super) fn state_color(state: AgentState) -> Color {
    match state {
        AgentState::Idle => Color::Gray,
        AgentState::Working => Color::Green,
        AgentState::Waiting => Color::Yellow,
        AgentState::Stopped => Color::Blue,
        AgentState::Failed => Color::Red,
    }
}

pub fn render(frame: &mut Frame, area: Rect, board: &Board) {
    let queued = board
        .tasks
        .values()
        .filter(|task| task.snapshot.status == TaskStatus::Queued)
        .count();
    let title = board.project.as_ref().map_or_else(
        || "floor".to_owned(),
        |project| format!("floor — {} (queue: {queued})", project.name),
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let units = board.units();
    if units.is_empty() {
        let message = if board.project.is_some() {
            "no agents yet"
        } else {
            "connecting…"
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let columns = (usize::from(inner.width) / CELL_WIDTH).max(1);
    let mut lines: Vec<Line> = Vec::new();
    for (row_index, chunk) in units.chunks(columns).enumerate() {
        if row_index > 0 {
            lines.push(Line::from(""));
        }
        let mut top: Vec<Span> = Vec::new();
        let mut bottom: Vec<Span> = Vec::new();
        for unit in chunk {
            let state = board.state_of(&unit.id);
            let is_orchestrator = unit.role == AgentRole::Orchestrator;
            let glyph = if is_orchestrator { 'Ω' } else { state.glyph() };
            let selected = board
                .selected_agent()
                .is_some_and(|selected| selected.id == unit.id);
            let marker = if selected { '*' } else { ' ' };
            let name = pad(unit.id.as_str(), CELL_WIDTH - 4);
            let mut style = Style::default().fg(state_color(state));
            if is_orchestrator {
                style = style.add_modifier(Modifier::BOLD);
            }
            top.push(Span::styled(format!("{marker}{glyph} {name}"), style));

            let desk = if is_orchestrator { "[helm]" } else { "[desk]" };
            let label = pad(&format!("  {desk} {}", state.label()), CELL_WIDTH);
            bottom.push(Span::styled(label, Style::default().fg(Color::DarkGray)));
        }
        lines.push(Line::from(top));
        lines.push(Line::from(bottom));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}
