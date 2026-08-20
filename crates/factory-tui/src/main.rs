//! `factory-tui`: the BUILDING/AGENT operator board. See `README.md` for the two screens,
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
mod keys;
mod model;
mod mouse;
mod net;
mod pane;
mod query;
#[cfg(test)]
mod test_fixtures;
mod theme;
mod ui;

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, MouseEvent,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};

use factory_core::SessionId;

use factoryctl::Client;

use model::{Board, Intent};
use net::NetMsg;
use pane::{Pane, PaneMap, PaneObservation};

struct Config {
    socket: Option<String>,
    project: Option<String>,
    dev_local_pty: bool,
    debug_log: Option<PathBuf>,
    theme: theme::Theme,
    resume: Option<client_state::RelaunchState>,
}

struct ContextRefresh {
    agent: Option<factory_core::AgentId>,
    was_agent_view: bool,
    last_refresh: Instant,
    force: bool,
}

#[derive(Default)]
struct UpdateWorker(Option<std::thread::JoinHandle<()>>);

impl UpdateWorker {
    fn replace(&mut self, worker: std::thread::JoinHandle<()>) {
        debug_assert!(self.0.is_none());
        self.0 = Some(worker);
    }

    fn join(&mut self) {
        if let Some(worker) = self.0.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for UpdateWorker {
    fn drop(&mut self) {
        self.join();
    }
}

impl ContextRefresh {
    fn new() -> Self {
        Self {
            agent: None,
            was_agent_view: false,
            last_refresh: Instant::now() - Duration::from_secs(10),
            force: false,
        }
    }

    fn should_refresh(
        &mut self,
        agent: &factory_core::AgentId,
        in_agent: bool,
        now: Instant,
    ) -> bool {
        if !in_agent {
            self.was_agent_view = false;
            return false;
        }
        let entering = !self.was_agent_view;
        let changed = self.agent.as_ref() != Some(agent);
        let due = now.duration_since(self.last_refresh) >= Duration::from_secs(5);
        self.was_agent_view = true;
        if entering || changed || due || self.force {
            self.agent = Some(agent.clone());
            self.last_refresh = now;
            self.force = false;
            true
        } else {
            false
        }
    }
}

fn parse_args() -> Config {
    let mut socket = None;
    let mut project = None;
    let mut dev_local_pty = false;
    let mut debug_log = None;
    let mut theme = theme::FORTRESS;
    let mut resume = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next(),
            "--project" => project = args.next(),
            "--dev-local-pty" => dev_local_pty = true,
            "--debug-log" => debug_log = args.next().map(PathBuf::from),
            "--resume-state" => {
                let value = args.next().unwrap_or_else(|| {
                    eprintln!("factory-tui: --resume-state requires a value");
                    std::process::exit(2);
                });
                resume = Some(
                    client_state::decode_relaunch(&value).unwrap_or_else(|error| {
                        eprintln!("factory-tui: {error}");
                        std::process::exit(2);
                    }),
                );
            }
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
        resume,
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
         --dev-local-pty       AGENT attaches a local shell instead of a live daemon\n                          \
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
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    );
}

fn enter_terminal() {
    let _ = enable_raw_mode();
    let _ = execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        cursor::Hide
    );
}

fn now_ms() -> i64 {
    factoryctl::update::now_ms()
}

fn base_relaunch_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut output = Vec::new();
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        if argument == OsStr::new("--resume-state") {
            let _ = args.next();
        } else {
            output.push(argument);
        }
    }
    output
}

fn apply_relaunch_state(board: &mut Board, state: &client_state::RelaunchState) {
    if let Some(project_id) = &state.focused_project
        && board
            .projects
            .iter()
            .any(|project| &project.id == project_id)
    {
        board.focus_project(project_id.clone());
    }
    if let Some(agent_id) = &state.selected_agent
        && board.agents.contains_key(agent_id)
    {
        board.selected_agent = Some(agent_id.clone());
    }
    board.view = if state.agent_view && board.selected_agent.is_some() {
        model::View::Agent
    } else {
        model::View::Building
    };
    board.pane_mode = model::PaneMode::Board;
    board.terminal_maximized = state.terminal_maximized && board.view == model::View::Agent;
}

fn relaunch_arguments(board: &Board, original: &[OsString]) -> Result<Vec<OsString>, String> {
    let state = client_state::RelaunchState {
        focused_project: board.focused_project.clone(),
        selected_agent: board.selected_agent.clone(),
        agent_view: board.view == model::View::Agent,
        terminal_maximized: board.terminal_maximized,
    };
    let mut arguments = original.to_vec();
    arguments.push(OsString::from("--resume-state"));
    arguments.push(OsString::from(client_state::encode_relaunch(&state)?));
    Ok(arguments)
}

fn finish_update(
    outcome: factoryctl::managed_update::InstalledUpdate,
    board: &mut Board,
    panes: &mut PaneMap,
    original_args: &[OsString],
) {
    let preparation = outcome.verified_tui_executable().and_then(|executable| {
        relaunch_arguments(board, original_args).map(|args| (executable, args))
    });
    let (executable, arguments) = match preparation {
        Ok(prepared) => prepared,
        Err(error) => {
            rollback_failed_relaunch(
                outcome,
                board,
                format!("relaunch preparation failed: {error}"),
            );
            return;
        }
    };

    for (_, mut pane) in panes.drain() {
        pane.kill();
    }
    restore_terminal();
    let exec_error = Command::new(&executable).args(&arguments).exec();
    enter_terminal();
    rollback_failed_relaunch(
        outcome,
        board,
        format!("could not exec {}: {exec_error}", executable.display()),
    );
}

fn rollback_failed_relaunch(
    outcome: factoryctl::managed_update::InstalledUpdate,
    board: &mut Board,
    seam_error: String,
) {
    board.update_progress = Some(factoryctl::managed_update::UpdateProgress::RollingBack);
    let message = reexec_failure_message(seam_error, || {
        outcome.rollback_after_reexec_failure(&mut |_| {})
    });
    board.update_progress = None;
    board.note_error(message);
}

