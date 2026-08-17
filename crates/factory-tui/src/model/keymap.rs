//! `View`, `Action`, one `keymap()` function, and all of `Board`'s key-handling — the
//! repomon-style navigation layer described in `REFS-HERDR-REPOMON.md`: "One `View` enum and one
//! `Action` enum drive every level uniformly ... same keys at every depth". `keymap()` is
//! deliberately context-free (it never looks at `self.view`): the *meaning* of an `Action` is
//! decided by `Board::dispatch`, not by which keys are reachable, so e.g. `Enter` always means
//! "zoom in *something*" and every view gets to decide what that something is.
//!
//! Modal states that need raw text entry (`Prompt`) or a scrollable list of their own (`Picker`,
//! `TaskMenu`, `Confirm`, `Help`) sit outside the `Action` enum entirely and get their own small
//! raw-`KeyEvent` handlers below, same as the pre-Track-6c board's prompt/picker handling.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use factory_core::local::LocalRequest;
use factory_core::{AgentId, AgentRole, ProjectId, RunId, SessionId, TaskId};

use super::{Board, StatusLevel};
use crate::fortress;

// ---------------------------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------------------------

/// The board's four zoom levels, per the design brief's "Ratatui UX — four views". `1`-`4` jump
/// straight to any of them; `Enter`/`Esc` (via [`Board::dispatch`]'s `ZoomIn`/`ZoomOut`) step one
/// level at a time along the same ladder: Fortress → Workshop → Terminals → Focus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum View {
    Fortress,
    Workshop,
    Terminals,
    Focus,
}

impl View {
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::Fortress => 1,
            Self::Workshop => 2,
            Self::Terminals => 3,
            Self::Focus => 4,
        }
    }

    #[must_use]
    pub const fn from_number(n: u8) -> Option<Self> {
        match n {
            1 => Some(Self::Fortress),
            2 => Some(Self::Workshop),
            3 => Some(Self::Terminals),
            4 => Some(Self::Focus),
            _ => None,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fortress => "FORTRESS",
            Self::Workshop => "WORKSHOP",
            Self::Terminals => "TERMINALS",
            Self::Focus => "FOCUS",
        }
    }
}

/// WORKSHOP's `Tab`-toggled pane focus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkshopPane {
    Tasks,
    Agents,
}

/// TERMINALS/FOCUS's input mode — see [`Board::pane_mode`]'s doc comment for the per-view
/// defaults and how to switch between them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneMode {
    /// Keys are board `Action`s: Tab/j/k/arrows move focus between panes, Enter zooms in.
    Board,
    /// Keys go to the focused pane. Only ever *actually* forwarded when a live pane exists — see
    /// `Board::has_live_pane`.
    Typing,
}

// ---------------------------------------------------------------------------------------------
// Action + keymap
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    SwitchView(View),
    /// `Enter`.
    ZoomIn,
    /// `Esc`.
    ZoomOut,
    /// `→`/`l`: FORTRESS moves the cursor right; every other view zooms in.
    Right,
    /// `←`/`h`: FORTRESS moves the cursor left; every other view zooms out.
    Left,
    /// `Tab`: cycles agents (FORTRESS/TERMINALS/FOCUS) or panes (WORKSHOP).
    Tab,
    /// `j`/`↓`: FORTRESS moves the cursor down a row; elsewhere, next item.
    MoveNext,
    /// `k`/`↑`: FORTRESS moves the cursor up a row; elsewhere, previous item.
    MovePrev,
    /// `]`: FORTRESS-only, cycles `selected_workshop` forward. See its field doc comment.
    NextWorkshop,
    /// `[`: FORTRESS-only, cycles `selected_workshop` backward.
    PrevWorkshop,
    NewTask,
    MessageAgent,
    MessageOrchestrator,
    /// `p`: pick the focused project from a list (remembered across runs, see `main.rs`).
    SwitchProject,
    /// `x`: stop the selected agent (2-press confirm).
    StopSelected,
    Detach,
    JumpNeedsAttention,
    JumpAndFocusNeedsAttention,
    ToggleHelp,
    /// `!`: WORKSHOP-only, but harmless (a no-op on rendering) everywhere else.
    ToggleAttentionFilter,
    /// `PgUp`/`PgDn`: FOCUS-only scrollback; a no-op elsewhere.
    ScrollUp,
    ScrollDown,
}

/// The one keymap: every key the board recognizes in its base navigation mode, independent of
/// which view is active (`Board::dispatch` supplies the context). Returns `None` for anything not
/// bound — callers fall through to whatever else might want the key (a live pane, while
/// `pane_mode` is `Typing`, is handled a level up in `Board::handle_normal_key`).
#[must_use]
pub fn keymap(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char(digit @ '1'..='4') => {
            // SAFETY-free but worth spelling out: `'1'..='4' as u8 - b'0'` is exactly
            // `View::from_number`'s domain (1-4) — see `View::number`, its inverse, used by
            // `ui::help::view_badge` so the two never drift apart.
            let n = digit as u8 - b'0';
            View::from_number(n).map(Action::SwitchView)
        }
        KeyCode::Enter => Some(Action::ZoomIn),
        KeyCode::Esc => Some(Action::ZoomOut),
        KeyCode::Right | KeyCode::Char('l') => Some(Action::Right),
        KeyCode::Left | KeyCode::Char('h') => Some(Action::Left),
        KeyCode::Tab => Some(Action::Tab),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::MoveNext),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::MovePrev),
        KeyCode::Char(']') => Some(Action::NextWorkshop),
        KeyCode::Char('[') => Some(Action::PrevWorkshop),
        KeyCode::Char('n') => Some(Action::NewTask),
        KeyCode::Char('m') => Some(Action::MessageAgent),
        KeyCode::Char('o') => Some(Action::MessageOrchestrator),
        KeyCode::Char('p') => Some(Action::SwitchProject),
        KeyCode::Char('x') => Some(Action::StopSelected),
        KeyCode::Char('q') => Some(Action::Detach),
        KeyCode::Char('g') => Some(Action::JumpNeedsAttention),
        KeyCode::Char('G') => Some(Action::JumpAndFocusNeedsAttention),
        KeyCode::Char('?') => Some(Action::ToggleHelp),
        KeyCode::Char('!') => Some(Action::ToggleAttentionFilter),
        KeyCode::PageUp => Some(Action::ScrollUp),
        KeyCode::PageDown => Some(Action::ScrollDown),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Modal state
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptKind {
    NewTask,
    MessageAgent(AgentId),
    MessageOrchestrator(AgentId),
    EditTaskTitle(TaskId),
}

/// A minimal in-TUI multi-field text prompt. `Tab`/`Enter` advance fields; `Enter` on the last
/// field submits; `Esc` cancels; `Backspace` edits; anything else printable is appended.
#[derive(Clone, Debug)]
pub struct PromptState {
    pub kind: PromptKind,
    pub labels: Vec<&'static str>,
    pub values: Vec<String>,
    pub field: usize,
}

impl PromptState {
    #[must_use]
    fn new_task() -> Self {
        Self {
            kind: PromptKind::NewTask,
            labels: vec!["title", "body"],
            values: vec![String::new(), String::new()],
            field: 0,
        }
    }

    #[must_use]
    fn message_agent(agent_id: AgentId) -> Self {
        Self {
            kind: PromptKind::MessageAgent(agent_id),
            labels: vec!["message"],
            values: vec![String::new()],
            field: 0,
        }
    }

    #[must_use]
    fn message_orchestrator(agent_id: AgentId) -> Self {
        Self {
            kind: PromptKind::MessageOrchestrator(agent_id),
            labels: vec!["message"],
            values: vec![String::new()],
            field: 0,
        }
    }

