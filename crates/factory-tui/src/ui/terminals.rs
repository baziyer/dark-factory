//! TERMINALS: tiled live PTYs of the focused project's live sessions (2-4 panes). `pane_rects` is
//! the spike's tiling math, extended from a fixed 2-pane split to 1-4 (design brief: "2-4 panes,
//! spike's `pane_rects`").

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use factory_core::SessionId;

use tui_term::widget::PseudoTerminal;

use crate::model::Board;
use crate::pane::PaneMap;

/// Splits `area` into `n` (1-4) tiles: 1 = full; 2 = side by side; 3 = one full-width pane on top
/// of two half-width panes; 4 = a 2x2 grid. `n == 0` returns no rects. `n > 4` is treated as 4 (a
/// defensive clamp — callers are expected to have already capped at `MAX_TERMINAL_PANES`).
#[must_use]
pub fn pane_rects(area: Rect, n: usize) -> Vec<Rect> {
    match n {
        0 => Vec::new(),
        1 => vec![area],
        2 => Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area)
            .to_vec(),
        3 => {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
            let bottom = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(rows[1]);
            vec![rows[0], bottom[0], bottom[1]]
        }
        _ => {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
            let top = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(rows[0]);
            let bottom = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(rows[1]);
            vec![top[0], top[1], bottom[0], bottom[1]]
        }
    }
}

pub fn draw(frame: &mut Frame, area: Rect, board: &Board, panes: &mut PaneMap) {
    let targets = board.terminal_targets();
    if targets.is_empty() {
        render_placeholder(frame, area, board);
        return;
    }

    let focused_session = board.focus_target();

    for (session_id, rect) in targets.iter().zip(pane_rects(area, targets.len())) {
        render_pane_tile(
            frame,
            rect,
            board,
            panes,
            session_id,
            focused_session.as_ref() == Some(session_id),
        );
    }
}

fn render_placeholder(frame: &mut Frame, area: Rect, board: &Board) {
    let block = Block::default().borders(Borders::ALL).title(" terminals ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let message = if board.focused_project.is_none() {
        "no project focused — press Enter on an agent in FORTRESS first"
    } else {
        "no live sessions in this project yet (pending session support landing — see README)"
    };
    frame.render_widget(
        Paragraph::new(message).style(Style::default().fg(Color::DarkGray)),
        inner,
    );
}

fn render_pane_tile(
    frame: &mut Frame,
    rect: Rect,
    board: &Board,
    panes: &mut PaneMap,
    session_id: &SessionId,
    is_focused: bool,
) {
    let Some(pane) = panes.get_mut(session_id) else {
        let agent_label = board
            .agent_for_pane_session(session_id)
            .map_or_else(|| session_id.to_string(), |agent| agent.id.to_string());
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {agent_label} "));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        frame.render_widget(
            Paragraph::new("attaching…").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };

    let exited = pane.has_exited();
    let marker = if exited { " [exited]" } else { "" };
    let command = match pane.kind {
        crate::pane::PaneKind::LocalPty => format!(" — {}", pane.command.join(" ")),
        crate::pane::PaneKind::Daemon => String::new(),
    };
    let color = if is_focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let title = format!(" {}{command}{marker} ", pane.title);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(title);
    let inner = block.inner(rect);
    if inner.width > 0 && inner.height > 0 {
        let _ = pane.resize(inner.height, inner.width);
    }

    if let Some(error) = pane.attach_error() {
        frame.render_widget(block, rect);
        let lines = vec![Line::from(Span::styled(
            error,
            Style::default().fg(Color::Red),
        ))];
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let parser = pane.lock_parser();
    let screen = parser.screen();
    let pseudo = PseudoTerminal::new(screen).block(block);
    frame.render_widget(pseudo, rect);
}
