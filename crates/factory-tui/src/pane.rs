//! One terminal pane: either a local child process under a PTY (`--dev-local-pty`, offline
//! testing only — see `README.md`), or a live daemon-proxied session attached over the wire
//! contract (`AttachTerminal`/`TerminalInput`/`ResizeTerminal`, keyed by `session_id`). Either
//! way, output is fed into a `vt100::Parser` for rendering and (local-PTY only) scanned for
//! terminal queries (see `query.rs`) — everything downstream of "bytes arrived" is identical
//! regardless of backend, per the spike's own forward-looking note in `SPIKE.md`.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{
    Child, CommandBuilder, MasterPty, NativePtySystem, PtyPair, PtySize, PtySystem,
};
// Re-exported by tui-term rather than depended on directly, so we always use the exact vt100
// version tui-term was built against (see the Cargo.toml comment on this crate's dependencies).
use tui_term::vt100;

use factory_core::local::{AttachRefusal, LocalRequest, ServerFrame};
use factory_core::runner::{decode_terminal_bytes, terminal_generation_is_contiguous};
use factory_core::{ProjectId, RunnerInstanceId, SessionId};

use crate::attach::{self, AttachConnection};
use crate::keys::KeyContext;
use crate::mouse::TerminalMouseContext;
use crate::query::QueryResponder;

/// How long after a local-PTY spawn we keep appending raw PTY bytes to the debug log, per the
/// spike's "detect which queries claude/codex actually send on startup" requirement.
const DEBUG_LOG_WINDOW: Duration = Duration::from_secs(6);

/// Which backend a [`Pane`] is reading from — surfaced so `ui/` can show a small "[dev-local-pty]"
/// badge and README's "what's stubbed" story stays honest about which pane is real.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneKind {
    LocalPty,
    Daemon,
}

/// Durable identity captured by one attach attempt. A session id alone is not sufficient: the
/// daemon can replace the runner while retaining the session row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneIdentity {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub runner_instance_id: Option<RunnerInstanceId>,
}

