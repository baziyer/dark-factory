//! Stable, provider-blind ownership wrapper for one attempt process.

use std::{
    collections::HashMap,
    future::pending,
    io,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use factory_core::{
    RunId, RunnerInstanceId,
    runner::{
        MAX_RUNNER_ERROR_BYTES, MAX_RUNNER_FRAME_BYTES, MAX_STARTUP_STDIN_BYTES,
        RUNNER_PROTOCOL_VERSION, RequestEnvelope, RunnerErrorCode, RunnerEvent,
        RunnerEventEnvelope, RunnerFrame, RunnerRequest,
    },
};
use rustix::process::{Pid, Signal, getpgid, kill_process_group, test_kill_process_group};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::{UnixListener, UnixStream, unix::OwnedWriteHalf},
    process::Command,
    sync::{Mutex, broadcast, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, Sleep, sleep, timeout},
};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_STOP_GRACE: Duration = Duration::from_secs(60);
/// Grace before escalating a process-group `TERM` to `KILL`.
const DEFAULT_GROUP_GRACE: Duration = Duration::from_secs(2);
const POST_KILL_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Grace the group cleanup (the pass that reaps anything still holding the
/// process group after the leader itself has already exited) gives a
/// straggler *only when a `Stop` or runner shutdown was actually requested*
/// during the leader's lifetime: that request's own grace/kill cycle
/// (`DEFAULT_GROUP_GRACE`, or the caller's `grace_ms`) has already run its
/// course by the time cleanup runs, so a straggler here is a race artifact
/// being mopped up, not a workload owed a second multi-second courtesy.
///
/// When the leader instead exited entirely on its own — no `Stop` was ever
/// requested — this grace does *not* apply; the cleanup uses
/// `DEFAULT_GROUP_GRACE` instead, because that TERM is the straggler's
/// first and only grace period, not a mop-up of one already spent.
///
/// Either way, the cleanup now polls (`GROUP_POLL_INTERVAL`) instead of
/// blind-waiting the full grace regardless of whether the group already
/// emptied out. See #55: an unconditional `DEFAULT_GROUP_GRACE` blind wait
/// here, stacked with `POST_KILL_DRAIN_TIMEOUT`, pushed a run's worst-case
/// stop-to-`Exited` latency to ~7.2-7.7s against `tests/runner.rs`'s and
/// `tests/terminal.rs`'s fixed 8s deadline; a deterministic repro
/// (`natural_leader_exit_terminates_a_descendant_that_retains_output_pipes`)
/// measured ~2.05s pre-fix vs. ~0.2-0.3s post-fix for a well-behaved
/// straggler reaped via the (still 2s-capped, but now polled) natural-exit path.
const GROUP_CLEANUP_GRACE: Duration = Duration::from_millis(200);
/// How often the group cleanup re-checks whether the group has already
/// emptied out, so it can move on immediately instead of always waiting out
/// the full grace it was given.
const GROUP_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_COMMAND_ID_BYTES: usize = 128;
const BROADCAST_CAPACITY: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid runner arguments: {0}")]
    InvalidArguments(String),
    #[error("runner I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("runner event serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("runner task failed: {0}")]
    Task(String),
}

pub struct Config {
    pub run_id: RunId,
    pub runner_instance_id: RunnerInstanceId,
    pub runtime_dir: PathBuf,
    pub cwd: PathBuf,
    pub startup_input: Option<Vec<u8>>,
    pub program: PathBuf,
    pub arguments: Vec<String>,
    /// The already validated factory-runner executable used for the hidden
    /// same-PID exec gate. This is deliberately the runner itself, not a
    /// shell wrapper or a second installed artifact.
    pub exec_gate_program: PathBuf,
}

struct PreparedRuntime {
    listener: UnixListener,
    log: Arc<EventLog>,
    socket_path: PathBuf,
}

struct EventLog {
    spool_path: PathBuf,
    inner: Mutex<LogInner>,
    events: broadcast::Sender<RunnerEventEnvelope>,
}

struct LogInner {
    file: BufWriter<File>,
    head: i64,
    terminal_sequence: Option<i64>,
}

#[derive(Clone, Copy)]
struct LogSnapshot {
    head: i64,
    terminal_sequence: Option<i64>,
}

struct RuntimeState {
    run_id: RunId,
    runner_instance_id: RunnerInstanceId,
    log: Arc<EventLog>,
    stop_tx: mpsc::Sender<StopCommand>,
    prepare_tx: mpsc::Sender<PrepareCommand>,
    prepare_claimed: Mutex<bool>,
    accepted_stops: Mutex<HashMap<String, u64>>,
    shutdown_tx: watch::Sender<bool>,
}

struct PrepareCommand {
    prepared: oneshot::Sender<Result<ProcessIdentity, ControlError>>,
    activation: oneshot::Receiver<()>,
}

#[derive(Clone, Copy)]
struct ProcessIdentity {
    child_pid: u32,
    process_group_id: u32,
}

struct StopCommand {
    grace: Duration,
    response: oneshot::Sender<Result<(), ControlError>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SupervisionOutcome {
    AwaitAcknowledgement,
    RunnerSignalled,
}

struct ProcessGroupGuard {
    pid: Pid,
    armed: bool,
}

impl ProcessGroupGuard {
    fn new(pid: Pid) -> Self {
        Self { pid, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = kill_process_group(self.pid, Signal::KILL);
        }
    }
}

struct PipeDrainTask {
    handle: JoinHandle<Result<(), Error>>,
    finished: bool,
}

impl PipeDrainTask {
    fn new(handle: JoinHandle<Result<(), Error>>) -> Self {
        Self {
            handle,
            finished: false,
        }
    }

