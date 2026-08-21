//! Versioned local control and persisted event stream.

use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use factory_core::{
    AgentId, AgentRole, ChangeStorageSnapshot, FactoryEvent, PROTOCOL_VERSION, ProjectId,
    ProjectSnapshot, RunId, RunPhase,
    local::{
        AgentDetail as LocalAgentDetail, AgentMessage as LocalAgentMessage,
        AgentProfile as LocalAgentProfile, ErrorCode, LocalRequest, LocalResponse,
        MAX_AGENT_MESSAGE_BYTES, MAX_AGENT_PAGE_ITEMS, MAX_CHANGE_PAGE_ITEMS, MAX_EVENT_PAGE_ITEMS,
        MAX_LEGACY_SOURCE_PAGE_ITEMS, MAX_LOCAL_FRAME_BYTES, MAX_PROJECT_PAGE_ITEMS,
        MAX_RUN_PAGE_ITEMS, MAX_TASK_BODY_BYTES, MAX_TASK_PAGE_ITEMS, MAX_TASK_TITLE_BYTES,
        ProjectDetail as LocalProjectDetail, RequestCredential, RequestEnvelope,
        RustStorageSnapshot, ServerFrame, normalize_task_title,
    },
    status,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Semaphore, broadcast, watch},
    task::JoinSet,
    time::timeout,
};

pub use crate::daemon_state::DaemonState as ApiState;

use crate::{
    daemon_state::DaemonStateError,
    execution,
    guidance::{self, GuidanceError},
    store::{
        AgentMessage, AttemptPrincipal, MAX_RUST_CACHE_BYTES, MAX_RUST_CACHE_COUNT, NewAgent,
        NewAgentMessage, NewProject, NewTask, Store, StoreError, UpdateAgentProfile,
    },
};

#[derive(Clone)]
enum Principal {
    Anonymous,
    Operator,
    Attempt(AttemptPrincipal),
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use factory_core::TaskId;

    fn orchestrator_attempt() -> AttemptPrincipal {
        AttemptPrincipal {
            run_id: RunId::try_from("2f5a1e2e-2222-4444-8888-0123456789ab").unwrap(),
            project_id: ProjectId::try_from("11111111-1111-4111-8111-111111111111").unwrap(),
            agent_id: AgentId::try_from("god").unwrap(),
            role: AgentRole::Orchestrator,
            source_root: "/private/runtime/policy".into(),
        }
    }

    #[test]
    fn protocol_version_is_rejected_before_unknown_request_variants_are_parsed() {
        let payload = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION - 1,
            "request": {
                "type": "future_unknown_request",
                "data": {"future_field": "future-shape"}
            }
        });
        let Err(response) = parse_envelope(&serde_json::to_vec(&payload).unwrap()) else {
            panic!("unsupported protocol was accepted");
        };
        assert!(matches!(
            *response,
            LocalResponse::Error {
                code: ErrorCode::UnsupportedProtocol,
                ..
            }
        ));
    }

    #[test]
    fn attempt_cannot_request_source_or_guidance_paths() {
        let attempt = orchestrator_attempt();
        assert!(
            authorize_attempt(
                &attempt,
                &LocalRequest::GetProject {
                    project_id: attempt.project_id.clone(),
                },
            )
            .is_err()
        );
        assert!(
            authorize_attempt(
                &attempt,
                &LocalRequest::GetAgent {
                    project_id: attempt.project_id.clone(),
                    agent_id: attempt.agent_id.clone(),
                },
            )
            .is_err()
        );
        assert!(
            authorize_attempt(
                &attempt,
                &LocalRequest::AgentStatus {
                    project_id: attempt.project_id.clone(),
                    agent_id: attempt.agent_id.clone(),
                },
            )
            .is_err()
        );
        assert!(
            authorize_attempt(
                &attempt,
                &LocalRequest::ListTasks {
                    project_id: attempt.project_id.clone(),
                    after_id: None,
                    agent_id: None,
                    queue_revision: None,
                    history: false,
                    limit: 1,
                },
            )
            .is_ok()
        );
        assert!(authorize_attempt(&attempt, &LocalRequest::RustStorageStatus).is_err());
    }

    #[test]
    fn attempt_shape_authorization_keeps_inbox_private_and_task_edits_operator_only() {
        let attempt = orchestrator_attempt();
        assert!(
            authorize_attempt(
                &attempt,
                &LocalRequest::ListAgentMessages {
                    project_id: attempt.project_id.clone(),
                    agent_id: attempt.agent_id.clone(),
                    after_id: None,
                    limit: 1,
                },
            )
            .is_ok()
        );
        assert!(
            authorize_attempt(
                &attempt,
                &LocalRequest::ListAgentMessages {
                    project_id: attempt.project_id.clone(),
                    agent_id: AgentId::try_from("descendant").unwrap(),
                    after_id: None,
                    limit: 1,
                },
            )
            .is_err()
        );
        assert!(
            authorize_attempt(
                &attempt,
                &LocalRequest::UpdateTask {
                    project_id: attempt.project_id.clone(),
                    task_id: TaskId::try_from("task").unwrap(),
                    title: Some("rewrite".into()),
                    body: None,
                    priority: None,
                },
            )
            .is_err()
        );

        let mut worker = attempt;
        worker.role = AgentRole::Worker;
        assert!(
            authorize_attempt(
                &worker,
                &LocalRequest::CreateTask {
                    id: TaskId::try_from("child").unwrap(),
                    project_id: worker.project_id.clone(),
                    parent_task_id: None,
                    title: "child".into(),
                    body: String::new(),
                    priority: 0,
                    agent_id: Some(AgentId::try_from("descendant").unwrap()),
                },
            )
            .is_err()
        );
        assert!(
            authorize_attempt(
                &worker,
                &LocalRequest::AssignTask {
                    project_id: worker.project_id.clone(),
                    task_id: TaskId::try_from("task").unwrap(),
                    agent_id: Some(AgentId::try_from("descendant").unwrap()),
                },
            )
            .is_err()
        );
    }
}

const EVENT_REPLAY_PAGE: usize = MAX_EVENT_PAGE_ITEMS as usize;
const MAX_CONNECTIONS: usize = 128;
const IO_TIMEOUT: Duration = Duration::from_secs(15);
/// Mirrors the `tasks.result` CHECK bound (migration 0006).
const MAX_TASK_RESULT_BYTES: usize = 131_072;
/// Mirrors the `tasks.blocked_reason` CHECK bound (migration 0014).
const MAX_BLOCKED_REASON_BYTES: usize = 4096;
#[derive(Debug)]
enum ApiFailure {
    Invalid(String),
    Unauthorized(String),
    Conflict(String),
    Store(StoreError),
    Internal(String),
}

impl From<DaemonStateError> for ApiFailure {
    fn from(error: DaemonStateError) -> Self {
        match error {
            DaemonStateError::Store(error) => Self::Store(error),
            DaemonStateError::StoreLockPoisoned | DaemonStateError::StoreWorkerFailed => {
                Self::Internal(error.to_string())
            }
        }
    }
}

impl ApiFailure {
    fn into_response(self) -> LocalResponse {
        let (code, message) = match self {
            Self::Invalid(message) => (ErrorCode::InvalidRequest, message),
            Self::Unauthorized(message) => (ErrorCode::Unauthorized, message),
            Self::Conflict(message) => (ErrorCode::Conflict, message),
            Self::Store(StoreError::InvalidEventLimit) => (
                ErrorCode::InvalidRequest,
                "event limit is outside the supported range".into(),
            ),
            Self::Store(StoreError::InvalidStateLimit) => (
                ErrorCode::InvalidRequest,
                "state page limit is outside the supported range".into(),
            ),
            Self::Store(StoreError::InvalidAgentProfile) => (
                ErrorCode::InvalidRequest,
                "agent profile is invalid or exceeds its bound".into(),
            ),
            Self::Store(StoreError::InvalidAgentModelPolicy(error)) => {
                (ErrorCode::InvalidRequest, error.to_string())
            }
            Self::Store(StoreError::UnsupportedAgentExecutionMode { provider, mode }) => (
                ErrorCode::InvalidRequest,
                format!("execution mode {mode:?} is not supported by provider {provider:?}"),
            ),
            Self::Store(StoreError::InvalidAgentMessage) => (
                ErrorCode::InvalidRequest,
                "agent message is invalid or exceeds its bound".into(),
            ),
            Self::Store(StoreError::InvalidTaskResult) => (
                ErrorCode::InvalidRequest,
                "task result exceeds its bound".into(),
            ),
            Self::Store(StoreError::InvalidTaskInput) => (
                ErrorCode::InvalidRequest,
                "task title or body is invalid or exceeds its bound".into(),
            ),
            Self::Store(StoreError::InvalidBlockedReason) => (
                ErrorCode::InvalidRequest,
                "task blocked reason is empty or exceeds its bound".into(),
            ),
            Self::Store(StoreError::InvalidChangeMetadata | StoreError::InvalidChangeCapacity) => (
                ErrorCode::InvalidRequest,
                "change metadata or retention capacity is invalid".into(),
            ),
            Self::Store(StoreError::InvalidHookToken) => (
                ErrorCode::Unauthorized,
                "attempt credential is invalid".into(),
            ),
            Self::Store(StoreError::AttemptScopeDenied) => (
                ErrorCode::Unauthorized,
                "request is outside the admitted attempt's authority".into(),
            ),
            Self::Store(
                error @ (StoreError::AgentNotFound
                | StoreError::ProjectNotFound
                | StoreError::TaskNotFound
                | StoreError::RunNotFound
                | StoreError::ChangeNotFound
                | StoreError::LegacySourceNotFound
                | StoreError::ResourceNotFound),
            ) => (ErrorCode::NotFound, error.to_string()),
            Self::Store(StoreError::StaleTaskCursor) => (
                ErrorCode::Conflict,
                "task page cursor is stale; restart the listing".into(),
            ),
            Self::Store(StoreError::MissingTaskCursorRevision)
            | Self::Store(StoreError::UnexpectedTaskCursorRevision) => (
                ErrorCode::InvalidRequest,
                "task cursor and queue revision must be supplied together".into(),
            ),
            Self::Store(
                error @ (StoreError::TaskNotQueued
                | StoreError::TaskNotRetryable
                | StoreError::CapacityReached { .. }
                | StoreError::ChangeCapacityReached { .. }
                | StoreError::RustStorageCapacityReached { .. }
                | StoreError::ChangeRevisionConflict
                | StoreError::InvalidChangeState
                | StoreError::ChangeIdentityMismatch
                | StoreError::ChangeLeased
                | StoreError::TaskHasChanges
                | StoreError::TaskChangeUnavailable
                | StoreError::ProjectHasChanges
                | StoreError::ProjectHasRustCaches
                | StoreError::RunResourcesUnreleased { .. }
                | StoreError::ResourceIdentityMismatch
                | StoreError::InvalidRunState
                | StoreError::AttemptOutcomeConflict
                | StoreError::TaskNotCancellable
                | StoreError::TaskNotEditable
                | StoreError::TaskHasActiveRun
                | StoreError::TaskHasSubtasks
                | StoreError::TaskRunHasDependents
                | StoreError::AgentHasActiveRun
                | StoreError::AgentHasChildren
                | StoreError::AgentRunHasDependents
                | StoreError::AgentBudgetExhausted
                | StoreError::ProjectHasActiveRun),
            ) => (ErrorCode::Conflict, error.to_string()),
            Self::Store(error) if is_constraint_error(&error) => {
                (ErrorCode::Conflict, error.to_string())
            }
            Self::Store(error) => (ErrorCode::Internal, error.to_string()),
            Self::Internal(message) => (ErrorCode::Internal, message),
        };
        LocalResponse::Error { code, message }
    }
}

