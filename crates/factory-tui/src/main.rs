//! `factory-tui`: the Dwarf-Fortress-flavored operator board. See `README.md` for keys, layout,
//! and what's stubbed pending the daemon's terminal-attach protocol; see `SPIKE.md` for the
//! terminal-fidelity research this crate grew out of.
//!
//! This binary is a thin shell: `model.rs` owns all state and key-handling (pure, unit tested),
//! `net.rs` owns every socket, `ui/` renders `Board`, and this file wires the three together
//! plus the one thing none of them can own alone — a live zoomed terminal pane (`pane.rs`,
//! reused verbatim from the fidelity spike), since that needs both `Board`'s mode and a PTY's
//! `vt100::Screen` at once.

mod keys;
mod model;
mod net;
mod pane;
mod query;
mod ui;

use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use tui_term::widget::PseudoTerminal;

use factory_core::AgentId;

use factoryctl::Client;

use model::{Board, Intent, Mode, PickerKind, PickerState};
use net::NetMsg;
use pane::Pane;

struct Config {
    socket: Option<String>,
    project: Option<String>,
    dev_local_pty: bool,
    debug_log: Option<PathBuf>,
}

fn parse_args() -> Config {
    let mut socket = None;
    let mut project = None;
    let mut dev_local_pty = false;
    let mut debug_log = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next(),
            "--project" => project = args.next(),
            "--dev-local-pty" => dev_local_pty = true,
            "--debug-log" => debug_log = args.next().map(PathBuf::from),
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
        socket,
        project,
        dev_local_pty,
        debug_log,
    }
}

fn print_help() {
    eprintln!(
        "factory-tui - the Dark Factory operator board\n\n\
         USAGE:\n    factory-tui [OPTIONS]\n\n\
         OPTIONS:\n    \
         --socket PATH        Control-socket path (default: see README.md's 3-step resolution)\n    \
         --project ID         Skip the project picker and use this project\n    \
         --dev-local-pty      Zooming an agent spawns a local shell pane instead of a\n                          \
         placeholder (the daemon's terminal-attach protocol hasn't landed yet)\n    \
         --debug-log DIR      With --dev-local-pty, log the zoom pane's raw PTY bytes to DIR\n    \
         -h, --help            Show this help\n\n\
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
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
    let client = Client::new(socket);

    let cli_project = config
        .project
        .as_deref()
        .map(|value| {
            factory_core::ProjectId::try_from(value)
                .map_err(|error| anyhow::anyhow!("invalid --project: {error}"))
        })
        .transpose()?;

    let (tx, rx) = mpsc::channel::<NetMsg>();
    net::spawn_project_list(client.clone(), tx.clone());

    let mut board = Board::new(config.dev_local_pty, now_ms());
    let mut zoom_pane: Option<(AgentId, Pane)> = None;

    run(
        &mut terminal,
        &mut board,
        &client,
        &tx,
        &rx,
        cli_project,
        &mut zoom_pane,
        config.debug_log.as_deref(),
    )?;

    if let Some((_, mut pane)) = zoom_pane {
        pane.kill();
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    board: &mut Board,
    client: &Client,
    tx: &mpsc::Sender<NetMsg>,
    rx: &mpsc::Receiver<NetMsg>,
    cli_project: Option<factory_core::ProjectId>,
    zoom_pane: &mut Option<(AgentId, Pane)>,
    debug_log: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    // Not a busy-poll: `event::poll` blocks (efficiently, via the OS) for up to `tick` with ~0
    // CPU when idle. This cadence is how the board notices net-thread messages and ticks elapsed
    // time; redraws themselves only happen when something actually changed (see `needs_redraw`).
    let tick = Duration::from_millis(150);
    let mut last_second = Instant::now();

    terminal.draw(|frame| draw(frame, board, zoom_pane))?;

    loop {
        if board.quit {
            break;
        }

        let mut needs_redraw = false;

        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) => {
                    let intent = board.handle_key(key);
                    needs_redraw |= apply_intent(intent, board, client, tx, zoom_pane, debug_log);
                }
                Event::Paste(text) => {
                    forward_paste_if_zoomed(board, zoom_pane, &text);
                    needs_redraw = true;
                }
                Event::Resize(_, _) => needs_redraw = true,
                // Mouse/focus are received but not acted on - see SPIKE.md "Mouse" for why
                // (neither target app's mouse support is wired through this crate yet).
                Event::Mouse(_) | Event::FocusGained | Event::FocusLost => {}
            }
        }

        while let Ok(msg) = rx.try_recv() {
            apply_net_msg(msg, board, client, tx, cli_project.as_ref());
            needs_redraw = true;
        }

        if let Some((_, pane)) = zoom_pane.as_ref() {
            if pane.dirty() {
                needs_redraw = true;
            }
        }

        if last_second.elapsed() >= Duration::from_secs(1) {
            board.tick(now_ms());
            last_second = Instant::now();
            needs_redraw = true;
        }

        if needs_redraw {
            terminal.draw(|frame| draw(frame, board, zoom_pane))?;
        }
    }

    Ok(())
}

