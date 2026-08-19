//! Fleet and per-agent status: what `factoryctl status` and `factoryctl
//! agent status` return, built by `factoryd` from durable state in one
//! store transaction so every field is from the same instant. `factory-tui`
//! reads the same responses; it never computes a status the CLI can't
//! show.
//!
//! Bounded in what it carries — live/most-recent sessions, current runs,
//! queued and blocked tasks (the full task/run/session ledgers stay behind
//! the paginated `List*` requests), per-agent queue previews truncated to
//! [`MAX_QUEUE_PREVIEW`] with the true depth alongside — but not paginated:
//! project backlogs and blocked tasks are listed in full, so a factory with
//! thousands of open tasks would push one frame past
//! `MAX_LOCAL_FRAME_BYTES`; that is where pagination would go.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    AgentBudget, AgentId, AgentSnapshot, ProjectId, ProjectSnapshot, ProviderNotificationKind,
    RunSnapshot, SessionId, SessionSnapshot, TaskId, TaskSnapshot,
    attention::{Attention, task_attention},
    local::AgentDetail,
};

/// How many queued tasks an [`AgentStatus`]/[`ProjectStatus`] lists in full
/// before falling back to the depth count alone.
pub const MAX_QUEUE_PREVIEW: usize = 10;
/// Maximum displayed characters in any operator-controlled attention summary.
pub const MAX_ATTENTION_SUMMARY_CHARS: usize = 160;

const fn legacy_event_sequence() -> i64 {
    -1
}

/// The whole daemon at one instant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FleetStatus {
    pub generated_at_ms: i64,
    /// Durable event high-water mark from the same store read as this snapshot.
    /// Old v1 daemons omit it; the negative sentinel keeps that legacy mode
    /// distinguishable from a sequenced daemon whose durable head is zero.
    #[serde(default = "legacy_event_sequence")]
    pub event_sequence: i64,
    /// Factory-wide provider bypass. Explicit per-agent permission modes
    /// still override this default.
    #[serde(default)]
    pub auto_mode: bool,
    /// `factoryd --max-active-runs`: the daemon-wide cap on live sessions.
    pub live_session_cap: u32,
    /// Sessions that have not ended, across every project.
    pub live_sessions: u32,
    pub projects: Vec<ProjectStatus>,
    /// Everything that needs an operator right now, most urgent first.
    pub attention: Vec<AttentionItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectStatus {
    pub project: ProjectSnapshot,
    pub agents: Vec<AgentStatus>,
    /// Project-backlog tasks not assigned to any agent (the operator's or an
    /// orchestrator's to hand out).
    pub backlog_depth: u32,
    pub backlog: Vec<TaskSnapshot>,
}

/// One agent's live picture: its session (if any), current run, and what is
/// waiting for it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentStatus {
    pub agent: AgentSnapshot,
    #[serde(default)]
    pub budget: AgentBudget,
    /// Independent durable reasons contributing to `agent.paused`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pause_reasons: Vec<AgentPauseReason>,
    /// Current git state of the agent's working directory. Fleet status
    /// includes this so an orchestrator can inspect work before deciding
    /// whether a stopped or blocked worker is safe to recover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run: Option<RunSnapshot>,
    /// Most recently started run, including a terminal run used as the
    /// source of inferred attention when there is no live session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run: Option<RunSnapshot>,
    /// Active tasks assigned to this agent in the canonical queue order (see
    /// [`MAX_QUEUE_PREVIEW`]): running first, then priority/creation order.
    pub queue_depth: u32,
    pub queue: Vec<TaskSnapshot>,
    /// Inbox messages not yet delivered into a session.
    pub inbox_pending: u32,
    /// The agent's attention level, and whether it was read from its
    /// session (`inferred == false`) or from its run (no session).
    pub attention: Attention,
    pub attention_inferred: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPauseReason {
    /// The ordinary agent hold set by `agent pause` or reliability logic.
    AgentHold,
    BudgetExhausted,
}

/// `factoryctl agent status`: everything [`AgentStatus`] has, plus the
/// profile and guidance paths. `worktree` is retained at the old outer
/// location for compatibility while [`AgentStatus::worktree`] is shared
/// with fleet status.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentStatusDetail {
    /// The instant this projection was built, used to render stable ages.
    #[serde(default)]
    pub generated_at_ms: i64,
    /// Durable event high-water mark from the same store read as this status.
    #[serde(default = "legacy_event_sequence")]
    pub event_sequence: i64,
    pub status: AgentStatus,
    pub detail: AgentDetail,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeStatus>,
    /// The same structured projection returned in [`FleetStatus::attention`],
    /// limited to this agent and its blocked tasks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attention: Vec<AttentionItem>,
}

