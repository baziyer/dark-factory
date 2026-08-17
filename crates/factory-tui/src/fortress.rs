//! FORTRESS: the spatial factory overview. A custom widget that writes glyphs directly into the
//! `ratatui::buffer::Buffer` (no `Canvas`, no `Paragraph` for the map — see the design brief) —
//! one persistent "workshop" region per project, agents as glyphs at stations inside it.
//!
//! Split in two on purpose: [`compute_workshops`] is pure geometry (project/agent identity in,
//! `Rect`s and station offsets out) with no notion of *state* — it never looks at whether an
//! agent is idle or failed, only who exists and in what order, which is what makes "same input →
//! same cells" and "adding a project doesn't move existing workshops" simple, mechanical
//! properties of the packing algorithm rather than something that has to be maintained by hand.
//! [`render`] is the impure half: it walks a computed layout and asks `Board` for each agent's
//! *current* state/attention only at draw time.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use factory_core::{AgentId, AgentRole, AgentSnapshot, ProjectId, ProjectSnapshot, Provider};

use crate::model::state::AgentState;
use crate::model::{Board, RouteKind};
use crate::theme::Theme;
use crate::ui;
use factory_core::attention::Attention;

/// Width, in cells, of one station "slot": the glyph itself plus one cell reserved for a route
/// connector immediately to its left (unused — left blank — for the first station and for
/// sub-agent slots, which have no route glyph of their own).
pub const STATION_SLOT_WIDTH: u16 = 2;
pub const MIN_WORKSHOP_WIDTH: u16 = 12;
pub const MAX_WORKSHOP_WIDTH: u16 = 34;
/// Longest a project name is shown before the box's own title truncates it (middle-truncated,
/// like every other id/name in this crate — see [`ui::truncate_middle`]) rather than growing the
/// box without bound for a pathological name.
const MAX_PROJECT_NAME_DISPLAY: usize = 24;
/// Fixed box height: top border, stations row, in-tray/capacity row, bottom border. Constant
/// regardless of station count, which is what makes "positions stable across updates; no
/// animation" trivially true — a workshop never grows taller as agents are added, only wider (and
/// only up to [`MAX_WORKSHOP_WIDTH`], past which stations simply pack tighter).
pub const WORKSHOP_HEIGHT: u16 = 4;
/// How many `▒`/`░` cells the in-tray/capacity row shows before switching to a `+N` overflow
/// marker.
pub const TRAY_CAP: usize = 6;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StationSlot {
    pub agent_id: AgentId,
    pub is_orchestrator: bool,
    pub is_subagent: bool,
    pub provider: Provider,
    /// Column offset from the workshop's inner-content left edge (0-based).
    pub offset: u16,
    /// Whether this slot reserves a route-connector cell immediately to its left. Only true for
    /// a top-level (non-orchestrator, non-subagent) worker — see [`RouteKind`] for what glyph, if
    /// any, actually goes there (a render-time, state-dependent decision, not a layout one).
    pub has_route_slot: bool,
}

#[derive(Clone, Debug)]
pub struct WorkshopLayout {
    pub project_id: ProjectId,
    pub project_name: String,
    /// The full box, including its one-cell border, in buffer-relative coordinates.
    pub rect: Rect,
    pub stations: Vec<StationSlot>,
}

/// Orders one project's agents for station layout: orchestrator(s) first (by id), then each
/// top-level worker (by id) immediately followed by its own sub-agents (by id). Flat and
/// single-row by design — see the module doc for why sub-agents don't get their own row.
fn ordered_agents<'a>(agents: &[&'a AgentSnapshot]) -> Vec<&'a AgentSnapshot> {
    let mut orchestrators: Vec<&AgentSnapshot> = agents
        .iter()
        .copied()
        .filter(|a| a.role == AgentRole::Orchestrator)
        .collect();
    orchestrators.sort_by_key(|a| a.id.as_str().to_owned());

    let mut workers: Vec<&AgentSnapshot> = agents
        .iter()
        .copied()
        .filter(|a| a.role == AgentRole::Worker && a.parent_agent_id.is_none())
        .collect();
    workers.sort_by_key(|a| a.id.as_str().to_owned());

    let mut ordered = orchestrators;
    for worker in workers {
        ordered.push(worker);
        let mut children: Vec<&AgentSnapshot> = agents
            .iter()
            .copied()
            .filter(|a| a.parent_agent_id.as_ref() == Some(&worker.id))
            .collect();
        children.sort_by_key(|a| a.id.as_str().to_owned());
        ordered.extend(children);
    }
    ordered
}