    fn finish(
        &mut self,
        result: Result<Result<(), Error>, tokio::task::JoinError>,
    ) -> Result<(), Error> {
        self.finished = true;
        join_drain(result)
    }

    async fn join(&mut self) -> Result<(), Error> {
        if self.finished {
            return Ok(());
        }
        let result = (&mut self.handle).await;
        self.finish(result)
    }

    async fn abort(&mut self) {
        if !self.finished {
            self.handle.abort();
            let _ = (&mut self.handle).await;
            self.finished = true;
        }
    }
}

impl Drop for PipeDrainTask {
    fn drop(&mut self) {
        if !self.finished {
            self.handle.abort();
        }
    }
}

#[derive(Debug)]
struct ControlError {
    code: RunnerErrorCode,
    message: String,
}

impl ControlError {
    fn new(code: RunnerErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: truncate_utf8(message.into(), MAX_RUNNER_ERROR_BYTES),
        }
    }
}

impl EventLog {
    fn new(spool_path: PathBuf, file: File) -> Arc<Self> {
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        Arc::new(Self {
            spool_path,
            inner: Mutex::new(LogInner {
                file: BufWriter::new(file),
                head: 0,
                terminal_sequence: None,
            }),
            events,
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<RunnerEventEnvelope> {
        self.events.subscribe()
    }

    async fn snapshot(&self) -> LogSnapshot {
        let inner = self.inner.lock().await;
        LogSnapshot {
            head: inner.head,
            terminal_sequence: inner.terminal_sequence,
        }
    }

    async fn append_lifecycle(&self, event: RunnerEvent, terminal: bool) -> Result<i64, Error> {
        let published = {
            let mut inner = self.inner.lock().await;
            if inner.terminal_sequence.is_some() {
                return Err(Error::Task(
                    "attempted to append a second terminal event".into(),
                ));
            }
            let envelope = next_envelope(&inner, event);
            let envelope = append_encoded(&mut inner, envelope).await?;
            if terminal {
                inner.terminal_sequence = Some(envelope.sequence);
            }
            envelope
        };
        let sequence = published.sequence;
        let _ = self.events.send(published);
        Ok(sequence)
    }
}

fn next_envelope(inner: &LogInner, event: RunnerEvent) -> RunnerEventEnvelope {
    RunnerEventEnvelope {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        sequence: inner.head + 1,
        occurred_at_ms: now_ms(),
        event,
    }
}

fn encode_event(event: &RunnerEventEnvelope) -> Result<Vec<u8>, Error> {
    let mut encoded = serde_json::to_vec(event)?;
    if encoded.len() > MAX_RUNNER_FRAME_BYTES {
        return Err(Error::Task("runner event exceeded the frame limit".into()));
    }
    encoded.push(b'\n');
    Ok(encoded)
}

async fn append_encoded(
    inner: &mut LogInner,
    event: RunnerEventEnvelope,
) -> Result<RunnerEventEnvelope, Error> {
    let encoded = encode_event(&event)?;
    inner.file.write_all(&encoded).await?;
    inner.file.flush().await?;
    inner.file.get_ref().sync_data().await?;
    inner.head = event.sequence;
    Ok(event)
}

pub async fn run(config: Config) -> Result<(), Error> {
    let (_runner_signal_tx, runner_signal_rx) = watch::channel(false);
    run_with_shutdown(config, runner_signal_rx).await
}

pub async fn run_with_shutdown(
    config: Config,
    mut runner_signal_rx: watch::Receiver<bool>,
) -> Result<(), Error> {
    validate_config(&config)?;
    let prepared = prepare_runtime(&config.runtime_dir).await?;
    let (stop_tx, stop_rx) = mpsc::channel(16);
    let (prepare_tx, prepare_rx) = mpsc::channel(1);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let state = Arc::new(RuntimeState {
        run_id: config.run_id.clone(),
        runner_instance_id: config.runner_instance_id.clone(),
        log: Arc::clone(&prepared.log),
        stop_tx,
        prepare_tx,
        prepare_claimed: Mutex::new(false),
        accepted_stops: Mutex::new(HashMap::new()),
        shutdown_tx,
    });
    let mut server = tokio::spawn(serve(
        prepared.listener,
        Arc::clone(&state),
        shutdown_rx.clone(),
    ));
    let mut supervisor = tokio::spawn(supervise(
        config,
        Arc::clone(&prepared.log),
        prepare_rx,
        stop_rx,
        runner_signal_rx.clone(),
    ));

    let supervisor_result = tokio::select! {
        result = &mut supervisor => joined_task("process supervisor", result),
        result = &mut server => {
            let server_result = joined_task("control server", result);
            if *shutdown_rx.borrow() {
                let supervisor_result = joined_task("process supervisor", supervisor.await);
                server_result?;
                supervisor_result?;
                cleanup_runtime(&prepared.socket_path)?;
                return Ok(());
            }
            supervisor.abort();
            let _ = supervisor.await;
            return match server_result {
                Ok(()) => Err(Error::Task(
                    "control server stopped before terminal acknowledgement".into(),
                )),
                Err(error) => Err(error),
            };
        }
    };
    let outcome = match supervisor_result {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = state.shutdown_tx.send(true);
            let _ = joined_task("control server", server.await);
            return Err(error);
        }
    };

    if outcome == SupervisionOutcome::RunnerSignalled || *runner_signal_rx.borrow() {
        let _ = state.shutdown_tx.send(true);
        let server_result = joined_task("control server", server.await);
        server_result?;
        return Ok(());
    }

    let mut server_finished = false;
    let mut preserve_runtime = false;
    while !*shutdown_rx.borrow() {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                changed.map_err(|_| Error::Task(
                    "control server stopped before terminal acknowledgement".into(),
                ))?;
            }
            result = &mut server => {
                let result = joined_task("control server", result);
                if !*shutdown_rx.borrow() {
                    return match result {
                        Ok(()) => Err(Error::Task(
                            "control server stopped before terminal acknowledgement".into(),
                        )),
                        Err(error) => Err(error),
                    };
                }
                result?;
                server_finished = true;
                break;
            }
            changed = runner_signal_rx.changed() => {
                match changed {
                    Ok(()) if *runner_signal_rx.borrow() => {
                        preserve_runtime = true;
                        let _ = state.shutdown_tx.send(true);
                        break;
                    }
                    Ok(()) => {}
                    Err(_) => {
                        let _ = state.shutdown_tx.send(true);
                        let _ = joined_task("control server", server.await);
                        return Err(Error::Task("runner signal watcher stopped".into()));
                    }
                }
            }
        }
    }
    if !server_finished {
        let server_result = joined_task("control server", server.await);
        server_result?;
    }
    if preserve_runtime {
        return Ok(());
    }
    cleanup_runtime(&prepared.socket_path)?;
    Ok(())
}

