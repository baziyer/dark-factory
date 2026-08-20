//! `factory-tui`: a detachable operator view over durable factory state.

mod client_state;
mod model;
mod mouse;
mod net;
mod theme;
mod ui;

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};

use factoryctl::Client;

use model::{Board, Intent};
use net::NetMsg;

struct Config {
    socket: Option<String>,
    project: Option<String>,
    theme: theme::Theme,
    resume: Option<client_state::RelaunchState>,
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
        let refresh = !self.was_agent_view
            || self.agent.as_ref() != Some(agent)
            || now.duration_since(self.last_refresh) >= Duration::from_secs(5)
            || self.force;
        self.was_agent_view = true;
        if refresh {
            self.agent = Some(agent.clone());
            self.last_refresh = now;
            self.force = false;
        }
        refresh
    }
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

fn parse_args() -> Config {
    let mut socket = None;
    let mut project = None;
    let mut theme = theme::FORTRESS;
    let mut resume = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next(),
            "--project" => project = args.next(),
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
                    eprintln!("factory-tui: --theme requires fortress or plain");
                    std::process::exit(2);
                };
                theme = theme::Theme::parse(&name).unwrap_or_else(|| {
                    eprintln!("factory-tui: unknown theme {name:?}");
                    std::process::exit(2);
                });
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
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
        theme,
        resume,
    }
}

fn print_help() {
    eprintln!(
        "factory-tui - the Dark Factory operator board\n\n\
         USAGE:\n    factory-tui [OPTIONS]\n\n\
         OPTIONS:\n    \
         --socket PATH        Control-socket path\n    \
         --project ID         Focus this project on startup\n    \
         --theme NAME         fortress or plain\n    \
         -h, --help           Show this help\n    \
         -V, --version        Print the version"
    );
}

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
}

fn relaunch_arguments(board: &Board, original: &[OsString]) -> Result<Vec<OsString>, String> {
    let state = client_state::RelaunchState {
        focused_project: board.focused_project.clone(),
        selected_agent: board.selected_agent.clone(),
        agent_view: board.view == model::View::Agent,
    };
    let mut arguments = original.to_vec();
    arguments.push(OsString::from("--resume-state"));
    arguments.push(OsString::from(client_state::encode_relaunch(&state)?));
    Ok(arguments)
}

