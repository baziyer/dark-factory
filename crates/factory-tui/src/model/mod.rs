//! The operator board's view-model: pure data + pure key-handling, no I/O.
//!
//! Everything in this module (and its submodules) is deliberately free of sockets, threads, and
//! PTYs so it can be unit tested directly. `net.rs` feeds it fleet snapshots/events; `main.rs`'s
//! event loop feeds it `crossterm` key/paste events and applies the [`keymap::Intent`]s it
//! returns (sending requests, attaching/detaching terminal panes, quitting).
//!
//! ## Multi-project scope (Track 6c)
//!
//! Unlike the pre-Track-6c board (which loaded one project at a time), `Board` holds **every**
//! project's agents/tasks/runs/sessions at once — FORTRESS is a fleet-wide floor plan of every
//! project as a "workshop", per the design brief, so the data underneath it has to be fleet-wide
//! too. `focused_project` narrows WORKSHOP/TERMINALS/FOCUS to one project at a time; FORTRESS
//! ignores it entirely.
//!
//! ## Deriving agent state and attention
//!
//! [`Board::agent_state`] and [`Board::agent_attention`] are the two functions everything else in
//! this crate goes through instead of inspecting [`SessionState`]/[`RunStatus`] itself: "session
//! state wins over run-status inference when a session exists" (design brief §5). Both return a
//! [`attention::Rated`] value so callers can show the `~` prefix the brief asks for whenever the
//! answer was inferred from run/task lifecycle state rather than observed from a session's hooks.

pub mod announcements;
pub mod attention;
mod keymap;
pub mod state;

use std::collections::{BTreeMap, HashSet};

use factory_core::local::{ErrorCode, LocalResponse};
use factory_core::{
    AgentId, AgentRole, AgentSnapshot, EventEnvelope, FactoryEvent, ProjectId, ProjectSnapshot,
    Provider, RunId, RunSnapshot, SessionId, SessionSnapshot, TaskDetail, TaskId, TaskStatus,
};

pub use announcements::Announcement;
pub use attention::{Attention, Rated};
pub use keymap::{
    Intent, Mode, PendingAction, PickerKind, PickerState, PromptKind, PromptState, TASK_MENU_ITEMS,
    TaskMenuState, View, WorkshopPane,
};
pub use state::AgentState;

use crate::fortress;
use crate::theme::Theme;

/// How many announcement lines the ring buffer keeps. Old lines fall off the front.
pub const ANNOUNCEMENT_CAPACITY: usize = 500;
/// A status/error line is shown in place of the key-help footer for this long after being set.
const STATUS_STICKY_MS: i64 = 6_000;
/// TERMINALS tiles at most this many panes at once (design brief: "2-4 panes").
pub const MAX_TERMINAL_PANES: usize = 4;

// ---------------------------------------------------------------------------------------------
// Small enums
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusLevel {
    Info,
    Error,
}

#[derive(Clone, Debug)]
pub struct StatusMessage {
    pub text: String,
    pub level: StatusLevel,
    pub at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Connection {
    Connecting,
    Live,
    Retrying,
}

/// A fortress workshop's route from its orchestrator to one of its top-level workers. `None`
/// draws no connector at all; see `fortress::render_workshop` and `Board::route_kind_for`'s doc
/// comment for the (documented, approximate) rule used to tell `Durable` from `Transient`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteKind {
    None,
    Durable,
    Transient,
}

// ---------------------------------------------------------------------------------------------
// Board
// ---------------------------------------------------------------------------------------------

pub struct Board {
    pub dev_local_pty: bool,
    pub now_ms: i64,
    pub theme: Theme,

    pub connection: Connection,
    pub connection_detail: Option<String>,
    /// A newer release's version, once the hourly manifest check has found one
    /// (`net::spawn_update_check`); shown in the status line.
    pub update_available: Option<String>,

    /// Every project on the daemon, in whatever order the last snapshot/event delivered them —
    /// use [`Board::projects_sorted`] for creation order.
    pub projects: Vec<ProjectSnapshot>,
    /// Which project WORKSHOP/TERMINALS/FOCUS are scoped to. FORTRESS ignores this.
    pub focused_project: Option<ProjectId>,

    pub agents: BTreeMap<AgentId, AgentSnapshot>,
    pub tasks: BTreeMap<TaskId, TaskDetail>,
    pub runs: BTreeMap<RunId, RunSnapshot>,
    pub sessions: BTreeMap<SessionId, SessionSnapshot>,