fn reexec_failure_message(
    seam_error: String,
    rollback: impl FnOnce() -> Result<factoryctl::managed_update::ReexecRecovery, String>,
) -> String {
    match rollback() {
        Ok(factoryctl::managed_update::ReexecRecovery::Restored) => {
            format!("{seam_error}; previous runtime restored")
        }
        Ok(factoryctl::managed_update::ReexecRecovery::NotNeeded) => {
            format!("{seam_error}; runtime was already active, so no rollback was needed")
        }
        Err(error) => format!("{seam_error}; rollback failed: {error}"),
    }
}

fn main() -> anyhow::Result<()> {
    let relaunch_args = base_relaunch_args(std::env::args_os().skip(1));
    let config = parse_args();

    let default_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_panic_hook(info);
    }));

    enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
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
    let _fleet_status_refresh = net::spawn_fleet_status_refresh(client.clone(), tx.clone());
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
        config.resume,
        &relaunch_args,
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
    resume: Option<client_state::RelaunchState>,
    relaunch_args: &[OsString],
) -> anyhow::Result<()> {
    // Not a busy-poll: `event::poll` blocks (efficiently, via the OS) for up to `tick` with ~0
    // CPU when idle. This cadence is how the board notices net-thread messages and ticks elapsed
    // time; redraws themselves only happen when something actually changed (see `needs_redraw`).
    let tick = Duration::from_millis(150);
    let mut last_second = Instant::now();
    let mut last_update_check = Instant::now();
    let mut initial_project_applied = false;
    let mut remembered_project = board.focused_project.clone();
    let mut context_refresh = ContextRefresh::new();
    let mut mouse_capture = mouse::Capture::default();
    let mut update_worker = UpdateWorker::default();
    net::spawn_update_check(socket.to_owned(), tx.clone(), now_ms());

    sync_panes(board, panes, socket, client, tx, debug_log);
    let mut hit_map = draw_board(terminal, board, panes)?;

    loop {
        if board.quit {
            break;
        }

        let mut needs_redraw = false;

        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) => {
                    let intent = board.handle_key(key);
                    needs_redraw |=
                        apply_intent(intent, board, client, socket, tx, panes, &mut update_worker);
                }
                Event::Paste(text) => {
                    forward_paste_if_applicable(board, panes, &text);
                    needs_redraw = true;
                }
                Event::Resize(_, _) => needs_redraw = true,
                Event::Mouse(event) => {
                    needs_redraw |= handle_mouse(
                        event,
                        &hit_map,
                        &mut mouse_capture,
                        &mut IntentContext {
                            board,
                            client,
                            socket,
                            tx,
                            panes,
                            update_worker: &mut update_worker,
                        },
                    );
                }
                Event::FocusGained | Event::FocusLost => {}
            }
        }

        while let Ok(msg) = rx.try_recv() {
            if matches!(&msg, NetMsg::ConnectionLive) {
                context_refresh.force = true;
            }
            if let NetMsg::UpdateFinished(result) = msg {
                update_worker.join();
                match result {
                    Ok(outcome) => finish_update(outcome, board, panes, relaunch_args),
                    Err(error) => {
                        board.update_progress = None;
                        board.note_error(format!(
                            "update failed; current TUI remains usable: {error}"
                        ));
                    }
                }
                needs_redraw = true;
                continue;
            }
            apply_net_msg(
                msg,
                board,
                initial_project.as_ref(),
                &mut initial_project_applied,
                resume.as_ref(),
            );
            needs_redraw = true;
        }

        if sync_panes(board, panes, socket, client, tx, debug_log) {
            needs_redraw = true;
        }
        sync_agent_context(board, client, tx, &mut context_refresh);
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
                net::spawn_update_check(socket.to_owned(), tx.clone(), now_ms());
                last_update_check = Instant::now();
            }
        }

        if needs_redraw {
            hit_map = draw_board(terminal, board, panes)?;
        }
    }

    Ok(())
}

fn draw_board(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    board: &Board,
    panes: &mut PaneMap,
) -> io::Result<mouse::HitMap> {
    let mut hits = mouse::HitMap::default();
    terminal.draw(|frame| hits = ui::draw(frame, board, panes))?;
    Ok(hits)
}

/// Reconciles `panes` against `Board::desired_sessions()`: attaches (or, under
/// `--dev-local-pty`, spawns a local shell for) anything newly needed, detaches anything no
/// longer needed. Cheap to call every loop iteration (a handful of id comparisons) — deliberately
/// not gated on a redraw, so leaving AGENT detaches promptly even if nothing else
/// changed that frame. Returns whether anything changed (worth a redraw).
fn sync_panes(
    board: &mut Board,
    panes: &mut PaneMap,
    socket: &std::path::Path,
    client: &Client,
    tx: &mpsc::Sender<NetMsg>,
    debug_log: Option<&std::path::Path>,
) -> bool {
    let desired: Vec<SessionId> = board.desired_sessions();
    let mut changed = board.clear_undesired_attach_failures(&desired);

    let stale: Vec<SessionId> = panes
        .keys()
        .filter(|id| !desired.contains(id))
        .cloned()
        .collect();
    for session_id in stale {
        if let Some(mut pane) = panes.remove(&session_id) {
            pane.kill();
            board.clear_local_attach_failure_if_identity_changed(&session_id);
            changed = true;
        }
    }

    for session_id in desired {
        if let Some(pane) = panes.get_mut(&session_id) {
            let current_identity =
                board
                    .sessions
                    .get(&session_id)
                    .map(|session| crate::pane::PaneIdentity {
                        project_id: session.project_id.clone(),
                        session_id: session.id.clone(),
                        runner_instance_id: session.runner_instance_id.clone(),
                    });
            if pane.identity() != current_identity.as_ref() {
                if let Some(mut replaced) = panes.remove(&session_id) {
                    replaced.kill();
                }
                board.clear_local_attach_failure(&session_id);
                changed = true;
                continue;
            }
            match pane.observation() {
                PaneObservation::AttachRefused(refusal) => {
                    if !board.note_attach_refusal(&refusal) {
                        if let Some(mut stale) = panes.remove(&session_id) {
                            stale.kill();
                        }
                        net::spawn_fleet_snapshot(client.clone(), tx.clone());
                        changed = true;
                        continue;
                    }
                    if let Some(mut failed) = panes.remove(&session_id) {
                        failed.kill();
                    }
                    net::spawn_fleet_snapshot(client.clone(), tx.clone());
                    changed = true;
                    continue;
                }
                PaneObservation::Error(error) => {
                    board.note_local_attach_failure(&session_id, &error);
                    if let Some(mut failed) = panes.remove(&session_id) {
                        failed.kill();
                    }
                    changed = true;
                    continue;
                }
                PaneObservation::Disconnected => {
                    board.note_local_attach_failure(&session_id, "terminal connection closed");
                    if let Some(mut failed) = panes.remove(&session_id) {
                        failed.kill();
                    }
                    changed = true;
                    continue;
                }
                PaneObservation::LocalPtyExited => continue,
                PaneObservation::Connecting | PaneObservation::Attached => continue,
            }
        }
        if !board.take_attach_retry(&session_id) {
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
                session.runner_instance_id.clone(),
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
            Err(error) => {
                board.note_local_attach_failure(&session_id, &error.to_string());
                board.note_error(format!("couldn't attach {session_id}: {error}"));
            }
        }
    }

    let ready_session = board.focus_target().and_then(|session_id| {
        panes
            .get(&session_id)
            .and_then(|pane| pane.observation().is_attached().then_some(session_id))
    });
    changed |= reconcile_pane_readiness(board, ready_session);
    changed
}