fn joined_task<T>(
    name: &str,
    result: Result<Result<T, Error>, tokio::task::JoinError>,
) -> Result<T, Error> {
    result.map_err(|error| Error::Task(format!("{name} task failed: {error}")))?
}

fn validate_config(config: &Config) -> Result<(), Error> {
    if config.program.as_os_str().is_empty() {
        return Err(Error::InvalidArguments(
            "agent program must not be empty".into(),
        ));
    }
    if config.exec_gate_program.as_os_str().is_empty() {
        return Err(Error::InvalidArguments(
            "exec-gate program must not be empty".into(),
        ));
    }
    if config
        .startup_input
        .as_ref()
        .is_some_and(|input| input.len() > MAX_STARTUP_STDIN_BYTES)
    {
        return Err(Error::InvalidArguments(format!(
            "startup stdin exceeds the {MAX_STARTUP_STDIN_BYTES}-byte limit"
        )));
    }
    let metadata = std::fs::metadata(&config.cwd)
        .map_err(|error| Error::InvalidArguments(format!("invalid cwd: {error}")))?;
    if !metadata.is_dir() {
        return Err(Error::InvalidArguments("cwd is not a directory".into()));
    }
    Ok(())
}

/// Creates `runtime_dir` fresh (mode `0700`), or adopts one the daemon
/// already created and staged an attempt credential into
/// before spawning this process at all (the daemon needs that file to
/// exist *before* the provider process can call `factoryctl hook`, so it
/// can no longer be this runner's exclusive privilege to create the
/// directory the way the old per-run ephemeral model assumed). Either way
/// the result must be a real, non-symlink, owner-only directory; an
/// existing directory that fails that check is rejected exactly as a
/// creation failure would be.
fn create_or_adopt_private_runtime_dir(runtime_dir: &Path) -> Result<(), Error> {
    match std::fs::DirBuilder::new().mode(0o700).create(runtime_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(runtime_dir)?;
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.mode() & 0o777 != 0o700
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "runtime directory exists but is not a private owner-only directory",
                )
                .into());
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

async fn prepare_runtime(runtime_dir: &Path) -> Result<PreparedRuntime, Error> {
    create_or_adopt_private_runtime_dir(runtime_dir)?;
    let spool_path = runtime_dir.join("events.ndjson");
    let socket_path = runtime_dir.join("control.sock");
    let setup = (|| -> io::Result<_> {
        let spool = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .mode(0o600)
            .open(&spool_path)?;
        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
        Ok((spool, listener))
    })();
    let (spool, listener) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_file(&spool_path);
            let _ = std::fs::remove_dir(runtime_dir);
            return Err(error.into());
        }
    };
    Ok(PreparedRuntime {
        listener,
        log: EventLog::new(spool_path.clone(), File::from_std(spool)),
        socket_path,
    })
}

