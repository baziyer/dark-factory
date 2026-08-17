//! `factory-tui`: the Dwarf-Fortress-flavored operator board. See `README.md` for the four views,
//! keys, theme flag, and what's stubbed pending 5C (live sessions); see `SPIKE.md` for the
//! terminal-fidelity research this crate grew out of.
//!
//! This binary is a thin shell: `model/` owns all state and key-handling (pure, unit tested),
//! `net.rs` owns every socket, `ui/` renders `Board`, and this file wires the three together plus
//! the one thing none of them can own alone — the live terminal panes (`pane.rs`), since
//! rendering/reconciling them needs both `Board`'s state and each `Pane`'s mutex-guarded
//! `vt100::Screen` at once.

mod attach;
mod client_state;
mod fortress;
mod keys;
mod model;
mod net;
mod pane;
mod query;
#[cfg(test)]
mod test_fixtures;
mod theme;
mod ui;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};

use factory_core::SessionId;

use factoryctl::Client;

use model::{Board, Intent};
use net::NetMsg;
use pane::{Pane, PaneMap};

struct Config {
    socket: Option<String>,
    project: Option<String>,
    dev_local_pty: bool,
    debug_log: Option<PathBuf>,
    theme: theme::Theme,
}

fn parse_args() -> Config {
    let mut socket = None;
    let mut project = None;
    let mut dev_local_pty = false;
    let mut debug_log = None;
    let mut theme = theme::FORTRESS;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next(),
            "--project" => project = args.next(),
            "--dev-local-pty" => dev_local_pty = true,
            "--debug-log" => debug_log = args.next().map(PathBuf::from),
            "--theme" => {
                let Some(name) = args.next() else {
                    eprintln!("factory-tui: --theme requires a value (fortress|plain)\n");
                    print_help();
                    std::process::exit(2);
                };
                let Some(parsed) = theme::Theme::parse(&name) else {
                    eprintln!("factory-tui: unknown theme {name:?} (expected fortress|plain)\n");
                    print_help();
                    std::process::exit(2);
                };
                theme = parsed;
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("factory-tui {}", factoryctl::update::CURRENT_VERSION);
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
        socket,
        project,
        dev_local_pty,
        debug_log,
        theme,
    }
}

fn print_help() {
    eprintln!(
        "factory-tui - the Dark Factory operator board\n\n\
         USAGE:\n    factory-tui [OPTIONS]\n\n\
         OPTIONS:\n    \
         --socket PATH        Control-socket path (default: see README.md's 3-step resolution)\n    \
         --project ID         Focus this project on startup (default: the oldest by creation order)\n    \
         --theme fortress|plain   Glyph theme (default: fortress)\n    \
         --dev-local-pty       TERMINALS/FOCUS attach a local shell instead of a live daemon\n                          \
         session (offline testing only — see README.md)\n    \
         --debug-log DIR       With --dev-local-pty, log a pane's raw PTY bytes to DIR\n    \
         -h, --help            Show this help\n    \
         --version             Print the version\n\n\
         See README.md for the full key reference."
    );
}

/// RAII guard: restores the terminal (raw mode, alternate screen, bracketed paste, cursor) on
/// drop, so a normal return *or* an early `?` both clean up. A panic hook (installed separately,
/// see `main`) covers the case Drop can't: it runs before unwinding starts.
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

