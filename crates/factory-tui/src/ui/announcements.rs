//! The announcements panel: attention-ranked, Dwarf-Fortress-terse daemon event log
//! (`20:41 bob    task#42 done`) — failures/needs-input float above routine chatter, per the
//! design brief. Backed by `Board::announcements`, a bounded ring buffer (`model::state`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::model::{Board, announcements::ranked};
use crate::ui;
use factory_core::attention::Attention;

fn attention_style(attention: Attention) -> Style {
    match attention {
        Attention::Routine => Style::default(),
        Attention::Completed => Style::default().fg(Color::Blue),
        Attention::Failed => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        Attention::NeedsInput => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    }
}

pub fn render(frame: &mut Frame, area: Rect, board: &Board) {
    let count = board.announcements.len();
    let title = if board.caught_up {
        format!("announcements ({count})")
    } else {
        format!("announcements ({count}, catching up…)")
    };
    let inner = ui::bordered(frame, area, ui::block(title));
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if board.announcements.is_empty() {
        ui::dim(frame, inner, "(nothing yet)");
        return;
    }

    let visible = usize::from(inner.height);
    let lines: Vec<Line> = ranked(board.announcements.iter(), visible)
        .into_iter()
        .map(|announcement| {
            Line::from(Span::styled(
                announcement.text.clone(),
                attention_style(announcement.attention),
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}
