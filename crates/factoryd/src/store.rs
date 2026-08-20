use std::path::Path;

use factory_core::{
    AgentBudget, AgentId, AgentRole, AgentSnapshot, EventEnvelope, FactoryEvent, MessageId,
    ObserverHealth, PROTOCOL_VERSION, ProjectId, ProjectSnapshot, Provider, ProviderHookEvent,
    ProviderNotificationKind, RunClosedBy, RunFailureReason, RunId, RunSnapshot, RunStatus,
    RunnerInstanceId, SessionId, SessionSnapshot, SessionState, TaskDetail, TaskId, TaskSnapshot,
    TaskStatus,
    attention::agent_attention,
    local::{MAX_TASK_BODY_BYTES, normalize_task_title},
    model_policy,
    status::{AgentPauseReason, AgentStatus, MAX_QUEUE_PREVIEW},
};
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, types::Type,
};
use thiserror::Error;
use uuid::Uuid;

use crate::session_work::{DeliveryLease, Phase as SessionWorkPhase, SessionWork, TaskWork};

/// One project's rows for `factory_core::status::FleetStatus`: the project,
/// its agents' statuses, its project backlog, and its blocked tasks (see
/// [`Store::fleet_status`]).
pub struct ProjectStatusRows {
    pub project: ProjectSnapshot,
    pub agents: Vec<AgentStatus>,
    pub backlog: Vec<TaskSnapshot>,
    pub blocked: Vec<factory_core::status::BlockedTaskStatus>,
}