/// `git status --porcelain --branch` of an agent's worktree, summarized.
/// Present whenever the agent has a worktree; if `git` could not report on
/// it (deleted directory, not a repository, ...) `error` says why and the
/// counts are zero — never silently "clean".
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorktreeStatus {
    pub path: String,
    /// `None` on a detached `HEAD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Modified, staged, or untracked entries.
    pub changed_files: u32,
    pub dirty: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    NeedsInput,
    BudgetExhausted,
    SessionFailed,
    TaskBlocked,
    PausedWithWork,
    WaitingForCapacity,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReasonKind {
    /// A provider surfaced a bounded question that can be answered in its terminal.
    ProviderQuestion,
    /// A provider is stopped at its own approval prompt.
    ProviderPermission,
    /// A worker explicitly blocked a durable task with a reason.
    WorkerBlocked,
    /// Delivery or resident-session recovery needs observation, not a human answer.
    DeliveryRecovery,
    /// The daemon cannot reliably observe the runner/session.
    ObserverProblem,
    /// The agent's durable provider budget must be reset or raised.
    BudgetExhausted,
    /// Attention inferred from run lifecycle because no live session reported a cause.
    Inferred,
    /// An agent has queued work but is paused.
    PausedWithWork,
    /// An agent has queued work and no session, and the daemon is at its
    /// live-session cap.
    WaitingForCapacity,
}

impl AttentionReasonKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ProviderQuestion => "provider question",
            Self::ProviderPermission => "provider permission",
            Self::WorkerBlocked => "worker blocked",
            Self::DeliveryRecovery => "delivery/recovery",
            Self::ObserverProblem => "observer problem",
            Self::BudgetExhausted => "budget exhausted",
            Self::Inferred => "inference",
            Self::PausedWithWork => "paused with work",
            Self::WaitingForCapacity => "waiting for capacity",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionAction {
    AnswerInTerminal,
    ReviewProviderPermission,
    InspectRecovery,
    InspectObserver,
    ResetBudget,
    RetryTask,
    InspectInferredState,
    ResumeAgent,
    WaitForCapacity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttentionReason {
    pub kind: AttentionReasonKind,
    /// Bounded and control-safe at projection time, so every client sees the
    /// same operator-facing text.
    pub summary: String,
    pub action: AttentionAction,
}

/// One thing that needs the operator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionItem {
    pub level: Attention,
    pub project_id: ProjectId,
    pub agent_id: Option<AgentId>,
    pub task_id: Option<TaskId>,
    pub session_id: Option<SessionId>,
    pub run_id: Option<crate::RunId>,
    /// When this condition began (session state change, task update, ...).
    pub since_ms: i64,
    pub reason: AttentionReason,
}

#[derive(Serialize)]
struct AttentionItemRef<'a> {
    kind: AttentionKind,
    level: Attention,
    project_id: &'a ProjectId,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: &'a Option<AgentId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: &'a Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: &'a Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: &'a Option<crate::RunId>,
    since_ms: i64,
    detail: &'a str,
    reason: &'a AttentionReason,
}

#[derive(Deserialize)]
struct AttentionItemWire {
    kind: AttentionKind,
    level: Attention,
    project_id: ProjectId,
    #[serde(default)]
    agent_id: Option<AgentId>,
    #[serde(default)]
    task_id: Option<TaskId>,
    #[serde(default)]
    session_id: Option<SessionId>,
    #[serde(default)]
    run_id: Option<crate::RunId>,
    since_ms: i64,
    detail: String,
    #[serde(default)]
    reason: Option<AttentionReason>,
}

impl Serialize for AttentionItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        AttentionItemRef {
            kind: self.legacy_kind(),
            level: self.level,
            project_id: &self.project_id,
            agent_id: &self.agent_id,
            task_id: &self.task_id,
            session_id: &self.session_id,
            run_id: &self.run_id,
            since_ms: self.since_ms,
            detail: &self.reason.summary,
            reason: &self.reason,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AttentionItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = AttentionItemWire::deserialize(deserializer)?;
        let reason = wire
            .reason
            .unwrap_or_else(|| legacy_reason(wire.kind, wire.detail));
        Ok(Self {
            level: wire.level,
            project_id: wire.project_id,
            agent_id: wire.agent_id,
            task_id: wire.task_id,
            session_id: wire.session_id,
            run_id: wire.run_id,
            since_ms: wire.since_ms,
            reason,
        })
    }
}

impl AttentionItem {
    fn legacy_kind(&self) -> AttentionKind {
        match self.reason.kind {
            AttentionReasonKind::ProviderQuestion | AttentionReasonKind::ProviderPermission => {
                AttentionKind::NeedsInput
            }
            AttentionReasonKind::WorkerBlocked => AttentionKind::TaskBlocked,
            AttentionReasonKind::DeliveryRecovery | AttentionReasonKind::ObserverProblem => {
                AttentionKind::SessionFailed
            }
            AttentionReasonKind::Inferred if self.level == Attention::NeedsInput => {
                AttentionKind::NeedsInput
            }
            AttentionReasonKind::Inferred => AttentionKind::SessionFailed,
            AttentionReasonKind::BudgetExhausted => AttentionKind::BudgetExhausted,
            AttentionReasonKind::PausedWithWork => AttentionKind::PausedWithWork,
            AttentionReasonKind::WaitingForCapacity => AttentionKind::WaitingForCapacity,
        }
    }

