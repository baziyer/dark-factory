use std::path::Path;

use factory_core::{
    AgentId, AgentRole, EventEnvelope, FactoryEvent, MessageId, ProjectId, Provider,
    RunFailureReason, RunId, RunOutcome, RunPhase, RunSnapshot, RunnerInstanceId, TaskId,
    TaskStatus,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    AgentMessage, MAX_BLOCKED_REASON_BYTES, MAX_PATH_BYTES, MAX_TASK_RESULT_BYTES,
    MAX_WAIT_REASON_BYTES, Result, Store, StoreError, append_agent_changed_event, append_event,
    load_agent, load_agent_profile, load_task, parse_agent_role, parse_id, parse_observer_health,
    parse_provider,
};

const CAPABILITY_HEX_LEN: usize = 64;
const MAX_RESOURCE_LOCATOR_BYTES: usize = 4096;
const MAX_RESOURCE_FINGERPRINT_BYTES: usize = 1024;
const MAX_RESOURCE_FAILURE_BYTES: usize = 4096;

pub struct NewRunAdmission {
    pub run_id: RunId,
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub expected_provider: Provider,
    pub capability_digest: String,
    pub runtime_claim: String,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
    pub max_active_runs: usize,
    /// Stage 1 has no production Change allocator. Stage 1 module tests insert
    /// a disposable fixture Change and pass its private identity here; Stage 2
    /// replaces this seam with daemon-owned provisioning.
    pub change_id: Option<String>,
    pub policy_cwd: Option<String>,
}

pub struct AdmittedRun {
    pub run: RunSnapshot,
    pub target: AttemptTarget,
    pub events: Vec<EventEnvelope>,
}

pub struct AttemptTarget {
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub role: AgentRole,
    pub provider: Provider,
    pub task_title: String,
    pub task_body: String,
    pub messages: Vec<AgentMessage>,
    pub worktree: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub permission_mode: Option<String>,
    pub auto_mode: bool,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
    pub runtime_claim: String,
}

