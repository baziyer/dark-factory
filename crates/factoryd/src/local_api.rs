//! Versioned local control and persisted event stream.

use std::{
    fs,
    future::Future,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use factory_core::{
    AgentId, FactoryEvent, PROTOCOL_VERSION, ProjectId, ProjectSnapshot, ProviderHookEvent, RunId,
    SessionId,
    local::{
        AgentDetail as LocalAgentDetail, AgentMessage as LocalAgentMessage,
        AgentProfile as LocalAgentProfile, ErrorCode, LocalRequest, LocalResponse,
        MAX_AGENT_MESSAGE_BYTES, MAX_AGENT_PAGE_ITEMS, MAX_EVENT_PAGE_ITEMS, MAX_LOCAL_FRAME_BYTES,
        MAX_PROJECT_PAGE_ITEMS, MAX_RUN_PAGE_ITEMS, MAX_SESSION_PAGE_ITEMS, MAX_TASK_BODY_BYTES,
        MAX_TASK_PAGE_ITEMS, MAX_TASK_TITLE_BYTES, MAX_TERMINAL_OUTPUT_BYTES,
        ManagedChange as LocalManagedChange, ProjectDetail as LocalProjectDetail, RequestEnvelope,
        RunTerminal, ServerFrame, normalize_task_title,
    },
    runner::{
        MAX_RUNNER_FRAME_BYTES, MAX_RUNNER_SPOOL_BYTES, OutputStream, RunnerErrorCode, RunnerEvent,
        RunnerEventEnvelope,
    },
    status,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, Take},
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{Semaphore, broadcast, mpsc, watch},
    task::JoinSet,
    time::timeout,
};

pub use crate::daemon_state::DaemonState as ApiState;

use crate::{
    daemon_state::DaemonStateError,
    execution::{self, StartTask},
    guidance::{self, GuidanceError},
    repository,
    runner_client::{RunnerClient, RunnerClientError},
    store::{
        AgentMessage, NewAgent, NewAgentMessage, NewProject, NewRepositoryOperation, NewTask,
        SessionControlTarget, StoreError, UpdateAgentProfile,
    },
};

const MAX_CONCURRENT_WORKTREE_PROBES: usize = 8;
const FLEET_WORKTREE_DEADLINE: Duration = Duration::from_secs(2);

enum RepositoryRequest {
    Status,
    Diff {
        staged: bool,
    },
    Commit {
        message: String,
    },
    Push,
    PrOpen {
        title: String,
        body: String,
    },
    PrUpdate {
        number: u64,
        title: String,
        body: String,
    },
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

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
}

struct RepositoryAudit {
    project_id: ProjectId,
    agent_id: factory_core::AgentId,
    session_id: SessionId,
    operation: String,
    phase: String,
    success: Option<bool>,
    reference: Option<String>,
}

impl RepositoryRequest {
    fn name(&self) -> &'static str {
        match self {
            Self::Status => "git_status",
            Self::Diff { .. } => "git_diff",
            Self::Commit { .. } => "git_commit",
            Self::Push => "git_push",
            Self::PrOpen { .. } => "pr_open",
            Self::PrUpdate { .. } => "pr_update",
        }
    }
}

const EVENT_REPLAY_PAGE: usize = MAX_EVENT_PAGE_ITEMS as usize;
const MAX_CONNECTIONS: usize = 128;
const IO_TIMEOUT: Duration = Duration::from_secs(15);
const TERMINAL_FRAME_CHANNEL_CAPACITY: usize = 64;
/// Mirrors the `tasks.result` CHECK bound (migration 0006).
const MAX_TASK_RESULT_BYTES: usize = 131_072;
/// Mirrors the `tasks.blocked_reason` CHECK bound (migration 0014).
const MAX_BLOCKED_REASON_BYTES: usize = 4096;
/// Mirrors the `sessions.activity`/`wait_reason` CHECK bound (migration 0014).
const MAX_HOOK_FIELD_BYTES: usize = 512;

type LimitedReader = Take<BufReader<OwnedReadHalf>>;

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
            Self::Store(StoreError::UnsupportedAgentPermissionMode { provider, mode }) => (
                ErrorCode::InvalidRequest,
                format!("permission mode {mode:?} is not supported by provider {provider:?}"),
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
            Self::Store(StoreError::InvalidHookToken) => (
                ErrorCode::InvalidRequest,
                "hook token is not recognized".into(),
            ),
            Self::Store(StoreError::AgentNotFound) => (
                ErrorCode::NotFound,
                "agent was not found in the project".into(),
            ),
            Self::Store(StoreError::ProjectNotFound) => {
                (ErrorCode::NotFound, "project was not found".into())
            }
            Self::Store(StoreError::RepositoryAuthorityMissing) => (
                ErrorCode::Conflict,
                "project repository authority is not configured; set it as the operator first"
                    .into(),
            ),
            Self::Store(StoreError::TaskNotFound) => (
                ErrorCode::NotFound,
                "task was not found in the project".into(),
            ),
            Self::Store(StoreError::StaleTaskCursor) => (
                ErrorCode::Conflict,
                "task page cursor is stale; restart the listing".into(),
            ),
            Self::Store(StoreError::MissingTaskCursorRevision)
            | Self::Store(StoreError::UnexpectedTaskCursorRevision) => (
                ErrorCode::InvalidRequest,
                "task cursor and queue revision must be supplied together".into(),
            ),
            Self::Store(StoreError::RunNotFound) => (
                ErrorCode::NotFound,
                "run was not found in the project".into(),
            ),
            Self::Store(StoreError::SessionNotFound) => (
                ErrorCode::NotFound,
                "session was not found in the project".into(),
            ),
            Self::Store(StoreError::TaskNotQueued) => (
                ErrorCode::Conflict,
                "task is not queued in the project".into(),
            ),
            Self::Store(StoreError::TaskNotRunning) => (
                ErrorCode::Conflict,
                "task is not running in the project".into(),
            ),
            Self::Store(StoreError::TaskAssignmentMismatch) => (
                ErrorCode::Conflict,
                "task is not assigned to the requesting agent".into(),
            ),
            Self::Store(StoreError::TaskNotRetryable) => (
                ErrorCode::Conflict,
                "task is not retryable in the project".into(),
            ),
            Self::Store(StoreError::AgentProviderMismatch) => (
                ErrorCode::Conflict,
                "agent provider does not match the requested execution".into(),
            ),
            Self::Store(StoreError::AgentUnavailable) => (
                ErrorCode::Conflict,
                "agent already has an open run or live session".into(),
            ),
            Self::Store(StoreError::SessionAlreadyLive) => (
                ErrorCode::Conflict,
                "agent already has a live session".into(),
            ),
            Self::Store(StoreError::SessionNotLive) => {
                (ErrorCode::Conflict, "session is not live".into())
            }
            Self::Store(
                error @ (StoreError::TaskNotCancellable
                | StoreError::TaskNotEditable
                | StoreError::TaskHasActiveRun
                | StoreError::TaskHasSubtasks
                | StoreError::TaskRunHasDependents
                | StoreError::AgentHasActiveRun
                | StoreError::AgentHasLiveSession
                | StoreError::AgentHasChildren
                | StoreError::AgentRunHasDependents
                | StoreError::AgentHasActiveChange
                | StoreError::AgentBudgetExhausted
                | StoreError::ProjectHasActiveRun
                | StoreError::ProjectHasActiveChange
                | StoreError::TaskHasActiveChange
                | StoreError::RunNotStoppable
                | StoreError::SessionStopping),
            ) => (ErrorCode::Conflict, error.to_string()),
            Self::Store(
                error @ (StoreError::ManagedChangeWrongTask
                | StoreError::ManagedChangeNeedsCurrentTask
                | StoreError::ManagedChangeNotFound
                | StoreError::ManagedChangeCollision),
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
            execution::Error::NoLiveSession => Self::Conflict(
                "agent has no live session yet; it will be spawned once it has queued work, or \
                 try again shortly"
                    .into(),
            ),
            execution::Error::SessionBusy => Self::Conflict(
                "agent's live session is not idle; the task stays queued and will \
                                 be delivered once it is"
                    .into(),
            ),
            execution::Error::NoWorktree => {
                Self::Invalid("agent has no worktree; create one first".into())
            }
            execution::Error::DeleteDrainTimeout => Self::Conflict(
                "an in-flight session spawn did not finish before the delete's drain timeout; \
                 try the delete again"
                    .into(),
            ),
            execution::Error::State(DaemonStateError::Store(
                error @ (StoreError::AgentNotFound
                | StoreError::TaskNotQueued
                | StoreError::TaskAssignmentMismatch
                | StoreError::AgentUnavailable
                | StoreError::SessionNotFound
                | StoreError::SessionNotLive
                | StoreError::SessionStopping),
            )) => Self::Store(error),
            _ => Self::Internal("execution manager could not accept the task".into()),
        }
    }
}