impl From<execution::Error> for ApiFailure {
    fn from(error: execution::Error) -> Self {
        match error {
            execution::Error::DeleteInProgress | execution::Error::DeleteDrainTimeout => {
                Self::Conflict(error.to_string())
            }
            execution::Error::State(DaemonStateError::Store(error)) => Self::Store(error),
            execution::Error::State(error) => Self::Internal(error.to_string()),
            error => Self::Internal(error.to_string()),
        }
    }
}

pub async fn serve<F>(
    listener: UnixListener,
    state: ApiState,
    execution: execution::Handle,
    guidance_root: PathBuf,
    operator_credential: RequestCredential,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    let guidance_root = Arc::new(guidance_root);
    let operator_credential = Arc::new(operator_credential);
    tokio::pin!(shutdown);
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut handlers = JoinSet::new();
    let result = loop {
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => break Err(error),
                };
                let permit = match Arc::clone(&connections).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::warn!("local connection limit reached");
                        continue;
                    }
                };
                let state = state.clone();
                let execution = execution.clone();
                let guidance_root = Arc::clone(&guidance_root);
                let operator_credential = Arc::clone(&operator_credential);
                let shutdown = stop_rx.clone();
                handlers.spawn(async move {
                    let _permit = permit;
                    if let Err(error) =
                        handle_connection(
                            stream,
                            state,
                            execution,
                            guidance_root,
                            operator_credential,
                            shutdown,
                        )
                        .await
                    {
                        tracing::warn!(%error, "local client disconnected with an error");
                    }
                });
            }
            completed = handlers.join_next(), if !handlers.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::warn!(%error, "local client task failed");
                }
            }
        }
    };
    drop(listener);
    let _ = stop_tx.send(true);
    while let Some(completed) = handlers.join_next().await {
        if let Err(error) = completed {
            tracing::warn!(%error, "local client task failed during shutdown");
        }
    }
    result
}

async fn handle_connection(
    stream: UnixStream,
    state: ApiState,
    execution: execution::Handle,
    guidance_root: Arc<PathBuf>,
    operator_credential: Arc<RequestCredential>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut limited = BufReader::new(read).take((MAX_LOCAL_FRAME_BYTES + 2) as u64);
    let mut payload = Vec::new();
    let read = tokio::select! {
        _ = shutdown.changed() => return Ok(()),
        result = timeout(IO_TIMEOUT, limited.read_until(b'\n', &mut payload)) => {
            result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "local request timed out"))??
        }
    };
    if read == 0 {
        return Ok(());
    }
    if payload.last() != Some(&b'\n') || payload.len() - 1 > MAX_LOCAL_FRAME_BYTES {
        return write_response(
            &mut write,
            LocalResponse::Error {
                code: ErrorCode::InvalidRequest,
                message: "request must be one newline-terminated JSON frame of at most 1 MiB"
                    .into(),
            },
        )
        .await;
    }
    payload.pop();

    let envelope = match parse_envelope(&payload) {
        Ok(envelope) => envelope,
        Err(response) => return write_response(&mut write, *response).await,
    };
    let principal = match resolve_principal(&state, envelope.credential, &operator_credential).await
    {
        Ok(principal) => principal,
        Err(failure) => return write_response(&mut write, failure.into_response()).await,
    };
    let request = envelope.request;

    if let LocalRequest::Subscribe { after_sequence } = request {
        require_operator(&principal).map_err(api_failure_to_io)?;
        if after_sequence < 0 {
            return write_response(
                &mut write,
                LocalResponse::Error {
                    code: ErrorCode::InvalidRequest,
                    message: "event cursor cannot be negative".into(),
                },
            )
            .await;
        }
        return tokio::select! {
            biased;
            _ = shutdown.changed() => Ok(()),
            result = stream_events(write, &state, after_sequence) => result,
        };
    }

    let response = handle_request(&state, &execution, &guidance_root, &principal, request)
        .await
        .unwrap_or_else(ApiFailure::into_response);
    write_response(&mut write, response).await
}