    #[must_use]
    fn edit_title(task_id: TaskId, current: String) -> Self {
        Self {
            kind: PromptKind::EditTaskTitle(task_id),
            labels: vec!["title"],
            values: vec![current],
            field: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerKind {
    /// Every project, oldest first, snapshotted at open time.
    Project(Vec<ProjectId>),
    AssignAgent(TaskId),
    /// Candidates snapshotted at open time (so the list can't drift mid-pick) — `Tab`/`j`/`k`
    /// cycle through them, matching the design brief's "`o` message the orchestrator ... if
    /// several, pick by Tab".
    Orchestrator(Vec<AgentId>),
}

#[derive(Clone, Debug)]
pub struct PickerState {
    pub kind: PickerKind,
    pub cursor: usize,
}

#[derive(Clone, Debug)]
pub struct TaskMenuState {
    pub task_id: TaskId,
    pub cursor: usize,
}

/// WORKSHOP's task action menu (`Enter` on a task) — "the only place those exist" per the design
/// brief, which is why `StartTask` isn't reachable from a top-level key any more.
pub const TASK_MENU_ITEMS: [&str; 6] =
    ["start", "assign", "cancel", "retry", "delete", "edit title"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PendingAction {
    DeleteTask(TaskId),
    StopSession {
        project_id: ProjectId,
        session_id: SessionId,
    },
    StopRun {
        project_id: ProjectId,
        run_id: RunId,
    },
}

#[derive(Clone, Debug)]
pub enum Mode {
    Normal,
    /// A second confirming keypress (`y`/`Enter`, or `x` again for a stop) carries out
    /// `PendingAction`; anything else cancels.
    Confirm(PendingAction),
    Prompt(PromptState),
    Picker(PickerState),
    /// WORKSHOP: `Enter` on a task.
    TaskMenu(TaskMenuState),
    Help,
}

/// A side effect for `main.rs` to carry out. `Board` never touches a socket or a PTY itself.
#[derive(Clone, Debug)]
pub enum Intent {
    None,
    Redraw,
    Quit,
    Send(LocalRequest),
    /// Only meaningful while a pane is attached (TERMINALS/FOCUS, `pane_mode` is `Typing`);
    /// `main.rs` encodes and forwards to whichever pane is currently focused.
    ForwardKey(KeyEvent),
    /// FOCUS only: scroll the focused pane's `vt100` scrollback up (`up: true`) or down.
    ScrollFocus {
        up: bool,
    },
}

fn step<T: Clone + PartialEq>(items: &[T], current: Option<&T>, forward: bool) -> Option<T> {
    if items.is_empty() {
        return None;
    }
    let index = current.and_then(|value| items.iter().position(|item| item == value));
    let next = match index {
        None => 0,
        Some(i) if forward => (i + 1) % items.len(),
        Some(i) => (i + items.len() - 1) % items.len(),
    };
    items.get(next).cloned()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
}

// ---------------------------------------------------------------------------------------------
// Board key-handling
// ---------------------------------------------------------------------------------------------

impl Board {
    pub fn handle_key(&mut self, key: KeyEvent) -> Intent {
        match &self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Confirm(action) => {
                let action = action.clone();
                self.handle_confirm_key(key, &action)
            }
            Mode::Prompt(_) => self.handle_prompt_key(key),
            Mode::Picker(_) => self.handle_picker_key(key),
            Mode::TaskMenu(_) => self.handle_task_menu_key(key),
            Mode::Help => self.handle_help_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Intent {
        if matches!(self.view, View::Terminals | View::Focus) {
            if key == crate::keys::PREFIX_KEY {
                self.pane_mode = match self.pane_mode {
                    PaneMode::Typing => PaneMode::Board,
                    PaneMode::Board => PaneMode::Typing,
                };
                return Intent::Redraw;
            }
            // Never forward when there's nothing to forward to — an empty TERMINALS/FOCUS
            // screen always leaves every key acting on the board, `Typing` or not.
            if self.pane_mode == PaneMode::Typing && self.has_live_pane() {
                return Intent::ForwardKey(key);
            }
            if self.pane_mode == PaneMode::Board
                && key.code == KeyCode::Char('i')
                && self.has_live_pane()
            {
                self.pane_mode = PaneMode::Typing;
                return Intent::Redraw;
            }
        }
        let Some(action) = keymap(key) else {
            return Intent::None;
        };
        self.dispatch(action)
    }

    fn dispatch(&mut self, action: Action) -> Intent {
        match action {
            Action::SwitchView(view) => self.switch_view(view),
            Action::ZoomIn => self.zoom_in(),
            Action::ZoomOut => self.zoom_out(),
            Action::Right if self.view == View::Fortress => {
                self.move_fortress_cursor(fortress::Direction::Right)
            }
            Action::Left if self.view == View::Fortress => {
                self.move_fortress_cursor(fortress::Direction::Left)
            }
            Action::Right => self.zoom_in(),
            Action::Left => self.zoom_out(),
            Action::Tab => self.handle_tab(),
            Action::MoveNext if self.view == View::Fortress => {
                self.move_fortress_cursor(fortress::Direction::Down)
            }
            Action::MovePrev if self.view == View::Fortress => {
                self.move_fortress_cursor(fortress::Direction::Up)
            }
            Action::MoveNext => self.handle_move(true),
            Action::MovePrev => self.handle_move(false),
            Action::NextWorkshop => self.cycle_selected_workshop(true),
            Action::PrevWorkshop => self.cycle_selected_workshop(false),
            Action::NewTask => self.begin_new_task(),
            Action::MessageAgent => self.begin_message_agent(),
            Action::MessageOrchestrator => self.begin_message_orchestrator(),
            Action::SwitchProject => self.begin_switch_project(),
            Action::StopSelected => self.begin_stop_selected(),
            Action::Detach => {
                self.quit = true;
                Intent::Quit
            }
            Action::JumpNeedsAttention => self.jump_to_attention(false),
            Action::JumpAndFocusNeedsAttention => self.jump_to_attention(true),
            Action::ToggleHelp => {
                self.mode = Mode::Help;
                Intent::Redraw
            }
            Action::ToggleAttentionFilter => {
                self.attention_filter = !self.attention_filter;
                Intent::Redraw
            }
            Action::ScrollUp if self.view == View::Focus => Intent::ScrollFocus { up: true },
            Action::ScrollDown if self.view == View::Focus => Intent::ScrollFocus { up: false },
            Action::ScrollUp | Action::ScrollDown => Intent::None,
        }
    }

    fn switch_view(&mut self, view: View) -> Intent {
        self.view = view;
        // TERMINALS *enters* in Board mode (Tab/j/k/arrows visibly move focus, no invisible
        // forwarding); FOCUS enters in Typing (you asked for this one pane to talk to). Other
        // views don't consult `pane_mode` at all, so leaving it as-is there is harmless.
        match view {
            View::Terminals => self.pane_mode = PaneMode::Board,
            View::Focus => self.pane_mode = PaneMode::Typing,
            View::Fortress | View::Workshop => {}
        }
        Intent::Redraw
    }

    fn zoom_in(&mut self) -> Intent {
        match self.view {
            View::Fortress => {
                // An explicitly selected workshop (`[`/`]`) wins over the agent cursor even if
                // it has no agents yet — the whole point of `selected_workshop` (see its field
                // doc comment): it's the only way to reach an empty project's WORKSHOP, where
                // `n` can then add its first task.
                if let Some(project_id) = self.selected_workshop.clone() {
                    self.focused_project = Some(project_id);
                    self.workshop_focus = WorkshopPane::Tasks;
                    self.view = View::Workshop;
                    return Intent::Redraw;
                }
                let target = self
                    .selected_agent
                    .clone()
                    .or_else(|| self.agents_in_fortress_order().first().cloned());
                let Some(agent_id) = target else {
                    self.set_status("no agents yet", StatusLevel::Error);
                    return Intent::Redraw;
                };
                let Some(agent) = self.agents.get(&agent_id) else {
                    return Intent::Redraw;
                };
                self.focused_project = Some(agent.project_id.clone());
                self.selected_agent = Some(agent_id);
                self.workshop_focus = WorkshopPane::Agents;
                self.view = View::Workshop;
                Intent::Redraw
            }
            View::Workshop => self.zoom_in_workshop(),
            View::Terminals => {
                let targets = self.terminal_targets();
                if targets.is_empty() {
                    self.set_status("no live sessions to zoom into", StatusLevel::Error);
                    return Intent::Redraw;
                }
                // `selected_agent` only tracks a TERMINALS pane once `Tab`/`j`/`k` has cycled to
                // it (see `cycle_selected_agent`) — jumping straight here via `3`/`Enter` without
                // ever cycling would otherwise leave it pointing nowhere (or at an agent whose
                // pane isn't even one of these tiles) and FOCUS would show "no agent selected"
                // despite a pane clearly being on screen. Default to the first visible tile.
                let already_visible = self.selected_agent.as_ref().is_some_and(|id| {
                    targets.iter().any(|session_id| {
                        self.agent_for_pane_session(session_id)
                            .is_some_and(|a| &a.id == id)
                    })
                });
                if !already_visible {
                    self.selected_agent = self
                        .agent_for_pane_session(&targets[0])
                        .map(|agent| agent.id.clone());
                }
                self.view = View::Focus;
                self.pane_mode = PaneMode::Typing;
                Intent::Redraw
            }
            View::Focus => Intent::None,
        }
    }

    fn zoom_in_workshop(&mut self) -> Intent {
        match self.workshop_focus {
            WorkshopPane::Tasks => {
                let task_id = self.selected_task.clone().or_else(|| {
                    self.focused_project.as_ref().and_then(|project_id| {
                        self.tasks_in(project_id)
                            .first()
                            .map(|t| t.snapshot.id.clone())
                    })
                });
                let Some(task_id) = task_id else {
                    self.set_status("no tasks yet", StatusLevel::Error);
                    return Intent::Redraw;
                };
                self.selected_task = Some(task_id.clone());
                self.mode = Mode::TaskMenu(TaskMenuState { task_id, cursor: 0 });
                Intent::Redraw
            }
            WorkshopPane::Agents => {
                // Same default-to-first fallback as the `Tasks` arm above: landing on this pane
                // (e.g. via `2` then `Tab`, never `j`/`k`) shouldn't dead-end zoom-in just
                // because nothing has been explicitly selected yet.
                let agent_id = self.selected_agent.clone().or_else(|| {
                    self.focused_project.as_ref().and_then(|project_id| {
                        self.visible_agent_tree(project_id)
                            .into_iter()
                            .map(|(id, _)| id)
                            .next()
                    })
                });
                let Some(agent_id) = agent_id else {
                    self.set_status("no agents yet", StatusLevel::Error);
                    return Intent::Redraw;
                };
                self.selected_agent = Some(agent_id);
                self.view = View::Terminals;
                self.pane_mode = PaneMode::Board;
                Intent::Redraw
            }
        }
    }

    fn zoom_out(&mut self) -> Intent {
        self.view = match self.view {
            View::Fortress => View::Fortress,
            View::Workshop => View::Fortress,
            View::Terminals => View::Workshop,
            View::Focus => View::Terminals,
        };
        Intent::Redraw
    }

    fn handle_tab(&mut self) -> Intent {
        match self.view {
            View::Fortress | View::Terminals | View::Focus => self.cycle_selected_agent(true),
            View::Workshop => {
                self.workshop_focus = match self.workshop_focus {
                    WorkshopPane::Tasks => WorkshopPane::Agents,
                    WorkshopPane::Agents => WorkshopPane::Tasks,
                };
                Intent::Redraw
            }
        }
    }

    fn handle_move(&mut self, forward: bool) -> Intent {
        match self.view {
            View::Fortress | View::Terminals | View::Focus => self.cycle_selected_agent(forward),
            View::Workshop => self.move_within_workshop(forward),
        }
    }

    /// FORTRESS: moves the cursor spatially over the same layout the floor renders (its width is
    /// what the renderer last drew with — `Board::fortress_width`), landing on the nearest station
    /// or empty workshop in `direction`. A cursor that isn't on the floor yet lands on the first
    /// target. Selecting a station also focuses its project, exactly like `Tab`.
    fn move_fortress_cursor(&mut self, direction: fortress::Direction) -> Intent {
        let workshops = fortress::compute_workshops(
            &self.projects_sorted(),
            &self.agents_vec(),
            self.fortress_width.get(),
        );
        let targets = fortress::cursor_targets(&workshops);
        let current = self
            .selected_agent
            .clone()
            .map(fortress::CursorTarget::Agent)
            .or_else(|| {
                self.selected_workshop
                    .clone()
                    .map(fortress::CursorTarget::Workshop)
            });
        let Some(next) = fortress::step_cursor(&targets, current.as_ref(), direction) else {
            return Intent::Redraw;
        };
        match next {
            fortress::CursorTarget::Agent(agent_id) => {
                if let Some(agent) = self.agents.get(&agent_id) {
                    self.focused_project = Some(agent.project_id.clone());
                }
                self.selected_agent = Some(agent_id);
                self.selected_workshop = None;
            }
            fortress::CursorTarget::Workshop(project_id) => {
                self.selected_workshop = Some(project_id);
                self.selected_agent = None;
            }
        }
        Intent::Redraw
    }

    fn cycle_selected_agent(&mut self, forward: bool) -> Intent {
        let candidates: Vec<AgentId> = match self.view {
            View::Terminals | View::Focus => self
                .terminal_targets()
                .into_iter()
                .filter_map(|session_id| self.agent_for_pane_session(&session_id))
                .map(|agent| agent.id.clone())
                .collect(),
            View::Fortress | View::Workshop => self.agents_in_fortress_order(),
        };
        let Some(next) = step(&candidates, self.selected_agent.as_ref(), forward) else {
            return Intent::Redraw;
        };
        if let Some(agent) = self.agents.get(&next) {
            self.focused_project = Some(agent.project_id.clone());
        }
        self.selected_agent = Some(next);
        self.selected_workshop = None;
        Intent::Redraw
    }

    /// `[`/`]`: cycles `selected_workshop` through every project in FORTRESS order, independent
    /// of `selected_agent` (see that field's doc comment). A no-op outside FORTRESS, same pattern
    /// as `!`/`PgUp`/`PgDn` elsewhere in this file.
    fn cycle_selected_workshop(&mut self, forward: bool) -> Intent {
        if self.view != View::Fortress {
            return Intent::None;
        }
        let ids: Vec<ProjectId> = self.projects_sorted().into_iter().map(|p| p.id).collect();
        let Some(next) = step(&ids, self.selected_workshop.as_ref(), forward) else {
            return Intent::Redraw;
        };
        self.selected_workshop = Some(next);
        self.selected_agent = None;
        Intent::Redraw
    }

    fn move_within_workshop(&mut self, forward: bool) -> Intent {
        let Some(project_id) = self.focused_project.clone() else {
            return Intent::Redraw;
        };
        match self.workshop_focus {
            WorkshopPane::Tasks => {
                let ids: Vec<TaskId> = self
                    .visible_tasks(&project_id)
                    .into_iter()
                    .map(|task| task.snapshot.id.clone())
                    .collect();
                self.selected_task = step(&ids, self.selected_task.as_ref(), forward);
            }
            WorkshopPane::Agents => {
                let ids: Vec<AgentId> = self
                    .visible_agent_tree(&project_id)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                self.selected_agent = step(&ids, self.selected_agent.as_ref(), forward);
            }
        }
        Intent::Redraw
    }

    pub(crate) fn task_project(&self, task_id: &TaskId) -> Option<ProjectId> {
        self.tasks
            .get(task_id)
            .map(|task| task.snapshot.project_id.clone())
    }

    pub(crate) fn agent_ids_in(&self, project_id: &ProjectId) -> Vec<AgentId> {
        self.agents_in_fortress_order()
            .into_iter()
            .filter(|id| {
                self.agents
                    .get(id)
                    .is_some_and(|agent| &agent.project_id == project_id)
            })
            .collect()
    }

    fn active_project(&self) -> Option<ProjectId> {
        self.focused_project.clone().or_else(|| {
            self.selected_agent
                .as_ref()
                .and_then(|id| self.agents.get(id))
                .map(|agent| agent.project_id.clone())
        })
    }

    fn begin_new_task(&mut self) -> Intent {
        if self.active_project().is_none() {
            self.set_status(
                "select a project first (zoom into a workshop)",
                StatusLevel::Error,
            );
            return Intent::Redraw;
        }
        self.mode = Mode::Prompt(PromptState::new_task());
        Intent::Redraw
    }

    fn begin_message_agent(&mut self) -> Intent {
        let Some(agent_id) = self.pane_target_agent() else {
            self.set_status("no agent selected", StatusLevel::Error);
            return Intent::Redraw;
        };
        self.mode = Mode::Prompt(PromptState::message_agent(agent_id));
        Intent::Redraw
    }

    fn begin_message_orchestrator(&mut self) -> Intent {
        if let Some(agent_id) = &self.selected_agent {
            if self
                .agents
                .get(agent_id)
                .is_some_and(|a| a.role == AgentRole::Orchestrator)
            {
                self.mode = Mode::Prompt(PromptState::message_orchestrator(agent_id.clone()));
                return Intent::Redraw;
            }
        }
        let scope = self.focused_project.clone();
        let candidates: Vec<AgentId> = self
            .orchestrators_in(scope.as_ref())
            .into_iter()
            .map(|agent| agent.id.clone())
            .collect();
        match candidates.len() {
            0 => {
                self.set_status("no orchestrator in scope", StatusLevel::Error);
                Intent::Redraw
            }
            1 => {
                self.mode = Mode::Prompt(PromptState::message_orchestrator(candidates[0].clone()));
                Intent::Redraw
            }
            _ => {
                self.mode = Mode::Picker(PickerState {
                    kind: PickerKind::Orchestrator(candidates),
                    cursor: 0,
                });
                Intent::Redraw
            }
        }
    }

    fn begin_switch_project(&mut self) -> Intent {
        let projects: Vec<ProjectId> = self
            .projects_sorted()
            .into_iter()
            .map(|project| project.id)
            .collect();
        if projects.is_empty() {
            self.set_status("no projects yet", StatusLevel::Error);
            return Intent::Redraw;
        }
        let cursor = self
            .focused_project
            .as_ref()
            .and_then(|focused| projects.iter().position(|id| id == focused))
            .unwrap_or(0);
        self.mode = Mode::Picker(PickerState {
            kind: PickerKind::Project(projects),
            cursor,
        });
        Intent::Redraw
    }

    fn begin_stop_selected(&mut self) -> Intent {
        let Some(agent_id) = self.pane_target_agent() else {
            self.set_status("no agent selected", StatusLevel::Error);
            return Intent::Redraw;
        };
        let Some(agent) = self.agents.get(&agent_id) else {
            return Intent::Redraw;
        };
        let project_id = agent.project_id.clone();
        if let Some(session_id) = agent.current_session_id.clone() {
            self.mode = Mode::Confirm(PendingAction::StopSession {
                project_id,
                session_id,
            });
            return Intent::Redraw;
        }
        if let Some(run_id) = agent.current_run_id.clone() {
            self.mode = Mode::Confirm(PendingAction::StopRun { project_id, run_id });
            return Intent::Redraw;
        }
        self.set_status("agent has nothing running", StatusLevel::Error);
        Intent::Redraw
    }

    fn jump_to_attention(&mut self, also_focus: bool) -> Intent {
        let candidates: Vec<AgentId> = self
            .agents_in_fortress_order()
            .into_iter()
            .filter(|id| {
                self.agents
                    .get(id)
                    .is_some_and(|agent| self.agent_attention(agent).value.needs_operator())
            })
            .collect();
        if candidates.is_empty() {
            self.set_status("no agents need attention", StatusLevel::Info);
            return Intent::Redraw;
        }
        let next = step(&candidates, self.selected_agent.as_ref(), true)
            .unwrap_or_else(|| candidates[0].clone());
        if let Some(agent) = self.agents.get(&next) {
            self.focused_project = Some(agent.project_id.clone());
        }
        self.selected_agent = Some(next);
        self.selected_workshop = None;
        if also_focus {
            self.view = View::Focus;
            self.pane_mode = PaneMode::Typing;
        }
        Intent::Redraw
    }

    // -- Confirm ----------------------------------------------------------------------------

    fn handle_confirm_key(&mut self, key: KeyEvent, action: &PendingAction) -> Intent {
        let extra_confirm_key = matches!(
            action,
            PendingAction::StopSession { .. } | PendingAction::StopRun { .. }
        ) && key.code == KeyCode::Char('x');
        let confirmed =
            matches!(key.code, KeyCode::Enter | KeyCode::Char('y')) || extra_confirm_key;
        self.mode = Mode::Normal;
        if !confirmed {
            self.set_status("cancelled", StatusLevel::Info);
            return Intent::Redraw;
        }
        match action.clone() {
            PendingAction::DeleteTask(task_id) => {
                let Some(project_id) = self.task_project(&task_id) else {
                    return Intent::Redraw;
                };
                Intent::Send(LocalRequest::DeleteTask {
                    project_id,
                    task_id,
                })
            }
            PendingAction::StopSession {
                project_id,
                session_id,
            } => Intent::Send(LocalRequest::StopSession {
                project_id,
                session_id,
                grace_ms: 5_000,
            }),
            PendingAction::StopRun { project_id, run_id } => Intent::Send(LocalRequest::StopRun {
                project_id,
                run_id,
                grace_ms: 5_000,
            }),
        }
    }

    // -- Prompt -------------------------------------------------------------------------------

    fn handle_prompt_key(&mut self, key: KeyEvent) -> Intent {
        let Mode::Prompt(prompt) = &mut self.mode else {
            return Intent::None;
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.set_status("cancelled", StatusLevel::Info);
                Intent::Redraw
            }
            KeyCode::Backspace => {
                prompt.values[prompt.field].pop();
                Intent::Redraw
            }
            KeyCode::Tab => {
                prompt.field = (prompt.field + 1) % prompt.labels.len();
                Intent::Redraw
            }
            KeyCode::Enter => {
                if prompt.field + 1 < prompt.labels.len() {
                    prompt.field += 1;
                    Intent::Redraw
                } else {
                    self.submit_prompt()
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                prompt.values[prompt.field].push(character);
                Intent::Redraw
            }
            _ => Intent::None,
        }
    }

    fn submit_prompt(&mut self) -> Intent {
        let Mode::Prompt(prompt) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return Intent::None;
        };
        match prompt.kind {
            PromptKind::NewTask => {
                let Some(project_id) = self.active_project() else {
                    self.set_status("no project selected yet", StatusLevel::Error);
                    return Intent::Redraw;
                };
                let title = prompt.values.first().cloned().unwrap_or_default();
                let body = prompt.values.get(1).cloned().unwrap_or_default();
                if title.trim().is_empty() {
                    self.set_status("task title can't be empty", StatusLevel::Error);
                    return Intent::Redraw;
                }
                let Ok(id) = TaskId::try_from(new_id()) else {
                    self.set_status("failed to generate a task id", StatusLevel::Error);
                    return Intent::Redraw;
                };
                Intent::Send(LocalRequest::CreateTask {
                    id,
                    project_id,
                    parent_task_id: None,
                    title,
                    body,
                    priority: 0,
                })
            }
            PromptKind::MessageAgent(recipient_agent_id)
            | PromptKind::MessageOrchestrator(recipient_agent_id) => {
                let Some(project_id) = self.active_project() else {
                    self.set_status("no project selected yet", StatusLevel::Error);
                    return Intent::Redraw;
                };
                let body = prompt.values.first().cloned().unwrap_or_default();
                if body.trim().is_empty() {
                    self.set_status("message can't be empty", StatusLevel::Error);
                    return Intent::Redraw;
                }
                let Ok(id) = factory_core::MessageId::try_from(new_id()) else {
                    self.set_status("failed to generate a message id", StatusLevel::Error);
                    return Intent::Redraw;
                };
                Intent::Send(LocalRequest::SendAgentMessage {
                    id,
                    project_id,
                    sender_agent_id: None,
                    recipient_agent_id,
                    body,
                })
            }
            PromptKind::EditTaskTitle(task_id) => {
                let Some(project_id) = self.task_project(&task_id) else {
                    return Intent::Redraw;
                };
                let title = prompt.values.first().cloned().unwrap_or_default();
                if title.trim().is_empty() {
                    self.set_status("task title can't be empty", StatusLevel::Error);
                    return Intent::Redraw;
                }
                Intent::Send(LocalRequest::UpdateTask {
                    project_id,
                    task_id,
                    title: Some(title),
                    body: None,
                })
            }
        }
    }

    // -- Picker -------------------------------------------------------------------------------

    fn handle_picker_key(&mut self, key: KeyEvent) -> Intent {
        let Mode::Picker(picker) = &self.mode else {
            return Intent::None;
        };
        let len = match &picker.kind {
            PickerKind::Project(projects) => projects.len(),
            PickerKind::AssignAgent(_) => picker_agent_count(self, picker),
            PickerKind::Orchestrator(candidates) => candidates.len(),
        };
        let Mode::Picker(picker) = &mut self.mode else {
            return Intent::None;
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                Intent::Redraw
            }
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => {
                if len > 0 {
                    picker.cursor = (picker.cursor + 1) % len;
                }
                Intent::Redraw
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if len > 0 {
                    picker.cursor = (picker.cursor + len - 1) % len;
                }
                Intent::Redraw
            }
            KeyCode::Enter => self.submit_picker(),
            _ => Intent::None,
        }
    }

    fn submit_picker(&mut self) -> Intent {
        let Mode::Picker(picker) = self.mode.clone() else {
            return Intent::None;
        };
        match picker.kind {
            PickerKind::Project(projects) => {
                self.mode = Mode::Normal;
                if let Some(project_id) = projects.get(picker.cursor).cloned() {
                    self.focus_project(project_id);
                    if self.view == View::Fortress {
                        self.view = View::Workshop;
                    }
                }
                Intent::Redraw
            }
            PickerKind::AssignAgent(task_id) => {
                self.mode = Mode::Normal;
                let Some(project_id) = self.task_project(&task_id) else {
                    return Intent::Redraw;
                };
                let ids = self.agent_ids_in(&project_id);
                let Some(agent_id) = ids.get(picker.cursor).cloned() else {
                    return Intent::Redraw;
                };
                Intent::Send(LocalRequest::AssignTask {
                    project_id,
                    task_id,
                    agent_id: Some(agent_id),
                })
            }
            PickerKind::Orchestrator(candidates) => {
                let Some(agent_id) = candidates.get(picker.cursor).cloned() else {
                    self.mode = Mode::Normal;
                    return Intent::Redraw;
                };
                self.mode = Mode::Prompt(PromptState::message_orchestrator(agent_id));
                Intent::Redraw
            }
        }
    }

    // -- TaskMenu -----------------------------------------------------------------------------

    fn handle_task_menu_key(&mut self, key: KeyEvent) -> Intent {
        let Mode::TaskMenu(state) = &mut self.mode else {
            return Intent::None;
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                Intent::Redraw
            }
            KeyCode::Char('j') | KeyCode::Down => {
                state.cursor = (state.cursor + 1) % TASK_MENU_ITEMS.len();
                Intent::Redraw
            }
            KeyCode::Char('k') | KeyCode::Up => {
                state.cursor = (state.cursor + TASK_MENU_ITEMS.len() - 1) % TASK_MENU_ITEMS.len();
                Intent::Redraw
            }
            KeyCode::Enter => self.submit_task_menu(),
            _ => Intent::None,
        }
    }

    fn submit_task_menu(&mut self) -> Intent {
        let Mode::TaskMenu(state) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return Intent::None;
        };
        let task_id = state.task_id;
        let Some(project_id) = self.task_project(&task_id) else {
            return Intent::Redraw;
        };
        match TASK_MENU_ITEMS[state.cursor] {
            "start" => {
                let Some(task) = self.tasks.get(&task_id) else {
                    return Intent::Redraw;
                };
                let Some(agent_id) = task.snapshot.assigned_agent_id.clone() else {
                    self.set_status("assign a task to an agent first", StatusLevel::Error);
                    return Intent::Redraw;
                };
                let worktree = self
                    .projects
                    .iter()
                    .find(|p| p.id == project_id)
                    .map(|p| p.root.clone());
                Intent::Send(LocalRequest::StartTask {
                    project_id,
                    task_id,
                    agent_id,
                    parent_run_id: None,
                    worktree,
                })
            }
            "assign" => {
                if self.agent_ids_in(&project_id).is_empty() {
                    self.set_status("no agents to assign", StatusLevel::Error);
                    return Intent::Redraw;
                }
                self.mode = Mode::Picker(PickerState {
                    kind: PickerKind::AssignAgent(task_id),
                    cursor: 0,
                });
                Intent::Redraw
            }
            "cancel" => Intent::Send(LocalRequest::CancelTask {
                project_id,
                task_id,
            }),
            "retry" => Intent::Send(LocalRequest::RetryTask {
                project_id,
                task_id,
            }),
            "delete" => {
                self.mode = Mode::Confirm(PendingAction::DeleteTask(task_id));
                Intent::Redraw
            }
            "edit title" => {
                let current = self
                    .tasks
                    .get(&task_id)
                    .map(|t| t.snapshot.title.clone())
                    .unwrap_or_default();
                self.mode = Mode::Prompt(PromptState::edit_title(task_id, current));
                Intent::Redraw
            }
            _ => Intent::None,
        }
    }

    // -- Help ---------------------------------------------------------------------------------

    fn handle_help_key(&mut self, key: KeyEvent) -> Intent {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?' | 'q') => {
                self.mode = Mode::Normal;
                Intent::Redraw
            }
            _ => Intent::None,
        }
    }
}

