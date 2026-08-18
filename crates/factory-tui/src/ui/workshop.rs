//! WORKSHOP: one project's tasks (queue order), its agent hierarchy (orchestrator → workers →
//! sub-agents, indented), and a detail pane for whichever item is selected. `Tab` toggles which
//! list has the cursor; `!` filters both lists to attention-worthy rows only.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, ListState, Paragraph, Wrap};

use factory_core::{AgentRole, ProjectId, TaskStatus};

use crate::model::state::{self, AgentState};
use crate::model::{Board, WorkshopPane, provider_letter};
use crate::ui::{self, pad, truncate, truncate_middle};
use factory_core::attention::Attention;

/// Same width as the pre-Track-6c board's unit-list sparkline — see `model::state::ActivitySeries`.
const SPARKLINE_WIDTH: usize = 8;
/// The agent-name field's floor even in a very narrow pane — the pre-#68 fixed width, kept as a
/// minimum now that the field otherwise gets whatever's left of the pane's width (see
/// `agent_name_width`).
const MIN_AGENT_NAME_WIDTH: usize = 12;

fn agent_state_style(state: AgentState) -> Style {
    match state {
        AgentState::Idle => Style::default().fg(Color::DarkGray),
        AgentState::Working => Style::default().fg(Color::Green),
        AgentState::Waiting => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        AgentState::Stopped => Style::default().fg(Color::Blue),
        AgentState::Failed => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

/// `theme.complete` is the only status glyph that varies by theme (`--theme plain`'s whole point
/// is no non-ASCII glyph escapes it — see `theme.rs`'s `glyph_tables_are_complete_and_ascii_for_
/// plain`); the rest are already plain ASCII in both themes, so they stay literal here.
fn task_glyph(status: TaskStatus, theme: &crate::theme::Theme) -> char {
    match status {
        TaskStatus::Queued => '#',
        // Not '>': the list highlight symbol (`ui::styled_list`) is itself "> ", so a selected
        // Running task rendered as "> >" — a literal double marker (issue #69).
        TaskStatus::Running => '*',
        TaskStatus::Blocked => '?',
        TaskStatus::Succeeded => theme.complete,
        TaskStatus::Failed => '!',
        TaskStatus::Cancelled => 'x',
    }
}

fn task_color(status: TaskStatus) -> Color {
    match status {
        TaskStatus::Queued => Color::Gray,
        TaskStatus::Running => Color::Green,
        TaskStatus::Blocked => Color::Yellow,
        TaskStatus::Succeeded => Color::Blue,
        TaskStatus::Failed => Color::Red,
        TaskStatus::Cancelled => Color::DarkGray,
    }
}

pub fn draw(frame: &mut Frame, area: Rect, board: &Board) {
    let Some(project_id) = board.focused_project.clone() else {
        render_no_project(frame, area);
        return;
    };
    let project_name = board
        .projects
        .iter()
        .find(|p| p.id == project_id)
        .map_or_else(|| project_id.to_string(), |p| p.name.clone());

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);
    let (top, detail_area) = (rows[0], rows[1]);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(top);

    render_tasks(frame, cols[0], board, &project_id, &project_name);
    render_agents(frame, cols[1], board, &project_id);
    render_detail(frame, detail_area, board, &project_id);
}

fn render_no_project(frame: &mut Frame, area: Rect) {
    let inner = ui::bordered(frame, area, ui::block(" workshop "));
    ui::dim(
        frame,
        inner,
        "no project focused — press Enter on an agent in FORTRESS to zoom in",
    );
}

fn render_tasks(
    frame: &mut Frame,
    area: Rect,
    board: &Board,
    project_id: &ProjectId,
    project_name: &str,
) {
    let tasks = board.visible_tasks(project_id);
    let mut items: Vec<ListItem> = tasks
        .iter()
        .map(|task| {
            let status = task.snapshot.status;
            let assignee = task
                .snapshot
                .assigned_agent_id
                .as_ref()
                .map_or_else(|| "-".to_owned(), |id| id.as_str().to_owned());
            let line = Line::from(vec![
                Span::styled(
                    format!("{} ", task_glyph(status, &board.theme)),
                    Style::default().fg(task_color(status)),
                ),
                Span::raw(pad(
                    &format!("#{}", truncate_middle(task.snapshot.id.as_str(), 9)),
                    11,
                )),
                // A literal separator space, not folded into `pad`'s width: a title exactly at
                // (or past) the pad width would otherwise glue directly onto the arrow that
                // follows (`pad` only pads when it has room to). Titles stay end-truncated
                // (`pad`'s own behavior) — issue #68 only asks for middle-truncation on ids/names.
                Span::raw(pad(&task.snapshot.title, 18)),
                Span::raw(" "),
                Span::styled(
                    format!("\u{2192} {assignee}"),
                    Style::default().fg(Color::Gray),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();
    if items.is_empty() {
        let hint = if board.attention_filter && !board.tasks_in(project_id).is_empty() {
            "no tasks need attention — press ! to clear the filter"
        } else {
            "no tasks yet — press n to add one"
        };
        items.push(ListItem::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let focused = board.workshop_focus == WorkshopPane::Tasks;
    let filter_badge = if board.attention_filter { " [!]" } else { "" };
    let title = format!(" {project_name} — tasks{filter_badge} ");
    let block = ui::block(title).border_style(panel_style(focused));
    let list = ui::styled_list(items, block);

    let mut state = ListState::default();
    if !tasks.is_empty() {
        let cursor = board
            .selected_task
            .as_ref()
            .and_then(|id| tasks.iter().position(|t| &t.snapshot.id == id))
            .unwrap_or(0);
        state.select(Some(cursor));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn wait_or_activity_text(
    board: &Board,
    agent: &factory_core::AgentSnapshot,
) -> Option<(String, bool)> {
    if let Some(session) = board.session_for(agent) {
        if let Some(reason) = &session.wait_reason {
            return Some((reason.clone(), false));
        }
        return None;
    }
    let run = board.latest_run_for(&agent.id)?;
    run.wait_reason
        .clone()
        .or_else(|| run.activity.clone())
        .map(|text| (text, true))
}

/// How many columns the agent-name field gets in one agents-pane row: whatever's left of `area`'s
/// width after the border, indent, state glyph, state label, and sparkline columns — not a width
/// hardcoded ahead of them. Issue #68: two agents sharing a long prefix (`first-floor-worker` /
/// `first-floor-worker-2`) both truncated to the same fragment at the old fixed 12 columns even
/// though the pane had 73 columns to spare; giving the name whatever's actually left fixes that
/// directly, and `truncate_middle` (rather than end-truncation) covers whatever's left over.
fn agent_name_width(area_width: u16, depth: u8) -> usize {
    let indent = usize::from(depth) * 2;
    let border = 2; // ui::block's left + right border
    let glyph = 2; // the state glyph + its separator space
    let state_label = 10; // pad(state label, 9) + its separator space
    let spark = SPARKLINE_WIDTH + 1; // the sparkline + its separator space
    let overhead = indent + border + glyph + state_label + spark;
    usize::from(area_width)
        .saturating_sub(overhead)
        .max(MIN_AGENT_NAME_WIDTH)
}

fn render_agents(frame: &mut Frame, area: Rect, board: &Board, project_id: &ProjectId) {
    let tree = board.visible_agent_tree(project_id);
    let mut items: Vec<ListItem> = tree
        .iter()
        .map(|(agent_id, depth)| {
            let indent = "  ".repeat(usize::from(*depth));
            let Some(agent) = board.agents.get(agent_id) else {
                return ListItem::new(Line::from(format!("{indent}{agent_id}")));
            };
            let rated_state = board.agent_state(agent);
            let glyph = if agent.role == AgentRole::Orchestrator {
                board.theme.orchestrator
            } else if agent.parent_agent_id.is_some() {
                board.theme.subagent
            } else {
                provider_letter(agent.provider)
            };
            let counts = board
                .activity
                .get(agent_id)
                .map(state::ActivitySeries::counts)
                .unwrap_or_default();
            let spark = state::braille_sparkline(&counts, SPARKLINE_WIDTH);
            let name_width = agent_name_width(area.width, *depth);
            let mut spans = vec![
                Span::raw(indent),
                Span::styled(format!("{glyph} "), agent_state_style(rated_state.value)),
                // Middle-truncated and padded to whatever's left of the pane's width (see
                // `agent_name_width`) — a 12-char-or-longer agent id must not glue onto the state
                // label that follows.
                Span::raw(pad(
                    &truncate_middle(agent_id.as_str(), name_width),
                    name_width,
                )),
                Span::raw(" "),
                Span::styled(
                    pad(rated_state.value.label(), 9),
                    agent_state_style(rated_state.value),
                ),
                Span::styled(format!("{spark} "), Style::default().fg(Color::Cyan)),
            ];
            if let Some((text, inferred)) = wait_or_activity_text(board, agent) {
                let prefix = if inferred { "~" } else { "" };
                spans.push(Span::styled(
                    format!("{prefix}{}", truncate(&text, 24)),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    if items.is_empty() {
        let hint = if board.attention_filter && !board.agent_tree(project_id).is_empty() {
            "no agents need attention — press ! to clear the filter"
        } else {
            "no agents in this project yet — create one with factoryctl agent add"
        };
        items.push(ListItem::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let focused = board.workshop_focus == WorkshopPane::Agents;
    let filter_badge = if board.attention_filter { " [!]" } else { "" };
    let block = ui::block(format!(" agents{filter_badge} ")).border_style(panel_style(focused));
    let list = ui::styled_list(items, block);

    let mut state = ListState::default();
    if let Some(agent) = agent_at_cursor(board, project_id) {
        let cursor = tree
            .iter()
            .position(|(row_id, _)| row_id == &agent.id)
            .expect("agent_at_cursor returns an agent from the visible tree");
        state.select(Some(cursor));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn panel_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn render_detail(frame: &mut Frame, area: Rect, board: &Board, project_id: &ProjectId) {
    let inner = ui::bordered(frame, area, ui::block(" detail "));
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let lines = match board.workshop_focus {
        WorkshopPane::Tasks => task_detail_lines(board, project_id),
        WorkshopPane::Agents => agent_detail_lines(board, project_id),
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// The task WORKSHOP's cursor is actually resting on: `board.selected_task` if it's one of
/// `project_id`'s *visible* tasks (honoring `attention_filter`, same as `render_tasks`'s list),
/// otherwise the first one — exactly the row `render_tasks` highlights via its own
/// `.unwrap_or(0)` fallback. One notion of selection, so the detail pane can never show a
/// different task than the one the cursor marker is on (issue #69): before any movement key,
/// `board.selected_task` is still `None`, but the cursor visibly rests on the first row, so detail
/// must too.
fn task_at_cursor<'a>(
    board: &'a Board,
    project_id: &ProjectId,
) -> Option<&'a factory_core::TaskDetail> {
    let visible = board.visible_tasks(project_id);
    board
        .selected_task
        .as_ref()
        .and_then(|id| visible.iter().find(|task| &task.snapshot.id == id))
        .or_else(|| visible.first())
        .copied()
}

/// The agent WORKSHOP's cursor is actually resting on, using the same visible tree as the list.
/// This mirrors [`task_at_cursor`]: before a movement key sets `selected_agent`, the list already
/// highlights its first row, so the detail pane and Enter target must treat that row as selected.
fn agent_at_cursor<'a>(
    board: &'a Board,
    project_id: &ProjectId,
) -> Option<&'a factory_core::AgentSnapshot> {
    let visible = board.visible_agent_tree(project_id);
    let agent_id = board
        .selected_agent
        .as_ref()
        .filter(|id| visible.iter().any(|(row_id, _)| row_id == *id))
        .or_else(|| visible.first().map(|(id, _)| id))?;
    board.agents.get(agent_id)
}

fn task_detail_lines(board: &Board, project_id: &ProjectId) -> Vec<Line<'static>> {
    let Some(task) = task_at_cursor(board, project_id) else {
        return vec![Line::from("(no task selected)")];
    };
    let attention = factory_core::attention::task_attention(task.snapshot.status);
    let mut lines = vec![
        Line::from(Span::styled(
            task.snapshot.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "status: {:?}   assignee: {}",
            task.snapshot.status,
            task.snapshot
                .assigned_agent_id
                .as_ref()
                .map_or_else(|| "-".to_owned(), |id| id.to_string())
        )),
        Line::from(""),
    ];
    if task.body.is_empty() {
        if board.is_task_detail_pending(&task.snapshot.id) {
            lines.push(Line::from(Span::styled(
                "(loading…)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            lines.push(Line::from("(no body)"));
        }
    } else {
        lines.push(Line::from(task.body.clone()));
    }
    if let Some(result) = &task.result {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "result:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(result.clone()));
    }
    if attention == Attention::NeedsInput {
        lines.push(Line::from(""));
        let reason = task.blocked_reason.as_deref().map_or_else(
            || {
                if board.is_task_detail_pending(&task.snapshot.id) {
                    "blocked — (loading reason…)".to_owned()
                } else {
                    "blocked — no reason given".to_owned()
                }
            },
            |reason| format!("blocked — {reason}"),
        );
        lines.push(Line::from(Span::styled(
            reason,
            Style::default().fg(Color::Yellow),
        )));
    }
    lines
}

/// Coarse "how long ago" text for the detail pane's ended-session line — whichever of
/// seconds/minutes/hours/days is coarsest without rounding to zero.
fn ago(now_ms: i64, at_ms: i64) -> String {
    let secs = (now_ms - at_ms).max(0) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// The `~latest run`/`~activity` lines shared by both "no live session" branches of
/// `agent_detail_lines` — what the agent's current-state inference (the `~` badge shown
/// everywhere else on the board) is actually based on, per issue #71.
fn push_run_history_lines(
    lines: &mut Vec<Line<'static>>,
    board: &Board,
    agent_id: &factory_core::AgentId,
) {
    if let Some(run) = board.latest_run_for(agent_id) {
        lines.push(Line::from(format!("~latest run: {:?}", run.status)));
        if let Some(activity) = &run.activity {
            lines.push(Line::from(format!("~activity: {activity}")));
        }
    }
}

fn agent_detail_lines(board: &Board, project_id: &ProjectId) -> Vec<Line<'static>> {
    let Some(agent) = agent_at_cursor(board, project_id) else {
        return vec![Line::from("(no agent selected)")];
    };
    let mut lines = vec![
        Line::from(Span::styled(
            agent.id.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "role: {:?}   provider: {:?}",
            agent.role, agent.provider
        )),
    ];
    if let Some(worktree) = &agent.worktree {
        lines.push(Line::from(format!("worktree: {worktree}")));
    }
    if let Some(worktree) = board.worktree_for(&agent.id) {
        let state = if let Some(error) = &worktree.error {
            format!("git state unavailable: {error}")
        } else if worktree.dirty {
            format!("git: DIRTY ({} changed)", worktree.changed_files)
        } else {
            "git: clean".to_owned()
        };
        lines.push(Line::from(state));
    }
    if let Some(session) = board.session_for(agent) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "session (observed via hooks):",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(format!("state: {:?}", session.state)));
        if let Some(event) = session.last_hook_event {
            lines.push(Line::from(format!("last hook: {event:?}")));
        }
        if let Some(reason) = &session.wait_reason {
            lines.push(Line::from(format!("wait reason: {reason}")));
        }
        lines.push(Line::from(format!("worktree: {}", session.worktree)));
    } else if let Some(session) = board.latest_session_for(&agent.id) {
        // `Board::session_for` only resolves a *live* session (the daemon clears an agent's
        // `current_session_id` once its session ends), so an agent with three stopped sessions
        // lands here, not in the branch above — never claim "no session yet" once one has
        // actually existed (issue #71); show what it last was and when.
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "no live session — last session {:?} {}",
                session.state,
                ago(
                    board.now_ms,
                    session.ended_at_ms.unwrap_or(session.state_since_ms)
                ),
            ),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(Span::styled(
            "current state inferred from run history:",
            Style::default().fg(Color::DarkGray),
        )));
        push_run_history_lines(&mut lines, board, &agent.id);
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "~no session yet — state inferred from run history",
            Style::default().fg(Color::DarkGray),
        )));
        push_run_history_lines(&mut lines, board, &agent.id);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory_core::{
        AgentId, ObserverHealth, Provider, SessionId, SessionSnapshot, SessionState, TaskId,
    };

    fn workshop_board() -> (Board, ProjectId) {
        let mut board = Board::new(false, 0, crate::theme::FORTRESS);
        board.apply_fleet_snapshot(
            vec![crate::test_fixtures::project("proj", 0)],
            Vec::new(),
            vec![
                crate::test_fixtures::task("t1", "proj", TaskStatus::Running, None, 0),
                crate::test_fixtures::task("t2", "proj", TaskStatus::Queued, None, 10),
            ],
            Vec::new(),
            Vec::new(),
        );
        let project_id = ProjectId::try_from("proj").unwrap();
        (board, project_id)
    }

    fn ended_session(
        id: &str,
        agent_id: &str,
        project: &str,
        state: SessionState,
        ended_at_ms: i64,
    ) -> SessionSnapshot {
        SessionSnapshot {
            id: SessionId::try_from(id).unwrap(),
            project_id: ProjectId::try_from(project).unwrap(),
            agent_id: AgentId::try_from(agent_id).unwrap(),
            provider: Provider::ClaudeCode,
            state,
            state_since_ms: ended_at_ms,
            worktree: "/work".into(),
            provider_session_id: None,
            current_run_id: None,
            activity: None,
            activity_inferred: false,
            last_hook_event: None,
            last_hook_at_ms: None,
            wait_reason: None,
            observer_health: ObserverHealth::Unknown,
            observer_health_since_ms: 0,
            started_at_ms: 0,
            updated_at_ms: ended_at_ms,
            ended_at_ms: Some(ended_at_ms),
            exit_code: None,
            exit_signal: None,
        }
    }

    // -- #69: WORKSHOP's cursor and its selection agree ----------------------------------------

    #[test]
    fn task_detail_shows_the_first_task_before_any_movement_key() {
        let (board, project_id) = workshop_board();
        assert_eq!(board.selected_task, None, "no movement key pressed yet");
        let lines = task_detail_lines(&board, &project_id);
        assert_eq!(
            lines[0].to_string(),
            "t1",
            "detail must show what the cursor visibly rests on (row 0), not '(no task selected)'"
        );
    }

    #[test]
    fn task_detail_follows_the_cursor_once_it_moves() {
        let (mut board, project_id) = workshop_board();
        board.selected_task = Some(TaskId::try_from("t2").unwrap());
        let lines = task_detail_lines(&board, &project_id);
        assert_eq!(lines[0].to_string(), "t2");
    }

    #[test]
    fn agent_detail_shows_the_first_visible_agent_before_any_movement_key() {
        let mut board = Board::new(false, 0, crate::theme::FORTRESS);
        let project_id = ProjectId::try_from("proj").unwrap();
        board.apply_fleet_snapshot(
            vec![crate::test_fixtures::project("proj", 0)],
            vec![crate::test_fixtures::agent(
                "alice",
                "proj",
                AgentRole::Worker,
                None,
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(board.selected_agent, None, "no movement key pressed yet");
        let lines = agent_detail_lines(&board, &project_id);
        assert_eq!(
            lines[0].to_string(),
            "alice",
            "detail must show the first row highlighted by the agents list"
        );
    }

    // -- #68: names get the width, truncated from the middle -----------------------------------

    #[test]
    fn agent_name_width_gives_the_field_whatever_is_left_of_the_real_workshop_pane_width() {
        // The WORKSHOP agents-pane width from issue #68's own dogfood screen dump.
        let width = agent_name_width(73, 0);
        assert!(
            width >= "first-floor-worker-2".chars().count(),
            "got {width}, both ids must fit without colliding"
        );
    }

    // -- #71: detail pane wording for an agent with only ended sessions ------------------------

    #[test]
    fn agent_detail_says_no_live_session_once_a_session_has_ended() {
        let mut board = Board::new(false, 5_000, crate::theme::FORTRESS);
        board.apply_fleet_snapshot(
            vec![crate::test_fixtures::project("proj", 0)],
            vec![crate::test_fixtures::agent(
                "alice",
                "proj",
                AgentRole::Worker,
                None,
            )],
            Vec::new(),
            Vec::new(),
            vec![ended_session(
                "s1",
                "alice",
                "proj",
                SessionState::Stopped,
                4_000,
            )],
        );
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        let text = agent_detail_lines(&board, &ProjectId::try_from("proj").unwrap())
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("no live session — last session Stopped"),
            "{text}"
        );
        assert!(
            !text.contains("no session yet"),
            "must never say 'no session yet' once one existed: {text}"
        );
    }

    #[test]
    fn agent_detail_says_no_session_yet_when_none_ever_existed() {
        let mut board = Board::new(false, 0, crate::theme::FORTRESS);
        board.apply_fleet_snapshot(
            vec![crate::test_fixtures::project("proj", 0)],
            vec![crate::test_fixtures::agent(
                "alice",
                "proj",
                AgentRole::Worker,
                None,
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        let text = agent_detail_lines(&board, &ProjectId::try_from("proj").unwrap())
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("no session yet"), "{text}");
    }
}
