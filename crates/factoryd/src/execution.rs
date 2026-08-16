//! Codex-only execution ownership for the first vertical slice.

use std::{
    fs, io,
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use factory_core::{
    AgentId, ObserverHealth, ProjectId, Provider, RunFailureReason, RunId, RunnerInstanceId,
    TaskId,
    runner::{RunnerEvent, RunnerEventEnvelope},
};
use thiserror::Error;
use tokio::{
    process::Child,
    sync::{mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep_until, timeout_at},
};
use uuid::Uuid;

use crate::{
    daemon_state::{DaemonState, DaemonStateError},
    providers::codex::{
        self, CodexLaunch, Decoder, FailureReason as CodexFailureReason, Observation,
        Outcome as CodexOutcome, Session,
    },
    runner_client::{RunnerClient, RunnerClientError, RunnerStreamItem, RunnerSubscription},
    runner_process,
    store::{
        ExecutionTarget, MAX_RUNNER_BATCH_EVENTS, RecoverableExecution, RunReservation,
        RunnerEventEffects, RunnerEventInput, StoreError, TerminalOutcome, WriteDisposition,
    },
};

const COMMAND_CAPACITY: usize = 32;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(25);
const RECOVERY_RETRY_DELAY: Duration = Duration::from_millis(250);
const MAX_RECOVERY_RETRY_DELAY: Duration = Duration::from_secs(30);
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const CONTROL_SOCKET_FILE: &str = "control.sock";

/// Fixed process and durability bounds for the Codex execution actor.
pub struct Config {
    pub runner_program: PathBuf,
    pub codex_program: PathBuf,
    pub runtime_root: PathBuf,
    pub max_active_runs: usize,
    pub startup_timeout: Duration,
    pub connect_grace: Duration,
    pub batch_delay: Duration,
}

/// One explicit queued task assigned to one existing Codex agent.
pub struct StartCodex {
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub parent_run_id: Option<RunId>,
    pub worktree: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartedRun {
    pub run_id: RunId,
}

/// Bounded command handle for the Codex execution actor.
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::Sender<Command>,
}

impl Handle {
    /// Durably reserves one queued task for Codex execution.
    ///
    /// Success means the reservation and its public events are committed. The
    /// runner may still be starting; readiness and completion are observable
    /// through the durable run state.
    pub async fn start_codex(&self, input: StartCodex) -> Result<StartedRun, Error> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Start {
                input,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::ManagerStopped)?;
        reply_rx.await.map_err(|_| Error::ManagerStopped)?
    }

    /// Stops daemon-side observation without stopping or acknowledging runners.
    pub async fn shutdown(&self) -> Result<(), Error> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Shutdown { reply: reply_tx })
            .await
            .map_err(|_| Error::ManagerStopped)?;
        reply_rx.await.map_err(|_| Error::ManagerStopped)
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("execution concurrency must be greater than zero")]
    InvalidConcurrency,
    #[error("execution timing bounds must be greater than zero")]
    InvalidTiming,
    #[error("runner runtime root is not a private owner-only directory")]
    InvalidRuntimeRoot,
    #[error("task worktree is not an existing directory")]
    InvalidWorktree,
    #[error("execution path is not valid UTF-8")]
    NonUtf8Path,
    #[error("daemon state failed: {0}")]
    State(#[from] DaemonStateError),
    #[error("Codex session binding is invalid")]
    InvalidSession,
    #[error("runner launch failed: {0}")]
    Launch(#[from] runner_process::Error),
    #[error("runner control failed: {0}")]
    Runner(#[from] RunnerClientError),
    #[error("runner did not become reachable within its launch grace")]
    RunnerUnavailable,
    #[error("too many execution starts are in progress")]
    StartBackpressure,
    #[error("execution manager has stopped")]
    ManagerStopped,
    #[error("execution worker stopped unexpectedly")]
    WorkerStopped,
    #[error("system clock is before the Unix epoch")]
    InvalidClock,
    #[error("durable execution metadata is inconsistent")]
    CorruptExecution,
}

struct SharedConfig {
    runner_program: PathBuf,
    codex_program: PathBuf,
    runtime_root: PathBuf,
    max_active_runs: usize,
    startup_timeout: Duration,
    connect_grace: Duration,
    batch_delay: Duration,
}

enum Command {
    Start {
        input: StartCodex,
        reply: oneshot::Sender<Result<StartedRun, Error>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

struct StartWork {
    input: StartCodex,
    reply: oneshot::Sender<Result<StartedRun, Error>>,
}

struct WorkerCompletion {
    start: bool,
    result: Result<Option<RunContext>, Error>,
}

/// Starts the bounded Codex execution actor on the current Tokio runtime.
///
/// The actor recovers durable runner identities before accepting new work.
pub fn spawn(
    config: Config,
    state: DaemonState,
) -> Result<(Handle, JoinHandle<Result<(), Error>>), Error> {
    if config.max_active_runs == 0 {
        return Err(Error::InvalidConcurrency);
    }
    if config.startup_timeout.is_zero()
        || config.connect_grace.is_zero()
        || config.batch_delay.is_zero()
    {
        return Err(Error::InvalidTiming);
    }
    let runtime_root = prepare_runtime_root(&config.runtime_root)?;
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::ManagerStopped)?;
    let config = Arc::new(SharedConfig {
        runner_program: config.runner_program,
        codex_program: config.codex_program,
        runtime_root,
        max_active_runs: config.max_active_runs,
        startup_timeout: config.startup_timeout,
        connect_grace: config.connect_grace,
        batch_delay: config.batch_delay,
    });
    let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
    let join = runtime.spawn(run_actor(config, state, rx));
    Ok((Handle { tx }, join))
}

async fn run_actor(
    config: Arc<SharedConfig>,
    state: DaemonState,
    mut commands: mpsc::Receiver<Command>,
) -> Result<(), Error> {
    let recoverable = state
        .with_store(|store| store.recoverable_executions())
        .await?;
    let deferred_provider_runs = recoverable
        .iter()
        .filter(|run| run.provider != Provider::Codex)
        .count();
    if deferred_provider_runs > 0 {
        tracing::warn!(
            count = deferred_provider_runs,
            "recoverable non-Codex runs remain untouched"
        );
    }
    let recovery = recoverable
        .into_iter()
        .filter(|run| run.provider == Provider::Codex)
        .map(RecoveryWork::from_recoverable)
        .collect::<Vec<_>>();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks = JoinSet::new();
    for run in recovery {
        let task_config = Arc::clone(&config);
        let task_state = state.clone();
        let task_shutdown = shutdown_rx.clone();
        tasks.spawn(async move {
            WorkerCompletion {
                start: false,
                result: recover_run(task_config, task_state, run, task_shutdown)
                    .await
                    .map(|()| None),
            }
        });
    }
    let mut start_workers = 0usize;

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Start { input, reply }) if start_workers < COMMAND_CAPACITY => {
                    let task_config = Arc::clone(&config);
                    let task_state = state.clone();
                    let task_shutdown = shutdown_rx.clone();
                    tasks.spawn(async move {
                        WorkerCompletion {
                            start: true,
                            result: start_run(
                                task_config,
                                task_state,
                                StartWork { input, reply },
                                task_shutdown,
                            )
                            .await,
                        }
                    });
                    start_workers += 1;
                }
                Some(Command::Start { reply, .. }) => {
                    let _ = reply.send(Err(Error::StartBackpressure));
                }
                Some(Command::Shutdown { reply }) => {
                    let _ = shutdown_tx.send(true);
                    commands.close();
                    while let Some(command) = commands.recv().await {
                        if let Command::Start { reply, .. } = command {
                            let _ = reply.send(Err(Error::ManagerStopped));
                        }
                    }
                    while let Some(result) = tasks.join_next().await {
                        drop(result.map_err(|_| Error::WorkerStopped)?);
                    }
                    let _ = reply.send(());
                    return Ok(());
                }
                None => {
                    let _ = shutdown_tx.send(true);
                    while let Some(result) = tasks.join_next().await {
                        drop(result.map_err(|_| Error::WorkerStopped)?);
                    }
                    return Ok(());
                }
            },
            joined = tasks.join_next(), if !tasks.is_empty() => {
                let completion = joined.expect("non-empty JoinSet returned no task")
                    .map_err(|_| Error::WorkerStopped)?;
                if completion.start {
                    start_workers = start_workers.checked_sub(1).ok_or(Error::WorkerStopped)?;
                }
                match completion.result {
                    Ok(Some(context)) => {
                        let task_config = Arc::clone(&config);
                        let task_state = state.clone();
                        let mut task_shutdown = shutdown_rx.clone();
                        tasks.spawn(async move {
                            WorkerCompletion {
                                start: false,
                                result: observe_run(
                                    &task_config,
                                    &task_state,
                                    context,
                                    &mut task_shutdown,
                                )
                                .await
                                .map(|()| None),
                            }
                        });
                    }
                    Ok(None) => {}
                    Err(error) if manager_fatal(&error) => {
                        let _ = shutdown_tx.send(true);
                        tasks.abort_all();
                        while tasks.join_next().await.is_some() {}
                        return Err(error);
                    }
                    Err(error) => tracing::warn!(
                            failure = error_category(&error),
                            "Codex execution worker stopped without affecting peers"
                        ),
                }
            }
        }
    }
}

