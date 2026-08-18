//! Rendering for BUILDING and AGENT (`Board::view`), dispatched from [`draw`], plus the shared bottom
//! status/help line and modal overlays (prompt/picker/task-menu/confirm/help), which every view
//! can show on top of itself.

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
use crate::pane::PaneMap;

pub fn draw(frame: &mut Frame, board: &Board, panes: &mut PaneMap) -> HitMap {
    let mut hits = HitMap::default();
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return hits;
    }

    let status_height = 1u16.min(area.height);
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(status_height)])
        .split(area);
    let (content_area, status_area) = (outer[0], outer[1]);

    match board.view {
        View::Building => building::draw(frame, content_area, board, &mut hits),
        View::Agent => agent::draw(frame, content_area, board, panes, &mut hits),
    }
    help::render_status_line(frame, status_area, board, &mut hits);

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

/// Truncates `text` to at most `max` characters by cutting the middle and inserting an ellipsis,
/// keeping both the head and the tail. The fix for ids/names that share a long prefix and would
/// otherwise truncate to the exact same fragment (`first-floor-worker` vs `first-floor-worker-2`,
/// issue #68) — [`truncate`] alone can't tell those apart since it only ever cuts the end.
/// Falls back to [`truncate`] when `max` is too small to keep a real head and tail either side of
/// the ellipsis.
pub(super) fn truncate_middle(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_owned();
    }
    if max <= 3 {
        return truncate(text, max);
    }
    let keep = max - 1; // one column for the ellipsis itself
    let head_len = keep / 2;
    let tail_len = keep - head_len;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

/// Pads `text` to exactly `width` columns (truncating first if it's already longer).
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

/// A bordered, titled block — every panel and overlay in `ui/` starts from this rather than
/// repeating `Block::default().borders(Borders::ALL).title(..)` at each call site. Chain
/// `.border_style(..)` on the result for a panel that needs a non-default border color (a focus
/// or attention highlight); plain panels can use the default as-is.
pub(super) fn block(title: impl Into<String>) -> Block<'static> {
    Block::default().borders(Borders::ALL).title(title.into())
}