pub async fn serve<F>(
    listener: UnixListener,
    state: ApiState,
    execution: execution::Handle,
    guidance_root: PathBuf,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
    let guidance_root = Arc::new(guidance_root);
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
                let shutdown = stop_rx.clone();
                handlers.spawn(async move {
                    let _permit = permit;
                    if let Err(error) =
                        handle_connection(stream, state, execution, guidance_root, shutdown).await
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

    let request = match parse_envelope(&payload) {
        Ok(request) => request,
        Err(response) => return write_response(&mut write, *response).await,
    };

    if let LocalRequest::Subscribe { after_sequence } = request {
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

    if let LocalRequest::AttachTerminal {
        project_id,
        session_id,
        since_offset,
    } = request
    {
        let session_shutdown = shutdown.clone();
        return tokio::select! {
            biased;
            _ = shutdown.changed() => Ok(()),
            result = terminal_attach_session(
                limited,
                write,
                state.clone(),
                session_shutdown,
                project_id,
                session_id,
                since_offset,
            ) => result,
        };
    }

    let response = handle_request(&state, &execution, &guidance_root, request)
        .await
        .unwrap_or_else(ApiFailure::into_response);
    write_response(&mut write, response).await
}

fn parse_envelope(payload: &[u8]) -> Result<LocalRequest, Box<LocalResponse>> {
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
    Ok(envelope.request)
}

/// Persistent multiplexed connection for one or more `AttachTerminal`
/// sessions: `ServerFrame::TerminalOutput` for every attached session
/// (tagged by `session_id`) is interleaved onto the same connection. Only
/// further `AttachTerminal` requests are accepted on it; anything else gets
/// an error response. Detaching happens implicitly on client disconnect,
/// which drops this function's `JoinSet` and aborts every attached
/// forwarding task.
#[allow(clippy::too_many_arguments)]
async fn terminal_attach_session(
    mut reader: LimitedReader,
    mut write: OwnedWriteHalf,
    state: ApiState,
    mut shutdown: watch::Receiver<bool>,
    project_id: ProjectId,
    session_id: SessionId,
    since_offset: u64,
) -> io::Result<()> {
    let (frame_tx, mut frame_rx) = mpsc::channel::<ServerFrame>(TERMINAL_FRAME_CHANNEL_CAPACITY);
    let mut attaches = JoinSet::new();
    spawn_terminal_attach(
        &mut attaches,
        state.clone(),
        frame_tx.clone(),
        project_id,
        session_id,
        since_offset,
    );

    let mut payload = Vec::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            frame = frame_rx.recv() => {
                let Some(frame) = frame else { return Ok(()); };
                write_frame(&mut write, &frame).await?;
            }
            Some(_) = attaches.join_next(), if !attaches.is_empty() => {}
            result = read_next_line(&mut reader, &mut payload) => {
                let Some(line) = result? else { return Ok(()); };
                match parse_envelope(&line) {
                    Ok(LocalRequest::AttachTerminal { project_id, session_id, since_offset }) => {
                        spawn_terminal_attach(
                            &mut attaches,
                            state.clone(),
                            frame_tx.clone(),
                            project_id,
                            session_id,
                            since_offset,
                        );
                    }
                    Ok(_) => {
                        write_response(
                            &mut write,
                            LocalResponse::Error {
                                code: ErrorCode::InvalidRequest,
                                message: "only AttachTerminal is accepted on an attached \
                                          terminal connection"
                                    .into(),
                            },
                        )
                        .await?;
                    }
                    Err(response) => write_response(&mut write, *response).await?,
                }
            }
        }
    }
}

fn spawn_terminal_attach(
    attaches: &mut JoinSet<()>,
    state: ApiState,
    frame_tx: mpsc::Sender<ServerFrame>,
    project_id: ProjectId,
    session_id: SessionId,
    since_offset: u64,
) {
    attaches.spawn(async move {
        let lookup_project_id = project_id.clone();
        let lookup_session_id = session_id.clone();
        let target = match state
            .with_store(move |store| {
                store.session_control_target(&lookup_project_id, &lookup_session_id)
            })
            .await
        {
            Ok(target) => target,
            Err(error) => {
                let _ =
                    send_terminal_error(&frame_tx, ApiFailure::from(error).into_response()).await;
                return;
            }
        };
        let control_run_id = match session_control_run_id(&session_id) {
            Ok(run_id) => run_id,
            Err(failure) => {
                let _ = send_terminal_error(&frame_tx, failure.into_response()).await;
                return;
            }
        };
        let client = RunnerClient::new(
            &target.runner_runtime,
            control_run_id,
            target.runner_instance_id,
        );
        let mut subscription = match client.attach_terminal(since_offset).await {
            Ok(subscription) => subscription,
            Err(error) => {
                let response = runner_control_failure(error, "attach terminal").into_response();
                let _ = send_terminal_error(&frame_tx, response).await;
                return;
            }
        };
        if frame_tx
            .send(ServerFrame::TerminalOutput {
                protocol_version: PROTOCOL_VERSION,
                session_id: session_id.clone(),
                offset: since_offset,
                bytes: String::new(),
            })
            .await
            .is_err()
        {
            return;
        }
        loop {
            match subscription.next_chunk().await {
                Ok((offset, bytes)) => {
                    let frame = ServerFrame::TerminalOutput {
                        protocol_version: PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        offset,
                        bytes,
                    };
                    if frame_tx.send(frame).await.is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let response = runner_control_failure(error, "attach terminal").into_response();
                    let _ = send_terminal_error(&frame_tx, response).await;
                    return;
                }
            }
        }
    });
}

async fn send_terminal_error(
    frame_tx: &mpsc::Sender<ServerFrame>,
    response: LocalResponse,
) -> Result<(), mpsc::error::SendError<ServerFrame>> {
    frame_tx
        .send(ServerFrame::Response {
            protocol_version: PROTOCOL_VERSION,
            response,
        })
        .await
}

/// Reads one more newline-delimited frame from an already-open connection,
/// resetting the per-frame size limit each time so a long-lived connection
/// is not bounded by the *cumulative* bytes it has ever read.
async fn read_next_line(
    reader: &mut LimitedReader,
    payload: &mut Vec<u8>,
) -> io::Result<Option<Vec<u8>>> {
    payload.clear();
    reader.set_limit((MAX_LOCAL_FRAME_BYTES + 2) as u64);
    let read = reader.read_until(b'\n', payload).await?;
    if read == 0 {
        return Ok(None);
    }
    if payload.last() != Some(&b'\n') || payload.len() - 1 > MAX_LOCAL_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request must be one newline-terminated JSON frame of at most 1 MiB",
        ));
    }
    payload.pop();
    Ok(Some(std::mem::take(payload)))
}

