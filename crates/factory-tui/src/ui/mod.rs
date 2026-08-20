//! Rendering for the fleet and selected-agent views.

mod agent;
mod building;
mod help;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::model::{Board, Mode, View};
use crate::mouse::{HitMap, Target};

pub fn draw(frame: &mut Frame, board: &Board) -> HitMap {
    let mut hits = HitMap::default();
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return hits;
    }
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    match board.view {
        View::Building => building::draw(frame, outer[0], board, &mut hits),
        View::Agent => agent::draw(frame, outer[0], board, &mut hits),
    }
    help::render_status_line(frame, outer[1], board, &mut hits);
    if matches!(
        board.mode,
        Mode::Prompt(_) | Mode::Picker(_) | Mode::TaskMenu(_) | Mode::Confirm(_) | Mode::Help
    ) {
        help::render_overlay(frame, area, board);
    }
    hits
}

pub(super) fn render_tabs(frame: &mut Frame, area: Rect, board: &Board, hits: &mut HitMap) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    const BUILDING_WIDTH: u16 = 10;
    const AGENT_START: u16 = 11;
    const AGENT_WIDTH: u16 = 7;
    let selected = Modifier::REVERSED | Modifier::BOLD;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("[{}]", View::Building.label()),
                Style::default().add_modifier(if board.view == View::Building {
                    selected
                } else {
                    Modifier::empty()
                }),
            ),
            Span::raw(" "),
            Span::styled(
                format!("[{}]", View::Agent.label()),
                Style::default().add_modifier(if board.view == View::Agent {
                    selected
                } else {
                    Modifier::empty()
                }),
            ),
        ])),
        area,
    );
    if area.width >= BUILDING_WIDTH {
        hits.add(
            Rect::new(area.x, area.y, BUILDING_WIDTH, 1),
            Target::View(View::Building),
        );
    }
    if area.width >= AGENT_START + AGENT_WIDTH {
        hits.add(
            Rect::new(area.x + AGENT_START, area.y, AGENT_WIDTH, 1),
            Target::View(View::Agent),
        );
    }
}

pub(super) fn display_width(text: &str) -> usize {
    Line::from(text).width()
}

pub(super) fn truncate_width(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max.saturating_sub(display_width("…"));
    let mut head = String::new();
    for ch in text.chars() {
        head.push(ch);
        if display_width(&head) > budget {
            head.pop();
            break;
        }
    }
    format!("{head}…")
}

pub(super) fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else if max == 0 {
        String::new()
    } else {
        format!("{}…", text.chars().take(max - 1).collect::<String>())
    }
}

pub(super) fn truncate_middle(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_owned();
    }
    if max <= 3 {
        return truncate(text, max);
    }
    let keep = max - 1;
    let head_len = keep / 2;
    let tail_len = keep - head_len;
    format!(
        "{}…{}",
        chars[..head_len].iter().collect::<String>(),
        chars[chars.len() - tail_len..].iter().collect::<String>()
    )
}

pub(super) fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

pub(super) fn block(title: impl Into<String>) -> Block<'static> {
    Block::default().borders(Borders::ALL).title(title.into())
}

pub(super) fn bordered(frame: &mut Frame, area: Rect, block: Block<'static>) -> Rect {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

pub(super) fn dim(frame: &mut Frame, area: Rect, text: impl Into<String>) {
    frame.render_widget(
        Paragraph::new(text.into()).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

pub(super) fn styled_list<'a>(items: Vec<ListItem<'a>>, block: Block<'static>) -> List<'a> {
    List::new(items)
        .block(block)
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_terminal_width() {
        assert_eq!(truncate_width("abcdef", 4), "abc…");
        assert_eq!(truncate_middle("abcdefgh", 5), "ab…gh");
    }
}
