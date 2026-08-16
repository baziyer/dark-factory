//! `factory-tui`: a ratatui-based operator front-end spike. See `SPIKE.md` for the fidelity
//! report this crate exists to produce. This binary does **not** talk to `factoryd` yet - both
//! panes are local child processes spawned directly under a PTY.

mod app;
mod keys;
mod pane;
mod query;

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};

use app::App;
use pane::Pane;

struct Config {
    left: Vec<String>,
    right: Vec<String>,
    debug_log: Option<PathBuf>,
}

fn parse_args() -> Config {
    let mut left = vec!["claude".to_string()];
    let mut right = vec!["codex".to_string()];
    let mut debug_log = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--shell" => {
                // Cheap-iteration mode: no subscription login, no risk of a stray real prompt.
                left = vec!["bash".to_string()];
                right = vec!["vim".to_string()];
            }
            "--left" => {
                if let Some(v) = args.next() {
                    left = v.split_whitespace().map(String::from).collect();
                }
            }
            "--right" => {
                if let Some(v) = args.next() {
                    right = v.split_whitespace().map(String::from).collect();
                }
            }
            "--debug-log" => {
                debug_log = args.next().map(PathBuf::from);
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                eprintln!("factory-tui: unknown argument {other:?}\n");
                print_help();
                std::process::exit(2);
            }
        }
    }

    Config {
        left,
        right,
        debug_log,
    }
}

fn print_help() {
    eprintln!(
        "factory-tui - ratatui operator UI spike\n\n\
         USAGE:\n    factory-tui [OPTIONS]\n\n\
         OPTIONS:\n    \
         --shell              Run bash (left) / vim (right) instead of claude/codex\n    \
         --left <CMD>         Override the left pane's command\n    \
         --right <CMD>        Override the right pane's command\n    \
         --debug-log <DIR>    Log each pane's raw PTY bytes for the first ~6s to DIR/{{left,right}}.debug.log\n    \
         -h, --help           Show this help\n\n\
         KEYS:\n    \
         Ctrl-]  toggle between PANE mode (keys go to the focused pane) and CONTROL mode\n    \
         In CONTROL mode: Tab / h / l = switch focus, z = zoom, q = quit, Esc = back to PANE mode"
    );
}

/// RAII guard: restores the terminal (raw mode, alternate screen, bracketed paste, cursor) on
/// drop, so a normal return *or* an early `?` both clean up. A panic hook (installed separately,
/// see `main`) covers the case Drop can't: it runs before unwinding starts, which matters if a
/// held lock is poisoned or a second panic occurs during unwind.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen,
        cursor::Show
    );
}

fn main() -> anyhow::Result<()> {
    let config = parse_args();

    let default_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_panic_hook(info);
    }));

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    let size = terminal.size()?;
    // Split the initial size in half up front; `App::sync_pty_sizes` reconciles this against
    // the real layout (borders included) on the very first draw.
    let half_cols = size.width / 2;

    let left_pane = Pane::spawn(
        "left",
        &config.left,
        size.height,
        half_cols.max(1),
        config
            .debug_log
            .as_ref()
            .map(|dir| dir.join("left.debug.log")),
    )?;
    let right_pane = Pane::spawn(
        "right",
        &config.right,
        size.height,
        size.width.saturating_sub(half_cols).max(1),
        config
            .debug_log
            .as_ref()
            .map(|dir| dir.join("right.debug.log")),
    )?;

    let mut app = App::new([left_pane, right_pane]);
    run(&mut terminal, &mut app)?;

    for pane in &mut app.panes {
        pane.kill();
    }

    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> anyhow::Result<()> {
    let tick = Duration::from_millis(33); // ~30fps ceiling; see SPIKE.md "CPU" measurements.
    let mut last_forced_redraw = Instant::now();
    // A redraw is always worth doing at least this often even if nothing looks dirty, as a
    // defensive fallback (e.g. a missed wakeup); in practice the dirty-flag path below fires
    // far more often than this.
    let forced_redraw_interval = Duration::from_millis(250);

    terminal.draw(|frame| {
        app.sync_pty_sizes(frame.area());
        app.draw(frame);
    })?;

    loop {
        if !app.running {
            break;
        }

        let mut needs_redraw = false;
        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) => needs_redraw |= app.handle_key(key),
                Event::Paste(text) => {
                    app.handle_paste(&text);
                    needs_redraw = true;
                }
                Event::Resize(_, _) => needs_redraw = true,
                // Mouse and focus events are received but not acted on in this spike; see
                // SPIKE.md "mouse" for whether the target apps need them.
                Event::Mouse(_) | Event::FocusGained | Event::FocusLost => {}
            }
        }

        // Don't short-circuit: `dirty()` clears each pane's flag as a side effect, and both
        // need to be checked every tick regardless of the other's result.
        let mut any_dirty = false;
        for pane in &app.panes {
            if pane.dirty() {
                any_dirty = true;
            }
        }
        needs_redraw |= any_dirty;

        if !needs_redraw && last_forced_redraw.elapsed() >= forced_redraw_interval {
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|frame| {
                app.sync_pty_sizes(frame.area());
                app.draw(frame);
            })?;
            last_forced_redraw = Instant::now();
        }

        if app.panes.iter_mut().all(Pane::has_exited) {
            app.running = false;
        }
    }

    Ok(())
}