fn draw(frame: &mut Frame, board: &Board, zoom_pane: &mut Option<(AgentId, Pane)>) {
    match &board.mode {
        Mode::Zoomed(agent_id) => {
            if board.dev_local_pty {
                if let Some((_, pane)) = zoom_pane.as_mut() {
                    draw_zoom_pane(frame, board, pane);
                    return;
                }
            }
            draw_zoom_placeholder(frame, board, agent_id);
        }
        _ => ui::draw(frame, board),
    }
}

fn split_zoom(area: Rect) -> (Rect, Rect) {
    let status_height = 1u16.min(area.height);
    let content = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(status_height),
    };
    let status = Rect {
        x: area.x,
        y: area.y + content.height,
        width: area.width,
        height: status_height,
    };
    (content, status)
}

fn draw_zoom_pane(frame: &mut Frame, board: &Board, pane: &mut Pane) {
    let area = frame.area();
    let (content, status) = split_zoom(area);

    let exited = pane.has_exited();
    let marker = if exited { " [exited]" } else { "" };
    let mode_hint = if board.zoom_forwarding {
        "PANE - Ctrl-] for zoom-control"
    } else {
        "CONTROL - Esc/z exits zoom"
    };
    let color = if board.zoom_forwarding {
        Color::Cyan
    } else {
        Color::Yellow
    };
    let title = format!(
        " {} — {}{marker}  [{mode_hint}] ",
        pane.title,
        pane.command.join(" ")
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(title);
    let inner = block.inner(content);
    if inner.width > 0 && inner.height > 0 {
        let _ = pane.resize(inner.height, inner.width);
    }

    let parser = pane.lock_parser();
    let screen = parser.screen();
    let pseudo = PseudoTerminal::new(screen).block(block);
    frame.render_widget(pseudo, content);
    let mut cursor_position = None;
    if board.zoom_forwarding && !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        let x = inner.x.saturating_add(col);
        let y = inner.y.saturating_add(row);
        if x < inner.x + inner.width && y < inner.y + inner.height {
            cursor_position = Some(Position { x, y });
        }
    }
    drop(parser);

    ui::draw_status_line(frame, status, board);
    if let Some(position) = cursor_position {
        frame.set_cursor_position(position);
    }
}

fn draw_zoom_placeholder(frame: &mut Frame, board: &Board, agent_id: &AgentId) {
    let area = frame.area();
    let (content, status) = split_zoom(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {agent_id} — terminal "));
    let inner = block.inner(content);
    frame.render_widget(block, content);

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("looking at {agent_id}"),
            Style::default().fg(Color::Cyan),
        )),
        Line::from(""),
        Line::from("the daemon's terminal-attach protocol hasn't landed yet (see README.md)."),
        Line::from(
            "run with --dev-local-pty to exercise the zoom mechanics against a local shell.",
        ),
        Line::from(""),
        Line::from("Esc / z / Enter — back to the board"),
    ];
    frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), inner);
    ui::draw_status_line(frame, status, board);
}