fn now_ms() -> i64 {
    factoryctl::update::now_ms()
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

    let socket = net::resolve_socket_path(config.socket.as_deref())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let client = Client::new(&socket);

    let cli_project = config
        .project
        .as_deref()
        .map(|value| {
            factory_core::ProjectId::try_from(value)
                .map_err(|error| anyhow::anyhow!("invalid --project: {error}"))
        })
        .transpose()?;

    let (tx, rx) = mpsc::channel::<NetMsg>();
    net::spawn_fleet_session(client.clone(), tx.clone());
    // `--project` wins; otherwise open on whatever was focused last time — but only for the
    // daemon `$DARK_FACTORY_HOME` names: an explicit `--socket`/`$DARK_FACTORY_SOCKET` may be a
    // scratch daemon, whose projects must not overwrite (or be seeded from) the real home's
    // remembered focus.
    let remember_focus = config.socket.is_none()
        && std::env::var_os("DARK_FACTORY_SOCKET").is_none_or(|value| value.is_empty());
    let initial_project = cli_project.or_else(|| {
        remember_focus
            .then(client_state::load_focused_project)
            .flatten()
    });

    let mut board = Board::new(config.dev_local_pty, now_ms(), config.theme);
    let mut panes: PaneMap = HashMap::new();

    run(
        &mut terminal,
        &mut board,
        &client,
        &socket,
        &tx,
        &rx,
        initial_project,
        remember_focus,
        &mut panes,
        config.debug_log.as_deref(),
    )?;

    for (_, mut pane) in panes {
        pane.kill();
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    board: &mut Board,
    client: &Client,
    socket: &std::path::Path,
    tx: &mpsc::Sender<NetMsg>,
    rx: &mpsc::Receiver<NetMsg>,
    initial_project: Option<factory_core::ProjectId>,
    remember_focus: bool,
    panes: &mut PaneMap,
    debug_log: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    // Not a busy-poll: `event::poll` blocks (efficiently, via the OS) for up to `tick` with ~0
    // CPU when idle. This cadence is how the board notices net-thread messages and ticks elapsed
    // time; redraws themselves only happen when something actually changed (see `needs_redraw`).
    let tick = Duration::from_millis(150);
    let mut last_second = Instant::now();
    let mut last_update_check = Instant::now();
    let mut initial_project_applied = false;
    let mut remembered_project = board.focused_project.clone();
    net::spawn_update_check(tx.clone(), now_ms());

    sync_panes(board, panes, socket, debug_log);
    terminal.draw(|frame| ui::draw(frame, board, panes))?;

    loop {
        if board.quit {
            break;
        }

        let mut needs_redraw = false;

        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) => {
                    let intent = board.handle_key(key);
                    needs_redraw |= apply_intent(intent, board, client, tx, panes);
                }
                Event::Paste(text) => {
                    forward_paste_if_applicable(board, panes, &text);
                    needs_redraw = true;
                }
                Event::Resize(_, _) => needs_redraw = true,
                // Mouse/focus are received but not acted on - see SPIKE.md "Mouse" for why
                // (neither target app's mouse support is wired through this crate yet).
                Event::Mouse(_) | Event::FocusGained | Event::FocusLost => {}
            }
        }

        while let Ok(msg) = rx.try_recv() {
            apply_net_msg(
                msg,
                board,
                initial_project.as_ref(),
                &mut initial_project_applied,
            );
            needs_redraw = true;
        }

        if sync_panes(board, panes, socket, debug_log) {
            needs_redraw = true;
        }
        sync_task_detail(board, client, tx);
        for pane in panes.values() {
            if pane.dirty() {
                needs_redraw = true;
            }
        }

        if remember_focus && board.focused_project != remembered_project {
            remembered_project.clone_from(&board.focused_project);
            if let Some(project_id) = &remembered_project {
                client_state::save_focused_project(project_id);
            }
        }

        if last_second.elapsed() >= Duration::from_secs(1) {
            board.tick(now_ms());
            last_second = Instant::now();
            needs_redraw = true;
            if last_update_check.elapsed() >= factoryctl::update::CHECK_INTERVAL {
                net::spawn_update_check(tx.clone(), now_ms());
                last_update_check = Instant::now();
            }
        }

        if needs_redraw {
            terminal.draw(|frame| ui::draw(frame, board, panes))?;
        }
    }

    Ok(())
}

/// Reconciles `panes` against `Board::desired_sessions()`: attaches (or, under
/// `--dev-local-pty`, spawns a local shell for) anything newly needed, detaches anything no
/// longer needed. Cheap to call every loop iteration (a handful of id comparisons) — deliberately
/// not gated on a redraw, so leaving TERMINALS/FOCUS detaches promptly even if nothing else
/// changed that frame. Returns whether anything changed (worth a redraw).
fn sync_panes(
    board: &mut Board,
    panes: &mut PaneMap,
    socket: &std::path::Path,
    debug_log: Option<&std::path::Path>,
) -> bool {
    let desired: Vec<SessionId> = board.desired_sessions();
    let mut changed = false;

    let stale: Vec<SessionId> = panes
        .keys()
        .filter(|id| !desired.contains(id))
        .cloned()
        .collect();
    for session_id in stale {
        if let Some(mut pane) = panes.remove(&session_id) {
            pane.kill();
            changed = true;
        }
    }

    for session_id in desired {
        if panes.contains_key(&session_id) {
            continue;
        }
        let Some(agent) = board.agent_for_pane_session(&session_id) else {
            continue;
        };
        let title = agent.id.to_string();
        let pane = if let Some(session) = board.sessions.get(&session_id) {
            Pane::attach(
                socket.to_path_buf(),
                session.project_id.clone(),
                session_id.clone(),
                title,
                24,
                80,
            )
        } else {
            // A synthetic `--dev-local-pty` id (never present in `board.sessions` — see
            // `Board::session_id_for_pane`'s doc comment).
            Pane::spawn(
                title,
                &["bash".to_owned()],
                24,
                80,
                debug_log.map(|dir| dir.join(format!("{session_id}.debug.log"))),
            )
        };
        match pane {
            Ok(pane) => {
                panes.insert(session_id, pane);
                changed = true;
            }
            Err(error) => board.note_error(format!("couldn't attach {session_id}: {error}")),
        }
    }

    changed
}

