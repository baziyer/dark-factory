//! The announcements panel: a scrolling, terse, Dwarf-Fortress-style log of daemon events
//! (`20:41 bob    task#42 done`), newest at the bottom. Backed by `Board::announcements`, a
//! bounded ring buffer - see `model.rs`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::model::Board;

pub fn render(frame: &mut Frame, area: Rect, board: &Board) {
    let title = if board.caught_up {
        "announcements".to_owned()
    } else {
        "announcements (catching up…)".to_owned()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if board.announcements.is_empty() {
        frame.render_widget(
            Paragraph::new("(nothing yet)").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let visible = usize::from(inner.height);
    let start = board.announcements.len().saturating_sub(visible);
    let lines: Vec<Line> = board
        .announcements
        .iter()
        .skip(start)
        .map(|announcement| Line::from(announcement.text.clone()))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}