fn forward_paste_if_zoomed(board: &Board, zoom_pane: &Option<(AgentId, Pane)>, text: &str) {
    if !matches!(board.mode, Mode::Zoomed(_)) || !board.dev_local_pty || !board.zoom_forwarding {
        return;
    }
    if let Some((_, pane)) = zoom_pane.as_ref() {
        let bytes = keys::encode_paste(text, pane.bracketed_paste());
        pane.write_input(&bytes);
    }
}

fn apply_intent(
    intent: Intent,
    board: &mut Board,
    client: &Client,
    tx: &mpsc::Sender<NetMsg>,
    zoom_pane: &mut Option<(AgentId, Pane)>,
    debug_log: Option<&std::path::Path>,
) -> bool {
    match intent {
        Intent::None => false,
        Intent::Redraw | Intent::Quit => true,
        Intent::Send(request) => {
            net::spawn_request(client.clone(), tx.clone(), request);
            true
        }
        Intent::ProjectSelected(project_id) => {
            net::spawn_project_session(client.clone(), project_id, tx.clone());
            true
        }
        Intent::EnterZoom(agent_id) => {
            if board.dev_local_pty {
                match Pane::spawn(
                    agent_id.as_str(),
                    &["bash".to_owned()],
                    24,
                    80,
                    debug_log.map(|dir| dir.join("zoom.debug.log")),
                ) {
                    Ok(pane) => *zoom_pane = Some((agent_id, pane)),
                    Err(error) => board.note_error(format!("couldn't spawn zoom pane: {error}")),
                }
            }
            true
        }
        Intent::ExitZoom => {
            if let Some((_, mut pane)) = zoom_pane.take() {
                pane.kill();
            }
            true
        }
        Intent::ForwardKey(key) => {
            if let Some((_, pane)) = zoom_pane.as_ref() {
                let bytes = keys::encode_key(key, pane.key_context());
                pane.write_input(&bytes);
            }
            true
        }
    }
}

fn apply_net_msg(
    msg: NetMsg,
    board: &mut Board,
    client: &Client,
    tx: &mpsc::Sender<NetMsg>,
    cli_project: Option<&factory_core::ProjectId>,
) {
    match msg {
        NetMsg::ConnectionRetrying(detail) => board.set_retrying(detail),
        NetMsg::ConnectionLive => board.set_live(),
        NetMsg::Projects(projects) => handle_projects(projects, board, client, tx, cli_project),
        NetMsg::ProjectSnapshot {
            agents,
            tasks,
            runs,
        } => {
            board.apply_project_snapshot(agents, tasks, runs);
        }
        NetMsg::Event(event) => board.apply_event(event),
        NetMsg::CaughtUp => board.caught_up = true,
        NetMsg::OperationResult(result) => board.apply_response(result),
    }
}

fn handle_projects(
    projects: Vec<factory_core::ProjectSnapshot>,
    board: &mut Board,
    client: &Client,
    tx: &mpsc::Sender<NetMsg>,
    cli_project: Option<&factory_core::ProjectId>,
) {
    board.set_projects(projects.clone());
    if board.project.is_some() {
        return;
    }

    let matched_cli = cli_project.and_then(|id| projects.iter().find(|project| &project.id == id));
    let chosen = matched_cli
        .cloned()
        .or_else(|| (projects.len() == 1).then(|| projects[0].clone()));

    if let Some(project) = chosen {
        let project_id = project.id.clone();
        board.select_project(project);
        net::spawn_project_session(client.clone(), project_id, tx.clone());
        return;
    }

    if board.projects.is_empty() {
        board.note_error("no projects on this daemon - create one with factoryctl project create");
        return;
    }
    if let Some(id) = cli_project {
        if matched_cli.is_none() {
            board.note_error(format!("--project {id} not found; pick one"));
        }
    }
    board.mode = Mode::Picker(PickerState {
        kind: PickerKind::Project,
        cursor: 0,
    });
}