/// Removes the control socket after the daemon acknowledges the durable exit.
/// The event spool remains for the daemon-owned resource finalizer.
fn cleanup_runtime(socket: &Path) -> Result<(), Error> {
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

async fn serve(
    listener: UnixListener,
    state: Arc<RuntimeState>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), Error> {
    let mut connections = JoinSet::new();
    let mut accept_error = None;
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        accept_error = Some(error);
                        break;
                    }
                };
                let state = Arc::clone(&state);
                let shutdown = shutdown.clone();
                connections.spawn(async move {
                    let _ = handle_connection(stream, state, shutdown).await;
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    if let Some(error) = accept_error {
        Err(error.into())
    } else {
        Ok(())
    }
}

async fn handle_connection(
    stream: UnixStream,
    state: Arc<RuntimeState>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), Error> {
    let (mut read_half, mut write_half) = stream.into_split();
    let request = match read_request(&mut read_half).await {
        Ok(request) => request,
        Err(error) => {
            return send_control_error(&mut write_half, error).await;
        }
    };
    if request.protocol_version != RUNNER_PROTOCOL_VERSION {
        return send_control_error(
            &mut write_half,
            ControlError::new(
                RunnerErrorCode::UnsupportedProtocol,
                format!(
                    "runner protocol {} is not supported",
                    request.protocol_version
                ),
            ),
        )
        .await;
    }
    if request.run_id != state.run_id || request.runner_instance_id != state.runner_instance_id {
        return send_control_error(
            &mut write_half,
            ControlError::new(
                RunnerErrorCode::Unauthorized,
                "runner identity does not match",
            ),
        )
        .await;
    }

    match request.request {
        RunnerRequest::Prepare { command_id } => {
            if let Err(message) = validate_command_id(&command_id) {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(RunnerErrorCode::InvalidRequest, message),
                )
                .await;
            }
            let mut claimed = state.prepare_claimed.lock().await;
            if *claimed {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(
                        RunnerErrorCode::Conflict,
                        "runner process was already prepared",
                    ),
                )
                .await;
            }
            *claimed = true;
            drop(claimed);

            let (prepared_tx, prepared_rx) = oneshot::channel();
            let (activate_tx, activate_rx) = oneshot::channel();
            if state
                .prepare_tx
                .send(PrepareCommand {
                    prepared: prepared_tx,
                    activation: activate_rx,
                })
                .await
                .is_err()
            {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(RunnerErrorCode::Conflict, "runner is stopping"),
                )
                .await;
            }
            let identity = match prepared_rx.await {
                Ok(Ok(identity)) => identity,
                Ok(Err(error)) => return send_control_error(&mut write_half, error).await,
                Err(_) => {
                    return send_control_error(
                        &mut write_half,
                        ControlError::new(
                            RunnerErrorCode::Internal,
                            "process supervisor disappeared before preparation",
                        ),
                    )
                    .await;
                }
            };
            send_frame(
                &mut write_half,
                &RunnerFrame::Prepared {
                    protocol_version: RUNNER_PROTOCOL_VERSION,
                    command_id: command_id.clone(),
                    runner_pid: std::process::id(),
                    child_pid: identity.child_pid,
                    process_group_id: identity.process_group_id,
                },
            )
            .await?;

            let activate = match read_request(&mut read_half).await {
                Ok(activate) => activate,
                // The preparation connection is the pre-exec leash. EOF,
                // timeout, or malformed input drops `activate_tx` now so the
                // supervisor kills the gate without ever running provider
                // code; there is no reliable peer left to receive an error.
                Err(_) => return Ok(()),
            };
            if activate.protocol_version != RUNNER_PROTOCOL_VERSION
                || activate.run_id != state.run_id
                || activate.runner_instance_id != state.runner_instance_id
            {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(
                        RunnerErrorCode::Unauthorized,
                        "runner identity does not match",
                    ),
                )
                .await;
            }
            match activate.request {
                RunnerRequest::Activate {
                    command_id: activate_id,
                } if activate_id == command_id => {
                    if activate_tx.send(()).is_err() {
                        return send_control_error(
                            &mut write_half,
                            ControlError::new(
                                RunnerErrorCode::Conflict,
                                "exec gate is no longer waiting",
                            ),
                        )
                        .await;
                    }
                    send_frame(
                        &mut write_half,
                        &RunnerFrame::CommandAck {
                            protocol_version: RUNNER_PROTOCOL_VERSION,
                            command_id,
                        },
                    )
                    .await
                }
                RunnerRequest::Activate { .. } => {
                    send_control_error(
                        &mut write_half,
                        ControlError::new(
                            RunnerErrorCode::Conflict,
                            "activation command does not match preparation",
                        ),
                    )
                    .await
                }
                _ => {
                    send_control_error(
                        &mut write_half,
                        ControlError::new(
                            RunnerErrorCode::InvalidRequest,
                            "expected activation on the preparation connection",
                        ),
                    )
                    .await
                }
            }
        }
        RunnerRequest::Activate { .. } => {
            send_control_error(
                &mut write_half,
                ControlError::new(
                    RunnerErrorCode::InvalidRequest,
                    "activation requires its preparation connection",
                ),
            )
            .await
        }
        RunnerRequest::Subscribe { after_sequence } => {
            subscribe_connection(&mut write_half, &state, shutdown, after_sequence).await
        }
        RunnerRequest::Stop {
            command_id,
            grace_ms,
        } => {
            if let Err(message) = validate_command_id(&command_id) {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(RunnerErrorCode::InvalidRequest, message),
                )
                .await;
            }
            if grace_ms > u64::try_from(MAX_STOP_GRACE.as_millis()).expect("grace fits u64") {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(
                        RunnerErrorCode::InvalidRequest,
                        "stop grace exceeds 60 seconds",
                    ),
                )
                .await;
            }
            let mut accepted = state.accepted_stops.lock().await;
            if let Some(accepted_grace_ms) = accepted.get(&command_id) {
                if *accepted_grace_ms != grace_ms {
                    return send_control_error(
                        &mut write_half,
                        ControlError::new(
                            RunnerErrorCode::Conflict,
                            "command ID was already accepted with different arguments",
                        ),
                    )
                    .await;
                }
                return send_frame(
                    &mut write_half,
                    &RunnerFrame::CommandAck {
                        protocol_version: RUNNER_PROTOCOL_VERSION,
                        command_id,
                    },
                )
                .await;
            }
            let (response, received) = oneshot::channel();
            if state
                .stop_tx
                .send(StopCommand {
                    grace: Duration::from_millis(grace_ms),
                    response,
                })
                .await
                .is_err()
            {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(
                        RunnerErrorCode::Conflict,
                        "attempt process is already terminal",
                    ),
                )
                .await;
            }
            match received.await {
                Ok(Ok(())) => {
                    accepted.insert(command_id.clone(), grace_ms);
                    send_frame(
                        &mut write_half,
                        &RunnerFrame::CommandAck {
                            protocol_version: RUNNER_PROTOCOL_VERSION,
                            command_id,
                        },
                    )
                    .await
                }
                Ok(Err(error)) => send_control_error(&mut write_half, error).await,
                Err(_) => {
                    send_control_error(
                        &mut write_half,
                        ControlError::new(RunnerErrorCode::Internal, "stop supervisor disappeared"),
                    )
                    .await
                }
            }
        }
        RunnerRequest::AcknowledgeExit {
            command_id,
            terminal_sequence,
        } => {
            if let Err(message) = validate_command_id(&command_id) {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(RunnerErrorCode::InvalidRequest, message),
                )
                .await;
            }
            let terminal = state.log.snapshot().await.terminal_sequence;
            if terminal != Some(terminal_sequence) {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(
                        RunnerErrorCode::Conflict,
                        "terminal sequence has not been durably reached",
                    ),
                )
                .await;
            }
            let result = send_frame(
                &mut write_half,
                &RunnerFrame::CommandAck {
                    protocol_version: RUNNER_PROTOCOL_VERSION,
                    command_id,
                },
            )
            .await;
            let _ = state.shutdown_tx.send(true);
            result
        }
    }
}