fn parse_envelope(payload: &[u8]) -> Result<RequestEnvelope, Box<LocalResponse>> {
    // Read the version discriminator before deserializing the request enum.
    // A newer client may contain a request variant this daemon does not know;
    // that must still produce UnsupportedProtocol, not a misleading invalid
    // request from enum deserialization.
    let value: serde_json::Value = serde_json::from_slice(payload).map_err(|_| {
        Box::new(LocalResponse::Error {
            code: ErrorCode::InvalidRequest,
            message: "request is not valid local protocol JSON".into(),
        })
    })?;
    let protocol_version = value
        .get("protocol_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u16::try_from(version).ok())
        .ok_or_else(|| {
            Box::new(LocalResponse::Error {
                code: ErrorCode::InvalidRequest,
                message: "request is missing a valid protocol_version".into(),
            })
        })?;
    if protocol_version != PROTOCOL_VERSION {
        return Err(Box::new(LocalResponse::Error {
            code: ErrorCode::UnsupportedProtocol,
            message: format!(
                "protocol {protocol_version} is unsupported; this daemon speaks {PROTOCOL_VERSION}",
            ),
        }));
    }
    let envelope: RequestEnvelope = serde_json::from_value(value).map_err(|_| {
        Box::new(LocalResponse::Error {
            code: ErrorCode::InvalidRequest,
            message: "request is not valid local protocol JSON".into(),
        })
    })?;
    Ok(envelope)
}

async fn resolve_principal(
    state: &ApiState,
    credential: Option<RequestCredential>,
    operator_credential: &RequestCredential,
) -> Result<Principal, ApiFailure> {
    let Some(credential) = credential else {
        return Ok(Principal::Anonymous);
    };
    if credential.expose_secret() == operator_credential.expose_secret() {
        return Ok(Principal::Operator);
    }
    let bearer = credential.expose_secret().to_owned();
    let principal = state
        .with_store(move |store| store.authenticate_attempt(&bearer))
        .await?
        .ok_or_else(|| ApiFailure::Unauthorized("invalid request credential".into()))?;
    Ok(Principal::Attempt(principal))
}

fn require_operator(principal: &Principal) -> Result<(), ApiFailure> {
    if matches!(principal, Principal::Operator) {
        Ok(())
    } else {
        Err(ApiFailure::Unauthorized(
            "operator authority is required".into(),
        ))
    }
}

fn authorize(principal: &Principal, request: &LocalRequest) -> Result<(), ApiFailure> {
    match principal {
        Principal::Anonymous if matches!(request, LocalRequest::Health) => Ok(()),
        Principal::Anonymous => Err(ApiFailure::Unauthorized(
            "a request credential is required".into(),
        )),
        Principal::Operator => match request {
            LocalRequest::CompleteAttempt { .. }
            | LocalRequest::BlockAttempt { .. }
            | LocalRequest::ProviderHook { .. } => Err(ApiFailure::Unauthorized(
                "this request requires an active attempt credential".into(),
            )),
            _ => Ok(()),
        },
        Principal::Attempt(attempt) => authorize_attempt(attempt, request),
    }
}

fn authorize_attempt(attempt: &AttemptPrincipal, request: &LocalRequest) -> Result<(), ApiFailure> {
    let same_project = |project_id: &ProjectId| project_id == &attempt.project_id;
    let orchestrator = attempt.role == AgentRole::Orchestrator;
    let allowed = match request {
        LocalRequest::Health
        | LocalRequest::CompleteAttempt { .. }
        | LocalRequest::BlockAttempt { .. } => true,
        LocalRequest::ProviderHook { .. } => true,
        LocalRequest::GetTask { project_id, .. }
        | LocalRequest::ListTasks { project_id, .. }
        | LocalRequest::ListRuns { project_id, .. } => same_project(project_id),
        LocalRequest::ListAgentMessages {
            project_id,
            agent_id,
            ..
        } => same_project(project_id) && agent_id == &attempt.agent_id,
        LocalRequest::SendAgentMessage { project_id, .. } => same_project(project_id),
        LocalRequest::CreateTask { project_id, .. }
        | LocalRequest::AssignTask { project_id, .. } => orchestrator && same_project(project_id),
        LocalRequest::ListAgents { project_id, .. } => orchestrator && same_project(project_id),
        LocalRequest::SetDispatchEnabled { .. }
        | LocalRequest::FleetStatus
        | LocalRequest::RustStorageStatus
        | LocalRequest::CreateProject { .. }
        | LocalRequest::GetProject { .. }
        | LocalRequest::ListProjects { .. }
        | LocalRequest::UpdateProjectGuidance { .. }
        | LocalRequest::SetProjectCompletionVerification { .. }
        | LocalRequest::CreateAgent { .. }
        | LocalRequest::GetAgent { .. }
        | LocalRequest::AgentStatus { .. }
        | LocalRequest::UpdateAgentProfile { .. }
        | LocalRequest::SetAgentBudget { .. }
        | LocalRequest::ResetAgentBudget { .. }
        | LocalRequest::UpdateTask { .. }
        | LocalRequest::RetryTask { .. }
        | LocalRequest::CancelTask { .. }
        | LocalRequest::PauseAgent { .. }
        | LocalRequest::ResumeAgent { .. }
        | LocalRequest::DeleteTask { .. }
        | LocalRequest::DeleteAgent { .. }
        | LocalRequest::DeleteProject { .. }
        | LocalRequest::CancelRun { .. }
        | LocalRequest::ListChanges { .. }
        | LocalRequest::RemoveChange { .. }
        | LocalRequest::ListLegacySources { .. }
        | LocalRequest::ForgetLegacySource { .. }
        | LocalRequest::EventsAfter { .. }
        | LocalRequest::LatestEventSequence
        | LocalRequest::Subscribe { .. } => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(ApiFailure::Unauthorized(
            "request is outside the admitted attempt's authority".into(),
        ))
    }
}

fn verify_mutation_attempt(store: &Store, run_id: &RunId) -> crate::store::Result<()> {
    let running = store
        .kernel_run(run_id)?
        .is_some_and(|run| run.phase == RunPhase::Running);
    if running {
        Ok(())
    } else {
        Err(StoreError::InvalidHookToken)
    }
}

async fn handle_request(
    state: &ApiState,
    execution: &execution::Handle,
    guidance_root: &Path,
    principal: &Principal,
    request: LocalRequest,
) -> Result<LocalResponse, ApiFailure> {
    authorize(principal, &request)?;
    match request {
        LocalRequest::Health => Ok(LocalResponse::Health {
            runner_path: execution.runner_program().to_string_lossy().into_owned(),
            factoryctl_path: execution.factoryctl_path().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            process_id: std::process::id(),
        }),
        LocalRequest::SetDispatchEnabled { enabled } => {
            state
                .commit_and_publish(move |store| {
                    let event = store.set_dispatch_enabled(enabled, now_ms()?)?;
                    Ok(((), vec![event]))
                })
                .await?;
            Ok(LocalResponse::DispatchSet { enabled })
        }
        LocalRequest::SetAgentBudget {
            project_id,
            agent_id,
            max_tool_calls,
        } => {
            if max_tool_calls == Some(0) {
                return Err(ApiFailure::Invalid(
                    "max tool calls must be greater than zero".into(),
                ));
            }
            let budget = state
                .commit_and_publish(move |store| {
                    let (budget, event) = store.set_agent_budget(
                        &project_id,
                        &agent_id,
                        max_tool_calls,
                        now_ms()?,
                    )?;
                    Ok((budget, vec![event]))
                })
                .await?;
            Ok(LocalResponse::AgentBudgetUpdated { budget })
        }
        LocalRequest::ResetAgentBudget {
            project_id,
            agent_id,
        } => {
            let budget = state
                .commit_and_publish(move |store| {
                    let (budget, event) =
                        store.reset_agent_budget(&project_id, &agent_id, now_ms()?)?;
                    Ok((budget, vec![event]))
                })
                .await?;
            Ok(LocalResponse::AgentBudgetUpdated { budget })
        }
        LocalRequest::FleetStatus => {
            let active_run_cap = u32::try_from(execution.max_active_runs()).unwrap_or(u32::MAX);
            let (projects, active_runs, generated_at_ms, dispatch_enabled, event_sequence) = state
                .with_store(move |store| {
                    Ok((
                        store.fleet_status()?,
                        store.recoverable_kernel_runs()?.len(),
                        now_ms()?,
                        store.dispatch_enabled()?,
                        store.latest_event_sequence()?,
                    ))
                })
                .await?;
            let active_runs = u32::try_from(active_runs).unwrap_or(u32::MAX);
            let at_capacity = active_runs >= active_run_cap;
            let mut attention = Vec::new();
            let projects: Vec<status::ProjectStatus> = projects
                .into_iter()
                .map(|rows| {
                    let crate::store::ProjectStatusRows {
                        project,
                        agents,
                        backlog,
                        blocked,
                    } = rows;
                    attention.extend(status::attention_items(
                        &project.id,
                        &agents,
                        &blocked,
                        at_capacity,
                    ));
                    status::ProjectStatus {
                        project,
                        agents,
                        backlog_depth: u32::try_from(backlog.len()).unwrap_or(u32::MAX),
                        backlog: backlog
                            .into_iter()
                            .take(status::MAX_QUEUE_PREVIEW)
                            .collect(),
                    }
                })
                .collect();
            status::sort_attention(&mut attention);
            Ok(LocalResponse::FleetStatus {
                status: status::FleetStatus {
                    generated_at_ms,
                    event_sequence,
                    dispatch_enabled,
                    active_run_cap,
                    active_runs,
                    projects,
                    attention,
                },
            })
        }
        LocalRequest::AgentStatus {
            project_id,
            agent_id,
        } => {
            let lookup_project_id = project_id.clone();
            let lookup_agent_id = agent_id.clone();
            let (agent_status, blocked, active_runs, generated_at_ms, event_sequence) = state
                .with_store(move |store| {
                    let status = store.agent_status(&lookup_project_id, &lookup_agent_id)?;
                    let blocked = store
                        .blocked_tasks(&lookup_project_id)?
                        .into_iter()
                        .filter(|blocked| {
                            blocked.task.assigned_agent_id.as_ref() == Some(&lookup_agent_id)
                        })
                        .collect::<Vec<_>>();
                    Ok((
                        status,
                        blocked,
                        store.recoverable_kernel_runs()?.len(),
                        now_ms()?,
                        store.latest_event_sequence()?,
                    ))
                })
                .await?;
            let at_capacity = active_runs >= execution.max_active_runs();
            let mut attention = status::attention_items(
                &project_id,
                std::slice::from_ref(&agent_status),
                &blocked,
                at_capacity,
            );
            status::sort_attention(&mut attention);
            let detail =
                agent_detail_with_guidance(state, execution, guidance_root, &project_id, &agent_id)
                    .await?;
            Ok(LocalResponse::AgentStatus {
                status: Box::new(status::AgentStatusDetail {
                    generated_at_ms,
                    event_sequence,
                    status: agent_status,
                    detail,
                    attention,
                }),
            })
        }
        LocalRequest::CreateProject { id, name, root } => {
            let name = required_text("project name", name, 160)?;
            let root = canonical_root(root).await?;
            let project_id = id.clone();
            let project = state
                .commit_and_publish(move |store| {
                    let (project, event) =
                        store.create_project(NewProject { id, name, root }, now_ms()?)?;
                    Ok((project, vec![event]))
                })
                .await?;
            ensure_project_guidance(guidance_root, &project_id).await?;
            Ok(LocalResponse::ProjectCreated { project })
        }
        LocalRequest::SetProjectCompletionVerification {
            project_id,
            verification,
        } => {
            if verification == factory_core::CompletionVerification::RustWorkspaceTest
                && !execution.rust_verification_available()
            {
                return Err(ApiFailure::Conflict(
                    "Rust verification is unavailable because factoryd found no fixed Cargo and rustc toolchain at startup"
                        .into(),
                ));
            }
            let project = state
                .commit_and_publish(move |store| {
                    let (project, event) = store.set_project_completion_verification(
                        &project_id,
                        verification,
                        now_ms()?,
                    )?;
                    Ok((project, vec![event]))
                })
                .await?;
            Ok(LocalResponse::ProjectCompletionVerificationUpdated { project })
        }
        LocalRequest::RustStorageStatus => {
            let storage = state
                .with_store(|store| {
                    let summary = store.rust_storage_summary()?;
                    Ok(RustStorageSnapshot {
                        max_cache_count: MAX_RUST_CACHE_COUNT,
                        max_cache_bytes: MAX_RUST_CACHE_BYTES,
                        cache_count: summary.cache_count,
                        cache_bytes: summary.cache_bytes,
                        protected_count: summary.protected_count,
                        reclaimable_count: summary.reclaimable_count,
                        failed_count: summary.failed_count,
                        cache_count_over_limit: summary.cache_count > MAX_RUST_CACHE_COUNT,
                        cache_bytes_over_limit: summary
                            .cache_bytes
                            .is_some_and(|bytes| bytes > MAX_RUST_CACHE_BYTES),
                        complete: summary.cache_bytes.is_some() && summary.failed_count == 0,
                    })
                })
                .await?;
            Ok(LocalResponse::RustStorageStatus { storage })
        }
        LocalRequest::ListProjects { after_id, limit } => {
            let limit = page_limit("project", limit, MAX_PROJECT_PAGE_ITEMS)?;
            let mut projects = state
                .with_store(move |store| store.list_projects(after_id.as_ref(), limit + 1))
                .await?;
            let next_after_id = next_cursor(&mut projects, limit, |project| project.id.clone());
            Ok(LocalResponse::Projects {
                projects,
                next_after_id,
            })
        }
        LocalRequest::CreateTask {
            id,
            project_id,
            parent_task_id,
            title,
            body,
            priority,
            agent_id,
        } => {
            let title = normalize_task_title(title).ok_or_else(|| {
                ApiFailure::Invalid(format!(
                    "task title must be between 1 and {MAX_TASK_TITLE_BYTES} bytes"
                ))
            })?;
            if body.len() > MAX_TASK_BODY_BYTES {
                return Err(ApiFailure::Invalid(format!(
                    "task body must be at most {MAX_TASK_BODY_BYTES} bytes"
                )));
            }
            let wake_project_id = project_id.clone();
            let wake_agent_id = agent_id.clone();
            let authority_run_id = match principal {
                Principal::Attempt(attempt) => Some(attempt.run_id.clone()),
                Principal::Operator => None,
                Principal::Anonymous => unreachable!("anonymous task creation was authorized"),
            };
            let task = state
                .commit_and_publish(move |store| {
                    let input = NewTask {
                        id,
                        project_id,
                        parent_task_id,
                        title,
                        body,
                        priority,
                    };
                    let (task, event) = if let Some(run_id) = authority_run_id {
                        store.create_task_as_attempt(&run_id, input, agent_id, now_ms()?)?
                    } else {
                        store.create_task_with_assignment(input, agent_id, now_ms()?)?
                    };
                    Ok((task, vec![event]))
                })
                .await?;
            if let Some(agent_id) = wake_agent_id {
                execution.wake(wake_project_id, agent_id);
            }
            Ok(LocalResponse::TaskCreated { task })
        }
        LocalRequest::CreateAgent {
            id,
            project_id,
            parent_agent_id,
            role,
            provider,
            model,
            reasoning_effort,
            model_selection_reason,
        } => {
            let created_project_id = project_id.clone();
            let created_agent_id = id.clone();
            let created_parent_agent_id = parent_agent_id.clone();
            // Deletion invariant (ARCHITECTURE.md #9, PR #50 review finding
            // 3): declines outright -- rather than silently skipping like
            // the dispatcher does -- if this project, this exact agent id,
            // or its intended parent is currently being deleted. Checked
            // and recorded in flight atomically with `DeleteProject`/
            // `DeleteAgent`'s own mark under the same lock, so a delete
            // already draining can never miss this create's guidance-file
            // writes. The
            // agent-id check also covers the narrower case of reusing an
            // id an in-flight `DeleteAgent` hasn't finished removing files
            // for yet. The parent-id check (PR #50 re-review round 3):
            // `AgentHasChildren` is the one precondition a *different*
            // request can flip from false to true after `DeleteAgent`'s
            // own precheck already passed -- without gating the parent
            // too, `CreateAgent --parent boss` racing `DeleteAgent boss`
            // could still turn it true after `boss`'s files were already
            // removed (reproduced 13/16 by the review), and leak a raw
            // `FOREIGN KEY constraint failed` SQLite error on the create's
            // own response on the other interleaving instead of this
            // gate's clear message.
            if !execution.try_begin_project_write(&created_project_id) {
                return Err(ApiFailure::Conflict(
                    "project is being deleted; cannot create an agent under it".into(),
                ));
            }
            if !execution.try_begin_agent_write(&created_agent_id) {
                execution.end_project_write(&created_project_id);
                return Err(ApiFailure::Conflict(
                    "an agent with this id is currently being deleted; wait for the delete to \
                     finish"
                        .into(),
                ));
            }
            if let Some(parent) = &created_parent_agent_id
                && !execution.try_begin_agent_write(parent)
            {
                execution.end_agent_write(&created_agent_id);
                execution.end_project_write(&created_project_id);
                return Err(ApiFailure::Conflict(
                    "the parent agent is currently being deleted; wait for the delete to finish"
                        .into(),
                ));
            }
            let create_result = create_agent_locked(
                state,
                guidance_root,
                NewAgent {
                    id,
                    project_id,
                    parent_agent_id,
                    role,
                    provider,
                },
                model,
                reasoning_effort,
                model_selection_reason,
            )
            .await;
            execution.end_agent_write(&created_agent_id);
            if let Some(parent) = &created_parent_agent_id {
                execution.end_agent_write(parent);
            }
            execution.end_project_write(&created_project_id);
            let agent = create_result?;
            Ok(LocalResponse::AgentCreated { agent })
        }
        LocalRequest::ListAgents {
            project_id,
            after_id,
            limit,
        } => {
            let limit = page_limit("agent", limit, MAX_AGENT_PAGE_ITEMS)?;
            let mut agents = state
                .with_store(move |store| {
                    store.list_agents(&project_id, after_id.as_ref(), limit + 1)
                })
                .await?;
            let next_after_id = next_cursor(&mut agents, limit, |agent| agent.id.clone());
            Ok(LocalResponse::Agents {
                agents,
                next_after_id,
            })
        }
        LocalRequest::GetAgent {
            project_id,
            agent_id,
        } => Ok(LocalResponse::Agent {
            agent: agent_detail_with_guidance(
                state,
                execution,
                guidance_root,
                &project_id,
                &agent_id,
            )
            .await?,
        }),
        LocalRequest::UpdateAgentProfile {
            project_id,
            agent_id,
            model,
            reasoning_effort,
            model_selection_reason,
            execution_mode,
            instructions,
            memory,
        } => {
            let store_project_id = project_id.clone();
            let store_agent_id = agent_id.clone();
            let agent = state
                .commit_and_publish(move |store| {
                    let (agent, event) = store.update_agent_profile(
                        &store_project_id,
                        &store_agent_id,
                        UpdateAgentProfile {
                            model,
                            reasoning_effort,
                            model_selection_reason,
                            execution_mode,
                        },
                        now_ms()?,
                    )?;
                    Ok((agent, vec![event]))
                })
                .await?;
            let agent_paths = AgentGuidancePaths::new(guidance_root, &project_id, &agent_id);
            // Deletion invariant (ARCHITECTURE.md #9, PR #50 review finding
            // 5): these writes would otherwise recreate an agent's
            // guidance files out from under a concurrent `DeleteAgent`'s
            // removal.
            if !execution.try_begin_agent_write(&agent_id) {
                return Err(ApiFailure::Conflict("agent is being deleted".into()));
            }
            let write_result =
                write_agent_guidance_files(&agent_paths, instructions.clone(), memory.clone())
                    .await;
            execution.end_agent_write(&agent_id);
            write_result?;
            let instructions_health = guidance::health_for_valid_bytes(instructions.len());
            Ok(LocalResponse::AgentProfileUpdated {
                agent: local_agent_detail(
                    agent,
                    instructions,
                    instructions_health,
                    memory.clone(),
                    guidance::health_for_valid_bytes(memory.len()),
                    agent_paths,
                ),
            })
        }
        LocalRequest::GetProject { project_id } => {
            let lookup_project_id = project_id.clone();
            let project = state
                .with_store(move |store| store.get_project(&lookup_project_id))
                .await?;
            let guidance_path =
                factory_core::paths::project_guidance_path(guidance_root, &project_id);
            let guidance = read_guidance_file(guidance_path.clone()).await?;
            Ok(LocalResponse::Project {
                project: local_project_detail(project, guidance, guidance_path),
            })
        }
        LocalRequest::UpdateProjectGuidance { project_id, text } => {
            if text.len() > guidance::MAX_GUIDANCE_FILE_BYTES {
                return Err(ApiFailure::Invalid(format!(
                    "project guidance must be at most {} bytes",
                    guidance::MAX_GUIDANCE_FILE_BYTES
                )));
            }
            let lookup_project_id = project_id.clone();
            let project = state
                .with_store(move |store| store.get_project(&lookup_project_id))
                .await?;
            let guidance_path =
                factory_core::paths::project_guidance_path(guidance_root, &project_id);
            write_guidance_file(guidance_path.clone(), text.clone()).await?;
            Ok(LocalResponse::ProjectGuidanceUpdated {
                project: local_project_detail(project, text, guidance_path),
            })
        }
        LocalRequest::SendAgentMessage {
            id,
            project_id,
            recipient_agent_id,
            body,
        } => {
            if body.len() > MAX_AGENT_MESSAGE_BYTES {
                return Err(ApiFailure::Invalid(format!(
                    "agent message must be at most {MAX_AGENT_MESSAGE_BYTES} bytes"
                )));
            }
            let wake_project_id = project_id.clone();
            let wake_agent_id = recipient_agent_id.clone();
            let authority_run_id = match principal {
                Principal::Attempt(attempt) => Some(attempt.run_id.clone()),
                Principal::Operator => None,
                Principal::Anonymous => unreachable!("anonymous messaging was authorized"),
            };
            let message = state
                .commit_and_publish(move |store| {
                    let message = if let Some(run_id) = authority_run_id {
                        store.send_message_as_attempt(
                            &run_id,
                            &project_id,
                            id,
                            recipient_agent_id,
                            body,
                            now_ms()?,
                        )?
                    } else {
                        store.send_agent_message(NewAgentMessage {
                            id,
                            project_id,
                            sender_agent_id: None,
                            recipient_agent_id,
                            body,
                            created_at_ms: now_ms()?,
                        })?
                    };
                    Ok((message, Vec::new()))
                })
                .await?;
            execution.wake(wake_project_id, wake_agent_id);
            Ok(LocalResponse::AgentMessageSent {
                message: local_agent_message(message),
            })
        }
        LocalRequest::ListAgentMessages {
            project_id,
            agent_id,
            after_id,
            limit,
        } => {
            let limit = page_limit("agent message", limit, MAX_AGENT_PAGE_ITEMS)?;
            let messages = state
                .with_store(move |store| {
                    store.list_agent_messages(&project_id, &agent_id, after_id.as_ref(), limit + 1)
                })
                .await?;
            let mut messages = messages
                .into_iter()
                .map(local_agent_message)
                .collect::<Vec<_>>();
            let next_after_id = next_cursor(&mut messages, limit, |message| message.id.clone());
            Ok(LocalResponse::AgentMessages {
                messages,
                next_after_id,
            })
        }
        LocalRequest::ListTasks {
            project_id,
            after_id,
            agent_id,
            history,
            queue_revision,
            limit,
        } => {
            let limit = page_limit("task", limit, MAX_TASK_PAGE_ITEMS)?;
            let (mut tasks, queue_revision) = state
                .with_store(move |store| {
                    store.list_tasks_filtered_at_revision(
                        &project_id,
                        after_id.as_ref(),
                        agent_id.as_ref(),
                        history,
                        limit + 1,
                        queue_revision,
                    )
                })
                .await?;
            let next_after_id = next_cursor(&mut tasks, limit, |task| task.snapshot.id.clone());
            let has_next = next_after_id.is_some();
            Ok(LocalResponse::Tasks {
                tasks,
                next_after_id,
                queue_revision: has_next.then_some(queue_revision),
            })
        }
        LocalRequest::GetTask {
            project_id,
            task_id,
        } => {
            let task = state
                .with_store(move |store| store.get_task(&project_id, &task_id))
                .await?;
            Ok(LocalResponse::Task { task })
        }
        LocalRequest::RetryTask {
            project_id,
            task_id,
        } => {
            let wake_project_id = project_id.clone();
            let task = state
                .commit_and_publish(move |store| {
                    let (task, event) = store.retry_task(&project_id, &task_id, now_ms()?)?;
                    Ok((task, vec![event]))
                })
                .await?;
            if let Some(agent_id) = task.snapshot.assigned_agent_id.clone() {
                execution.wake(wake_project_id, agent_id);
            }
            Ok(LocalResponse::TaskRetried { task })
        }
        LocalRequest::CancelTask {
            project_id,
            task_id,
        } => {
            let task = state
                .commit_and_publish(move |store| {
                    let (task, event) = store.cancel_task(&project_id, &task_id, now_ms()?)?;
                    Ok((task, vec![event]))
                })
                .await?;
            Ok(LocalResponse::TaskCancelled { task })
        }
        LocalRequest::UpdateTask {
            project_id,
            task_id,
            title,
            body,
            priority,
        } => {
            if title.is_none() && body.is_none() && priority.is_none() {
                return Err(ApiFailure::Invalid(
                    "task update must include title or body".into(),
                ));
            }
            let title = title
                .map(|title| {
                    normalize_task_title(title).ok_or_else(|| {
                        ApiFailure::Invalid(format!(
                            "task title must be between 1 and {MAX_TASK_TITLE_BYTES} bytes"
                        ))
                    })
                })
                .transpose()?;
            if let Some(body) = body.as_ref()
                && body.len() > MAX_TASK_BODY_BYTES
            {
                return Err(ApiFailure::Invalid(format!(
                    "task body must be at most {MAX_TASK_BODY_BYTES} bytes"
                )));
            }
            let task = state
                .commit_and_publish(move |store| {
                    let (task, event) = store.update_task(
                        &project_id,
                        &task_id,
                        title,
                        body,
                        priority,
                        now_ms()?,
                    )?;
                    Ok((task, vec![event]))
                })
                .await?;
            Ok(LocalResponse::TaskUpdated { task })
        }
        LocalRequest::DeleteTask {
            project_id,
            task_id,
        } => {
            let response_project_id = project_id.clone();
            let response_task_id = task_id.clone();
            state
                .commit_and_publish(move |store| {
                    let event = store.delete_task(&project_id, &task_id, now_ms()?)?;
                    Ok(((), vec![event]))
                })
                .await?;
            Ok(LocalResponse::TaskDeleted {
                project_id: response_project_id,
                task_id: response_task_id,
            })
        }
        LocalRequest::DeleteAgent {
            project_id,
            agent_id,
        } => {
            let response_project_id = project_id.clone();
            let response_agent_id = agent_id.clone();
            // Deletion invariant (ARCHITECTURE.md #9): from this call on,
            // no gated writer -- attempt preparation or a handler using
            // `try_begin_agent_write` (`GetAgent`/`AgentStatus` or
            // `UpdateAgentProfile`) -- can begin a new write for this
            // agent, and this call has waited out any write already in
            // flight, so nothing can still be writing into its guidance
            // directory below.
            execution.begin_delete(&agent_id).await?;
            let result = delete_agent_locked(state, guidance_root, project_id, agent_id).await;
            execution.end_delete(&response_agent_id);
            result?;
            Ok(LocalResponse::AgentDeleted {
                project_id: response_project_id,
                agent_id: response_agent_id,
            })
        }
        LocalRequest::DeleteProject { project_id } => {
            let response_project_id = project_id.clone();
            // Deletion invariant (ARCHITECTURE.md #9): mark the project
            // first, so no `CreateAgent` can start writing a new agent's
            // guidance tree under it (PR #50 review finding 3 --
            // the one writer the per-agent marks below can never already
            // cover, since a brand new agent doesn't exist yet for this
            // loop to have marked); then mark and drain every agent the
            // project currently has, same as `DeleteAgent`, before any
            // files go.
            execution.begin_delete_project(&project_id).await?;
            let agent_ids = list_all_agent_ids(state, &project_id).await?;
            let mut begun = Vec::with_capacity(agent_ids.len());
            let mut begin_error = None;
            for agent_id in &agent_ids {
                match execution.begin_delete(agent_id).await {
                    Ok(()) => begun.push(agent_id.clone()),
                    Err(error) => {
                        begin_error = Some(error);
                        break;
                    }
                }
            }
            let result = match begin_error {
                Some(error) => Err(ApiFailure::from(error)),
                None => delete_project_locked(state, guidance_root, project_id).await,
            };
            for agent_id in &begun {
                execution.end_delete(agent_id);
            }
            execution.end_delete_project(&response_project_id);
            result?;
            Ok(LocalResponse::ProjectDeleted {
                project_id: response_project_id,
            })
        }
        LocalRequest::AssignTask {
            project_id,
            task_id,
            agent_id,
        } => {
            let wake_project_id = project_id.clone();
            let authority_run_id = match principal {
                Principal::Attempt(attempt) => Some(attempt.run_id.clone()),
                Principal::Operator => None,
                Principal::Anonymous => unreachable!("anonymous assignment was authorized"),
            };
            let task = state
                .commit_and_publish(move |store| {
                    let (task, event) = if let Some(run_id) = authority_run_id {
                        store.assign_task_as_attempt(
                            &run_id,
                            &project_id,
                            &task_id,
                            agent_id.as_ref(),
                            now_ms()?,
                        )?
                    } else {
                        store.assign_task(&project_id, &task_id, agent_id.as_ref(), now_ms()?)?
                    };
                    Ok((task, vec![event]))
                })
                .await?;
            if let Some(agent_id) = task.snapshot.assigned_agent_id.clone() {
                execution.wake(wake_project_id, agent_id);
            }
            Ok(LocalResponse::TaskAssigned { task })
        }
        LocalRequest::CancelRun {
            project_id,
            run_id,
            grace_ms,
        } => {
            if grace_ms > 60_000 {
                return Err(ApiFailure::Invalid(
                    "runner stop grace must be at most 60000 ms".into(),
                ));
            }
            let response_run_id = run_id.clone();
            execution.cancel_run(project_id, run_id, grace_ms).await?;
            Ok(LocalResponse::RunCancelled {
                run_id: response_run_id,
            })
        }
        LocalRequest::CompleteAttempt { result } => {
            if result.len() > MAX_TASK_RESULT_BYTES {
                return Err(ApiFailure::Invalid(format!(
                    "task result must be at most {MAX_TASK_RESULT_BYTES} bytes"
                )));
            }
            let Principal::Attempt(attempt) = principal else {
                return Err(ApiFailure::Unauthorized(
                    "attempt outcome requires an active attempt".into(),
                ));
            };
            let run_id = attempt.run_id.clone();
            let request_run_id = run_id.clone();
            state
                .commit_and_publish(move |store| {
                    let (_, events) = store.request_attempt_outcome(
                        &request_run_id,
                        &factory_core::RunOutcome::Succeeded,
                        Some(&result),
                        now_ms()?,
                    )?;
                    Ok(((), events))
                })
                .await?;
            execution.wake_run(run_id.clone());
            Ok(LocalResponse::AttemptFinalizing { run_id })
        }
        LocalRequest::BlockAttempt { reason } => {
            if reason.is_empty() || reason.len() > MAX_BLOCKED_REASON_BYTES {
                return Err(ApiFailure::Invalid(format!(
                    "block reason must be between 1 and {MAX_BLOCKED_REASON_BYTES} bytes"
                )));
            }
            let Principal::Attempt(attempt) = principal else {
                return Err(ApiFailure::Unauthorized(
                    "attempt outcome requires an active attempt".into(),
                ));
            };
            let run_id = attempt.run_id.clone();
            let request_run_id = run_id.clone();
            state
                .commit_and_publish(move |store| {
                    let (_, events) = store.request_attempt_outcome(
                        &request_run_id,
                        &factory_core::RunOutcome::Blocked { reason },
                        None,
                        now_ms()?,
                    )?;
                    Ok(((), events))
                })
                .await?;
            execution.wake_run(run_id.clone());
            Ok(LocalResponse::AttemptFinalizing { run_id })
        }
        LocalRequest::PauseAgent {
            project_id,
            agent_id,
        } => {
            let agent = state
                .commit_and_publish(move |store| {
                    let (agent, event) = store.pause_agent(&project_id, &agent_id, now_ms()?)?;
                    Ok((agent, vec![event]))
                })
                .await?;
            Ok(LocalResponse::AgentPaused { agent })
        }
        LocalRequest::ResumeAgent {
            project_id,
            agent_id,
        } => {
            let wake_project_id = project_id.clone();
            let wake_agent_id = agent_id.clone();
            let agent = state
                .commit_and_publish(move |store| {
                    let (agent, event) = store.resume_agent(&project_id, &agent_id, now_ms()?)?;
                    Ok((agent, vec![event]))
                })
                .await?;
            execution.wake(wake_project_id, wake_agent_id);
            Ok(LocalResponse::AgentResumed { agent })
        }
        LocalRequest::ProviderHook { payload, .. } => {
            let Principal::Attempt(attempt) = principal else {
                return Err(ApiFailure::Unauthorized(
                    "provider hooks require an active attempt".into(),
                ));
            };
            let decision = crate::policy::decide(&payload, Path::new(&attempt.source_root));
            let project_id = attempt.project_id.clone();
            let agent_id = attempt.agent_id.clone();
            let authority_run_id = attempt.run_id.clone();
            let budget_denied = state
                .commit_and_publish(move |store| {
                    verify_mutation_attempt(store, &authority_run_id)?;
                    let (_, denied, event) =
                        store.observe_tool_call(&project_id, &agent_id, now_ms()?)?;
                    Ok((denied, vec![event]))
                })
                .await?;
            let reply = {
                let denied_by = decision.denied_by.map(str::to_owned);
                let policy_event = FactoryEvent::PolicyDecision {
                    project_id: attempt.project_id.clone(),
                    agent_id: attempt.agent_id.clone(),
                    run_id: attempt.run_id.clone(),
                    tool_name: decision.tool_name,
                    decision: if budget_denied || denied_by.is_some() {
                        "deny"
                    } else {
                        "allow"
                    }
                    .to_owned(),
                    rule: denied_by.clone(),
                };
                state
                    .commit_and_publish(move |store| {
                        let event = store.record_policy_decision(policy_event, now_ms()?)?;
                        Ok(((), vec![event]))
                    })
                    .await?;
                if budget_denied {
                    serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PreToolUse",
                            "permissionDecision": "deny",
                            "permissionDecisionReason": "Dark Factory budget exhausted"
                        }
                    })
                } else {
                    denied_by.map_or_else(
                        || serde_json::json!({}),
                        |rule| serde_json::json!({
                            "hookSpecificOutput": {
                                "hookEventName": "PreToolUse",
                                "permissionDecision": "deny",
                                "permissionDecisionReason": format!("Dark Factory policy: {rule}")
                            }
                        }),
                    )
                }
            };
            Ok(LocalResponse::ProviderHookReply { reply })
        }
        LocalRequest::ListRuns {
            project_id,
            after_id,
            limit,
        } => {
            let limit = page_limit("run", limit, MAX_RUN_PAGE_ITEMS)?;
            let mut runs = state
                .with_store(move |store| store.list_runs(&project_id, after_id.as_ref(), limit + 1))
                .await?;
            let next_after_id = next_cursor(&mut runs, limit, |run| run.id.clone());
            Ok(LocalResponse::Runs {
                runs,
                next_after_id,
            })
        }
        LocalRequest::ListChanges {
            project_id,
            after_id,
            limit,
        } => {
            let limit = page_limit("change", limit, MAX_CHANGE_PAGE_ITEMS)?;
            let (mut changes, project_summary, factory_summary) = state
                .with_store(move |store| {
                    Ok((
                        store.list_changes(&project_id, after_id.as_ref(), limit + 1)?,
                        store.change_storage_summary(Some(&project_id))?,
                        store.change_storage_summary(None)?,
                    ))
                })
                .await?;
            let next_after_id = next_cursor(&mut changes, limit, |change| change.id.clone());
            Ok(LocalResponse::Changes {
                changes,
                next_after_id,
                project_storage: ChangeStorageSnapshot {
                    retained_count: project_summary.retained_count,
                    measured_bytes: project_summary
                        .complete
                        .then_some(project_summary.measured_bytes),
                    measured_at_ms: project_summary
                        .complete
                        .then_some(project_summary.measured_at_ms)
                        .flatten(),
                    active_leases: project_summary.active_leases,
                    complete: project_summary.complete,
                },
                factory_storage: ChangeStorageSnapshot {
                    retained_count: factory_summary.retained_count,
                    measured_bytes: factory_summary
                        .complete
                        .then_some(factory_summary.measured_bytes),
                    measured_at_ms: factory_summary
                        .complete
                        .then_some(factory_summary.measured_at_ms)
                        .flatten(),
                    active_leases: factory_summary.active_leases,
                    complete: factory_summary.complete,
                },
                hard_factory_count_cap: execution.max_retained_changes_factory_wide() as u64,
            })
        }
        LocalRequest::RemoveChange {
            project_id,
            change_id,
            expected_revision,
        } => {
            let change = execution
                .remove_change(project_id, change_id, expected_revision)
                .await?;
            Ok(LocalResponse::ChangeRemovalStarted { change })
        }
        LocalRequest::ListLegacySources {
            project_id,
            after_id,
            limit,
        } => {
            let limit = page_limit("legacy source", limit, MAX_LEGACY_SOURCE_PAGE_ITEMS)?;
            let mut sources = state
                .with_store(move |store| {
                    store.list_legacy_sources(&project_id, after_id.as_ref(), limit + 1)
                })
                .await?;
            let next_after_id = next_cursor(&mut sources, limit, |source| source.id.clone());
            Ok(LocalResponse::LegacySources {
                sources,
                next_after_id,
            })
        }
        LocalRequest::ForgetLegacySource {
            project_id,
            legacy_source_id,
        } => {
            let response_project = project_id.clone();
            let response_source = legacy_source_id.clone();
            state
                .commit_and_publish(move |store| {
                    let forgotten_at_ms = now_ms()?;
                    let event = store.forget_legacy_source(
                        &project_id,
                        &legacy_source_id,
                        forgotten_at_ms,
                    )?;
                    Ok(((), vec![event]))
                })
                .await?;
            Ok(LocalResponse::LegacySourceForgotten {
                project_id: response_project,
                legacy_source_id: response_source,
            })
        }
        LocalRequest::EventsAfter { sequence, limit } => {
            if sequence < 0 {
                return Err(ApiFailure::Invalid(
                    "event cursor cannot be negative".into(),
                ));
            }
            let limit = page_limit("event", limit, MAX_EVENT_PAGE_ITEMS)?;
            let events = state
                .with_store(move |store| store.events_after(sequence, limit))
                .await?;
            Ok(LocalResponse::Events { events })
        }
        LocalRequest::LatestEventSequence => {
            let sequence = state
                .with_store(|store| store.latest_event_sequence())
                .await?;
            Ok(LocalResponse::EventHead { sequence })
        }
        LocalRequest::Subscribe { .. } => unreachable!("subscriptions are handled per connection"),
    }
}