    /// `updated_at_ms` of the task snapshot as of the last successful full-detail fetch
    /// (`GetTask`, or any other response that happens to carry a fresh `TaskDetail` — see
    /// `merge_response`), per task. `TaskChanged` events only ever carry the durable snapshot
    /// (see `apply_event`'s doc comment), so this is what lets [`Board::task_detail_needs_fetch`]
    /// tell "never fetched" and "fetched, but a newer snapshot has since arrived" apart from
    /// "fetched and still current".
    detail_synced_at: BTreeMap<TaskId, i64>,
    /// Tasks with a `GetTask` request currently in flight, so `main.rs` doesn't fire a duplicate
    /// one every loop tick while waiting on the first (see `begin_task_detail_fetch`).
    pending_detail: HashSet<TaskId>,

    pub announcements: state::RingBuffer<Announcement>,
    pub activity: BTreeMap<AgentId, state::ActivitySeries>,

    pub view: View,
    /// The one cross-view agent selection cursor (FORTRESS glyph, WORKSHOP's agent tree,
    /// TERMINALS/FOCUS's pane) — "one selection cursor follows through" per the repomon reference
    /// this design adopts (`REFS-HERDR-REPOMON.md`).
    pub selected_agent: Option<AgentId>,
    /// FORTRESS-only: `[`/`]` cycle this independently of `selected_agent`, over workshops
    /// (projects) themselves rather than the agents inside them — the only way to reach a
    /// project with zero agents, which `selected_agent`'s cursor can never land on since it has
    /// no candidates there (see `Board::cycle_selected_workshop` and `zoom_in`'s doc comments).
    /// Mutually exclusive with `selected_agent` in practice (each cursor clears the other) so
    /// FORTRESS never shows two different "selections" onscreen at once.
    pub selected_workshop: Option<ProjectId>,
    /// WORKSHOP's task-list cursor.
    pub selected_task: Option<TaskId>,
    /// WORKSHOP's `Tab`-toggled pane focus (tasks vs. the agent tree).
    pub workshop_focus: WorkshopPane,
    /// WORKSHOP's `!` toggle: show only tasks/agents needing operator attention.
    pub attention_filter: bool,

    pub mode: Mode,
    /// TERMINALS/FOCUS only: whether keystrokes go to the focused pane (`true`, the default) or
    /// are interpreted as board `Action`s (`false`), toggled by the spike's `Ctrl-]` prefix —
    /// Herdr's "exclusive input ownership for a pane" adopted per `REFS-HERDR-REPOMON.md`.
    pub pane_forwarding: bool,

    pub status: Option<StatusMessage>,

    pub caught_up: bool,
    pub quit: bool,
}

impl Board {
    #[must_use]
    pub fn new(dev_local_pty: bool, now_ms: i64, theme: Theme) -> Self {
        Self {
            dev_local_pty,
            now_ms,
            theme,
            connection: Connection::Connecting,
            connection_detail: None,
            update_available: None,
            projects: Vec::new(),
            focused_project: None,
            agents: BTreeMap::new(),
            tasks: BTreeMap::new(),
            runs: BTreeMap::new(),
            sessions: BTreeMap::new(),
            detail_synced_at: BTreeMap::new(),
            pending_detail: HashSet::new(),
            announcements: state::RingBuffer::new(ANNOUNCEMENT_CAPACITY),
            activity: BTreeMap::new(),
            view: View::Fortress,
            selected_agent: None,
            selected_workshop: None,
            selected_task: None,
            workshop_focus: WorkshopPane::Tasks,
            attention_filter: false,
            mode: Mode::Normal,
            pane_forwarding: true,
            status: None,
            caught_up: false,
            quit: false,
        }
    }

    // -- derived views: projects/agents/tasks -----------------------------------------------

    /// All projects, oldest first (ties broken by id) — the order FORTRESS lays workshops out in.
    #[must_use]
    pub fn projects_sorted(&self) -> Vec<ProjectSnapshot> {
        let mut projects = self.projects.clone();
        projects.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        projects
    }

    #[must_use]
    pub fn agents_vec(&self) -> Vec<AgentSnapshot> {
        self.agents.values().cloned().collect()
    }

    /// Every agent, in FORTRESS's exact left-to-right/top-to-bottom visual order (project
    /// creation order, then orchestrator/worker/sub-agent order within each workshop). The single
    /// source of truth for that order — `Tab`/`j`/`k` cycling, `g`/`G`, and TERMINALS' pane order
    /// all call this rather than re-deriving it, so they can never drift from what's drawn.
    #[must_use]
    pub fn agents_in_fortress_order(&self) -> Vec<AgentId> {
        let projects = self.projects_sorted();
        let agents = self.agents_vec();
        // `wrap_width` only affects where a workshop wraps to a new row, never the sequence
        // agents are visited in, so any width works here — `u16::MAX` guarantees no wrapping.
        fortress::compute_workshops(&projects, &agents, u16::MAX)
            .into_iter()
            .flat_map(|workshop| {
                workshop
                    .stations
                    .into_iter()
                    .map(|station| station.agent_id)
            })
            .collect()
    }

