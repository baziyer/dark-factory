//! Stable, provider-blind ownership wrapper for one agent process.

use std::{
    collections::HashMap,
    future::pending,
    io,
    io::{Read as _, Write as _},
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
        MAX_RUNNER_ERROR_BYTES, MAX_RUNNER_FRAME_BYTES, MAX_RUNNER_OUTPUT_TEXT_BYTES,
        MAX_RUNNER_SPOOL_BYTES, MAX_STARTUP_STDIN_BYTES, MAX_TERMINAL_INPUT_BYTES,
        MAX_TERMINAL_LOG_BYTES, MAX_TERMINAL_OUTPUT_CHUNK_BYTES, OutputStream,
        RUNNER_PROTOCOL_VERSION, RequestEnvelope, RunnerErrorCode, RunnerEvent,
        RunnerEventEnvelope, RunnerFrame, RunnerRequest, TerminalSize, decode_terminal_bytes,
        encode_terminal_bytes,
    },
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rustix::process::{
    Pid, Signal, WaitOptions, kill_process_group, test_kill_process_group, waitpid,
};
use tokio::{
    fs::{File, OpenOptions},
    io::{
        AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader, BufWriter,
    },
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
const TERMINAL_LOG_FILE: &str = "terminal.log";
const TERMINAL_LOG_ROTATED_FILE: &str = "terminal.log.1";
const TERMINAL_BROADCAST_CAPACITY: usize = 64;
const TERMINAL_COMMAND_CAPACITY: usize = 16;
const TERMINAL_READ_CHUNK: usize = MAX_TERMINAL_OUTPUT_CHUNK_BYTES;

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
    /// When `Some`, the agent is spawned under a PTY of this size instead of
    /// with piped stdout/stderr: `startup_input` is not sent (interactive
    /// programs take input from the operator via `TerminalInput`), and
    /// output is retained raw in `terminal.log` instead of decoded into
    /// bounded `RunnerEvent::Output` text events.
    pub terminal: Option<TerminalSize>,
}

struct PreparedRuntime {
    listener: UnixListener,
    log: Arc<EventLog>,
    terminal_log: Option<Arc<TerminalLog>>,
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
    terminal_log: Option<Arc<TerminalLog>>,
    stop_tx: mpsc::Sender<StopCommand>,
    terminal_tx: Option<mpsc::Sender<TerminalCommand>>,
    accepted_stops: Mutex<HashMap<String, u64>>,
    shutdown_tx: watch::Sender<bool>,
}

struct StopCommand {
    grace: Duration,
    response: oneshot::Sender<Result<(), ControlError>>,
}

struct TerminalCommand {
    kind: TerminalCommandKind,
    response: oneshot::Sender<Result<(), ControlError>>,
}

enum TerminalCommandKind {
    Input(Vec<u8>),
    Resize { cols: u16, rows: u16 },
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

/// Retained, bounded, raw byte log for one terminal-mode run's PTY output.
///
/// Unlike [`EventLog`], this is not part of the durable command-acknowledgement
/// path: bytes are appended best-effort (no `sync_data`) purely so an operator
/// can inspect or re-attach to a run's terminal after the fact. Positions in
/// the log are a single monotonic byte-stream offset, independent of which
/// physical file currently holds a given byte.
struct TerminalLog {
    dir: PathBuf,
    max_bytes: u64,
    inner: Mutex<TerminalLogInner>,
    chunks: broadcast::Sender<TerminalChunk>,
}

struct TerminalLogInner {
    active_file: File,
    active_start_offset: u64,
    active_len: u64,
    /// Start offset and length of `terminal.log.1`, the previous rotation,
    /// when one exists.
    previous: Option<(u64, u64)>,
}

impl TerminalLogInner {
    const fn total_bytes(&self) -> u64 {
        self.active_start_offset + self.active_len
    }