const SCHEMA_VERSION: i64 = 30;
const MAX_EVENT_PAGE: usize = 10_000;
/// Every `List*` handler in `local_api.rs` fetches `limit + 1` rows (one
/// extra, to detect whether a next page exists) where `limit` is bounded by
/// the largest wire page cap, `factory_core::local::MAX_*_PAGE_ITEMS`
/// (1000). This used to be 101, silently rejecting a client that paged at
/// the documented wire maximum (`factoryctl session list`, `agent list`,
/// ... with no `--limit`, which default to the wire max) with "state page
/// limit is outside the supported range" -- found while building this
/// track's E2E tests, not a hypothetical.
const MAX_STATE_PAGE: usize = 1_001;
const MAX_PROVIDER_SESSION_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 4096;
const MAX_WEBHOOK_DOCUMENT_REFS: usize = 8;
const MAX_WEBHOOK_ACTIVE_TASKS: usize = 100;
const MAX_WEBHOOK_DONE_TASKS: usize = 12;
const MAX_WEBHOOK_SNAPSHOT_AGENTS: usize = 64;
const MAX_WEBHOOK_CREATE_TITLE_BYTES: usize = 160;
const MAX_BODY_BYTES: usize = 100_000;
const MAX_WEBHOOK_TITLE_BYTES: usize = 240;
const MAX_WEBHOOK_TEXT_BYTES: usize = 4_000;
const MAX_AGENT_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_AGENT_MODEL_BYTES: usize = 256;
const MAX_AGENT_PERMISSION_MODE_BYTES: usize = 64;
const MAX_RUNTIME_METADATA_BYTES: usize = 256;
const MAX_DELIVERY_ATTEMPT_FAILURES: i64 = 2;
const DELIVERY_RETRY_DELAY_MS: i64 = 5_000;
/// Mirrors the `sessions.wait_reason`/`activity` CHECK bounds (migration
/// 0014): the operator-facing explanation the hook state machine records.
const MAX_WAIT_REASON_BYTES: usize = 512;
const MAX_ACTIVITY_BYTES: usize = 512;
/// Mirrors the `tasks.blocked_reason` CHECK bound (migration 0014).
const MAX_BLOCKED_REASON_BYTES: usize = 4096;
/// Mirrors the `tasks.result` CHECK bound (migration 0006).
const MAX_TASK_RESULT_BYTES: usize = 131_072;
/// Mirrors the `sessions.hook_token` CHECK bound (migration 0014): 32
/// random bytes, lowercase-hex-encoded.
const HOOK_TOKEN_HEX_LEN: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewProject {
    pub id: ProjectId,
    pub name: String,
    pub root: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewTask {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub parent_task_id: Option<TaskId>,
    pub title: String,
    pub body: String,
    pub priority: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewAgent {
    pub id: AgentId,
    pub project_id: ProjectId,
    pub parent_agent_id: Option<AgentId>,
    pub role: AgentRole,
    pub provider: Provider,
}

/// Durable, provider-scoped model selection and permission mode. Standing
/// instructions and memory used to live here as TEXT columns; they are now
/// operator- and agent-editable files under the state directory (see
/// `factoryd::guidance` and `factory_core::paths`), composed at launch by
/// the execution track. `permission_mode` is consumed by provider launch and
/// retained separately from each session's resolved runtime metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentProfile {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub model_selection_reason: Option<String>,
    pub permission_mode: Option<String>,
    pub updated_at_ms: i64,
}

pub struct AgentDetail {
    pub snapshot: AgentSnapshot,
    pub profile: AgentProfile,
}

pub struct UpdateAgentProfile {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub model_selection_reason: Option<String>,
    pub permission_mode: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewAgentMessage {
    pub id: MessageId,
    pub project_id: ProjectId,
    pub sender_agent_id: Option<AgentId>,
    pub recipient_agent_id: AgentId,
    pub body: String,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectorEventInput {
    Task {
        id: TaskId,
        project_id: ProjectId,
        title: String,
        body: String,
        priority: i32,
    },
    Message {
        id: MessageId,
        project_id: ProjectId,
        recipient_agent_id: AgentId,
        body: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectorEventResult {
    Accepted { kind: &'static str, id: String },
    Duplicate { kind: String, id: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentMessage {
    pub id: MessageId,
    pub project_id: ProjectId,
    pub sender_agent_id: Option<AgentId>,
    pub recipient_agent_id: AgentId,
    pub body: String,
    pub created_at_ms: i64,
    pub delivered_at_ms: Option<i64>,
    pub delivered_run_id: Option<RunId>,
    /// The session a message was typed/replied into. A message may be
    /// delivered without a run ever opening (a standalone nudge into an
    /// idle session), so delivery is keyed to the session, with the run id
    /// recorded alongside only when one happened to be open.
    pub delivered_session_id: Option<SessionId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryAttempt {
    pub id: String,
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub task_incarnation_id: Option<String>,
    pub task_revision: Option<i64>,
    pub run_id: Option<RunId>,
    pub message_ids: Vec<MessageId>,
    pub text: String,
    pub failure_count: u32,
    pub next_attempt_at_ms: Option<i64>,
    pub state: DeliveryAttemptState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryAttemptState {
    InFlight,
    Retryable,
    Terminal,
    Acknowledged,
    Cancelled,
}

/// Winner of the durable acknowledgement-vs-recovery fence for one exact
/// delivery attempt. Both decisions are made under one immediate SQLite
/// transaction, so a timeout can never retire a prompt whose hook already
/// admitted it.
pub enum ProviderDeliveryRecovery {
    Acknowledged,
    StopRequested,
}

#[derive(Clone)]
pub struct NewDeliveryAttempt {
    pub id: String,
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub task_incarnation_id: Option<String>,
    pub task_revision: Option<i64>,
    /// Automatic dispatch must still own the canonical queue head when the
    /// reservation commits. Explicit operator starts intentionally bypass
    /// FIFO order but retain every other identity fence.
    pub require_queue_head: bool,
    pub message_ids: Vec<MessageId>,
    pub text: String,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDeliveryMarker {
    pub incarnation_id: String,
    pub task_revision: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionWorkSnapshot {
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    /// `None` only for a migration quarantine. Normal runtime rows always
    /// parse to the pure authority state.
    pub work: Option<SessionWork>,
    pub quarantine_reason: Option<String>,
}

/// Provider-independent task vocabulary exposed by authenticated integrations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationalTaskStatus {
    Todo,
    Doing,
    Blocked,
    Done,
}

/// Private task creation input accepted by an authenticated webhook endpoint.
pub struct NewWebhookTask {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub orchestrator_agent_id: AgentId,
    pub endpoint_id: String,
    pub title: String,
    pub body: String,
    pub token_sha256: [u8; 32],
    pub created_at_ms: i64,
}

/// Private question answer and the caller-generated notification identity.
pub struct WebhookAnswer {
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub answer: String,
    pub orchestrator_agent_id: AgentId,
    pub notification_task_id: TaskId,
    pub answered_at_ms: i64,
}

pub struct WebhookTaskCounts {
    pub todo: i64,
    pub doing: i64,
    pub blocked: i64,
    pub done: i64,
}

pub struct WebhookSnapshot {
    pub generated_at_ms: i64,
    pub counts: WebhookTaskCounts,
    pub tasks: Vec<WebhookSnapshotTask>,
    pub agents: Vec<WebhookSnapshotAgent>,
}

pub struct WebhookSnapshotTask {
    pub id: TaskId,
    pub title: String,
    pub status: OperationalTaskStatus,
    pub assignee: Option<String>,
    pub priority: i32,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub question: Option<WebhookOpenQuestion>,
    pub result: Option<String>,
}

pub struct WebhookOpenQuestion {
    pub text: String,
    pub asked_at_ms: Option<i64>,
    pub documents: Vec<WebhookDocumentRef>,
}

pub struct WebhookDocumentRef {
    pub id: String,
    pub name: String,
    pub reference: String,
}

pub struct WebhookSnapshotAgent {
    pub id: AgentId,
    pub name: String,
    pub role: String,
    pub control_role: AgentRole,
    pub provider: Option<Provider>,
    pub is_orchestrator: bool,
    pub last_active_sec_ago: Option<i64>,
    pub inbox_backlog: i64,
    pub observer_health: ObserverHealth,
}

/// Capability-scoped status. Result content keeps this non-`Debug`.
pub struct WebhookTaskPoll {
    pub status: OperationalTaskStatus,
    pub title: String,
    pub result: Option<String>,
}

pub struct WebhookCreated {
    pub task_id: TaskId,
    pub status: OperationalTaskStatus,
    pub title: String,
}

pub struct WebhookMutation {
    pub events: Vec<EventEnvelope>,
    pub open_question_remains: bool,
}

/// Capability-scoped immutable document. Content keeps this non-`Debug`.
pub struct WebhookDocument {
    pub id: String,
    pub name: String,
    pub reference: String,
    pub revision: String,
    pub content: String,
}

pub struct NewRepositoryOperation {
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub operation: String,
    pub phase: String,
    pub success: Option<bool>,
    pub reference: Option<String>,
    pub occurred_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryAuthority {
    pub remote_url: String,
    pub base_branch: String,
}
// --- Sessions -------------------------------------------------------------
//
// One resident interactive provider process per agent (PTY-backed,
// `factory-runner` terminal mode), spanning many task episodes (`runs`).
// See TRACK5-DESIGN.md / TRACK5-WIRE.md.

/// A session row together with the private control/authentication fields
/// that never appear on `SessionSnapshot`. This deliberately has no
/// `Debug`/`Clone` implementation: `hook_token` is a bearer credential.
pub struct SessionRow {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    pub provider: Provider,
    pub runtime_model: Option<String>,
    pub runtime_reasoning_effort: Option<String>,
    pub runtime_permission_mode: Option<String>,
    pub runtime_control_mode: Option<String>,
    pub provider_session_id: Option<String>,
    pub worktree: String,
    pub codex_home: Option<String>,
    pub hook_token: String,
    pub state: SessionState,
    pub state_since_ms: i64,
    /// Bounded free-text activity label (e.g. `"tool: Read"`), durable but
    /// not yet part of `SessionSnapshot`.
    pub activity: Option<String>,
    /// Whether `activity` was inferred from a generic hook (`true`) or
    /// named an exact tool (`false`); the handoff requires inferred state
    /// be marked, not silently presented as exact.
    pub activity_inferred: bool,
    pub wait_reason: Option<String>,
    pub observer_reason: Option<String>,
    pub observer_health: ObserverHealth,
    pub observer_health_since_ms: i64,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
    pub runner_protocol_version: u16,
    pub last_hook_event: Option<ProviderHookEvent>,
    pub notification_kind: Option<ProviderNotificationKind>,
    pub last_hook_at_ms: Option<i64>,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub stop_requested_at_ms: Option<i64>,
    /// Recovery stop intent is distinct from an operator stop so startup
    /// supervision knows it must actively replay the idempotent runner stop.
    pub delivery_recovery_stop_requested_at_ms: Option<i64>,
    /// The open run (task episode), if any, currently inside this session.
    pub current_run_id: Option<RunId>,
}

impl SessionRow {
    #[must_use]
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            id: self.id.clone(),
            project_id: self.project_id.clone(),
            agent_id: self.agent_id.clone(),
            provider: self.provider,
            runtime_model: self.runtime_model.clone(),
            runtime_reasoning_effort: self.runtime_reasoning_effort.clone(),
            runtime_permission_mode: self.runtime_permission_mode.clone(),
            runtime_control_mode: self.runtime_control_mode.clone(),
            state: self.state,
            state_since_ms: self.state_since_ms,
            worktree: self.worktree.clone(),
            provider_session_id: self.provider_session_id.clone(),
            runner_instance_id: Some(self.runner_instance_id.clone()),
            current_run_id: self.current_run_id.clone(),
            activity: self.activity.clone(),
            activity_inferred: self.activity_inferred,
            last_hook_event: self.last_hook_event,
            notification_kind: self.notification_kind,
            last_hook_at_ms: self.last_hook_at_ms,
            wait_reason: self.wait_reason.clone(),
            observer_reason: self.observer_reason.clone(),
            observer_health: self.observer_health,
            observer_health_since_ms: self.observer_health_since_ms,
            started_at_ms: self.started_at_ms,
            updated_at_ms: self.updated_at_ms,
            ended_at_ms: self.ended_at_ms,
            exit_code: self.exit_code,
            exit_signal: self.exit_signal,
        }
    }
}

/// Private input to reserve a new resident session for an agent.
pub struct NewSession {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    pub provider: Provider,
    pub runtime_model: Option<String>,
    pub runtime_reasoning_effort: Option<String>,
    pub runtime_permission_mode: Option<String>,
    pub runtime_control_mode: Option<String>,
    pub provider_session_id: Option<String>,
    pub worktree: String,
    pub codex_home: Option<String>,
    pub hook_token: String,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
    pub runner_protocol_version: u16,
}

/// Private identity and runtime path needed for direct local runner control
/// (attach/terminal-input/resize/stop), resolved either directly by session
/// id or by the session backing an exact run.
pub struct SessionControlTarget {
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
    pub state: SessionState,
    pub ended_at_ms: Option<i64>,
}

/// Minimal private identity required to resume observing a durable
/// resident session after a daemon restart.
pub struct RecoverableSession {
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub provider: Provider,
    pub provider_session_id: Option<String>,
    pub worktree: String,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
    pub runner_protocol_version: u16,
    pub observer_health: ObserverHealth,
    pub delivery_recovery_stop_requested: bool,
}

/// Authority derived by the daemon from one fresh live-session credential.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedPrincipal {
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub role: AgentRole,
}

/// Result of opening a task-episode inside a live session.
pub struct OpenedEpisode {
    pub run: RunSnapshot,
    pub task: TaskDetail,
    pub agent_messages: Vec<AgentMessage>,
    pub events: Vec<EventEnvelope>,
}

/// Result of closing an open task-episode, by `complete_task`, `block_task`,
/// `cancel_run`, `cancel_task`, or a session ending.
pub struct ClosedEpisode {
    pub run: RunSnapshot,
    pub task: TaskDetail,
    pub events: Vec<EventEnvelope>,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("event payload error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database schema {found} is newer than supported schema {supported}")]
    UnsupportedSchema { found: i64, supported: i64 },
    #[error("database schema version {0} is invalid")]
    InvalidSchemaVersion(i64),
    #[error("migration left a foreign key violation behind")]
    ForeignKeyViolation,
    #[error("event page size must be between 1 and {MAX_EVENT_PAGE}")]
    InvalidEventLimit,
    #[error("state page size must be between 1 and {MAX_STATE_PAGE}")]
    InvalidStateLimit,
    #[error("corrupt event protocol version {0}")]
    CorruptProtocolVersion(i64),
    #[error("serialized factory event has no string type tag")]
    MissingEventKind,
    #[error("event cursor must not be negative")]
    InvalidEventCursor,
    #[error("event log gap: expected sequence {expected}, found {found}")]
    EventSequenceGap { expected: i64, found: i64 },
    #[error("agent was not found in the requested project")]
    AgentNotFound,
    #[error("project repository authority is not configured")]
    RepositoryAuthorityMissing,
    #[error("project repository authority must be configured before any factory session starts")]
    RepositoryAuthorityRequiresIdleProject,
    #[error("task was not found in the requested project")]
    TaskNotFound,
    #[error("task page cursor is stale; restart the listing")]
    StaleTaskCursor,
    #[error("task page cursor requires its queue revision")]
    MissingTaskCursorRevision,
    #[error("task page revision requires its cursor")]
    UnexpectedTaskCursorRevision,
    #[error("agent provider does not match the requested execution provider")]
    AgentProviderMismatch,
    #[error("agent profile is invalid or exceeds its bound")]
    InvalidAgentProfile,
    #[error("agent model policy rejected the profile: {0}")]
    InvalidAgentModelPolicy(#[from] model_policy::ModelPolicyError),
    #[error("permission mode {mode:?} is not supported by provider {provider:?}")]
    UnsupportedAgentPermissionMode { provider: Provider, mode: String },
    #[error("agent budget is invalid or exceeds its bound")]
    InvalidAgentBudget,
    #[error("agent budget is exhausted; reset it before resuming")]
    AgentBudgetExhausted,
    #[error("agent message is invalid or exceeds its bound")]
    InvalidAgentMessage,
    #[error("task is not queued in the requested project")]
    TaskNotQueued,
    #[error("task is not running in the requested project")]
    TaskNotRunning,
    #[error("task is not assigned to the requesting agent")]
    TaskAssignmentMismatch,
    #[error("task is not retryable in the requested project")]
    TaskNotRetryable,
    #[error("task result exceeds its bound")]
    InvalidTaskResult,
    #[error("task title or body is invalid or exceeds its bound")]
    InvalidTaskInput,
    #[error("task blocked reason is empty or exceeds its bound")]
    InvalidBlockedReason,
    #[error("agent already has a live session or open run")]
    AgentUnavailable,
    #[error("run was not found")]
    RunNotFound,
    #[error("session was not found")]
    SessionNotFound,
    #[error("session already has a live session for this agent")]
    SessionAlreadyLive,
    #[error("session is not live")]
    SessionNotLive,
    #[error("session is stopping")]
    SessionStopping,
    #[error("session hook token is invalid")]
    InvalidHookToken,
    #[error("run is not in the required state")]
    InvalidRunState,
    #[error("private execution metadata is empty, relative, or too large")]
    InvalidExecutionMetadata,
    #[error("webhook input is invalid")]
    InvalidWebhookInput,
    #[error("connector event ID was reused with a different payload")]
    ConnectorEventPayloadMismatch,
    #[error("webhook project was not found")]
    WebhookProjectNotFound,
    #[error("webhook orchestrator was not found")]
    WebhookOrchestratorNotFound,
    #[error("webhook task was not found")]
    WebhookTaskNotFound,
    #[error("webhook task has no open question")]
    WebhookQuestionNotOpen,
    #[error("webhook operational snapshot exceeds its bounded capacity")]
    WebhookSnapshotTooLarge,
    #[error("project was not found")]
    ProjectNotFound,
    #[error("task is not cancellable in the requested project")]
    TaskNotCancellable,
    #[error("task is not editable in the requested project")]
    TaskNotEditable,
    #[error("task has a non-terminal run and cannot be deleted")]
    TaskHasActiveRun,
    #[error("task has subtasks and cannot be deleted")]
    TaskHasSubtasks,
    #[error("a run of this task is the parent of another run and cannot be deleted")]
    TaskRunHasDependents,
    #[error("agent has an open run and cannot be deleted")]
    AgentHasActiveRun,
    #[error("agent has a live session and cannot be deleted")]
    AgentHasLiveSession,
    #[error("agent has child agents and cannot be deleted")]
    AgentHasChildren,
    #[error("a run of this agent is the parent of another run and cannot be deleted")]
    AgentRunHasDependents,
    #[error("project has a non-terminal run or a live session and cannot be deleted")]
    ProjectHasActiveRun,
    #[error("run is not in a stoppable state")]
    RunNotStoppable,
    #[error("could not migrate stored agent instructions/memory to guidance files: {0}")]
    AgentProfileMigration(String),
    #[error("a starting session unexpectedly has an open run episode")]
    StartingSessionHasOpenRun,
    #[error("session work is owned by another delivery or run")]
    SessionWorkConflict,
    #[error("session work is quarantined: {0}")]
    SessionWorkQuarantined(String),
    #[error("stored session work is corrupt")]
    CorruptSessionWork,
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn apply_connector_event(
        &mut self,
        endpoint_id: &str,
        event_id: &str,
        payload_digest: [u8; 32],
        input: ConnectorEventInput,
        now_ms: i64,
    ) -> Result<(ConnectorEventResult, Vec<EventEnvelope>)> {
        if endpoint_id.is_empty() || event_id.is_empty() || event_id.len() > 128 {
            return Err(StoreError::InvalidWebhookInput);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = transaction
            .query_row(
                "SELECT event_kind, result_id, payload_digest FROM connector_events WHERE endpoint_id = ?1 AND event_id = ?2",
                params![endpoint_id, event_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?
        {
            if !constant_time_eq(&existing.2, &payload_digest) {
                return Err(StoreError::ConnectorEventPayloadMismatch);
            }
            transaction.commit()?;
            return Ok((ConnectorEventResult::Duplicate { kind: existing.0, id: existing.1 }, Vec::new()));
        }

        let (kind, id, event) = match input {
            ConnectorEventInput::Task {
                id,
                project_id,
                title,
                body,
                priority,
            } => {
                transaction
                    .query_row(
                        "SELECT 1 FROM projects WHERE id = ?1",
                        [project_id.as_str()],
                        |_| Ok(()),
                    )
                    .optional()?
                    .ok_or(StoreError::WebhookProjectNotFound)?;
                let (_, event) = insert_task(
                    &transaction,
                    NewTask {
                        id: id.clone(),
                        project_id,
                        parent_task_id: None,
                        title,
                        body,
                        priority,
                    },
                    None,
                    now_ms,
                )?;
                ("task", id.to_string(), Some(event))
            }
            ConnectorEventInput::Message {
                id,
                project_id,
                recipient_agent_id,
                body,
            } => {
                validate_agent_message(&body, now_ms)?;
                load_agent(&transaction, &recipient_agent_id)?
                    .filter(|agent| agent.snapshot.project_id == project_id)
                    .ok_or(StoreError::AgentNotFound)?;
                transaction.execute(
                    "INSERT INTO agent_messages (id, project_id, sender_agent_id, recipient_agent_id, body, created_at_ms, delivered_at_ms) VALUES (?1, ?2, NULL, ?3, ?4, ?5, NULL)",
                    params![id.as_str(), project_id.as_str(), recipient_agent_id.as_str(), body, now_ms],
                )?;
                ("message", id.to_string(), None)
            }
        };
        transaction.execute(
            "INSERT INTO connector_events (endpoint_id, event_id, payload_digest, event_kind, result_id, received_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![endpoint_id, event_id, payload_digest.as_slice(), kind, id, now_ms],
        )?;
        let events = if let Some(event) = event {
            let sequence = append_event(&transaction, now_ms, &event)?;
            vec![EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            }]
        } else {
            Vec::new()
        };
        transaction.commit()?;
        Ok((ConnectorEventResult::Accepted { kind, id }, events))
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut connection: Connection) -> Result<Self> {
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;",
        )?;
        migrate(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn auto_mode(&self) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT auto_mode FROM factory_settings WHERE singleton = 1",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(Into::into)
    }

    pub fn set_auto_mode(&mut self, enabled: bool, now_ms: i64) -> Result<EventEnvelope> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE factory_settings SET auto_mode = ?1, updated_at_ms = ?2 WHERE singleton = 1",
            params![enabled, now_ms],
        )?;
        let event = FactoryEvent::AutoModeChanged { enabled };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: now_ms,
            event,
        })
    }

    pub fn record_policy_decision(
        &mut self,
        event: FactoryEvent,
        now_ms: i64,
    ) -> Result<EventEnvelope> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: now_ms,
            event,
        })
    }

    /// Appends one credential- and content-free repository audit record.
    pub fn record_repository_operation(
        &mut self,
        input: NewRepositoryOperation,
    ) -> Result<EventEnvelope> {
        let transaction = self.connection.transaction()?;
        let event = FactoryEvent::RepositoryOperation {
            project_id: input.project_id,
            agent_id: input.agent_id,
            session_id: input.session_id,
            operation: input.operation,
            phase: input.phase,
            success: input.success,
            reference: input.reference,
        };
        let sequence = append_event(&transaction, input.occurred_at_ms, &event)?;
        transaction.commit()?;
        Ok(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: input.occurred_at_ms,
            event,
        })
    }

    pub fn create_project(
        &mut self,
        input: NewProject,
        now_ms: i64,
    ) -> Result<(ProjectSnapshot, EventEnvelope)> {
        let project = ProjectSnapshot {
            id: input.id,
            name: input.name,
            root: input.root,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let event = FactoryEvent::ProjectChanged {
            project: project.clone(),
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        transaction.execute(
            "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                project.id.as_str(),
                project.name,
                project.root,
                project.created_at_ms,
                project.updated_at_ms
            ],
        )?;
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;

        Ok((
            project,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    pub fn create_task(
        &mut self,
        input: NewTask,
        now_ms: i64,
    ) -> Result<(TaskDetail, EventEnvelope)> {
        self.create_task_with_assignment(input, None, now_ms)
    }

    pub fn create_assigned_task(
        &mut self,
        input: NewTask,
        agent_id: AgentId,
        now_ms: i64,
    ) -> Result<(TaskDetail, EventEnvelope)> {
        self.create_task_with_assignment(input, Some(agent_id), now_ms)
    }

    pub fn create_task_with_assignment(
        &mut self,
        input: NewTask,
        assigned_agent_id: Option<AgentId>,
        now_ms: i64,
    ) -> Result<(TaskDetail, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (record, event) = insert_task(&transaction, input, assigned_agent_id, now_ms)?;
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;

        Ok((
            record,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    pub fn create_agent(
        &mut self,
        input: NewAgent,
        now_ms: i64,
    ) -> Result<(AgentSnapshot, EventEnvelope)> {
        self.insert_agent(input, None, None, None, now_ms)
    }

    pub fn create_agent_with_model(
        &mut self,
        input: NewAgent,
        model: Option<String>,
        now_ms: i64,
    ) -> Result<(AgentSnapshot, EventEnvelope)> {
        self.create_agent_with_profile(input, model, None, None, now_ms)
    }

    pub fn create_agent_with_profile(
        &mut self,
        input: NewAgent,
        model: Option<String>,
        reasoning_effort: Option<String>,
        model_selection_reason: Option<String>,
        now_ms: i64,
    ) -> Result<(AgentSnapshot, EventEnvelope)> {
        validate_agent_model(model.as_deref())?;
        let selection = model_policy::normalize_profile(
            input.provider,
            input.role,
            model_policy::ModelSelection {
                model,
                reasoning_effort,
                reason: model_selection_reason,
            },
            true,
        )?;
        self.insert_agent(
            input,
            selection.model,
            selection.reasoning_effort,
            selection.reason,
            now_ms,
        )
    }

    fn insert_agent(
        &mut self,
        input: NewAgent,
        model: Option<String>,
        reasoning_effort: Option<String>,
        model_selection_reason: Option<String>,
        now_ms: i64,
    ) -> Result<(AgentSnapshot, EventEnvelope)> {
        let agent = AgentSnapshot {
            id: input.id,
            project_id: input.project_id,
            parent_agent_id: input.parent_agent_id,
            role: input.role,
            provider: input.provider,
            current_run_id: None,
            paused: false,
            current_session_id: None,
            worktree: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let event = FactoryEvent::AgentChanged {
            agent: agent.clone(),
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO agents (
                id, project_id, parent_agent_id, role, provider, paused, worktree,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL, ?6, ?6)",
            params![
                agent.id.as_str(),
                agent.project_id.as_str(),
                agent.parent_agent_id.as_ref().map(AgentId::as_str),
                agent_role_value(agent.role),
                provider_value(agent.provider),
                agent.created_at_ms,
            ],
        )?;
        transaction.execute(
            "INSERT INTO agent_profiles (
                agent_id, model, reasoning_effort, model_selection_reason, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                agent.id.as_str(),
                model,
                reasoning_effort,
                model_selection_reason,
                agent.updated_at_ms
            ],
        )?;
        transaction.execute(
            "INSERT INTO agent_budgets (agent_id, max_tool_calls, reset_at_ms, updated_at_ms)
             VALUES (?1, 1000, ?2, ?2)",
            params![agent.id.as_str(), now_ms],
        )?;
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            agent,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    pub fn agent_budget(&self, project_id: &ProjectId, agent_id: &AgentId) -> Result<AgentBudget> {
        self.connection
            .query_row(
                "SELECT b.max_tool_calls, b.tool_calls, b.exhausted, b.reset_at_ms, b.updated_at_ms
             FROM agent_budgets b JOIN agents a ON a.id = b.agent_id
             WHERE b.agent_id = ?1 AND a.project_id = ?2",
                params![agent_id.as_str(), project_id.as_str()],
                budget_from_row,
            )
            .optional()?
            .ok_or(StoreError::AgentNotFound)
    }

    pub fn set_agent_budget(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        max_tool_calls: Option<u64>,
        now_ms: i64,
    ) -> Result<(AgentBudget, EventEnvelope)> {
        let max_tool_calls = max_tool_calls
            .map(i64::try_from)
            .transpose()
            .map_err(|_| StoreError::InvalidAgentBudget)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE agent_budgets SET max_tool_calls = ?1, updated_at_ms = ?2
             WHERE agent_id = ?3 AND EXISTS (SELECT 1 FROM agents WHERE id = ?3 AND project_id = ?4)",
            params![max_tool_calls, now_ms, agent_id.as_str(), project_id.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::AgentNotFound);
        }
        let budget = transaction.query_row("SELECT max_tool_calls, tool_calls, exhausted, reset_at_ms, updated_at_ms FROM agent_budgets WHERE agent_id = ?1", [agent_id.as_str()], budget_from_row)?;
        let pause_reasons = agent_pause_reasons(&transaction, project_id, agent_id)?;
        let event = FactoryEvent::AgentBudgetChanged {
            project_id: project_id.clone(),
            agent_id: agent_id.clone(),
            budget: budget.clone(),
            action: "configured".into(),
            paused: !pause_reasons.is_empty(),
            pause_reasons,
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            budget,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    pub fn reset_agent_budget(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        now_ms: i64,
    ) -> Result<(AgentBudget, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE agent_budgets SET tool_calls = 0, exhausted = 0, reset_at_ms = ?1, updated_at_ms = ?1
             WHERE agent_id = ?2 AND EXISTS (SELECT 1 FROM agents WHERE id = ?2 AND project_id = ?3)",
            params![now_ms, agent_id.as_str(), project_id.as_str()],
        )?;
        if changed == 0 {
            return Err(StoreError::AgentNotFound);
        }
        let budget = transaction.query_row("SELECT max_tool_calls, tool_calls, exhausted, reset_at_ms, updated_at_ms FROM agent_budgets WHERE agent_id = ?1", [agent_id.as_str()], budget_from_row)?;
        let pause_reasons = agent_pause_reasons(&transaction, project_id, agent_id)?;
        let event = FactoryEvent::AgentBudgetChanged {
            project_id: project_id.clone(),
            agent_id: agent_id.clone(),
            budget: budget.clone(),
            action: "reset".into(),
            paused: !pause_reasons.is_empty(),
            pause_reasons,
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            budget,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    /// Counts one authenticated tool-call attempt. Returns `true` when the
    /// provider must deny it. The first `max_tool_calls` attempts are allowed;
    /// the next one exhausts and durably pauses the agent.
    pub fn observe_tool_call(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        now_ms: i64,
    ) -> Result<(AgentBudget, bool, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let before = transaction.query_row("SELECT max_tool_calls, tool_calls, exhausted, reset_at_ms, updated_at_ms FROM agent_budgets WHERE agent_id = ?1 AND EXISTS (SELECT 1 FROM agents WHERE id = ?1 AND project_id = ?2)", params![agent_id.as_str(), project_id.as_str()], budget_from_row).optional()?.ok_or(StoreError::AgentNotFound)?;
        let denied = before.exhausted
            || before
                .max_tool_calls
                .is_some_and(|limit| before.tool_calls >= limit);
        if denied {
            transaction.execute(
                "UPDATE agent_budgets SET exhausted = 1, updated_at_ms = ?1 WHERE agent_id = ?2",
                params![now_ms, agent_id.as_str()],
            )?;
        } else {
            transaction.execute("UPDATE agent_budgets SET tool_calls = tool_calls + 1, updated_at_ms = ?1 WHERE agent_id = ?2", params![now_ms, agent_id.as_str()])?;
        }
        let budget = transaction.query_row("SELECT max_tool_calls, tool_calls, exhausted, reset_at_ms, updated_at_ms FROM agent_budgets WHERE agent_id = ?1", [agent_id.as_str()], budget_from_row)?;
        let pause_reasons = agent_pause_reasons(&transaction, project_id, agent_id)?;
        let event = FactoryEvent::AgentBudgetChanged {
            project_id: project_id.clone(),
            agent_id: agent_id.clone(),
            budget: budget.clone(),
            action: if denied { "denied" } else { "observed" }.into(),
            paused: !pause_reasons.is_empty(),
            pause_reasons,
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            budget,
            denied,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    /// Durably holds an agent's queue: the daemon stops delivering new work
    /// into its session until `resume_agent`.
    pub fn pause_agent(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        now_ms: i64,
    ) -> Result<(AgentSnapshot, EventEnvelope)> {
        self.set_agent_paused(project_id, agent_id, true, now_ms)
    }

    pub fn resume_agent(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        now_ms: i64,
    ) -> Result<(AgentSnapshot, EventEnvelope)> {
        if self.agent_budget(project_id, agent_id)?.exhausted {
            return Err(StoreError::AgentBudgetExhausted);
        }
        self.set_agent_paused(project_id, agent_id, false, now_ms)
    }

    /// Effective durable hold, composing the ordinary agent hold with the
    /// independent budget circuit breaker.
    pub fn agent_is_held(&self, project_id: &ProjectId, agent_id: &AgentId) -> Result<bool> {
        Ok(!agent_pause_reasons(&self.connection, project_id, agent_id)?.is_empty())
    }

    fn set_agent_paused(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        paused: bool,
        now_ms: i64,
    ) -> Result<(AgentSnapshot, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        load_agent(&transaction, agent_id)?
            .filter(|agent| agent.snapshot.project_id == *project_id)
            .ok_or(StoreError::AgentNotFound)?;
        transaction.execute(
            "UPDATE agents SET paused = ?1, updated_at_ms = ?2 WHERE id = ?3 AND project_id = ?4",
            params![paused, now_ms, agent_id.as_str(), project_id.as_str()],
        )?;
        let agent = load_agent(&transaction, agent_id)?
            .ok_or(StoreError::AgentNotFound)?
            .snapshot;
        let event = FactoryEvent::AgentChanged {
            agent: agent.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            agent,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    /// Records the agent's git worktree (D3). Creating the git worktree
    /// itself is execution's job; this only durably records an already
    /// existing absolute path.
    pub fn set_agent_worktree(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        worktree: String,
        now_ms: i64,
    ) -> Result<(AgentSnapshot, EventEnvelope)> {
        validate_absolute_path(&worktree)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        load_agent(&transaction, agent_id)?
            .filter(|agent| agent.snapshot.project_id == *project_id)
            .ok_or(StoreError::AgentNotFound)?;
        transaction.execute(
            "UPDATE agents SET worktree = ?1, updated_at_ms = ?2 WHERE id = ?3 AND project_id = ?4",
            params![worktree, now_ms, agent_id.as_str(), project_id.as_str()],
        )?;
        let agent = load_agent(&transaction, agent_id)?
            .ok_or(StoreError::AgentNotFound)?
            .snapshot;
        let event = FactoryEvent::AgentChanged {
            agent: agent.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            agent,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    /// The next queued task assigned to `agent_id`, in the canonical active
    /// queue order, or `None` when the agent is paused or has no queued work.
    pub fn next_deliverable(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
    ) -> Result<Option<TaskId>> {
        if self.agent_is_held(project_id, agent_id)? {
            return Ok(None);
        }
        let id: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM tasks
                 WHERE project_id = ?1 AND assigned_agent_id = ?2 AND status = 'queued'
                 ORDER BY priority DESC, created_at_ms, id
                 LIMIT 1",
                params![project_id.as_str(), agent_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id.map(|id| parse_id::<TaskId>(id, 0)).transpose()?)
    }

    // --- Sessions -----------------------------------------------------

    /// Reserves a new resident session for an agent. One live session per
    /// agent is enforced by `sessions_one_live_per_agent`.
    pub fn create_session(
        &mut self,
        input: NewSession,
        now_ms: i64,
    ) -> Result<(SessionSnapshot, Vec<EventEnvelope>)> {
        validate_absolute_path(&input.worktree)?;
        if let Some(codex_home) = input.codex_home.as_deref() {
            validate_absolute_path(codex_home)?;
            if input.provider != Provider::Codex {
                return Err(StoreError::InvalidExecutionMetadata);
            }
        }
        validate_provider_session(input.provider_session_id.as_deref())?;
        validate_hook_token(&input.hook_token)?;
        validate_runtime_metadata(&input)?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let agent = load_agent(&transaction, &input.agent_id)?
            .filter(|agent| agent.snapshot.project_id == input.project_id)
            .ok_or(StoreError::AgentNotFound)?;
        if agent.snapshot.provider != input.provider {
            return Err(StoreError::AgentProviderMismatch);
        }
        let already_live: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE agent_id = ?1 AND ended_at_ms IS NULL)",
            params![input.agent_id.as_str()],
            |row| row.get(0),
        )?;
        if already_live {
            return Err(StoreError::SessionAlreadyLive);
        }
        let resumed_provider_session = match input.provider_session_id.as_deref() {
            Some(provider_session_id) => transaction.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sessions
                    WHERE project_id = ?1 AND agent_id = ?2 AND provider = ?3
                      AND provider_session_id = ?4
                 )",
                params![
                    input.project_id.as_str(),
                    input.agent_id.as_str(),
                    provider_value(input.provider),
                    provider_session_id,
                ],
                |row| row.get::<_, bool>(0),
            )?,
            None => false,
        };
        transaction.execute(
            "INSERT INTO sessions (
                id, project_id, agent_id, provider, runtime_model,
                runtime_reasoning_effort, runtime_permission_mode, runtime_control_mode,
                provider_session_id, worktree,
                codex_home, hook_token, state, state_since_ms, activity,
                activity_inferred, wait_reason, observer_health,
                observer_health_since_ms, runner_instance_id, runner_runtime,
                runner_protocol_version, last_hook_event, notification_kind, last_hook_at_ms,
                started_at_ms, updated_at_ms, ended_at_ms, exit_code, exit_signal,
                stop_requested_at_ms, resumed_provider_session, principal_version
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'starting', ?13,
                NULL, 0, NULL, ?14, ?13, ?15, ?16, ?17, NULL, NULL, NULL, ?13,
                ?13, NULL, NULL, NULL, NULL, ?18, 1
             )",
            params![
                input.id.as_str(),
                input.project_id.as_str(),
                input.agent_id.as_str(),
                provider_value(input.provider),
                input.runtime_model,
                input.runtime_reasoning_effort,
                input.runtime_permission_mode,
                input.runtime_control_mode,
                input.provider_session_id,
                input.worktree,
                input.codex_home,
                input.hook_token,
                now_ms,
                observer_health_value(ObserverHealth::Unknown),
                input.runner_instance_id.as_str(),
                input.runner_runtime,
                i64::from(input.runner_protocol_version),
                resumed_provider_session,
            ],
        )?;
        transaction.execute(
            "INSERT INTO session_work (
                session_id, project_id, agent_id, state, revision, attempt_id,
                task_id, task_incarnation_id, task_revision,
                run_id, quarantine_reason, updated_at_ms
             ) VALUES (?1, ?2, ?3, 'empty', 0, NULL, NULL, NULL, NULL, NULL, NULL, ?4)",
            params![
                input.id.as_str(),
                input.project_id.as_str(),
                input.agent_id.as_str(),
                now_ms,
            ],
        )?;
        let session = load_session(&transaction, &input.id)?.ok_or(StoreError::SessionNotFound)?;
        let snapshot = session.snapshot();
        let event = FactoryEvent::SessionChanged {
            session: snapshot.clone(),
        };
        let agent_event = append_agent_changed_event(&transaction, &input.agent_id, now_ms)?;
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            snapshot,
            vec![
                agent_event,
                EventEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    sequence,
                    occurred_at_ms: now_ms,
                    event,
                },
            ],
        ))
    }

    /// Resolves an opaque bearer by scanning every live session with a
    /// constant-time comparison. Version-zero rows predate this boundary and
    /// are never authenticated.
    pub fn authenticate_session(
        &self,
        credential: &[u8],
    ) -> Result<Option<AuthenticatedPrincipal>> {
        let mut statement = self.connection.prepare(
            "SELECT id, hook_token, principal_version
             FROM sessions WHERE ended_at_ms IS NULL ORDER BY id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        let mut matched = None;
        for (id, expected, principal_version) in rows {
            if constant_time_eq(expected.as_bytes(), credential) {
                matched = (principal_version == 1).then_some(id);
            }
        }
        let Some(id) = matched else {
            return Ok(None);
        };
        let session_id: SessionId = parse_id(id, 0)?;
        let session = load_session(&self.connection, &session_id)?
            .filter(|session| session.ended_at_ms.is_none())
            .ok_or(StoreError::SessionNotFound)?;
        let agent =
            load_agent(&self.connection, &session.agent_id)?.ok_or(StoreError::AgentNotFound)?;
        Ok(Some(AuthenticatedPrincipal {
            project_id: session.project_id,
            agent_id: session.agent_id,
            session_id: session.id,
            role: agent.snapshot.role,
        }))
    }

    /// Durable provider-independent work owner for one resident session.
    pub fn session_work(&self, session_id: &SessionId) -> Result<SessionWorkSnapshot> {
        let snapshot =
            load_session_work(&self.connection, session_id)?.ok_or(StoreError::SessionNotFound)?;
        if let Some(work) = snapshot.work.as_ref() {
            validate_session_work_identity(
                &self.connection,
                &snapshot.session_id,
                &snapshot.project_id,
                &snapshot.agent_id,
                work,
            )?;
        }
        Ok(snapshot)
    }

    /// Implements the hook state machine: `SessionStart` -> `idle` (only
    /// from `starting`); `UserPromptSubmit`/`PreToolUse`/`PostToolUse` ->
    /// `working`; typed actionable `Notification` -> `waiting_for_input`,
    /// routine/unknown `Notification` -> `idle`; `Stop` -> `idle`
    /// (clears activity); `SubagentStop`/`SessionEnd` record only, without
    /// changing session state. Never closes a run.
    pub fn record_hook_event(
        &mut self,
        session_id: &SessionId,
        event: ProviderHookEvent,
        activity: Option<String>,
        inferred: bool,
        wait_reason: Option<String>,
        now_ms: i64,
    ) -> Result<(SessionSnapshot, EventEnvelope)> {
        self.record_hook_event_with_notification(
            session_id,
            event,
            activity,
            inferred,
            wait_reason,
            None,
            now_ms,
        )
    }

    /// Records a provider hook with its typed notification cause. The cause
    /// is durable session state; free-form wait text is never consulted to
    /// decide whether a notification is answerable.
    #[allow(clippy::too_many_arguments)]
    pub fn record_hook_event_with_notification(
        &mut self,
        session_id: &SessionId,
        event: ProviderHookEvent,
        activity: Option<String>,
        inferred: bool,
        wait_reason: Option<String>,
        notification_kind: Option<ProviderNotificationKind>,
        now_ms: i64,
    ) -> Result<(SessionSnapshot, EventEnvelope)> {
        if activity
            .as_deref()
            .is_some_and(|value| value.len() > MAX_ACTIVITY_BYTES)
        {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        if wait_reason
            .as_deref()
            .is_some_and(|value| value.len() > MAX_WAIT_REASON_BYTES)
        {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        if !session.state.is_live() {
            return Err(StoreError::SessionNotLive);
        }
        let mut state = session.state;
        let mut next_activity = session.activity.clone();
        let mut next_inferred = session.activity_inferred;
        let mut next_wait_reason = session.wait_reason.clone();
        let mut next_notification_kind = session.notification_kind;
        match event {
            ProviderHookEvent::SessionStart => {
                if session.state == SessionState::Starting {
                    state = SessionState::Idle;
                    next_activity = None;
                    next_inferred = false;
                    next_wait_reason = None;
                    next_notification_kind = None;
                }
            }
            ProviderHookEvent::UserPromptSubmit
            | ProviderHookEvent::PreToolUse
            | ProviderHookEvent::PostToolUse => {
                state = SessionState::Working;
                next_activity = activity;
                next_inferred = inferred;
                next_wait_reason = None;
                next_notification_kind = None;
            }
            ProviderHookEvent::Notification => {
                if matches!(
                    notification_kind,
                    Some(
                        ProviderNotificationKind::PermissionPrompt
                            | ProviderNotificationKind::ElicitationDialog
                            | ProviderNotificationKind::ElicitationUrlDialog
                            | ProviderNotificationKind::AgentNeedsInput
                    )
                ) {
                    state = SessionState::WaitingForInput;
                    next_wait_reason = wait_reason;
                } else {
                    state = SessionState::Idle;
                    next_activity = None;
                    next_inferred = false;
                    next_wait_reason = None;
                }
                next_notification_kind = notification_kind;
            }
            ProviderHookEvent::PermissionRequest => {
                state = SessionState::WaitingForInput;
                next_wait_reason = wait_reason;
                next_notification_kind = None;
            }
            ProviderHookEvent::Stop => {
                state = SessionState::Idle;
                next_activity = None;
                next_inferred = false;
                next_wait_reason = None;
                next_notification_kind = None;
            }
            ProviderHookEvent::SubagentStop | ProviderHookEvent::SessionEnd => {
                // Records only: a subagent finishing does not mean the
                // top-level session is idle, and a `SessionEnd` hook is
                // advisory -- the daemon learns the process actually
                // exited from the runner, via `end_session`.
            }
        }
        let state_since_ms = if state == session.state {
            session.state_since_ms
        } else {
            now_ms
        };
        transaction.execute(
            "UPDATE sessions
             SET state = ?1, state_since_ms = ?2, activity = ?3, activity_inferred = ?4,
                 wait_reason = ?5, last_hook_event = ?6, notification_kind = ?7,
                 last_hook_at_ms = ?8, updated_at_ms = ?8
             WHERE id = ?9",
            params![
                session_state_value(state),
                state_since_ms,
                next_activity,
                next_inferred,
                next_wait_reason,
                provider_hook_event_value(event),
                next_notification_kind.map(provider_notification_kind_value),
                now_ms,
                session_id.as_str(),
            ],
        )?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        let snapshot = session.snapshot();
        let changed = FactoryEvent::SessionChanged {
            session: snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &changed)?;
        transaction.commit()?;
        Ok((
            snapshot,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event: changed,
            },
        ))
    }

    /// Synthesizes exactly the `starting -> idle` transition a real
    /// `SessionStart` hook would make (this arm of
    /// [`Self::record_hook_event`]), for a Codex session whose own hook is
    /// not expected to arrive before the daemon needs to start delivering
    /// (`crates/factoryd/src/execution.rs`'s `synthesize_codex_session_start`
    /// -- see its own doc comment, and `docs/providers.md`, for why Codex
    /// 0.147 does not fire it at TUI startup).
    ///
    /// Deliberately never touches `last_hook_event`/`last_hook_at_ms`,
    /// unlike [`Self::record_hook_event`] -- leaving them at whatever they
    /// already were (`None` for a session no hook has ever reached) keeps a
    /// synthesized start durably distinguishable from a real one: `state =
    /// idle` with `last_hook_event IS NULL` can only happen here, never
    /// from a genuine hook POST. `session list`/`agent status` already
    /// surface `last_hook_event`, so no schema or wire change is needed to
    /// tell the two apart.
    ///
    /// A no-op -- `Ok(None)`, publishing nothing -- once the session is no
    /// longer `starting` (already live-delivered, already ended, or a
    /// second `RunnerEvent::TerminalRaw` for the same session): stricter
    /// than `record_hook_event`'s own "second `SessionStart` is harmless"
    /// contract, since this only ever exists to make the one transition it
    /// asserts, nothing else worth logging again.
    pub fn synthesize_session_start(
        &mut self,
        session_id: &SessionId,
        now_ms: i64,
    ) -> Result<Option<(SessionSnapshot, EventEnvelope)>> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        if session.state != SessionState::Starting {
            return Ok(None);
        }
        transaction.execute(
            "UPDATE sessions
             SET state = ?1, state_since_ms = ?2, activity = NULL, activity_inferred = 0,
                 wait_reason = NULL, updated_at_ms = ?2
             WHERE id = ?3",
            params![
                session_state_value(SessionState::Idle),
                now_ms,
                session_id.as_str(),
            ],
        )?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        let snapshot = session.snapshot();
        let changed = FactoryEvent::SessionChanged {
            session: snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &changed)?;
        transaction.commit()?;
        Ok(Some((
            snapshot,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event: changed,
            },
        )))
    }

    /// Persists a session's provider-assigned identity once it becomes
    /// known after the session was created without one -- today only
    /// Codex, whose thread id the daemon does not choose up front (unlike
    /// Claude's `--session-id`, `TRACK5-DESIGN.md` §1) but learns from the
    /// payload of that session's own first `SessionStart` hook. Called
    /// unconditionally from the `ProviderHook` handler for every provider:
    /// a no-op (not an error), returning `None`, if the session already
    /// carries a `provider_session_id` -- Claude's is assigned at creation
    /// time (always already set), a resumed session already carries its
    /// prior identity forward, and a duplicate/replayed hook must not
    /// clobber an established identity. Returns `Some` with the updated
    /// snapshot and event when it actually set one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::SessionNotFound`] if `session_id` does not
    /// exist, or [`StoreError::InvalidExecutionMetadata`] if
    /// `provider_session_id` fails the same validation
    /// [`Store::create_session`] applies.
    pub fn set_provider_session_id(
        &mut self,
        session_id: &SessionId,
        provider_session_id: &str,
        now_ms: i64,
    ) -> Result<Option<(SessionSnapshot, EventEnvelope)>> {
        validate_provider_session(Some(provider_session_id))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        if session.provider_session_id.is_some() {
            return Ok(None);
        }
        transaction.execute(
            "UPDATE sessions SET provider_session_id = ?1, updated_at_ms = ?2 WHERE id = ?3",
            params![provider_session_id, now_ms, session_id.as_str()],
        )?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        let snapshot = session.snapshot();
        let event = FactoryEvent::SessionChanged {
            session: snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok(Some((
            snapshot,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        )))
    }

    /// Persists stop intent on a live session so the daemon knows a
    /// `process exited`/`failed`-shaped end is actually a graceful stop.
    pub fn request_session_stop(
        &mut self,
        project_id: &ProjectId,
        session_id: &SessionId,
        now_ms: i64,
    ) -> Result<(SessionSnapshot, EventEnvelope)> {
        self.request_session_stop_inner(project_id, session_id, now_ms)
    }

    /// Stops a Codex session whose resumed composer could not acknowledge a
    /// delivery and durably excludes the failed provider thread from future
    /// resume selection. Its uncommitted task remains queued for the fresh
    /// conversation the dispatcher starts after the runner exits.
    pub fn request_fresh_provider_recovery(
        &mut self,
        project_id: &ProjectId,
        session_id: &SessionId,
        attempt_id: &str,
        now_ms: i64,
    ) -> Result<(ProviderDeliveryRecovery, Option<EventEnvelope>)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&transaction, session_id)?
            .filter(|session| session.project_id == *project_id)
            .ok_or(StoreError::SessionNotFound)?;
        if !session.state.is_live() {
            return Err(StoreError::SessionNotLive);
        }
        let attempt = load_delivery_attempt(&transaction, attempt_id)?
            .filter(|attempt| {
                attempt.project_id == *project_id && attempt.session_id == *session_id
            })
            .ok_or(StoreError::InvalidExecutionMetadata)?;
        if attempt.state == DeliveryAttemptState::Acknowledged {
            transaction.commit()?;
            return Ok((ProviderDeliveryRecovery::Acknowledged, None));
        }
        if attempt.state == DeliveryAttemptState::Cancelled
            && session.delivery_recovery_stop_requested_at_ms.is_some()
        {
            transaction.commit()?;
            return Ok((ProviderDeliveryRecovery::StopRequested, None));
        }
        if !matches!(
            attempt.state,
            DeliveryAttemptState::InFlight | DeliveryAttemptState::Retryable
        ) {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        let resumed_provider_session: bool = transaction.query_row(
            "SELECT resumed_provider_session FROM sessions WHERE id = ?1",
            params![session_id.as_str()],
            |row| row.get(0),
        )?;
        if !resumed_provider_session {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        let provider_session_id = session
            .provider_session_id
            .as_deref()
            .filter(|_| session.provider == Provider::Codex)
            .ok_or(StoreError::InvalidExecutionMetadata)?;
        transaction.execute(
            "UPDATE sessions
             SET provider_resume_blocked_at_ms = ?1
             WHERE project_id = ?2 AND agent_id = ?3 AND provider_session_id = ?4",
            params![
                now_ms,
                project_id.as_str(),
                session.agent_id.as_str(),
                provider_session_id
            ],
        )?;
        transaction.execute(
            "UPDATE sessions
             SET stop_requested_at_ms = COALESCE(stop_requested_at_ms, ?1),
                 delivery_recovery_stop_requested_at_ms =
                    COALESCE(delivery_recovery_stop_requested_at_ms, ?1),
                 updated_at_ms = ?1
             WHERE id = ?2",
            params![now_ms, session_id.as_str()],
        )?;
        cancel_reserved_session_work_in_transaction(&transaction, session_id, now_ms)?;
        transaction.execute(
            "UPDATE delivery_attempts
             SET state = 'cancelled', next_attempt_at_ms = NULL, updated_at_ms = ?1
             WHERE id = ?2 AND state IN ('in_flight', 'retryable')",
            params![now_ms, attempt_id],
        )?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        let snapshot = session.snapshot();
        let event = FactoryEvent::SessionChanged {
            session: snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            ProviderDeliveryRecovery::StopRequested,
            Some(EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            }),
        ))
    }

    fn request_session_stop_inner(
        &mut self,
        project_id: &ProjectId,
        session_id: &SessionId,
        now_ms: i64,
    ) -> Result<(SessionSnapshot, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&transaction, session_id)?
            .filter(|session| session.project_id == *project_id)
            .ok_or(StoreError::SessionNotFound)?;
        if !session.state.is_live() {
            return Err(StoreError::SessionNotLive);
        }
        transaction.execute(
            "UPDATE sessions
             SET stop_requested_at_ms = COALESCE(stop_requested_at_ms, ?1), updated_at_ms = ?1
             WHERE id = ?2",
            params![now_ms, session_id.as_str()],
        )?;
        cancel_reserved_session_work_in_transaction(&transaction, session_id, now_ms)?;
        transaction.execute(
            "UPDATE delivery_attempts
             SET state = 'cancelled', next_attempt_at_ms = NULL, updated_at_ms = ?1
             WHERE session_id = ?2
               AND state IN ('in_flight', 'retryable', 'terminal')",
            params![now_ms, session_id.as_str()],
        )?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        let snapshot = session.snapshot();
        let event = FactoryEvent::SessionChanged {
            session: snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            snapshot,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    /// The provider process exited (or is being torn down): moves the
    /// session to `stopped` (a clean exit, or one the operator asked for
    /// via `StopSession`/`StopRun`) or `failed` (anything else -- a crash,
    /// an unverifiable absence after a daemon restart), and -- in the same
    /// transaction -- closes any still-open run episode to match: an
    /// operator-requested stop closes it `stopped`/`closed_by =
    /// operator_stop`, task `cancelled` (TRACK5-DESIGN.md §6); anything
    /// else closes it `failed`/`process`, `closed_by = session_ended`, task
    /// `failed`.
    pub fn end_session(
        &mut self,
        session_id: &SessionId,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
        now_ms: i64,
    ) -> Result<(SessionSnapshot, Vec<EventEnvelope>)> {
        self.end_session_with_reason(session_id, exit_code, exit_signal, None, now_ms)
    }

    /// [`Store::end_session`], additionally recording `reason` (bounded,
    /// like `record_hook_event`'s `activity`/`wait_reason`) into both the
    /// ended session's `activity` and `wait_reason` columns instead of
    /// clearing them to `NULL` -- used when the daemon itself supplies a
    /// reason a live session never got the chance to report through a
    /// hook, concretely a spawn failure (`execution.rs`'s
    /// `spawn_session_for_agent`, this track's item 1): the session never
    /// got past `starting`, so this is the only way its failure is ever
    /// explained anywhere durable/visible (`session list`/the TUI), not
    /// just in the daemon's own log.
    pub fn end_session_with_reason(
        &mut self,
        session_id: &SessionId,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
        reason: Option<String>,
        now_ms: i64,
    ) -> Result<(SessionSnapshot, Vec<EventEnvelope>)> {
        if reason
            .as_deref()
            .is_some_and(|value| value.len() > MAX_WAIT_REASON_BYTES)
        {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        if !session.state.is_live() {
            return Err(StoreError::SessionNotLive);
        }
        let operator_stopped = session.stop_requested_at_ms.is_some();
        let graceful = operator_stopped || (exit_code == Some(0) && exit_signal.is_none());
        transaction.execute(
            "UPDATE delivery_attempts
             SET state = 'cancelled', next_attempt_at_ms = NULL, updated_at_ms = ?1
             WHERE session_id = ?2
               AND state IN ('in_flight', 'retryable', 'terminal')",
            params![now_ms, session_id.as_str()],
        )?;
        let state = if graceful {
            SessionState::Stopped
        } else {
            SessionState::Failed
        };
        transaction.execute(
            "UPDATE sessions
             SET state = ?1, state_since_ms = ?2, updated_at_ms = ?2, ended_at_ms = ?2,
                 exit_code = ?3, exit_signal = ?4, activity = ?5, wait_reason = ?5
             WHERE id = ?6",
            params![
                session_state_value(state),
                now_ms,
                exit_code,
                exit_signal,
                reason,
                session_id.as_str()
            ],
        )?;

        let mut events = Vec::new();
        if let Some(run_id) = session.current_run_id.clone() {
            let run = load_run(&transaction, &run_id)?.ok_or(StoreError::RunNotFound)?;
            let closed = if operator_stopped {
                close_run_in_transaction(
                    &transaction,
                    &run,
                    RunStatus::Stopped,
                    RunClosedBy::OperatorStop,
                    None,
                    TaskStatus::Cancelled,
                    None,
                    None,
                    now_ms,
                )?
            } else {
                // A confirmed OS exit status (from watching the process
                // directly, or from a runner's own `RunnerEvent::Exited`)
                // is `process`; a session recovered after a daemon restart
                // whose control endpoint is simply gone, with no exit
                // status ever observed, is `unverifiable` -- distinct
                // enough to matter operationally (TRACK5-DESIGN.md §6's
                // "unverifiable" recovery language).
                let failure_reason = if exit_code.is_none() && exit_signal.is_none() {
                    RunFailureReason::Unverifiable
                } else {
                    RunFailureReason::Process
                };
                close_run_in_transaction(
                    &transaction,
                    &run,
                    RunStatus::Failed,
                    RunClosedBy::SessionEnded,
                    Some(failure_reason),
                    TaskStatus::Failed,
                    None,
                    None,
                    now_ms,
                )?
            };
            events.extend(closed.events);
        }

        end_session_work_in_transaction(&transaction, session_id, now_ms)?;

        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        let snapshot = session.snapshot();
        let changed = FactoryEvent::SessionChanged {
            session: snapshot.clone(),
        };
        let agent_event = append_agent_changed_event(&transaction, &session.agent_id, now_ms)?;
        let sequence = append_event(&transaction, now_ms, &changed)?;
        events.push(agent_event);
        events.push(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: now_ms,
            event: changed,
        });
        transaction.commit()?;
        Ok((snapshot, events))
    }

    /// [`Store::end_session_with_reason`], but guarded to only ever apply
    /// while the session is still exactly `starting` -- `WHERE state =
    /// 'starting'` inside the same `IMMEDIATE` transaction that commits it,
    /// not `SessionState::is_live()` (`execution.rs`'s
    /// `enforce_start_deadline`, issue #24's start deadline). `DaemonState`
    /// serializes every store access through one `Arc<Mutex<Store>>`
    /// (`daemon_state.rs`) -- there is no second connection, and the two
    /// paths can never interleave *inside* one transaction -- but that
    /// mutex is released between separate `with_store`/`commit_and_publish`
    /// calls, and the caller's own read of the session (one such call) and
    /// this method's own later transaction (another) are exactly two such
    /// calls: the provider's own `SessionStart` hook can fully run via
    /// `record_hook_event`, in its own transaction, in the gap between
    /// them. `is_live()` alone would still accept the `idle`/`working`
    /// session that leaves behind and overwrite it with a `failed`/
    /// `stopped` reason that is by then false; this guard is belt-and-
    /// braces on top of that serialization, not a substitute for it, and
    /// makes the invariant checkable at this one statement instead of
    /// depending on a fact about `DaemonState` two files away. Returns
    /// `Ok(None)`, not an error, if the session had already left
    /// `starting` by the time this committed -- the caller's enforcement
    /// then becomes a no-op instead of clobbering a session its own hook
    /// already rescued.
    ///
    /// Like [`Store::end_session_with_reason`], an already-pending
    /// operator stop (`stop_requested_at_ms`) still ends the session
    /// `stopped` rather than `failed`; callers that want a deadline's
    /// specific reason text to never attach to an operator-initiated stop
    /// should check `stop_requested_at_ms` themselves before calling this
    /// (`enforce_start_deadline` does, so the ordinary stop-completion
    /// path -- not this one -- is what ends that session, with its real
    /// exit status, not a synthesized deadline reason).
    pub fn fail_starting_session(
        &mut self,
        session_id: &SessionId,
        reason: String,
        now_ms: i64,
    ) -> Result<Option<(SessionSnapshot, Vec<EventEnvelope>)>> {
        if reason.len() > MAX_WAIT_REASON_BYTES {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        if session.state != SessionState::Starting {
            return Ok(None);
        }
        // A session still `starting` never has an open run episode --
        // every path that opens one (`open_run_episode`'s callers) only
        // ever does so once a session has reached `idle` or later -- so,
        // unlike `end_session_with_reason`, there is never one to close
        // here. A real, always-compiled check, not `debug_assert!`
        // (compiled out of release builds): if this invariant is ever
        // violated, silently proceeding to fail the session below would
        // orphan the still-open `runs` row forever, exactly the failure
        // mode this whole guarded method exists to avoid elsewhere. Fails
        // the transaction instead (the session stays `starting`, visible
        // and retried, rather than corrupted) -- believed unreachable
        // today, not merely assumed so.
        if session.current_run_id.is_some() {
            return Err(StoreError::StartingSessionHasOpenRun);
        }
        let operator_stopped = session.stop_requested_at_ms.is_some();
        let new_state = if operator_stopped {
            SessionState::Stopped
        } else {
            SessionState::Failed
        };
        let changed_rows = transaction.execute(
            "UPDATE sessions
             SET state = ?1, state_since_ms = ?2, updated_at_ms = ?2, ended_at_ms = ?2,
                 activity = ?3, wait_reason = ?3
             WHERE id = ?4 AND state = ?5",
            params![
                session_state_value(new_state),
                now_ms,
                reason,
                session_id.as_str(),
                session_state_value(SessionState::Starting),
            ],
        )?;
        if changed_rows == 0 {
            // Lost the guard inside this same transaction: belt-and-braces
            // for the identical condition the load above already tested
            // (`IMMEDIATE` already serializes writers against each other,
            // so this is not expected to actually differ from the load,
            // just cheap insurance against ever relying on that alone).
            return Ok(None);
        }
        end_session_work_in_transaction(&transaction, session_id, now_ms)?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        let snapshot = session.snapshot();
        let changed = FactoryEvent::SessionChanged {
            session: snapshot.clone(),
        };
        let agent_event = append_agent_changed_event(&transaction, &session.agent_id, now_ms)?;
        let sequence = append_event(&transaction, now_ms, &changed)?;
        transaction.commit()?;
        Ok(Some((
            snapshot,
            vec![
                agent_event,
                EventEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    sequence,
                    occurred_at_ms: now_ms,
                    event: changed,
                },
            ],
        )))
    }

    pub fn list_sessions(
        &self,
        project_id: &ProjectId,
        after_id: Option<&SessionId>,
        limit: usize,
    ) -> Result<Vec<SessionSnapshot>> {
        if !(1..=MAX_STATE_PAGE).contains(&limit) {
            return Err(StoreError::InvalidStateLimit);
        }
        let mut statement = self.connection.prepare(
            "SELECT id FROM sessions
             WHERE project_id = ?1 AND (?2 IS NULL OR id > ?2)
             ORDER BY id
             LIMIT ?3",
        )?;
        let ids = statement
            .query_map(
                params![
                    project_id.as_str(),
                    after_id.map(SessionId::as_str),
                    limit as i64
                ],
                |row| parse_id::<SessionId>(row.get(0)?, 0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        ids.into_iter()
            .map(|id| {
                load_session(&self.connection, &id)?
                    .map(|session| session.snapshot())
                    .ok_or(StoreError::SessionNotFound)
            })
            .collect()
    }

    /// The agent's current live session, if any.
    pub fn live_session_for_agent(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
    ) -> Result<Option<SessionRow>> {
        let id: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM sessions
                 WHERE project_id = ?1 AND agent_id = ?2 AND ended_at_ms IS NULL",
                params![project_id.as_str(), agent_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            return Ok(None);
        };
        load_session(&self.connection, &parse_id(id, 0)?)
    }

    /// Count of live sessions (`ended_at_ms IS NULL`) across every project
    /// in this daemon instance -- what `Config::max_active_runs` (this
    /// track's item 2) bounds. Daemon-wide, not per-project: one `factoryd`
    /// process is one resource pool (README's "allows four concurrently
    /// active sessions").
    pub fn live_session_count(&self) -> Result<usize> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// The most recent provider session/thread identity this agent's prior
    /// sessions confirmed, if any -- live or historical, most recent first.
    /// A fresh session spawn passes this to `SpawnContext::resume` (when the
    /// provider's `Capabilities::resume` allows it) instead of storing a
    /// separate `resumes_provider_session` bit: TRACK5-DESIGN.md §1 --
    /// "the daemon simply checks 'does this agent have a prior sessions row
    /// with a non-null provider_session_id'".
    pub fn last_provider_session_id(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
    ) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT provider_session_id FROM sessions
                 WHERE project_id = ?1 AND agent_id = ?2 AND provider_session_id IS NOT NULL
                   AND provider_resume_blocked_at_ms IS NULL
                 ORDER BY started_at_ms DESC, id DESC
                 LIMIT 1",
                params![project_id.as_str(), agent_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Whether this session was launched with a prior provider thread id.
    /// Stored at creation time so a later SessionStart cannot make a fresh
    /// Codex conversation look like a resumed one merely by assigning it an
    /// identity before its first delivery.
    pub fn session_resumed_provider_thread(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT resumed_provider_session FROM sessions
                 WHERE project_id = ?1 AND id = ?2",
                params![project_id.as_str(), session_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::SessionNotFound)
    }

    /// Marks a live session `waiting_for_input` outside the hook state
    /// machine: the dispatcher's own synthetic transition for "typed
    /// delivery went unacknowledged twice" (TRACK5-DESIGN.md §3/A3), as
    /// opposed to [`Store::record_hook_event`]'s `Notification` arm, which
    /// is the same state reached from a real hook.
    pub fn mark_session_waiting(
        &mut self,
        session_id: &SessionId,
        wait_reason: String,
        now_ms: i64,
    ) -> Result<(SessionSnapshot, EventEnvelope)> {
        if wait_reason.is_empty() || wait_reason.len() > MAX_WAIT_REASON_BYTES {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        if !session.state.is_live() {
            return Err(StoreError::SessionNotLive);
        }
        transaction.execute(
            "UPDATE sessions
             SET state = 'waiting_for_input', state_since_ms = ?1, updated_at_ms = ?1,
                 wait_reason = ?2, notification_kind = NULL
             WHERE id = ?3",
            params![now_ms, wait_reason, session_id.as_str()],
        )?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        let snapshot = session.snapshot();
        let event = FactoryEvent::SessionChanged {
            session: snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            snapshot,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    /// Manual delivery timed out after its journal failure was recorded.
    /// The exact late hook may still acknowledge `Uncertain` work between
    /// those two transactions, so this projection is conditional on still
    /// owning that same unacknowledged attempt. `None` means the exact hook
    /// already won and the caller must report the admitted run instead of a
    /// timeout.
    pub fn mark_session_waiting_for_delivery(
        &mut self,
        session_id: &SessionId,
        attempt_id: &str,
        wait_reason: String,
        now_ms: i64,
    ) -> Result<Option<EventEnvelope>> {
        if wait_reason.is_empty() || wait_reason.len() > MAX_WAIT_REASON_BYTES {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        if !session.state.is_live() {
            return Err(StoreError::SessionNotLive);
        }
        if session.stop_requested_at_ms.is_some() {
            return Err(StoreError::SessionStopping);
        }
        let owner = require_session_work(&transaction, session_id)?;
        let current = owner.work.as_ref().ok_or(StoreError::CorruptSessionWork)?;
        match &current.phase {
            SessionWorkPhase::Running(lease) if lease.attempt_id == attempt_id => {
                return Ok(None);
            }
            SessionWorkPhase::Uncertain(lease) if lease.attempt_id == attempt_id => {}
            _ => return Err(StoreError::SessionWorkConflict),
        }
        let attempt = load_delivery_attempt(&transaction, attempt_id)?
            .filter(|attempt| attempt.session_id == *session_id)
            .ok_or(StoreError::InvalidExecutionMetadata)?;
        if !matches!(
            attempt.state,
            DeliveryAttemptState::Retryable | DeliveryAttemptState::Terminal
        ) {
            return Err(StoreError::SessionWorkConflict);
        }
        transaction.execute(
            "UPDATE sessions
             SET state = 'waiting_for_input', state_since_ms = ?1, updated_at_ms = ?1,
                 wait_reason = ?2, notification_kind = NULL
             WHERE id = ?3",
            params![now_ms, wait_reason, session_id.as_str()],
        )?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        let event = FactoryEvent::SessionChanged {
            session: session.snapshot(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok(Some(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: now_ms,
            event,
        }))
    }

    /// Records whether the daemon can attach to a live session's runner PTY.
    /// Attach failures are shared durable observer state, not a TUI-only note;
    /// the next successful attach clears the exact daemon-owned failure reason.
    pub fn record_terminal_attach_health(
        &mut self,
        session_id: &SessionId,
        failure: Option<String>,
        now_ms: i64,
    ) -> Result<Option<(SessionSnapshot, EventEnvelope)>> {
        if failure
            .as_deref()
            .is_some_and(|value| value.is_empty() || value.len() > MAX_WAIT_REASON_BYTES)
        {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        if !session.state.is_live() {
            return Err(StoreError::SessionNotLive);
        }
        let attach_was_degraded = session
            .observer_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("terminal attach failed: "));
        let (health, next_reason) = match failure {
            Some(reason) => (ObserverHealth::Degraded, Some(reason)),
            None if attach_was_degraded => (ObserverHealth::Healthy, None),
            None if session.observer_health == ObserverHealth::Unknown => {
                (ObserverHealth::Healthy, session.observer_reason.clone())
            }
            None => (session.observer_health, session.observer_reason.clone()),
        };
        if session.observer_health == health && session.observer_reason == next_reason {
            return Ok(None);
        }
        let health_since_ms = if session.observer_health == health {
            session.observer_health_since_ms
        } else {
            now_ms
        };
        transaction.execute(
            "UPDATE sessions
             SET observer_health = ?1, observer_health_since_ms = ?2,
                 observer_reason = ?3, updated_at_ms = ?4
             WHERE id = ?5",
            params![
                observer_health_value(health),
                health_since_ms,
                next_reason,
                now_ms,
                session_id.as_str(),
            ],
        )?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        let snapshot = session.snapshot();
        let changed = FactoryEvent::SessionChanged {
            session: snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &changed)?;
        transaction.commit()?;
        Ok(Some((
            snapshot,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event: changed,
            },
        )))
    }

    /// Every session the daemon must reconnect to after a restart.
    pub fn recoverable_sessions(&self) -> Result<Vec<RecoverableSession>> {
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, provider, provider_session_id, worktree, runner_instance_id,
                    runner_runtime, runner_protocol_version, observer_health,
                    delivery_recovery_stop_requested_at_ms IS NOT NULL
             FROM sessions
             WHERE ended_at_ms IS NULL AND principal_version = 1
             ORDER BY project_id, started_at_ms, id",
        )?;
        let rows = statement.query_map([], |row| {
            let provider: String = row.get(2)?;
            let protocol: i64 = row.get(7)?;
            let observer_health: String = row.get(8)?;
            Ok(RecoverableSession {
                session_id: parse_id(row.get(0)?, 0)?,
                project_id: parse_id(row.get(1)?, 1)?,
                provider: parse_provider(&provider, 2)?,
                provider_session_id: row.get(3)?,
                worktree: row.get(4)?,
                runner_instance_id: parse_id(row.get(5)?, 5)?,
                runner_runtime: row.get(6)?,
                runner_protocol_version: u16::try_from(protocol).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(7, Type::Integer, Box::new(error))
                })?,
                observer_health: parse_observer_health(&observer_health, 8)?,
                delivery_recovery_stop_requested: row.get(9)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Sessions resident across the schema upgrade. Their provider
    /// environment still points at the credentialless operator socket, so
    /// startup must retire them before publishing either API endpoint.
    pub fn legacy_live_sessions(&self) -> Result<Vec<RecoverableSession>> {
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, provider, provider_session_id, worktree, runner_instance_id,
                    runner_runtime, runner_protocol_version, observer_health,
                    delivery_recovery_stop_requested_at_ms IS NOT NULL
             FROM sessions
             WHERE ended_at_ms IS NULL AND principal_version = 0
             ORDER BY project_id, started_at_ms, id",
        )?;
        let rows = statement.query_map([], |row| {
            let provider: String = row.get(2)?;
            let protocol: i64 = row.get(7)?;
            let observer_health: String = row.get(8)?;
            Ok(RecoverableSession {
                session_id: parse_id(row.get(0)?, 0)?,
                project_id: parse_id(row.get(1)?, 1)?,
                provider: parse_provider(&provider, 2)?,
                provider_session_id: row.get(3)?,
                worktree: row.get(4)?,
                runner_instance_id: parse_id(row.get(5)?, 5)?,
                runner_runtime: row.get(6)?,
                runner_protocol_version: u16::try_from(protocol).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(7, Type::Integer, Box::new(error))
                })?,
                observer_health: parse_observer_health(&observer_health, 8)?,
                delivery_recovery_stop_requested: row.get(9)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn session_control_target(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<SessionControlTarget> {
        self.connection
            .query_row(
                "SELECT runner_instance_id, runner_runtime, state, ended_at_ms FROM sessions
                 WHERE id = ?1 AND project_id = ?2",
                params![session_id.as_str(), project_id.as_str()],
                |row| {
                    let state: String = row.get(2)?;
                    Ok(SessionControlTarget {
                        runner_instance_id: parse_id(row.get(0)?, 0)?,
                        runner_runtime: row.get(1)?,
                        state: parse_session_state(&state, 2)?,
                        ended_at_ms: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)?
            .ok_or(StoreError::SessionNotFound)
    }

    pub fn session_snapshot(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
    ) -> Result<SessionSnapshot> {
        load_session(&self.connection, session_id)?
            .filter(|session| session.project_id == *project_id)
            .map(|session| session.snapshot())
            .ok_or(StoreError::SessionNotFound)
    }

    /// Resolves a run to the control target of the session it ran inside.
    pub fn run_control_target(
        &self,
        project_id: &ProjectId,
        run_id: &RunId,
    ) -> Result<SessionControlTarget> {
        let run = load_run(&self.connection, run_id)?
            .filter(|run| run.project_id == *project_id)
            .ok_or(StoreError::RunNotFound)?;
        let session_id = run.session_id.ok_or(StoreError::SessionNotFound)?;
        self.session_control_target(project_id, &session_id)
    }

    pub fn run_session_snapshot(
        &self,
        project_id: &ProjectId,
        run_id: &RunId,
    ) -> Result<SessionSnapshot> {
        let run = load_run(&self.connection, run_id)?
            .filter(|run| run.project_id == *project_id)
            .ok_or(StoreError::RunNotFound)?;
        let session_id = run.session_id.ok_or(StoreError::SessionNotFound)?;
        self.session_snapshot(project_id, &session_id)
    }

    // --- Task episodes (runs) ------------------------------------------

    /// Opens a task-episode inside a live session: the task moves
    /// `queued -> running`, a new `runs` row opens `running`, and any
    /// undelivered inbox messages for the agent are delivered alongside it.
    pub fn open_run_episode(
        &mut self,
        session_id: &SessionId,
        task_id: &TaskId,
        now_ms: i64,
    ) -> Result<OpenedEpisode> {
        self.open_run_episode_with_message_ids(session_id, task_id, None, now_ms)
    }

    /// Opens a task episode and delivers only the message ids captured by
    /// the durable delivery attempt. `None` retains the legacy operation's
    /// meaning of delivering the whole current inbox; delivery retries pass
    /// `Some` so messages arriving after composition remain queued.
    pub fn open_run_episode_with_message_ids(
        &mut self,
        session_id: &SessionId,
        task_id: &TaskId,
        message_ids: Option<&[MessageId]>,
        now_ms: i64,
    ) -> Result<OpenedEpisode> {
        self.open_run_episode_with_delivery_attempt(session_id, task_id, message_ids, None, now_ms)
    }

    /// Hook acknowledgement and task/run admission are one durable commit.
    /// This prevents a daemon crash between opening the run and fencing a
    /// timeout-driven provider recovery.
    pub fn open_run_episode_with_delivery_attempt(
        &mut self,
        session_id: &SessionId,
        task_id: &TaskId,
        message_ids: Option<&[MessageId]>,
        delivery_attempt_id: Option<&str>,
        now_ms: i64,
    ) -> Result<OpenedEpisode> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session = load_session(&transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
        if !session.state.is_live() {
            return Err(StoreError::SessionNotLive);
        }
        if session.stop_requested_at_ms.is_some() {
            return Err(StoreError::SessionStopping);
        }
        let mut task = load_task(&transaction, task_id)?
            .filter(|task| task.snapshot.project_id == session.project_id)
            .filter(|task| task.snapshot.status == TaskStatus::Queued)
            .ok_or(StoreError::TaskNotQueued)?;
        if task.snapshot.assigned_agent_id.as_ref() != Some(&session.agent_id) {
            return Err(StoreError::TaskAssignmentMismatch);
        }
        let owner = require_session_work(&transaction, session_id)?;
        let current = owner.work.as_ref().ok_or(StoreError::CorruptSessionWork)?;
        let (attempt_id, run_id, running, audit_state) =
            if let Some(attempt_id) = delivery_attempt_id {
                let attempt = load_delivery_attempt(&transaction, attempt_id)?
                    .filter(|attempt| {
                        attempt.project_id == session.project_id
                            && attempt.agent_id == session.agent_id
                            && attempt.session_id == *session_id
                            && attempt.task_id.as_ref() == Some(task_id)
                    })
                    .ok_or(StoreError::InvalidExecutionMetadata)?;
                if message_ids.is_some_and(|ids| ids != attempt.message_ids.as_slice()) {
                    return Err(StoreError::SessionWorkConflict);
                }
                let (ready, audit_state) = prepare_delivery_acknowledgement(current, &attempt)?;
                let lease = ready.phase.lease().ok_or(StoreError::SessionWorkConflict)?;
                let task_work = lease.task.as_ref().ok_or(StoreError::SessionWorkConflict)?;
                if task_work.task_id != *task_id {
                    return Err(StoreError::SessionWorkConflict);
                }
                let running = ready.acknowledge(attempt_id).map_err(transition_error)?;
                (
                    attempt_id.to_owned(),
                    task_work.run_id.clone(),
                    running,
                    audit_state,
                )
            } else {
                if !matches!(current.phase, SessionWorkPhase::Empty) {
                    return Err(StoreError::AgentUnavailable);
                }
                let (incarnation_id, task_revision): (String, i64) = transaction.query_row(
                    "SELECT incarnation_id, work_revision FROM tasks WHERE id = ?1",
                    params![task_id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let run_id = new_run_id()?;
                let attempt_id = format!("direct:{}", Uuid::new_v4().hyphenated());
                let lease = DeliveryLease {
                    attempt_id: attempt_id.clone(),
                    task: Some(TaskWork {
                        task_id: task_id.clone(),
                        incarnation_id: incarnation_id.clone(),
                        revision: task_revision,
                        run_id: run_id.clone(),
                    }),
                };
                let running = current
                    .reserve(lease)
                    .and_then(|work| work.external_effect_possible(&attempt_id))
                    .and_then(|work| work.acknowledge(&attempt_id))
                    .map_err(transition_error)?;
                transaction.execute(
                    "INSERT INTO delivery_attempts (
                    id, project_id, agent_id, session_id, task_id,
                    task_incarnation_id, prior_run_count, message_ids_json, text,
                    failure_count, next_attempt_at_ms, state, created_at_ms, updated_at_ms,
                    task_revision, run_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, '[]', ?7,
                           0, NULL, 'in_flight', ?8, ?8, ?9, ?10)",
                    params![
                        attempt_id,
                        session.project_id.as_str(),
                        session.agent_id.as_str(),
                        session_id.as_str(),
                        task_id.as_str(),
                        incarnation_id,
                        "direct acknowledged store episode",
                        now_ms,
                        task_revision,
                        run_id.as_str(),
                    ],
                )?;
                (attempt_id, run_id, running, "in_flight")
            };
        transaction.execute(
            "INSERT INTO runs (
                id, project_id, agent_id, session_id, parent_run_id, task_id, status,
                activity, wait_reason, worktree, started_at_ms, status_since_ms,
                updated_at_ms, ended_at_ms, closed_by, failure_reason,
                stop_requested_at_ms
             ) VALUES (
                ?1, ?2, ?3, ?4, NULL, ?5, 'running', NULL, NULL, ?6, ?7, ?7, ?7, NULL,
                NULL, NULL, NULL
             )",
            params![
                run_id.as_str(),
                session.project_id.as_str(),
                session.agent_id.as_str(),
                session_id.as_str(),
                task_id.as_str(),
                session.worktree,
                now_ms,
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE tasks
             SET status = 'running', updated_at_ms = ?1,
                 started_at_ms = COALESCE(started_at_ms, ?1),
                 work_revision = work_revision + 1
             WHERE id = ?2 AND project_id = ?3 AND status = 'queued'
               AND work_revision = ?4",
            params![
                now_ms,
                task_id.as_str(),
                session.project_id.as_str(),
                running
                    .phase
                    .lease()
                    .and_then(|lease| lease.task.as_ref())
                    .map(|task| task.revision)
                    .ok_or(StoreError::CorruptSessionWork)?,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::SessionWorkConflict);
        }
        transaction.execute(
            "UPDATE agents SET updated_at_ms = ?1 WHERE id = ?2 AND project_id = ?3",
            params![
                now_ms,
                session.agent_id.as_str(),
                session.project_id.as_str()
            ],
        )?;
        task.snapshot.status = TaskStatus::Running;
        task.snapshot.updated_at_ms = now_ms;

        let agent_messages = match message_ids {
            Some(message_ids) => Self::deliver_agent_messages_by_ids_in_transaction(
                &transaction,
                &session.project_id,
                &session.agent_id,
                session_id,
                Some(&run_id),
                message_ids,
                now_ms,
            )?,
            None => Self::deliver_agent_messages_in_transaction(
                &transaction,
                &session.project_id,
                &session.agent_id,
                session_id,
                Some(&run_id),
                now_ms,
            )?,
        };
        let agent = load_agent(&transaction, &session.agent_id)?
            .ok_or(StoreError::AgentNotFound)?
            .snapshot;
        let run = load_run(&transaction, &run_id)?.ok_or(StoreError::RunNotFound)?;
        let events = append_execution_events(&transaction, now_ms, &task.snapshot, &agent, &run)?;
        let changed = transaction.execute(
            "UPDATE delivery_attempts
             SET state = 'acknowledged', next_attempt_at_ms = NULL, updated_at_ms = ?1
             WHERE id = ?2 AND session_id = ?3
               AND state = ?4",
            params![now_ms, attempt_id, session_id.as_str(), audit_state],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        persist_session_work(&transaction, session_id, current.revision, &running, now_ms)?;
        transaction.commit()?;
        Ok(OpenedEpisode {
            run,
            task,
            agent_messages,
            events,
        })
    }

    /// Creates or resumes the one durable delivery attempt for this agent.
    /// The stored text and identities are authoritative on retry/restart;
    /// callers must not recompose a newer queue head for an existing row.
    pub fn ensure_delivery_attempt(
        &mut self,
        input: NewDeliveryAttempt,
    ) -> Result<DeliveryAttempt> {
        if input.id.is_empty()
            || input.text.is_empty()
            || input.text.len() > 65_536
            || input.created_at_ms < 0
            || input.task_id.is_none() != input.task_incarnation_id.is_none()
            || input.task_id.is_none() != input.task_revision.is_none()
        {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        let message_ids_json = serde_json::to_string(
            &input
                .message_ids
                .iter()
                .map(MessageId::as_str)
                .collect::<Vec<_>>(),
        )?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) =
            load_active_delivery_attempt(&transaction, &input.project_id, &input.agent_id)?
        {
            if existing.session_id == input.session_id {
                let owner = require_session_work(&transaction, &input.session_id)?;
                if owner
                    .work
                    .as_ref()
                    .and_then(|work| work.phase.lease())
                    .is_some_and(|lease| lease.attempt_id == existing.id)
                {
                    return Ok(existing);
                }
            }
            return Err(StoreError::SessionWorkConflict);
        }
        let session = load_session(&transaction, &input.session_id)?
            .filter(|session| {
                session.project_id == input.project_id
                    && session.agent_id == input.agent_id
                    && session.state.is_live()
            })
            .ok_or(StoreError::SessionNotLive)?;
        if session.stop_requested_at_ms.is_some() {
            return Err(StoreError::SessionStopping);
        }
        let owner = require_session_work(&transaction, &input.session_id)?;
        let current_work = owner.work.as_ref().ok_or(StoreError::CorruptSessionWork)?;
        if !matches!(current_work.phase, SessionWorkPhase::Empty) {
            return Err(StoreError::SessionWorkConflict);
        }
        let task_work = if let (Some(task_id), Some(incarnation_id), Some(task_revision)) = (
            &input.task_id,
            &input.task_incarnation_id,
            input.task_revision,
        ) {
            let task = load_task(&transaction, task_id)?
                .filter(|task| task.snapshot.project_id == input.project_id)
                .filter(|task| task.snapshot.status == TaskStatus::Queued)
                .filter(|task| task.snapshot.assigned_agent_id.as_ref() == Some(&input.agent_id))
                .ok_or(StoreError::TaskNotQueued)?;
            let current_identity: (String, i64) = transaction.query_row(
                "SELECT incarnation_id, work_revision FROM tasks WHERE id = ?1",
                params![task_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if current_identity != (incarnation_id.clone(), task_revision) {
                return Err(StoreError::TaskNotQueued);
            }
            if input.require_queue_head {
                let head: Option<String> = transaction
                    .query_row(
                        "SELECT id FROM tasks
                         WHERE project_id = ?1 AND assigned_agent_id = ?2 AND status = 'queued'
                         ORDER BY priority DESC, created_at_ms, id LIMIT 1",
                        params![input.project_id.as_str(), input.agent_id.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if head.as_deref() != Some(task_id.as_str()) {
                    return Err(StoreError::SessionWorkConflict);
                }
            }
            let _ = task;
            Some(TaskWork {
                task_id: task_id.clone(),
                incarnation_id: incarnation_id.clone(),
                revision: task_revision,
                run_id: new_run_id()?,
            })
        } else {
            None
        };
        let lease = DeliveryLease {
            attempt_id: input.id.clone(),
            task: task_work,
        };
        let reserved = current_work
            .reserve(lease.clone())
            .map_err(transition_error)?;
        transaction.execute(
            "INSERT INTO delivery_attempts (
                id, project_id, agent_id, session_id, task_id,
                task_incarnation_id, prior_run_count, message_ids_json, text, failure_count,
                next_attempt_at_ms, state, created_at_ms, updated_at_ms,
                task_revision, run_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, NULL, 'in_flight',
                       ?10, ?10, ?11, ?12)",
            params![
                input.id.as_str(),
                input.project_id.as_str(),
                input.agent_id.as_str(),
                input.session_id.as_str(),
                input.task_id.as_ref().map(TaskId::as_str),
                input.task_incarnation_id.as_deref(),
                Option::<i64>::None,
                message_ids_json,
                input.text.as_str(),
                input.created_at_ms,
                input.task_revision,
                lease.task.as_ref().map(|task| task.run_id.as_str()),
            ],
        )?;
        persist_session_work(
            &transaction,
            &input.session_id,
            current_work.revision,
            &reserved,
            input.created_at_ms,
        )?;
        let attempt = load_delivery_attempt(&transaction, &input.id)?
            .ok_or(StoreError::InvalidExecutionMetadata)?;
        transaction.commit()?;
        Ok(attempt)
    }

    pub fn delivery_attempt_for_session(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        session_id: &SessionId,
    ) -> Result<Option<DeliveryAttempt>> {
        self.connection
            .query_row(
                "SELECT id, project_id, agent_id, session_id, task_id,
                        task_incarnation_id, message_ids_json,
                        text, failure_count, next_attempt_at_ms, state,
                        task_revision, run_id
                 FROM delivery_attempts
                 WHERE project_id = ?1 AND agent_id = ?2 AND session_id = ?3
                   AND state IN ('in_flight', 'retryable', 'terminal')",
                params![project_id.as_str(), agent_id.as_str(), session_id.as_str()],
                delivery_attempt_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Resolves the exact nonce-bearing prompt even after recovery cancelled
    /// its attempt. This is the late-hook side of the acknowledgement fence:
    /// a cancelled match with durable recovery-stop intent must be denied by
    /// the provider hook instead of allowed to begin model/tool work.
    pub fn delivery_attempt_for_prompt(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        session_id: &SessionId,
        prompt: &str,
    ) -> Result<Option<DeliveryAttempt>> {
        self.connection
            .query_row(
                "SELECT id, project_id, agent_id, session_id, task_id,
                        task_incarnation_id, message_ids_json,
                        text, failure_count, next_attempt_at_ms, state,
                        task_revision, run_id
                 FROM delivery_attempts
                 WHERE project_id = ?1 AND agent_id = ?2 AND session_id = ?3
                   AND text = ?4
                 ORDER BY created_at_ms DESC, id DESC
                 LIMIT 1",
                params![
                    project_id.as_str(),
                    agent_id.as_str(),
                    session_id.as_str(),
                    prompt
                ],
                delivery_attempt_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn delivery_attempt_due(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        session_id: &SessionId,
        now_ms: i64,
    ) -> Result<bool> {
        let Some(attempt) = self.delivery_attempt_for_session(project_id, agent_id, session_id)?
        else {
            return Ok(false);
        };
        if !matches!(
            attempt.state,
            DeliveryAttemptState::InFlight | DeliveryAttemptState::Retryable
        ) || attempt.next_attempt_at_ms.is_some_and(|at| at > now_ms)
        {
            return Ok(false);
        }
        let owner = require_session_work(&self.connection, session_id)?;
        Ok(matches!(
            owner.work.as_ref().map(|work| &work.phase),
            Some(SessionWorkPhase::Delivering(lease)) if lease.attempt_id == attempt.id
        ))
    }

    /// Atomically claims the exact reservation before any provider bytes are
    /// written. Once this moves authority to `uncertain`, neither journal
    /// retry state nor restart may call it again; only the exact late hook or
    /// exact session termination can resolve ownership.
    pub fn begin_delivery_attempt(
        &mut self,
        attempt_id: &str,
        now_ms: i64,
    ) -> Result<Option<DeliveryAttempt>> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(attempt) = load_delivery_attempt(&transaction, attempt_id)? else {
            return Ok(None);
        };
        if !matches!(
            attempt.state,
            DeliveryAttemptState::InFlight | DeliveryAttemptState::Retryable
        ) {
            return Ok(None);
        }
        let owner = require_session_work(&transaction, &attempt.session_id)?;
        let current = owner.work.as_ref().ok_or(StoreError::CorruptSessionWork)?;
        let uncertain = match current.external_effect_possible(attempt_id) {
            Ok(uncertain) => uncertain,
            Err(_) => return Ok(None),
        };
        transaction.execute(
            "UPDATE delivery_attempts
             SET state = 'in_flight', next_attempt_at_ms = NULL, updated_at_ms = ?1
             WHERE id = ?2 AND state IN ('in_flight', 'retryable')",
            params![now_ms, attempt_id],
        )?;
        persist_session_work(
            &transaction,
            &attempt.session_id,
            current.revision,
            &uncertain,
            now_ms,
        )?;
        let attempt = load_delivery_attempt(&transaction, attempt_id)?;
        transaction.commit()?;
        Ok(attempt)
    }

    pub fn record_delivery_failure(&mut self, attempt_id: &str, now_ms: i64) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE delivery_attempts
             SET failure_count = failure_count + 1,
                 state = CASE WHEN failure_count + 1 >= ?1 THEN 'terminal' ELSE 'retryable' END,
                 next_attempt_at_ms = CASE WHEN failure_count + 1 >= ?1 THEN NULL ELSE ?2 END,
                 updated_at_ms = ?2
             WHERE id = ?3 AND state IN ('in_flight', 'retryable')",
            params![
                MAX_DELIVERY_ATTEMPT_FAILURES,
                now_ms + DELIVERY_RETRY_DELAY_MS,
                attempt_id
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        Ok(())
    }

    pub fn reset_delivery_attempt(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        now_ms: i64,
    ) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(attempt) = load_active_delivery_attempt(&transaction, project_id, agent_id)?
        else {
            transaction.commit()?;
            return Ok(());
        };
        let owner = require_session_work(&transaction, &attempt.session_id)?;
        let exact_reservation = matches!(
            owner.work.as_ref().map(|work| &work.phase),
            Some(SessionWorkPhase::Delivering(lease)) if lease.attempt_id == attempt.id
        );
        if !exact_reservation {
            return Err(StoreError::SessionWorkConflict);
        }
        transaction.execute(
            "UPDATE delivery_attempts
             SET failure_count = 0, state = 'retryable', next_attempt_at_ms = ?1,
                 updated_at_ms = ?1
             WHERE id = ?2 AND state IN ('retryable', 'terminal')",
            params![now_ms, attempt.id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// A daemon restart interrupts `in_flight` journal work. Count that in
    /// the audit record, but never treat journal retryability as permission
    /// to replay an authority that is already `uncertain`.
    pub fn recover_delivery_attempts(&mut self, now_ms: i64) -> Result<()> {
        self.connection.execute(
            "UPDATE delivery_attempts
             SET failure_count = failure_count + 1,
                 state = CASE WHEN failure_count + 1 >= ?1 THEN 'terminal' ELSE 'retryable' END,
                 next_attempt_at_ms = CASE WHEN failure_count + 1 >= ?1 THEN NULL ELSE ?2 END,
                 updated_at_ms = ?2
             WHERE state = 'in_flight'",
            params![
                MAX_DELIVERY_ATTEMPT_FAILURES,
                now_ms + DELIVERY_RETRY_DELAY_MS
            ],
        )?;
        Ok(())
    }

    pub fn delivery_attempt_state(&self, attempt_id: &str) -> Result<Option<DeliveryAttemptState>> {
        self.connection
            .query_row(
                "SELECT state FROM delivery_attempts WHERE id = ?1",
                params![attempt_id],
                |row| parse_delivery_attempt_state(&row.get::<_, String>(0)?, 0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    fn cancel_delivery_attempts_for_task(
        transaction: &Transaction<'_>,
        project_id: &ProjectId,
        task_id: &TaskId,
        now_ms: i64,
    ) -> Result<()> {
        let session_ids = {
            let mut statement = transaction.prepare(
                "SELECT session_id FROM session_work
                 WHERE project_id = ?1 AND task_id = ?2 AND state <> 'empty'",
            )?;
            statement
                .query_map(params![project_id.as_str(), task_id.as_str()], |row| {
                    parse_id::<SessionId>(row.get(0)?, 0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for session_id in session_ids {
            let owner = require_session_work(transaction, &session_id)?;
            let current = owner.work.as_ref().ok_or(StoreError::CorruptSessionWork)?;
            let SessionWorkPhase::Delivering(lease) = &current.phase else {
                return Err(StoreError::SessionWorkConflict);
            };
            let released = current
                .cancel_reservation(&lease.attempt_id)
                .map_err(transition_error)?;
            persist_session_work(
                transaction,
                &session_id,
                current.revision,
                &released,
                now_ms,
            )?;
        }
        transaction.execute(
            "UPDATE delivery_attempts
             SET state = 'cancelled', next_attempt_at_ms = NULL, updated_at_ms = ?1
             WHERE project_id = ?2 AND task_id = ?3
               AND state IN ('in_flight', 'retryable', 'terminal')",
            params![now_ms, project_id.as_str(), task_id.as_str()],
        )?;
        Ok(())
    }

    /// Exact task identity captured before reserving session work.
    pub fn task_delivery_marker(&self, task_id: &TaskId) -> Result<TaskDeliveryMarker> {
        let (incarnation_id, task_revision): (String, i64) = self
            .connection
            .query_row(
                "SELECT incarnation_id, work_revision FROM tasks WHERE id = ?1",
                params![task_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::TaskNotFound)?;
        Ok(TaskDeliveryMarker {
            incarnation_id,
            task_revision,
        })
    }

    /// Whether the exact reserved identity was acknowledged atomically with
    /// authority admission. Task state and historical run counts are not
    /// evidence that this prompt landed.
    pub fn delivery_attempt_acknowledged(
        &self,
        attempt_id: &str,
        session_id: &SessionId,
        task_id: &TaskId,
        incarnation_id: &str,
        task_revision: i64,
        run_id: &RunId,
    ) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM delivery_attempts
                    WHERE id = ?1 AND session_id = ?2 AND task_id = ?3
                      AND task_incarnation_id = ?4 AND task_revision = ?5
                      AND run_id = ?6 AND state = 'acknowledged'
                 )",
                params![
                    attempt_id,
                    session_id.as_str(),
                    task_id.as_str(),
                    incarnation_id,
                    task_revision,
                    run_id.as_str(),
                ],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    /// `factoryctl task done`: closes the open episode `succeeded`,
    /// `closed_by = task_done`, the task `succeeded` with `result`.
    pub fn complete_task(
        &mut self,
        project_id: &ProjectId,
        task_id: &TaskId,
        session_id: &SessionId,
        result: String,
        now_ms: i64,
    ) -> Result<ClosedEpisode> {
        if result.len() > MAX_TASK_RESULT_BYTES {
            return Err(StoreError::InvalidTaskResult);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = open_run_for_task(&transaction, project_id, task_id)?;
        if run.session_id.as_ref() != Some(session_id) {
            return Err(StoreError::SessionNotFound);
        }
        let closed = close_run_in_transaction(
            &transaction,
            &run,
            RunStatus::Succeeded,
            RunClosedBy::TaskDone,
            None,
            TaskStatus::Succeeded,
            Some(result.as_str()),
            None,
            now_ms,
        )?;
        transaction.commit()?;
        Ok(closed)
    }

    /// `factoryctl task blocked`: closes the open episode `stopped`,
    /// `closed_by = task_blocked`, the task `blocked` with `blocked_reason`.
    pub fn block_task(
        &mut self,
        project_id: &ProjectId,
        task_id: &TaskId,
        session_id: &SessionId,
        reason: String,
        now_ms: i64,
    ) -> Result<ClosedEpisode> {
        if reason.is_empty() || reason.len() > MAX_BLOCKED_REASON_BYTES {
            return Err(StoreError::InvalidBlockedReason);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = open_run_for_task(&transaction, project_id, task_id)?;
        if run.session_id.as_ref() != Some(session_id) {
            return Err(StoreError::SessionNotFound);
        }
        let closed = close_run_in_transaction(
            &transaction,
            &run,
            RunStatus::Stopped,
            RunClosedBy::TaskBlocked,
            None,
            TaskStatus::Blocked,
            None,
            Some(reason.as_str()),
            now_ms,
        )?;
        transaction.commit()?;
        Ok(closed)
    }

    /// Closes an open run (task-episode) without touching its session's
    /// process: `stopped`, `closed_by = operator_cancel`, task `cancelled`.
    pub fn cancel_run(
        &mut self,
        project_id: &ProjectId,
        run_id: &RunId,
        now_ms: i64,
    ) -> Result<ClosedEpisode> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_run(&transaction, run_id)?
            .filter(|run| run.project_id == *project_id)
            .ok_or(StoreError::RunNotFound)?;
        if run.status.is_terminal() {
            return Err(StoreError::RunNotStoppable);
        }
        let closed = close_run_in_transaction(
            &transaction,
            &run,
            RunStatus::Stopped,
            RunClosedBy::OperatorCancel,
            None,
            TaskStatus::Cancelled,
            None,
            None,
            now_ms,
        )?;
        transaction.commit()?;
        Ok(closed)
    }

    // --- Agent messages --------------------------------------------------

    /// Stores a private message without appending a public factory event.
    pub fn send_agent_message(&mut self, input: NewAgentMessage) -> Result<AgentMessage> {
        validate_agent_message(&input.body, input.created_at_ms)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        load_agent(&transaction, &input.recipient_agent_id)?
            .filter(|agent| agent.snapshot.project_id == input.project_id)
            .ok_or(StoreError::AgentNotFound)?;
        if let Some(sender_agent_id) = &input.sender_agent_id {
            load_agent(&transaction, sender_agent_id)?
                .filter(|agent| agent.snapshot.project_id == input.project_id)
                .ok_or(StoreError::AgentNotFound)?;
        }
        transaction.execute(
            "INSERT INTO agent_messages (
                id, project_id, sender_agent_id, recipient_agent_id,
                body, created_at_ms, delivered_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
            params![
                input.id.as_str(),
                input.project_id.as_str(),
                input.sender_agent_id.as_ref().map(AgentId::as_str),
                input.recipient_agent_id.as_str(),
                input.body,
                input.created_at_ms,
            ],
        )?;
        let message =
            load_agent_message(&transaction, &input.id)?.ok_or(StoreError::InvalidAgentMessage)?;
        transaction.commit()?;
        Ok(message)
    }

    pub fn list_agent_messages(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        after_id: Option<&MessageId>,
        limit: usize,
    ) -> Result<Vec<AgentMessage>> {
        if !(1..=MAX_STATE_PAGE).contains(&limit) {
            return Err(StoreError::InvalidStateLimit);
        }
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, sender_agent_id, recipient_agent_id,
                    body, created_at_ms, delivered_at_ms, delivered_run_id,
                    delivered_session_id
             FROM agent_messages
             WHERE project_id = ?1 AND recipient_agent_id = ?2
               AND (?3 IS NULL OR id > ?3)
             ORDER BY id
             LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                project_id.as_str(),
                agent_id.as_str(),
                after_id.map(MessageId::as_str),
                limit as i64,
            ],
            agent_message_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Every undelivered inbox message for an agent, oldest first.
    pub fn undelivered_messages_for_agent(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
    ) -> Result<Vec<AgentMessage>> {
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, sender_agent_id, recipient_agent_id,
                    body, created_at_ms, delivered_at_ms, delivered_run_id,
                    delivered_session_id
             FROM agent_messages
             WHERE project_id = ?1 AND recipient_agent_id = ?2 AND delivered_at_ms IS NULL
             ORDER BY created_at_ms, id",
        )?;
        let rows = statement.query_map(
            params![project_id.as_str(), agent_id.as_str()],
            agent_message_from_row,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    fn deliver_agent_messages_in_transaction(
        transaction: &Transaction<'_>,
        project_id: &ProjectId,
        agent_id: &AgentId,
        session_id: &SessionId,
        run_id: Option<&RunId>,
        delivered_at_ms: i64,
    ) -> Result<Vec<AgentMessage>> {
        let mut statement = transaction.prepare(
            "SELECT id
             FROM agent_messages
             WHERE project_id = ?1 AND recipient_agent_id = ?2
               AND delivered_at_ms IS NULL
             ORDER BY created_at_ms, id",
        )?;
        let ids = statement
            .query_map(params![project_id.as_str(), agent_id.as_str()], |row| {
                parse_id::<MessageId>(row.get(0)?, 0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        Self::deliver_agent_messages_by_ids_in_transaction(
            transaction,
            project_id,
            agent_id,
            session_id,
            run_id,
            &ids,
            delivered_at_ms,
        )
    }

    fn deliver_agent_messages_by_ids_in_transaction(
        transaction: &Transaction<'_>,
        project_id: &ProjectId,
        agent_id: &AgentId,
        session_id: &SessionId,
        run_id: Option<&RunId>,
        message_ids: &[MessageId],
        delivered_at_ms: i64,
    ) -> Result<Vec<AgentMessage>> {
        let mut messages = Vec::with_capacity(message_ids.len());
        for message_id in message_ids {
            let changed = transaction.execute(
                "UPDATE agent_messages
                 SET delivered_at_ms = ?1, delivered_session_id = ?2, delivered_run_id = ?3
                 WHERE id = ?4 AND project_id = ?5 AND recipient_agent_id = ?6
                   AND delivered_at_ms IS NULL",
                params![
                    delivered_at_ms,
                    session_id.as_str(),
                    run_id.map(RunId::as_str),
                    message_id.as_str(),
                    project_id.as_str(),
                    agent_id.as_str(),
                ],
            )?;
            if changed == 1 {
                messages.push(
                    load_agent_message(transaction, message_id)?
                        .ok_or(StoreError::InvalidAgentMessage)?,
                );
            }
        }
        Ok(messages)
    }

    /// Delivers every undelivered inbox message as a standalone nudge (no
    /// task, no run) into a live session.
    pub fn deliver_agent_messages(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        session_id: &SessionId,
        delivered_at_ms: i64,
    ) -> Result<Vec<AgentMessage>> {
        if delivered_at_ms < 0 {
            return Err(StoreError::InvalidAgentMessage);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let messages = Self::deliver_agent_messages_in_transaction(
            &transaction,
            project_id,
            agent_id,
            session_id,
            None,
            delivered_at_ms,
        )?;
        transaction.commit()?;
        Ok(messages)
    }

    pub fn deliver_agent_messages_by_ids(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        session_id: &SessionId,
        message_ids: &[MessageId],
        delivered_at_ms: i64,
    ) -> Result<Vec<AgentMessage>> {
        self.deliver_agent_messages_by_ids_with_delivery_attempt(
            project_id,
            agent_id,
            session_id,
            message_ids,
            None,
            delivered_at_ms,
        )
    }

    pub fn deliver_agent_messages_by_ids_with_delivery_attempt(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        session_id: &SessionId,
        message_ids: &[MessageId],
        delivery_attempt_id: Option<&str>,
        delivered_at_ms: i64,
    ) -> Result<Vec<AgentMessage>> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let acknowledged = if let Some(attempt_id) = delivery_attempt_id {
            let session = load_session(&transaction, session_id)?
                .filter(|session| {
                    session.project_id == *project_id && session.agent_id == *agent_id
                })
                .ok_or(StoreError::SessionNotFound)?;
            if !session.state.is_live() {
                return Err(StoreError::SessionNotLive);
            }
            if session.stop_requested_at_ms.is_some() {
                return Err(StoreError::SessionStopping);
            }
            let attempt = load_delivery_attempt(&transaction, attempt_id)?
                .filter(|attempt| {
                    attempt.project_id == *project_id
                        && attempt.agent_id == *agent_id
                        && attempt.session_id == *session_id
                        && attempt.task_id.is_none()
                        && attempt.message_ids == message_ids
                })
                .ok_or(StoreError::InvalidExecutionMetadata)?;
            let owner = require_session_work(&transaction, session_id)?;
            let current = owner.work.as_ref().ok_or(StoreError::CorruptSessionWork)?;
            let (ready, audit_state) = prepare_delivery_acknowledgement(current, &attempt)?;
            Some((
                current.revision,
                ready.acknowledge(attempt_id).map_err(transition_error)?,
                audit_state,
            ))
        } else {
            None
        };
        let messages = Self::deliver_agent_messages_by_ids_in_transaction(
            &transaction,
            project_id,
            agent_id,
            session_id,
            None,
            message_ids,
            delivered_at_ms,
        )?;
        if let Some(attempt_id) = delivery_attempt_id {
            let changed = transaction.execute(
                "UPDATE delivery_attempts
                 SET state = 'acknowledged', next_attempt_at_ms = NULL, updated_at_ms = ?1
                 WHERE id = ?2 AND session_id = ?3
                   AND state = ?4",
                params![
                    delivered_at_ms,
                    attempt_id,
                    session_id.as_str(),
                    acknowledged
                        .as_ref()
                        .map(|(_, _, audit_state)| *audit_state)
                        .ok_or(StoreError::InvalidExecutionMetadata)?
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::InvalidExecutionMetadata);
            }
        }
        if let Some((expected_revision, acknowledged, _)) = acknowledged {
            if !matches!(acknowledged.phase, SessionWorkPhase::Empty) {
                return Err(StoreError::CorruptSessionWork);
            }
            persist_session_work(
                &transaction,
                session_id,
                expected_revision,
                &acknowledged,
                delivered_at_ms,
            )?;
        }
        transaction.commit()?;
        Ok(messages)
    }

    // --- Everything below is largely unchanged from the pre-sessions store.

    pub fn webhook_snapshot(
        &self,
        project_id: &ProjectId,
        orchestrator_agent_id: &AgentId,
        now_ms: i64,
    ) -> Result<WebhookSnapshot> {
        validate_webhook_project_and_orchestrator(
            &self.connection,
            project_id,
            orchestrator_agent_id,
        )?;
        let (todo, doing, blocked, done) = self.connection.query_row(
            "SELECT
                COALESCE(SUM(status = 'queued'), 0),
                COALESCE(SUM(status = 'running'), 0),
                COALESCE(SUM(status = 'blocked'), 0),
                COALESCE(SUM(status IN ('succeeded', 'failed', 'cancelled')), 0)
             FROM tasks WHERE project_id = ?1",
            params![project_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let mut tasks = load_webhook_snapshot_tasks(
            &self.connection,
            project_id,
            false,
            MAX_WEBHOOK_ACTIVE_TASKS,
        )?;
        tasks.extend(load_webhook_snapshot_tasks(
            &self.connection,
            project_id,
            true,
            MAX_WEBHOOK_DONE_TASKS,
        )?);

        let agent_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM agents WHERE project_id = ?1",
            params![project_id.as_str()],
            |row| row.get(0),
        )?;
        if agent_count > usize_to_i64(MAX_WEBHOOK_SNAPSHOT_AGENTS)? {
            return Err(StoreError::WebhookSnapshotTooLarge);
        }
        let mut statement = self.connection.prepare(
            "SELECT a.id, a.role, a.provider,
                    (SELECT MAX(r.updated_at_ms) FROM runs r WHERE r.agent_id = a.id),
                    (SELECT COUNT(*) FROM tasks t
                     WHERE t.project_id = a.project_id
                       AND t.assigned_agent_id = a.id
                       AND t.status IN ('queued', 'blocked')),
                    (SELECT s.observer_health FROM sessions s
                     WHERE s.agent_id = a.id AND s.ended_at_ms IS NULL)
             FROM agents a
             WHERE a.project_id = ?1
             ORDER BY a.id",
        )?;
        let agents = statement
            .query_map(params![project_id.as_str()], |row| {
                let id: AgentId = parse_id(row.get(0)?, 0)?;
                let core_role: String = row.get(1)?;
                let provider: String = row.get(2)?;
                let core_role = parse_agent_role(&core_role, 1)?;
                let last_active_at_ms: Option<i64> = row.get(3)?;
                let observer_health: Option<String> = row.get(5)?;
                Ok(WebhookSnapshotAgent {
                    is_orchestrator: &id == orchestrator_agent_id,
                    id: id.clone(),
                    name: id.to_string(),
                    role: agent_role_value(core_role).to_owned(),
                    control_role: core_role,
                    provider: Some(parse_provider(&provider, 2)?),
                    last_active_sec_ago: last_active_at_ms
                        .map(|last_active| now_ms.saturating_sub(last_active).max(0) / 1_000),
                    inbox_backlog: row.get(4)?,
                    observer_health: observer_health
                        .as_deref()
                        .map(|value| parse_observer_health(value, 5))
                        .transpose()?
                        .unwrap_or(ObserverHealth::Unknown),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);

        Ok(WebhookSnapshot {
            generated_at_ms: now_ms,
            counts: WebhookTaskCounts {
                todo,
                doing,
                blocked,
                done,
            },
            tasks,
            agents,
        })
    }

    /// Creates one queued task already assigned to the explicit orchestrator and
    /// stores only the hash of the caller-returned capability token.
    pub fn create_webhook_task(
        &mut self,
        input: NewWebhookTask,
    ) -> Result<(WebhookCreated, Vec<EventEnvelope>)> {
        validate_webhook_create_input(&input)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_webhook_project_and_orchestrator(
            &transaction,
            &input.project_id,
            &input.orchestrator_agent_id,
        )?;
        let collision: bool = transaction.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM tasks WHERE id = ?1
                UNION ALL
                SELECT 1 FROM webhook_task_capabilities WHERE token_sha256 = ?2
             )",
            params![input.id.as_str(), input.token_sha256.as_slice()],
            |row| row.get(0),
        )?;
        if collision {
            return Err(StoreError::InvalidWebhookInput);
        }
        transaction.execute(
            "INSERT INTO tasks (
                id, project_id, parent_task_id, assigned_agent_id, title, body,
                status, priority, created_at_ms, updated_at_ms,
                started_at_ms, completed_at_ms, result
             ) VALUES (
                ?1, ?2, NULL, ?3, ?4, ?5, 'queued', 1, ?6, ?6,
                NULL, NULL, NULL
             )",
            params![
                input.id.as_str(),
                input.project_id.as_str(),
                input.orchestrator_agent_id.as_str(),
                input.title.as_str(),
                input.body.as_str(),
                input.created_at_ms,
            ],
        )?;
        transaction.execute(
            "INSERT INTO webhook_task_capabilities (
                task_id, project_id, endpoint_id, token_sha256
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                input.id.as_str(),
                input.project_id.as_str(),
                input.endpoint_id.as_str(),
                input.token_sha256.as_slice(),
            ],
        )?;
        let snapshot = TaskSnapshot {
            id: input.id.clone(),
            project_id: input.project_id,
            parent_task_id: None,
            assigned_agent_id: Some(input.orchestrator_agent_id),
            title: input.title.clone(),
            status: TaskStatus::Queued,
            priority: 1,
            created_at_ms: input.created_at_ms,
            updated_at_ms: input.created_at_ms,
        };
        let event = FactoryEvent::TaskChanged { task: snapshot };
        let sequence = append_event(&transaction, input.created_at_ms, &event)?;
        transaction.commit()?;
        Ok((
            WebhookCreated {
                task_id: input.id,
                status: OperationalTaskStatus::Todo,
                title: input.title,
            },
            vec![EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: input.created_at_ms,
                event,
            }],
        ))
    }

    pub fn poll_webhook_task(
        &self,
        endpoint_id: &str,
        project_id: &ProjectId,
        token_sha256: &[u8; 32],
    ) -> Result<Option<WebhookTaskPoll>> {
        if !valid_endpoint_id(endpoint_id) {
            return Ok(None);
        }
        self.connection
            .query_row(
                "SELECT t.status, t.title, t.result
                 FROM webhook_task_capabilities mt
                 JOIN tasks t ON t.id = mt.task_id
                 WHERE mt.endpoint_id = ?1 AND mt.project_id = ?2
                   AND mt.token_sha256 = ?3",
                params![endpoint_id, project_id.as_str(), token_sha256.as_slice()],
                |row| {
                    let status: String = row.get(0)?;
                    Ok(WebhookTaskPoll {
                        status: operational_task_status(&status, 0)?,
                        title: truncate_utf8(&row.get::<_, String>(1)?, MAX_WEBHOOK_TITLE_BYTES),
                        result: row
                            .get::<_, Option<String>>(2)?
                            .map(|value| truncate_utf8(&value, MAX_WEBHOOK_TEXT_BYTES)),
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Records the latest open answer and queues a private orchestrator notification in
    /// the same transaction. The source task's current status is preserved for
    /// orchestrator review.
    pub fn answer_webhook_question(&mut self, input: WebhookAnswer) -> Result<WebhookMutation> {
        validate_webhook_answer_input(&input)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_webhook_project_and_orchestrator(
            &transaction,
            &input.project_id,
            &input.orchestrator_agent_id,
        )?;
        let source = load_task(&transaction, &input.task_id)?
            .filter(|task| task.snapshot.project_id == input.project_id)
            .ok_or(StoreError::WebhookTaskNotFound)?;
        if input.answered_at_ms < source.snapshot.updated_at_ms {
            return Err(StoreError::InvalidWebhookInput);
        }
        let notification_exists: bool = transaction.query_row(
            "SELECT EXISTS (SELECT 1 FROM tasks WHERE id = ?1)",
            params![input.notification_task_id.as_str()],
            |row| row.get(0),
        )?;
        if notification_exists || input.notification_task_id == input.task_id {
            return Err(StoreError::InvalidWebhookInput);
        }
        let question: Option<(i64, String)> = transaction
            .query_row(
                "SELECT id, text FROM task_questions
                 WHERE task_id = ?1 AND project_id = ?2 AND answer IS NULL
                 ORDER BY asked_at_ms DESC, ordinal DESC, id DESC
                 LIMIT 1",
                params![input.task_id.as_str(), input.project_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (question_id, question_text) = question.ok_or(StoreError::WebhookQuestionNotOpen)?;
        let answered = transaction.execute(
            "UPDATE task_questions
             SET answer = ?1, answered_at_ms = ?2
             WHERE id = ?3 AND project_id = ?4 AND answer IS NULL",
            params![
                input.answer.as_str(),
                input.answered_at_ms,
                question_id,
                input.project_id.as_str(),
            ],
        )?;
        if answered != 1 {
            return Err(StoreError::WebhookQuestionNotOpen);
        }
        transaction.execute(
            "UPDATE tasks SET updated_at_ms = ?1
             WHERE id = ?2 AND project_id = ?3",
            params![
                input.answered_at_ms,
                input.task_id.as_str(),
                input.project_id.as_str(),
            ],
        )?;

        let notification_title = format!("Webhook answer for {}", input.task_id.as_str());
        let notification_body = format!(
            "Source task: {}\nSource title: {}\n\nQuestion:\n{}\n\nAnswer:\n{}",
            input.task_id.as_str(),
            truncate_utf8(&source.snapshot.title, 512),
            truncate_utf8(&question_text, 48_000),
            truncate_utf8(&input.answer, 48_000),
        );
        transaction.execute(
            "INSERT INTO tasks (
                id, project_id, parent_task_id, assigned_agent_id, title, body,
                status, priority, created_at_ms, updated_at_ms,
                started_at_ms, completed_at_ms, result
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, 'queued', 1, ?7, ?7,
                NULL, NULL, NULL
             )",
            params![
                input.notification_task_id.as_str(),
                input.project_id.as_str(),
                input.task_id.as_str(),
                input.orchestrator_agent_id.as_str(),
                notification_title.as_str(),
                notification_body.as_str(),
                input.answered_at_ms,
            ],
        )?;

        let source_snapshot = load_task(&transaction, &input.task_id)?
            .ok_or(StoreError::WebhookTaskNotFound)?
            .snapshot;
        let notification_snapshot = TaskSnapshot {
            id: input.notification_task_id,
            project_id: input.project_id,
            parent_task_id: Some(input.task_id),
            assigned_agent_id: Some(input.orchestrator_agent_id),
            title: notification_title,
            status: TaskStatus::Queued,
            priority: 1,
            created_at_ms: input.answered_at_ms,
            updated_at_ms: input.answered_at_ms,
        };
        let factory_events = [
            FactoryEvent::TaskChanged {
                task: source_snapshot,
            },
            FactoryEvent::TaskChanged {
                task: notification_snapshot,
            },
        ];
        let events = factory_events
            .into_iter()
            .map(|event| {
                let sequence = append_event(&transaction, input.answered_at_ms, &event)?;
                Ok(EventEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    sequence,
                    occurred_at_ms: input.answered_at_ms,
                    event,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let open_question_remains: bool = transaction.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM task_questions
                WHERE task_id = ?1 AND project_id = ?2 AND answer IS NULL
             )",
            params![
                source.snapshot.id.as_str(),
                source.snapshot.project_id.as_str()
            ],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        Ok(WebhookMutation {
            events,
            open_question_remains,
        })
    }

    /// Resolves only an immutable document still attached to an unanswered
    /// question on the requested task.
    pub fn webhook_document(
        &self,
        project_id: &ProjectId,
        task_id: &TaskId,
        document_id: &str,
    ) -> Result<Option<WebhookDocument>> {
        if !valid_webhook_document_id(document_id) {
            return Ok(None);
        }
        self.connection
            .query_row(
                "SELECT d.id, d.name, d.reference, d.revision, d.content
                 FROM tasks t
                 JOIN task_questions q
                   ON q.task_id = t.id AND q.project_id = t.project_id
                  AND q.id = (
                      SELECT latest.id
                      FROM task_questions latest
                      WHERE latest.task_id = t.id
                        AND latest.project_id = t.project_id
                        AND latest.answer IS NULL
                      ORDER BY latest.asked_at_ms DESC, latest.ordinal DESC,
                               latest.id DESC
                      LIMIT 1
                  )
                 JOIN task_question_documents qd
                   ON qd.question_id = q.id AND qd.project_id = q.project_id
                 JOIN task_documents d
                   ON d.project_id = qd.project_id AND d.id = qd.document_id
                 WHERE t.project_id = ?1 AND t.id = ?2 AND d.id = ?3
                 LIMIT 1",
                params![project_id.as_str(), task_id.as_str(), document_id],
                |row| {
                    Ok(WebhookDocument {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        reference: row.get(2)?,
                        revision: row.get(3)?,
                        content: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn get_project(&self, project_id: &ProjectId) -> Result<ProjectSnapshot> {
        self.connection
            .query_row(
                "SELECT id, name, root, created_at_ms, updated_at_ms
                 FROM projects WHERE id = ?1",
                params![project_id.as_str()],
                |row| {
                    Ok(ProjectSnapshot {
                        id: parse_id(row.get(0)?, 0)?,
                        name: row.get(1)?,
                        root: row.get(2)?,
                        created_at_ms: row.get(3)?,
                        updated_at_ms: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)?
            .ok_or(StoreError::ProjectNotFound)
    }

    pub fn set_repository_authority(
        &mut self,
        project_id: &ProjectId,
        authority: &RepositoryAuthority,
        now_ms: i64,
    ) -> Result<EventEnvelope> {
        self.get_project(project_id)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_live_session: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE ended_at_ms IS NULL)",
            [],
            |row| row.get(0),
        )?;
        if has_live_session {
            return Err(StoreError::RepositoryAuthorityRequiresIdleProject);
        }
        transaction.execute(
            "INSERT INTO project_repository_authority (project_id, remote_url, base_branch, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![project_id.as_str(), authority.remote_url, authority.base_branch, now_ms],
        )?;
        let event = FactoryEvent::RepositoryAuthorityChanged {
            project_id: project_id.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: now_ms,
            event,
        })
    }

    pub fn repository_authority(&self, project_id: &ProjectId) -> Result<RepositoryAuthority> {
        self.connection.query_row(
            "SELECT remote_url, base_branch FROM project_repository_authority WHERE project_id = ?1",
            params![project_id.as_str()],
            |row| Ok(RepositoryAuthority { remote_url: row.get(0)?, base_branch: row.get(1)? }),
        ).optional()?.ok_or(StoreError::RepositoryAuthorityMissing)
    }

    pub fn list_projects(
        &self,
        after_id: Option<&ProjectId>,
        limit: usize,
    ) -> Result<Vec<ProjectSnapshot>> {
        if !(1..=MAX_STATE_PAGE).contains(&limit) {
            return Err(StoreError::InvalidStateLimit);
        }
        let mut statement = self.connection.prepare(
            "SELECT id, name, root, created_at_ms, updated_at_ms
             FROM projects
             WHERE (?1 IS NULL OR id > ?1)
             ORDER BY id
             LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![after_id.map(ProjectId::as_str), limit as i64],
            |row| {
                Ok(ProjectSnapshot {
                    id: parse_id(row.get(0)?, 0)?,
                    name: row.get(1)?,
                    root: row.get(2)?,
                    created_at_ms: row.get(3)?,
                    updated_at_ms: row.get(4)?,
                })
            },
        )?;

        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn list_tasks(
        &self,
        project_id: &ProjectId,
        after_id: Option<&TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskDetail>> {
        self.list_tasks_filtered(project_id, after_id, None, true, limit)
    }

    pub fn list_tasks_filtered(
        &self,
        project_id: &ProjectId,
        after_id: Option<&TaskId>,
        agent_id: Option<&AgentId>,
        include_history: bool,
        limit: usize,
    ) -> Result<Vec<TaskDetail>> {
        self.list_tasks_filtered_at_revision(
            project_id,
            after_id,
            agent_id,
            include_history,
            limit,
            None,
        )
        .map(|(tasks, _)| tasks)
    }

    pub fn list_tasks_filtered_at_revision(
        &self,
        project_id: &ProjectId,
        after_id: Option<&TaskId>,
        agent_id: Option<&AgentId>,
        include_history: bool,
        limit: usize,
        expected_revision: Option<i64>,
    ) -> Result<(Vec<TaskDetail>, i64)> {
        if !(1..=MAX_STATE_PAGE).contains(&limit) {
            return Err(StoreError::InvalidStateLimit);
        }
        match (after_id, expected_revision) {
            (Some(_), None) => return Err(StoreError::MissingTaskCursorRevision),
            (None, Some(_)) => return Err(StoreError::UnexpectedTaskCursorRevision),
            _ => {}
        }
        let revision: i64 =
            self.connection
                .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| {
                    row.get(0)
                })?;
        if expected_revision.is_some_and(|expected| expected != revision) {
            return Err(StoreError::StaleTaskCursor);
        }
        if let Some(agent_id) = agent_id {
            let exists = load_agent(&self.connection, agent_id)?
                .is_some_and(|agent| agent.snapshot.project_id == *project_id);
            if !exists {
                return Err(StoreError::AgentNotFound);
            }
        }
        if let Some(after_id) = after_id {
            let cursor_exists: bool = self.connection.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM tasks
                    WHERE project_id = ?1 AND id = ?2
                      AND (?3 IS NULL OR assigned_agent_id = ?3)
                      AND (?4 OR status IN ('queued', 'running'))
                )",
                params![
                    project_id.as_str(),
                    after_id.as_str(),
                    agent_id.map(AgentId::as_str),
                    include_history,
                ],
                |row| row.get(0),
            )?;
            if !cursor_exists {
                return Err(StoreError::TaskNotFound);
            }
        }
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, parent_task_id, assigned_agent_id, title, body, result,
                    status, priority, created_at_ms, updated_at_ms, blocked_reason
             FROM tasks
            WHERE project_id = ?1
               AND (?2 IS NULL OR assigned_agent_id = ?2)
               AND (?3 OR status IN ('queued', 'running'))
               AND (?4 IS NULL OR
                    (CASE WHEN status = 'running' THEN 1 ELSE 0 END) < (
                        SELECT CASE WHEN status = 'running' THEN 1 ELSE 0 END
                        FROM tasks WHERE project_id = ?1 AND id = ?4
                    ) OR (
                        (CASE WHEN status = 'running' THEN 1 ELSE 0 END) = (
                            SELECT CASE WHEN status = 'running' THEN 1 ELSE 0 END
                            FROM tasks WHERE project_id = ?1 AND id = ?4
                        )
                        AND (priority < (SELECT priority FROM tasks WHERE project_id = ?1 AND id = ?4)
                             OR (priority = (SELECT priority FROM tasks WHERE project_id = ?1 AND id = ?4)
                                 AND (created_at_ms, id) > (SELECT created_at_ms, id FROM tasks
                                                             WHERE project_id = ?1 AND id = ?4)))
                    ))
             ORDER BY (status = 'running') DESC, priority DESC, created_at_ms, id
             LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![
                project_id.as_str(),
                agent_id.map(AgentId::as_str),
                include_history,
                after_id.map(TaskId::as_str),
                limit as i64,
            ],
            |row| {
                let parent_id: Option<String> = row.get(2)?;
                let assigned_id: Option<String> = row.get(3)?;
                let status: String = row.get(7)?;
                Ok(TaskDetail {
                    snapshot: TaskSnapshot {
                        id: parse_id(row.get(0)?, 0)?,
                        project_id: parse_id(row.get(1)?, 1)?,
                        parent_task_id: parse_optional_id(parent_id, 2)?,
                        assigned_agent_id: parse_optional_id(assigned_id, 3)?,
                        title: row.get(4)?,
                        status: parse_task_status(&status, 7)?,
                        priority: row.get(8)?,
                        created_at_ms: row.get(9)?,
                        updated_at_ms: row.get(10)?,
                    },
                    body: row.get(5)?,
                    result: row.get(6)?,
                    blocked_reason: row.get(11)?,
                })
            },
        )?;

        Ok((rows.collect::<std::result::Result<Vec<_>, _>>()?, revision))
    }

    pub fn get_task(&self, project_id: &ProjectId, task_id: &TaskId) -> Result<TaskDetail> {
        let task = load_task(&self.connection, task_id)?.ok_or(StoreError::TaskNotFound)?;
        if task.snapshot.project_id != *project_id {
            return Err(StoreError::TaskNotFound);
        }
        Ok(task)
    }

    /// Whether an authenticated session may address `target` directly.
    /// Workers may report to their daemon-owned parent (or themselves);
    /// orchestrators may address only themselves and current descendants.
    pub fn message_recipient_is_in_principal_scope(
        &self,
        principal: &AuthenticatedPrincipal,
        target: &AgentId,
    ) -> Result<bool> {
        if principal.role == AgentRole::Orchestrator {
            return self
                .connection
                .query_row(
                    "WITH RECURSIVE visible(id) AS (
                         SELECT ?1
                         UNION ALL
                         SELECT agents.id FROM agents
                         JOIN visible ON agents.parent_agent_id = visible.id
                         WHERE agents.project_id = ?2
                     )
                     SELECT EXISTS(SELECT 1 FROM visible WHERE id = ?3)",
                    params![
                        principal.agent_id.as_str(),
                        principal.project_id.as_str(),
                        target.as_str(),
                    ],
                    |row| row.get(0),
                )
                .map_err(StoreError::from);
        }
        self.connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM agents sender
                     JOIN agents recipient ON recipient.project_id = sender.project_id
                     WHERE sender.id = ?1
                       AND sender.project_id = ?2
                       AND recipient.id = ?3
                       AND (recipient.id = sender.id
                            OR recipient.id = sender.parent_agent_id)
                 )",
                params![
                    principal.agent_id.as_str(),
                    principal.project_id.as_str(),
                    target.as_str(),
                ],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    pub fn authenticated_session_row(&self, session_id: &SessionId) -> Result<SessionRow> {
        let enabled: bool = self
            .connection
            .query_row(
                "SELECT principal_version = 1 FROM sessions
             WHERE id = ?1 AND ended_at_ms IS NULL",
                params![session_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::SessionNotFound)?;
        if !enabled {
            return Err(StoreError::SessionNotFound);
        }
        load_session(&self.connection, session_id)?.ok_or(StoreError::SessionNotFound)
    }

    pub fn retry_task(
        &mut self,
        project_id: &ProjectId,
        task_id: &TaskId,
        now_ms: i64,
    ) -> Result<(TaskDetail, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, task_id)?
            .filter(|task| task.snapshot.project_id == *project_id)
            .ok_or(StoreError::TaskNotFound)?;
        if !matches!(
            task.snapshot.status,
            TaskStatus::Blocked | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            return Err(StoreError::TaskNotRetryable);
        }
        Self::cancel_delivery_attempts_for_task(&transaction, project_id, task_id, now_ms)?;
        let changed = transaction.execute(
            "UPDATE tasks
             SET status = 'queued', result = NULL, started_at_ms = NULL,
                 completed_at_ms = NULL, blocked_reason = NULL, updated_at_ms = ?1,
                 work_revision = work_revision + 1
             WHERE id = ?2 AND project_id = ?3
               AND status IN ('blocked', 'failed', 'cancelled')",
            params![now_ms, task_id.as_str(), project_id.as_str()],
        )?;
        if changed != 1 {
            return Err(StoreError::TaskNotRetryable);
        }
        let task = load_task(&transaction, task_id)?.ok_or(StoreError::TaskNotFound)?;
        let event = FactoryEvent::TaskChanged {
            task: task.snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            task,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    pub fn assign_task(
        &mut self,
        project_id: &ProjectId,
        task_id: &TaskId,
        agent_id: Option<&AgentId>,
        now_ms: i64,
    ) -> Result<(TaskDetail, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, task_id)?
            .filter(|task| task.snapshot.project_id == *project_id)
            .ok_or(StoreError::TaskNotFound)?;
        if task.snapshot.status != TaskStatus::Queued {
            return Err(StoreError::TaskNotQueued);
        }
        if let Some(agent_id) = agent_id {
            let exists = load_agent(&transaction, agent_id)?
                .is_some_and(|agent| agent.snapshot.project_id == *project_id);
            if !exists {
                return Err(StoreError::AgentNotFound);
            }
        }
        Self::cancel_delivery_attempts_for_task(&transaction, project_id, task_id, now_ms)?;
        transaction.execute(
            "UPDATE tasks
             SET assigned_agent_id = ?1, updated_at_ms = ?2,
                 work_revision = work_revision + 1
             WHERE id = ?3 AND project_id = ?4 AND status = 'queued'",
            params![
                agent_id.map(AgentId::as_str),
                now_ms,
                task_id.as_str(),
                project_id.as_str(),
            ],
        )?;
        let task = load_task(&transaction, task_id)?.ok_or(StoreError::TaskNotFound)?;
        let event = FactoryEvent::TaskChanged {
            task: task.snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            task,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    /// Cancels a queued or blocked task (kept assignment; `retry_task` can
    /// requeue it), or -- if the task is `running` -- closes its open
    /// episode with `closed_by = operator_cancel` and the task `cancelled`,
    /// leaving the session untouched.
    pub fn cancel_task(
        &mut self,
        project_id: &ProjectId,
        task_id: &TaskId,
        now_ms: i64,
    ) -> Result<(TaskDetail, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, task_id)?
            .filter(|task| task.snapshot.project_id == *project_id)
            .ok_or(StoreError::TaskNotFound)?;
        if task.snapshot.status == TaskStatus::Running {
            let run_id: String = transaction
                .query_row(
                    "SELECT id FROM runs WHERE task_id = ?1 AND project_id = ?2
                     AND ended_at_ms IS NULL",
                    params![task_id.as_str(), project_id.as_str()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or(StoreError::TaskNotCancellable)?;
            let run_id: RunId = parse_id(run_id, 0)?;
            let run = load_run(&transaction, &run_id)?.ok_or(StoreError::RunNotFound)?;
            let closed = close_run_in_transaction(
                &transaction,
                &run,
                RunStatus::Stopped,
                RunClosedBy::OperatorCancel,
                None,
                TaskStatus::Cancelled,
                None,
                None,
                now_ms,
            )?;
            let event = closed
                .events
                .into_iter()
                .find(|event| matches!(event.event, FactoryEvent::TaskChanged { .. }))
                .ok_or(StoreError::TaskNotFound)?;
            transaction.commit()?;
            return Ok((closed.task, event));
        }
        Self::cancel_delivery_attempts_for_task(&transaction, project_id, task_id, now_ms)?;
        let changed = transaction.execute(
            "UPDATE tasks
             SET status = 'cancelled', updated_at_ms = ?1, completed_at_ms = ?1,
                 work_revision = work_revision + 1
             WHERE id = ?2 AND project_id = ?3 AND status IN ('queued', 'blocked')",
            params![now_ms, task_id.as_str(), project_id.as_str()],
        )?;
        if changed != 1 {
            return Err(StoreError::TaskNotCancellable);
        }
        let task = load_task(&transaction, task_id)?.ok_or(StoreError::TaskNotFound)?;
        let event = FactoryEvent::TaskChanged {
            task: task.snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            task,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    /// Edits a queued task's title and/or body. Bounds are enforced by the
    /// local API layer, mirroring `CreateTask`.
    pub fn update_task(
        &mut self,
        project_id: &ProjectId,
        task_id: &TaskId,
        title: Option<String>,
        body: Option<String>,
        priority: Option<i32>,
        now_ms: i64,
    ) -> Result<(TaskDetail, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, task_id)?
            .filter(|task| task.snapshot.project_id == *project_id)
            .ok_or(StoreError::TaskNotFound)?;
        if task.snapshot.status != TaskStatus::Queued {
            return Err(StoreError::TaskNotEditable);
        }
        Self::cancel_delivery_attempts_for_task(&transaction, project_id, task_id, now_ms)?;
        let changed = transaction.execute(
            "UPDATE tasks
             SET title = COALESCE(?1, title), body = COALESCE(?2, body),
                 priority = COALESCE(?3, priority), updated_at_ms = ?4,
                 work_revision = work_revision + 1
             WHERE id = ?5 AND project_id = ?6 AND status = 'queued'",
            params![
                title,
                body,
                priority,
                now_ms,
                task_id.as_str(),
                project_id.as_str()
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::TaskNotEditable);
        }
        let task = load_task(&transaction, task_id)?.ok_or(StoreError::TaskNotFound)?;
        let event = FactoryEvent::TaskChanged {
            task: task.snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            task,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    /// Deletes a task that has no non-terminal run, no subtasks, and no run
    /// that is itself the parent of another run. Terminal runs and every row
    /// that references the task (questions, webhook capabilities) are
    /// removed in the same transaction.
    pub fn delete_task(
        &mut self,
        project_id: &ProjectId,
        task_id: &TaskId,
        now_ms: i64,
    ) -> Result<EventEnvelope> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let _task = load_task(&transaction, task_id)?
            .filter(|task| task.snapshot.project_id == *project_id)
            .ok_or(StoreError::TaskNotFound)?;
        let has_active_run: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM runs
                WHERE task_id = ?1 AND status NOT IN ('succeeded', 'failed', 'stopped')
             )",
            params![task_id.as_str()],
            |row| row.get(0),
        )?;
        if has_active_run {
            return Err(StoreError::TaskHasActiveRun);
        }
        let has_subtasks: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM tasks WHERE parent_task_id = ?1 AND project_id = ?2
             )",
            params![task_id.as_str(), project_id.as_str()],
            |row| row.get(0),
        )?;
        if has_subtasks {
            return Err(StoreError::TaskHasSubtasks);
        }
        let has_dependent_runs: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM runs child
                JOIN runs parent ON parent.id = child.parent_run_id
                WHERE parent.task_id = ?1
             )",
            params![task_id.as_str()],
            |row| row.get(0),
        )?;
        if has_dependent_runs {
            return Err(StoreError::TaskRunHasDependents);
        }

        transaction.execute_batch("PRAGMA defer_foreign_keys = ON;")?;
        Self::cancel_delivery_attempts_for_task(&transaction, project_id, task_id, now_ms)?;
        transaction.execute(
            "DELETE FROM task_question_documents
             WHERE project_id = ?1
               AND question_id IN (
                   SELECT id FROM task_questions WHERE task_id = ?2 AND project_id = ?1
               )",
            params![project_id.as_str(), task_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM task_questions WHERE task_id = ?1 AND project_id = ?2",
            params![task_id.as_str(), project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM webhook_task_capabilities WHERE task_id = ?1 AND project_id = ?2",
            params![task_id.as_str(), project_id.as_str()],
        )?;
        // Agent messages delivered to a run of this task reference that run
        // via delivered_run_id. The run row is about to be deleted, but the
        // message itself is history: it was genuinely delivered, so it is
        // kept (not deleted) with delivered_run_id cleared rather than
        // cascading the delete onto it.
        transaction.execute(
            "UPDATE agent_messages SET delivered_run_id = NULL
             WHERE delivered_run_id IN (
                 SELECT id FROM runs WHERE task_id = ?1 AND project_id = ?2
             )",
            params![task_id.as_str(), project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM runs WHERE task_id = ?1 AND project_id = ?2",
            params![task_id.as_str(), project_id.as_str()],
        )?;
        let deleted = transaction.execute(
            "DELETE FROM tasks WHERE id = ?1 AND project_id = ?2",
            params![task_id.as_str(), project_id.as_str()],
        )?;
        if deleted != 1 {
            return Err(StoreError::TaskNotFound);
        }
        let event = FactoryEvent::TaskDeleted {
            project_id: project_id.clone(),
            task_id: task_id.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: now_ms,
            event,
        })
    }

    /// Read-only: would `delete_agent(project_id, agent_id, _)` succeed right
    /// now? Lets a caller refuse a delete request before removing any files
    /// (`local_api::delete_agent_locked`), so a refusal has no side effects.
    /// Not transactional by itself -- callers that need this check to still
    /// hold once they act on it rely on the execution manager's deletion
    /// gate (`execution::Handle::begin_delete`) to keep a *new* session from
    /// appearing in between, the same way `delete_agent`'s own transaction
    /// does the authoritative final check.
    pub fn check_agent_deletable(&self, project_id: &ProjectId, agent_id: &AgentId) -> Result<()> {
        check_agent_deletable(&self.connection, project_id, agent_id)
    }

    /// See [`Store::check_agent_deletable`]: same reasoning, one level up.
    pub fn check_project_deletable(&self, project_id: &ProjectId) -> Result<()> {
        check_project_deletable(&self.connection, project_id)
    }

    /// Deletes an agent that has no open run, no live session, no child
    /// agents, and no run that is itself the parent of another run. Its
    /// terminal runs and sessions are deleted too; tasks still assigned to
    /// it become unassigned (queue owner reverts to the operator).
    pub fn delete_agent(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        now_ms: i64,
    ) -> Result<Vec<EventEnvelope>> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_agent_deletable(&transaction, project_id, agent_id)?;

        transaction.execute_batch("PRAGMA defer_foreign_keys = ON;")?;
        let mut events = Vec::new();
        let unassigned_task_ids: Vec<TaskId> = {
            let mut statement = transaction
                .prepare("SELECT id FROM tasks WHERE assigned_agent_id = ?1 AND project_id = ?2")?;
            let rows = statement
                .query_map(params![agent_id.as_str(), project_id.as_str()], |row| {
                    parse_id::<TaskId>(row.get(0)?, 0)
                })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        if !unassigned_task_ids.is_empty() {
            transaction.execute(
                "UPDATE tasks SET assigned_agent_id = NULL, updated_at_ms = ?1,
                                  work_revision = work_revision + 1
                 WHERE assigned_agent_id = ?2 AND project_id = ?3",
                params![now_ms, agent_id.as_str(), project_id.as_str()],
            )?;
            for unassigned_task_id in &unassigned_task_ids {
                let task =
                    load_task(&transaction, unassigned_task_id)?.ok_or(StoreError::TaskNotFound)?;
                let event = FactoryEvent::TaskChanged {
                    task: task.snapshot,
                };
                let sequence = append_event(&transaction, now_ms, &event)?;
                events.push(EventEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    sequence,
                    occurred_at_ms: now_ms,
                    event,
                });
            }
        }

        // Messages addressed to this agent are its inbox; once the agent is
        // gone there is no one to read them, so they are deleted. Messages
        // it sent to others are history for the recipient and survive, with
        // the sender reference cleared.
        transaction.execute(
            "DELETE FROM agent_messages WHERE recipient_agent_id = ?1 AND project_id = ?2",
            params![agent_id.as_str(), project_id.as_str()],
        )?;
        transaction.execute(
            "UPDATE agent_messages SET sender_agent_id = NULL
             WHERE sender_agent_id = ?1 AND project_id = ?2",
            params![agent_id.as_str(), project_id.as_str()],
        )?;
        transaction.execute(
            "UPDATE agent_messages SET delivered_session_id = NULL
             WHERE delivered_session_id IN (
                 SELECT id FROM sessions WHERE agent_id = ?1 AND project_id = ?2
             )",
            params![agent_id.as_str(), project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM agent_profiles WHERE agent_id = ?1",
            params![agent_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM session_work WHERE agent_id = ?1 AND project_id = ?2",
            params![agent_id.as_str(), project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM delivery_attempts WHERE agent_id = ?1 AND project_id = ?2",
            params![agent_id.as_str(), project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM runs WHERE agent_id = ?1 AND project_id = ?2",
            params![agent_id.as_str(), project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM sessions WHERE agent_id = ?1 AND project_id = ?2",
            params![agent_id.as_str(), project_id.as_str()],
        )?;
        let deleted = transaction.execute(
            "DELETE FROM agents WHERE id = ?1 AND project_id = ?2",
            params![agent_id.as_str(), project_id.as_str()],
        )?;
        if deleted != 1 {
            return Err(StoreError::AgentNotFound);
        }
        let event = FactoryEvent::AgentDeleted {
            project_id: project_id.clone(),
            agent_id: agent_id.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        events.push(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: now_ms,
            event,
        });
        transaction.commit()?;
        Ok(events)
    }

    /// Deletes a project and cascades to every task, agent, run, and
    /// session scoped to it in one transaction. Refused while any
    /// non-terminal run or live session remains.
    pub fn delete_project(&mut self, project_id: &ProjectId, now_ms: i64) -> Result<EventEnvelope> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_project_deletable(&transaction, project_id)?;

        transaction.execute_batch("PRAGMA defer_foreign_keys = ON;")?;
        transaction.execute(
            "DELETE FROM task_question_documents WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM task_questions WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM webhook_task_capabilities WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM agent_messages WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM agent_profiles
             WHERE agent_id IN (SELECT id FROM agents WHERE project_id = ?1)",
            params![project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM session_work WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM delivery_attempts WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM runs WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM sessions WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM tasks WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM agents WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        // Immutable by design (see task_documents_immutable_delete); no code
        // path inserts rows here today, so this is normally a no-op. If rows
        // exist, the trigger aborts the delete and the whole transaction
        // rolls back, surfacing as a Conflict.
        transaction.execute(
            "DELETE FROM task_documents WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        let deleted = transaction.execute(
            "DELETE FROM projects WHERE id = ?1",
            params![project_id.as_str()],
        )?;
        if deleted != 1 {
            return Err(StoreError::ProjectNotFound);
        }
        let event = FactoryEvent::ProjectDeleted {
            project_id: project_id.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: now_ms,
            event,
        })
    }

    /// Persists stop intent on a non-terminal run and bumps its
    /// `updated_at_ms` so subscribers see the request land.
    pub fn request_run_stop(
        &mut self,
        project_id: &ProjectId,
        run_id: &RunId,
        now_ms: i64,
    ) -> Result<(RunSnapshot, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_run(&transaction, run_id)?
            .filter(|run| run.project_id == *project_id)
            .ok_or(StoreError::RunNotFound)?;
        if run.status.is_terminal() {
            return Err(StoreError::RunNotStoppable);
        }
        transaction.execute(
            "UPDATE runs
             SET stop_requested_at_ms = COALESCE(stop_requested_at_ms, ?1),
                 updated_at_ms = ?1
             WHERE id = ?2",
            params![now_ms, run_id.as_str()],
        )?;
        let run = load_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        let event = FactoryEvent::RunChanged { run: run.clone() };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            run,
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    pub fn list_agents(
        &self,
        project_id: &ProjectId,
        after_id: Option<&AgentId>,
        limit: usize,
    ) -> Result<Vec<AgentSnapshot>> {
        if !(1..=MAX_STATE_PAGE).contains(&limit) {
            return Err(StoreError::InvalidStateLimit);
        }
        let mut statement = self.connection.prepare(
            "SELECT a.id
             FROM agents a
             WHERE a.project_id = ?1 AND (?2 IS NULL OR a.id > ?2)
             ORDER BY a.id
             LIMIT ?3",
        )?;
        let ids = statement
            .query_map(
                params![
                    project_id.as_str(),
                    after_id.map(AgentId::as_str),
                    limit as i64
                ],
                |row| parse_id::<AgentId>(row.get(0)?, 0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        ids.into_iter()
            .map(|id| {
                load_agent(&self.connection, &id)?
                    .map(|record| record.snapshot)
                    .ok_or(StoreError::AgentNotFound)
            })
            .collect()
    }

    /// Every project's live picture in one read: agents with their live
    /// (or, failing that, most recent) session, current run, queued tasks,
    /// and undelivered inbox count; per-project backlog; the
    /// project's blocked tasks (for the attention list). One connection,
    /// so every field is from the same instant. See
    /// `factory_core::status`.
    pub fn fleet_status(&self) -> Result<Vec<ProjectStatusRows>> {
        let mut projects = self.connection.prepare(
            "SELECT id, name, root, created_at_ms, updated_at_ms
             FROM projects ORDER BY created_at_ms, id",
        )?;
        let projects = projects
            .query_map([], |row| {
                Ok(ProjectSnapshot {
                    id: parse_id(row.get(0)?, 0)?,
                    name: row.get(1)?,
                    root: row.get(2)?,
                    created_at_ms: row.get(3)?,
                    updated_at_ms: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(projects.len());
        for project in projects {
            let mut ids = self.connection.prepare(
                "SELECT id FROM agents WHERE project_id = ?1 ORDER BY created_at_ms, id",
            )?;
            let agent_ids = ids
                .query_map(params![project.id.as_str()], |row| {
                    parse_id::<AgentId>(row.get(0)?, 0)
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(ids);
            let agents = agent_ids
                .iter()
                .map(|agent_id| self.agent_status(&project.id, agent_id))
                .collect::<Result<Vec<_>>>()?;
            let backlog = self.active_tasks(&project.id, None)?;
            let blocked = self.blocked_tasks(&project.id)?;
            out.push(ProjectStatusRows {
                project,
                agents,
                backlog,
                blocked,
            });
        }
        Ok(out)
    }

    /// One agent's live picture (see [`Store::fleet_status`]).
    pub fn agent_status(&self, project_id: &ProjectId, agent_id: &AgentId) -> Result<AgentStatus> {
        let agent = load_agent(&self.connection, agent_id)?
            .filter(|agent| agent.snapshot.project_id == *project_id)
            .ok_or(StoreError::AgentNotFound)?
            .snapshot;
        let session = self
            .latest_session_for_agent(project_id, agent_id)?
            .map(|session| session.snapshot());
        let current_run = match &agent.current_run_id {
            Some(run_id) => load_run(&self.connection, run_id)?,
            None => None,
        };
        let queue = self.active_tasks(project_id, Some(agent_id))?;
        let inbox_pending: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM agent_messages
             WHERE project_id = ?1 AND recipient_agent_id = ?2 AND delivered_at_ms IS NULL",
            params![project_id.as_str(), agent_id.as_str()],
            |row| row.get(0),
        )?;
        let latest_run = match &current_run {
            Some(run) => Some(run.clone()),
            None => self.latest_run_for_agent(project_id, agent_id)?,
        };
        let rated = agent_attention(session.as_ref(), latest_run.as_ref());
        Ok(AgentStatus {
            agent,
            budget: self.agent_budget(project_id, agent_id)?,
            pause_reasons: agent_pause_reasons(&self.connection, project_id, agent_id)?,
            // Git is inspected asynchronously by the local API after the
            // consistent store snapshot has been released.
            worktree: None,
            session,
            current_run,
            latest_run,
            queue_depth: u32::try_from(queue.len()).unwrap_or(u32::MAX),
            queue: queue.into_iter().take(MAX_QUEUE_PREVIEW).collect(),
            inbox_pending: u32::try_from(inbox_pending).unwrap_or(u32::MAX),
            attention: rated.value,
            attention_inferred: rated.inferred,
        })
    }

    /// The agent's most recently started run, if any.
    fn latest_run_for_agent(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
    ) -> Result<Option<RunSnapshot>> {
        let id: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM runs
                 WHERE project_id = ?1 AND agent_id = ?2
                 ORDER BY started_at_ms DESC, id DESC
                 LIMIT 1",
                params![project_id.as_str(), agent_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            return Ok(None);
        };
        load_run(&self.connection, &parse_id(id, 0)?)
    }

    /// The agent's live session, else its most recently started one (so a
    /// failure stays visible after the session ended), else `None`.
    fn latest_session_for_agent(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
    ) -> Result<Option<SessionRow>> {
        let id: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM sessions
                 WHERE project_id = ?1 AND agent_id = ?2
                 ORDER BY (ended_at_ms IS NULL) DESC, started_at_ms DESC, id DESC
                 LIMIT 1",
                params![project_id.as_str(), agent_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            return Ok(None);
        };
        load_session(&self.connection, &parse_id(id, 0)?)
    }

    /// Active tasks assigned to `agent_id` (or unassigned when `None`): the
    /// canonical queue projection used by status and the TUI. Running work
    /// is first, followed by queued work in priority/creation order.
    fn active_tasks(
        &self,
        project_id: &ProjectId,
        agent_id: Option<&AgentId>,
    ) -> Result<Vec<TaskSnapshot>> {
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, parent_task_id, assigned_agent_id, title, status, priority,
                    created_at_ms, updated_at_ms
             FROM tasks
             WHERE project_id = ?1 AND status IN ('queued', 'running')
               AND ((?2 IS NULL AND assigned_agent_id IS NULL) OR assigned_agent_id = ?2)
             ORDER BY (status = 'running') DESC, priority DESC, created_at_ms, id",
        )?;
        let rows = statement.query_map(
            params![project_id.as_str(), agent_id.map(AgentId::as_str)],
            task_snapshot_from_row,
        )?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Tasks an agent marked blocked, oldest update first.
    pub fn blocked_tasks(
        &self,
        project_id: &ProjectId,
    ) -> Result<Vec<factory_core::status::BlockedTaskStatus>> {
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, parent_task_id, assigned_agent_id, title, status, priority,
                    created_at_ms, updated_at_ms, blocked_reason
             FROM tasks
             WHERE project_id = ?1 AND status = 'blocked'
             ORDER BY updated_at_ms, id",
        )?;
        let rows = statement.query_map(params![project_id.as_str()], |row| {
            Ok(factory_core::status::BlockedTaskStatus {
                task: task_snapshot_from_row(row)?,
                reason: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn get_agent_detail(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
    ) -> Result<AgentDetail> {
        let agent = load_agent(&self.connection, agent_id)?
            .filter(|agent| agent.snapshot.project_id == *project_id)
            .ok_or(StoreError::AgentNotFound)?;
        Ok(AgentDetail {
            snapshot: agent.snapshot,
            profile: load_agent_profile(&self.connection, agent_id)?.unwrap_or(AgentProfile {
                model: None,
                reasoning_effort: None,
                model_selection_reason: None,
                permission_mode: None,
                updated_at_ms: 0,
            }),
        })
    }

    pub fn update_agent_profile(
        &mut self,
        project_id: &ProjectId,
        agent_id: &AgentId,
        input: UpdateAgentProfile,
        now_ms: i64,
    ) -> Result<(AgentDetail, EventEnvelope)> {
        validate_agent_profile(&input)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let agent = load_agent(&transaction, agent_id)?
            .filter(|agent| agent.snapshot.project_id == *project_id)
            .ok_or(StoreError::AgentNotFound)?;
        if let Some(mode) = input.permission_mode.as_deref() {
            let capabilities = crate::providers::capabilities_for(agent.snapshot.provider);
            if !capabilities.permission_modes.contains(&mode) {
                return Err(StoreError::UnsupportedAgentPermissionMode {
                    provider: agent.snapshot.provider,
                    mode: mode.to_owned(),
                });
            }
        }
        let selection = model_policy::normalize_profile(
            agent.snapshot.provider,
            agent.snapshot.role,
            model_policy::ModelSelection {
                model: input.model,
                reasoning_effort: input.reasoning_effort,
                reason: input.model_selection_reason,
            },
            false,
        )?;
        transaction.execute(
            "INSERT INTO agent_profiles (
                agent_id, model, reasoning_effort, model_selection_reason,
                permission_mode, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(agent_id) DO UPDATE SET
                model = excluded.model,
                reasoning_effort = excluded.reasoning_effort,
                model_selection_reason = excluded.model_selection_reason,
                permission_mode = excluded.permission_mode,
                updated_at_ms = excluded.updated_at_ms",
            params![
                agent_id.as_str(),
                selection.model,
                selection.reasoning_effort,
                selection.reason,
                input.permission_mode,
                now_ms
            ],
        )?;
        transaction.execute(
            "UPDATE agents SET updated_at_ms = ?1 WHERE id = ?2",
            params![now_ms, agent_id.as_str()],
        )?;
        let snapshot = load_agent(&transaction, agent_id)?
            .ok_or(StoreError::AgentNotFound)?
            .snapshot;
        let event = FactoryEvent::AgentChanged {
            agent: snapshot.clone(),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        let profile =
            load_agent_profile(&transaction, agent_id)?.ok_or(StoreError::AgentNotFound)?;
        transaction.commit()?;
        Ok((
            AgentDetail { snapshot, profile },
            EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    pub fn list_runs(
        &self,
        project_id: &ProjectId,
        after_id: Option<&RunId>,
        limit: usize,
    ) -> Result<Vec<RunSnapshot>> {
        if !(1..=MAX_STATE_PAGE).contains(&limit) {
            return Err(StoreError::InvalidStateLimit);
        }
        let mut statement = self.connection.prepare(
            "SELECT id
             FROM runs
             WHERE project_id = ?1 AND (?2 IS NULL OR id > ?2)
             ORDER BY id
             LIMIT ?3",
        )?;
        let ids = statement
            .query_map(
                params![
                    project_id.as_str(),
                    after_id.map(RunId::as_str),
                    limit as i64
                ],
                |row| parse_id::<RunId>(row.get(0)?, 0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        ids.into_iter()
            .map(|id| load_run(&self.connection, &id)?.ok_or(StoreError::RunNotFound))
            .collect()
    }

    pub fn events_after(&self, sequence: i64, limit: usize) -> Result<Vec<EventEnvelope>> {
        if !(1..=MAX_EVENT_PAGE).contains(&limit) {
            return Err(StoreError::InvalidEventLimit);
        }
        if sequence < 0 {
            return Err(StoreError::InvalidEventCursor);
        }

        let mut statement = self.connection.prepare(
            "SELECT id, occurred_at_ms, schema_version, payload_json
             FROM events
             WHERE id > ?1
             ORDER BY id
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![sequence, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let stored = rows.collect::<std::result::Result<Vec<_>, _>>()?;

        let mut expected = sequence;
        for (found, _, _, _) in &stored {
            expected = expected
                .checked_add(1)
                .ok_or(StoreError::InvalidEventCursor)?;
            if *found != expected {
                return Err(StoreError::EventSequenceGap {
                    expected,
                    found: *found,
                });
            }
        }

        stored
            .into_iter()
            .map(|(sequence, occurred_at_ms, version, payload)| {
                let protocol_version = u16::try_from(version)
                    .map_err(|_| StoreError::CorruptProtocolVersion(version))?;
                Ok(EventEnvelope {
                    protocol_version,
                    sequence,
                    occurred_at_ms,
                    event: serde_json::from_str(&payload)?,
                })
            })
            .collect()
    }

    pub fn latest_event_sequence(&self) -> Result<i64> {
        Ok(self
            .connection
            .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| {
                row.get(0)
            })?)
    }
}

fn valid_endpoint_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn valid_webhook_document_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn usize_to_i64(value: usize) -> Result<i64> {
    i64::try_from(value).map_err(|_| StoreError::InvalidStateLimit)
}

fn operational_task_status(status: &str, column: usize) -> rusqlite::Result<OperationalTaskStatus> {
    match status {
        "queued" => Ok(OperationalTaskStatus::Todo),
        "running" => Ok(OperationalTaskStatus::Doing),
        "blocked" => Ok(OperationalTaskStatus::Blocked),
        "succeeded" | "failed" | "cancelled" => Ok(OperationalTaskStatus::Done),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            column,
            Type::Text,
            "invalid operational task status".into(),
        )),
    }
}

fn validate_webhook_project_and_orchestrator(
    connection: &Connection,
    project_id: &ProjectId,
    orchestrator_agent_id: &AgentId,
) -> Result<()> {
    let project_exists: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM projects WHERE id = ?1)",
        params![project_id.as_str()],
        |row| row.get(0),
    )?;
    if !project_exists {
        return Err(StoreError::WebhookProjectNotFound);
    }
    let orchestrator_exists: bool = connection.query_row(
        "SELECT EXISTS (
            SELECT 1 FROM agents
            WHERE id = ?1 AND project_id = ?2 AND role = 'orchestrator'
         )",
        params![orchestrator_agent_id.as_str(), project_id.as_str()],
        |row| row.get(0),
    )?;
    if !orchestrator_exists {
        return Err(StoreError::WebhookOrchestratorNotFound);
    }
    Ok(())
}

fn validate_webhook_create_input(input: &NewWebhookTask) -> Result<()> {
    if input.created_at_ms < 0
        || !valid_endpoint_id(&input.endpoint_id)
        || input.title.is_empty()
        || input.title.len() > MAX_WEBHOOK_CREATE_TITLE_BYTES
        || input.body.is_empty()
        || input.body.len() > MAX_BODY_BYTES
    {
        Err(StoreError::InvalidWebhookInput)
    } else {
        Ok(())
    }
}

fn validate_webhook_answer_input(input: &WebhookAnswer) -> Result<()> {
    if input.answered_at_ms < 0
        || input.answer.trim().is_empty()
        || input.answer.len() > MAX_BODY_BYTES
    {
        Err(StoreError::InvalidWebhookInput)
    } else {
        Ok(())
    }
}

fn load_webhook_snapshot_tasks(
    connection: &Connection,
    project_id: &ProjectId,
    terminal: bool,
    limit: usize,
) -> Result<Vec<WebhookSnapshotTask>> {
    let predicate = if terminal {
        "status IN ('succeeded', 'failed', 'cancelled')"
    } else {
        "status NOT IN ('succeeded', 'failed', 'cancelled')"
    };
    let ordering = if terminal {
        "updated_at_ms DESC, id"
    } else {
        "priority DESC, created_at_ms, id"
    };
    let sql = format!(
        "SELECT id, title, status,
                assigned_agent_id, priority, created_at_ms, started_at_ms,
                completed_at_ms, result
         FROM tasks
         WHERE project_id = ?1 AND {predicate}
         ORDER BY {ordering}
         LIMIT ?2"
    );
    let stored = {
        let mut statement = connection.prepare(&sql)?;
        let rows =
            statement.query_map(params![project_id.as_str(), usize_to_i64(limit)?], |row| {
                Ok((
                    parse_id::<TaskId>(row.get(0)?, 0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i32>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    stored
        .into_iter()
        .map(
            |(
                id,
                title,
                status,
                assignee,
                priority,
                created_at_ms,
                started_at_ms,
                completed_at_ms,
                result,
            )| {
                Ok(WebhookSnapshotTask {
                    question: load_webhook_open_question(connection, project_id, &id)?,
                    id,
                    title: truncate_utf8(&title, MAX_WEBHOOK_TITLE_BYTES),
                    status: operational_task_status(&status, 2)?,
                    assignee: assignee.map(|value| truncate_utf8(&value, 256)),
                    priority,
                    created_at_ms,
                    started_at_ms,
                    completed_at_ms,
                    result: result.map(|value| truncate_utf8(&value, MAX_WEBHOOK_TEXT_BYTES)),
                })
            },
        )
        .collect()
}

fn load_webhook_open_question(
    connection: &Connection,
    project_id: &ProjectId,
    task_id: &TaskId,
) -> Result<Option<WebhookOpenQuestion>> {
    let question: Option<(i64, String, Option<i64>)> = connection
        .query_row(
            "SELECT id, text, asked_at_ms
             FROM task_questions
             WHERE task_id = ?1 AND project_id = ?2 AND answer IS NULL
             ORDER BY asked_at_ms DESC, ordinal DESC, id DESC
             LIMIT 1",
            params![task_id.as_str(), project_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((question_id, text, asked_at_ms)) = question else {
        return Ok(None);
    };
    let documents = {
        let mut statement = connection.prepare(
            "SELECT d.id, d.name, d.reference
             FROM task_question_documents qd
             JOIN task_documents d
               ON d.project_id = qd.project_id AND d.id = qd.document_id
             WHERE qd.question_id = ?1 AND qd.project_id = ?2
             ORDER BY qd.ordinal
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                question_id,
                project_id.as_str(),
                usize_to_i64(MAX_WEBHOOK_DOCUMENT_REFS + 1)?,
            ],
            |row| {
                Ok(WebhookDocumentRef {
                    id: row.get(0)?,
                    name: truncate_utf8(&row.get::<_, String>(1)?, 512),
                    reference: truncate_utf8(&row.get::<_, String>(2)?, MAX_PATH_BYTES),
                })
            },
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    if documents.len() > MAX_WEBHOOK_DOCUMENT_REFS {
        return Err(StoreError::WebhookSnapshotTooLarge);
    }
    Ok(Some(WebhookOpenQuestion {
        text: truncate_utf8(&text, MAX_WEBHOOK_TEXT_BYTES),
        asked_at_ms,
        documents,
    }))
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// Finds the exact open run backing a running task, scoped to the project.
fn open_run_for_task(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    task_id: &TaskId,
) -> Result<RunSnapshot> {
    let task = load_task(transaction, task_id)?
        .filter(|task| task.snapshot.project_id == *project_id)
        .ok_or(StoreError::TaskNotFound)?;
    if task.snapshot.status != TaskStatus::Running {
        return Err(StoreError::TaskNotRunning);
    }
    let run_id: String = transaction
        .query_row(
            "SELECT id FROM runs WHERE task_id = ?1 AND project_id = ?2 AND ended_at_ms IS NULL",
            params![task_id.as_str(), project_id.as_str()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(StoreError::RunNotFound)?;
    let run_id: RunId = parse_id(run_id, 0)?;
    load_run(transaction, &run_id)?.ok_or(StoreError::RunNotFound)
}

/// Closes one open run (task-episode) and moves its task to a terminal or
/// blocked state in the same transaction, emitting `TaskChanged`,
/// `AgentChanged`, and `RunChanged` events.
#[allow(clippy::too_many_arguments)]
fn close_run_in_transaction(
    transaction: &Transaction<'_>,
    run: &RunSnapshot,
    run_status: RunStatus,
    closed_by: RunClosedBy,
    failure_reason: Option<RunFailureReason>,
    task_status: TaskStatus,
    task_result: Option<&str>,
    task_blocked_reason: Option<&str>,
    now_ms: i64,
) -> Result<ClosedEpisode> {
    let session_id = run.session_id.as_ref().ok_or(StoreError::SessionNotFound)?;
    let owner =
        load_session_work(transaction, session_id)?.ok_or(StoreError::CorruptSessionWork)?;
    let released = if let Some(reason) = owner.quarantine_reason.as_deref() {
        if !matches!(
            closed_by,
            RunClosedBy::OperatorStop | RunClosedBy::SessionEnded
        ) {
            return Err(StoreError::SessionWorkQuarantined(reason.to_owned()));
        }
        None
    } else {
        let current = owner.work.as_ref().ok_or(StoreError::CorruptSessionWork)?;
        Some(current.complete(&run.id).map_err(transition_error)?)
    };
    let changed = transaction.execute(
        "UPDATE runs
         SET status = ?1, status_since_ms = ?2, updated_at_ms = ?2, ended_at_ms = ?2,
             closed_by = ?3, failure_reason = ?4, activity = NULL, wait_reason = NULL
         WHERE id = ?5 AND ended_at_ms IS NULL",
        params![
            run_status_value(run_status),
            now_ms,
            run_closed_by_value(closed_by),
            failure_reason.map(failure_reason_value),
            run.id.as_str(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::RunNotStoppable);
    }
    let task_id = run.task_id.as_ref().ok_or(StoreError::TaskNotFound)?;
    let is_terminal_task_status = task_status.is_terminal();
    let changed = transaction.execute(
        "UPDATE tasks
         SET status = ?1, updated_at_ms = ?2,
             completed_at_ms = CASE WHEN ?3 THEN ?2 ELSE completed_at_ms END,
             result = COALESCE(?4, result), blocked_reason = ?5,
             work_revision = work_revision + 1
         WHERE id = ?6 AND project_id = ?7 AND status = 'running'",
        params![
            task_status_value(task_status),
            now_ms,
            is_terminal_task_status,
            task_result,
            task_blocked_reason,
            task_id.as_str(),
            run.project_id.as_str(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::TaskNotFound);
    }
    transaction.execute(
        "UPDATE agents SET updated_at_ms = ?1 WHERE id = ?2 AND project_id = ?3",
        params![now_ms, run.agent_id.as_str(), run.project_id.as_str()],
    )?;
    if let Some(released) = released {
        persist_session_work(
            transaction,
            session_id,
            owner
                .work
                .as_ref()
                .ok_or(StoreError::CorruptSessionWork)?
                .revision,
            &released,
            now_ms,
        )?;
    }
    let task = load_task(transaction, task_id)?.ok_or(StoreError::TaskNotFound)?;
    let agent = load_agent(transaction, &run.agent_id)?
        .ok_or(StoreError::AgentNotFound)?
        .snapshot;
    let run = load_run(transaction, &run.id)?.ok_or(StoreError::RunNotFound)?;
    let events = append_execution_events(transaction, now_ms, &task.snapshot, &agent, &run)?;
    Ok(ClosedEpisode { run, task, events })
}

fn validate_agent_profile(input: &UpdateAgentProfile) -> Result<()> {
    validate_agent_model(input.model.as_deref())?;
    validate_agent_permission_mode(input.permission_mode.as_deref())
}

fn validate_runtime_metadata(input: &NewSession) -> Result<()> {
    for value in [
        &input.runtime_model,
        &input.runtime_reasoning_effort,
        &input.runtime_permission_mode,
        &input.runtime_control_mode,
    ]
    .into_iter()
    .flatten()
    {
        if value.is_empty()
            || value.len() > MAX_RUNTIME_METADATA_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(StoreError::InvalidExecutionMetadata);
        }
    }
    Ok(())
}

fn validate_agent_message(body: &str, created_at_ms: i64) -> Result<()> {
    if created_at_ms < 0
        || body.is_empty()
        || body.len() > MAX_AGENT_MESSAGE_BYTES
        || body.contains('\0')
        || body
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(StoreError::InvalidAgentMessage);
    }
    Ok(())
}

fn validate_agent_model(model: Option<&str>) -> Result<()> {
    if model.is_some_and(|value| {
        value.is_empty()
            || value.len() > MAX_AGENT_MODEL_BYTES
            || value.chars().any(char::is_control)
    }) {
        return Err(StoreError::InvalidAgentProfile);
    }
    Ok(())
}

/// Provider-scoped, free-form permission mode (e.g. Claude's `acceptEdits`
/// or `plan`; Codex's `on-request` or `never`); `None` means the provider
/// default. Validated the same way as `model`; provider launch consumes it.
fn validate_agent_permission_mode(permission_mode: Option<&str>) -> Result<()> {
    if permission_mode.is_some_and(|value| {
        value.is_empty()
            || value.len() > MAX_AGENT_PERMISSION_MODE_BYTES
            || value.chars().any(char::is_control)
    }) {
        return Err(StoreError::InvalidAgentProfile);
    }
    Ok(())
}

struct AgentRecord {
    snapshot: AgentSnapshot,
}

fn load_agent(connection: &Connection, agent_id: &AgentId) -> Result<Option<AgentRecord>> {
    connection
        .query_row(
            "SELECT a.id, a.project_id, a.parent_agent_id, a.role, a.provider,
                    (a.paused OR COALESCE((SELECT b.exhausted FROM agent_budgets b WHERE b.agent_id = a.id), 0)),
                    a.worktree, a.created_at_ms, a.updated_at_ms,
                    (SELECT r.id FROM runs r
                     WHERE r.agent_id = a.id
                       AND r.ended_at_ms IS NULL
                     LIMIT 1),
                    (SELECT s.id FROM sessions s
                     WHERE s.agent_id = a.id
                       AND s.ended_at_ms IS NULL
                     LIMIT 1)
             FROM agents a
             WHERE a.id = ?1",
            params![agent_id.as_str()],
            |row| {
                let parent_agent_id: Option<String> = row.get(2)?;
                let role: String = row.get(3)?;
                let provider: String = row.get(4)?;
                let current_run_id: Option<String> = row.get(9)?;
                let current_session_id: Option<String> = row.get(10)?;
                Ok(AgentRecord {
                    snapshot: AgentSnapshot {
                        id: parse_id(row.get(0)?, 0)?,
                        project_id: parse_id(row.get(1)?, 1)?,
                        parent_agent_id: parse_optional_id(parent_agent_id, 2)?,
                        role: parse_agent_role(&role, 3)?,
                        provider: parse_provider(&provider, 4)?,
                        current_run_id: parse_optional_id(current_run_id, 9)?,
                        paused: row.get(5)?,
                        current_session_id: parse_optional_id(current_session_id, 10)?,
                        worktree: row.get(6)?,
                        created_at_ms: row.get(7)?,
                        updated_at_ms: row.get(8)?,
                    },
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

/// The same precondition checks `delete_agent` performs, factored out so a
/// caller can verify a delete would succeed *before* touching any files
/// (`local_api::delete_agent_locked`'s file-then-row ordering needs this to
/// keep a refusal side-effect-free -- PR #50 re-review, new blocking
/// finding). Read-only: takes `&Connection` so it works unchanged whether
/// called directly (a plain read, `Store::check_agent_deletable`) or from
/// inside `delete_agent`'s own transaction (`Transaction` derefs to
/// `Connection`) -- the checks live in exactly one place either way.
fn check_agent_deletable(
    connection: &Connection,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<()> {
    let agent = load_agent(connection, agent_id)?
        .filter(|agent| agent.snapshot.project_id == *project_id)
        .ok_or(StoreError::AgentNotFound)?;
    if agent.snapshot.current_run_id.is_some() {
        return Err(StoreError::AgentHasActiveRun);
    }
    if agent.snapshot.current_session_id.is_some() {
        return Err(StoreError::AgentHasLiveSession);
    }
    let has_children: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM agents WHERE parent_agent_id = ?1 AND project_id = ?2
         )",
        params![agent_id.as_str(), project_id.as_str()],
        |row| row.get(0),
    )?;
    if has_children {
        return Err(StoreError::AgentHasChildren);
    }
    let has_dependent_runs: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM runs child
            JOIN runs parent ON parent.id = child.parent_run_id
            WHERE parent.agent_id = ?1
         )",
        params![agent_id.as_str()],
        |row| row.get(0),
    )?;
    if has_dependent_runs {
        return Err(StoreError::AgentRunHasDependents);
    }
    Ok(())
}

/// See [`check_agent_deletable`]: same reasoning, one level up, factored
/// out of `delete_project`.
fn check_project_deletable(connection: &Connection, project_id: &ProjectId) -> Result<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1)",
        params![project_id.as_str()],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(StoreError::ProjectNotFound);
    }
    let has_active_run: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM runs
            WHERE project_id = ?1 AND status NOT IN ('succeeded', 'failed', 'stopped')
            UNION ALL
            SELECT 1 FROM sessions WHERE project_id = ?1 AND ended_at_ms IS NULL
         )",
        params![project_id.as_str()],
        |row| row.get(0),
    )?;
    if has_active_run {
        return Err(StoreError::ProjectHasActiveRun);
    }
    Ok(())
}

fn load_agent_profile(connection: &Connection, agent_id: &AgentId) -> Result<Option<AgentProfile>> {
    connection
        .query_row(
            "SELECT model, reasoning_effort, model_selection_reason,
                    permission_mode, updated_at_ms
             FROM agent_profiles WHERE agent_id = ?1",
            params![agent_id.as_str()],
            |row| {
                Ok(AgentProfile {
                    model: row.get(0)?,
                    reasoning_effort: row.get(1)?,
                    model_selection_reason: row.get(2)?,
                    permission_mode: row.get(3)?,
                    updated_at_ms: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn insert_task(
    transaction: &Transaction<'_>,
    input: NewTask,
    assigned_agent_id: Option<AgentId>,
    now_ms: i64,
) -> Result<(TaskDetail, FactoryEvent)> {
    let title = normalize_task_title(input.title).ok_or(StoreError::InvalidTaskInput)?;
    if input.body.len() > MAX_TASK_BODY_BYTES {
        return Err(StoreError::InvalidTaskInput);
    }
    if let Some(agent_id) = assigned_agent_id.as_ref() {
        let valid = load_agent(transaction, agent_id)?
            .is_some_and(|agent| agent.snapshot.project_id == input.project_id);
        if !valid {
            return Err(StoreError::AgentNotFound);
        }
    }
    let record = TaskDetail {
        snapshot: TaskSnapshot {
            id: input.id,
            project_id: input.project_id,
            parent_task_id: input.parent_task_id,
            assigned_agent_id,
            title,
            status: TaskStatus::Queued,
            priority: input.priority,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        },
        body: input.body,
        result: None,
        blocked_reason: None,
    };
    transaction.execute(
        "INSERT INTO tasks (
            id, project_id, parent_task_id, assigned_agent_id, title, body,
            status, priority, created_at_ms, updated_at_ms, incarnation_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', ?7, ?8, ?9, ?10)",
        params![
            record.snapshot.id.as_str(),
            record.snapshot.project_id.as_str(),
            record.snapshot.parent_task_id.as_ref().map(TaskId::as_str),
            record
                .snapshot
                .assigned_agent_id
                .as_ref()
                .map(AgentId::as_str),
            &record.snapshot.title,
            &record.body,
            record.snapshot.priority,
            record.snapshot.created_at_ms,
            record.snapshot.updated_at_ms,
            Uuid::new_v4().hyphenated().to_string(),
        ],
    )?;
    let event = FactoryEvent::TaskChanged {
        task: record.snapshot.clone(),
    };
    Ok((record, event))
}

fn load_task(connection: &Connection, task_id: &TaskId) -> Result<Option<TaskDetail>> {
    connection
        .query_row(
            "SELECT id, project_id, parent_task_id, assigned_agent_id, title, body, result,
                    status, priority, created_at_ms, updated_at_ms, blocked_reason
             FROM tasks WHERE id = ?1",
            params![task_id.as_str()],
            |row| {
                let parent_id: Option<String> = row.get(2)?;
                let assigned_id: Option<String> = row.get(3)?;
                let status: String = row.get(7)?;
                Ok(TaskDetail {
                    snapshot: TaskSnapshot {
                        id: parse_id(row.get(0)?, 0)?,
                        project_id: parse_id(row.get(1)?, 1)?,
                        parent_task_id: parse_optional_id(parent_id, 2)?,
                        assigned_agent_id: parse_optional_id(assigned_id, 3)?,
                        title: row.get(4)?,
                        status: parse_task_status(&status, 7)?,
                        priority: row.get(8)?,
                        created_at_ms: row.get(9)?,
                        updated_at_ms: row.get(10)?,
                    },
                    body: row.get(5)?,
                    result: row.get(6)?,
                    blocked_reason: row.get(11)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn load_run(connection: &Connection, run_id: &RunId) -> Result<Option<RunSnapshot>> {
    connection
        .query_row(
            "SELECT id, project_id, agent_id, session_id, parent_run_id, task_id, status,
                    activity, wait_reason, worktree, started_at_ms, status_since_ms,
                    updated_at_ms, ended_at_ms, closed_by, failure_reason
             FROM runs WHERE id = ?1",
            params![run_id.as_str()],
            |row| {
                let session_id: Option<String> = row.get(3)?;
                let parent_run_id: Option<String> = row.get(4)?;
                let task_id: Option<String> = row.get(5)?;
                let status: String = row.get(6)?;
                let closed_by: Option<String> = row.get(14)?;
                let failure_reason: Option<String> = row.get(15)?;
                Ok(RunSnapshot {
                    id: parse_id(row.get(0)?, 0)?,
                    project_id: parse_id(row.get(1)?, 1)?,
                    agent_id: parse_id(row.get(2)?, 2)?,
                    parent_run_id: parse_optional_id(parent_run_id, 4)?,
                    task_id: parse_optional_id(task_id, 5)?,
                    session_id: parse_optional_id(session_id, 3)?,
                    closed_by: closed_by
                        .map(|value| parse_run_closed_by(&value, 14))
                        .transpose()?,
                    status: parse_run_status(&status, 6)?,
                    activity: row.get(7)?,
                    wait_reason: row.get(8)?,
                    worktree: row.get(9)?,
                    observer_health: ObserverHealth::default(),
                    observer_health_since_ms: 0,
                    started_at_ms: row.get(10)?,
                    status_since_ms: row.get(11)?,
                    updated_at_ms: row.get(12)?,
                    ended_at_ms: row.get(13)?,
                    exit_code: None,
                    exit_signal: None,
                    failure_reason: parse_optional_failure_reason(failure_reason, 15)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn load_session_work(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Option<SessionWorkSnapshot>> {
    connection
        .query_row(
            "SELECT session_id, project_id, agent_id, state, revision, attempt_id,
                    task_id, task_incarnation_id, task_revision, run_id,
                    quarantine_reason
             FROM session_work WHERE session_id = ?1",
            params![session_id.as_str()],
            |row| {
                let state: String = row.get(3)?;
                let revision: i64 = row.get(4)?;
                let attempt_id: Option<String> = row.get(5)?;
                let task_id: Option<String> = row.get(6)?;
                let task_incarnation_id: Option<String> = row.get(7)?;
                let task_revision: Option<i64> = row.get(8)?;
                let run_id: Option<String> = row.get(9)?;
                let quarantine_reason: Option<String> = row.get(10)?;
                let work = if quarantine_reason.is_some() {
                    None
                } else {
                    let phase = match state.as_str() {
                        "empty" => SessionWorkPhase::Empty,
                        "delivering" | "running" | "uncertain" => {
                            let attempt_id = attempt_id.clone().ok_or_else(|| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    5,
                                    Type::Null,
                                    "occupied session work has no attempt".into(),
                                )
                            })?;
                            let task = match (
                                task_id.clone(),
                                task_incarnation_id.clone(),
                                task_revision,
                                run_id.clone(),
                            ) {
                                (None, None, None, None) => None,
                                (
                                    Some(task_id),
                                    Some(incarnation_id),
                                    Some(revision),
                                    Some(run_id),
                                ) => Some(TaskWork {
                                    task_id: parse_id(task_id, 6)?,
                                    incarnation_id,
                                    revision,
                                    run_id: parse_id(run_id, 9)?,
                                }),
                                _ => {
                                    return Err(rusqlite::Error::FromSqlConversionFailure(
                                        6,
                                        Type::Text,
                                        "partial session work task identity".into(),
                                    ));
                                }
                            };
                            let lease = DeliveryLease { attempt_id, task };
                            match state.as_str() {
                                "delivering" => SessionWorkPhase::Delivering(lease),
                                "running" if lease.task.is_some() => {
                                    SessionWorkPhase::Running(lease)
                                }
                                "uncertain" => SessionWorkPhase::Uncertain(lease),
                                "running" => {
                                    return Err(rusqlite::Error::FromSqlConversionFailure(
                                        3,
                                        Type::Text,
                                        "running session work has no task".into(),
                                    ));
                                }
                                _ => unreachable!(),
                            }
                        }
                        _ => {
                            return Err(rusqlite::Error::FromSqlConversionFailure(
                                3,
                                Type::Text,
                                format!("invalid session work state {state:?}").into(),
                            ));
                        }
                    };
                    Some(SessionWork { revision, phase })
                };
                Ok(SessionWorkSnapshot {
                    session_id: parse_id(row.get(0)?, 0)?,
                    project_id: parse_id(row.get(1)?, 1)?,
                    agent_id: parse_id(row.get(2)?, 2)?,
                    work,
                    quarantine_reason,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn require_session_work(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<SessionWorkSnapshot> {
    let snapshot =
        load_session_work(connection, session_id)?.ok_or(StoreError::CorruptSessionWork)?;
    if let Some(reason) = snapshot.quarantine_reason.clone() {
        return Err(StoreError::SessionWorkQuarantined(reason));
    }
    if snapshot.work.is_none() {
        return Err(StoreError::CorruptSessionWork);
    }
    validate_session_work_identity(
        connection,
        &snapshot.session_id,
        &snapshot.project_id,
        &snapshot.agent_id,
        snapshot
            .work
            .as_ref()
            .ok_or(StoreError::CorruptSessionWork)?,
    )?;
    Ok(snapshot)
}

fn persist_session_work(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    expected_revision: i64,
    next: &SessionWork,
    now_ms: i64,
) -> Result<()> {
    let session = load_session(transaction, session_id)?.ok_or(StoreError::SessionNotFound)?;
    validate_session_work_identity(
        transaction,
        session_id,
        &session.project_id,
        &session.agent_id,
        next,
    )?;
    let (state, lease) = match &next.phase {
        SessionWorkPhase::Empty => ("empty", None),
        SessionWorkPhase::Delivering(lease) => ("delivering", Some(lease)),
        SessionWorkPhase::Running(lease) => ("running", Some(lease)),
        SessionWorkPhase::Uncertain(lease) => ("uncertain", Some(lease)),
    };
    let task = lease.and_then(|lease| lease.task.as_ref());
    let changed = transaction.execute(
        "UPDATE session_work
         SET state = ?1, revision = ?2, attempt_id = ?3, task_id = ?4,
             task_incarnation_id = ?5, task_revision = ?6,
             run_id = ?7, quarantine_reason = NULL, updated_at_ms = ?8
         WHERE session_id = ?9 AND revision = ?10 AND quarantine_reason IS NULL",
        params![
            state,
            next.revision,
            lease.map(|lease| lease.attempt_id.as_str()),
            task.map(|task| task.task_id.as_str()),
            task.map(|task| task.incarnation_id.as_str()),
            task.map(|task| task.revision),
            task.map(|task| task.run_id.as_str()),
            now_ms,
            session_id.as_str(),
            expected_revision,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::SessionWorkConflict);
    }
    Ok(())
}

/// Enforces the relationships that cannot all be represented as foreign
/// keys: Delivering/Uncertain reserve a run id before the run row exists,
/// while Running requires that exact row. The attempt, task incarnation,
/// assignment, phase-specific task status, and run owner must all agree with
/// the resident session before authority can be read or persisted.
fn validate_session_work_identity(
    connection: &Connection,
    session_id: &SessionId,
    project_id: &ProjectId,
    agent_id: &AgentId,
    work: &SessionWork,
) -> Result<()> {
    let Some(lease) = work.phase.lease() else {
        return Ok(());
    };
    let attempt = connection
        .query_row(
            "SELECT id, project_id, agent_id, session_id, task_id,
                    task_incarnation_id, message_ids_json,
                    text, failure_count, next_attempt_at_ms, state,
                    task_revision, run_id
             FROM delivery_attempts WHERE id = ?1",
            params![lease.attempt_id.as_str()],
            delivery_attempt_from_row,
        )
        .optional()?
        .ok_or(StoreError::CorruptSessionWork)?;
    if attempt.project_id != *project_id
        || attempt.agent_id != *agent_id
        || attempt.session_id != *session_id
    {
        return Err(StoreError::CorruptSessionWork);
    }
    let expected_attempt_state = match &work.phase {
        SessionWorkPhase::Delivering(_) => matches!(
            attempt.state,
            DeliveryAttemptState::InFlight | DeliveryAttemptState::Retryable
        ),
        SessionWorkPhase::Uncertain(_) => matches!(
            attempt.state,
            DeliveryAttemptState::InFlight
                | DeliveryAttemptState::Retryable
                | DeliveryAttemptState::Terminal
        ),
        SessionWorkPhase::Running(_) => attempt.state == DeliveryAttemptState::Acknowledged,
        SessionWorkPhase::Empty => unreachable!(),
    };
    if !expected_attempt_state {
        return Err(StoreError::CorruptSessionWork);
    }

    let Some(task) = lease.task.as_ref() else {
        if attempt.task_id.is_some()
            || attempt.task_incarnation_id.is_some()
            || attempt.task_revision.is_some()
            || attempt.run_id.is_some()
            || matches!(work.phase, SessionWorkPhase::Running(_))
        {
            return Err(StoreError::CorruptSessionWork);
        }
        return Ok(());
    };
    if attempt.task_id.as_ref() != Some(&task.task_id)
        || attempt.task_incarnation_id.as_deref() != Some(task.incarnation_id.as_str())
        || attempt.task_revision != Some(task.revision)
        || attempt.run_id.as_ref() != Some(&task.run_id)
    {
        return Err(StoreError::CorruptSessionWork);
    }
    let detail = load_task(connection, &task.task_id)?
        .filter(|detail| detail.snapshot.project_id == *project_id)
        .filter(|detail| detail.snapshot.assigned_agent_id.as_ref() == Some(agent_id))
        .ok_or(StoreError::CorruptSessionWork)?;
    let incarnation: String = connection.query_row(
        "SELECT incarnation_id FROM tasks WHERE id = ?1",
        params![task.task_id.as_str()],
        |row| row.get(0),
    )?;
    if incarnation != task.incarnation_id {
        return Err(StoreError::CorruptSessionWork);
    }

    match &work.phase {
        SessionWorkPhase::Delivering(_) | SessionWorkPhase::Uncertain(_)
            if detail.snapshot.status == TaskStatus::Queued =>
        {
            if load_run(connection, &task.run_id)?.is_some() {
                return Err(StoreError::CorruptSessionWork);
            }
        }
        SessionWorkPhase::Running(_) if detail.snapshot.status == TaskStatus::Running => {
            let run = load_run(connection, &task.run_id)?.ok_or(StoreError::CorruptSessionWork)?;
            if run.project_id != *project_id
                || run.agent_id != *agent_id
                || run.session_id.as_ref() != Some(session_id)
                || run.task_id.as_ref() != Some(&task.task_id)
                || run.ended_at_ms.is_some()
            {
                return Err(StoreError::CorruptSessionWork);
            }
        }
        _ => return Err(StoreError::CorruptSessionWork),
    }
    Ok(())
}

fn end_session_work_in_transaction(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    now_ms: i64,
) -> Result<()> {
    let owner =
        load_session_work(transaction, session_id)?.ok_or(StoreError::CorruptSessionWork)?;
    if owner.quarantine_reason.is_some() {
        let changed = transaction.execute(
            "UPDATE session_work
             SET state = 'empty', revision = revision + 1, attempt_id = NULL,
                 task_id = NULL, task_incarnation_id = NULL, task_revision = NULL,
                 run_id = NULL, quarantine_reason = NULL,
                 updated_at_ms = ?1
             WHERE session_id = ?2 AND quarantine_reason IS NOT NULL",
            params![now_ms, session_id.as_str()],
        )?;
        if changed != 1 {
            return Err(StoreError::SessionWorkConflict);
        }
        return Ok(());
    }
    let current = owner.work.as_ref().ok_or(StoreError::CorruptSessionWork)?;
    let ended = current.end_session().map_err(transition_error)?;
    persist_session_work(transaction, session_id, current.revision, &ended, now_ms)
}

fn cancel_reserved_session_work_in_transaction(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    now_ms: i64,
) -> Result<()> {
    let owner =
        load_session_work(transaction, session_id)?.ok_or(StoreError::CorruptSessionWork)?;
    if owner.quarantine_reason.is_some() {
        return Ok(());
    }
    let current = owner.work.as_ref().ok_or(StoreError::CorruptSessionWork)?;
    let SessionWorkPhase::Delivering(lease) = &current.phase else {
        return Ok(());
    };
    let released = current
        .cancel_reservation(&lease.attempt_id)
        .map_err(transition_error)?;
    persist_session_work(transaction, session_id, current.revision, &released, now_ms)
}

fn transition_error(_: crate::session_work::TransitionError) -> StoreError {
    StoreError::SessionWorkConflict
}

fn load_session(connection: &Connection, session_id: &SessionId) -> Result<Option<SessionRow>> {
    connection
        .query_row(
            "SELECT s.id, s.project_id, s.agent_id, s.provider,
                    s.runtime_model, s.runtime_reasoning_effort,
                    s.runtime_permission_mode, s.runtime_control_mode,
                    s.provider_session_id, s.worktree, s.codex_home, s.hook_token,
                    s.state, s.state_since_ms,
                    s.activity, s.activity_inferred, s.wait_reason, s.observer_reason,
                    s.observer_health, s.observer_health_since_ms, s.runner_instance_id, s.runner_runtime,
                    s.runner_protocol_version, s.last_hook_event, s.notification_kind,
                    s.last_hook_at_ms,
                    s.started_at_ms, s.updated_at_ms, s.ended_at_ms, s.exit_code,
                    s.exit_signal, s.stop_requested_at_ms,
                    s.delivery_recovery_stop_requested_at_ms,
                    (SELECT r.id FROM runs r
                     WHERE r.session_id = s.id AND r.ended_at_ms IS NULL LIMIT 1)
             FROM sessions s WHERE s.id = ?1",
            params![session_id.as_str()],
            session_row_from_row,
        )
        .optional()
        .map_err(StoreError::from)
}

fn session_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    let provider: String = row.get(3)?;
    let state: String = row.get(12)?;
    let observer_health: String = row.get(18)?;
    let protocol: i64 = row.get(22)?;
    let last_hook_event: Option<String> = row.get(23)?;
    let notification_kind: Option<String> = row.get(24)?;
    let current_run_id: Option<String> = row.get(33)?;
    Ok(SessionRow {
        id: parse_id(row.get(0)?, 0)?,
        project_id: parse_id(row.get(1)?, 1)?,
        agent_id: parse_id(row.get(2)?, 2)?,
        provider: parse_provider(&provider, 3)?,
        runtime_model: row.get(4)?,
        runtime_reasoning_effort: row.get(5)?,
        runtime_permission_mode: row.get(6)?,
        runtime_control_mode: row.get(7)?,
        provider_session_id: row.get(8)?,
        worktree: row.get(9)?,
        codex_home: row.get(10)?,
        hook_token: row.get(11)?,
        state: parse_session_state(&state, 12)?,
        state_since_ms: row.get(13)?,
        activity: row.get(14)?,
        activity_inferred: row.get(15)?,
        wait_reason: row.get(16)?,
        observer_reason: row.get(17)?,
        observer_health: parse_observer_health(&observer_health, 18)?,
        observer_health_since_ms: row.get(19)?,
        runner_instance_id: parse_id(row.get(20)?, 20)?,
        runner_runtime: row.get(21)?,
        runner_protocol_version: u16::try_from(protocol).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(22, Type::Integer, Box::new(error))
        })?,
        last_hook_event: last_hook_event
            .map(|value| parse_provider_hook_event(&value, 23))
            .transpose()?,
        notification_kind: notification_kind
            .map(|value| parse_provider_notification_kind(&value, 24))
            .transpose()?,
        last_hook_at_ms: row.get(25)?,
        started_at_ms: row.get(26)?,
        updated_at_ms: row.get(27)?,
        ended_at_ms: row.get(28)?,
        exit_code: row.get(29)?,
        exit_signal: row.get(30)?,
        stop_requested_at_ms: row.get(31)?,
        delivery_recovery_stop_requested_at_ms: row.get(32)?,
        current_run_id: parse_optional_id(current_run_id, 33)?,
    })
}

fn budget_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentBudget> {
    let max_tool_calls: Option<i64> = row.get(0)?;
    let tool_calls: i64 = row.get(1)?;
    Ok(AgentBudget {
        max_tool_calls: max_tool_calls
            .map(|value| {
                u64::try_from(value).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(0, Type::Integer, Box::new(error))
                })
            })
            .transpose()?,
        tool_calls: u64::try_from(tool_calls).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(1, Type::Integer, Box::new(error))
        })?,
        exhausted: row.get(2)?,
        reset_at_ms: row.get(3)?,
        updated_at_ms: row.get(4)?,
        monetary_spend: None,
    })
}

fn agent_pause_reasons(
    connection: &Connection,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<Vec<AgentPauseReason>> {
    let holds: Option<(bool, bool)> = connection
        .query_row(
            "SELECT a.paused, b.exhausted FROM agents a
             JOIN agent_budgets b ON b.agent_id = a.id
             WHERE a.id = ?1 AND a.project_id = ?2",
            params![agent_id.as_str(), project_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (agent_hold, budget_exhausted) = holds.ok_or(StoreError::AgentNotFound)?;
    let mut reasons = Vec::with_capacity(2);
    if agent_hold {
        reasons.push(AgentPauseReason::AgentHold);
    }
    if budget_exhausted {
        reasons.push(AgentPauseReason::BudgetExhausted);
    }
    Ok(reasons)
}

fn load_agent_message(
    connection: &Connection,
    message_id: &MessageId,
) -> Result<Option<AgentMessage>> {
    connection
        .query_row(
            "SELECT id, project_id, sender_agent_id, recipient_agent_id,
                    body, created_at_ms, delivered_at_ms, delivered_run_id,
                    delivered_session_id
             FROM agent_messages WHERE id = ?1",
            params![message_id.as_str()],
            agent_message_from_row,
        )
        .optional()
        .map_err(StoreError::from)
}

fn agent_message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentMessage> {
    let sender_agent_id: Option<String> = row.get(2)?;
    let delivered_run_id: Option<String> = row.get(7)?;
    let delivered_session_id: Option<String> = row.get(8)?;
    Ok(AgentMessage {
        id: parse_id(row.get(0)?, 0)?,
        project_id: parse_id(row.get(1)?, 1)?,
        sender_agent_id: parse_optional_id(sender_agent_id, 2)?,
        recipient_agent_id: parse_id(row.get(3)?, 3)?,
        body: row.get(4)?,
        created_at_ms: row.get(5)?,
        delivered_at_ms: row.get(6)?,
        delivered_run_id: parse_optional_id(delivered_run_id, 7)?,
        delivered_session_id: parse_optional_id(delivered_session_id, 8)?,
    })
}

fn load_active_delivery_attempt(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> rusqlite::Result<Option<DeliveryAttempt>> {
    transaction
        .query_row(
            "SELECT id, project_id, agent_id, session_id, task_id,
                    task_incarnation_id, message_ids_json,
                    text, failure_count, next_attempt_at_ms, state,
                    task_revision, run_id
             FROM delivery_attempts
             WHERE project_id = ?1 AND agent_id = ?2
               AND state IN ('in_flight', 'retryable', 'terminal')",
            params![project_id.as_str(), agent_id.as_str()],
            delivery_attempt_from_row,
        )
        .optional()
}

fn load_delivery_attempt(
    transaction: &Transaction<'_>,
    attempt_id: &str,
) -> rusqlite::Result<Option<DeliveryAttempt>> {
    transaction
        .query_row(
            "SELECT id, project_id, agent_id, session_id, task_id,
                    task_incarnation_id, message_ids_json,
                    text, failure_count, next_attempt_at_ms, state,
                    task_revision, run_id
             FROM delivery_attempts WHERE id = ?1",
            params![attempt_id],
            delivery_attempt_from_row,
        )
        .optional()
}

/// The journal records what happened to a delivery attempt; only the exact
/// session-work lease authorizes acknowledgement. A terminal journal row can
/// therefore resolve a late hook only when that same lease is already
/// `uncertain` (provider bytes may have crossed). Terminal audit state alone
/// never manufactures external-effect authority.
fn prepare_delivery_acknowledgement(
    current: &SessionWork,
    attempt: &DeliveryAttempt,
) -> Result<(SessionWork, &'static str)> {
    let lease = current
        .phase
        .lease()
        .ok_or(StoreError::SessionWorkConflict)?;
    if lease.attempt_id != attempt.id
        || match (&lease.task, &attempt.task_id) {
            (None, None) => {
                attempt.task_incarnation_id.is_some()
                    || attempt.task_revision.is_some()
                    || attempt.run_id.is_some()
            }
            (Some(task), Some(task_id)) => {
                task.task_id != *task_id
                    || attempt.task_incarnation_id.as_deref() != Some(task.incarnation_id.as_str())
                    || attempt.task_revision != Some(task.revision)
                    || attempt.run_id.as_ref() != Some(&task.run_id)
            }
            _ => true,
        }
    {
        return Err(StoreError::SessionWorkConflict);
    }

    match attempt.state {
        DeliveryAttemptState::InFlight | DeliveryAttemptState::Retryable => {
            let ready = match &current.phase {
                SessionWorkPhase::Delivering(_) => current
                    .external_effect_possible(&attempt.id)
                    .map_err(transition_error)?,
                SessionWorkPhase::Uncertain(_) => current.clone(),
                _ => return Err(StoreError::SessionWorkConflict),
            };
            Ok((
                ready,
                if attempt.state == DeliveryAttemptState::InFlight {
                    "in_flight"
                } else {
                    "retryable"
                },
            ))
        }
        DeliveryAttemptState::Terminal
            if matches!(&current.phase, SessionWorkPhase::Uncertain(_)) =>
        {
            Ok((current.clone(), "terminal"))
        }
        DeliveryAttemptState::Terminal
        | DeliveryAttemptState::Acknowledged
        | DeliveryAttemptState::Cancelled => Err(StoreError::SessionWorkConflict),
    }
}

fn delivery_attempt_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeliveryAttempt> {
    let task_id: Option<String> = row.get(4)?;
    let message_ids: Vec<String> =
        serde_json::from_str(&row.get::<_, String>(6)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(6, Type::Text, Box::new(error))
        })?;
    let failure_count = row.get::<_, i64>(8)?;
    Ok(DeliveryAttempt {
        id: row.get(0)?,
        project_id: parse_id(row.get(1)?, 1)?,
        agent_id: parse_id(row.get(2)?, 2)?,
        session_id: parse_id(row.get(3)?, 3)?,
        task_id: task_id.map(|value| parse_id(value, 4)).transpose()?,
        task_incarnation_id: row.get(5)?,
        message_ids: message_ids
            .into_iter()
            .map(|value| parse_id(value, 6))
            .collect::<rusqlite::Result<Vec<_>>>()?,
        text: row.get(7)?,
        failure_count: u32::try_from(failure_count).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(8, Type::Integer, Box::new(error))
        })?,
        next_attempt_at_ms: row.get(9)?,
        state: parse_delivery_attempt_state(&row.get::<_, String>(10)?, 10)?,
        task_revision: row.get(11)?,
        run_id: parse_optional_id(row.get(12)?, 12)?,
    })
}

fn append_execution_events(
    transaction: &Transaction<'_>,
    now_ms: i64,
    task: &TaskSnapshot,
    agent: &AgentSnapshot,
    run: &RunSnapshot,
) -> Result<Vec<EventEnvelope>> {
    let factory_events = [
        FactoryEvent::TaskChanged { task: task.clone() },
        FactoryEvent::AgentChanged {
            agent: agent.clone(),
        },
        FactoryEvent::RunChanged { run: run.clone() },
    ];
    factory_events
        .into_iter()
        .map(|event| {
            let sequence = append_event(transaction, now_ms, &event)?;
            Ok(EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            })
        })
        .collect()
}

fn validate_provider_session(value: Option<&str>) -> Result<()> {
    if value.is_some_and(|value| {
        value.is_empty()
            || value.len() > MAX_PROVIDER_SESSION_BYTES
            || value.chars().any(char::is_control)
    }) {
        Err(StoreError::InvalidExecutionMetadata)
    } else {
        Ok(())
    }
}

fn validate_absolute_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_PATH_BYTES
        || value.contains('\0')
        || !Path::new(value).is_absolute()
    {
        Err(StoreError::InvalidExecutionMetadata)
    } else {
        Ok(())
    }
}

fn validate_hook_token(value: &str) -> Result<()> {
    if value.len() == HOOK_TOKEN_HEX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(StoreError::InvalidHookToken)
    }
}

/// Constant-time byte comparison for secret material (hook tokens, webhook
/// signatures). Shared between the sessions store and the webhook HTTP
/// front door so neither reinvents it.
pub(crate) fn constant_time_eq(expected: &[u8], provided: &[u8]) -> bool {
    if expected.len() != provided.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (&left, &right) in expected.iter().zip(provided) {
        difference |= left ^ right;
    }
    std::hint::black_box(difference) == 0
}

fn new_run_id() -> Result<RunId> {
    RunId::try_from(Uuid::new_v4().hyphenated().to_string())
        .map_err(|_| StoreError::InvalidExecutionMetadata)
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let mut current: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current < 0 {
        return Err(StoreError::InvalidSchemaVersion(current));
    }
    if current > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    if current == 0 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0001_state_and_events.sql"))?;
        transaction.pragma_update(None, "user_version", 1)?;
        transaction.commit()?;
        current = 1;
    }
    if current == 1 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0002_execution_ledger.sql"))?;
        transaction.pragma_update(None, "user_version", 2)?;
        transaction.commit()?;
        current = 2;
    }
    if current == 2 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0003_runner_reconciliation.sql"))?;
        transaction.pragma_update(None, "user_version", 3)?;
        transaction.commit()?;
        current = 3;
    }
    if current == 3 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0004_observer_health.sql"))?;
        transaction.pragma_update(None, "user_version", 4)?;
        transaction.commit()?;
        current = 4;
    }
    if current == 4 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!(
            "../migrations/0005_provider_session_context.sql"
        ))?;
        transaction.pragma_update(None, "user_version", 5)?;
        transaction.commit()?;
        current = 5;
    }
    if current == 5 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0006_webhooks.sql"))?;
        transaction.pragma_update(None, "user_version", 6)?;
        transaction.commit()?;
        current = 6;
    }
    if current == 6 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0007_subscription_usage.sql"))?;
        transaction.pragma_update(None, "user_version", 7)?;
        transaction.commit()?;
        current = 7;
    }
    if current == 7 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0008_subscription_windows.sql"))?;
        transaction.pragma_update(None, "user_version", 8)?;
        transaction.commit()?;
        current = 8;
    }
    if current == 8 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0009_agent_profiles.sql"))?;
        transaction.pragma_update(None, "user_version", 9)?;
        transaction.commit()?;
        current = 9;
    }
    if current == 9 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0010_agent_messages.sql"))?;
        transaction.pragma_update(None, "user_version", 10)?;
        transaction.commit()?;
        current = 10;
    }
    if current == 10 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0011_run_stop_intent.sql"))?;
        transaction.pragma_update(None, "user_version", 11)?;
        transaction.commit()?;
        current = 11;
    }
    if current == 11 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!(
            "../migrations/0012_drop_subscription_usage_and_task_dependencies.sql"
        ))?;
        transaction.pragma_update(None, "user_version", 12)?;
        transaction.commit()?;
        current = 12;
    }
    if current == 12 {
        // Standing instructions and memory move from `agent_profiles` TEXT
        // columns to files under `$DARK_FACTORY_HOME/projects`; write out any
        // existing non-empty text before the columns are dropped below.
        migrate_agent_profile_text_to_files(connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0013_agent_profile_files.sql"))?;
        transaction.pragma_update(None, "user_version", 13)?;
        transaction.commit()?;
        current = 13;
    }
    if current == 13 {
        // `PRAGMA foreign_keys` cannot be toggled inside a transaction; the
        // rebuild below drops/recreates `agents`/`runs` and their foreign
        // keys, so it must run with the pragma off, then be verified with
        // `PRAGMA foreign_key_check` once it is back on (TRACK5-DESIGN.md
        // section 1, section 8 risk 8).
        connection.pragma_update(None, "foreign_keys", false)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0014_sessions.sql"))?;
        transaction.pragma_update(None, "user_version", 14)?;
        transaction.commit()?;
        connection.pragma_update(None, "foreign_keys", true)?;
        verify_no_foreign_key_violations(connection)?;
        current = 14;
    }
    if current == 14 {
        // `runs.session_id`/`agent_messages.delivered_session_id` both
        // reference `sessions`, rebuilt below to widen its
        // `last_hook_event` CHECK -- same off/verify dance as 0014, for
        // the same reason (`PRAGMA foreign_keys` cannot toggle inside a
        // transaction).
        connection.pragma_update(None, "foreign_keys", false)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!(
            "../migrations/0015_permission_request_hook_event.sql"
        ))?;
        transaction.pragma_update(None, "user_version", 15)?;
        transaction.commit()?;
        connection.pragma_update(None, "foreign_keys", true)?;
        verify_no_foreign_key_violations(connection)?;
        current = 15;
    }
    if current == 15 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0016_task_incarnations.sql"))?;
        transaction.pragma_update(None, "user_version", 16)?;
        transaction.commit()?;
        current = 16;
    }
    if current == 16 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0017_auto_mode.sql"))?;
        transaction.pragma_update(None, "user_version", 17)?;
        transaction.commit()?;
        current = 17;
    }
    if current == 17 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0018_agent_budgets.sql"))?;
        transaction.pragma_update(None, "user_version", 18)?;
        transaction.commit()?;
        current = 18;
    }
    if current == 18 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0019_repository_authority.sql"))?;
        transaction.pragma_update(None, "user_version", 19)?;
        transaction.commit()?;
        current = 19;
    }
    if current == 19 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0020_connector_events.sql"))?;
        transaction.pragma_update(None, "user_version", 20)?;
        transaction.commit()?;
        current = 20;
    }
    if current == 20 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let already_present: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'runtime_model'
             )",
            [],
            |row| row.get(0),
        )?;
        if !already_present {
            transaction.execute_batch(include_str!(
                "../migrations/0021_session_runtime_metadata.sql"
            ))?;
        }
        transaction.pragma_update(None, "user_version", 21)?;
        transaction.commit()?;
        current = 21;
    }
    if current == 21 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!(
            "../migrations/0022_repair_legacy_permission_modes.sql"
        ))?;
        transaction.pragma_update(None, "user_version", 22)?;
        transaction.commit()?;
        current = 22;
    }
    if current == 22 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0023_agent_model_policy.sql"))?;
        transaction.pragma_update(None, "user_version", 23)?;
        transaction.commit()?;
        current = 23;
    }
    if current == 23 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0024_delivery_attempts.sql"))?;
        transaction.pragma_update(None, "user_version", 24)?;
        transaction.commit()?;
        current = 24;
    }
    if current == 24 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!(
            "../migrations/0025_provider_resume_recovery.sql"
        ))?;
        transaction.pragma_update(None, "user_version", 25)?;
        transaction.commit()?;
        current = 25;
    }
    if current == 25 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!(
            "../migrations/0026_session_observer_reason.sql"
        ))?;
        transaction.pragma_update(None, "user_version", 26)?;
        transaction.commit()?;
        current = 26;
    }
    if current == 26 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!(
            "../migrations/0027_session_notification_kind.sql"
        ))?;
        transaction.pragma_update(None, "user_version", 27)?;
        transaction.commit()?;
        current = 27;
    }
    if current == 27 {
        // The rebuild drops the sessions parent of populated child tables;
        // disable foreign-key enforcement for the transaction and verify the
        // restored graph before returning to normal operation.
        connection.pragma_update(None, "foreign_keys", false)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!(
            "../migrations/0028_widen_session_notification_kind.sql"
        ))?;
        transaction.pragma_update(None, "user_version", 28)?;
        transaction.commit()?;
        connection.pragma_update(None, "foreign_keys", true)?;
        verify_no_foreign_key_violations(connection)?;
        current = 28;
    }
    if current == 28 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0029_session_work.sql"))?;
        backfill_session_work(&transaction)?;
        transaction.execute(
            "CREATE UNIQUE INDEX runs_one_open_per_session
             ON runs(session_id) WHERE session_id IS NOT NULL AND ended_at_ms IS NULL",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 29)?;
        transaction.commit()?;
        verify_no_foreign_key_violations(connection)?;
        current = 29;
    }
    if current == 29 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!(
            "../migrations/0030_authenticated_principals.sql"
        ))?;
        transaction.pragma_update(None, "user_version", 30)?;
        transaction.commit()?;
    }
    Ok(())
}

/// Confirms the schema rebuild in `0014_sessions.sql` left no dangling
/// references. `PRAGMA foreign_key_check` never raises a SQL error itself;
/// it returns one row per violation, so absence of rows is the only proof.
fn verify_no_foreign_key_violations(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA foreign_key_check;")?;
    let mut rows = statement.query([])?;
    if rows.next()?.is_some() {
        return Err(StoreError::ForeignKeyViolation);
    }
    Ok(())
}

/// Conservatively adopts legacy ownership into the one-session authority.
/// Clean open runs become `running`; an active attempt whose external effect
/// cannot be disproved becomes `uncertain`; every contradictory combination
/// is retained as a durable quarantine instead of being cancelled or guessed
/// through. Slice B supervision consumes those quarantines by stopping the
/// exact session under the normal lifecycle contract.
fn backfill_session_work(transaction: &Transaction<'_>) -> Result<()> {
    #[derive(Clone)]
    struct LegacySession {
        id: String,
        project_id: String,
        agent_id: String,
        ended: bool,
        updated_at_ms: i64,
    }
    #[derive(Clone)]
    struct LegacyRun {
        id: String,
        project_id: String,
        agent_id: String,
        task_id: Option<String>,
    }
    #[derive(Clone)]
    struct LegacyAttempt {
        id: String,
        project_id: String,
        agent_id: String,
        task_id: Option<String>,
        task_incarnation_id: Option<String>,
    }

    let sessions = {
        let mut statement = transaction.prepare(
            "SELECT id, project_id, agent_id, ended_at_ms IS NOT NULL, updated_at_ms
             FROM sessions ORDER BY id",
        )?;
        statement
            .query_map([], |row| {
                Ok(LegacySession {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    agent_id: row.get(2)?,
                    ended: row.get(3)?,
                    updated_at_ms: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for session in sessions {
        let open_runs = {
            let mut statement = transaction.prepare(
                "SELECT id, project_id, agent_id, task_id FROM runs
                 WHERE session_id = ?1 AND ended_at_ms IS NULL ORDER BY id",
            )?;
            statement
                .query_map(params![session.id.as_str()], |row| {
                    Ok(LegacyRun {
                        id: row.get(0)?,
                        project_id: row.get(1)?,
                        agent_id: row.get(2)?,
                        task_id: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let active_attempts = {
            let mut statement = transaction.prepare(
                "SELECT id, project_id, agent_id, task_id, task_incarnation_id
                 FROM delivery_attempts
                 WHERE session_id = ?1
                   AND state IN ('in_flight', 'retryable', 'terminal')
                 ORDER BY created_at_ms, id",
            )?;
            statement
                .query_map(params![session.id.as_str()], |row| {
                    Ok(LegacyAttempt {
                        id: row.get(0)?,
                        project_id: row.get(1)?,
                        agent_id: row.get(2)?,
                        task_id: row.get(3)?,
                        task_incarnation_id: row.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut identity_issue = None;
        for run in &open_runs {
            if run.project_id != session.project_id || run.agent_id != session.agent_id {
                identity_issue = Some(format!(
                    "open run {} does not belong to the session project and agent",
                    run.id
                ));
                break;
            }
            let Some(task_id) = run.task_id.as_deref() else {
                identity_issue = Some(format!("open run {} has no task identity", run.id));
                break;
            };
            let task_owner: Option<(String, Option<String>, String)> = transaction
                .query_row(
                    "SELECT project_id, assigned_agent_id, status FROM tasks WHERE id = ?1",
                    params![task_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if !matches!(
                task_owner,
                Some((project_id, Some(agent_id), status))
                    if project_id == session.project_id
                        && agent_id == session.agent_id
                        && status == "running"
            ) {
                identity_issue = Some(format!(
                    "open run {} does not own an exact running task",
                    run.id
                ));
                break;
            }
        }
        if identity_issue.is_none() {
            for attempt in &active_attempts {
                if attempt.project_id != session.project_id || attempt.agent_id != session.agent_id
                {
                    identity_issue = Some(format!(
                        "active attempt {} does not belong to the session project and agent",
                        attempt.id
                    ));
                    break;
                }
                match (&attempt.task_id, &attempt.task_incarnation_id) {
                    (None, None) => {}
                    (Some(task_id), Some(incarnation_id)) => {
                        let task_owner: Option<(String, Option<String>, String, String)> =
                            transaction
                                .query_row(
                                    "SELECT project_id, assigned_agent_id, status, incarnation_id
                                     FROM tasks WHERE id = ?1",
                                    params![task_id],
                                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                                )
                                .optional()?;
                        if !matches!(
                            task_owner,
                            Some((project_id, Some(agent_id), status, current_incarnation))
                                if project_id == session.project_id
                                    && agent_id == session.agent_id
                                    && status == "queued"
                                    && current_incarnation == *incarnation_id
                        ) {
                            identity_issue = Some(format!(
                                "active attempt {} does not own an exact queued task",
                                attempt.id
                            ));
                            break;
                        }
                    }
                    _ => {
                        identity_issue = Some(format!(
                            "active attempt {} has a partial task identity",
                            attempt.id
                        ));
                        break;
                    }
                }
            }
        }

        let quarantine = identity_issue.or_else(|| {
            match (session.ended, open_runs.len(), active_attempts.len()) {
                (true, 0, 0) | (false, 0, 0) => None,
                (false, 1, 0) | (false, 0, 1) => None,
                (true, runs, attempts) => Some(format!(
                    "ended session retained {runs} open run(s) and {attempts} active delivery attempt(s)"
                )),
                (false, runs, attempts) => Some(format!(
                    "live session retained contradictory ownership: {runs} open run(s), {attempts} active delivery attempt(s)"
                )),
            }
        });

        if let Some(reason) = quarantine {
            let retained_attempt = active_attempts.first().filter(|attempt| {
                attempt.project_id == session.project_id && attempt.agent_id == session.agent_id
            });
            let retained_run = open_runs.first().filter(|run| {
                run.project_id == session.project_id && run.agent_id == session.agent_id
            });
            transaction.execute(
                "INSERT INTO session_work (
                    session_id, project_id, agent_id, state, revision, attempt_id,
                    task_id, task_incarnation_id, task_revision,
                    run_id, quarantine_reason, updated_at_ms
                 ) VALUES (?1, ?2, ?3, 'uncertain', 0, ?4, ?5, ?6, NULL, ?7, ?8, ?9)",
                params![
                    session.id,
                    session.project_id,
                    session.agent_id,
                    retained_attempt.map(|attempt| attempt.id.as_str()),
                    retained_attempt.and_then(|attempt| attempt.task_id.as_deref()),
                    retained_attempt.and_then(|attempt| attempt.task_incarnation_id.as_deref()),
                    retained_run.map(|run| run.id.as_str()),
                    reason,
                    session.updated_at_ms,
                ],
            )?;

            // A uniqueness constraint cannot be installed while a legacy
            // session owns multiple open runs. Terminalize each ambiguous
            // run together with its still-live task and durable events; run
            // rows alone are not enough lifecycle truth. An already-ended
            // session cannot wait for future supervision, so resolve all of
            // its retained ownership during migration too.
            if open_runs.len() > 1 || session.ended {
                terminalize_open_runs_for_migration(
                    transaction,
                    &session.id,
                    session.updated_at_ms,
                )?;
            }
            if session.ended {
                transaction.execute(
                    "UPDATE delivery_attempts
                     SET state = 'cancelled', next_attempt_at_ms = NULL, updated_at_ms = ?1
                     WHERE session_id = ?2
                       AND state IN ('in_flight', 'retryable', 'terminal')",
                    params![session.updated_at_ms, session.id],
                )?;
                end_session_work_in_transaction(
                    transaction,
                    &parse_id(session.id.clone(), 0)?,
                    session.updated_at_ms,
                )?;
            }
            continue;
        }

        if session.ended || (open_runs.is_empty() && active_attempts.is_empty()) {
            transaction.execute(
                "INSERT INTO session_work (
                    session_id, project_id, agent_id, state, revision, updated_at_ms
                 ) VALUES (?1, ?2, ?3, 'empty', 0, ?4)",
                params![
                    session.id,
                    session.project_id,
                    session.agent_id,
                    session.updated_at_ms,
                ],
            )?;
            continue;
        }

        if let Some(run) = open_runs.first() {
            let Some(task_id) = run.task_id.as_deref() else {
                transaction.execute(
                    "INSERT INTO session_work (
                        session_id, project_id, agent_id, state, revision, run_id,
                        quarantine_reason, updated_at_ms
                     ) VALUES (?1, ?2, ?3, 'uncertain', 0, ?4, ?5, ?6)",
                    params![
                        session.id,
                        session.project_id,
                        session.agent_id,
                        run.id,
                        "open run has no task identity",
                        session.updated_at_ms,
                    ],
                )?;
                continue;
            };
            let task_identity: Option<(String, i64)> = transaction
                .query_row(
                    "SELECT incarnation_id, work_revision FROM tasks WHERE id = ?1",
                    params![task_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((incarnation_id, task_revision)) = task_identity else {
                transaction.execute(
                    "INSERT INTO session_work (
                        session_id, project_id, agent_id, state, revision, run_id,
                        quarantine_reason, updated_at_ms
                     ) VALUES (?1, ?2, ?3, 'uncertain', 0, ?4, ?5, ?6)",
                    params![
                        session.id,
                        session.project_id,
                        session.agent_id,
                        run.id,
                        "open run references a missing task",
                        session.updated_at_ms,
                    ],
                )?;
                continue;
            };
            let attempt_id = format!("migration:run:{}", run.id);
            transaction.execute(
                "INSERT INTO delivery_attempts (
                    id, project_id, agent_id, session_id, task_id,
                    task_incarnation_id, prior_run_count, message_ids_json, text,
                    failure_count, next_attempt_at_ms, state, created_at_ms, updated_at_ms,
                    task_revision, run_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, '[]', ?7,
                           0, NULL, 'acknowledged', ?8, ?8, ?9, ?10)",
                params![
                    attempt_id,
                    session.project_id,
                    session.agent_id,
                    session.id,
                    task_id,
                    incarnation_id,
                    "migration backfill for an open run",
                    session.updated_at_ms,
                    task_revision,
                    run.id,
                ],
            )?;
            transaction.execute(
                "INSERT INTO session_work (
                    session_id, project_id, agent_id, state, revision, attempt_id,
                    task_id, task_incarnation_id, task_revision,
                    run_id, updated_at_ms
                 ) VALUES (?1, ?2, ?3, 'running', 0, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    session.id,
                    session.project_id,
                    session.agent_id,
                    attempt_id,
                    task_id,
                    incarnation_id,
                    task_revision,
                    run.id,
                    session.updated_at_ms,
                ],
            )?;
            continue;
        }

        let attempt = active_attempts
            .first()
            .ok_or(StoreError::CorruptSessionWork)?;
        let (task_id, incarnation_id, task_revision, run_id, quarantine_reason) =
            if let (Some(task_id), Some(attempt_incarnation)) =
                (&attempt.task_id, &attempt.task_incarnation_id)
            {
                let current: Option<(String, i64)> = transaction
                    .query_row(
                        "SELECT incarnation_id, work_revision FROM tasks WHERE id = ?1",
                        params![task_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                match current {
                    Some((current_incarnation, revision))
                        if current_incarnation == *attempt_incarnation =>
                    {
                        (
                            Some(task_id.as_str()),
                            Some(attempt_incarnation.as_str()),
                            Some(revision),
                            Some(new_run_id()?),
                            None,
                        )
                    }
                    _ => (
                        Some(task_id.as_str()),
                        Some(attempt_incarnation.as_str()),
                        None,
                        None,
                        Some("active attempt references stale or missing task identity"),
                    ),
                }
            } else if attempt.task_id.is_none() && attempt.task_incarnation_id.is_none() {
                (None, None, None, None, None)
            } else {
                (
                    attempt.task_id.as_deref(),
                    attempt.task_incarnation_id.as_deref(),
                    None,
                    None,
                    Some("active attempt has a partial task identity"),
                )
            };
        let run_id_text = run_id.as_ref().map(RunId::as_str);
        transaction.execute(
            "UPDATE delivery_attempts
             SET task_revision = ?1, run_id = ?2
             WHERE id = ?3",
            params![task_revision, run_id_text, attempt.id],
        )?;
        transaction.execute(
            "INSERT INTO session_work (
                session_id, project_id, agent_id, state, revision, attempt_id,
                task_id, task_incarnation_id, task_revision,
                run_id, quarantine_reason, updated_at_ms
             ) VALUES (?1, ?2, ?3, 'uncertain', 0, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session.id,
                session.project_id,
                session.agent_id,
                attempt.id,
                task_id,
                incarnation_id,
                task_revision,
                run_id_text,
                quarantine_reason,
                session.updated_at_ms,
            ],
        )?;
    }
    let backfilled = {
        let mut statement = transaction.prepare(
            "SELECT session_id FROM session_work
             WHERE quarantine_reason IS NULL ORDER BY session_id",
        )?;
        statement
            .query_map([], |row| parse_id::<SessionId>(row.get(0)?, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for session_id in backfilled {
        require_session_work(transaction, &session_id)?;
    }
    Ok(())
}

/// Resolves corrupt legacy run ownership without leaving its tasks in the
/// non-deliverable `running` state. This is migration-only because normal
/// runtime closes one exact run through `close_run_in_transaction`.
fn terminalize_open_runs_for_migration(
    transaction: &Transaction<'_>,
    session_id: &str,
    now_ms: i64,
) -> Result<()> {
    let run_ids = {
        let mut statement = transaction.prepare(
            "SELECT id FROM runs
             WHERE session_id = ?1 AND ended_at_ms IS NULL ORDER BY id",
        )?;
        statement
            .query_map(params![session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for run_id in run_ids {
        let run_id: RunId = parse_id(run_id, 0)?;
        let run = load_run(transaction, &run_id)?.ok_or(StoreError::RunNotFound)?;
        transaction.execute(
            "UPDATE runs
             SET status = 'failed', status_since_ms = ?1, updated_at_ms = ?1,
                 ended_at_ms = ?1, closed_by = 'session_ended',
                 failure_reason = 'unverifiable', activity = NULL, wait_reason = NULL
             WHERE id = ?2 AND ended_at_ms IS NULL",
            params![now_ms, run_id.as_str()],
        )?;
        if let Some(task_id) = run.task_id.as_ref() {
            let changed = transaction.execute(
                "UPDATE tasks
                 SET status = 'failed', updated_at_ms = ?1, completed_at_ms = ?1,
                     blocked_reason = NULL, work_revision = work_revision + 1
                 WHERE id = ?2 AND project_id = ?3
                   AND status IN ('queued', 'running', 'blocked')",
                params![now_ms, task_id.as_str(), run.project_id.as_str()],
            )?;
            if changed == 1 {
                let task = load_task(transaction, task_id)?.ok_or(StoreError::TaskNotFound)?;
                append_event(
                    transaction,
                    now_ms,
                    &FactoryEvent::TaskChanged {
                        task: task.snapshot,
                    },
                )?;
            }
        }
        append_agent_changed_event(transaction, &run.agent_id, now_ms)?;
        let run = load_run(transaction, &run_id)?.ok_or(StoreError::RunNotFound)?;
        append_event(transaction, now_ms, &FactoryEvent::RunChanged { run })?;
    }
    Ok(())
}

/// One-time data migration paired with `0013_agent_profile_files.sql`:
/// writes any pre-existing `agent_profiles.instructions`/`.memory` text out
/// to the new guidance files before those columns are dropped. A fresh
/// database never has agent rows at migration time, so this is a no-op in
/// practice; it exists for correctness on a real upgrade.
fn migrate_agent_profile_text_to_files(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT ap.agent_id, a.project_id, ap.instructions, ap.memory
         FROM agent_profiles ap
         JOIN agents a ON a.id = ap.agent_id
         WHERE ap.instructions <> '' OR ap.memory <> ''",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Ok(());
    }
    let home = factory_core::paths::dark_factory_home()
        .map_err(|error| StoreError::AgentProfileMigration(error.to_string()))?;
    for (agent_id, project_id, instructions, memory) in rows {
        let project_id = ProjectId::try_from(project_id)
            .map_err(|error| StoreError::AgentProfileMigration(error.to_string()))?;
        let agent_id = AgentId::try_from(agent_id)
            .map_err(|error| StoreError::AgentProfileMigration(error.to_string()))?;
        if !instructions.is_empty() {
            let path = factory_core::paths::agent_instructions_path(&home, &project_id, &agent_id);
            crate::guidance::write(&path, &instructions)
                .map_err(|error| StoreError::AgentProfileMigration(error.to_string()))?;
        }
        if !memory.is_empty() {
            let path = factory_core::paths::agent_memory_path(&home, &project_id, &agent_id);
            crate::guidance::write(&path, &memory)
                .map_err(|error| StoreError::AgentProfileMigration(error.to_string()))?;
        }
    }
    Ok(())
}

fn append_event(
    transaction: &Transaction<'_>,
    occurred_at_ms: i64,
    event: &FactoryEvent,
) -> Result<i64> {
    let metadata = event_metadata(event);
    let payload_value = serde_json::to_value(event)?;
    let kind = payload_value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or(StoreError::MissingEventKind)?;
    let payload = serde_json::to_string(&payload_value)?;
    transaction.execute(
        "INSERT INTO events (
            occurred_at_ms, project_id, task_id, agent_id, run_id,
            kind, schema_version, payload_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            occurred_at_ms,
            metadata.project_id.map(ProjectId::as_str),
            metadata.task_id.map(TaskId::as_str),
            metadata.agent_id.map(AgentId::as_str),
            metadata.run_id.map(RunId::as_str),
            kind,
            i64::from(PROTOCOL_VERSION),
            payload
        ],
    )?;
    Ok(transaction.last_insert_rowid())
}

fn append_agent_changed_event(
    transaction: &Transaction<'_>,
    agent_id: &AgentId,
    occurred_at_ms: i64,
) -> Result<EventEnvelope> {
    let agent = load_agent(transaction, agent_id)?
        .ok_or(StoreError::AgentNotFound)?
        .snapshot;
    let event = FactoryEvent::AgentChanged { agent };
    let sequence = append_event(transaction, occurred_at_ms, &event)?;
    Ok(EventEnvelope {
        protocol_version: PROTOCOL_VERSION,
        sequence,
        occurred_at_ms,
        event,
    })
}

struct EventMetadata<'a> {
    project_id: Option<&'a ProjectId>,
    task_id: Option<&'a TaskId>,
    agent_id: Option<&'a AgentId>,
    run_id: Option<&'a RunId>,
}

fn event_metadata(event: &FactoryEvent) -> EventMetadata<'_> {
    match event {
        FactoryEvent::AutoModeChanged { .. } => EventMetadata {
            project_id: None,
            task_id: None,
            agent_id: None,
            run_id: None,
        },
        FactoryEvent::PolicyDecision {
            project_id,
            agent_id,
            ..
        } => EventMetadata {
            project_id: Some(project_id),
            task_id: None,
            agent_id: Some(agent_id),
            run_id: None,
        },
        FactoryEvent::AgentBudgetChanged {
            project_id,
            agent_id,
            ..
        } => EventMetadata {
            project_id: Some(project_id),
            task_id: None,
            agent_id: Some(agent_id),
            run_id: None,
        },
        FactoryEvent::RepositoryAuthorityChanged { project_id } => EventMetadata {
            project_id: Some(project_id),
            task_id: None,
            agent_id: None,
            run_id: None,
        },
        FactoryEvent::ProjectChanged { project } => EventMetadata {
            project_id: Some(&project.id),
            task_id: None,
            agent_id: None,
            run_id: None,
        },
        FactoryEvent::TaskChanged { task } => EventMetadata {
            project_id: Some(&task.project_id),
            task_id: Some(&task.id),
            agent_id: task.assigned_agent_id.as_ref(),
            run_id: None,
        },
        FactoryEvent::AgentChanged { agent } => EventMetadata {
            project_id: Some(&agent.project_id),
            task_id: None,
            agent_id: Some(&agent.id),
            run_id: agent.current_run_id.as_ref(),
        },
        FactoryEvent::RunChanged { run } => EventMetadata {
            project_id: Some(&run.project_id),
            task_id: run.task_id.as_ref(),
            agent_id: Some(&run.agent_id),
            run_id: Some(&run.id),
        },
        FactoryEvent::SessionChanged { session } => EventMetadata {
            project_id: Some(&session.project_id),
            task_id: None,
            agent_id: Some(&session.agent_id),
            run_id: session.current_run_id.as_ref(),
        },
        FactoryEvent::TaskDeleted {
            project_id,
            task_id,
        } => EventMetadata {
            project_id: Some(project_id),
            task_id: Some(task_id),
            agent_id: None,
            run_id: None,
        },
        FactoryEvent::AgentDeleted {
            project_id,
            agent_id,
        } => EventMetadata {
            project_id: Some(project_id),
            task_id: None,
            agent_id: Some(agent_id),
            run_id: None,
        },
        FactoryEvent::ProjectDeleted { project_id } => EventMetadata {
            project_id: Some(project_id),
            task_id: None,
            agent_id: None,
            run_id: None,
        },
        FactoryEvent::RepositoryOperation {
            project_id,
            agent_id,
            ..
        } => EventMetadata {
            project_id: Some(project_id),
            task_id: None,
            agent_id: Some(agent_id),
            run_id: None,
        },
    }
}

fn parse_id<T>(value: String, column: usize) -> rusqlite::Result<T>
where
    T: TryFrom<String>,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    T::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn parse_optional_id<T>(value: Option<String>, column: usize) -> rusqlite::Result<Option<T>>
where
    T: TryFrom<String>,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    value.map(|value| parse_id(value, column)).transpose()
}

/// Maps `SELECT id, project_id, parent_task_id, assigned_agent_id, title,
/// status, priority, created_at_ms, updated_at_ms FROM tasks` onto a
/// [`TaskSnapshot`].
fn task_snapshot_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskSnapshot> {
    let parent_id: Option<String> = row.get(2)?;
    let assigned_id: Option<String> = row.get(3)?;
    let status: String = row.get(5)?;
    Ok(TaskSnapshot {
        id: parse_id(row.get(0)?, 0)?,
        project_id: parse_id(row.get(1)?, 1)?,
        parent_task_id: parse_optional_id(parent_id, 2)?,
        assigned_agent_id: parse_optional_id(assigned_id, 3)?,
        title: row.get(4)?,
        status: parse_task_status(&status, 5)?,
        priority: row.get(6)?,
        created_at_ms: row.get(7)?,
        updated_at_ms: row.get(8)?,
    })
}

fn parse_task_status(value: &str, column: usize) -> rusqlite::Result<TaskStatus> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn parse_agent_role(value: &str, column: usize) -> rusqlite::Result<AgentRole> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn parse_provider(value: &str, column: usize) -> rusqlite::Result<Provider> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn parse_run_status(value: &str, column: usize) -> rusqlite::Result<RunStatus> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn parse_observer_health(value: &str, column: usize) -> rusqlite::Result<ObserverHealth> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn parse_session_state(value: &str, column: usize) -> rusqlite::Result<SessionState> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn parse_run_closed_by(value: &str, column: usize) -> rusqlite::Result<RunClosedBy> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn parse_provider_hook_event(value: &str, column: usize) -> rusqlite::Result<ProviderHookEvent> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn parse_provider_notification_kind(
    value: &str,
    column: usize,
) -> rusqlite::Result<ProviderNotificationKind> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn parse_optional_failure_reason(
    value: Option<String>,
    column: usize,
) -> rusqlite::Result<Option<RunFailureReason>> {
    value
        .map(|value| {
            serde_json::from_value(serde_json::Value::String(value)).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
            })
        })
        .transpose()
}

fn parse_delivery_attempt_state(
    value: &str,
    column: usize,
) -> rusqlite::Result<DeliveryAttemptState> {
    match value {
        "in_flight" => Ok(DeliveryAttemptState::InFlight),
        "retryable" => Ok(DeliveryAttemptState::Retryable),
        "terminal" => Ok(DeliveryAttemptState::Terminal),
        "acknowledged" => Ok(DeliveryAttemptState::Acknowledged),
        "cancelled" => Ok(DeliveryAttemptState::Cancelled),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            column,
            Type::Text,
            format!("invalid delivery attempt state {value:?}").into(),
        )),
    }
}

const fn agent_role_value(value: AgentRole) -> &'static str {
    match value {
        AgentRole::Orchestrator => "orchestrator",
        AgentRole::Worker => "worker",
    }
}

const fn provider_value(value: Provider) -> &'static str {
    match value {
        Provider::ClaudeCode => "claude_code",
        Provider::Codex => "codex",
        Provider::Shell => "shell",
    }
}

const fn run_status_value(value: RunStatus) -> &'static str {
    match value {
        RunStatus::Starting => "starting",
        RunStatus::Running => "running",
        RunStatus::Waiting => "waiting",
        RunStatus::Blocked => "blocked",
        RunStatus::Paused => "paused",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Stopped => "stopped",
    }
}

const fn observer_health_value(value: ObserverHealth) -> &'static str {
    match value {
        ObserverHealth::Unknown => "unknown",
        ObserverHealth::Healthy => "healthy",
        ObserverHealth::Degraded => "degraded",
    }
}

const fn provider_notification_kind_value(value: ProviderNotificationKind) -> &'static str {
    match value {
        ProviderNotificationKind::PermissionPrompt => "permission_prompt",
        ProviderNotificationKind::ElicitationDialog => "elicitation_dialog",
        ProviderNotificationKind::ElicitationUrlDialog => "elicitation_url_dialog",
        ProviderNotificationKind::AgentNeedsInput => "agent_needs_input",
        ProviderNotificationKind::IdlePrompt => "idle_prompt",
        ProviderNotificationKind::AuthSuccess => "auth_success",
        ProviderNotificationKind::ElicitationComplete => "elicitation_complete",
        ProviderNotificationKind::ElicitationResponse => "elicitation_response",
        ProviderNotificationKind::AgentCompleted => "agent_completed",
    }
}

const fn failure_reason_value(value: RunFailureReason) -> &'static str {
    match value {
        RunFailureReason::Protocol => "protocol",
        RunFailureReason::Provider => "provider",
        RunFailureReason::Permission => "permission",
        RunFailureReason::Limit => "limit",
        RunFailureReason::Process => "process",
        RunFailureReason::Spawn => "spawn",
        RunFailureReason::Incomplete => "incomplete",
        RunFailureReason::Unverifiable => "unverifiable",
    }
}

const fn task_status_value(value: TaskStatus) -> &'static str {
    match value {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Succeeded => "succeeded",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

const fn session_state_value(value: SessionState) -> &'static str {
    match value {
        SessionState::Starting => "starting",
        SessionState::Idle => "idle",
        SessionState::Working => "working",
        SessionState::WaitingForInput => "waiting_for_input",
        SessionState::Stopped => "stopped",
        SessionState::Failed => "failed",
    }
}

const fn run_closed_by_value(value: RunClosedBy) -> &'static str {
    match value {
        RunClosedBy::TaskDone => "task_done",
        RunClosedBy::TaskBlocked => "task_blocked",
        RunClosedBy::OperatorCancel => "operator_cancel",
        RunClosedBy::OperatorStop => "operator_stop",
        RunClosedBy::SessionEnded => "session_ended",
    }
}

const fn provider_hook_event_value(value: ProviderHookEvent) -> &'static str {
    match value {
        ProviderHookEvent::SessionStart => "session_start",
        ProviderHookEvent::UserPromptSubmit => "user_prompt_submit",
        ProviderHookEvent::PreToolUse => "pre_tool_use",
        ProviderHookEvent::PermissionRequest => "permission_request",
        ProviderHookEvent::PostToolUse => "post_tool_use",
        ProviderHookEvent::Notification => "notification",
        ProviderHookEvent::Stop => "stop",
        ProviderHookEvent::SubagentStop => "subagent_stop",
        ProviderHookEvent::SessionEnd => "session_end",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_mode_defaults_on_and_changes_with_an_audit_event() {
        let mut store = Store::open_in_memory().unwrap();
        assert!(store.auto_mode().unwrap());
        let event = store.set_auto_mode(false, 42).unwrap();
        assert!(!store.auto_mode().unwrap());
        assert_eq!(
            event.event,
            FactoryEvent::AutoModeChanged { enabled: false }
        );
        assert_eq!(store.events_after(0, 10).unwrap(), vec![event]);
    }

    #[test]
    fn rejects_a_newer_schema() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();

        let error = migrate(&mut connection).unwrap_err();
        assert!(matches!(
            error,
            StoreError::UnsupportedSchema {
                found: 99,
                supported: SCHEMA_VERSION
            }
        ));
    }

    #[test]
    fn rejects_a_negative_schema() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection.pragma_update(None, "user_version", -1).unwrap();

        let error = migrate(&mut connection).unwrap_err();
        assert!(matches!(error, StoreError::InvalidSchemaVersion(-1)));
    }

    #[test]
    fn terminal_audit_state_needs_the_exact_uncertain_authority() {
        let lease = DeliveryLease {
            attempt_id: "attempt-1".into(),
            task: Some(TaskWork {
                task_id: TaskId::try_from("task-1").unwrap(),
                incarnation_id: "incarnation-1".into(),
                revision: 7,
                run_id: RunId::try_from("11111111-1111-4111-8111-111111111111").unwrap(),
            }),
        };
        let attempt = DeliveryAttempt {
            id: "attempt-1".into(),
            project_id: ProjectId::try_from("factory").unwrap(),
            agent_id: AgentId::try_from("curie").unwrap(),
            session_id: SessionId::try_from("22222222-2222-4222-8222-222222222222").unwrap(),
            task_id: Some(TaskId::try_from("task-1").unwrap()),
            task_incarnation_id: Some("incarnation-1".into()),
            task_revision: Some(7),
            run_id: Some(RunId::try_from("11111111-1111-4111-8111-111111111111").unwrap()),
            message_ids: Vec::new(),
            text: "exact prompt".into(),
            failure_count: u32::try_from(MAX_DELIVERY_ATTEMPT_FAILURES).unwrap(),
            next_attempt_at_ms: None,
            state: DeliveryAttemptState::Terminal,
        };
        let delivering = SessionWork {
            revision: 1,
            phase: SessionWorkPhase::Delivering(lease.clone()),
        };
        assert!(matches!(
            prepare_delivery_acknowledgement(&delivering, &attempt),
            Err(StoreError::SessionWorkConflict)
        ));

        let wrong_uncertain = SessionWork {
            revision: 2,
            phase: SessionWorkPhase::Uncertain(DeliveryLease {
                attempt_id: "other-attempt".into(),
                task: lease.task.clone(),
            }),
        };
        assert!(matches!(
            prepare_delivery_acknowledgement(&wrong_uncertain, &attempt),
            Err(StoreError::SessionWorkConflict)
        ));

        let exact_uncertain = SessionWork {
            revision: 2,
            phase: SessionWorkPhase::Uncertain(lease),
        };
        assert_eq!(
            prepare_delivery_acknowledgement(&exact_uncertain, &attempt)
                .unwrap()
                .1,
            "terminal"
        );
    }

    #[test]
    fn delivery_attempt_survives_restart_and_requires_explicit_resume() {
        let mut store = Store::open_in_memory().unwrap();
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("curie").unwrap();
        let session_id = SessionId::try_from("11111111-1111-4111-8111-111111111111").unwrap();
        let task_id = TaskId::try_from("task-1").unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Factory".to_owned(),
                    root: "/tmp/factory".to_owned(),
                },
                1,
            )
            .unwrap();
        store
            .create_agent(
                NewAgent {
                    id: agent_id.clone(),
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Shell,
                },
                1,
            )
            .unwrap();
        store
            .create_session(
                NewSession {
                    id: session_id.clone(),
                    project_id: project_id.clone(),
                    agent_id: agent_id.clone(),
                    provider: Provider::Shell,
                    runtime_model: None,
                    runtime_reasoning_effort: None,
                    runtime_permission_mode: None,
                    runtime_control_mode: None,
                    provider_session_id: None,
                    worktree: "/tmp/factory".to_owned(),
                    codex_home: None,
                    hook_token: "a".repeat(64),
                    runner_instance_id: RunnerInstanceId::try_from(
                        "22222222-2222-4222-8222-222222222222",
                    )
                    .unwrap(),
                    runner_runtime: "/tmp/factory-runner".to_owned(),
                    runner_protocol_version: 1,
                },
                1,
            )
            .unwrap();
        store
            .record_hook_event(
                &session_id,
                ProviderHookEvent::SessionStart,
                None,
                false,
                None,
                2,
            )
            .unwrap();
        store
            .create_task(
                NewTask {
                    id: task_id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "First".to_owned(),
                    body: "Body".to_owned(),
                    priority: 0,
                },
                3,
            )
            .unwrap();
        store
            .assign_task(&project_id, &task_id, Some(&agent_id), 4)
            .unwrap();
        let marker = store.task_delivery_marker(&task_id).unwrap();
        let input = NewDeliveryAttempt {
            id: "attempt-1".to_owned(),
            project_id: project_id.clone(),
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            task_id: Some(task_id.clone()),
            task_incarnation_id: Some(marker.incarnation_id),
            task_revision: Some(marker.task_revision),
            require_queue_head: false,
            message_ids: Vec::new(),
            text: "exact prompt".to_owned(),
            created_at_ms: 5,
        };
        let attempt = store.ensure_delivery_attempt(input.clone()).unwrap();
        assert_eq!(attempt.state, DeliveryAttemptState::InFlight);

        // The daemon's first restart consumes the in-flight attempt as one
        // failure. Reopening the same durable row cannot reset that count.
        store.recover_delivery_attempts(100).unwrap();
        assert_eq!(
            store.delivery_attempt_state("attempt-1").unwrap(),
            Some(DeliveryAttemptState::Retryable)
        );
        let recovered = store
            .delivery_attempt_for_session(&project_id, &agent_id, &session_id)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.failure_count, 1);
        assert_eq!(recovered.state, DeliveryAttemptState::Retryable);
        assert_eq!(recovered.next_attempt_at_ms, Some(5_100));
        assert_eq!(
            store.ensure_delivery_attempt(input).unwrap().failure_count,
            1
        );

        // A retry must be claimed durably before its first PTY write. From
        // that boundary onward the effect may have happened, so neither a
        // restart nor an operator resume may make the journal replayable.
        let claimed = store.begin_delivery_attempt("attempt-1", 5_100).unwrap();
        assert_eq!(
            claimed.as_ref().map(|a| a.state),
            Some(DeliveryAttemptState::InFlight)
        );
        store.recover_delivery_attempts(5_200).unwrap();
        let terminal = store
            .delivery_attempt_for_session(&project_id, &agent_id, &session_id)
            .unwrap()
            .unwrap();
        assert_eq!(terminal.failure_count, 2);
        assert_eq!(terminal.state, DeliveryAttemptState::Terminal);
        assert!(
            store
                .begin_delivery_attempt("attempt-1", 5_300)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            store.reset_delivery_attempt(&project_id, &agent_id, 7_000),
            Err(StoreError::SessionWorkConflict)
        ));
        assert!(matches!(
            store.session_work(&session_id).unwrap().work.unwrap().phase,
            SessionWorkPhase::Uncertain(lease) if lease.attempt_id == "attempt-1"
        ));
        assert!(
            !store
                .delivery_attempt_due(&project_id, &agent_id, &session_id, 7_000)
                .unwrap()
        );
        assert!(
            store
                .mark_session_waiting_for_delivery(
                    &session_id,
                    "attempt-1",
                    "delivery unacknowledged".into(),
                    7_001,
                )
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store
                .session_snapshot(&project_id, &session_id)
                .unwrap()
                .state,
            SessionState::WaitingForInput
        );

        // Stop intent is durable and cancels the same attempt before any
        // later run admission can win the race.
        store
            .request_session_stop(&project_id, &session_id, 8_000)
            .unwrap();
        assert_eq!(
            store.delivery_attempt_state("attempt-1").unwrap(),
            Some(DeliveryAttemptState::Cancelled)
        );
        assert!(matches!(
            store.open_run_episode(&session_id, &task_id, 8_001),
            Err(StoreError::SessionStopping)
        ));
        store
            .end_session(&session_id, Some(0), None, 8_002)
            .unwrap();
        assert!(matches!(
            store.session_work(&session_id).unwrap().work.unwrap().phase,
            SessionWorkPhase::Empty
        ));
    }
}