async fn read_request(
    read_half: &mut tokio::net::unix::OwnedReadHalf,
) -> Result<RequestEnvelope, ControlError> {
    let mut reader = BufReader::new(read_half)
        .take(u64::try_from(MAX_RUNNER_FRAME_BYTES + 2).expect("runner frame limit fits u64"));
    let mut bytes = Vec::new();
    let read = timeout(CONTROL_TIMEOUT, reader.read_until(b'\n', &mut bytes))
        .await
        .map_err(|_| ControlError::new(RunnerErrorCode::InvalidRequest, "request timed out"))?
        .map_err(|error| ControlError::new(RunnerErrorCode::InvalidRequest, error.to_string()))?;
    if read == 0 {
        return Err(ControlError::new(
            RunnerErrorCode::InvalidRequest,
            "request was empty",
        ));
    }
    if bytes.last() != Some(&b'\n') || bytes.len() - 1 > MAX_RUNNER_FRAME_BYTES {
        return Err(ControlError::new(
            RunnerErrorCode::InvalidRequest,
            "request exceeded the frame limit",
        ));
    }
    bytes.pop();
    serde_json::from_slice(&bytes).map_err(|error| {
        ControlError::new(
            RunnerErrorCode::InvalidRequest,
            format!("invalid request: {error}"),
        )
    })
}