async fn start_run(
    config: Arc<SharedConfig>,
    state: DaemonState,
    work: StartWork,
    mut shutdown: watch::Receiver<bool>,
) -> Result<Option<RunContext>, Error> {
    let mut reply = Some(work.reply);
    let result = start_run_inner(&config, &state, work.input, &mut reply, &mut shutdown).await;
    match result {
        Ok(context) => Ok(context),
        Err(error) => {
            if let Some(reply) = reply.take() {
                let fatal = manager_fatal(&error);
                let _ = reply.send(Err(error));
                if fatal {
                    Err(Error::WorkerStopped)
                } else {
                    Ok(None)
                }
            } else {
                Err(error)
            }
        }
    }
}

async fn start_run_inner(
    config: &SharedConfig,
    state: &DaemonState,
    input: StartCodex,
    reply: &mut Option<oneshot::Sender<Result<StartedRun, Error>>>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Option<RunContext>, Error> {
    let worktree = canonical_worktree(&input.worktree)?;
    let run_id = new_id::<RunId>()?;
    let runner_instance_id = new_id::<RunnerInstanceId>()?;
    let runtime_dir = config.runtime_root.join(run_id.as_str());
    let worktree_text = path_text(&worktree)?;
    let runtime_text = path_text(&runtime_dir)?;
    let reservation = RunReservation {
        project_id: input.project_id,
        task_id: input.task_id,
        agent_id: input.agent_id,
        expected_provider: Provider::Codex,
        run_id: run_id.clone(),
        parent_run_id: input.parent_run_id,
        worktree: worktree_text,
        fresh_provider_session_id: None,
        runner_instance_id: runner_instance_id.clone(),
        runner_runtime: runtime_text,
    };
    let max_active_runs = config.max_active_runs;
    let reserved_at_ms = now_ms()?;
    let reserved = state
        .commit_and_publish(move |store| {
            let reserved = store.reserve_task_run(reservation, max_active_runs, reserved_at_ms)?;
            let crate::store::ReservedRun { target, events, .. } = reserved;
            Ok((target, events))
        })
        .await?;
    if let Some(reply) = reply.take() {
        let _ = reply.send(Ok(StartedRun {
            run_id: run_id.clone(),
        }));
    }

    if shutdown_requested(shutdown) {
        fail_launch(state, run_id.clone(), runner_instance_id.clone()).await?;
        return Ok(None);
    }

    let session = match launch_session(&reserved) {
        Ok(session) => session,
        Err(_) => {
            fail_launch(state, run_id.clone(), runner_instance_id.clone()).await?;
            return Ok(None);
        }
    };
    let watch_target = WatchTarget::from_execution_target(&reserved);
    let instructions = reserved.task_body;
    let prepared = codex::prepare(CodexLaunch {
        runner_program: config.runner_program.clone(),
        codex_program: config.codex_program.clone(),
        run_id: run_id.clone(),
        runner_instance_id: runner_instance_id.clone(),
        runtime_dir: runtime_dir.clone(),
        cwd: worktree,
        codex_home: reserved.codex_home.as_ref().map(PathBuf::from),
        instructions,
        session,
    })
    .map_err(|_| Error::InvalidSession);
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(_) => {
            fail_launch(state, run_id.clone(), runner_instance_id.clone()).await?;
            return Ok(None);
        }
    };

    let child =
        match runner_process::spawn_runner(prepared.launch_spec, config.startup_timeout).await {
            Ok(child) => child,
            Err(_) => {
                fail_launch(state, run_id.clone(), runner_instance_id.clone()).await?;
                return Ok(None);
            }
        };
    Ok(Some(RunContext {
        run_id,
        target: watch_target,
        runtime_dir,
        terminal_sequence: None,
        child: Some(child),
        observer_health: ObserverHealth::Unknown,
        absence_authoritative: true,
        launch_pending: true,
        retry_delay: RECOVERY_RETRY_DELAY,
    }))
}