    /// One project's agents as `(id, depth)` pairs for WORKSHOP's agent tree: depth 0 for the
    /// orchestrator and top-level workers, depth 1 for sub-agents, in the same order FORTRESS
    /// draws them in.
    #[must_use]
    pub fn agent_tree(&self, project_id: &ProjectId) -> Vec<(AgentId, u8)> {
        self.agents_in_fortress_order()
            .into_iter()
            .filter_map(|id| self.agents.get(&id).map(|agent| (id, agent)))
            .filter(|(_, agent)| &agent.project_id == project_id)
            .map(|(id, agent)| {
                let depth = u8::from(agent.parent_agent_id.is_some());
                (id, depth)
            })
            .collect()
    }

    /// WORKSHOP's agent tree, respecting `attention_filter` when set (only rows needing operator
    /// attention survive).
    #[must_use]
    pub fn visible_agent_tree(&self, project_id: &ProjectId) -> Vec<(AgentId, u8)> {
        let all = self.agent_tree(project_id);
        if !self.attention_filter {
            return all;
        }
        all.into_iter()
            .filter(|(id, _)| {
                self.agents
                    .get(id)
                    .is_some_and(|agent| self.agent_attention(agent).value.needs_operator())
            })
            .collect()
    }

    /// One project's tasks in queue order (oldest first).
    #[must_use]
    pub fn tasks_in(&self, project_id: &ProjectId) -> Vec<&TaskDetail> {
        let mut tasks: Vec<&TaskDetail> = self
            .tasks
            .values()
            .filter(|task| &task.snapshot.project_id == project_id)
            .collect();
        tasks.sort_by(|a, b| {
            a.snapshot
                .created_at_ms
                .cmp(&b.snapshot.created_at_ms)
                .then_with(|| a.snapshot.id.as_str().cmp(b.snapshot.id.as_str()))
        });
        tasks
    }

    /// WORKSHOP's task list, respecting `attention_filter` when set.
    #[must_use]
    pub fn visible_tasks(&self, project_id: &ProjectId) -> Vec<&TaskDetail> {
        let all = self.tasks_in(project_id);
        if !self.attention_filter {
            return all;
        }
        all.into_iter()
            .filter(|task| attention::task_attention(task.snapshot.status).needs_operator())
            .collect()
    }

    // -- derived views: task detail (lazy `GetTask` fetch) ----------------------------------

    /// Whether `task_id`'s cached [`TaskDetail`] (`body`/`result`/`blocked_reason`) is missing or
    /// possibly stale, and therefore worth a `GetTask` round-trip: `FactoryEvent::TaskChanged`
    /// only ever carries the durable snapshot (see `apply_event`'s doc comment), so a task
    /// created — or changed, e.g. completed and gained a `result` — after this client started
    /// watching has an up-to-date `snapshot` but a `body`/`result` frozen at whatever it was the
    /// last time (if ever) `GetTask` actually ran for it. `false` for a task id the board doesn't
    /// know about at all (nothing to fetch a project id for).
    #[must_use]
    pub fn task_detail_needs_fetch(&self, task_id: &TaskId) -> bool {
        let Some(task) = self.tasks.get(task_id) else {
            return false;
        };
        self.detail_synced_at
            .get(task_id)
            .is_none_or(|&synced_at| synced_at < task.snapshot.updated_at_ms)
    }

    /// Whether `task_id`'s hasn't been fully loaded yet and a `GetTask` fetch is in flight for it
    /// — WORKSHOP's detail pane shows "(loading…)" in this state rather than "(no body)".
    #[must_use]
    pub fn is_task_detail_pending(&self, task_id: &TaskId) -> bool {
        self.pending_detail.contains(task_id)
    }