/// A workshop box's width: wide enough for its packed stations (clamped to
/// [`MIN_WORKSHOP_WIDTH`]/[`MAX_WORKSHOP_WIDTH`] as before), *or* wide enough for its own name —
/// whichever needs more room (issue #68: a fixed 10-column box titled `Dark Facto` truncated a
/// project name into its own border). `display_name` is `project_name`, already middle-truncated
/// to [`MAX_PROJECT_NAME_DISPLAY`] (`render_workshop`'s title uses the exact same string, so the
/// two never disagree about what fits).
fn workshop_width(station_count: usize, display_name: &str) -> u16 {
    let inner = u16::try_from(station_count.max(1)).unwrap_or(u16::MAX) * STATION_SLOT_WIDTH;
    let stations = (inner + 2).clamp(MIN_WORKSHOP_WIDTH, MAX_WORKSHOP_WIDTH);
    // The title is rendered as " {name} " (one border column, then a leading space, the name,
    // a trailing space, then at least one clear column before the closing border - see
    // `render_workshop`'s `set_text` call, which would otherwise overwrite it).
    let name_width = u16::try_from(display_name.chars().count())
        .unwrap_or(u16::MAX)
        .saturating_add(4);
    stations.max(name_width)
}

/// Packs one workshop box per project, left-to-right in the given (already creation-ordered)
/// slice, wrapping to a new row when a box would exceed `wrap_width`. Because boxes are placed in
/// order and never revisited, appending a project at the end can only ever add a new box — every
/// earlier box's `rect` is untouched, which is the "adding a project doesn't move existing
/// workshops" property the design brief asks for.
#[must_use]
pub fn compute_workshops(
    projects_in_creation_order: &[ProjectSnapshot],
    agents: &[AgentSnapshot],
    wrap_width: u16,
) -> Vec<WorkshopLayout> {
    let wrap_width = wrap_width.max(MIN_WORKSHOP_WIDTH);
    let mut cursor_x: u16 = 0;
    let mut cursor_y: u16 = 0;
    let mut out = Vec::with_capacity(projects_in_creation_order.len());

    for project in projects_in_creation_order {
        let project_agents: Vec<&AgentSnapshot> = agents
            .iter()
            .filter(|a| a.project_id == project.id)
            .collect();
        let ordered = ordered_agents(&project_agents);
        let display_name = ui::truncate_middle(&project.name, MAX_PROJECT_NAME_DISPLAY);
        let width = workshop_width(ordered.len(), &display_name);

        if cursor_x > 0 && cursor_x.saturating_add(width) > wrap_width {
            cursor_x = 0;
            cursor_y += WORKSHOP_HEIGHT;
        }

        let rect = Rect {
            x: cursor_x,
            y: cursor_y,
            width,
            height: WORKSHOP_HEIGHT,
        };

        let mut offset: u16 = 0;
        let stations = ordered
            .into_iter()
            .map(|agent| {
                let is_orchestrator = agent.role == AgentRole::Orchestrator;
                let is_subagent = agent.parent_agent_id.is_some();
                let has_route_slot = !is_orchestrator && !is_subagent;
                let slot = StationSlot {
                    agent_id: agent.id.clone(),
                    is_orchestrator,
                    is_subagent,
                    provider: agent.provider,
                    offset,
                    has_route_slot,
                };
                offset += STATION_SLOT_WIDTH;
                slot
            })
            .collect();

        out.push(WorkshopLayout {
            project_id: project.id.clone(),
            project_name: project.name.clone(),
            rect,
            stations,
        });

        cursor_x += width;
    }

    out
}

/// Something the FORTRESS cursor can rest on: an agent's station, or an empty workshop's
/// placeholder (so a project with no agents yet is reachable by keyboard).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CursorTarget {
    Agent(AgentId),
    Workshop(ProjectId),
}

/// The keyboard-navigable cells of a layout, in layout-relative coordinates: one per station at
/// `(rect.x + 1 + offset, rect.y + 1)`, or one per empty workshop at its first inner cell.
#[must_use]
pub fn cursor_targets(workshops: &[WorkshopLayout]) -> Vec<(u16, u16, CursorTarget)> {
    let mut targets = Vec::new();
    for workshop in workshops {
        let y = workshop.rect.y + 1;
        if workshop.stations.is_empty() {
            targets.push((
                workshop.rect.x + 1,
                y,
                CursorTarget::Workshop(workshop.project_id.clone()),
            ));
        }
        for station in &workshop.stations {
            targets.push((
                workshop.rect.x + 1 + station.offset,
                y,
                CursorTarget::Agent(station.agent_id.clone()),
            ));
        }
    }
    targets
}

