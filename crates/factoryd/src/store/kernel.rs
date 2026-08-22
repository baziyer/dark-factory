use std::path::Path;

use factory_core::{
    AgentId, AgentRole, ChangeId, ChangePhase, EventEnvelope, ExecutionMode, FactoryEvent,
    MessageId, PROTOCOL_VERSION, ProjectId, Provider, RunFailureReason, RunId, RunOutcome,
    RunPhase, RunSnapshot, RunnerInstanceId, TaskId, local::MAX_PROVIDER_TOOL_NAME_BYTES,
    runner::RUNNER_STARTUP_LEASE_FILE,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::changes::{
    change_mutation, invalidate_change_measurement, load_change, parse_change_phase, reserve_change,
};
use super::rust_builds::insert_completion_check_if_required;
use super::{
    AgentMessage, ChangeReservation, MAX_BLOCKED_REASON_BYTES, MAX_PATH_BYTES,
    MAX_TASK_RESULT_BYTES, MAX_WAIT_REASON_BYTES, NewAgentMessage, NewTask, Result, Store,
    StoreError, TaskDetail, agent_pause_reasons, append_agent_changed_event, append_event,
    assign_task_in_transaction, budget_from_row, insert_agent_message, insert_task, load_agent,
    load_agent_profile, load_task, parse_agent_role, parse_execution_mode, parse_id,
    parse_observer_health, parse_provider,
};

const CAPABILITY_HEX_LEN: usize = 64;
const MAX_RESOURCE_LOCATOR_BYTES: usize = 4096;
const MAX_RESOURCE_FINGERPRINT_BYTES: usize = 1024;
const MAX_RESOURCE_FAILURE_BYTES: usize = 4096;
const MAX_POLICY_RULE_BYTES: usize = 128;

pub struct NewRunAdmission {
    pub run_id: RunId,
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    pub capability_digest: String,
    pub runtime_claim: String,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
    pub max_active_runs: usize,
    pub change_reservation: ChangeReservation,
    pub policy_cwd: String,
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
    pub source_root: String,
    pub change_id: Option<ChangeId>,
    pub change_phase: Option<ChangePhase>,
    pub change_revision: Option<i64>,
    pub base_oid: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub execution_mode: ExecutionMode,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
    pub runtime_claim: String,
}

#[derive(Clone)]
pub struct AttemptPrincipal {
    pub run_id: RunId,
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    pub role: AgentRole,
    pub source_root: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptToolPolicy {
    pub tool_name: String,
    pub denied_by: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttemptToolVerdict {
    Allow,
    DenyBudget,
    DenyPolicy { rule: String },
}

pub struct RecordedAttemptToolDecision {
    pub verdict: AttemptToolVerdict,
    pub events: Vec<EventEnvelope>,
}

struct RunningAttemptAuthority {
    project_id: ProjectId,
    task_id: TaskId,
    agent_id: AgentId,
    parent_agent_id: Option<AgentId>,
    role: AgentRole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelResourceKind {
    RunnerProcess,
    ProviderProcess,
    ProcessGroup,
    RuntimeRoot,
    EffectProcess,
    EffectGroup,
    TemporaryRoot,
}

impl KernelResourceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::RunnerProcess => "runner_process",
            Self::ProviderProcess => "provider_process",
            Self::ProcessGroup => "process_group",
            Self::RuntimeRoot => "runtime_root",
            Self::EffectProcess => "effect_process",
            Self::EffectGroup => "effect_group",
            Self::TemporaryRoot => "temporary_root",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "runner_process" => Self::RunnerProcess,
            "provider_process" => Self::ProviderProcess,
            "process_group" => Self::ProcessGroup,
            "runtime_root" => Self::RuntimeRoot,
            "effect_process" => Self::EffectProcess,
            "effect_group" => Self::EffectGroup,
            "temporary_root" => Self::TemporaryRoot,
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
    const fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Active => "active",
            Self::Releasing => "releasing",
            Self::Released => "released",
            Self::Unresolved => "unresolved",
        }
    }

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
    pub change_id: Option<ChangeId>,
    pub source_root: String,
    pub runner_instance_id: RunnerInstanceId,
    pub runner_runtime: String,
    pub resources: Vec<KernelResource>,
}

impl Store {
    pub fn admit_next_run(
        &mut self,
        input: NewRunAdmission,
        now_ms: i64,
    ) -> Result<Option<AdmittedRun>> {
        validate_capability_digest(&input.capability_digest)?;
        validate_absolute_path(&input.runner_runtime)?;
        validate_absolute_path(&input.policy_cwd)?;
        validate_runtime_claim(&input.runtime_claim)?;
        if input.max_active_runs == 0 {
            return Err(StoreError::InvalidConcurrencyLimit);
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let dispatch_enabled: bool = transaction.query_row(
            "SELECT dispatch_enabled FROM factory_settings WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if !dispatch_enabled {
            return Ok(None);
        }
        let agent = load_agent(&transaction, &input.agent_id)?
            .filter(|agent| agent.snapshot.project_id == input.project_id)
            .ok_or(StoreError::AgentNotFound)?;
        if agent.snapshot.paused {
            return Ok(None);
        }
        if agent.snapshot.current_run_id.is_some() {
            return Ok(None);
        }
        let task_id = transaction
            .query_row(
                "SELECT id FROM tasks
                 WHERE project_id = ?1 AND assigned_agent_id = ?2 AND status = 'queued'
                 ORDER BY priority DESC, created_at_ms, id
                 LIMIT 1",
                params![input.project_id.as_str(), input.agent_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|id| parse_id(id, 0))
            .transpose()?;
        let Some(task_id) = task_id else {
            return Ok(None);
        };
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
        let task = load_task(&transaction, &task_id)?.ok_or(StoreError::TaskNotFound)?;
        let task_title = task.snapshot.title.clone();
        let task_body = task.body.clone();
        let (task_incarnation_id, admitted_task_work_revision): (String, i64) = transaction
            .query_row(
                "SELECT incarnation_id, work_revision FROM tasks
                 WHERE id = ?1 AND project_id = ?2",
                params![task_id.as_str(), input.project_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;

        let (source_root, change_id, change_phase, change_revision, base_oid, change_event) =
            match agent.snapshot.role {
                AgentRole::Worker => {
                    let row: Option<(String, String, String, i64, Option<String>)> = transaction
                        .query_row(
                            "SELECT c.id, c.source_root, c.phase, c.revision, c.base_oid
                         FROM changes c
                         WHERE c.project_id = ?1 AND c.task_id = ?2
                           AND c.task_incarnation_id = ?3",
                            params![
                                input.project_id.as_str(),
                                task_id.as_str(),
                                task_incarnation_id,
                            ],
                            |row| {
                                Ok((
                                    row.get(0)?,
                                    row.get(1)?,
                                    row.get(2)?,
                                    row.get(3)?,
                                    row.get(4)?,
                                ))
                            },
                        )
                        .optional()?;
                    let (change_id, source_root, phase, mut revision, base_oid, mut change_event) =
                        match row {
                            Some((change_id, source_root, phase, revision, base_oid)) => {
                                (change_id, source_root, phase, revision, base_oid, None)
                            }
                            None => {
                                let mutation = reserve_change(
                                    &transaction,
                                    &input.project_id,
                                    &task_id,
                                    &task_incarnation_id,
                                    &input.change_reservation,
                                    now_ms,
                                )?;
                                let change_event = mutation.event;
                                let change = mutation.change;
                                (
                                    change.id.to_string(),
                                    change.source_root,
                                    "provisioning".to_owned(),
                                    change.revision,
                                    change.base_oid,
                                    change_event,
                                )
                            }
                        };
                    let change_id = ChangeId::try_from(change_id)
                        .map_err(|_| StoreError::InvalidChangeMetadata)?;
                    let phase = parse_change_phase(&phase).ok_or(StoreError::InvalidChangeState)?;
                    if phase == ChangePhase::Removed {
                        return Err(StoreError::TaskChangeUnavailable);
                    }
                    if phase == ChangePhase::Removing {
                        return Err(StoreError::InvalidChangeState);
                    }
                    if phase == ChangePhase::Available {
                        let changed = transaction.execute(
                            "UPDATE changes
                         SET size_bytes = NULL, measured_at_ms = NULL,
                             revision = revision + 1, updated_at_ms = ?1
                         WHERE id = ?2 AND project_id = ?3 AND revision = ?4
                           AND phase = 'available'",
                            params![
                                now_ms,
                                change_id.as_str(),
                                input.project_id.as_str(),
                                revision,
                            ],
                        )?;
                        if changed != 1 {
                            return Err(StoreError::ChangeRevisionConflict);
                        }
                        revision += 1;
                        let change = load_change(&transaction, &input.project_id, &change_id)?
                            .ok_or(StoreError::ChangeNotFound)?;
                        change_event = change_mutation(&transaction, change, now_ms)?.event;
                    }
                    (
                        source_root,
                        Some(change_id),
                        Some(phase),
                        Some(revision),
                        base_oid,
                        change_event,
                    )
                }
                AgentRole::Orchestrator => (input.policy_cwd.clone(), None, None, None, None, None),
            };
        validate_absolute_path(&source_root)?;

        let profile =
            load_agent_profile(&transaction, &input.agent_id)?.ok_or(StoreError::AgentNotFound)?;
        if !profile.execution_mode.supported_by(agent.snapshot.provider) {
            return Err(StoreError::UnsupportedAgentExecutionMode {
                provider: agent.snapshot.provider,
                mode: profile.execution_mode,
            });
        }
        transaction.execute(
            "UPDATE tasks
             SET status = 'running', updated_at_ms = ?1, work_revision = work_revision + 1,
                 blocked_reason = NULL, completed_at_ms = NULL
             WHERE id = ?2 AND project_id = ?3 AND status = 'queued'",
            params![now_ms, task_id.as_str(), input.project_id.as_str()],
        )?;
        transaction.execute(
            "INSERT INTO runs (
                id, project_id, agent_id, task_id, task_incarnation_id,
                admitted_task_work_revision, change_id, parent_run_id, source_root,
                phase, outcome, outcome_detail, outcome_result, capability_digest,
                provider, runtime_model, runtime_reasoning_effort, runtime_execution_mode,
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
                task_id.as_str(),
                task_incarnation_id,
                admitted_task_work_revision,
                change_id.as_ref().map(ChangeId::as_str),
                source_root,
                input.capability_digest,
                provider_str(agent.snapshot.provider),
                profile.model,
                profile.reasoning_effort,
                profile.execution_mode.as_str(),
                input.runner_instance_id.as_str(),
                input.runner_runtime,
                i64::from(factory_core::runner::RUNNER_PROTOCOL_VERSION),
                now_ms,
            ],
        )?;
        let runtime_locator = serde_json::json!({ "path": input.runner_runtime }).to_string();
        let runner_locator = runner_setup_locator(&input.runner_runtime, &input.runner_instance_id);
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

        let task = load_task(&transaction, &task_id)?
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

        let mut events = Vec::with_capacity(4);
        events.extend(change_event);
        events.push(EventEnvelope {
            protocol_version: factory_core::PROTOCOL_VERSION,
            sequence: task_sequence,
            occurred_at_ms: now_ms,
            event: task_event,
        });
        events.push(agent_event);
        events.push(EventEnvelope {
            protocol_version: factory_core::PROTOCOL_VERSION,
            sequence: run_sequence,
            occurred_at_ms: now_ms,
            event: run_event_value,
        });

        Ok(Some(AdmittedRun {
            run,
            target: AttemptTarget {
                project_id: input.project_id,
                task_id,
                agent_id: input.agent_id,
                role: agent.snapshot.role,
                provider: agent.snapshot.provider,
                task_title,
                task_body,
                messages,
                source_root,
                change_id,
                change_phase,
                change_revision,
                base_oid,
                model: profile.model,
                reasoning_effort: profile.reasoning_effort,
                execution_mode: profile.execution_mode,
                runner_instance_id: input.runner_instance_id,
                runner_runtime: input.runner_runtime,
                runtime_claim: input.runtime_claim,
            },
            events,
        }))
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

    /// Binds the exact locked startup file before the outer runner gate may
    /// be spawned. A finalizer can therefore prove whether a gate created
    /// before PID registration still holds the inherited lease.
    pub fn register_admitted_runner_setup(
        &mut self,
        run_id: &RunId,
        setup_locator: &str,
        setup_birth_fingerprint: &str,
        now_ms: i64,
    ) -> Result<()> {
        validate_resource_identity(setup_locator, setup_birth_fingerprint)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        if run.phase != RunPhase::Admitted {
            return Err(StoreError::InvalidRunState);
        }
        let runner_instance_id = run
            .runner_instance_id
            .as_ref()
            .ok_or(StoreError::InvalidExecutionMetadata)?;
        let runner_runtime: String = transaction.query_row(
            "SELECT runner_runtime FROM runs WHERE id = ?1",
            [run_id.as_str()],
            |row| row.get(0),
        )?;
        let expected_locator = runner_setup_locator(&runner_runtime, runner_instance_id);
        if setup_locator != expected_locator {
            return Err(StoreError::ResourceIdentityMismatch);
        }
        bind_runner_setup(
            &transaction,
            run_id,
            setup_locator,
            setup_birth_fingerprint,
            now_ms,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Replaces the locked setup identity with the exact stable runner PID.
    /// Cancellation is allowed to win between spawn and this transaction: in
    /// that case the PID is still recorded in `releasing`, never discarded.
    pub fn register_admitted_runner(
        &mut self,
        run_id: &RunId,
        expected_setup_locator: &str,
        expected_setup_birth_fingerprint: &str,
        runner_locator: &str,
        runner_birth_fingerprint: &str,
        now_ms: i64,
    ) -> Result<RunPhase> {
        validate_resource_identity(expected_setup_locator, expected_setup_birth_fingerprint)?;
        validate_resource_identity(runner_locator, runner_birth_fingerprint)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = load_kernel_run(&transaction, run_id)?.ok_or(StoreError::RunNotFound)?;
        let resource_state = match run.phase {
            RunPhase::Admitted => KernelResourceState::Active,
            RunPhase::Finalizing => KernelResourceState::Releasing,
            RunPhase::Running | RunPhase::Terminal => return Err(StoreError::InvalidRunState),
        };
        bind_registered_runner(
            &transaction,
            run_id,
            resource_state,
            (expected_setup_locator, expected_setup_birth_fingerprint),
            (runner_locator, runner_birth_fingerprint),
            now_ms,
        )?;
        transaction.commit()?;
        Ok(run.phase)
    }

    pub fn authenticate_attempt(&self, bearer: &str) -> Result<Option<AttemptPrincipal>> {
        let digest = capability_digest(bearer);
        self.connection
            .query_row(
                "SELECT r.id, r.project_id, r.agent_id, a.role, r.source_root
                 FROM runs r
                 JOIN agents a ON a.id = r.agent_id AND a.project_id = r.project_id
                 LEFT JOIN changes c ON c.id = r.change_id AND c.project_id = r.project_id
                WHERE r.capability_digest = ?1 AND r.phase = 'running'
                   AND (a.role = 'orchestrator' OR c.phase = 'available')",
                params![digest],
                attempt_principal_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Linearizes one provider tool call against attempt revocation. The run
    /// authority check, budget transition, and both durable audit events share
    /// one immediate transaction, so callers can observe neither a split audit
    /// trail, and a revocation that commits first makes this call fail.
    pub fn decide_tool_call_as_attempt(
        &mut self,
        run_id: &RunId,
        policy: AttemptToolPolicy,
        now_ms: i64,
    ) -> Result<RecordedAttemptToolDecision> {
        validate_attempt_tool_policy(&policy)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authority = load_running_attempt_authority(&transaction, run_id)?;
        let before = transaction.query_row(
            "SELECT max_tool_calls, tool_calls, exhausted, reset_at_ms, updated_at_ms
             FROM agent_budgets WHERE agent_id = ?1",
            [authority.agent_id.as_str()],
            budget_from_row,
        )?;
        let budget_denied = before.exhausted
            || before
                .max_tool_calls
                .is_some_and(|limit| before.tool_calls >= limit);
        if budget_denied {
            transaction.execute(
                "UPDATE agent_budgets SET exhausted = 1, updated_at_ms = ?1
                 WHERE agent_id = ?2",
                params![now_ms, authority.agent_id.as_str()],
            )?;
        } else {
            transaction.execute(
                "UPDATE agent_budgets
                 SET tool_calls = tool_calls + 1, updated_at_ms = ?1
                 WHERE agent_id = ?2",
                params![now_ms, authority.agent_id.as_str()],
            )?;
        }
        let budget = transaction.query_row(
            "SELECT max_tool_calls, tool_calls, exhausted, reset_at_ms, updated_at_ms
             FROM agent_budgets WHERE agent_id = ?1",
            [authority.agent_id.as_str()],
            budget_from_row,
        )?;
        let pause_reasons =
            agent_pause_reasons(&transaction, &authority.project_id, &authority.agent_id)?;
        let budget_event = FactoryEvent::AgentBudgetChanged {
            project_id: authority.project_id.clone(),
            agent_id: authority.agent_id.clone(),
            budget,
            action: if budget_denied { "denied" } else { "observed" }.into(),
            paused: !pause_reasons.is_empty(),
            pause_reasons,
        };
        let budget_sequence = append_event(&transaction, now_ms, &budget_event)?;
        let verdict = if budget_denied {
            AttemptToolVerdict::DenyBudget
        } else if let Some(rule) = policy.denied_by.as_ref() {
            AttemptToolVerdict::DenyPolicy { rule: rule.clone() }
        } else {
            AttemptToolVerdict::Allow
        };
        let policy_event = FactoryEvent::PolicyDecision {
            project_id: authority.project_id,
            agent_id: authority.agent_id,
            run_id: run_id.clone(),
            tool_name: policy.tool_name,
            decision: if verdict == AttemptToolVerdict::Allow {
                "allow"
            } else {
                "deny"
            }
            .into(),
            rule: policy.denied_by,
        };
        let policy_sequence = append_event(&transaction, now_ms, &policy_event)?;
        transaction.commit()?;

        Ok(RecordedAttemptToolDecision {
            verdict,
            events: vec![
                EventEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    sequence: budget_sequence,
                    occurred_at_ms: now_ms,
                    event: budget_event,
                },
                EventEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    sequence: policy_sequence,
                    occurred_at_ms: now_ms,
                    event: policy_event,
                },
            ],
        })
    }

    pub fn send_message_as_attempt(
        &mut self,
        run_id: &RunId,
        requested_project_id: &ProjectId,
        id: MessageId,
        recipient_agent_id: AgentId,
        body: String,
        now_ms: i64,
    ) -> Result<AgentMessage> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authority = load_running_attempt_authority(&transaction, run_id)?;
        require_attempt_project(&authority, requested_project_id)?;
        let recipient = load_agent(&transaction, &recipient_agent_id)?
            .filter(|agent| agent.snapshot.project_id == authority.project_id)
            .ok_or(StoreError::AttemptScopeDenied)?;
        let allowed = recipient.snapshot.id == authority.agent_id
            || match authority.role {
                AgentRole::Worker => {
                    authority.parent_agent_id.as_ref() == Some(&recipient.snapshot.id)
                        || nearest_orchestrator_ancestor(
                            &transaction,
                            &authority.project_id,
                            &authority.agent_id,
                        )? == Some(recipient.snapshot.id.clone())
                }
                AgentRole::Orchestrator => is_strict_descendant(
                    &transaction,
                    &authority.project_id,
                    &authority.agent_id,
                    &recipient.snapshot.id,
                )?,
            };
        if !allowed {
            return Err(StoreError::AttemptScopeDenied);
        }
        let message = insert_agent_message(
            &transaction,
            NewAgentMessage {
                id,
                project_id: authority.project_id,
                sender_agent_id: Some(authority.agent_id),
                recipient_agent_id,
                body,
                created_at_ms: now_ms,
            },
        )?;
        transaction.commit()?;
        Ok(message)
    }

    pub fn create_task_as_attempt(
        &mut self,
        run_id: &RunId,
        mut input: NewTask,
        assigned_agent_id: Option<AgentId>,
        now_ms: i64,
    ) -> Result<(TaskDetail, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authority = load_running_attempt_authority(&transaction, run_id)?;
        require_attempt_project(&authority, &input.project_id)?;
        let assigned_agent_id = assigned_agent_id.ok_or(StoreError::AttemptScopeDenied)?;
        if authority.role != AgentRole::Orchestrator
            || !is_strict_descendant(
                &transaction,
                &authority.project_id,
                &authority.agent_id,
                &assigned_agent_id,
            )?
            || input
                .parent_task_id
                .as_ref()
                .is_some_and(|parent| parent != &authority.task_id)
        {
            return Err(StoreError::AttemptScopeDenied);
        }
        input.parent_task_id = Some(authority.task_id);
        let (task, event) = insert_task(&transaction, input, Some(assigned_agent_id), now_ms)?;
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            task,
            EventEnvelope {
                protocol_version: factory_core::PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
    }

    pub fn assign_task_as_attempt(
        &mut self,
        run_id: &RunId,
        requested_project_id: &ProjectId,
        task_id: &TaskId,
        assigned_agent_id: Option<&AgentId>,
        now_ms: i64,
    ) -> Result<(TaskDetail, EventEnvelope)> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authority = load_running_attempt_authority(&transaction, run_id)?;
        require_attempt_project(&authority, requested_project_id)?;
        let assigned_agent_id = assigned_agent_id.ok_or(StoreError::AttemptScopeDenied)?;
        if authority.role != AgentRole::Orchestrator
            || !is_strict_descendant(
                &transaction,
                &authority.project_id,
                &authority.agent_id,
                assigned_agent_id,
            )?
        {
            return Err(StoreError::AttemptScopeDenied);
        }
        let (task, event) = assign_task_in_transaction(
            &transaction,
            &authority.project_id,
            task_id,
            Some(assigned_agent_id),
            now_ms,
        )?;
        let sequence = append_event(&transaction, now_ms, &event)?;
        transaction.commit()?;
        Ok((
            task,
            EventEnvelope {
                protocol_version: factory_core::PROTOCOL_VERSION,
                sequence,
                occurred_at_ms: now_ms,
                event,
            },
        ))
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
             SET phase = 'finalizing', outcome = ?1,
                 outcome_detail = ?2, outcome_result = ?3,
                 stop_requested_at_ms = ?4,
                 finalizing_at_ms = ?4, phase_since_ms = ?4, updated_at_ms = ?4
             WHERE id = ?5 AND phase = 'running'",
            params![kind, detail, result, now_ms, run_id.as_str()],
        )?;
        insert_completion_check_if_required(&transaction, &run, outcome, now_ms)?;
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
             SET phase = 'finalizing', outcome = 'failed',
                 outcome_detail = ?1,
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
             SET phase = 'finalizing', outcome = 'cancelled',
                 outcome_detail = ?1,
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
                 SET phase = 'finalizing', outcome = 'failed',
                     outcome_detail = ?1,
                     finalizing_at_ms = ?2, phase_since_ms = ?2,
                     stop_requested_at_ms = COALESCE(stop_requested_at_ms, ?2),
                     updated_at_ms = ?2
                 WHERE id = ?3 AND phase = 'running'",
                params![failure_detail, now_ms, run_id.as_str()],
            )?;
        } else if run.phase == RunPhase::Admitted {
            transaction.execute(
                "UPDATE runs
                 SET phase = 'finalizing', outcome = 'failed',
                     outcome_detail = 'spawn',
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
             SET state = 'released',
                 released_at_ms = COALESCE(released_at_ms, ?1),
                 updated_at_ms = CASE WHEN state = 'released' THEN updated_at_ms ELSE ?1 END,
                 last_failure = CASE WHEN state = 'released' THEN last_failure ELSE NULL END
             WHERE id = ?2 AND locator = ?3
               AND ((?4 IS NULL AND birth_fingerprint IS NULL)
                    OR birth_fingerprint = ?4)
               AND state IN ('declared', 'active', 'releasing', 'released', 'unresolved')",
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

    /// Declares one external build effect before it can exist. Only a
    /// finalizing run with a nonterminal Rust completion check may add these
    /// resources; ordinary provider lifecycle resources use admission APIs.
    pub fn declare_finalizing_resource(
        &mut self,
        run_id: &RunId,
        resource_id: &str,
        kind: KernelResourceKind,
        locator: &str,
        birth_fingerprint: Option<&str>,
        now_ms: i64,
    ) -> Result<()> {
        if !matches!(
            kind,
            KernelResourceKind::EffectProcess
                | KernelResourceKind::EffectGroup
                | KernelResourceKind::TemporaryRoot
        ) {
            return Err(StoreError::InvalidExecutionMetadata);
        }
        if kind == KernelResourceKind::TemporaryRoot {
            validate_temporary_claim(
                birth_fingerprint.ok_or(StoreError::InvalidExecutionMetadata)?,
            )?;
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_active_finalizing_check(&transaction, run_id)?;
        let existing: Option<(String, String, Option<String>, String)> = transaction
            .query_row(
                "SELECT kind, locator, birth_fingerprint, state
                 FROM resources WHERE id = ?1 AND run_id = ?2",
                params![resource_id, run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        match existing {
            Some((stored_kind, stored_locator, stored_fingerprint, state))
                if stored_kind == kind.as_str()
                    && stored_locator == locator
                    && stored_fingerprint.as_deref() == birth_fingerprint
                    && state == "declared" => {}
            Some(_) => return Err(StoreError::ResourceIdentityMismatch),
            None => insert_resource(
                &transaction,
                resource_id,
                run_id,
                kind,
                locator,
                birth_fingerprint,
                now_ms,
            )?,
        }
        transaction.commit()?;
        Ok(())
    }

    /// Binds a previously declared build effect to its exact external
    /// identity. The deterministic resource ID avoids kind-wide ambiguity
    /// when a check owns more than one process group or temporary root.
    pub fn bind_finalizing_resource(
        &mut self,
        run_id: &RunId,
        resource_id: &str,
        expected_locator: &str,
        birth_fingerprint: &str,
        now_ms: i64,
    ) -> Result<()> {
        validate_resource_identity(expected_locator, birth_fingerprint)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_active_finalizing_check(&transaction, run_id)?;
        let changed = transaction.execute(
            "UPDATE resources
             SET state = 'active', birth_fingerprint = ?1, updated_at_ms = ?2
             WHERE id = ?3 AND run_id = ?4 AND locator = ?5
               AND kind IN ('effect_process', 'effect_group', 'temporary_root')
               AND state = 'declared'
               AND (birth_fingerprint IS NULL OR birth_fingerprint = ?1)",
            params![
                birth_fingerprint,
                now_ms,
                resource_id,
                run_id.as_str(),
                expected_locator,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::ResourceIdentityMismatch);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Replaces a pre-mkdir temporary-root claim with its exact directory
    /// identity. This is deliberately separate from process/group binding so
    /// a caller must present the durable claim it is replacing.
    #[allow(clippy::too_many_arguments)]
    pub fn bind_claimed_finalizing_root(
        &mut self,
        run_id: &RunId,
        resource_id: &str,
        locator: &str,
        expected_claim: &str,
        exact_fingerprint: &str,
        now_ms: i64,
    ) -> Result<()> {
        validate_resource_identity(locator, expected_claim)?;
        validate_resource_identity(locator, exact_fingerprint)?;
        validate_temporary_claim(expected_claim)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_active_finalizing_check(&transaction, run_id)?;
        let changed = transaction.execute(
            "UPDATE resources
             SET state = 'active', birth_fingerprint = ?1, updated_at_ms = ?2
             WHERE id = ?3 AND run_id = ?4 AND locator = ?5
               AND kind = 'temporary_root' AND state = 'declared'
               AND birth_fingerprint = ?6",
            params![
                exact_fingerprint,
                now_ms,
                resource_id,
                run_id.as_str(),
                locator,
                expected_claim,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::ResourceIdentityMismatch);
        }
        transaction.commit()?;
        Ok(())
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
        let proposal = run.outcome.as_ref().ok_or(StoreError::InvalidRunState)?;
        let proposed_result: Option<String> = transaction.query_row(
            "SELECT outcome_result FROM runs WHERE id = ?1",
            params![run_id.as_str()],
            |row| row.get(0),
        )?;
        let outcome = actual_outcome(&transaction, run_id, proposal)?;
        let outcome_result = matches!(outcome, RunOutcome::Succeeded)
            .then_some(proposed_result.as_deref())
            .flatten();
        let (task_status, blocked_reason, result) = task_projection(&outcome, outcome_result);
        if matches!(
            &outcome,
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
        let (outcome_kind, outcome_detail) = outcome_parts(&outcome);
        transaction.execute(
            "UPDATE runs
             SET phase = 'terminal', outcome = ?1, outcome_detail = ?2,
                 outcome_result = ?3, phase_since_ms = ?4, updated_at_ms = ?4,
                 ended_at_ms = ?4, capability_digest = NULL
             WHERE id = ?5 AND phase = 'finalizing'",
            params![
                outcome_kind,
                outcome_detail,
                outcome_result,
                now_ms,
                run_id.as_str()
            ],
        )?;
        let change_event = transaction
            .query_row(
                "SELECT change_id FROM runs WHERE id = ?1",
                [run_id.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )?
            .map(|value| parse_id::<ChangeId>(value, 0))
            .transpose()?
            .map(|change_id| {
                invalidate_change_measurement(&transaction, &run.project_id, &change_id, now_ms)
            })
            .transpose()?
            .flatten();
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
        let mut events = Vec::with_capacity(4);
        if let Some(event) = change_event {
            events.push(event);
        }
        events.extend([
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
        ]);
        Ok((run, events))
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
                let (runner_runtime, change_id, source_root): (String, Option<String>, String) =
                    self.connection.query_row(
                        "SELECT runner_runtime, change_id, source_root FROM runs WHERE id = ?1",
                        params![run_id.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )?;
                Ok(RecoverableKernelRun {
                    resources: load_resources(&self.connection, &run_id)?,
                    run,
                    change_id: change_id.map(|value| parse_id(value, 1)).transpose()?,
                    source_root,
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

fn attempt_principal_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttemptPrincipal> {
    let role: String = row.get(3)?;
    Ok(AttemptPrincipal {
        run_id: parse_id(row.get(0)?, 0)?,
        project_id: parse_id(row.get(1)?, 1)?,
        agent_id: parse_id(row.get(2)?, 2)?,
        role: parse_agent_role(&role, 3)?,
        source_root: row.get(4)?,
    })
}

fn load_running_attempt_authority(
    transaction: &Transaction<'_>,
    run_id: &RunId,
) -> Result<RunningAttemptAuthority> {
    transaction
        .query_row(
            "SELECT r.project_id, r.task_id, r.agent_id, a.parent_agent_id, a.role
             FROM runs r
             JOIN agents a ON a.id = r.agent_id AND a.project_id = r.project_id
             WHERE r.id = ?1 AND r.phase = 'running'",
            [run_id.as_str()],
            |row| {
                let parent_agent_id: Option<String> = row.get(3)?;
                let role: String = row.get(4)?;
                Ok(RunningAttemptAuthority {
                    project_id: parse_id(row.get(0)?, 0)?,
                    task_id: parse_id(row.get(1)?, 1)?,
                    agent_id: parse_id(row.get(2)?, 2)?,
                    parent_agent_id: parent_agent_id
                        .map(|value| parse_id(value, 3))
                        .transpose()?,
                    role: parse_agent_role(&role, 4)?,
                })
            },
        )
        .optional()?
        .ok_or(StoreError::InvalidHookToken)
}

fn validate_attempt_tool_policy(policy: &AttemptToolPolicy) -> Result<()> {
    let invalid_tool_name = policy.tool_name.is_empty()
        || policy.tool_name.len() > MAX_PROVIDER_TOOL_NAME_BYTES
        || policy.tool_name.chars().any(char::is_control);
    let invalid_rule = policy.denied_by.as_deref().is_some_and(|rule| {
        rule.is_empty() || rule.len() > MAX_POLICY_RULE_BYTES || rule.chars().any(char::is_control)
    });
    if invalid_tool_name || invalid_rule {
        Err(StoreError::InvalidToolPolicy)
    } else {
        Ok(())
    }
}

fn require_attempt_project(
    authority: &RunningAttemptAuthority,
    requested_project_id: &ProjectId,
) -> Result<()> {
    if authority.project_id == *requested_project_id {
        Ok(())
    } else {
        Err(StoreError::AttemptScopeDenied)
    }
}

fn is_strict_descendant(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    ancestor_id: &AgentId,
    candidate_id: &AgentId,
) -> Result<bool> {
    transaction
        .query_row(
            "WITH RECURSIVE descendants(id) AS (
                 SELECT id FROM agents
                 WHERE project_id = ?1 AND parent_agent_id = ?2
                 UNION
                 SELECT child.id FROM agents child
                 JOIN descendants parent ON child.parent_agent_id = parent.id
                 WHERE child.project_id = ?1
             )
             SELECT EXISTS(SELECT 1 FROM descendants WHERE id = ?3)",
            params![
                project_id.as_str(),
                ancestor_id.as_str(),
                candidate_id.as_str()
            ],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}

fn nearest_orchestrator_ancestor(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<Option<AgentId>> {
    transaction
        .query_row(
            "WITH RECURSIVE ancestors(id, parent_agent_id, role, depth, path) AS (
                 SELECT parent.id, parent.parent_agent_id, parent.role, 1,
                        '/' || parent.id || '/'
                 FROM agents child
                 JOIN agents parent
                   ON parent.id = child.parent_agent_id
                  AND parent.project_id = child.project_id
                 WHERE child.project_id = ?1 AND child.id = ?2
                 UNION ALL
                 SELECT parent.id, parent.parent_agent_id, parent.role,
                        ancestor.depth + 1, ancestor.path || parent.id || '/'
                 FROM ancestors ancestor
                 JOIN agents parent
                   ON parent.id = ancestor.parent_agent_id
                  AND parent.project_id = ?1
                 WHERE instr(ancestor.path, '/' || parent.id || '/') = 0
             )
             SELECT id FROM ancestors
             WHERE role = 'orchestrator'
             ORDER BY depth
             LIMIT 1",
            params![project_id.as_str(), agent_id.as_str()],
            |row| parse_id(row.get(0)?, 0),
        )
        .optional()
        .map_err(StoreError::from)
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

fn actual_outcome(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    proposal: &RunOutcome,
) -> Result<RunOutcome> {
    if !matches!(proposal, RunOutcome::Succeeded) {
        return Ok(proposal.clone());
    }
    let phase: Option<String> = transaction
        .query_row(
            "SELECT phase FROM rust_completion_checks WHERE run_id = ?1",
            [run_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    match phase.as_deref() {
        None | Some("passed") => Ok(RunOutcome::Succeeded),
        Some("failed") => Ok(RunOutcome::Failed {
            reason: RunFailureReason::Unverifiable,
        }),
        Some(_) => Err(StoreError::CompletionVerificationPending),
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

fn runner_setup_locator(runtime: &str, runner_instance_id: &RunnerInstanceId) -> String {
    serde_json::json!({
        "runner_instance_id": runner_instance_id.as_str(),
        "setup_path": Path::new(runtime).join(RUNNER_STARTUP_LEASE_FILE),
    })
    .to_string()
}

fn bind_runner_setup(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    locator: &str,
    fingerprint: &str,
    now_ms: i64,
) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE resources
         SET state = 'active', birth_fingerprint = ?1, updated_at_ms = ?2
         WHERE run_id = ?3 AND kind = 'runner_process' AND state = 'declared'
           AND locator = ?4 AND birth_fingerprint IS NULL",
        params![fingerprint, now_ms, run_id.as_str(), locator],
    )?;
    if changed == 1 {
        return Ok(());
    }
    let current: Option<(String, Option<String>, String)> = transaction
        .query_row(
            "SELECT locator, birth_fingerprint, state FROM resources
             WHERE run_id = ?1 AND kind = 'runner_process'",
            [run_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if matches!(
        current,
        Some((current_locator, Some(current_fingerprint), state))
            if current_locator == locator
                && current_fingerprint == fingerprint
                && state == "active"
    ) {
        Ok(())
    } else {
        Err(StoreError::ResourceIdentityMismatch)
    }
}

fn bind_registered_runner(
    transaction: &Transaction<'_>,
    run_id: &RunId,
    state: KernelResourceState,
    expected_setup: (&str, &str),
    runner: (&str, &str),
    now_ms: i64,
) -> Result<()> {
    let (expected_setup_locator, expected_setup_fingerprint) = expected_setup;
    let (runner_locator, runner_fingerprint) = runner;
    let state = state.as_str();
    let changed = transaction.execute(
        "UPDATE resources
         SET locator = ?1, birth_fingerprint = ?2, updated_at_ms = ?3
         WHERE run_id = ?4 AND kind = 'runner_process' AND state = ?5
           AND locator = ?6 AND birth_fingerprint = ?7",
        params![
            runner_locator,
            runner_fingerprint,
            now_ms,
            run_id.as_str(),
            state,
            expected_setup_locator,
            expected_setup_fingerprint,
        ],
    )?;
    if changed == 1 {
        return Ok(());
    }
    let current: Option<(String, Option<String>, String)> = transaction
        .query_row(
            "SELECT locator, birth_fingerprint, state FROM resources
             WHERE run_id = ?1 AND kind = 'runner_process'",
            [run_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if matches!(
        current,
        Some((current_locator, Some(current_fingerprint), current_state))
            if current_locator == runner_locator
                && current_fingerprint == runner_fingerprint
                && current_state == state
    ) {
        Ok(())
    } else {
        Err(StoreError::ResourceIdentityMismatch)
    }
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

fn require_active_finalizing_check(transaction: &Transaction<'_>, run_id: &RunId) -> Result<()> {
    let valid: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM runs r
             JOIN rust_completion_checks c ON c.run_id = r.id
             WHERE r.id = ?1 AND r.phase = 'finalizing'
               AND c.phase NOT IN ('passed', 'failed')
         )",
        [run_id.as_str()],
        |row| row.get(0),
    )?;
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidRunState)
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

fn validate_temporary_claim(claim: &str) -> Result<()> {
    let nonce = claim
        .strip_prefix("temp-claim:")
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
                    outcome, outcome_detail, outcome_result,
                    runner_instance_id,
                    runtime_model, runtime_reasoning_effort, runtime_execution_mode,
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
                    runtime_execution_mode: {
                        let value: Option<String> = row.get(12)?;
                        value
                            .map(|value| parse_execution_mode(&value, 12))
                            .transpose()?
                    },
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
    use crate::store::{
        ChangeBaseIdentity, ChangeMaterialization, ChangeRemovalKind, ChangeSourceIdentity,
        NewAgent, NewAgentMessage, NewProject, NewTask, UpdateAgentProfile,
    };
    use factory_core::TaskStatus;

    const BEARER: &str = "attempt-secret";

    fn project_incarnation(store: &Store, project_id: &ProjectId) -> String {
        store
            .connection
            .query_row(
                "SELECT incarnation_id FROM projects WHERE id = ?1",
                [project_id.as_str()],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn next_admission(run: &str, project_id: ProjectId, agent_id: AgentId) -> NewRunAdmission {
        NewRunAdmission {
            run_id: RunId::try_from(run).unwrap(),
            project_id,
            agent_id,
            capability_digest: capability_digest(BEARER),
            runtime_claim: "runtime-claim:55555555555545558555555555555555".into(),
            runner_instance_id: RunnerInstanceId::try_from("22222222-2222-4222-8222-222222222222")
                .unwrap(),
            runner_runtime: "/tmp/factory-runner".into(),
            max_active_runs: 1,
            change_reservation: ChangeReservation {
                id: ChangeId::try_from("change-1").unwrap(),
                source_root: "/tmp/factory-change-1".into(),
                max_factory_changes: 1,
            },
            policy_cwd: "/tmp/factory-runner/policy".into(),
        }
    }

    fn seed_worker(store: &mut Store) -> (ProjectId, AgentId) {
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("worker").unwrap();
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
        (project_id, agent_id)
    }

    fn queued_worker() -> (Store, ProjectId, AgentId) {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, agent_id) = seed_worker(&mut store);
        (store, project_id, agent_id)
    }

    fn queue_task(
        store: &mut Store,
        project_id: &ProjectId,
        agent_id: &AgentId,
        id: &str,
        priority: i32,
        now_ms: i64,
    ) -> TaskId {
        let id = TaskId::try_from(id).unwrap();
        store
            .create_assigned_task(
                NewTask {
                    id: id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: id.to_string(),
                    body: String::new(),
                    priority,
                },
                agent_id.clone(),
                now_ms,
            )
            .unwrap();
        id
    }

    #[test]
    fn admission_uses_the_current_head_role_and_provider_after_a_stale_read() {
        let (mut store, project_id, agent_id) = queued_worker();
        let low = queue_task(&mut store, &project_id, &agent_id, "low", 0, 3);
        let stale = store.agent_status(&project_id, &agent_id).unwrap();
        assert_eq!(stale.queue[0].id, low);
        assert_eq!(
            (stale.agent.role, stale.agent.provider),
            (AgentRole::Worker, Provider::Shell)
        );
        let high = queue_task(&mut store, &project_id, &agent_id, "high", 1, 4);
        store
            .connection
            .execute(
                "UPDATE agents SET role = 'orchestrator', provider = 'claude_code' WHERE id = ?1",
                [agent_id.as_str()],
            )
            .unwrap();

        let admitted = store
            .admit_next_run(
                next_admission(
                    "11111111-1111-4111-8111-111111111111",
                    project_id.clone(),
                    agent_id,
                ),
                5,
            )
            .unwrap()
            .unwrap();
        assert_eq!(admitted.target.task_id, high);
        assert_eq!(
            (admitted.target.role, admitted.target.provider),
            (AgentRole::Orchestrator, Provider::ClaudeCode)
        );
        assert_eq!(admitted.target.source_root, "/tmp/factory-runner/policy");
        assert!(admitted.target.change_id.is_none());
        assert_eq!(
            store.get_task(&project_id, &low).unwrap().snapshot.status,
            TaskStatus::Queued
        );
    }

    #[test]
    fn admission_uses_the_canonical_created_at_then_id_queue_tiebreakers() {
        let (mut store, project_id, agent_id) = queued_worker();
        let later_id = queue_task(&mut store, &project_id, &agent_id, "task-b", 1, 3);
        let earlier_id = queue_task(&mut store, &project_id, &agent_id, "task-a", 1, 3);

        let admitted = store
            .admit_next_run(
                next_admission(
                    "11111111-1111-4111-8111-111111111111",
                    project_id.clone(),
                    agent_id,
                ),
                4,
            )
            .unwrap()
            .unwrap();

        assert_eq!(admitted.target.task_id, earlier_id);
        assert_eq!(
            store
                .get_task(&project_id, &later_id)
                .unwrap()
                .snapshot
                .status,
            TaskStatus::Queued
        );
    }

    #[test]
    fn disabled_dispatch_admits_nothing_and_consumes_nothing() {
        let (mut store, project_id, agent_id) = queued_worker();
        let task_id = queue_task(&mut store, &project_id, &agent_id, "task", 0, 3);
        store
            .send_agent_message(NewAgentMessage {
                id: MessageId::try_from("message").unwrap(),
                project_id: project_id.clone(),
                sender_agent_id: None,
                recipient_agent_id: agent_id.clone(),
                body: "wait".into(),
                created_at_ms: 4,
            })
            .unwrap();
        store.set_dispatch_enabled(false, 5).unwrap();
        let sequence = store.latest_event_sequence().unwrap();

        assert!(
            store
                .admit_next_run(
                    next_admission(
                        "11111111-1111-4111-8111-111111111111",
                        project_id.clone(),
                        agent_id.clone(),
                    ),
                    6,
                )
                .unwrap()
                .is_none()
        );
        let counts: (i64, i64, i64) = store
            .connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM runs), (SELECT COUNT(*) FROM changes),
                        (SELECT COUNT(*) FROM resources)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(counts, (0, 0, 0));
        assert_eq!(store.latest_event_sequence().unwrap(), sequence);
        assert_eq!(
            store
                .get_task(&project_id, &task_id)
                .unwrap()
                .snapshot
                .status,
            TaskStatus::Queued
        );
        assert!(
            store
                .list_agent_messages(&project_id, &agent_id, None, 1)
                .unwrap()[0]
                .delivered_at_ms
                .is_none()
        );
    }

    fn admit_worker_provisioning_with_verification(
        store: &mut Store,
        verification: factory_core::CompletionVerification,
    ) -> (RunId, ProjectId, ChangeId) {
        let (project_id, agent_id) = seed_worker(store);
        queue_task(store, &project_id, &agent_id, "task-1", 0, 3);
        let run_id = RunId::try_from("11111111-1111-4111-8111-111111111111").unwrap();
        store
            .set_project_completion_verification(&project_id, verification, 1)
            .unwrap();
        let change_id = ChangeId::try_from("change-1").unwrap();
        let admitted = store
            .admit_next_run(
                next_admission(run_id.as_str(), project_id.clone(), agent_id),
                5,
            )
            .unwrap()
            .unwrap();
        assert_eq!(admitted.run.phase, RunPhase::Admitted);
        assert!(matches!(
            admitted.events.first(),
            Some(EventEnvelope {
                event: FactoryEvent::ChangeChanged { change },
                ..
            }) if change.id == change_id
                && change.phase == ChangePhase::Provisioning
                && change.revision == 0
        ));
        (run_id, project_id, change_id)
    }

    fn admit_worker_provisioning(store: &mut Store) -> (RunId, ProjectId, ChangeId) {
        admit_worker_provisioning_with_verification(
            store,
            factory_core::CompletionVerification::None,
        )
    }

    fn admit_worker_with_verification(
        store: &mut Store,
        verification: factory_core::CompletionVerification,
    ) -> RunId {
        let (run_id, project_id, change_id) =
            admit_worker_provisioning_with_verification(store, verification);
        let base = ChangeBaseIdentity {
            repository_root: "/tmp/factory".into(),
            device: 1,
            inode: 2,
        };
        let oid = "0123456789abcdef0123456789abcdef01234567";
        store
            .record_change_base(&project_id, &change_id, 0, oid, &base, 5)
            .unwrap();
        store
            .mark_change_available(
                &project_id,
                &change_id,
                1,
                &ChangeMaterialization {
                    base_oid: oid.into(),
                    base,
                    source: ChangeSourceIdentity {
                        source_root: "/tmp/factory-change-1".into(),
                        device: 3,
                        inode: 4,
                        size_bytes: 5,
                    },
                },
                5,
            )
            .unwrap();
        run_id
    }

    fn admit_worker(store: &mut Store) -> RunId {
        admit_worker_with_verification(store, factory_core::CompletionVerification::None)
    }

    fn codex_admission_fixture(
        store: &mut Store,
        execution_mode: ExecutionMode,
    ) -> (ProjectId, AgentId, TaskId, NewRunAdmission) {
        let (project_id, agent_id) = seed_worker(store);
        store
            .connection
            .execute(
                "UPDATE agents SET provider = 'codex' WHERE id = ?1",
                [agent_id.as_str()],
            )
            .unwrap();
        store
            .update_agent_profile(
                &project_id,
                &agent_id,
                UpdateAgentProfile {
                    model: None,
                    reasoning_effort: None,
                    model_selection_reason: None,
                    execution_mode,
                },
                3,
            )
            .unwrap();
        let task_id = queue_task(store, &project_id, &agent_id, "task-1", 0, 4);
        let admission = next_admission(
            "11111111-1111-4111-8111-111111111111",
            project_id.clone(),
            agent_id.clone(),
        );
        (project_id, agent_id, task_id, admission)
    }

    #[test]
    fn admitted_attempt_keeps_the_execution_mode_frozen_on_its_run() {
        let mut store = Store::open_in_memory().unwrap();
        let (project_id, agent_id, _task_id, admission) =
            codex_admission_fixture(&mut store, ExecutionMode::PlanOnly);

        let admitted = store.admit_next_run(admission, 5).unwrap().unwrap();
        store
            .update_agent_profile(
                &project_id,
                &agent_id,
                UpdateAgentProfile {
                    model: None,
                    reasoning_effort: None,
                    model_selection_reason: None,
                    execution_mode: ExecutionMode::Unrestricted,
                },
                6,
            )
            .unwrap();

        assert_eq!(admitted.target.execution_mode, ExecutionMode::PlanOnly);
        assert_eq!(
            admitted.run.runtime_execution_mode,
            Some(ExecutionMode::PlanOnly)
        );
        let runs = store.list_runs(&project_id, None, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].runtime_execution_mode,
            Some(ExecutionMode::PlanOnly)
        );
        assert_eq!(
            store
                .get_agent_detail(&project_id, &agent_id)
                .unwrap()
                .profile
                .execution_mode,
            ExecutionMode::Unrestricted
        );
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

    fn id<T>(value: &str) -> T
    where
        T: TryFrom<String>,
        <T as TryFrom<String>>::Error: std::fmt::Debug,
    {
        T::try_from(value.to_owned()).unwrap()
    }

    fn create_scope_agent(
        store: &mut Store,
        project_id: &ProjectId,
        agent_id: &str,
        parent_agent_id: Option<&str>,
        role: AgentRole,
        now_ms: i64,
    ) {
        store
            .create_agent(
                NewAgent {
                    id: id(agent_id),
                    project_id: project_id.clone(),
                    parent_agent_id: parent_agent_id.map(id),
                    role,
                    provider: Provider::Shell,
                },
                now_ms,
            )
            .unwrap();
    }

    fn scope_attempt(authority_agent_id: &str) -> (Store, RunId, ProjectId, TaskId) {
        let mut store = Store::open_in_memory().unwrap();
        let project_id: ProjectId = id("factory");
        let other_project_id: ProjectId = id("other-project");
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
            .create_project(
                NewProject {
                    id: other_project_id.clone(),
                    name: "Other".into(),
                    root: "/tmp/other-project".into(),
                },
                1,
            )
            .unwrap();

        for (agent, parent, role) in [
            ("root-orchestrator", None, AgentRole::Orchestrator),
            ("grand-worker", Some("root-orchestrator"), AgentRole::Worker),
            ("middle-worker", Some("grand-worker"), AgentRole::Worker),
            ("worker", Some("middle-worker"), AgentRole::Worker),
            ("worker-child", Some("worker"), AgentRole::Worker),
            ("sibling-worker", Some("middle-worker"), AgentRole::Worker),
            (
                "nested-orchestrator",
                Some("grand-worker"),
                AgentRole::Orchestrator,
            ),
            (
                "orchestrator-child",
                Some("nested-orchestrator"),
                AgentRole::Worker,
            ),
            (
                "orchestrator-grandchild",
                Some("orchestrator-child"),
                AgentRole::Worker,
            ),
        ] {
            create_scope_agent(&mut store, &project_id, agent, parent, role, 2);
        }
        create_scope_agent(
            &mut store,
            &other_project_id,
            "external-worker",
            None,
            AgentRole::Worker,
            2,
        );

        let authority_task_id: TaskId = id("authority-task");
        let authority_agent_id: AgentId = id(authority_agent_id);
        store
            .create_assigned_task(
                NewTask {
                    id: authority_task_id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Authority task".into(),
                    body: "Exercise exact attempt scope".into(),
                    priority: 0,
                },
                authority_agent_id.clone(),
                3,
            )
            .unwrap();
        let run_id: RunId = id("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        let runner_instance_id: RunnerInstanceId = id("22222222-2222-4222-8222-222222222222");
        let role = if authority_agent_id.as_str() == "worker" {
            AgentRole::Worker
        } else {
            AgentRole::Orchestrator
        };
        let change_id: Option<ChangeId> = (role == AgentRole::Worker).then(|| id("scope-change"));
        store
            .admit_next_run(
                NewRunAdmission {
                    run_id: run_id.clone(),
                    project_id: project_id.clone(),
                    agent_id: authority_agent_id,
                    capability_digest: capability_digest("scope-secret"),
                    runtime_claim: "runtime-claim:cccccccccccc4ccc8ccccccccccccccc".into(),
                    runner_instance_id,
                    runner_runtime: "/tmp/factory-runner".into(),
                    max_active_runs: 1,
                    change_reservation: ChangeReservation {
                        id: change_id
                            .clone()
                            .unwrap_or_else(|| id("unused-scope-change")),
                        source_root: "/tmp/scope-change".into(),
                        max_factory_changes: 1,
                    },
                    policy_cwd: "/tmp/factory-policy".into(),
                },
                4,
            )
            .unwrap()
            .expect("queued authority task should be admitted");
        if let Some(change_id) = change_id {
            let base = ChangeBaseIdentity {
                repository_root: "/tmp/factory".into(),
                device: 11,
                inode: 12,
            };
            let base_oid = "0123456789abcdef0123456789abcdef01234567";
            store
                .record_change_base(&project_id, &change_id, 0, base_oid, &base, 5)
                .unwrap();
            store
                .mark_change_available(
                    &project_id,
                    &change_id,
                    1,
                    &ChangeMaterialization {
                        base_oid: base_oid.into(),
                        base,
                        source: ChangeSourceIdentity {
                            source_root: "/tmp/scope-change".into(),
                            device: 13,
                            inode: 14,
                            size_bytes: 15,
                        },
                    },
                    5,
                )
                .unwrap();
        }
        store
            .activate_prepared_run(&run_id, prepared_identity(), 6)
            .unwrap();
        (store, run_id, project_id, authority_task_id)
    }

    fn mutation_footprint(store: &Store) -> (i64, i64, i64, i64) {
        let event_sequence = store.latest_event_sequence().unwrap();
        let task_count = store
            .connection
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .unwrap();
        let work_revision = store
            .connection
            .query_row(
                "SELECT COALESCE(SUM(work_revision), 0) FROM tasks",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let message_count = store
            .connection
            .query_row("SELECT COUNT(*) FROM agent_messages", [], |row| row.get(0))
            .unwrap();
        (event_sequence, task_count, work_revision, message_count)
    }

    fn request_rust_success(store: &mut Store, run_id: &RunId) -> Vec<EventEnvelope> {
        store
            .activate_prepared_run(run_id, prepared_identity(), 7)
            .unwrap();
        store
            .request_attempt_outcome(run_id, &RunOutcome::Succeeded, Some("verified"), 8)
            .unwrap()
            .1
    }

    fn pass_rust_check(store: &mut Store, run_id: &RunId) {
        let cache_key = "a".repeat(64);
        let source_digest = "b".repeat(64);
        let bundle_digest = "c".repeat(64);
        let check = store
            .claim_rust_completion_check(run_id, &cache_key, 9)
            .unwrap();
        store
            .declare_rust_cache(run_id, "/tmp/factory-rust-cache", 9)
            .unwrap();
        store
            .bind_rust_cache_identity(run_id, "/tmp/factory-rust-cache", 1, 2, 9)
            .unwrap();
        store
            .record_rust_cache_measurement(
                run_id,
                check.revision,
                "/tmp/factory-rust-cache",
                1,
                2,
                100,
                12,
            )
            .unwrap();
        store
            .pass_rust_completion_check(run_id, check.revision, &source_digest, &bundle_digest, 13)
            .unwrap();
    }

    #[test]
    fn credential_resolves_only_its_exact_attempt_and_available_change() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        assert!(store.authenticate_attempt("wrong").unwrap().is_none());
        assert!(store.authenticate_attempt(BEARER).unwrap().is_none());
        store
            .activate_prepared_run(&run_id, prepared_identity(), 6)
            .unwrap();
        let principal = store.authenticate_attempt(BEARER).unwrap().unwrap();
        assert_eq!(principal.run_id, run_id);
        assert_eq!(principal.source_root, "/tmp/factory-change-1");
        assert!(matches!(
            store.begin_change_removal(
                &ProjectId::try_from("factory").unwrap(),
                &ChangeId::try_from("change-1").unwrap(),
                2,
                6,
            ),
            Err(StoreError::ChangeLeased)
        ));
    }

    fn tool_policy(denied_by: Option<&str>) -> AttemptToolPolicy {
        AttemptToolPolicy {
            tool_name: "Read".into(),
            denied_by: denied_by.map(str::to_owned),
        }
    }

    #[test]
    fn tool_decision_commits_budget_and_exact_audit_events_together() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        store
            .activate_prepared_run(&run_id, prepared_identity(), 6)
            .unwrap();
        let project_id = id("factory");
        let agent_id = id("worker");
        store
            .set_agent_budget(&project_id, &agent_id, Some(2), 7)
            .unwrap();

        let allowed = store
            .decide_tool_call_as_attempt(&run_id, tool_policy(None), 8)
            .unwrap();
        assert_eq!(allowed.verdict, AttemptToolVerdict::Allow);
        assert_eq!(allowed.events.len(), 2);
        assert_eq!(allowed.events[1].sequence, allowed.events[0].sequence + 1);
        assert!(matches!(
            &allowed.events[0].event,
            FactoryEvent::AgentBudgetChanged {
                project_id: event_project,
                agent_id: event_agent,
                budget,
                action,
                ..
            } if event_project == &project_id
                && event_agent == &agent_id
                && budget.tool_calls == 1
                && !budget.exhausted
                && action == "observed"
        ));
        assert!(matches!(
            &allowed.events[1].event,
            FactoryEvent::PolicyDecision {
                project_id: event_project,
                agent_id: event_agent,
                run_id: event_run,
                decision,
                rule: None,
                ..
            } if event_project == &project_id
                && event_agent == &agent_id
                && event_run == &run_id
                && decision == "allow"
        ));

        let policy_denied = store
            .decide_tool_call_as_attempt(&run_id, tool_policy(Some("secret_path")), 9)
            .unwrap();
        assert_eq!(
            policy_denied.verdict,
            AttemptToolVerdict::DenyPolicy {
                rule: "secret_path".into()
            }
        );
        assert!(matches!(
            &policy_denied.events[1].event,
            FactoryEvent::PolicyDecision {
                decision,
                rule: Some(rule),
                ..
            } if decision == "deny" && rule == "secret_path"
        ));

        let budget_denied = store
            .decide_tool_call_as_attempt(&run_id, tool_policy(None), 10)
            .unwrap();
        assert_eq!(budget_denied.verdict, AttemptToolVerdict::DenyBudget);
        assert!(matches!(
            &budget_denied.events[0].event,
            FactoryEvent::AgentBudgetChanged { budget, action, .. }
                if budget.tool_calls == 2 && budget.exhausted && action == "denied"
        ));
        assert!(matches!(
            &budget_denied.events[1].event,
            FactoryEvent::PolicyDecision { decision, .. } if decision == "deny"
        ));
        assert!(matches!(
            store.resume_agent(&project_id, &agent_id, 11),
            Err(StoreError::AgentBudgetExhausted)
        ));

        store.pause_agent(&project_id, &agent_id, 12).unwrap();
        store
            .reset_agent_budget(&project_id, &agent_id, 13)
            .unwrap();
        let status = store.agent_status(&project_id, &agent_id).unwrap();
        assert!(status.agent.paused);
        assert_eq!(
            status.pause_reasons,
            vec![factory_core::status::AgentPauseReason::AgentHold]
        );
        store.resume_agent(&project_id, &agent_id, 14).unwrap();
    }

    #[test]
    fn tool_decision_rolls_back_budget_when_policy_audit_insert_fails() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        store
            .activate_prepared_run(&run_id, prepared_identity(), 6)
            .unwrap();
        let project_id = id("factory");
        let agent_id = id("worker");
        let budget_before = store.agent_budget(&project_id, &agent_id).unwrap();
        let sequence_before = store.latest_event_sequence().unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TEMP TRIGGER reject_policy_audit
                 BEFORE INSERT ON events WHEN NEW.kind = 'policy_decision'
                 BEGIN SELECT RAISE(ABORT, 'reject policy audit'); END;",
            )
            .unwrap();

        assert!(matches!(
            store.decide_tool_call_as_attempt(&run_id, tool_policy(None), 7),
            Err(StoreError::Sqlite(_))
        ));
        assert_eq!(
            store.agent_budget(&project_id, &agent_id).unwrap(),
            budget_before
        );
        assert_eq!(store.latest_event_sequence().unwrap(), sequence_before);
    }

    #[test]
    fn finalizing_before_tool_decision_revokes_authority_without_mutation() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        store
            .activate_prepared_run(&run_id, prepared_identity(), 6)
            .unwrap();
        store
            .request_attempt_outcome(&run_id, &RunOutcome::Succeeded, None, 7)
            .unwrap();
        let project_id = id("factory");
        let agent_id = id("worker");
        let budget_before = store.agent_budget(&project_id, &agent_id).unwrap();
        let sequence_before = store.latest_event_sequence().unwrap();

        assert!(matches!(
            store.decide_tool_call_as_attempt(&run_id, tool_policy(None), 8),
            Err(StoreError::InvalidHookToken)
        ));
        assert_eq!(
            store.agent_budget(&project_id, &agent_id).unwrap(),
            budget_before
        );
        assert_eq!(store.latest_event_sequence().unwrap(), sequence_before);
    }

    #[test]
    fn concurrent_tool_decisions_cannot_cross_the_budget_limit() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("factory.db");
        let mut store = Store::open(&database).unwrap();
        let run_id = admit_worker(&mut store);
        store
            .activate_prepared_run(&run_id, prepared_identity(), 6)
            .unwrap();
        let project_id = id("factory");
        let agent_id = id("worker");
        store
            .set_agent_budget(&project_id, &agent_id, Some(1), 7)
            .unwrap();
        let sequence_before = store.latest_event_sequence().unwrap();
        drop(store);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut joins = Vec::new();
        for now_ms in [8, 9] {
            let database = database.clone();
            let barrier = barrier.clone();
            let run_id = run_id.clone();
            joins.push(std::thread::spawn(move || {
                let mut store = Store::open(database).unwrap();
                barrier.wait();
                store
                    .decide_tool_call_as_attempt(&run_id, tool_policy(None), now_ms)
                    .unwrap()
                    .verdict
            }));
        }
        let mut verdicts = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect::<Vec<_>>();
        verdicts.sort_by_key(|verdict| match verdict {
            AttemptToolVerdict::Allow => 0,
            AttemptToolVerdict::DenyBudget => 1,
            AttemptToolVerdict::DenyPolicy { .. } => 2,
        });
        assert_eq!(
            verdicts,
            vec![AttemptToolVerdict::Allow, AttemptToolVerdict::DenyBudget]
        );

        let store = Store::open(database).unwrap();
        let budget = store.agent_budget(&project_id, &agent_id).unwrap();
        assert_eq!(budget.tool_calls, 1);
        assert!(budget.exhausted);
        let events = store.events_after(sequence_before, 4).unwrap();
        assert_eq!(events.len(), 4);
        for pair in events.chunks_exact(2) {
            assert!(matches!(
                pair[0].event,
                FactoryEvent::AgentBudgetChanged { .. }
            ));
            assert!(matches!(pair[1].event, FactoryEvent::PolicyDecision { .. }));
            assert_eq!(pair[1].sequence, pair[0].sequence + 1);
        }
    }

    #[test]
    fn worker_messages_only_self_parent_and_nearest_orchestrator() {
        let (mut store, run_id, project_id, _) = scope_attempt("worker");
        for (message_id, recipient) in [
            ("message-self", "worker"),
            ("message-parent", "middle-worker"),
            ("message-orchestrator", "root-orchestrator"),
        ] {
            let message = store
                .send_message_as_attempt(
                    &run_id,
                    &project_id,
                    id(message_id),
                    id(recipient),
                    format!("message to {recipient}"),
                    7,
                )
                .unwrap();
            assert_eq!(message.sender_agent_id, Some(id("worker")));
            assert_eq!(message.recipient_agent_id, id(recipient));
        }

        let before = mutation_footprint(&store);
        for (message_id, recipient) in [
            ("denied-grandparent", "grand-worker"),
            ("denied-sibling", "sibling-worker"),
            ("denied-descendant", "worker-child"),
            ("denied-other-branch", "nested-orchestrator"),
            ("denied-cross-project", "external-worker"),
        ] {
            assert!(matches!(
                store.send_message_as_attempt(
                    &run_id,
                    &project_id,
                    id(message_id),
                    id(recipient),
                    "scope escape".into(),
                    8,
                ),
                Err(StoreError::AttemptScopeDenied)
            ));
        }
        assert_eq!(mutation_footprint(&store), before);
    }

    #[test]
    fn orchestrator_messages_only_self_and_strict_descendants() {
        let (mut store, run_id, project_id, _) = scope_attempt("nested-orchestrator");
        for (message_id, recipient) in [
            ("message-self", "nested-orchestrator"),
            ("message-child", "orchestrator-child"),
            ("message-grandchild", "orchestrator-grandchild"),
        ] {
            store
                .send_message_as_attempt(
                    &run_id,
                    &project_id,
                    id(message_id),
                    id(recipient),
                    format!("message to {recipient}"),
                    7,
                )
                .unwrap();
        }

        let before = mutation_footprint(&store);
        for (message_id, recipient) in [
            ("denied-parent", "grand-worker"),
            ("denied-ancestor", "root-orchestrator"),
            ("denied-sibling", "middle-worker"),
            ("denied-cross-project", "external-worker"),
        ] {
            assert!(matches!(
                store.send_message_as_attempt(
                    &run_id,
                    &project_id,
                    id(message_id),
                    id(recipient),
                    "scope escape".into(),
                    8,
                ),
                Err(StoreError::AttemptScopeDenied)
            ));
        }
        assert_eq!(mutation_footprint(&store), before);
    }

    #[test]
    fn worker_attempt_cannot_create_or_assign_tasks() {
        let (mut store, run_id, project_id, authority_task_id) = scope_attempt("worker");
        store
            .create_task(
                NewTask {
                    id: id("assignment-target"),
                    project_id: project_id.clone(),
                    parent_task_id: Some(authority_task_id.clone()),
                    title: "Assignment target".into(),
                    body: String::new(),
                    priority: 0,
                },
                7,
            )
            .unwrap();
        let before = mutation_footprint(&store);
        assert!(matches!(
            store.create_task_as_attempt(
                &run_id,
                NewTask {
                    id: id("worker-created-task"),
                    project_id: project_id.clone(),
                    parent_task_id: Some(authority_task_id),
                    title: "Forbidden".into(),
                    body: String::new(),
                    priority: 0,
                },
                Some(id("worker-child")),
                8,
            ),
            Err(StoreError::AttemptScopeDenied)
        ));
        assert!(matches!(
            store.assign_task_as_attempt(
                &run_id,
                &project_id,
                &id("assignment-target"),
                Some(&id("worker-child")),
                8,
            ),
            Err(StoreError::AttemptScopeDenied)
        ));
        assert_eq!(mutation_footprint(&store), before);
    }

    #[test]
    fn orchestrator_task_creation_derives_parent_and_descendant_assignment() {
        let (mut store, run_id, project_id, authority_task_id) =
            scope_attempt("nested-orchestrator");
        let (derived, _) = store
            .create_task_as_attempt(
                &run_id,
                NewTask {
                    id: id("derived-child-task"),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Derived child".into(),
                    body: String::new(),
                    priority: 0,
                },
                Some(id("orchestrator-child")),
                7,
            )
            .unwrap();
        assert_eq!(
            derived.snapshot.parent_task_id,
            Some(authority_task_id.clone())
        );
        assert_eq!(
            derived.snapshot.assigned_agent_id,
            Some(id("orchestrator-child"))
        );
        let (explicit, _) = store
            .create_task_as_attempt(
                &run_id,
                NewTask {
                    id: id("explicit-child-task"),
                    project_id: project_id.clone(),
                    parent_task_id: Some(authority_task_id.clone()),
                    title: "Explicit child".into(),
                    body: String::new(),
                    priority: 0,
                },
                Some(id("orchestrator-grandchild")),
                8,
            )
            .unwrap();
        assert_eq!(
            explicit.snapshot.parent_task_id,
            Some(authority_task_id.clone())
        );

        store
            .create_task(
                NewTask {
                    id: id("wrong-parent"),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Wrong parent".into(),
                    body: String::new(),
                    priority: 0,
                },
                9,
            )
            .unwrap();
        let before = mutation_footprint(&store);
        for (task_id, parent_task_id, assigned_agent_id) in [
            (
                "denied-wrong-parent",
                Some(id("wrong-parent")),
                Some(id("orchestrator-child")),
            ),
            (
                "denied-self-assignment",
                Some(authority_task_id.clone()),
                Some(id("nested-orchestrator")),
            ),
            (
                "denied-sibling-assignment",
                Some(authority_task_id.clone()),
                Some(id("middle-worker")),
            ),
            (
                "denied-ancestor-assignment",
                Some(authority_task_id.clone()),
                Some(id("root-orchestrator")),
            ),
            (
                "denied-cross-project-assignment",
                Some(authority_task_id.clone()),
                Some(id("external-worker")),
            ),
            (
                "denied-unassigned-task",
                Some(authority_task_id.clone()),
                None,
            ),
        ] {
            assert!(matches!(
                store.create_task_as_attempt(
                    &run_id,
                    NewTask {
                        id: id(task_id),
                        project_id: project_id.clone(),
                        parent_task_id,
                        title: "Forbidden child".into(),
                        body: String::new(),
                        priority: 0,
                    },
                    assigned_agent_id,
                    10,
                ),
                Err(StoreError::AttemptScopeDenied)
            ));
        }
        assert!(matches!(
            store.create_task_as_attempt(
                &run_id,
                NewTask {
                    id: id("denied-cross-project"),
                    project_id: id("other-project"),
                    parent_task_id: None,
                    title: "Cross-project child".into(),
                    body: String::new(),
                    priority: 0,
                },
                Some(id("external-worker")),
                10,
            ),
            Err(StoreError::AttemptScopeDenied)
        ));
        assert_eq!(mutation_footprint(&store), before);
    }

    #[test]
    fn orchestrator_assignment_requires_a_strict_descendant_and_never_unassigns() {
        let (mut store, run_id, project_id, authority_task_id) =
            scope_attempt("nested-orchestrator");
        store
            .create_task(
                NewTask {
                    id: id("assignment-target"),
                    project_id: project_id.clone(),
                    parent_task_id: Some(authority_task_id),
                    title: "Assignment target".into(),
                    body: String::new(),
                    priority: 0,
                },
                7,
            )
            .unwrap();
        let (assigned, _) = store
            .assign_task_as_attempt(
                &run_id,
                &project_id,
                &id("assignment-target"),
                Some(&id("orchestrator-grandchild")),
                8,
            )
            .unwrap();
        assert_eq!(
            assigned.snapshot.assigned_agent_id,
            Some(id("orchestrator-grandchild"))
        );

        let before = mutation_footprint(&store);
        assert!(matches!(
            store.assign_task_as_attempt(&run_id, &project_id, &id("assignment-target"), None, 9,),
            Err(StoreError::AttemptScopeDenied)
        ));
        for recipient in [
            "nested-orchestrator",
            "grand-worker",
            "root-orchestrator",
            "middle-worker",
            "external-worker",
        ] {
            assert!(matches!(
                store.assign_task_as_attempt(
                    &run_id,
                    &project_id,
                    &id("assignment-target"),
                    Some(&id(recipient)),
                    9,
                ),
                Err(StoreError::AttemptScopeDenied)
            ));
        }
        assert!(matches!(
            store.assign_task_as_attempt(
                &run_id,
                &id("other-project"),
                &id("assignment-target"),
                Some(&id("external-worker")),
                9,
            ),
            Err(StoreError::AttemptScopeDenied)
        ));
        assert_eq!(mutation_footprint(&store), before);
    }

    #[test]
    fn fabricated_task_and_run_ancestry_cannot_expand_agent_authority() {
        let (mut store, run_id, project_id, authority_task_id) =
            scope_attempt("nested-orchestrator");
        let fabricated_task_id: TaskId = id("fabricated-child-task");
        store
            .create_task(
                NewTask {
                    id: fabricated_task_id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: Some(authority_task_id.clone()),
                    title: "Fabricated ancestry".into(),
                    body: String::new(),
                    priority: 0,
                },
                7,
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO runs (
                    id, project_id, agent_id, task_id, task_incarnation_id,
                    admitted_task_work_revision, change_id, parent_run_id, source_root,
                    phase, outcome, outcome_detail, outcome_result, capability_digest,
                    provider, runtime_model, runtime_reasoning_effort, runtime_execution_mode,
                    runtime_control_mode, activity, wait_reason, observer_health, observer_reason,
                    runner_instance_id, runner_runtime, runner_protocol_version,
                    last_runner_sequence, terminal_runner_sequence, runner_reconciled_at_ms,
                    stop_requested_at_ms, admitted_at_ms, running_at_ms, finalizing_at_ms,
                    phase_since_ms, updated_at_ms, ended_at_ms, exit_code, exit_signal
                 )
                 SELECT 'fabricated-parent-run', project_id, 'middle-worker', id, incarnation_id,
                        work_revision, NULL, NULL, '/tmp/fabricated-parent',
                        'terminal', 'succeeded', NULL, NULL, NULL,
                        'shell', NULL, NULL, 'unrestricted', NULL, NULL, NULL, 'unknown', NULL,
                        NULL, NULL, NULL, 0, NULL, NULL, NULL,
                        7, NULL, 7, 7, 7, 7, NULL, NULL
                 FROM tasks WHERE id = ?1 AND project_id = ?2",
                params![fabricated_task_id.as_str(), project_id.as_str()],
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE runs SET parent_run_id = 'fabricated-parent-run' WHERE id = ?1",
                [run_id.as_str()],
            )
            .unwrap();

        let before = mutation_footprint(&store);
        assert!(matches!(
            store.assign_task_as_attempt(
                &run_id,
                &project_id,
                &fabricated_task_id,
                Some(&id("middle-worker")),
                8,
            ),
            Err(StoreError::AttemptScopeDenied)
        ));
        assert!(matches!(
            store.create_task_as_attempt(
                &run_id,
                NewTask {
                    id: id("fabricated-created-task"),
                    project_id: project_id.clone(),
                    parent_task_id: Some(authority_task_id),
                    title: "Fabricated created task".into(),
                    body: String::new(),
                    priority: 0,
                },
                Some(id("middle-worker")),
                8,
            ),
            Err(StoreError::AttemptScopeDenied)
        ));
        assert_eq!(mutation_footprint(&store), before);
    }

    #[test]
    fn finalizing_and_stale_runs_cannot_mutate_attempt_scoped_state() {
        let (mut store, run_id, project_id, authority_task_id) =
            scope_attempt("nested-orchestrator");
        store
            .create_task(
                NewTask {
                    id: id("assignment-target"),
                    project_id: project_id.clone(),
                    parent_task_id: Some(authority_task_id),
                    title: "Assignment target".into(),
                    body: String::new(),
                    priority: 0,
                },
                7,
            )
            .unwrap();
        store
            .request_attempt_outcome(&run_id, &RunOutcome::Succeeded, Some("done"), 8)
            .unwrap();
        let before = mutation_footprint(&store);
        assert!(matches!(
            store.send_message_as_attempt(
                &run_id,
                &project_id,
                id("finalizing-message"),
                id("orchestrator-child"),
                "too late".into(),
                9,
            ),
            Err(StoreError::InvalidHookToken)
        ));
        assert!(matches!(
            store.create_task_as_attempt(
                &run_id,
                NewTask {
                    id: id("finalizing-task"),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Too late".into(),
                    body: String::new(),
                    priority: 0,
                },
                Some(id("orchestrator-child")),
                9,
            ),
            Err(StoreError::InvalidHookToken)
        ));
        assert!(matches!(
            store.assign_task_as_attempt(
                &run_id,
                &project_id,
                &id("assignment-target"),
                Some(&id("orchestrator-child")),
                9,
            ),
            Err(StoreError::InvalidHookToken)
        ));
        assert_eq!(mutation_footprint(&store), before);

        release_all(&mut store, &run_id, 10);
        store.finalize_run(&run_id, 11).unwrap();
        let before = mutation_footprint(&store);
        assert!(matches!(
            store.send_message_as_attempt(
                &run_id,
                &project_id,
                id("stale-message"),
                id("orchestrator-child"),
                "stale".into(),
                12,
            ),
            Err(StoreError::InvalidHookToken)
        ));
        assert_eq!(mutation_footprint(&store), before);
    }

    #[test]
    fn rust_completion_revokes_authority_and_defers_actual_success() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker_with_verification(
            &mut store,
            factory_core::CompletionVerification::RustWorkspaceTest,
        );
        let project_id = ProjectId::try_from("factory").unwrap();
        let original_incarnation = project_incarnation(&store, &project_id);
        let _ = request_rust_success(&mut store, &run_id);

        assert!(store.authenticate_attempt(BEARER).unwrap().is_none());
        assert!(
            store
                .connection
                .query_row(
                    "SELECT capability_digest IS NOT NULL FROM runs WHERE id = ?1",
                    [run_id.as_str()],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        assert_eq!(
            store.rust_completion_check(&run_id).unwrap().unwrap().phase,
            super::super::RustCompletionPhase::Pending
        );
        assert_eq!(
            project_incarnation(&store, &project_id),
            original_incarnation
        );
        release_all(&mut store, &run_id, 9);
        assert!(matches!(
            store.finalize_run(&run_id, 10),
            Err(StoreError::CompletionVerificationPending)
        ));

        pass_rust_check(&mut store, &run_id);
        let (terminal, _) = store.finalize_run(&run_id, 14).unwrap();
        assert_eq!(terminal.outcome, Some(RunOutcome::Succeeded));
        assert!(
            !store
                .connection
                .query_row(
                    "SELECT capability_digest IS NOT NULL FROM runs WHERE id = ?1",
                    [run_id.as_str()],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        let (actual, result): (String, String) = store
            .connection
            .query_row(
                "SELECT outcome, outcome_result FROM runs WHERE id = ?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (actual.as_str(), result.as_str()),
            ("succeeded", "verified")
        );
    }

    #[test]
    fn failed_rust_completion_preserves_success_proposal_in_event_history() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker_with_verification(
            &mut store,
            factory_core::CompletionVerification::RustWorkspaceTest,
        );
        let proposal_events = request_rust_success(&mut store, &run_id);
        assert!(proposal_events.iter().any(|event| matches!(
            &event.event,
            FactoryEvent::RunChanged { run }
                if run.phase == RunPhase::Finalizing
                    && run.outcome == Some(RunOutcome::Succeeded)
        )));
        release_all(&mut store, &run_id, 9);
        let check = store
            .claim_rust_completion_check(&run_id, &"d".repeat(64), 9)
            .unwrap();
        store
            .fail_rust_completion_check(&run_id, check.revision, "cargo failed", 10)
            .unwrap();

        let (terminal, terminal_events) = store.finalize_run(&run_id, 11).unwrap();
        assert_eq!(
            terminal.outcome,
            Some(RunOutcome::Failed {
                reason: RunFailureReason::Unverifiable
            })
        );
        assert_eq!(
            store
                .get_task(
                    &ProjectId::try_from("factory").unwrap(),
                    &TaskId::try_from("task-1").unwrap(),
                )
                .unwrap()
                .snapshot
                .status,
            factory_core::TaskStatus::Failed
        );
        assert!(terminal_events.iter().any(|event| matches!(
            &event.event,
            FactoryEvent::RunChanged { run }
                if run.phase == RunPhase::Terminal
                    && run.outcome == Some(RunOutcome::Failed {
                        reason: RunFailureReason::Unverifiable
                    })
        )));
        let (actual, detail): (String, String) = store
            .connection
            .query_row(
                "SELECT outcome, outcome_detail FROM runs WHERE id = ?1",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (actual.as_str(), detail.as_str()),
            ("failed", "unverifiable")
        );
    }

    #[test]
    fn reclaim_intent_survives_restart_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("factory.sqlite3");
        let cache;
        {
            let mut store = Store::open(&database).unwrap();
            let run_id = admit_worker_with_verification(
                &mut store,
                factory_core::CompletionVerification::RustWorkspaceTest,
            );
            let _ = request_rust_success(&mut store, &run_id);
            release_all(&mut store, &run_id, 9);
            pass_rust_check(&mut store, &run_id);
            store.finalize_run(&run_id, 14).unwrap();
            cache = store.rust_reclaim_candidates(1).unwrap().remove(0);
            store.begin_rust_cache_reclaim(&cache, 15).unwrap();
        }
        let mut store = Store::open(&database).unwrap();
        assert_eq!(store.recoverable_rust_reclaims().unwrap().len(), 1);
        store.begin_rust_cache_reclaim(&cache, 16).unwrap();
        store
            .finish_rust_cache_reclaim(&cache.project_incarnation_id, &cache.cache_key)
            .unwrap();
        assert!(store.recoverable_rust_reclaims().unwrap().is_empty());
    }

    #[test]
    fn project_delete_requires_cache_absence() {
        let mut store = Store::open_in_memory().unwrap();
        let project_id = ProjectId::try_from("factory").unwrap();
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
        let incarnation = project_incarnation(&store, &project_id);
        store
            .connection
            .execute(
                "INSERT INTO rust_build_caches (
                    project_incarnation_id, cache_key, project_id, path,
                    dev, inode, bytes, lifecycle, failure,
                    created_at_ms, updated_at_ms, last_used_at_ms
                 ) VALUES (?1, ?2, ?3, '/tmp/cache', 1, 2, 3,
                           'available', NULL, 2, 2, 2)",
                params![incarnation, "e".repeat(64), project_id.as_str()],
            )
            .unwrap();
        assert!(matches!(
            store.check_project_deletable(&project_id),
            Err(StoreError::ProjectHasRustCaches)
        ));
        let cache = store.rust_reclaim_candidates(1).unwrap().remove(0);
        store.begin_rust_cache_reclaim(&cache, 3).unwrap();
        store
            .finish_rust_cache_reclaim(&cache.project_incarnation_id, &cache.cache_key)
            .unwrap();
        assert_eq!(store.rust_storage_summary().unwrap().cache_count, 0);
        store.delete_project(&project_id, 5).unwrap();
        assert!(matches!(
            store.get_project(&project_id),
            Err(StoreError::ProjectNotFound)
        ));
    }

    #[test]
    fn project_verification_and_cache_reclamation_refuse_a_live_run() {
        let mut store = Store::open_in_memory().unwrap();
        let _run_id = admit_worker(&mut store);
        let project_id = ProjectId::try_from("factory").unwrap();
        store
            .set_project_completion_verification(
                &project_id,
                factory_core::CompletionVerification::None,
                6,
            )
            .unwrap();
        assert!(matches!(
            store.set_project_completion_verification(
                &project_id,
                factory_core::CompletionVerification::RustWorkspaceTest,
                7,
            ),
            Err(StoreError::ProjectHasActiveRun)
        ));
        let incarnation = project_incarnation(&store, &project_id);
        store
            .connection
            .execute(
                "INSERT INTO rust_build_caches (
                    project_incarnation_id, cache_key, project_id, path,
                    dev, inode, bytes, lifecycle, failure,
                    created_at_ms, updated_at_ms, last_used_at_ms
                 ) VALUES (?1, ?2, ?3, '/tmp/live-cache', 1, 2, 3,
                           'available', NULL, 8, 8, 8)",
                params![incarnation, "9".repeat(64), project_id.as_str()],
            )
            .unwrap();
        assert!(matches!(
            store.begin_project_rust_cache_reclamation(&project_id, 9),
            Err(StoreError::ProjectHasActiveRun)
        ));
    }

    #[test]
    fn one_project_cache_key_has_one_active_writer() {
        let mut store = Store::open_in_memory().unwrap();
        let first_run = admit_worker_with_verification(
            &mut store,
            factory_core::CompletionVerification::RustWorkspaceTest,
        );
        let project_id = ProjectId::try_from("factory").unwrap();
        let _ = request_rust_success(&mut store, &first_run);
        let cache_key = "f".repeat(64);
        store
            .claim_rust_completion_check(&first_run, &cache_key, 9)
            .unwrap();

        let agent_id = AgentId::try_from("worker-2").unwrap();
        let task_id = TaskId::try_from("task-2").unwrap();
        let run_id = RunId::try_from("33333333-3333-4333-8333-333333333333").unwrap();
        let change_id = ChangeId::try_from("change-2").unwrap();
        store
            .create_agent(
                NewAgent {
                    id: agent_id.clone(),
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Shell,
                },
                10,
            )
            .unwrap();
        store
            .create_assigned_task(
                NewTask {
                    id: task_id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Other work".into(),
                    body: "Body".into(),
                    priority: 0,
                },
                agent_id.clone(),
                10,
            )
            .unwrap();
        store
            .admit_next_run(
                NewRunAdmission {
                    run_id: run_id.clone(),
                    project_id: project_id.clone(),
                    agent_id,
                    capability_digest: capability_digest("second-secret"),
                    runtime_claim: "runtime-claim:66666666666646668666666666666666".into(),
                    runner_instance_id: RunnerInstanceId::try_from(
                        "44444444-4444-4444-8444-444444444444",
                    )
                    .unwrap(),
                    runner_runtime: "/tmp/factory-runner-2".into(),
                    max_active_runs: 2,
                    change_reservation: ChangeReservation {
                        id: change_id.clone(),
                        source_root: "/tmp/factory-change-2".into(),
                        max_factory_changes: 2,
                    },
                    policy_cwd: "/tmp/factory-runner-2/policy".into(),
                },
                11,
            )
            .unwrap()
            .unwrap();
        let base = ChangeBaseIdentity {
            repository_root: "/tmp/factory".into(),
            device: 1,
            inode: 2,
        };
        store
            .record_change_base(
                &project_id,
                &change_id,
                0,
                "0123456789abcdef0123456789abcdef01234567",
                &base,
                12,
            )
            .unwrap();
        store
            .mark_change_available(
                &project_id,
                &change_id,
                1,
                &ChangeMaterialization {
                    base_oid: "0123456789abcdef0123456789abcdef01234567".into(),
                    base,
                    source: ChangeSourceIdentity {
                        source_root: "/tmp/factory-change-2".into(),
                        device: 3,
                        inode: 5,
                        size_bytes: 6,
                    },
                },
                12,
            )
            .unwrap();
        let mut identity = prepared_identity();
        identity.runtime_locator =
            serde_json::json!({ "path": "/tmp/factory-runner-2" }).to_string();
        identity.runner_locator = serde_json::json!({
            "pid": 19,
            "runner_instance_id": "44444444-4444-4444-8444-444444444444"
        })
        .to_string();
        identity.provider_locator = serde_json::json!({ "pid": 20 }).to_string();
        identity.process_group_locator = serde_json::json!({ "pgid": 20 }).to_string();
        store.activate_prepared_run(&run_id, identity, 13).unwrap();
        store
            .request_attempt_outcome(&run_id, &RunOutcome::Succeeded, None, 14)
            .unwrap();
        assert!(matches!(
            store.claim_rust_completion_check(&run_id, &cache_key, 15),
            Err(StoreError::RustCacheWriterBusy)
        ));
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
    fn failed_provisioning_attempt_releases_its_lease_before_change_reclamation() {
        let mut store = Store::open_in_memory().unwrap();
        let (run_id, project_id, change_id) = admit_worker_provisioning(&mut store);
        let reserved = store.change(&project_id, &change_id).unwrap().unwrap();
        assert_eq!(reserved.phase, ChangePhase::Provisioning);
        assert_eq!(reserved.revision, 0);
        assert!(
            store.recoverable_changes().unwrap().is_empty(),
            "a live materializer tree is never eligible for measurement"
        );

        let (finalizing, _) = store
            .fail_admitted_run(&run_id, RunFailureReason::Spawn, 6)
            .unwrap();
        assert_eq!(finalizing.phase, RunPhase::Finalizing);
        assert!(matches!(
            store.begin_change_removal(&project_id, &change_id, 0, 7),
            Err(StoreError::ChangeLeased)
        ));
        release_all(&mut store, &run_id, 8);
        let (terminal, _) = store.finalize_run(&run_id, 9).unwrap();
        assert_eq!(terminal.phase, RunPhase::Terminal);
        assert_eq!(
            store.recoverable_changes().unwrap(),
            vec![store.change(&project_id, &change_id).unwrap().unwrap()],
            "the same Provisioning Change becomes measurable only after terminalization"
        );
        assert!(
            store
                .kernel_resources(&run_id)
                .unwrap()
                .iter()
                .all(|resource| resource.state == KernelResourceState::Released)
        );

        let removing = store
            .begin_change_removal(&project_id, &change_id, 0, 10)
            .unwrap()
            .change;
        assert_eq!(removing.phase, ChangePhase::Removing);
        assert_eq!(removing.revision, 1);
        assert_eq!(store.recoverable_changes().unwrap(), vec![removing.clone()]);
        assert_eq!(
            removing.removal_kind().unwrap(),
            ChangeRemovalKind::Provisioning
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
        let (terminal, events) = store.finalize_run(&run_id, 10).unwrap();
        assert_eq!(terminal.phase, RunPhase::Terminal);
        assert!(store.authenticate_attempt(BEARER).unwrap().is_none());
        let change = store
            .change(
                &terminal.project_id,
                &ChangeId::try_from("change-1").unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(change.revision, 3);
        assert_eq!(change.size_bytes, None);
        assert_eq!(change.measured_at_ms, None);
        assert!(events.iter().any(|event| {
            matches!(
                &event.event,
                FactoryEvent::ChangeChanged { change }
                    if change.id.as_str() == "change-1"
                        && change.revision == 3
                        && change.measured_bytes.is_none()
            )
        }));
        assert_eq!(store.recoverable_changes().unwrap(), vec![change]);
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
    fn exact_resource_release_is_idempotent_without_rewriting_first_release() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        let resource = store.kernel_resources(&run_id).unwrap().remove(0);

        store
            .mark_resource_released(
                &resource.id,
                &resource.locator,
                resource.birth_fingerprint.as_deref(),
                7,
            )
            .unwrap();
        store
            .mark_resource_released(
                &resource.id,
                &resource.locator,
                resource.birth_fingerprint.as_deref(),
                11,
            )
            .unwrap();

        let released = store
            .kernel_resources(&run_id)
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == resource.id)
            .unwrap();
        assert_eq!(released.state, KernelResourceState::Released);
        assert_eq!(released.released_at_ms, Some(7));
        assert_eq!(released.updated_at_ms, 7);
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
            .admit_next_run(
                NewRunAdmission {
                    run_id: RunId::try_from("33333333-3333-4333-8333-333333333333").unwrap(),
                    project_id,
                    agent_id,
                    capability_digest: capability_digest("retry-secret"),
                    runtime_claim: "runtime-claim:66666666666646668666666666666666".into(),
                    runner_instance_id: RunnerInstanceId::try_from(
                        "44444444-4444-4444-8444-444444444444",
                    )
                    .unwrap(),
                    runner_runtime: "/tmp/factory-runner-retry".into(),
                    max_active_runs: 1,
                    change_reservation: ChangeReservation {
                        id: ChangeId::try_from("unused-retry-change").unwrap(),
                        source_root: "/tmp/unused-retry-change".into(),
                        max_factory_changes: 1,
                    },
                    policy_cwd: "/tmp/factory-runner-retry/policy".into(),
                },
                11,
            )
            .unwrap()
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
        assert!(store.authenticate_attempt(BEARER).unwrap().is_none());
        let runner = recovered[0]
            .resources
            .iter()
            .find(|resource| resource.kind == KernelResourceKind::RunnerProcess)
            .unwrap();
        assert_eq!(
            runner.locator,
            runner_setup_locator(
                "/tmp/factory-runner",
                &RunnerInstanceId::try_from("22222222-2222-4222-8222-222222222222").unwrap()
            )
        );
        assert_eq!(runner.birth_fingerprint, None);
    }

    #[test]
    fn cancellation_winning_runner_registration_keeps_the_exact_pid_releasing() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        let runner_instance_id =
            RunnerInstanceId::try_from("22222222-2222-4222-8222-222222222222").unwrap();
        let setup_locator = runner_setup_locator("/tmp/factory-runner", &runner_instance_id);
        store
            .register_admitted_runner_setup(&run_id, &setup_locator, "setup-birth", 6)
            .unwrap();
        store
            .cancel_admitted_or_running_run(&run_id, "operator cancelled".into(), 7)
            .unwrap();

        let identity = prepared_identity();
        assert_eq!(
            store
                .register_admitted_runner(
                    &run_id,
                    &setup_locator,
                    "setup-birth",
                    &identity.runner_locator,
                    &identity.runner_birth_fingerprint,
                    8,
                )
                .unwrap(),
            RunPhase::Finalizing
        );
        let runner = store
            .kernel_resources(&run_id)
            .unwrap()
            .into_iter()
            .find(|resource| resource.kind == KernelResourceKind::RunnerProcess)
            .unwrap();
        assert_eq!(runner.state, KernelResourceState::Releasing);
        assert_eq!(runner.locator, identity.runner_locator);
        assert_eq!(
            runner.birth_fingerprint.as_deref(),
            Some(identity.runner_birth_fingerprint.as_str())
        );
    }

    #[test]
    fn runner_setup_and_pid_binding_refuse_malformed_or_replaced_identity() {
        let mut store = Store::open_in_memory().unwrap();
        let run_id = admit_worker(&mut store);
        let runner_instance_id =
            RunnerInstanceId::try_from("22222222-2222-4222-8222-222222222222").unwrap();
        let setup_locator = runner_setup_locator("/tmp/factory-runner", &runner_instance_id);
        let wrong_locator = runner_setup_locator("/tmp/other-runner", &runner_instance_id);
        assert!(matches!(
            store.register_admitted_runner_setup(&run_id, &wrong_locator, "setup-birth", 6),
            Err(StoreError::ResourceIdentityMismatch)
        ));
        store
            .register_admitted_runner_setup(&run_id, &setup_locator, "setup-birth", 7)
            .unwrap();

        let identity = prepared_identity();
        assert!(matches!(
            store.register_admitted_runner(
                &run_id,
                &setup_locator,
                "replacement-birth",
                &identity.runner_locator,
                &identity.runner_birth_fingerprint,
                8,
            ),
            Err(StoreError::ResourceIdentityMismatch)
        ));
        let runner = store
            .kernel_resources(&run_id)
            .unwrap()
            .into_iter()
            .find(|resource| resource.kind == KernelResourceKind::RunnerProcess)
            .unwrap();
        assert_eq!(runner.state, KernelResourceState::Active);
        assert_eq!(runner.locator, setup_locator);
        assert_eq!(runner.birth_fingerprint.as_deref(), Some("setup-birth"));
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
        let setup_locator = runner_setup_locator(
            "/tmp/factory-runner",
            &RunnerInstanceId::try_from("22222222-2222-4222-8222-222222222222").unwrap(),
        );
        store
            .register_admitted_runner_setup(&run_id, &setup_locator, "setup-birth", 7)
            .unwrap();
        store
            .register_admitted_runner(
                &run_id,
                &setup_locator,
                "setup-birth",
                &identity.runner_locator,
                &identity.runner_birth_fingerprint,
                8,
            )
            .unwrap();
        assert_eq!(
            store
                .activate_prepared_run(&run_id, prepared_identity(), 9)
                .unwrap()
                .0
                .phase,
            RunPhase::Running
        );
    }
}
