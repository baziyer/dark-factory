//! Versioned local control and persisted event stream.

use std::{
    fs,
    future::Future,
    io::{self, Read},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use factory_core::{
    AgentId, AgentSnapshot, MessageId, PROTOCOL_VERSION, ProjectId, ProjectSnapshot, RunId,
    RunSnapshot, TaskDetail, TaskId,
    local::{
        AgentDetail as LocalAgentDetail, AgentMessage as LocalAgentMessage,
        AgentProfile as LocalAgentProfile, ErrorCode, LocalRequest, LocalResponse,
        MAX_AGENT_MESSAGE_BYTES, MAX_AGENT_PAGE_ITEMS, MAX_EVENT_PAGE_ITEMS, MAX_LOCAL_FRAME_BYTES,
        MAX_PROJECT_PAGE_ITEMS, MAX_RUN_PAGE_ITEMS, MAX_TASK_BODY_BYTES, MAX_TASK_PAGE_ITEMS,
        MAX_TERMINAL_OUTPUT_BYTES, RequestEnvelope, RunTerminal, ServerFrame,
        SubscriptionFailureCategory as LocalSubscriptionFailureCategory,
        SubscriptionLimitWindow as LocalSubscriptionLimitWindow,
        SubscriptionProbeOutcome as LocalSubscriptionProbeOutcome,
        SubscriptionProviderStatus as LocalSubscriptionProviderStatus,
        SubscriptionSeverity as LocalSubscriptionSeverity,
        SubscriptionUsageStatus as LocalSubscriptionUsageStatus,
        SubscriptionUsageWindow as LocalSubscriptionUsageWindow,
    },
    runner::{
        MAX_RUNNER_FRAME_BYTES, MAX_RUNNER_SPOOL_BYTES, OutputStream, RunnerEvent,
        RunnerEventEnvelope,
    },
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
    execution::{self, StartTask},
    runner_client::{RunnerClient, RunnerClientError},
    store::{
        AgentMessage, NewAgent, NewAgentMessage, NewProject, NewTask, RunControlTarget, StoreError,
        SubscriptionFailureCategory, SubscriptionLimitWindow, SubscriptionProbe,
        SubscriptionProbeOutcome, SubscriptionSeverity, SubscriptionUsageWindow,
        UpdateAgentProfile,
    },
};

const EVENT_REPLAY_PAGE: usize = MAX_EVENT_PAGE_ITEMS as usize;
const MAX_CONNECTIONS: usize = 128;
const IO_TIMEOUT: Duration = Duration::from_secs(15);

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
            Self::Store(StoreError::InvalidSubscriptionProbe) => (
                ErrorCode::InvalidRequest,
                "subscription probe is invalid".into(),
            ),
            Self::Store(StoreError::InvalidAgentProfile) => (
                ErrorCode::InvalidRequest,
                "agent profile is invalid or exceeds its bound".into(),
            ),
            Self::Store(StoreError::InvalidAgentMessage) => (
                ErrorCode::InvalidRequest,
                "agent message is invalid or exceeds its bound".into(),
            ),
            Self::Store(StoreError::AgentNotFound) => (
                ErrorCode::NotFound,
                "agent was not found in the project".into(),
            ),
            Self::Store(StoreError::TaskNotFound) => (
                ErrorCode::NotFound,
                "task was not found in the project".into(),
            ),
            Self::Store(StoreError::RunNotFound) => (
                ErrorCode::NotFound,
                "run was not found in the project".into(),
            ),
            Self::Store(StoreError::TaskNotQueued) => (
                ErrorCode::Conflict,
                "task is not queued in the project".into(),
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
                "agent already has an active run".into(),
            ),
            Self::Store(StoreError::ParentRunLineageMismatch) => (
                ErrorCode::Conflict,
                "parent run does not match the agent hierarchy".into(),
            ),
            Self::Store(StoreError::CapacityReached { .. }) => (
                ErrorCode::Conflict,
                "active execution capacity has been reached".into(),
            ),
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
            execution::Error::InvalidWorktree | execution::Error::NonUtf8Path => {
                Self::Invalid("task worktree must be an existing UTF-8 directory".into())
            }
            execution::Error::StartBackpressure => {
                Self::Conflict("too many execution starts are in progress".into())
            }
            execution::Error::State(DaemonStateError::Store(
                error @ (StoreError::AgentNotFound
                | StoreError::AgentProviderMismatch
                | StoreError::TaskNotQueued
                | StoreError::AgentUnavailable
                | StoreError::ParentRunLineageMismatch
                | StoreError::CapacityReached { .. }),
            )) => Self::Store(error),
            _ => Self::Internal("execution manager could not accept the task".into()),
        }
    }
}

