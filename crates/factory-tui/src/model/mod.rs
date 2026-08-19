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
//! project's agents/tasks/runs/sessions at once. BUILDING is fleet-wide; AGENT follows the
//! selected agent while `focused_project` supplies scope for project actions.
//!
//! ## Deriving agent state
//!
//! [`Board::agent_state`] is the one function everything else in this crate goes through instead
//! of inspecting [`SessionState`]/[`RunStatus`] itself. Operator attention is not derived here:
//! it is the shared CLI-first [`factory_core::status::AttentionItem`] projection received in
//! fleet status.

pub mod announcements;
mod keymap;
pub mod state;

use std::collections::{BTreeMap, HashMap};

use factory_core::local::{
    AgentDetail, AgentMessage, AttachRefusal, AttachRefusalReason, ErrorCode, LocalResponse,
};
use factory_core::status::{
    AttentionAction, AttentionItem, AttentionReason, AttentionReasonKind, age_text, display_text,
};
use factory_core::{
    AgentId, AgentRole, AgentSnapshot, EventEnvelope, FactoryEvent, ProjectId, ProjectSnapshot,
    Provider, RunId, RunSnapshot, RunnerInstanceId, SessionId, SessionSnapshot, SessionState,
    TaskDetail, TaskId,
};
use factoryctl::managed_update::UpdateProgress;
use factoryctl::update::UpdateCheck;

pub use announcements::Announcement;
pub use factory_core::attention::Rated;
pub use keymap::{
    Intent, Mode, PaneMode, PendingAction, PickerKind, PickerState, PromptKind, PromptState,
    TaskMenuState, View,
};
pub use state::AgentState;

use crate::theme::Theme;

/// How many announcement lines the ring buffer keeps. Old lines fall off the front.
pub const ANNOUNCEMENT_CAPACITY: usize = 500;
/// A status/error line is shown in place of the key-help footer for this long after being set.
const STATUS_STICKY_MS: i64 = 6_000;
/// Maximum length of a sticky status/error message in the non-wrapping footer.
const STATUS_TEXT_MAX_CHARS: usize = 64;
/// How many recent durable event sequence numbers are retained to prevent replay/live overlap
/// from counting activity twice. This exceeds the connect-time replay batch while remaining
/// bounded for a long-running board.
const EVENT_DEDUPE_CAPACITY: usize = 1_024;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionFocus {
    pub item: AttentionItem,
    pub resolved: bool,
}

