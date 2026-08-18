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
use std::io;
use std::path::PathBuf;
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
use pane::{Pane, PaneMap};

struct Config {
    socket: Option<String>,
    project: Option<String>,
    dev_local_pty: bool,
    debug_log: Option<PathBuf>,
    theme: theme::Theme,
}

struct ContextRefresh {
    agent: Option<factory_core::AgentId>,
    was_agent_view: bool,
    last_refresh: Instant,
    force: bool,
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
    let mut context_refresh = ContextRefresh::new();
    let mut mouse_capture = mouse::Capture::default();
    net::spawn_update_check(tx.clone(), now_ms());

    sync_panes(board, panes, socket, debug_log);
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
                    needs_redraw |= apply_intent(intent, board, client, tx, panes);
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
                        board,
                        client,
                        tx,
                        panes,
                    );
                }
                Event::FocusGained | Event::FocusLost => {}
            }
        }

        while let Ok(msg) = rx.try_recv() {
            if matches!(&msg, NetMsg::ConnectionLive) {
                context_refresh.force = true;
            }
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
                net::spawn_update_check(tx.clone(), now_ms());
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

    let ready = board.focus_target().is_some_and(|session_id| {
        panes
            .get(&session_id)
            .is_some_and(|pane| pane.is_ready() && pane.attach_error().is_none())
    });
    changed |= board.pane_ready != ready;
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
        net::spawn_request(client.clone(), tx.clone(), request);
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
fn forwarding_target(board: &Board) -> Option<SessionId> {
    (board.view == model::View::Agent)
        .then(|| board.focus_target())
        .flatten()
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

fn handle_mouse(
    event: MouseEvent,
    hits: &mouse::HitMap,
    capture: &mut mouse::Capture,
    board: &mut Board,
    client: &Client,
    tx: &mpsc::Sender<NetMsg>,
    panes: &PaneMap,
) -> bool {
    if !matches!(board.mode, model::Mode::Normal) {
        capture.clear();
        return false;
    }
    let terminal_context = (board.view == model::View::Agent
        && board.pane_mode == model::PaneMode::Typing)
        .then_some(hits.terminal.as_ref())
        .flatten()
        .filter(|terminal| board.focus_target().as_ref() == Some(&terminal.session_id))
        .and_then(|terminal| panes.get(&terminal.session_id))
        .filter(|pane| pane.is_ready() && pane.attach_error().is_none())
        .map(Pane::mouse_context)
        .filter(|context| context.enabled());

    match mouse::route(event, hits, terminal_context, capture) {
        mouse::Route::None => false,
        mouse::Route::Board(target) => {
            let intent = board.handle_mouse_target(target);
            apply_intent(intent, board, client, tx, panes)
        }
        mouse::Route::Scroll { session_id, up } => {
            let Some(pane) = panes
                .get(&session_id)
                .filter(|pane| pane.is_ready() && pane.attach_error().is_none())
            else {
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
        mouse::Route::Terminal { session_id, bytes } => {
            let Some(pane) = panes
                .get(&session_id)
                .filter(|pane| pane.is_ready() && pane.attach_error().is_none())
            else {
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
        NetMsg::EventsReplay(events) => board.apply_replay(events),
        NetMsg::CaughtUp => board.caught_up = true,
        NetMsg::OperationResult(result) => board.apply_response(result),
        NetMsg::UpdateCheck(check) => {
            board.update_available = check.available().map(|manifest| manifest.version.clone());
        }
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
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
        sync_panes(&mut board, &mut panes, &socket, None);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !board.pane_ready && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            sync_panes(&mut board, &mut panes, &socket, None);
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
        apply_intent(first_key, &mut board, &client, &tx, &panes);
        server.join().unwrap();
    }
}
