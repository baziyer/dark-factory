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

// ---------------------------------------------------------------------------------------------
// Action + keymap
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    SwitchView(View),
    /// `Enter`/`→`/`l`.
    ZoomIn,
    /// `Esc`/`←`/`h`.
    ZoomOut,
    /// `Tab`: cycles agents (FORTRESS/TERMINALS/FOCUS) or panes (WORKSHOP).
    Tab,
    /// `j`/`↓`.
    MoveNext,
    /// `k`/`↑`.
    MovePrev,
    NewTask,
    MessageAgent,
    MessageOrchestrator,
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
/// bound — callers fall through to whatever else might want the key (a live pane, when
/// `pane_forwarding` is on, is handled a level up in `Board::handle_normal_key`).
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
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => Some(Action::ZoomIn),
        KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => Some(Action::ZoomOut),
        KeyCode::Tab => Some(Action::Tab),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::MoveNext),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::MovePrev),
        KeyCode::Char('n') => Some(Action::NewTask),
        KeyCode::Char('m') => Some(Action::MessageAgent),
        KeyCode::Char('o') => Some(Action::MessageOrchestrator),
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
    /// Only meaningful while a pane is attached (TERMINALS/FOCUS, `pane_forwarding` on);
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
                self.pane_forwarding = !self.pane_forwarding;
                return Intent::Redraw;
            }
            if self.pane_forwarding {
                return Intent::ForwardKey(key);
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
            Action::Tab => self.handle_tab(),
            Action::MoveNext => self.handle_move(true),
            Action::MovePrev => self.handle_move(false),
            Action::NewTask => self.begin_new_task(),
            Action::MessageAgent => self.begin_message_agent(),
            Action::MessageOrchestrator => self.begin_message_orchestrator(),
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
        if matches!(view, View::Terminals | View::Focus) {
            self.pane_forwarding = true;
        }
        Intent::Redraw
    }

    fn zoom_in(&mut self) -> Intent {
        match self.view {
            View::Fortress => {
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
                if self.terminal_targets().is_empty() {
                    self.set_status("no live sessions to zoom into", StatusLevel::Error);
                    return Intent::Redraw;
                }
                self.view = View::Focus;
                self.pane_forwarding = true;
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
                if self.selected_agent.is_none() {
                    self.set_status("no agent selected", StatusLevel::Error);
                    return Intent::Redraw;
                }
                self.view = View::Terminals;
                self.pane_forwarding = true;
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

    fn begin_stop_selected(&mut self) -> Intent {
        let Some(agent_id) = self.selected_agent.clone() else {
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
        if also_focus {
            self.view = View::Focus;
            self.pane_forwarding = true;
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
    use factory_core::{
        AgentRole, AgentSnapshot, ObserverHealth, ProjectSnapshot, Provider, RunSnapshot,
        RunStatus, TaskDetail, TaskSnapshot, TaskStatus,
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn project(id: &str, created_at_ms: i64) -> ProjectSnapshot {
        ProjectSnapshot {
            id: ProjectId::try_from(id).unwrap(),
            name: id.to_owned(),
            root: "/work".into(),
            created_at_ms,
            updated_at_ms: created_at_ms,
        }
    }

    fn agent(id: &str, project: &str, role: AgentRole, parent: Option<&str>) -> AgentSnapshot {
        AgentSnapshot {
            id: AgentId::try_from(id).unwrap(),
            project_id: ProjectId::try_from(project).unwrap(),
            parent_agent_id: parent.map(|p| AgentId::try_from(p).unwrap()),
            role,
            provider: Provider::ClaudeCode,
            current_run_id: None,
            paused: false,
            current_session_id: None,
            worktree: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn task(
        id: &str,
        project: &str,
        status: TaskStatus,
        assigned: Option<&str>,
        created_at_ms: i64,
    ) -> TaskDetail {
        TaskDetail {
            snapshot: TaskSnapshot {
                id: TaskId::try_from(id).unwrap(),
                project_id: ProjectId::try_from(project).unwrap(),
                parent_task_id: None,
                assigned_agent_id: assigned.map(|a| AgentId::try_from(a).unwrap()),
                title: id.to_owned(),
                status,
                priority: 0,
                created_at_ms,
                updated_at_ms: created_at_ms,
            },
            body: String::new(),
            result: None,
            blocked_reason: None,
        }
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
            (KeyCode::Right, Action::ZoomIn),
            (KeyCode::Char('l'), Action::ZoomIn),
            (KeyCode::Esc, Action::ZoomOut),
            (KeyCode::Left, Action::ZoomOut),
            (KeyCode::Char('h'), Action::ZoomOut),
            (KeyCode::Tab, Action::Tab),
            (KeyCode::Char('j'), Action::MoveNext),
            (KeyCode::Down, Action::MoveNext),
            (KeyCode::Char('k'), Action::MovePrev),
            (KeyCode::Up, Action::MovePrev),
            (KeyCode::Char('n'), Action::NewTask),
            (KeyCode::Char('m'), Action::MessageAgent),
            (KeyCode::Char('o'), Action::MessageOrchestrator),
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
        board.handle_key(key(KeyCode::Char('k')));
        assert_eq!(board.selected_agent.as_ref().unwrap().as_str(), "orch");
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

    #[test]
    fn g_jumps_to_next_agent_needing_attention() {
        let mut board = board_with_one_project();
        board.runs.insert(
            RunId::try_from("run-1").unwrap(),
            RunSnapshot {
                id: RunId::try_from("run-1").unwrap(),
                project_id: ProjectId::try_from("proj").unwrap(),
                agent_id: AgentId::try_from("alice").unwrap(),
                parent_run_id: None,
                task_id: None,
                session_id: None,
                closed_by: None,
                status: RunStatus::Blocked,
                activity: None,
                wait_reason: None,
                worktree: "/work".into(),
                observer_health: ObserverHealth::Unknown,
                observer_health_since_ms: 0,
                started_at_ms: 0,
                status_since_ms: 0,
                updated_at_ms: 0,
                ended_at_ms: None,
                exit_code: None,
                exit_signal: None,
                failure_reason: None,
            },
        );
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
        board.runs.insert(
            RunId::try_from("run-1").unwrap(),
            RunSnapshot {
                id: RunId::try_from("run-1").unwrap(),
                project_id: ProjectId::try_from("proj").unwrap(),
                agent_id: AgentId::try_from("alice").unwrap(),
                parent_run_id: None,
                task_id: None,
                session_id: None,
                closed_by: None,
                status: RunStatus::Failed,
                activity: None,
                wait_reason: None,
                worktree: "/work".into(),
                observer_health: ObserverHealth::Unknown,
                observer_health_since_ms: 0,
                started_at_ms: 0,
                status_since_ms: 0,
                updated_at_ms: 0,
                ended_at_ms: None,
                exit_code: None,
                exit_signal: None,
                failure_reason: None,
            },
        );
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

    #[test]
    fn terminals_forwards_keys_by_default() {
        let mut board = board_in_workshop();
        board.view = View::Terminals;
        let intent = board.handle_key(key(KeyCode::Char('a')));
        assert!(matches!(intent, Intent::ForwardKey(_)));
    }

    #[test]
    fn prefix_key_toggles_forwarding_in_terminals() {
        let mut board = board_in_workshop();
        board.view = View::Terminals;
        assert!(board.pane_forwarding);
        let intent = board.handle_key(crate::keys::PREFIX_KEY);
        assert!(matches!(intent, Intent::Redraw));
        assert!(!board.pane_forwarding);
        // Now in control mode, normal keymap applies.
        let intent = board.handle_key(key(KeyCode::Char('1')));
        assert!(matches!(intent, Intent::Redraw));
        assert_eq!(board.view, View::Fortress);
    }

    #[test]
    fn focus_scroll_actions_only_fire_in_focus_view() {
        let mut board = board_in_workshop();
        board.view = View::Terminals;
        board.pane_forwarding = false;
        let intent = board.handle_key(key(KeyCode::PageUp));
        assert!(matches!(intent, Intent::None));

        board.view = View::Focus;
        let intent = board.handle_key(key(KeyCode::PageUp));
        assert!(matches!(intent, Intent::ScrollFocus { up: true }));
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