/// Renders `block` into `area` and returns its inner content rect — the `let inner = ..;
/// frame.render_widget(block, area);` pair repeated across every panel that draws its own content
/// (as opposed to handing the block straight to a `List`/`PseudoTerminal` via `.block(..)`).
pub(super) fn bordered(frame: &mut Frame, area: Rect, block: Block<'static>) -> Rect {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Renders dim placeholder text (an empty-state hint, "attaching…", etc.) into `area`.
pub(super) fn dim(frame: &mut Frame, area: Rect, text: impl Into<String>) {
    frame.render_widget(
        Paragraph::new(text.into()).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

/// A `List` with this crate's one highlight convention (`"> "`, bold) — every list-backed panel
/// and picker (WORKSHOP's tasks/agents, the assign/orchestrator pickers, the task menu) shares it
/// rather than repeating `.highlight_symbol(..).highlight_style(..)` at each call site.
pub(super) fn styled_list<'a>(items: Vec<ListItem<'a>>, block: Block<'static>) -> List<'a> {
    List::new(items)
        .block(block)
        .highlight_symbol("> ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mouse::{Route, Target, route};
    use crate::test_fixtures::{agent, project, task};
    use factory_core::{AgentId, AgentRole, TaskId, TaskStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    fn render(board: &Board, width: u16, height: u16) -> HitMap {
        render_frame(board, width, height).0
    }

    fn render_frame(board: &Board, width: u16, height: u16) -> (HitMap, Terminal<TestBackend>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut panes = PaneMap::new();
        let mut hits = HitMap::default();
        terminal
            .draw(|frame| hits = draw(frame, board, &mut panes))
            .unwrap();
        (hits, terminal)
    }

    #[test]
    fn truncate_middle_passes_through_text_that_already_fits() {
        assert_eq!(truncate_middle("short", 10), "short");
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail_around_one_ellipsis() {
        // 10 chars -> 5: keep=4, split 2/2.
        assert_eq!(truncate_middle("abcdefghij", 5), "ab\u{2026}ij");
    }

    #[test]
    fn truncate_middle_falls_back_to_end_truncation_when_too_narrow_for_both_halves() {
        assert_eq!(truncate_middle("abcdefghij", 3), truncate("abcdefghij", 3));
    }

    #[test]
    fn truncate_middle_distinguishes_ids_that_share_a_long_prefix() {
        // The exact strings from issue #68, at the width that used to collide (WORKSHOP's old
        // fixed 12-column name field) — `truncate` (end-truncation) makes both "first-floor…";
        // `truncate_middle` must not.
        let a = truncate_middle("first-floor-worker", 12);
        let b = truncate_middle("first-floor-worker-2", 12);
        assert_ne!(a, b, "{a:?} vs {b:?} are still indistinguishable");
        // Confirms this width really was the bug: end-truncation alone collapses both to the
        // same fragment.
        assert_eq!(
            truncate("first-floor-worker", 12),
            truncate("first-floor-worker-2", 12)
        );
    }

    #[test]
    fn rendered_building_rows_and_tabs_are_the_hit_authority() {
        let mut board = Board::new(false, 0, crate::theme::PLAIN);
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![agent("alice", "proj", AgentRole::Worker, None)],
            vec![task("blocked", "proj", TaskStatus::Blocked, None, 0)],
            Vec::new(),
            Vec::new(),
        );
        let hits = render(&board, 120, 24);
        assert_eq!(hits.target_at(0, 23), Some(Target::View(View::Building)));
        assert_eq!(hits.target_at(11, 23), Some(Target::View(View::Agent)));
        assert_eq!(
            hits.target_at(1, 2),
            Some(Target::Agent(AgentId::try_from("alice").unwrap()))
        );
        assert_eq!(
            hits.target_at(83, 1),
            Some(Target::Attention(crate::model::AttentionTarget::Task(
                TaskId::try_from("blocked").unwrap()
            )))
        );
    }

    #[test]
    fn agent_task_and_unattached_pane_regions_are_clickable_but_not_terminal_input() {
        let mut board = Board::new(true, 0, crate::theme::PLAIN);
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![agent("alice", "proj", AgentRole::Worker, None)],
            vec![task("task-1", "proj", TaskStatus::Queued, Some("alice"), 0)],
            Vec::new(),
            Vec::new(),
        );
        board.view = View::Agent;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        let hits = render(&board, 120, 24);
        assert_eq!(
            hits.target_at(83, 1),
            Some(Target::Task(TaskId::try_from("task-1").unwrap()))
        );
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            route(click, &hits, None, &mut crate::mouse::Capture::default()),
            Route::Board(Target::Pane(_))
        ));
    }

    #[test]
    fn resize_replaces_clipped_and_empty_row_targets() {
        let mut board = Board::new(false, 0, crate::theme::PLAIN);
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![agent("alice", "proj", AgentRole::Worker, None)],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let full = render(&board, 120, 24);
        assert!(matches!(full.target_at(1, 2), Some(Target::Agent(_))));

        let clipped = render(&board, 30, 3);
        assert_eq!(clipped.target_at(1, 1), None);
        assert_eq!(clipped.target_at(0, 2), Some(Target::View(View::Building)));

        let empty = render(&Board::new(false, 0, crate::theme::PLAIN), 120, 24);
        assert_eq!(empty.target_at(1, 2), None);
    }

    #[test]
    fn footer_tabs_are_clickable_only_when_their_complete_labels_fit() {
        let board = Board::new(false, 0, crate::theme::PLAIN);

        let below_building = render(&board, 9, 1);
        assert_eq!(below_building.target_at(0, 0), None);
        assert_eq!(below_building.target_at(8, 0), None);

        let building = render(&board, 10, 1);
        assert_eq!(building.target_at(0, 0), Some(Target::View(View::Building)));
        assert_eq!(building.target_at(9, 0), Some(Target::View(View::Building)));

        let below_agent = render(&board, 17, 1);
        assert_eq!(
            below_agent.target_at(9, 0),
            Some(Target::View(View::Building))
        );
        assert_eq!(below_agent.target_at(11, 0), None);
        assert_eq!(below_agent.target_at(16, 0), None);
        assert_eq!(
            route(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 11,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                },
                &below_agent,
                None,
                &mut crate::mouse::Capture::default(),
            ),
            Route::None
        );

        let complete = render(&board, 18, 1);
        assert_eq!(complete.target_at(11, 0), Some(Target::View(View::Agent)));
        assert_eq!(complete.target_at(17, 0), Some(Target::View(View::Agent)));

        // Every redraw returns a fresh hit map, so shrinking below the threshold cannot retain
        // the complete frame's AGENT target.
        let resized = render(&board, 12, 1);
        assert_eq!(resized.target_at(11, 0), None);
    }

    #[test]
    fn footer_has_selected_tabs_and_only_stable_essential_controls() {
        let board = Board::new(false, 0, crate::theme::PLAIN);
        let (hits, terminal) = render_frame(&board, 120, 24);
        let footer_y = 23;
        let footer = (0..120)
            .map(|x| terminal.backend().buffer()[(x, footer_y)].symbol())
            .collect::<String>();

        assert!(footer.contains("[BUILDING] [AGENT]"));
        assert!(footer.contains("BOARD"));
        assert!(footer.contains("[? help]"));
        assert!(footer.contains("[q detach]"));
        for old_action in ["j/k", "Enter", "needs-you", "type"] {
            assert!(!footer.contains(old_action));
        }
        let selected = terminal.backend().buffer()[(0, footer_y)].modifier;
        let unselected = terminal.backend().buffer()[(11, footer_y)].modifier;
        assert!(selected.contains(Modifier::REVERSED | Modifier::BOLD));
        assert!(!unselected.contains(Modifier::REVERSED));

        let help_x = (0..120)
            .find(|x| hits.target_at(*x, footer_y) == Some(Target::Help))
            .unwrap();
        let detach_x = (0..120)
            .find(|x| hits.target_at(*x, footer_y) == Some(Target::Detach))
            .unwrap();
        for (column, expected) in [(help_x, Target::Help), (detach_x, Target::Detach)] {
            let click = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row: footer_y,
                modifiers: KeyModifiers::NONE,
            };
            assert_eq!(
                route(click, &hits, None, &mut crate::mouse::Capture::default()),
                Route::Board(expected)
            );
        }
    }

    #[test]
    fn footer_shows_terminal_escape_only_while_typing() {
        let board = Board::new(false, 0, crate::theme::PLAIN);
        let (_, terminal) = render_frame(&board, 120, 24);
        let footer_y = 23;
        let board_footer = (0..120)
            .map(|x| terminal.backend().buffer()[(x, footer_y)].symbol())
            .collect::<String>();
        assert!(!board_footer.contains("Ctrl-]"));

        let mut typing = board;
        typing.view = View::Agent;
        typing.selected_agent = Some(AgentId::try_from("alice").unwrap());
        typing.pane_mode = crate::model::PaneMode::Typing;
        typing.pane_ready = true;
        let (_, terminal) = render_frame(&typing, 120, 24);
        let typing_footer = (0..120)
            .map(|x| terminal.backend().buffer()[(x, footer_y)].symbol())
            .collect::<String>();
        assert!(typing_footer.contains("Ctrl-] board"));
        assert!(typing_footer.contains("[? help] [q detach]"));
    }
}
