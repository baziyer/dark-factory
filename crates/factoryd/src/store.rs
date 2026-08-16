use std::{
    path::{Component, Path},
    time::Duration,
};

use factory_core::{
    AgentId, AgentRole, AgentSnapshot, EventEnvelope, FactoryEvent, ObserverHealth,
    PROTOCOL_VERSION, ProjectId, ProjectSnapshot, Provider, RunFailureReason, RunId, RunSnapshot,
    RunStatus, RunnerInstanceId, TaskDetail, TaskId, TaskSnapshot, TaskStatus,
    runner::{RUNNER_PROTOCOL_VERSION, RunnerEvent, RunnerEventEnvelope},
};
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, types::Type,
};
use thiserror::Error;
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 7;
const MAX_EVENT_PAGE: usize = 10_000;
const MAX_STATE_PAGE: usize = 101;
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
const MAX_TERMINAL_RESULT_BYTES: usize = 4 * 1024;
pub const MAX_RUNNER_BATCH_EVENTS: usize = 64;

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
    pub subscription_usage: SubscriptionUsageSnapshot,
}

/// Deterministic subscription headroom band. This is intentionally independent
/// of imported per-run token and dollar receipts.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SubscriptionSeverity {
    Ok,
    Warning,
    Critical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionFailureCategory {
    Timeout,
    Protocol,
    Process,
    OutputLimit,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionLimitWindow {
    Primary,
    Secondary,
    CurrentSession,
    CurrentWeek,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionProbeOutcome {
    Observed {
        used_percent: u8,
        limit_window: SubscriptionLimitWindow,
        resets_at_ms: Option<i64>,
        exhausted: bool,
    },
    Failed {
        category: SubscriptionFailureCategory,
    },
}

pub struct SubscriptionProbe {
    pub project_id: ProjectId,
    pub orchestrator_agent_id: AgentId,
    pub provider: Provider,
    pub attempted_at_ms: i64,
    pub outcome: SubscriptionProbeOutcome,
    pub notification_task_id: TaskId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionProviderState {
    pub provider: Provider,
    pub last_attempt_at_ms: i64,
    pub last_success_at_ms: Option<i64>,
    pub used_percent: Option<u8>,
    pub limit_window: Option<SubscriptionLimitWindow>,
    pub resets_at_ms: Option<i64>,
    pub exhausted: Option<bool>,
    pub severity: SubscriptionSeverity,
    pub consecutive_failures: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionUsageSnapshot {
    pub overall_severity: SubscriptionSeverity,
    pub providers: Vec<SubscriptionProviderState>,
}

pub struct SubscriptionProbeCommit {
    pub state: SubscriptionProviderState,
    pub notification_created: bool,
    pub events: Vec<EventEnvelope>,
}

pub struct WebhookSnapshotTask {
    pub id: TaskId,
    pub title: String,
    pub status: OperationalTaskStatus,
    pub assignee: Option<String>,
    pub depends_on: Vec<TaskId>,
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

/// Exact private provider context imported with an existing agent.
///
/// This deliberately has no `Debug` or `Clone` implementation because all
/// fields are private execution metadata.
pub enum AdoptedProviderSession {
    ClaudeCode {
        session_id: String,
        cwd: String,
    },
    Codex {
        thread_id: String,
        cwd: String,
        codex_home: Option<String>,
    },
}

/// The minimal private pre-reservation identity required by execution.
///
/// The actual provider session identity remains inside the store until an
/// execution target has been atomically reserved. This type deliberately has
/// no `Debug` implementation.
pub struct AgentExecutionIdentity {
    pub provider: Provider,
    pub has_provider_session: bool,
}

/// Private launch metadata for reserving one explicit queued task.
///
/// This deliberately has no `Debug` or `Clone` implementation because it can
/// contain a provider session identity and private paths.
pub struct RunReservation {
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub expected_provider: Provider,
    pub run_id: RunId,
    pub parent_run_id: Option<RunId>,
    pub worktree: String,
    pub fresh_provider_session_id: Option<String>,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
}

pub struct ReservedRun {
    pub task: TaskSnapshot,
    pub agent: AgentSnapshot,
    pub run: RunSnapshot,
    pub target: ExecutionTarget,
    pub events: Vec<EventEnvelope>,
}

/// Privacy-sensitive inputs and durable replay position needed to supervise a run.
pub struct ExecutionTarget {
    pub provider: Provider,
    pub project_root: String,
    pub task_body: String,
    pub worktree: String,
    pub provider_session_id: Option<String>,
    pub codex_home: Option<String>,
    pub resumes_provider_session: bool,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_protocol_version: u16,
    pub runner_runtime: String,
    /// Highest runner event whose database effects are committed.
    ///
    /// Recovery must replay the runner spool from sequence zero into a fresh
    /// provider decoder, suppressing database mutations through this cursor.
    /// This is never a runner subscription cursor because decoder state is
    /// deliberately transient.
    pub last_committed_runner_sequence: i64,
}

pub struct RecoverableRun {
    pub run: RunSnapshot,
    pub target: ExecutionTarget,
    pub provider_session_confirmed_at_ms: Option<i64>,
    pub terminal_runner_sequence: Option<i64>,
    pub runner_reconciled_at_ms: Option<i64>,
}

/// Minimal private identity required to resume observing a durable runner.
///
/// This deliberately omits task bodies and has no `Debug` or `Clone`
/// implementation so daemon startup does not copy queued instructions for
/// every recoverable run.
pub struct RecoverableExecution {
    pub run_id: RunId,
    pub provider: Provider,
    pub provider_session_id: Option<String>,
    pub resumes_provider_session: bool,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
    pub terminal_runner_sequence: Option<i64>,
    pub observer_health: ObserverHealth,
}

/// Effects already normalized from exactly one runner event.
///
/// This deliberately has no `Debug` implementation because provider session
/// identities stay out of logs and public events.
pub struct RunnerEventEffects {
    pub confirmed_provider_session_id: Option<String>,
    pub terminal_outcome: Option<TerminalOutcome>,
}

/// One durable runner event and its already-normalized private effects.
///
/// This deliberately has no `Debug` or `Clone` implementation because output
/// events and provider session identities must not enter logs accidentally.
pub struct RunnerEventInput {
    pub event: RunnerEventEnvelope,
    pub effects: RunnerEventEffects,
}

#[derive(Clone, Eq, PartialEq)]
pub enum TerminalOutcome {
    Succeeded { result: Option<String> },
    Failed(RunFailureReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngestDisposition {
    Recorded,
    Duplicate,
}

pub struct IngestResult {
    pub disposition: IngestDisposition,
    pub events: Vec<EventEnvelope>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteDisposition {
    Applied,
    Duplicate,
}

pub struct ExecutionTransition {
    pub disposition: WriteDisposition,
    pub task: TaskSnapshot,
    pub agent: AgentSnapshot,
    pub run: RunSnapshot,
    pub events: Vec<EventEnvelope>,
}

pub struct ObserverHealthTransition {
    pub disposition: WriteDisposition,
    pub run: RunSnapshot,
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
    #[error("active-run limit must be greater than zero")]
    InvalidConcurrencyLimit,
    #[error("active-run capacity {limit} has been reached")]
    CapacityReached { limit: usize },
    #[error("agent was not found in the requested project")]
    AgentNotFound,
    #[error("task was not found in the requested project")]
    TaskNotFound,
    #[error("agent provider does not match the requested execution provider")]
    AgentProviderMismatch,
    #[error("task is not queued in the requested project")]
    TaskNotQueued,
    #[error("only an idle same-project agent can reserve a task")]
    AgentUnavailable,
    #[error("parent run does not match the agent parent and project")]
    ParentRunLineageMismatch,
    #[error("run was not found")]
    RunNotFound,
    #[error("runner identity does not match the reserved run")]
    RunnerIdentityMismatch,
    #[error("runner protocol mismatch: expected {expected}, found {found}")]
    RunnerProtocolMismatch { expected: u16, found: u16 },
    #[error("runner sequence {0} must be positive")]
    InvalidRunnerSequence(i64),
    #[error("runner event batch must contain 1-{MAX_RUNNER_BATCH_EVENTS} events")]
    InvalidRunnerBatchSize,
    #[error("durable runner sequence {0} cannot advance")]
    CorruptRunnerSequence(i64),
    #[error("runner event gap: expected sequence {expected}, found {found}")]
    RunnerSequenceGap { expected: i64, found: i64 },
    #[error("runner has already reached its terminal event")]
    RunnerAlreadyTerminal,
    #[error("runner event is invalid for the current lifecycle state")]
    InvalidRunnerLifecycle,
    #[error("provider session confirmation is only valid on provider stdout")]
    InvalidSessionConfirmation,
    #[error("provider session identity conflicts with durable ownership")]
    ProviderSessionConflict,
    #[error("adopted provider session does not match the agent provider")]
    InvalidProviderSessionAdoption,
    #[error("reserved worktree does not match the adopted provider session")]
    ProviderSessionCwdMismatch,
    #[error("terminal runner events require a normalized outcome")]
    TerminalOutcomeRequired,
    #[error("non-terminal runner events cannot carry a terminal outcome")]
    UnexpectedTerminalOutcome,
    #[error("normalized outcome conflicts with the terminal runner event")]
    InvalidTerminalOutcome,
    #[error("terminal sequence mismatch: expected {expected}, found {found}")]
    TerminalSequenceMismatch { expected: i64, found: i64 },
    #[error("run is not in the required state")]
    InvalidRunState,
    #[error("private execution metadata is empty, relative, or too large")]
    InvalidExecutionMetadata,
    #[error("webhook input is invalid")]
    InvalidWebhookInput,
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
    #[error("subscription usage input is invalid")]
    InvalidSubscriptionProbe,
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut connection: Connection) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;",
        )?;
        migrate(&mut connection)?;
        Ok(Self { connection })
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
        let record = TaskDetail {
            snapshot: TaskSnapshot {
                id: input.id,
                project_id: input.project_id,
                parent_task_id: input.parent_task_id,
                depends_on: Vec::new(),
                assigned_agent_id: None,
                title: input.title,
                status: TaskStatus::Queued,
                priority: input.priority,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
            },
            body: input.body,
            result: None,
        };
        let event = FactoryEvent::TaskChanged {
            task: record.snapshot.clone(),
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        transaction.execute(
            "INSERT INTO tasks (
                id, project_id, parent_task_id, assigned_agent_id, title, body,
                status, priority, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, 'queued', ?6, ?7, ?8)",
            params![
                record.snapshot.id.as_str(),
                record.snapshot.project_id.as_str(),
                record.snapshot.parent_task_id.as_ref().map(TaskId::as_str),
                record.snapshot.title,
                record.body,
                record.snapshot.priority,
                record.snapshot.created_at_ms,
                record.snapshot.updated_at_ms
            ],
        )?;
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
        self.insert_agent(input, None, now_ms)
    }

    /// Atomically creates an agent already bound to one exact provider
    /// session. Private provider metadata is stored only on the agent row and
    /// is intentionally absent from the returned public snapshot and event.
    pub fn adopt_agent(
        &mut self,
        input: NewAgent,
        session: AdoptedProviderSession,
        now_ms: i64,
    ) -> Result<(AgentSnapshot, EventEnvelope)> {
        let session = adopted_session_context(session)?;
        if input.provider != session.provider {
            return Err(StoreError::InvalidProviderSessionAdoption);
        }
        self.insert_agent(input, Some(session), now_ms)
    }

    fn insert_agent(
        &mut self,
        input: NewAgent,
        session: Option<AgentSessionContext>,
        now_ms: i64,
    ) -> Result<(AgentSnapshot, EventEnvelope)> {
        let agent = AgentSnapshot {
            id: input.id,
            project_id: input.project_id,
            parent_agent_id: input.parent_agent_id,
            role: input.role,
            provider: input.provider,
            current_run_id: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let event = FactoryEvent::AgentChanged {
            agent: agent.clone(),
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(session) = session.as_ref() {
            let already_owned: bool = transaction.query_row(
                "SELECT EXISTS (
                    SELECT 1 FROM agents
                    WHERE provider = ?1 AND provider_session_id = ?2
                 )",
                params![provider_value(session.provider), session.session_id],
                |row| row.get(0),
            )?;
            if already_owned {
                return Err(StoreError::ProviderSessionConflict);
            }
        }
        transaction.execute(
            "INSERT INTO agents (
                id, project_id, parent_agent_id, role, provider,
                provider_session_id, provider_session_cwd, codex_home,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                agent.id.as_str(),
                agent.project_id.as_str(),
                agent.parent_agent_id.as_ref().map(AgentId::as_str),
                agent_role_value(agent.role),
                provider_value(agent.provider),
                session.as_ref().map(|session| session.session_id.as_str()),
                session.as_ref().map(|session| session.cwd.as_str()),
                session
                    .as_ref()
                    .and_then(|session| session.codex_home.as_deref()),
                agent.created_at_ms,
                agent.updated_at_ms,
            ],
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

    pub fn agent_execution_identity(
        &self,
        project_id: &ProjectId,
        agent_id: &AgentId,
    ) -> Result<AgentExecutionIdentity> {
        let agent = load_agent(&self.connection, agent_id)?
            .filter(|agent| &agent.snapshot.project_id == project_id)
            .ok_or(StoreError::AgentNotFound)?;
        Ok(AgentExecutionIdentity {
            provider: agent.snapshot.provider,
            has_provider_session: agent.provider_session_id.is_some(),
        })
    }

    /// Atomically assigns one explicit queued task to one idle agent.
    ///
    /// `max_active_runs` is a transactionally checked factory-wide capacity,
    /// not a scheduler. Queue ordering remains outside this store slice.
    pub fn reserve_task_run(
        &mut self,
        input: RunReservation,
        max_active_runs: usize,
        now_ms: i64,
    ) -> Result<ReservedRun> {
        if max_active_runs == 0 {
            return Err(StoreError::InvalidConcurrencyLimit);
        }
        validate_absolute_path(&input.worktree)?;
        validate_absolute_path(&input.runner_runtime)?;
        validate_provider_session(input.fresh_provider_session_id.as_deref())?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM runs WHERE ended_at_ms IS NULL",
            [],
            |row| row.get(0),
        )?;
        if active >= i64::try_from(max_active_runs).unwrap_or(i64::MAX) {
            return Err(StoreError::CapacityReached {
                limit: max_active_runs,
            });
        }

        let mut task = load_task(&transaction, &input.task_id)?
            .filter(|task| task.snapshot.project_id == input.project_id)
            .filter(|task| task.snapshot.status == TaskStatus::Queued)
            .ok_or(StoreError::TaskNotQueued)?;
        let agent_record = load_agent(&transaction, &input.agent_id)?
            .filter(|agent| agent.snapshot.project_id == input.project_id)
            .ok_or(StoreError::AgentNotFound)?;
        if agent_record.snapshot.provider != input.expected_provider {
            return Err(StoreError::AgentProviderMismatch);
        }
        if agent_record.snapshot.current_run_id.is_some() {
            return Err(StoreError::AgentUnavailable);
        }
        validate_parent_run(
            &transaction,
            &input.project_id,
            agent_record.snapshot.parent_agent_id.as_ref(),
            input.parent_run_id.as_ref(),
        )?;

        let (provider_session_id, resumes_provider_session) = match agent_record.provider_session_id
        {
            Some(established) => {
                if input.fresh_provider_session_id.is_some() {
                    return Err(StoreError::ProviderSessionConflict);
                }
                let established_cwd = agent_record
                    .provider_session_cwd
                    .as_deref()
                    .ok_or(StoreError::InvalidExecutionMetadata)?;
                if established_cwd != input.worktree {
                    return Err(StoreError::ProviderSessionCwdMismatch);
                }
                (Some(established), true)
            }
            None => {
                if agent_record.provider_session_cwd.is_some() || agent_record.codex_home.is_some()
                {
                    return Err(StoreError::InvalidExecutionMetadata);
                }
                (input.fresh_provider_session_id, false)
            }
        };
        transaction.execute(
            "INSERT INTO runs (
                id, project_id, agent_id, parent_run_id, task_id, status,
                activity, wait_reason, worktree, provider_session_id,
                resumes_provider_session, provider_session_confirmed_at_ms,
                runner_instance_id, runner_protocol_version, runner_runtime,
                last_runner_sequence, terminal_runner_sequence,
                runner_reconciled_at_ms, runner_terminal_kind,
                observer_health, observer_health_since_ms,
                started_at_ms, status_since_ms, updated_at_ms, ended_at_ms,
                exit_code, exit_signal, failure_reason
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, 'starting', NULL, NULL, ?6, ?7, ?8,
                NULL, ?9, ?10, ?11, 0, NULL, NULL, NULL, 'unknown', ?12,
                ?13, ?14, ?15,
                NULL, NULL, NULL, NULL
             )",
            params![
                input.run_id.as_str(),
                input.project_id.as_str(),
                input.agent_id.as_str(),
                input.parent_run_id.as_ref().map(RunId::as_str),
                input.task_id.as_str(),
                input.worktree,
                provider_session_id,
                resumes_provider_session,
                input.runner_instance_id.as_str(),
                i64::from(RUNNER_PROTOCOL_VERSION),
                input.runner_runtime,
                now_ms,
                now_ms,
                now_ms,
                now_ms,
            ],
        )?;
        let assigned = transaction.execute(
            "UPDATE tasks
             SET assigned_agent_id = ?1, status = 'running', updated_at_ms = ?2,
                 started_at_ms = COALESCE(started_at_ms, ?2)
             WHERE id = ?3 AND project_id = ?4 AND status = 'queued'",
            params![
                input.agent_id.as_str(),
                now_ms,
                input.task_id.as_str(),
                input.project_id.as_str(),
            ],
        )?;
        if assigned != 1 {
            return Err(StoreError::TaskNotQueued);
        }
        transaction.execute(
            "UPDATE agents SET updated_at_ms = ?1 WHERE id = ?2",
            params![now_ms, input.agent_id.as_str()],
        )?;

        task.snapshot.assigned_agent_id = Some(input.agent_id.clone());
        task.snapshot.status = TaskStatus::Running;
        task.snapshot.updated_at_ms = now_ms;
        let agent = load_agent(&transaction, &input.agent_id)?
            .ok_or(StoreError::AgentNotFound)?
            .snapshot;
        let run = load_run(&transaction, &input.run_id)?.ok_or(StoreError::RunNotFound)?;
        let events = append_execution_events(&transaction, now_ms, &task.snapshot, &agent, &run)?;
        let target =
            load_execution_target(&transaction, &input.run_id)?.ok_or(StoreError::RunNotFound)?;
        transaction.commit()?;

        Ok(ReservedRun {
            task: task.snapshot,
            agent,
            run,
            target,
            events,
        })
    }

    pub fn execution_target(&self, run_id: &RunId) -> Result<ExecutionTarget> {
        load_execution_target(&self.connection, run_id)?.ok_or(StoreError::RunNotFound)
    }

    pub fn ingest_runner_event(
        &mut self,
        run_id: &RunId,
        runner_instance_id: &RunnerInstanceId,
        event: &RunnerEventEnvelope,
        effects: RunnerEventEffects,
        now_ms: i64,
    ) -> Result<IngestResult> {
        self.ingest_runner_events(
            run_id,
            runner_instance_id,
            vec![RunnerEventInput {
                event: event.clone(),
                effects,
            }],
            now_ms,
        )
    }

    /// Commits a bounded contiguous runner replay in one synchronous write.
    pub fn ingest_runner_events(
        &mut self,
        run_id: &RunId,
        runner_instance_id: &RunnerInstanceId,
        items: Vec<RunnerEventInput>,
        now_ms: i64,
    ) -> Result<IngestResult> {
        if !(1..=MAX_RUNNER_BATCH_EVENTS).contains(&items.len()) {
            return Err(StoreError::InvalidRunnerBatchSize);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut any_recorded = false;
        let mut events = Vec::new();
        let mut previous_sequence: Option<i64> = None;
        for input in items {
            if let Some(previous) = previous_sequence {
                let expected = previous
                    .checked_add(1)
                    .ok_or(StoreError::InvalidRunnerSequence(input.event.sequence))?;
                if input.event.sequence != expected {
                    return Err(StoreError::RunnerSequenceGap {
                        expected,
                        found: input.event.sequence,
                    });
                }
            }
            previous_sequence = Some(input.event.sequence);
            let result = ingest_runner_event_in_transaction(
                &transaction,
                run_id,
                runner_instance_id,
                &input.event,
                &input.effects,
                now_ms,
            )?;
            if result.disposition == IngestDisposition::Recorded {
                any_recorded = true;
            }
            events.extend(result.events);
        }
        transaction.commit()?;
        Ok(IngestResult {
            disposition: if any_recorded {
                IngestDisposition::Recorded
            } else {
                IngestDisposition::Duplicate
            },
            events,
        })
    }

    pub fn fail_run_launch(
        &mut self,
        run_id: &RunId,
        runner_instance_id: &RunnerInstanceId,
        now_ms: i64,
    ) -> Result<ExecutionTransition> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ledger = load_run_ledger(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if &ledger.runner_instance_id != runner_instance_id {
            return Err(StoreError::RunnerIdentityMismatch);
        }
        if ledger.snapshot.status == RunStatus::Failed
            && ledger.snapshot.failure_reason == Some(RunFailureReason::Spawn)
            && ledger.terminal_runner_sequence.is_none()
        {
            return duplicate_failure_transition(&transaction, &ledger);
        }
        if ledger.snapshot.status != RunStatus::Starting
            || ledger.last_runner_sequence != 0
            || ledger.terminal_runner_sequence.is_some()
        {
            return Err(StoreError::InvalidRunState);
        }
        let transition =
            fail_run_in_transaction(&transaction, &ledger, RunFailureReason::Spawn, now_ms)?;
        transaction.commit()?;
        Ok(transition)
    }

    pub fn fail_run_unverifiable(
        &mut self,
        run_id: &RunId,
        runner_instance_id: &RunnerInstanceId,
        now_ms: i64,
    ) -> Result<ExecutionTransition> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ledger = load_run_ledger(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if &ledger.runner_instance_id != runner_instance_id {
            return Err(StoreError::RunnerIdentityMismatch);
        }
        if ledger.snapshot.status == RunStatus::Failed
            && ledger.snapshot.failure_reason == Some(RunFailureReason::Unverifiable)
            && ledger.terminal_runner_sequence.is_none()
        {
            return duplicate_failure_transition(&transaction, &ledger);
        }
        if ledger.snapshot.status.is_terminal() || ledger.terminal_runner_sequence.is_some() {
            return Err(StoreError::InvalidRunState);
        }
        let transition = fail_run_in_transaction(
            &transaction,
            &ledger,
            RunFailureReason::Unverifiable,
            now_ms,
        )?;
        transaction.commit()?;
        Ok(transition)
    }

    /// Records that terminal runner cleanup is reconciled.
    ///
    /// Callers may use this after the exact acknowledgement was received or
    /// after the exact runner endpoint was proven absent.
    pub fn mark_runner_terminal_reconciled(
        &mut self,
        run_id: &RunId,
        runner_instance_id: &RunnerInstanceId,
        terminal_sequence: i64,
        now_ms: i64,
    ) -> Result<WriteDisposition> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ledger = load_run_ledger(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if &ledger.runner_instance_id != runner_instance_id {
            return Err(StoreError::RunnerIdentityMismatch);
        }
        let expected = ledger
            .terminal_runner_sequence
            .ok_or(StoreError::InvalidRunState)?;
        if terminal_sequence != expected {
            return Err(StoreError::TerminalSequenceMismatch {
                expected,
                found: terminal_sequence,
            });
        }
        if ledger.runner_reconciled_at_ms.is_some() {
            return Ok(WriteDisposition::Duplicate);
        }
        transaction.execute(
            "UPDATE runs SET runner_reconciled_at_ms = ?1 WHERE id = ?2",
            params![now_ms, run_id.as_str()],
        )?;
        transaction.commit()?;
        Ok(WriteDisposition::Applied)
    }

    /// Changes the durable supervision state for one exact runner identity.
    ///
    /// Health is independent of run lifecycle, so terminal runs awaiting
    /// reconciliation may also become degraded or healthy.
    pub fn set_observer_health(
        &mut self,
        run_id: &RunId,
        runner_instance_id: &RunnerInstanceId,
        observer_health: ObserverHealth,
        now_ms: i64,
    ) -> Result<ObserverHealthTransition> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ledger = load_run_ledger(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if &ledger.runner_instance_id != runner_instance_id {
            return Err(StoreError::RunnerIdentityMismatch);
        }
        if ledger.snapshot.observer_health == observer_health {
            return Ok(ObserverHealthTransition {
                disposition: WriteDisposition::Duplicate,
                run: ledger.snapshot,
                events: Vec::new(),
            });
        }

        transaction.execute(
            "UPDATE runs
             SET observer_health = ?1, observer_health_since_ms = ?2,
                 updated_at_ms = ?2
             WHERE id = ?3 AND runner_instance_id = ?4",
            params![
                observer_health_value(observer_health),
                now_ms,
                run_id.as_str(),
                runner_instance_id.as_str(),
            ],
        )?;
        let run = load_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        let event = FactoryEvent::RunChanged { run: run.clone() };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;

        Ok(ObserverHealthTransition {
            disposition: WriteDisposition::Applied,
            run,
            events: vec![EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            }],
        })
    }

    pub fn recoverable_runs(&self) -> Result<Vec<RecoverableRun>> {
        let mut statement = self.connection.prepare(
            "SELECT id, provider_session_confirmed_at_ms,
                    terminal_runner_sequence, runner_reconciled_at_ms
             FROM runs
             WHERE ended_at_ms IS NULL
                OR (terminal_runner_sequence IS NOT NULL AND runner_reconciled_at_ms IS NULL)
             ORDER BY project_id, started_at_ms, id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;
        let rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        rows.into_iter()
            .map(|(run_id, confirmed, terminal, reconciled)| {
                let run_id: RunId = RunId::try_from(run_id).map_err(|error| {
                    StoreError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                        0,
                        Type::Text,
                        Box::new(error),
                    ))
                })?;
                Ok(RecoverableRun {
                    run: load_run(&self.connection, &run_id)?.ok_or(StoreError::RunNotFound)?,
                    target: load_execution_target(&self.connection, &run_id)?
                        .ok_or(StoreError::RunNotFound)?,
                    provider_session_confirmed_at_ms: confirmed,
                    terminal_runner_sequence: terminal,
                    runner_reconciled_at_ms: reconciled,
                })
            })
            .collect()
    }

    pub fn recoverable_executions(&self) -> Result<Vec<RecoverableExecution>> {
        let mut statement = self.connection.prepare(
            "SELECT r.id, a.provider, r.provider_session_id,
                    r.resumes_provider_session, r.runner_instance_id,
                    r.runner_runtime, r.terminal_runner_sequence,
                    r.observer_health
             FROM runs r
             JOIN agents a
               ON a.id = r.agent_id AND a.project_id = r.project_id
             WHERE r.ended_at_ms IS NULL
                OR (r.terminal_runner_sequence IS NOT NULL
                    AND r.runner_reconciled_at_ms IS NULL)
             ORDER BY r.project_id, r.started_at_ms, r.id",
        )?;
        let rows = statement.query_map([], |row| {
            let provider: String = row.get(1)?;
            let observer_health: String = row.get(7)?;
            Ok(RecoverableExecution {
                run_id: parse_id(row.get(0)?, 0)?,
                provider: parse_provider(&provider, 1)?,
                provider_session_id: row.get(2)?,
                resumes_provider_session: row.get(3)?,
                runner_instance_id: parse_id(row.get(4)?, 4)?,
                runner_runtime: row.get(5)?,
                terminal_runner_sequence: row.get(6)?,
                observer_health: parse_observer_health(&observer_health, 7)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Returns the bounded, public operational projection used by webhooks.
    /// Counts cover the full project; task rows are capped to 100 active and
    /// 12 most-recent terminal tasks.
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
                    (SELECT r.observer_health FROM runs r
                     WHERE r.agent_id = a.id
                     ORDER BY r.updated_at_ms DESC, r.id DESC LIMIT 1)
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
        let subscription_usage = self.subscription_usage_snapshot()?;

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
            subscription_usage,
        })
    }

    /// Records one bounded, normalized subscription-capacity probe. Raw CLI or
    /// protocol output is never accepted by this store boundary.
    pub fn record_subscription_probe(
        &mut self,
        input: SubscriptionProbe,
    ) -> Result<SubscriptionProbeCommit> {
        if input.attempted_at_ms < 0
            || matches!(
                input.outcome,
                SubscriptionProbeOutcome::Observed {
                    used_percent: 101..=u8::MAX,
                    ..
                }
            )
        {
            return Err(StoreError::InvalidSubscriptionProbe);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_webhook_project_and_orchestrator(
            &transaction,
            &input.project_id,
            &input.orchestrator_agent_id,
        )?;
        let provider = provider_value(input.provider);
        let replayed = transaction
            .query_row(
                "SELECT outcome, used_percent, limit_window, resets_at_ms, exhausted,
                    failure_category
             FROM subscription_usage_probes
             WHERE provider = ?1 AND attempted_at_ms = ?2",
                params![provider, input.attempted_at_ms],
                |row| {
                    Ok(StoredSubscriptionProbe {
                        outcome: row.get(0)?,
                        used_percent: row.get(1)?,
                        limit_window: row.get(2)?,
                        resets_at_ms: row.get(3)?,
                        exhausted: row.get(4)?,
                        failure_category: row.get(5)?,
                    })
                },
            )
            .optional()?;
        if let Some(replayed) = replayed {
            if !stored_subscription_probe_matches(&replayed, input.outcome) {
                return Err(StoreError::InvalidSubscriptionProbe);
            }
            let state = load_subscription_provider_state(&transaction, input.provider)?
                .ok_or(StoreError::InvalidSubscriptionProbe)?;
            transaction.commit()?;
            return Ok(SubscriptionProbeCommit {
                state,
                notification_created: false,
                events: Vec::new(),
            });
        }

        let prior = load_subscription_state_row(&transaction, input.provider)?;
        if prior
            .as_ref()
            .is_some_and(|state| input.attempted_at_ms < state.public.last_attempt_at_ms)
        {
            return Err(StoreError::InvalidSubscriptionProbe);
        }
        let previous_severity = prior
            .as_ref()
            .map_or(SubscriptionSeverity::Ok, |state| state.public.severity);
        let prior_capacity = prior
            .as_ref()
            .map_or(SubscriptionSeverity::Ok, |state| state.capacity_severity);
        let prior_failures = prior
            .as_ref()
            .map_or(0, |state| state.public.consecutive_failures);
        let prior_success = prior
            .as_ref()
            .and_then(|state| state.public.last_success_at_ms);
        let prior_percent = prior.as_ref().and_then(|state| state.public.used_percent);
        let prior_limit = prior.as_ref().and_then(|state| state.public.limit_window);
        let prior_reset = prior.as_ref().and_then(|state| state.public.resets_at_ms);
        let prior_exhausted = prior.as_ref().and_then(|state| state.public.exhausted);

        let (
            outcome_name,
            used_percent,
            limit_window,
            resets_at_ms,
            exhausted,
            capacity_severity,
            severity,
            failures,
            failure_category,
            last_success_at_ms,
        ) = match input.outcome {
            SubscriptionProbeOutcome::Observed {
                used_percent,
                limit_window,
                resets_at_ms,
                exhausted,
            } => {
                if resets_at_ms.is_some_and(|reset| reset < 0) {
                    return Err(StoreError::InvalidSubscriptionProbe);
                }
                let capacity = subscription_capacity_severity(used_percent, exhausted);
                (
                    "observed",
                    Some(used_percent),
                    Some(limit_window),
                    resets_at_ms,
                    Some(exhausted),
                    capacity,
                    capacity,
                    0,
                    None,
                    Some(input.attempted_at_ms),
                )
            }
            SubscriptionProbeOutcome::Failed { category } => {
                let failures = prior_failures.saturating_add(1);
                let visibility = if failures >= 3 {
                    SubscriptionSeverity::Warning
                } else {
                    SubscriptionSeverity::Ok
                };
                (
                    "failed",
                    prior_percent,
                    prior_limit,
                    prior_reset,
                    prior_exhausted,
                    prior_capacity,
                    prior_capacity.max(visibility),
                    failures,
                    Some(subscription_failure_value(category)),
                    prior_success,
                )
            }
        };
        let upward_transition = severity > previous_severity;

        transaction.execute(
            "INSERT INTO subscription_usage_probes (
                provider, attempted_at_ms, outcome, used_percent, limit_window,
                resets_at_ms, exhausted, severity, failure_category, notification_task_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
            params![
                provider,
                input.attempted_at_ms,
                outcome_name,
                if outcome_name == "observed" {
                    used_percent.map(i64::from)
                } else {
                    None
                },
                if outcome_name == "observed" {
                    limit_window.map(subscription_limit_window_value)
                } else {
                    None
                },
                if outcome_name == "observed" {
                    resets_at_ms
                } else {
                    None
                },
                if outcome_name == "observed" {
                    exhausted.map(i64::from)
                } else {
                    None
                },
                subscription_severity_value(severity),
                failure_category,
            ],
        )?;
        transaction.execute(
            "INSERT INTO subscription_usage_state (
                provider, last_attempt_at_ms, last_success_at_ms, used_percent, limit_window,
                resets_at_ms, exhausted, capacity_severity, severity,
                consecutive_failures
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(provider) DO UPDATE SET
                last_attempt_at_ms = excluded.last_attempt_at_ms,
                last_success_at_ms = excluded.last_success_at_ms,
                used_percent = excluded.used_percent,
                limit_window = excluded.limit_window,
                resets_at_ms = excluded.resets_at_ms,
                exhausted = excluded.exhausted,
                capacity_severity = excluded.capacity_severity,
                severity = excluded.severity,
                consecutive_failures = excluded.consecutive_failures",
            params![
                provider,
                input.attempted_at_ms,
                last_success_at_ms,
                used_percent.map(i64::from),
                limit_window.map(subscription_limit_window_value),
                resets_at_ms,
                exhausted.map(i64::from),
                subscription_severity_value(capacity_severity),
                subscription_severity_value(severity),
                i64::from(failures),
            ],
        )?;

        let mut events = Vec::new();
        if upward_transition {
            let notification_exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
                params![input.notification_task_id.as_str()],
                |row| row.get(0),
            )?;
            if notification_exists {
                return Err(StoreError::InvalidSubscriptionProbe);
            }
            let title = format!(
                "Subscription {}: {}",
                subscription_severity_value(severity),
                subscription_provider_title(input.provider)
            );
            let body = subscription_notification_advice(input.outcome, severity);
            transaction.execute(
                "INSERT INTO tasks (
                    id, project_id, parent_task_id, assigned_agent_id, title, body,
                    status, priority, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, 'queued', 10, ?6, ?6)",
                params![
                    input.notification_task_id.as_str(),
                    input.project_id.as_str(),
                    input.orchestrator_agent_id.as_str(),
                    title,
                    body,
                    input.attempted_at_ms,
                ],
            )?;
            let snapshot = TaskSnapshot {
                id: input.notification_task_id.clone(),
                project_id: input.project_id,
                parent_task_id: None,
                depends_on: Vec::new(),
                assigned_agent_id: Some(input.orchestrator_agent_id),
                title,
                status: TaskStatus::Queued,
                priority: 10,
                created_at_ms: input.attempted_at_ms,
                updated_at_ms: input.attempted_at_ms,
            };
            let event = FactoryEvent::TaskChanged { task: snapshot };
            let sequence = append_event(&transaction, input.attempted_at_ms, &event)?;
            events.push(EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: input.attempted_at_ms,
                event,
            });
            transaction.execute(
                "UPDATE subscription_usage_probes SET notification_task_id = ?3
                 WHERE provider = ?1 AND attempted_at_ms = ?2",
                params![
                    provider,
                    input.attempted_at_ms,
                    input.notification_task_id.as_str()
                ],
            )?;
        }
        let state = load_subscription_provider_state(&transaction, input.provider)?
            .ok_or(StoreError::InvalidSubscriptionProbe)?;
        transaction.commit()?;
        Ok(SubscriptionProbeCommit {
            state,
            notification_created: upward_transition,
            events,
        })
    }

    /// Public, provider-neutral projection of the latest normalized allowance
    /// state. It is intentionally independent from per-run billing receipts.
    pub fn subscription_usage_snapshot(&self) -> Result<SubscriptionUsageSnapshot> {
        let mut statement = self.connection.prepare(
            "SELECT provider, last_attempt_at_ms, last_success_at_ms, used_percent,
                    resets_at_ms, exhausted, severity, consecutive_failures, limit_window
             FROM subscription_usage_state ORDER BY provider",
        )?;
        let providers = statement
            .query_map([], parse_subscription_provider_state)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let overall_severity = providers
            .iter()
            .map(|state| state.severity)
            .max()
            .unwrap_or(SubscriptionSeverity::Ok);
        Ok(SubscriptionUsageSnapshot {
            overall_severity,
            providers,
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
            depends_on: Vec::new(),
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
            depends_on: Vec::new(),
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
        if !(1..=MAX_STATE_PAGE).contains(&limit) {
            return Err(StoreError::InvalidStateLimit);
        }
        let mut statement = self.connection.prepare(
            "SELECT id, project_id, parent_task_id, assigned_agent_id, title, body, result,
                    status, priority, created_at_ms, updated_at_ms
             FROM tasks
             WHERE project_id = ?1 AND (?2 IS NULL OR id > ?2)
             ORDER BY id
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                project_id.as_str(),
                after_id.map(TaskId::as_str),
                limit as i64
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
                        depends_on: Vec::new(),
                        assigned_agent_id: parse_optional_id(assigned_id, 3)?,
                        title: row.get(4)?,
                        status: parse_task_status(&status, 7)?,
                        priority: row.get(8)?,
                        created_at_ms: row.get(9)?,
                        updated_at_ms: row.get(10)?,
                    },
                    body: row.get(5)?,
                    result: row.get(6)?,
                })
            },
        )?;

        let mut tasks = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        for task in &mut tasks {
            task.snapshot.depends_on = load_task_dependencies(&self.connection, &task.snapshot.id)?;
        }
        Ok(tasks)
    }

    pub fn get_task(&self, project_id: &ProjectId, task_id: &TaskId) -> Result<TaskDetail> {
        let task = load_task(&self.connection, task_id)?.ok_or(StoreError::TaskNotFound)?;
        if task.snapshot.project_id != *project_id {
            return Err(StoreError::TaskNotFound);
        }
        Ok(task)
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

struct SubscriptionStateRow {
    public: SubscriptionProviderState,
    capacity_severity: SubscriptionSeverity,
}

struct StoredSubscriptionProbe {
    outcome: String,
    used_percent: Option<i64>,
    limit_window: Option<String>,
    resets_at_ms: Option<i64>,
    exhausted: Option<i64>,
    failure_category: Option<String>,
}

fn stored_subscription_probe_matches(
    stored: &StoredSubscriptionProbe,
    requested: SubscriptionProbeOutcome,
) -> bool {
    match requested {
        SubscriptionProbeOutcome::Observed {
            used_percent,
            limit_window,
            resets_at_ms,
            exhausted,
        } => {
            stored.outcome == "observed"
                && stored.used_percent == Some(i64::from(used_percent))
                && stored.limit_window.as_deref()
                    == Some(subscription_limit_window_value(limit_window))
                && stored.resets_at_ms == resets_at_ms
                && stored.exhausted == Some(if exhausted { 1 } else { 0 })
                && stored.failure_category.is_none()
        }
        SubscriptionProbeOutcome::Failed { category } => {
            stored.outcome == "failed"
                && stored.used_percent.is_none()
                && stored.limit_window.is_none()
                && stored.resets_at_ms.is_none()
                && stored.exhausted.is_none()
                && stored.failure_category.as_deref() == Some(subscription_failure_value(category))
        }
    }
}

fn load_subscription_state_row(
    connection: &Connection,
    provider: Provider,
) -> Result<Option<SubscriptionStateRow>> {
    connection
        .query_row(
            "SELECT provider, last_attempt_at_ms, last_success_at_ms, used_percent,
                    resets_at_ms, exhausted, severity, consecutive_failures, limit_window,
                    capacity_severity
             FROM subscription_usage_state WHERE provider = ?1",
            params![provider_value(provider)],
            |row| {
                Ok(SubscriptionStateRow {
                    public: parse_subscription_provider_state(row)?,
                    capacity_severity: parse_subscription_severity(&row.get::<_, String>(9)?, 9)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn load_subscription_provider_state(
    connection: &Connection,
    provider: Provider,
) -> Result<Option<SubscriptionProviderState>> {
    connection
        .query_row(
            "SELECT provider, last_attempt_at_ms, last_success_at_ms, used_percent,
                    resets_at_ms, exhausted, severity, consecutive_failures, limit_window
             FROM subscription_usage_state WHERE provider = ?1",
            params![provider_value(provider)],
            parse_subscription_provider_state,
        )
        .optional()
        .map_err(StoreError::from)
}

fn parse_subscription_provider_state(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<SubscriptionProviderState> {
    let provider = parse_provider(&row.get::<_, String>(0)?, 0)?;
    let used_percent = row
        .get::<_, Option<i64>>(3)?
        .map(|value| {
            u8::try_from(value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(3, Type::Integer, Box::new(error))
            })
        })
        .transpose()?;
    let exhausted = row
        .get::<_, Option<i64>>(5)?
        .map(|value| match value {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(rusqlite::Error::IntegralValueOutOfRange(5, value)),
        })
        .transpose()?;
    let consecutive_failures = u32::try_from(row.get::<_, i64>(7)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(7, Type::Integer, Box::new(error))
    })?;
    Ok(SubscriptionProviderState {
        provider,
        last_attempt_at_ms: row.get(1)?,
        last_success_at_ms: row.get(2)?,
        used_percent,
        limit_window: row
            .get::<_, Option<String>>(8)?
            .map(|value| parse_subscription_limit_window(&value, 8))
            .transpose()?,
        resets_at_ms: row.get(4)?,
        exhausted,
        severity: parse_subscription_severity(&row.get::<_, String>(6)?, 6)?,
        consecutive_failures,
    })
}

const fn subscription_capacity_severity(used_percent: u8, exhausted: bool) -> SubscriptionSeverity {
    if exhausted || used_percent >= 95 {
        SubscriptionSeverity::Critical
    } else if used_percent >= 80 {
        SubscriptionSeverity::Warning
    } else {
        SubscriptionSeverity::Ok
    }
}

const fn subscription_severity_value(severity: SubscriptionSeverity) -> &'static str {
    match severity {
        SubscriptionSeverity::Ok => "ok",
        SubscriptionSeverity::Warning => "warning",
        SubscriptionSeverity::Critical => "critical",
    }
}

fn parse_subscription_severity(
    value: &str,
    column: usize,
) -> rusqlite::Result<SubscriptionSeverity> {
    match value {
        "ok" => Ok(SubscriptionSeverity::Ok),
        "warning" => Ok(SubscriptionSeverity::Warning),
        "critical" => Ok(SubscriptionSeverity::Critical),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            column,
            Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid subscription severity",
            )),
        )),
    }
}

const fn subscription_failure_value(category: SubscriptionFailureCategory) -> &'static str {
    match category {
        SubscriptionFailureCategory::Timeout => "timeout",
        SubscriptionFailureCategory::Protocol => "protocol",
        SubscriptionFailureCategory::Process => "process",
        SubscriptionFailureCategory::OutputLimit => "output_limit",
        SubscriptionFailureCategory::Unavailable => "unavailable",
    }
}

const fn subscription_limit_window_value(window: SubscriptionLimitWindow) -> &'static str {
    match window {
        SubscriptionLimitWindow::Primary => "primary",
        SubscriptionLimitWindow::Secondary => "secondary",
        SubscriptionLimitWindow::CurrentSession => "current_session",
        SubscriptionLimitWindow::CurrentWeek => "current_week",
    }
}

fn parse_subscription_limit_window(
    value: &str,
    column: usize,
) -> rusqlite::Result<SubscriptionLimitWindow> {
    match value {
        "primary" => Ok(SubscriptionLimitWindow::Primary),
        "secondary" => Ok(SubscriptionLimitWindow::Secondary),
        "current_session" => Ok(SubscriptionLimitWindow::CurrentSession),
        "current_week" => Ok(SubscriptionLimitWindow::CurrentWeek),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            column,
            Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid subscription limit window",
            )),
        )),
    }
}