    /// If `task_id`'s detail needs fetching ([`Board::task_detail_needs_fetch`]) and no fetch for
    /// it is already in flight, marks one in flight and returns the project id to fetch it from.
    /// Idempotent: calling again before the response lands (`apply_task_detail_result`) returns
    /// `None`, so `main.rs` can call this unconditionally on every loop tick — e.g. whenever
    /// WORKSHOP's selected task changes, or a `TaskChanged` event bumps the selected task's
    /// `updated_at_ms` — without flooding the daemon with duplicate requests per task id.
    #[must_use = "a Some(project_id) means pending_detail was just marked — the caller must \
                  actually send the GetTask request or it will never be retried"]
    pub fn begin_task_detail_fetch(&mut self, task_id: &TaskId) -> Option<ProjectId> {
        if self.pending_detail.contains(task_id) || !self.task_detail_needs_fetch(task_id) {
            return None;
        }
        let project_id = self.tasks.get(task_id)?.snapshot.project_id.clone();
        self.pending_detail.insert(task_id.clone());
        Some(project_id)
    }

    /// Folds a background `GetTask` fetch's result back into board state (see
    /// `net::spawn_task_detail_request`). Kept separate from `apply_response`'s generic
    /// `NetMsg::OperationResult` path because a failed fetch needs `pending_detail` cleared for
    /// the *specific* task it was for, which a generic `LocalResponse::Error` (no request-echo)
    /// can't tell us.
    pub fn apply_task_detail_result(
        &mut self,
        task_id: TaskId,
        result: Result<LocalResponse, String>,
    ) {
        self.pending_detail.remove(&task_id);
        match result {
            Ok(LocalResponse::Task { task }) => {
                self.detail_synced_at
                    .insert(task.snapshot.id.clone(), task.snapshot.updated_at_ms);
                self.tasks.insert(task.snapshot.id.clone(), task);
            }
            Ok(LocalResponse::Error { code, message }) => {
                self.set_status(
                    format!("{}: {message}", error_code_word(code)),
                    StatusLevel::Error,
                );
            }
            Ok(_) => {} // unexpected response shape for `GetTask`; nothing to fold in
            Err(error) => {
                self.set_status(
                    format!("couldn't load task#{task_id} detail: {error}"),
                    StatusLevel::Error,
                );
            }
        }
        self.clamp_selection();
    }

    #[must_use]
    pub fn orchestrators_in(&self, project_id: Option<&ProjectId>) -> Vec<&AgentSnapshot> {
        let mut orchestrators: Vec<&AgentSnapshot> = self
            .agents
            .values()
            .filter(|agent| agent.role == AgentRole::Orchestrator)
            .filter(|agent| project_id.is_none_or(|id| &agent.project_id == id))
            .collect();
        orchestrators.sort_by_key(|agent| agent.id.as_str());
        orchestrators
    }

    // -- derived views: state/attention ------------------------------------------------------

    /// The session a hook has reported for this agent, if any — regardless of whether it's still
    /// live, since a `Stopped`/`Failed` session is still meaningful, real, observed state (see
    /// `Board::agent_state`'s doc comment).
    #[must_use]
    pub fn session_for(&self, agent: &AgentSnapshot) -> Option<&SessionSnapshot> {
        agent
            .current_session_id
            .as_ref()
            .and_then(|id| self.sessions.get(id))
    }

    /// The agent's most recent run attempt by start time, or `None` if it has never run.
    #[must_use]
    pub fn latest_run_for(&self, agent_id: &AgentId) -> Option<&RunSnapshot> {
        self.runs
            .values()
            .filter(|run| &run.agent_id == agent_id)
            .max_by_key(|run| (run.started_at_ms, run.id.clone()))
    }

    /// The single mapping point from durable daemon state to the board's five-way
    /// [`AgentState`]. Session state wins whenever a session exists (hooks supersede inference,
    /// per the design brief); otherwise falls back to the pre-sessions run-status mapping, and
    /// `inferred` on the result is set so callers can show the `~` prefix the brief asks for.
    #[must_use]
    pub fn agent_state(&self, agent: &AgentSnapshot) -> Rated<AgentState> {
        if let Some(session) = self.session_for(agent) {
            return Rated::observed(state::agent_state_from_session(session.state));
        }
        Rated::inferred(state::agent_state_from_run(
            self.latest_run_for(&agent.id).map(|run| run.status),
        ))
    }

    /// Same precedence rule as [`Board::agent_state`], for the [`Attention`] taxonomy instead.
    #[must_use]
    pub fn agent_attention(&self, agent: &AgentSnapshot) -> Rated<Attention> {
        if let Some(session) = self.session_for(agent) {
            return Rated::observed(attention::session_attention(session.state));
        }
        Rated::inferred(
            self.latest_run_for(&agent.id)
                .map_or(Attention::Routine, |run| {
                    attention::run_attention(run.status)
                }),
        )
    }