/// Absolute guidance-file paths for one agent, computed from the daemon's
/// state root; never touches the filesystem itself.
#[derive(Clone)]
struct AgentGuidancePaths {
    instructions: PathBuf,
    memory: PathBuf,
    project_guidance: PathBuf,
}

impl AgentGuidancePaths {
    fn new(guidance_root: &Path, project_id: &ProjectId, agent_id: &AgentId) -> Self {
        Self {
            instructions: factory_core::paths::agent_instructions_path(
                guidance_root,
                project_id,
                agent_id,
            ),
            memory: factory_core::paths::agent_memory_path(guidance_root, project_id, agent_id),
            project_guidance: factory_core::paths::project_guidance_path(guidance_root, project_id),
        }
    }
}

/// The agent's durable row plus its guidance files' current contents and
/// paths — what `GetAgent` returns and `AgentStatus` embeds.
async fn agent_detail_with_guidance(
    state: &ApiState,
    execution: &execution::Handle,
    guidance_root: &Path,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<LocalAgentDetail, ApiFailure> {
    let lookup_project_id = project_id.clone();
    let lookup_agent_id = agent_id.clone();
    let agent = state
        .with_store(move |store| store.get_agent_detail(&lookup_project_id, &lookup_agent_id))
        .await?;
    // Deletion invariant (ARCHITECTURE.md #9, PR #50 review finding 5):
    // `read_guidance_file` below lazily recreates this agent's guidance
    // files (`guidance::read_or_create`) if they're missing -- gated the
    // same way attempt preparation is (same per-agent lock), so a
    // concurrent `DeleteAgent`'s drain can never miss this read.
    if !execution.try_begin_agent_write(agent_id) {
        return Err(ApiFailure::Conflict("agent is being deleted".into()));
    }
    let agent_paths = AgentGuidancePaths::new(guidance_root, project_id, agent_id);
    let guidance = read_agent_guidance_files(&agent_paths).await;
    execution.end_agent_write(agent_id);
    let (instructions, instructions_health, memory, memory_health) = guidance?;
    Ok(local_agent_detail(
        agent,
        instructions,
        instructions_health,
        memory,
        memory_health,
        agent_paths,
    ))
}

async fn read_agent_guidance_files(
    paths: &AgentGuidancePaths,
) -> Result<
    (
        String,
        factory_core::local::GuidanceHealth,
        String,
        factory_core::local::GuidanceHealth,
    ),
    ApiFailure,
> {
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || {
        let instructions = guidance::inspect(&paths.instructions);
        let memory = guidance::inspect(&paths.memory);
        Ok((
            instructions.content.unwrap_or_default(),
            instructions.health,
            memory.content.unwrap_or_default(),
            memory.health,
        ))
    })
    .await
    .map_err(|error| ApiFailure::Internal(format!("guidance worker failed: {error}")))?
}