async fn handle_request(
    state: &ApiState,
    execution: &execution::Handle,
    guidance_root: &Path,
    request: LocalRequest,
) -> Result<LocalResponse, ApiFailure> {
    match request {
        LocalRequest::Health => Ok(LocalResponse::Health {
            runner_path: execution.runner_program().to_string_lossy().into_owned(),
            factoryctl_path: execution.factoryctl_path().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            process_id: std::process::id(),
        }),
        LocalRequest::SetAutoMode { enabled } => {
            state
                .commit_and_publish(move |store| {
                    let event = store.set_auto_mode(enabled, now_ms()?)?;
                    Ok(((), vec![event]))
                })
                .await?;
            Ok(LocalResponse::AutoModeSet { enabled })
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
            let live_session_cap = u32::try_from(execution.max_active_runs()).unwrap_or(u32::MAX);
            let (projects, live_sessions, generated_at_ms, auto_mode) = state
                .with_store(move |store| {
                    Ok((
                        store.fleet_status()?,
                        store.live_session_count()?,
                        now_ms()?,
                        store.auto_mode()?,
                    ))
                })
                .await?;
            let live_sessions = u32::try_from(live_sessions).unwrap_or(u32::MAX);
            let at_capacity = live_sessions >= live_session_cap;
            let mut attention = Vec::new();
            let mut projects: Vec<status::ProjectStatus> = projects
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
            populate_fleet_worktrees(&mut projects).await;
            status::sort_attention(&mut attention);
            Ok(LocalResponse::FleetStatus {
                status: status::FleetStatus {
                    generated_at_ms,
                    auto_mode,
                    live_session_cap,
                    live_sessions,
                    projects,
                    attention,
                },
            })
        }
        LocalRequest::GitStatus { token } => {
            repository_request(state, token, RepositoryRequest::Status).await
        }
        LocalRequest::GitDiff { token, staged } => {
            repository_request(state, token, RepositoryRequest::Diff { staged }).await
        }
        LocalRequest::GitCommit { token, message } => {
            repository_request(state, token, RepositoryRequest::Commit { message }).await
        }
        LocalRequest::GitPush { token } => {
            repository_request(state, token, RepositoryRequest::Push).await
        }
        LocalRequest::CreateManagedChange { token } => {
            create_managed_change_request(state, guidance_root, token).await
        }
        LocalRequest::AbandonManagedChange { token } => {
            abandon_managed_change_request(state, token).await
        }
        LocalRequest::PrOpen { token, title, body } => {
            repository_request(state, token, RepositoryRequest::PrOpen { title, body }).await
        }
        LocalRequest::PrUpdate {
            token,
            number,
            title,
            body,
        } => {
            repository_request(
                state,
                token,
                RepositoryRequest::PrUpdate {
                    number,
                    title,
                    body,
                },
            )
            .await
        }
        LocalRequest::AgentStatus {
            project_id,
            agent_id,
        } => {
            let lookup_project_id = project_id.clone();
            let lookup_agent_id = agent_id.clone();
            let mut agent_status = state
                .with_store(move |store| store.agent_status(&lookup_project_id, &lookup_agent_id))
                .await?;
            let detail =
                agent_detail_with_guidance(state, execution, guidance_root, &project_id, &agent_id)
                    .await?;
            let worktree = match agent_status.agent.worktree.as_deref() {
                Some(path) => Some(crate::worktrees::status(Path::new(path)).await),
                None => None,
            };
            agent_status.worktree = worktree.clone();
            Ok(LocalResponse::AgentStatus {
                status: Box::new(status::AgentStatusDetail {
                    status: agent_status,
                    detail,
                    worktree,
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
            let task = state
                .commit_and_publish(move |store| {
                    let (task, event) = store.create_task_with_assignment(
                        NewTask {
                            id,
                            project_id,
                            parent_task_id,
                            title,
                            body,
                            priority,
                        },
                        agent_id,
                        now_ms()?,
                    )?;
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
            worktree,
        } => {
            let worktree = match worktree {
                Some(worktree) => Some(validate_agent_worktree(worktree).await?),
                None => None,
            };
            let created_project_id = project_id.clone();
            let created_agent_id = id.clone();
            let created_parent_agent_id = parent_agent_id.clone();
            // Deletion invariant (ARCHITECTURE.md #9, PR #50 review finding
            // 3): declines outright -- rather than silently skipping like
            // the dispatcher does -- if this project, this exact agent id,
            // or its intended parent is currently being deleted. Checked
            // and recorded in flight atomically with `DeleteProject`/
            // `DeleteAgent`'s own mark under the same lock, so a delete
            // already draining can never miss this create's writes
            // (`provision_agent_worktree`, `ensure_agent_guidance`,
            // below): the new agent's worktree and guidance directory are
            // exactly what a `DeleteProject` running concurrently would
            // otherwise `rm -rf` right out from under this request. The
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
            if let Some(parent) = &created_parent_agent_id {
                if !execution.try_begin_agent_write(parent) {
                    execution.end_agent_write(&created_agent_id);
                    execution.end_project_write(&created_project_id);
                    return Err(ApiFailure::Conflict(
                        "the parent agent is currently being deleted; wait for the delete to \
                         finish"
                            .into(),
                    ));
                }
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
                worktree,
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
            permission_mode,
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
                            permission_mode,
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
            Ok(LocalResponse::AgentProfileUpdated {
                agent: local_agent_detail(agent, instructions, memory, agent_paths),
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
        LocalRequest::SetProjectRepositoryAuthority {
            project_id,
            remote_url,
            base_branch,
        } => {
            let authority = repository::validate_authority(remote_url, base_branch)
                .map_err(repository_failure)?;
            let response_project_id = project_id.clone();
            state
                .commit_and_publish(move |store| {
                    let event =
                        store.set_repository_authority(&project_id, &authority, now_ms()?)?;
                    Ok(((), vec![event]))
                })
                .await?;
            Ok(LocalResponse::ProjectRepositoryAuthoritySet {
                project_id: response_project_id,
            })
        }
        LocalRequest::SendAgentMessage {
            id,
            project_id,
            sender_agent_id,
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
            let message = state
                .commit_and_publish(move |store| {
                    let message = store.send_agent_message(NewAgentMessage {
                        id,
                        project_id,
                        sender_agent_id,
                        recipient_agent_id,
                        body,
                        created_at_ms: now_ms()?,
                    })?;
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
        LocalRequest::StartTask {
            project_id,
            task_id,
            agent_id,
            parent_run_id,
            worktree,
        } => {
            let worktree = match worktree {
                Some(worktree) => worktree,
                None => {
                    let lookup_project_id = project_id.clone();
                    let lookup_agent_id = agent_id.clone();
                    let agent = state
                        .with_store(move |store| {
                            store.get_agent_detail(&lookup_project_id, &lookup_agent_id)
                        })
                        .await?;
                    agent.snapshot.worktree.ok_or_else(|| {
                        ApiFailure::Invalid(
                            "agent has no worktree; pass one explicitly or set one first".into(),
                        )
                    })?
                }
            };
            let started = execution
                .start_task(StartTask {
                    project_id,
                    task_id,
                    agent_id,
                    parent_run_id,
                    worktree: PathBuf::from(worktree),
                })
                .await?;
            Ok(LocalResponse::RunAccepted {
                run_id: started.run_id,
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
            let _repository_slot = state.repository_slot().await;
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
            if let Some(body) = body.as_ref() {
                if body.len() > MAX_TASK_BODY_BYTES {
                    return Err(ApiFailure::Invalid(format!(
                        "task body must be at most {MAX_TASK_BODY_BYTES} bytes"
                    )));
                }
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
            let _repository_slot = state.repository_slot().await;
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
            let _repository_slot = state.repository_slot().await;
            let response_project_id = project_id.clone();
            let response_agent_id = agent_id.clone();
            // Deletion invariant (ARCHITECTURE.md #9): from this call on,
            // no gated writer -- the dispatcher's spawn preparation, an
            // idle session's delivery, or a handler using
            // `try_begin_agent_write` (`GetAgent`/`AgentStatus`,
            // `UpdateAgentProfile`) -- can begin a new write for this
            // agent, and this call has waited out any write already in
            // flight, so nothing can still be writing into its guidance
            // directory below.
            execution.begin_delete(&agent_id).await?;
            // Assignment changes and delivery share the owner-side barrier.
            // Hold it while the delete transaction unassigns every task so
            // no in-flight prompt can escape to an agent that is being
            // removed.
            let _delivery_slot = execution.lock_delivery_slot(&agent_id).await;
            let result = delete_agent_locked(state, guidance_root, project_id, agent_id).await;
            execution.end_delete(&response_agent_id);
            result?;
            Ok(LocalResponse::AgentDeleted {
                project_id: response_project_id,
                agent_id: response_agent_id,
            })
        }
        LocalRequest::DeleteProject { project_id } => {
            let _repository_slot = state.repository_slot().await;
            let response_project_id = project_id.clone();
            // Deletion invariant (ARCHITECTURE.md #9): mark the project
            // first, so no `CreateAgent` can start writing a new agent's
            // worktree/guidance tree under it (PR #50 review finding 3 --
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
            let delivery_slots = if begin_error.is_none() {
                let mut slots = Vec::with_capacity(begun.len());
                for agent_id in &begun {
                    slots.push(execution.lock_delivery_slot(agent_id).await);
                }
                slots
            } else {
                Vec::new()
            };
            let result = match begin_error {
                Some(error) => Err(ApiFailure::from(error)),
                None => delete_project_locked(state, guidance_root, project_id).await,
            };
            drop(delivery_slots);
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
            let _assignment_slot = execution.lock_assignment_slot().await;
            let lookup_project_id = project_id.clone();
            let lookup_task_id = task_id.clone();
            let previous_owner = state
                .with_store(move |store| {
                    Ok(store
                        .get_task(&lookup_project_id, &lookup_task_id)?
                        .snapshot
                        .assigned_agent_id)
                })
                .await?;
            // The task-scoped lock orders competing moves. This owner-side
            // barrier orders the move against delivery itself: when the
            // move commits, the old worker's delivery slot is definitely
            // free, and a later delivery observes the new assignment.
            let _delivery_slot = previous_owner
                .as_ref()
                .map(|owner| execution.lock_delivery_slot(owner));
            let _delivery_slot = match _delivery_slot {
                Some(future) => Some(future.await),
                None => None,
            };
            let wake_project_id = project_id.clone();
            let task = state
                .commit_and_publish(move |store| {
                    let (task, event) =
                        store.assign_task(&project_id, &task_id, agent_id.as_ref(), now_ms()?)?;
                    Ok((task, vec![event]))
                })
                .await?;
            if let Some(agent_id) = task.snapshot.assigned_agent_id.clone() {
                execution.wake(wake_project_id, agent_id);
            }
            Ok(LocalResponse::TaskAssigned { task })
        }
        LocalRequest::GetRunTerminal { project_id, run_id } => {
            let lookup_run_id = run_id.clone();
            let target = state
                .with_store(move |store| store.run_control_target(&project_id, &lookup_run_id))
                .await?;
            let terminal = tokio::task::spawn_blocking(move || read_run_terminal(&target, run_id))
                .await
                .map_err(|_| ApiFailure::Internal("terminal reader stopped".into()))??;
            Ok(LocalResponse::RunTerminal { terminal })
        }
        LocalRequest::StopRun {
            project_id,
            run_id,
            grace_ms,
        } => {
            if grace_ms > 60_000 {
                return Err(ApiFailure::Invalid(
                    "runner stop grace must be at most 60000 ms".into(),
                ));
            }
            let lookup_project_id = project_id.clone();
            let lookup_run_id = run_id.clone();
            let session = state
                .with_store(move |store| {
                    store.run_session_snapshot(&lookup_project_id, &lookup_run_id)
                })
                .await?;
            let _delivery_admission = execution.lock_delivery_admission(&session.agent_id).await;
            let stop_project_id = project_id.clone();
            let stop_run_id = run_id.clone();
            let _run = state
                .commit_and_publish(move |store| {
                    let (run, event) =
                        store.request_run_stop(&stop_project_id, &stop_run_id, now_ms()?)?;
                    Ok((run, vec![event]))
                })
                .await?;
            let session_project_id = project_id.clone();
            let session_id = session.id.clone();
            state
                .commit_and_publish(move |store| {
                    let (session, event) =
                        store.request_session_stop(&session_project_id, &session_id, now_ms()?)?;
                    Ok((session, vec![event]))
                })
                .await?;
            let target_project_id = project_id.clone();
            let target_run_id = run_id.clone();
            let target = state
                .with_store(move |store| {
                    store.run_control_target(&target_project_id, &target_run_id)
                })
                .await?;
            let control_run_id = run_id.clone();
            RunnerClient::new(
                &target.runner_runtime,
                control_run_id,
                target.runner_instance_id,
            )
            .stop(grace_ms)
            .await
            .map_err(|error| runner_control_failure(error, "stop"))?;
            Ok(LocalResponse::RunStopped { run_id })
        }
        LocalRequest::CancelRun { project_id, run_id } => {
            let _repository_slot = state.repository_slot().await;
            let response_run_id = run_id.clone();
            state
                .commit_and_publish(move |store| {
                    let closed = store.cancel_run(&project_id, &run_id, now_ms()?)?;
                    Ok(((), closed.events))
                })
                .await?;
            Ok(LocalResponse::RunCancelled {
                run_id: response_run_id,
            })
        }
        LocalRequest::CompleteTask {
            project_id,
            task_id,
            result,
        } => {
            let _repository_slot = state.repository_slot().await;
            if result.len() > MAX_TASK_RESULT_BYTES {
                return Err(ApiFailure::Invalid(format!(
                    "task result must be at most {MAX_TASK_RESULT_BYTES} bytes"
                )));
            }
            let task = state
                .commit_and_publish(move |store| {
                    let closed = store.complete_task(&project_id, &task_id, result, now_ms()?)?;
                    Ok((closed.task, closed.events))
                })
                .await?;
            Ok(LocalResponse::TaskCompleted { task })
        }
        LocalRequest::BlockTask {
            project_id,
            task_id,
            reason,
        } => {
            let _repository_slot = state.repository_slot().await;
            if reason.is_empty() || reason.len() > MAX_BLOCKED_REASON_BYTES {
                return Err(ApiFailure::Invalid(format!(
                    "block reason must be between 1 and {MAX_BLOCKED_REASON_BYTES} bytes"
                )));
            }
            let task = state
                .commit_and_publish(move |store| {
                    let closed = store.block_task(&project_id, &task_id, reason, now_ms()?)?;
                    Ok((closed.task, closed.events))
                })
                .await?;
            Ok(LocalResponse::TaskBlocked { task })
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
            // Issue #24 finding 4: a resume is itself the operator's retry
            // decision for an agent the dispatcher may have paused after
            // repeated session-start-deadline failures, so it gets a clean
            // backoff/streak slate rather than being immediately eligible
            // to re-trip the same pause on its very next deadline.
            execution.resume_backoff(&wake_agent_id);
            execution
                .reset_delivery_attempt(&wake_project_id, &wake_agent_id)
                .await?;
            execution.wake(wake_project_id, wake_agent_id);
            Ok(LocalResponse::AgentResumed { agent })
        }
        LocalRequest::ListSessions {
            project_id,
            after_id,
            limit,
        } => {
            let limit = session_page_limit(limit)?;
            let mut sessions = state
                .with_store(move |store| {
                    store.list_sessions(&project_id, after_id.as_ref(), limit + 1)
                })
                .await?;
            let next_after_id = next_cursor(&mut sessions, limit, |session| session.id.clone());
            Ok(LocalResponse::Sessions {
                sessions,
                next_after_id,
            })
        }
        LocalRequest::StopSession {
            project_id,
            session_id,
            grace_ms,
        } => {
            if grace_ms > 60_000 {
                return Err(ApiFailure::Invalid(
                    "runner stop grace must be at most 60000 ms".into(),
                ));
            }
            let lookup_project_id = project_id.clone();
            let lookup_session_id = session_id.clone();
            let session = state
                .with_store(move |store| {
                    store.session_snapshot(&lookup_project_id, &lookup_session_id)
                })
                .await?;
            let _delivery_admission = execution.lock_delivery_admission(&session.agent_id).await;
            let target_project_id = project_id.clone();
            let target_session_id = session_id.clone();
            let target = state
                .with_store(move |store| {
                    store.session_control_target(&target_project_id, &target_session_id)
                })
                .await?;
            let control_run_id = session_control_run_id(&session_id)?;
            let stop_project_id = project_id.clone();
            let stop_session_id = session_id.clone();
            state
                .commit_and_publish(move |store| {
                    let (session, event) = store.request_session_stop(
                        &stop_project_id,
                        &stop_session_id,
                        now_ms()?,
                    )?;
                    Ok((session, vec![event]))
                })
                .await?;
            RunnerClient::new(
                &target.runner_runtime,
                control_run_id,
                target.runner_instance_id,
            )
            .stop(grace_ms)
            .await
            .map_err(|error| runner_control_failure(error, "stop"))?;
            Ok(LocalResponse::SessionStopped { session_id })
        }
        LocalRequest::ProviderHook {
            token,
            event,
            payload,
        } => {
            if token.is_empty() || token.len() > 4096 {
                return Err(ApiFailure::Invalid("hook token is invalid".into()));
            }
            let lookup_token = token.clone();
            let session = state
                .with_store(move |store| store.find_session_by_hook_token(&lookup_token))
                .await?
                .ok_or_else(|| ApiFailure::Invalid("hook token is not recognized".into()))?;
            let project_id = session.project_id.clone();
            let agent_id = session.agent_id.clone();
            let session_id = session.id.clone();
            let (activity, inferred, wait_reason) = compute_hook_fields(event, &payload);
            let policy_decision = (event == ProviderHookEvent::PreToolUse)
                .then(|| crate::policy::decide(&payload, Path::new(&session.worktree)));
            let budget_denied = if event == ProviderHookEvent::PreToolUse {
                let budget_project_id = project_id.clone();
                let budget_agent_id = agent_id.clone();
                state
                    .commit_and_publish(move |store| {
                        let (_, denied, event) = store.observe_tool_call(
                            &budget_project_id,
                            &budget_agent_id,
                            now_ms()?,
                        )?;
                        Ok((denied, vec![event]))
                    })
                    .await?
            } else {
                false
            };
            if event == ProviderHookEvent::UserPromptSubmit {
                // Bind and commit the exact durable delivery before
                // publishing the hook event. The ack waiter then observes
                // `acknowledged`, never merely an unrelated prompt event.
                if execution.try_begin_agent_write(&agent_id) {
                    let result = execution::commit_pending_delivery_on_prompt(
                        state,
                        &session.snapshot(),
                        &payload,
                    )
                    .await;
                    execution.end_agent_write(&agent_id);
                    result?;
                }
            }
            let record_session_id = session_id.clone();
            let updated_session = state
                .commit_and_publish(move |store| {
                    let (session, event_envelope) = store.record_hook_event(
                        &record_session_id,
                        event,
                        activity,
                        inferred,
                        wait_reason,
                        now_ms()?,
                    )?;
                    Ok((session, vec![event_envelope]))
                })
                .await?;
            let reply = if let Some(decision) = policy_decision {
                let denied_by = decision.denied_by.map(str::to_owned);
                let policy_event = FactoryEvent::PolicyDecision {
                    project_id: project_id.clone(),
                    agent_id: agent_id.clone(),
                    session_id: session_id.clone(),
                    tool_name: decision.tool_name,
                    decision: if denied_by.is_some() { "deny" } else { "allow" }.to_owned(),
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
                            "permissionDecisionReason": "Dark Factory budget exhausted; run factoryctl agent budget reset"
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
            } else if matches!(
                event,
                ProviderHookEvent::Stop | ProviderHookEvent::SubagentStop
            ) {
                let stop_hook_active = payload
                    .get("stop_hook_active")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                // Deletion invariant (ARCHITECTURE.md #9, PR #50 re-review
                // correction): `stop_hook_reply` calls `compose_delivery`,
                // which can lazily recreate this agent's guidance files
                // the same way `deliver_pending`'s does -- gated the same
                // way, at this call site, since `execution` and `agent_id`
                // are already in scope here. A decline replies `{}`,
                // matching `stop_hook_active`'s own silent reply just
                // above: this is a live provider process's own hook call,
                // not an operator request it can retry, so nothing here
                // surfaces as an error into it.
                if execution.try_begin_agent_write(&agent_id) {
                    let result = execution::stop_hook_reply(
                        state,
                        guidance_root,
                        &updated_session,
                        stop_hook_active,
                    )
                    .await;
                    execution.end_agent_write(&agent_id);
                    result?
                } else {
                    serde_json::json!({})
                }
            } else {
                serde_json::json!({})
            };
            if event == ProviderHookEvent::SessionStart {
                // Codex reports its own thread id back in this hook's
                // payload (a Claude-shaped `session_id` field -- its
                // `--session-id` is instead assigned by the daemon up
                // front, so `Store::create_session` already set it there;
                // see `TRACK5-DESIGN.md` §1 and `TRACK5D` item 5).
                // Unconditional for every provider: `set_provider_session_id`
                // is a no-op once a session already carries one, which
                // Claude's and a resumed session's already do.
                if let Some(provider_session_id) = payload
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                {
                    let identify_session_id = session_id.clone();
                    let identify_provider_session_id = provider_session_id.to_owned();
                    state
                        .commit_and_publish(move |store| {
                            match store.set_provider_session_id(
                                &identify_session_id,
                                &identify_provider_session_id,
                                now_ms()?,
                            )? {
                                Some((_, event)) => Ok(((), vec![event])),
                                None => Ok(((), Vec::new())),
                            }
                        })
                        .await?;
                }
                // Proof the CLI is up: an agent's very first delivery is
                // PTY-typed (idle-session path), which needs the session to
                // be `idle` before typing anything (TRACK5-DESIGN.md §3);
                // anything queued before the session finished booting is
                // picked up here rather than waiting for the 5 second
                // safety tick. For Claude and `shell`, this real hook is
                // what makes that transition. For Codex, it is usually
                // already `idle` by the time this real (once-delayed) hook
                // arrives -- `execution::synthesize_codex_session_start`
                // made that transition earlier, once
                // `RunnerEvent::TerminalRaw` reported the provider's own
                // tty leaving canonical mode (`docs/providers.md`'s Codex
                // `SessionStart` section) -- so this `wake` is then a
                // harmless no-op for an agent that is already delivering;
                // it still matters for the (bounded) window before that
                // signal arrives, and for every other provider.
                execution.wake(project_id, agent_id);
            }
            Ok(LocalResponse::ProviderHookReply { reply })
        }
        LocalRequest::AttachTerminal { .. } => {
            unreachable!("AttachTerminal is handled per connection")
        }
        LocalRequest::TerminalInput {
            project_id,
            session_id,
            bytes,
        } => {
            let lookup_project_id = project_id.clone();
            let lookup_session_id = session_id.clone();
            let target = state
                .with_store(move |store| {
                    store.session_control_target(&lookup_project_id, &lookup_session_id)
                })
                .await?;
            let control_run_id = session_control_run_id(&session_id)?;
            RunnerClient::new(
                &target.runner_runtime,
                control_run_id,
                target.runner_instance_id,
            )
            .terminal_input(bytes)
            .await
            .map_err(|error| runner_control_failure(error, "terminal input"))?;
            Ok(LocalResponse::TerminalInputAccepted { session_id })
        }
        LocalRequest::ResizeTerminal {
            project_id,
            session_id,
            cols,
            rows,
        } => {
            let lookup_project_id = project_id.clone();
            let lookup_session_id = session_id.clone();
            let target = state
                .with_store(move |store| {
                    store.session_control_target(&lookup_project_id, &lookup_session_id)
                })
                .await?;
            let control_run_id = session_control_run_id(&session_id)?;
            RunnerClient::new(
                &target.runner_runtime,
                control_run_id,
                target.runner_instance_id,
            )
            .resize_terminal(cols, rows)
            .await
            .map_err(|error| runner_control_failure(error, "resize terminal"))?;
            Ok(LocalResponse::TerminalResized { session_id })
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

async fn populate_fleet_worktrees(projects: &mut [status::ProjectStatus]) {
    populate_fleet_worktrees_with(
        projects,
        MAX_CONCURRENT_WORKTREE_PROBES,
        FLEET_WORKTREE_DEADLINE,
        |path| async move { crate::worktrees::status(&path).await },
    )
    .await;
}

async fn populate_fleet_worktrees_with<Probe, ProbeFuture>(
    projects: &mut [status::ProjectStatus],
    max_concurrent: usize,
    deadline: Duration,
    probe: Probe,
) where
    Probe: Fn(PathBuf) -> ProbeFuture + Clone + Send + 'static,
    ProbeFuture: Future<Output = status::WorktreeStatus> + Send + 'static,
{
    let mut pending = Vec::new();
    for (project_index, project) in projects.iter_mut().enumerate() {
        for (agent_index, agent) in project.agents.iter_mut().enumerate() {
            if let Some(path) = agent.agent.worktree.clone() {
                agent.worktree = Some(status::WorktreeStatus {
                    path: path.clone(),
                    branch: None,
                    changed_files: 0,
                    dirty: false,
                    error: Some("fleet git status deadline exceeded".to_owned()),
                });
                pending.push((project_index, agent_index, PathBuf::from(path)));
            }
        }
    }

    let mut probes = JoinSet::new();
    let mut pending = pending.into_iter();
    let deadline = tokio::time::sleep(deadline);
    tokio::pin!(deadline);
    loop {
        while probes.len() < max_concurrent {
            let Some((project_index, agent_index, path)) = pending.next() else {
                break;
            };
            let probe = probe.clone();
            probes.spawn(async move {
                let worktree = probe(path).await;
                (project_index, agent_index, worktree)
            });
        }
        if probes.is_empty() {
            break;
        }
        tokio::select! {
            () = &mut deadline => {
                probes.abort_all();
                while probes.join_next().await.is_some() {}
                break;
            }
            result = probes.join_next() => {
                let Some(result) = result else {
                    break;
                };
                if let Ok((project_index, agent_index, worktree)) = result {
                    projects[project_index].agents[agent_index].worktree = Some(worktree);
                }
            }
        }
    }
}

#[cfg(test)]
mod worktree_status_tests {
    use super::*;
    use factory_core::{AgentRole, AgentSnapshot, ProjectSnapshot, Provider};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ActiveProbe(Arc<AtomicUsize>);

    impl Drop for ActiveProbe {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn fleet_probe_window_deadline_and_cancellation_are_bounded() {
        let project_id = ProjectId::try_from("factory").unwrap();
        let mut agents = Vec::new();
        for index in 0..12 {
            let id = AgentId::try_from(format!("worker-{index}")).unwrap();
            agents.push(status::AgentStatus {
                budget: factory_core::AgentBudget::default(),
                pause_reasons: Vec::new(),
                agent: AgentSnapshot {
                    id,
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Shell,
                    current_run_id: None,
                    paused: false,
                    current_session_id: None,
                    worktree: Some(format!("/work/{index}")),
                    created_at_ms: 0,
                    updated_at_ms: 0,
                },
                worktree: None,
                session: None,
                current_run: None,
                queue_depth: 0,
                queue: Vec::new(),
                inbox_pending: 0,
                attention: factory_core::attention::Attention::Routine,
                attention_inferred: true,
            });
        }
        let mut projects = vec![status::ProjectStatus {
            project: ProjectSnapshot {
                id: project_id,
                name: "Factory".into(),
                root: "/work".into(),
                created_at_ms: 0,
                updated_at_ms: 0,
            },
            agents,
            backlog_depth: 0,
            backlog: Vec::new(),
        }];
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let probe = {
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            move |_path: PathBuf| {
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    let _active = ActiveProbe(active);
                    std::future::pending::<status::WorktreeStatus>().await
                }
            }
        };
        let started = std::time::Instant::now();

        populate_fleet_worktrees_with(&mut projects, 8, Duration::from_millis(30), probe).await;

        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(peak.load(Ordering::SeqCst), 8);
        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "probes were not cancelled"
        );
        assert!(projects[0].agents.iter().all(|agent| {
            agent.worktree.as_ref().is_some_and(|worktree| {
                !worktree.dirty
                    && worktree.error.as_deref() == Some("fleet git status deadline exceeded")
            })
        }));
    }
}

async fn repository_request(
    state: &ApiState,
    token: String,
    request: RepositoryRequest,
) -> Result<LocalResponse, ApiFailure> {
    let session = state
        .with_store(move |store| store.find_session_by_hook_token(&token))
        .await?
        .ok_or_else(|| ApiFailure::Unauthorized("session authentication failed".into()))?;
    let project_id = session.project_id.clone();
    let change_project_id = session.project_id.clone();
    let change_agent_id = session.agent_id.clone();
    let change_run_id = session.current_run_id.clone();
    let registered_change = state
        .with_store(move |store| {
            store.managed_change_for_identity(
                &change_project_id,
                &change_agent_id,
                change_run_id.as_ref(),
            )
        })
        .await?;
    let project = state
        .with_store(move |store| store.get_project(&project_id))
        .await?;
    let authority_project_id = session.project_id.clone();
    let authority = state
        .with_store(move |store| store.repository_authority(&authority_project_id))
        .await?;
    // Reads use immutable snapshots and must not let a slow diff monopolize the
    // process-wide mutation boundary. Every operation that can change local or
    // remote state remains serialized until its final revalidation completes.
    let _slot = match &request {
        RepositoryRequest::Status | RepositoryRequest::Diff { .. } => None,
        _ => Some(state.repository_slot().await),
    };
    let operation = request.name().to_owned();
    let returns_reference = matches!(
        request,
        RepositoryRequest::Commit { .. }
            | RepositoryRequest::Push
            | RepositoryRequest::PrOpen { .. }
            | RepositoryRequest::PrUpdate { .. }
    );
    record_repository_audit(
        state,
        RepositoryAudit {
            project_id: session.project_id.clone(),
            agent_id: session.agent_id.clone(),
            session_id: session.id.clone(),
            operation: operation.clone(),
            phase: "requested".into(),
            success: None,
            reference: None,
        },
    )
    .await?;
    let audit_project_id = session.project_id.clone();
    let audit_agent_id = session.agent_id.clone();
    let audit_session_id = session.id.clone();
    let result = match repository::Target::validate_with_change(
        session,
        project,
        authority,
        registered_change,
    )
    .await
    {
        Ok(target) => {
            let command = match request {
                RepositoryRequest::Status => target.status().await,
                RepositoryRequest::Diff { staged } => target.diff(staged).await,
                RepositoryRequest::Commit { message } => target.commit(&message).await,
                RepositoryRequest::Push => target.push().await,
                RepositoryRequest::PrOpen { title, body } => target.pr_open(&title, &body).await,
                RepositoryRequest::PrUpdate {
                    number,
                    title,
                    body,
                } => target.pr_update(number, &title, &body).await,
            };
            (target, command)
        }
        Err(error) => {
            record_repository_audit(
                state,
                RepositoryAudit {
                    project_id: audit_project_id,
                    agent_id: audit_agent_id,
                    session_id: audit_session_id,
                    operation: operation.clone(),
                    phase: "finished".into(),
                    success: Some(false),
                    reference: None,
                },
            )
            .await?;
            return Err(repository_failure(error));
        }
    };
    let (target, command) = result;
    match command {
        Ok(output) => {
            if let Some(change) = target
                .registered_change()
                .cloned()
                .filter(|_| operation == "git_push")
            {
                let project_id = change.project_id.clone();
                let task_id = change.task_id.clone();
                let agent_id = change.agent_id.clone();
                let head = target
                    .revalidate_head_for_audit()
                    .await
                    .map_err(repository_failure)?;
                state
                    .commit_and_publish(move |store| {
                        let _ = store.publish_managed_change_head(
                            &project_id,
                            &task_id,
                            &agent_id,
                            &head,
                            now_ms()?,
                        )?;
                        Ok(((), Vec::new()))
                    })
                    .await?;
            }
            let reference = returns_reference.then(|| output.clone());
            record_repository_audit(
                state,
                RepositoryAudit {
                    project_id: target.project_id.clone(),
                    agent_id: target.agent_id.clone(),
                    session_id: target.session_id.clone(),
                    operation: operation.clone(),
                    phase: "finished".into(),
                    success: Some(true),
                    reference,
                },
            )
            .await?;
            Ok(LocalResponse::GitOutput { operation, output })
        }
        Err(error) => {
            record_repository_audit(
                state,
                RepositoryAudit {
                    project_id: target.project_id,
                    agent_id: target.agent_id,
                    session_id: target.session_id,
                    operation: operation.clone(),
                    phase: "finished".into(),
                    success: Some(false),
                    reference: None,
                },
            )
            .await?;
            Err(repository_failure(error))
        }
    }
}

fn managed_change_response(record: &crate::store::ManagedChangeRecord) -> LocalResponse {
    LocalResponse::ManagedChange {
        change: LocalManagedChange {
            project_id: record.project_id.clone(),
            task_id: record.task_id.clone(),
            agent_id: record.agent_id.clone(),
            worktree: record.worktree.clone(),
            branch: record.branch.clone(),
            base_sha: record.base_sha.clone(),
            head_sha: record.head_sha.clone(),
            published_head_sha: record.published_head_sha.clone(),
        },
    }
}

async fn create_managed_change_request(
    state: &ApiState,
    guidance_root: &Path,
    token: String,
) -> Result<LocalResponse, ApiFailure> {
    // Creation owns the repository slot from authentication through the
    // durable insert. Terminal task operations take the same slot, so no
    // task can close between filesystem provisioning and row registration.
    let _slot = state.repository_slot().await;
    let session = state
        .with_store(move |store| store.find_session_by_hook_token(&token))
        .await?
        .ok_or_else(|| ApiFailure::Unauthorized("session authentication failed".into()))?;
    let project_id = session.project_id.clone();
    let agent_id = session.agent_id.clone();
    let run_id = session.current_run_id.clone();
    let session_id = session.id.clone();
    record_repository_audit(
        state,
        RepositoryAudit {
            project_id: project_id.clone(),
            agent_id: agent_id.clone(),
            session_id: session.id.clone(),
            operation: "change_create".into(),
            phase: "requested".into(),
            success: None,
            reference: None,
        },
    )
    .await?;
    let result: Result<crate::store::ManagedChangeRecord, ApiFailure> = async {
        let (task, active) = state
            .with_store({
                let project_id = project_id.clone();
                let agent_id = agent_id.clone();
                let run_id = run_id.clone();
                move |store| {
                    let task =
                        store.current_task_for_identity(&project_id, &agent_id, run_id.as_ref())?;
                    let change = store.managed_change_for_identity(
                        &project_id,
                        &agent_id,
                        run_id.as_ref(),
                    )?;
                    Ok((task, change))
                }
            })
            .await?;
        let Some(task) = task else {
            return Err(ApiFailure::Invalid(
                "managed change requires an assigned current task".into(),
            ));
        };
        if let Some(active) = active {
            if active.state == "removing" {
                return Err(ApiFailure::Conflict(
                    "managed change abandonment is still in progress".into(),
                ));
            }
            return Ok(active);
        }
        let project = state
            .with_store({
                let project_id = project_id.clone();
                move |store| store.get_project(&project_id)
            })
            .await?;
        let authority = state
            .with_store({
                let project_id = project_id.clone();
                move |store| store.repository_authority(&project_id)
            })
            .await?;
        let path = factory_core::paths::managed_change_worktree_dir(
            guidance_root,
            &project_id,
            task.snapshot.id.as_str(),
        );
        let record = repository::create_managed_change(
            &project,
            authority,
            &task.snapshot.id,
            &agent_id,
            &path,
        )
        .await
        .map_err(repository_failure)?;
        let record_for_store = record.clone();
        let stored = state
            .commit_and_publish(move |store| {
                let record = store.create_managed_change(record_for_store, now_ms()?)?;
                Ok((record, Vec::new()))
            })
            .await;
        match stored {
            Ok(record) => Ok(record),
            Err(error) => {
                if let Err(cleanup) =
                    repository::discard_unregistered_change(Path::new(&project.root), &record).await
                {
                    return Err(ApiFailure::Conflict(format!(
                        "managed change registration failed: {error}; cleanup failed: {cleanup}"
                    )));
                }
                Err(ApiFailure::from(error))
            }
        }
    }
    .await;
    let finished = RepositoryAudit {
        project_id: project_id.clone(),
        agent_id: agent_id.clone(),
        session_id: session_id.clone(),
        operation: "change_create".into(),
        phase: "finished".into(),
        success: Some(result.is_ok()),
        reference: result.as_ref().ok().map(|change| change.branch.clone()),
    };
    record_repository_audit(state, finished).await?;
    result.map(|record| managed_change_response(&record))
}

async fn abandon_managed_change_request(
    state: &ApiState,
    token: String,
) -> Result<LocalResponse, ApiFailure> {
    let session = state
        .with_store(move |store| store.find_session_by_hook_token(&token))
        .await?
        .ok_or_else(|| ApiFailure::Unauthorized("session authentication failed".into()))?;
    let project_id = session.project_id.clone();
    let agent_id = session.agent_id.clone();
    let run_id = session.current_run_id.clone();
    let session_id = session.id.clone();
    record_repository_audit(
        state,
        RepositoryAudit {
            project_id: project_id.clone(),
            agent_id: agent_id.clone(),
            session_id: session.id.clone(),
            operation: "change_abandon".into(),
            phase: "requested".into(),
            success: None,
            reference: None,
        },
    )
    .await?;
    let result: Result<crate::store::ManagedChangeRecord, ApiFailure> = async {
        let change = state
            .with_store({
                let project_id = project_id.clone();
                let agent_id = agent_id.clone();
                let run_id = run_id.clone();
                move |store| {
                    store.managed_change_for_identity(&project_id, &agent_id, run_id.as_ref())
                }
            })
            .await
            .map_err(ApiFailure::from)?
            .ok_or_else(|| {
                ApiFailure::Conflict("no active managed change for the current task".into())
            })?;
        let project = state
            .with_store({
                let project_id = project_id.clone();
                move |store| store.get_project(&project_id)
            })
            .await?;
        let authority = state
            .with_store({
                let project_id = project_id.clone();
                move |store| store.repository_authority(&project_id)
            })
            .await?;
        let _slot = state.repository_slot().await;
        let change = state
            .commit_and_publish({
                let project_id = project_id.clone();
                let agent_id = agent_id.clone();
                let task_id = change.task_id.clone();
                move |store| {
                    let change = store.begin_abandon_managed_change(
                        &project_id,
                        &task_id,
                        &agent_id,
                        now_ms()?,
                    )?;
                    Ok((change, Vec::new()))
                }
            })
            .await
            .map_err(ApiFailure::from)?;
        let worktree_exists = Path::new(&change.worktree).exists();
        if worktree_exists {
            let target = repository::Target::validate_with_change(
                session,
                project.clone(),
                authority,
                Some(change.clone()),
            )
            .await
            .map_err(repository_failure)?;
            target
                .ensure_abandonable()
                .await
                .map_err(repository_failure)?;
        }
        if worktree_exists {
            crate::worktrees::remove(Path::new(&project.root), Path::new(&change.worktree))
                .await
                .map_err(|error| ApiFailure::Conflict(error.to_string()))?;
        }
        state
            .commit_and_publish({
                let project_id = project_id.clone();
                let task_id = change.task_id.clone();
                let agent_id = agent_id.clone();
                move |store| {
                    let change = store.finish_abandon_managed_change(
                        &project_id,
                        &task_id,
                        &agent_id,
                        now_ms()?,
                    )?;
                    Ok((change, Vec::new()))
                }
            })
            .await
            .map_err(ApiFailure::from)
    }
    .await;
    let finished = RepositoryAudit {
        project_id,
        agent_id,
        session_id,
        operation: "change_abandon".into(),
        phase: "finished".into(),
        success: Some(result.is_ok()),
        reference: result.as_ref().ok().map(|change| change.branch.clone()),
    };
    record_repository_audit(state, finished).await?;
    result.map(|record| managed_change_response(&record))
}

async fn record_repository_audit(
    state: &ApiState,
    audit: RepositoryAudit,
) -> Result<(), ApiFailure> {
    state
        .commit_and_publish(move |store| {
            let event = store.record_repository_operation(NewRepositoryOperation {
                project_id: audit.project_id,
                agent_id: audit.agent_id,
                session_id: audit.session_id,
                operation: audit.operation,
                phase: audit.phase,
                success: audit.success,
                reference: audit.reference,
                occurred_at_ms: now_ms()?,
            })?;
            Ok(((), vec![event]))
        })
        .await?;
    Ok(())
}

fn repository_failure(error: repository::Error) -> ApiFailure {
    match error {
        repository::Error::Rejected(_) => ApiFailure::Invalid(error.to_string()),
        repository::Error::Command(_) | repository::Error::Timeout => {
            ApiFailure::Conflict(error.to_string())
        }
    }
}

/// Absolute guidance-file paths for one agent, computed from the daemon's
/// state root; never touches the filesystem itself.
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
    // same way spawn preparation is (same per-agent lock), so a
    // concurrent `DeleteAgent`'s drain can never miss this read.
    if !execution.try_begin_agent_write(agent_id) {
        return Err(ApiFailure::Conflict("agent is being deleted".into()));
    }
    let agent_paths = AgentGuidancePaths::new(guidance_root, project_id, agent_id);
    let guidance = read_agent_guidance_files(&agent_paths).await;
    execution.end_agent_write(agent_id);
    let (instructions, memory) = guidance?;
    Ok(local_agent_detail(agent, instructions, memory, agent_paths))
}

async fn read_agent_guidance_files(
    paths: &AgentGuidancePaths,
) -> Result<(String, String), ApiFailure> {
    let instructions = read_guidance_file(paths.instructions.clone()).await?;
    let memory = read_guidance_file(paths.memory.clone()).await?;
    Ok((instructions, memory))
}

fn local_agent_detail(
    agent: crate::store::AgentDetail,
    instructions: String,
    memory: String,
    paths: AgentGuidancePaths,
) -> LocalAgentDetail {
    LocalAgentDetail {
        snapshot: agent.snapshot,
        profile: LocalAgentProfile {
            model: agent.profile.model,
            reasoning_effort: agent.profile.reasoning_effort,
            model_selection_reason: agent.profile.model_selection_reason,
            permission_mode: agent.profile.permission_mode,
            instructions,
            memory,
            updated_at_ms: agent.profile.updated_at_ms,
        },
        instructions_path: path_to_string(&paths.instructions),
        memory_path: path_to_string(&paths.memory),
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
    worktree: Option<String>,
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
    let resolved_worktree = match worktree {
        Some(worktree) => Some(worktree),
        None => Some(
            provision_agent_worktree(state, guidance_root, &created_project_id, &created_agent_id)
                .await?,
        ),
    };
    let agent = if let Some(worktree) = resolved_worktree {
        let worktree_project_id = created_project_id.clone();
        let worktree_agent_id = created_agent_id.clone();
        state
            .commit_and_publish(move |store| {
                let (agent, event) = store.set_agent_worktree(
                    &worktree_project_id,
                    &worktree_agent_id,
                    worktree,
                    now_ms()?,
                )?;
                Ok((agent, vec![event]))
            })
            .await?
    } else {
        agent
    };
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

/// Resolves the worktree a newly created agent should get when `agent add`
/// did not pass `--worktree` explicitly (D3, `TRACK5-WIRE.md`): a fresh
/// `git worktree add -b agent/<agent_id>` under
/// `agent_worktree_dir(guidance_root, project_id, agent_id)` from the
/// project's `origin/HEAD` (or local `main` without a remote) when the
/// project root is a git repo,
/// else the project root itself. A `git worktree add` failure (a real one,
/// not "branch already exists" -- `worktrees::add` already retries that)
/// falls back to the project root rather than blocking agent creation
/// entirely: a working root beats no agent at all.
async fn provision_agent_worktree(
    state: &ApiState,
    guidance_root: &Path,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<String, ApiFailure> {
    let lookup_project_id = project_id.clone();
    let project = state
        .with_store(move |store| store.get_project(&lookup_project_id))
        .await?;
    let project_root = PathBuf::from(&project.root);
    if !crate::worktrees::is_git_repo(&project_root).await {
        return Ok(project.root);
    }
    let worktree_dir = factory_core::paths::agent_worktree_dir(guidance_root, project_id, agent_id);
    let branch = format!("agent/{}", agent_id.as_str());
    match crate::worktrees::add(&project_root, &worktree_dir, &branch).await {
        Ok(()) => Ok(path_to_string(&worktree_dir)),
        Err(error) => {
            tracing::warn!(
                %error, %project_id, %agent_id,
                "git worktree add failed; using the project root instead"
            );
            Ok(project.root)
        }
    }
}

/// Runs the file/database work of `DeleteAgent`, called only after
/// `execution.begin_delete` has confirmed no spawn preparation can still be
/// mid-write into this agent's guidance directory (ARCHITECTURE.md's
/// deletion invariant): checks `store.check_agent_deletable` first, then
/// removes the git worktree and the guidance directory, *then* deletes the
/// ledger row (PR #50 re-review's blocking finding).
///
/// The precheck exists because a refusal must be completely side-effect
/// free: `store.delete_agent`'s own preconditions (`AgentHasActiveRun`,
/// `AgentHasLiveSession`, `AgentHasChildren`, `AgentRunHasDependents`) live
/// *inside* its transaction, which only runs after the files below are
/// already gone -- so without this check, the single most ordinary operator
/// mistake ("delete a busy or parent agent") answered `Conflict` while
/// silently destroying `instructions.md`/`memory.md`, a data-loss
/// regression a re-review caught and reproduced through the public API
/// (`UpdateAgentProfile` writes distinctive instructions, `DeleteAgent` on
/// a parent answers `AgentHasChildren`, `instructions.md` is gone). The
/// precheck is sound run here, right after `execution.begin_delete`'s
/// drain and before any file is touched: the deletion gate already rules
/// out a *new* session appearing between this read and the removals below,
/// which is the only way `check_agent_deletable`'s answer could go stale
/// before `delete_agent`'s own transaction re-confirms it as the
/// authoritative last word.
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
    remove_agent_worktree_if_any(state, &project_id, &agent_id).await?;
    remove_agent_guidance(guidance_root, &project_id, &agent_id).await?;
    state
        .commit_and_publish(move |store| {
            let events = store.delete_agent(&project_id, &agent_id, now_ms()?)?;
            Ok(((), events))
        })
        .await?;
    Ok(())
}

/// Recursively removes one agent's guidance directory, run after
/// `DeleteAgent`'s transaction has already committed. The ledger row is
/// gone either way, but a filesystem failure here is now still reported as
/// the request's own error (AGENTS.md rule 3: no silent fallback) rather
/// than merely logged -- `execution.begin_delete` having already drained
/// any in-flight preparation means a failure here is a real problem (a
/// permission issue, an unexpected file), not the race this task closes.
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

/// Removes an agent's git worktree before `DeleteAgent`'s transaction
/// commits (D3, `TRACK5-WIRE.md`): dirty refuses the whole request
/// (`ApiFailure::Conflict`), matching the design's "unless dirty ->
/// Conflict" -- the agent row and its worktree must not diverge. A missing
/// worktree (already removed by hand) or one that equals the project root
/// (the `provision_agent_worktree` fallback: not a git repo, or `git
/// worktree add` itself failed at creation time -- nothing separate to
/// remove either way) is a no-op. Any other git failure is logged and
/// otherwise ignored: getting the agent row deleted matters more than a
/// stray leftover directory the operator can clean up by hand.
async fn remove_agent_worktree_if_any(
    state: &ApiState,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<(), ApiFailure> {
    let lookup_project_id = project_id.clone();
    let lookup_agent_id = agent_id.clone();
    let agent = state
        .with_store(move |store| store.get_agent_detail(&lookup_project_id, &lookup_agent_id))
        .await?;
    let Some(worktree) = agent.snapshot.worktree else {
        return Ok(());
    };
    let lookup_project_id = project_id.clone();
    let project = state
        .with_store(move |store| store.get_project(&lookup_project_id))
        .await?;
    let project_root = PathBuf::from(&project.root);
    let worktree_path = PathBuf::from(&worktree);
    if worktree_path == project_root || !worktree_path.exists() {
        return Ok(());
    }
    match crate::worktrees::remove(&project_root, &worktree_path).await {
        Ok(()) => Ok(()),
        Err(crate::worktrees::WorktreeError::Dirty) => Err(ApiFailure::Conflict(
            "agent worktree has modified or untracked files; commit, discard, or remove it \
             manually with `git worktree remove --force` before deleting the agent"
                .into(),
        )),
        Err(error) => {
            tracing::warn!(
                %error, %project_id, %agent_id,
                "git worktree remove failed; deleting the agent anyway"
            );
            Ok(())
        }
    }
}

/// Runs the file/database work of `DeleteProject`, called only after
/// `execution.begin_delete_project` and every one of the project's agents
/// has been through `execution.begin_delete` (its caller's loop): checks
/// `store.check_project_deletable` first (same reasoning as
/// [`delete_agent_locked`]'s precheck -- `ProjectHasActiveRun` must refuse
/// before `projects/<p>/`, which holds every one of the project's agents'
/// worktrees, is removed, not after), then removes the project's whole
/// guidance directory tree, *then* deletes the ledger row.
async fn delete_project_locked(
    state: &ApiState,
    guidance_root: &Path,
    project_id: ProjectId,
) -> Result<(), ApiFailure> {
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

/// Recursively removes one project's guidance directory, run after
/// `DeleteProject`'s transaction has already committed. See
/// [`remove_agent_guidance`] for why a failure here is now the request's
/// own error rather than merely logged.
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

/// Validates an operator-supplied agent worktree override (D3): must be an
/// absolute, existing directory. Creating the git worktree itself is
/// execution's job; this only records the path.
async fn validate_agent_worktree(worktree: String) -> Result<String, ApiFailure> {
    if !Path::new(&worktree).is_absolute() {
        return Err(ApiFailure::Invalid(
            "agent worktree must be an absolute path".into(),
        ));
    }
    tokio::task::spawn_blocking(move || {
        if !Path::new(&worktree).is_dir() {
            return Err(ApiFailure::Invalid(
                "agent worktree must be an existing directory".into(),
            ));
        }
        Ok(worktree)
    })
    .await
    .map_err(|error| ApiFailure::Internal(format!("worktree check worker failed: {error}")))?
}

fn session_page_limit(limit: Option<usize>) -> Result<usize, ApiFailure> {
    // `ListSessions.limit` is `Option<usize>` (every other `List*` request
    // takes a required `u32`, defaulted client-side instead): a caller that
    // omits it gets this default rather than the wire max.
    const DEFAULT_SESSION_PAGE: usize = 100;
    let limit = limit.unwrap_or(DEFAULT_SESSION_PAGE);
    if !(1..=MAX_SESSION_PAGE_ITEMS as usize).contains(&limit) {
        return Err(ApiFailure::Invalid(format!(
            "session page limit must be between 1 and {MAX_SESSION_PAGE_ITEMS}"
        )));
    }
    Ok(limit)
}

/// Computes the `record_hook_event` inputs for one hook event from its
/// opaque JSON payload: `tool_name` for `PreToolUse`, `message` for
/// `Notification`; every other event carries no payload-derived field.
fn compute_hook_fields(
    event: ProviderHookEvent,
    payload: &serde_json::Value,
) -> (Option<String>, bool, Option<String>) {
    match event {
        ProviderHookEvent::SessionStart | ProviderHookEvent::Stop => (None, false, None),
        ProviderHookEvent::UserPromptSubmit | ProviderHookEvent::PostToolUse => {
            (Some("thinking".into()), true, None)
        }
        ProviderHookEvent::PreToolUse => {
            let tool_name = payload
                .get("tool_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            (
                Some(bounded_hook_field(&format!("tool: {tool_name}"))),
                false,
                None,
            )
        }
        ProviderHookEvent::Notification => {
            let message = payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("waiting for input");
            (None, false, Some(bounded_hook_field(message)))
        }
        ProviderHookEvent::PermissionRequest => {
            // Codex's own approval prompt (`docs/providers.md`'s
            // observe-only contract: this daemon never answers it, only
            // records that the session is now blocked on one). Claude
            // Code's equivalent surfaces through `Notification` above --
            // both land the session in the same `waiting_for_input` state
            // via `Store::record_hook_event`, with a wait reason an
            // operator can read at a glance.
            let tool_name = payload
                .get("tool_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            (
                None,
                false,
                Some(bounded_hook_field(&format!(
                    "provider approval prompt: {tool_name}"
                ))),
            )
        }
        ProviderHookEvent::SubagentStop | ProviderHookEvent::SessionEnd => (None, false, None),
    }
}

fn bounded_hook_field(value: &str) -> String {
    if value.len() <= MAX_HOOK_FIELD_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_HOOK_FIELD_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn runner_control_failure(error: RunnerClientError, action: &'static str) -> ApiFailure {
    match error {
        RunnerClientError::RunnerRejected {
            code: RunnerErrorCode::Conflict,
        } => ApiFailure::Conflict(format!("runner rejected the {action} request")),
        RunnerClientError::RunnerRejected {
            code: RunnerErrorCode::InvalidRequest,
        } => ApiFailure::Invalid(format!("runner rejected the {action} request")),
        RunnerClientError::InvalidStopGrace { found } => ApiFailure::Invalid(format!(
            "runner stop grace must be at most 60000 ms, got {found}"
        )),
        _ => ApiFailure::Internal(format!("runner {action} request failed")),
    }
}

fn read_run_terminal(
    target: &SessionControlTarget,
    run_id: RunId,
) -> Result<RunTerminal, ApiFailure> {
    let spool_path = PathBuf::from(&target.runner_runtime).join("events.ndjson");
    let file = match fs::File::open(spool_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ApiFailure::Store(StoreError::RunNotFound));
        }
        Err(_) => {
            return Err(ApiFailure::Internal(
                "runner terminal spool could not be read".into(),
            ));
        }
    };
    let mut bytes = Vec::new();
    file.take(u64::try_from(MAX_RUNNER_SPOOL_BYTES + 1).expect("spool bound fits u64"))
        .read_to_end(&mut bytes)
        .map_err(|_| ApiFailure::Internal("runner terminal spool could not be read".into()))?;
    if bytes.len() > MAX_RUNNER_SPOOL_BYTES {
        return Err(ApiFailure::Internal(
            "runner terminal spool exceeded its bound".into(),
        ));
    }

    let terminated = bytes.last() == Some(&b'\n');
    let lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let line_count = lines.len();
    let mut head_sequence = 0;
    let mut output = String::new();
    let mut truncated = false;
    for (index, line) in lines.into_iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_RUNNER_FRAME_BYTES {
            return Err(ApiFailure::Internal(
                "runner terminal frame exceeded its bound".into(),
            ));
        }
        let event: RunnerEventEnvelope = match serde_json::from_slice(line) {
            Ok(event) => event,
            Err(_) if !terminated && index + 1 == line_count => break,
            // A malformed non-final line means a concurrent writer left a
            // torn record behind (the spool is append-only, so this cannot
            // be later "fixed" by more appends); degrade to a truncated
            // read of whatever was durably complete before it rather than
            // failing the whole request.
            Err(_) => {
                truncated = true;
                break;
            }
        };
        if event.protocol_version != factory_core::runner::RUNNER_PROTOCOL_VERSION {
            return Err(ApiFailure::Internal(
                "runner terminal protocol is unsupported".into(),
            ));
        }
        head_sequence = head_sequence.max(event.sequence);
        match event.event {
            RunnerEvent::Output { stream, text, .. } => {
                let prefix = match stream {
                    OutputStream::Stdout => "[stdout] ",
                    OutputStream::Stderr => "[stderr] ",
                };
                append_terminal_text(&mut output, prefix, &mut truncated);
                let text = sanitize_terminal_text(&text);
                append_terminal_text(&mut output, &text, &mut truncated);
            }
            RunnerEvent::OutputTruncated { .. } => truncated = true,
            RunnerEvent::Started { .. }
            | RunnerEvent::SpawnFailed { .. }
            | RunnerEvent::TerminalRaw
            | RunnerEvent::TerminalRawTimedOut
            | RunnerEvent::Exited { .. }
            | RunnerEvent::Unknown => {}
        }
    }

    Ok(RunTerminal {
        run_id,
        head_sequence,
        output,
        truncated,
    })
}

fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect()
}

fn append_terminal_text(output: &mut String, text: &str, truncated: &mut bool) {
    output.push_str(text);
    if output.len() <= MAX_TERMINAL_OUTPUT_BYTES {
        return;
    }
    *truncated = true;
    let mut first = output.len() - MAX_TERMINAL_OUTPUT_BYTES;
    while !output.is_char_boundary(first) {
        first += 1;
    }
    output.drain(..first);
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

/// The runner protocol still keys control requests by `RunId` (see
/// `factory_core::runner`); `SessionId` and `RunId` share the same
/// charset/length validation, so a session's own id doubles as its
/// runner-facing identity until that protocol grows a session concept of
/// its own.
fn session_control_run_id(session_id: &SessionId) -> Result<RunId, ApiFailure> {
    RunId::try_from(session_id.as_str())
        .map_err(|_| ApiFailure::Internal("session id is not runner-addressable".into()))
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

/// Wiring-level tests for the two deletion-gate call sites this file adds
/// around `execution::stop_hook_reply`/`execution::commit_pending_delivery_on_prompt`
/// (PR #50 review, round 3's nit): every existing `tests/local_api.rs`/
/// `tests/sessions_e2e.rs` suite still passes even if `try_begin_agent_write`'s
/// result is discarded at both call sites (the reviewer verified this by
/// mutation), because neither hook path's *ordinary* behavior depends on
/// the gate -- only the race this PR closes does, and that race needs a
/// second, concurrent request to observe. These tests drive
/// `handle_request` directly (visible here, not from the external
/// `tests/` crate, since it is module-private) with the agent already
/// marked deleting, so no real race or timing is needed: the assertion is
/// "this exact call, with the mark already set, must not touch the store
/// the way it normally would" -- exactly what the mutation the reviewer
/// tried would break.
#[cfg(test)]
mod deletion_gate_tests {
    use std::os::unix::fs::PermissionsExt;

    use factory_core::{AgentRole, Provider, RunnerInstanceId, SessionId, TaskId, TaskStatus};

    use super::*;
    use crate::{
        execution::DELIVERY_ATTEMPT_MARKER,
        store::{
            DeliveryAttemptState, NewAgent, NewDeliveryAttempt, NewProject, NewSession, NewTask,
            Store,
        },
    };

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn config(directory: &Path) -> execution::Config {
        execution::Config {
            runner_program: directory.join("factory-runner"),
            factoryctl_path: directory.join("factoryctl"),
            runtime_root: directory.join("runs"),
            guidance_root: directory.to_path_buf(),
            socket_path: directory.join("f.sock"),
            max_active_runs: 1,
            session_start_deadline: execution::SESSION_START_DEADLINE,
        }
    }

    const HOOK_TOKEN_LEN: usize = 64;

    /// A project, one agent, one live session (with a fixed, known hook
    /// token so a test can address it via `LocalRequest::ProviderHook`
    /// exactly like a real provider process would), and one task assigned
    /// to that agent -- pending work for `compose_delivery` to find, so a
    /// gate decline is distinguishable from "nothing to deliver anyway".
    async fn setup(directory: &Path) -> (ApiState, execution::Handle, ProjectId, AgentId, TaskId) {
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("curie").unwrap();
        let task_id = TaskId::try_from("task-1").unwrap();

        let mut store = Store::open_in_memory().unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Factory".to_owned(),
                    root: directory.to_string_lossy().into_owned(),
                },
                1_000,
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
                1_000,
            )
            .unwrap();
        store
            .create_task(
                NewTask {
                    id: task_id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Do the thing".to_owned(),
                    body: "Do the thing.".to_owned(),
                    priority: 0,
                },
                1_000,
            )
            .unwrap();
        let state = ApiState::new(store);
        let (execution, join) = execution::spawn(config(directory), state.clone()).unwrap();
        // These hook tests exercise request-local state transitions, not
        // runner recovery. Stop the dispatcher while the store has no live
        // session, then install the synthetic session. Otherwise startup
        // recovery correctly ends that runner-less row and races token lookup.
        execution.shutdown().await.unwrap();
        join.await.unwrap().unwrap();
        let session_project_id = project_id.clone();
        let session_agent_id = agent_id.clone();
        let session_task_id = task_id.clone();
        let session_worktree = directory.to_string_lossy().into_owned();
        let session_runtime = directory
            .join("runs")
            .join("session-1")
            .to_string_lossy()
            .into_owned();
        state
            .commit_and_publish(move |store| {
                let (_, session_event) = store.create_session(
                    NewSession {
                        id: SessionId::try_from("11111111-1111-4111-8111-111111111111").unwrap(),
                        project_id: session_project_id.clone(),
                        agent_id: session_agent_id.clone(),
                        provider: Provider::Shell,
                        runtime_model: None,
                        runtime_reasoning_effort: None,
                        runtime_permission_mode: None,
                        runtime_control_mode: None,
                        provider_session_id: None,
                        worktree: session_worktree,
                        codex_home: None,
                        hook_token: "a".repeat(HOOK_TOKEN_LEN),
                        runner_instance_id: RunnerInstanceId::try_from(
                            "22222222-2222-4222-8222-222222222222",
                        )
                        .unwrap(),
                        runner_runtime: session_runtime,
                        runner_protocol_version: 1,
                    },
                    1_000,
                )?;
                let (_, task_event) = store.assign_task(
                    &session_project_id,
                    &session_task_id,
                    Some(&session_agent_id),
                    1_000,
                )?;
                Ok(((), vec![session_event, task_event]))
            })
            .await
            .unwrap();
        (state, execution, project_id, agent_id, task_id)
    }

    /// `commit_pending_delivery_on_prompt`'s gated call site
    /// (`UserPromptSubmit`): with `curie` marked deleting, a
    /// `UserPromptSubmit` hook -- even with a delivery genuinely in
    /// flight (`try_delivery_slot` held, satisfying
    /// `commit_pending_delivery_on_prompt`'s own precondition) and a task
    /// assigned and waiting -- must not open the run episode. Verified by
    /// mutation: discarding `try_begin_agent_write`'s result at that call
    /// site (matching the reviewer's own repro) makes this test's final
    /// assertion fail (`task-1` moves to `Running`); restoring the gate
    /// makes it pass again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn commit_pending_delivery_on_prompt_declines_while_the_agent_is_deleting() {
        let directory = private_tempdir();
        let (state, execution, project_id, agent_id, task_id) = setup(directory.path()).await;

        let _delivery_slot = state.try_delivery_slot(&agent_id).unwrap();
        execution.begin_delete(&agent_id).await.unwrap();

        let response = handle_request(
            &state,
            &execution,
            directory.path(),
            LocalRequest::ProviderHook {
                token: "a".repeat(HOOK_TOKEN_LEN),
                event: ProviderHookEvent::UserPromptSubmit,
                payload: serde_json::json!({}),
            },
        )
        .await;
        assert!(
            response.is_ok(),
            "a declined gate must not surface as the hook request's own error, got {response:?}"
        );

        let task = state
            .with_store({
                let project_id = project_id.clone();
                let task_id = task_id.clone();
                move |store| store.get_task(&project_id, &task_id)
            })
            .await
            .unwrap();
        assert_eq!(
            task.snapshot.status,
            TaskStatus::Queued,
            "no run episode must open for a deleting agent's UserPromptSubmit hook"
        );

        execution.end_delete(&agent_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delayed_same_text_hook_cannot_commit_a_recreated_task_attempt() {
        let directory = private_tempdir();
        let (state, execution, project_id, agent_id, task_id) = setup(directory.path()).await;
        let session_id = SessionId::try_from("11111111-1111-4111-8111-111111111111").unwrap();
        let visible_text = "same prompt";
        let old_text = format!("{visible_text}\n{DELIVERY_ATTEMPT_MARKER}attempt-a\u{2063}");
        let old_attempt_text = old_text.clone();
        let (old_incarnation, old_run_count) = state
            .with_store({
                let session_id = session_id.clone();
                let task_id = task_id.clone();
                move |store| store.task_delivery_marker(&session_id, &task_id)
            })
            .await
            .unwrap();

        state
            .commit_and_publish({
                let project_id = project_id.clone();
                let agent_id = agent_id.clone();
                let session_id = session_id.clone();
                let task_id = task_id.clone();
                move |store| {
                    store.ensure_delivery_attempt(NewDeliveryAttempt {
                        id: "attempt-a".to_owned(),
                        project_id,
                        agent_id,
                        session_id,
                        task_id: Some(task_id),
                        task_incarnation_id: Some(old_incarnation),
                        prior_run_count: Some(old_run_count),
                        message_ids: Vec::new(),
                        text: old_attempt_text,
                        created_at_ms: 1_001,
                    })?;
                    Ok(((), Vec::new()))
                }
            })
            .await
            .unwrap();

        // Delete/recreate the same operator-facing id. Deletion cancels A;
        // B has the same visible prompt but a different immutable attempt
        // nonce and task incarnation.
        state
            .commit_and_publish({
                let project_id = project_id.clone();
                let agent_id = agent_id.clone();
                let task_id = task_id.clone();
                move |store| {
                    store.delete_task(&project_id, &task_id, 1_002)?;
                    store.create_task(
                        NewTask {
                            id: task_id.clone(),
                            project_id: project_id.clone(),
                            parent_task_id: None,
                            title: "Replacement".to_owned(),
                            body: visible_text.to_owned(),
                            priority: 0,
                        },
                        1_003,
                    )?;
                    store.assign_task(&project_id, &task_id, Some(&agent_id), 1_004)?;
                    Ok(((), Vec::new()))
                }
            })
            .await
            .unwrap();
        let (incarnation, prior_run_count) = state
            .with_store({
                let session_id = session_id.clone();
                let task_id = task_id.clone();
                move |store| store.task_delivery_marker(&session_id, &task_id)
            })
            .await
            .unwrap();
        let new_text = format!("{visible_text}\n{DELIVERY_ATTEMPT_MARKER}attempt-b\u{2063}");
        state
            .commit_and_publish({
                let project_id = project_id.clone();
                let agent_id = agent_id.clone();
                let session_id = session_id.clone();
                let task_id = task_id.clone();
                move |store| {
                    store.ensure_delivery_attempt(NewDeliveryAttempt {
                        id: "attempt-b".to_owned(),
                        project_id,
                        agent_id,
                        session_id,
                        task_id: Some(task_id),
                        task_incarnation_id: Some(incarnation),
                        prior_run_count: Some(prior_run_count),
                        message_ids: Vec::new(),
                        text: new_text,
                        created_at_ms: 1_005,
                    })?;
                    Ok(((), Vec::new()))
                }
            })
            .await
            .unwrap();

        let _delivery_slot = state.try_delivery_slot(&agent_id).unwrap();
        let response = handle_request(
            &state,
            &execution,
            directory.path(),
            LocalRequest::ProviderHook {
                token: "a".repeat(HOOK_TOKEN_LEN),
                event: ProviderHookEvent::UserPromptSubmit,
                payload: serde_json::json!({"prompt": old_text}),
            },
        )
        .await;
        assert!(response.is_ok());
        let task = state
            .with_store({
                let project_id = project_id.clone();
                let task_id = task_id.clone();
                move |store| store.get_task(&project_id, &task_id)
            })
            .await
            .unwrap();
        assert_eq!(task.snapshot.status, TaskStatus::Queued);
        assert_eq!(
            state
                .with_store(|store| store.delivery_attempt_state("attempt-b"))
                .await
                .unwrap(),
            Some(DeliveryAttemptState::InFlight)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pre_tool_use_denial_is_returned_and_durably_audited() {
        let directory = private_tempdir();
        let (state, execution, ..) = setup(directory.path()).await;
        let response = handle_request(
            &state,
            &execution,
            directory.path(),
            LocalRequest::ProviderHook {
                token: "a".repeat(HOOK_TOKEN_LEN),
                event: ProviderHookEvent::PreToolUse,
                payload: serde_json::json!({"tool_name":"Bash","tool_input":{"command":"git reset --hard HEAD~1"}}),
            },
        ).await.unwrap();
        let LocalResponse::ProviderHookReply { reply } = response else {
            panic!("unexpected response")
        };
        assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
        let events = state
            .with_store(|store| store.events_after(0, 100))
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.event,
            FactoryEvent::PolicyDecision { decision, rule: Some(rule), .. }
                if decision == "deny" && rule == "destructive_git"
        )));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exhausted_budget_denies_hook_and_reset_reopens_it() {
        let directory = private_tempdir();
        let (state, execution, project_id, agent_id, _) = setup(directory.path()).await;
        handle_request(
            &state,
            &execution,
            directory.path(),
            LocalRequest::SetAgentBudget {
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
                max_tool_calls: Some(1),
            },
        )
        .await
        .unwrap();
        let hook = || LocalRequest::ProviderHook {
            token: "a".repeat(HOOK_TOKEN_LEN),
            event: ProviderHookEvent::PreToolUse,
            payload: serde_json::json!({"tool_name":"Read","tool_input":{"file_path":"README.md"}}),
        };
        let first = handle_request(&state, &execution, directory.path(), hook())
            .await
            .unwrap();
        assert!(
            matches!(first, LocalResponse::ProviderHookReply { ref reply } if *reply == serde_json::json!({}))
        );
        let second = handle_request(&state, &execution, directory.path(), hook())
            .await
            .unwrap();
        assert_eq!(
            match second {
                LocalResponse::ProviderHookReply { reply } =>
                    reply["hookSpecificOutput"]["permissionDecision"].clone(),
                _ => unreachable!(),
            },
            "deny"
        );
        let status = state
            .with_store({
                let project_id = project_id.clone();
                let agent_id = agent_id.clone();
                move |store| store.agent_status(&project_id, &agent_id)
            })
            .await
            .unwrap();
        assert!(status.budget.exhausted && status.agent.paused);
        let resume = handle_request(
            &state,
            &execution,
            directory.path(),
            LocalRequest::ResumeAgent {
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
            },
        )
        .await;
        assert!(matches!(
            resume,
            Err(ApiFailure::Store(StoreError::AgentBudgetExhausted))
        ));
        let stopped = handle_request(
            &state,
            &execution,
            directory.path(),
            LocalRequest::ProviderHook {
                token: "a".repeat(HOOK_TOKEN_LEN),
                event: ProviderHookEvent::Stop,
                payload: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            stopped,
            LocalResponse::ProviderHookReply { ref reply }
                if *reply == serde_json::json!({})
        ));
        handle_request(
            &state,
            &execution,
            directory.path(),
            LocalRequest::ResetAgentBudget {
                project_id,
                agent_id,
            },
        )
        .await
        .unwrap();
        let third = handle_request(&state, &execution, directory.path(), hook())
            .await
            .unwrap();
        assert!(
            matches!(third, LocalResponse::ProviderHookReply { ref reply } if *reply == serde_json::json!({}))
        );
    }

    /// `stop_hook_reply`'s gated call site (`Stop`/`SubagentStop`): with
    /// `curie` marked deleting, a `Stop` hook -- with a task assigned and
    /// waiting, and the delivery slot free so `stop_hook_reply` could
    /// otherwise claim it -- must reply `{}` (nothing to deliver, from
    /// this hook's point of view) rather than block-replying the pending
    /// task, and must not open its run episode either. Verified by
    /// mutation the same way as the `UserPromptSubmit` test above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_hook_reply_declines_while_the_agent_is_deleting() {
        let directory = private_tempdir();
        let (state, execution, project_id, agent_id, task_id) = setup(directory.path()).await;

        execution.begin_delete(&agent_id).await.unwrap();

        let response = handle_request(
            &state,
            &execution,
            directory.path(),
            LocalRequest::ProviderHook {
                token: "a".repeat(HOOK_TOKEN_LEN),
                event: ProviderHookEvent::Stop,
                payload: serde_json::json!({}),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(
                &response,
                LocalResponse::ProviderHookReply { reply } if *reply == serde_json::json!({})
            ),
            "a declined gate must reply {{}} even with pending work, got {response:?}"
        );

        let task = state
            .with_store({
                let project_id = project_id.clone();
                let task_id = task_id.clone();
                move |store| store.get_task(&project_id, &task_id)
            })
            .await
            .unwrap();
        assert_eq!(
            task.snapshot.status,
            TaskStatus::Queued,
            "no run episode must open for a deleting agent's Stop hook"
        );

        execution.end_delete(&agent_id);
    }

    #[tokio::test]
    async fn repository_api_binds_live_tokens_and_redacts_audit_content() {
        use std::process::Command;
        fn git(cwd: &Path, args: &[&str]) {
            assert!(
                Command::new("git")
                    .current_dir(cwd)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let directory = private_tempdir();
        let repo = directory.path().join("repo");
        let remote = directory.path().join("remote.git");
        let work_a = directory.path().join("a");
        let work_b = directory.path().join("b");
        git(
            directory.path(),
            &["init", "--bare", remote.to_str().unwrap()],
        );
        git(
            directory.path(),
            &["init", "-b", "main", repo.to_str().unwrap()],
        );
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(repo.join("README"), "initial\n").unwrap();
        git(&repo, &["add", "README"]);
        git(&repo, &["commit", "-m", "initial"]);
        git(
            &repo,
            &["worktree", "add", "-b", "agent/a", work_a.to_str().unwrap()],
        );
        git(
            &repo,
            &["worktree", "add", "-b", "agent/b", work_b.to_str().unwrap()],
        );
        let project_id = ProjectId::try_from("project").unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Project".into(),
                    root: repo.to_string_lossy().into_owned(),
                },
                1,
            )
            .unwrap();
        let unconfigured_project_id = ProjectId::try_from("unconfigured-project").unwrap();
        let unconfigured_root = directory.path().join("unconfigured-repo");
        std::fs::create_dir(&unconfigured_root).unwrap();
        store
            .create_project(
                NewProject {
                    id: unconfigured_project_id.clone(),
                    name: "Unconfigured project".into(),
                    root: unconfigured_root.to_string_lossy().into_owned(),
                },
                1,
            )
            .unwrap();
        store
            .set_repository_authority(
                &project_id,
                &crate::store::RepositoryAuthority {
                    remote_url: remote.to_string_lossy().into_owned(),
                    base_branch: "main".into(),
                },
                2,
            )
            .unwrap();
        assert!(
            store
                .set_repository_authority(
                    &project_id,
                    &crate::store::RepositoryAuthority {
                        remote_url: "https://github.com/attacker/retarget.git".into(),
                        base_branch: "attacker".into(),
                    },
                    3,
                )
                .is_err(),
            "repository authority must be write-once"
        );
        for (name, worktree, token, session) in [
            ("a", &work_a, "a".repeat(64), "session-a"),
            ("b", &work_b, "b".repeat(64), "session-b"),
            ("stale", &work_a, "c".repeat(64), "session-stale"),
        ] {
            let agent_id = AgentId::try_from(name).unwrap();
            store
                .create_agent(
                    NewAgent {
                        id: agent_id.clone(),
                        project_id: project_id.clone(),
                        parent_agent_id: None,
                        role: AgentRole::Worker,
                        provider: Provider::Shell,
                    },
                    3,
                )
                .unwrap();
            store
                .create_session(
                    NewSession {
                        id: SessionId::try_from(session).unwrap(),
                        project_id: project_id.clone(),
                        agent_id,
                        provider: Provider::Shell,
                        runtime_model: None,
                        runtime_reasoning_effort: None,
                        runtime_permission_mode: None,
                        runtime_control_mode: None,
                        provider_session_id: None,
                        worktree: worktree.to_string_lossy().into_owned(),
                        codex_home: None,
                        hook_token: token,
                        runner_instance_id: RunnerInstanceId::try_from(format!("runner-{name}"))
                            .unwrap(),
                        runner_runtime: format!("/tmp/runner-{name}"),
                        runner_protocol_version: 1,
                    },
                    4,
                )
                .unwrap();
        }
        store
            .end_session(
                &SessionId::try_from("session-stale").unwrap(),
                Some(0),
                None,
                5,
            )
            .unwrap();
        assert!(
            matches!(
                store.set_repository_authority(
                    &unconfigured_project_id,
                    &crate::store::RepositoryAuthority {
                        remote_url: "https://github.com/attacker/poison.git".into(),
                        base_branch: "main".into(),
                    },
                    6,
                ),
                Err(crate::store::StoreError::RepositoryAuthorityRequiresIdleProject)
            ),
            "a live session in project A must block first-write poisoning of project B"
        );
        let state = ApiState::new(store);
        let secret = "PRIVATE_COMMIT_MESSAGE_SENTINEL";
        std::fs::write(work_a.join("a.txt"), "PRIVATE_DIFF_SENTINEL\n").unwrap();
        let response = repository_request(
            &state,
            "a".repeat(64),
            RepositoryRequest::Commit {
                message: secret.into(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(response, LocalResponse::GitOutput { ref operation, .. } if operation == "git_commit")
        );
        assert!(matches!(
            repository_request(&state, "c".repeat(64), RepositoryRequest::Status).await,
            Err(ApiFailure::Unauthorized(_))
        ));
        std::fs::write(work_b.join("b.txt"), "b\n").unwrap();
        repository_request(
            &state,
            "b".repeat(64),
            RepositoryRequest::Commit {
                message: "B commit".into(),
            },
        )
        .await
        .unwrap();
        assert!(
            !work_a.join("b.txt").exists(),
            "agent B token crossed into agent A worktree"
        );
        let events = state
            .with_store(|store| store.events_after(0, 100))
            .await
            .unwrap();
        let json = serde_json::to_string(&events).unwrap();
        assert!(!json.contains(secret));
        assert!(!json.contains("PRIVATE_DIFF_SENTINEL"));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, FactoryEvent::RepositoryOperation { .. }))
                .count(),
            4
        );
    }
}