enum Backend {
    LocalPty {
        writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn Child + Send + Sync>,
        exited: Arc<AtomicBool>,
    },
    Daemon {
        attach: AttachConnection,
        input_tx: mpsc::Sender<Vec<u8>>,
        resize_tx: mpsc::Sender<(u16, u16)>,
        observation: Arc<(Mutex<AttachState>, Condvar)>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaneObservation {
    Connecting,
    Attached,
    /// The local child reached EOF. This is a terminal pane state, not an attach
    /// failure: the pane remains renderable and must not be respawned.
    LocalPtyExited,
    Disconnected,
    AttachRefused(AttachRefusal),
    Error(String),
}

impl PaneObservation {
    #[must_use]
    pub fn is_attached(&self) -> bool {
        matches!(self, Self::Attached)
    }

    #[cfg(test)]
    #[must_use]
    fn is_attach_refusal(&self) -> bool {
        matches!(self, Self::AttachRefused(_))
    }
}

struct AttachState {
    observation: PaneObservation,
    finished: bool,
}

pub struct Pane {
    pub title: String,
    pub command: Vec<String>,
    pub kind: PaneKind,
    parser: Arc<Mutex<vt100::Parser>>,
    /// Set by a reader thread whenever new bytes were processed; cleared by the render loop.
    dirty: Arc<AtomicBool>,
    rows: u16,
    cols: u16,
    identity: Option<PaneIdentity>,
    backend: Backend,
}

impl Pane {
    /// Spawns `command` under a new local PTY sized `cols x rows`. `--dev-local-pty` only — see
    /// `README.md`. `debug_log` is an optional path to append raw PTY output bytes to for
    /// `DEBUG_LOG_WINDOW` after spawn (see `SPIKE.md` "Terminal-query handling").
    pub fn spawn(
        title: impl Into<String>,
        command: &[String],
        rows: u16,
        cols: u16,
        debug_log: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let pty_system = NativePtySystem::default();
        let pair: PtyPair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let program = command.first().cloned().unwrap_or_else(|| "bash".into());
        let mut cmd = CommandBuilder::new(program);
        for arg in command.iter().skip(1) {
            cmd.arg(arg);
        }
        // Inherit the operator's real environment (HOME/PATH in particular) so `claude`/`codex`
        // find their subscription login, per the brief's "no API keys" rule.
        cmd.env("TERM", "xterm-256color");
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }

        let child = pair.slave.spawn_command(cmd)?;
        // Dropping the slave end in this process is required: otherwise we'd hold our own
        // extra reference to the child's controlling terminal open, and the child would never
        // see EOF/HUP behavior correctly on exit.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let writer = Arc::new(Mutex::new(writer));
        let exited = Arc::new(AtomicBool::new(false));

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
        let dirty = Arc::new(AtomicBool::new(true));

        spawn_local_reader_thread(
            reader,
            Arc::clone(&parser),
            Arc::clone(&writer),
            Arc::clone(&dirty),
            Arc::clone(&exited),
            debug_log,
        );

        Ok(Self {
            title: title.into(),
            command: command.to_vec(),
            kind: PaneKind::LocalPty,
            parser,
            dirty,
            rows,
            cols,
            identity: None,
            backend: Backend::LocalPty {
                writer,
                master: pair.master,
                child,
                exited,
            },
        })
    }

    /// Attaches to a live daemon-proxied session over the wire contract: opens
    /// `AttachTerminal { since_offset: 0 }` on a dedicated connection (see `attach.rs` for why not
    /// the multiplexed one), replays the retained log, then streams live bytes into the same
    /// `vt100::Parser` a local-PTY pane would use. Reattach is identical — always from offset 0,
    /// letting the retained log rebuild the correct current screen (per the wire contract: "no
    /// partial resume in this client").
    pub fn attach(
        socket: PathBuf,
        project_id: ProjectId,
        session_id: SessionId,
        runner_instance_id: Option<RunnerInstanceId>,
        title: impl Into<String>,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<Self> {
        let (attach_conn, reader_half) = AttachConnection::open(
            &socket,
            LocalRequest::AttachTerminal {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                since_offset: 0,
                mode: factory_core::runner::TerminalAttachMode::Tail,
            },
        )?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
        let dirty = Arc::new(AtomicBool::new(true));
        let observation = Arc::new((
            Mutex::new(AttachState {
                observation: PaneObservation::Connecting,
                finished: false,
            }),
            Condvar::new(),
        ));

        spawn_attach_reader_thread(
            reader_half,
            Arc::clone(&parser),
            Arc::clone(&dirty),
            Arc::clone(&observation),
        );

        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
        let (resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>();
        attach::spawn_input_worker(
            socket.clone(),
            project_id.clone(),
            session_id.clone(),
            input_rx,
        );
        attach::spawn_resize_worker(socket, project_id.clone(), session_id.clone(), resize_rx);

        Ok(Self {
            title: title.into(),
            command: Vec::new(),
            kind: PaneKind::Daemon,
            parser,
            dirty,
            // Sentinel: forces the first `resize()` call to actually send, per the wire
            // contract's "initial + on resize" — see that method's doc comment.
            rows: 0,
            cols: 0,
            identity: Some(PaneIdentity {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                runner_instance_id,
            }),
            backend: Backend::Daemon {
                attach: attach_conn,
                input_tx,
                resize_tx,
                observation,
            },
        })
    }

    #[must_use]
    pub fn dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// Marks the pane dirty without checking - used to force a redraw (e.g. right after
    /// resizing, before new bytes have arrived).
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        let parser = self.parser.lock().expect("parser mutex poisoned");
        f(parser.screen())
    }

    /// Locks the parser directly. Used for rendering, where the returned `vt100::Screen`
    /// reference must outlive a `ratatui` widget's `render_widget` call - a scope that a
    /// `with_screen`-style closure can't express without capturing `frame` by move (breaking
    /// later loop iterations that also need to draw to it).
    pub fn lock_parser(&self) -> std::sync::MutexGuard<'_, vt100::Parser> {
        self.parser.lock().expect("parser mutex poisoned")
    }

    #[must_use]
    pub fn key_context(&self) -> KeyContext {
        self.with_screen(|screen| KeyContext {
            application_cursor: screen.application_cursor(),
            application_keypad: screen.application_keypad(),
        })
    }

    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.with_screen(vt100::Screen::bracketed_paste)
    }