/// Issues a `GetTask` fetch for WORKSHOP's currently selected task if its cached detail is
/// missing or stale (`Board::begin_task_detail_fetch`) — the fix for task detail showing empty
/// for a task created (or completed) after this client started (`TaskChanged` events only ever
/// carry the durable snapshot, not `body`/`result`). Cheap to call every loop iteration like
/// `sync_panes`: a couple of map lookups when nothing needs fetching, and `begin_task_detail_fetch`
/// itself dedupes so a request already in flight is never fired twice.
fn sync_task_detail(board: &mut Board, client: &Client, tx: &mpsc::Sender<NetMsg>) {
    if board.view != model::View::Workshop {
        return;
    }
    let Some(task_id) = board.selected_task.clone() else {
        return;
    };
    if let Some(project_id) = board.begin_task_detail_fetch(&task_id) {
        net::spawn_task_detail_request(client.clone(), tx.clone(), project_id, task_id);
    }
}

/// Which pane currently owns the keyboard: the focused TERMINALS tile, or FOCUS's one pane. The
/// same `Board::terminals_focused_pane` the highlight in `ui::terminals` reads, so the two can
/// never point at different panes.
fn forwarding_target(board: &Board) -> Option<SessionId> {
    match board.view {
        model::View::Terminals => board.terminals_focused_pane(),
        model::View::Focus => board.focus_target(),
        model::View::Fortress | model::View::Workshop => None,
    }
}

fn forward_paste_if_applicable(board: &Board, panes: &PaneMap, text: &str) {
    if board.pane_mode != model::PaneMode::Typing {
        return;
    }
    let Some(session_id) = forwarding_target(board) else {
        return;
    };
    if let Some(pane) = panes.get(&session_id) {
        let bytes = keys::encode_paste(text, pane.bracketed_paste());
        pane.write_input(&bytes);
    }
}

fn apply_intent(
    intent: Intent,
    board: &mut Board,
    client: &Client,
    tx: &mpsc::Sender<NetMsg>,
    panes: &PaneMap,
) -> bool {
    match intent {
        Intent::None => false,
        Intent::Redraw | Intent::Quit => true,
        Intent::Send(request) => {
            net::spawn_request(client.clone(), tx.clone(), request);
            true
        }
        Intent::ForwardKey(key) => {
            if let Some(session_id) = forwarding_target(board) {
                if let Some(pane) = panes.get(&session_id) {
                    // Typing while scrolled back snaps to the live tail — matches common
                    // terminal-emulator behavior and gives `scroll_reset` its one production
                    // call site (scrolling itself only ever moves the offset forward/back).
                    if pane.scroll_offset() > 0 {
                        pane.scroll_reset();
                    }
                    let bytes = keys::encode_key(key, pane.key_context());
                    pane.write_input(&bytes);
                }
            }
            true
        }
        Intent::ScrollFocus { up } => {
            if let Some(session_id) = board.focus_target() {
                if let Some(pane) = panes.get(&session_id) {
                    const SCROLL_STEP_LINES: usize = 10;
                    if up {
                        pane.scroll_up(SCROLL_STEP_LINES);
                    } else {
                        pane.scroll_down(SCROLL_STEP_LINES);
                    }
                }
            }
            true
        }
    }
}

fn apply_net_msg(
    msg: NetMsg,
    board: &mut Board,
    initial_project: Option<&factory_core::ProjectId>,
    initial_project_applied: &mut bool,
) {
    match msg {
        NetMsg::ConnectionRetrying(detail) => board.set_retrying(detail),
        NetMsg::ConnectionLive => board.set_live(),
        NetMsg::FleetSnapshot {
            projects,
            agents,
            tasks,
            runs,
            sessions,
        } => {
            board.apply_fleet_snapshot(projects, agents, tasks, runs, sessions);
            if !*initial_project_applied {
                if let Some(project_id) = initial_project {
                    board.focus_project(project_id.clone());
                }
                *initial_project_applied = true;
            }
        }
        NetMsg::Event(event) => board.apply_event(event),
        NetMsg::CaughtUp => board.caught_up = true,
        NetMsg::OperationResult(result) => board.apply_response(result),
        NetMsg::TaskDetailResult { task_id, result } => {
            board.apply_task_detail_result(task_id, result);
        }
        NetMsg::UpdateCheck(check) => {
            board.update_available = check.available().map(|manifest| manifest.version.clone());
        }
        NetMsg::FleetStatus(status) => board.live_session_cap = Some(status.live_session_cap),
    }
}