/// Applies the final readiness observation for the focused pane and its coupled local refusal
/// state. Keeping this transition together makes a late-ready pane unable to leave `pane_ready`
/// true while stale refusal attention remains visible.
fn reconcile_pane_readiness(board: &mut Board, ready_session: Option<SessionId>) -> bool {
    let ready = ready_session.is_some();
    if let Some(session_id) = ready_session {
        board.clear_local_attach_failure(&session_id);
    }
    let mut changed = board.pane_ready != ready;
    board.pane_ready = ready;
    if !ready && board.pane_mode == model::PaneMode::Typing {
        board.pane_mode = model::PaneMode::Board;
        changed = true;
    }
    changed
}

/// Loads private profile and inbox data when AGENT is entered and on a bounded refresh cadence.
fn sync_agent_context(
    board: &mut Board,
    client: &Client,
    tx: &mpsc::Sender<NetMsg>,
    refresh: &mut ContextRefresh,
) {
    let in_agent = board.view == model::View::Agent;
    let Some(agent_id) = board.selected_agent.clone() else {
        refresh.was_agent_view = in_agent;
        return;
    };
    let now = Instant::now();
    if !refresh.should_refresh(&agent_id, in_agent, now) {
        return;
    }
    let Some(agent) = board.agents.get(&agent_id) else {
        return;
    };
    let project_id = agent.project_id.clone();
    for request in context_requests(project_id, agent_id) {
        let operation_id = board.allocate_operation_id();
        net::spawn_request(client.clone(), tx.clone(), operation_id, request);
    }
}

fn context_requests(
    project_id: factory_core::ProjectId,
    agent_id: factory_core::AgentId,
) -> [factory_core::local::LocalRequest; 2] {
    [
        factory_core::local::LocalRequest::GetAgent {
            project_id: project_id.clone(),
            agent_id: agent_id.clone(),
        },
        factory_core::local::LocalRequest::ListAgentMessages {
            project_id,
            agent_id,
            after_id: None,
            limit: 100,
        },
    ]
}

/// AGENT.s selected pane owns the keyboard only in TYPING mode. The
/// same `Board::terminals_focused_pane` the highlight in `ui::terminals` reads, so the two can
/// never point at different panes.
fn attached_pane<'a>(panes: &'a PaneMap, session_id: &SessionId) -> Option<&'a Pane> {
    panes
        .get(session_id)
        .filter(|pane| pane.observation().is_attached())
}

fn forwarding_pane<'a>(board: &Board, panes: &'a PaneMap) -> Option<&'a Pane> {
    if board.view != model::View::Agent || board.pane_mode != model::PaneMode::Typing {
        return None;
    }
    let session_id = board.focus_target()?;
    attached_pane(panes, &session_id)
}

fn forward_paste_if_applicable(board: &Board, panes: &PaneMap, text: &str) {
    let Some(pane) = forwarding_pane(board, panes) else {
        return;
    };
    let bytes = keys::encode_paste(text, pane.bracketed_paste());
    let _ = pane.write_input(&bytes);
}

struct IntentContext<'a> {
    board: &'a mut Board,
    client: &'a Client,
    socket: &'a std::path::Path,
    tx: &'a mpsc::Sender<NetMsg>,
    panes: &'a PaneMap,
    update_worker: &'a mut UpdateWorker,
}

fn handle_mouse(
    event: MouseEvent,
    hits: &mouse::HitMap,
    capture: &mut mouse::Capture,
    context: &mut IntentContext<'_>,
) -> bool {
    if !matches!(context.board.mode, model::Mode::Normal) {
        capture.clear();
        return false;
    }
    let terminal_context = (context.board.view == model::View::Agent
        && context.board.pane_mode == model::PaneMode::Typing)
        .then_some(hits.terminal.as_ref())
        .flatten()
        .filter(|terminal| context.board.focus_target().as_ref() == Some(&terminal.session_id))
        .and_then(|terminal| context.panes.get(&terminal.session_id))
        .filter(|pane| pane.observation().is_attached())
        .map(Pane::mouse_context)
        .filter(|context| context.enabled());

    match mouse::route(event, hits, terminal_context, capture) {
        mouse::Route::None => false,
        mouse::Route::Board(target) => {
            let intent = context.board.handle_mouse_target(target);
            apply_intent(
                intent,
                context.board,
                context.client,
                context.socket,
                context.tx,
                context.panes,
                context.update_worker,
            )
        }
        mouse::Route::Scroll { session_id, up } => {
            let Some(pane) = attached_pane(context.panes, &session_id) else {
                return false;
            };
            const SCROLL_STEP_LINES: usize = 3;
            if up {
                pane.scroll_up(SCROLL_STEP_LINES);
            } else {
                pane.scroll_down(SCROLL_STEP_LINES);
            }
            true
        }
        mouse::Route::ResetScrollback { session_id } => {
            let Some(pane) = attached_pane(context.panes, &session_id) else {
                return false;
            };
            pane.scroll_reset();
            true
        }
        mouse::Route::Terminal { session_id, bytes } => {
            let Some(pane) = attached_pane(context.panes, &session_id) else {
                return false;
            };
            pane.write_input(&bytes);
            true
        }
    }
}