    /// Mouse mode and encoding requested by the child through DECSET. `main.rs` treats `None`
    /// mode as a hard do-not-forward boundary, rather than inferring support from pane focus.
    #[must_use]
    pub fn mouse_context(&self) -> TerminalMouseContext {
        self.with_screen(|screen| TerminalMouseContext {
            mode: screen.mouse_protocol_mode(),
            encoding: screen.mouse_protocol_encoding(),
            scrolled_back: screen.scrollback() > 0,
        })
    }

    /// Writes raw, already-encoded terminal input to the child (local PTY) or forwards it to the
    /// daemon as a `TerminalInput` request (daemon-attached) — best-effort either way. Daemon
    /// input holds the observation lock across the readiness check and enqueue, so a refusal
    /// already observed cannot leak bytes through a stale caller-side check.
    pub fn write_input(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }
        if !self.observation().is_attached() {
            return false;
        }
        match &self.backend {
            Backend::LocalPty { writer, .. } => {
                if self.scroll_offset() > 0 {
                    self.scroll_reset();
                }
                if let Ok(mut writer) = writer.lock() {
                    writer.write_all(bytes).is_ok() && writer.flush().is_ok()
                } else {
                    false
                }
            }
            Backend::Daemon {
                input_tx,
                observation,
                ..
            } => {
                let (state, _) = &**observation;
                let Ok(state) = state.lock() else {
                    return false;
                };
                if !state.observation.is_attached() {
                    return false;
                }
                if self.scroll_offset() > 0 {
                    self.scroll_reset();
                }
                input_tx.send(bytes.to_vec()).is_ok()
            }
        }
    }

    /// Resizes both the PTY (local) or sends `ResizeTerminal` (daemon) and the `vt100` model. A
    /// no-op if the size didn't change — except for a freshly `attach`ed pane, whose `rows`/`cols`
    /// start at the sentinel `(0, 0)` specifically so the very first call here always sends,
    /// satisfying the wire contract's "sends... size via `ResizeTerminal` (initial + on resize)".
    pub fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()> {
        if rows == self.rows && cols == self.cols {
            return Ok(());
        }
        self.rows = rows;
        self.cols = cols;
        match &mut self.backend {
            Backend::LocalPty { master, .. } => {
                master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })?;
            }
            Backend::Daemon { resize_tx, .. } => {
                let _ = resize_tx.send((cols, rows));
            }
        }
        if let Ok(mut parser) = self.parser.lock() {
            // vt100 truncates rows from the bottom when the live grid shrinks. Re-feed one
            // bounded, formatted snapshot of the visible screen after changing the viewport so
            // a prompt at the lower edge survives narrow/wide TUI resizing. The daemon attach
            // observation remains authoritative and unchanged by this local render operation.
            let scrollback = parser.screen().scrollback();
            if scrollback > 0 {
                // vt100's visible-row iterator cannot snapshot an offset beyond the viewport.
                parser.set_scrollback(0);
            }
            let formatted = parser.screen().contents_formatted();
            parser.set_size(rows, cols);
            parser.process(&formatted);
            if scrollback > 0 {
                parser.set_scrollback(scrollback.min(usize::from(rows)));
            }
        }
        self.mark_dirty();
        Ok(())
    }

    /// The one authoritative observation of daemon attach state. In particular, a typed refusal
    /// replaces `Attached` even when it arrives after the first terminal frame.
    #[must_use]
    pub fn observation(&self) -> PaneObservation {
        match &self.backend {
            Backend::LocalPty { exited, .. } => {
                if exited.load(Ordering::Acquire) {
                    PaneObservation::LocalPtyExited
                } else {
                    PaneObservation::Attached
                }
            }
            Backend::Daemon { observation, .. } => self_observation(observation),
        }
    }

    #[must_use]
    pub fn identity(&self) -> Option<&PaneIdentity> {
        self.identity.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn wait_for_attach_outcome(&self, timeout: Duration) -> bool {
        let Backend::Daemon { observation, .. } = &self.backend else {
            return true;
        };
        let (state, signal) = &**observation;
        let state = state.lock().expect("attach state mutex poisoned");
        signal
            .wait_timeout_while(state, timeout, |state| !state.finished)
            .expect("attach state mutex poisoned")
            .0
            .finished
    }

    #[cfg(test)]
    pub(crate) fn wait_until_ready(&self, timeout: Duration) -> bool {
        let Backend::Daemon { observation, .. } = &self.backend else {
            return self.observation().is_attached();
        };
        let (state, signal) = &**observation;
        let state = state.lock().expect("attach state mutex poisoned");
        signal
            .wait_timeout_while(state, timeout, |state| !state.observation.is_attached())
            .expect("attach state mutex poisoned")
            .0
            .observation
            .is_attached()
    }

    #[cfg(test)]
    pub(crate) fn wait_for_attach_refusal(&self, timeout: Duration) -> bool {
        let Backend::Daemon { observation, .. } = &self.backend else {
            return false;
        };
        let (state, signal) = &**observation;
        let state = state.lock().expect("attach state mutex poisoned");
        signal
            .wait_timeout_while(state, timeout, |state| {
                !state.observation.is_attach_refusal()
            })
            .expect("attach state mutex poisoned")
            .0
            .observation
            .is_attach_refusal()
    }

    /// Ends this pane: kills the local child (`--dev-local-pty` only — never a real agent), or,
    /// for a daemon-attached pane, **only** detaches (shuts the attach socket down so its reader
    /// thread exits promptly). Detaching a daemon-attached pane must never stop the session it
    /// was watching — "closing/crashing/rebuilding the TUI must not stop agents" is a hard
    /// constraint from the design brief, not a preference.
    pub fn kill(&mut self) {
        match &mut self.backend {
            Backend::LocalPty { child, .. } => {
                let _ = child.kill();
            }
            Backend::Daemon { attach, .. } => attach.shutdown(),
        }
    }

    // -- scrollback (AGENT.s PgUp/PgDn) ------------------------------------------------------

    /// How many scrollback lines are currently in view (0 = live tail). Reads straight from
    /// `vt100::Screen::scrollback`, which is already clamped to how much history actually exists.
    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.with_screen(vt100::Screen::scrollback)
    }

    /// Scrolls further back into history by `lines`. `vt100::Parser::set_scrollback` clamps
    /// internally to however much scrollback has actually accumulated, so over-scrolling is
    /// always safe.
    pub fn scroll_up(&self, lines: usize) {
        if let Ok(mut parser) = self.parser.lock() {
            // Bound one movement to one viewport: vt100 cannot render an offset beyond the
            // visible row count, and PgUp may deliberately request `usize::MAX`.
            let max_page = usize::from(parser.screen().size().0);
            let target = parser
                .screen()
                .scrollback()
                .saturating_add(lines)
                .min(max_page);
            parser.set_scrollback(target);
        }
        self.mark_dirty();
    }

    /// Scrolls back toward the live tail by `lines` (saturating at 0).
    pub fn scroll_down(&self, lines: usize) {
        if let Ok(mut parser) = self.parser.lock() {
            let target = parser.screen().scrollback().saturating_sub(lines);
            parser.set_scrollback(target);
        }
        self.mark_dirty();
    }

    /// Jumps back to the live tail.
    pub fn scroll_reset(&self) {
        if let Ok(mut parser) = self.parser.lock() {
            parser.set_scrollback(0);
        }
        self.mark_dirty();
    }
}