async fn recover_run(
    config: Arc<SharedConfig>,
    state: DaemonState,
    run: RecoveryWork,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), Error> {
    observe_run(
        &config,
        &state,
        RunContext {
            run_id: run.run_id,
            target: run.target,
            runtime_dir: run.runtime_dir,
            terminal_sequence: run.terminal_sequence,
            child: None,
            observer_health: run.observer_health,
            absence_authoritative: run.observer_health == ObserverHealth::Healthy,
            launch_pending: false,
            retry_delay: RECOVERY_RETRY_DELAY,
        },
        &mut shutdown,
    )
    .await
}

struct RecoveryWork {
    run_id: RunId,
    target: WatchTarget,
    runtime_dir: PathBuf,
    terminal_sequence: Option<i64>,
    observer_health: ObserverHealth,
}

impl RecoveryWork {
    fn from_recoverable(run: RecoverableExecution) -> Self {
        Self {
            run_id: run.run_id,
            target: WatchTarget {
                provider_session_id: run.provider_session_id,
                resumes_provider_session: run.resumes_provider_session,
                runner_instance_id: run.runner_instance_id,
            },
            runtime_dir: PathBuf::from(run.runner_runtime),
            terminal_sequence: run.terminal_runner_sequence,
            observer_health: run.observer_health,
        }
    }
}