async fn subscribe_connection(
    write: &mut OwnedWriteHalf,
    state: &RuntimeState,
    mut shutdown: watch::Receiver<bool>,
    after_sequence: i64,
) -> Result<(), Error> {
    if after_sequence < 0 {
        return send_control_error(
            write,
            ControlError::new(
                RunnerErrorCode::InvalidRequest,
                "event cursor must not be negative",
            ),
        )
        .await;
    }
    let mut events = state.log.subscribe();
    let snapshot = state.log.snapshot().await;
    if after_sequence > snapshot.head {
        return send_control_error(
            write,
            ControlError::new(
                RunnerErrorCode::Conflict,
                "event cursor is beyond the durable head",
            ),
        )
        .await;
    }
    send_frame(
        write,
        &RunnerFrame::Hello {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            run_id: state.run_id.clone(),
            runner_instance_id: state.runner_instance_id.clone(),
            runner_pid: std::process::id(),
            replay_through: snapshot.head,
            terminal_sequence: snapshot.terminal_sequence,
        },
    )
    .await?;
    replay_events(&state.log.spool_path, write, after_sequence, snapshot.head).await?;
    send_frame(
        write,
        &RunnerFrame::CaughtUp {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            sequence: snapshot.head,
        },
    )
    .await?;
    let mut delivered = snapshot.head;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            received = events.recv() => match received {
                Ok(event) if event.sequence <= delivered => {}
                Ok(event) if event.sequence == delivered + 1 => {
                    send_event(write, &event).await?;
                    delivered = event.sequence;
                }
                Ok(event) => {
                    replay_events(&state.log.spool_path, write, delivered, event.sequence).await?;
                    delivered = event.sequence;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let head = state.log.snapshot().await.head;
                    replay_events(&state.log.spool_path, write, delivered, head).await?;
                    delivered = head;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }
}

async fn replay_events(
    spool_path: &Path,
    write: &mut OwnedWriteHalf,
    after: i64,
    through: i64,
) -> Result<(), Error> {
    if after >= through {
        return Ok(());
    }
    let file = File::open(spool_path).await?;
    let mut lines = BufReader::new(file).lines();
    let mut expected = after + 1;
    while let Some(line) = lines.next_line().await? {
        if line.len() > MAX_RUNNER_FRAME_BYTES {
            return Err(Error::Task(
                "durable runner event exceeded the frame limit".into(),
            ));
        }
        let event: RunnerEventEnvelope = serde_json::from_str(&line)?;
        if event.sequence > after && event.sequence <= through {
            if event.sequence != expected {
                return Err(Error::Task(format!(
                    "runner spool gap: expected sequence {expected}, found {}",
                    event.sequence
                )));
            }
            send_event(write, &event).await?;
            expected += 1;
        }
        if event.sequence >= through {
            break;
        }
    }
    if expected != through + 1 {
        return Err(Error::Task(format!(
            "runner spool ended before sequence {through}"
        )));
    }
    Ok(())
}

async fn send_event(write: &mut OwnedWriteHalf, event: &RunnerEventEnvelope) -> Result<(), Error> {
    send_frame(
        write,
        &RunnerFrame::Event {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            event: event.clone(),
        },
    )
    .await
}

async fn send_control_error(write: &mut OwnedWriteHalf, error: ControlError) -> Result<(), Error> {
    send_frame(
        write,
        &RunnerFrame::Error {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            code: error.code,
            message: error.message,
        },
    )
    .await
}

async fn send_frame(write: &mut OwnedWriteHalf, frame: &RunnerFrame) -> Result<(), Error> {
    let mut encoded = serde_json::to_vec(frame)?;
    if encoded.len() > MAX_RUNNER_FRAME_BYTES {
        return Err(Error::Task(
            "runner response exceeded the frame limit".into(),
        ));
    }
    encoded.push(b'\n');
    timeout(CONTROL_TIMEOUT, write.write_all(&encoded))
        .await
        .map_err(|_| Error::Task("runner response timed out".into()))??;
    Ok(())
}

async fn supervise(
    config: Config,
    log: Arc<EventLog>,
    mut prepares: mpsc::Receiver<PrepareCommand>,
    stops: mpsc::Receiver<StopCommand>,
    mut runner_shutdown: watch::Receiver<bool>,
) -> Result<SupervisionOutcome, Error> {
    if *runner_shutdown.borrow() {
        return Ok(SupervisionOutcome::RunnerSignalled);
    }
    let prepare = tokio::select! {
        prepare = prepares.recv() => prepare.ok_or_else(|| {
            Error::Task("runner prepare channel stopped before activation".into())
        })?,
        changed = runner_shutdown.changed() => {
            changed.map_err(|_| Error::Task("runner signal watcher stopped".into()))?;
            return Ok(SupervisionOutcome::RunnerSignalled);
        }
    };
    supervise_piped(config, log, prepare, stops, runner_shutdown).await
}

async fn supervise_piped(
    config: Config,
    log: Arc<EventLog>,
    prepare: PrepareCommand,
    mut stops: mpsc::Receiver<StopCommand>,
    mut runner_shutdown: watch::Receiver<bool>,
) -> Result<SupervisionOutcome, Error> {
    let gate_path = exec_gate_path(&config);
    ensure_exec_gate_absent(&gate_path)?;
    let stdin = match prepare_startup_stdin(config.startup_input) {
        Ok(stdin) => stdin,
        Err(error) => {
            log.append_lifecycle(
                RunnerEvent::SpawnFailed {
                    message: truncate_utf8(error.to_string(), MAX_RUNNER_ERROR_BYTES),
                },
                true,
            )
            .await?;
            return Ok(SupervisionOutcome::AwaitAcknowledgement);
        }
    };
    let mut command = Command::new(&config.exec_gate_program);
    command
        .arg("--exec-gate")
        .arg(&gate_path)
        .arg("--expected-parent-pid")
        .arg(rustix::process::getpid().as_raw_nonzero().get().to_string())
        .arg("--")
        .arg(&config.program)
        .args(&config.arguments)
        .current_dir(&config.cwd)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.as_std_mut().process_group(0);
    command.kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            log.append_lifecycle(
                RunnerEvent::SpawnFailed {
                    message: truncate_utf8(error.to_string(), MAX_RUNNER_ERROR_BYTES),
                },
                true,
            )
            .await?;
            return Ok(SupervisionOutcome::AwaitAcknowledgement);
        }
    };
    drop(command);
    let child_pid = child
        .id()
        .ok_or_else(|| Error::Task("spawned child has no process ID".into()))?;
    let pid = Pid::from_raw(
        i32::try_from(child_pid).map_err(|_| Error::Task("child PID overflow".into()))?,
    )
    .ok_or_else(|| Error::Task("child PID was zero".into()))?;
    let mut process_group = ProcessGroupGuard::new(pid);
    let identity = process_identity(child_pid, pid)?;
    if prepare.prepared.send(Ok(identity)).is_err() {
        signal_process_group(pid, Signal::KILL)?;
        let _ = child.wait().await;
        process_group.disarm();
        return Ok(SupervisionOutcome::RunnerSignalled);
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Task("child stdout was not captured".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Task("child stderr was not captured".into()))?;
    let mut stdout_task = PipeDrainTask::new(tokio::spawn(drain_output(stdout)));
    let mut stderr_task = PipeDrainTask::new(tokio::spawn(drain_output(stderr)));
    let mut activation = prepare.activation;
    let activated = loop {
        tokio::select! {
            result = &mut activation => break result.is_ok(),
            status = child.wait() => {
                status?;
                break false;
            }
            command = stops.recv() => {
                let Some(command) = command else {
                    continue;
                };
                let result = signal_process_group(pid, Signal::KILL).map_err(|error| {
                    ControlError::new(RunnerErrorCode::Internal, format!("failed to stop exec gate: {error}"))
                });
                let _ = command.response.send(result);
                break false;
            }
            changed = runner_shutdown.changed() => {
                changed.map_err(|_| Error::Task("runner signal watcher stopped".into()))?;
                break false;
            }
        }
    };
    if !activated {
        let _ = signal_process_group(pid, Signal::KILL);
        let _ = child.wait().await;
        stdout_task.abort().await;
        stderr_task.abort().await;
        process_group.disarm();
        return Ok(SupervisionOutcome::RunnerSignalled);
    }
    if let Err(error) = release_exec_gate(&gate_path) {
        signal_process_group(pid, Signal::KILL)?;
        let _ = child.wait().await;
        stdout_task.abort().await;
        stderr_task.abort().await;
        log.append_lifecycle(
            RunnerEvent::SpawnFailed {
                message: truncate_utf8(error.to_string(), MAX_RUNNER_ERROR_BYTES),
            },
            true,
        )
        .await?;
        process_group.disarm();
        return Ok(SupervisionOutcome::AwaitAcknowledgement);
    }
    log.append_lifecycle(RunnerEvent::Started { child_pid }, false)
        .await?;
    let mut kill_deadline: Option<Pin<Box<Sleep>>> = None;
    let mut runner_signalled = false;
    let mut stop_requested = false;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            command = stops.recv() => {
                let Some(command) = command else {
                    continue;
                };
                stop_requested = true;
                if let Err(error) = begin_group_termination(pid, command.grace, &mut kill_deadline) {
                    let _ = command.response.send(Err(ControlError::new(
                        RunnerErrorCode::Internal,
                        format!("failed to stop process group: {error}"),
                    )));
                    continue;
                }
                let _ = command.response.send(Ok(()));
            }
            changed = runner_shutdown.changed(), if !runner_signalled => {
                changed.map_err(|_| Error::Task("runner signal watcher stopped".into()))?;
                runner_signalled = true;
                begin_group_termination(pid, DEFAULT_GROUP_GRACE, &mut kill_deadline)?;
            }
            () = wait_for_deadline(&mut kill_deadline), if kill_deadline.is_some() => {
                signal_process_group(pid, Signal::KILL)?;
                kill_deadline = None;
            }
            result = &mut stdout_task.handle, if !stdout_task.finished => {
                if let Err(error) = stdout_task.finish(result) {
                    signal_process_group(pid, Signal::KILL)?;
                    stderr_task.abort().await;
                    let _ = child.wait().await;
                    return Err(error);
                }
            }
            result = &mut stderr_task.handle, if !stderr_task.finished => {
                if let Err(error) = stderr_task.finish(result) {
                    signal_process_group(pid, Signal::KILL)?;
                    stdout_task.abort().await;
                    let _ = child.wait().await;
                    return Err(error);
                }
            }
        }
    };

    let cleanup_grace = if stop_requested || runner_signalled {
        GROUP_CLEANUP_GRACE
    } else {
        DEFAULT_GROUP_GRACE
    };
    reap_group_stragglers(
        pid,
        cleanup_grace,
        &mut kill_deadline,
        &mut stops,
        &mut runner_shutdown,
        &mut runner_signalled,
    )
    .await?;

    let output_result = timeout(POST_KILL_DRAIN_TIMEOUT, async {
        stdout_task.join().await?;
        stderr_task.join().await
    })
    .await;
    match output_result {
        Ok(result) => result?,
        Err(_) => {
            stdout_task.abort().await;
            stderr_task.abort().await;
            return Err(Error::Task(
                "attempt output pipes remained open after process-group termination".into(),
            ));
        }
    }
    log.append_lifecycle(
        RunnerEvent::Exited {
            exit_code: status.code(),
            signal: status.signal(),
        },
        true,
    )
    .await?;
    process_group.disarm();
    if runner_signalled {
        Ok(SupervisionOutcome::RunnerSignalled)
    } else {
        Ok(SupervisionOutcome::AwaitAcknowledgement)
    }
}

