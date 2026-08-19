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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{
    Child, CommandBuilder, MasterPty, NativePtySystem, PtyPair, PtySize, PtySystem,
};
// Re-exported by tui-term rather than depended on directly, so we always use the exact vt100
// version tui-term was built against (see the Cargo.toml comment on this crate's dependencies).
use tui_term::vt100;

use factory_core::local::{LocalRequest, ServerFrame};
use factory_core::runner::{
    decode_terminal_bytes, encode_terminal_bytes, terminal_generation_is_contiguous,
};
use factory_core::{ProjectId, SessionId};

use factoryctl::Client;

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

enum Backend {
    LocalPty {
        writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
        master: Box<dyn MasterPty + Send>,
        child: Box<dyn Child + Send + Sync>,
    },
    Daemon {
        attach: AttachConnection,
        input_tx: mpsc::Sender<Vec<u8>>,
        resize_tx: mpsc::Sender<(u16, u16)>,
        disconnected: Arc<AtomicBool>,
        attached: Arc<AtomicBool>,
        attach_error: Arc<Mutex<Option<String>>>,
    },
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

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
        let dirty = Arc::new(AtomicBool::new(true));

        spawn_local_reader_thread(
            reader,
            Arc::clone(&parser),
            Arc::clone(&writer),
            Arc::clone(&dirty),
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
            backend: Backend::LocalPty {
                writer,
                master: pair.master,
                child,
            },
        })
    }

    /// Attaches to a live daemon-proxied session over the shared bounded-tail
    /// contract on a dedicated connection (see `attach.rs` for why not the
    /// multiplexed one), then streams bytes into the same `vt100::Parser` a
    /// local-PTY pane would use.
    pub fn attach(
        socket: PathBuf,
        project_id: ProjectId,
        session_id: SessionId,
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
        let disconnected = Arc::new(AtomicBool::new(false));
        let attached = Arc::new(AtomicBool::new(false));
        let attach_error = Arc::new(Mutex::new(None));

        spawn_attach_reader_thread(
            reader_half,
            Arc::clone(&parser),
            Arc::clone(&dirty),
            Arc::clone(&disconnected),
            Arc::clone(&attached),
            Arc::clone(&attach_error),
        );

        let (input_tx, input_rx) = mpsc::channel::<Vec<u8>>();
        let (resize_tx, resize_rx) = mpsc::channel::<(u16, u16)>();
        spawn_input_worker(
            socket.clone(),
            project_id.clone(),
            session_id.clone(),
            input_rx,
        );
        spawn_resize_worker(socket, project_id, session_id, resize_rx);

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
            backend: Backend::Daemon {
                attach: attach_conn,
                input_tx,
                resize_tx,
                disconnected,
                attached,
                attach_error,
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
    /// daemon as a `TerminalInput` request (daemon-attached) — best-effort either way. All input
    /// first returns the pane to its live tail so key, paste, and mouse coordinates cannot act on
    /// a live screen while the operator is looking at historical scrollback.
    pub fn write_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.scroll_offset() > 0 {
            self.scroll_reset();
        }
        match &self.backend {
            Backend::LocalPty { writer, .. } => {
                if let Ok(mut writer) = writer.lock() {
                    let _ = writer.write_all(bytes);
                    let _ = writer.flush();
                }
            }
            Backend::Daemon { input_tx, .. } => {
                let _ = input_tx.send(bytes.to_vec());
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
            // vt100's grid resize truncates rows from the bottom when a live screen shrinks.
            // Capture its formatted visible state first so the new viewport lays out the same
            // terminal state at the new width/height; otherwise a prompt on the last row is
            // silently lost on the first TUI resize. This is a bounded snapshot of the current
            // screen, not a second unbounded replay buffer, and preserves the parser's ANSI/UTF-8
            // state through the ordinary terminal parser.
            let scrollback = parser.screen().scrollback();
            if scrollback > 0 {
                // vt100 0.15's visible-row iterator assumes its offset is no greater than the
                // current viewport. Capture the live screen before resizing, then restore the
                // bounded historical view after the new viewport is valid.
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

    /// Whether the pane is no longer usable: the local child exited, or (daemon-attached) the
    /// attach connection ended.
    #[must_use]
    pub fn has_exited(&mut self) -> bool {
        match &mut self.backend {
            Backend::LocalPty { child, .. } => matches!(child.try_wait(), Ok(Some(_))),
            Backend::Daemon { disconnected, .. } => disconnected.load(Ordering::Acquire),
        }
    }

    /// The last error the daemon sent back on this attach connection (e.g. "sessions are not
    /// implemented yet" while 5A/5C haven't landed), if any — surfaced by `ui/terminals.rs`/
    /// `ui/focus.rs` instead of a silent blank pane. Always `None` for a local-PTY pane.
    #[must_use]
    pub fn attach_error(&self) -> Option<String> {
        match &self.backend {
            Backend::LocalPty { .. } => None,
            Backend::Daemon { attach_error, .. } => {
                attach_error.lock().ok().and_then(|guard| guard.clone())
            }
        }
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        match &self.backend {
            Backend::LocalPty { .. } => true,
            Backend::Daemon {
                attached,
                disconnected,
                ..
            } => attached.load(Ordering::Acquire) && !disconnected.load(Ordering::Acquire),
        }
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
            // The vt100 version used by tui-term keeps a visible viewport of `rows` and its
            // iterator cannot represent an offset beyond that viewport. Clamp each movement to
            // one safe page; a large PgUp therefore remains a safe, bounded request rather than
            // turning into a client panic.
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

fn spawn_local_reader_thread(
    mut reader: Box<dyn std::io::Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    dirty: Arc<AtomicBool>,
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
    });
}

fn spawn_attach_reader_thread(
    reader: std::os::unix::net::UnixStream,
    parser: Arc<Mutex<vt100::Parser>>,
    dirty: Arc<AtomicBool>,
    disconnected: Arc<AtomicBool>,
    attached: Arc<AtomicBool>,
    attach_error: Arc<Mutex<Option<String>>>,
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
                            if let Ok(mut guard) = attach_error.lock() {
                                *guard = Some(format!(
                                    "terminal output continuity broke at offset {offset}"
                                ));
                            }
                            return false;
                        }
                        next_offset = Some(offset.saturating_add(decoded.len() as u64));
                        next_generation = Some(generation);
                        attached.store(true, Ordering::Release);
                        if let Ok(mut parser) = parser.lock() {
                            parser.process(&decoded);
                            dirty.store(true, Ordering::Release);
                        }
                    }
                    Err(error) => {
                        if let Ok(mut guard) = attach_error.lock() {
                            *guard = Some(format!("invalid terminal bytes: {error}"));
                        }
                        return false;
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
                            attached.store(true, Ordering::Release);
                            if let Ok(mut parser) = parser.lock() {
                                parser.process(&decoded);
                                dirty.store(true, Ordering::Release);
                            }
                        }
                        Err(error) => {
                            if let Ok(mut guard) = attach_error.lock() {
                                *guard = Some(format!("invalid terminal state: {error}"));
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
                    requested_offset,
                    reason,
                    ..
                } => {
                    if let Ok(mut guard) = attach_error.lock() {
                        *guard = Some(format!(
                            "attach cursor unavailable: {reason}; retained generation {base_generation} offsets {base_offset}..{end_offset}, requested {requested_offset}, generation {generation}, replay generation {start_generation} at {start_offset}"
                        ));
                    }
                    return false;
                }
                ServerFrame::Response { response, .. } => {
                    if let factory_core::local::LocalResponse::Error { message, .. } = response {
                        if let Ok(mut guard) = attach_error.lock() {
                            *guard = Some(message);
                        }
                        return false;
                    }
                }
                ServerFrame::Event { .. } => {}
            }
            true
        });
        disconnected.store(true, Ordering::Release);
        dirty.store(true, Ordering::Release);
    });
}

