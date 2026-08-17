//! FORTRESS: the fleet-wide floor plan (left) plus the attention-ranked announcements log
//! (right). Below `MIN_HEIGHT_FOR_FLOOR`x`MIN_WIDTH_FOR_FLOOR` (80x24, the design brief's exact
//! boundary) the floor is dropped first — announcements gets the freed width instead.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;

use crate::fortress;
use crate::model::Board;
use crate::ui::{self, announcements};

const MIN_HEIGHT_FOR_FLOOR: u16 = 24;
const MIN_WIDTH_FOR_FLOOR: u16 = 80;

pub fn draw(frame: &mut Frame, area: Rect, board: &Board) {
    let show_floor = area.height > MIN_HEIGHT_FOR_FLOOR && area.width >= MIN_WIDTH_FOR_FLOOR;
    if !show_floor {
        announcements::render(frame, area, board);
        return;
    }

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);
    let (floor_area, log_area) = (columns[0], columns[1]);

    render_floor(frame, floor_area, board);
    announcements::render(frame, log_area, board);
}

fn render_floor(frame: &mut Frame, area: Rect, board: &Board) {
    let project_count = board.projects.len();
    let title = format!(
        " fortress — {project_count} project{} ",
        if project_count == 1 { "" } else { "s" }
    );
    let inner = ui::bordered(frame, area, ui::block(title));
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if board.projects.is_empty() {
        let message = if board.connection == crate::model::Connection::Live {
            "no projects on this daemon — create one with factoryctl project add"
        } else {
            "connecting…"
        };
        ui::dim(frame, inner, message);
        return;
    }

    let projects = board.projects_sorted();
    let agents = board.agents_vec();
    board.fortress_width.set(inner.width);
    let workshops = fortress::compute_workshops(&projects, &agents, inner.width);
    fortress::render(
        frame.buffer_mut(),
        inner,
        &workshops,
        board,
        &board.theme,
        board.selected_agent.as_ref(),
        board.selected_workshop.as_ref(),
    );

    let (_, needed_height) = fortress::layout_extent(&workshops);
    if needed_height > inner.height {
        let hint_area = Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(" more below — resize taller or zoom into a project (Enter) ")
                .style(Style::default().fg(Color::DarkGray).bg(Color::Black)),
            hint_area,
        );
    }
}
