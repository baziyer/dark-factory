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
use factory_core::{AgentId, AgentRole, ExecutionMode, ProjectId, RunId, TaskId, TaskStatus};

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
    TogglePause,
    EditInstructions,
    EditMemory,
    ManageTask,
    EditModel,
    EditExecutionMode,
    EditCapacity,
    /// `u`: run the visible verified update once.
    Update,
}

/// The one keymap: every key the board recognizes in its base navigation mode, independent of
/// which view is active (`Board::dispatch` supplies the context). Returns `None` for anything not
/// bound — callers fall through to whatever else might want the key (a live pane, while
/// The active view supplies the context for each action.
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
        KeyCode::Char(' ') => Some(Action::TogglePause),
        KeyCode::Char('I') => Some(Action::EditInstructions),
        KeyCode::Char('M') => Some(Action::EditMemory),
        KeyCode::Char('t') => Some(Action::ManageTask),
        KeyCode::Char('v') => Some(Action::EditModel),
        KeyCode::Char('a') => Some(Action::EditExecutionMode),
        KeyCode::Char('C') => Some(Action::EditCapacity),
        KeyCode::Char('u') => Some(Action::Update),
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
    EditModel(AgentId),
    EditExecutionMode(AgentId),
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

    fn edit_profile(agent_id: AgentId, execution_mode: bool, current: String) -> Self {
        Self {
            kind: if execution_mode {
                PromptKind::EditExecutionMode(agent_id)
            } else {
                PromptKind::EditModel(agent_id)
            },
            labels: vec![if execution_mode {
                "execution mode"
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
            labels: vec!["active-run capacity"],
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
    ResetBudget {
        source: Box<AttentionItem>,
        request: LocalRequest,
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
    SendWithIdentity {
        operation_id: u64,
        request: LocalRequest,
    },
    SetCapacity(usize),
    Update,
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
            MouseTarget::Update => self.dispatch(Action::Update),
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
            Action::Detach if self.update_progress.is_some() => {
                self.note_error("update in progress; this viewer will relaunch when it is safe");
                Intent::Redraw
            }
            Action::Detach => {
                self.quit = true;
                Intent::Quit
            }
            Action::JumpNeedsAttention => self.jump_to_attention(),
            Action::ToggleHelp => {
                self.mode = Mode::Help;
                Intent::Redraw
            }
            Action::TogglePause => self.toggle_pause(),
            Action::EditInstructions => self.edit_guidance(true),
            Action::EditMemory => self.edit_guidance(false),
            Action::ManageTask => self.manage_task(),
            Action::EditModel => self.begin_profile_edit(false),
            Action::EditExecutionMode => self.begin_profile_edit(true),
            Action::EditCapacity => self.begin_capacity_edit(),
            Action::Update => {
                if self.update_available.is_some() && self.update_progress.is_none() {
                    self.update_progress =
                        Some(factoryctl::managed_update::UpdateProgress::Checking);
                    Intent::Update
                } else {
                    Intent::None
                }
            }
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
        Intent::Redraw
    }

    fn back_to_building(&mut self) -> Intent {
        self.view = View::Building;
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

    fn begin_profile_edit(&mut self, execution_mode: bool) -> Intent {
        let Some(agent_id) = self.selected_agent.clone() else {
            return Intent::None;
        };
        let Some(detail) = self.agent_details.get(&agent_id) else {
            self.set_status("agent settings are still loading", StatusLevel::Error);
            return Intent::Redraw;
        };
        let current = if execution_mode {
            detail.profile.execution_mode.to_string()
        } else {
            detail.profile.model.clone().unwrap_or_default()
        };
        self.mode = Mode::Prompt(PromptState::edit_profile(agent_id, execution_mode, current));
        Intent::Redraw
    }

    fn begin_capacity_edit(&mut self) -> Intent {
        let current = self
            .active_run_cap
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
        let Some(agent_id) = self.selected_agent.clone() else {
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
        let Some(agent_id) = self.selected_agent.clone() else {
            self.set_status("no agent selected", StatusLevel::Error);
            return Intent::Redraw;
        };
        let Some(agent) = self.agents.get(&agent_id) else {
            return Intent::Redraw;
        };
        let project_id = agent.project_id.clone();
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
                self.attention_request(&source, choice.action, request)
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
                self.attention_request(&source, choice.action, request)
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
                self.mode = Mode::Confirm(PendingAction::ResetBudget {
                    source: Box::new(source),
                    request,
                });
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
        let extra_confirm_key =
            matches!(action, PendingAction::StopRun { .. }) && key.code == KeyCode::Char('x');
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
            PendingAction::ResetBudget { source, request } => {
                let still_current = self
                    .decision_items()
                    .into_iter()
                    .any(|item| super::same_attention_source(&item, &source) && item == *source);
                if !still_current {
                    self.set_status(
                        "budget decision changed before confirmation",
                        StatusLevel::Info,
                    );
                    return Intent::Redraw;
                }
                if self.attention_is_pending(&source) {
                    self.set_status("decision request is still pending", StatusLevel::Info);
                    return Intent::Redraw;
                }
                self.attention_request(
                    &source,
                    factory_core::status::AttentionAction::ResetBudget,
                    request,
                )
            }
            PendingAction::StopRun { project_id, run_id } => {
                Intent::Send(LocalRequest::CancelRun {
                    project_id,
                    run_id,
                    grace_ms: 5_000,
                })
            }
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
                    execution_mode: detail.profile.execution_mode,
                    instructions: detail.profile.instructions.clone(),
                    memory: detail.profile.memory.clone(),
                })
            }
            PromptKind::EditExecutionMode(agent_id) => {
                let Some(detail) = self.agent_details.get(&agent_id) else {
                    return Intent::Redraw;
                };
                let execution_mode = prompt
                    .values
                    .first()
                    .and_then(|value| value.trim().parse::<ExecutionMode>().ok());
                let Some(execution_mode) = execution_mode else {
                    self.set_status(
                        "execution mode must be plan-only, workspace-write, or unrestricted",
                        StatusLevel::Error,
                    );
                    return Intent::Redraw;
                };
                Intent::Send(LocalRequest::UpdateAgentProfile {
                    project_id: detail.snapshot.project_id.clone(),
                    agent_id,
                    model: detail.profile.model.clone(),
                    reasoning_effort: detail.profile.reasoning_effort.clone(),
                    model_selection_reason: detail.profile.model_selection_reason.clone(),
                    execution_mode,
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