fn spawn_input_worker(
    socket: PathBuf,
    project_id: ProjectId,
    session_id: SessionId,
    rx: mpsc::Receiver<Vec<u8>>,
) {
    std::thread::spawn(move || {
        let client = Client::new(socket);
        while let Ok(bytes) = rx.recv() {
            let request = LocalRequest::TerminalInput {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                bytes: encode_terminal_bytes(&bytes),
            };
            let _ = client.request(request);
        }
    });
}

fn spawn_resize_worker(
    socket: PathBuf,
    project_id: ProjectId,
    session_id: SessionId,
    rx: mpsc::Receiver<(u16, u16)>,
) {
    std::thread::spawn(move || {
        let client = Client::new(socket);
        while let Ok((cols, rows)) = rx.recv() {
            let request = LocalRequest::ResizeTerminal {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                cols,
                rows,
            };
            let _ = client.request(request);
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
    use std::io::{self, BufRead, BufReader, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use factory_core::local::{LocalRequest, LocalResponse, RequestEnvelope, ServerFrame};
    use factory_core::{PROTOCOL_VERSION, ProjectId};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::{Block, Borders};
    use tui_term::widget::PseudoTerminal;

    struct PeerConsumption {
        observed: Mutex<bool>,
        wake: Condvar,
    }

    impl PeerConsumption {
        fn new() -> Self {
            Self {
                observed: Mutex::new(false),
                wake: Condvar::new(),
            }
        }

        fn wait(&self) {
            let mut observed = self.observed.lock().unwrap();
            while !*observed {
                observed = self.wake.wait(observed).unwrap();
            }
        }

        fn mark(&self) {
            *self.observed.lock().unwrap() = true;
            self.wake.notify_all();
        }

        fn was_marked(&self) -> bool {
            *self.observed.lock().unwrap()
        }
    }

    struct AttachFixture {
        socket: PathBuf,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        handler_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
        active_handlers: Arc<AtomicUsize>,
        send_complete: mpsc::Receiver<Result<(), String>>,
        peer_consumption: Arc<PeerConsumption>,
        send_errors: Arc<Mutex<Vec<String>>>,
        _directory: tempfile::TempDir,
    }

    impl AttachFixture {
        fn start(output: Vec<u8>) -> Self {
            Self::start_with_send_failure(output, None)
        }

        fn start_with_send_failure(output: Vec<u8>, fail_after_frames: Option<usize>) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("factory.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let (send_complete, send_complete_rx) = mpsc::channel();
            let peer_consumption = Arc::new(PeerConsumption::new());
            let send_errors = Arc::new(Mutex::new(Vec::new()));
            let thread_stop = Arc::clone(&stop);
            let handler_threads = Arc::new(Mutex::new(Vec::new()));
            let thread_handler_threads = Arc::clone(&handler_threads);
            let active_handlers = Arc::new(AtomicUsize::new(0));
            let thread_active_handlers = Arc::clone(&active_handlers);
            let thread_peer_consumption = Arc::clone(&peer_consumption);
            let thread_send_errors = Arc::clone(&send_errors);
            let thread = std::thread::spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // Only accept polling is nonblocking; fixture writes must block so
                            // send completion cannot spuriously fail with EWOULDBLOCK.
                            stream.set_nonblocking(false).unwrap();
                            let stop = Arc::clone(&thread_stop);
                            let output = output.clone();
                            let peer_consumption = Arc::clone(&thread_peer_consumption);
                            let send_errors = Arc::clone(&thread_send_errors);
                            let send_complete = send_complete.clone();
                            let active_handlers = Arc::clone(&thread_active_handlers);
                            active_handlers.fetch_add(1, Ordering::AcqRel);
                            let handler = std::thread::spawn(move || {
                                serve_attach_connection(
                                    stream,
                                    output,
                                    stop,
                                    send_complete,
                                    peer_consumption,
                                    send_errors,
                                    fail_after_frames,
                                );
                                active_handlers.fetch_sub(1, Ordering::AcqRel);
                            });
                            thread_handler_threads.lock().unwrap().push(handler);
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
                handler_threads,
                active_handlers,
                send_complete: send_complete_rx,
                peer_consumption,
                send_errors,
                _directory: directory,
            }
        }

        fn wait_for_send_complete(&self) -> Result<(), String> {
            self.send_complete
                .recv_timeout(Duration::from_secs(15))
                .map_err(|error| {
                    format!("attach fixture did not report send completion: {error}")
                })?
        }

        fn mark_peer_consumed(&self) {
            self.peer_consumption.mark();
        }

        fn peer_consumed(&self) -> bool {
            self.peer_consumption.was_marked()
        }

        fn send_errors(&self) -> Vec<String> {
            self.send_errors.lock().unwrap().clone()
        }
    }

    impl Drop for AttachFixture {
        fn drop(&mut self) {
            self.peer_consumption.mark();
            self.stop.store(true, Ordering::Release);
            let _ = UnixStream::connect(&self.socket);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
            let handlers = self
                .handler_threads
                .lock()
                .unwrap()
                .drain(..)
                .collect::<Vec<_>>();
            for handler in handlers {
                handler.join().unwrap();
            }
            assert_eq!(
                self.active_handlers.load(Ordering::Acquire),
                0,
                "attach fixture leaked a connection handler"
            );
        }
    }

    fn serve_attach_connection(
        mut stream: UnixStream,
        output: Vec<u8>,
        stop: Arc<AtomicBool>,
        send_complete: mpsc::Sender<Result<(), String>>,
        peer_consumption: Arc<PeerConsumption>,
        send_errors: Arc<Mutex<Vec<String>>>,
        fail_after_frames: Option<usize>,
    ) {
        let mut request = String::new();
        if BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut request)
            .unwrap_or(0)
            == 0
        {
            return;
        }
        let envelope: RequestEnvelope = serde_json::from_str(&request).unwrap();
        match envelope.request {
            LocalRequest::AttachTerminal { session_id, .. } => {
                let mut fail_after_frames = fail_after_frames;
                let result =
                    send_attach_frames(&mut stream, &session_id, &output, &mut fail_after_frames);
                if let Err(error) = &result {
                    send_errors.lock().unwrap().push(error.to_string());
                }
                let succeeded = result.is_ok();
                let _ = send_complete.send(result.map_err(|error| error.to_string()));
                if !succeeded {
                    return;
                }
                // The fixture must not let the attach socket close before the test has
                // consumed the complete UTF-8/ANSI history and observed the live prompt.
                peer_consumption.wait();
                let _ = stream.set_read_timeout(Some(Duration::from_millis(25)));
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
                        Err(_) => break,
                    }
                }
            }
            LocalRequest::ResizeTerminal { session_id, .. } => {
                let mut no_injected_failure = None;
                if let Err(error) = send_frame(
                    &mut stream,
                    ServerFrame::Response {
                        protocol_version: PROTOCOL_VERSION,
                        response: LocalResponse::TerminalResized { session_id },
                    },
                    &mut no_injected_failure,
                ) {
                    send_errors.lock().unwrap().push(error.to_string());
                }
            }
            _ => {}
        }
    }

    fn send_attach_frames(
        stream: &mut UnixStream,
        session_id: &SessionId,
        output: &[u8],
        fail_after_frames: &mut Option<usize>,
    ) -> io::Result<()> {
        send_frame(
            stream,
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
            fail_after_frames,
        )?;
        // Deliberately split every frame at a different boundary. This sends UTF-8 scalar
        // values and CSI sequences across frame boundaries, just as a real runner's bounded
        // chunking can, while keeping every wire frame small.
        let mut offset = 0_u64;
        for chunk in output.chunks(7) {
            send_frame(
                stream,
                ServerFrame::TerminalOutput {
                    protocol_version: PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    generation: 0,
                    offset,
                    bytes: encode_terminal_bytes(chunk),
                },
                fail_after_frames,
            )?;
            offset += chunk.len() as u64;
        }
        Ok(())
    }

    fn send_frame(
        stream: &mut UnixStream,
        frame: ServerFrame,
        fail_after_frames: &mut Option<usize>,
    ) -> io::Result<()> {
        if let Some(remaining) = fail_after_frames {
            if *remaining == 0 {
                return Err(io::Error::other("injected attach fixture send failure"));
            }
            *remaining -= 1;
        }
        serde_json::to_writer(&mut *stream, &frame).map_err(io::Error::other)?;
        stream.write_all(b"\n")?;
        stream.flush()
    }

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
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
        rendered_text(&terminal)
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
            "attached",
            8,
            48,
        )
        .unwrap();

        let send_result = fixture.wait_for_send_complete();
        assert!(
            send_result.is_ok(),
            "real attach fixture failed before sending the complete stream: {send_result:?}; errors={:?}",
            fixture.send_errors()
        );

        let deadline = Instant::now() + Duration::from_secs(15);
        while (!pane.is_ready()
            || !pane.with_screen(|screen| screen.contents().contains("LIVE-PROMPT")))
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            pane.is_ready(),
            "real attach never reached its ready boundary: error={:?}, screen={:?}",
            pane.attach_error(),
            pane.with_screen(|screen| screen.contents())
        );
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
        assert!(
            pane.attach_error().is_none(),
            "attach failed: {:?}",
            pane.attach_error()
        );
        assert!(
            fixture.send_errors().is_empty(),
            "real attach fixture recorded send errors: {:?}",
            fixture.send_errors()
        );
        fixture.mark_peer_consumed();
        pane.kill();
    }

    #[test]
    fn failed_attach_fixture_cannot_satisfy_complete_readiness_barrier() {
        let fixture = AttachFixture::start_with_send_failure(
            b"history-0000 caf\xC3\xA9 \x1b[32mansi-0000\x1b[0m\r\n\x1b[1;34mLIVE-PROMPT\x1b[0m $ "
                .to_vec(),
            Some(1),
        );
        let mut pane = Pane::attach(
            fixture.socket.clone(),
            ProjectId::try_from("project-1").unwrap(),
            SessionId::try_from("session-1").unwrap(),
            "attached",
            8,
            48,
        )
        .unwrap();

        let send_result = fixture.wait_for_send_complete();
        assert!(
            send_result
                .as_ref()
                .is_err_and(|error| error.contains("injected attach fixture send failure")),
            "failed fixture was reported as ready: {send_result:?}"
        );
        assert_eq!(
            fixture.send_errors(),
            vec!["injected attach fixture send failure"]
        );
        assert!(!fixture.peer_consumed());
        let deadline = Instant::now() + Duration::from_secs(15);
        while pane.attach_error().is_none() && !pane.has_exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let attach_error = pane.attach_error();
        let disconnected = pane.has_exited();
        assert!(
            attach_error.is_some() || disconnected,
            "truncated fixture produced no client-side attach failure or disconnect projection"
        );
        assert!(
            disconnected,
            "truncated fixture did not disconnect the client: error={attach_error:?}"
        );
        assert!(
            !(pane.is_ready()
                && pane.with_screen(|screen| screen.contents().contains("LIVE-PROMPT"))),
            "truncated fixture appeared complete: screen={:?}",
            pane.with_screen(|screen| screen.contents())
        );
        pane.kill();
    }
}