fn picker_agent_count(board: &Board, picker: &PickerState) -> usize {
    let PickerKind::AssignAgent(task_id) = &picker.kind else {
        return 0;
    };
    board
        .task_project(task_id)
        .map_or(0, |project_id| board.agent_ids_in(&project_id).len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::state::AgentState;
    use crate::test_fixtures::{agent, project, run, session, task};
    use factory_core::{AgentRole, RunStatus, SessionState, TaskStatus};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn board_with_one_project() -> Board {
        let mut board = Board::new(false, 0, crate::theme::FORTRESS);
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![
                agent("orch", "proj", AgentRole::Orchestrator, None),
                agent("alice", "proj", AgentRole::Worker, None),
                agent("bob", "proj", AgentRole::Worker, None),
            ],
            vec![task("t1", "proj", TaskStatus::Queued, None, 0)],
            Vec::new(),
            Vec::new(),
        );
        board
    }

    // -- keymap() table -----------------------------------------------------------------------

    #[test]
    fn keymap_covers_every_documented_key() {
        let cases: &[(KeyCode, Action)] = &[
            (KeyCode::Char('1'), Action::SwitchView(View::Fortress)),
            (KeyCode::Char('2'), Action::SwitchView(View::Workshop)),
            (KeyCode::Char('3'), Action::SwitchView(View::Terminals)),
            (KeyCode::Char('4'), Action::SwitchView(View::Focus)),
            (KeyCode::Enter, Action::ZoomIn),
            (KeyCode::Right, Action::Right),
            (KeyCode::Char('l'), Action::Right),
            (KeyCode::Esc, Action::ZoomOut),
            (KeyCode::Left, Action::Left),
            (KeyCode::Char('h'), Action::Left),
            (KeyCode::Tab, Action::Tab),
            (KeyCode::Char('j'), Action::MoveNext),
            (KeyCode::Down, Action::MoveNext),
            (KeyCode::Char('k'), Action::MovePrev),
            (KeyCode::Up, Action::MovePrev),
            (KeyCode::Char(']'), Action::NextWorkshop),
            (KeyCode::Char('['), Action::PrevWorkshop),
            (KeyCode::Char('n'), Action::NewTask),
            (KeyCode::Char('m'), Action::MessageAgent),
            (KeyCode::Char('o'), Action::MessageOrchestrator),
            (KeyCode::Char('p'), Action::SwitchProject),
            (KeyCode::Char('x'), Action::StopSelected),
            (KeyCode::Char('q'), Action::Detach),
            (KeyCode::Char('g'), Action::JumpNeedsAttention),
            (KeyCode::Char('G'), Action::JumpAndFocusNeedsAttention),
            (KeyCode::Char('?'), Action::ToggleHelp),
            (KeyCode::Char('!'), Action::ToggleAttentionFilter),
            (KeyCode::PageUp, Action::ScrollUp),
            (KeyCode::PageDown, Action::ScrollDown),
        ];
        for (code, expected) in cases {
            assert_eq!(keymap(key(*code)), Some(*expected), "{code:?}");
        }
    }

    #[test]
    fn keymap_returns_none_for_unbound_keys() {
        assert_eq!(keymap(key(KeyCode::Char('z'))), None);
        assert_eq!(keymap(key(KeyCode::F(5))), None);
    }

    // -- FORTRESS -------------------------------------------------------------------------

    #[test]
    fn fortress_tab_cycles_selected_agent_in_fortress_order() {
        let mut board = board_with_one_project();
        assert_eq!(board.view, View::Fortress);
        board.handle_key(key(KeyCode::Tab));
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "orch");
        board.handle_key(key(KeyCode::Tab));
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "alice");
        board.handle_key(key(KeyCode::Char('h')));
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "orch");
    }

    #[test]
    fn fortress_hjkl_and_arrows_move_the_cursor_over_the_floor() {
        // Two workshops rendered narrow enough that "empty" wraps under "proj".
        let mut board = board_with_an_empty_second_project();
        board
            .fortress_width
            .set(crate::fortress::MIN_WORKSHOP_WIDTH);
        assert_eq!(board.selected_agent, None);

        board.handle_key(key(KeyCode::Char('l')));
        assert_eq!(
            board.selected_agent.as_ref().unwrap().as_str(),
            "orch",
            "first target"
        );
        board.handle_key(key(KeyCode::Right));
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "alice");
        board.handle_key(key(KeyCode::Char('l')));
        assert_eq!(
            board.selected_agent.as_ref().unwrap().as_str(),
            "alice",
            "row end: no move"
        );
        board.handle_key(key(KeyCode::Char('h')));
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "orch");

        board.handle_key(key(KeyCode::Char('j')));
        assert_eq!(board.selected_agent, None);
        assert_eq!(
            board.selected_workshop.as_ref().unwrap().as_str(),
            "empty",
            "down lands on the empty workshop's placeholder"
        );
        board.handle_key(key(KeyCode::Up));
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "orch");
        assert_eq!(board.selected_workshop, None);
        assert_eq!(board.focused_project.as_ref().unwrap().as_str(), "proj");
        assert_eq!(board.view, View::Fortress, "moving never changes the view");

        // Enter still zooms into whatever the cursor is on.
        board.handle_key(key(KeyCode::Char('j')));
        board.handle_key(key(KeyCode::Enter));
        assert_eq!(board.view, View::Workshop);
        assert_eq!(board.focused_project.as_ref().unwrap().as_str(), "empty");
        // ...and outside FORTRESS, h/l keep zooming.
        board.handle_key(key(KeyCode::Char('h')));
        assert_eq!(board.view, View::Fortress);
    }

    #[test]
    fn fortress_zoom_in_enters_workshop_focused_on_that_agent() {
        let mut board = board_with_one_project();
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        let intent = board.handle_key(key(KeyCode::Enter));
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.view, View::Workshop);
        assert_eq!(board.focused_project.as_ref().unwrap().as_str(), "proj");
        assert_eq!(board.workshop_focus, WorkshopPane::Agents);
    }

    #[test]
    fn fortress_zoom_out_is_a_no_op() {
        let mut board = board_with_one_project();
        board.handle_key(key(KeyCode::Esc));
        assert_eq!(board.view, View::Fortress);
    }

    /// A second project with zero agents — `Tab`'s agent cursor (`agents_in_fortress_order`) has
    /// no candidate there at all, which is exactly the "can't reach it by keyboard" bug `[`/`]`
    /// fixes.
    fn board_with_an_empty_second_project() -> Board {
        let mut board = Board::new(false, 0, crate::theme::FORTRESS);
        board.apply_fleet_snapshot(
            vec![project("proj", 0), project("empty", 1)],
            vec![
                agent("orch", "proj", AgentRole::Orchestrator, None),
                agent("alice", "proj", AgentRole::Worker, None),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        board
    }

    #[test]
    fn fortress_brackets_cycle_selected_workshop_independent_of_agent_cursor() {
        let mut board = board_with_an_empty_second_project();
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());

        board.handle_key(key(KeyCode::Char(']')));
        assert_eq!(board.selected_workshop.as_ref().unwrap().as_str(), "proj");
        assert_eq!(
            board.selected_agent, None,
            "picking a workshop clears the agent cursor - only one selection shows at once"
        );

        board.handle_key(key(KeyCode::Char(']')));
        assert_eq!(board.selected_workshop.as_ref().unwrap().as_str(), "empty");

        board.handle_key(key(KeyCode::Char('[')));
        assert_eq!(board.selected_workshop.as_ref().unwrap().as_str(), "proj");
    }

    #[test]
    fn p_opens_a_project_picker_that_refocuses_and_zooms_to_workshop() {
        let mut board = board_with_an_empty_second_project();
        assert_eq!(board.view, View::Fortress);
        assert_eq!(board.focused_project.as_ref().unwrap().as_str(), "proj");

        board.handle_key(key(KeyCode::Char('p')));
        let Mode::Picker(PickerState {
            kind: PickerKind::Project(projects),
            cursor,
        }) = &board.mode
        else {
            panic!("expected a project picker, got {:?}", board.mode);
        };
        assert_eq!(projects.len(), 2, "every project, oldest first");
        assert_eq!(*cursor, 0, "cursor starts on the focused project");

        board.handle_key(key(KeyCode::Char('j')));
        board.handle_key(key(KeyCode::Enter));
        assert!(matches!(board.mode, Mode::Normal));
        assert_eq!(board.focused_project.as_ref().unwrap().as_str(), "empty");
        assert_eq!(
            board.view,
            View::Workshop,
            "from FORTRESS, picking a project zooms in"
        );

        // From WORKSHOP the picker keeps the view; the cursor follows the focused project.
        board.handle_key(key(KeyCode::Char('p')));
        let Mode::Picker(PickerState { cursor, .. }) = &board.mode else {
            panic!("expected a project picker");
        };
        assert_eq!(*cursor, 1);
        board.handle_key(key(KeyCode::Char('k')));
        board.handle_key(key(KeyCode::Enter));
        assert_eq!(board.focused_project.as_ref().unwrap().as_str(), "proj");
        assert_eq!(board.view, View::Workshop);

        // No projects at all: a status message, no picker — alongside the key hints, not
        // instead of them (see `status_line_keeps_the_key_hints_visible_after_cancelling_a_prompt`).
        let mut empty = Board::new(false, 0, crate::theme::FORTRESS);
        empty.handle_key(key(KeyCode::Char('p')));
        assert!(matches!(empty.mode, Mode::Normal));
        assert!(empty.status_line_text().contains("no projects yet"));
        assert!(empty.status_line_text().contains(&empty.help_text()));
    }

    #[test]
    fn fortress_agent_cursor_clears_the_workshop_cursor() {
        let mut board = board_with_an_empty_second_project();
        board.selected_workshop = Some(ProjectId::try_from("empty").unwrap());

        board.handle_key(key(KeyCode::Tab));

        assert_eq!(board.selected_workshop, None);
        assert!(board.selected_agent.is_some());
    }

    #[test]
    fn workshop_bracket_keys_are_a_no_op() {
        let mut board = board_with_an_empty_second_project();
        board.view = View::Workshop;
        let intent = board.handle_key(key(KeyCode::Char(']')));
        assert!(matches!(intent, Intent::None));
        assert_eq!(board.selected_workshop, None);
    }

    #[test]
    fn fortress_zoom_in_on_an_empty_selected_workshop_opens_its_workshop_view() {
        let mut board = board_with_an_empty_second_project();
        board.selected_workshop = Some(ProjectId::try_from("empty").unwrap());

        let intent = board.handle_key(key(KeyCode::Enter));

        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.view, View::Workshop);
        assert_eq!(board.focused_project.as_ref().unwrap().as_str(), "empty");
        assert!(
            !board.status_line_is_error(),
            "should not hit the \"no agents yet\" error path"
        );
    }

    #[test]
    fn new_task_works_after_zooming_into_an_empty_workshop() {
        let mut board = board_with_an_empty_second_project();
        board.selected_workshop = Some(ProjectId::try_from("empty").unwrap());
        board.handle_key(key(KeyCode::Enter));
        assert_eq!(board.view, View::Workshop);

        board.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(board.mode, Mode::Prompt(_)));
        for ch in "first task".chars() {
            board.handle_key(key(KeyCode::Char(ch)));
        }
        board.handle_key(key(KeyCode::Enter)); // advance to the body field
        let intent = board.handle_key(key(KeyCode::Enter)); // submit with an empty body

        match intent {
            Intent::Send(LocalRequest::CreateTask {
                project_id, title, ..
            }) => {
                assert_eq!(project_id.as_str(), "empty");
                assert_eq!(title, "first task");
            }
            other => panic!("expected CreateTask, got {other:?}"),
        }
    }

    #[test]
    fn g_jumps_to_next_agent_needing_attention() {
        let mut board = board_with_one_project();
        let blocked = run("alice", "proj", RunStatus::Blocked, 0);
        board.runs.insert(blocked.id.clone(), blocked);
        let intent = board.handle_key(key(KeyCode::Char('g')));
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "alice");
    }

    #[test]
    fn g_with_nothing_needing_attention_leaves_selection_alone() {
        let mut board = board_with_one_project();
        board.handle_key(key(KeyCode::Char('g')));
        assert_eq!(board.selected_agent, None);
        assert!(!board.status_line_is_error());
    }

    #[test]
    fn capital_g_jumps_and_switches_to_focus() {
        let mut board = board_with_one_project();
        let failed = run("alice", "proj", RunStatus::Failed, 0);
        board.runs.insert(failed.id.clone(), failed);
        board.handle_key(key(KeyCode::Char('G')));
        assert_eq!(board.view, View::Focus);
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "alice");
    }

    // -- WORKSHOP ---------------------------------------------------------------------------

    fn board_in_workshop() -> Board {
        let mut board = board_with_one_project();
        board.focused_project = Some(ProjectId::try_from("proj").unwrap());
        board.view = View::Workshop;
        board
    }

    #[test]
    fn workshop_tab_toggles_pane_focus() {
        let mut board = board_in_workshop();
        assert_eq!(board.workshop_focus, WorkshopPane::Tasks);
        board.handle_key(key(KeyCode::Tab));
        assert_eq!(board.workshop_focus, WorkshopPane::Agents);
        board.handle_key(key(KeyCode::Tab));
        assert_eq!(board.workshop_focus, WorkshopPane::Tasks);
    }

    #[test]
    fn workshop_enter_on_a_task_opens_the_task_menu() {
        let mut board = board_in_workshop();
        let intent = board.handle_key(key(KeyCode::Enter));
        assert!(matches!(intent, Intent::Redraw));
        assert!(matches!(board.mode, Mode::TaskMenu(_)));
    }

    #[test]
    fn workshop_enter_on_agents_pane_zooms_to_terminals() {
        let mut board = board_in_workshop();
        board.workshop_focus = WorkshopPane::Agents;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        let intent = board.handle_key(key(KeyCode::Enter));
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.view, View::Terminals);
    }

    /// Landing on WORKSHOP's agents pane without ever pressing `j`/`k` (e.g. `2` then `Tab`)
    /// leaves `selected_agent` unset — `Enter` should still zoom in on the first agent rather
    /// than dead-ending with a "no agent selected" status.
    #[test]
    fn workshop_enter_on_agents_pane_with_no_prior_selection_defaults_to_the_first_agent() {
        let mut board = board_in_workshop();
        board.workshop_focus = WorkshopPane::Agents;
        assert_eq!(board.selected_agent, None);
        let intent = board.handle_key(key(KeyCode::Enter));
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.view, View::Terminals);
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "orch");
    }

    #[test]
    fn workshop_esc_zooms_out_to_fortress() {
        let mut board = board_in_workshop();
        board.handle_key(key(KeyCode::Esc));
        assert_eq!(board.view, View::Fortress);
    }

    #[test]
    fn workshop_j_moves_task_cursor() {
        let mut board = board_in_workshop();
        board.tasks.insert(
            TaskId::try_from("t2").unwrap(),
            task("t2", "proj", TaskStatus::Queued, None, 10),
        );
        board.handle_key(key(KeyCode::Char('j')));
        assert_eq!(board.selected_task.as_ref().unwrap().as_str(), "t1");
        board.handle_key(key(KeyCode::Char('j')));
        assert_eq!(board.selected_task.as_ref().unwrap().as_str(), "t2");
    }

    #[test]
    fn task_menu_start_sends_start_task() {
        let mut board = board_in_workshop();
        board.tasks.insert(
            TaskId::try_from("t1").unwrap(),
            task("t1", "proj", TaskStatus::Queued, Some("alice"), 0),
        );
        board.mode = Mode::TaskMenu(TaskMenuState {
            task_id: TaskId::try_from("t1").unwrap(),
            cursor: 0,
        });
        let intent = board.handle_key(key(KeyCode::Enter));
        match intent {
            Intent::Send(LocalRequest::StartTask {
                agent_id, worktree, ..
            }) => {
                assert_eq!(agent_id.as_str(), "alice");
                assert_eq!(worktree.as_deref(), Some("/work"));
            }
            other => panic!("expected StartTask, got {other:?}"),
        }
    }

    #[test]
    fn task_menu_delete_requires_confirmation() {
        let mut board = board_in_workshop();
        board.mode = Mode::TaskMenu(TaskMenuState {
            task_id: TaskId::try_from("t1").unwrap(),
            cursor: 4, // "delete"
        });
        board.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            board.mode,
            Mode::Confirm(PendingAction::DeleteTask(_))
        ));
        let intent = board.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(
            intent,
            Intent::Send(LocalRequest::DeleteTask { .. })
        ));
    }

    #[test]
    fn task_menu_delete_cancelled_by_any_other_key() {
        let mut board = board_in_workshop();
        board.mode = Mode::Confirm(PendingAction::DeleteTask(TaskId::try_from("t1").unwrap()));
        let intent = board.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(intent, Intent::Redraw));
        assert!(matches!(board.mode, Mode::Normal));
    }

    #[test]
    fn workshop_bang_toggles_attention_filter() {
        let mut board = board_in_workshop();
        assert!(!board.attention_filter);
        board.handle_key(key(KeyCode::Char('!')));
        assert!(board.attention_filter);
    }

    // -- TERMINALS / FOCUS -------------------------------------------------------------------

    /// The bug this fixes: entering TERMINALS used to silently turn pane-forwarding on, so
    /// `Tab`/`j`/`k`/`1`-`4` looked dead (they were being typed into the pane instead). TERMINALS
    /// must enter in `Board` mode: navigation keys act on the board, and an unbound key is simply
    /// a no-op rather than being sent to a pane.
    #[test]
    fn terminals_enters_in_board_mode_not_typing() {
        let mut board = board_in_workshop();
        board.dev_local_pty = true;
        let intent = board.handle_key(key(KeyCode::Char('3')));
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.view, View::Terminals);
        assert_eq!(board.pane_mode, PaneMode::Board);

        let intent = board.handle_key(key(KeyCode::Tab));
        assert!(matches!(intent, Intent::Redraw));
        assert!(
            board.selected_agent.is_some(),
            "Tab should cycle the pane cursor, not be forwarded"
        );

        let intent = board.handle_key(key(KeyCode::Char('a')));
        assert!(
            matches!(intent, Intent::None),
            "an unbound key is a no-op in board mode, not pane input"
        );
    }

    #[test]
    fn i_switches_terminals_to_typing_when_a_pane_is_focused() {
        let mut board = board_in_workshop();
        board.dev_local_pty = true;
        board.handle_key(key(KeyCode::Char('3')));
        assert_eq!(board.pane_mode, PaneMode::Board);

        let intent = board.handle_key(key(KeyCode::Char('i')));
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.pane_mode, PaneMode::Typing);

        let intent = board.handle_key(key(KeyCode::Char('a')));
        assert!(matches!(intent, Intent::ForwardKey(_)));
    }

    #[test]
    fn prefix_key_toggles_between_board_and_typing_in_terminals() {
        let mut board = board_in_workshop();
        board.dev_local_pty = true;
        board.handle_key(key(KeyCode::Char('3')));
        assert_eq!(board.pane_mode, PaneMode::Board);

        let intent = board.handle_key(crate::keys::PREFIX_KEY);
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.pane_mode, PaneMode::Typing);
        let intent = board.handle_key(key(KeyCode::Char('a')));
        assert!(matches!(intent, Intent::ForwardKey(_)));

        let intent = board.handle_key(crate::keys::PREFIX_KEY);
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.pane_mode, PaneMode::Board);
        // Back in board mode, normal keymap applies.
        let intent = board.handle_key(key(KeyCode::Char('1')));
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.view, View::Fortress);
    }

    /// No live sessions at all (TERMINALS' empty-state screen): every key, including `i`, must
    /// keep acting on the board rather than being silently swallowed as input for a pane that
    /// isn't there — forwarding must be impossible here, not just off by default.
    #[test]
    fn terminals_empty_state_never_forwards_keys() {
        let mut board = board_in_workshop();
        assert!(board.terminal_targets().is_empty());
        board.handle_key(key(KeyCode::Char('3')));
        assert_eq!(board.view, View::Terminals);

        let intent = board.handle_key(key(KeyCode::Char('i')));
        assert_eq!(
            board.pane_mode,
            PaneMode::Board,
            "no live pane to type into"
        );
        assert!(!matches!(intent, Intent::ForwardKey(_)));

        let intent = board.handle_key(key(KeyCode::Char('1')));
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.view, View::Fortress, "1-4 must still switch views");
    }

    /// FOCUS is a deliberate single-pane zoom-in — it should be ready to type into immediately,
    /// unlike TERMINALS' multi-pane board.
    #[test]
    fn focus_defaults_to_typing_mode() {
        let mut board = board_in_workshop();
        board.dev_local_pty = true;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        let intent = board.handle_key(key(KeyCode::Char('4')));
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.view, View::Focus);
        assert_eq!(board.pane_mode, PaneMode::Typing);

        let intent = board.handle_key(key(KeyCode::Char('a')));
        assert!(matches!(intent, Intent::ForwardKey(_)));
    }

    /// Jumping straight to TERMINALS (`3`) and zooming in (`Enter`) without ever cycling a pane
    /// via `Tab`/`j`/`k` used to leave `selected_agent` unset, so FOCUS rendered "no agent
    /// selected" even though a pane was plainly on screen (`terminal_targets` non-empty). `Enter`
    /// should default to the first visible pane instead.
    #[test]
    fn terminals_zoom_in_with_no_prior_selection_defaults_to_the_first_pane() {
        let mut board = board_in_workshop();
        board.view = View::Terminals;
        board.dev_local_pty = true;
        board.pane_mode = PaneMode::Board;
        assert_eq!(board.selected_agent, None);

        let intent = board.handle_key(key(KeyCode::Enter));

        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.view, View::Focus);
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "orch");
        assert!(
            board.focus_target().is_some(),
            "FOCUS should have a pane to attach, not \"no agent selected\""
        );
    }

    /// A `selected_agent` left over from a *different* project's TERMINALS pane (e.g. after
    /// switching the focused project) is not one of this project's visible panes, so it should
    /// be replaced by the first visible one rather than zooming FOCUS onto a pane that isn't
    /// even on screen.
    #[test]
    fn terminals_zoom_in_replaces_a_selection_not_among_the_visible_panes() {
        let mut board = board_in_workshop();
        board.view = View::Terminals;
        board.dev_local_pty = true;
        board.pane_mode = PaneMode::Board;
        board.selected_agent = Some(AgentId::try_from("someone-else").unwrap());

        board.handle_key(key(KeyCode::Enter));

        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "orch");
    }

    /// The design's second worked example: zooming into TERMINALS from WORKSHOP's agents pane
    /// with the *second* agent selected must focus that agent's pane, not fall back to the
    /// first (the orchestrator's) — the "zoom in from a selected worker lands on the
    /// orchestrator's terminal" bug. `terminals_focused_pane` is the single source of truth both
    /// the highlight and the key-forwarding target read from, so asserting it here covers both.
    #[test]
    fn zoom_from_the_second_agent_focuses_that_agents_terminal() {
        let mut board = board_in_workshop();
        board.dev_local_pty = true;
        board.workshop_focus = WorkshopPane::Agents;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());

        let intent = board.handle_key(key(KeyCode::Enter));

        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.view, View::Terminals);
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "alice");
        assert_eq!(
            board.terminals_focused_pane(),
            Some(SessionId::try_from("dev-alice").unwrap()),
            "the focused pane must be the agent that was selected, not the first tile"
        );

        // Tab in TERMINALS' board mode moves the cursor and the focused pane together.
        board.handle_key(key(KeyCode::Tab));
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "bob");
        assert_eq!(
            board.terminals_focused_pane(),
            Some(SessionId::try_from("dev-bob").unwrap()),
            "the highlighted pane must follow the cursor Tab just moved"
        );
    }

    /// Same bug, reached from FORTRESS: cursor on the second agent, zoom in twice (FORTRESS →
    /// WORKSHOP → TERMINALS) — the focused terminal must still be that agent's, not the first
    /// tile's.
    #[test]
    fn zoom_from_a_fortress_cursor_on_the_second_agent_focuses_that_agents_terminal() {
        let mut board = board_with_one_project();
        board.dev_local_pty = true;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());

        board.handle_key(key(KeyCode::Enter)); // FORTRESS -> WORKSHOP, agents pane, alice selected
        assert_eq!(board.view, View::Workshop);
        assert_eq!(board.workshop_focus, WorkshopPane::Agents);

        board.handle_key(key(KeyCode::Enter)); // WORKSHOP agents -> TERMINALS
        assert_eq!(board.view, View::Terminals);
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "alice");
        assert_eq!(
            board.terminals_focused_pane(),
            Some(SessionId::try_from("dev-alice").unwrap())
        );
    }

    fn board_with_alice_selected_but_only_orch_live() -> Board {
        let mut board = board_in_workshop();
        let orch_id = AgentId::try_from("orch").unwrap();
        let mut orch = board.agents.get(&orch_id).unwrap().clone();
        orch.current_session_id = Some(SessionId::try_from("sess-orch").unwrap());
        board.agents.insert(orch_id, orch);
        board.sessions.insert(
            SessionId::try_from("sess-orch").unwrap(),
            session("sess-orch", "orch", "proj", SessionState::Working),
        );
        board.view = View::Terminals;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        board
    }

    #[test]
    fn typing_label_follows_the_pane_fallback_not_the_pane_less_selected_agent() {
        let mut board = board_with_alice_selected_but_only_orch_live();
        board.pane_mode = PaneMode::Typing;
        assert_eq!(
            board.terminals_focused_pane(),
            Some(SessionId::try_from("sess-orch").unwrap()),
            "sanity: the pane fallback should have landed on orch"
        );

        let text = board.help_text();
        assert!(
            text.contains("orch"),
            "label should follow the fallback: {text}"
        );
        assert!(
            !text.contains("alice"),
            "label must not name the pane-less selected agent: {text}"
        );
    }

    #[test]
    fn typing_status_line_never_advertises_forwarded_keys() {
        let mut board = board_with_alice_selected_but_only_orch_live();
        board.pane_mode = PaneMode::Typing;

        let text = board.help_text();
        assert!(
            !text.contains("1-4"),
            "1-4 is forwarded while typing: {text}"
        );
        assert!(
            !text.contains("q detach"),
            "q is forwarded while typing: {text}"
        );
        assert!(
            text.contains("Ctrl-]"),
            "the one key that actually escapes typing must still be listed: {text}"
        );

        // And it's not just the hint text lying - the keys really are forwarded, not dispatched.
        let intent = board.handle_key(key(KeyCode::Char('1')));
        assert!(matches!(intent, Intent::ForwardKey(_)));
        assert_eq!(
            board.view,
            View::Terminals,
            "1 must not have switched views"
        );
    }

    #[test]
    fn m_and_x_target_the_pane_fallback_not_the_pane_less_selected_agent() {
        let mut board = board_with_alice_selected_but_only_orch_live();
        board.pane_mode = PaneMode::Board;

        board.handle_key(key(KeyCode::Char('m')));
        match &board.mode {
            Mode::Prompt(prompt) => assert_eq!(
                prompt.kind,
                PromptKind::MessageAgent(AgentId::try_from("orch").unwrap()),
                "m must message the agent whose pane is focused, not the pane-less selection"
            ),
            other => panic!("expected Prompt, got {other:?}"),
        }
        board.mode = Mode::Normal;

        board.handle_key(key(KeyCode::Char('x')));
        match &board.mode {
            Mode::Confirm(PendingAction::StopSession { session_id, .. }) => assert_eq!(
                session_id,
                &SessionId::try_from("sess-orch").unwrap(),
                "x must stop the focused agent's session"
            ),
            other => panic!("expected stop-session confirmation, got {other:?}"),
        }
    }

    #[test]
    fn focus_scroll_actions_only_fire_in_focus_view() {
        let mut board = board_in_workshop();
        board.view = View::Terminals;
        board.pane_mode = PaneMode::Board;
        let intent = board.handle_key(key(KeyCode::PageUp));
        assert!(matches!(intent, Intent::None));

        board.view = View::Focus;
        let intent = board.handle_key(key(KeyCode::PageUp));
        assert!(matches!(intent, Intent::ScrollFocus { up: true }));
    }

    /// The handoff's "up one level" ladder, one `Esc` at a time: FOCUS → TERMINALS → WORKSHOP →
    /// FORTRESS. Only reachable while `pane_mode` is `Board` in TERMINALS/FOCUS — while `Typing`
    /// (FOCUS's default), `Esc` goes to the live pane instead, by design.
    #[test]
    fn esc_steps_up_one_level_at_a_time_through_every_view() {
        let mut board = board_in_workshop();
        board.pane_mode = PaneMode::Board;

        board.view = View::Focus;
        board.handle_key(key(KeyCode::Esc));
        assert_eq!(board.view, View::Terminals);

        board.handle_key(key(KeyCode::Esc));
        assert_eq!(board.view, View::Workshop);

        board.handle_key(key(KeyCode::Esc));
        assert_eq!(board.view, View::Fortress);

        // FORTRESS is the top of the ladder: Esc is a no-op from there.
        board.handle_key(key(KeyCode::Esc));
        assert_eq!(board.view, View::Fortress);
    }

    #[test]
    fn q_detaches_from_any_view() {
        for view in [View::Fortress, View::Workshop] {
            let mut board = board_in_workshop();
            board.view = view;
            let intent = board.handle_key(key(KeyCode::Char('q')));
            assert!(matches!(intent, Intent::Quit));
            assert!(board.quit);
        }
    }

    // -- messaging ----------------------------------------------------------------------------

    #[test]
    fn m_messages_the_selected_agent() {
        let mut board = board_in_workshop();
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        board.handle_key(key(KeyCode::Char('m')));
        match &board.mode {
            Mode::Prompt(prompt) => assert_eq!(
                prompt.kind,
                PromptKind::MessageAgent(AgentId::try_from("alice").unwrap())
            ),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn o_with_single_orchestrator_messages_it_directly() {
        let mut board = board_in_workshop();
        board.handle_key(key(KeyCode::Char('o')));
        match &board.mode {
            Mode::Prompt(prompt) => assert_eq!(
                prompt.kind,
                PromptKind::MessageOrchestrator(AgentId::try_from("orch").unwrap())
            ),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn o_with_multiple_orchestrators_opens_a_picker() {
        let mut board = board_in_workshop();
        board.agents.insert(
            AgentId::try_from("orch2").unwrap(),
            agent("orch2", "proj", AgentRole::Orchestrator, None),
        );
        board.handle_key(key(KeyCode::Char('o')));
        assert!(matches!(
            board.mode,
            Mode::Picker(PickerState {
                kind: PickerKind::Orchestrator(_),
                ..
            })
        ));
        board.handle_key(key(KeyCode::Tab));
        let intent = board.handle_key(key(KeyCode::Enter));
        assert!(matches!(intent, Intent::Redraw));
        assert!(matches!(board.mode, Mode::Prompt(_)));
    }

    // -- stop / confirm -----------------------------------------------------------------------

    #[test]
    fn x_with_a_session_confirms_stop_session() {
        let mut board = board_in_workshop();
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        let mut alice = board
            .agents
            .get(&AgentId::try_from("alice").unwrap())
            .unwrap()
            .clone();
        alice.current_session_id = Some(SessionId::try_from("sess-1").unwrap());
        board.agents.insert(alice.id.clone(), alice);
        board.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(
            board.mode,
            Mode::Confirm(PendingAction::StopSession { .. })
        ));
        let intent = board.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(
            intent,
            Intent::Send(LocalRequest::StopSession { .. })
        ));
    }

    #[test]
    fn x_without_a_session_falls_back_to_stop_run() {
        let mut board = board_in_workshop();
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        let mut alice = board
            .agents
            .get(&AgentId::try_from("alice").unwrap())
            .unwrap()
            .clone();
        alice.current_run_id = Some(RunId::try_from("run-1").unwrap());
        board.agents.insert(alice.id.clone(), alice);
        board.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(
            board.mode,
            Mode::Confirm(PendingAction::StopRun { .. })
        ));
    }

    #[test]
    fn x_with_nothing_running_errors() {
        let mut board = board_in_workshop();
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        let intent = board.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(intent, Intent::Redraw));
        assert!(board.status_line_is_error());
    }

    // -- new task -----------------------------------------------------------------------------

    #[test]
    fn n_opens_new_task_prompt_and_submits() {
        let mut board = board_in_workshop();
        board.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(board.mode, Mode::Prompt(_)));
        for ch in "fix bug".chars() {
            board.handle_key(key(KeyCode::Char(ch)));
        }
        board.handle_key(key(KeyCode::Enter));
        for ch in "details".chars() {
            board.handle_key(key(KeyCode::Char(ch)));
        }
        let intent = board.handle_key(key(KeyCode::Enter));
        match intent {
            Intent::Send(LocalRequest::CreateTask { title, body, .. }) => {
                assert_eq!(title, "fix bug");
                assert_eq!(body, "details");
            }
            other => panic!("expected CreateTask, got {other:?}"),
        }
    }

    #[test]
    fn help_toggles_and_closes() {
        let mut board = board_with_one_project();
        board.handle_key(key(KeyCode::Char('?')));
        assert!(matches!(board.mode, Mode::Help));
        board.handle_key(key(KeyCode::Esc));
        assert!(matches!(board.mode, Mode::Normal));
    }

    // -- status line -------------------------------------------------------------------------

    /// The bug: after Esc out of a prompt, the status bar showed only the "cancelled" message
    /// until it expired, several seconds during which the operator had no key-hint reference at
    /// all. The hint line must always be present, with the status message alongside it.
    #[test]
    fn status_line_keeps_the_key_hints_visible_after_cancelling_a_prompt() {
        let mut board = board_with_one_project();
        board.handle_key(key(KeyCode::Char('n'))); // open the new-task prompt
        assert!(matches!(board.mode, Mode::Prompt(_)));

        board.handle_key(key(KeyCode::Esc)); // cancel — sets a sticky "cancelled" status
        assert!(matches!(board.mode, Mode::Normal));

        let text = board.status_line_text();
        assert!(text.contains("cancelled"), "status message missing: {text}");
        assert!(
            text.contains(&board.help_text()),
            "key hints must not disappear behind the status message: {text}"
        );
    }

    // -- state precedence smoke test (see also model::tests) ----------------------------------

    #[test]
    fn agent_state_is_idle_by_default_with_no_run_or_session() {
        let board = board_with_one_project();
        let alice = board
            .agents
            .get(&AgentId::try_from("alice").unwrap())
            .unwrap();
        assert_eq!(board.agent_state(alice).value, AgentState::Idle);
        assert!(board.agent_state(alice).inferred);
    }
}