fn process_identity(child_pid: u32, pid: Pid) -> Result<ProcessIdentity, Error> {
    let process_group = getpgid(Some(pid)).map_err(io::Error::from)?;
    let process_group_id = u32::try_from(process_group.as_raw_nonzero().get())
        .map_err(|_| Error::Task("process group ID overflow".into()))?;
    Ok(ProcessIdentity {
        child_pid,
        process_group_id,
    })
}

fn exec_gate_path(config: &Config) -> PathBuf {
    config.runtime_dir.join(format!(
        "exec-gate-{}.activate",
        config.runner_instance_id.as_str()
    ))
}

fn ensure_exec_gate_absent(path: &Path) -> Result<(), Error> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(Error::Task(
            "exec-gate activation path already exists".into(),
        )),
        Err(error) => Err(error.into()),
    }
}

fn release_exec_gate(path: &Path) -> Result<(), Error> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.sync_all()?;
    Ok(())
}

fn prepare_startup_stdin(input: Option<Vec<u8>>) -> Result<Stdio, Error> {
    let Some(input) = input else {
        return Ok(Stdio::null());
    };
    let mut file = tempfile::tempfile()?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    std::io::Write::write_all(&mut file, &input)?;
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0))?;
    Ok(Stdio::from(file))
}

fn begin_group_termination(
    pid: Pid,
    grace: Duration,
    deadline: &mut Option<Pin<Box<Sleep>>>,
) -> Result<(), Error> {
    if deadline.is_none() {
        signal_process_group(pid, Signal::TERM)?;
        *deadline = Some(Box::pin(tokio::time::sleep(grace)));
    } else {
        shorten_deadline(grace, deadline);
    }
    Ok(())
}

