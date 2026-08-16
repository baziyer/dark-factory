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

const SCHEMA_VERSION: i64 = 5;
const MAX_EVENT_PAGE: usize = 10_000;
const MAX_STATE_PAGE: usize = 101;
const MAX_PROVIDER_SESSION_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 4096;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalOutcome {
    Succeeded,
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
             SET assigned_agent_id = ?1, status = 'running', updated_at_ms = ?2
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
            "SELECT id, project_id, parent_task_id, assigned_agent_id, title, body,
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
                let status: String = row.get(6)?;
                Ok(TaskDetail {
                    snapshot: TaskSnapshot {
                        id: parse_id(row.get(0)?, 0)?,
                        project_id: parse_id(row.get(1)?, 1)?,
                        parent_task_id: parse_optional_id(parent_id, 2)?,
                        depends_on: Vec::new(),
                        assigned_agent_id: parse_optional_id(assigned_id, 3)?,
                        title: row.get(4)?,
                        status: parse_task_status(&status, 6)?,
                        priority: row.get(7)?,
                        created_at_ms: row.get(8)?,
                        updated_at_ms: row.get(9)?,
                    },
                    body: row.get(5)?,
                })
            },
        )?;

        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
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
        "UPDATE tasks SET status = 'failed', updated_at_ms = ?1
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
    match (terminal_kind, effects.terminal_outcome) {
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
         SET status = ?1, updated_at_ms = ?2
         WHERE id = ?3 AND project_id = ?4 AND assigned_agent_id = ?5
           AND status = 'running'",
        params![
            task_status_value(terminal.task_status),
            now_ms,
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
                    runner_reconciled_at_ms, runner_terminal_kind
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
}

fn validate_terminal_outcome(
    event: &RunnerEvent,
    outcome: TerminalOutcome,
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
            })
        }
        (
            RunnerEvent::Exited {
                exit_code: Some(0),
                signal: None,
            },
            TerminalOutcome::Succeeded,
        ) if provider_session_confirmed => Ok(TerminalState {
            run_status: RunStatus::Succeeded,
            task_status: TaskStatus::Succeeded,
            failure_reason: None,
            exit_code: Some(0),
            exit_signal: None,
        }),
        (RunnerEvent::Exited { exit_code, signal }, TerminalOutcome::Failed(reason))
            if valid_failed_process_outcome(*exit_code, *signal, reason) =>
        {
            Ok(TerminalState {
                run_status: RunStatus::Failed,
                task_status: TaskStatus::Failed,
                failure_reason: Some(reason),
                exit_code: *exit_code,
                exit_signal: *signal,
            })
        }
        _ => Err(StoreError::InvalidTerminalOutcome),
    }
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
    connection
        .query_row(
            "SELECT id, project_id, parent_task_id, assigned_agent_id, title, body,
                    status, priority, created_at_ms, updated_at_ms
             FROM tasks WHERE id = ?1",
            params![task_id.as_str()],
            |row| {
                let parent_id: Option<String> = row.get(2)?;
                let assigned_id: Option<String> = row.get(3)?;
                let status: String = row.get(6)?;
                Ok(TaskDetail {
                    snapshot: TaskSnapshot {
                        id: parse_id(row.get(0)?, 0)?,
                        project_id: parse_id(row.get(1)?, 1)?,
                        parent_task_id: parse_optional_id(parent_id, 2)?,
                        depends_on: Vec::new(),
                        assigned_agent_id: parse_optional_id(assigned_id, 3)?,
                        title: row.get(4)?,
                        status: parse_task_status(&status, 6)?,
                        priority: row.get(7)?,
                        created_at_ms: row.get(8)?,
                        updated_at_ms: row.get(9)?,
                    },
                    body: row.get(5)?,
                })
            },
        )
        .optional()
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