fn local_agent_detail(
    agent: crate::store::AgentDetail,
    instructions: String,
    instructions_health: factory_core::local::GuidanceHealth,
    memory: String,
    memory_health: factory_core::local::GuidanceHealth,
    paths: AgentGuidancePaths,
) -> LocalAgentDetail {
    LocalAgentDetail {
        snapshot: agent.snapshot,
        profile: LocalAgentProfile {
            model: agent.profile.model,
            reasoning_effort: agent.profile.reasoning_effort,
            model_selection_reason: agent.profile.model_selection_reason,
            execution_mode: agent.profile.execution_mode,
            instructions,
            memory,
            updated_at_ms: agent.profile.updated_at_ms,
        },
        instructions_path: path_to_string(&paths.instructions),
        instructions_health,
        memory_path: path_to_string(&paths.memory),
        memory_health,
        project_guidance_path: path_to_string(&paths.project_guidance),
    }
}

fn local_project_detail(
    project: ProjectSnapshot,
    guidance: String,
    guidance_path: PathBuf,
) -> LocalProjectDetail {
    LocalProjectDetail {
        snapshot: project,
        guidance,
        guidance_path: path_to_string(&guidance_path),
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Reads a guidance file on the blocking pool, lazily creating it if absent.
async fn read_guidance_file(path: PathBuf) -> Result<String, ApiFailure> {
    tokio::task::spawn_blocking(move || guidance::read_or_create(&path))
        .await
        .map_err(|error| ApiFailure::Internal(format!("guidance worker failed: {error}")))?
        .map_err(ApiFailure::from)
}

/// Atomically overwrites a guidance file on the blocking pool.
async fn write_guidance_file(path: PathBuf, text: String) -> Result<(), ApiFailure> {
    tokio::task::spawn_blocking(move || guidance::write(&path, &text))
        .await
        .map_err(|error| ApiFailure::Internal(format!("guidance worker failed: {error}")))?
        .map_err(ApiFailure::from)
}

async fn write_agent_guidance_files(
    paths: &AgentGuidancePaths,
    instructions: String,
    memory: String,
) -> Result<(), ApiFailure> {
    write_guidance_file(paths.instructions.clone(), instructions).await?;
    write_guidance_file(paths.memory.clone(), memory).await?;
    Ok(())
}

/// Idempotently creates a project's guidance directory and empty
/// `PROJECT.md` on the blocking pool.
async fn ensure_project_guidance(
    guidance_root: &Path,
    project_id: &ProjectId,
) -> Result<(), ApiFailure> {
    let home = guidance_root.to_path_buf();
    let project_id = project_id.clone();
    tokio::task::spawn_blocking(move || guidance::ensure_project(&home, &project_id))
        .await
        .map_err(|error| ApiFailure::Internal(format!("guidance worker failed: {error}")))?
        .map_err(ApiFailure::from)
}

/// Idempotently creates an agent's guidance directory, empty
/// `instructions.md`, and empty `memory.md` on the blocking pool.
/// Runs `CreateAgent`'s database and file work, called only after
/// `execution.try_begin_project_write`/`try_begin_agent_write` have
/// confirmed neither the project nor this exact agent id is currently
/// being deleted (ARCHITECTURE.md invariant 9, PR #50 review finding 3).
async fn create_agent_locked(
    state: &ApiState,
    guidance_root: &Path,
    new_agent: NewAgent,
    model: Option<String>,
    reasoning_effort: Option<String>,
    model_selection_reason: Option<String>,
) -> Result<factory_core::AgentSnapshot, ApiFailure> {
    let created_project_id = new_agent.project_id.clone();
    let created_agent_id = new_agent.id.clone();
    let agent = state
        .commit_and_publish(move |store| {
            let (agent, event) = store.create_agent_with_profile(
                new_agent,
                model,
                reasoning_effort,
                model_selection_reason,
                now_ms()?,
            )?;
            Ok((agent, vec![event]))
        })
        .await?;
    ensure_agent_guidance(guidance_root, &created_project_id, &created_agent_id).await?;
    Ok(agent)
}

async fn ensure_agent_guidance(
    guidance_root: &Path,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<(), ApiFailure> {
    let home = guidance_root.to_path_buf();
    let project_id = project_id.clone();
    let agent_id = agent_id.clone();
    tokio::task::spawn_blocking(move || guidance::ensure_agent(&home, &project_id, &agent_id))
        .await
        .map_err(|error| ApiFailure::Internal(format!("guidance worker failed: {error}")))?
        .map_err(ApiFailure::from)
}

async fn delete_agent_locked(
    state: &ApiState,
    guidance_root: &Path,
    project_id: ProjectId,
    agent_id: AgentId,
) -> Result<(), ApiFailure> {
    let check_project_id = project_id.clone();
    let check_agent_id = agent_id.clone();
    state
        .with_store(move |store| store.check_agent_deletable(&check_project_id, &check_agent_id))
        .await?;
    remove_agent_guidance(guidance_root, &project_id, &agent_id).await?;
    state
        .commit_and_publish(move |store| {
            let events = store.delete_agent(&project_id, &agent_id, now_ms()?)?;
            Ok(((), events))
        })
        .await?;
    Ok(())
}

/// Recursively removes one agent's guidance directory before the database
/// deletion commits. A filesystem failure is reported as the request's own
/// error and leaves the ledger row intact; `execution.begin_delete` has
/// already drained in-flight preparation.
async fn remove_agent_guidance(
    guidance_root: &Path,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<(), ApiFailure> {
    let home = guidance_root.to_path_buf();
    let project_id = project_id.clone();
    let agent_id = agent_id.clone();
    let result =
        tokio::task::spawn_blocking(move || guidance::remove_agent(&home, &project_id, &agent_id))
            .await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(error) => Err(ApiFailure::Internal(format!(
            "guidance worker panicked while removing agent guidance directory: {error}"
        ))),
    }
}

async fn delete_project_locked(
    state: &ApiState,
    guidance_root: &Path,
    project_id: ProjectId,
) -> Result<(), ApiFailure> {
    let reclaim_project_id = project_id.clone();
    let scheduled = state
        .commit_and_publish(move |store| {
            let scheduled =
                store.begin_project_rust_cache_reclamation(&reclaim_project_id, now_ms()?)?;
            Ok((scheduled, Vec::new()))
        })
        .await?;
    if scheduled > 0 {
        return Err(ApiFailure::Conflict(
            "project Rust cache cleanup was scheduled; retry deletion after finalization".into(),
        ));
    }
    let check_project_id = project_id.clone();
    state
        .with_store(move |store| store.check_project_deletable(&check_project_id))
        .await?;
    remove_project_guidance(guidance_root, &project_id).await?;
    state
        .commit_and_publish(move |store| {
            let event = store.delete_project(&project_id, now_ms()?)?;
            Ok(((), vec![event]))
        })
        .await?;
    Ok(())
}

/// Recursively removes one project's guidance directory before the database
/// deletion commits. See [`remove_agent_guidance`] for failure semantics.
async fn remove_project_guidance(
    guidance_root: &Path,
    project_id: &ProjectId,
) -> Result<(), ApiFailure> {
    let home = guidance_root.to_path_buf();
    let project_id = project_id.clone();
    let result =
        tokio::task::spawn_blocking(move || guidance::remove_project(&home, &project_id)).await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.into()),
        Err(error) => Err(ApiFailure::Internal(format!(
            "guidance worker panicked while removing project guidance directory: {error}"
        ))),
    }
}

/// The store's generic state-page cap (`store::MAX_STATE_PAGE`, not
/// exported) is not much larger than the wire's advertised
/// `MAX_AGENT_PAGE_ITEMS`: asking for `MAX_AGENT_PAGE_ITEMS + 1` (the
/// look-ahead-by-one this function used to use, matching `ListAgents`'
/// own paging) lands exactly on that cap today, one bump away from
/// `InvalidStateLimit` turning every `DeleteProject` into an error. Same
/// mismatch `execution::reconcile_all`'s own `RECONCILE_PAGE` already
/// documents and works around with a page size well under the cap (PR #50
/// review, nit 7) -- this does the same rather than relying on the exact
/// coincidence.
const DELETE_PROJECT_AGENT_PAGE: usize = 100;

/// Every agent id currently in `project_id`, paged like `ListAgents` but
/// at [`DELETE_PROJECT_AGENT_PAGE`], not the wire's larger ceiling.
/// `DeleteProject` uses this to mark each of the project's agents deleting
/// (`execution::Handle::begin_delete`) before removing any files.
async fn list_all_agent_ids(
    state: &ApiState,
    project_id: &ProjectId,
) -> Result<Vec<AgentId>, ApiFailure> {
    let page = DELETE_PROJECT_AGENT_PAGE;
    let mut ids = Vec::new();
    let mut after_id = None;
    loop {
        let lookup_after_id = after_id.clone();
        let lookup_project_id = project_id.clone();
        let mut agents = state
            .with_store(move |store| {
                store.list_agents(&lookup_project_id, lookup_after_id.as_ref(), page + 1)
            })
            .await?;
        let next_after_id = next_cursor(&mut agents, page, |agent| agent.id.clone());
        ids.extend(agents.into_iter().map(|agent| agent.id));
        match next_after_id {
            Some(cursor) => after_id = Some(cursor),
            None => return Ok(ids),
        }
    }
}

impl From<GuidanceError> for ApiFailure {
    fn from(error: GuidanceError) -> Self {
        match error {
            GuidanceError::TooLarge { .. } | GuidanceError::InvalidText => {
                Self::Invalid(error.to_string())
            }
            GuidanceError::NotUtf8 { .. }
            | GuidanceError::Directory { .. }
            | GuidanceError::Remove { .. }
            | GuidanceError::File { .. } => Self::Internal(error.to_string()),
        }
    }
}

fn local_agent_message(message: AgentMessage) -> LocalAgentMessage {
    LocalAgentMessage {
        id: message.id,
        project_id: message.project_id,
        sender_agent_id: message.sender_agent_id,
        recipient_agent_id: message.recipient_agent_id,
        body: message.body,
        created_at_ms: message.created_at_ms,
        delivered_at_ms: message.delivered_at_ms,
    }
}

async fn stream_events<W>(mut write: W, state: &ApiState, after_sequence: i64) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut receiver = state.subscribe();
    let replay_through = latest_event_sequence(state)
        .await
        .map_err(api_failure_to_io)?;
    if after_sequence > replay_through {
        return write_response(
            &mut write,
            LocalResponse::Error {
                code: ErrorCode::InvalidRequest,
                message: format!(
                    "event cursor {after_sequence} is ahead of the durable head; durable head is {replay_through}"
                ),
            },
        )
        .await;
    }
    write_response(
        &mut write,
        LocalResponse::Subscribed {
            after_sequence,
            replay_through,
        },
    )
    .await?;
    let mut cursor = replay_events(&mut write, state, after_sequence, replay_through).await?;
    write_response(&mut write, LocalResponse::CaughtUp { sequence: cursor }).await?;

    loop {
        match receiver.recv().await {
            Ok(event) if event.sequence <= cursor => {}
            Ok(event) if event.sequence == cursor + 1 => {
                cursor = event.sequence;
                write_frame(
                    &mut write,
                    &ServerFrame::Event {
                        protocol_version: PROTOCOL_VERSION,
                        event,
                    },
                )
                .await?;
            }
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                let replay_through = latest_event_sequence(state)
                    .await
                    .map_err(api_failure_to_io)?;
                cursor = replay_events(&mut write, state, cursor, replay_through).await?;
                write_response(&mut write, LocalResponse::CaughtUp { sequence: cursor }).await?;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

async fn replay_events<W>(
    write: &mut W,
    state: &ApiState,
    mut cursor: i64,
    replay_through: i64,
) -> io::Result<i64>
where
    W: AsyncWrite + Unpin,
{
    while cursor < replay_through {
        let remaining = usize::try_from(replay_through - cursor)
            .unwrap_or(EVENT_REPLAY_PAGE)
            .min(EVENT_REPLAY_PAGE);
        let events = state
            .with_store(move |store| store.events_after(cursor, remaining))
            .await
            .map_err(ApiFailure::from)
            .map_err(api_failure_to_io)?;
        if events.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("event log ended at {cursor} before replay boundary {replay_through}"),
            ));
        }
        for event in events {
            if event.sequence <= cursor {
                continue;
            }
            if event.sequence > replay_through {
                break;
            }
            if event.sequence != cursor + 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "event log has a gap after {cursor}; next sequence is {}",
                        event.sequence
                    ),
                ));
            }
            cursor = event.sequence;
            write_frame(
                write,
                &ServerFrame::Event {
                    protocol_version: PROTOCOL_VERSION,
                    event,
                },
            )
            .await?;
        }
    }
    Ok(cursor)
}