    /// Whether, and how, to draw a route connector from a workshop's orchestrator to `worker`.
    /// `Transient` while the worker currently has an open episode; `Durable` if it has ever been
    /// assigned a task in this project (a documented approximation — the wire protocol doesn't
    /// track *which* orchestrator assigned/messaged a worker, only whether a task is/was assigned
    /// to it, so this reads as "this workshop has a standing relationship with this worker" rather
    /// than tracing a specific message); `None` otherwise.
    #[must_use]
    pub fn route_kind_for(&self, worker: &AgentSnapshot) -> RouteKind {
        if self.agent_state(worker).value == AgentState::Working {
            return RouteKind::Transient;
        }
        let ever_assigned = self.tasks.values().any(|task| {
            task.snapshot.project_id == worker.project_id
                && task.snapshot.assigned_agent_id.as_ref() == Some(&worker.id)
        });
        if ever_assigned {
            RouteKind::Durable
        } else {
            RouteKind::None
        }
    }

    #[must_use]
    pub fn queued_task_count(&self, project_id: &ProjectId) -> usize {
        self.tasks
            .values()
            .filter(|task| &task.snapshot.project_id == project_id)
            .filter(|task| task.snapshot.status == TaskStatus::Queued)
            .count()
    }

    #[must_use]
    pub fn idle_capacity_count(&self, project_id: &ProjectId) -> usize {
        self.agents
            .values()
            .filter(|agent| &agent.project_id == project_id && agent.role == AgentRole::Worker)
            .filter(|agent| self.agent_state(agent).value == AgentState::Idle)
            .count()
    }

    // -- derived views: terminal attach targets ----------------------------------------------

    /// The session id `main.rs` should attach a pane to for `agent`, if any. Real usage: only an
    /// agent with a live session produces one. `--dev-local-pty` (offline testing only — see
    /// `README.md`): every agent gets a deterministic synthetic id (`dev-<agent_id>`, never
    /// inserted into `self.sessions`) so the pane mechanics can be exercised without a daemon
    /// that implements sessions yet; `main.rs`'s reconciliation tells real from synthetic by
    /// checking `self.sessions.contains_key(..)` and spawns a local shell instead of a daemon
    /// attach for the latter (see `agent_for_pane_session`, its inverse).
    fn session_id_for_pane(&self, agent: &AgentSnapshot) -> Option<SessionId> {
        if let Some(session_id) = &agent.current_session_id {
            if self
                .sessions
                .get(session_id)
                .is_some_and(|session| session.state.is_live())
            {
                return Some(session_id.clone());
            }
        }
        if self.dev_local_pty {
            return SessionId::try_from(format!("dev-{}", agent.id)).ok();
        }
        None
    }

    /// The inverse of `session_id_for_pane`: which agent a (possibly synthetic) pane session id
    /// belongs to. Used by `main.rs`'s pane reconciliation to build titles/targets without ever
    /// needing to parse the synthetic id's shape itself.
    #[must_use]
    pub fn agent_for_pane_session(&self, session_id: &SessionId) -> Option<&AgentSnapshot> {
        self.agents
            .values()
            .find(|agent| self.session_id_for_pane(agent).as_ref() == Some(session_id))
    }

    /// Up to [`MAX_TERMINAL_PANES`] targets in the focused project, in fortress order — the panes
    /// TERMINALS tiles.
    #[must_use]
    pub fn terminal_targets(&self) -> Vec<SessionId> {
        let Some(project_id) = self.focused_project.clone() else {
            return Vec::new();
        };
        self.agents_in_fortress_order()
            .into_iter()
            .filter_map(|agent_id| self.agents.get(&agent_id))
            .filter(|agent| agent.project_id == project_id)
            .filter_map(|agent| self.session_id_for_pane(agent))
            .take(MAX_TERMINAL_PANES)
            .collect()
    }

    /// The selected agent's target — what FOCUS attaches when entered directly (e.g. via `G`),
    /// not necessarily one of `terminal_targets`.
    #[must_use]
    pub fn focus_target(&self) -> Option<SessionId> {
        let agent = self
            .selected_agent
            .as_ref()
            .and_then(|id| self.agents.get(id))?;
        self.session_id_for_pane(agent)
    }

    /// Which sessions should currently be attached, given `self.view`. FORTRESS/WORKSHOP attach
    /// nothing — "detach without stopping the worker" applies just as much to *leaving* TERMINALS
    /// or FOCUS as it does to quitting the whole client.
    #[must_use]
    pub fn desired_sessions(&self) -> Vec<SessionId> {
        match self.view {
            View::Terminals => self.terminal_targets(),
            View::Focus => self.focus_target().into_iter().collect(),
            View::Fortress | View::Workshop => Vec::new(),
        }
    }

    // -- status/help text -----------------------------------------------------------------