/// Durable ownership for an activity series. `None` is retained only for replay events received
/// before a fleet snapshot supplies the agent generation; snapshots discard such unproven history.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ActivityIdentity {
    project_id: ProjectId,
    created_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AttachIdentity {
    project_id: ProjectId,
    runner_instance_id: Option<RunnerInstanceId>,
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
    /// Version reported by the connected daemon's health response. An empty
    /// value means an older daemon that cannot identify itself.
    pub daemon_version: Option<String>,
    /// A newer release's version, once the hourly manifest check has found one
    /// (`net::spawn_update_check`); shown in the status line.
    pub update_available: Option<String>,
    /// Exact release check behind the visible manual update action.
    pub update_check: Option<UpdateCheck>,
    pub update_progress: Option<UpdateProgress>,
    /// `factoryd --max-active-runs`, learned from `FleetStatus` after bootstrap; the status line
    /// shows live sessions against it.
    pub live_session_cap: Option<u32>,
    /// Every project on the daemon, in whatever order the last snapshot/event delivered them —
    /// use [`Board::projects_sorted`] for creation order.
    pub projects: Vec<ProjectSnapshot>,
    /// Project used by project-scoped actions and remembered across TUI runs.
    pub focused_project: Option<ProjectId>,

    pub agents: BTreeMap<AgentId, AgentSnapshot>,
    /// Git summaries received through the CLI-first fleet-status request.
    pub worktrees: BTreeMap<AgentId, factory_core::status::WorktreeStatus>,
    pub tasks: BTreeMap<TaskId, TaskDetail>,
    pub runs: BTreeMap<RunId, RunSnapshot>,
    pub sessions: BTreeMap<SessionId, SessionSnapshot>,
    pub agent_details: BTreeMap<AgentId, AgentDetail>,
    pub messages: BTreeMap<AgentId, Vec<AgentMessage>>,
    /// Authoritative CLI-first attention projection from `FleetStatus`.
    pub attention: Vec<AttentionItem>,
    /// Client-only attach failures that the unreachable daemon cannot own.
    local_attention: BTreeMap<SessionId, AttentionItem>,

    pub announcements: state::RingBuffer<Announcement>,
    pub activity: BTreeMap<AgentId, state::ActivitySeries>,
    activity_identities: BTreeMap<AgentId, ActivityIdentity>,
    seen_event_sequences: state::RingBuffer<i64>,
    /// Causal revision of the currently displayed attention projection.
    attention_revision: i64,
    /// Highest live event sequence folded into the state maps. A FleetStatus
    /// at the same sequence may have arrived first; it must not block the
    /// corresponding live event from updating task/session state.
    event_revision: i64,
    /// Bounded retry deadlines for client-side terminal attach failures.
    attach_retry_after: BTreeMap<SessionId, i64>,
    /// Identity of the durable session generation that owns each local failure.
    attach_failure_identities: BTreeMap<SessionId, AttachIdentity>,
    /// Non-retryable refusals remain fenced while the same durable identity is selected.
    attach_retry_blocked: BTreeMap<SessionId, AttachIdentity>,

    pub view: View,
    /// The one agent selection shared by BUILDING and AGENT.
    pub selected_agent: Option<AgentId>,
    /// Task targeted by a task action modal.
    pub selected_task: Option<TaskId>,
    /// Explicit action card selected by `g` or a NEEDS YOU click.
    pub attention_focus: Option<AttentionFocus>,
    pub mode: Mode,
    /// Whether AGENT keys control the board or go exclusively to the terminal.
    pub pane_mode: PaneMode,
    /// Set by the pane reconciler only after the selected terminal is actually attached.
    pub pane_ready: bool,
    /// AGENT's terminal consumes the full content area while true.
    pub terminal_maximized: bool,

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
            daemon_version: None,
            update_available: None,
            update_check: None,
            update_progress: None,
            live_session_cap: None,
            projects: Vec::new(),
            focused_project: None,
            agents: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            tasks: BTreeMap::new(),
            runs: BTreeMap::new(),
            sessions: BTreeMap::new(),
            agent_details: BTreeMap::new(),
            messages: BTreeMap::new(),
            attention: Vec::new(),
            local_attention: BTreeMap::new(),
            announcements: state::RingBuffer::new(ANNOUNCEMENT_CAPACITY),
            activity: BTreeMap::new(),
            activity_identities: BTreeMap::new(),
            seen_event_sequences: state::RingBuffer::new(EVENT_DEDUPE_CAPACITY),
            attention_revision: 0,
            event_revision: -1,
            attach_retry_after: BTreeMap::new(),
            attach_failure_identities: BTreeMap::new(),
            attach_retry_blocked: BTreeMap::new(),
            view: View::Building,
            selected_agent: None,
            selected_task: None,
            attention_focus: None,
            mode: Mode::Normal,
            pane_mode: PaneMode::Board,
            pane_ready: false,
            terminal_maximized: false,
            status: None,
            caught_up: false,
            quit: false,
        }
    }

    pub fn apply_fleet_status(&mut self, status: factory_core::status::FleetStatus) {
        self.live_session_cap = Some(status.live_session_cap);
        if status.event_sequence < 0
            || (status.event_sequence >= self.attention_revision
                && status.event_sequence >= self.event_revision)
        {
            if status.event_sequence >= 0 {
                self.attention_revision = self.attention_revision.max(status.event_sequence);
            }
            self.attention = status.attention;
            self.reconcile_attention_focus();
        }
        self.worktrees = status
            .projects
            .into_iter()
            .flat_map(|project| project.agents)
            .filter_map(|agent| agent.worktree.map(|worktree| (agent.agent.id, worktree)))
            .collect();
    }

    /// Records the durable head paired with the authoritative bootstrap.
    /// Replay may fill historical activity, but cannot lower this causal
    /// boundary or allow an older independently requested status to win.
    pub fn note_fleet_snapshot_sequence(&mut self, event_sequence: i64) {
        if event_sequence > self.attention_revision {
            self.attention_revision = event_sequence;
            self.attention.clear();
            self.reconcile_attention_focus();
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

    /// Every agent, in FORTRESS's exact left-to-right/top-to-bottom visual order (project
    /// creation order, then orchestrator/worker/sub-agent order within each workshop). The single
    /// source of truth for that order — `Tab`/`j`/`k` cycling, `g`/`G`, and TERMINALS' pane order
    /// all call this rather than re-deriving it, so they can never drift from what's drawn.
    #[must_use]
    pub fn agents_in_fortress_order(&self) -> Vec<AgentId> {
        self.projects_sorted()
            .into_iter()
            .flat_map(|project| {
                let mut agents: Vec<_> = self
                    .agents
                    .values()
                    .filter(|agent| agent.project_id == project.id)
                    .collect();
                agents.sort_by_key(|agent| {
                    (
                        agent.role != AgentRole::Orchestrator,
                        agent.parent_agent_id.is_some(),
                        agent.created_at_ms,
                        agent.id.clone(),
                    )
                });
                agents
                    .into_iter()
                    .map(|agent| agent.id.clone())
                    .collect::<Vec<_>>()
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

    /// The canonical assigned queue for one agent, in the same stable order the AGENT view renders
    /// and targets it.
    #[must_use]
    pub fn active_tasks_for_agent(&self, agent_id: &AgentId) -> Vec<&TaskDetail> {
        let mut tasks: Vec<_> = self
            .tasks
            .values()
            .filter(|task| {
                task.snapshot.assigned_agent_id.as_ref() == Some(agent_id)
                    && matches!(
                        task.snapshot.status,
                        factory_core::TaskStatus::Queued | factory_core::TaskStatus::Running
                    )
            })
            .collect();
        tasks.sort_by(|a, b| factory_core::active_task_cmp(&a.snapshot, &b.snapshot));
        tasks
    }

    /// Terminal assignment history is deliberately separate from the active
    /// queue, but remains in the board so retry/cancelled work is operable.
    #[must_use]
    pub fn task_history_for_agent(&self, agent_id: &AgentId) -> Vec<&TaskDetail> {
        let mut tasks: Vec<_> = self
            .tasks
            .values()
            .filter(|task| {
                task.snapshot.assigned_agent_id.as_ref() == Some(agent_id)
                    && !matches!(
                        task.snapshot.status,
                        factory_core::TaskStatus::Queued | factory_core::TaskStatus::Running
                    )
            })
            .collect();
        tasks.sort_by(|a, b| {
            a.snapshot
                .updated_at_ms
                .cmp(&b.snapshot.updated_at_ms)
                .then_with(|| a.snapshot.id.as_str().cmp(b.snapshot.id.as_str()))
        });
        tasks
    }

    // -- derived views: task detail (lazy `GetTask` fetch) ----------------------------------

    /// Whether `task_id`'s cached [`TaskDetail`] (`body`/`result`/`blocked_reason`) is missing or
    /// possibly stale, and therefore worth a `GetTask` round-trip: `FactoryEvent::TaskChanged`
    /// only ever carries the durable snapshot (see `apply_event`'s doc comment), so a task
    /// created — or changed, e.g. completed and gained a `result` — after this client started
    /// watching has an up-to-date `snapshot` but a `body`/`result` frozen at whatever it was the
    /// last time (if ever) `GetTask` actually ran for it. `false` for a task id the board doesn't
    /// Whether `task_id`'s hasn't been fully loaded yet and a `GetTask` fetch is in flight for it
    /// If `task_id`'s detail needs fetching ([`Board::task_detail_needs_fetch`]) and no fetch for
    /// it is already in flight, marks one in flight and returns the project id to fetch it from.
    /// Idempotent: calling again before the response lands (`apply_task_detail_result`) returns
    /// `None`, so `main.rs` can call this unconditionally on every loop tick — e.g. whenever
    /// WORKSHOP's selected task changes, or a `TaskChanged` event bumps the selected task's
    /// Folds a background `GetTask` fetch's result back into board state (see
    /// `net::spawn_task_detail_request`). Kept separate from `apply_response`'s generic
    /// `NetMsg::OperationResult` path because a failed fetch needs `pending_detail` cleared for
    /// the *specific* task it was for, which a generic `LocalResponse::Error` (no request-echo)
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

    #[must_use]
    pub fn attention_items(&self) -> Vec<AttentionItem> {
        let mut items: Vec<_> = self
            .attention
            .iter()
            .filter(|item| {
                item.session_id.as_ref().is_none_or(|session_id| {
                    !self.local_attention.contains_key(session_id)
                        || item.reason.kind == AttentionReasonKind::ObserverProblem
                })
            })
            .chain(self.local_attention.values().filter(|local| {
                !self.attention.iter().any(|item| {
                    item.session_id == local.session_id
                        && item.reason.kind == AttentionReasonKind::ObserverProblem
                })
            }))
            .filter(|item| item.level.needs_operator())
            .cloned()
            .collect();
        factory_core::status::sort_attention(&mut items);
        items
    }

    fn reconcile_attention_focus(&mut self) {
        let Some(source) = self
            .attention_focus
            .as_ref()
            .map(|focus| focus.item.clone())
        else {
            return;
        };
        let current = self
            .attention_items()
            .into_iter()
            .find(|item| same_attention_source(item, &source));
        let focus = self.attention_focus.as_mut().expect("focus checked above");
        if let Some(current) = current {
            focus.item = current.clone();
            focus.resolved = false;
        } else if !focus.resolved {
            focus.resolved = true;
            self.set_status(
                "attention changed or resolved before action",
                StatusLevel::Info,
            );
        }
    }

    fn invalidate_attention(&mut self, mut invalid: impl FnMut(&AttentionItem) -> bool) {
        self.attention.retain(|item| !invalid(item));
        self.reconcile_attention_focus();
    }

    /// Surfaces a client-local socket failure through the same action list as
    /// daemon-owned attention. Runner-side attach failures are persisted by
    /// `factoryd`; this covers the only case it cannot record because the TUI
    /// could not reach it at all.
    pub fn note_local_attach_failure(&mut self, session_id: &SessionId, error: &str) {
        let identity = self.session_attach_identity(session_id);
        self.note_attach_failure(session_id, error, true, identity);
    }

    /// Folds the daemon's typed refusal into the board without changing task or session state.
    /// Ended/missing sessions are not retried: refreshing the durable fleet is their recovery.
    /// Runner replacement/unavailability may recover in place, so those retain the bounded retry.
    pub fn note_attach_refusal(&mut self, refusal: &AttachRefusal) -> bool {
        let Some(identity) = self.session_attach_identity(&refusal.session_id) else {
            return false;
        };
        if identity.project_id != refusal.project_id
            || identity.runner_instance_id != refusal.runner_instance_id
        {
            return false;
        }
        let message = match refusal.reason {
            AttachRefusalReason::SessionNotFound => {
                "terminal unavailable: session no longer exists; state refreshed"
            }
            AttachRefusalReason::SessionEnded => {
                "terminal unavailable: session ended; task state unchanged"
            }
            AttachRefusalReason::RunnerRejected => {
                "terminal unavailable: runner rejected attach; state refreshed"
            }
            AttachRefusalReason::RunnerReplaced => {
                "terminal unavailable: runner replaced; retrying after refresh"
            }
            AttachRefusalReason::RunnerUnavailable => {
                "terminal unavailable: runner unavailable; retrying after refresh"
            }
        };
        self.note_attach_failure(
            &refusal.session_id,
            message,
            refusal.reason.retryable(),
            Some(identity),
        );
        self.note_error(message);
        true
    }

    fn note_attach_failure(
        &mut self,
        session_id: &SessionId,
        error: &str,
        retry: bool,
        identity: Option<AttachIdentity>,
    ) {
        let Some(session) = self.sessions.get(session_id) else {
            return;
        };
        if let Some(identity) = identity {
            self.attach_failure_identities
                .insert(session_id.clone(), identity.clone());
            if retry {
                self.attach_retry_blocked.remove(session_id);
            } else {
                self.attach_retry_blocked
                    .insert(session_id.clone(), identity);
            }
        }
        if retry {
            self.attach_retry_after
                .entry(session_id.clone())
                .or_insert(self.now_ms.saturating_add(1_000));
        } else {
            self.attach_retry_after.remove(session_id);
        }
        if self.local_attention.contains_key(session_id) {
            return;
        }
        self.local_attention.insert(
            session_id.clone(),
            AttentionItem {
                level: factory_core::attention::Attention::Failed,
                project_id: session.project_id.clone(),
                agent_id: Some(session.agent_id.clone()),
                task_id: None,
                session_id: Some(session_id.clone()),
                run_id: session.current_run_id.clone(),
                since_ms: self.now_ms,
                reason: AttentionReason {
                    kind: AttentionReasonKind::ObserverProblem,
                    summary: display_text(&format!("local terminal attach failed: {error}")),
                    action: AttentionAction::InspectObserver,
                },
            },
        );
        self.reconcile_attention_focus();
    }

    fn session_attach_identity(&self, session_id: &SessionId) -> Option<AttachIdentity> {
        self.sessions.get(session_id).map(|session| AttachIdentity {
            project_id: session.project_id.clone(),
            runner_instance_id: session.runner_instance_id.clone(),
        })
    }

    fn clear_changed_attach_failures(&mut self) {
        let stale: Vec<SessionId> = self
            .attach_failure_identities
            .iter()
            .filter(|(session_id, identity)| {
                self.session_attach_identity(session_id).as_ref() != Some(*identity)
            })
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in stale {
            self.clear_local_attach_failure(&session_id);
        }
    }

    fn clear_unlinked_attach_failures(&mut self) {
        let stale: Vec<SessionId> = self
            .attach_failure_identities
            .keys()
            .filter(|session_id| {
                !self
                    .agents
                    .values()
                    .any(|agent| agent.current_session_id.as_ref() == Some(*session_id))
            })
            .cloned()
            .collect();
        for session_id in stale {
            self.clear_local_attach_failure(&session_id);
        }
    }

    pub fn clear_local_attach_failure(&mut self, session_id: &SessionId) {
        self.attach_retry_after.remove(session_id);
        self.attach_retry_blocked.remove(session_id);
        self.attach_failure_identities.remove(session_id);
        self.local_attention.remove(session_id);
        self.reconcile_attention_focus();
    }

    /// Removes a local attach failure only when the durable session link or its runner generation
    /// no longer matches the failure owner. Detaching a pane because AGENT is no longer desired
    /// must not reopen an identity-matching nonretryable refusal fence.
    pub fn clear_local_attach_failure_if_identity_changed(
        &mut self,
        session_id: &SessionId,
    ) -> bool {
        let failure_identity = self.attach_failure_identities.get(session_id).cloned();
        let durable_identity = self.session_attach_identity(session_id);
        let still_linked = self
            .agents
            .values()
            .any(|agent| agent.current_session_id.as_ref() == Some(session_id));
        if failure_identity != durable_identity || !still_linked {
            self.clear_local_attach_failure(session_id);
            true
        } else {
            false
        }
    }

    pub fn clear_undesired_attach_failures(&mut self, desired: &[SessionId]) -> bool {
        let previous = (
            self.local_attention.len(),
            self.attach_retry_after.len(),
            self.attach_retry_blocked.len(),
        );
        let linked: Vec<SessionId> = self
            .agents
            .values()
            .filter_map(|agent| agent.current_session_id.clone())
            .collect();
        let keep =
            |session_id: &SessionId| desired.contains(session_id) || linked.contains(session_id);
        self.local_attention
            .retain(|session_id, _| keep(session_id));
        self.attach_retry_after
            .retain(|session_id, _| keep(session_id));
        self.attach_retry_blocked
            .retain(|session_id, _| keep(session_id));
        let changed = previous
            != (
                self.local_attention.len(),
                self.attach_retry_after.len(),
                self.attach_retry_blocked.len(),
            );
        if changed {
            self.reconcile_attention_focus();
        }
        changed
    }

    /// Returns true for a first attempt or once a failed attach's bounded
    /// retry deadline has elapsed. Taking an elapsed deadline lets the next
    /// failure establish a fresh delay instead of spinning every frame.
    pub fn take_attach_retry(&mut self, session_id: &SessionId) -> bool {
        if self.attach_retry_blocked.contains_key(session_id) {
            return false;
        }
        match self.attach_retry_after.get(session_id).copied() {
            Some(deadline) if self.now_ms < deadline => false,
            Some(_) => {
                self.attach_retry_after.remove(session_id);
                true
            }
            None => true,
        }
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

    /// The agent targeted by the focused pane, falling back to the selection outside TERMINALS.
    #[must_use]
    pub fn pane_target_agent(&self) -> Option<AgentId> {
        self.selected_agent.clone()
    }

    /// Whether the current view has a live pane keys could actually be forwarded to right now.
    /// `PaneMode::Typing` only ever forwards when this is true — an empty TERMINALS/FOCUS screen
    /// (no live session at all) always leaves every key acting on the board, never silently
    /// eating it as input for a pane that isn't there.
    #[must_use]
    pub(crate) fn has_live_pane(&self) -> bool {
        self.view == View::Agent && self.pane_ready
    }

    /// Which sessions should currently be attached, given `self.view`. FORTRESS/WORKSHOP attach
    /// nothing — "detach without stopping the worker" applies just as much to *leaving* TERMINALS
    /// or FOCUS as it does to quitting the whole client.
    #[must_use]
    pub fn desired_sessions(&self) -> Vec<SessionId> {
        if self.view == View::Agent {
            self.focus_target().into_iter().collect()
        } else {
            Vec::new()
        }
    }

    // -- status/help text -----------------------------------------------------------------

    fn set_status(&mut self, text: impl Into<String>, level: StatusLevel) {
        self.status = Some(StatusMessage {
            text: truncate_status(&text.into(), STATUS_TEXT_MAX_CHARS),
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

    pub fn note_info(&mut self, text: impl Into<String>) {
        self.set_status(text, StatusLevel::Info);
    }

    /// The current mode/modal hint followed by a recent bounded status or error. The footer lays
    /// this variable text between fixed tab and essential-control regions, so clipping cannot
    /// move or hide controls that fit the rendered width.
    #[must_use]
    pub fn status_line_text(&self) -> String {
        let hint = self.help_text();
        match &self.status {
            Some(status) if self.now_ms - status.at_ms < STATUS_STICKY_MS => {
                format!("{hint}   {}", status.text)
            }
            _ => hint,
        }
    }

    #[must_use]
    pub fn status_line_is_error(&self) -> bool {
        self.status.as_ref().is_some_and(|status| {
            status.level == StatusLevel::Error && self.now_ms - status.at_ms < STATUS_STICKY_MS
        })
    }

    /// The current mode or modal safety hint. The footer renders the small permanent help and
    /// detach controls separately instead of repeating the action catalog on every frame.
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
        if self.view == View::Agent {
            if self.pane_mode == PaneMode::Typing && self.has_live_pane() {
                let target = self
                    .pane_target_agent()
                    .map_or_else(|| "pane".to_owned(), |id| id.to_string());
                return format!("TYPING \u{2192} {target}");
            }
            return "BOARD".to_owned();
        }
        if self.view == View::Building {
            return "BOARD".to_owned();
        }
        String::new()
    }

    // -- ticking --------------------------------------------------------------------------

    /// Called on every ~1s tick (and whenever `now_ms` otherwise needs bumping) so elapsed-time
    /// displays and activity series age even for idle agents. The series only gains counts from
    /// durable events; this time-only path never invents activity.
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

    pub fn set_daemon_version(&mut self, version: impl Into<String>) {
        self.daemon_version = Some(version.into());
    }

    /// A stale client must be explicit and actionable. The daemon version is
    /// also the active runtime version because health is answered by the
    /// running factoryd binary, so relaunching only this TUI is sufficient.
    #[must_use]
    pub fn version_mismatch(&self) -> Option<String> {
        let daemon = self.daemon_version.as_deref()?;
        if daemon.is_empty() {
            return Some("STALE TUI — daemon version unknown; detach + relaunch".to_owned());
        }
        (daemon != env!("CARGO_PKG_VERSION")).then(|| {
            format!(
                "STALE TUI v{} / active runtime v{} — detach + relaunch",
                env!("CARGO_PKG_VERSION"),
                daemon
            )
        })
    }

    /// Current activity is deliberately separate from durable lifecycle state.
    /// A recent named hook is useful; an old hook is explicitly stale, and no
    /// sample is explicitly reported rather than rendered as evidence of inactivity.
    #[must_use]
    pub fn activity_label(&self, agent: &AgentSnapshot) -> String {
        let Some(session_id) = agent.current_session_id.as_ref() else {
            return "no recent activity".to_owned();
        };
        let Some(session) = self.sessions.get(session_id) else {
            return "no recent activity".to_owned();
        };
        let Some(at_ms) = session.last_hook_at_ms else {
            return "no recent activity".to_owned();
        };
        let age = age_text(self.now_ms, at_ms);
        if self.now_ms.saturating_sub(at_ms) > 60_000 {
            return format!("stale activity {age} ago");
        }
        let activity = session
            .activity
            .as_deref()
            .map(display_text)
            .unwrap_or_else(|| "recent activity".to_owned());
        format!("{activity} {age} ago")
    }

    /// Sessions that have not ended, fleet-wide — what the daemon's live-session cap counts.
    #[must_use]
    pub fn live_session_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|session| session.ended_at_ms.is_none())
            .count()
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
        self.clear_changed_attach_failures();
        self.clear_unlinked_attach_failures();
        self.prune_activity_to_current_agents();
        self.ensure_default_focus();
    }

    /// Applies a complete snapshot only when its consistency point is not older than a live
    /// event already folded into the board. The sequence check must happen before replacing any
    /// maps; otherwise a delayed refusal refresh can regress a newer current-session link.
    pub fn apply_fleet_snapshot_at(
        &mut self,
        projects: Vec<ProjectSnapshot>,
        agents: Vec<AgentSnapshot>,
        tasks: Vec<TaskDetail>,
        runs: Vec<RunSnapshot>,
        sessions: Vec<SessionSnapshot>,
        event_sequence: i64,
    ) -> bool {
        if (event_sequence >= 0 && event_sequence < self.attention_revision)
            || (self.event_revision >= 0 && event_sequence <= self.event_revision)
        {
            return false;
        }
        self.apply_fleet_snapshot(projects, agents, tasks, runs, sessions);
        self.note_fleet_snapshot_sequence(event_sequence);
        true
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

    /// Whether `event` is worth a new announcement line. Every event type narrates
    /// unconditionally except `SessionChanged`: the daemon emits one for *every* hook
    /// (`SessionStart`/`UserPromptSubmit`/`PreToolUse`/`PostToolUse`/...), and most of those
    /// don't change `state` at all — only `activity`/`last_hook_event`, which the detail pane
    /// already shows. Without this, a session working through one task announces "working"
    /// dozens of times (65 in a few minutes during the first dogfood run) and drowns the handful
    /// of real transitions. Compares against the state this board already has recorded for the
    /// session (`None`, i.e. never seen before, always announces) rather than tracking a separate
    /// "last announced" snapshot — `self.sessions` already *is* that snapshot as of the moment
    /// just before this event folds into it.
    fn should_announce(&self, event: &EventEnvelope) -> bool {
        let FactoryEvent::SessionChanged { session } = &event.event else {
            return true;
        };
        self.sessions
            .get(&session.id)
            .is_none_or(|previous| previous.state != session.state)
    }

    /// Pushes `event`'s announcement (if `worth_announcing` and it produces one via
    /// `announcements::format_event`) unless an announcement for this exact event — by
    /// `sequence`, the daemon's unique event id — is already in the log. The dedupe that keeps
    /// the connect-time backfill (`apply_replay`, issue #67) and the live stream that starts
    /// right after it from ever double-announcing the same event, however they overlap.
    fn maybe_announce(&mut self, event: &EventEnvelope, worth_announcing: bool) {
        if !worth_announcing {
            return;
        }
        if self
            .announcements
            .iter()
            .any(|a| a.sequence == event.sequence)
        {
            return;
        }
        if let Some(announcement) = announcements::format_event(event) {
            self.announcements.push(announcement);
        }
    }

    /// Feeds a batch of already-durable events through the announcement and per-agent activity
    /// paths only — never through [`Board::apply_event`]'s full state fold. Used once at connect
    /// time for the bounded backfill `net::spawn_fleet_session` fetches via `EventsAfter` (oldest
    /// first regardless of the order they arrive in) right before subscribing live: a fresh board
    /// otherwise starts with an empty announcements log and blank activity sparklines, even though
    /// the daemon retains everything (issue #67, and #70's blank-sparkline symptom, which shares
    /// the same root cause — a fresh client had never been fed any of the recent history that made
    /// up the sparkline in the first place).
    ///
    /// Deliberately doesn't fold `event.event` into `agents`/`tasks`/`runs`/`sessions`:
    /// `apply_fleet_snapshot` already has the current, authoritative state for those, and replaying
    /// stale historical snapshots on top of it would regress them. `maybe_announce`'s
    /// sequence-based dedupe covers the case where the live stream (started right after this
    /// backfill, or after a later reconnect) overlaps it.
    pub fn apply_replay(&mut self, mut events: Vec<EventEnvelope>) {
        events.sort_by_key(|event| event.sequence);
        // A local, replay-only view of "what did we last see this session at" — distinct from
        // `should_announce`'s use of `self.sessions` (the board's *current* state), which isn't
        // the right reference point for judging whether an old event in this batch was a real
        // transition at the time.
        let mut last_session_state: HashMap<SessionId, SessionState> = HashMap::new();
        for event in events {
            if !self.admit_event_sequence(event.sequence, false) {
                continue;
            }
            self.apply_event_activity(&event.event, event.occurred_at_ms);
            let worth_announcing = match &event.event {
                FactoryEvent::SessionChanged { session } => {
                    last_session_state.insert(session.id.clone(), session.state)
                        != Some(session.state)
                }
                _ => true,
            };
            self.maybe_announce(&event, worth_announcing);
        }
    }

    pub fn apply_event(&mut self, event: EventEnvelope) {
        if !self.admit_event_sequence(event.sequence, true) {
            return;
        }
        self.apply_event_activity(&event.event, event.occurred_at_ms);

        let worth_announcing = self.should_announce(&event);
        self.maybe_announce(&event, worth_announcing);

        match &event.event {
            FactoryEvent::AgentBudgetChanged { agent_id, .. } => {
                self.invalidate_attention(|item| {
                    item.agent_id.as_ref() == Some(agent_id)
                        && item.reason.kind == AttentionReasonKind::BudgetExhausted
                })
            }
            FactoryEvent::AgentChanged { agent } => self.invalidate_attention(|item| {
                item.agent_id.as_ref() == Some(&agent.id)
                    && item.reason.kind == AttentionReasonKind::PausedWithWork
            }),
            FactoryEvent::TaskChanged { task } => {
                self.invalidate_attention(|item| item.task_id.as_ref() == Some(&task.id));
            }
            FactoryEvent::RunChanged { run } => {
                self.invalidate_attention(|item| item.run_id.as_ref() == Some(&run.id));
            }
            FactoryEvent::SessionChanged { session } => {
                let retain_reason = factory_core::status::session_attention_reason_kind(session);
                self.invalidate_attention(|item| {
                    item.session_id.as_ref() == Some(&session.id)
                        && (retain_reason != Some(item.reason.kind))
                });
            }
            FactoryEvent::TaskDeleted { task_id, .. } => {
                self.invalidate_attention(|item| item.task_id.as_ref() == Some(task_id));
            }
            FactoryEvent::AgentDeleted { agent_id, .. } => {
                self.invalidate_attention(|item| item.agent_id.as_ref() == Some(agent_id));
            }
            FactoryEvent::ProjectDeleted { project_id } => {
                self.invalidate_attention(|item| &item.project_id == project_id);
            }
            FactoryEvent::AutoModeChanged { .. }
            | FactoryEvent::PolicyDecision { .. }
            | FactoryEvent::RepositoryOperation { .. }
            | FactoryEvent::RepositoryAuthorityChanged { .. }
            | FactoryEvent::ProjectChanged { .. } => {}
        }

        match event.event {
            FactoryEvent::AutoModeChanged { .. } | FactoryEvent::PolicyDecision { .. } => {}
            FactoryEvent::AgentBudgetChanged {
                agent_id, paused, ..
            } => {
                if let Some(agent) = self.agents.get_mut(&agent_id) {
                    agent.paused = paused;
                }
            }
            FactoryEvent::RepositoryOperation { .. }
            | FactoryEvent::RepositoryAuthorityChanged { .. } => {}
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
                self.replace_agent(agent);
                self.clear_unlinked_attach_failures();
            }
            FactoryEvent::RunChanged { run } => {
                self.runs.insert(run.id.clone(), run);
            }
            FactoryEvent::SessionChanged { session } => {
                self.sessions.insert(session.id.clone(), session);
                self.clear_changed_attach_failures();
            }
            FactoryEvent::TaskDeleted { task_id, .. } => {
                self.tasks.remove(&task_id);
            }
            FactoryEvent::AgentDeleted { agent_id, .. } => {
                self.agents.remove(&agent_id);
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
                self.tasks.insert(task.snapshot.id.clone(), task);
                format!("task#{id} {}", announcements::task_status_word(status))
            }
            LocalResponse::TaskDeleted { task_id, .. } => {
                self.tasks.remove(&task_id);
                format!("deleted task#{task_id}")
            }
            LocalResponse::AgentCreated { agent } => {
                let id = agent.id.clone();
                self.replace_agent(agent);
                format!("created agent {id}")
            }
            LocalResponse::Agent { agent } | LocalResponse::AgentProfileUpdated { agent } => {
                let id = agent.snapshot.id.clone();
                self.replace_agent(agent.snapshot.clone());
                self.agent_details.insert(id.clone(), agent);
                format!("agent {id} updated")
            }
            LocalResponse::AgentDeleted { agent_id, .. } => {
                self.agents.remove(&agent_id);
                self.remove_activity(&agent_id);
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
            LocalResponse::AgentMessages { messages, .. } => {
                if let Some(agent_id) = messages
                    .first()
                    .map(|message| message.recipient_agent_id.clone())
                {
                    self.messages.insert(agent_id.clone(), messages);
                    format!("loaded messages for {agent_id}")
                } else {
                    "loaded messages".to_owned()
                }
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
        if let Some(id) = &self.selected_task {
            if !self.tasks.contains_key(id) {
                self.selected_task = None;
            }
        }
    }

    /// Admits durable events through the live state-folding sequence boundary.
    /// Attention snapshots use a separate boundary: a snapshot at sequence N
    /// must not reject the live event N that carries the corresponding state.
    fn admit_event_sequence(&mut self, sequence: i64, advances_attention: bool) -> bool {
        if self
            .seen_event_sequences
            .iter()
            .any(|seen| *seen == sequence)
        {
            return false;
        }
        if advances_attention
            && (sequence < self.attention_revision || sequence <= self.event_revision)
        {
            return false;
        }
        self.seen_event_sequences.push(sequence);
        if advances_attention {
            self.event_revision = sequence;
            self.attention_revision = self.attention_revision.max(sequence);
        }
        true
    }

    /// Records or removes the activity projection for one durable event. Keeping lifecycle
    /// cleanup here makes replay and live delivery share the same ownership rules.
    fn apply_event_activity(&mut self, event: &FactoryEvent, occurred_at_ms: i64) {
        match event {
            FactoryEvent::AgentDeleted { agent_id, .. } => self.remove_activity(agent_id),
            FactoryEvent::ProjectDeleted { project_id } => self.remove_project_activity(project_id),
            _ => {
                if let Some((agent_id, identity)) = self.event_agent_identity(event) {
                    if self.activity_event_is_current_generation(
                        &agent_id,
                        &identity,
                        occurred_at_ms,
                    ) {
                        self.record_activity(&agent_id, identity, occurred_at_ms);
                    }
                }
            }
        }
    }

    /// Records durable activity and immediately anchors the series to the board clock. This
    /// keeps replayed history in its true recent bucket, or drops it if it is already outside the
    /// horizon, without waiting for the next repaint tick. Future timestamps are clamped so a
    /// skewed provider cannot pin a live bucket ahead of the board clock.
    fn record_activity(
        &mut self,
        agent_id: &AgentId,
        identity: ActivityIdentity,
        occurred_at_ms: i64,
    ) {
        if self.activity_identities.get(agent_id) != Some(&identity) {
            self.remove_activity(agent_id);
        }
        self.activity_identities
            .insert(agent_id.clone(), identity.clone());
        let occurred_at_ms = occurred_at_ms.min(self.now_ms);
        let series = self.activity.entry(agent_id.clone()).or_default();
        series.record(occurred_at_ms);
        series.roll_to(self.now_ms);
    }

    fn event_agent_identity(&self, event: &FactoryEvent) -> Option<(AgentId, ActivityIdentity)> {
        match event {
            FactoryEvent::TaskChanged { task } => task.assigned_agent_id.as_ref().map(|agent_id| {
                (
                    agent_id.clone(),
                    self.activity_identity(agent_id, &task.project_id),
                )
            }),
            FactoryEvent::RunChanged { run } => Some((
                run.agent_id.clone(),
                self.activity_identity(&run.agent_id, &run.project_id),
            )),
            FactoryEvent::AgentChanged { agent } => Some((
                agent.id.clone(),
                ActivityIdentity {
                    project_id: agent.project_id.clone(),
                    created_at_ms: Some(agent.created_at_ms),
                },
            )),
            FactoryEvent::SessionChanged { session } => Some((
                session.agent_id.clone(),
                self.activity_identity(&session.agent_id, &session.project_id),
            )),
            FactoryEvent::PolicyDecision {
                project_id,
                agent_id,
                ..
            }
            | FactoryEvent::AgentBudgetChanged {
                project_id,
                agent_id,
                ..
            }
            | FactoryEvent::RepositoryOperation {
                project_id,
                agent_id,
                ..
            } => Some((
                agent_id.clone(),
                self.activity_identity(agent_id, project_id),
            )),
            FactoryEvent::AgentDeleted { .. }
            | FactoryEvent::RepositoryAuthorityChanged { .. }
            | FactoryEvent::AutoModeChanged { .. }
            | FactoryEvent::TaskDeleted { .. }
            | FactoryEvent::ProjectChanged { .. }
            | FactoryEvent::ProjectDeleted { .. } => None,
        }
    }

    fn remove_activity(&mut self, agent_id: &AgentId) {
        self.activity.remove(agent_id);
        self.activity_identities.remove(agent_id);
    }

    fn replace_agent(&mut self, agent: AgentSnapshot) {
        let generation_changed = self.agents.get(&agent.id).is_some_and(|current| {
            current.project_id != agent.project_id || current.created_at_ms != agent.created_at_ms
        });
        if generation_changed {
            let id = agent.id.clone();
            self.remove_activity(&id);
        }
        self.agents.insert(agent.id.clone(), agent);
    }

    fn remove_project_activity(&mut self, project_id: &ProjectId) {
        let agent_ids: Vec<AgentId> = self
            .activity_identities
            .iter()
            .filter(|(_, identity)| identity.project_id == *project_id)
            .map(|(agent_id, _)| agent_id.clone())
            .collect();
        for agent_id in agent_ids {
            self.remove_activity(&agent_id);
        }
    }

    fn prune_activity_to_current_agents(&mut self) {
        self.activity.retain(|agent_id, _| {
            self.agents.get(agent_id).is_some_and(|agent| {
                self.activity_identities
                    .get(agent_id)
                    .is_some_and(|identity| {
                        identity.project_id == agent.project_id
                            && identity.created_at_ms == Some(agent.created_at_ms)
                    })
            })
        });
        self.activity_identities.retain(|agent_id, identity| {
            self.activity.contains_key(agent_id)
                && self.agents.get(agent_id).is_some_and(|agent| {
                    identity.project_id == agent.project_id
                        && identity.created_at_ms == Some(agent.created_at_ms)
                })
        });
    }

    fn activity_identity(&self, agent_id: &AgentId, project_id: &ProjectId) -> ActivityIdentity {
        ActivityIdentity {
            project_id: project_id.clone(),
            created_at_ms: self
                .agents
                .get(agent_id)
                .filter(|agent| agent.project_id == *project_id)
                .map(|agent| agent.created_at_ms),
        }
    }

    fn activity_event_is_current_generation(
        &self,
        agent_id: &AgentId,
        identity: &ActivityIdentity,
        occurred_at_ms: i64,
    ) -> bool {
        self.agents.get(agent_id).is_none_or(|agent| {
            agent.project_id == identity.project_id
                && occurred_at_ms >= agent.created_at_ms
                && identity
                    .created_at_ms
                    .is_none_or(|created_at_ms| created_at_ms == agent.created_at_ms)
        })
    }
}

pub(crate) fn same_attention_source(a: &AttentionItem, b: &AttentionItem) -> bool {
    a.reason.kind == b.reason.kind
        && a.project_id == b.project_id
        && a.agent_id == b.agent_id
        && a.task_id == b.task_id
        && a.session_id == b.session_id
        && a.run_id == b.run_id
        && a.since_ms == b.since_ms
}

fn truncate_status(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let head: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{head}\u{2026}")
    }
}

fn error_code_word(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidRequest => "invalid request",
        ErrorCode::Unauthorized => "unauthorized",
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
