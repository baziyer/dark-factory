//! The two-screen operator navigation model. BUILDING is the fleet overview; AGENT is the
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

use super::{AttentionTarget, Board, StatusLevel};
use crate::mouse::Target as MouseTarget;

// ---------------------------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------------------------

/// The two operator screens.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum View {
    Building,
    Agent,
}

impl View {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Building => "BUILDING",
            Self::Agent => "AGENT",
        }
    }
}

/// Whether AGENT keys control the board or the live terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneMode {
    /// Keys control TUI navigation and actions.
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
    /// `Enter`.
    ZoomIn,
    /// `Esc`.
    ZoomOut,
    /// `→`/`l`: open AGENT.
    Right,
    /// `←`/`h`: return to BUILDING.
    Left,
    /// `j`/`↓`: next agent.
    MoveNext,
    /// `k`/`↑`: previous agent.
    MovePrev,
    /// `]`: next agent.
    NextAgent,
    /// `[`: previous agent.
    PrevAgent,
    NewTask,
    MessageAgent,
    MessageOrchestrator,
    /// `p`: pick the focused project from a list (remembered across runs, see `main.rs`).
    SwitchProject,
    /// `x`: stop the selected agent (2-press confirm).
    StopSelected,
    Detach,
    JumpNeedsAttention,
    ToggleHelp,
    /// `z`: toggle the maximised terminal.
    MaximizeTerminal,
    TogglePause,
    EditInstructions,
    EditMemory,
    ManageTask,
    EditModel,
    EditPermission,
    EditCapacity,
    /// `PgUp`/`PgDn`: AGENT terminal scrollback.
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
        KeyCode::Enter => Some(Action::ZoomIn),
        KeyCode::Esc => Some(Action::ZoomOut),
        KeyCode::Right | KeyCode::Char('l') => Some(Action::Right),
        KeyCode::Left | KeyCode::Char('h') => Some(Action::Left),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::MoveNext),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::MovePrev),
        KeyCode::Char(']') => Some(Action::NextAgent),
        KeyCode::Char('[') => Some(Action::PrevAgent),
        KeyCode::Char('n') => Some(Action::NewTask),
        KeyCode::Char('m') => Some(Action::MessageAgent),
        KeyCode::Char('o') => Some(Action::MessageOrchestrator),
        KeyCode::Char('p') => Some(Action::SwitchProject),
        KeyCode::Char('x') => Some(Action::StopSelected),
        KeyCode::Char('q') => Some(Action::Detach),
        KeyCode::Char('g') => Some(Action::JumpNeedsAttention),
        KeyCode::Char('?') => Some(Action::ToggleHelp),
        KeyCode::Char('z') => Some(Action::MaximizeTerminal),
        KeyCode::Char(' ') => Some(Action::TogglePause),
        KeyCode::Char('I') => Some(Action::EditInstructions),
        KeyCode::Char('M') => Some(Action::EditMemory),
        KeyCode::Char('t') => Some(Action::ManageTask),
        KeyCode::Char('v') => Some(Action::EditModel),
        KeyCode::Char('a') => Some(Action::EditPermission),
        KeyCode::Char('C') => Some(Action::EditCapacity),
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
    NewTask(Option<AgentId>),
    MessageAgent(AgentId),
    MessageOrchestrator(AgentId),
    EditTaskTitle(TaskId),
    EditModel(AgentId),
    EditPermission(AgentId),
    Capacity,
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
    fn new_task(agent_id: Option<AgentId>) -> Self {
        Self {
            kind: PromptKind::NewTask(agent_id),
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

    fn edit_profile(agent_id: AgentId, permission: bool, current: String) -> Self {
        Self {
            kind: if permission {
                PromptKind::EditPermission(agent_id)
            } else {
                PromptKind::EditModel(agent_id)
            },
            labels: vec![if permission {
                "permission mode"
            } else {
                "model"
            }],
            values: vec![current],
            field: 0,
        }
    }

    #[must_use]
    fn capacity(current: String) -> Self {
        Self {
            kind: PromptKind::Capacity,
            labels: vec!["live-session capacity"],
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
pub const TASK_MENU_ITEMS: [&str; 7] = [
    "start",
    "assign",
    "backlog",
    "cancel",
    "retry",
    "delete",
    "edit title",
];

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
    SetCapacity(usize),
    /// Only meaningful while a pane is attached (TERMINALS/FOCUS, `pane_mode` is `Typing`);
    /// `main.rs` encodes and forwards to whichever pane is currently focused.
    ForwardKey(KeyEvent),
    /// FOCUS only: scroll the focused pane's `vt100` scrollback up (`up: true`) or down.
    ScrollFocus {
        up: bool,
    },
    EditFile(String),
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

    /// Applies a target from the most recently rendered mouse hit map. Every id is checked again
    /// against current board state so a fleet update can only turn an old target into a no-op.
    pub fn handle_mouse_target(&mut self, target: MouseTarget) -> Intent {
        if !matches!(self.mode, Mode::Normal) {
            return Intent::None;
        }
        match target {
            MouseTarget::View(View::Building) => self.back_to_building(),
            MouseTarget::View(View::Agent) => self.open_agent(),
            MouseTarget::Help => self.dispatch(Action::ToggleHelp),
            MouseTarget::Detach => self.dispatch(Action::Detach),
            MouseTarget::Agent(agent_id) => {
                if self.select_agent(agent_id) {
                    Intent::Redraw
                } else {
                    Intent::None
                }
            }
            MouseTarget::Task(task_id) => {
                let Some(agent_id) = self.selected_agent.as_ref() else {
                    return Intent::None;
                };
                let Some(task) = self
                    .active_tasks_for_agent(agent_id)
                    .into_iter()
                    .find(|task| task.snapshot.id == task_id)
                else {
                    return Intent::None;
                };
                self.focused_project = Some(task.snapshot.project_id.clone());
                self.selected_task = Some(task_id);
                self.pane_mode = PaneMode::Board;
                Intent::Redraw
            }
            MouseTarget::Attention(target) => {
                if self.select_attention_target(&target) {
                    Intent::Redraw
                } else {
                    Intent::None
                }
            }
            MouseTarget::Pane(session_id) => {
                let Some(agent_id) = self
                    .agent_for_pane_session(&session_id)
                    .map(|agent| agent.id.clone())
                else {
                    return Intent::None;
                };
                self.select_agent(agent_id);
                self.view = View::Agent;
                self.pane_mode = PaneMode::Board;
                Intent::Redraw
            }
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Intent {
        if self.view == View::Agent {
            if key == crate::keys::PREFIX_KEY {
                if self.pane_mode == PaneMode::Typing {
                    self.pane_mode = PaneMode::Board;
                } else if self.has_live_pane() {
                    self.pane_mode = PaneMode::Typing;
                } else {
                    self.set_status("terminal is not attached yet", StatusLevel::Error);
                }
                return Intent::Redraw;
            }
            if self.pane_mode == PaneMode::Typing && self.has_live_pane() {
                return Intent::ForwardKey(key);
            }
            if self.pane_mode == PaneMode::Board
                && matches!(key.code, KeyCode::Char('i') | KeyCode::Enter)
            {
                if self.has_live_pane() {
                    self.pane_mode = PaneMode::Typing;
                } else {
                    self.set_status("terminal is not attached yet", StatusLevel::Error);
                }
                return Intent::Redraw;
            }
        }
        keymap(key).map_or(Intent::None, |action| self.dispatch(action))
    }

    fn dispatch(&mut self, action: Action) -> Intent {
        match action {
            Action::ZoomIn | Action::Right => self.open_agent(),
            Action::ZoomOut | Action::Left => self.back_to_building(),
            Action::MoveNext | Action::NextAgent => self.cycle_selected_agent(true),
            Action::MovePrev | Action::PrevAgent => self.cycle_selected_agent(false),
            Action::NewTask => self.begin_new_task(),
            Action::MessageAgent => self.begin_message_agent(),
            Action::MessageOrchestrator => self.begin_message_orchestrator(),
            Action::SwitchProject => self.begin_switch_project(),
            Action::StopSelected => self.begin_stop_selected(),
            Action::Detach => {
                self.quit = true;
                Intent::Quit
            }
            Action::JumpNeedsAttention => self.jump_to_attention(),
            Action::ToggleHelp => {
                self.mode = Mode::Help;
                Intent::Redraw
            }
            Action::MaximizeTerminal => {
                self.terminal_maximized = !self.terminal_maximized;
                Intent::Redraw
            }
            Action::TogglePause => self.toggle_pause(),
            Action::EditInstructions => self.edit_guidance(true),
            Action::EditMemory => self.edit_guidance(false),
            Action::ManageTask => self.manage_task(),
            Action::EditModel => self.begin_profile_edit(false),
            Action::EditPermission => self.begin_profile_edit(true),
            Action::EditCapacity => self.begin_capacity_edit(),
            Action::ScrollUp if self.view == View::Agent => Intent::ScrollFocus { up: true },
            Action::ScrollDown if self.view == View::Agent => Intent::ScrollFocus { up: false },
            Action::ScrollUp | Action::ScrollDown => Intent::None,
        }
    }

    fn open_agent(&mut self) -> Intent {
        if self.view == View::Agent {
            return Intent::None;
        }
        let Some(agent_id) = self
            .selected_agent
            .clone()
            .or_else(|| self.agents_in_fortress_order().first().cloned())
        else {
            self.set_status("no agents yet", StatusLevel::Error);
            return Intent::Redraw;
        };
        if let Some(agent) = self.agents.get(&agent_id) {
            self.focused_project = Some(agent.project_id.clone());
        }
        self.selected_agent = Some(agent_id);
        self.view = View::Agent;
        self.pane_mode = PaneMode::Board;
        Intent::Redraw
    }

    fn back_to_building(&mut self) -> Intent {
        self.view = View::Building;
        self.pane_mode = PaneMode::Board;
        self.terminal_maximized = false;
        Intent::Redraw
    }

    fn cycle_selected_agent(&mut self, forward: bool) -> Intent {
        let candidates = self.agents_in_fortress_order();
        if let Some(agent_id) = step(&candidates, self.selected_agent.as_ref(), forward) {
            self.select_agent(agent_id);
        }
        Intent::Redraw
    }

    fn select_agent(&mut self, agent_id: AgentId) -> bool {
        let Some(agent) = self.agents.get(&agent_id) else {
            return false;
        };
        self.focused_project = Some(agent.project_id.clone());
        self.selected_agent = Some(agent_id);
        self.selected_task = None;
        true
    }

    fn toggle_pause(&mut self) -> Intent {
        let Some(agent_id) = self.selected_agent.clone() else {
            return Intent::None;
        };
        let Some(agent) = self.agents.get(&agent_id) else {
            return Intent::None;
        };
        let request = if agent.paused {
            LocalRequest::ResumeAgent {
                project_id: agent.project_id.clone(),
                agent_id,
            }
        } else {
            LocalRequest::PauseAgent {
                project_id: agent.project_id.clone(),
                agent_id,
            }
        };
        Intent::Send(request)
    }

    fn edit_guidance(&mut self, instructions: bool) -> Intent {
        let Some(agent_id) = self.selected_agent.as_ref() else {
            return Intent::None;
        };
        let Some(detail) = self.agent_details.get(agent_id) else {
            self.set_status("agent settings are still loading", StatusLevel::Error);
            return Intent::Redraw;
        };
        Intent::EditFile(if instructions {
            detail.instructions_path.clone()
        } else {
            detail.memory_path.clone()
        })
    }

    fn manage_task(&mut self) -> Intent {
        let Some(agent_id) = self.selected_agent.as_ref() else {
            return Intent::None;
        };
        let tasks = self.active_tasks_for_agent(agent_id);
        let Some(task_id) = self
            .selected_task
            .as_ref()
            .filter(|selected| tasks.iter().any(|task| &task.snapshot.id == *selected))
            .cloned()
            .or_else(|| tasks.first().map(|task| task.snapshot.id.clone()))
        else {
            self.set_status("agent queue is empty", StatusLevel::Info);
            return Intent::Redraw;
        };
        self.mode = Mode::TaskMenu(TaskMenuState { task_id, cursor: 0 });
        Intent::Redraw
    }

    fn begin_profile_edit(&mut self, permission: bool) -> Intent {
        let Some(agent_id) = self.selected_agent.clone() else {
            return Intent::None;
        };
        let Some(detail) = self.agent_details.get(&agent_id) else {
            self.set_status("agent settings are still loading", StatusLevel::Error);
            return Intent::Redraw;
        };
        let current = if permission {
            detail.profile.permission_mode.clone()
        } else {
            detail.profile.model.clone()
        }
        .unwrap_or_default();
        self.mode = Mode::Prompt(PromptState::edit_profile(agent_id, permission, current));
        Intent::Redraw
    }

    fn begin_capacity_edit(&mut self) -> Intent {
        let current = self
            .live_session_cap
            .map_or_else(|| "4".to_owned(), |capacity| capacity.to_string());
        self.mode = Mode::Prompt(PromptState::capacity(current));
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
        let agent_id = (self.view == View::Agent)
            .then(|| self.selected_agent.clone())
            .flatten();
        self.mode = Mode::Prompt(PromptState::new_task(agent_id));
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

    fn jump_to_attention(&mut self) -> Intent {
        let items = self.attention_items();
        if items.is_empty() {
            self.set_status("nothing needs attention", StatusLevel::Info);
            return Intent::Redraw;
        }
        let current = items.iter().position(|item| match &item.target {
            AttentionTarget::Agent(id) => {
                self.selected_task.is_none() && self.selected_agent.as_ref() == Some(id)
            }
            AttentionTarget::Task(id) => self.selected_task.as_ref() == Some(id),
        });
        let next = &items[current.map_or(0, |index| (index + 1) % items.len())];
        self.select_attention_target(&next.target);
        Intent::Redraw
    }

    fn select_attention_target(&mut self, target: &AttentionTarget) -> bool {
        let Some(item) = self
            .attention_items()
            .into_iter()
            .find(|item| &item.target == target)
        else {
            return false;
        };
        self.focused_project = Some(item.project_id);
        match target {
            AttentionTarget::Agent(id) => {
                self.selected_agent = Some(id.clone());
                self.selected_task = None;
                self.set_status(format!("attention: agent {id}"), StatusLevel::Info);
            }
            AttentionTarget::Task(id) => {
                self.selected_task = Some(id.clone());
                self.selected_agent = self
                    .tasks
                    .get(id)
                    .and_then(|task| task.snapshot.assigned_agent_id.clone());
                self.set_status(format!("attention: task#{id}"), StatusLevel::Info);
            }
        }
        self.view = View::Building;
        self.pane_mode = PaneMode::Board;
        true
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
            PromptKind::NewTask(agent_id) => {
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
                let request = if let Some(agent_id) = agent_id {
                    LocalRequest::CreateAssignedTask {
                        id,
                        project_id,
                        parent_task_id: None,
                        title,
                        body,
                        priority: 0,
                        agent_id,
                    }
                } else {
                    LocalRequest::CreateTask {
                        id,
                        project_id,
                        parent_task_id: None,
                        title,
                        body,
                        priority: 0,
                    }
                };
                Intent::Send(request)
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
            PromptKind::EditModel(agent_id) => {
                let Some(detail) = self.agent_details.get(&agent_id) else {
                    return Intent::Redraw;
                };
                let value = prompt
                    .values
                    .first()
                    .cloned()
                    .filter(|value| !value.trim().is_empty());
                Intent::Send(LocalRequest::UpdateAgentProfile {
                    project_id: detail.snapshot.project_id.clone(),
                    agent_id,
                    model: value,
                    permission_mode: detail.profile.permission_mode.clone(),
                    instructions: detail.profile.instructions.clone(),
                    memory: detail.profile.memory.clone(),
                })
            }
            PromptKind::EditPermission(agent_id) => {
                let Some(detail) = self.agent_details.get(&agent_id) else {
                    return Intent::Redraw;
                };
                let permission_mode = prompt
                    .values
                    .first()
                    .cloned()
                    .filter(|value| !value.trim().is_empty());
                Intent::Send(LocalRequest::UpdateAgentProfile {
                    project_id: detail.snapshot.project_id.clone(),
                    agent_id,
                    model: detail.profile.model.clone(),
                    permission_mode,
                    instructions: detail.profile.instructions.clone(),
                    memory: detail.profile.memory.clone(),
                })
            }
            PromptKind::Capacity => {
                let value = prompt
                    .values
                    .first()
                    .and_then(|value| value.trim().parse::<usize>().ok());
                let Some(value) = value else {
                    self.set_status("capacity must be a positive integer", StatusLevel::Error);
                    return Intent::Redraw;
                };
                if let Err(error) = factoryctl::capacity::validate(value) {
                    self.set_status(error, StatusLevel::Error);
                    return Intent::Redraw;
                }
                Intent::SetCapacity(value)
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
                    if self.view == View::Building {
                        self.view = View::Building;
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
            "backlog" => Intent::Send(LocalRequest::AssignTask {
                project_id,
                task_id,
                agent_id: None,
            }),
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
    use crate::test_fixtures::{agent, project, session, task};
    use factory_core::{AgentRole, SessionState, TaskStatus};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn board() -> Board {
        let mut board = Board::new(false, 0, crate::theme::FORTRESS);
        let mut alice = agent("alice", "proj", AgentRole::Worker, None);
        alice.current_session_id = Some(SessionId::try_from("session").unwrap());
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![alice, agent("bob", "proj", AgentRole::Worker, None)],
            Vec::new(),
            Vec::new(),
            vec![session("session", "alice", "proj", SessionState::Working)],
        );
        board
    }

    #[test]
    fn old_numbered_views_are_gone() {
        for digit in '1'..='4' {
            assert_eq!(keymap(key(KeyCode::Char(digit))), None);
        }
    }

    #[test]
    fn building_opens_selected_agent_and_escape_returns_home() {
        let mut board = board();
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        assert!(matches!(
            board.handle_key(key(KeyCode::Enter)),
            Intent::Redraw
        ));
        assert_eq!(board.view, View::Agent);
        assert_eq!(board.pane_mode, PaneMode::Board);
        board.handle_key(key(KeyCode::Esc));
        assert_eq!(board.view, View::Building);
    }

    #[test]
    fn agent_switching_stays_on_agent_screen() {
        let mut board = board();
        board.view = View::Agent;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        board.handle_key(key(KeyCode::Char(']')));
        assert_eq!(
            board.selected_agent.as_ref().map(AgentId::as_str),
            Some("bob")
        );
        assert_eq!(board.view, View::Agent);
    }

    #[test]
    fn typing_has_exclusive_input_until_prefix() {
        let mut board = board();
        board.view = View::Agent;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        board.pane_ready = true;
        board.handle_key(key(KeyCode::Char('i')));
        assert_eq!(board.pane_mode, PaneMode::Typing);
        assert!(matches!(
            board.handle_key(key(KeyCode::Char('q'))),
            Intent::ForwardKey(_)
        ));
        board.handle_key(crate::keys::PREFIX_KEY);
        assert_eq!(board.pane_mode, PaneMode::Board);
    }

    #[test]
    fn pause_uses_the_shared_local_api_request() {
        let mut board = board();
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        assert!(matches!(
            board.handle_key(key(KeyCode::Char(' '))),
            Intent::Send(LocalRequest::PauseAgent { .. })
        ));
    }

    #[test]
    fn capacity_setting_is_an_operator_prompt_and_shared_intent() {
        let mut board = board();
        board.live_session_cap = Some(8);
        assert!(matches!(
            board.handle_key(key(KeyCode::Char('C'))),
            Intent::Redraw
        ));
        assert!(matches!(
            board.handle_key(key(KeyCode::Enter)),
            Intent::SetCapacity(8)
        ));
    }

    #[test]
    fn agent_new_task_uses_atomic_assigned_create() {
        let mut board = board();
        board.view = View::Agent;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        assert!(matches!(
            board.handle_key(key(KeyCode::Char('n'))),
            Intent::Redraw
        ));
        for character in "build".chars() {
            board.handle_key(key(KeyCode::Char(character)));
        }
        board.handle_key(key(KeyCode::Enter));
        for character in "body".chars() {
            board.handle_key(key(KeyCode::Char(character)));
        }
        let Intent::Send(request) = board.handle_key(key(KeyCode::Enter)) else {
            panic!("task prompt did not submit");
        };
        assert!(matches!(
            request,
            LocalRequest::CreateTask { agent_id, .. }
                if agent_id == Some(AgentId::try_from("alice").unwrap())
        ));
    }

    #[test]
    fn queue_rows_are_stable_and_mouse_targets_the_same_assigned_queue() {
        let mut board = board();
        board.view = View::Agent;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        board.tasks.insert(
            TaskId::try_from("later").unwrap(),
            task("later", "proj", TaskStatus::Queued, Some("alice"), 20),
        );
        board.tasks.insert(
            TaskId::try_from("first").unwrap(),
            task("first", "proj", TaskStatus::Queued, Some("alice"), 10),
        );
        assert_eq!(
            board
                .active_tasks_for_agent(&AgentId::try_from("alice").unwrap())
                .iter()
                .map(|task| task.snapshot.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "later"]
        );
        assert!(matches!(
            board.handle_mouse_target(MouseTarget::Task(TaskId::try_from("later").unwrap())),
            Intent::Redraw
        ));
        assert_eq!(
            board.selected_task.as_ref().map(TaskId::as_str),
            Some("later")
        );
    }

    #[test]
    fn attention_is_globally_oldest_and_g_selects_an_unassigned_task() {
        let mut board = board();
        let mut waiting = session("waiting", "alice", "proj", SessionState::WaitingForInput);
        waiting.updated_at_ms = 20;
        waiting.state_since_ms = 20;
        let mut alice = agent("alice", "proj", AgentRole::Worker, None);
        alice.current_session_id = Some(SessionId::try_from("waiting").unwrap());
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![alice],
            vec![
                task("old-task", "proj", TaskStatus::Blocked, None, 10),
                task("new-task", "proj", TaskStatus::Failed, None, 30),
            ],
            Vec::new(),
            vec![waiting],
        );
        let items = board.attention_items();
        assert!(matches!(&items[0].target, AttentionTarget::Task(id) if id.as_str() == "old-task"));
        assert!(matches!(&items[1].target, AttentionTarget::Agent(id) if id.as_str() == "alice"));
        assert!(matches!(&items[2].target, AttentionTarget::Task(id) if id.as_str() == "new-task"));
        board.selected_agent = None;
        board.handle_key(key(KeyCode::Char('g')));
        assert_eq!(
            board.selected_task.as_ref().map(TaskId::as_str),
            Some("old-task")
        );
        assert_eq!(
            board.focused_project.as_ref().map(ProjectId::as_str),
            Some("proj")
        );
    }

    #[test]
    fn unattached_terminal_never_enters_typing_or_loses_keys() {
        let mut board = board();
        board.view = View::Agent;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        board.pane_ready = false;
        assert!(matches!(
            board.handle_key(key(KeyCode::Char('i'))),
            Intent::Redraw
        ));
        assert_eq!(board.pane_mode, PaneMode::Board);
        assert!(
            board
                .status
                .as_ref()
                .is_some_and(|status| status.text.contains("not attached"))
        );
        assert!(matches!(
            board.handle_key(key(KeyCode::Char('q'))),
            Intent::Quit
        ));
    }

    #[test]
    fn mouse_tabs_agents_and_attention_use_keyboard_state_transitions() {
        let mut keyboard = board();
        keyboard.selected_agent = Some(AgentId::try_from("alice").unwrap());
        keyboard.handle_key(key(KeyCode::Down));

        let mut mouse = board();
        mouse.selected_agent = Some(AgentId::try_from("alice").unwrap());
        mouse.handle_mouse_target(MouseTarget::Agent(AgentId::try_from("bob").unwrap()));
        assert_eq!(mouse.selected_agent, keyboard.selected_agent);
        assert_eq!(mouse.focused_project, keyboard.focused_project);
        assert_eq!(mouse.selected_task, keyboard.selected_task);

        keyboard.handle_key(key(KeyCode::Right));
        mouse.handle_mouse_target(MouseTarget::View(View::Agent));
        assert_eq!(mouse.view, keyboard.view);
        assert_eq!(mouse.pane_mode, keyboard.pane_mode);

        let blocked = task("blocked", "proj", TaskStatus::Blocked, None, 1);
        keyboard.apply_fleet_snapshot(
            vec![project("proj", 0)],
            Vec::new(),
            vec![blocked.clone()],
            Vec::new(),
            Vec::new(),
        );
        mouse.apply_fleet_snapshot(
            vec![project("proj", 0)],
            Vec::new(),
            vec![blocked],
            Vec::new(),
            Vec::new(),
        );
        keyboard.selected_agent = None;
        mouse.selected_agent = None;
        keyboard.handle_key(key(KeyCode::Char('g')));
        mouse.handle_mouse_target(MouseTarget::Attention(AttentionTarget::Task(
            TaskId::try_from("blocked").unwrap(),
        )));
        assert_eq!(mouse.selected_agent, keyboard.selected_agent);
        assert_eq!(mouse.selected_task, keyboard.selected_task);
        assert_eq!(mouse.focused_project, keyboard.focused_project);
        assert_eq!(mouse.view, keyboard.view);
    }

    #[test]
    fn footer_help_and_detach_use_the_keyboard_action_paths() {
        let mut keyboard = board();
        let mut mouse = board();
        assert!(matches!(
            mouse.handle_mouse_target(MouseTarget::Help),
            Intent::Redraw
        ));
        assert!(matches!(
            keyboard.handle_key(key(KeyCode::Char('?'))),
            Intent::Redraw
        ));
        assert!(matches!(mouse.mode, Mode::Help));
        assert!(matches!(keyboard.mode, Mode::Help));

        let mut keyboard = board();
        let mut mouse = board();
        assert!(matches!(
            mouse.handle_mouse_target(MouseTarget::Detach),
            Intent::Quit
        ));
        assert!(matches!(
            keyboard.handle_key(key(KeyCode::Char('q'))),
            Intent::Quit
        ));
        assert!(mouse.quit);
        assert_eq!(mouse.quit, keyboard.quit);
    }

    #[test]
    fn task_click_selects_the_exact_visible_task_for_the_existing_task_action() {
        let mut board = board();
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![agent("alice", "proj", AgentRole::Worker, None)],
            vec![
                task("first", "proj", TaskStatus::Queued, Some("alice"), 1),
                task("second", "proj", TaskStatus::Running, Some("alice"), 2),
            ],
            Vec::new(),
            Vec::new(),
        );
        board.view = View::Agent;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        board.handle_mouse_target(MouseTarget::Task(TaskId::try_from("second").unwrap()));
        board.handle_key(key(KeyCode::Char('t')));
        assert!(matches!(
            &board.mode,
            Mode::TaskMenu(TaskMenuState { task_id, .. }) if task_id.as_str() == "second"
        ));
    }

    #[test]
    fn stale_rows_and_unready_panes_fail_closed() {
        let mut board = board();
        let before = board.selected_agent.clone();
        assert!(matches!(
            board.handle_mouse_target(MouseTarget::Agent(AgentId::try_from("deleted").unwrap())),
            Intent::None
        ));
        assert_eq!(board.selected_agent, before);

        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        board.tasks.insert(
            TaskId::try_from("done").unwrap(),
            task("done", "proj", TaskStatus::Succeeded, Some("alice"), 0),
        );
        assert!(matches!(
            board.handle_mouse_target(MouseTarget::Task(TaskId::try_from("done").unwrap())),
            Intent::None
        ));
        assert!(board.selected_task.is_none());

        board.view = View::Agent;
        board.selected_agent = Some(AgentId::try_from("alice").unwrap());
        board.pane_mode = PaneMode::Typing;
        board.pane_ready = false;
        board.handle_mouse_target(MouseTarget::Pane(SessionId::try_from("session").unwrap()));
        assert_eq!(board.pane_mode, PaneMode::Board);

        board.mode = Mode::Help;
        assert!(matches!(
            board.handle_mouse_target(MouseTarget::View(View::Building)),
            Intent::None
        ));
        assert_eq!(board.view, View::Agent);
    }
}