#[derive(Clone)]
pub struct AttemptPrincipal {
    pub run_id: RunId,
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub role: AgentRole,
    pub provider: Provider,
    pub phase: RunPhase,
    pub worktree: String,
    pub change_id: Option<String>,
    pub branch: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelResourceKind {
    RunnerProcess,
    ProviderProcess,
    ProcessGroup,
    RuntimeRoot,
}

impl KernelResourceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::RunnerProcess => "runner_process",
            Self::ProviderProcess => "provider_process",
            Self::ProcessGroup => "process_group",
            Self::RuntimeRoot => "runtime_root",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "runner_process" => Self::RunnerProcess,
            "provider_process" => Self::ProviderProcess,
            "process_group" => Self::ProcessGroup,
            "runtime_root" => Self::RuntimeRoot,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelResourceState {
    Declared,
    Active,
    Releasing,
    Released,
    Unresolved,
}

impl KernelResourceState {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "declared" => Self::Declared,
            "active" => Self::Active,
            "releasing" => Self::Releasing,
            "released" => Self::Released,
            "unresolved" => Self::Unresolved,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelResource {
    pub id: String,
    pub run_id: RunId,
    pub kind: KernelResourceKind,
    pub state: KernelResourceState,
    pub locator: String,
    pub birth_fingerprint: Option<String>,
    pub retry_count: u32,
    pub last_failure: Option<String>,
    pub declared_at_ms: i64,
    pub updated_at_ms: i64,
    pub released_at_ms: Option<i64>,
}

pub struct PreparedProcessIdentity {
    pub runtime_locator: String,
    pub runtime_birth_fingerprint: String,
    pub runner_locator: String,
    pub runner_birth_fingerprint: String,
    pub provider_locator: String,
    pub provider_birth_fingerprint: String,
    pub process_group_locator: String,
    pub process_group_birth_fingerprint: String,
}

pub struct RecoverableKernelRun {
    pub run: RunSnapshot,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
    pub resources: Vec<KernelResource>,
}

impl Store {
    pub fn admit_run(&mut self, input: NewRunAdmission, now_ms: i64) -> Result<AdmittedRun> {
        validate_capability_digest(&input.capability_digest)?;
        validate_absolute_path(&input.runner_runtime)?;
        validate_runtime_claim(&input.runtime_claim)?;
        if input.max_active_runs == 0 {
            return Err(StoreError::InvalidConcurrencyLimit);
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM runs WHERE phase <> 'terminal'",
            [],
            |row| row.get(0),
        )?;
        if active >= i64::try_from(input.max_active_runs).unwrap_or(i64::MAX) {
            return Err(StoreError::CapacityReached {
                limit: input.max_active_runs,
            });
        }

        let task = load_task(&transaction, &input.task_id)?
            .filter(|task| task.snapshot.project_id == input.project_id)
            .filter(|task| task.snapshot.status == TaskStatus::Queued)
            .ok_or(StoreError::TaskNotQueued)?;
        if task.snapshot.assigned_agent_id.as_ref() != Some(&input.agent_id) {
            return Err(StoreError::TaskAssignmentMismatch);
        }
        let task_title = task.snapshot.title.clone();
        let task_body = task.body.clone();
        let (task_incarnation_id, admitted_task_work_revision): (String, i64) = transaction
            .query_row(
                "SELECT incarnation_id, work_revision FROM tasks
                 WHERE id = ?1 AND project_id = ?2",
                params![input.task_id.as_str(), input.project_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
        let agent = load_agent(&transaction, &input.agent_id)?
            .filter(|agent| agent.snapshot.project_id == input.project_id)
            .ok_or(StoreError::AgentNotFound)?;
        if agent.snapshot.provider != input.expected_provider {
            return Err(StoreError::AgentProviderMismatch);
        }
        if agent.snapshot.paused {
            return Err(StoreError::AgentUnavailable);
        }
        let already_open: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM runs
                 WHERE phase <> 'terminal' AND (agent_id = ?1 OR task_id = ?2)
             )",
            params![input.agent_id.as_str(), input.task_id.as_str()],
            |row| row.get(0),
        )?;
        if already_open {
            return Err(StoreError::AgentUnavailable);
        }

        let worktree = match agent.snapshot.role {
            AgentRole::Worker => {
                let change_id = input
                    .change_id
                    .as_deref()
                    .ok_or(StoreError::SourceProvisioningUnavailable)?;
                transaction
                    .query_row(
                        "SELECT c.worktree FROM changes c
                         JOIN tasks t ON t.id = c.task_id AND t.project_id = c.project_id
                         WHERE c.id = ?1 AND c.project_id = ?2 AND c.task_id = ?3
                           AND c.task_incarnation_id = t.incarnation_id
                           AND c.ready_at_ms IS NOT NULL",
                        params![change_id, input.project_id.as_str(), input.task_id.as_str()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or(StoreError::ChangeNotFound)?
            }
            AgentRole::Orchestrator => input
                .policy_cwd
                .clone()
                .ok_or(StoreError::SourceProvisioningUnavailable)?,
        };
        validate_absolute_path(&worktree)?;

        let profile =
            load_agent_profile(&transaction, &input.agent_id)?.ok_or(StoreError::AgentNotFound)?;
        let auto_mode: bool = transaction.query_row(
            "SELECT auto_mode FROM factory_settings WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;

        transaction.execute(
            "UPDATE tasks
             SET status = 'running', updated_at_ms = ?1, work_revision = work_revision + 1,
                 blocked_reason = NULL, completed_at_ms = NULL
             WHERE id = ?2 AND project_id = ?3 AND status = 'queued'",
            params![now_ms, input.task_id.as_str(), input.project_id.as_str()],
        )?;
        transaction.execute(
            "INSERT INTO runs (
                id, project_id, agent_id, task_id, task_incarnation_id,
                admitted_task_work_revision, change_id, parent_run_id, worktree,
                phase, outcome, outcome_detail, outcome_result, capability_digest,
                provider, runtime_model, runtime_reasoning_effort, runtime_permission_mode,
                runtime_control_mode, activity, wait_reason, observer_health, observer_reason,
                runner_instance_id, runner_runtime, runner_protocol_version,
                last_runner_sequence, terminal_runner_sequence, runner_reconciled_at_ms,
                stop_requested_at_ms, admitted_at_ms, running_at_ms, finalizing_at_ms,
                phase_since_ms, updated_at_ms, ended_at_ms, exit_code, exit_signal
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8,
                'admitted', NULL, NULL, NULL, ?9,
                ?10, ?11, ?12, ?13, NULL, NULL, NULL, 'unknown', NULL,
                ?14, ?15, ?16, 0, NULL, NULL, NULL, ?17, NULL, NULL,
                ?17, ?17, NULL, NULL, NULL
             )",
            params![
                input.run_id.as_str(),
                input.project_id.as_str(),
                input.agent_id.as_str(),
                input.task_id.as_str(),
                task_incarnation_id,
                admitted_task_work_revision,
                input.change_id,
                worktree,
                input.capability_digest,
                provider_str(input.expected_provider),
                profile.model,
                profile.reasoning_effort,
                profile.permission_mode,
                input.runner_instance_id.as_str(),
                input.runner_runtime,
                i64::from(factory_core::runner::RUNNER_PROTOCOL_VERSION),
                now_ms,
            ],
        )?;
        let runtime_locator = serde_json::json!({ "path": input.runner_runtime }).to_string();
        let runner_locator =
            serde_json::json!({ "runner_instance_id": input.runner_instance_id.as_str() })
                .to_string();
        insert_resource(
            &transaction,
            &format!("{}:runtime", input.run_id.as_str()),
            &input.run_id,
            KernelResourceKind::RuntimeRoot,
            &runtime_locator,
            Some(&input.runtime_claim),
            now_ms,
        )?;
        insert_resource(
            &transaction,
            &format!("{}:runner", input.run_id.as_str()),
            &input.run_id,
            KernelResourceKind::RunnerProcess,
            &runner_locator,
            None,
            now_ms,
        )?;

        let messages = undelivered_messages(&transaction, &input.project_id, &input.agent_id)?;
        if !messages.is_empty() {
            transaction.execute(
                "UPDATE agent_messages
                 SET delivered_at_ms = ?1, delivered_run_id = ?2
                 WHERE project_id = ?3 AND recipient_agent_id = ?4
                   AND delivered_at_ms IS NULL",
                params![
                    now_ms,
                    input.run_id.as_str(),
                    input.project_id.as_str(),
                    input.agent_id.as_str()
                ],
            )?;
        }

        let task = load_task(&transaction, &input.task_id)?
            .ok_or(StoreError::TaskNotFound)?
            .snapshot;
        let run = load_kernel_run(&transaction, &input.run_id)?.ok_or(StoreError::RunNotFound)?;
        let task_event = FactoryEvent::TaskChanged { task: task.clone() };
        let task_sequence = append_event(&transaction, now_ms, &task_event)?;
        let agent_event = append_agent_changed_event(&transaction, &input.agent_id, now_ms)?;
        let run_event_value = FactoryEvent::RunChanged {
            run: Box::new(run.clone()),
        };
        let run_sequence = append_event(&transaction, now_ms, &run_event_value)?;
        transaction.commit()?;

        Ok(AdmittedRun {
            run,
            target: AttemptTarget {
                project_id: input.project_id,
                task_id: input.task_id,
                agent_id: input.agent_id,
                role: agent.snapshot.role,
                provider: input.expected_provider,
                task_title,
                task_body,
                messages,
                worktree,
                model: profile.model,
                reasoning_effort: profile.reasoning_effort,
                permission_mode: profile.permission_mode,
                auto_mode,
                runner_instance_id: input.runner_instance_id,
                runner_runtime: input.runner_runtime,
                runtime_claim: input.runtime_claim,
            },
            events: vec![
                EventEnvelope {
                    protocol_version: factory_core::PROTOCOL_VERSION,
                    sequence: task_sequence,
                    occurred_at_ms: now_ms,
                    event: task_event,
                },
                agent_event,
                EventEnvelope {
                    protocol_version: factory_core::PROTOCOL_VERSION,
                    sequence: run_sequence,
                    occurred_at_ms: now_ms,
                    event: run_event_value,
                },
            ],
        })
    }

    pub fn activate_prepared_run(
        &mut self,
        run_id: &RunId,
        identity: PreparedProcessIdentity,
        now_ms: i64,
    ) -> Result<(RunSnapshot, Vec<EventEnvelope>)> {
        validate_resource_identity(
            &identity.runtime_locator,
            &identity.runtime_birth_fingerprint,
        )?;
        validate_resource_identity(&identity.runner_locator, &identity.runner_birth_fingerprint)?;
        validate_resource_identity(
            &identity.provider_locator,
            &identity.provider_birth_fingerprint,
        )?;
        validate_resource_identity(
            &identity.process_group_locator,
            &identity.process_group_birth_fingerprint,
        )?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if run.phase != RunPhase::Admitted {
            return Err(StoreError::InvalidRunState);
        }
        activate_or_confirm_resource(
            &transaction,
            run_id,
            KernelResourceKind::RuntimeRoot,
            &identity.runtime_locator,
            &identity.runtime_birth_fingerprint,
            now_ms,
        )?;
        activate_or_confirm_resource(
            &transaction,
            run_id,
            KernelResourceKind::RunnerProcess,
            &identity.runner_locator,
            &identity.runner_birth_fingerprint,
            now_ms,
        )?;
        upsert_active_resource(
            &transaction,
            &format!("{}:provider", run_id.as_str()),
            run_id,
            KernelResourceKind::ProviderProcess,
            &identity.provider_locator,
            &identity.provider_birth_fingerprint,
            now_ms,
        )?;
        upsert_active_resource(
            &transaction,
            &format!("{}:group", run_id.as_str()),
            run_id,
            KernelResourceKind::ProcessGroup,
            &identity.process_group_locator,
            &identity.process_group_birth_fingerprint,
            now_ms,
        )?;
        transaction.execute(
            "UPDATE runs
             SET phase = 'running', running_at_ms = ?1, phase_since_ms = ?1,
                 updated_at_ms = ?1
             WHERE id = ?2 AND phase = 'admitted'",
            params![now_ms, run_id.as_str()],
        )?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        let event = FactoryEvent::RunChanged {
            run: Box::new(run.clone()),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            run,
            vec![EventEnvelope {
                protocol_version: factory_core::PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            }],
        ))
    }

    /// Durably binds the daemon-created runtime before any credential,
    /// provider configuration, or process is created inside it.
    pub fn register_admitted_runtime(
        &mut self,
        run_id: &RunId,
        runtime_locator: &str,
        expected_claim: &str,
        runtime_birth_fingerprint: &str,
        now_ms: i64,
    ) -> Result<()> {
        validate_resource_identity(runtime_locator, expected_claim)?;
        validate_resource_identity(runtime_locator, runtime_birth_fingerprint)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if run.phase != RunPhase::Admitted {
            return Err(StoreError::InvalidRunState);
        }
        bind_claimed_runtime(
            &transaction,
            run_id,
            runtime_locator,
            expected_claim,
            runtime_birth_fingerprint,
            now_ms,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Durably binds the exact stable runner before Prepare can create the
    /// provider exec gate.
    pub fn register_admitted_runner(
        &mut self,
        run_id: &RunId,
        runner_locator: &str,
        runner_birth_fingerprint: &str,
        now_ms: i64,
    ) -> Result<()> {
        validate_resource_identity(runner_locator, runner_birth_fingerprint)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if run.phase != RunPhase::Admitted {
            return Err(StoreError::InvalidRunState);
        }
        activate_or_confirm_resource(
            &transaction,
            run_id,
            KernelResourceKind::RunnerProcess,
            runner_locator,
            runner_birth_fingerprint,
            now_ms,
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn authenticate_attempt(&self, bearer: &str) -> Result<Option<AttemptPrincipal>> {
        let digest = capability_digest(bearer);
        self.connection
            .query_row(
                "SELECT r.id, r.project_id, r.task_id, r.agent_id, a.role,
                        r.provider, r.phase, r.worktree, r.change_id, c.branch
                 FROM runs r
                 JOIN agents a ON a.id = r.agent_id AND a.project_id = r.project_id
                 LEFT JOIN changes c ON c.id = r.change_id AND c.project_id = r.project_id
                 WHERE r.capability_digest = ?1 AND r.phase <> 'terminal'",
                params![digest],
                |row| {
                    let role: String = row.get(4)?;
                    let provider: String = row.get(5)?;
                    let phase: String = row.get(6)?;
                    Ok(AttemptPrincipal {
                        run_id: parse_id(row.get(0)?, 0)?,
                        project_id: parse_id(row.get(1)?, 1)?,
                        task_id: parse_id(row.get(2)?, 2)?,
                        agent_id: parse_id(row.get(3)?, 3)?,
                        role: parse_agent_role(&role, 4)?,
                        provider: parse_provider(&provider, 5)?,
                        phase: parse_phase(&phase, 6)?,
                        worktree: row.get(7)?,
                        change_id: row.get(8)?,
                        branch: row.get(9)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn request_attempt_outcome(
        &mut self,
        run_id: &RunId,
        outcome: &RunOutcome,
        result: Option<&str>,
        now_ms: i64,
    ) -> Result<(RunSnapshot, Vec<EventEnvelope>)> {
        validate_outcome(outcome)?;
        if result.is_some_and(|value| value.len() > MAX_TASK_RESULT_BYTES) {
            return Err(StoreError::InvalidTaskResult);
        }
        if !matches!(outcome, RunOutcome::Succeeded) && result.is_some() {
            return Err(StoreError::InvalidTaskResult);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        match run.phase {
            RunPhase::Running => {}
            RunPhase::Finalizing | RunPhase::Terminal => {
                let stored_result: Option<String> = transaction.query_row(
                    "SELECT outcome_result FROM runs WHERE id = ?1",
                    params![run_id.as_str()],
                    |row| row.get(0),
                )?;
                if run.outcome.as_ref() == Some(outcome) && stored_result.as_deref() == result {
                    return Ok((run, Vec::new()));
                }
                return Err(StoreError::AttemptOutcomeConflict);
            }
            RunPhase::Admitted => return Err(StoreError::InvalidRunState),
        }
        let (kind, detail) = outcome_parts(outcome);
        transaction.execute(
            "UPDATE runs
             SET phase = 'finalizing', outcome = ?1, outcome_detail = ?2,
                 outcome_result = ?3, stop_requested_at_ms = ?4,
                 finalizing_at_ms = ?4, phase_since_ms = ?4, updated_at_ms = ?4
             WHERE id = ?5 AND phase = 'running'",
            params![kind, detail, result, now_ms, run_id.as_str()],
        )?;
        transaction.execute(
            "UPDATE resources SET state = 'releasing', updated_at_ms = ?1
             WHERE run_id = ?2 AND state IN ('declared', 'active')",
            params![now_ms, run_id.as_str()],
        )?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        let event = FactoryEvent::RunChanged {
            run: Box::new(run.clone()),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            run,
            vec![EventEnvelope {
                protocol_version: factory_core::PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            }],
        ))
    }

    /// Records a launch failure after admission but before the runner can
    /// emit an authenticated terminal event. Finalization remains responsible
    /// for proving every pre-registered resource released.
    pub fn fail_admitted_run(
        &mut self,
        run_id: &RunId,
        reason: RunFailureReason,
        now_ms: i64,
    ) -> Result<(RunSnapshot, Vec<EventEnvelope>)> {
        self.fail_open_run(run_id, RunPhase::Admitted, reason, now_ms)
    }

    /// Records that a durably activated process vanished before it could
    /// append an authenticated terminal event. Resource reconciliation still
    /// has to prove every registered identity absent before terminalization.
    pub fn fail_running_run(
        &mut self,
        run_id: &RunId,
        reason: RunFailureReason,
        now_ms: i64,
    ) -> Result<(RunSnapshot, Vec<EventEnvelope>)> {
        self.fail_open_run(run_id, RunPhase::Running, reason, now_ms)
    }

    fn fail_open_run(
        &mut self,
        run_id: &RunId,
        expected_phase: RunPhase,
        reason: RunFailureReason,
        now_ms: i64,
    ) -> Result<(RunSnapshot, Vec<EventEnvelope>)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if matches!(run.phase, RunPhase::Finalizing | RunPhase::Terminal) {
            return Ok((run, Vec::new()));
        }
        if run.phase != expected_phase {
            return Err(StoreError::InvalidRunState);
        }
        let (_, detail) = outcome_parts(&RunOutcome::Failed { reason });
        let expected_phase = match expected_phase {
            RunPhase::Admitted => "admitted",
            RunPhase::Running => "running",
            RunPhase::Finalizing | RunPhase::Terminal => return Err(StoreError::InvalidRunState),
        };
        transaction.execute(
            "UPDATE runs
             SET phase = 'finalizing', outcome = 'failed', outcome_detail = ?1,
                 stop_requested_at_ms = ?2, finalizing_at_ms = ?2,
                 phase_since_ms = ?2, updated_at_ms = ?2
             WHERE id = ?3 AND phase = ?4",
            params![detail, now_ms, run_id.as_str(), expected_phase],
        )?;
        transaction.execute(
            "UPDATE resources SET state = 'releasing', updated_at_ms = ?1
             WHERE run_id = ?2 AND state IN ('declared', 'active')",
            params![now_ms, run_id.as_str()],
        )?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        let event = FactoryEvent::RunChanged {
            run: Box::new(run.clone()),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            run,
            vec![EventEnvelope {
                protocol_version: factory_core::PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            }],
        ))
    }

    pub fn cancel_admitted_or_running_run(
        &mut self,
        run_id: &RunId,
        reason: String,
        now_ms: i64,
    ) -> Result<(RunSnapshot, Vec<EventEnvelope>)> {
        if reason.is_empty() || reason.len() > MAX_WAIT_REASON_BYTES {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if matches!(run.phase, RunPhase::Finalizing | RunPhase::Terminal) {
            return if matches!(run.outcome, Some(RunOutcome::Cancelled { .. })) {
                Ok((run, Vec::new()))
            } else {
                Err(StoreError::InvalidRunState)
            };
        }
        transaction.execute(
            "UPDATE runs
             SET phase = 'finalizing', outcome = 'cancelled', outcome_detail = ?1,
                 stop_requested_at_ms = ?2, finalizing_at_ms = ?2,
                 phase_since_ms = ?2, updated_at_ms = ?2
             WHERE id = ?3 AND phase IN ('admitted', 'running')",
            params![reason, now_ms, run_id.as_str()],
        )?;
        transaction.execute(
            "UPDATE resources SET state = 'releasing', updated_at_ms = ?1
             WHERE run_id = ?2 AND state IN ('declared', 'active')",
            params![now_ms, run_id.as_str()],
        )?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        let event = FactoryEvent::RunChanged {
            run: Box::new(run.clone()),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            run,
            vec![EventEnvelope {
                protocol_version: factory_core::PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            }],
        ))
    }

    pub fn observe_attempt_exit(
        &mut self,
        run_id: &RunId,
        terminal_sequence: i64,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
        failure_reason: Option<RunFailureReason>,
        now_ms: i64,
    ) -> Result<(RunSnapshot, Vec<EventEnvelope>)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if run.phase == RunPhase::Terminal {
            return Ok((run, Vec::new()));
        }
        if run.phase == RunPhase::Running {
            let (_, failure_detail) = outcome_parts(&RunOutcome::Failed {
                reason: failure_reason.unwrap_or(RunFailureReason::Process),
            });
            transaction.execute(
                "UPDATE runs
                 SET phase = 'finalizing', outcome = 'failed', outcome_detail = ?1,
                     finalizing_at_ms = ?2, phase_since_ms = ?2,
                     stop_requested_at_ms = COALESCE(stop_requested_at_ms, ?2),
                     updated_at_ms = ?2
                 WHERE id = ?3 AND phase = 'running'",
                params![failure_detail, now_ms, run_id.as_str()],
            )?;
        } else if run.phase == RunPhase::Admitted {
            transaction.execute(
                "UPDATE runs
                 SET phase = 'finalizing', outcome = 'failed', outcome_detail = 'spawn',
                     finalizing_at_ms = ?1, phase_since_ms = ?1,
                     stop_requested_at_ms = COALESCE(stop_requested_at_ms, ?1),
                     updated_at_ms = ?1
                 WHERE id = ?2 AND phase = 'admitted'",
                params![now_ms, run_id.as_str()],
            )?;
        }
        transaction.execute(
            "UPDATE resources SET state = 'releasing', updated_at_ms = ?1
             WHERE run_id = ?2 AND state IN ('declared', 'active')",
            params![now_ms, run_id.as_str()],
        )?;
        transaction.execute(
            "UPDATE runs
             SET terminal_runner_sequence = ?1, exit_code = ?2, exit_signal = ?3,
                 updated_at_ms = ?4
             WHERE id = ?5",
            params![
                terminal_sequence,
                exit_code,
                exit_signal,
                now_ms,
                run_id.as_str()
            ],
        )?;
        release_kinds(
            &transaction,
            run_id,
            &[KernelResourceKind::ProviderProcess],
            now_ms,
        )?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        let event = FactoryEvent::RunChanged {
            run: Box::new(run.clone()),
        };
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            run,
            vec![EventEnvelope {
                protocol_version: factory_core::PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            }],
        ))
    }

    pub fn mark_resource_released(
        &mut self,
        resource_id: &str,
        expected_locator: &str,
        expected_birth_fingerprint: Option<&str>,
        now_ms: i64,
    ) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE resources
             SET state = 'released', released_at_ms = ?1, updated_at_ms = ?1,
                 last_failure = NULL
             WHERE id = ?2 AND locator = ?3
               AND ((?4 IS NULL AND birth_fingerprint IS NULL)
                    OR birth_fingerprint = ?4)
               AND state IN ('declared', 'active', 'releasing', 'unresolved')",
            params![
                now_ms,
                resource_id,
                expected_locator,
                expected_birth_fingerprint
            ],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::ResourceIdentityMismatch)
        }
    }

    pub fn mark_resource_unresolved(
        &mut self,
        resource_id: &str,
        failure: &str,
        now_ms: i64,
    ) -> Result<()> {
        if failure.is_empty() || failure.len() > MAX_RESOURCE_FAILURE_BYTES {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        let changed = self.connection.execute(
            "UPDATE resources
             SET state = 'unresolved', retry_count = retry_count + 1,
                 last_failure = ?1, updated_at_ms = ?2
             WHERE id = ?3 AND state <> 'released'",
            params![failure, now_ms, resource_id],
        )?;
        if changed == 1 {
            Ok(())
        } else {
            Err(StoreError::ResourceNotFound)
        }
    }

    pub fn finalize_run(
        &mut self,
        run_id: &RunId,
        now_ms: i64,
    ) -> Result<(RunSnapshot, Vec<EventEnvelope>)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if run.phase == RunPhase::Terminal {
            return Ok((run, Vec::new()));
        }
        if run.phase != RunPhase::Finalizing {
            return Err(StoreError::InvalidRunState);
        }
        let unreleased: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM resources WHERE run_id = ?1 AND state <> 'released'",
            params![run_id.as_str()],
            |row| row.get(0),
        )?;
        if unreleased != 0 {
            return Err(StoreError::RunResourcesUnreleased { count: unreleased });
        }
        let outcome = run.outcome.as_ref().ok_or(StoreError::InvalidRunState)?;
        let outcome_result: Option<String> = transaction.query_row(
            "SELECT outcome_result FROM runs WHERE id = ?1",
            params![run_id.as_str()],
            |row| row.get(0),
        )?;
        let (task_status, blocked_reason, result) =
            task_projection(outcome, outcome_result.as_deref());
        if matches!(
            outcome,
            RunOutcome::Failed { .. } | RunOutcome::Cancelled { .. }
        ) {
            transaction.execute(
                "UPDATE agent_messages
                 SET delivered_at_ms = NULL, delivered_run_id = NULL
                 WHERE delivered_run_id = ?1",
                params![run_id.as_str()],
            )?;
        }
        let (task_incarnation_id, admitted_task_work_revision): (String, i64) = transaction
            .query_row(
                "SELECT task_incarnation_id, admitted_task_work_revision
                 FROM runs WHERE id = ?1",
                params![run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
        let projected_work_revision = admitted_task_work_revision
            .checked_add(1)
            .ok_or(StoreError::InvalidRunState)?;
        let projected = transaction.execute(
            "UPDATE tasks
             SET status = ?1, blocked_reason = ?2, result = ?3,
                 updated_at_ms = ?4,
                 completed_at_ms = CASE WHEN ?1 IN ('succeeded', 'failed', 'cancelled')
                                        THEN ?4 ELSE NULL END,
                 work_revision = work_revision + 1
             WHERE id = ?5 AND project_id = ?6 AND status = 'running'
               AND incarnation_id = ?7 AND work_revision = ?8",
            params![
                task_status,
                blocked_reason,
                result,
                now_ms,
                run.task_id.as_str(),
                run.project_id.as_str(),
                task_incarnation_id,
                projected_work_revision,
            ],
        )?;
        if projected != 1 {
            return Err(StoreError::InvalidRunState);
        }
        transaction.execute(
            "UPDATE runs
             SET phase = 'terminal', phase_since_ms = ?1, updated_at_ms = ?1,
                 ended_at_ms = ?1, capability_digest = NULL
             WHERE id = ?2 AND phase = 'finalizing'",
            params![now_ms, run_id.as_str()],
        )?;
        let task = load_task(&transaction, &run.task_id)?
            .ok_or(StoreError::TaskNotFound)?
            .snapshot;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        let task_event = FactoryEvent::TaskChanged { task };
        let task_sequence = append_event(&transaction, now_ms, &task_event)?;
        let agent_event = append_agent_changed_event(&transaction, &run.agent_id, now_ms)?;
        let run_event = FactoryEvent::RunChanged {
            run: Box::new(run.clone()),
        };
        let run_sequence = append_event(&transaction, now_ms, &run_event)?;
        transaction.commit()?;
        Ok((
            run,
            vec![
                EventEnvelope {
                    protocol_version: factory_core::PROTOCOL_VERSION,
                    sequence: task_sequence,
                    occurred_at_ms: now_ms,
                    event: task_event,
                },
                agent_event,
                EventEnvelope {
                    protocol_version: factory_core::PROTOCOL_VERSION,
                    sequence: run_sequence,
                    occurred_at_ms: now_ms,
                    event: run_event,
                },
            ],
        ))
    }

    pub fn recoverable_kernel_runs(&self) -> Result<Vec<RecoverableKernelRun>> {
        let mut statement = self.connection.prepare(
            "SELECT id FROM runs WHERE phase <> 'terminal' ORDER BY project_id, admitted_at_ms, id",
        )?;
        let run_ids = statement
            .query_map([], |row| parse_id::<RunId>(row.get(0)?, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        run_ids
            .into_iter()
            .map(|run_id| {
                let run =
                    load_kernel_run(&self.connection, &run_id)?.ok_or(StoreError::RunNotFound)?;
                let runner_instance_id = run
                    .runner_instance_id
                    .clone()
                    .ok_or(StoreError::InvalidExecutionMetadata)?;
                let runner_runtime: String = self.connection.query_row(
                    "SELECT runner_runtime FROM runs WHERE id = ?1",
                    params![run_id.as_str()],
                    |row| row.get(0),
                )?;
                Ok(RecoverableKernelRun {
                    resources: load_resources(&self.connection, &run_id)?,
                    run,
                    runner_instance_id,
                    runner_runtime,
                })
            })
            .collect()
    }

    pub fn kernel_resources(&self, run_id: &RunId) -> Result<Vec<KernelResource>> {
        load_resources(&self.connection, run_id)
    }

    pub fn kernel_run(&self, run_id: &RunId) -> Result<Option<RunSnapshot>> {
        load_kernel_run(&self.connection, run_id)
    }
}

pub fn capability_digest(bearer: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bearer.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn validate_capability_digest(value: &str) -> Result<()> {
    if value.len() == CAPABILITY_HEX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(StoreError::InvalidHookToken)
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

fn validate_outcome(outcome: &RunOutcome) -> Result<()> {
    match outcome {
        RunOutcome::Succeeded => Ok(()),
        RunOutcome::Blocked { reason } => {
            if reason.is_empty() || reason.len() > MAX_BLOCKED_REASON_BYTES {
                Err(StoreError::InvalidBlockedReason)
            } else {
                Ok(())
            }
        }
        RunOutcome::Failed { .. } => Ok(()),
        RunOutcome::Cancelled { reason } => {
            if reason.is_empty() || reason.len() > MAX_WAIT_REASON_BYTES {
                Err(StoreError::InvalidExecutionMetadata)
            } else {
                Ok(())
            }
        }
    }
}

fn outcome_parts(outcome: &RunOutcome) -> (&'static str, Option<String>) {
    match outcome {
        RunOutcome::Succeeded => ("succeeded", None),
        RunOutcome::Blocked { reason } => ("blocked", Some(reason.clone())),
        RunOutcome::Failed { reason } => (
            "failed",
            Some(
                serde_json::to_value(reason)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or("process")
                    .to_owned(),
            ),
        ),
        RunOutcome::Cancelled { reason } => ("cancelled", Some(reason.clone())),
    }
}

fn task_projection<'a>(
    outcome: &'a RunOutcome,
    result: Option<&'a str>,
) -> (&'static str, Option<&'a str>, Option<&'a str>) {
    match outcome {
        RunOutcome::Succeeded => ("succeeded", None, result),
        RunOutcome::Blocked { reason } => ("blocked", Some(reason), None),
        RunOutcome::Failed { .. } => ("failed", None, None),
        RunOutcome::Cancelled { .. } => ("cancelled", None, None),
    }
}

fn provider_str(provider: Provider) -> &'static str {
    match provider {
        Provider::ClaudeCode => "claude_code",
        Provider::Codex => "codex",
        Provider::Shell => "shell",
    }
}

fn insert_resource(
    transaction: &Transaction<'_>,
    id: &str,
    run_id: &RunId,
    kind: KernelResourceKind,
    locator: &str,
    birth_fingerprint: Option<&str>,
    now_ms: i64,
) -> Result<()> {
    if locator.len() < 2 || locator.len() > MAX_RESOURCE_LOCATOR_BYTES {
        return Err(StoreError::InvalidExecutionMetadata);
    }
    if birth_fingerprint.is_some_and(|fingerprint| {
        fingerprint.is_empty() || fingerprint.len() > MAX_RESOURCE_FINGERPRINT_BYTES
    }) {
        return Err(StoreError::InvalidExecutionMetadata);
    }
    transaction.execute(
        "INSERT INTO resources (
            id, run_id, kind, state, locator, birth_fingerprint,
            retry_count, last_failure, declared_at_ms, updated_at_ms, released_at_ms
         ) VALUES (?1, ?2, ?3, 'declared', ?4, ?5, 0, NULL, ?6, ?6, NULL)",
        params![
            id,
            run_id.as_str(),
            kind.as_str(),
            locator,
            birth_fingerprint,
            now_ms
        ],
    )?;
    Ok(())
}

fn bind_claimed_runtime(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    locator: &str,
    expected_claim: &str,
    fingerprint: &str,
    now_ms: i64,
) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE resources
         SET state = 'active', birth_fingerprint = ?1, updated_at_ms = ?2
         WHERE run_id = ?3 AND kind = 'runtime_root' AND state = 'declared'
           AND locator = ?4 AND birth_fingerprint = ?5",
        params![
            fingerprint,
            now_ms,
            run_id.as_str(),
            locator,
            expected_claim
        ],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::ResourceIdentityMismatch)
    }
}

fn validate_resource_identity(locator: &str, fingerprint: &str) -> Result<()> {
    if locator.len() < 2
        || locator.len() > MAX_RESOURCE_LOCATOR_BYTES
        || fingerprint.is_empty()
        || fingerprint.len() > MAX_RESOURCE_FINGERPRINT_BYTES
    {
        Err(StoreError::InvalidExecutionMetadata)
    } else {
        Ok(())
    }
}

fn validate_runtime_claim(claim: &str) -> Result<()> {
    let nonce = claim
        .strip_prefix("runtime-claim:")
        .ok_or(StoreError::InvalidExecutionMetadata)?;
    let parsed = Uuid::parse_str(nonce).map_err(|_| StoreError::InvalidExecutionMetadata)?;
    if parsed.simple().to_string() == nonce {
        Ok(())
    } else {
        Err(StoreError::InvalidExecutionMetadata)
    }
}

fn activate_resource(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    kind: KernelResourceKind,
    locator: &str,
    fingerprint: &str,
    now_ms: i64,
) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE resources
         SET state = 'active', locator = ?1, birth_fingerprint = ?2,
             updated_at_ms = ?3
         WHERE run_id = ?4 AND kind = ?5 AND state = 'declared'",
        params![locator, fingerprint, now_ms, run_id.as_str(), kind.as_str()],
    )?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::ResourceIdentityMismatch)
    }
}

