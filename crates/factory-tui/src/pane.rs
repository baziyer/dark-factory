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
use tui_term::vt100::{MouseProtocolEncoding, MouseProtocolMode};

use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

use factory_core::local::{LocalRequest, ServerFrame};
use factory_core::runner::{decode_terminal_bytes, encode_terminal_bytes};
use factory_core::{ProjectId, SessionId};

use factoryctl::Client;

use crate::attach::{self, AttachConnection};
use crate::keys::KeyContext;
use crate::mouse::TerminalMouseContext;
use crate::query::QueryResponder;

/// How long after a local-PTY spawn we keep appending raw PTY bytes to the debug log, per the
/// spike's "detect which queries claude/codex actually send on startup" requirement.
const DEBUG_LOG_WINDOW: Duration = Duration::from_secs(6);

fn mouse_button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

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

    /// Encodes one crossterm event using the mode negotiated by the child.
    /// Returning `false` leaves board scrolling available when the child has
    /// not requested mouse input.
    pub fn write_mouse(&self, event: MouseEvent, origin_x: u16, origin_y: u16) -> bool {
        let (mode, encoding) = self.with_screen(|screen| {
            (
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
            )
        });
        if mode == MouseProtocolMode::None {
            return false;
        }
        let x = event.column.saturating_sub(origin_x).saturating_add(1);
        let y = event.row.saturating_sub(origin_y).saturating_add(1);
        let (mut code, suffix) = match event.kind {
            MouseEventKind::Down(button) => (mouse_button_code(button), 'M'),
            MouseEventKind::Up(button) => (mouse_button_code(button), 'm'),
            MouseEventKind::Drag(button) => {
                if !matches!(
                    mode,
                    MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
                ) {
                    return false;
                }
                (mouse_button_code(button) + 32, 'M')
            }
            MouseEventKind::Moved => {
                if mode != MouseProtocolMode::AnyMotion {
                    return false;
                }
                (32, 'M')
            }
            MouseEventKind::ScrollUp => (64, 'M'),
            MouseEventKind::ScrollDown => (65, 'M'),
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => return false,
        };
        if event
            .modifiers
            .contains(ratatui::crossterm::event::KeyModifiers::SHIFT)
        {
            code += 4;
        }
        if event
            .modifiers
            .contains(ratatui::crossterm::event::KeyModifiers::ALT)
        {
            code += 8;
        }
        if event
            .modifiers
            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL)
        {
            code += 16;
        }
        let bytes = if encoding == MouseProtocolEncoding::Sgr {
            format!("\x1b[<{code};{x};{y}{suffix}").into_bytes()
        } else if x < 224 && y < 224 {
            vec![
                0x1b,
                b'[',
                b'M',
                32 + code,
                u8::try_from(32 + x).expect("mouse x is bounded above"),
                u8::try_from(32 + y).expect("mouse y is bounded above"),
            ]
        } else {
            return false;
        };
        self.write_input(&bytes);
        true
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
            parser.set_size(rows, cols);
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
            let target = parser.screen().scrollback() + lines;
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
        attach::read_frames(reader, |frame| {
            match frame {
                ServerFrame::TerminalOutput { bytes, .. } => match decode_terminal_bytes(&bytes) {
                    Ok(decoded) => {
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
                    }
                },
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
}
