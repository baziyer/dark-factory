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
    AgentId, PROTOCOL_VERSION, ProjectId, ProjectSnapshot, ProviderHookEvent, RunId, SessionId,
    local::{
        AgentDetail as LocalAgentDetail, AgentMessage as LocalAgentMessage,
        AgentProfile as LocalAgentProfile, ErrorCode, LocalRequest, LocalResponse,
        MAX_AGENT_MESSAGE_BYTES, MAX_AGENT_PAGE_ITEMS, MAX_EVENT_PAGE_ITEMS, MAX_LOCAL_FRAME_BYTES,
        MAX_PROJECT_PAGE_ITEMS, MAX_RUN_PAGE_ITEMS, MAX_SESSION_PAGE_ITEMS, MAX_TASK_BODY_BYTES,
        MAX_TASK_PAGE_ITEMS, MAX_TERMINAL_OUTPUT_BYTES, ProjectDetail as LocalProjectDetail,
        RequestEnvelope, RunTerminal, ServerFrame,
    },
    runner::{
        MAX_RUNNER_FRAME_BYTES, MAX_RUNNER_SPOOL_BYTES, OutputStream, RunnerErrorCode, RunnerEvent,
        RunnerEventEnvelope,
    },
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
    runner_client::{RunnerClient, RunnerClientError},
    store::{
        AgentMessage, NewAgent, NewAgentMessage, NewProject, NewTask, SessionControlTarget,
        StoreError, UpdateAgentProfile,
    },
};

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
            Self::Store(StoreError::InvalidAgentMessage) => (
                ErrorCode::InvalidRequest,
                "agent message is invalid or exceeds its bound".into(),
            ),
            Self::Store(StoreError::InvalidTaskResult) => (
                ErrorCode::InvalidRequest,
                "task result exceeds its bound".into(),
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
            Self::Store(StoreError::TaskNotFound) => (
                ErrorCode::NotFound,
                "task was not found in the project".into(),
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
                | StoreError::ProjectHasActiveRun
                | StoreError::RunNotStoppable),
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
            execution::Error::State(DaemonStateError::Store(
                error @ (StoreError::AgentNotFound
                | StoreError::TaskNotQueued
                | StoreError::TaskAssignmentMismatch
                | StoreError::AgentUnavailable
                | StoreError::SessionNotFound
                | StoreError::SessionNotLive),
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
    let envelope: RequestEnvelope = serde_json::from_slice(payload).map_err(|_| {
        Box::new(LocalResponse::Error {
            code: ErrorCode::InvalidRequest,
            message: "request is not valid local protocol JSON".into(),
        })
    })?;
    if envelope.protocol_version != PROTOCOL_VERSION {
        return Err(Box::new(LocalResponse::Error {
            code: ErrorCode::UnsupportedProtocol,
            message: format!(
                "protocol {} is unsupported; this daemon speaks {}",
                envelope.protocol_version, PROTOCOL_VERSION
            ),
        }));
    }
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
        LocalRequest::Health => Ok(LocalResponse::Health),
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
        } => {
            let title = required_text("task title", title, 240)?;
            if body.len() > MAX_TASK_BODY_BYTES {
                return Err(ApiFailure::Invalid(format!(
                    "task body must be at most {MAX_TASK_BODY_BYTES} bytes"
                )));
            }
            let task = state
                .commit_and_publish(move |store| {
                    let (task, event) = store.create_task(
                        NewTask {
                            id,
                            project_id,
                            parent_task_id,
                            title,
                            body,
                            priority,
                        },
                        now_ms()?,
                    )?;
                    Ok((task, vec![event]))
                })
                .await?;
            Ok(LocalResponse::TaskCreated { task })
        }
        LocalRequest::CreateAgent {
            id,
            project_id,
            parent_agent_id,
            role,
            provider,
            model,
            worktree,
        } => {
            let worktree = match worktree {
                Some(worktree) => Some(validate_agent_worktree(worktree).await?),
                None => None,
            };
            let created_project_id = project_id.clone();
            let created_agent_id = id.clone();
            let agent = state
                .commit_and_publish(move |store| {
                    let (agent, event) = store.create_agent_with_model(
                        NewAgent {
                            id,
                            project_id,
                            parent_agent_id,
                            role,
                            provider,
                        },
                        model,
                        now_ms()?,
                    )?;
                    Ok((agent, vec![event]))
                })
                .await?;
            let agent = if let Some(worktree) = worktree {
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
        } => {
            let lookup_project_id = project_id.clone();
            let lookup_agent_id = agent_id.clone();
            let agent = state
                .with_store(move |store| {
                    store.get_agent_detail(&lookup_project_id, &lookup_agent_id)
                })
                .await?;
            let agent_paths = AgentGuidancePaths::new(guidance_root, &project_id, &agent_id);
            let instructions = read_guidance_file(agent_paths.instructions.clone()).await?;
            let memory = read_guidance_file(agent_paths.memory.clone()).await?;
            Ok(LocalResponse::Agent {
                agent: local_agent_detail(agent, instructions, memory, agent_paths),
            })
        }
        LocalRequest::UpdateAgentProfile {
            project_id,
            agent_id,
            model,
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
                            permission_mode,
                        },
                        now_ms()?,
                    )?;
                    Ok((agent, vec![event]))
                })
                .await?;
            let agent_paths = AgentGuidancePaths::new(guidance_root, &project_id, &agent_id);
            write_guidance_file(agent_paths.instructions.clone(), instructions.clone()).await?;
            write_guidance_file(agent_paths.memory.clone(), memory.clone()).await?;
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
            limit,
        } => {
            let limit = page_limit("task", limit, MAX_TASK_PAGE_ITEMS)?;
            let mut tasks = state
                .with_store(move |store| {
                    store.list_tasks(&project_id, after_id.as_ref(), limit + 1)
                })
                .await?;
            let next_after_id = next_cursor(&mut tasks, limit, |task| task.snapshot.id.clone());
            Ok(LocalResponse::Tasks {
                tasks,
                next_after_id,
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
        } => {
            if title.is_none() && body.is_none() {
                return Err(ApiFailure::Invalid(
                    "task update must include title or body".into(),
                ));
            }
            let title = title
                .map(|title| required_text("task title", title, 240))
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
                    let (task, event) =
                        store.update_task(&project_id, &task_id, title, body, now_ms()?)?;
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
            state
                .commit_and_publish(move |store| {
                    let events = store.delete_agent(&project_id, &agent_id, now_ms()?)?;
                    Ok(((), events))
                })
                .await?;
            remove_agent_guidance(guidance_root, &response_project_id, &response_agent_id).await;
            Ok(LocalResponse::AgentDeleted {
                project_id: response_project_id,
                agent_id: response_agent_id,
            })
        }
        LocalRequest::DeleteProject { project_id } => {
            let response_project_id = project_id.clone();
            state
                .commit_and_publish(move |store| {
                    let event = store.delete_project(&project_id, now_ms()?)?;
                    Ok(((), vec![event]))
                })
                .await?;
            remove_project_guidance(guidance_root, &response_project_id).await;
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
            let target = state
                .with_store(move |store| {
                    store.run_control_target(&lookup_project_id, &lookup_run_id)
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
            let stop_project_id = project_id.clone();
            let stop_run_id = run_id.clone();
            state
                .commit_and_publish(move |store| {
                    let (run, event) =
                        store.request_run_stop(&stop_project_id, &stop_run_id, now_ms()?)?;
                    Ok((run, vec![event]))
                })
                .await?;
            Ok(LocalResponse::RunStopped { run_id })
        }
        LocalRequest::CancelRun { project_id, run_id } => {
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
            .stop(grace_ms)
            .await
            .map_err(|error| runner_control_failure(error, "stop"))?;
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
            let session_id = session.id;
            let (activity, inferred, wait_reason) = compute_hook_fields(event, &payload);
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
            let reply = if matches!(
                event,
                ProviderHookEvent::Stop | ProviderHookEvent::SubagentStop
            ) {
                let stop_hook_active = payload
                    .get("stop_hook_active")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                execution::stop_hook_reply(state, guidance_root, &updated_session, stop_hook_active)
                    .await?
            } else {
                serde_json::json!({})
            };
            if event == ProviderHookEvent::SessionStart {
                // Proof the CLI is up: an agent's very first delivery is
                // PTY-typed (idle-session path), which needs this hook to
                // have fired at least once before typing anything
                // (TRACK5-DESIGN.md §3); anything queued before the session
                // finished booting is picked up here rather than waiting
                // for the 5 second safety tick.
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

/// Best-effort recursive removal of one agent's guidance directory, run
/// after `DeleteAgent`'s transaction has already committed. The ledger row
/// is gone either way, so a filesystem failure here is logged and otherwise
/// ignored rather than surfaced to the caller.
async fn remove_agent_guidance(guidance_root: &Path, project_id: &ProjectId, agent_id: &AgentId) {
    let home = guidance_root.to_path_buf();
    let project_id = project_id.clone();
    let agent_id = agent_id.clone();
    let result =
        tokio::task::spawn_blocking(move || guidance::remove_agent(&home, &project_id, &agent_id))
            .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "failed to remove agent guidance directory after delete");
        }
        Err(error) => {
            tracing::warn!(%error, "guidance worker panicked while removing agent guidance directory");
        }
    }
}

/// Best-effort recursive removal of one project's guidance directory, run
/// after `DeleteProject`'s transaction has already committed. See
/// [`remove_agent_guidance`] for why failures are only logged.
async fn remove_project_guidance(guidance_root: &Path, project_id: &ProjectId) {
    let home = guidance_root.to_path_buf();
    let project_id = project_id.clone();
    let result =
        tokio::task::spawn_blocking(move || guidance::remove_project(&home, &project_id)).await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, "failed to remove project guidance directory after delete");
        }
        Err(error) => {
            tracing::warn!(%error, "guidance worker panicked while removing project guidance directory");
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
    // The store's generic state-page cap (`MAX_STATE_PAGE`, currently 101)
    // is smaller than the wire's advertised `MAX_SESSION_PAGE_ITEMS`; a
    // caller that omits `limit` gets a page that is guaranteed to fit
    // rather than the wire maximum.
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
            | RunnerEvent::Exited { .. } => {}
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
        | ApiFailure::Conflict(message)
        | ApiFailure::Internal(message) => message,
        ApiFailure::Store(error) => error.to_string(),
    })
}
