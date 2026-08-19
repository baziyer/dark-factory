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
use factory_core::status::AttentionItem;
use factory_core::{AgentId, AgentRole, ProjectId, RunId, SessionId, TaskId, TaskStatus};

use super::{Board, StatusLevel};
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
    ReorderTask(TaskId),
    AttentionAnswer {
        project_id: ProjectId,
        session_id: SessionId,
    },
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

    #[must_use]
    fn reorder_task(task_id: TaskId, current: i32) -> Self {
        Self {
            kind: PromptKind::ReorderTask(task_id),
            labels: vec!["priority"],
            values: vec![current.to_string()],
            field: 0,
        }
    }

    #[must_use]
    fn attention_answer(project_id: ProjectId, session_id: SessionId) -> Self {
        Self {
            kind: PromptKind::AttentionAnswer {
                project_id,
                session_id,
            },
            labels: vec!["answer"],
            values: vec![String::new()],
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
    pub items: Vec<&'static str>,
}

fn task_menu_items(task: &factory_core::TaskDetail) -> Vec<&'static str> {
    let status = task.snapshot.status;
    let mut items = Vec::new();
    if status == TaskStatus::Queued && task.snapshot.assigned_agent_id.is_some() {
        items.push("start");
    }
    if status == TaskStatus::Queued {
        items.push("assign");
    }
    if status == TaskStatus::Queued && task.snapshot.assigned_agent_id.is_some() {
        items.push("backlog");
    }
    if status == TaskStatus::Queued {
        items.push("reorder");
    }
    if matches!(status, TaskStatus::Queued | TaskStatus::Blocked) {
        items.push("cancel");
    }
    if matches!(status, TaskStatus::Failed | TaskStatus::Cancelled) {
        items.push("retry");
    }
    if status != TaskStatus::Running {
        items.push("delete");
    }
    if status == TaskStatus::Queued {
        items.push("edit title");
    }
    items
}

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
                let Some(task) = self.tasks.get(&task_id).filter(|task| {
                    matches!(
                        task.snapshot.status,
                        TaskStatus::Queued
                            | TaskStatus::Running
                            | TaskStatus::Failed
                            | TaskStatus::Cancelled
                    )
                }) else {
                    return Intent::None;
                };
                self.focused_project = Some(task.snapshot.project_id.clone());
                self.selected_task = Some(task_id);
                self.attention_focus = None;
                self.pane_mode = PaneMode::Board;
                Intent::Redraw
            }
            MouseTarget::Attention(item) => {
                if self.select_attention_item(&item) {
                    Intent::Redraw
                } else {
                    Intent::None
                }
            }
            MouseTarget::AttentionChoice(item, index) => {
                let current = self
                    .decision_items()
                    .into_iter()
                    .find(|current| super::same_attention_source(current, &item));
                let Some(current) = current else {
                    self.set_status("that decision changed before the click", StatusLevel::Info);
                    return Intent::Redraw;
                };
                self.select_attention_item(&current);
                self.choose_attention(index)
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
        if self.view == View::Building
            && self
                .attention_focus
                .as_ref()
                .is_some_and(|focus| !focus.resolved)
        {
            if key.code == KeyCode::Enter {
                let recommended = self
                    .attention_focus
                    .as_ref()
                    .and_then(|focus| focus.item.decision().recommended);
                let Some(recommended) = recommended else {
                    self.set_status("choose an explicit decision with 1-9", StatusLevel::Info);
                    return Intent::Redraw;
                };
                return self.choose_attention(recommended);
            }
            if let KeyCode::Char(choice @ '1'..='9') = key.code {
                return self.choose_attention(usize::from(choice as u8 - b'1'));
            }
        }
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
        self.attention_focus = None;
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
        self.attention_focus = None;
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
            .filter(|selected| self.tasks.contains_key(*selected))
            .cloned()
            .or_else(|| tasks.first().map(|task| task.snapshot.id.clone()))
        else {
            self.set_status("no task selected", StatusLevel::Info);
            return Intent::Redraw;
        };
        let Some(task) = self.tasks.get(&task_id) else {
            return Intent::Redraw;
        };
        let items = task_menu_items(task);
        if items.is_empty() {
            self.set_status("task has no available actions", StatusLevel::Info);
            return Intent::Redraw;
        }
        self.mode = Mode::TaskMenu(TaskMenuState {
            task_id,
            cursor: 0,
            items,
        });
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
        let items = self.decision_items();
        if items.is_empty() {
            self.set_status("nothing needs attention", StatusLevel::Info);
            return Intent::Redraw;
        }
        let current = self.attention_focus.as_ref().and_then(|focus| {
            (!focus.resolved).then_some(()).and_then(|()| {
                items
                    .iter()
                    .position(|item| super::same_attention_source(item, &focus.item))
            })
        });
        let next = &items[current.map_or(0, |index| (index + 1) % items.len())];
        self.select_attention_item(next);
        Intent::Redraw
    }

    fn select_attention_item(&mut self, selected: &AttentionItem) -> bool {
        let current = self
            .decision_items()
            .into_iter()
            .find(|item| super::same_attention_source(item, selected));
        let (item, resolved) =
            current.map_or_else(|| (selected.clone(), true), |item| (item, false));
        self.focused_project = Some(item.project_id.clone());
        self.selected_task = item.task_id.clone();
        self.selected_agent = item.agent_id.clone().or_else(|| {
            item.task_id.as_ref().and_then(|task_id| {
                self.tasks
                    .get(task_id)
                    .and_then(|task| task.snapshot.assigned_agent_id.clone())
            })
        });
        self.attention_focus = Some(super::AttentionFocus {
            item: item.clone(),
            resolved,
        });
        if resolved {
            self.set_status("attention resolved before selection", StatusLevel::Info);
        } else {
            self.set_status(
                format!("attention: {}", item.reason.kind.label()),
                StatusLevel::Info,
            );
        }
        // NEEDS YOU is a BUILDING decision inbox. Keep the selected floor and
        // row visible while the right pane shows the bounded decision card.
        self.view = View::Building;
        self.pane_mode = PaneMode::Board;
        self.terminal_maximized = false;
        true
    }

    fn choose_attention(&mut self, index: usize) -> Intent {
        let Some(source) = self
            .attention_focus
            .as_ref()
            .filter(|focus| !focus.resolved)
            .map(|focus| focus.item.clone())
        else {
            return Intent::None;
        };
        if self.attention_is_pending(&source) {
            self.set_status("decision request is still pending", StatusLevel::Info);
            return Intent::Redraw;
        }
        let decision = source.decision();
        let Some(choice) = decision.choices.get(index) else {
            self.set_status("that decision choice is not available", StatusLevel::Error);
            return Intent::Redraw;
        };
        match choice.action {
            factory_core::status::AttentionAction::RetryTask => {
                let (Some(task_id), Some(project_id)) =
                    (source.task_id.clone(), Some(source.project_id.clone()))
                else {
                    self.set_status("retry choice has no exact task", StatusLevel::Error);
                    return Intent::Redraw;
                };
                let request = LocalRequest::RetryTask {
                    project_id,
                    task_id,
                };
                self.begin_attention_request(&source, choice.action);
                Intent::Send(request)
            }
            factory_core::status::AttentionAction::ResumeAgent => {
                let Some(agent_id) = source.agent_id.clone() else {
                    self.set_status("resume choice has no exact agent", StatusLevel::Error);
                    return Intent::Redraw;
                };
                let request = LocalRequest::ResumeAgent {
                    project_id: source.project_id.clone(),
                    agent_id,
                };
                self.begin_attention_request(&source, choice.action);
                Intent::Send(request)
            }
            factory_core::status::AttentionAction::ResetBudget => {
                let Some(agent_id) = source.agent_id.clone() else {
                    self.set_status("budget choice has no exact agent", StatusLevel::Error);
                    return Intent::Redraw;
                };
                let request = LocalRequest::ResetAgentBudget {
                    project_id: source.project_id.clone(),
                    agent_id,
                };
                self.begin_attention_request(&source, choice.action);
                Intent::Send(request)
            }
            factory_core::status::AttentionAction::AnswerInTerminal => {
                let Some(session_id) = source.session_id.clone() else {
                    self.set_status("provider choice has no exact session", StatusLevel::Error);
                    return Intent::Redraw;
                };
                self.mode = Mode::Prompt(PromptState::attention_answer(
                    source.project_id.clone(),
                    session_id,
                ));
                Intent::Redraw
            }
            factory_core::status::AttentionAction::ApproveProviderPermission
            | factory_core::status::AttentionAction::RejectProviderPermission => {
                let Some(session_id) = source.session_id.clone() else {
                    self.set_status("permission choice has no exact session", StatusLevel::Error);
                    return Intent::Redraw;
                };
                let bytes = if choice.action
                    == factory_core::status::AttentionAction::ApproveProviderPermission
                {
                    b"y\n".as_slice()
                } else {
                    b"n\n".as_slice()
                };
                let request = LocalRequest::TerminalInput {
                    project_id: source.project_id.clone(),
                    session_id,
                    bytes: factory_core::runner::encode_terminal_bytes(bytes),
                };
                self.begin_attention_request(&source, choice.action);
                Intent::Send(request)
            }
            factory_core::status::AttentionAction::ReviewProviderPermission => {
                self.set_status("choose approve or reject", StatusLevel::Info);
                Intent::Redraw
            }
            _ => {
                self.set_status(
                    "this decision has no client-side mutation",
                    StatusLevel::Info,
                );
                Intent::Redraw
            }
        }
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
                let request = LocalRequest::CreateTask {
                    id,
                    project_id,
                    parent_task_id: None,
                    title,
                    body,
                    priority: 0,
                    agent_id,
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
                    priority: None,
                })
            }
            PromptKind::ReorderTask(task_id) => {
                let Some(project_id) = self.task_project(&task_id) else {
                    return Intent::Redraw;
                };
                let Some(value) = prompt.values.first() else {
                    return Intent::Redraw;
                };
                let Ok(priority) = value.trim().parse::<i32>() else {
                    self.set_status("priority must be a signed integer", StatusLevel::Error);
                    return Intent::Redraw;
                };
                Intent::Send(LocalRequest::UpdateTask {
                    project_id,
                    task_id,
                    title: None,
                    body: None,
                    priority: Some(priority),
                })
            }
            PromptKind::AttentionAnswer {
                project_id,
                session_id,
            } => {
                let answer = prompt
                    .values
                    .first()
                    .map(String::as_str)
                    .map(str::trim)
                    .unwrap_or_default();
                if answer.is_empty() {
                    self.set_status("answer can't be empty", StatusLevel::Error);
                    return Intent::Redraw;
                }
                let mut bytes = answer.as_bytes().to_vec();
                bytes.push(b'\n');
                if let Some(source) = self
                    .attention_focus
                    .as_ref()
                    .map(|focus| focus.item.clone())
                    .filter(|source| {
                        source.project_id == project_id
                            && source.session_id.as_ref() == Some(&session_id)
                    })
                {
                    self.begin_attention_request(
                        &source,
                        factory_core::status::AttentionAction::AnswerInTerminal,
                    );
                }
                Intent::Send(LocalRequest::TerminalInput {
                    project_id,
                    session_id,
                    bytes: factory_core::runner::encode_terminal_bytes(&bytes),
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
                let model_selection_reason = (value == detail.profile.model)
                    .then(|| detail.profile.model_selection_reason.clone())
                    .flatten();
                Intent::Send(LocalRequest::UpdateAgentProfile {
                    project_id: detail.snapshot.project_id.clone(),
                    agent_id,
                    model: value,
                    reasoning_effort: detail.profile.reasoning_effort.clone(),
                    model_selection_reason,
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
                    reasoning_effort: detail.profile.reasoning_effort.clone(),
                    model_selection_reason: detail.profile.model_selection_reason.clone(),
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
                state.cursor = (state.cursor + 1) % state.items.len();
                Intent::Redraw
            }
            KeyCode::Char('k') | KeyCode::Up => {
                state.cursor = (state.cursor + state.items.len() - 1) % state.items.len();
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
        match state.items[state.cursor] {
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
            "reorder" => {
                let current = self
                    .tasks
                    .get(&task_id)
                    .map_or(0, |task| task.snapshot.priority);
                self.mode = Mode::Prompt(PromptState::reorder_task(task_id, current));
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
    use crate::test_fixtures::{agent, attention, project, session, task};
    use factory_core::{
        AgentRole, ObserverHealth, ProviderHookEvent, ProviderNotificationKind, SessionState,
        TaskStatus, status::AttentionReasonKind,
    };

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
    fn g_selects_the_first_shared_attention_item_and_opens_its_action_card() {
        let mut board = board();
        board.attention = vec![
            attention(
                AttentionReasonKind::WorkerBlocked,
                None,
                Some("old-task"),
                None,
                10,
            ),
            attention(
                AttentionReasonKind::ProviderQuestion,
                Some("alice"),
                None,
                Some("session"),
                20,
            ),
        ];
        let items = board.attention_items();
        assert_eq!(items[0].reason.kind, AttentionReasonKind::WorkerBlocked);
        assert_eq!(items[1].reason.kind, AttentionReasonKind::ProviderQuestion);
        board.selected_agent = None;
        board.terminal_maximized = true;
        board.handle_key(key(KeyCode::Char('g')));
        assert_eq!(
            board.selected_task.as_ref().map(TaskId::as_str),
            Some("old-task")
        );
        assert_eq!(
            board.focused_project.as_ref().map(ProjectId::as_str),
            Some("proj")
        );
        assert_eq!(board.view, View::Building);
        assert_eq!(board.pane_mode, PaneMode::Board);
        assert!(!board.terminal_maximized);
        assert!(board.attention_focus.as_ref().is_some_and(|focus| {
            !focus.resolved && focus.item.reason.kind == AttentionReasonKind::WorkerBlocked
        }));
    }

    #[test]
    fn building_decision_enter_sends_the_typed_recommended_retry_without_opening_agent() {
        let mut board = board();
        let item = attention(
            AttentionReasonKind::WorkerBlocked,
            Some("alice"),
            Some("blocked"),
            None,
            10,
        );
        board.attention = vec![item];
        assert!(matches!(
            board.handle_key(key(KeyCode::Char('g'))),
            Intent::Redraw
        ));
        let Intent::Send(LocalRequest::RetryTask {
            project_id,
            task_id,
        }) = board.handle_key(key(KeyCode::Enter))
        else {
            panic!("recommended retry was not a typed local request");
        };
        assert_eq!(project_id.as_str(), "proj");
        assert_eq!(task_id.as_str(), "blocked");
        assert_eq!(board.view, View::Building);
    }

    #[test]
    fn permission_decision_requires_explicit_choice_and_waits_for_authoritative_projection() {
        let mut board = board();
        let item = attention(
            AttentionReasonKind::ProviderPermission,
            Some("alice"),
            None,
            Some("session"),
            10,
        );
        board.attention = vec![item.clone()];
        board.handle_key(key(KeyCode::Char('g')));
        assert!(matches!(
            board.handle_key(key(KeyCode::Enter)),
            Intent::Redraw
        ));
        assert!(board.status.as_ref().is_some_and(|status| {
            status.text.contains("explicit decision")
        }));
        assert!(matches!(
            board.handle_key(key(KeyCode::Char('1'))),
            Intent::Send(LocalRequest::TerminalInput { .. })
        ));
        board.apply_response(Ok(LocalResponse::TerminalInputAccepted {
            session_id: SessionId::try_from("session").unwrap(),
        }));
        assert_eq!(board.decision_items().len(), 1);
        assert!(matches!(
            board.handle_key(key(KeyCode::Char('2'))),
            Intent::Redraw
        ));

        let mut changed = item;
        changed.reason.summary = "approve a different command".to_owned();
        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 11,
            event_sequence: 11,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: vec![changed],
        });
        assert!(matches!(
            board.handle_key(key(KeyCode::Char('2'))),
            Intent::Send(LocalRequest::TerminalInput { .. })
        ));
    }

    #[test]
    fn provider_decision_uses_a_typed_local_answer_request_without_terminal_detour() {
        let mut board = board();
        board.attention = vec![attention(
            AttentionReasonKind::ProviderQuestion,
            Some("alice"),
            None,
            Some("session"),
            10,
        )];
        board.handle_key(key(KeyCode::Char('g')));
        assert!(matches!(
            board.handle_key(key(KeyCode::Enter)),
            Intent::Redraw
        ));
        assert!(matches!(board.mode, Mode::Prompt(_)));
        for character in "use the stable branch".chars() {
            board.handle_key(key(KeyCode::Char(character)));
        }
        let Intent::Send(LocalRequest::TerminalInput {
            project_id,
            session_id,
            bytes,
        }) = board.handle_key(key(KeyCode::Enter))
        else {
            panic!("provider decision did not become a typed local request");
        };
        assert_eq!(project_id.as_str(), "proj");
        assert_eq!(session_id.as_str(), "session");
        assert_eq!(
            factory_core::runner::decode_terminal_bytes(&bytes).unwrap(),
            b"use the stable branch\n"
        );
        assert_eq!(board.view, View::Building);
    }

    #[test]
    fn manual_pause_decision_emits_resume_request_only_after_explicit_choice() {
        let mut board = board();
        board.attention = vec![attention(
            AttentionReasonKind::PausedWithWork,
            Some("alice"),
            Some("queued"),
            None,
            10,
        )];
        board.handle_key(key(KeyCode::Char('g')));
        assert!(
            matches!(board.handle_key(key(KeyCode::Char('1'))), Intent::Send(LocalRequest::ResumeAgent { project_id, agent_id }) if project_id.as_str() == "proj" && agent_id.as_str() == "alice")
        );
        assert_eq!(board.view, View::Building);
    }

    #[test]
    fn mouse_choice_uses_the_same_typed_path_as_keyboard_choice() {
        let mut board = board();
        let item = attention(
            AttentionReasonKind::WorkerBlocked,
            Some("alice"),
            Some("blocked"),
            None,
            10,
        );
        board.attention = vec![item.clone()];
        let Intent::Send(LocalRequest::RetryTask { task_id, .. }) =
            board.handle_mouse_target(MouseTarget::AttentionChoice(item, 0))
        else {
            panic!("mouse choice did not use the typed retry request");
        };
        assert_eq!(task_id.as_str(), "blocked");
        assert_eq!(board.view, View::Building);
    }

    #[test]
    fn machine_recovery_does_not_enter_the_building_decision_inbox() {
        let mut board = board();
        board.attention = vec![attention(
            AttentionReasonKind::DeliveryRecovery,
            Some("alice"),
            None,
            Some("session"),
            10,
        )];
        assert!(board.decision_items().is_empty());
        assert!(matches!(
            board.handle_key(key(KeyCode::Char('g'))),
            Intent::Redraw
        ));
        assert!(board.attention_focus.is_none());
    }

    #[test]
    fn ordinary_keyboard_and_mouse_navigation_clear_the_bound_attention_card() {
        let item = attention(
            AttentionReasonKind::ProviderQuestion,
            Some("alice"),
            None,
            Some("session"),
            10,
        );

        let mut keyboard = board();
        keyboard.attention = vec![item.clone()];
        keyboard.handle_key(key(KeyCode::Char('g')));
        assert!(keyboard.attention_focus.is_some());
        keyboard.handle_key(key(KeyCode::Char(']')));
        assert_eq!(
            keyboard.selected_agent.as_ref().map(AgentId::as_str),
            Some("bob")
        );
        assert!(keyboard.attention_focus.is_none());

        let mut mouse = board();
        mouse.attention = vec![item];
        mouse.handle_key(key(KeyCode::Char('g')));
        assert!(mouse.attention_focus.is_some());
        mouse.handle_mouse_target(MouseTarget::Agent(AgentId::try_from("bob").unwrap()));
        assert_eq!(
            mouse.selected_agent.as_ref().map(AgentId::as_str),
            Some("bob")
        );
        assert!(mouse.attention_focus.is_none());
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

        let item = attention(
            AttentionReasonKind::ProviderQuestion,
            Some("alice"),
            None,
            Some("session"),
            1,
        );
        keyboard.attention = vec![item.clone()];
        mouse.attention = vec![item.clone()];
        keyboard.selected_agent = None;
        mouse.selected_agent = None;
        keyboard.handle_key(key(KeyCode::Char('g')));
        mouse.handle_mouse_target(MouseTarget::Attention(item));
        assert_eq!(mouse.selected_agent, keyboard.selected_agent);
        assert_eq!(mouse.selected_task, keyboard.selected_task);
        assert_eq!(mouse.focused_project, keyboard.focused_project);
        assert_eq!(mouse.view, keyboard.view);
        assert_eq!(mouse.pane_mode, keyboard.pane_mode);
        assert_eq!(mouse.attention_focus, keyboard.attention_focus);
    }

    #[test]
    fn concurrent_resolution_marks_the_card_resolved_and_stale_click_explains_itself() {
        let mut board = board();
        let item = attention(
            AttentionReasonKind::ProviderPermission,
            Some("alice"),
            None,
            Some("session"),
            1,
        );
        board.attention = vec![item.clone()];
        board.handle_key(key(KeyCode::Char('g')));
        board.apply_event(factory_core::EventEnvelope {
            protocol_version: 1,
            sequence: 1,
            occurred_at_ms: 2,
            event: factory_core::FactoryEvent::SessionChanged {
                session: session("session", "alice", "proj", SessionState::Working),
            },
        });
        assert!(board.attention_items().is_empty());
        assert!(
            board
                .attention_focus
                .as_ref()
                .is_some_and(|focus| focus.resolved)
        );
        assert!(
            board
                .status
                .as_ref()
                .is_some_and(|status| status.text.contains("resolved"))
        );

        board.attention_focus = None;
        board.handle_mouse_target(MouseTarget::Attention(item));
        assert_eq!(board.view, View::Building);
        assert_eq!(board.pane_mode, PaneMode::Board);
        assert!(
            board
                .attention_focus
                .as_ref()
                .is_some_and(|focus| focus.resolved)
        );
    }

    #[test]
    fn delayed_fleet_status_cannot_resurrect_attention_after_a_newer_event() {
        let mut board = board();
        let item = attention(
            AttentionReasonKind::ProviderPermission,
            Some("alice"),
            None,
            Some("session"),
            1,
        );
        board.attention = vec![item.clone()];
        board.apply_event(factory_core::EventEnvelope {
            protocol_version: 1,
            sequence: 12,
            occurred_at_ms: 2,
            event: factory_core::FactoryEvent::SessionChanged {
                session: session("session", "alice", "proj", SessionState::Working),
            },
        });
        assert!(board.attention.is_empty());
        board.apply_event(factory_core::EventEnvelope {
            protocol_version: 1,
            sequence: 11,
            occurred_at_ms: 1,
            event: factory_core::FactoryEvent::SessionChanged {
                session: session("session", "alice", "proj", SessionState::WaitingForInput),
            },
        });
        assert_eq!(
            board
                .sessions
                .get(&SessionId::try_from("session").unwrap())
                .map(|session| session.state),
            Some(SessionState::Working)
        );

        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 3,
            event_sequence: 11,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: vec![item],
        });
        assert!(board.attention.is_empty());

        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 4,
            event_sequence: 13,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: Vec::new(),
        });
        assert!(board.attention.is_empty());
    }

    #[test]
    fn fleet_status_then_same_sequence_event_still_folds_state() {
        let mut board = board();
        let item = attention(
            AttentionReasonKind::ProviderPermission,
            Some("alice"),
            None,
            Some("session"),
            1,
        );
        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 2,
            event_sequence: 12,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: vec![item],
        });
        board.apply_event(factory_core::EventEnvelope {
            protocol_version: 1,
            sequence: 12,
            occurred_at_ms: 3,
            event: factory_core::FactoryEvent::SessionChanged {
                session: {
                    let mut session =
                        session("session", "alice", "proj", SessionState::WaitingForInput);
                    session.last_hook_event =
                        Some(factory_core::ProviderHookEvent::PermissionRequest);
                    session.wait_reason = Some("approve command".into());
                    session
                },
            },
        });
        assert_eq!(
            board
                .sessions
                .get(&SessionId::try_from("session").unwrap())
                .map(|session| session.state),
            Some(SessionState::WaitingForInput)
        );
        assert_eq!(board.attention_items().len(), 1);
    }

    #[test]
    fn same_sequence_event_then_fleet_status_restores_one_actionable_row() {
        let mut board = board();
        board.apply_event(factory_core::EventEnvelope {
            protocol_version: 1,
            sequence: 12,
            occurred_at_ms: 2,
            event: factory_core::FactoryEvent::SessionChanged {
                session: {
                    let mut session =
                        session("session", "alice", "proj", SessionState::WaitingForInput);
                    session.last_hook_event =
                        Some(factory_core::ProviderHookEvent::PermissionRequest);
                    session.wait_reason = Some("approve command".into());
                    session
                },
            },
        });
        assert_eq!(
            board.sessions[&SessionId::try_from("session").unwrap()].state,
            SessionState::WaitingForInput
        );
        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 3,
            event_sequence: 12,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: vec![attention(
                AttentionReasonKind::ProviderPermission,
                Some("alice"),
                None,
                Some("session"),
                1,
            )],
        });
        assert_eq!(board.attention_items().len(), 1);
        assert_eq!(
            board.attention_items()[0].reason.kind,
            AttentionReasonKind::ProviderPermission
        );
    }

    #[test]
    fn same_sequence_status_and_event_preserve_every_structured_session_reason() {
        let cases = [
            AttentionReasonKind::ProviderPermission,
            AttentionReasonKind::ProviderQuestion,
            AttentionReasonKind::ObserverProblem,
            AttentionReasonKind::DeliveryRecovery,
        ];

        for kind in cases {
            let item = attention(kind, Some("alice"), None, Some("session"), 1);
            let snapshot = session_for_attention(kind);

            let mut status_first = board();
            status_first.apply_fleet_status(factory_core::status::FleetStatus {
                generated_at_ms: 2,
                event_sequence: 12,
                auto_mode: true,
                live_session_cap: 4,
                live_sessions: 1,
                projects: Vec::new(),
                attention: vec![item.clone()],
            });
            status_first.apply_event(factory_core::EventEnvelope {
                protocol_version: 1,
                sequence: 12,
                occurred_at_ms: 3,
                event: factory_core::FactoryEvent::SessionChanged {
                    session: snapshot.clone(),
                },
            });
            assert_eq!(
                status_first
                    .attention_items()
                    .iter()
                    .map(|item| item.reason.kind)
                    .collect::<Vec<_>>(),
                vec![kind],
                "status-first lost {kind:?}"
            );

            let mut event_first = board();
            event_first.apply_event(factory_core::EventEnvelope {
                protocol_version: 1,
                sequence: 12,
                occurred_at_ms: 2,
                event: factory_core::FactoryEvent::SessionChanged { session: snapshot },
            });
            event_first.apply_fleet_status(factory_core::status::FleetStatus {
                generated_at_ms: 3,
                event_sequence: 12,
                auto_mode: true,
                live_session_cap: 4,
                live_sessions: 1,
                projects: Vec::new(),
                attention: vec![item],
            });
            assert_eq!(
                event_first
                    .attention_items()
                    .iter()
                    .map(|item| item.reason.kind)
                    .collect::<Vec<_>>(),
                vec![kind],
                "event-first lost {kind:?}"
            );
        }
    }

    fn session_for_attention(kind: AttentionReasonKind) -> factory_core::SessionSnapshot {
        let mut snapshot = session("session", "alice", "proj", SessionState::WaitingForInput);
        match kind {
            AttentionReasonKind::ProviderPermission => {
                snapshot.last_hook_event = Some(ProviderHookEvent::PermissionRequest);
                snapshot.wait_reason = Some("approve command".into());
            }
            AttentionReasonKind::ProviderQuestion => {
                snapshot.last_hook_event = Some(ProviderHookEvent::Notification);
                snapshot.notification_kind = Some(ProviderNotificationKind::ElicitationDialog);
                snapshot.wait_reason = Some("Which branch should I use?".into());
            }
            AttentionReasonKind::ObserverProblem => {
                snapshot.state = SessionState::Working;
                snapshot.observer_health = ObserverHealth::Degraded;
                snapshot.observer_reason = Some("runner disconnected".into());
            }
            AttentionReasonKind::DeliveryRecovery => {
                snapshot.wait_reason = Some("delivery unacknowledged".into());
            }
            _ => unreachable!("only session-owned structured reasons belong here"),
        }
        snapshot
    }

    #[test]
    fn routine_notification_projection_has_no_tui_action_card() {
        let mut board = board();
        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 2,
            event_sequence: -1,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: Vec::new(),
        });
        assert!(board.attention_items().is_empty());
        board.handle_key(key(KeyCode::Char('g')));
        assert!(board.attention_focus.is_none());
    }

    #[test]
    fn bootstrap_high_water_rejects_an_older_delayed_status_after_replay() {
        let mut board = board();
        let stale = attention(
            AttentionReasonKind::ProviderPermission,
            Some("alice"),
            None,
            Some("session"),
            1,
        );
        board.note_fleet_snapshot_sequence(100);
        board.apply_replay(vec![factory_core::EventEnvelope {
            protocol_version: 1,
            sequence: 100,
            occurred_at_ms: 2,
            event: factory_core::FactoryEvent::SessionChanged {
                session: session("session", "alice", "proj", SessionState::Working),
            },
        }]);
        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 3,
            event_sequence: 90,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: vec![stale],
        });
        assert!(board.attention_items().is_empty());
    }

    #[test]
    fn legacy_unsequenced_status_remains_live_after_an_event() {
        let mut board = board();
        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 1,
            event_sequence: -1,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: Vec::new(),
        });
        board.apply_event(factory_core::EventEnvelope {
            protocol_version: 1,
            sequence: 12,
            occurred_at_ms: 2,
            event: factory_core::FactoryEvent::SessionChanged {
                session: session("session", "alice", "proj", SessionState::Working),
            },
        });
        let fresh = attention(
            AttentionReasonKind::ProviderQuestion,
            Some("alice"),
            None,
            Some("session"),
            3,
        );
        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 3,
            event_sequence: -1,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: vec![fresh.clone()],
        });
        assert_eq!(board.attention_items(), vec![fresh]);
    }

    #[test]
    fn client_local_attach_failure_uses_action_focus_and_clears_on_recovery() {
        let mut board = board();
        let session_id = SessionId::try_from("session").unwrap();
        board.attention = vec![attention(
            AttentionReasonKind::ProviderQuestion,
            Some("alice"),
            None,
            Some("session"),
            99,
        )];
        board.handle_key(key(KeyCode::Char('g')));
        assert_eq!(
            board.attention_focus.as_ref().unwrap().item.reason.action,
            factory_core::status::AttentionAction::AnswerInTerminal
        );
        board.note_local_attach_failure(&session_id, "socket refused\n\u{1b}[2J");
        assert!(
            board
                .attention_focus
                .as_ref()
                .is_some_and(|focus| focus.resolved),
            "the previously focused provider action must become stale immediately"
        );
        let items = board.attention_items();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.reason.kind, AttentionReasonKind::ObserverProblem);
        assert!(
            item.reason
                .summary
                .starts_with("local terminal attach failed: ")
        );
        assert!(!item.reason.summary.contains('\n'));
        assert!(!item.reason.summary.contains('\u{1b}'));

        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 1,
            event_sequence: 1,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: Vec::new(),
        });
        assert_eq!(
            board.attention_items()[0].reason.kind,
            AttentionReasonKind::ObserverProblem
        );

        let mut daemon_observer = attention(
            AttentionReasonKind::ObserverProblem,
            Some("alice"),
            None,
            Some("session"),
            2,
        );
        daemon_observer.reason.summary = "daemon terminal attach failed".to_owned();
        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 2,
            event_sequence: 2,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: vec![daemon_observer],
        });
        let items = board.attention_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].reason.summary, "daemon terminal attach failed");

        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 3,
            event_sequence: 3,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: Vec::new(),
        });
        assert!(
            board.attention_items()[0]
                .reason
                .summary
                .starts_with("local terminal attach failed: ")
        );
        assert!(board.decision_items().is_empty());

        board.handle_key(key(KeyCode::Char('g')));
        assert!(
            board
                .attention_focus
                .as_ref()
                .is_some_and(|focus| focus.resolved)
        );
        assert_eq!(board.pane_mode, PaneMode::Board);

        board.clear_local_attach_failure(&session_id);
        assert!(board.attention_items().is_empty());
        assert!(board.attention_focus.is_some_and(|focus| focus.resolved));
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
        board.attention_focus = Some(crate::model::AttentionFocus {
            item: attention(
                AttentionReasonKind::ProviderQuestion,
                Some("alice"),
                None,
                Some("session"),
                1,
            ),
            resolved: false,
        });
        board.handle_mouse_target(MouseTarget::Task(TaskId::try_from("first").unwrap()));
        assert!(board.attention_focus.is_none());
        board.handle_key(key(KeyCode::Char('t')));
        assert!(matches!(
            &board.mode,
            Mode::TaskMenu(TaskMenuState { task_id, .. }) if task_id.as_str() == "first"
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

        board.tasks.insert(
            TaskId::try_from("retryable").unwrap(),
            task("retryable", "proj", TaskStatus::Failed, Some("alice"), 0),
        );
        assert!(matches!(
            board.handle_mouse_target(MouseTarget::Task(TaskId::try_from("retryable").unwrap())),
            Intent::Redraw
        ));
        assert!(matches!(
            board.handle_key(key(KeyCode::Char('t'))),
            Intent::Redraw
        ));
        assert!(matches!(
            &board.mode,
            Mode::TaskMenu(TaskMenuState { items, .. }) if items.contains(&"retry")
        ));
        board.mode = Mode::Normal;

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