/// A direction the FORTRESS cursor can move (`h`/`j`/`k`/`l`, arrows).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Where the cursor goes from `from` in `direction`: on the same row, the nearest target that
/// way; up/down, the nearest row that way and, within it, the target nearest in `x`. `None` if
/// nothing lies that way. `from == None` (no cursor yet) goes to the first target.
#[must_use]
pub fn step_cursor(
    targets: &[(u16, u16, CursorTarget)],
    from: Option<&CursorTarget>,
    direction: Direction,
) -> Option<CursorTarget> {
    let (cx, cy) = match from.and_then(|from| targets.iter().find(|(_, _, t)| t == from)) {
        Some(&(x, y, _)) => (x, y),
        None => return targets.first().map(|(_, _, target)| target.clone()),
    };
    let candidates = targets.iter().filter(|(x, y, _)| match direction {
        Direction::Left => *y == cy && *x < cx,
        Direction::Right => *y == cy && *x > cx,
        Direction::Up => *y < cy,
        Direction::Down => *y > cy,
    });
    let best = match direction {
        // Nearest horizontally: largest x to the left, smallest to the right.
        Direction::Left => candidates.max_by_key(|(x, _, _)| *x),
        Direction::Right => candidates.min_by_key(|(x, _, _)| *x),
        // Nearest row first (largest y above / smallest below), then nearest column.
        Direction::Up => candidates.min_by_key(|(x, y, _)| (cy - *y, x.abs_diff(cx))),
        Direction::Down => candidates.min_by_key(|(x, y, _)| (*y - cy, x.abs_diff(cx))),
    };
    best.map(|(_, _, target)| target.clone())
}

/// Total virtual size (in cells) the last-computed layout occupies, so a caller can decide
/// whether to offer scrolling. `compute_workshops` always starts at `(0, 0)`.
#[must_use]
pub fn layout_extent(workshops: &[WorkshopLayout]) -> (u16, u16) {
    workshops.iter().fold((0, 0), |(w, h), workshop| {
        (
            w.max(workshop.rect.x + workshop.rect.width),
            h.max(workshop.rect.y + workshop.rect.height),
        )
    })
}

fn glyph_for(theme: &Theme, station: &StationSlot) -> char {
    if station.is_orchestrator {
        theme.orchestrator
    } else if station.is_subagent {
        theme.subagent
    } else {
        match station.provider {
            Provider::ClaudeCode => theme.claude_worker,
            Provider::Codex => theme.codex_worker,
            Provider::Shell => theme.shell_worker,
        }
    }
}

fn state_color(theme: &Theme, state: AgentState) -> Style {
    if !theme.use_color {
        return match state {
            AgentState::Idle => Style::default().add_modifier(Modifier::DIM),
            AgentState::Working => Style::default(),
            AgentState::Waiting => Style::default().add_modifier(Modifier::BOLD),
            AgentState::Stopped => Style::default().add_modifier(Modifier::DIM),
            AgentState::Failed => Style::default().add_modifier(Modifier::BOLD),
        };
    }
    match state {
        AgentState::Idle => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
        AgentState::Working => Style::default().fg(Color::Green),
        AgentState::Waiting => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        AgentState::Stopped => Style::default().fg(Color::Blue),
        AgentState::Failed => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

fn route_glyph(theme: &Theme, kind: RouteKind) -> Option<char> {
    match kind {
        RouteKind::None => None,
        RouteKind::Durable => Some(theme.durable_route),
        RouteKind::Transient => Some(theme.transient_route),
    }
}

fn set_cell(buf: &mut Buffer, area: Rect, x: u16, y: u16, ch: char, style: Style) {
    if x < area.x + area.width && y < area.y + area.height && x >= area.x && y >= area.y {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch).set_style(style);
        }
    }
}

fn set_text(buf: &mut Buffer, area: Rect, x: u16, y: u16, text: &str, style: Style) {
    let max_width = area.x + area.width;
    for (i, ch) in text.chars().enumerate() {
        let cx = x + u16::try_from(i).unwrap_or(u16::MAX);
        if cx >= max_width {
            break;
        }
        set_cell(buf, area, cx, y, ch, style);
    }
}