    /// A safe, shared action description. Commands contain only validated IDs.
    #[must_use]
    pub fn action_text(&self) -> String {
        match self.reason.action {
            AttentionAction::AnswerInTerminal => {
                "review the question, then enter terminal typing".to_owned()
            }
            AttentionAction::ReviewProviderPermission => {
                "review the provider prompt before entering terminal typing".to_owned()
            }
            AttentionAction::InspectRecovery => self.session_id.as_ref().map_or_else(
                || "inspect daemon recovery; no human answer is pending".to_owned(),
                |session_id| format!("inspect session {session_id}; no human answer is pending"),
            ),
            AttentionAction::InspectObserver => self.session_id.as_ref().map_or_else(
                || "inspect observer health before taking control".to_owned(),
                |session_id| format!("inspect observer health for session {session_id}"),
            ),
            AttentionAction::ResetBudget => self.agent_id.as_ref().map_or_else(
                || "reset or raise the agent budget".to_owned(),
                |agent_id| {
                    format!(
                        "factoryctl agent budget reset --project {} --agent {agent_id}",
                        self.project_id
                    )
                },
            ),
            AttentionAction::RetryTask => self.task_id.as_ref().map_or_else(
                || "retry the blocked task after resolving its reason".to_owned(),
                |task_id| {
                    format!(
                        "factoryctl task retry --project {} --task {task_id}",
                        self.project_id
                    )
                },
            ),
            AttentionAction::InspectInferredState => self.run_id.as_ref().map_or_else(
                || "inspect the recorded run before choosing a recovery action".to_owned(),
                |run_id| format!("inspect run {run_id} before choosing a recovery action"),
            ),
            AttentionAction::ResumeAgent => self.agent_id.as_ref().map_or_else(
                || "resume the paused agent".to_owned(),
                |agent_id| {
                    format!(
                        "factoryctl agent resume --project {} --agent {agent_id}",
                        self.project_id
                    )
                },
            ),
            AttentionAction::WaitForCapacity => {
                "wait for capacity or stop an unneeded live session".to_owned()
            }
        }
    }
}

fn legacy_reason(kind: AttentionKind, detail: String) -> AttentionReason {
    let (kind, action, fallback) = match kind {
        AttentionKind::NeedsInput => (
            AttentionReasonKind::Inferred,
            AttentionAction::InspectInferredState,
            "session needs attention",
        ),
        AttentionKind::BudgetExhausted => (
            AttentionReasonKind::BudgetExhausted,
            AttentionAction::ResetBudget,
            "agent budget is exhausted",
        ),
        AttentionKind::SessionFailed => (
            AttentionReasonKind::DeliveryRecovery,
            AttentionAction::InspectRecovery,
            "session needs recovery",
        ),
        AttentionKind::TaskBlocked => (
            AttentionReasonKind::WorkerBlocked,
            AttentionAction::RetryTask,
            "task is blocked",
        ),
        AttentionKind::PausedWithWork => (
            AttentionReasonKind::PausedWithWork,
            AttentionAction::ResumeAgent,
            "agent is paused with queued work",
        ),
        AttentionKind::WaitingForCapacity => (
            AttentionReasonKind::WaitingForCapacity,
            AttentionAction::WaitForCapacity,
            "queued work is waiting for capacity",
        ),
    };
    reason(kind, detail, fallback, action)
}

/// A blocked task and its durable worker-supplied reason. Kept separate
/// from [`TaskSnapshot`] because full task detail is not part of fleet status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockedTaskStatus {
    pub task: TaskSnapshot,
    pub reason: Option<String>,
}