fn activate_or_confirm_resource(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    kind: KernelResourceKind,
    locator: &str,
    fingerprint: &str,
    now_ms: i64,
) -> Result<()> {
    let current: Option<(String, Option<String>, String)> = transaction
        .query_row(
            "SELECT locator, birth_fingerprint, state FROM resources
             WHERE run_id = ?1 AND kind = ?2",
            params![run_id.as_str(), kind.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    match current {
        Some((current_locator, current_fingerprint, state))
            if state == "active"
                && current_locator == locator
                && current_fingerprint.as_deref() == Some(fingerprint) =>
        {
            Ok(())
        }
        Some((_, _, state)) if state == "declared" => {
            activate_resource(transaction, run_id, kind, locator, fingerprint, now_ms)
        }
        _ => Err(StoreError::ResourceIdentityMismatch),
    }
}

fn upsert_active_resource(
    transaction: &Transaction<'_>,
    id: &str,
    run_id: &RunId,
    kind: KernelResourceKind,
    locator: &str,
    fingerprint: &str,
    now_ms: i64,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO resources (
            id, run_id, kind, state, locator, birth_fingerprint,
            retry_count, last_failure, declared_at_ms, updated_at_ms, released_at_ms
         ) VALUES (?1, ?2, ?3, 'active', ?4, ?5, 0, NULL, ?6, ?6, NULL)",
        params![
            id,
            run_id.as_str(),
            kind.as_str(),
            locator,
            fingerprint,
            now_ms
        ],
    )?;
    Ok(())
}

fn release_kinds(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    kinds: &[KernelResourceKind],
    now_ms: i64,
) -> Result<()> {
    for kind in kinds {
        transaction.execute(
            "UPDATE resources
             SET state = 'released', released_at_ms = ?1, updated_at_ms = ?1,
                 last_failure = NULL
             WHERE run_id = ?2 AND kind = ?3 AND state <> 'released'",
            params![now_ms, run_id.as_str(), kind.as_str()],
        )?;
    }
    Ok(())
}

fn load_resources(
    connection: &rusqlite::Connection,
    run_id: &RunId,
) -> Result<Vec<KernelResource>> {
    let mut statement = connection.prepare(
        "SELECT id, run_id, kind, state, locator, birth_fingerprint,
                retry_count, last_failure, declared_at_ms, updated_at_ms, released_at_ms
         FROM resources WHERE run_id = ?1 ORDER BY kind, id",
    )?;
    statement
        .query_map(params![run_id.as_str()], |row| {
            let kind: String = row.get(2)?;
            let state: String = row.get(3)?;
            Ok(KernelResource {
                id: row.get(0)?,
                run_id: parse_id(row.get(1)?, 1)?,
                kind: KernelResourceKind::parse(&kind).ok_or_else(|| {
                    rusqlite::Error::InvalidColumnType(2, kind, rusqlite::types::Type::Text)
                })?,
                state: KernelResourceState::parse(&state).ok_or_else(|| {
                    rusqlite::Error::InvalidColumnType(3, state, rusqlite::types::Type::Text)
                })?,
                locator: row.get(4)?,
                birth_fingerprint: row.get(5)?,
                retry_count: row.get(6)?,
                last_failure: row.get(7)?,
                declared_at_ms: row.get(8)?,
                updated_at_ms: row.get(9)?,
                released_at_ms: row.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(StoreError::from)
}

pub(super) fn load_kernel_run(
    connection: &rusqlite::Connection,
    run_id: &RunId,
) -> Result<Option<RunSnapshot>> {
    connection
        .query_row(
            "SELECT id, project_id, agent_id, task_id, provider, phase,
                    outcome, outcome_detail, outcome_result, runner_instance_id,
                    runtime_model, runtime_reasoning_effort, runtime_permission_mode,
                    runtime_control_mode, activity, wait_reason,
                    observer_health, observer_reason, admitted_at_ms, running_at_ms,
                    phase_since_ms, updated_at_ms, ended_at_ms, exit_code, exit_signal
             FROM runs WHERE id = ?1",
            params![run_id.as_str()],
            |row| {
                let provider: String = row.get(4)?;
                let phase: String = row.get(5)?;
                let outcome: Option<String> = row.get(6)?;
                let outcome_detail: Option<String> = row.get(7)?;
                let observer_health: String = row.get(16)?;
                Ok(RunSnapshot {
                    id: parse_id(row.get(0)?, 0)?,
                    project_id: parse_id(row.get(1)?, 1)?,
                    agent_id: parse_id(row.get(2)?, 2)?,
                    task_id: parse_id(row.get(3)?, 3)?,
                    provider: parse_provider(&provider, 4)?,
                    phase: parse_phase(&phase, 5)?,
                    outcome: parse_outcome(outcome.as_deref(), outcome_detail, 6)?,
                    runner_instance_id: {
                        let value: Option<String> = row.get(9)?;
                        value.map(|value| parse_id(value, 9)).transpose()?
                    },
                    runtime_model: row.get(10)?,
                    runtime_reasoning_effort: row.get(11)?,
                    runtime_permission_mode: row.get(12)?,
                    runtime_control_mode: row.get(13)?,
                    activity: row.get(14)?,
                    wait_reason: row.get(15)?,
                    observer_health: parse_observer_health(&observer_health, 16)?,
                    observer_reason: row.get(17)?,
                    admitted_at_ms: row.get(18)?,
                    started_at_ms: row.get(19)?,
                    phase_since_ms: row.get(20)?,
                    updated_at_ms: row.get(21)?,
                    ended_at_ms: row.get(22)?,
                    exit_code: row.get(23)?,
                    exit_signal: row.get(24)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn parse_phase(value: &str, column: usize) -> rusqlite::Result<RunPhase> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn parse_outcome(
    kind: Option<&str>,
    detail: Option<String>,
    column: usize,
) -> rusqlite::Result<Option<RunOutcome>> {
    let Some(kind) = kind else {
        return Ok(None);
    };
    let invalid =
        || rusqlite::Error::InvalidColumnType(column, kind.to_owned(), rusqlite::types::Type::Text);
    Ok(Some(match kind {
        "succeeded" => RunOutcome::Succeeded,
        "blocked" => RunOutcome::Blocked {
            reason: detail.ok_or_else(invalid)?,
        },
        "failed" => {
            let reason = detail.ok_or_else(invalid)?;
            let reason =
                serde_json::from_value(serde_json::Value::String(reason)).map_err(|_| invalid())?;
            RunOutcome::Failed { reason }
        }
        "cancelled" => RunOutcome::Cancelled {
            reason: detail.ok_or_else(invalid)?,
        },
        _ => return Err(invalid()),
    }))
}

fn undelivered_messages(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<Vec<AgentMessage>> {
    let mut statement = transaction.prepare(
        "SELECT id, project_id, sender_agent_id, recipient_agent_id, body,
                created_at_ms, delivered_at_ms, delivered_run_id
         FROM agent_messages
         WHERE project_id = ?1 AND recipient_agent_id = ?2 AND delivered_at_ms IS NULL
         ORDER BY created_at_ms, id",
    )?;
    statement
        .query_map(params![project_id.as_str(), agent_id.as_str()], |row| {
            let sender: Option<String> = row.get(2)?;
            let delivered_run: Option<String> = row.get(7)?;
            Ok(AgentMessage {
                id: parse_id::<MessageId>(row.get(0)?, 0)?,
                project_id: parse_id(row.get(1)?, 1)?,
                sender_agent_id: sender.map(|value| parse_id(value, 2)).transpose()?,
                recipient_agent_id: parse_id(row.get(3)?, 3)?,
                body: row.get(4)?,
                created_at_ms: row.get(5)?,
                delivered_at_ms: row.get(6)?,
                delivered_run_id: delivered_run.map(|value| parse_id(value, 7)).transpose()?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(StoreError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewAgent, NewAgentMessage, NewProject, NewTask};

    const BEARER: &str = "attempt-secret";

    fn admit_worker(store: &mut Store) -> RunId {
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("worker").unwrap();
        let task_id = TaskId::try_from("task-1").unwrap();
        let run_id = RunId::try_from("11111111-1111-4111-8111-111111111111").unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Factory".into(),
                    root: "/tmp/factory".into(),
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
                2,
            )
            .unwrap();
        store
            .create_assigned_task(
                NewTask {
                    id: task_id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Do work".into(),
                    body: "Body".into(),
                    priority: 0,
                },
                agent_id.clone(),
                3,
            )
            .unwrap();
        let incarnation: String = store
            .connection
            .query_row(
                "SELECT incarnation_id FROM tasks WHERE id = ?1",
                [task_id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO changes (
                    id, project_id, task_id, task_incarnation_id, branch, worktree,
                    ready_at_ms, retained_reason, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?7, ?7)",
                params![
                    "change-1",
                    project_id.as_str(),
                    task_id.as_str(),
                    incarnation,
                    "factory/change-1",
                    "/tmp/factory-change-1",
                    4,
                ],
            )
            .unwrap();
        let admitted = store
            .admit_run(
                NewRunAdmission {
                    run_id: run_id.clone(),
                    project_id,
                    task_id,
                    agent_id,
                    expected_provider: Provider::Shell,
                    capability_digest: capability_digest(BEARER),
                    runtime_claim: "runtime-claim:55555555555545558555555555555555".into(),
                    runner_instance_id: RunnerInstanceId::try_from(
                        "22222222-2222-4222-8222-222222222222",
                    )
                    .unwrap(),
                    runner_runtime: "/tmp/factory-runner".into(),
                    max_active_runs: 1,
                    change_id: Some("change-1".into()),
                    policy_cwd: None,
                },
                5,
            )
            .unwrap();
        assert_eq!(admitted.run.phase, RunPhase::Admitted);
        run_id
    }

    fn prepared_identity() -> PreparedProcessIdentity {
        PreparedProcessIdentity {
            runtime_locator: serde_json::json!({ "path": "/tmp/factory-runner" }).to_string(),
            runtime_birth_fingerprint: "runtime-birth".into(),
            runner_locator: serde_json::json!({
                "pid": 9,
                "runner_instance_id": "22222222-2222-4222-8222-222222222222"
            })
            .to_string(),
            runner_birth_fingerprint: "runner-birth".into(),
            provider_locator: serde_json::json!({ "pid": 10 }).to_string(),
            provider_birth_fingerprint: "provider-birth".into(),
            process_group_locator: serde_json::json!({ "pgid": 10 }).to_string(),
            process_group_birth_fingerprint: "provider-birth".into(),
        }
    }

    fn release_all(store: &mut Store, run_id: &RunId, now_ms: i64) {
        for resource in store.kernel_resources(run_id).unwrap() {
            store
                .mark_resource_released(
                    &resource.id,
                    &resource.locator,
                    resource.birth_fingerprint.as_deref(),
                    now_ms,
                )
                .unwrap();
        }
    }

    #[test]
    fn credential_resolves_only_its_exact_attempt_and_durable_change() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        assert!(store.authenticate_attempt("wrong").unwrap().is_none());
        let principal = store.authenticate_attempt(BEARER).unwrap().unwrap();
        assert_eq!(principal.run_id, run_id);
        assert_eq!(principal.change_id.as_deref(), Some("change-1"));
        assert_eq!(principal.branch.as_deref(), Some("factory/change-1"));
        assert_eq!(principal.worktree, "/tmp/factory-change-1");
    }

    #[test]
    fn spawn_failure_finalizes_without_inventing_a_runner_event() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        let (finalizing, events) = store
            .fail_admitted_run(&run_id, RunFailureReason::Spawn, 6)
            .unwrap();
        assert_eq!(finalizing.phase, RunPhase::Finalizing);
        assert_eq!(
            finalizing.outcome,
            Some(RunOutcome::Failed {
                reason: RunFailureReason::Spawn
            })
        );
        assert_eq!(events.len(), 1);
        for resource in store.kernel_resources(&run_id).unwrap() {
            assert_eq!(resource.state, KernelResourceState::Releasing);
            store
                .mark_resource_released(
                    &resource.id,
                    &resource.locator,
                    resource.birth_fingerprint.as_deref(),
                    7,
                )
                .unwrap();
        }
        assert_eq!(
            store.finalize_run(&run_id, 8).unwrap().0.phase,
            RunPhase::Terminal
        );
    }

    #[test]
    fn finalization_waits_for_every_registered_resource_then_revokes_authority() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        let (running, _) = store
            .activate_prepared_run(&run_id, prepared_identity(), 6)
            .unwrap();
        assert_eq!(running.phase, RunPhase::Running);
        let (finalizing, _) = store
            .request_attempt_outcome(&run_id, &RunOutcome::Succeeded, Some("done"), 7)
            .unwrap();
        assert_eq!(finalizing.phase, RunPhase::Finalizing);
        assert!(matches!(
            store.finalize_run(&run_id, 8),
            Err(StoreError::RunResourcesUnreleased { count: 4 })
        ));
        for resource in store.kernel_resources(&run_id).unwrap() {
            store
                .mark_resource_released(
                    &resource.id,
                    &resource.locator,
                    resource.birth_fingerprint.as_deref(),
                    9,
                )
                .unwrap();
        }
        let (terminal, _) = store.finalize_run(&run_id, 10).unwrap();
        assert_eq!(terminal.phase, RunPhase::Terminal);
        assert!(store.authenticate_attempt(BEARER).unwrap().is_none());
        assert_eq!(
            store
                .get_task(&terminal.project_id, &terminal.task_id)
                .unwrap()
                .snapshot
                .status,
            TaskStatus::Succeeded
        );
    }

    #[test]
    fn activated_gate_loss_fails_without_inventing_a_terminal_event() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        store
            .activate_prepared_run(&run_id, prepared_identity(), 6)
            .unwrap();

        let (failed, events) = store
            .fail_running_run(&run_id, RunFailureReason::Process, 7)
            .unwrap();
        assert_eq!(failed.phase, RunPhase::Finalizing);
        assert_eq!(
            failed.outcome,
            Some(RunOutcome::Failed {
                reason: RunFailureReason::Process
            })
        );
        assert_eq!(failed.exit_code, None);
        assert_eq!(failed.exit_signal, None);
        assert_eq!(events.len(), 1);
        assert!(
            store
                .kernel_resources(&run_id)
                .unwrap()
                .iter()
                .all(|resource| resource.state == KernelResourceState::Releasing)
        );
        assert!(
            store
                .fail_running_run(&run_id, RunFailureReason::Process, 8)
                .unwrap()
                .1
                .is_empty()
        );
    }

    #[test]
    fn spawn_failed_event_preserves_spawn_failure_reason() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        store
            .activate_prepared_run(&run_id, prepared_identity(), 6)
            .unwrap();
        let (failed, _) = store
            .observe_attempt_exit(&run_id, 1, None, None, Some(RunFailureReason::Spawn), 7)
            .unwrap();
        assert_eq!(
            failed.outcome,
            Some(RunOutcome::Failed {
                reason: RunFailureReason::Spawn
            })
        );
    }

    #[test]
    fn cancellation_cannot_replace_a_successful_finalizing_outcome() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        store
            .activate_prepared_run(&run_id, prepared_identity(), 6)
            .unwrap();
        store
            .request_attempt_outcome(&run_id, &RunOutcome::Succeeded, Some("done"), 7)
            .unwrap();
        assert!(matches!(
            store.cancel_admitted_or_running_run(&run_id, "too late".into(), 8),
            Err(StoreError::InvalidRunState)
        ));
        assert_eq!(
            store.kernel_run(&run_id).unwrap().unwrap().outcome,
            Some(RunOutcome::Succeeded)
        );
    }

    #[test]
    fn first_attempt_outcome_wins_and_only_exact_retries_are_idempotent() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        store
            .activate_prepared_run(&run_id, prepared_identity(), 6)
            .unwrap();
        store
            .request_attempt_outcome(&run_id, &RunOutcome::Succeeded, Some("done"), 7)
            .unwrap();

        assert!(
            store
                .request_attempt_outcome(&run_id, &RunOutcome::Succeeded, Some("done"), 8)
                .unwrap()
                .1
                .is_empty()
        );
        assert!(matches!(
            store.request_attempt_outcome(
                &run_id,
                &RunOutcome::Blocked {
                    reason: "opposite result".to_owned()
                },
                None,
                9,
            ),
            Err(StoreError::AttemptOutcomeConflict)
        ));
        assert_eq!(
            store.kernel_run(&run_id).unwrap().unwrap().outcome,
            Some(RunOutcome::Succeeded)
        );
    }

    #[test]
    fn finalizer_refuses_a_different_task_incarnation_and_revision() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        store
            .fail_admitted_run(&run_id, RunFailureReason::Spawn, 6)
            .unwrap();
        release_all(&mut store, &run_id, 7);
        store
            .connection
            .execute(
                "UPDATE tasks
                 SET incarnation_id = 'replacement-incarnation',
                     work_revision = work_revision + 1
                 WHERE id = 'task-1'",
                [],
            )
            .unwrap();
        assert!(matches!(
            store.finalize_run(&run_id, 8),
            Err(StoreError::InvalidRunState)
        ));
        assert_eq!(
            store.kernel_run(&run_id).unwrap().unwrap().phase,
            RunPhase::Finalizing
        );
    }

    #[test]
    fn failed_pre_exec_attempt_restores_messages_for_the_retry() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("worker").unwrap();
        let task_id = TaskId::try_from("task-1").unwrap();
        let message_id = MessageId::try_from("message-1").unwrap();
        store
            .send_agent_message(NewAgentMessage {
                id: message_id.clone(),
                project_id: project_id.clone(),
                sender_agent_id: None,
                recipient_agent_id: agent_id.clone(),
                body: "Do not lose this".into(),
                created_at_ms: 6,
            })
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE agent_messages
                 SET delivered_at_ms = 6, delivered_run_id = ?1 WHERE id = ?2",
                params![run_id.as_str(), message_id.as_str()],
            )
            .unwrap();
        store
            .fail_admitted_run(&run_id, RunFailureReason::Spawn, 7)
            .unwrap();
        release_all(&mut store, &run_id, 8);
        store.finalize_run(&run_id, 9).unwrap();
        store.retry_task(&project_id, &task_id, 10).unwrap();

        let retry = store
            .admit_run(
                NewRunAdmission {
                    run_id: RunId::try_from("33333333-3333-4333-8333-333333333333").unwrap(),
                    project_id,
                    task_id,
                    agent_id,
                    expected_provider: Provider::Shell,
                    capability_digest: capability_digest("retry-secret"),
                    runtime_claim: "runtime-claim:66666666666646668666666666666666".into(),
                    runner_instance_id: RunnerInstanceId::try_from(
                        "44444444-4444-4444-8444-444444444444",
                    )
                    .unwrap(),
                    runner_runtime: "/tmp/factory-runner-retry".into(),
                    max_active_runs: 1,
                    change_id: Some("change-1".into()),
                    policy_cwd: None,
                },
                11,
            )
            .unwrap();
        assert_eq!(retry.target.messages.len(), 1);
        assert_eq!(retry.target.messages[0].id, message_id);
    }

    #[test]
    fn admitted_attempt_and_resources_survive_store_restart() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        let run_id = {
            let mut store = Store::open(&path).unwrap();
            admit_worker(&mut store)
        };
        let store = Store::open(&path).unwrap();
        let recovered = store.recoverable_kernel_runs().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].run.id, run_id);
        assert_eq!(recovered[0].run.phase, RunPhase::Admitted);
        assert_eq!(recovered[0].resources.len(), 2);
        assert!(recovered[0].resources.iter().any(|resource| {
            resource.kind == KernelResourceKind::RuntimeRoot
                && resource.birth_fingerprint.as_deref()
                    == Some("runtime-claim:55555555555545558555555555555555")
        }));
        assert!(store.authenticate_attempt(BEARER).unwrap().is_some());
    }

    #[test]
    fn runtime_registration_survives_crash_before_runner_spawn() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        let run_id = {
            let mut store = Store::open(&path).unwrap();
            let run_id = admit_worker(&mut store);
            let identity = prepared_identity();
            store
                .register_admitted_runtime(
                    &run_id,
                    &identity.runtime_locator,
                    "runtime-claim:55555555555545558555555555555555",
                    &identity.runtime_birth_fingerprint,
                    6,
                )
                .unwrap();
            run_id
        };
        let mut store = Store::open(&path).unwrap();
        let resources = store.kernel_resources(&run_id).unwrap();
        let runtime = resources
            .iter()
            .find(|resource| resource.kind == KernelResourceKind::RuntimeRoot)
            .unwrap();
        let runner = resources
            .iter()
            .find(|resource| resource.kind == KernelResourceKind::RunnerProcess)
            .unwrap();
        assert_eq!(runtime.state, KernelResourceState::Active);
        assert_eq!(runner.state, KernelResourceState::Declared);
        assert!(matches!(
            store.mark_resource_released(&runtime.id, &runtime.locator, None, 7),
            Err(StoreError::ResourceIdentityMismatch)
        ));
        assert!(resources.iter().any(|resource| {
            resource.kind == KernelResourceKind::RuntimeRoot
                && resource.state == KernelResourceState::Active
                && resource.birth_fingerprint.as_deref() == Some("runtime-birth")
        }));
        let identity = prepared_identity();
        store
            .register_admitted_runner(
                &run_id,
                &identity.runner_locator,
                &identity.runner_birth_fingerprint,
                7,
            )
            .unwrap();
        assert_eq!(
            store
                .activate_prepared_run(&run_id, prepared_identity(), 8)
                .unwrap()
                .0
                .phase,
            RunPhase::Running
        );
    }
}