    fn set_status(&mut self, text: impl Into<String>, level: StatusLevel) {
        self.status = Some(StatusMessage {
            text: text.into(),
            level,
            at_ms: self.now_ms,
        });
    }

    /// A public status-line setter for `main.rs`, which has no other way to surface things like
    /// "couldn't attach the terminal pane" (a PTY/socket-level failure `Board` itself never
    /// sees).
    pub fn note_error(&mut self, text: impl Into<String>) {
        self.set_status(text, StatusLevel::Error);
    }

    /// The text the bottom status/help line should show right now: a recent status/error message
    /// if one is still "sticky", otherwise the key-help reminder for the current mode/view.
    #[must_use]
    pub fn status_line_text(&self) -> String {
        if let Some(status) = &self.status {
            if self.now_ms - status.at_ms < STATUS_STICKY_MS {
                return status.text.clone();
            }
        }
        self.help_text()
    }

    #[must_use]
    pub fn status_line_is_error(&self) -> bool {
        self.status.as_ref().is_some_and(|status| {
            status.level == StatusLevel::Error && self.now_ms - status.at_ms < STATUS_STICKY_MS
        })
    }

    /// The key-help reminder for the current mode/view (used as the status line's default text,
    /// and as a hint inside prompt overlays).
    #[must_use]
    pub fn help_text(&self) -> String {
        match &self.mode {
            Mode::Normal => self.normal_help_text(),
            Mode::Confirm(action) => match action {
                PendingAction::DeleteTask(_) => {
                    "delete this task? y/Enter confirms, anything else cancels".to_owned()
                }
                PendingAction::StopSession { .. } | PendingAction::StopRun { .. } => {
                    "stop this agent? x/y/Enter confirms, anything else cancels".to_owned()
                }
            },
            Mode::Prompt(prompt) => format!(
                "{}: type to edit, Tab/Enter next field, Esc cancels",
                prompt.labels.get(prompt.field).unwrap_or(&"")
            ),
            Mode::Picker(_) => "j/k/Tab move, Enter select, Esc cancel".to_owned(),
            Mode::TaskMenu(_) => "j/k move, Enter choose, Esc cancel".to_owned(),
            Mode::Help => "press ? or Esc to close help".to_owned(),
        }
    }

    fn normal_help_text(&self) -> String {
        if matches!(self.view, View::Terminals | View::Focus) {
            if self.pane_forwarding {
                return "Ctrl-] board control  1-4 views  q detach".to_owned();
            }
            return "CONTROL  1-4 views  Tab/j/k switch pane  Enter/l zoom  Esc/h back  \
                     Ctrl-] back to pane  ? help"
                .to_owned();
        }
        "1-4 views  j/k move  Tab switch  Enter/l zoom in  Esc/h zoom out  n new  m message  \
         o orchestrator  x stop  g/G attention  ? help  q detach"
            .to_owned()
    }

    // -- ticking --------------------------------------------------------------------------

    /// Called on every ~1s tick (and whenever `now_ms` otherwise needs bumping) so elapsed-time
    /// displays and sparklines keep moving even for idle agents.
    pub fn tick(&mut self, now_ms: i64) {
        self.now_ms = now_ms;
        for series in self.activity.values_mut() {
            series.roll_to(now_ms);
        }
    }

    // -- applying network state -------------------------------------------------------------

    pub fn set_retrying(&mut self, detail: impl Into<String>) {
        self.connection = Connection::Retrying;
        self.connection_detail = Some(detail.into());
    }

    pub fn set_live(&mut self) {
        self.connection = Connection::Live;
        self.connection_detail = None;
    }

    /// Replaces the entire fleet snapshot (every project's projects/agents/tasks/runs/sessions).
    /// If no project is focused yet, focuses the oldest one (by creation order) — so WORKSHOP has
    /// something to show without requiring the operator to zoom in first, unless `--project`
    /// already chose one (`focus_project`, called by `main.rs` before this on startup).
    pub fn apply_fleet_snapshot(
        &mut self,
        projects: Vec<ProjectSnapshot>,
        agents: Vec<AgentSnapshot>,
        tasks: Vec<TaskDetail>,
        runs: Vec<RunSnapshot>,
        sessions: Vec<SessionSnapshot>,
    ) {
        self.projects = projects;
        self.agents = agents.into_iter().map(|a| (a.id.clone(), a)).collect();
        self.tasks = tasks
            .into_iter()
            .map(|t| (t.snapshot.id.clone(), t))
            .collect();
        self.runs = runs.into_iter().map(|r| (r.id.clone(), r)).collect();
        self.sessions = sessions.into_iter().map(|s| (s.id.clone(), s)).collect();
        self.ensure_default_focus();
    }