/// Derives the attention list for one project's agents and blocked tasks.
/// Pure over snapshots so `factoryd` (for `factoryctl status`) and any
/// client can agree on it. Unsorted; see [`sort_attention`].
#[must_use]
pub fn attention_items(
    project_id: &ProjectId,
    agents: &[AgentStatus],
    blocked_tasks: &[BlockedTaskStatus],
    at_capacity: bool,
) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    for status in agents {
        let agent = &status.agent;
        // Fleet status projects all active assigned work, running first. Keep
        // queued-only attention derived from that shared projection so an old
        // v1 daemon (whose queue contains only queued rows) remains compatible.
        let running_depth = u32::try_from(
            status
                .queue
                .iter()
                .filter(|task| task.status == crate::TaskStatus::Running)
                .count(),
        )
        .unwrap_or(u32::MAX);
        let queued_depth = status.queue_depth.saturating_sub(running_depth);
        let next_queued = status
            .queue
            .iter()
            .find(|task| task.status == crate::TaskStatus::Queued);
        if status.budget.exhausted {
            let source_run = status.current_run.as_ref();
            items.push(AttentionItem {
                level: Attention::NeedsInput,
                project_id: project_id.clone(),
                agent_id: Some(agent.id.clone()),
                task_id: source_run.and_then(|run| run.task_id.clone()),
                session_id: source_run.and_then(|run| run.session_id.clone()),
                run_id: source_run.map(|run| run.id.clone()),
                since_ms: status.budget.updated_at_ms,
                reason: reason(
                    AttentionReasonKind::BudgetExhausted,
                    format!("tool-call budget exhausted at {}", status.budget.tool_calls),
                    "tool-call budget exhausted",
                    AttentionAction::ResetBudget,
                ),
            });
        }
        if let Some(session) = &status.session
            && let Some((level, reason)) = session_reason(session)
        {
            let source_run = status.current_run.as_ref().filter(|run| {
                session.current_run_id.as_ref() == Some(&run.id)
                    && run.session_id.as_ref() == Some(&session.id)
            });
            let queued_delivery = (session.state == crate::SessionState::WaitingForInput
                && session.wait_reason.as_deref() == Some("delivery unacknowledged"))
            .then_some(next_queued)
            .flatten();
            items.push(AttentionItem {
                level,
                project_id: project_id.clone(),
                agent_id: Some(agent.id.clone()),
                task_id: source_run
                    .and_then(|run| run.task_id.clone())
                    .or_else(|| queued_delivery.map(|task| task.id.clone())),
                session_id: Some(session.id.clone()),
                run_id: source_run.map(|run| run.id.clone()),
                since_ms: if session.observer_health == crate::ObserverHealth::Degraded {
                    session.observer_health_since_ms
                } else {
                    session.state_since_ms
                },
                reason,
            });
        }
        if status.attention_inferred && status.attention.needs_operator() {
            let latest_run = status.latest_run.as_ref();
            let (summary, fallback) = latest_run.map_or_else(
                || (None, "lifecycle state needs attention"),
                |run| {
                    (
                        run.wait_reason.clone(),
                        if run.status == crate::RunStatus::Failed {
                            "latest run failed without a live session report"
                        } else {
                            "latest run needs attention without a live session report"
                        },
                    )
                },
            );
            items.push(AttentionItem {
                level: status.attention,
                project_id: project_id.clone(),
                agent_id: Some(agent.id.clone()),
                task_id: latest_run.and_then(|run| run.task_id.clone()),
                session_id: latest_run.and_then(|run| run.session_id.clone()),
                run_id: latest_run.map(|run| run.id.clone()),
                since_ms: latest_run.map_or(agent.updated_at_ms, |run| run.status_since_ms),
                reason: reason(
                    AttentionReasonKind::Inferred,
                    summary.unwrap_or_else(|| fallback.to_owned()),
                    fallback,
                    AttentionAction::InspectInferredState,
                ),
            });
        }
        if queued_depth > 0 {
            if agent.paused && !status.budget.exhausted {
                items.push(AttentionItem {
                    level: Attention::NeedsInput,
                    project_id: project_id.clone(),
                    agent_id: Some(agent.id.clone()),
                    task_id: next_queued.map(|task| task.id.clone()),
                    session_id: None,
                    run_id: None,
                    since_ms: status
                        .queue
                        .iter()
                        .find(|task| task.status == crate::TaskStatus::Queued)
                        .map_or(agent.updated_at_ms, |task| task.created_at_ms),
                    reason: reason(
                        AttentionReasonKind::PausedWithWork,
                        format!("paused with {queued_depth} queued task(s)"),
                        "paused with queued work",
                        AttentionAction::ResumeAgent,
                    ),
                });
            } else if agent.current_session_id.is_none() && at_capacity {
                items.push(AttentionItem {
                    level: Attention::Routine,
                    project_id: project_id.clone(),
                    agent_id: Some(agent.id.clone()),
                    task_id: next_queued.map(|task| task.id.clone()),
                    session_id: None,
                    run_id: None,
                    since_ms: status
                        .queue
                        .iter()
                        .find(|task| task.status == crate::TaskStatus::Queued)
                        .map_or(agent.updated_at_ms, |task| task.updated_at_ms),
                    reason: reason(
                        AttentionReasonKind::WaitingForCapacity,
                        format!(
                            "{queued_depth} queued task(s) but the daemon is at its live-session cap"
                        ),
                        "queued work is waiting for live-session capacity",
                        AttentionAction::WaitForCapacity,
                    ),
                });
            }
        }
    }
    for blocked in blocked_tasks {
        let task = &blocked.task;
        if task_attention(task.status) == Attention::NeedsInput {
            items.push(AttentionItem {
                level: Attention::NeedsInput,
                project_id: project_id.clone(),
                agent_id: task.assigned_agent_id.clone(),
                task_id: Some(task.id.clone()),
                session_id: None,
                run_id: None,
                since_ms: task.updated_at_ms,
                reason: reason(
                    AttentionReasonKind::WorkerBlocked,
                    blocked
                        .reason
                        .clone()
                        .unwrap_or_else(|| format!("task \"{}\" is blocked", task.title)),
                    "worker blocked the task without a readable reason",
                    AttentionAction::RetryTask,
                ),
            });
        }
    }
    items
}

/// Returns the structured reason currently authoritative for one session,
/// using the same provider/observer/delivery precedence as fleet status.
#[must_use]
pub fn session_attention_reason_kind(session: &SessionSnapshot) -> Option<AttentionReasonKind> {
    session_reason(session).map(|(_, reason)| reason.kind)
}