/// Shortest hint text that still reads as meaningful once truncated to a workshop's content
/// width — see the empty-workshop branch in [`render_workshop`]. Below this many columns the
/// hint is skipped entirely rather than shown as an unreadable fragment.
const MIN_EMPTY_HINT_WIDTH: u16 = 6;
const EMPTY_WORKSHOP_HINT: &str = "factoryctl agent add …";

/// Draws one workshop's border, title, station glyphs (with route connectors and attention
/// badges), and its in-tray/capacity row — writing every cell directly into `buf`. `is_selected`
/// highlights the workshop's own border (FORTRESS's `[`/`]` cursor, `Board::selected_workshop` —
/// independent of `selected`, the per-agent glyph highlight).
fn render_workshop(
    buf: &mut Buffer,
    origin: Rect,
    layout: &WorkshopLayout,
    board: &Board,
    theme: &Theme,
    selected: Option<&AgentId>,
    is_selected_workshop: bool,
) {
    let rect = Rect {
        x: origin.x + layout.rect.x,
        y: origin.y + layout.rect.y,
        width: layout.rect.width,
        height: layout.rect.height,
    };
    if rect.y + rect.height > origin.y + origin.height
        || rect.x + rect.width > origin.x + origin.width
    {
        return; // fully or partially off the visible area; nothing to draw
    }

    let border_style = if is_selected_workshop {
        let style = Style::default().add_modifier(Modifier::BOLD);
        if theme.use_color {
            style.fg(Color::Cyan)
        } else {
            style
        }
    } else {
        Style::default().fg(if theme.use_color {
            Color::DarkGray
        } else {
            Color::Reset
        })
    };
    // Borders (box-drawing even in plain theme: these are UI chrome, not part of the glyph
    // vocabulary the design brief pins to ASCII).
    for x in rect.x..rect.x + rect.width {
        set_cell(buf, rect, x, rect.y, '─', border_style);
        set_cell(buf, rect, x, rect.y + rect.height - 1, '─', border_style);
    }
    for y in rect.y..rect.y + rect.height {
        set_cell(buf, rect, rect.x, y, '│', border_style);
        set_cell(buf, rect, rect.x + rect.width - 1, y, '│', border_style);
    }
    set_cell(buf, rect, rect.x, rect.y, '┌', border_style);
    set_cell(
        buf,
        rect,
        rect.x + rect.width - 1,
        rect.y,
        '┐',
        border_style,
    );
    set_cell(
        buf,
        rect,
        rect.x,
        rect.y + rect.height - 1,
        '└',
        border_style,
    );
    set_cell(
        buf,
        rect,
        rect.x + rect.width - 1,
        rect.y + rect.height - 1,
        '┘',
        border_style,
    );

    // Same truncation `compute_workshops` already sized this box's width against, so the title
    // can never run into (let alone overwrite) the box's own right border.
    let title = format!(
        " {} ",
        ui::truncate_middle(&layout.project_name, MAX_PROJECT_NAME_DISPLAY)
    );
    set_text(buf, rect, rect.x + 1, rect.y, &title, Style::default());

    let content_y = rect.y + 1;
    let tray_y = rect.y + 2;
    let content_x0 = rect.x + 1;

    if layout.stations.is_empty() && rect.width.saturating_sub(2) >= MIN_EMPTY_HINT_WIDTH {
        // No agents yet in this workshop, and reachable only via `selected_workshop` (see
        // `Board::zoom_in`) since `selected_agent`'s cursor has nothing here to land on — point
        // at the CLI command that fixes that, truncated (`set_text` clips) to whatever fits.
        set_text(
            buf,
            rect,
            content_x0,
            content_y,
            EMPTY_WORKSHOP_HINT,
            Style::default().fg(Color::DarkGray),
        );
    }

    for station in &layout.stations {
        let x = content_x0 + station.offset;
        let agent = board.agents.get(&station.agent_id);
        let state = agent.map_or(AgentState::Idle, |agent| board.agent_state(agent).value);
        let attention = agent.map_or(Attention::Routine, |agent| {
            board.agent_attention(agent).value
        });

        if station.has_route_slot && station.offset > 0 {
            let kind = agent.map_or(RouteKind::None, |agent| board.route_kind_for(agent));
            if let Some(glyph) = route_glyph(theme, kind) {
                set_cell(buf, rect, x - 1, content_y, glyph, border_style);
            }
        }

        let glyph = glyph_for(theme, station);
        let mut style = state_color(theme, state);
        let is_selected = selected == Some(&station.agent_id);
        if is_selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        set_cell(buf, rect, x, content_y, glyph, style);

        let badge = match attention {
            Attention::NeedsInput => Some(theme.needs_input),
            Attention::Failed => Some(theme.failed),
            Attention::Completed | Attention::Routine => None,
        };
        if let Some(badge) = badge {
            let badge_style = if theme.use_color {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            set_cell(buf, rect, x + 1, content_y, badge, badge_style);
        }
    }

    let queued = board.queued_task_count(&layout.project_id);
    let capacity = board.idle_capacity_count(&layout.project_id);
    let queued_style = if theme.use_color {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let capacity_style = Style::default().add_modifier(Modifier::DIM);
    let mut tray_x = content_x0;
    let filled = queued.min(TRAY_CAP);
    for _ in 0..filled {
        set_cell(buf, rect, tray_x, tray_y, theme.queued, queued_style);
        tray_x += 1;
    }
    if queued > TRAY_CAP {
        let overflow = format!("+{}", queued - TRAY_CAP);
        set_text(buf, rect, tray_x, tray_y, &overflow, queued_style);
        tray_x += u16::try_from(overflow.chars().count()).unwrap_or(0);
    }
    for _ in 0..capacity.min(TRAY_CAP) {
        set_cell(buf, rect, tray_x, tray_y, theme.capacity, capacity_style);
        tray_x += 1;
    }
}

/// Renders every workshop in `workshops` into `area` of `buf`. `selected` highlights one agent's
/// glyph (the board's current cross-view selection cursor).
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    workshops: &[WorkshopLayout],
    board: &Board,
    theme: &Theme,
    selected: Option<&AgentId>,
    selected_workshop: Option<&ProjectId>,
) {
    for workshop in workshops {
        let is_selected_workshop = selected_workshop == Some(&workshop.project_id);
        render_workshop(
            buf,
            area,
            workshop,
            board,
            theme,
            selected,
            is_selected_workshop,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{agent, project};
    use factory_core::ProjectId;

    #[test]
    fn same_input_produces_identical_layout() {
        let projects = vec![project("a", 0), project("b", 1)];
        let agents = vec![
            agent("orch-a", "a", AgentRole::Orchestrator, None),
            agent("worker-a", "a", AgentRole::Worker, None),
            agent("orch-b", "b", AgentRole::Orchestrator, None),
        ];
        let first = compute_workshops(&projects, &agents, 200);
        let second = compute_workshops(&projects, &agents, 200);
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(a.rect, b.rect);
            assert_eq!(
                a.stations.iter().map(|s| s.offset).collect::<Vec<_>>(),
                b.stations.iter().map(|s| s.offset).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn cursor_steps_across_stations_rows_and_empty_workshops() {
        // Three projects: "a" (orchestrator + worker), "b" (empty), "c" (one orchestrator),
        // wrapped so "c" lands on a second row under "a".
        let projects = vec![project("a", 0), project("b", 1), project("c", 2)];
        let agents = vec![
            agent("orch-a", "a", AgentRole::Orchestrator, None),
            agent("worker-a", "a", AgentRole::Worker, None),
            agent("orch-c", "c", AgentRole::Orchestrator, None),
        ];
        let width_a = workshop_width(2, "a");
        let width_b = workshop_width(0, "b");
        let layout = compute_workshops(&projects, &agents, width_a + width_b);
        assert_eq!(
            layout[2].rect.y, WORKSHOP_HEIGHT,
            "c wrapped to the second row"
        );
        let targets = cursor_targets(&layout);
        let a = |id: &str| CursorTarget::Agent(AgentId::try_from(id).unwrap());
        let ws = |id: &str| CursorTarget::Workshop(ProjectId::try_from(id).unwrap());

        assert_eq!(
            step_cursor(&targets, None, Direction::Right),
            Some(a("orch-a"))
        );
        assert_eq!(
            step_cursor(&targets, Some(&a("orch-a")), Direction::Right),
            Some(a("worker-a"))
        );
        assert_eq!(
            step_cursor(&targets, Some(&a("worker-a")), Direction::Right),
            Some(ws("b")),
            "empty workshops are reachable"
        );
        assert_eq!(
            step_cursor(&targets, Some(&ws("b")), Direction::Right),
            None,
            "row end"
        );
        assert_eq!(
            step_cursor(&targets, Some(&ws("b")), Direction::Left),
            Some(a("worker-a"))
        );
        assert_eq!(
            step_cursor(&targets, Some(&a("orch-a")), Direction::Left),
            None
        );
        assert_eq!(
            step_cursor(&targets, Some(&a("orch-a")), Direction::Down),
            Some(a("orch-c"))
        );
        assert_eq!(
            step_cursor(&targets, Some(&ws("b")), Direction::Down),
            Some(a("orch-c")),
            "nearest column on the row below"
        );
        assert_eq!(
            step_cursor(&targets, Some(&a("orch-c")), Direction::Up),
            Some(a("orch-a"))
        );
        assert_eq!(
            step_cursor(&targets, Some(&a("orch-c")), Direction::Down),
            None
        );
        assert_eq!(
            step_cursor(&targets, Some(&a("ghost")), Direction::Up),
            Some(a("orch-a")),
            "an unknown cursor restarts at the first target"
        );
        assert_eq!(step_cursor(&[], None, Direction::Left), None);
    }

    #[test]
    fn adding_a_project_does_not_move_existing_workshops() {
        let projects_before = vec![project("a", 0), project("b", 1)];
        let projects_after = vec![project("a", 0), project("b", 1), project("c", 2)];
        let agents = vec![
            agent("orch-a", "a", AgentRole::Orchestrator, None),
            agent("orch-b", "b", AgentRole::Orchestrator, None),
            agent("orch-c", "c", AgentRole::Orchestrator, None),
        ];
        let before = compute_workshops(&projects_before, &agents, 200);
        let after = compute_workshops(&projects_after, &agents, 200);
        assert_eq!(before[0].rect, after[0].rect);
        assert_eq!(before[1].rect, after[1].rect);
        assert_eq!(after.len(), 3);
    }

    #[test]
    fn workshops_wrap_to_a_new_row_past_the_wrap_width() {
        let projects = vec![project("a", 0), project("b", 1), project("c", 2)];
        let agents = vec![
            agent("orch-a", "a", AgentRole::Orchestrator, None),
            agent("orch-b", "b", AgentRole::Orchestrator, None),
            agent("orch-c", "c", AgentRole::Orchestrator, None),
        ];
        // Each workshop with one station clamps to MIN_WORKSHOP_WIDTH; force wrapping after two.
        let wrap_width = MIN_WORKSHOP_WIDTH * 2;
        let layout = compute_workshops(&projects, &agents, wrap_width);
        assert_eq!(layout[0].rect.y, 0);
        assert_eq!(layout[1].rect.y, 0);
        assert_eq!(
            layout[2].rect.y, WORKSHOP_HEIGHT,
            "third workshop should wrap"
        );
        assert_eq!(layout[2].rect.x, 0);
    }

    #[test]
    fn orchestrator_is_always_the_first_station() {
        let projects = vec![project("a", 0)];
        let agents = vec![
            agent("zzz-worker", "a", AgentRole::Worker, None),
            agent("aaa-orch", "a", AgentRole::Orchestrator, None),
        ];
        let layout = compute_workshops(&projects, &agents, 200);
        assert_eq!(layout[0].stations[0].agent_id.as_str(), "aaa-orch");
        assert!(layout[0].stations[0].is_orchestrator);
    }

    #[test]
    fn subagent_immediately_follows_its_parent() {
        let projects = vec![project("a", 0)];
        let agents = vec![
            agent("orch", "a", AgentRole::Orchestrator, None),
            agent("worker", "a", AgentRole::Worker, None),
            agent("sub", "a", AgentRole::Worker, Some("worker")),
        ];
        let layout = compute_workshops(&projects, &agents, 200);
        let ids: Vec<&str> = layout[0]
            .stations
            .iter()
            .map(|s| s.agent_id.as_str())
            .collect();
        assert_eq!(ids, vec!["orch", "worker", "sub"]);
        assert!(layout[0].stations[2].is_subagent);
        assert!(!layout[0].stations[2].has_route_slot);
    }

    #[test]
    fn empty_project_still_gets_a_minimum_width_workshop() {
        let projects = vec![project("a", 0)];
        let layout = compute_workshops(&projects, &[], 200);
        assert_eq!(layout.len(), 1);
        assert_eq!(layout[0].rect.width, MIN_WORKSHOP_WIDTH);
        assert!(layout[0].stations.is_empty());
    }

    // -- render: selected-workshop border + empty-workshop hint -----------------------------

    #[test]
    fn selected_workshop_gets_a_highlighted_border() {
        let projects = vec![project("a", 0), project("b", 1)];
        let layout = compute_workshops(&projects, &[], 200);
        let board = crate::model::Board::new(false, 0, crate::theme::FORTRESS);
        let area = Rect::new(0, 0, 40, WORKSHOP_HEIGHT);
        let mut buf = Buffer::empty(area);
        let selected = ProjectId::try_from("b").unwrap();

        render(
            &mut buf,
            area,
            &layout,
            &board,
            &crate::theme::FORTRESS,
            None,
            Some(&selected),
        );

        let a_top_left = buf.cell((layout[0].rect.x, layout[0].rect.y)).unwrap();
        let b_top_left = buf.cell((layout[1].rect.x, layout[1].rect.y)).unwrap();
        assert_eq!(a_top_left.symbol(), "┌");
        assert_eq!(b_top_left.symbol(), "┌");
        assert_ne!(
            a_top_left.fg, b_top_left.fg,
            "only the selected workshop's border should be highlighted"
        );
        assert_eq!(b_top_left.fg, Color::Cyan);
    }

    #[test]
    fn empty_workshop_shows_the_agent_add_hint_when_theres_room() {
        let projects = vec![project("a", 0)];
        let layout = compute_workshops(&projects, &[], 200);
        assert!(layout[0].stations.is_empty());
        let board = crate::model::Board::new(false, 0, crate::theme::FORTRESS);
        let area = Rect::new(0, 0, 40, WORKSHOP_HEIGHT);
        let mut buf = Buffer::empty(area);

        render(
            &mut buf,
            area,
            &layout,
            &board,
            &crate::theme::FORTRESS,
            None,
            None,
        );

        let content_row: String = (layout[0].rect.x + 1..layout[0].rect.x + layout[0].rect.width)
            .map(|x| {
                buf.cell((x, layout[0].rect.y + 1))
                    .unwrap()
                    .symbol()
                    .to_owned()
            })
            .collect();
        assert!(
            content_row.trim().starts_with("factoryctl"),
            "expected a truncated CLI hint, got {content_row:?}"
        );
    }

    // -- #68: the project box sizes to its name, and never overwrites its own border -----------

    #[test]
    fn workshop_box_grows_to_fit_a_name_longer_than_the_station_driven_width() {
        let mut proj = project("dark-factory", 0);
        proj.name = "Dark Factory".to_owned();
        let agents = vec![agent("orch", "dark-factory", AgentRole::Orchestrator, None)];
        let layout = compute_workshops(&[proj], &agents, 200);
        // One station clamps the station-driven width to MIN_WORKSHOP_WIDTH (12); "Dark Factory"
        // (12 chars) needs 12+4=16 to leave its own border alone - the box must grow past the min
        // instead of truncating the name into `Dark Facto` (issue #68's exact symptom).
        assert!(
            layout[0].rect.width >= 16,
            "expected the box to grow for its name, got width {}",
            layout[0].rect.width
        );

        let board = crate::model::Board::new(false, 0, crate::theme::FORTRESS);
        let area = Rect::new(0, 0, layout[0].rect.width, WORKSHOP_HEIGHT);
        let mut buf = Buffer::empty(area);
        render(
            &mut buf,
            area,
            &layout,
            &board,
            &crate::theme::FORTRESS,
            None,
            None,
        );

        let top_right = buf
            .cell((
                layout[0].rect.x + layout[0].rect.width - 1,
                layout[0].rect.y,
            ))
            .unwrap();
        assert_eq!(
            top_right.symbol(),
            "┐",
            "the title must not overwrite the box's own border"
        );
    }

    #[test]
    fn workshop_box_width_caps_at_a_pathologically_long_project_name() {
        let mut proj = project("p", 0);
        proj.name = "a".repeat(200);
        let layout = compute_workshops(&[proj], &[], 200);
        assert!(
            layout[0].rect.width <= u16::try_from(MAX_PROJECT_NAME_DISPLAY).unwrap() + 4,
            "the name must be truncated rather than growing the box without bound, got width {}",
            layout[0].rect.width
        );
    }
}