struct RunContext {
    run_id: RunId,
    target: WatchTarget,
    runtime_dir: PathBuf,
    terminal_sequence: Option<i64>,
    child: Option<Child>,
    observer_health: ObserverHealth,
    absence_authoritative: bool,
    launch_pending: bool,
    retry_delay: Duration,
}

impl RunContext {
    fn next_retry_delay(&mut self) -> Duration {
        let delay = self.retry_delay;
        self.retry_delay = delay
            .checked_mul(2)
            .unwrap_or(MAX_RECOVERY_RETRY_DELAY)
            .min(MAX_RECOVERY_RETRY_DELAY);
        delay
    }

    fn reset_retry_delay(&mut self) {
        self.retry_delay = RECOVERY_RETRY_DELAY;
    }
}

struct WatchTarget {
    provider_session_id: Option<String>,
    resumes_provider_session: bool,
    runner_instance_id: RunnerInstanceId,
}

impl WatchTarget {
    fn from_execution_target(target: &ExecutionTarget) -> Self {
        Self {
            provider_session_id: target.provider_session_id.clone(),
            resumes_provider_session: target.resumes_provider_session,
            runner_instance_id: target.runner_instance_id.clone(),
        }
    }
}

async fn observe_run(
    config: &SharedConfig,
    state: &DaemonState,
    mut context: RunContext,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), Error> {
    loop {
        match supervise_run(config, state, &mut context, shutdown).await {
            Ok(()) => return Ok(()),
            Err(error) if manager_fatal(&error) => return Err(error),
            Err(error) => {
                if distrusts_absence(&error) {
                    set_observer_health(state, &mut context, ObserverHealth::Degraded).await?;
                    context.absence_authoritative = false;
                }
                tracing::warn!(
                    run_id = %context.run_id,
                    failure = error_category(&error),
                    "Codex run observation will retry"
                );
                let retry_delay = context.next_retry_delay();
                tokio::select! {
                    _ = wait_for_shutdown(shutdown) => return Ok(()),
                    _ = tokio::time::sleep(retry_delay) => {}
                }
            }
        }
    }
}

async fn supervise_run(
    config: &SharedConfig,
    state: &DaemonState,
    context: &mut RunContext,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), Error> {
    let client = RunnerClient::new(
        &context.runtime_dir,
        context.run_id.clone(),
        context.target.runner_instance_id.clone(),
    );
    loop {
        if shutdown_requested(shutdown) {
            return Ok(());
        }
        if let Some(terminal_sequence) = context.terminal_sequence {
            if endpoint_absent(&context.runtime_dir)? {
                if !context.absence_authoritative {
                    return Err(Error::RunnerUnavailable);
                }
                if child_is_running(&mut context.child)? {
                    return Err(Error::RunnerUnavailable);
                }
                reconcile_terminal(state, context, terminal_sequence).await?;
                return Ok(());
            }
        }
        let active_subscription = match attach_with_grace(
            &client,
            &context.runtime_dir,
            config.connect_grace,
            shutdown,
        )
        .await?
        {
            Attach::Connected(subscription) => {
                context.launch_pending = false;
                subscription
            }
            Attach::Shutdown => return Ok(()),
            Attach::Missing => {
                if !context.absence_authoritative {
                    set_observer_health(state, context, ObserverHealth::Degraded).await?;
                    return Err(Error::RunnerUnavailable);
                }
                if let Some(terminal) = context.terminal_sequence {
                    if child_is_running(&mut context.child)? {
                        return Err(Error::RunnerUnavailable);
                    }
                    reconcile_terminal(state, context, terminal).await?;
                } else {
                    match poll_child(&mut context.child)? {
                        ChildState::Running => {
                            set_observer_health(state, context, ObserverHealth::Degraded).await?;
                            return Err(Error::RunnerUnavailable);
                        }
                        ChildState::ExitedBySignal => {
                            set_observer_health(state, context, ObserverHealth::Degraded).await?;
                            context.absence_authoritative = false;
                            return Err(Error::RunnerUnavailable);
                        }
                        ChildState::ExitedNormally | ChildState::Absent => {
                            if context.launch_pending {
                                fail_launch(
                                    state,
                                    context.run_id.clone(),
                                    context.target.runner_instance_id.clone(),
                                )
                                .await?;
                            } else {
                                fail_unverifiable(state, context).await?;
                            }
                        }
                    }
                }
                return Ok(());
            }
            Attach::Unreachable => {
                if !context.absence_authoritative && context.child.is_none() {
                    set_observer_health(state, context, ObserverHealth::Degraded).await?;
                    return Err(Error::RunnerUnavailable);
                }
                match poll_child(&mut context.child)? {
                    ChildState::ExitedBySignal => {
                        set_observer_health(state, context, ObserverHealth::Degraded).await?;
                        context.absence_authoritative = false;
                    }
                    ChildState::ExitedNormally if context.launch_pending => {
                        fail_launch(
                            state,
                            context.run_id.clone(),
                            context.target.runner_instance_id.clone(),
                        )
                        .await?;
                        return Ok(());
                    }
                    ChildState::Running | ChildState::ExitedNormally => {
                        set_observer_health(state, context, ObserverHealth::Degraded).await?;
                    }
                    ChildState::Absent => {
                        set_observer_health(state, context, ObserverHealth::Degraded).await?;
                        context.absence_authoritative = false;
                    }
                }
                return Err(Error::RunnerUnavailable);
            }
        };
        match consume_subscription(
            config,
            state,
            &client,
            context,
            active_subscription,
            shutdown,
        )
        .await?
        {
            Consume::Finished => return Ok(()),
            Consume::Shutdown => return Ok(()),
            Consume::Reconnect { terminal_sequence } => {
                context.terminal_sequence = terminal_sequence;
                set_observer_health(state, context, ObserverHealth::Degraded).await?;
                context.absence_authoritative = false;
                let retry_delay = context.next_retry_delay();
                tokio::select! {
                    _ = wait_for_shutdown(shutdown) => return Ok(()),
                    _ = tokio::time::sleep(retry_delay) => {}
                }
            }
        }
    }
}

