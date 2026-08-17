//! FOCUS: one pane, full-screen. Reuses `tui_term::widget::PseudoTerminal` for rendering (see
//! `pane.rs`'s scrollback methods for why this counts as the design brief's "small custom render
//! path over `vt100::Screen`'s scrollback" without hand-rolling a cell renderer: `vt100` already
//! renders the scrolled-back view correctly once `Parser::set_scrollback` is set, so the "custom"
//! part is entirely the operator-facing scroll-offset plumbing — `PgUp`/`PgDn` here, `pane.rs`'s
//! `scroll_up`/`scroll_down`/`scroll_offset` there — not a reimplementation of the widget).

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use tui_term::widget::PseudoTerminal;

use crate::model::Board;
use crate::pane::PaneMap;
use crate::ui;

pub fn draw(frame: &mut Frame, area: Rect, board: &Board, panes: &mut PaneMap) {
    let Some(session_id) = board.focus_target() else {
        render_placeholder(frame, area, board, "no agent selected");
        return;
    };
    let Some(pane) = panes.get_mut(&session_id) else {
        render_placeholder(frame, area, board, "attaching…");
        return;
    };

    let exited = pane.has_exited();
    let marker = if exited { " [exited]" } else { "" };
    let command = match pane.kind {
        crate::pane::PaneKind::LocalPty => format!(" — {}", pane.command.join(" ")),
        crate::pane::PaneKind::Daemon => String::new(),
    };
    let scroll_offset = pane.scroll_offset();
    let mode_hint = if !board.pane_forwarding {
        "CONTROL — Ctrl-] back to pane"
    } else if scroll_offset > 0 {
        "PANE — PgDn/scroll to return to live"
    } else {
        "PANE — Ctrl-] for board control"
    };
    let scroll_hint = if scroll_offset > 0 {
        format!("  [scrolled back {scroll_offset}]")
    } else {
        String::new()
    };
    let color = if board.pane_forwarding {
        Color::Cyan
    } else {
        Color::Yellow
    };
    let title = format!(
        " {}{command}{marker} — [{mode_hint}]{scroll_hint} ",
        pane.title
    );
    let block = ui::block(title).border_style(Style::default().fg(color));
    let inner = block.inner(area);
    if inner.width > 0 && inner.height > 0 {
        let _ = pane.resize(inner.height, inner.width);
    }

    if let Some(error) = pane.attach_error() {
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                error,
                Style::default().fg(Color::Red),
            ))),
            inner,
        );
        return;
    }

    let parser = pane.lock_parser();
    let screen = parser.screen();
    let pseudo = PseudoTerminal::new(screen).block(block);
    frame.render_widget(pseudo, area);

    // Suppress the hardware cursor while scrolled back — showing the *live* cursor position
    // among historical content would be actively misleading (see this module's doc comment).
    if board.pane_forwarding && scroll_offset == 0 && !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        let x = inner.x.saturating_add(col);
        let y = inner.y.saturating_add(row);
        if x < inner.x + inner.width && y < inner.y + inner.height {
            frame.set_cursor_position(Position { x, y });
        }
    }
}

fn render_placeholder(frame: &mut Frame, area: Rect, board: &Board, message: &str) {
    let inner = ui::bordered(frame, area, ui::block(" focus "));
    let text = if board.focused_project.is_none() {
        "no project focused — press Enter on an agent in FORTRESS first"
    } else {
        message
    };
    ui::dim(frame, inner, text);
}