    const fn oldest_retained_offset(&self) -> u64 {
        match self.previous {
            Some((start, _)) => start,
            None => self.active_start_offset,
        }
    }
}

#[derive(Clone)]
struct TerminalChunk {
    offset: u64,
    bytes: Arc<Vec<u8>>,
}

#[derive(Clone, Copy)]
struct TerminalSnapshot {
    total_bytes: u64,
    oldest_retained_offset: u64,
    active_start_offset: u64,
    previous: Option<(u64, u64)>,
}

impl TerminalLog {
    fn new(dir: PathBuf, max_bytes: u64, active_file: File) -> Arc<Self> {
        let (chunks, _) = broadcast::channel(TERMINAL_BROADCAST_CAPACITY);
        Arc::new(Self {
            dir,
            max_bytes: max_bytes.max(1),
            inner: Mutex::new(TerminalLogInner {
                active_file,
                active_start_offset: 0,
                active_len: 0,
                previous: None,
            }),
            chunks,
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<TerminalChunk> {
        self.chunks.subscribe()
    }

    async fn snapshot(&self) -> TerminalSnapshot {
        let inner = self.inner.lock().await;
        TerminalSnapshot {
            total_bytes: inner.total_bytes(),
            oldest_retained_offset: inner.oldest_retained_offset(),
            active_start_offset: inner.active_start_offset,
            previous: inner.previous,
        }
    }

    /// Appends raw bytes, rotating the active file when it is full, then
    /// broadcasts the whole chunk (as one unit, regardless of whether it was
    /// physically split across a rotation) to live subscribers.
    async fn append(&self, bytes: Vec<u8>) -> Result<(), Error> {
        if bytes.is_empty() {
            return Ok(());
        }
        let mut inner = self.inner.lock().await;
        let start_offset = inner.total_bytes();
        let mut remaining: &[u8] = &bytes;
        while !remaining.is_empty() {
            let space = self.max_bytes.saturating_sub(inner.active_len);
            if space == 0 {
                self.rotate(&mut inner).await?;
                continue;
            }
            let take = remaining
                .len()
                .min(usize::try_from(space).unwrap_or(usize::MAX));
            inner.active_file.write_all(&remaining[..take]).await?;
            inner.active_len += take as u64;
            remaining = &remaining[take..];
        }
        inner.active_file.flush().await?;
        drop(inner);
        let _ = self.chunks.send(TerminalChunk {
            offset: start_offset,
            bytes: Arc::new(bytes),
        });
        Ok(())
    }

    async fn rotate(&self, inner: &mut TerminalLogInner) -> Result<(), Error> {
        inner.active_file.flush().await?;
        let active_path = self.dir.join(TERMINAL_LOG_FILE);
        let rotated_path = self.dir.join(TERMINAL_LOG_ROTATED_FILE);
        tokio::fs::rename(&active_path, &rotated_path).await?;
        let fresh = OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .mode(0o600)
            .open(&active_path)
            .await?;
        inner.previous = Some((inner.active_start_offset, inner.active_len));
        inner.active_start_offset += inner.active_len;
        inner.active_len = 0;
        inner.active_file = fresh;
        Ok(())
    }

    /// Replays retained bytes `[from, through)` to `write` as `TerminalOutput`
    /// frames, reading `terminal.log.1` (if it covers any of the range) then
    /// `terminal.log`, using the file boundaries already fixed in `snapshot`.
    ///
    /// Files are opened fresh by path and are only ever appended to, never
    /// rewritten in place, so a concurrent append cannot corrupt a read.
    /// A pathologically fast *second* rotation while this replay is still in
    /// flight (the active file filling up again before this function reaches
    /// it) can shift what `terminal.log` refers to on disk; live streaming is
    /// unaffected since it carries bytes directly rather than reading files.
    async fn replay(
        &self,
        write: &mut OwnedWriteHalf,
        snapshot: TerminalSnapshot,
        from: u64,
        through: u64,
    ) -> Result<(), Error> {
        let mut cursor = from;
        if let Some((start, len)) = snapshot.previous {
            cursor = self
                .replay_file(
                    write,
                    &self.dir.join(TERMINAL_LOG_ROTATED_FILE),
                    start,
                    len,
                    cursor,
                    through,
                )
                .await?;
        }
        let active_len = snapshot.total_bytes - snapshot.active_start_offset;
        self.replay_file(
            write,
            &self.dir.join(TERMINAL_LOG_FILE),
            snapshot.active_start_offset,
            active_len,
            cursor,
            through,
        )
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn replay_file(
        &self,
        write: &mut OwnedWriteHalf,
        path: &Path,
        file_start: u64,
        file_len: u64,
        cursor: u64,
        through: u64,
    ) -> Result<u64, Error> {
        let file_end = file_start + file_len;
        let read_from = cursor.max(file_start);
        let read_through = through.min(file_end);
        if read_from >= read_through {
            return Ok(cursor);
        }
        let mut file = File::open(path).await?;
        file.seek(io::SeekFrom::Start(read_from - file_start))
            .await?;
        let mut remaining = read_through - read_from;
        let mut position = read_from;
        let mut buffer = vec![0_u8; TERMINAL_READ_CHUNK];
        while remaining > 0 {
            let want = usize::try_from(remaining.min(TERMINAL_READ_CHUNK as u64))
                .expect("chunk size fits usize");
            file.read_exact(&mut buffer[..want]).await?;
            send_terminal_output(write, position, &buffer[..want]).await?;
            position += want as u64;
            remaining -= want as u64;
        }
        Ok(position.max(cursor))
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
    let terminal_mode = config.terminal.is_some();
    let prepared = prepare_runtime(&config.runtime_dir, terminal_mode).await?;
    let (stop_tx, stop_rx) = mpsc::channel(16);
    let (terminal_tx, terminal_rx) = if terminal_mode {
        let (tx, rx) = mpsc::channel(TERMINAL_COMMAND_CAPACITY);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let state = Arc::new(RuntimeState {
        run_id: config.run_id.clone(),
        runner_instance_id: config.runner_instance_id.clone(),
        log: Arc::clone(&prepared.log),
        terminal_log: prepared.terminal_log.clone(),
        stop_tx,
        terminal_tx,
        accepted_stops: Mutex::new(HashMap::new()),
        shutdown_tx,
    });
    let terminal = prepared.terminal_log.clone().zip(terminal_rx);

    let mut server = tokio::spawn(serve(
        prepared.listener,
        Arc::clone(&state),
        shutdown_rx.clone(),
    ));
    let mut supervisor = tokio::spawn(supervise(
        config,
        Arc::clone(&prepared.log),
        terminal,
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
    if config
        .startup_input
        .as_ref()
        .is_some_and(|input| input.len() > MAX_STARTUP_STDIN_BYTES)
    {
        return Err(Error::InvalidArguments(format!(
            "startup stdin exceeds the {MAX_STARTUP_STDIN_BYTES}-byte limit"
        )));
    }
    if config.terminal.is_some() && config.startup_input.is_some() {
        return Err(Error::InvalidArguments(
            "terminal mode does not accept startup stdin; interactive programs take input from \
             the operator via TerminalInput"
                .into(),
        ));
    }
    let metadata = std::fs::metadata(&config.cwd)
        .map_err(|error| Error::InvalidArguments(format!("invalid cwd: {error}")))?;
    if !metadata.is_dir() {
        return Err(Error::InvalidArguments("cwd is not a directory".into()));
    }
    Ok(())
}

/// Creates `runtime_dir` fresh (mode `0700`), or -- new for resident
/// sessions, see `factoryd::execution::spawn_session_for_agent` -- adopts
/// one the daemon already created and staged a `hook.token` file into
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

async fn prepare_runtime(
    runtime_dir: &Path,
    terminal_mode: bool,
) -> Result<PreparedRuntime, Error> {
    create_or_adopt_private_runtime_dir(runtime_dir)?;
    let spool_path = runtime_dir.join("events.ndjson");
    let socket_path = runtime_dir.join("control.sock");
    let terminal_log_path = runtime_dir.join(TERMINAL_LOG_FILE);
    let setup = (|| -> io::Result<_> {
        let spool = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .mode(0o600)
            .open(&spool_path)?;
        let terminal_log_file = terminal_mode
            .then(|| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .read(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&terminal_log_path)
            })
            .transpose()?;
        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
        Ok((spool, terminal_log_file, listener))
    })();
    let (spool, terminal_log_file, listener) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_file(&spool_path);
            let _ = std::fs::remove_file(&terminal_log_path);
            let _ = std::fs::remove_dir(runtime_dir);
            return Err(error.into());
        }
    };
    let terminal_log = terminal_log_file.map(|file| {
        TerminalLog::new(
            runtime_dir.to_path_buf(),
            MAX_TERMINAL_LOG_BYTES,
            File::from_std(file),
        )
    });
    Ok(PreparedRuntime {
        listener,
        log: EventLog::new(spool_path.clone(), File::from_std(spool)),
        terminal_log,
        socket_path,
    })
}

/// Removes the control socket only. `events.ndjson` and, in terminal mode,
/// `terminal.log`/`terminal.log.1` are retained private per-run logs: they
/// are never published as events, but the operator can inspect them (via
/// `GetRunTerminal` or `AttachTerminal`) even after the run has been
/// acknowledged and this process has exited. The runtime directory itself is
/// therefore also retained; nothing above the runner ever deletes it.
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
        RunnerRequest::AttachTerminal { since_offset } => {
            let Some(terminal_log) = state.terminal_log.as_ref() else {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(
                        RunnerErrorCode::InvalidRequest,
                        "run was not launched in terminal mode",
                    ),
                )
                .await;
            };
            attach_terminal_connection(&mut write_half, terminal_log, shutdown, since_offset).await
        }
        RunnerRequest::TerminalInput { bytes } => {
            let Some(terminal_tx) = state.terminal_tx.clone() else {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(
                        RunnerErrorCode::InvalidRequest,
                        "run was not launched in terminal mode",
                    ),
                )
                .await;
            };
            let decoded = match decode_terminal_bytes(&bytes) {
                Ok(decoded) => decoded,
                Err(_) => {
                    return send_control_error(
                        &mut write_half,
                        ControlError::new(
                            RunnerErrorCode::InvalidRequest,
                            "terminal input is not valid base64",
                        ),
                    )
                    .await;
                }
            };
            if decoded.len() > MAX_TERMINAL_INPUT_BYTES {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(
                        RunnerErrorCode::InvalidRequest,
                        format!("terminal input exceeds {MAX_TERMINAL_INPUT_BYTES} bytes"),
                    ),
                )
                .await;
            }
            send_terminal_command(
                &mut write_half,
                &terminal_tx,
                TerminalCommandKind::Input(decoded),
                "terminal-input",
            )
            .await
        }
        RunnerRequest::ResizeTerminal { cols, rows } => {
            let Some(terminal_tx) = state.terminal_tx.clone() else {
                return send_control_error(
                    &mut write_half,
                    ControlError::new(
                        RunnerErrorCode::InvalidRequest,
                        "run was not launched in terminal mode",
                    ),
                )
                .await;
            };
            send_terminal_command(
                &mut write_half,
                &terminal_tx,
                TerminalCommandKind::Resize { cols, rows },
                "resize-terminal",
            )
            .await
        }
    }
}

/// Forwards a `TerminalInput`/`ResizeTerminal` command to the supervisor and
/// relays its outcome as a `CommandAck` (with a fixed, non-idempotency
/// command ID: unlike `Stop`, these are not deduplicated retries) or error.
async fn send_terminal_command(
    write: &mut OwnedWriteHalf,
    terminal_tx: &mpsc::Sender<TerminalCommand>,
    kind: TerminalCommandKind,
    command_id: &str,
) -> Result<(), Error> {
    let (response, received) = oneshot::channel();
    if terminal_tx
        .send(TerminalCommand { kind, response })
        .await
        .is_err()
    {
        return send_control_error(
            write,
            ControlError::new(
                RunnerErrorCode::Conflict,
                "agent process is already terminal",
            ),
        )
        .await;
    }
    match received.await {
        Ok(Ok(())) => {
            send_frame(
                write,
                &RunnerFrame::CommandAck {
                    protocol_version: RUNNER_PROTOCOL_VERSION,
                    command_id: command_id.to_owned(),
                },
            )
            .await
        }
        Ok(Err(error)) => send_control_error(write, error).await,
        Err(_) => {
            send_control_error(
                write,
                ControlError::new(RunnerErrorCode::Internal, "terminal supervisor disappeared"),
            )
            .await
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

/// Streams retained-then-live PTY output to one attached client.
///
/// Subscribes to the live broadcast before taking the retained-log snapshot,
/// exactly like [`subscribe_connection`], so no byte can be missed between
/// replay and the live feed. If the client falls behind (`Lagged`), it is
/// dropped rather than resynchronized: it can reattach with a fresh
/// `since_offset` covering whatever it still needs.
async fn attach_terminal_connection(
    write: &mut OwnedWriteHalf,
    terminal_log: &TerminalLog,
    mut shutdown: watch::Receiver<bool>,
    since_offset: u64,
) -> Result<(), Error> {
    let mut chunks = terminal_log.subscribe();
    let snapshot = terminal_log.snapshot().await;
    if since_offset > snapshot.total_bytes {
        return send_control_error(
            write,
            ControlError::new(
                RunnerErrorCode::InvalidRequest,
                "terminal offset is ahead of the live head",
            ),
        )
        .await;
    }
    if since_offset < snapshot.oldest_retained_offset {
        return send_control_error(
            write,
            ControlError::new(
                RunnerErrorCode::InvalidRequest,
                "terminal offset has been rotated out of the retained window",
            ),
        )
        .await;
    }
    terminal_log
        .replay(write, snapshot, since_offset, snapshot.total_bytes)
        .await?;
    let mut delivered = snapshot.total_bytes;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            received = chunks.recv() => match received {
                Ok(chunk) if chunk.offset + chunk.bytes.len() as u64 <= delivered => {}
                Ok(chunk) if chunk.offset == delivered => {
                    send_terminal_output(write, chunk.offset, &chunk.bytes).await?;
                    delivered += chunk.bytes.len() as u64;
                }
                Ok(_) => {
                    return send_control_error(
                        write,
                        ControlError::new(
                            RunnerErrorCode::Internal,
                            "terminal stream desynchronized; reattach from the last offset",
                        ),
                    )
                    .await;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    return send_control_error(
                        write,
                        ControlError::new(
                            RunnerErrorCode::Conflict,
                            "terminal subscriber fell behind; reattach from the last offset",
                        ),
                    )
                    .await;
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

async fn send_terminal_output(
    write: &mut OwnedWriteHalf,
    offset: u64,
    bytes: &[u8],
) -> Result<(), Error> {
    send_frame(
        write,
        &RunnerFrame::TerminalOutput {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            offset,
            bytes: encode_terminal_bytes(bytes),
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
    terminal: Option<(Arc<TerminalLog>, mpsc::Receiver<TerminalCommand>)>,
    stops: mpsc::Receiver<StopCommand>,
    runner_shutdown: watch::Receiver<bool>,
) -> Result<SupervisionOutcome, Error> {
    if *runner_shutdown.borrow() {
        return Ok(SupervisionOutcome::RunnerSignalled);
    }
    match terminal {
        Some((terminal_log, terminal_commands)) => {
            let size = config
                .terminal
                .expect("a terminal log is only prepared for a terminal-mode config");
            supervise_terminal(
                config,
                size,
                log,
                terminal_log,
                stops,
                terminal_commands,
                runner_shutdown,
            )
            .await
        }
        None => supervise_piped(config, log, stops, runner_shutdown).await,
    }
}

async fn supervise_piped(
    config: Config,
    log: Arc<EventLog>,
    mut stops: mpsc::Receiver<StopCommand>,
    mut runner_shutdown: watch::Receiver<bool>,
) -> Result<SupervisionOutcome, Error> {
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

/// Command sent to the dedicated PTY control thread, which is the sole owner
/// of the PTY master handle and its writer for the run's whole lifetime.
enum PtyControl {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// Spawns the agent under a PTY and supervises it exactly like
/// [`supervise_piped`] (same stop/kill/grace/runner-shutdown handling reused
/// via the same helper functions), except output is raw retained bytes
/// instead of decoded `RunnerEvent::Output` text, input flows from
/// `TerminalInput` instead of `startup_input`, and resize commands reach the
/// child's controlling terminal.
///
/// PTY I/O is fundamentally blocking (`portable_pty` exposes synchronous
/// `Read`/`Write`), so it is bridged onto three dedicated OS threads: one
/// blocked reading PTY output, one blocked owning the PTY master (writes and
/// resizes), and one blocked in `waitpid` for the child's exit. Termination
/// signals are still sent from this async task directly, by process group,
/// reusing [`begin_group_termination`]/[`signal_process_group`] exactly as
/// pipe mode does; the wait thread only observes the outcome.
async fn supervise_terminal(
    config: Config,
    size: TerminalSize,
    log: Arc<EventLog>,
    terminal_log: Arc<TerminalLog>,
    mut stops: mpsc::Receiver<StopCommand>,
    mut terminal_commands: mpsc::Receiver<TerminalCommand>,
    mut runner_shutdown: watch::Receiver<bool>,
) -> Result<SupervisionOutcome, Error> {
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(pair) => pair,
        Err(error) => {
            log.append_lifecycle(
                RunnerEvent::SpawnFailed {
                    message: truncate_utf8(
                        format!("failed to open pty: {error}"),
                        MAX_RUNNER_ERROR_BYTES,
                    ),
                },
                true,
            )
            .await?;
            return Ok(SupervisionOutcome::AwaitAcknowledgement);
        }
    };

    let mut builder = CommandBuilder::new(&config.program);
    builder.args(&config.arguments);
    builder.cwd(&config.cwd);
    let spawned = pair.slave.spawn_command(builder);
    drop(pair.slave);
    let child = match spawned {
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
    let child_pid = child
        .process_id()
        .ok_or_else(|| Error::Task("spawned pty child has no process ID".into()))?;
    drop(child);
    let pid = Pid::from_raw(
        i32::try_from(child_pid).map_err(|_| Error::Task("child PID overflow".into()))?,
    )
    .ok_or_else(|| Error::Task("child PID was zero".into()))?;
    let mut process_group = ProcessGroupGuard::new(pid);
    log.append_lifecycle(RunnerEvent::Started { child_pid }, false)
        .await?;

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| Error::Task(format!("failed to clone pty reader: {error}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| Error::Task(format!("failed to take pty writer: {error}")))?;
    let master = pair.master;

    let (output_tx, mut output_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || pty_reader_loop(reader, output_tx));
    let (control_tx, control_rx) = mpsc::channel::<PtyControl>(TERMINAL_COMMAND_CAPACITY);
    std::thread::spawn(move || pty_control_loop(master, writer, control_rx));
    let (exit_tx, mut exit_rx) = oneshot::channel();
    std::thread::spawn(move || {
        let result = waitpid(Some(pid), WaitOptions::empty()).map_err(io::Error::from);
        let _ = exit_tx.send(result);
    });

    let mut kill_deadline: Option<Pin<Box<Sleep>>> = None;
    let mut runner_signalled = false;
    let mut reader_open = true;
    let status = loop {
        tokio::select! {
            result = &mut exit_rx => {
                let waited = result
                    .map_err(|_| Error::Task("pty wait thread disappeared".into()))??;
                let Some((_, status)) = waited else {
                    return Err(Error::Task(
                        "waitpid returned no status for a blocking wait".into(),
                    ));
                };
                break status;
            }
            chunk = output_rx.recv(), if reader_open => match chunk {
                Some(bytes) => terminal_log.append(bytes).await?,
                None => reader_open = false,
            },
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
            command = terminal_commands.recv() => {
                let Some(command) = command else {
                    continue;
                };
                let control = match command.kind {
                    TerminalCommandKind::Input(bytes) => PtyControl::Write(bytes),
                    TerminalCommandKind::Resize { cols, rows } => PtyControl::Resize { cols, rows },
                };
                let outcome = control_tx.send(control).await.map_err(|_| {
                    ControlError::new(RunnerErrorCode::Internal, "pty control thread disappeared")
                });
                let _ = command.response.send(outcome);
            }
        }
    };
    let status = wait_status_to_exit(status);

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

    if reader_open {
        let drain_result = timeout(POST_KILL_DRAIN_TIMEOUT, async {
            while let Some(bytes) = output_rx.recv().await {
                terminal_log.append(bytes).await?;
            }
            Ok::<(), Error>(())
        })
        .await;
        match drain_result {
            Ok(result) => result?,
            Err(_) => {
                // The reader thread is still blocked (or its bytes are still
                // being appended); we cannot forcibly cancel a blocking OS
                // thread without unsafe, so we stop waiting for it here. It
                // will finish and exit on its own once the pty fully closes.
            }
        }
    }
    log.append_lifecycle(
        RunnerEvent::Exited {
            exit_code: status.0,
            signal: status.1,
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

fn wait_status_to_exit(status: rustix::process::WaitStatus) -> (Option<i32>, Option<i32>) {
    (status.exit_status(), status.terminating_signal())
}

fn pty_reader_loop(mut reader: Box<dyn std::io::Read + Send>, output_tx: mpsc::Sender<Vec<u8>>) {
    let mut buffer = vec![0_u8; TERMINAL_READ_CHUNK];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        if output_tx.blocking_send(buffer[..read].to_vec()).is_err() {
            break;
        }
    }
}

fn pty_control_loop(
    master: Box<dyn MasterPty + Send>,
    mut writer: Box<dyn std::io::Write + Send>,
    mut control_rx: mpsc::Receiver<PtyControl>,
) {
    while let Some(command) = control_rx.blocking_recv() {
        match command {
            PtyControl::Write(bytes) => {
                let _ = writer.write_all(&bytes);
                let _ = writer.flush();
            }
            PtyControl::Resize { cols, rows } => {
                let _ = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }
    }
    drop(master);
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
        Config, DecodedChunk, Error, MAX_STARTUP_STDIN_BYTES, TERMINAL_LOG_FILE,
        TERMINAL_LOG_ROTATED_FILE, TerminalLog, decode_available, validate_config,
    };
    use factory_core::{
        RunId, RunnerInstanceId,
        runner::{MAX_RUNNER_OUTPUT_TEXT_BYTES, RunnerFrame, decode_terminal_bytes},
    };
    use std::os::unix::fs::OpenOptionsExt as _;
    use tokio::{
        fs::File,
        io::{AsyncBufReadExt, BufReader},
        net::UnixStream,
    };

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
            terminal: None,
        })
        .unwrap_err();
        assert!(matches!(error, Error::InvalidArguments(message) if message.contains("stdin")));
    }

    #[test]
    fn config_rejects_startup_input_combined_with_terminal_mode() {
        let directory = tempfile::tempdir().unwrap();
        let error = validate_config(&Config {
            run_id: RunId::try_from("run-1").unwrap(),
            runner_instance_id: RunnerInstanceId::try_from("instance-1").unwrap(),
            runtime_dir: directory.path().join("run"),
            cwd: directory.path().to_owned(),
            startup_input: Some(b"hello".to_vec()),
            program: "/bin/true".into(),
            arguments: Vec::new(),
            terminal: Some(factory_core::runner::TerminalSize { cols: 80, rows: 24 }),
        })
        .unwrap_err();
        assert!(
            matches!(error, Error::InvalidArguments(message) if message.contains("terminal mode"))
        );
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

    fn open_terminal_log(dir: &std::path::Path, max_bytes: u64) -> std::sync::Arc<TerminalLog> {
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .mode(0o600)
            .open(dir.join(TERMINAL_LOG_FILE))
            .unwrap();
        TerminalLog::new(dir.to_path_buf(), max_bytes, File::from_std(file))
    }

    #[tokio::test]
    async fn terminal_log_rotates_exactly_once_dropping_the_oldest_generation() {
        let directory = tempfile::tempdir().unwrap();
        let log = open_terminal_log(directory.path(), 4);

        log.append(b"ABCD".to_vec()).await.unwrap(); // exactly fills the active file
        log.append(b"EFGH".to_vec()).await.unwrap(); // forces the first rotation
        log.append(b"IJKL".to_vec()).await.unwrap(); // forces a second rotation

        let snapshot = log.snapshot().await;
        assert_eq!(snapshot.total_bytes, 12);
        assert_eq!(
            snapshot.oldest_retained_offset, 4,
            "the first generation was dropped"
        );
        assert_eq!(snapshot.active_start_offset, 8);
        assert_eq!(snapshot.previous, Some((4, 4)));
        assert_eq!(
            std::fs::read(directory.path().join(TERMINAL_LOG_ROTATED_FILE)).unwrap(),
            b"EFGH"
        );
        assert_eq!(
            std::fs::read(directory.path().join(TERMINAL_LOG_FILE)).unwrap(),
            b"IJKL"
        );
    }

    #[tokio::test]
    async fn terminal_log_replay_stitches_the_rotated_and_active_files() {
        let directory = tempfile::tempdir().unwrap();
        let log = open_terminal_log(directory.path(), 4);
        log.append(b"ABCD".to_vec()).await.unwrap();
        log.append(b"EFGH".to_vec()).await.unwrap();
        log.append(b"IJKL".to_vec()).await.unwrap();
        let snapshot = log.snapshot().await;

        let (mut client, server) = UnixStream::pair().unwrap();
        let (_read, mut write) = server.into_split();
        log.replay(&mut write, snapshot, 4, snapshot.total_bytes)
            .await
            .unwrap();
        drop(write);

        let mut reader = BufReader::new(&mut client);
        let mut collected = Vec::new();
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap() > 0 {
            let frame: RunnerFrame = serde_json::from_str(line.trim_end()).unwrap();
            let RunnerFrame::TerminalOutput { bytes, .. } = frame else {
                panic!("expected terminal output, got {frame:?}");
            };
            collected.extend(decode_terminal_bytes(&bytes).unwrap());
            line.clear();
        }
        assert_eq!(collected, b"EFGHIJKL");
    }
}
