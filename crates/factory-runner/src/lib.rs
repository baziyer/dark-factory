//! Stable, provider-blind ownership wrapper for one agent process.

use std::{
    collections::HashMap,
    future::pending,
    io,
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
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
        MAX_RUNNER_ERROR_BYTES, MAX_RUNNER_FRAME_BYTES, MAX_RUNNER_OUTPUT_TEXT_BYTES,
        MAX_RUNNER_SPOOL_BYTES, MAX_STARTUP_STDIN_BYTES, OutputStream, RUNNER_PROTOCOL_VERSION,
        RequestEnvelope, RunnerErrorCode, RunnerEvent, RunnerEventEnvelope, RunnerFrame,
        RunnerRequest,
    },
};
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::{UnixListener, UnixStream, unix::OwnedWriteHalf},
    process::Command,
    sync::{Mutex, broadcast, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, Sleep, timeout},
};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_STOP_GRACE: Duration = Duration::from_secs(60);
const DEFAULT_GROUP_GRACE: Duration = Duration::from_secs(2);
const POST_KILL_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_COMMAND_ID_BYTES: usize = 128;
const BROADCAST_CAPACITY: usize = 32;
const TERMINAL_RESERVE_BYTES: usize = MAX_RUNNER_ERROR_BYTES + 4096;

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
}

struct PreparedRuntime {
    listener: UnixListener,
    log: Arc<EventLog>,
    socket_path: PathBuf,
    spool_path: PathBuf,
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
    bytes: usize,
    output_truncated: bool,
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
    accepted_stops: Mutex<HashMap<String, u64>>,
    shutdown_tx: watch::Sender<bool>,
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

struct OutputTask {
    handle: JoinHandle<Result<(), Error>>,
    finished: bool,
}

impl OutputTask {
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
        join_output(result)
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

impl Drop for OutputTask {
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
                bytes: 0,
                output_truncated: false,
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