fn self_observation(observation: &Arc<(Mutex<AttachState>, Condvar)>) -> PaneObservation {
    observation
        .0
        .lock()
        .map(|state| state.observation.clone())
        .unwrap_or(PaneObservation::Disconnected)
}

fn spawn_local_reader_thread(
    mut reader: Box<dyn std::io::Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    dirty: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    debug_log: Option<PathBuf>,
) {
    std::thread::spawn(move || {
        let started = Instant::now();
        let mut debug_file = debug_log.and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        let mut responder = QueryResponder::new();
        let mut buf = [0u8; 32 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let chunk = &buf[..n];

            if let Some(file) = debug_file.as_mut() {
                if started.elapsed() < DEBUG_LOG_WINDOW {
                    let _ = file.write_all(chunk);
                } else {
                    debug_file = None;
                }
            }

            let cursor = if let Ok(mut parser) = parser.lock() {
                parser.process(chunk);
                dirty.store(true, Ordering::Release);
                parser.screen().cursor_position()
            } else {
                break;
            };

            let reply = responder.scan(chunk, cursor.0, cursor.1);
            if !reply.is_empty() {
                if let Ok(mut w) = writer.lock() {
                    let _ = w.write_all(&reply);
                    let _ = w.flush();
                }
            }
        }
        exited.store(true, Ordering::Release);
    });
}

