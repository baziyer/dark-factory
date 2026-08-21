use std::path::Path;

use factory_core::{
    AgentBudget, AgentId, AgentRole, AgentSnapshot, EventEnvelope, ExecutionMode, FactoryEvent,
    MessageId, ObserverHealth, PROTOCOL_VERSION, ProjectId, ProjectSnapshot, Provider, RunId,
    RunSnapshot, TaskDetail, TaskId, TaskSnapshot, TaskStatus,
    attention::{Attention, run_attention},
    local::{MAX_TASK_BODY_BYTES, normalize_task_title},
    model_policy,
    status::{AgentPauseReason, AgentStatus, MAX_QUEUE_PREVIEW},
};
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, types::Type,
};
use thiserror::Error;
use uuid::Uuid;

mod changes;
mod kernel;
mod rust_builds;
pub use changes::{
    Change, ChangeBaseIdentity, ChangeMaterialization, ChangeMutation, ChangeRemovalKind,
    ChangeReservation, ChangeSourceIdentity, ChangeStorageSummary,
};
pub use kernel::{
    AdmittedRun, AttemptPrincipal, AttemptTarget, AttemptToolPolicy, AttemptToolVerdict,
    KernelResource, KernelResourceKind, KernelResourceState, NewRunAdmission,
    PreparedProcessIdentity, RecordedAttemptToolDecision, RecoverableKernelRun,
};
pub(crate) use rust_builds::{MAX_RUST_CACHE_BYTES, MAX_RUST_CACHE_COUNT};
pub use rust_builds::{
    RustBuildCache, RustCacheLifecycle, RustCompletionCheck, RustCompletionPhase,
    RustStorageSummary,
};

/// One project's rows for `factory_core::status::FleetStatus`: the project,
/// its agents' statuses, its project backlog, and its blocked tasks (see
/// [`Store::fleet_status`]).
pub struct ProjectStatusRows {
    pub project: ProjectSnapshot,
    pub agents: Vec<AgentStatus>,
    pub backlog: Vec<TaskSnapshot>,
    pub blocked: Vec<factory_core::status::BlockedTaskStatus>,
}

const SCHEMA_VERSION: i64 = 34;
const MAX_EVENT_PAGE: usize = 10_000;
/// Every `List*` handler in `local_api.rs` fetches `limit + 1` rows (one
/// extra, to detect whether a next page exists) where `limit` is bounded by
/// the largest wire page cap, `factory_core::local::MAX_*_PAGE_ITEMS`
/// (1000). This used to be 101, silently rejecting a client that paged at
/// the documented wire maximum (`factoryctl run list`, `agent list`,
/// ... with no `--limit`, which default to the wire max) with "state page
/// limit is outside the supported range" -- found while building this
/// track's E2E tests, not a hypothetical.
const MAX_STATE_PAGE: usize = 1_001;
const MAX_PATH_BYTES: usize = 4096;
const MAX_AGENT_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_AGENT_MODEL_BYTES: usize = 256;
const MAX_WAIT_REASON_BYTES: usize = 512;
/// Mirrors the `tasks.blocked_reason` CHECK bound (migration 0014).
const MAX_BLOCKED_REASON_BYTES: usize = 4096;
/// Mirrors the `tasks.result` CHECK bound (migration 0006).
const MAX_TASK_RESULT_BYTES: usize = 131_072;

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