const fn subscription_provider_title(provider: Provider) -> &'static str {
    match provider {
        Provider::ClaudeCode => "Claude",
        Provider::Codex => "Codex",
    }
}

const fn subscription_notification_advice(
    outcome: SubscriptionProbeOutcome,
    severity: SubscriptionSeverity,
) -> &'static str {
    match outcome {
        SubscriptionProbeOutcome::Failed { .. } => {
            "The local subscription allowance collector has failed repeatedly. Verify the collector and account status. No work was automatically changed."
        }
        SubscriptionProbeOutcome::Observed { .. } => match severity {
            SubscriptionSeverity::Critical => {
                "Subscription headroom is critical. Review provider availability and current work allocation. No work was automatically paused, switched, purchased, or reassigned."
            }
            SubscriptionSeverity::Warning | SubscriptionSeverity::Ok => {
                "Subscription headroom needs review. Check provider availability and current work allocation. No work was automatically paused, switched, purchased, or reassigned."
            }
        },
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
                    depends_on: load_task_dependencies(connection, &id)?,
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

fn duplicate_failure_transition(
    transaction: &Transaction<'_>,
    ledger: &RunLedger,
) -> Result<ExecutionTransition> {
    let task = load_task(
        transaction,
        ledger
            .snapshot
            .task_id
            .as_ref()
            .ok_or(StoreError::InvalidRunState)?,
    )?
    .ok_or(StoreError::InvalidRunState)?
    .snapshot;
    let agent = load_agent(transaction, &ledger.snapshot.agent_id)?
        .ok_or(StoreError::AgentNotFound)?
        .snapshot;
    Ok(ExecutionTransition {
        disposition: WriteDisposition::Duplicate,
        task,
        agent,
        run: ledger.snapshot.clone(),
        events: Vec::new(),
    })
}

fn fail_run_in_transaction(
    transaction: &Transaction<'_>,
    ledger: &RunLedger,
    reason: RunFailureReason,
    now_ms: i64,
) -> Result<ExecutionTransition> {
    let may_fail_blocked_task = reason == RunFailureReason::Unverifiable;
    let changed = transaction.execute(
        "UPDATE runs
         SET status = 'failed', status_since_ms = ?1, updated_at_ms = ?1,
             ended_at_ms = ?1, exit_code = NULL, exit_signal = NULL,
             failure_reason = ?2, activity = NULL, wait_reason = NULL
         WHERE id = ?3 AND ended_at_ms IS NULL",
        params![
            now_ms,
            failure_reason_value(reason),
            ledger.snapshot.id.as_str(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidRunState);
    }
    let task_id = ledger
        .snapshot
        .task_id
        .as_ref()
        .ok_or(StoreError::InvalidRunState)?;
    let changed = transaction.execute(
        "UPDATE tasks SET status = 'failed', updated_at_ms = ?1,
                          completed_at_ms = ?1
         WHERE id = ?2 AND project_id = ?3 AND assigned_agent_id = ?4
           AND (status = 'running' OR (?5 AND status = 'blocked'))",
        params![
            now_ms,
            task_id.as_str(),
            ledger.snapshot.project_id.as_str(),
            ledger.snapshot.agent_id.as_str(),
            may_fail_blocked_task,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidRunState);
    }
    transaction.execute(
        "UPDATE agents SET updated_at_ms = ?1
         WHERE id = ?2 AND project_id = ?3",
        params![
            now_ms,
            ledger.snapshot.agent_id.as_str(),
            ledger.snapshot.project_id.as_str(),
        ],
    )?;
    let task = load_task(transaction, task_id)?
        .ok_or(StoreError::InvalidRunState)?
        .snapshot;
    let agent = load_agent(transaction, &ledger.snapshot.agent_id)?
        .ok_or(StoreError::AgentNotFound)?
        .snapshot;
    let run = load_run(transaction, &ledger.snapshot.id)?.ok_or(StoreError::RunNotFound)?;
    let events = append_execution_events(transaction, now_ms, &task, &agent, &run)?;
    Ok(ExecutionTransition {
        disposition: WriteDisposition::Applied,
        task,
        agent,
        run,
        events,
    })
}

fn ingest_runner_event_in_transaction(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    runner_instance_id: &RunnerInstanceId,
    event: &RunnerEventEnvelope,
    effects: &RunnerEventEffects,
    now_ms: i64,
) -> Result<IngestResult> {
    if event.sequence <= 0 {
        return Err(StoreError::InvalidRunnerSequence(event.sequence));
    }
    validate_provider_session(effects.confirmed_provider_session_id.as_deref())?;
    let ledger = load_run_ledger(transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
    validate_runner_identity(&ledger, runner_instance_id, event.protocol_version)?;

    if event.sequence <= ledger.last_runner_sequence {
        validate_duplicate(&ledger, event, effects)?;
        return Ok(IngestResult {
            disposition: IngestDisposition::Duplicate,
            events: Vec::new(),
        });
    }
    if ledger.terminal_runner_sequence.is_some() || ledger.snapshot.status.is_terminal() {
        return Err(StoreError::RunnerAlreadyTerminal);
    }
    let expected =
        ledger
            .last_runner_sequence
            .checked_add(1)
            .ok_or(StoreError::CorruptRunnerSequence(
                ledger.last_runner_sequence,
            ))?;
    if event.sequence != expected {
        return Err(StoreError::RunnerSequenceGap {
            expected,
            found: event.sequence,
        });
    }

    let is_provider_stdout = matches!(
        event.event,
        RunnerEvent::Output {
            stream: factory_core::runner::OutputStream::Stdout,
            ..
        }
    );
    if effects.confirmed_provider_session_id.is_some() && !is_provider_stdout {
        return Err(StoreError::InvalidSessionConfirmation);
    }
    let terminal_kind = terminal_kind(&event.event);
    match (terminal_kind, effects.terminal_outcome.as_ref()) {
        (Some(_), None) => return Err(StoreError::TerminalOutcomeRequired),
        (None, Some(_)) => return Err(StoreError::UnexpectedTerminalOutcome),
        _ => {}
    }
    validate_runner_lifecycle(&ledger, &event.event)?;
    if let Some(session_id) = effects.confirmed_provider_session_id.as_deref() {
        confirm_provider_session(transaction, &ledger, session_id, now_ms)?;
    }

    let Some(kind) = terminal_kind else {
        let mut events = Vec::new();
        if matches!(event.event, RunnerEvent::Started { .. }) {
            transaction.execute(
                "UPDATE runs
                 SET status = 'running', last_runner_sequence = ?1,
                     status_since_ms = ?2, updated_at_ms = ?3
                 WHERE id = ?4",
                params![event.sequence, now_ms, now_ms, run_id.as_str()],
            )?;
            let run = load_run(transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
            let changed = FactoryEvent::RunChanged { run };
            let sequence = append_event(transaction, now_ms, &changed)?;
            events.push(EventEnvelope {
                protocol_version: PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event: changed,
            });
        } else {
            transaction.execute(
                "UPDATE runs SET last_runner_sequence = ?1 WHERE id = ?2",
                params![event.sequence, run_id.as_str()],
            )?;
        }
        return Ok(IngestResult {
            disposition: IngestDisposition::Recorded,
            events,
        });
    };

    let outcome = effects
        .terminal_outcome
        .as_ref()
        .ok_or(StoreError::TerminalOutcomeRequired)?;
    let confirmed_session = transaction
        .query_row(
            "SELECT provider_session_confirmed_at_ms FROM runs WHERE id = ?1",
            params![run_id.as_str()],
            |row| row.get::<_, Option<i64>>(0),
        )?
        .is_some();
    let terminal = validate_terminal_outcome(&event.event, outcome, confirmed_session)?;
    transaction.execute(
        "UPDATE runs
         SET status = ?1, last_runner_sequence = ?2,
             terminal_runner_sequence = ?2, runner_terminal_kind = ?3,
             status_since_ms = ?4, updated_at_ms = ?4, ended_at_ms = ?4,
             exit_code = ?5, exit_signal = ?6, failure_reason = ?7,
             activity = NULL, wait_reason = NULL
         WHERE id = ?8",
        params![
            run_status_value(terminal.run_status),
            event.sequence,
            kind,
            now_ms,
            terminal.exit_code,
            terminal.exit_signal,
            terminal.failure_reason.map(failure_reason_value),
            run_id.as_str(),
        ],
    )?;
    let task_id = ledger
        .snapshot
        .task_id
        .as_ref()
        .ok_or(StoreError::InvalidRunState)?;
    let changed = transaction.execute(
        "UPDATE tasks
         SET status = ?1, updated_at_ms = ?2, completed_at_ms = ?2, result = ?3
         WHERE id = ?4 AND project_id = ?5 AND assigned_agent_id = ?6
           AND status = 'running'",
        params![
            task_status_value(terminal.task_status),
            now_ms,
            terminal.result,
            task_id.as_str(),
            ledger.snapshot.project_id.as_str(),
            ledger.snapshot.agent_id.as_str(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidRunState);
    }
    transaction.execute(
        "UPDATE agents SET updated_at_ms = ?1 WHERE id = ?2",
        params![now_ms, ledger.snapshot.agent_id.as_str()],
    )?;
    let task = load_task(transaction, task_id)?
        .ok_or(StoreError::InvalidRunState)?
        .snapshot;
    let agent = load_agent(transaction, &ledger.snapshot.agent_id)?
        .ok_or(StoreError::AgentNotFound)?
        .snapshot;
    let run = load_run(transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
    let events = append_execution_events(transaction, now_ms, &task, &agent, &run)?;
    Ok(IngestResult {
        disposition: IngestDisposition::Recorded,
        events,
    })
}

struct AgentSessionContext {
    provider: Provider,
    session_id: String,
    cwd: String,
    codex_home: Option<String>,
}

fn adopted_session_context(session: AdoptedProviderSession) -> Result<AgentSessionContext> {
    let context = match session {
        AdoptedProviderSession::ClaudeCode { session_id, cwd } => AgentSessionContext {
            provider: Provider::ClaudeCode,
            session_id,
            cwd,
            codex_home: None,
        },
        AdoptedProviderSession::Codex {
            thread_id,
            cwd,
            codex_home,
        } => AgentSessionContext {
            provider: Provider::Codex,
            session_id: thread_id,
            cwd,
            codex_home,
        },
    };
    validate_canonical_provider_session(&context.session_id)?;
    validate_canonical_absolute_path(&context.cwd)?;
    if let Some(codex_home) = context.codex_home.as_deref() {
        validate_canonical_absolute_path(codex_home)?;
    }
    Ok(context)
}

struct AgentRecord {
    snapshot: AgentSnapshot,
    provider_session_id: Option<String>,
    provider_session_cwd: Option<String>,
    codex_home: Option<String>,
}

struct RunLedger {
    snapshot: RunSnapshot,
    provider_session_id: Option<String>,
    provider_session_confirmed_at_ms: Option<i64>,
    runner_instance_id: RunnerInstanceId,
    runner_protocol_version: u16,
    last_runner_sequence: i64,
    terminal_runner_sequence: Option<i64>,
    runner_reconciled_at_ms: Option<i64>,
    runner_terminal_kind: Option<String>,
    task_result: Option<String>,
}

fn load_run_ledger(connection: &Connection, run_id: &RunId) -> Result<Option<RunLedger>> {
    let Some(snapshot) = load_run(connection, run_id)? else {
        return Ok(None);
    };
    connection
        .query_row(
            "SELECT provider_session_id, provider_session_confirmed_at_ms,
                    runner_instance_id, runner_protocol_version,
                    last_runner_sequence, terminal_runner_sequence,
                    runner_reconciled_at_ms, runner_terminal_kind,
                    (SELECT result FROM tasks WHERE id = runs.task_id)
             FROM runs WHERE id = ?1",
            params![run_id.as_str()],
            |row| {
                let protocol: i64 = row.get(3)?;
                Ok(RunLedger {
                    snapshot,
                    provider_session_id: row.get(0)?,
                    provider_session_confirmed_at_ms: row.get(1)?,
                    runner_instance_id: parse_id(row.get(2)?, 2)?,
                    runner_protocol_version: u16::try_from(protocol).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(3, Type::Integer, Box::new(error))
                    })?,
                    last_runner_sequence: row.get(4)?,
                    terminal_runner_sequence: row.get(5)?,
                    runner_reconciled_at_ms: row.get(6)?,
                    runner_terminal_kind: row.get(7)?,
                    task_result: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn validate_runner_identity(
    ledger: &RunLedger,
    runner_instance_id: &RunnerInstanceId,
    protocol_version: u16,
) -> Result<()> {
    if &ledger.runner_instance_id != runner_instance_id {
        return Err(StoreError::RunnerIdentityMismatch);
    }
    if protocol_version != ledger.runner_protocol_version {
        return Err(StoreError::RunnerProtocolMismatch {
            expected: ledger.runner_protocol_version,
            found: protocol_version,
        });
    }
    Ok(())
}

fn validate_runner_lifecycle(ledger: &RunLedger, event: &RunnerEvent) -> Result<()> {
    let valid = match ledger.snapshot.status {
        RunStatus::Starting => {
            ledger.last_runner_sequence == 0
                && matches!(
                    event,
                    RunnerEvent::Started { .. } | RunnerEvent::SpawnFailed { .. }
                )
        }
        RunStatus::Running => matches!(
            event,
            RunnerEvent::Output { .. }
                | RunnerEvent::OutputTruncated { .. }
                | RunnerEvent::Exited { .. }
        ),
        RunStatus::Waiting
        | RunStatus::Blocked
        | RunStatus::Paused
        | RunStatus::Succeeded
        | RunStatus::Failed
        | RunStatus::Stopped => false,
    };
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidRunnerLifecycle)
    }
}

fn terminal_kind(event: &RunnerEvent) -> Option<&'static str> {
    match event {
        RunnerEvent::SpawnFailed { .. } => Some("spawn_failed"),
        RunnerEvent::Exited { .. } => Some("exited"),
        RunnerEvent::Started { .. }
        | RunnerEvent::Output { .. }
        | RunnerEvent::OutputTruncated { .. } => None,
    }
}

fn confirm_provider_session(
    transaction: &Transaction<'_>,
    ledger: &RunLedger,
    session_id: &str,
    now_ms: i64,
) -> Result<()> {
    if ledger
        .provider_session_id
        .as_deref()
        .is_some_and(|expected| expected != session_id)
    {
        return Err(StoreError::ProviderSessionConflict);
    }
    let agent =
        load_agent(transaction, &ledger.snapshot.agent_id)?.ok_or(StoreError::AgentNotFound)?;
    if agent
        .provider_session_id
        .as_deref()
        .is_some_and(|established| established != session_id)
    {
        return Err(StoreError::ProviderSessionConflict);
    }
    let owner = transaction
        .query_row(
            "SELECT id FROM agents
             WHERE provider = ?1 AND provider_session_id = ?2",
            params![provider_value(agent.snapshot.provider), session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if owner
        .as_deref()
        .is_some_and(|owner| owner != ledger.snapshot.agent_id.as_str())
    {
        return Err(StoreError::ProviderSessionConflict);
    }
    transaction.execute(
        "UPDATE runs
         SET provider_session_id = COALESCE(provider_session_id, ?1),
             provider_session_confirmed_at_ms =
                 COALESCE(provider_session_confirmed_at_ms, ?2)
         WHERE id = ?3",
        params![session_id, now_ms, ledger.snapshot.id.as_str()],
    )?;
    transaction.execute(
        "UPDATE agents
         SET provider_session_id = COALESCE(provider_session_id, ?1),
             provider_session_cwd = COALESCE(provider_session_cwd, ?2)
         WHERE id = ?3",
        params![
            session_id,
            ledger.snapshot.worktree,
            ledger.snapshot.agent_id.as_str()
        ],
    )?;
    Ok(())
}

fn validate_duplicate(
    ledger: &RunLedger,
    event: &RunnerEventEnvelope,
    effects: &RunnerEventEffects,
) -> Result<()> {
    if let Some(session_id) = effects.confirmed_provider_session_id.as_deref() {
        if ledger.provider_session_id.as_deref() != Some(session_id)
            || ledger.provider_session_confirmed_at_ms.is_none()
        {
            return Err(StoreError::ProviderSessionConflict);
        }
        if !matches!(
            event.event,
            RunnerEvent::Output {
                stream: factory_core::runner::OutputStream::Stdout,
                ..
            }
        ) {
            return Err(StoreError::InvalidSessionConfirmation);
        }
    }
    if Some(event.sequence) == ledger.terminal_runner_sequence {
        let outcome = effects
            .terminal_outcome
            .as_ref()
            .ok_or(StoreError::TerminalOutcomeRequired)?;
        let kind = terminal_kind(&event.event).ok_or(StoreError::InvalidTerminalOutcome)?;
        if ledger.runner_terminal_kind.as_deref() != Some(kind) {
            return Err(StoreError::InvalidTerminalOutcome);
        }
        let terminal = validate_terminal_outcome(
            &event.event,
            outcome,
            ledger.provider_session_confirmed_at_ms.is_some(),
        )?;
        if ledger.snapshot.status != terminal.run_status
            || ledger.snapshot.failure_reason != terminal.failure_reason
            || ledger.snapshot.exit_code != terminal.exit_code
            || ledger.snapshot.exit_signal != terminal.exit_signal
            || ledger.task_result != terminal.result
        {
            return Err(StoreError::InvalidTerminalOutcome);
        }
    } else if effects.terminal_outcome.is_some() || terminal_kind(&event.event).is_some() {
        return Err(StoreError::InvalidTerminalOutcome);
    }
    Ok(())
}

struct TerminalState {
    run_status: RunStatus,
    task_status: TaskStatus,
    failure_reason: Option<RunFailureReason>,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    result: Option<String>,
}

fn validate_terminal_outcome(
    event: &RunnerEvent,
    outcome: &TerminalOutcome,
    provider_session_confirmed: bool,
) -> Result<TerminalState> {
    match (event, outcome) {
        (RunnerEvent::SpawnFailed { .. }, TerminalOutcome::Failed(RunFailureReason::Spawn)) => {
            Ok(TerminalState {
                run_status: RunStatus::Failed,
                task_status: TaskStatus::Failed,
                failure_reason: Some(RunFailureReason::Spawn),
                exit_code: None,
                exit_signal: None,
                result: None,
            })
        }
        (
            RunnerEvent::Exited {
                exit_code: Some(0),
                signal: None,
            },
            TerminalOutcome::Succeeded { result },
        ) if provider_session_confirmed && valid_terminal_result(result.as_deref()) => {
            Ok(TerminalState {
                run_status: RunStatus::Succeeded,
                task_status: TaskStatus::Succeeded,
                failure_reason: None,
                exit_code: Some(0),
                exit_signal: None,
                result: result.clone(),
            })
        }
        (RunnerEvent::Exited { exit_code, signal }, TerminalOutcome::Failed(reason))
            if valid_failed_process_outcome(*exit_code, *signal, *reason) =>
        {
            Ok(TerminalState {
                run_status: RunStatus::Failed,
                task_status: TaskStatus::Failed,
                failure_reason: Some(*reason),
                exit_code: *exit_code,
                exit_signal: *signal,
                result: None,
            })
        }
        _ => Err(StoreError::InvalidTerminalOutcome),
    }
}

fn valid_terminal_result(result: Option<&str>) -> bool {
    result.is_none_or(|value| {
        value.len() <= MAX_TERMINAL_RESULT_BYTES
            && value
                .chars()
                .all(|character| !character.is_control() || matches!(character, '\n' | '\t'))
    })
}

fn valid_failed_process_outcome(
    exit_code: Option<i32>,
    signal: Option<i32>,
    reason: RunFailureReason,
) -> bool {
    let valid_exit = matches!((exit_code, signal), (Some(code), None) if code >= 0)
        || matches!((exit_code, signal), (None, Some(signal)) if signal > 0);
    if !valid_exit {
        return false;
    }
    let process_failed = signal.is_some() || exit_code != Some(0);
    reason != RunFailureReason::Spawn && (reason != RunFailureReason::Process || process_failed)
}

fn load_agent(connection: &Connection, agent_id: &AgentId) -> Result<Option<AgentRecord>> {
    connection
        .query_row(
            "SELECT a.id, a.project_id, a.parent_agent_id, a.role, a.provider,
                    a.provider_session_id, a.provider_session_cwd, a.codex_home,
                    a.created_at_ms, a.updated_at_ms,
                    (SELECT r.id FROM runs r
                     WHERE r.agent_id = a.id
                       AND r.ended_at_ms IS NULL
                     LIMIT 1)
             FROM agents a
             WHERE a.id = ?1",
            params![agent_id.as_str()],
            |row| {
                let parent_agent_id: Option<String> = row.get(2)?;
                let role: String = row.get(3)?;
                let provider: String = row.get(4)?;
                let current_run_id: Option<String> = row.get(10)?;
                Ok(AgentRecord {
                    snapshot: AgentSnapshot {
                        id: parse_id(row.get(0)?, 0)?,
                        project_id: parse_id(row.get(1)?, 1)?,
                        parent_agent_id: parse_optional_id(parent_agent_id, 2)?,
                        role: parse_agent_role(&role, 3)?,
                        provider: parse_provider(&provider, 4)?,
                        current_run_id: parse_optional_id(current_run_id, 10)?,
                        created_at_ms: row.get(8)?,
                        updated_at_ms: row.get(9)?,
                    },
                    provider_session_id: row.get(5)?,
                    provider_session_cwd: row.get(6)?,
                    codex_home: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn load_task(connection: &Connection, task_id: &TaskId) -> Result<Option<TaskDetail>> {
    let mut task = connection
        .query_row(
            "SELECT id, project_id, parent_task_id, assigned_agent_id, title, body, result,
                    status, priority, created_at_ms, updated_at_ms
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
                        depends_on: Vec::new(),
                        assigned_agent_id: parse_optional_id(assigned_id, 3)?,
                        title: row.get(4)?,
                        status: parse_task_status(&status, 7)?,
                        priority: row.get(8)?,
                        created_at_ms: row.get(9)?,
                        updated_at_ms: row.get(10)?,
                    },
                    body: row.get(5)?,
                    result: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)?;
    if let Some(task) = task.as_mut() {
        task.snapshot.depends_on = load_task_dependencies(connection, &task.snapshot.id)?;
    }
    Ok(task)
}

fn load_task_dependencies(connection: &Connection, task_id: &TaskId) -> Result<Vec<TaskId>> {
    let mut statement = connection.prepare(
        "SELECT depends_on_task_id FROM task_dependencies
         WHERE task_id = ?1 ORDER BY ordinal",
    )?;
    let rows = statement.query_map(params![task_id.as_str()], |row| parse_id(row.get(0)?, 0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn load_run(connection: &Connection, run_id: &RunId) -> Result<Option<RunSnapshot>> {
    connection
        .query_row(
            "SELECT id, project_id, agent_id, parent_run_id, task_id, status,
                    activity, wait_reason, worktree, observer_health,
                    observer_health_since_ms, started_at_ms, status_since_ms,
                    updated_at_ms, ended_at_ms, exit_code, exit_signal, failure_reason
             FROM runs WHERE id = ?1",
            params![run_id.as_str()],
            |row| {
                let parent_run_id: Option<String> = row.get(3)?;
                let task_id: Option<String> = row.get(4)?;
                let status: String = row.get(5)?;
                let observer_health: String = row.get(9)?;
                let failure_reason: Option<String> = row.get(17)?;
                Ok(RunSnapshot {
                    id: parse_id(row.get(0)?, 0)?,
                    project_id: parse_id(row.get(1)?, 1)?,
                    agent_id: parse_id(row.get(2)?, 2)?,
                    parent_run_id: parse_optional_id(parent_run_id, 3)?,
                    task_id: parse_optional_id(task_id, 4)?,
                    status: parse_run_status(&status, 5)?,
                    activity: row.get(6)?,
                    wait_reason: row.get(7)?,
                    worktree: row.get(8)?,
                    observer_health: parse_observer_health(&observer_health, 9)?,
                    observer_health_since_ms: row.get(10)?,
                    started_at_ms: row.get(11)?,
                    status_since_ms: row.get(12)?,
                    updated_at_ms: row.get(13)?,
                    ended_at_ms: row.get(14)?,
                    exit_code: row.get(15)?,
                    exit_signal: row.get(16)?,
                    failure_reason: parse_optional_failure_reason(failure_reason, 17)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn load_execution_target(
    connection: &Connection,
    run_id: &RunId,
) -> Result<Option<ExecutionTarget>> {
    connection
        .query_row(
            "SELECT a.provider, p.root, t.body, r.worktree, r.provider_session_id,
                    a.codex_home, r.resumes_provider_session, r.runner_instance_id,
                    r.runner_protocol_version, r.runner_runtime,
                    r.last_runner_sequence
             FROM runs r
             JOIN agents a ON a.id = r.agent_id
             JOIN projects p ON p.id = r.project_id
             JOIN tasks t ON t.id = r.task_id
             WHERE r.id = ?1",
            params![run_id.as_str()],
            |row| {
                let provider: String = row.get(0)?;
                let protocol: i64 = row.get(8)?;
                Ok(ExecutionTarget {
                    provider: parse_provider(&provider, 0)?,
                    project_root: row.get(1)?,
                    task_body: row.get(2)?,
                    worktree: row.get(3)?,
                    provider_session_id: row.get(4)?,
                    codex_home: row.get(5)?,
                    resumes_provider_session: row.get(6)?,
                    runner_instance_id: parse_id(row.get(7)?, 7)?,
                    runner_protocol_version: u16::try_from(protocol).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(8, Type::Integer, Box::new(error))
                    })?,
                    runner_runtime: row.get(9)?,
                    last_committed_runner_sequence: row.get(10)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn validate_parent_run(
    connection: &Connection,
    project_id: &ProjectId,
    parent_agent_id: Option<&AgentId>,
    parent_run_id: Option<&RunId>,
) -> Result<()> {
    let Some(parent_run_id) = parent_run_id else {
        return Ok(());
    };
    let Some(parent_agent_id) = parent_agent_id else {
        return Err(StoreError::ParentRunLineageMismatch);
    };
    let matches = connection
        .query_row(
            "SELECT 1 FROM runs
             WHERE id = ?1 AND project_id = ?2 AND agent_id = ?3",
            params![
                parent_run_id.as_str(),
                project_id.as_str(),
                parent_agent_id.as_str(),
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if matches {
        Ok(())
    } else {
        Err(StoreError::ParentRunLineageMismatch)
    }
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

fn validate_canonical_provider_session(value: &str) -> Result<()> {
    validate_provider_session(Some(value))?;
    let parsed = Uuid::parse_str(value).map_err(|_| StoreError::InvalidExecutionMetadata)?;
    if parsed.hyphenated().to_string() != value {
        return Err(StoreError::InvalidExecutionMetadata);
    }
    Ok(())
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

fn validate_canonical_absolute_path(value: &str) -> Result<()> {
    validate_absolute_path(value)?;
    let mut components = Path::new(value).components();
    if !matches!(components.next(), Some(Component::RootDir))
        || components.any(|component| !matches!(component, Component::Normal(_)))
        || value.chars().any(char::is_control)
        || value
            .split('/')
            .skip(1)
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(StoreError::InvalidExecutionMetadata);
    }
    Ok(())
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

struct EventMetadata<'a> {
    project_id: Option<&'a ProjectId>,
    task_id: Option<&'a TaskId>,
    agent_id: Option<&'a AgentId>,
    run_id: Option<&'a RunId>,
}

fn event_metadata(event: &FactoryEvent) -> EventMetadata<'_> {
    match event {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
