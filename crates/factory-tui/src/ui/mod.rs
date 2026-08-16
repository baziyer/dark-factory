//! Rendering for the board (everything except a zoomed live terminal pane, which `main.rs`
//! draws directly since it needs `Pane`'s mutex-guarded `vt100::Screen` - see `README.md`
//! "Architecture").
//!
//! Layout, top to bottom: floor + announcements side by side, then units + jobs side by side,
//! then a one-line status/help footer. On a small terminal (80x24 or smaller) the floor is the
//! first thing dropped, per the design brief - announcements, units, jobs, and the status line
//! are considered more operationally essential than the floor's spatial view.

mod floor;
mod help;
mod lists;
mod log;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::model::{Board, Mode};

/// Below this terminal height (inclusive), or narrower than this width, the floor panel is
/// hidden and announcements take the freed width. 80x24 is the exact boundary the design brief
/// calls out; both dimensions are checked so a tall-but-narrow or wide-but-short terminal also
/// degrades sensibly.
const MIN_HEIGHT_FOR_FLOOR: u16 = 24;
const MIN_WIDTH_FOR_FLOOR: u16 = 80;

/// Renders just the bottom status/help line. Exposed so `main.rs` can reuse it in the zoomed
/// (single live pane, full-screen) layout, which it draws itself since it - unlike everything
/// else in this module - needs direct access to `Pane`'s `vt100::Screen`.
pub fn draw_status_line(frame: &mut Frame, area: Rect, board: &Board) {
    help::render_status_line(frame, area, board);
}

pub fn draw(frame: &mut Frame, board: &Board) {
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

    let show_floor = area.height > MIN_HEIGHT_FOR_FLOOR && area.width >= MIN_WIDTH_FOR_FLOOR;
    draw_content(frame, content_area, board, show_floor);
    help::render_status_line(frame, status_area, board);

    if matches!(
        board.mode,
        Mode::Prompt(_) | Mode::Picker(_) | Mode::ConfirmDelete(_)
    ) {
        help::render_overlay(frame, area, board);
    }
}

fn draw_content(frame: &mut Frame, area: Rect, board: &Board, show_floor: bool) {
    let top_height = if show_floor {
        (area.height * 2 / 5).clamp(6, area.height.saturating_sub(3))
    } else {
        (area.height * 3 / 10).clamp(3, area.height.saturating_sub(3))
    };
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(top_height), Constraint::Min(3)])
        .split(area);
    let (top_area, lists_area) = (outer[0], outer[1]);

    if show_floor {
        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(top_area);
        floor::render(frame, top[0], board);
        log::render(frame, top[1], board);
    } else {
        log::render(frame, top_area, board);
    }

    let lists = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(lists_area);
    lists::render_units(frame, lists[0], board);
    lists::render_jobs(frame, lists[1], board);
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