fn spawn_attach_reader_thread(
    reader: std::os::unix::net::UnixStream,
    parser: Arc<Mutex<vt100::Parser>>,
    dirty: Arc<AtomicBool>,
    observation: Arc<(Mutex<AttachState>, Condvar)>,
) {
    std::thread::spawn(move || {
        let mut next_offset = None;
        let mut next_generation = None;
        attach::read_frames(reader, |frame| {
            match frame {
                ServerFrame::TerminalOutput {
                    generation,
                    offset,
                    bytes,
                    ..
                } => match decode_terminal_bytes(&bytes) {
                    Ok(decoded) => {
                        let generation_jump = next_generation.is_some_and(|expected| {
                            !terminal_generation_is_contiguous(expected, generation)
                        });
                        if next_offset.is_some_and(|expected| expected != offset)
                            || next_generation.is_some_and(|expected| generation < expected)
                            || generation_jump
                        {
                            let (state, signal) = &*observation;
                            if let Ok(mut state) = state.lock() {
                                state.observation = PaneObservation::Error(format!(
                                    "terminal output continuity broke at offset {offset}"
                                ));
                                state.finished = true;
                                signal.notify_all();
                            }
                            return false;
                        }
                        next_offset = Some(offset.saturating_add(decoded.len() as u64));
                        next_generation = Some(generation);
                        let (state, signal) = &*observation;
                        if let Ok(mut state) = state.lock() {
                            if matches!(state.observation, PaneObservation::Connecting) {
                                state.observation = PaneObservation::Attached;
                            }
                            state.finished = true;
                            signal.notify_all();
                        }
                        if let Ok(mut parser) = parser.lock() {
                            parser.process(&decoded);
                            dirty.store(true, Ordering::Release);
                        }
                    }
                    Err(error) => {
                        let (state, signal) = &*observation;
                        if let Ok(mut state) = state.lock() {
                            state.observation =
                                PaneObservation::Error(format!("invalid terminal bytes: {error}"));
                            state.finished = true;
                            signal.notify_all();
                        }
                    }
                },
                ServerFrame::TerminalAttachReady {
                    reset_prefix,
                    start_generation,
                    start_offset,
                    ..
                } => {
                    next_offset = Some(start_offset);
                    next_generation = Some(start_generation);
                    match decode_terminal_bytes(&reset_prefix) {
                        Ok(decoded) => {
                            let (state, signal) = &*observation;
                            if let Ok(mut state) = state.lock() {
                                if matches!(state.observation, PaneObservation::Connecting) {
                                    state.observation = PaneObservation::Attached;
                                }
                                state.finished = true;
                                signal.notify_all();
                            }
                            if let Ok(mut parser) = parser.lock() {
                                parser.process(&decoded);
                                dirty.store(true, Ordering::Release);
                            }
                        }
                        Err(error) => {
                            let (state, signal) = &*observation;
                            if let Ok(mut state) = state.lock() {
                                state.observation = PaneObservation::Error(format!(
                                    "invalid terminal state: {error}"
                                ));
                                state.finished = true;
                                signal.notify_all();
                            }
                            return false;
                        }
                    }
                }
                ServerFrame::TerminalAttachGap {
                    generation,
                    base_generation,
                    base_offset,
                    start_generation,
                    start_offset,
                    end_offset,
                    requested_generation,
                    requested_offset,
                    reason,
                    ..
                } => {
                    let (state, signal) = &*observation;
                    if let Ok(mut state) = state.lock() {
                        state.observation = PaneObservation::Error(format!(
                            "attach cursor unavailable: {reason}; retained generation {base_generation} offsets {base_offset}..{end_offset}, requested generation {requested_generation:?} offset {requested_offset}, generation {generation}, replay generation {start_generation} at {start_offset}"
                        ));
                        state.finished = true;
                        signal.notify_all();
                    }
                    return false;
                }
                ServerFrame::Response { response, .. } => match response {
                    factory_core::local::LocalResponse::AttachRefused { refusal } => {
                        let (state, signal) = &*observation;
                        if let Ok(mut state) = state.lock() {
                            state.observation = PaneObservation::AttachRefused(refusal);
                            state.finished = true;
                            signal.notify_all();
                        }
                        return false;
                    }
                    factory_core::local::LocalResponse::Error { message, .. } => {
                        let (state, signal) = &*observation;
                        if let Ok(mut state) = state.lock() {
                            state.observation = PaneObservation::Error(message);
                            state.finished = true;
                            signal.notify_all();
                        }
                        return false;
                    }
                    _ => {}
                },
                ServerFrame::Event { .. } => {}
            }
            true
        });
        dirty.store(true, Ordering::Release);
        let (state, signal) = &*observation;
        if let Ok(mut state) = state.lock() {
            if matches!(
                state.observation,
                PaneObservation::Connecting | PaneObservation::Attached
            ) {
                state.observation = PaneObservation::Disconnected;
            }
            state.finished = true;
            signal.notify_all();
        }
    });
}