fn finish_update(
    outcome: factoryctl::managed_update::InstalledUpdate,
    board: &mut Board,
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
    let message = match outcome.rollback_after_reexec_failure(&mut |_| {}) {
        Ok(factoryctl::managed_update::ReexecRecovery::Restored) => {
            format!("{seam_error}; previous runtime restored")
        }
        Ok(factoryctl::managed_update::ReexecRecovery::NotNeeded) => {
            format!("{seam_error}; runtime was already active")
        }
        Err(error) => format!("{seam_error}; rollback failed: {error}"),
    };
    board.update_progress = None;
    board.note_error(message);
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

    let socket = net::resolve_socket_path(config.socket.as_deref()).map_err(anyhow::Error::msg)?;
    let home = factory_core::paths::dark_factory_home()
        .map_err(|error| anyhow::anyhow!("cannot resolve Dark Factory home: {error}"))?;
    let client = Client::authenticated_from_file(&socket, home.join("operator.token"))
        .map_err(|error| anyhow::anyhow!("cannot load operator credential: {error}"))?;
    let cli_project = config
        .project
        .as_deref()
        .map(factory_core::ProjectId::try_from)
        .transpose()
        .map_err(|error| anyhow::anyhow!("invalid --project: {error}"))?;
    let (tx, rx) = mpsc::channel::<NetMsg>();
    net::spawn_fleet_session(client.clone(), tx.clone());
    let _fleet_status_refresh = net::spawn_fleet_status_refresh(client.clone(), tx.clone());
    let remember_focus = config.socket.is_none()
        && std::env::var_os("DARK_FACTORY_SOCKET").is_none_or(|value| value.is_empty());
    let initial_project = cli_project.or_else(|| {
        remember_focus
            .then(client_state::load_focused_project)
            .flatten()
    });
    let mut board = Board::new(now_ms(), config.theme);
    run(
        &mut terminal,
        &mut board,
        &client,
        &socket,
        &tx,
        &rx,
        initial_project,
        remember_focus,
        config.resume,
        &relaunch_args,
    )
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
    resume: Option<client_state::RelaunchState>,
    relaunch_args: &[OsString],
) -> anyhow::Result<()> {
    let tick = Duration::from_millis(150);
    let mut last_second = Instant::now();
    let mut last_update_check = Instant::now();
    let mut initial_project_applied = false;
    let mut remembered_project = board.focused_project.clone();
    let mut context_refresh = ContextRefresh::new();
    let mut update_worker = UpdateWorker::default();
    net::spawn_update_check(socket.to_owned(), tx.clone(), now_ms());
    let mut hit_map = draw_board(terminal, board)?;

    while !board.quit {
        let mut redraw = false;
        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) => {
                    redraw |= apply_intent(
                        board.handle_key(key),
                        board,
                        client,
                        socket,
                        tx,
                        &mut update_worker,
                    );
                }
                Event::Mouse(event) => {
                    if let Some(target) = mouse::route(event, &hit_map) {
                        redraw |= apply_intent(
                            board.handle_mouse_target(target),
                            board,
                            client,
                            socket,
                            tx,
                            &mut update_worker,
                        );
                    }
                }
                Event::Resize(_, _) => redraw = true,
                Event::Paste(_) | Event::FocusGained | Event::FocusLost => {}
            }
        }

        while let Ok(message) = rx.try_recv() {
            if matches!(&message, NetMsg::ConnectionLive) {
                context_refresh.force = true;
            }
            if let NetMsg::UpdateFinished(result) = message {
                update_worker.join();
                match result {
                    Ok(outcome) => finish_update(outcome, board, relaunch_args),
                    Err(error) => {
                        board.update_progress = None;
                        board.note_error(format!(
                            "update failed; current TUI remains usable: {error}"
                        ));
                    }
                }
            } else {
                apply_net_msg(
                    message,
                    board,
                    initial_project.as_ref(),
                    &mut initial_project_applied,
                    resume.as_ref(),
                );
            }
            redraw = true;
        }

        sync_agent_context(board, client, tx, &mut context_refresh);
        if remember_focus && board.focused_project != remembered_project {
            remembered_project.clone_from(&board.focused_project);
            if let Some(project_id) = &remembered_project {
                client_state::save_focused_project(project_id);
            }
        }
        if last_second.elapsed() >= Duration::from_secs(1) {
            board.tick(now_ms());
            last_second = Instant::now();
            redraw = true;
            if last_update_check.elapsed() >= factoryctl::update::CHECK_INTERVAL {
                net::spawn_update_check(socket.to_owned(), tx.clone(), now_ms());
                last_update_check = Instant::now();
            }
        }
        if redraw {
            hit_map = draw_board(terminal, board)?;
        }
    }
    Ok(())
}

fn draw_board(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    board: &Board,
) -> io::Result<mouse::HitMap> {
    let mut hits = mouse::HitMap::default();
    terminal.draw(|frame| hits = ui::draw(frame, board))?;
    Ok(hits)
}

fn sync_agent_context(
    board: &mut Board,
    client: &Client,
    tx: &mpsc::Sender<NetMsg>,
    refresh: &mut ContextRefresh,
) {
    let Some(agent_id) = board.selected_agent.clone() else {
        refresh.was_agent_view = board.view == model::View::Agent;
        return;
    };
    if !refresh.should_refresh(&agent_id, board.view == model::View::Agent, Instant::now()) {
        return;
    }
    let Some(agent) = board.agents.get(&agent_id) else {
        return;
    };
    for request in context_requests(agent.project_id.clone(), agent_id) {
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

fn apply_intent(
    intent: Intent,
    board: &mut Board,
    client: &Client,
    socket: &std::path::Path,
    tx: &mpsc::Sender<NetMsg>,
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
                "active-run capacity -> {capacity}; factoryd will restart"
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
        Intent::EditFile(path) => {
            restore_terminal();
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_owned());
            let result = editor_command(&editor, &path).and_then(|mut command| {
                command
                    .status()
                    .map_err(|error| format!("editor failed: {error}"))
            });
            enter_terminal();
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
    message: NetMsg,
    board: &mut Board,
    initial_project: Option<&factory_core::ProjectId>,
    initial_project_applied: &mut bool,
    resume: Option<&client_state::RelaunchState>,
) {
    match message {
        NetMsg::ConnectionRetrying(detail) => board.set_retrying(detail),
        NetMsg::ConnectionLive => board.set_live(),
        NetMsg::DaemonHealth { version } => board.set_daemon_version(version),
        NetMsg::FleetSnapshot {
            projects,
            agents,
            tasks,
            runs,
            event_sequence,
        } => {
            let applied =
                board.apply_fleet_snapshot_at(projects, agents, tasks, runs, event_sequence);
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
        } => {
            board.apply_operation_response(operation_id, request, result);
        }
        NetMsg::CapacityResult(result) => match result {
            Ok(change) => board.note_info(format!(
                "active-run capacity {} -> {}",
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
