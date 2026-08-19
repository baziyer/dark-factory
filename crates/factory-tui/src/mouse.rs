//! Mouse routing for the operator board and embedded terminal.
//!
//! Board hit targets come only from the most recently rendered frame. Terminal input is a
//! separate route: coordinates must be inside the rendered terminal content, the pane must be in
//! typing mode, and the child must have enabled an xterm mouse protocol in its own output.

use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tui_term::vt100::{MouseProtocolEncoding, MouseProtocolMode};

use factory_core::status::AttentionItem;
use factory_core::{AgentId, SessionId, TaskId};

use crate::model::View;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    View(View),
    Help,
    Detach,
    Agent(AgentId),
    Task(TaskId),
    Attention(AttentionItem),
    AttentionChoice(AttentionItem, usize),
    Pane(SessionId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Region {
    area: Rect,
    target: Target,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalRegion {
    pub area: Rect,
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HitMap {
    regions: Vec<Region>,
    pub terminal: Option<TerminalRegion>,
}

impl HitMap {
    pub fn add(&mut self, area: Rect, target: Target) {
        if area.width > 0 && area.height > 0 {
            self.regions.push(Region { area, target });
        }
    }

    pub fn add_row(&mut self, area: Rect, row: usize, target: Target) {
        let Ok(row) = u16::try_from(row) else {
            return;
        };
        if row >= area.height {
            return;
        }
        self.add(
            Rect {
                x: area.x,
                y: area.y.saturating_add(row),
                width: area.width,
                height: 1,
            },
            target,
        );
    }

    pub fn set_terminal(&mut self, area: Rect, session_id: SessionId) {
        if area.width > 0 && area.height > 0 {
            self.terminal = Some(TerminalRegion { area, session_id });
        }
    }

    #[must_use]
    pub fn target_at(&self, column: u16, row: u16) -> Option<Target> {
        self.regions
            .iter()
            .rev()
            .find(|region| contains(region.area, column, row))
            .map(|region| region.target.clone())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalMouseContext {
    pub mode: MouseProtocolMode,
    pub encoding: MouseProtocolEncoding,
    pub scrolled_back: bool,
}

/// A terminal mouse press that must receive its matching drag/release even if the pointer leaves
/// the pane. Presses that begin on board controls never enter this state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Capture {
    session_id: Option<SessionId>,
}

impl Capture {
    pub fn clear(&mut self) {
        self.session_id = None;
    }
}

impl TerminalMouseContext {
    #[must_use]
    pub fn enabled(self) -> bool {
        self.mode != MouseProtocolMode::None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Route {
    None,
    Board(Target),
    Scroll {
        session_id: SessionId,
        up: bool,
    },
    ResetScrollback {
        session_id: SessionId,
    },
    Terminal {
        session_id: SessionId,
        bytes: Vec<u8>,
    },
}

/// Resolves one outer-terminal mouse event. Board targets and terminal bytes are mutually
/// exclusive by construction: terminal encoding is considered only inside `terminal.area`.
#[must_use]
pub fn route(
    event: MouseEvent,
    hits: &HitMap,
    terminal_context: Option<TerminalMouseContext>,
    capture: &mut Capture,
) -> Route {
    let in_terminal = hits
        .terminal
        .as_ref()
        .filter(|terminal| contains(terminal.area, event.column, event.row));

    // Coordinates drawn from historical scrollback do not describe the child's live screen.
    // Consume the first protocol event, clear any old press ownership, and force a redraw at the
    // live tail. Only a later event against that newly rendered frame may reach the child.
    if terminal_context.is_some_and(|context| context.enabled() && context.scrolled_back) {
        capture.clear();
        if let Some(terminal) = in_terminal {
            return match event.kind {
                MouseEventKind::ScrollUp => Route::Scroll {
                    session_id: terminal.session_id.clone(),
                    up: true,
                },
                MouseEventKind::ScrollDown => Route::Scroll {
                    session_id: terminal.session_id.clone(),
                    up: false,
                },
                _ => Route::ResetScrollback {
                    session_id: terminal.session_id.clone(),
                },
            };
        }
    }

    if let Some(session_id) = capture.session_id.clone() {
        let terminal = hits
            .terminal
            .as_ref()
            .filter(|terminal| terminal.session_id == session_id);
        let context = terminal_context.filter(|context| context.enabled());
        match (terminal, context) {
            (Some(terminal), Some(context)) => match event.kind {
                MouseEventKind::Drag(_) | MouseEventKind::Up(_) => {
                    let release = matches!(event.kind, MouseEventKind::Up(_));
                    if release {
                        capture.clear();
                    }
                    let (column, row) = relative_clamped(event, terminal.area);
                    let bytes = encode(event, column, row, context);
                    return if bytes.is_empty() {
                        Route::None
                    } else {
                        Route::Terminal { session_id, bytes }
                    };
                }
                MouseEventKind::Down(_) => capture.clear(),
                _ => {}
            },
            _ => capture.clear(),
        }
    }

    // A release or drag whose press began on a board control must never cross into the child.
    if matches!(event.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_)) {
        return Route::None;
    }

    if let Some(terminal) = in_terminal {
        if terminal_context.is_some_and(TerminalMouseContext::enabled) {
            let context = terminal_context.expect("checked above");
            let (column, row) = relative_clamped(event, terminal.area);
            let bytes = encode(event, column, row, context);
            if !bytes.is_empty() {
                if matches!(event.kind, MouseEventKind::Down(_))
                    && context.mode != MouseProtocolMode::Press
                {
                    capture.session_id = Some(terminal.session_id.clone());
                }
                return Route::Terminal {
                    session_id: terminal.session_id.clone(),
                    bytes,
                };
            }
            // An enabled child owns its content rectangle even when this particular event is
            // not representable in its chosen protocol (for example a release in X10 mode).
            // Never reinterpret such an event as a board click.
            return Route::None;
        }
        match event.kind {
            MouseEventKind::ScrollUp => {
                return Route::Scroll {
                    session_id: terminal.session_id.clone(),
                    up: true,
                };
            }
            MouseEventKind::ScrollDown => {
                return Route::Scroll {
                    session_id: terminal.session_id.clone(),
                    up: false,
                };
            }
            _ => {}
        }
    }

    if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
        return hits
            .target_at(event.column, event.row)
            .map_or(Route::None, Route::Board);
    }
    Route::None
}

fn relative_clamped(event: MouseEvent, area: Rect) -> (u16, u16) {
    let column = event
        .column
        .saturating_sub(area.x)
        .saturating_add(1)
        .clamp(1, area.width);
    let row = event
        .row
        .saturating_sub(area.y)
        .saturating_add(1)
        .clamp(1, area.height);
    (column, row)
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn encode(event: MouseEvent, column: u16, row: u16, context: TerminalMouseContext) -> Vec<u8> {
    let Some((mut button, release, motion)) = event_code(event.kind, context.mode) else {
        return Vec::new();
    };
    if release && context.encoding != MouseProtocolEncoding::Sgr {
        // Legacy encodings do not identify the released button; button 3 means release.
        button = 3;
    }
    // X10/DECSET 9 reports button presses without modifier bits. Those bits were added by the
    // later VT200 tracking modes and must not leak into X10 packets.
    if context.mode != MouseProtocolMode::Press {
        if event.modifiers.contains(KeyModifiers::SHIFT) {
            button += 4;
        }
        if event.modifiers.contains(KeyModifiers::ALT) {
            button += 8;
        }
        if event.modifiers.contains(KeyModifiers::CONTROL) {
            button += 16;
        }
    }
    if motion {
        button += 32;
    }

    match context.encoding {
        MouseProtocolEncoding::Sgr => {
            let suffix = if release { 'm' } else { 'M' };
            format!("\x1b[<{button};{column};{row}{suffix}").into_bytes()
        }
        MouseProtocolEncoding::Default => {
            let values = [button + 32, u32::from(column) + 32, u32::from(row) + 32];
            if values.iter().any(|value| *value > u32::from(u8::MAX)) {
                return Vec::new();
            }
            vec![
                0x1b,
                b'[',
                b'M',
                values[0] as u8,
                values[1] as u8,
                values[2] as u8,
            ]
        }
        MouseProtocolEncoding::Utf8 => {
            let mut bytes = b"\x1b[M".to_vec();
            for value in [button + 32, u32::from(column) + 32, u32::from(row) + 32] {
                // Xterm's 1005 extension is a one- or two-byte UTF-8 encoding, limiting
                // coordinates to 2015 (2047 after the protocol's +32 offset).
                if value > 2_047 {
                    return Vec::new();
                }
                let Some(character) = char::from_u32(value) else {
                    return Vec::new();
                };
                let mut encoded = [0; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
            bytes
        }
    }
}

fn event_code(kind: MouseEventKind, mode: MouseProtocolMode) -> Option<(u32, bool, bool)> {
    let down = |button| Some((button_code(button)?, false, false));
    match kind {
        MouseEventKind::Down(button) => down(button),
        MouseEventKind::Up(button) if mode != MouseProtocolMode::Press => {
            Some((button_code(button)?, true, false))
        }
        MouseEventKind::Drag(button)
            if matches!(
                mode,
                MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
            ) =>
        {
            Some((button_code(button)?, false, true))
        }
        MouseEventKind::Moved if mode == MouseProtocolMode::AnyMotion => Some((3, false, true)),
        MouseEventKind::ScrollUp => Some((64, false, false)),
        MouseEventKind::ScrollDown => Some((65, false, false)),
        MouseEventKind::ScrollLeft => Some((66, false, false)),
        MouseEventKind::ScrollRight => Some((67, false, false)),
        _ => None,
    }
}

fn button_code(button: MouseButton) -> Option<u32> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::MouseEvent;

    fn event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn context(mode: MouseProtocolMode, encoding: MouseProtocolEncoding) -> TerminalMouseContext {
        TerminalMouseContext {
            mode,
            encoding,
            scrolled_back: false,
        }
    }

    fn routed(
        event: MouseEvent,
        hits: &HitMap,
        terminal_context: Option<TerminalMouseContext>,
    ) -> Route {
        route(event, hits, terminal_context, &mut Capture::default())
    }

    #[test]
    fn board_click_and_terminal_input_are_disjoint() {
        let session_id = SessionId::try_from("session-1").unwrap();
        let mut hits = HitMap::default();
        hits.add(Rect::new(0, 0, 12, 1), Target::View(View::Building));
        hits.add(Rect::new(0, 2, 20, 8), Target::Pane(session_id.clone()));
        hits.set_terminal(Rect::new(1, 3, 18, 6), session_id.clone());
        let enabled = Some(context(
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Sgr,
        ));

        assert_eq!(
            routed(
                event(MouseEventKind::Down(MouseButton::Left), 3, 0),
                &hits,
                enabled
            ),
            Route::Board(Target::View(View::Building))
        );
        assert_eq!(
            routed(
                event(MouseEventKind::Down(MouseButton::Left), 0, 2),
                &hits,
                enabled
            ),
            Route::Board(Target::Pane(session_id.clone()))
        );
        assert_eq!(
            routed(
                event(MouseEventKind::Down(MouseButton::Left), 4, 5),
                &hits,
                None
            ),
            Route::Board(Target::Pane(session_id.clone()))
        );
        assert_eq!(
            routed(
                event(MouseEventKind::Down(MouseButton::Left), 4, 5),
                &hits,
                enabled
            ),
            Route::Terminal {
                session_id,
                bytes: b"\x1b[<0;4;3M".to_vec(),
            }
        );
    }

    #[test]
    fn child_mode_controls_which_events_are_encoded() {
        let mouse = event(MouseEventKind::Up(MouseButton::Left), 0, 0);
        assert!(
            encode(
                mouse,
                1,
                1,
                context(MouseProtocolMode::Press, MouseProtocolEncoding::Sgr)
            )
            .is_empty()
        );
        assert_eq!(
            encode(
                mouse,
                1,
                1,
                context(MouseProtocolMode::PressRelease, MouseProtocolEncoding::Sgr)
            ),
            b"\x1b[<0;1;1m"
        );

        let mut shifted = mouse;
        shifted.modifiers = KeyModifiers::SHIFT;
        assert_eq!(
            encode(
                shifted,
                1,
                1,
                context(
                    MouseProtocolMode::PressRelease,
                    MouseProtocolEncoding::Default
                )
            ),
            [0x1b, b'[', b'M', 39, 33, 33]
        );

        let moved = event(MouseEventKind::Moved, 0, 0);
        assert!(
            encode(
                moved,
                1,
                1,
                context(
                    MouseProtocolMode::ButtonMotion,
                    MouseProtocolEncoding::Default
                )
            )
            .is_empty()
        );
        assert_eq!(
            encode(
                moved,
                1,
                1,
                context(MouseProtocolMode::AnyMotion, MouseProtocolEncoding::Default)
            ),
            [0x1b, b'[', b'M', 67, 33, 33]
        );
    }

    #[test]
    fn x10_ignores_modifiers_and_utf8_coordinates_fail_closed_past_2015() {
        let mut shifted_press = event(MouseEventKind::Down(MouseButton::Left), 0, 0);
        shifted_press.modifiers = KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL;
        assert_eq!(
            encode(
                shifted_press,
                1,
                1,
                context(MouseProtocolMode::Press, MouseProtocolEncoding::Default)
            ),
            [0x1b, b'[', b'M', 32, 33, 33]
        );

        let press = event(MouseEventKind::Down(MouseButton::Left), 0, 0);
        assert!(
            !encode(
                press,
                2_015,
                2_015,
                context(MouseProtocolMode::PressRelease, MouseProtocolEncoding::Utf8)
            )
            .is_empty()
        );
        assert!(
            encode(
                press,
                2_016,
                1,
                context(MouseProtocolMode::PressRelease, MouseProtocolEncoding::Utf8)
            )
            .is_empty()
        );
        assert!(
            encode(
                press,
                1,
                2_016,
                context(MouseProtocolMode::PressRelease, MouseProtocolEncoding::Utf8)
            )
            .is_empty()
        );
    }

    #[test]
    fn wheel_scrolls_locally_until_the_child_enables_mouse() {
        let session_id = SessionId::try_from("session-1").unwrap();
        let mut hits = HitMap::default();
        hits.set_terminal(Rect::new(10, 5, 20, 10), session_id.clone());
        let wheel = event(MouseEventKind::ScrollUp, 12, 8);
        assert_eq!(
            routed(wheel, &hits, None),
            Route::Scroll {
                session_id: session_id.clone(),
                up: true,
            }
        );
        assert_eq!(
            routed(
                wheel,
                &hits,
                Some(context(
                    MouseProtocolMode::PressRelease,
                    MouseProtocolEncoding::Sgr
                ))
            ),
            Route::Terminal {
                session_id,
                bytes: b"\x1b[<64;3;4M".to_vec(),
            }
        );
    }

    #[test]
    fn scrolled_back_coordinate_is_consumed_without_bytes_or_capture_until_next_frame() {
        let session_id = SessionId::try_from("session-1").unwrap();
        let mut hits = HitMap::default();
        hits.set_terminal(Rect::new(10, 5, 20, 10), session_id.clone());
        let click = event(MouseEventKind::Down(MouseButton::Left), 12, 8);
        let mut capture = Capture::default();
        let mut scrolled = context(MouseProtocolMode::PressRelease, MouseProtocolEncoding::Sgr);
        scrolled.scrolled_back = true;
        let mut written = Vec::new();

        let first = route(click, &hits, Some(scrolled), &mut capture);
        if let Route::Terminal { bytes, .. } = &first {
            written.extend_from_slice(bytes);
        }
        assert_eq!(
            first,
            Route::ResetScrollback {
                session_id: session_id.clone()
            }
        );
        assert!(written.is_empty());
        assert!(capture.session_id.is_none());

        let second = route(
            click,
            &hits,
            Some(context(
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Sgr,
            )),
            &mut capture,
        );
        if let Route::Terminal { bytes, .. } = &second {
            written.extend_from_slice(bytes);
        }
        assert_eq!(written, b"\x1b[<0;3;4M");
        assert_eq!(capture.session_id, Some(session_id));
    }

    #[test]
    fn clipped_rows_and_unknown_coordinates_have_no_target() {
        let mut hits = HitMap::default();
        hits.add_row(Rect::new(2, 4, 10, 1), 0, Target::View(View::Building));
        hits.add_row(Rect::new(2, 4, 10, 1), 1, Target::View(View::Agent));
        assert_eq!(hits.target_at(2, 4), Some(Target::View(View::Building)));
        assert_eq!(hits.target_at(2, 5), None);
        assert_eq!(hits.target_at(80, 24), None);
    }

    #[test]
    fn an_unencodable_enabled_terminal_event_never_becomes_a_board_click() {
        let session_id = SessionId::try_from("session-1").unwrap();
        let mut hits = HitMap::default();
        hits.add(Rect::new(0, 0, 300, 10), Target::Pane(session_id.clone()));
        hits.set_terminal(Rect::new(0, 0, 300, 10), session_id);
        let click = event(MouseEventKind::Down(MouseButton::Left), 260, 2);
        assert_eq!(
            routed(
                click,
                &hits,
                Some(context(
                    MouseProtocolMode::Press,
                    MouseProtocolEncoding::Default
                ))
            ),
            Route::None
        );
    }

    #[test]
    fn only_a_press_started_in_the_terminal_can_forward_drag_and_release() {
        let session_id = SessionId::try_from("session-1").unwrap();
        let mut hits = HitMap::default();
        hits.add(Rect::new(0, 0, 10, 5), Target::Pane(session_id.clone()));
        hits.set_terminal(Rect::new(1, 1, 8, 3), session_id.clone());
        let enabled = Some(context(
            MouseProtocolMode::ButtonMotion,
            MouseProtocolEncoding::Sgr,
        ));
        let mut capture = Capture::default();

        // A board press dragged into the terminal cannot inject a release.
        assert!(matches!(
            route(
                event(MouseEventKind::Down(MouseButton::Left), 0, 0),
                &hits,
                enabled,
                &mut capture
            ),
            Route::Board(Target::Pane(_))
        ));
        assert_eq!(
            route(
                event(MouseEventKind::Up(MouseButton::Left), 2, 2),
                &hits,
                enabled,
                &mut capture
            ),
            Route::None
        );

        assert!(matches!(
            route(
                event(MouseEventKind::Down(MouseButton::Left), 2, 2),
                &hits,
                enabled,
                &mut capture
            ),
            Route::Terminal { .. }
        ));
        assert_eq!(
            route(
                event(MouseEventKind::Up(MouseButton::Left), 30, 20),
                &hits,
                enabled,
                &mut capture
            ),
            Route::Terminal {
                session_id,
                bytes: b"\x1b[<0;8;3m".to_vec(),
            }
        );
    }

    #[test]
    fn capture_loss_requires_a_fresh_down_after_recovery_and_never_blocks_the_board() {
        let session_id = SessionId::try_from("session-1").unwrap();
        let replacement_id = SessionId::try_from("session-2").unwrap();
        let mut hits = HitMap::default();
        hits.add(Rect::new(0, 0, 1, 1), Target::View(View::Building));
        hits.set_terminal(Rect::new(1, 1, 8, 3), session_id.clone());
        let enabled = Some(context(
            MouseProtocolMode::ButtonMotion,
            MouseProtocolEncoding::Sgr,
        ));
        let down = event(MouseEventKind::Down(MouseButton::Left), 2, 2);
        let drag = event(MouseEventKind::Drag(MouseButton::Left), 3, 2);
        let up = event(MouseEventKind::Up(MouseButton::Left), 3, 2);

        for (lost_hits, lost_context) in [
            (HitMap::default(), enabled),
            (
                {
                    let mut replaced = HitMap::default();
                    replaced.set_terminal(Rect::new(1, 1, 8, 3), replacement_id.clone());
                    replaced
                },
                enabled,
            ),
            (hits.clone(), None),
            (
                hits.clone(),
                Some(context(MouseProtocolMode::None, MouseProtocolEncoding::Sgr)),
            ),
        ] {
            let mut capture = Capture::default();
            assert!(matches!(
                route(down, &hits, enabled, &mut capture),
                Route::Terminal { .. }
            ));
            assert_eq!(capture.session_id, Some(session_id.clone()));

            assert_eq!(
                route(drag, &lost_hits, lost_context, &mut capture),
                Route::None
            );
            assert!(capture.session_id.is_none());

            // Recovery alone cannot resurrect ownership for a later drag or release.
            assert_eq!(route(drag, &hits, enabled, &mut capture), Route::None);
            assert_eq!(route(up, &hits, enabled, &mut capture), Route::None);

            // The stale capture cannot consume a normal board press.
            assert_eq!(
                route(
                    event(MouseEventKind::Down(MouseButton::Left), 0, 0),
                    &hits,
                    enabled,
                    &mut capture,
                ),
                Route::Board(Target::View(View::Building))
            );

            // Terminal motion resumes only after a new terminal press acquires capture.
            assert!(matches!(
                route(down, &hits, enabled, &mut capture),
                Route::Terminal { .. }
            ));
            assert!(matches!(
                route(drag, &hits, enabled, &mut capture),
                Route::Terminal { .. }
            ));
        }
    }
}
