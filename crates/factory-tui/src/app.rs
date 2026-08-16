//! Application state: two terminal panes, focus/zoom, and the prefix-key mode toggle.
//!
//! Mode model (see `SPIKE.md` "Input routing" for the rationale behind the prefix key choice):
//! the app is always in exactly one of two modes. In `Pane` mode (the default) every key is
//! re-encoded (see `keys.rs`) and forwarded to the focused child PTY. In `Control` mode, keys
//! are interpreted as app commands (`Tab`/`h`/`l` change focus, `z` zooms, `q` quits) and
//! nothing is forwarded. The prefix key (`Ctrl-]`) toggles between the two modes from either
//! side; `Esc` also returns to `Pane` mode from `Control` mode.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use tui_term::widget::PseudoTerminal;

use crate::keys::{self};
use crate::pane::Pane;

/// The prefix key is the physical `Ctrl-]` (raw byte 0x1D, ASCII GS). Chosen because: it's the
/// classic telnet/rlogin escape character, so operators with terminal muscle memory already
/// associate it with "control the wrapper, not the thing inside it"; it's a single byte with no
/// `Alt`/multi-key ambiguity to resolve; and in practice neither `claude`/`codex` nor common
/// shells/editors (bash, vim, htop) bind it to anything reachable from normal (non-prefixed)
/// input. The known cost: an app that specifically wants literal `Ctrl-]` (e.g. vim's tag-jump
/// in normal mode) can't reach it while focused through us - the same trade every terminal
/// multiplexer makes with its own prefix key.
///
/// The code below is `Char('5')`, not `Char(']')` - this is not a typo. Without Kitty keyboard
/// protocol negotiation, crossterm's legacy Unix decoder maps the raw bytes 0x1C-0x1F to
/// `Char('4'..'7') + CONTROL` rather than to the punctuation characters those bytes are
/// conventionally named after (confirmed by reading `crossterm::event::sys::unix::parse::parse_event`;
/// verified empirically too - see `SPIKE.md` "Input routing"). `keys::ctrl_byte` maps `'5'` back
/// to the same 0x1D byte, so this still round-trips correctly for panes that want literal
/// `Ctrl-]`; only the *name* crossterm assigns it is surprising.
pub const PREFIX_KEY: KeyEvent = KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Pane,
    Control,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Left,
    Right,
}

impl Focus {
    fn toggle(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }
}

pub struct App {
    pub panes: [Pane; 2],
    pub focus: Focus,
    pub mode: Mode,
    pub zoomed: bool,
    pub running: bool,
}

impl App {
    #[must_use]
    pub fn new(panes: [Pane; 2]) -> Self {
        Self {
            panes,
            focus: Focus::Left,
            mode: Mode::Pane,
            zoomed: false,
            running: true,
        }
    }

    fn focused_pane(&self) -> &Pane {
        &self.panes[self.focus.index()]
    }

    /// Handles one crossterm key event. Returns `true` if it caused a state change worth an
    /// immediate redraw (we redraw on a tick regardless, but this keeps input feeling instant).
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key == PREFIX_KEY {
            self.mode = match self.mode {
                Mode::Pane => Mode::Control,
                Mode::Control => Mode::Pane,
            };
            return true;
        }