fn shorten_deadline(grace: Duration, deadline: &mut Option<Pin<Box<Sleep>>>) {
    if let Some(deadline) = deadline {
        let requested = Instant::now() + grace;
        if requested < deadline.deadline() {
            deadline.as_mut().reset(requested);
        }
    }
}

/// A process-group ID stops being ours the moment its leader is reaped: the
/// kernel is then free to hand that same numeric PID to an unrelated future
/// process on the same host. If that new process belongs to a different
/// user or session, `kill(-pgid, ...)` reaching it can return `EPERM`
/// instead of the `ESRCH` a truly-gone group would give — same underlying
/// situation (nothing of ours is there anymore), different errno. On a busy
/// host (this repo's own self-hosted runner spawns dozens of these
/// processes concurrently) that reuse window is real, not theoretical:
/// reproduced directly by spawning this exact natural-leader-exit scenario
/// in a tight loop — 3 `EPERM` crashes in 20 runs on `main`, pre-`GROUP_POLL_INTERVAL`.
/// Polling `process_group_exists` far more often than the old single check
/// did only widens the exposure. Treat both errnos as "gone", not a hard
/// I/O failure that aborts the run without ever recording its `Exited`
/// event.
fn is_group_gone(error: rustix::io::Errno) -> bool {
    matches!(error, rustix::io::Errno::SRCH | rustix::io::Errno::PERM)
}

fn signal_process_group(pid: Pid, signal: Signal) -> Result<(), Error> {
    match kill_process_group(pid, signal) {
        Ok(()) => Ok(()),
        Err(error) if is_group_gone(error) => Ok(()),
        Err(error) => Err(Error::Io(error.into())),
    }
}

fn process_group_exists(pid: Pid) -> Result<bool, Error> {
    match test_kill_process_group(pid) {
        Ok(()) => Ok(true),
        Err(error) if is_group_gone(error) => Ok(false),
        Err(error) => Err(Error::Io(error.into())),
    }
}

async fn wait_for_deadline(deadline: &mut Option<Pin<Box<Sleep>>>) {
    if let Some(deadline) = deadline {
        deadline.as_mut().await;
    } else {
        pending::<()>().await;
    }
}

/// Reaps anything still holding the process group after the leader itself
/// has already exited: a no-op if the group is already empty, otherwise a
/// single TERM followed by polling (`GROUP_POLL_INTERVAL`) for the group to
/// empty out on its own, escalating to KILL only once `grace` elapses
/// without that happening. `grace` is the caller's choice — see
/// `GROUP_CLEANUP_GRACE`'s doc comment for why it differs depending on
/// whether a `Stop`/shutdown was ever requested. Shared by both
/// the piped supervisor.
async fn reap_group_stragglers(
    pid: Pid,
    grace: Duration,
    kill_deadline: &mut Option<Pin<Box<Sleep>>>,
    stops: &mut mpsc::Receiver<StopCommand>,
    runner_shutdown: &mut watch::Receiver<bool>,
    runner_signalled: &mut bool,
) -> Result<(), Error> {
    if !process_group_exists(pid)? {
        return Ok(());
    }
    begin_group_termination(pid, grace, kill_deadline)?;
    loop {
        tokio::select! {
            () = wait_for_deadline(kill_deadline), if kill_deadline.is_some() => {
                signal_process_group(pid, Signal::KILL)?;
                return Ok(());
            }
            () = sleep(GROUP_POLL_INTERVAL) => {
                if !process_group_exists(pid)? {
                    return Ok(());
                }
            }
            command = stops.recv() => {
                let Some(command) = command else {
                    continue;
                };
                shorten_deadline(command.grace, kill_deadline);
                let _ = command.response.send(Ok(()));
            }
            changed = runner_shutdown.changed(), if !*runner_signalled => {
                changed.map_err(|_| Error::Task("runner signal watcher stopped".into()))?;
                *runner_signalled = true;
            }
        }
    }
}

fn join_drain(result: Result<Result<(), Error>, tokio::task::JoinError>) -> Result<(), Error> {
    result.map_err(|error| Error::Task(format!("pipe drain task failed: {error}")))?
}

async fn drain_output(mut input: impl AsyncRead + Unpin) -> Result<(), Error> {
    tokio::io::copy(&mut input, &mut tokio::io::sink()).await?;
    Ok(())
}

fn validate_command_id(command_id: &str) -> Result<(), String> {
    if command_id.is_empty()
        || command_id.len() > MAX_COMMAND_ID_BYTES
        || command_id.chars().any(char::is_control)
    {
        return Err("command ID must be 1..=128 non-control UTF-8 bytes".into());
    }
    Ok(())
}

fn truncate_utf8(mut text: String, maximum: usize) -> String {
    if text.len() <= maximum {
        return text;
    }
    let mut length = maximum;
    while !text.is_char_boundary(length) {
        length -= 1;
    }
    text.truncate(length);
    text
}

fn now_ms() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