enum Consume {
    Finished,
    Shutdown,
    Reconnect { terminal_sequence: Option<i64> },
}

async fn consume_subscription(
    config: &SharedConfig,
    state: &DaemonState,
    client: &RunnerClient,
    context: &mut RunContext,
    mut subscription: RunnerSubscription,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Consume, Error> {
    let mut decoder = Some(decoder_for(&context.target)?);
    let mut pending = Vec::new();
    let mut deadline = None;
    let mut caught_up = false;
    let mut terminal_sequence = context.terminal_sequence;
    let mut session_seen = false;

    loop {
        let read = next_stream_item(&mut subscription, shutdown, deadline).await;
        let item = match read {
            StreamRead::Shutdown => return Ok(Consume::Shutdown),
            StreamRead::Flush => {
                flush_pending(state, context, &mut pending).await?;
                deadline = None;
                continue;
            }
            StreamRead::Item(Ok(item)) => item,
            StreamRead::Item(Err(error)) if reconnectable(&error) => {
                return Ok(Consume::Reconnect { terminal_sequence });
            }
            StreamRead::Item(Err(error)) => return Err(Error::Runner(error)),
        };

        match item {
            RunnerStreamItem::CaughtUp { .. } => {
                flush_pending(state, context, &mut pending).await?;
                deadline = None;
                set_observer_health(state, context, ObserverHealth::Healthy).await?;
                context.absence_authoritative = true;
                caught_up = true;
                if let Some(terminal) = terminal_sequence {
                    acknowledge_and_reconcile(client, state, context, terminal, shutdown).await?;
                    context.child = None;
                    return Ok(Consume::Finished);
                }
            }
            RunnerStreamItem::Event(event) => {
                if caught_up {
                    context.reset_retry_delay();
                }
                if deadline.is_none() {
                    deadline = Some(Instant::now() + config.batch_delay);
                }
                let is_terminal = terminal_event(&event.event);
                let effects = normalize_effects(
                    decoder.as_mut().ok_or(Error::CorruptExecution)?,
                    &event,
                    &mut session_seen,
                );
                if is_terminal {
                    let outcome = finish_outcome(
                        decoder.take().ok_or(Error::CorruptExecution)?,
                        session_seen,
                    );
                    pending.push(RunnerEventInput {
                        event: event.clone(),
                        effects: RunnerEventEffects {
                            confirmed_provider_session_id: effects.confirmed_provider_session_id,
                            terminal_outcome: Some(outcome),
                        },
                    });
                    terminal_sequence = Some(event.sequence);
                    context.terminal_sequence = Some(event.sequence);
                    flush_pending(state, context, &mut pending).await?;
                    deadline = None;
                    if caught_up {
                        acknowledge_and_reconcile(client, state, context, event.sequence, shutdown)
                            .await?;
                        context.child = None;
                        return Ok(Consume::Finished);
                    }
                } else {
                    pending.push(RunnerEventInput { event, effects });
                    if pending.len() == MAX_RUNNER_BATCH_EVENTS {
                        flush_pending(state, context, &mut pending).await?;
                        deadline = None;
                    }
                }
            }
        }
    }
}

fn normalize_effects(
    decoder: &mut Decoder,
    event: &RunnerEventEnvelope,
    session_seen: &mut bool,
) -> RunnerEventEffects {
    let observations = decoder.push(&event.event);
    let confirmed = observations
        .into_iter()
        .find_map(|observation| match observation {
            Observation::ThreadStarted { thread_id } => Some(thread_id),
            _ => None,
        });
    if confirmed.is_some() {
        *session_seen = true;
    }
    RunnerEventEffects {
        confirmed_provider_session_id: confirmed,
        terminal_outcome: None,
    }
}

fn finish_outcome(decoder: Decoder, session_seen: bool) -> TerminalOutcome {
    match decoder.finish().outcome {
        CodexOutcome::Succeeded { .. } if session_seen => TerminalOutcome::Succeeded,
        CodexOutcome::Succeeded { .. } => TerminalOutcome::Failed(RunFailureReason::Protocol),
        CodexOutcome::Failed { reason, .. } => TerminalOutcome::Failed(match reason {
            CodexFailureReason::Protocol => RunFailureReason::Protocol,
            CodexFailureReason::Provider => RunFailureReason::Provider,
            CodexFailureReason::Process => RunFailureReason::Process,
            CodexFailureReason::Spawn => RunFailureReason::Spawn,
            CodexFailureReason::Incomplete => RunFailureReason::Incomplete,
        }),
    }
}

async fn flush_pending(
    state: &DaemonState,
    context: &RunContext,
    pending: &mut Vec<RunnerEventInput>,
) -> Result<(), Error> {
    if pending.is_empty() {
        return Ok(());
    }
    let items = std::mem::take(pending);
    let run_id = context.run_id.clone();
    let runner_instance_id = context.target.runner_instance_id.clone();
    let committed_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let result =
                store.ingest_runner_events(&run_id, &runner_instance_id, items, committed_at_ms)?;
            Ok(((), result.events))
        })
        .await?;
    Ok(())
}