fn apply_intent(
    intent: Intent,
    board: &mut Board,
    client: &Client,
    socket: &std::path::Path,
    tx: &mpsc::Sender<NetMsg>,
    panes: &PaneMap,
    update_worker: &mut UpdateWorker,
) -> bool {
    match intent {
        Intent::None => false,
        Intent::Redraw | Intent::Quit => true,
        Intent::Send(request) => {
            let operation_id = board.allocate_operation_id();
            net::spawn_request(client.clone(), tx.clone(), operation_id, request);
            true
        }
        Intent::SendWithIdentity {
            operation_id,
            request,
        } => {
            net::spawn_request(client.clone(), tx.clone(), operation_id, request);
            true
        }
        Intent::SetCapacity(capacity) => {
            board.note_info(format!(
                "capacity -> {capacity}; only factoryd restarts, runner sessions preserved; provider use may change"
            ));
            net::spawn_capacity_update(socket.to_owned(), tx.clone(), capacity);
            true
        }
        Intent::Update => {
            let Some(check) = board.update_check.clone() else {
                board.note_error("update details are unavailable; wait for the next check");
                return true;
            };
            update_worker.replace(net::spawn_update_install(
                socket.to_owned(),
                tx.clone(),
                check,
            ));
            true
        }
        Intent::ForwardKey(key) => {
            if let Some(pane) = forwarding_pane(board, panes) {
                let bytes = keys::encode_key(key, pane.key_context());
                let _ = pane.write_input(&bytes);
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
        Intent::EditFile(path) => {
            restore_terminal();
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_owned());
            let result = editor_command(&editor, &path).and_then(|mut command| {
                command
                    .status()
                    .map_err(|error| format!("editor failed: {error}"))
            });
            let _ = enable_raw_mode();
            let _ = execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableMouseCapture,
                cursor::Hide
            );
            if let Err(error) = result {
                board.note_error(error);
            }
            true
        }
    }
}

fn editor_command(editor: &str, path: &str) -> Result<std::process::Command, String> {
    let mut words =
        shell_words::split(editor).map_err(|error| format!("invalid $EDITOR: {error}"))?;
    if words.is_empty() {
        return Err("$EDITOR is empty".to_owned());
    }
    let mut command = std::process::Command::new(words.remove(0));
    command.args(words).arg(path);
    Ok(command)
}

fn apply_net_msg(
    msg: NetMsg,
    board: &mut Board,
    initial_project: Option<&factory_core::ProjectId>,
    initial_project_applied: &mut bool,
    resume: Option<&client_state::RelaunchState>,
) {
    match msg {
        NetMsg::ConnectionRetrying(detail) => board.set_retrying(detail),
        NetMsg::ConnectionLive => board.set_live(),
        NetMsg::DaemonHealth { version } => board.set_daemon_version(version),
        NetMsg::FleetSnapshot {
            projects,
            agents,
            tasks,
            runs,
            sessions,
            event_sequence,
        } => {
            let applied = board.apply_fleet_snapshot_at(
                projects,
                agents,
                tasks,
                runs,
                sessions,
                event_sequence,
            );
            if applied && !*initial_project_applied {
                if let Some(project_id) = initial_project {
                    board.focus_project(project_id.clone());
                }
                if let Some(resume) = resume {
                    apply_relaunch_state(board, resume);
                }
                *initial_project_applied = true;
            }
        }
        NetMsg::Event(event) => board.apply_event(event),
        NetMsg::EventsReplay(events) => board.apply_replay(events),
        NetMsg::CaughtUp => board.caught_up = true,
        NetMsg::OperationResult {
            operation_id,
            request,
            result,
        } => board.apply_operation_response(operation_id, request, result),
        NetMsg::CapacityResult(result) => match result {
            Ok(change) => board.note_info(format!(
                "capacity {} -> {} (runner sessions preserved)",
                change.previous, change.current
            )),
            Err(error) => board.note_error(error),
        },
        NetMsg::UpdateCheck {
            check,
            active_version,
        } => {
            board.update_check = Some(check.clone());
            match active_version {
                Ok(active_version) => {
                    board.update_available =
                        net::manual_update_candidate(&check, active_version.as_deref())
                            .map(|manifest| manifest.version.clone());
                }
                Err(error) => {
                    board.update_available = None;
                    board.note_error(format!("update check: {error}"));
                }
            }
        }
        NetMsg::UpdateProgress(progress) => board.update_progress = Some(progress),
        NetMsg::UpdateFinished(_) => unreachable!("handled by the event loop"),
        NetMsg::FleetStatus(status) => board.apply_fleet_status(status),
    }
}

#[cfg(test)]
mod main_tests {
    use super::*;
    use std::ffi::OsStr;
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::net::UnixListener;

    use factory_core::local::{LocalResponse, ServerFrame};
    use factory_core::{AgentRole, PROTOCOL_VERSION, SessionState};
    use factoryctl::update::{Asset, Manifest, UpdateCheck};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
    use ratatui::layout::Rect;

    use crate::test_fixtures::{agent, project, session};

    #[test]
    fn editor_command_parses_arguments_and_quotes_without_a_shell() {
        let command = editor_command("code --wait 'folder name'", "/tmp/file name.md").unwrap();
        assert_eq!(command.get_program(), OsStr::new("code"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("--wait"),
                OsStr::new("folder name"),
                OsStr::new("/tmp/file name.md")
            ]
        );
        assert!(editor_command("'unterminated", "/tmp/file").is_err());
        assert!(editor_command("   ", "/tmp/file").is_err());
        assert!(
            editor_command("/definitely/missing/editor --wait", "/tmp/file")
                .unwrap()
                .status()
                .is_err()
        );
    }

    #[test]
    fn context_refreshes_on_reentry_reconnect_and_a_bounded_interval() {
        let agent = factory_core::AgentId::try_from("alice").unwrap();
        let now = Instant::now();
        let mut refresh = ContextRefresh::new();
        assert!(refresh.should_refresh(&agent, true, now));
        assert!(!refresh.should_refresh(&agent, true, now + Duration::from_secs(1)));
        assert!(!refresh.should_refresh(&agent, false, now + Duration::from_secs(2)));
        assert!(refresh.should_refresh(&agent, true, now + Duration::from_secs(3)));
        assert!(!refresh.should_refresh(&agent, true, now + Duration::from_secs(4)));
        refresh.force = true;
        assert!(refresh.should_refresh(&agent, true, now + Duration::from_secs(4)));
        assert!(refresh.should_refresh(&agent, true, now + Duration::from_secs(9)));
        let requests = context_requests(factory_core::ProjectId::try_from("proj").unwrap(), agent);
        assert!(matches!(
            requests[0],
            factory_core::local::LocalRequest::GetAgent { .. }
        ));
        assert!(matches!(
            requests[1],
            factory_core::local::LocalRequest::ListAgentMessages {
                after_id: None,
                limit: 100,
                ..
            }
        ));
    }