    /// Focuses `project_id` for WORKSHOP/TERMINALS/FOCUS, if it exists among `self.projects`.
    pub fn focus_project(&mut self, project_id: ProjectId) {
        if self.projects.iter().any(|p| p.id == project_id) {
            self.focused_project = Some(project_id);
        }
    }

    fn ensure_default_focus(&mut self) {
        if self.focused_project.is_none() {
            if let Some(first) = self.projects_sorted().first() {
                self.focused_project = Some(first.id.clone());
            }
        }
    }

    pub fn apply_event(&mut self, event: EventEnvelope) {
        if let Some(agent_id) = event_agent(&event.event) {
            self.activity
                .entry(agent_id.clone())
                .or_default()
                .record(event.occurred_at_ms);
        }

        if let Some(announcement) = announcements::format_event(&event) {
            self.announcements.push(announcement);
        }

        match event.event {
            FactoryEvent::ProjectChanged { project } => {
                if let Some(existing) = self.projects.iter_mut().find(|p| p.id == project.id) {
                    *existing = project;
                } else {
                    self.projects.push(project);
                }
                self.ensure_default_focus();
            }
            FactoryEvent::TaskChanged { task } => {
                // `TaskChanged` only carries the durable snapshot, not `body`/`result` (those
                // live in `TaskDetail`, loaded separately) - preserve whatever we already have
                // for this task rather than blanking it out.
                let existing = self.tasks.get(&task.id);
                let body = existing.map_or_else(String::new, |detail| detail.body.clone());
                let result = existing.and_then(|detail| detail.result.clone());
                let blocked_reason = existing.and_then(|detail| detail.blocked_reason.clone());
                self.tasks.insert(
                    task.id.clone(),
                    TaskDetail {
                        snapshot: task,
                        body,
                        result,
                        blocked_reason,
                    },
                );
            }
            FactoryEvent::AgentChanged { agent } => {
                self.agents.insert(agent.id.clone(), agent);
            }
            FactoryEvent::RunChanged { run } => {
                self.runs.insert(run.id.clone(), run);
            }
            FactoryEvent::SessionChanged { session } => {
                self.sessions.insert(session.id.clone(), session);
            }
            FactoryEvent::TaskDeleted { task_id, .. } => {
                self.tasks.remove(&task_id);
            }
            FactoryEvent::AgentDeleted { agent_id, .. } => {
                self.agents.remove(&agent_id);
                self.activity.remove(&agent_id);
            }
            FactoryEvent::ProjectDeleted { project_id } => {
                self.projects.retain(|p| p.id != project_id);
                self.agents.retain(|_, a| a.project_id != project_id);
                self.tasks
                    .retain(|_, t| t.snapshot.project_id != project_id);
                self.runs.retain(|_, r| r.project_id != project_id);
                self.sessions.retain(|_, s| s.project_id != project_id);
                if self.focused_project.as_ref() == Some(&project_id) {
                    self.focused_project = None;
                    self.ensure_default_focus();
                }
            }
        }
        self.clamp_selection();
    }

    pub fn apply_response(&mut self, result: Result<LocalResponse, String>) {
        match result {
            Ok(LocalResponse::Error { code, message }) => {
                self.set_status(
                    format!("{}: {message}", error_code_word(code)),
                    StatusLevel::Error,
                );
            }
            Ok(response) => {
                let text = self.merge_response(response);
                self.set_status(text, StatusLevel::Info);
            }
            Err(error) => {
                self.set_status(format!("request failed: {error}"), StatusLevel::Error);
            }
        }
        self.clamp_selection();
    }