enum StreamRead {
    Item(Result<RunnerStreamItem, RunnerClientError>),
    Flush,
    Shutdown,
}

async fn next_stream_item(
    subscription: &mut RunnerSubscription,
    shutdown: &mut watch::Receiver<bool>,
    deadline: Option<Instant>,
) -> StreamRead {
    if shutdown_requested(shutdown) {
        return StreamRead::Shutdown;
    }
    match deadline {
        Some(deadline) => tokio::select! {
            _ = wait_for_shutdown(shutdown) => StreamRead::Shutdown,
            result = timeout_at(deadline, subscription.next_item()) => match result {
                Ok(item) => StreamRead::Item(item),
                Err(_) => StreamRead::Flush,
            },
        },
        None => tokio::select! {
            _ = wait_for_shutdown(shutdown) => StreamRead::Shutdown,
            item = subscription.next_item() => StreamRead::Item(item),
        },
    }
}

enum Attach {
    Connected(RunnerSubscription),
    Missing,
    Unreachable,
    Shutdown,
}

async fn attach_with_grace(
    client: &RunnerClient,
    runtime_dir: &Path,
    grace: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Attach, Error> {
    let deadline = Instant::now() + grace;
    loop {
        if shutdown_requested(shutdown) {
            return Ok(Attach::Shutdown);
        }
        let attempted = tokio::select! {
            _ = wait_for_shutdown(shutdown) => return Ok(Attach::Shutdown),
            result = timeout_at(deadline, client.subscribe()) => result,
        };
        match attempted {
            Ok(Ok(subscription)) => return Ok(Attach::Connected(subscription)),
            Ok(Err(error)) if unavailable(&error) => {}
            Ok(Err(error)) => return Err(Error::Runner(error)),
            Err(_) => return Ok(Attach::Unreachable),
        }
        if Instant::now() >= deadline {
            return classify_unavailable(runtime_dir);
        }
        let wake = (Instant::now() + CONNECT_RETRY_DELAY).min(deadline);
        tokio::select! {
            _ = wait_for_shutdown(shutdown) => return Ok(Attach::Shutdown),
            _ = sleep_until(wake) => {}
        }
    }
}

async fn acknowledge_and_reconcile(
    client: &RunnerClient,
    state: &DaemonState,
    context: &mut RunContext,
    terminal_sequence: i64,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), Error> {
    loop {
        let acknowledgement = tokio::select! {
            _ = wait_for_shutdown(shutdown) => return Ok(()),
            result = client.acknowledge_exit(terminal_sequence) => result,
        };
        match acknowledgement {
            Ok(()) => {
                reconcile_terminal(state, context, terminal_sequence).await?;
                return Ok(());
            }
            Err(error) if reconnectable(&error) => {
                if context.absence_authoritative && endpoint_absent(&context.runtime_dir)? {
                    reconcile_terminal(state, context, terminal_sequence).await?;
                    return Ok(());
                }
            }
            Err(error) => return Err(Error::Runner(error)),
        }
        let retry_delay = context.next_retry_delay();
        tokio::select! {
            _ = wait_for_shutdown(shutdown) => return Ok(()),
            _ = tokio::time::sleep(retry_delay) => {}
        }
    }
}

async fn reconcile_terminal(
    state: &DaemonState,
    context: &RunContext,
    terminal_sequence: i64,
) -> Result<(), Error> {
    let run_id = context.run_id.clone();
    let runner_instance_id = context.target.runner_instance_id.clone();
    let reconciled_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let disposition = store.mark_runner_terminal_reconciled(
                &run_id,
                &runner_instance_id,
                terminal_sequence,
                reconciled_at_ms,
            )?;
            Ok((disposition, Vec::new()))
        })
        .await?;
    Ok(())
}