async fn latest_event_sequence(state: &ApiState) -> Result<i64, ApiFailure> {
    state
        .with_store(|store| store.latest_event_sequence())
        .await
        .map_err(ApiFailure::from)
}

async fn write_response<W>(write: &mut W, response: LocalResponse) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_frame(
        write,
        &ServerFrame::Response {
            protocol_version: PROTOCOL_VERSION,
            response,
        },
    )
    .await
}

async fn write_frame<W>(write: &mut W, frame: &ServerFrame) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut json = serde_json::to_vec(frame).map_err(io::Error::other)?;
    if json.len() > MAX_LOCAL_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "local protocol frame is {} bytes; maximum is {MAX_LOCAL_FRAME_BYTES}",
                json.len()
            ),
        ));
    }
    json.push(b'\n');
    timeout(IO_TIMEOUT, async {
        write.write_all(&json).await?;
        write.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "local response timed out"))?
}

fn required_text(label: &str, value: String, max_len: usize) -> Result<String, ApiFailure> {
    let value = value.trim().to_owned();
    if value.is_empty() || value.len() > max_len {
        return Err(ApiFailure::Invalid(format!(
            "{label} must be between 1 and {max_len} bytes"
        )));
    }
    Ok(value)
}

async fn canonical_root(root: String) -> Result<String, ApiFailure> {
    tokio::task::spawn_blocking(move || {
        let root = PathBuf::from(root);
        let canonical = root.canonicalize().map_err(|_| {
            ApiFailure::Invalid("project root must be an existing readable directory".into())
        })?;
        if !canonical.is_dir() {
            return Err(ApiFailure::Invalid(
                "project root must be an existing readable directory".into(),
            ));
        }
        canonical
            .into_os_string()
            .into_string()
            .map_err(|_| ApiFailure::Invalid("project root must be valid UTF-8".into()))
    })
    .await
    .map_err(|error| ApiFailure::Internal(format!("project root worker failed: {error}")))?
}