/// The set of panes currently attached, keyed by session id — `main.rs` reconciles this against
/// `Board::desired_sessions()` every loop iteration; `ui::terminals`/`ui::focus` render whatever's
/// in it.
pub type PaneMap = std::collections::HashMap<SessionId, Pane>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::thread::JoinHandle;

    use factory_core::PROTOCOL_VERSION;
    use factory_core::local::{LocalResponse, RequestEnvelope};
    use factory_core::runner::encode_terminal_bytes;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::{Block, Borders};
    use tui_term::widget::PseudoTerminal;

    type AttachHandler = JoinHandle<Result<(), String>>;

    struct AttachFixture {
        socket: PathBuf,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        handlers: Arc<Mutex<Vec<AttachHandler>>>,
        _directory: tempfile::TempDir,
    }

    impl AttachFixture {
        fn start(output: Vec<u8>) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("factory.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let handlers = Arc::new(Mutex::new(Vec::new()));
            let thread_handlers = Arc::clone(&handlers);
            let thread = std::thread::spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let stop = Arc::clone(&thread_stop);
                            let output = output.clone();
                            let handler = std::thread::spawn(move || {
                                // The listening socket is nonblocking only so fixture teardown can
                                // wake it. Accepted streams must exercise ordinary blocking local
                                // API writes, including the full replay payload.
                                stream.set_nonblocking(false).map_err(|error| {
                                    format!("failed to make accepted stream blocking: {error}")
                                })?;
                                serve_attach_connection(stream, output, stop)
                            });
                            thread_handlers.lock().unwrap().push(handler);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                socket,
                stop,
                thread: Some(thread),
                handlers,
                _directory: directory,
            }
        }
    }

    impl Drop for AttachFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let _ = UnixStream::connect(&self.socket);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
            for handler in self.handlers.lock().unwrap().drain(..) {
                handler
                    .join()
                    .expect("attach fixture handler panicked")
                    .expect("attach fixture handler failed");
            }
        }
    }

    fn serve_attach_connection(
        mut stream: UnixStream,
        output: Vec<u8>,
        stop: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let mut request = String::new();
        let request_stream = stream
            .try_clone()
            .map_err(|error| format!("failed to clone accepted stream: {error}"))?;
        if BufReader::new(request_stream)
            .read_line(&mut request)
            .map_err(|error| format!("failed to read local request: {error}"))?
            == 0
        {
            return Ok(());
        }
        let envelope: RequestEnvelope = serde_json::from_str(&request)
            .map_err(|error| format!("failed to parse local request: {error}"))?;
        match envelope.request {
            LocalRequest::AttachTerminal { session_id, .. } => {
                send_frame(
                    &mut stream,
                    ServerFrame::TerminalAttachReady {
                        protocol_version: PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        generation: 0,
                        base_generation: 0,
                        base_offset: 0,
                        start_generation: 0,
                        start_offset: 0,
                        end_offset: output.len() as u64,
                        reset_prefix: encode_terminal_bytes(
                            b"\x1bc\x1b[?1049l\x1b[?2004l\x1b[0m\x1b[2J\x1b[H",
                        ),
                    },
                )?;
                for (offset, chunk) in output.chunks(7).scan(0_u64, |offset, chunk| {
                    let start = *offset;
                    *offset += chunk.len() as u64;
                    Some((start, chunk))
                }) {
                    send_frame(
                        &mut stream,
                        ServerFrame::TerminalOutput {
                            protocol_version: PROTOCOL_VERSION,
                            session_id: session_id.clone(),
                            generation: 0,
                            offset,
                            bytes: encode_terminal_bytes(chunk),
                        },
                    )?;
                }
                stream
                    .set_read_timeout(Some(Duration::from_millis(25)))
                    .map_err(|error| format!("failed to bound attach fixture read: {error}"))?;
                let mut discard = [0_u8; 1];
                while !stop.load(Ordering::Acquire) {
                    match stream.read(&mut discard) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(error) => {
                            return Err(format!("attach fixture read failed: {error}"));
                        }
                    }
                }
            }
            LocalRequest::ResizeTerminal { session_id, .. } => send_frame(
                &mut stream,
                ServerFrame::Response {
                    protocol_version: PROTOCOL_VERSION,
                    response: LocalResponse::TerminalResized { session_id },
                },
            )?,
            _ => {}
        }
        Ok(())
    }

    fn send_frame(stream: &mut UnixStream, frame: ServerFrame) -> Result<(), String> {
        let mut payload = serde_json::to_vec(&frame)
            .map_err(|error| format!("failed to encode server frame: {error}"))?;
        payload.push(b'\n');
        stream
            .write_all(&payload)
            .and_then(|()| stream.flush())
            .map_err(|error| format!("failed to send server frame: {error}"))
    }

    fn render_pane(pane: &mut Pane, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let block = Block::default().borders(Borders::ALL);
                let inner = block.inner(area);
                pane.resize(inner.height, inner.width).unwrap();
                let parser = pane.lock_parser();
                frame.render_widget(PseudoTerminal::new(parser.screen()).block(block), area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_nonempty_input_returns_scrollback_to_the_live_tail() {
        let mut pane = Pane::spawn("test", &["/bin/cat".to_owned()], 2, 8, None).unwrap();
        {
            let mut parser = pane.parser.lock().unwrap();
            parser.process(b"one\r\ntwo\r\nthree\r\n");
            parser.set_scrollback(1);
        }
        assert_eq!(pane.scroll_offset(), 1);

        pane.write_input(b"x");

        assert_eq!(pane.scroll_offset(), 0);
        pane.kill();
    }

    #[test]
    fn real_long_attach_renders_utf8_ansi_scrollback_and_live_prompt_after_resize() {
        let mut output = Vec::new();
        for line in 0..1_500 {
            output.extend_from_slice(
                format!("history-{line:04} café \x1b[32mansi-{line:04}\x1b[0m\r\n").as_bytes(),
            );
        }
        output.extend_from_slice(b"\x1b[1;34mLIVE-PROMPT\x1b[0m $ ");

        let fixture = AttachFixture::start(output);
        let mut pane = Pane::attach(
            fixture.socket.clone(),
            ProjectId::try_from("project-1").unwrap(),
            SessionId::try_from("session-1").unwrap(),
            None,
            "attached",
            8,
            48,
        )
        .unwrap();

        // Observing the parsed live prompt is the peer-consumption barrier: resize does not begin
        // merely because the fixture finished writing the replay.
        let deadline = Instant::now() + Duration::from_secs(15);
        while (!pane.observation().is_attached()
            || !pane.with_screen(|screen| screen.contents().contains("LIVE-PROMPT")))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(pane.observation(), PaneObservation::Attached);
        assert!(
            pane.with_screen(|screen| screen.contents().contains("LIVE-PROMPT")),
            "real attach replay never reached the live prompt"
        );

        let wide = render_pane(&mut pane, 48, 8);
        assert!(
            wide.contains("LIVE-PROMPT"),
            "wide render lost the live prompt: {wide:?}"
        );
        assert_eq!(wide.matches("LIVE-PROMPT").count(), 1);

        pane.scroll_up(usize::MAX);
        let narrow = render_pane(&mut pane, 18, 8);
        assert!(
            narrow.contains("history-1494"),
            "narrow scrolled render lost the retained historical lines: {narrow:?}"
        );
        assert!(
            narrow.contains("café") || narrow.contains("caf"),
            "narrow scrolled render lost the UTF-8 text: {narrow:?}"
        );

        pane.scroll_reset();
        let restored = render_pane(&mut pane, 72, 8);
        assert!(
            restored.contains("LIVE-PROMPT"),
            "wide live render lost the prompt after scroll/resize: {restored:?}"
        );
        assert_eq!(restored.matches("LIVE-PROMPT").count(), 1);
        assert_eq!(pane.observation(), PaneObservation::Attached);
        pane.kill();
    }
}