    async fn append_output(
        &self,
        stream: OutputStream,
        text: String,
        lossy: bool,
    ) -> Result<(), Error> {
        debug_assert!(text.len() <= MAX_RUNNER_OUTPUT_TEXT_BYTES);
        let event = RunnerEvent::Output {
            stream,
            text,
            lossy,
        };
        let published = {
            let mut inner = self.inner.lock().await;
            if inner.terminal_sequence.is_some() || inner.output_truncated {
                return Ok(());
            }
            let envelope = next_envelope(&inner, event);
            let encoded = encode_event(&envelope)?;
            if inner.bytes + encoded.len() + TERMINAL_RESERVE_BYTES > MAX_RUNNER_SPOOL_BYTES {
                inner.output_truncated = true;
                let truncated = next_envelope(
                    &inner,
                    RunnerEvent::OutputTruncated {
                        limit_bytes: u64::try_from(MAX_RUNNER_SPOOL_BYTES)
                            .expect("spool limit fits u64"),
                    },
                );
                Some(append_encoded(&mut inner, truncated, false).await?)
            } else {
                Some(append_encoded(&mut inner, envelope, false).await?)
            }
        };
        if let Some(event) = published {
            let _ = self.events.send(event);
        }
        Ok(())
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
            let encoded_len = encode_event(&envelope)?.len();
            if inner.bytes + encoded_len > MAX_RUNNER_SPOOL_BYTES {
                return Err(Error::Task(
                    "terminal event does not fit the bounded spool".into(),
                ));
            }
            let envelope = append_encoded(&mut inner, envelope, true).await?;
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
    sync: bool,
) -> Result<RunnerEventEnvelope, Error> {
    let encoded = encode_event(&event)?;
    inner.file.write_all(&encoded).await?;
    inner.file.flush().await?;
    if sync {
        inner.file.get_ref().sync_data().await?;
    }
    inner.bytes += encoded.len();
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
    let runtime_dir = config.runtime_dir.clone();
    let prepared = prepare_runtime(&config.runtime_dir).await?;
    let (stop_tx, stop_rx) = mpsc::channel(16);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let state = Arc::new(RuntimeState {
        run_id: config.run_id.clone(),
        runner_instance_id: config.runner_instance_id.clone(),
        log: Arc::clone(&prepared.log),
        stop_tx,
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
                cleanup_runtime(&prepared.socket_path, &prepared.spool_path, &runtime_dir)?;
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
    cleanup_runtime(&prepared.socket_path, &prepared.spool_path, &runtime_dir)?;
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

async fn prepare_runtime(runtime_dir: &Path) -> Result<PreparedRuntime, Error> {
    std::fs::DirBuilder::new().mode(0o700).create(runtime_dir)?;
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
        spool_path,
    })
}

fn cleanup_runtime(socket: &Path, spool: &Path, runtime: &Path) -> Result<(), Error> {
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match std::fs::remove_file(spool) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    std::fs::remove_dir(runtime)?;
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
    let (read_half, mut write_half) = stream.into_split();
    let request = match read_request(read_half).await {
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
                        "agent process is already terminal",
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
    read_half: tokio::net::unix::OwnedReadHalf,
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
    mut stops: mpsc::Receiver<StopCommand>,
    mut runner_shutdown: watch::Receiver<bool>,
) -> Result<SupervisionOutcome, Error> {
    if *runner_shutdown.borrow() {
        return Ok(SupervisionOutcome::RunnerSignalled);
    }
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
    let mut command = Command::new(&config.program);
    command
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
    log.append_lifecycle(RunnerEvent::Started { child_pid }, false)
        .await?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Task("child stdout was not captured".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Task("child stderr was not captured".into()))?;
    let mut stdout_task = OutputTask::new(tokio::spawn(drain_output(
        stdout,
        OutputStream::Stdout,
        Arc::clone(&log),
    )));
    let mut stderr_task = OutputTask::new(tokio::spawn(drain_output(
        stderr,
        OutputStream::Stderr,
        Arc::clone(&log),
    )));
    let mut kill_deadline: Option<Pin<Box<Sleep>>> = None;
    let mut runner_signalled = false;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            command = stops.recv() => {
                let Some(command) = command else {
                    continue;
                };
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

    if process_group_exists(pid)? {
        begin_group_termination(pid, DEFAULT_GROUP_GRACE, &mut kill_deadline)?;
        loop {
            tokio::select! {
                () = wait_for_deadline(&mut kill_deadline), if kill_deadline.is_some() => {
                    signal_process_group(pid, Signal::KILL)?;
                    break;
                }
                command = stops.recv() => {
                    let Some(command) = command else {
                        continue;
                    };
                    shorten_deadline(command.grace, &mut kill_deadline);
                    let _ = command.response.send(Ok(()));
                }
                changed = runner_shutdown.changed(), if !runner_signalled => {
                    changed.map_err(|_| Error::Task("runner signal watcher stopped".into()))?;
                    runner_signalled = true;
                }
            }
        }
    }

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
                "agent output pipes remained open after process-group termination".into(),
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

fn signal_process_group(pid: Pid, signal: Signal) -> Result<(), Error> {
    match kill_process_group(pid, signal) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::SRCH => Ok(()),
        Err(error) => Err(Error::Io(error.into())),
    }
}

fn process_group_exists(pid: Pid) -> Result<bool, Error> {
    match test_kill_process_group(pid) {
        Ok(()) => Ok(true),
        Err(error) if error == rustix::io::Errno::SRCH => Ok(false),
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

fn join_output(result: Result<Result<(), Error>, tokio::task::JoinError>) -> Result<(), Error> {
    result.map_err(|error| Error::Task(format!("output reader task failed: {error}")))?
}

async fn drain_output(
    mut input: impl AsyncRead + Unpin,
    stream: OutputStream,
    log: Arc<EventLog>,
) -> Result<(), Error> {
    let mut read_buffer = [0_u8; 8192];
    let mut pending_bytes = Vec::new();
    loop {
        let read = input.read(&mut read_buffer).await?;
        if read == 0 {
            for chunk in decode_available(&mut pending_bytes, true) {
                log.append_output(stream, chunk.text, chunk.lossy).await?;
            }
            break;
        }
        pending_bytes.extend_from_slice(&read_buffer[..read]);
        for chunk in decode_available(&mut pending_bytes, false) {
            log.append_output(stream, chunk.text, chunk.lossy).await?;
        }
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
struct DecodedChunk {
    text: String,
    lossy: bool,
}

fn decode_available(bytes: &mut Vec<u8>, eof: bool) -> Vec<DecodedChunk> {
    let mut chunks = Vec::new();
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(valid) => {
                let take = utf8_prefix_len(valid, MAX_RUNNER_OUTPUT_TEXT_BYTES);
                chunks.push(DecodedChunk {
                    text: valid[..take].to_owned(),
                    lossy: false,
                });
                bytes.drain(..take);
            }
            Err(error) if error.valid_up_to() > 0 => {
                let valid = std::str::from_utf8(&bytes[..error.valid_up_to()])
                    .expect("Utf8Error valid prefix is valid UTF-8");
                let take = utf8_prefix_len(valid, MAX_RUNNER_OUTPUT_TEXT_BYTES);
                chunks.push(DecodedChunk {
                    text: valid[..take].to_owned(),
                    lossy: false,
                });
                bytes.drain(..take);
            }
            Err(error) => match error.error_len() {
                Some(length) => {
                    chunks.push(DecodedChunk {
                        text: String::from_utf8_lossy(&bytes[..length]).into_owned(),
                        lossy: true,
                    });
                    bytes.drain(..length);
                }
                None if eof => {
                    chunks.push(DecodedChunk {
                        text: String::from_utf8_lossy(bytes).into_owned(),
                        lossy: true,
                    });
                    bytes.clear();
                }
                None => break,
            },
        }
    }
    chunks
}

fn utf8_prefix_len(text: &str, maximum: usize) -> usize {
    if text.len() <= maximum {
        return text.len();
    }
    let mut length = maximum;
    while !text.is_char_boundary(length) {
        length -= 1;
    }
    length
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

#[cfg(test)]
mod tests {
    use super::{
        Config, DecodedChunk, Error, MAX_STARTUP_STDIN_BYTES, decode_available, validate_config,
    };
    use factory_core::{RunId, RunnerInstanceId, runner::MAX_RUNNER_OUTPUT_TEXT_BYTES};

    #[test]
    fn config_rejects_startup_input_over_the_hard_limit() {
        let directory = tempfile::tempdir().unwrap();
        let error = validate_config(&Config {
            run_id: RunId::try_from("run-1").unwrap(),
            runner_instance_id: RunnerInstanceId::try_from("instance-1").unwrap(),
            runtime_dir: directory.path().join("run"),
            cwd: directory.path().to_owned(),
            startup_input: Some(vec![0; MAX_STARTUP_STDIN_BYTES + 1]),
            program: "/bin/true".into(),
            arguments: Vec::new(),
        })
        .unwrap_err();
        assert!(matches!(error, Error::InvalidArguments(message) if message.contains("stdin")));
    }

    #[test]
    fn decoder_preserves_a_scalar_split_across_reads() {
        let crab = "🦀".as_bytes();
        let mut bytes = crab[..2].to_vec();
        assert!(decode_available(&mut bytes, false).is_empty());
        bytes.extend_from_slice(&crab[2..]);
        assert_eq!(
            decode_available(&mut bytes, false),
            vec![DecodedChunk {
                text: "🦀".into(),
                lossy: false,
            }]
        );
    }

    #[test]
    fn decoder_discloses_invalid_and_incomplete_eof_bytes() {
        let mut invalid = vec![0xff];
        assert!(decode_available(&mut invalid, false)[0].lossy);
        let mut incomplete = vec![0xf0, 0x9f];
        assert!(decode_available(&mut incomplete, false).is_empty());
        assert!(decode_available(&mut incomplete, true)[0].lossy);
    }

    #[test]
    fn decoder_keeps_multibyte_scalars_inside_the_chunk_limit() {
        let mut bytes = vec![b'x'; MAX_RUNNER_OUTPUT_TEXT_BYTES - 1];
        bytes.extend_from_slice("🦀".as_bytes());
        let chunks = decode_available(&mut bytes, false);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text.len(), MAX_RUNNER_OUTPUT_TEXT_BYTES - 1);
        assert_eq!(chunks[1].text, "🦀");
        assert!(chunks.iter().all(|chunk| !chunk.lossy));
    }
}