fn page_limit(label: &str, limit: u32, maximum: u32) -> Result<usize, ApiFailure> {
    if !(1..=maximum).contains(&limit) {
        return Err(ApiFailure::Invalid(format!(
            "{label} page limit must be between 1 and {maximum}"
        )));
    }
    Ok(limit as usize)
}

/// If `items` overflowed `limit` (the store always fetches one extra row to
/// detect this), drop that extra row and return the cursor to resume from.
fn next_cursor<T, Id>(items: &mut Vec<T>, limit: usize, id: impl FnOnce(&T) -> Id) -> Option<Id> {
    if items.len() <= limit {
        return None;
    }
    items.pop();
    items.last().map(id)
}

fn now_ms() -> Result<i64, StoreError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        })?;
    i64::try_from(elapsed.as_millis()).map_err(|error| {
        StoreError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
    })
}

fn is_constraint_error(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
            if failure.code == rusqlite::ffi::ErrorCode::ConstraintViolation
    )
}

fn api_failure_to_io(error: ApiFailure) -> io::Error {
    io::Error::other(match error {
        ApiFailure::Invalid(message)
        | ApiFailure::Unauthorized(message)
        | ApiFailure::Conflict(message)
        | ApiFailure::Internal(message) => message,
        ApiFailure::Store(error) => error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory_core::ExecutionMode;

    fn attempt() -> Principal {
        Principal::Attempt(AttemptPrincipal {
            run_id: RunId::try_from("run").unwrap(),
            project_id: ProjectId::try_from("project").unwrap(),
            agent_id: AgentId::try_from("agent").unwrap(),
            role: AgentRole::Orchestrator,
            source_root: "/tmp/source".into(),
        })
    }

    #[test]
    fn only_the_operator_may_change_dispatch_or_execution_authority() {
        let dispatch = LocalRequest::SetDispatchEnabled { enabled: false };
        let execution_mode = LocalRequest::UpdateAgentProfile {
            project_id: ProjectId::try_from("project").unwrap(),
            agent_id: AgentId::try_from("agent").unwrap(),
            model: None,
            reasoning_effort: None,
            model_selection_reason: None,
            execution_mode: ExecutionMode::Unrestricted,
            instructions: String::new(),
            memory: String::new(),
        };

        for request in [&dispatch, &execution_mode] {
            assert!(authorize(&Principal::Operator, request).is_ok());
            assert!(matches!(
                authorize(&attempt(), request),
                Err(ApiFailure::Unauthorized(_))
            ));
        }
    }
}