    /// Folds a successful response's payload back into board state immediately (in addition to
    /// the `TaskChanged`/`AgentChanged`/etc. event that will also arrive over the subscription;
    /// this just removes the round-trip latency for the client that made the request) and
    /// returns a short human-readable description for the status line.
    fn merge_response(&mut self, response: LocalResponse) -> String {
        match response {
            LocalResponse::TaskCreated { task } => {
                let id = task.snapshot.id.clone();
                self.detail_synced_at
                    .insert(id.clone(), task.snapshot.updated_at_ms);
                self.tasks.insert(task.snapshot.id.clone(), task);
                format!("created task#{id}")
            }
            LocalResponse::Task { task }
            | LocalResponse::TaskRetried { task }
            | LocalResponse::TaskCancelled { task }
            | LocalResponse::TaskUpdated { task }
            | LocalResponse::TaskAssigned { task }
            | LocalResponse::TaskCompleted { task }
            | LocalResponse::TaskBlocked { task } => {
                let id = task.snapshot.id.clone();
                let status = task.snapshot.status;
                // Every one of these carries a fresh, full `TaskDetail` (body/result/
                // blocked_reason all current as of this response), so mark it synced too - not
                // just the `GetTask` path - to avoid a redundant fetch on the next render.
                self.detail_synced_at
                    .insert(id.clone(), task.snapshot.updated_at_ms);
                self.pending_detail.remove(&id);
                self.tasks.insert(task.snapshot.id.clone(), task);
                format!("task#{id} {}", announcements::task_status_word(status))
            }
            LocalResponse::TaskDeleted { task_id, .. } => {
                self.tasks.remove(&task_id);
                format!("deleted task#{task_id}")
            }
            LocalResponse::AgentCreated { agent } => {
                let id = agent.id.clone();
                self.agents.insert(agent.id.clone(), agent);
                format!("created agent {id}")
            }
            LocalResponse::Agent { agent } | LocalResponse::AgentProfileUpdated { agent } => {
                let id = agent.snapshot.id.clone();
                self.agents
                    .insert(agent.snapshot.id.clone(), agent.snapshot);
                format!("agent {id} updated")
            }
            LocalResponse::AgentDeleted { agent_id, .. } => {
                self.agents.remove(&agent_id);
                self.activity.remove(&agent_id);
                format!("removed agent {agent_id}")
            }
            LocalResponse::AgentPaused { agent } | LocalResponse::AgentResumed { agent } => {
                let id = agent.id.clone();
                let paused = agent.paused;
                self.agents.insert(agent.id.clone(), agent);
                format!("agent {id} {}", if paused { "paused" } else { "resumed" })
            }
            LocalResponse::AgentMessageSent { message } => {
                format!("message sent to {}", message.recipient_agent_id)
            }
            LocalResponse::RunAccepted { run_id } => format!("started run {run_id}"),
            LocalResponse::RunStopped { run_id } => format!("stop requested for run {run_id}"),
            LocalResponse::RunCancelled { run_id } => format!("run {run_id} cancelled"),
            LocalResponse::SessionStopped { session_id } => {
                format!("stop requested for session {session_id}")
            }
            LocalResponse::Sessions { sessions, .. } => {
                for session in sessions {
                    self.sessions.insert(session.id.clone(), session);
                }
                "sessions refreshed".to_owned()
            }
            _ => "ok".to_owned(),
        }
    }

    fn clamp_selection(&mut self) {
        if let Some(id) = &self.selected_agent {
            if !self.agents.contains_key(id) {
                self.selected_agent = None;
            }
        }
        if let Some(id) = &self.selected_workshop {
            if !self.projects.iter().any(|p| &p.id == id) {
                self.selected_workshop = None;
            }
        }
        if let Some(id) = &self.selected_task {
            if !self.tasks.contains_key(id) {
                self.selected_task = None;
            }
        }
        // Deleted tasks shouldn't leak into these two indefinitely.
        self.detail_synced_at
            .retain(|id, _| self.tasks.contains_key(id));
        self.pending_detail.retain(|id| self.tasks.contains_key(id));
    }
}

fn event_agent(event: &FactoryEvent) -> Option<&AgentId> {
    match event {
        FactoryEvent::TaskChanged { task } => task.assigned_agent_id.as_ref(),
        FactoryEvent::RunChanged { run } => Some(&run.agent_id),
        FactoryEvent::AgentChanged { agent } => Some(&agent.id),
        FactoryEvent::AgentDeleted { agent_id, .. } => Some(agent_id),
        FactoryEvent::SessionChanged { session } => Some(&session.agent_id),
        FactoryEvent::TaskDeleted { .. }
        | FactoryEvent::ProjectChanged { .. }
        | FactoryEvent::ProjectDeleted { .. } => None,
    }
}

fn error_code_word(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidRequest => "invalid request",
        ErrorCode::UnsupportedProtocol => "unsupported protocol",
        ErrorCode::NotFound => "not found",
        ErrorCode::Conflict => "conflict",
        ErrorCode::Internal => "internal error",
    }
}

/// Glyph a worker's provider maps to, independent of theme — used by non-fortress panels
/// (WORKSHOP's agent tree, TERMINALS' pane titles) that want the same letter FORTRESS uses.
#[must_use]
pub const fn provider_letter(provider: Provider) -> char {
    match provider {
        Provider::ClaudeCode => 'C',
        Provider::Codex => 'X',
        Provider::Shell => 'S',
    }
}

#[cfg(test)]
mod tests;