/// Durable model selection and typed provider execution authority. Standing
/// instructions and memory used to live here as TEXT columns; they are now
/// operator- and agent-editable files under the state directory (see
/// `factoryd::guidance` and `factory_core::paths`), composed at launch by
/// the execution track. `execution_mode` is consumed by provider launch and
/// retained separately from each admitted attempt's resolved runtime metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentProfile {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub model_selection_reason: Option<String>,
    pub execution_mode: ExecutionMode,
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
    pub execution_mode: ExecutionMode,
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
pub struct AgentMessage {
    pub id: MessageId,
    pub project_id: ProjectId,
    pub sender_agent_id: Option<AgentId>,
    pub recipient_agent_id: AgentId,
    pub body: String,
    pub created_at_ms: i64,
    pub delivered_at_ms: Option<i64>,
    pub delivered_run_id: Option<RunId>,
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
    #[error(
        "safe-kernel migration requires a quiescent schema-29 database \
         (live sessions: {live_sessions}, owned session work: {owned_session_work}, \
         active deliveries: {active_deliveries}, nonterminal runs: {nonterminal_runs}, \
         taskless runs: {taskless_runs})"
    )]
    KernelMigrationRequiresQuiescence {
        live_sessions: i64,
        owned_session_work: i64,
        active_deliveries: i64,
        nonterminal_runs: i64,
        taskless_runs: i64,
    },
    #[error(
        "daemon-owned Change migration requires an empty schema-30 Change authority \
         (runs linked to Changes: {linked_runs})"
    )]
    ChangeMigrationRequiresEmptyAuthority { linked_runs: i64 },
    #[error("migration left a foreign key violation behind")]
    ForeignKeyViolation,
    #[error("event page size must be between 1 and {MAX_EVENT_PAGE}")]
    InvalidEventLimit,
    #[error("state page size must be between 1 and {MAX_STATE_PAGE}")]
    InvalidStateLimit,
    #[error("corrupt event protocol version {0}")]
    CorruptProtocolVersion(i64),
    #[error("legacy event is missing a valid indexed identity")]
    CorruptLegacyEvent,
    #[error("serialized factory event has no string type tag")]
    MissingEventKind,
    #[error("event cursor must not be negative")]
    InvalidEventCursor,
    #[error("event log gap: expected sequence {expected}, found {found}")]
    EventSequenceGap { expected: i64, found: i64 },
    #[error("agent was not found in the requested project")]
    AgentNotFound,
    #[error("task was not found in the requested project")]
    TaskNotFound,
    #[error("task page cursor is stale; restart the listing")]
    StaleTaskCursor,
    #[error("task page cursor requires its queue revision")]
    MissingTaskCursorRevision,
    #[error("task page revision requires its cursor")]
    UnexpectedTaskCursorRevision,
    #[error("agent profile is invalid or exceeds its bound")]
    InvalidAgentProfile,
    #[error("agent model policy rejected the profile: {0}")]
    InvalidAgentModelPolicy(#[from] model_policy::ModelPolicyError),
    #[error("execution mode {mode:?} is not supported by provider {provider:?}")]
    UnsupportedAgentExecutionMode {
        provider: Provider,
        mode: ExecutionMode,
    },
    #[error("agent budget is invalid or exceeds its bound")]
    InvalidAgentBudget,
    #[error("agent budget is exhausted; reset it before resuming")]
    AgentBudgetExhausted,
    #[error("agent message is invalid or exceeds its bound")]
    InvalidAgentMessage,
    #[error("task is not queued in the requested project")]
    TaskNotQueued,
    #[error("task is not retryable in the requested project")]
    TaskNotRetryable,
    #[error("task result exceeds its bound")]
    InvalidTaskResult,
    #[error("task title or body is invalid or exceeds its bound")]
    InvalidTaskInput,
    #[error("task blocked reason is empty or exceeds its bound")]
    InvalidBlockedReason,
    #[error("execution concurrency must be greater than zero")]
    InvalidConcurrencyLimit,
    #[error("factory execution capacity is {limit} active runs")]
    CapacityReached { limit: usize },
    #[error("change was not found in the requested project")]
    ChangeNotFound,
    #[error("legacy source metadata was not found in the requested project")]
    LegacySourceNotFound,
    #[error("change metadata is invalid or exceeds its bound")]
    InvalidChangeMetadata,
    #[error("change retention capacity must be greater than zero")]
    InvalidChangeCapacity,
    #[error("factory Change retention capacity is {limit}")]
    ChangeCapacityReached { limit: usize },
    #[error("change revision is stale")]
    ChangeRevisionConflict,
    #[error("change is not in the required state")]
    InvalidChangeState,
    #[error("change source identity no longer matches the registered source")]
    ChangeIdentityMismatch,
    #[error("change is leased by a nonterminal run")]
    ChangeLeased,
    #[error("task owns retained Change metadata")]
    TaskHasChanges,
    #[error("task incarnation has a Change that is being removed or was removed")]
    TaskChangeUnavailable,
    #[error("project owns retained Change or legacy-source metadata")]
    ProjectHasChanges,
    #[error("project owns a regenerable Rust build cache")]
    ProjectHasRustCaches,
    #[error("run was not found")]
    RunNotFound,
    #[error("run still owns {count} unreleased resources")]
    RunResourcesUnreleased { count: i64 },
    #[error("run completion verification has not reached a terminal result")]
    CompletionVerificationPending,
    #[error("Rust completion verification was not found")]
    RustCompletionCheckNotFound,
    #[error("Rust completion verification is not in the required phase")]
    InvalidRustCompletionPhase,
    #[error("another Rust build holds the project cache writer lease")]
    RustCacheWriterBusy,
    #[error("Rust {kind} storage capacity is {limit} artifacts")]
    RustStorageCapacityReached { kind: &'static str, limit: u64 },
    #[error("Rust build metadata is invalid or exceeds its bound")]
    InvalidRustBuildMetadata,
    #[error("resource was not found")]
    ResourceNotFound,
    #[error("resource identity no longer matches the registered resource")]
    ResourceIdentityMismatch,
    #[error("attempt credential is invalid")]
    InvalidHookToken,
    #[error("provider tool policy input is invalid or exceeds its bound")]
    InvalidToolPolicy,
    #[error("request is outside the admitted attempt's authority")]
    AttemptScopeDenied,
    #[error("run is not in the required state")]
    InvalidRunState,
    #[error("attempt already has a different terminal outcome")]
    AttemptOutcomeConflict,
    #[error("private execution metadata is empty, relative, or too large")]
    InvalidExecutionMetadata,
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
    #[error("agent has child agents and cannot be deleted")]
    AgentHasChildren,
    #[error("a run of this agent is the parent of another run and cannot be deleted")]
    AgentRunHasDependents,
    #[error("project has a non-terminal run and cannot be deleted")]
    ProjectHasActiveRun,
    #[error("could not migrate stored agent instructions/memory to guidance files: {0}")]
    AgentProfileMigration(String),
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
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;",
        )?;
        migrate(&mut connection)?;
        Ok(Self { connection })
    }

    pub fn dispatch_enabled(&self) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT dispatch_enabled FROM factory_settings WHERE singleton = 1",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(Into::into)
    }

    pub fn set_dispatch_enabled(&mut self, enabled: bool, now_ms: i64) -> Result<EventEnvelope> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE factory_settings
             SET dispatch_enabled = ?1, updated_at_ms = ?2
             WHERE singleton = 1",
            params![enabled, now_ms],
        )?;
        let event = FactoryEvent::DispatchPolicyChanged { enabled };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok(EventEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: now_ms,
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
            completion_verification: factory_core::CompletionVerification::None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        let event = FactoryEvent::ProjectChanged {
            project: project.clone(),
        };
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let incarnation_id = Uuid::new_v4().simple().to_string();

        transaction.execute(
            "INSERT INTO projects (
                id, name, root, created_at_ms, updated_at_ms,
                incarnation_id, completion_verification
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'none')",
            params![
                project.id.as_str(),
                project.name,
                project.root,
                project.created_at_ms,
                project.updated_at_ms,
                incarnation_id,
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
                id, project_id, parent_agent_id, role, provider, paused,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?6)",
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
                agent_id, model, reasoning_effort, model_selection_reason,
                execution_mode, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                agent.id.as_str(),
                model,
                reasoning_effort,
                model_selection_reason,
                ExecutionMode::default_for_provider(agent.provider).as_str(),
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

    /// Durably holds an agent's queue: the daemon stops admitting new work
    /// until `resume_agent`.
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

    // --- Agent messages --------------------------------------------------

    /// Stores a private message without appending a public factory event.
    pub fn send_agent_message(&mut self, input: NewAgentMessage) -> Result<AgentMessage> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let message = insert_agent_message(&transaction, input)?;
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
                    body, created_at_ms, delivered_at_ms, delivered_run_id
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
                    body, created_at_ms, delivered_at_ms, delivered_run_id
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

    pub fn get_project(&self, project_id: &ProjectId) -> Result<ProjectSnapshot> {
        self.connection
            .query_row(
                "SELECT id, name, root, completion_verification, created_at_ms, updated_at_ms
                 FROM projects WHERE id = ?1",
                params![project_id.as_str()],
                |row| {
                    Ok(ProjectSnapshot {
                        id: parse_id(row.get(0)?, 0)?,
                        name: row.get(1)?,
                        root: row.get(2)?,
                        completion_verification: parse_completion_verification(row.get(3)?, 3)?,
                        created_at_ms: row.get(4)?,
                        updated_at_ms: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)?
            .ok_or(StoreError::ProjectNotFound)
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
            "SELECT id, name, root, completion_verification, created_at_ms, updated_at_ms
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
                    completion_verification: parse_completion_verification(row.get(3)?, 3)?,
                    created_at_ms: row.get(4)?,
                    updated_at_ms: row.get(5)?,
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
        let unavailable_change: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM changes c
                 JOIN tasks t ON t.id = c.task_id AND t.project_id = c.project_id
                 WHERE c.project_id = ?1 AND c.task_id = ?2
                   AND c.task_incarnation_id = t.incarnation_id
                   AND c.phase IN ('removing', 'removed')
             )",
            params![project_id.as_str(), task_id.as_str()],
            |row| row.get(0),
        )?;
        if unavailable_change {
            return Err(StoreError::TaskChangeUnavailable);
        }
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
        let (task, event) =
            assign_task_in_transaction(&transaction, project_id, task_id, agent_id, now_ms)?;
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

    /// Cancels a queued or blocked task. A running task is owned by its
    /// admitted attempt and must instead enter finalization through the
    /// attempt cancellation path.
    pub fn cancel_task(
        &mut self,
        project_id: &ProjectId,
        task_id: &TaskId,
        now_ms: i64,
    ) -> Result<(TaskDetail, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        load_task(&transaction, task_id)?
            .filter(|task| task.snapshot.project_id == *project_id)
            .ok_or(StoreError::TaskNotFound)?;
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
    /// that still references the task is removed in the same transaction.
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
                WHERE task_id = ?1 AND phase <> 'terminal'
             )",
            params![task_id.as_str()],
            |row| row.get(0),
        )?;
        if has_active_run {
            return Err(StoreError::TaskHasActiveRun);
        }
        let has_change: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM changes
                 WHERE task_id = ?1 AND project_id = ?2 AND phase <> 'removed'
             )",
            params![task_id.as_str(), project_id.as_str()],
            |row| row.get(0),
        )?;
        if has_change {
            return Err(StoreError::TaskHasChanges);
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
        // Schema-6 integration tables are inert, but upgraded databases may
        // retain rows whose foreign keys must be cleared with their task.
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
        // Removed rows are tombstone metadata, not retained source authority.
        // Their immutable ChangeChanged events remain in the audit ledger.
        transaction.execute(
            "DELETE FROM changes
             WHERE task_id = ?1 AND project_id = ?2 AND phase = 'removed'",
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
    /// gate (`execution::Handle::begin_delete`) to keep a *new* attempt from
    /// appearing in between, the same way `delete_agent`'s own transaction
    /// does the authoritative final check.
    pub fn check_agent_deletable(&self, project_id: &ProjectId, agent_id: &AgentId) -> Result<()> {
        check_agent_deletable(&self.connection, project_id, agent_id)
    }

    /// See [`Store::check_agent_deletable`]: same reasoning, one level up.
    pub fn check_project_deletable(&self, project_id: &ProjectId) -> Result<()> {
        check_project_deletable(&self.connection, project_id)
    }

    /// Deletes an agent that has no active run, no child agents, and no run
    /// that is itself the parent of another run. Its terminal attempts and
    /// resources are deleted too; tasks still assigned to
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
            "DELETE FROM agent_profiles WHERE agent_id = ?1",
            params![agent_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM resources WHERE run_id IN (
                 SELECT id FROM runs WHERE agent_id = ?1 AND project_id = ?2
             )",
            params![agent_id.as_str(), project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM runs WHERE agent_id = ?1 AND project_id = ?2",
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

    /// Deletes a project and all of its terminal attempts and resources in
    /// one transaction. Refused while any non-terminal run remains.
    pub fn delete_project(&mut self, project_id: &ProjectId, now_ms: i64) -> Result<EventEnvelope> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_project_deletable(&transaction, project_id)?;

        transaction.execute_batch("PRAGMA defer_foreign_keys = ON;")?;
        // Schema-6 integration tables are inert, but upgraded databases may
        // retain rows whose foreign keys must be cleared with their project.
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
            "DELETE FROM resources WHERE run_id IN (
                 SELECT id FROM runs WHERE project_id = ?1
             )",
            params![project_id.as_str()],
        )?;
        transaction.execute(
            "DELETE FROM runs WHERE project_id = ?1",
            params![project_id.as_str()],
        )?;
        // Project deletion discards terminal tombstone metadata only after
        // every referencing run has gone; durable events remain append-only.
        transaction.execute(
            "DELETE FROM changes WHERE project_id = ?1 AND phase = 'removed'",
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
        // Webhook intake is gone, but an upgraded database may retain immutable
        // document snapshots. They remain untouched until the operator deletes
        // their project, when this transaction removes the exact project rows.
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

    /// Every project's live picture in one read: agents with their current
    /// or most recent run, queued tasks, and undelivered inbox count; per-project backlog; the
    /// project's blocked tasks (for the attention list). One connection,
    /// so every field is from the same instant. See
    /// `factory_core::status`.
    pub fn fleet_status(&self) -> Result<Vec<ProjectStatusRows>> {
        let mut projects = self.connection.prepare(
            "SELECT id, name, root, completion_verification, created_at_ms, updated_at_ms
             FROM projects ORDER BY created_at_ms, id",
        )?;
        let projects = projects
            .query_map([], |row| {
                Ok(ProjectSnapshot {
                    id: parse_id(row.get(0)?, 0)?,
                    name: row.get(1)?,
                    root: row.get(2)?,
                    completion_verification: parse_completion_verification(row.get(3)?, 3)?,
                    created_at_ms: row.get(4)?,
                    updated_at_ms: row.get(5)?,
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
        let attention = latest_run
            .as_ref()
            .map_or(Attention::Routine, run_attention);
        Ok(AgentStatus {
            agent,
            budget: self.agent_budget(project_id, agent_id)?,
            pause_reasons: agent_pause_reasons(&self.connection, project_id, agent_id)?,
            current_run,
            latest_run,
            queue_depth: u32::try_from(queue.len()).unwrap_or(u32::MAX),
            queue: queue.into_iter().take(MAX_QUEUE_PREVIEW).collect(),
            inbox_pending: u32::try_from(inbox_pending).unwrap_or(u32::MAX),
            attention,
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
                 ORDER BY admitted_at_ms DESC, id DESC
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
        let default_execution_mode = ExecutionMode::default_for_provider(agent.snapshot.provider);
        Ok(AgentDetail {
            snapshot: agent.snapshot,
            profile: load_agent_profile(&self.connection, agent_id)?.unwrap_or(AgentProfile {
                model: None,
                reasoning_effort: None,
                model_selection_reason: None,
                execution_mode: default_execution_mode,
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
        if !input.execution_mode.supported_by(agent.snapshot.provider) {
            return Err(StoreError::UnsupportedAgentExecutionMode {
                provider: agent.snapshot.provider,
                mode: input.execution_mode,
            });
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
                execution_mode, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(agent_id) DO UPDATE SET
                model = excluded.model,
                reasoning_effort = excluded.reasoning_effort,
                model_selection_reason = excluded.model_selection_reason,
                execution_mode = excluded.execution_mode,
                updated_at_ms = excluded.updated_at_ms",
            params![
                agent_id.as_str(),
                selection.model,
                selection.reasoning_effort,
                selection.reason,
                input.execution_mode.as_str(),
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
            "SELECT id, occurred_at_ms, schema_version, kind, payload_json,
                    project_id, task_id, agent_id, run_id
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
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?;
        let stored = rows.collect::<std::result::Result<Vec<_>, _>>()?;

        let mut expected = sequence;
        for (found, ..) in &stored {
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
            .map(
                |(
                    sequence,
                    occurred_at_ms,
                    version,
                    kind,
                    payload,
                    project_id,
                    task_id,
                    agent_id,
                    run_id,
                )| {
                    let protocol_version = u16::try_from(version)
                        .map_err(|_| StoreError::CorruptProtocolVersion(version))?;
                    Ok(EventEnvelope {
                        protocol_version,
                        sequence,
                        occurred_at_ms,
                        event: decode_stored_event(
                            protocol_version,
                            &kind,
                            &payload,
                            project_id,
                            task_id,
                            agent_id,
                            run_id,
                        )?,
                    })
                },
            )
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

fn decode_stored_event(
    protocol_version: u16,
    kind: &str,
    payload: &str,
    project_id: Option<String>,
    task_id: Option<String>,
    agent_id: Option<String>,
    run_id: Option<String>,
) -> Result<FactoryEvent> {
    if protocol_version >= PROTOCOL_VERSION {
        return serde_json::from_str(payload).map_err(StoreError::from);
    }
    if kind == "run_changed" {
        let project_id = project_id
            .as_deref()
            .and_then(|value| ProjectId::try_from(value).ok())
            .ok_or(StoreError::CorruptLegacyEvent)?;
        let task_id = task_id
            .as_deref()
            .map(TaskId::try_from)
            .transpose()
            .map_err(|_| StoreError::CorruptLegacyEvent)?;
        let agent_id = agent_id
            .as_deref()
            .and_then(|value| AgentId::try_from(value).ok())
            .ok_or(StoreError::CorruptLegacyEvent)?;
        let run_id = run_id
            .as_deref()
            .and_then(|value| RunId::try_from(value).ok())
            .ok_or(StoreError::CorruptLegacyEvent)?;
        return Ok(FactoryEvent::LegacyRunChanged {
            project_id,
            task_id,
            agent_id,
            run_id,
        });
    }
    if kind == "session_changed" {
        let project_id = project_id
            .as_deref()
            .and_then(|value| ProjectId::try_from(value).ok())
            .ok_or(StoreError::CorruptLegacyEvent)?;
        let agent_id = agent_id
            .as_deref()
            .and_then(|value| AgentId::try_from(value).ok())
            .ok_or(StoreError::CorruptLegacyEvent)?;
        let run_id = run_id
            .as_deref()
            .map(RunId::try_from)
            .transpose()
            .map_err(|_| StoreError::CorruptLegacyEvent)?;
        return Ok(FactoryEvent::LegacySessionChanged {
            project_id,
            agent_id,
            run_id,
        });
    }
    serde_json::from_str(payload).map_err(StoreError::from)
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

fn validate_agent_profile(input: &UpdateAgentProfile) -> Result<()> {
    validate_agent_model(input.model.as_deref())
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

struct AgentRecord {
    snapshot: AgentSnapshot,
}

fn load_agent(connection: &Connection, agent_id: &AgentId) -> Result<Option<AgentRecord>> {
    connection
        .query_row(
            "SELECT a.id, a.project_id, a.parent_agent_id, a.role, a.provider,
                    (a.paused OR COALESCE((SELECT b.exhausted FROM agent_budgets b WHERE b.agent_id = a.id), 0)),
                    a.created_at_ms, a.updated_at_ms,
                    (SELECT r.id FROM runs r
                     WHERE r.agent_id = a.id
                       AND r.phase <> 'terminal'
                     ORDER BY r.admitted_at_ms DESC
                     LIMIT 1)
             FROM agents a
             WHERE a.id = ?1",
            params![agent_id.as_str()],
            |row| {
                let parent_agent_id: Option<String> = row.get(2)?;
                let role: String = row.get(3)?;
                let provider: String = row.get(4)?;
                let current_run_id: Option<String> = row.get(8)?;
                Ok(AgentRecord {
                    snapshot: AgentSnapshot {
                        id: parse_id(row.get(0)?, 0)?,
                        project_id: parse_id(row.get(1)?, 1)?,
                        parent_agent_id: parse_optional_id(parent_agent_id, 2)?,
                        role: parse_agent_role(&role, 3)?,
                        provider: parse_provider(&provider, 4)?,
                        current_run_id: parse_optional_id(current_run_id, 8)?,
                        paused: row.get(5)?,
                        created_at_ms: row.get(6)?,
                        updated_at_ms: row.get(7)?,
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
            WHERE project_id = ?1 AND phase <> 'terminal'
         )",
        params![project_id.as_str()],
        |row| row.get(0),
    )?;
    if has_active_run {
        return Err(StoreError::ProjectHasActiveRun);
    }
    let has_change_metadata: bool = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM changes WHERE project_id = ?1 AND phase <> 'removed'
             UNION ALL
             SELECT 1 FROM legacy_sources WHERE project_id = ?1
         )",
        params![project_id.as_str()],
        |row| row.get(0),
    )?;
    if has_change_metadata {
        return Err(StoreError::ProjectHasChanges);
    }
    let has_rust_artifacts: bool = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM rust_build_caches
             WHERE project_id = ?1
         )",
        params![project_id.as_str()],
        |row| row.get(0),
    )?;
    if has_rust_artifacts {
        return Err(StoreError::ProjectHasRustCaches);
    }
    Ok(())
}

fn load_agent_profile(connection: &Connection, agent_id: &AgentId) -> Result<Option<AgentProfile>> {
    connection
        .query_row(
            "SELECT model, reasoning_effort, model_selection_reason,
                    execution_mode, updated_at_ms
             FROM agent_profiles WHERE agent_id = ?1",
            params![agent_id.as_str()],
            |row| {
                Ok(AgentProfile {
                    model: row.get(0)?,
                    reasoning_effort: row.get(1)?,
                    model_selection_reason: row.get(2)?,
                    execution_mode: parse_execution_mode(&row.get::<_, String>(3)?, 3)?,
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

fn assign_task_in_transaction(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    task_id: &TaskId,
    agent_id: Option<&AgentId>,
    now_ms: i64,
) -> Result<(TaskDetail, FactoryEvent)> {
    let task = load_task(transaction, task_id)?
        .filter(|task| task.snapshot.project_id == *project_id)
        .ok_or(StoreError::TaskNotFound)?;
    if task.snapshot.status != TaskStatus::Queued {
        return Err(StoreError::TaskNotQueued);
    }
    if let Some(agent_id) = agent_id {
        let exists = load_agent(transaction, agent_id)?
            .is_some_and(|agent| agent.snapshot.project_id == *project_id);
        if !exists {
            return Err(StoreError::AgentNotFound);
        }
    }
    let changed = transaction.execute(
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
    if changed != 1 {
        return Err(StoreError::TaskNotQueued);
    }
    let task = load_task(transaction, task_id)?.ok_or(StoreError::TaskNotFound)?;
    let event = FactoryEvent::TaskChanged {
        task: task.snapshot.clone(),
    };
    Ok((task, event))
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
    kernel::load_kernel_run(connection, run_id)
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
                    body, created_at_ms, delivered_at_ms, delivered_run_id
             FROM agent_messages WHERE id = ?1",
            params![message_id.as_str()],
            agent_message_from_row,
        )
        .optional()
        .map_err(StoreError::from)
}

fn insert_agent_message(
    transaction: &Transaction<'_>,
    input: NewAgentMessage,
) -> Result<AgentMessage> {
    validate_agent_message(&input.body, input.created_at_ms)?;
    load_agent(transaction, &input.recipient_agent_id)?
        .filter(|agent| agent.snapshot.project_id == input.project_id)
        .ok_or(StoreError::AgentNotFound)?;
    if let Some(sender_agent_id) = &input.sender_agent_id {
        load_agent(transaction, sender_agent_id)?
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
    load_agent_message(transaction, &input.id)?.ok_or(StoreError::InvalidAgentMessage)
}

fn agent_message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentMessage> {
    let sender_agent_id: Option<String> = row.get(2)?;
    let delivered_run_id: Option<String> = row.get(7)?;
    Ok(AgentMessage {
        id: parse_id(row.get(0)?, 0)?,
        project_id: parse_id(row.get(1)?, 1)?,
        sender_agent_id: parse_optional_id(sender_agent_id, 2)?,
        recipient_agent_id: parse_id(row.get(3)?, 3)?,
        body: row.get(4)?,
        created_at_ms: row.get(5)?,
        delivered_at_ms: row.get(6)?,
        delivered_run_id: parse_optional_id(delivered_run_id, 7)?,
    })
}

fn migrate(connection: &mut Connection) -> Result<()> {
    migrate_to(connection, SCHEMA_VERSION)
}

fn migrate_to(connection: &mut Connection, target: i64) -> Result<()> {
    debug_assert!(matches!(target, 29 | 30 | 31 | 32 | 33 | SCHEMA_VERSION));
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
        transaction.pragma_update(None, "user_version", 29)?;
        transaction.commit()?;
        verify_no_foreign_key_violations(connection)?;
        current = 29;
    }
    if current == 29 && target >= 30 {
        ensure_kernel_migration_quiescent(connection)?;
        connection.pragma_update(None, "foreign_keys", false)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0030_attempt_kernel.sql"))?;
        transaction.pragma_update(None, "user_version", 30)?;
        transaction.commit()?;
        connection.pragma_update(None, "foreign_keys", true)?;
        verify_no_foreign_key_violations(connection)?;
        current = 30;
    }
    if current == 30 && target >= 31 {
        ensure_change_migration_empty_authority(connection)?;
        connection.pragma_update(None, "foreign_keys", false)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0031_daemon_owned_changes.sql"))?;
        transaction.pragma_update(None, "user_version", 31)?;
        transaction.commit()?;
        connection.pragma_update(None, "foreign_keys", true)?;
        verify_no_foreign_key_violations(connection)?;
        current = 31;
    }
    if current == 31 && target >= 32 {
        connection.pragma_update(None, "foreign_keys", false)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!("../migrations/0032_bounded_rust_builds.sql"))?;
        transaction.pragma_update(None, "user_version", 32)?;
        transaction.commit()?;
        connection.pragma_update(None, "foreign_keys", true)?;
        verify_no_foreign_key_violations(connection)?;
        current = 32;
    }
    if current == 32 && target >= 33 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!(
            "../migrations/0033_retire_webhook_document_delete_guard.sql"
        ))?;
        transaction.pragma_update(None, "user_version", 33)?;
        transaction.commit()?;
        verify_no_foreign_key_violations(connection)?;
        current = 33;
    }
    if current == 33 && target == SCHEMA_VERSION {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(include_str!(
            "../migrations/0034_dispatch_execution_modes.sql"
        ))?;
        transaction.pragma_update(None, "user_version", 34)?;
        transaction.commit()?;
        verify_no_foreign_key_violations(connection)?;
    }
    Ok(())
}

fn ensure_change_migration_empty_authority(connection: &Connection) -> Result<()> {
    let linked_runs = connection.query_row(
        "SELECT COUNT(*) FROM runs WHERE change_id IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if linked_runs != 0 {
        return Err(StoreError::ChangeMigrationRequiresEmptyAuthority { linked_runs });
    }
    Ok(())
}

fn ensure_kernel_migration_quiescent(connection: &Connection) -> Result<()> {
    let live_sessions = connection.query_row(
        "SELECT COUNT(*) FROM sessions WHERE ended_at_ms IS NULL",
        [],
        |row| row.get(0),
    )?;
    let owned_session_work = connection.query_row(
        "SELECT COUNT(*) FROM session_work
         WHERE state <> 'empty' OR quarantine_reason IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    let active_deliveries = connection.query_row(
        "SELECT COUNT(*) FROM delivery_attempts
         WHERE state IN ('in_flight', 'retryable', 'terminal')",
        [],
        |row| row.get(0),
    )?;
    let nonterminal_runs = connection.query_row(
        "SELECT COUNT(*) FROM runs WHERE ended_at_ms IS NULL",
        [],
        |row| row.get(0),
    )?;
    let taskless_runs = connection.query_row(
        "SELECT COUNT(*) FROM runs WHERE task_id IS NULL",
        [],
        |row| row.get(0),
    )?;
    if live_sessions == 0
        && owned_session_work == 0
        && active_deliveries == 0
        && nonterminal_runs == 0
        && taskless_runs == 0
    {
        Ok(())
    } else {
        Err(StoreError::KernelMigrationRequiresQuiescence {
            live_sessions,
            owned_session_work,
            active_deliveries,
            nonterminal_runs,
            taskless_runs,
        })
    }
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

/// Moves legacy profile text into the filesystem before its schema columns
/// are removed.
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
        FactoryEvent::DispatchPolicyChanged { .. } | FactoryEvent::LegacyAutoModeChanged { .. } => {
            EventMetadata {
                project_id: None,
                task_id: None,
                agent_id: None,
                run_id: None,
            }
        }
        FactoryEvent::PolicyDecision {
            project_id,
            agent_id,
            run_id,
            ..
        } => EventMetadata {
            project_id: Some(project_id),
            task_id: None,
            agent_id: Some(agent_id),
            run_id: Some(run_id),
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
        FactoryEvent::LegacyRepositoryOperation {
            project_id,
            agent_id,
            run_id,
            ..
        } => EventMetadata {
            project_id: Some(project_id),
            task_id: None,
            agent_id: Some(agent_id),
            run_id: Some(run_id),
        },
        FactoryEvent::LegacyRepositoryAuthorityChanged { project_id } => EventMetadata {
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
            task_id: Some(&run.task_id),
            agent_id: Some(&run.agent_id),
            run_id: Some(&run.id),
        },
        FactoryEvent::ChangeChanged { change } => EventMetadata {
            project_id: Some(&change.project_id),
            task_id: Some(&change.task_id),
            agent_id: None,
            run_id: None,
        },
        FactoryEvent::LegacySourceForgotten { project_id, .. } => EventMetadata {
            project_id: Some(project_id),
            task_id: None,
            agent_id: None,
            run_id: None,
        },
        FactoryEvent::LegacyRunChanged {
            project_id,
            task_id,
            agent_id,
            run_id,
        } => EventMetadata {
            project_id: Some(project_id),
            task_id: task_id.as_ref(),
            agent_id: Some(agent_id),
            run_id: Some(run_id),
        },
        FactoryEvent::LegacySessionChanged {
            project_id,
            agent_id,
            run_id,
        } => EventMetadata {
            project_id: Some(project_id),
            task_id: None,
            agent_id: Some(agent_id),
            run_id: run_id.as_ref(),
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

fn parse_completion_verification(
    value: String,
    column: usize,
) -> rusqlite::Result<factory_core::CompletionVerification> {
    serde_json::from_value(serde_json::Value::String(value)).map_err(|error| {
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

fn parse_execution_mode(value: &str, column: usize) -> rusqlite::Result<ExecutionMode> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
}

fn parse_observer_health(value: &str, column: usize) -> rusqlite::Result<ObserverHealth> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(column, Type::Text, Box::new(error))
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_defaults_on_and_changes_with_an_audit_event() {
        let mut store = Store::open_in_memory().unwrap();
        assert!(store.dispatch_enabled().unwrap());
        let event = store.set_dispatch_enabled(false, 42).unwrap();
        assert!(!store.dispatch_enabled().unwrap());
        assert_eq!(
            event.event,
            FactoryEvent::DispatchPolicyChanged { enabled: false }
        );
        assert_eq!(store.events_after(0, 10).unwrap(), vec![event]);
    }

    #[test]
    fn schema_34_separates_dispatch_from_explicit_profile_authority() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;",
            )
            .unwrap();
        migrate_to(&mut connection, 33).unwrap();
        connection
            .execute_batch(
                "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                 VALUES ('project', 'Project', '/tmp/project', 1, 1);
                 INSERT INTO agents (
                     id, project_id, parent_agent_id, role, provider, paused,
                     created_at_ms, updated_at_ms
                 ) VALUES
                     ('claude-plan', 'project', NULL, 'worker', 'claude_code', 0, 2, 2),
                     ('codex-default', 'project', NULL, 'worker', 'codex', 0, 2, 2),
                     ('codex-never', 'project', NULL, 'worker', 'codex', 0, 2, 2),
                     ('claude-edits', 'project', NULL, 'worker', 'claude_code', 0, 2, 2),
                     ('shell', 'project', NULL, 'worker', 'shell', 0, 2, 2);
                 INSERT INTO agent_profiles (
                     agent_id, model, updated_at_ms, permission_mode,
                     reasoning_effort, model_selection_reason
                 ) VALUES
                     ('claude-plan', NULL, 2, 'plan', NULL, NULL),
                     ('codex-default', NULL, 2, NULL, NULL, NULL),
                     ('codex-never', NULL, 2, 'never', NULL, NULL),
                     ('claude-edits', NULL, 2, 'acceptEdits', NULL, NULL),
                     ('shell', NULL, 2, NULL, NULL, NULL);
                 INSERT INTO tasks (
                     id, project_id, assigned_agent_id, title, body, status,
                     priority, created_at_ms, updated_at_ms, completed_at_ms,
                     incarnation_id, work_revision
                 ) VALUES
                     ('task-plan', 'project', 'claude-plan', 'Plan', 'Body',
                      'succeeded', 0, 2, 4, 4, 'task-plan-incarnation', 0),
                     ('task-unknown', 'project', 'codex-default', 'Unknown', 'Body',
                      'succeeded', 0, 2, 4, 4, 'task-unknown-incarnation', 0),
                     ('task-cross-provider', 'project', 'codex-never', 'Cross', 'Body',
                      'succeeded', 0, 2, 4, 4, 'task-cross-incarnation', 0);
                 INSERT INTO runs (
                     id, project_id, agent_id, task_id, task_incarnation_id,
                     admitted_task_work_revision, source_root, phase, outcome,
                     provider, runtime_permission_mode, observer_health,
                     admitted_at_ms, running_at_ms, finalizing_at_ms,
                     phase_since_ms, updated_at_ms, ended_at_ms
                 ) VALUES
                     ('run-plan', 'project', 'claude-plan', 'task-plan',
                      'task-plan-incarnation', 0, '/tmp/source-plan', 'terminal',
                      'succeeded', 'claude_code', 'plan', 'healthy', 2, 2, 3, 3, 4, 4),
                     ('run-unknown', 'project', 'codex-default', 'task-unknown',
                      'task-unknown-incarnation', 0, '/tmp/source-unknown', 'terminal',
                      'succeeded', 'codex', NULL, 'healthy', 2, 2, 3, 3, 4, 4),
                     ('run-cross-provider', 'project', 'codex-never',
                      'task-cross-provider', 'task-cross-incarnation', 0,
                      '/tmp/source-cross', 'terminal', 'succeeded', 'codex',
                      'acceptEdits', 'healthy', 2, 2, 3, 3, 4, 4);
                 UPDATE factory_settings
                 SET auto_mode = 1, updated_at_ms = 3
                 WHERE singleton = 1;",
            )
            .unwrap();

        migrate_to(&mut connection, SCHEMA_VERSION).unwrap();
        let store = Store { connection };
        let project_id = ProjectId::try_from("project").unwrap();
        let mode = |id: &str| {
            store
                .get_agent_detail(&project_id, &AgentId::try_from(id).unwrap())
                .unwrap()
                .profile
                .execution_mode
        };

        assert!(store.dispatch_enabled().unwrap());
        assert_eq!(mode("claude-plan"), ExecutionMode::PlanOnly);
        assert_eq!(mode("codex-default"), ExecutionMode::Unrestricted);
        assert_eq!(mode("codex-never"), ExecutionMode::WorkspaceWrite);
        assert_eq!(mode("claude-edits"), ExecutionMode::WorkspaceWrite);
        assert_eq!(mode("shell"), ExecutionMode::Unrestricted);
        let runs = store.list_runs(&project_id, None, 10).unwrap();
        let runtime_mode = |id: &str| {
            runs.iter()
                .find(|run| run.id.as_str() == id)
                .unwrap()
                .runtime_execution_mode
        };
        assert_eq!(runtime_mode("run-plan"), Some(ExecutionMode::PlanOnly));
        assert_eq!(runtime_mode("run-unknown"), None);
        assert_eq!(runtime_mode("run-cross-provider"), None);
        for (table, removed, added) in [
            ("factory_settings", "auto_mode", "dispatch_enabled"),
            ("agent_profiles", "permission_mode", "execution_mode"),
            ("runs", "runtime_permission_mode", "runtime_execution_mode"),
        ] {
            let columns = store
                .connection
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert!(!columns.iter().any(|column| column == removed));
            assert!(columns.iter().any(|column| column == added));
        }
    }

    #[test]
    fn schema_34_rejects_unknown_or_cross_provider_legacy_profile_authority() {
        for (provider, permission_mode) in [
            ("codex", "corrupt-legacy-mode"),
            ("codex", "unrestricted"),
            ("claude_code", "never"),
            ("codex", "acceptEdits"),
        ] {
            let mut connection = Connection::open_in_memory().unwrap();
            connection
                .execute_batch(
                    "PRAGMA foreign_keys = ON;
                     PRAGMA journal_mode = WAL;
                     PRAGMA synchronous = FULL;",
                )
                .unwrap();
            migrate_to(&mut connection, 33).unwrap();
            connection
                .execute_batch(
                    "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                     VALUES ('project', 'Project', '/tmp/project', 1, 1);
                     UPDATE factory_settings
                     SET auto_mode = 1, updated_at_ms = 3
                     WHERE singleton = 1;",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO agents (
                         id, project_id, parent_agent_id, role, provider, paused,
                         created_at_ms, updated_at_ms
                     ) VALUES ('agent', 'project', NULL, 'worker', ?1, 0, 2, 2)",
                    [provider],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO agent_profiles (
                         agent_id, model, updated_at_ms, permission_mode,
                         reasoning_effort, model_selection_reason
                     ) VALUES ('agent', NULL, 2, ?1, NULL, NULL)",
                    [permission_mode],
                )
                .unwrap();

            let error = migrate_to(&mut connection, SCHEMA_VERSION).unwrap_err();
            assert!(
                error.to_string().contains("CHECK constraint failed"),
                "legacy authority {provider}={permission_mode:?} must abort migration, got {error}"
            );
        }
    }

    #[test]
    fn an_unknown_persisted_execution_mode_fails_closed() {
        let mut store = Store::open_in_memory().unwrap();
        let project_id = ProjectId::try_from("project").unwrap();
        let agent_id = AgentId::try_from("agent").unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Project".into(),
                    root: "/tmp/project".into(),
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
                    provider: Provider::Codex,
                },
                2,
            )
            .unwrap();
        store
            .connection
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE agent_profiles SET execution_mode = 'interactive' WHERE agent_id = ?1",
                [agent_id.as_str()],
            )
            .unwrap();

        assert!(store.get_agent_detail(&project_id, &agent_id).is_err());
    }

    #[test]
    fn rejects_a_newer_or_negative_schema() {
        for version in [99, -1] {
            let mut connection = Connection::open_in_memory().unwrap();
            connection
                .pragma_update(None, "user_version", version)
                .unwrap();
            let error = migrate(&mut connection).unwrap_err();
            assert!(matches!(
                (version, error),
                (
                    99,
                    StoreError::UnsupportedSchema {
                        found: 99,
                        supported: SCHEMA_VERSION
                    }
                ) | (-1, StoreError::InvalidSchemaVersion(-1))
            ));
        }
    }

    fn schema_29_with_two_terminal_runs_and_legacy_event() -> Store {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;",
            )
            .unwrap();
        migrate_to(&mut connection, 29).unwrap();
        let store = Store { connection };
        let project_id = ProjectId::try_from("project-1").unwrap();
        let agent_id = AgentId::try_from("worker-1").unwrap();
        let first_task_id = TaskId::try_from("task-1").unwrap();
        let second_task_id = TaskId::try_from("task-2").unwrap();
        store
            .connection
            .execute(
                "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                 VALUES (?1, 'Project', '/tmp/project-1', 1, 1)",
                params![project_id.as_str()],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                 VALUES ('project-2', 'Other project', '/tmp/project-2', 1, 1)",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO agents (
                    id, project_id, role, provider, paused, worktree,
                    created_at_ms, updated_at_ms
                 ) VALUES ('worker-3', 'project-2', 'worker', 'shell', 0,
                           '/tmp/shared-legacy-source', 2, 2)",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO agents (
                    id, project_id, role, provider, paused, worktree,
                    created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, 'worker', 'shell', 0,
                           '/tmp/shared-legacy-source', 2, 2)",
                params![agent_id.as_str(), project_id.as_str()],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO agents (
                    id, project_id, role, provider, paused, worktree,
                    created_at_ms, updated_at_ms
                 ) VALUES ('worker-2', ?1, 'worker', 'shell', 0,
                           '/tmp/shared-legacy-source', 2, 2)",
                params![project_id.as_str()],
            )
            .unwrap();
        for (task_id, now_ms) in [(&first_task_id, 3), (&second_task_id, 4)] {
            store
                .connection
                .execute(
                    "INSERT INTO tasks (
                        id, project_id, assigned_agent_id, title, body, status,
                        priority, created_at_ms, updated_at_ms, completed_at_ms,
                        incarnation_id, work_revision
                     ) VALUES (?1, ?2, ?3, ?4, 'body', 'succeeded',
                               1, ?5, 10, 10, ?6, 0)",
                    params![
                        task_id.as_str(),
                        project_id.as_str(),
                        agent_id.as_str(),
                        format!("Task {now_ms}"),
                        now_ms,
                        format!("incarnation-{now_ms}"),
                    ],
                )
                .unwrap();
        }
        for (task_id, status, blocked_reason, now_ms) in [
            (
                "task-blocked",
                "blocked",
                Some("needs operator input"),
                5_i64,
            ),
            ("task-cancelled", "cancelled", None, 6_i64),
            ("task-failed", "failed", None, 7_i64),
        ] {
            store
                .connection
                .execute(
                    "INSERT INTO tasks (
                        id, project_id, assigned_agent_id, title, body, status,
                        priority, created_at_ms, updated_at_ms, completed_at_ms,
                        blocked_reason, incarnation_id, work_revision
                     ) VALUES (?1, ?2, ?3, ?4, 'body', ?5,
                               1, ?6, 20, 20, ?7, ?8, 0)",
                    params![
                        task_id,
                        project_id.as_str(),
                        agent_id.as_str(),
                        format!("Task {now_ms}"),
                        status,
                        now_ms,
                        blocked_reason,
                        format!("incarnation-{now_ms}"),
                    ],
                )
                .unwrap();
        }
        store
            .connection
            .execute(
                "INSERT INTO sessions (
                    id, project_id, agent_id, provider, worktree, hook_token,
                    state, state_since_ms, observer_health, observer_health_since_ms,
                    runner_instance_id, runner_runtime, runner_protocol_version,
                    started_at_ms, updated_at_ms, ended_at_ms, exit_code
                 ) VALUES (
                    'session-1', ?1, ?2, 'shell', '/tmp/legacy-worker', ?3,
                    'stopped', 10, 'healthy', 10,
                    'runner-instance-1', '/tmp/legacy-runner', 1,
                    5, 10, 10, 0
                 )",
                params![project_id.as_str(), agent_id.as_str(), "a".repeat(64)],
            )
            .unwrap();
        for (run_id, task_id, started_at_ms) in [
            ("run-1", &first_task_id, 6_i64),
            ("run-2", &second_task_id, 8_i64),
        ] {
            store
                .connection
                .execute(
                    "INSERT INTO runs (
                        id, project_id, agent_id, session_id, task_id, status,
                        worktree, started_at_ms, status_since_ms, updated_at_ms,
                        ended_at_ms, closed_by
                     ) VALUES (
                        ?1, ?2, ?3, 'session-1', ?4, 'succeeded',
                        '/tmp/legacy-worker', ?5, ?6, ?6, ?6, 'task_done'
                     )",
                    params![
                        run_id,
                        project_id.as_str(),
                        agent_id.as_str(),
                        task_id.as_str(),
                        started_at_ms,
                        started_at_ms + 1,
                    ],
                )
                .unwrap();
        }
        for (run_id, task_id, status, closed_by, failure_reason, started_at_ms) in [
            (
                "run-blocked",
                "task-blocked",
                "stopped",
                "task_blocked",
                None,
                11_i64,
            ),
            (
                "run-cancelled",
                "task-cancelled",
                "stopped",
                "operator_cancel",
                None,
                13_i64,
            ),
            (
                "run-failed",
                "task-failed",
                "failed",
                "session_ended",
                Some("provider"),
                15_i64,
            ),
        ] {
            store
                .connection
                .execute(
                    "INSERT INTO runs (
                        id, project_id, agent_id, session_id, task_id, status,
                        worktree, started_at_ms, status_since_ms, updated_at_ms,
                        ended_at_ms, closed_by, failure_reason
                     ) VALUES (
                        ?1, ?2, ?3, 'session-1', ?4, ?5,
                        '/tmp/legacy-worker', ?6, ?7, ?7, ?7, ?8, ?9
                     )",
                    params![
                        run_id,
                        project_id.as_str(),
                        agent_id.as_str(),
                        task_id,
                        status,
                        started_at_ms,
                        started_at_ms + 1,
                        closed_by,
                        failure_reason,
                    ],
                )
                .unwrap();
        }
        store.connection.execute("DELETE FROM events", []).unwrap();
        let legacy_payload = serde_json::json!({
            "type": "run_changed",
            "data": {
                "run": {
                    "id": "run-1",
                    "project_id": "project-1",
                    "agent_id": "worker-1",
                    "task_id": "task-1",
                    "session_id": "session-1",
                    "status": "succeeded",
                    "worktree": "/tmp/legacy-worker",
                    "observer_health": "healthy",
                    "observer_health_since_ms": 6,
                    "started_at_ms": 6,
                    "status_since_ms": 7,
                    "updated_at_ms": 7,
                    "ended_at_ms": 7,
                    "closed_by": "task_done"
                }
            }
        });
        store
            .connection
            .execute(
                "INSERT INTO events (
                    occurred_at_ms, project_id, task_id, agent_id, run_id,
                    kind, schema_version, payload_json
                 ) VALUES (7, ?1, ?2, ?3, 'run-1', 'run_changed', 2, ?4)",
                params![
                    project_id.as_str(),
                    first_task_id.as_str(),
                    agent_id.as_str(),
                    legacy_payload.to_string(),
                ],
            )
            .unwrap();
        let legacy_session_payload = serde_json::json!({
            "type": "session_changed",
            "data": {
                "session": {
                    "id": "session-1",
                    "project_id": "project-1",
                    "agent_id": "worker-1",
                    "provider": "shell",
                    "state": "stopped",
                    "state_since_ms": 10,
                    "worktree": "/tmp/legacy-worker",
                    "runner_instance_id": "runner-instance-1",
                    "current_run_id": "run-1",
                    "last_hook_event": "stop",
                    "observer_health": "healthy",
                    "observer_health_since_ms": 10,
                    "started_at_ms": 5,
                    "updated_at_ms": 10,
                    "ended_at_ms": 10,
                    "exit_code": 0
                }
            }
        });
        store
            .connection
            .execute(
                "INSERT INTO events (
                    occurred_at_ms, project_id, task_id, agent_id, run_id,
                    kind, schema_version, payload_json
                 ) VALUES (10, ?1, NULL, ?2, 'run-1', 'session_changed', 2, ?3)",
                params![
                    project_id.as_str(),
                    agent_id.as_str(),
                    legacy_session_payload.to_string(),
                ],
            )
            .unwrap();
        store
    }

    #[test]
    fn kernel_migration_drops_shared_session_process_identity_from_historical_runs() {
        let mut store = schema_29_with_two_terminal_runs_and_legacy_event();
        migrate(&mut store.connection).unwrap();

        let migrated: (i64, i64) = store
            .connection
            .query_row(
                "SELECT COUNT(*),
                        COUNT(*) FILTER (
                            WHERE runner_instance_id IS NULL AND runner_runtime IS NULL
                        )
                 FROM runs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(migrated, (5, 5));
    }

    #[test]
    fn kernel_migration_preserves_legacy_block_cancel_and_failure_semantics() {
        let mut store = schema_29_with_two_terminal_runs_and_legacy_event();
        migrate(&mut store.connection).unwrap();

        let mut statement = store
            .connection
            .prepare(
                "SELECT id, outcome, outcome_detail, exit_code, exit_signal
                 FROM runs
                 WHERE id IN ('run-blocked', 'run-cancelled', 'run-failed')
                 ORDER BY id",
            )
            .unwrap();
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    "run-blocked".to_owned(),
                    "blocked".to_owned(),
                    Some("needs operator input".to_owned()),
                    None,
                    None,
                ),
                (
                    "run-cancelled".to_owned(),
                    "cancelled".to_owned(),
                    Some("operator_cancel".to_owned()),
                    None,
                    None,
                ),
                (
                    "run-failed".to_owned(),
                    "failed".to_owned(),
                    Some("provider".to_owned()),
                    None,
                    None,
                ),
            ]
        );
    }

    #[test]
    fn change_migration_quarantines_every_shared_legacy_source_association() {
        let mut store = schema_29_with_two_terminal_runs_and_legacy_event();
        migrate(&mut store.connection).unwrap();

        let migrated: (i64, i64, i64) = store
            .connection
            .query_row(
                "SELECT COUNT(*),
                        COUNT(*) FILTER (WHERE project_id = 'project-1'),
                        COUNT(*) FILTER (WHERE project_id = 'project-2')
                 FROM legacy_sources
                 WHERE source_path = '/tmp/shared-legacy-source'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(migrated, (3, 2, 1));
    }

    #[test]
    fn actual_chain_preserves_max_valid_agent_id_in_bounded_legacy_metadata() {
        let mut store = schema_29_with_two_terminal_runs_and_legacy_event();
        let max_agent_id = "a".repeat(128);
        store
            .connection
            .execute(
                "INSERT INTO agents (
                    id, project_id, role, provider, paused, worktree,
                    created_at_ms, updated_at_ms
                 ) VALUES (?1, 'project-1', 'worker', 'shell', 0,
                           '/tmp/max-id-legacy-source', 3, 3)",
                [&max_agent_id],
            )
            .unwrap();

        migrate(&mut store.connection).unwrap();

        let project_id = ProjectId::try_from("project-1").unwrap();
        let migrated = store
            .list_legacy_sources(&project_id, None, MAX_STATE_PAGE)
            .unwrap()
            .into_iter()
            .find(|source| {
                source
                    .former_agent_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == max_agent_id)
            })
            .unwrap();
        assert!(migrated.id.as_str().len() <= 128);
        assert_eq!(migrated.source_path, "/tmp/max-id-legacy-source");
    }

    #[test]
    fn legacy_source_listing_is_bounded_and_cursor_paginated() {
        let mut store = schema_29_with_two_terminal_runs_and_legacy_event();
        migrate(&mut store.connection).unwrap();
        let project_id = ProjectId::try_from("project-1").unwrap();

        let first = store.list_legacy_sources(&project_id, None, 1).unwrap();
        assert_eq!(first.len(), 1);
        let second = store
            .list_legacy_sources(&project_id, Some(&first[0].id), 1)
            .unwrap();
        assert_eq!(second.len(), 1);
        assert!(first[0].id < second[0].id);
        assert!(matches!(
            store.list_legacy_sources(&project_id, None, 0),
            Err(StoreError::InvalidStateLimit)
        ));
        assert!(matches!(
            store.list_legacy_sources(&project_id, None, MAX_STATE_PAGE + 1),
            Err(StoreError::InvalidStateLimit)
        ));
    }

    #[test]
    fn forgetting_legacy_metadata_never_touches_its_source_path() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("preserved-source");
        std::fs::write(&source, b"preserve me").unwrap();
        let source = source.to_string_lossy().into_owned();
        let mut store = schema_29_with_two_terminal_runs_and_legacy_event();
        store
            .connection
            .execute(
                "UPDATE agents SET worktree = ?1 WHERE id = 'worker-1'",
                [&source],
            )
            .unwrap();
        migrate(&mut store.connection).unwrap();
        let project_id = ProjectId::try_from("project-1").unwrap();
        assert!(matches!(
            store.check_project_deletable(&project_id),
            Err(StoreError::ProjectHasChanges)
        ));
        let sources = store
            .list_legacy_sources(&project_id, None, MAX_STATE_PAGE)
            .unwrap();
        let retained_id = sources
            .iter()
            .find(|item| {
                item.former_agent_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == "worker-1")
            })
            .unwrap()
            .id
            .clone();
        let event = store
            .forget_legacy_source(&project_id, &retained_id, 11)
            .unwrap();
        assert!(matches!(
            &event.event,
            FactoryEvent::LegacySourceForgotten {
                project_id: event_project,
                legacy_source_id,
            } if event_project == &project_id && legacy_source_id == &retained_id
        ));
        assert_eq!(
            store.events_after(event.sequence - 1, 1).unwrap(),
            vec![event]
        );

        assert_eq!(std::fs::read(&source).unwrap(), b"preserve me");
        assert!(
            store
                .list_legacy_sources(&project_id, None, MAX_STATE_PAGE)
                .unwrap()
                .iter()
                .all(|item| item.id != retained_id)
        );
    }

    #[test]
    fn change_migration_refuses_schema_30_linked_authority() {
        let mut store = schema_29_with_two_terminal_runs_and_legacy_event();
        migrate_to(&mut store.connection, 30).unwrap();
        let incarnation: String = store
            .connection
            .query_row(
                "SELECT incarnation_id FROM tasks WHERE id = 'task-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO changes (
                    id, project_id, task_id, task_incarnation_id, branch, worktree,
                    ready_at_ms, retained_reason, created_at_ms, updated_at_ms
                 ) VALUES (
                    'linked-change', 'project-1', 'task-1', ?1, NULL,
                    '/tmp/linked-change', 20, NULL, 20, 20
                 )",
                [incarnation],
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE runs SET change_id = 'linked-change' WHERE id = 'run-1'",
                [],
            )
            .unwrap();

        assert!(matches!(
            migrate(&mut store.connection),
            Err(StoreError::ChangeMigrationRequiresEmptyAuthority { linked_runs: 1 })
        ));
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 30);
    }

    #[test]
    fn kernel_migration_replays_schema_2_run_events_as_minimal_legacy_events() {
        let mut store = schema_29_with_two_terminal_runs_and_legacy_event();
        migrate(&mut store.connection).unwrap();

        let events = store.events_after(0, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].protocol_version, 2);
        assert!(matches!(
            &events[0].event,
            FactoryEvent::LegacyRunChanged {
                project_id,
                task_id: Some(task_id),
                agent_id,
                run_id,
            } if project_id.as_str() == "project-1"
                && task_id.as_str() == "task-1"
                && agent_id.as_str() == "worker-1"
                && run_id.as_str() == "run-1"
        ));
    }

    #[test]
    fn kernel_migration_replays_schema_2_session_events_as_minimal_legacy_events() {
        let mut store = schema_29_with_two_terminal_runs_and_legacy_event();
        migrate(&mut store.connection).unwrap();

        let events = store.events_after(0, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].protocol_version, 2);
        assert!(matches!(
            &events[1].event,
            FactoryEvent::LegacySessionChanged {
                project_id,
                agent_id,
                run_id: Some(run_id),
            } if project_id.as_str() == "project-1"
                && agent_id.as_str() == "worker-1"
                && run_id.as_str() == "run-1"
        ));
    }

    fn legacy_preflight_connection() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sessions (ended_at_ms INTEGER);
                 CREATE TABLE session_work (state TEXT, quarantine_reason TEXT);
                 CREATE TABLE delivery_attempts (state TEXT);
                 CREATE TABLE runs (ended_at_ms INTEGER, task_id TEXT);",
            )
            .unwrap();
        connection
    }

    #[test]
    fn kernel_migration_preflight_requires_every_legacy_authority_to_be_quiescent() {
        let connection = legacy_preflight_connection();
        connection
            .execute("INSERT INTO sessions VALUES (NULL)", [])
            .unwrap();
        connection
            .execute("INSERT INTO session_work VALUES ('running', NULL)", [])
            .unwrap();
        connection
            .execute("INSERT INTO delivery_attempts VALUES ('in_flight')", [])
            .unwrap();
        connection
            .execute("INSERT INTO runs VALUES (NULL, NULL)", [])
            .unwrap();

        assert!(matches!(
            ensure_kernel_migration_quiescent(&connection),
            Err(StoreError::KernelMigrationRequiresQuiescence {
                live_sessions: 1,
                owned_session_work: 1,
                active_deliveries: 1,
                nonterminal_runs: 1,
                taskless_runs: 1,
            })
        ));
    }

    #[test]
    fn kernel_migration_preflight_accepts_only_terminal_legacy_rows() {
        let connection = legacy_preflight_connection();
        connection
            .execute("INSERT INTO sessions VALUES (1)", [])
            .unwrap();
        connection
            .execute("INSERT INTO session_work VALUES ('empty', NULL)", [])
            .unwrap();
        connection
            .execute("INSERT INTO delivery_attempts VALUES ('acknowledged')", [])
            .unwrap();
        connection
            .execute("INSERT INTO runs VALUES (1, 'task-1')", [])
            .unwrap();

        ensure_kernel_migration_quiescent(&connection).unwrap();
    }

    #[test]
    fn fresh_schema_contains_kernel_tables_and_no_session_authority() {
        let store = Store::open_in_memory().unwrap();
        let version: i64 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        for table in ["changes", "legacy_sources", "runs", "resources"] {
            let exists: bool = store
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "missing kernel table {table}");
        }
        for table in [
            "sessions",
            "session_work",
            "delivery_attempts",
            "project_repository_authority",
            "rust_storage_policy",
        ] {
            let exists: bool = store
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!exists, "legacy authority table {table} survived");
        }
        let agent_worktree_columns: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('agents') WHERE name = 'worktree'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(agent_worktree_columns, 0);
    }

    #[test]
    fn schema_6_document_survives_upgrade_until_explicit_project_deletion() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .pragma_update(None, "foreign_keys", true)
            .unwrap();
        for migration in [
            include_str!("../migrations/0001_state_and_events.sql"),
            include_str!("../migrations/0002_execution_ledger.sql"),
            include_str!("../migrations/0003_runner_reconciliation.sql"),
            include_str!("../migrations/0004_observer_health.sql"),
            include_str!("../migrations/0005_provider_session_context.sql"),
            include_str!("../migrations/0006_webhooks.sql"),
        ] {
            connection.execute_batch(migration).unwrap();
        }
        connection.pragma_update(None, "user_version", 6).unwrap();
        connection
            .execute_batch(
                "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                 VALUES ('document-project', 'Documents', '/tmp/document-project', 1, 1);
                 INSERT INTO task_documents (
                     project_id, id, name, reference, revision, content
                 ) VALUES (
                     'document-project', 'document-1', 'Historical document',
                     'https://example.invalid/document-1', 'revision-1', 'preserve me'
                 );",
            )
            .unwrap();

        migrate(&mut connection).unwrap();

        let retained: String = connection
            .query_row(
                "SELECT content FROM task_documents
                 WHERE project_id = 'document-project' AND id = 'document-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, "preserve me");
        let delete_guard_exists: bool = connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_schema
                     WHERE type = 'trigger' AND name = 'task_documents_immutable_delete'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!delete_guard_exists);
        let update_guard_exists: bool = connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_schema
                     WHERE type = 'trigger' AND name = 'task_documents_immutable_update'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(update_guard_exists);

        let mut store = Store { connection };
        store
            .delete_project(&ProjectId::try_from("document-project").unwrap(), 2)
            .unwrap();
        let remaining: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM task_documents WHERE project_id = 'document-project'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn bounded_build_migration_preserves_schema_31_runs() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate_to(&mut connection, 31).unwrap();
        connection
            .execute_batch(
                "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                 VALUES ('project-1', 'Project', '/tmp/project-1', 1, 1);
                 INSERT INTO agents (
                    id, project_id, role, provider, paused, created_at_ms, updated_at_ms
                 ) VALUES ('worker-1', 'project-1', 'worker', 'shell', 0, 1, 1);
                 INSERT INTO tasks (
                    id, project_id, assigned_agent_id, title, body, status, priority,
                    created_at_ms, updated_at_ms, incarnation_id, work_revision
                 ) VALUES (
                    'task-1', 'project-1', 'worker-1', 'Task', 'Body', 'running', 0,
                    1, 1, 'task-incarnation-1', 0
                 );
                 INSERT INTO runs (
                    id, project_id, agent_id, task_id, task_incarnation_id,
                    admitted_task_work_revision, source_root, phase, outcome,
                    capability_digest, provider, runner_instance_id, runner_runtime,
                    runner_protocol_version, admitted_at_ms, running_at_ms,
                    finalizing_at_ms, phase_since_ms, updated_at_ms
                 ) VALUES (
                    '11111111-1111-4111-8111-111111111111', 'project-1', 'worker-1',
                    'task-1', 'task-incarnation-1', 0, '/tmp/source', 'finalizing',
                    'succeeded',
                    'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
                    'shell', '22222222-2222-4222-8222-222222222222', '/tmp/runner',
                    1, 1, 2, 3, 3, 3
                 );",
            )
            .unwrap();
        let runs_before: String = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'runs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let indexes_before: Vec<(String, String)> = {
            let mut statement = connection
                .prepare(
                    "SELECT name, sql FROM sqlite_schema
                     WHERE type = 'index' AND tbl_name = 'runs' AND sql IS NOT NULL
                     ORDER BY name",
                )
                .unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };

        migrate_to(&mut connection, 32).unwrap();

        let runs_after: String = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'runs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let indexes_after: Vec<(String, String)> = {
            let mut statement = connection
                .prepare(
                    "SELECT name, sql FROM sqlite_schema
                     WHERE type = 'index' AND tbl_name = 'runs' AND sql IS NOT NULL
                     ORDER BY name",
                )
                .unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(runs_after, runs_before);
        assert_eq!(indexes_after, indexes_before);
        let preserved: (String, bool) = connection
            .query_row(
                "SELECT phase, capability_digest IS NOT NULL FROM runs WHERE id = ?1",
                ["11111111-1111-4111-8111-111111111111"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(preserved, ("finalizing".to_owned(), true));
    }
}