async fn fail_unverifiable(state: &DaemonState, context: &RunContext) -> Result<(), Error> {
    fail_unverifiable_identity(
        state,
        context.run_id.clone(),
        context.target.runner_instance_id.clone(),
    )
    .await
}

async fn fail_unverifiable_identity(
    state: &DaemonState,
    run_id: RunId,
    runner_instance_id: RunnerInstanceId,
) -> Result<(), Error> {
    let failed_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let transition =
                store.fail_run_unverifiable(&run_id, &runner_instance_id, failed_at_ms)?;
            Ok((transition.disposition, transition.events))
        })
        .await?;
    Ok(())
}

async fn set_observer_health(
    state: &DaemonState,
    context: &mut RunContext,
    health: ObserverHealth,
) -> Result<(), Error> {
    if context.observer_health == health {
        return Ok(());
    }
    let run_id = context.run_id.clone();
    let runner_instance_id = context.target.runner_instance_id.clone();
    let changed_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let transition =
                store.set_observer_health(&run_id, &runner_instance_id, health, changed_at_ms)?;
            Ok((transition.disposition, transition.events))
        })
        .await?;
    context.observer_health = health;
    Ok(())
}

async fn fail_launch(
    state: &DaemonState,
    run_id: RunId,
    runner_instance_id: RunnerInstanceId,
) -> Result<WriteDisposition, Error> {
    let failed_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let transition = store.fail_run_launch(&run_id, &runner_instance_id, failed_at_ms)?;
            Ok((transition.disposition, transition.events))
        })
        .await
        .map_err(Error::State)
}

fn child_is_running(child: &mut Option<Child>) -> Result<bool, Error> {
    Ok(matches!(poll_child(child)?, ChildState::Running))
}

enum ChildState {
    Absent,
    Running,
    ExitedNormally,
    ExitedBySignal,
}

fn poll_child(child: &mut Option<Child>) -> Result<ChildState, Error> {
    let Some(process) = child.as_mut() else {
        return Ok(ChildState::Absent);
    };
    let status = process.try_wait().map_err(|_| Error::RunnerUnavailable)?;
    let Some(status) = status else {
        return Ok(ChildState::Running);
    };
    *child = None;
    if status.signal().is_some() {
        Ok(ChildState::ExitedBySignal)
    } else {
        Ok(ChildState::ExitedNormally)
    }
}

fn decoder_for(target: &WatchTarget) -> Result<Decoder, Error> {
    if target.resumes_provider_session {
        let session = target
            .provider_session_id
            .clone()
            .ok_or(Error::CorruptExecution)?;
        Decoder::resume(session).map_err(|_| Error::InvalidSession)
    } else {
        Ok(Decoder::fresh())
    }
}

fn launch_session(target: &ExecutionTarget) -> Result<Session, Error> {
    if target.resumes_provider_session {
        Ok(Session::Resume {
            thread_id: target
                .provider_session_id
                .clone()
                .ok_or(Error::CorruptExecution)?,
        })
    } else {
        Ok(Session::New)
    }
}

fn terminal_event(event: &RunnerEvent) -> bool {
    matches!(
        event,
        RunnerEvent::SpawnFailed { .. } | RunnerEvent::Exited { .. }
    )
}

fn manager_fatal(error: &Error) -> bool {
    match error {
        Error::State(DaemonStateError::Store(error)) => matches!(
            error,
            StoreError::Sqlite(_)
                | StoreError::Json(_)
                | StoreError::UnsupportedSchema { .. }
                | StoreError::InvalidSchemaVersion(_)
                | StoreError::CorruptProtocolVersion(_)
                | StoreError::MissingEventKind
                | StoreError::EventSequenceGap { .. }
                | StoreError::CorruptRunnerSequence(_)
        ),
        Error::State(DaemonStateError::StoreLockPoisoned | DaemonStateError::StoreWorkerFailed)
        | Error::WorkerStopped
        | Error::InvalidClock => true,
        _ => false,
    }
}

fn distrusts_absence(error: &Error) -> bool {
    matches!(error, Error::Runner(_) | Error::CorruptExecution)
}