fn session_reason(session: &SessionSnapshot) -> Option<(Attention, AttentionReason)> {
    use crate::{ObserverHealth, ProviderHookEvent, SessionState};

    if session.observer_health == ObserverHealth::Degraded {
        return Some((
            Attention::Failed,
            reason(
                AttentionReasonKind::ObserverProblem,
                session
                    .observer_reason
                    .clone()
                    .unwrap_or_else(|| "runner observation is degraded".to_owned()),
                "runner observation is degraded",
                AttentionAction::InspectObserver,
            ),
        ));
    }
    if session.state == SessionState::WaitingForInput {
        let summary = session.wait_reason.clone();
        // This exact reason is owned by the daemon's delivery dispatcher. The
        // synthetic transition deliberately retains the preceding hook event,
        // so it must outrank that stale provenance. Never infer this state from
        // words in provider-controlled prompt text.
        if summary.as_deref() == Some("delivery unacknowledged") {
            return Some((
                Attention::Failed,
                reason(
                    AttentionReasonKind::DeliveryRecovery,
                    summary.unwrap(),
                    "delivery recovery is pending",
                    AttentionAction::InspectRecovery,
                ),
            ));
        }
        if session.last_hook_event == Some(ProviderHookEvent::PermissionRequest) {
            return Some((
                Attention::NeedsInput,
                reason(
                    AttentionReasonKind::ProviderPermission,
                    summary.unwrap_or_else(|| "provider approval prompt".to_owned()),
                    "provider approval prompt",
                    AttentionAction::ReviewProviderPermission,
                ),
            ));
        }
        match (
            session.last_hook_event,
            session.notification_kind,
            summary.clone(),
        ) {
            (
                Some(ProviderHookEvent::Notification),
                Some(ProviderNotificationKind::PermissionPrompt),
                Some(summary),
            ) => {
                return Some((
                    Attention::NeedsInput,
                    reason(
                        AttentionReasonKind::ProviderPermission,
                        summary,
                        "provider approval prompt",
                        AttentionAction::ReviewProviderPermission,
                    ),
                ));
            }
            (
                Some(ProviderHookEvent::Notification),
                Some(
                    ProviderNotificationKind::ElicitationDialog
                    | ProviderNotificationKind::ElicitationUrlDialog
                    | ProviderNotificationKind::AgentNeedsInput,
                ),
                Some(summary),
            ) => {
                return Some((
                    Attention::NeedsInput,
                    reason(
                        AttentionReasonKind::ProviderQuestion,
                        summary,
                        "provider is waiting for an answer",
                        AttentionAction::AnswerInTerminal,
                    ),
                ));
            }
            _ => {}
        }
        // A legacy or malformed Notification without a typed cause is not
        // evidence of an answerable prompt. Keep it routine until a typed
        // actionable hook arrives.
        if session.last_hook_event == Some(ProviderHookEvent::Notification) {
            return None;
        }
        return Some((
            Attention::NeedsInput,
            reason(
                AttentionReasonKind::Inferred,
                summary
                    .unwrap_or_else(|| "session is waiting without a reported question".to_owned()),
                "session is waiting without a reported question",
                AttentionAction::InspectInferredState,
            ),
        ));
    }
    (session.state == SessionState::Failed).then(|| {
        (
            Attention::Failed,
            reason(
                AttentionReasonKind::DeliveryRecovery,
                session
                    .wait_reason
                    .clone()
                    .unwrap_or_else(|| "session failed and needs recovery".to_owned()),
                "session failed and needs recovery",
                AttentionAction::InspectRecovery,
            ),
        )
    })
}

fn reason(
    kind: AttentionReasonKind,
    summary: String,
    fallback: &str,
    action: AttentionAction,
) -> AttentionReason {
    let summary = display_text(&summary);
    AttentionReason {
        kind,
        summary: if summary.is_empty() {
            fallback.to_owned()
        } else {
            summary
        },
        action,
    }
}

/// Normalizes whitespace, removes terminal and bidirectional display controls,
/// and bounds operator-controlled text by displayed Unicode scalar count.
#[must_use]
pub fn display_text(value: &str) -> String {
    let mut output = String::new();
    let mut length = 0;
    let mut pending_space = false;
    let mut truncated = false;

    for character in value.chars() {
        if character.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if character.is_control() || is_bidi_control(character) {
            continue;
        }
        let separator = usize::from(pending_space && !output.is_empty());
        if length + separator + 1 > MAX_ATTENTION_SUMMARY_CHARS {
            truncated = true;
            break;
        }
        if separator == 1 {
            output.push(' ');
            length += 1;
        }
        output.push(character);
        length += 1;
        pending_space = false;
    }

    if truncated {
        if length == MAX_ATTENTION_SUMMARY_CHARS {
            output.pop();
        }
        output.push('…');
    }
    output
}

const fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Compact stable age used by CLI and TUI action cards.
#[must_use]
pub fn age_text(now_ms: i64, since_ms: i64) -> String {
    let seconds = now_ms.saturating_sub(since_ms).max(0) / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

/// Most urgent first, then oldest first — the order `factoryctl status`
/// prints and the board would triage in.
pub fn sort_attention(items: &mut [AttentionItem]) {
    items.sort_by(|a, b| {
        b.level
            .cmp(&a.level)
            .then_with(|| a.since_ms.cmp(&b.since_ms))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AgentRole, ObserverHealth, Provider, ProviderHookEvent, RunId, RunStatus, SessionState,
        TaskStatus, attention::session_attention,
    };

    fn agent(id: &str, paused: bool) -> AgentSnapshot {
        AgentSnapshot {
            id: AgentId::try_from(id).unwrap(),
            project_id: ProjectId::try_from("p").unwrap(),
            parent_agent_id: None,
            role: AgentRole::Worker,
            provider: Provider::Shell,
            current_run_id: None,
            paused,
            current_session_id: None,
            worktree: None,
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    fn session(state: SessionState, since: i64) -> SessionSnapshot {
        SessionSnapshot {
            id: SessionId::try_from("s1").unwrap(),
            project_id: ProjectId::try_from("p").unwrap(),
            agent_id: AgentId::try_from("a").unwrap(),
            provider: Provider::Shell,
            runtime_model: None,
            runtime_reasoning_effort: None,
            runtime_permission_mode: None,
            runtime_control_mode: None,
            state,
            state_since_ms: since,
            worktree: "/w".to_owned(),
            provider_session_id: None,
            current_run_id: None,
            activity: None,
            activity_inferred: false,
            last_hook_event: None,
            notification_kind: None,
            last_hook_at_ms: None,
            wait_reason: None,
            observer_reason: None,
            observer_health: ObserverHealth::Unknown,
            observer_health_since_ms: 0,
            started_at_ms: 0,
            updated_at_ms: 0,
            ended_at_ms: None,
            exit_code: None,
            exit_signal: None,
        }
    }

    fn task(id: &str, status: TaskStatus, updated: i64) -> TaskSnapshot {
        TaskSnapshot {
            id: TaskId::try_from(id).unwrap(),
            project_id: ProjectId::try_from("p").unwrap(),
            parent_task_id: None,
            assigned_agent_id: Some(AgentId::try_from("a").unwrap()),
            title: format!("task {id}"),
            status,
            priority: 0,
            created_at_ms: 0,
            updated_at_ms: updated,
        }
    }

    fn status(
        agent: AgentSnapshot,
        session: Option<SessionSnapshot>,
        queue: Vec<TaskSnapshot>,
    ) -> AgentStatus {
        AgentStatus {
            budget: AgentBudget::default(),
            pause_reasons: Vec::new(),
            queue_depth: u32::try_from(queue.len()).unwrap(),
            attention: session
                .as_ref()
                .map_or(Attention::Routine, |s| session_attention(s.state)),
            attention_inferred: session.is_none(),
            agent,
            worktree: None,
            session,
            current_run: None,
            latest_run: None,
            queue,
            inbox_pending: 0,
        }
    }

    #[test]
    fn attention_orders_needs_input_above_failed_and_oldest_first_within_a_level() {
        let project = ProjectId::try_from("p").unwrap();
        let agents = vec![
            status(
                agent("a", false),
                Some(session(SessionState::Failed, 10)),
                vec![],
            ),
            status(
                agent("b", false),
                Some(session(SessionState::WaitingForInput, 30)),
                vec![],
            ),
            status(
                agent("c", true),
                None,
                vec![task("t1", TaskStatus::Queued, 5)],
            ),
            status(
                agent("d", false),
                None,
                vec![task("t2", TaskStatus::Queued, 40)],
            ),
        ];
        let blocked = vec![BlockedTaskStatus {
            task: task("t3", TaskStatus::Blocked, 20),
            reason: Some("dependency is unavailable".to_owned()),
        }];
        let mut items = attention_items(&project, &agents, &blocked, true);
        sort_attention(&mut items);
        let kinds: Vec<AttentionReasonKind> = items.iter().map(|item| item.reason.kind).collect();
        assert_eq!(
            kinds,
            [
                AttentionReasonKind::PausedWithWork,
                AttentionReasonKind::WorkerBlocked,
                AttentionReasonKind::Inferred,
                AttentionReasonKind::DeliveryRecovery,
                AttentionReasonKind::WaitingForCapacity,
            ]
        );
        // Not at capacity: no WaitingForCapacity item.
        assert!(
            attention_items(&project, &agents, &blocked, false)
                .iter()
                .all(|item| item.reason.kind != AttentionReasonKind::WaitingForCapacity)
        );
    }

    #[test]
    fn waiting_for_capacity_means_no_live_session_not_no_session_ever() {
        // An agent whose latest session ENDED still has no live session: with
        // queued work at the cap it is waiting for capacity. One whose
        // session is live is not, whatever its state.
        let project = ProjectId::try_from("p").unwrap();
        let mut ended = session(SessionState::Stopped, 5);
        ended.ended_at_ms = Some(6);
        let waiting = status(
            agent("a", false),
            Some(ended),
            vec![task("t1", TaskStatus::Queued, 7)],
        );
        let mut live_agent = agent("b", false);
        live_agent.current_session_id = Some(SessionId::try_from("s1").unwrap());
        let live = status(
            live_agent,
            Some(session(SessionState::Idle, 8)),
            vec![task("t2", TaskStatus::Queued, 9)],
        );
        let kinds: Vec<AttentionReasonKind> =
            attention_items(&project, &[waiting, live], &[], true)
                .iter()
                .map(|item| item.reason.kind)
                .collect();
        assert_eq!(kinds, [AttentionReasonKind::WaitingForCapacity]);
    }

    #[test]
    fn a_working_session_and_an_empty_queue_need_no_attention() {
        let project = ProjectId::try_from("p").unwrap();
        let agents = vec![status(
            agent("a", false),
            Some(session(SessionState::Working, 1)),
            vec![],
        )];
        assert!(attention_items(&project, &agents, &[], true).is_empty());
    }

    fn run(status: RunStatus, reason: Option<&str>) -> RunSnapshot {
        RunSnapshot {
            id: RunId::try_from("run-1").unwrap(),
            project_id: ProjectId::try_from("p").unwrap(),
            agent_id: AgentId::try_from("a").unwrap(),
            parent_run_id: None,
            task_id: Some(TaskId::try_from("t1").unwrap()),
            session_id: None,
            status,
            activity: None,
            wait_reason: reason.map(str::to_owned),
            worktree: "/w".to_owned(),
            observer_health: ObserverHealth::Unknown,
            observer_health_since_ms: 0,
            started_at_ms: 40,
            status_since_ms: 50,
            updated_at_ms: 50,
            ended_at_ms: None,
            exit_code: None,
            exit_signal: None,
            failure_reason: None,
            closed_by: None,
        }
    }

    #[test]
    fn structured_reasons_distinguish_every_actionable_source_and_sanitize_text() {
        let project = ProjectId::try_from("p").unwrap();
        let mut question = session(SessionState::WaitingForInput, 10);
        question.last_hook_event = Some(ProviderHookEvent::Notification);
        question.wait_reason = Some("Which branch?".to_owned());
        for kind in [
            ProviderNotificationKind::ElicitationDialog,
            ProviderNotificationKind::ElicitationUrlDialog,
            ProviderNotificationKind::AgentNeedsInput,
        ] {
            question.notification_kind = Some(kind);
            let (_, reason) = session_reason(&question).unwrap();
            assert_eq!(reason.kind, AttentionReasonKind::ProviderQuestion);
        }
        let mut permission = session(SessionState::WaitingForInput, 20);
        permission.id = SessionId::try_from("s2").unwrap();
        permission.last_hook_event = Some(ProviderHookEvent::PermissionRequest);
        permission.wait_reason = Some("Approve shell command?".to_owned());
        let mut delivery = session(SessionState::WaitingForInput, 30);
        delivery.id = SessionId::try_from("s3").unwrap();
        delivery.wait_reason = Some("delivery unacknowledged".to_owned());
        let mut observer = session(SessionState::Working, 40);
        observer.id = SessionId::try_from("s4").unwrap();
        observer.observer_health = ObserverHealth::Degraded;
        observer.observer_health_since_ms = 35;

        let mut agents = vec![
            status(agent("a", false), Some(question), vec![]),
            status(agent("b", false), Some(permission), vec![]),
            status(agent("c", false), Some(delivery), vec![]),
            status(agent("d", false), Some(observer), vec![]),
        ];
        agents[0].budget = AgentBudget {
            exhausted: true,
            tool_calls: 1_000,
            updated_at_ms: 5,
            ..AgentBudget::default()
        };
        let mut inferred = status(agent("e", false), None, vec![]);
        inferred.attention = Attention::NeedsInput;
        inferred.attention_inferred = true;
        inferred.latest_run = Some(run(RunStatus::Waiting, Some("awaiting lifecycle evidence")));
        agents.push(inferred);

        let blocked = [BlockedTaskStatus {
            task: task("t1", TaskStatus::Blocked, 60),
            reason: Some(format!(
                "blocked\n\u{1b}[2J{}",
                "x".repeat(MAX_ATTENTION_SUMMARY_CHARS + 20)
            )),
        }];
        let items = attention_items(&project, &agents, &blocked, false);
        let kinds: Vec<_> = items.iter().map(|item| item.reason.kind).collect();
        for expected in [
            AttentionReasonKind::ProviderQuestion,
            AttentionReasonKind::ProviderPermission,
            AttentionReasonKind::WorkerBlocked,
            AttentionReasonKind::DeliveryRecovery,
            AttentionReasonKind::ObserverProblem,
            AttentionReasonKind::BudgetExhausted,
            AttentionReasonKind::Inferred,
        ] {
            assert!(kinds.contains(&expected), "missing {expected:?}: {kinds:?}");
        }
        let worker = items
            .iter()
            .find(|item| item.reason.kind == AttentionReasonKind::WorkerBlocked)
            .unwrap();
        assert!(!worker.reason.summary.contains('\n'));
        assert!(!worker.reason.summary.contains('\u{1b}'));
        assert!(worker.reason.summary.chars().count() <= MAX_ATTENTION_SUMMARY_CHARS);
        assert!(worker.reason.summary.ends_with('…'));
        let inferred = items
            .iter()
            .find(|item| item.reason.kind == AttentionReasonKind::Inferred)
            .unwrap();
        assert_eq!(
            inferred.run_id.as_ref().map(crate::RunId::as_str),
            Some("run-1")
        );
        assert!(inferred.action_text().contains("run run-1"));
    }

    #[test]
    fn display_text_and_age_are_bounded_and_stable() {
        assert_eq!(
            display_text(" ask\n\u{1b}[31m\u{202e}  now\u{2066} "),
            "ask [31m now"
        );
        assert_eq!(age_text(65_000, 5_000), "1m");
        assert_eq!(age_text(1_000, 2_000), "0s");
    }

    #[test]
    fn authoritative_wait_causes_do_not_infer_answers_from_notification_text() {
        let mut question = session(SessionState::WaitingForInput, 10);
        question.last_hook_event = Some(ProviderHookEvent::Notification);
        question.notification_kind = Some(ProviderNotificationKind::ElicitationDialog);
        question.wait_reason = Some("Which recovery branch should I use?".to_owned());
        let (_, question) = session_reason(&question).unwrap();
        assert_eq!(question.kind, AttentionReasonKind::ProviderQuestion);
        assert_eq!(question.action, AttentionAction::AnswerInTerminal);

        let mut permission = session(SessionState::WaitingForInput, 20);
        permission.last_hook_event = Some(ProviderHookEvent::PermissionRequest);
        permission.wait_reason = Some("Approve delivery to production?".to_owned());
        let (_, permission) = session_reason(&permission).unwrap();
        assert_eq!(permission.kind, AttentionReasonKind::ProviderPermission);
        assert_eq!(permission.action, AttentionAction::ReviewProviderPermission);

        let mut idle = session(SessionState::Idle, 25);
        idle.last_hook_event = Some(ProviderHookEvent::Notification);
        idle.notification_kind = Some(ProviderNotificationKind::IdlePrompt);
        assert!(session_reason(&idle).is_none());

        let mut completed = session(SessionState::Idle, 26);
        completed.last_hook_event = Some(ProviderHookEvent::Notification);
        completed.notification_kind = Some(ProviderNotificationKind::AgentCompleted);
        assert!(session_reason(&completed).is_none());

        let mut old_notification = session(SessionState::WaitingForInput, 26);
        old_notification.last_hook_event = Some(ProviderHookEvent::Notification);
        old_notification.wait_reason = Some("Approve delivery?".to_owned());
        assert!(session_reason(&old_notification).is_none());

        let mut synthetic_delivery = session(SessionState::WaitingForInput, 30);
        synthetic_delivery.last_hook_event = Some(ProviderHookEvent::Notification);
        synthetic_delivery.wait_reason = Some("delivery unacknowledged".to_owned());
        let (_, synthetic_delivery) = session_reason(&synthetic_delivery).unwrap();
        assert_eq!(
            synthetic_delivery.kind,
            AttentionReasonKind::DeliveryRecovery
        );
        assert_eq!(synthetic_delivery.action, AttentionAction::InspectRecovery);
    }

    #[test]
    fn completed_prior_run_is_never_bound_to_the_next_queued_task_attention() {
        let project = ProjectId::try_from("p").unwrap();
        let mut waiting = session(SessionState::WaitingForInput, 30);
        waiting.wait_reason = Some("delivery unacknowledged".to_owned());
        let running = task("current-task", TaskStatus::Running, 35);
        let next = task("next-task", TaskStatus::Queued, 40);
        let mut delivery_status = status(agent("a", false), Some(waiting), vec![running, next]);
        delivery_status.latest_run = Some(run(RunStatus::Succeeded, None));

        let item = attention_items(&project, &[delivery_status], &[], false)
            .into_iter()
            .find(|item| item.reason.kind == AttentionReasonKind::DeliveryRecovery)
            .unwrap();
        assert_eq!(item.task_id.as_ref().map(TaskId::as_str), Some("next-task"));
        assert!(item.run_id.is_none());

        let mut budget = status(
            agent("b", false),
            None,
            vec![task("later-task", TaskStatus::Queued, 50)],
        );
        budget.latest_run = Some(run(RunStatus::Succeeded, None));
        budget.budget.exhausted = true;
        let item = attention_items(&project, &[budget], &[], false)
            .into_iter()
            .find(|item| item.reason.kind == AttentionReasonKind::BudgetExhausted)
            .unwrap();
        assert!(item.task_id.is_none());
        assert!(item.session_id.is_none());
        assert!(item.run_id.is_none());
    }

    #[test]
    fn active_queue_attention_counts_and_targets_only_queued_work() {
        let project = ProjectId::try_from("p").unwrap();
        let running = task("current-task", TaskStatus::Running, 10);

        let running_only = status(agent("a", true), None, vec![running.clone()]);
        let items = attention_items(&project, &[running_only], &[], true);
        assert!(items.iter().all(|item| !matches!(
            item.reason.kind,
            AttentionReasonKind::PausedWithWork | AttentionReasonKind::WaitingForCapacity
        )));

        let next = task("next-task", TaskStatus::Queued, 20);
        let paused = status(agent("a", true), None, vec![running.clone(), next.clone()]);
        let paused_item = attention_items(&project, &[paused], &[], true)
            .into_iter()
            .find(|item| item.reason.kind == AttentionReasonKind::PausedWithWork)
            .unwrap();
        assert_eq!(
            paused_item.task_id.as_ref().map(TaskId::as_str),
            Some("next-task")
        );
        assert_eq!(paused_item.reason.summary, "paused with 1 queued task(s)");

        let waiting = status(agent("a", false), None, vec![running, next]);
        let waiting_item = attention_items(&project, &[waiting], &[], true)
            .into_iter()
            .find(|item| item.reason.kind == AttentionReasonKind::WaitingForCapacity)
            .unwrap();
        assert_eq!(
            waiting_item.task_id.as_ref().map(TaskId::as_str),
            Some("next-task")
        );
        assert_eq!(
            waiting_item.reason.summary,
            "1 queued task(s) but the daemon is at its live-session cap"
        );
    }
}