    #[test]
    fn update_badge_compares_the_release_with_the_active_runtime() {
        let mut board = Board::new(false, 0, theme::FORTRESS);
        let check = UpdateCheck {
            checked_at_ms: 0,
            current: "0.2.0".to_owned(),
            latest: Some(Manifest {
                version: "0.2.0".to_owned(),
                assets: [(
                    factoryctl::update::platform_key().to_owned(),
                    Asset {
                        url: "https://example.invalid/release.tar.gz".to_owned(),
                        sha256: "00".repeat(32),
                    },
                )]
                .into(),
            }),
            error: None,
        };
        let mut initial_project_applied = false;

        apply_net_msg(
            NetMsg::UpdateCheck {
                check,
                active_version: Ok(Some("0.1.0".to_owned())),
            },
            &mut board,
            None,
            &mut initial_project_applied,
            None,
        );

        assert_eq!(board.update_available.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn relaunch_arguments_replace_old_state_and_restore_current_navigation() {
        let base = base_relaunch_args([
            OsString::from("--socket"),
            OsString::from("/tmp/f.sock"),
            OsString::from("--resume-state"),
            OsString::from("old"),
            OsString::from("--theme"),
            OsString::from("plain"),
        ]);
        assert_eq!(
            base,
            ["--socket", "/tmp/f.sock", "--theme", "plain"].map(OsString::from)
        );

        let mut board = Board::new(false, 0, theme::PLAIN);
        board.focused_project = Some(factory_core::ProjectId::try_from("proj").unwrap());
        board.selected_agent = Some(factory_core::AgentId::try_from("alice").unwrap());
        board.view = model::View::Agent;
        board.terminal_maximized = true;
        let args = relaunch_arguments(&board, &base).unwrap();
        let encoded = args.last().unwrap().to_str().unwrap();
        let state = client_state::decode_relaunch(encoded).unwrap();
        assert!(state.agent_view);
        assert!(state.terminal_maximized);
        assert_eq!(state.focused_project.unwrap().as_str(), "proj");
        assert_eq!(state.selected_agent.unwrap().as_str(), "alice");
    }

    #[test]
    fn relaunch_state_is_applied_only_after_ids_exist_in_the_snapshot() {
        let mut board = Board::new(false, 0, theme::PLAIN);
        let project = project("proj", 0);
        let worker = agent("alice", "proj", AgentRole::Worker, None);
        board.apply_fleet_snapshot_at(
            vec![project],
            vec![worker],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            1,
        );
        let state = client_state::RelaunchState {
            focused_project: Some(factory_core::ProjectId::try_from("proj").unwrap()),
            selected_agent: Some(factory_core::AgentId::try_from("alice").unwrap()),
            agent_view: true,
            terminal_maximized: true,
        };
        apply_relaunch_state(&mut board, &state);
        assert_eq!(board.view, model::View::Agent);
        assert_eq!(board.selected_agent.unwrap().as_str(), "alice");
        assert!(board.terminal_maximized);
        assert_eq!(board.pane_mode, model::PaneMode::Board);
    }

    #[test]
    fn fake_exec_failure_always_runs_rollback_and_reports_its_result() {
        let called = std::cell::Cell::new(false);
        let message = reexec_failure_message("exec failed".to_owned(), || {
            called.set(true);
            Ok(factoryctl::managed_update::ReexecRecovery::Restored)
        });
        assert!(called.get());
        assert_eq!(message, "exec failed; previous runtime restored");
        let message = reexec_failure_message("exec failed".to_owned(), || {
            Err("old daemon unhealthy".to_owned())
        });
        assert_eq!(
            message,
            "exec failed; rollback failed: old daemon unhealthy"
        );
        let message = reexec_failure_message("exec failed".to_owned(), || {
            Ok(factoryctl::managed_update::ReexecRecovery::NotNeeded)
        });
        assert_eq!(
            message,
            "exec failed; runtime was already active, so no rollback was needed"
        );
    }

    #[test]
    fn bootstrap_message_carries_attention_high_water_to_delayed_status_admission() {
        let mut board = Board::new(false, 0, theme::FORTRESS);
        let mut initial_project_applied = false;
        apply_net_msg(
            NetMsg::FleetSnapshot {
                projects: Vec::new(),
                agents: Vec::new(),
                tasks: Vec::new(),
                runs: Vec::new(),
                sessions: Vec::new(),
                event_sequence: 100,
            },
            &mut board,
            None,
            &mut initial_project_applied,
            None,
        );
        apply_net_msg(
            NetMsg::FleetStatus(factory_core::status::FleetStatus {
                generated_at_ms: 1,
                event_sequence: 90,
                auto_mode: true,
                live_session_cap: 4,
                live_sessions: 0,
                projects: Vec::new(),
                attention: vec![crate::test_fixtures::attention(
                    factory_core::status::AttentionReasonKind::Inferred,
                    None,
                    None,
                    None,
                    1,
                )],
            }),
            &mut board,
            None,
            &mut initial_project_applied,
            None,
        );
        assert!(board.attention_items().is_empty());
    }

    #[test]
    fn newer_bootstrap_retires_status_that_arrived_first_from_refresh_worker() {
        let mut board = Board::new(false, 0, theme::FORTRESS);
        let mut initial_project_applied = false;
        apply_net_msg(
            NetMsg::FleetStatus(factory_core::status::FleetStatus {
                generated_at_ms: 1,
                event_sequence: 90,
                auto_mode: true,
                live_session_cap: 4,
                live_sessions: 0,
                projects: Vec::new(),
                attention: vec![crate::test_fixtures::attention(
                    factory_core::status::AttentionReasonKind::Inferred,
                    None,
                    None,
                    None,
                    1,
                )],
            }),
            &mut board,
            None,
            &mut initial_project_applied,
            None,
        );
        assert_eq!(board.attention_items().len(), 1);
        apply_net_msg(
            NetMsg::FleetSnapshot {
                projects: Vec::new(),
                agents: Vec::new(),
                tasks: Vec::new(),
                runs: Vec::new(),
                sessions: Vec::new(),
                event_sequence: 100,
            },
            &mut board,
            None,
            &mut initial_project_applied,
            None,
        );
        assert!(board.attention_items().is_empty());
    }

    #[test]
    fn silent_attach_ack_enters_typing_and_first_key_reaches_the_child_path() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut attach, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(attach.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&request).unwrap()["request"]["type"],
                "attach_terminal"
            );
            let ready = ServerFrame::TerminalOutput {
                protocol_version: PROTOCOL_VERSION,
                session_id: factory_core::SessionId::try_from("session-1").unwrap(),
                generation: 0,
                offset: 0,
                bytes: String::new(),
            };
            serde_json::to_writer(&mut attach, &ready).unwrap();
            attach.write_all(b"\n").unwrap();
            attach.flush().unwrap();

            let (mut input, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(input.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let value = serde_json::from_str::<serde_json::Value>(&request).unwrap();
            assert_eq!(value["request"]["type"], "terminal_input");
            assert_eq!(
                factory_core::runner::decode_terminal_bytes(
                    value["request"]["data"]["bytes"].as_str().unwrap()
                )
                .unwrap(),
                b"x"
            );
            let accepted = ServerFrame::Response {
                protocol_version: PROTOCOL_VERSION,
                response: LocalResponse::TerminalInputAccepted {
                    session_id: factory_core::SessionId::try_from("session-1").unwrap(),
                },
            };
            serde_json::to_writer(&mut input, &accepted).unwrap();
            input.write_all(b"\n").unwrap();
            input.flush().unwrap();
        });

        let mut board = Board::new(false, 0, theme::FORTRESS);
        let mut alice = agent("alice", "proj", AgentRole::Worker, None);
        alice.current_session_id = Some(factory_core::SessionId::try_from("session-1").unwrap());
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![alice],
            Vec::new(),
            Vec::new(),
            vec![session("session-1", "alice", "proj", SessionState::Idle)],
        );
        board.view = model::View::Agent;
        board.selected_agent = Some(factory_core::AgentId::try_from("alice").unwrap());
        let mut panes = PaneMap::new();
        let client = Client::new(&socket);
        let (tx, _rx) = mpsc::channel();
        sync_panes(&mut board, &mut panes, &socket, &client, &tx, None);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !board.pane_ready && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            sync_panes(&mut board, &mut panes, &socket, &client, &tx, None);
        }
        assert!(
            board.pane_ready,
            "silent pane never observed readiness output"
        );

        let enter = board.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert_eq!(board.pane_mode, model::PaneMode::Typing);
        assert!(matches!(enter, Intent::Redraw));
        let first_key = board.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(first_key, Intent::ForwardKey(_)));
        let client = Client::new(&socket);
        let (tx, _rx) = mpsc::channel();
        apply_intent(
            first_key,
            &mut board,
            &client,
            &socket,
            &tx,
            &panes,
            &mut UpdateWorker::default(),
        );
        server.join().unwrap();
    }

    #[test]
    fn delayed_refusal_after_ready_revokes_readiness_and_blocks_all_forwarding() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (release_tx, release_rx) = mpsc::channel();
        let (input_seen_tx, input_seen_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut attach, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(attach.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&request).unwrap()["request"]["type"],
                "attach_terminal"
            );
            let ready = ServerFrame::TerminalOutput {
                protocol_version: PROTOCOL_VERSION,
                session_id: factory_core::SessionId::try_from("session-1").unwrap(),
                generation: 0,
                offset: 0,
                bytes: factory_core::runner::encode_terminal_bytes(b"\x1b[?1000h"),
            };
            serde_json::to_writer(&mut attach, &ready).unwrap();
            attach.write_all(b"\n").unwrap();
            attach.flush().unwrap();
            release_rx.recv().unwrap();

            let refusal = factory_core::local::AttachRefusal {
                project_id: factory_core::ProjectId::try_from("proj").unwrap(),
                session_id: factory_core::SessionId::try_from("session-1").unwrap(),
                runner_instance_id: Some(
                    factory_core::RunnerInstanceId::try_from("runner").unwrap(),
                ),
                session_state: Some(SessionState::Idle),
                reason: factory_core::local::AttachRefusalReason::RunnerRejected,
            };
            serde_json::to_writer(
                &mut attach,
                &ServerFrame::Response {
                    protocol_version: PROTOCOL_VERSION,
                    response: LocalResponse::AttachRefused { refusal },
                },
            )
            .unwrap();
            attach.write_all(b"\n").unwrap();
            attach.flush().unwrap();

            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((input, _)) => {
                        let _ = input_seen_tx.send(());
                        let mut ignored = String::new();
                        let _ = BufReader::new(input.try_clone().unwrap()).read_line(&mut ignored);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("unexpected input accept error: {error}"),
                }
            }
        });

        let mut board = Board::new(false, 0, theme::FORTRESS);
        let mut alice = agent("alice", "proj", AgentRole::Worker, None);
        alice.current_session_id = Some(factory_core::SessionId::try_from("session-1").unwrap());
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![alice],
            Vec::new(),
            Vec::new(),
            vec![session("session-1", "alice", "proj", SessionState::Idle)],
        );
        board.view = model::View::Agent;
        board.selected_agent = Some(factory_core::AgentId::try_from("alice").unwrap());
        let session_id = factory_core::SessionId::try_from("session-1").unwrap();
        let mut panes = PaneMap::new();
        let client = Client::new(&socket);
        let (tx, _rx) = mpsc::channel();
        sync_panes(&mut board, &mut panes, &socket, &client, &tx, None);
        assert!(
            panes
                .get(&session_id)
                .expect("attach pane")
                .wait_until_ready(Duration::from_secs(2))
        );
        sync_panes(&mut board, &mut panes, &socket, &client, &tx, None);
        assert!(board.pane_ready);
        assert!(matches!(
            board.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
            Intent::Redraw
        ));

        release_tx.send(()).unwrap();
        let pane = panes.get(&session_id).expect("ready attach pane");
        assert!(pane.wait_for_attach_refusal(Duration::from_secs(2)));
        assert!(matches!(
            pane.observation(),
            PaneObservation::AttachRefused(_)
        ));
        assert!(board.pane_ready, "readiness changes on reconciliation");
        assert!(!pane.write_input(b"direct"));

        let key = board.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(key, Intent::ForwardKey(_)));
        assert!(apply_intent(
            key,
            &mut board,
            &client,
            &socket,
            &tx,
            &panes,
            &mut UpdateWorker::default(),
        ));
        forward_paste_if_applicable(&board, &panes, "paste");

        let mut hits = mouse::HitMap::default();
        let mut update_worker = UpdateWorker::default();
        hits.set_terminal(Rect::new(0, 0, 10, 5), session_id.clone());
        assert!(!handle_mouse(
            ratatui::crossterm::event::MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
            &hits,
            &mut mouse::Capture::default(),
            &mut IntentContext {
                board: &mut board,
                client: &client,
                socket: &socket,
                tx: &tx,
                panes: &panes,
                update_worker: &mut update_worker,
            },
        ));
        assert!(
            input_seen_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );

        sync_panes(&mut board, &mut panes, &socket, &client, &tx, None);
        assert!(!board.pane_ready);
        assert_eq!(board.pane_mode, model::PaneMode::Board);
        assert!(panes.is_empty());
        assert!(board.status_line_text().contains("runner rejected attach"));
        server.join().unwrap();
    }

    #[test]
    fn asynchronous_attach_failure_is_actionable_and_retries_until_ready() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let mut attempt = 0;
            while attempt < 2 {
                let (mut attach, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(attach.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                let request = serde_json::from_str::<serde_json::Value>(&request).unwrap();
                if request["request"]["type"] != "attach_terminal" {
                    let response = match request["request"]["type"].as_str().unwrap() {
                        "latest_event_sequence" => LocalResponse::EventHead { sequence: 0 },
                        "list_projects" => LocalResponse::Projects {
                            projects: Vec::new(),
                            next_after_id: None,
                        },
                        other => panic!("unexpected refresh request: {other}"),
                    };
                    serde_json::to_writer(
                        &mut attach,
                        &ServerFrame::Response {
                            protocol_version: PROTOCOL_VERSION,
                            response,
                        },
                    )
                    .unwrap();
                    attach.write_all(b"\n").unwrap();
                    attach.flush().unwrap();
                    continue;
                }
                let frame = if attempt == 0 {
                    ServerFrame::Response {
                        protocol_version: PROTOCOL_VERSION,
                        response: LocalResponse::AttachRefused {
                            refusal: factory_core::local::AttachRefusal {
                                project_id: factory_core::ProjectId::try_from("proj").unwrap(),
                                session_id: factory_core::SessionId::try_from("session-1").unwrap(),
                                runner_instance_id: Some(
                                    factory_core::RunnerInstanceId::try_from("runner").unwrap(),
                                ),
                                session_state: Some(SessionState::Idle),
                                reason: factory_core::local::AttachRefusalReason::RunnerUnavailable,
                            },
                        },
                    }
                } else {
                    ServerFrame::TerminalOutput {
                        protocol_version: PROTOCOL_VERSION,
                        session_id: factory_core::SessionId::try_from("session-1").unwrap(),
                        generation: 0,
                        offset: 0,
                        bytes: String::new(),
                    }
                };
                serde_json::to_writer(&mut attach, &frame).unwrap();
                attach.write_all(b"\n").unwrap();
                attach.flush().unwrap();
                if attempt == 1 {
                    release_rx.recv().unwrap();
                }
                attempt += 1;
            }
        });

        let mut board = Board::new(false, 0, theme::FORTRESS);
        let mut alice = agent("alice", "proj", AgentRole::Worker, None);
        alice.current_session_id = Some(factory_core::SessionId::try_from("session-1").unwrap());
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![alice],
            Vec::new(),
            Vec::new(),
            vec![session("session-1", "alice", "proj", SessionState::Idle)],
        );
        board.view = model::View::Agent;
        board.selected_agent = Some(factory_core::AgentId::try_from("alice").unwrap());
        let mut panes = PaneMap::new();
        let client = Client::new(&socket);
        let (tx, _rx) = mpsc::channel();
        sync_panes(&mut board, &mut panes, &socket, &client, &tx, None);

        let pane = panes
            .get(&factory_core::SessionId::try_from("session-1").unwrap())
            .expect("first attach pane");
        assert!(pane.wait_for_attach_outcome(Duration::from_secs(2)));
        sync_panes(&mut board, &mut panes, &socket, &client, &tx, None);
        assert_eq!(
            board.attention_items()[0].reason.kind,
            factory_core::status::AttentionReasonKind::ObserverProblem
        );
        assert!(
            panes.is_empty(),
            "refused pane must never remain renderable"
        );
        assert!(board.status_line_text().contains("runner unavailable"));

        board.apply_fleet_status(factory_core::status::FleetStatus {
            generated_at_ms: 1,
            event_sequence: 1,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: Vec::new(),
        });
        assert!(!board.attention_items().is_empty());

        board.tick(1_001);
        sync_panes(&mut board, &mut panes, &socket, &client, &tx, None);
        let pane = panes
            .get(&factory_core::SessionId::try_from("session-1").unwrap())
            .expect("retry attach pane");
        assert!(pane.wait_until_ready(Duration::from_secs(2)));
        sync_panes(&mut board, &mut panes, &socket, &client, &tx, None);
        assert!(board.pane_ready);
        assert!(board.attention_items().is_empty());
        release_tx.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn late_ready_reconciliation_clears_refusal_with_readiness_transition() {
        let mut board = Board::new(false, 0, theme::FORTRESS);
        let session_id = factory_core::SessionId::try_from("session-1").unwrap();
        let mut alice = agent("alice", "proj", AgentRole::Worker, None);
        alice.current_session_id = Some(session_id.clone());
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![alice],
            Vec::new(),
            Vec::new(),
            vec![session("session-1", "alice", "proj", SessionState::Idle)],
        );
        assert!(
            board.note_attach_refusal(&factory_core::local::AttachRefusal {
                project_id: factory_core::ProjectId::try_from("proj").unwrap(),
                session_id: session_id.clone(),
                runner_instance_id: Some(
                    factory_core::RunnerInstanceId::try_from("runner").unwrap(),
                ),
                session_state: Some(SessionState::Stopped),
                reason: factory_core::local::AttachRefusalReason::SessionEnded,
            })
        );
        board.view = model::View::Agent;
        board.selected_agent = Some(factory_core::AgentId::try_from("alice").unwrap());

        assert!(!reconcile_pane_readiness(&mut board, None));
        assert!(!board.pane_ready);
        assert!(!board.attention_items().is_empty());

        assert!(reconcile_pane_readiness(&mut board, Some(session_id)));
        assert!(board.pane_ready);
        assert!(board.attention_items().is_empty());
    }

    #[test]
    fn stale_pane_detach_preserves_an_identity_matching_nonretryable_fence() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let mut board = Board::new(false, 0, theme::FORTRESS);
        let session_id = factory_core::SessionId::try_from("session-1").unwrap();
        let mut alice = agent("alice", "proj", AgentRole::Worker, None);
        alice.current_session_id = Some(session_id.clone());
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![alice],
            Vec::new(),
            Vec::new(),
            vec![session("session-1", "alice", "proj", SessionState::Idle)],
        );
        assert!(
            board.note_attach_refusal(&factory_core::local::AttachRefusal {
                project_id: factory_core::ProjectId::try_from("proj").unwrap(),
                session_id: session_id.clone(),
                runner_instance_id: Some(
                    factory_core::RunnerInstanceId::try_from("runner").unwrap(),
                ),
                session_state: Some(SessionState::Stopped),
                reason: factory_core::local::AttachRefusalReason::SessionEnded,
            })
        );

        board.view = model::View::Building;
        let mut panes = PaneMap::new();
        panes.insert(
            session_id.clone(),
            Pane::spawn("stale", &["/bin/cat".to_owned()], 24, 80, None).unwrap(),
        );
        let client = Client::new(&socket);
        let (tx, _rx) = mpsc::channel();
        sync_panes(&mut board, &mut panes, &socket, &client, &tx, None);

        assert!(panes.is_empty());
        assert!(!board.take_attach_retry(&session_id));
        assert!(board.attention_items().iter().any(|item| {
            item.session_id.as_ref() == Some(&session_id)
                && item.reason.kind == factory_core::status::AttentionReasonKind::ObserverProblem
        }));
    }

    #[test]
    fn exited_local_pty_stays_renderable_across_repeated_syncs_without_respawn() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let mut board = Board::new(true, 0, theme::FORTRESS);
        let alice = agent("alice", "proj", AgentRole::Worker, None);
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![alice],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        board.view = model::View::Agent;
        board.selected_agent = Some(factory_core::AgentId::try_from("alice").unwrap());
        let session_id = factory_core::SessionId::try_from("dev-alice").unwrap();
        let mut panes = PaneMap::new();
        panes.insert(
            session_id.clone(),
            Pane::spawn(
                "exiting",
                &["/bin/sh".into(), "-c".into(), "exit 0".into()],
                24,
                80,
                None,
            )
            .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(
            panes.get(&session_id).unwrap().observation(),
            PaneObservation::LocalPtyExited
        ) && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            panes.get(&session_id).unwrap().observation(),
            PaneObservation::LocalPtyExited
        ));

        let client = Client::new(&socket);
        let (tx, _rx) = mpsc::channel();
        for _ in 0..5 {
            sync_panes(&mut board, &mut panes, &socket, &client, &tx, None);
            let pane = panes.get(&session_id).expect("exited pane remains present");
            assert!(matches!(
                pane.observation(),
                PaneObservation::LocalPtyExited
            ));
            assert_eq!(
                pane.command,
                vec!["/bin/sh".to_owned(), "-c".to_owned(), "exit 0".to_owned(),],
                "sync must not replace an exited local shell with a new one"
            );
        }
        assert!(!board.pane_ready);
    }

    #[test]
    fn refusal_after_reconcile_is_not_rendered_as_an_exited_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut attach, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(attach.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let refusal = factory_core::local::AttachRefusal {
                project_id: factory_core::ProjectId::try_from("proj").unwrap(),
                session_id: factory_core::SessionId::try_from("session-1").unwrap(),
                runner_instance_id: Some(
                    factory_core::RunnerInstanceId::try_from("runner").unwrap(),
                ),
                session_state: Some(SessionState::Idle),
                reason: factory_core::local::AttachRefusalReason::RunnerRejected,
            };
            serde_json::to_writer(
                &mut attach,
                &ServerFrame::Response {
                    protocol_version: PROTOCOL_VERSION,
                    response: LocalResponse::AttachRefused { refusal },
                },
            )
            .unwrap();
            attach.write_all(b"\n").unwrap();
            attach.flush().unwrap();
        });

        let mut board = Board::new(false, 0, theme::FORTRESS);
        let mut alice = agent("alice", "proj", AgentRole::Worker, None);
        alice.current_session_id = Some(factory_core::SessionId::try_from("session-1").unwrap());
        board.apply_fleet_snapshot(
            vec![project("proj", 0)],
            vec![alice],
            Vec::new(),
            Vec::new(),
            vec![session("session-1", "alice", "proj", SessionState::Idle)],
        );
        board.view = model::View::Agent;
        board.selected_agent = Some(factory_core::AgentId::try_from("alice").unwrap());
        let session_id = factory_core::SessionId::try_from("session-1").unwrap();
        let mut panes = PaneMap::new();
        panes.insert(
            session_id,
            Pane::attach(
                socket,
                factory_core::ProjectId::try_from("proj").unwrap(),
                factory_core::SessionId::try_from("session-1").unwrap(),
                Some(factory_core::RunnerInstanceId::try_from("runner").unwrap()),
                "alice",
                24,
                80,
            )
            .unwrap(),
        );
        assert!(
            panes
                .values()
                .next()
                .unwrap()
                .wait_for_attach_refusal(Duration::from_secs(2))
        );

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &board, &mut panes);
            })
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("terminal attach refused"));
        assert!(!text.contains("[exited]"));
        server.join().unwrap();
    }
}
#[test]
fn dropping_the_update_guard_joins_the_worker() {
    let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_completed = completed.clone();
    let mut worker = UpdateWorker::default();
    worker.replace(std::thread::spawn(move || {
        worker_completed.store(true, std::sync::atomic::Ordering::Release);
    }));
    drop(worker);
    assert!(completed.load(std::sync::atomic::Ordering::Acquire));
}