        match self.mode {
            Mode::Control => self.handle_control_key(key),
            Mode::Pane => self.forward_key(key),
        }
    }

    fn handle_control_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Tab | KeyCode::Char('h') | KeyCode::Char('l') => {
                self.focus = self.focus.toggle();
                true
            }
            KeyCode::Char('z') => {
                self.zoomed = !self.zoomed;
                true
            }
            KeyCode::Char('q') => {
                self.running = false;
                true
            }
            KeyCode::Esc => {
                self.mode = Mode::Pane;
                true
            }
            _ => false,
        }
    }

    fn forward_key(&mut self, key: KeyEvent) -> bool {
        let pane = self.focused_pane();
        let bytes = keys::encode_key(key, pane.key_context());
        if bytes.is_empty() {
            return false;
        }
        pane.write_input(&bytes);
        true
    }

    /// Forwards pasted text to the focused pane, matching the child's bracketed-paste mode.
    pub fn handle_paste(&mut self, text: &str) {
        if self.mode != Mode::Pane {
            return;
        }
        let pane = self.focused_pane();
        let bytes = keys::encode_paste(text, pane.bracketed_paste());
        pane.write_input(&bytes);
    }

    /// Computes each pane's outer rect for the given frame area (before border trimming).
    /// Zoomed-out (hidden) panes get an empty rect.
    fn pane_rects(&self, area: Rect) -> [Rect; 2] {
        if self.zoomed {
            let mut rects = [Rect::default(), Rect::default()];
            rects[self.focus.index()] = area;
            rects
        } else {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
            [chunks[0], chunks[1]]
        }
    }

    /// Resizes each visible pane's PTY (and `vt100` model) to match its current inner area, so
    /// the child receives SIGWINCH and can reflow. Called once per frame before drawing; a
    /// no-op when the size hasn't changed (see `Pane::resize`).
    pub fn sync_pty_sizes(&mut self, area: Rect) {
        let status_area = status_line_area(area);
        let content_area = content_area(area, status_area);
        let rects = self.pane_rects(content_area);
        for (pane, rect) in self.panes.iter_mut().zip(rects) {
            let inner = bordered_inner(rect);
            if inner.width > 0 && inner.height > 0 {
                let _ = pane.resize(inner.height, inner.width);
            }
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let status_area = status_line_area(area);
        let content_area = content_area(area, status_area);
        let rects = self.pane_rects(content_area);

        let mut cursor: Option<Position> = None;
        for (index, rect) in rects.into_iter().enumerate() {
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            let focused = self.focus.index() == index;
            let exited = self.panes[index].has_exited();
            let title = pane_title(&self.panes[index], focused, exited);
            let block = pane_block(focused, self.mode == Mode::Control, &title);
            let inner = bordered_inner(rect);

            // Locked for the span of this render: `PseudoTerminal` borrows the screen for its
            // lifetime, so the mutex guard has to outlive the `render_widget` call.
            let parser = self.panes[index].lock_parser();
            let screen = parser.screen();
            let pseudo = PseudoTerminal::new(screen).block(block);
            frame.render_widget(pseudo, rect);

            if focused && self.mode == Mode::Pane && !screen.hide_cursor() {
                let (row, col) = screen.cursor_position();
                let x = inner.x.saturating_add(col);
                let y = inner.y.saturating_add(row);
                if x < inner.x + inner.width && y < inner.y + inner.height {
                    cursor = Some(Position { x, y });
                }
            }
            drop(parser);
        }

        frame.render_widget(self.status_line(), status_area);
        if let Some(pos) = cursor {
            frame.set_cursor_position(pos);
        }
    }

    fn status_line(&self) -> Paragraph<'_> {
        let mode = match self.mode {
            Mode::Pane => "PANE",
            Mode::Control => "CONTROL",
        };
        let focus = match self.focus {
            Focus::Left => "left",
            Focus::Right => "right",
        };
        let text = Line::from(vec![
            Span::styled(
                format!(" {mode} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(if self.mode == Mode::Control {
                        Color::Yellow
                    } else {
                        Color::Cyan
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " focus={focus} zoom={} · Ctrl-] toggle mode · Tab/h/l focus · z zoom · q quit ",
                self.zoomed
            )),
        ]);
        Paragraph::new(text)
    }
}

fn status_line_area(area: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(1),
        width: area.width,
        height: area.height.min(1),
    }
}

fn content_area(area: Rect, status: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(status.height),
    }
}

fn pane_title(pane: &Pane, focused: bool, exited: bool) -> String {
    let cmd = pane.command.join(" ");
    let marker = if exited { " [exited]" } else { "" };
    let focus_marker = if focused { "*" } else { " " };
    format!("{focus_marker} {} — {cmd}{marker}", pane.title)
}

/// The drawable area inside a single-cell `Borders::ALL` block, independent of its title.
fn bordered_inner(rect: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(rect)
}

fn pane_block(focused: bool, control_mode: bool, title: &str) -> Block<'_> {
    let color = match (focused, control_mode) {
        (true, true) => Color::Yellow,
        (true, false) => Color::Cyan,
        (false, _) => Color::DarkGray,
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(title.to_string())
}