fn error_category(error: &Error) -> &'static str {
    match error {
        Error::InvalidConcurrency => "invalid_concurrency",
        Error::InvalidTiming => "invalid_timing",
        Error::InvalidRuntimeRoot => "invalid_runtime_root",
        Error::InvalidWorktree => "invalid_worktree",
        Error::NonUtf8Path => "non_utf8_path",
        Error::State(_) => "state",
        Error::InvalidSession => "invalid_session",
        Error::Launch(_) => "launch",
        Error::Runner(_) => "runner_protocol",
        Error::RunnerUnavailable => "runner_unavailable",
        Error::StartBackpressure => "start_backpressure",
        Error::ManagerStopped => "manager_stopped",
        Error::WorkerStopped => "worker_stopped",
        Error::InvalidClock => "invalid_clock",
        Error::CorruptExecution => "corrupt_execution",
    }
}

fn reconnectable(error: &RunnerClientError) -> bool {
    matches!(
        error,
        RunnerClientError::Io(source) if retryable_io_kind(source.kind())
    ) || matches!(
        error,
        RunnerClientError::UnexpectedEof
            | RunnerClientError::UnterminatedFrame
            | RunnerClientError::TimedOut { .. }
    )
}

fn retryable_io_kind(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::TimedOut
    )
}

fn unavailable(error: &RunnerClientError) -> bool {
    matches!(
        error,
        RunnerClientError::Io(source)
            if matches!(source.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused)
    )
}

fn classify_unavailable(runtime_dir: &Path) -> Result<Attach, Error> {
    if endpoint_absent(runtime_dir)? {
        Ok(Attach::Missing)
    } else {
        Ok(Attach::Unreachable)
    }
}

fn endpoint_absent(runtime_dir: &Path) -> Result<bool, Error> {
    let runtime = match fs::symlink_metadata(runtime_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(_) => return Err(Error::CorruptExecution),
    };
    if runtime.file_type().is_symlink()
        || !runtime.is_dir()
        || runtime.uid() != rustix::process::geteuid().as_raw()
        || runtime.mode() & 0o777 != PRIVATE_DIRECTORY_MODE
    {
        return Err(Error::CorruptExecution);
    }
    let socket = match fs::symlink_metadata(runtime_dir.join(CONTROL_SOCKET_FILE)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(_) => return Err(Error::CorruptExecution),
    };
    if !socket.file_type().is_socket()
        || socket.uid() != rustix::process::geteuid().as_raw()
        || socket.mode() & 0o777 != 0o600
    {
        return Err(Error::CorruptExecution);
    }
    Ok(false)
}

fn shutdown_requested(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    let _ = shutdown.wait_for(|requested| *requested).await;
}

fn new_id<T>() -> Result<T, Error>
where
    T: TryFrom<String>,
{
    T::try_from(Uuid::new_v4().hyphenated().to_string()).map_err(|_| Error::CorruptExecution)
}

fn now_ms() -> Result<i64, Error> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::InvalidClock)?
        .as_millis();
    i64::try_from(millis).map_err(|_| Error::InvalidClock)
}

fn canonical_worktree(path: &Path) -> Result<PathBuf, Error> {
    let canonical = fs::canonicalize(path).map_err(|_| Error::InvalidWorktree)?;
    let metadata = fs::metadata(&canonical).map_err(|_| Error::InvalidWorktree)?;
    if metadata.is_dir() {
        Ok(canonical)
    } else {
        Err(Error::InvalidWorktree)
    }
}

fn prepare_runtime_root(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() {
        return Err(Error::InvalidRuntimeRoot);
    }
    let parent = path.parent().ok_or(Error::InvalidRuntimeRoot)?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) => verify_private_directory_metadata(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(PRIVATE_DIRECTORY_MODE)
                .create(parent)
                .map_err(|_| Error::InvalidRuntimeRoot)?;
            verify_private_directory(parent)?;
        }
        Err(_) => return Err(Error::InvalidRuntimeRoot),
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => verify_private_directory_metadata(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(PRIVATE_DIRECTORY_MODE)
                .create(path)
                .map_err(|_| Error::InvalidRuntimeRoot)?;
            let metadata = fs::symlink_metadata(path).map_err(|_| Error::InvalidRuntimeRoot)?;
            verify_private_directory_metadata(&metadata)?;
        }
        Err(_) => return Err(Error::InvalidRuntimeRoot),
    }
    fs::canonicalize(path).map_err(|_| Error::InvalidRuntimeRoot)
}

fn verify_private_directory(path: &Path) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path).map_err(|_| Error::InvalidRuntimeRoot)?;
    verify_private_directory_metadata(&metadata)
}

fn verify_private_directory_metadata(metadata: &fs::Metadata) -> Result<(), Error> {
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != PRIVATE_DIRECTORY_MODE
    {
        return Err(Error::InvalidRuntimeRoot);
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<String, Error> {
    path.to_str().map(str::to_owned).ok_or(Error::NonUtf8Path)
}