pub async fn serve<F>(
    listener: UnixListener,
    state: ApiState,
    execution: execution::Handle,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()>,
{
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
                let shutdown = stop_rx.clone();
                handlers.spawn(async move {
                    let _permit = permit;
                    if let Err(error) =
                        handle_connection(stream, state, execution, shutdown).await
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

    let envelope: RequestEnvelope = match serde_json::from_slice(&payload) {
        Ok(envelope) => envelope,
        Err(_) => {
            return write_response(
                &mut write,
                LocalResponse::Error {
                    code: ErrorCode::InvalidRequest,
                    message: "request is not valid local protocol JSON".into(),
                },
            )
            .await;
        }
    };
    if envelope.protocol_version != PROTOCOL_VERSION {
        return write_response(
            &mut write,
            LocalResponse::Error {
                code: ErrorCode::UnsupportedProtocol,
                message: format!(
                    "protocol {} is unsupported; this daemon speaks {}",
                    envelope.protocol_version, PROTOCOL_VERSION
                ),
            },
        )
        .await;
    }

    if let LocalRequest::Subscribe { after_sequence } = envelope.request {
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

    let response = handle_request(&state, &execution, envelope.request)
        .await
        .unwrap_or_else(ApiFailure::into_response);
    write_response(&mut write, response).await
}

async fn handle_request(
    state: &ApiState,
    execution: &execution::Handle,
    request: LocalRequest,
) -> Result<LocalResponse, ApiFailure> {
    match request {
        LocalRequest::Health => Ok(LocalResponse::Health),
        LocalRequest::CreateProject { id, name, root } => {
            let name = required_text("project name", name, 160)?;
            let root = canonical_root(root).await?;
            let project = state
                .commit_and_publish(move |store| {
                    let (project, event) =
                        store.create_project(NewProject { id, name, root }, now_ms()?)?;
                    Ok((project, vec![event]))
                })
                .await?;
            Ok(LocalResponse::ProjectCreated { project })
        }
        LocalRequest::ListProjects { after_id, limit } => {
            let limit = page_limit("project", limit, MAX_PROJECT_PAGE_ITEMS)?;
            let mut projects = state
                .with_store(move |store| store.list_projects(after_id.as_ref(), limit + 1))
                .await?;
            let next_after_id = next_project_cursor(&mut projects, limit);
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
        } => {
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
            let next_after_id = next_agent_cursor(&mut agents, limit);
            Ok(LocalResponse::Agents {
                agents,
                next_after_id,
            })
        }
        LocalRequest::GetAgent {
            project_id,
            agent_id,
        } => {
            let agent = state
                .with_store(move |store| store.get_agent_detail(&project_id, &agent_id))
                .await?;
            Ok(LocalResponse::Agent {
                agent: local_agent_detail(agent),
            })
        }
        LocalRequest::UpdateAgentProfile {
            project_id,
            agent_id,
            model,
            instructions,
            memory,
        } => {
            let agent = state
                .commit_and_publish(move |store| {
                    let (agent, event) = store.update_agent_profile(
                        &project_id,
                        &agent_id,
                        UpdateAgentProfile {
                            model,
                            instructions,
                            memory,
                        },
                        now_ms()?,
                    )?;
                    Ok((agent, vec![event]))
                })
                .await?;
            Ok(LocalResponse::AgentProfileUpdated {
                agent: local_agent_detail(agent),
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
            let next_after_id = next_message_cursor(&mut messages, limit);
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
            let next_after_id = next_task_cursor(&mut tasks, limit);
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
            let task = state
                .commit_and_publish(move |store| {
                    let (task, event) = store.retry_task(&project_id, &task_id, now_ms()?)?;
                    Ok((task, vec![event]))
                })
                .await?;
            Ok(LocalResponse::TaskRetried { task })
        }
        LocalRequest::AssignTask {
            project_id,
            task_id,
            agent_id,
        } => {
            let task = state
                .commit_and_publish(move |store| {
                    let (task, event) =
                        store.assign_task(&project_id, &task_id, agent_id.as_ref(), now_ms()?)?;
                    Ok((task, vec![event]))
                })
                .await?;
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
            let lookup_run_id = run_id.clone();
            let target = state
                .with_store(move |store| store.run_control_target(&project_id, &lookup_run_id))
                .await?;
            RunnerClient::new(
                &target.runner_runtime,
                run_id.clone(),
                target.runner_instance_id,
            )
            .stop(grace_ms)
            .await
            .map_err(runner_control_failure)?;
            Ok(LocalResponse::RunStopped { run_id })
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
            let next_after_id = next_run_cursor(&mut runs, limit);
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
        LocalRequest::SubscriptionUsage => {
            let snapshot = state
                .with_store(|store| store.subscription_usage_snapshot())
                .await?;
            Ok(LocalResponse::SubscriptionUsage {
                usage: LocalSubscriptionUsageStatus {
                    overall_severity: local_subscription_severity(snapshot.overall_severity),
                    providers: snapshot
                        .providers
                        .into_iter()
                        .map(|provider| LocalSubscriptionProviderStatus {
                            provider: provider.provider,
                            last_attempt_at_ms: provider.last_attempt_at_ms,
                            last_success_at_ms: provider.last_success_at_ms,
                            used_percent: provider.used_percent,
                            limit_window: provider.limit_window.map(local_subscription_window),
                            resets_at_ms: provider.resets_at_ms,
                            exhausted: provider.exhausted,
                            severity: local_subscription_severity(provider.severity),
                            consecutive_failures: provider.consecutive_failures,
                            windows: provider
                                .windows
                                .into_iter()
                                .map(|window| LocalSubscriptionUsageWindow {
                                    window: local_subscription_window(window.window),
                                    used_percent: window.used_percent,
                                    resets_at_ms: window.resets_at_ms,
                                    exhausted: window.exhausted,
                                })
                                .collect(),
                        })
                        .collect(),
                },
            })
        }
        LocalRequest::RecordSubscriptionProbe {
            project_id,
            orchestrator_agent_id,
            provider,
            attempted_at_ms,
            outcome,
            notification_task_id,
        } => {
            let outcome = store_subscription_outcome(outcome);
            let committed = state
                .commit_and_publish(move |store| {
                    let committed = store.record_subscription_probe(SubscriptionProbe {
                        project_id,
                        orchestrator_agent_id,
                        provider,
                        attempted_at_ms,
                        outcome,
                        notification_task_id,
                    })?;
                    let response = (
                        committed.state.provider,
                        local_subscription_severity(committed.state.severity),
                        committed.state.consecutive_failures,
                        committed.notification_created,
                    );
                    Ok((response, committed.events))
                })
                .await?;
            Ok(LocalResponse::SubscriptionProbeRecorded {
                provider: committed.0,
                severity: committed.1,
                consecutive_failures: committed.2,
                notification_created: committed.3,
            })
        }
        LocalRequest::Subscribe { .. } => unreachable!("subscriptions are handled per connection"),
    }
}

fn store_subscription_outcome(outcome: LocalSubscriptionProbeOutcome) -> SubscriptionProbeOutcome {
    match outcome {
        LocalSubscriptionProbeOutcome::Observed {
            used_percent,
            limit_window,
            resets_at_ms,
            exhausted,
            windows,
        } => SubscriptionProbeOutcome::Observed {
            used_percent,
            limit_window: match limit_window {
                LocalSubscriptionLimitWindow::Primary => SubscriptionLimitWindow::Primary,
                LocalSubscriptionLimitWindow::Secondary => SubscriptionLimitWindow::Secondary,
                LocalSubscriptionLimitWindow::CurrentSession => {
                    SubscriptionLimitWindow::CurrentSession
                }
                LocalSubscriptionLimitWindow::CurrentWeek => SubscriptionLimitWindow::CurrentWeek,
            },
            resets_at_ms,
            exhausted,
            windows: windows
                .into_iter()
                .map(|window| SubscriptionUsageWindow {
                    window: match window.window {
                        LocalSubscriptionLimitWindow::Primary => SubscriptionLimitWindow::Primary,
                        LocalSubscriptionLimitWindow::Secondary => {
                            SubscriptionLimitWindow::Secondary
                        }
                        LocalSubscriptionLimitWindow::CurrentSession => {
                            SubscriptionLimitWindow::CurrentSession
                        }
                        LocalSubscriptionLimitWindow::CurrentWeek => {
                            SubscriptionLimitWindow::CurrentWeek
                        }
                    },
                    used_percent: window.used_percent,
                    resets_at_ms: window.resets_at_ms,
                    exhausted: window.exhausted,
                })
                .collect(),
        },
        LocalSubscriptionProbeOutcome::Failed { category } => SubscriptionProbeOutcome::Failed {
            category: match category {
                LocalSubscriptionFailureCategory::Timeout => SubscriptionFailureCategory::Timeout,
                LocalSubscriptionFailureCategory::Protocol => SubscriptionFailureCategory::Protocol,
                LocalSubscriptionFailureCategory::Process => SubscriptionFailureCategory::Process,
                LocalSubscriptionFailureCategory::OutputLimit => {
                    SubscriptionFailureCategory::OutputLimit
                }
                LocalSubscriptionFailureCategory::Unavailable => {
                    SubscriptionFailureCategory::Unavailable
                }
            },
        },
    }
}

const fn local_subscription_severity(severity: SubscriptionSeverity) -> LocalSubscriptionSeverity {
    match severity {
        SubscriptionSeverity::Ok => LocalSubscriptionSeverity::Ok,
        SubscriptionSeverity::Warning => LocalSubscriptionSeverity::Warning,
        SubscriptionSeverity::Critical => LocalSubscriptionSeverity::Critical,
    }
}

const fn local_subscription_window(
    window: SubscriptionLimitWindow,
) -> LocalSubscriptionLimitWindow {
    match window {
        SubscriptionLimitWindow::Primary => LocalSubscriptionLimitWindow::Primary,
        SubscriptionLimitWindow::Secondary => LocalSubscriptionLimitWindow::Secondary,
        SubscriptionLimitWindow::CurrentSession => LocalSubscriptionLimitWindow::CurrentSession,
        SubscriptionLimitWindow::CurrentWeek => LocalSubscriptionLimitWindow::CurrentWeek,
    }
}

fn local_agent_detail(agent: crate::store::AgentDetail) -> LocalAgentDetail {
    LocalAgentDetail {
        snapshot: agent.snapshot,
        profile: LocalAgentProfile {
            model: agent.profile.model,
            instructions: agent.profile.instructions,
            memory: agent.profile.memory,
            updated_at_ms: agent.profile.updated_at_ms,
        },
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

fn runner_control_failure(error: RunnerClientError) -> ApiFailure {
    match error {
        RunnerClientError::RunnerRejected {
            code: factory_core::runner::RunnerErrorCode::Conflict,
        } => ApiFailure::Conflict("runner rejected the stop request".into()),
        RunnerClientError::InvalidStopGrace { found } => ApiFailure::Invalid(format!(
            "runner stop grace must be at most 60000 ms, got {found}"
        )),
        _ => ApiFailure::Internal("runner control request failed".into()),
    }
}

fn read_run_terminal(target: &RunControlTarget, run_id: RunId) -> Result<RunTerminal, ApiFailure> {
    let spool_path = PathBuf::from(&target.runner_runtime).join("events.ndjson");
    let file = match fs::File::open(spool_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RunTerminal {
                run_id,
                head_sequence: 0,
                output: String::new(),
                truncated: false,
            });
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
            Err(_) => {
                return Err(ApiFailure::Internal(
                    "runner terminal spool is invalid".into(),
                ));
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

fn next_project_cursor(projects: &mut Vec<ProjectSnapshot>, limit: usize) -> Option<ProjectId> {
    if projects.len() <= limit {
        return None;
    }
    projects.pop();
    projects.last().map(|project| project.id.clone())
}

fn next_task_cursor(tasks: &mut Vec<TaskDetail>, limit: usize) -> Option<TaskId> {
    if tasks.len() <= limit {
        return None;
    }
    tasks.pop();
    tasks.last().map(|task| task.snapshot.id.clone())
}

fn next_agent_cursor(agents: &mut Vec<AgentSnapshot>, limit: usize) -> Option<AgentId> {
    if agents.len() <= limit {
        return None;
    }
    agents.pop();
    agents.last().map(|agent| agent.id.clone())
}

fn next_run_cursor(runs: &mut Vec<RunSnapshot>, limit: usize) -> Option<RunId> {
    if runs.len() <= limit {
        return None;
    }
    runs.pop();
    runs.last().map(|run| run.id.clone())
}

fn next_message_cursor(messages: &mut Vec<LocalAgentMessage>, limit: usize) -> Option<MessageId> {
    if messages.len() <= limit {
        return None;
    }
    messages.pop();
    messages.last().map(|message| message.id.clone())
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
