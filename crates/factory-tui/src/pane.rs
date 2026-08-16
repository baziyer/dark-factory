//! A single terminal pane: a child process running under a PTY, whose output is fed into a
//! `vt100::Parser` for rendering and scanned for terminal queries (see `query.rs`).

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{
    Child, CommandBuilder, MasterPty, NativePtySystem, PtyPair, PtySize, PtySystem,
};
// Re-exported by tui-term rather than depended on directly, so we always use the exact vt100
// version tui-term was built against (see the Cargo.toml comment on this crate's dependencies).
use tui_term::vt100;

use crate::keys::KeyContext;
use crate::query::QueryResponder;

/// How long after spawn we keep appending raw PTY bytes to the debug log, per the spike's
/// "detect which queries claude/codex actually send on startup" requirement.
const DEBUG_LOG_WINDOW: Duration = Duration::from_secs(6);

pub struct Pane {
    pub title: String,
    pub command: Vec<String>,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Set by the reader thread whenever new bytes were processed; cleared by the render loop.
    dirty: Arc<AtomicBool>,
    rows: u16,
    cols: u16,
}

impl Pane {
    /// Spawns `command` under a new PTY sized `cols x rows`. `debug_log` is an optional path to
    /// append raw PTY output bytes to for `DEBUG_LOG_WINDOW` after spawn (see `SPIKE.md`
    /// "Terminal-query handling").
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

        spawn_reader_thread(
            reader,
            Arc::clone(&parser),
            Arc::clone(&writer),
            Arc::clone(&dirty),
            debug_log,
        );

        Ok(Self {
            title: title.into(),
            command: command.to_vec(),
            parser,
            writer,
            master: pair.master,
            child,
            dirty,
            rows,
            cols,
        })
    }

    #[must_use]
    pub fn dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// Marks the pane dirty without checking - used to force a redraw (e.g. right after
    /// resizing, before the child has written anything new).
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

    /// Writes raw bytes to the child's stdin (already-encoded terminal input).
    pub fn write_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    /// Resizes both the PTY (so the child gets SIGWINCH) and the `vt100` model.
    pub fn resize(&mut self, rows: u16, cols: u16) -> anyhow::Result<()> {
        if rows == self.rows && cols == self.cols {
            return Ok(());
        }
        self.rows = rows;
        self.cols = cols;
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        if let Ok(mut parser) = self.parser.lock() {
            parser.set_size(rows, cols);
        }
        self.mark_dirty();
        Ok(())
    }

    /// Whether the child process has exited.
    #[must_use]
    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

fn spawn_reader_thread(
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
