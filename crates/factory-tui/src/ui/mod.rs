//! Rendering for all four views (`Board::view`), dispatched from [`draw`], plus the shared bottom
//! status/help line and modal overlays (prompt/picker/task-menu/confirm/help), which every view
//! can show on top of itself.

mod announcements;
mod focus;
mod fortress_view;
mod help;
mod terminals;
mod workshop;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::model::{Board, Mode, View};
use crate::pane::PaneMap;

pub fn draw(frame: &mut Frame, board: &Board, panes: &mut PaneMap) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let status_height = 1u16.min(area.height);
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(status_height)])
        .split(area);
    let (content_area, status_area) = (outer[0], outer[1]);

    match board.view {
        View::Fortress => fortress_view::draw(frame, content_area, board),
        View::Workshop => workshop::draw(frame, content_area, board),
        View::Terminals => terminals::draw(frame, content_area, board, panes),
        View::Focus => focus::draw(frame, content_area, board, panes),
    }
    help::render_status_line(frame, status_area, board);

    if matches!(
        board.mode,
        Mode::Prompt(_) | Mode::Picker(_) | Mode::TaskMenu(_) | Mode::Confirm(_) | Mode::Help
    ) {
        help::render_overlay(frame, area, board);
    }
}

/// Truncates `text` to at most `max` characters, appending an ellipsis if it was cut.
pub(super) fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else if max == 0 {
        String::new()
    } else {
        let head: String = text.chars().take(max - 1).collect();
        format!("{head}\u{2026}")
    }
}

/// Pads `text` to exactly `width` columns (truncating first if it's already longer).
pub(super) fn pad(text: &str, width: usize) -> String {
    let truncated = truncate(text, width);
    let len = truncated.chars().count();
    if len >= width {
        truncated
    } else {
        format!("{truncated}{}", " ".repeat(width - len))
    }
}

/// A `Rect` centered within `area`, `width` columns by `height` rows (clamped to fit).
pub(super) fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}
