//! Re-encodes crossterm's parsed key events back into the raw byte sequences a real terminal
//! would have sent, so they can be written into a child PTY.
//!
//! We deliberately do **not** negotiate the Kitty keyboard protocol on our own (outer) terminal:
//! doing so would let crossterm report unambiguous press/release/repeat events with exact
//! modifiers, but it roughly doubles the surface area of this encoder for a spike, and neither
//! target app depends on it being on (see `SPIKE.md`). What follows is the legacy/xterm
//! encoding, which is what every terminal sends without protocol negotiation, and what both
//! `claude` and `codex` fall back to.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Terminal modes that change how a key is encoded. These mirror `vt100::Screen`'s
/// `application_cursor` / `application_keypad`, which the child controls via DECSET/DECRST
/// (`CSI ?1h` / `CSI ?1l`, `CSI ?66h` / `CSI ?66l`) — i.e. this is state the *child* asked for,
/// not something we choose.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyContext {
    pub application_cursor: bool,
    #[allow(dead_code)] // Reserved: keypad-specific encoding is a documented follow-up.
    pub application_keypad: bool,
}

/// Encodes a single key event as the bytes a real terminal would write to its PTY master.
///
/// Returns an empty vector for events that shouldn't be forwarded at all (key-up/repeat when a
/// terminal we don't support would have produced them, or keys with no terminal representation
/// such as `CapsLock`).
#[must_use]
pub fn encode_key(key: KeyEvent, ctx: KeyContext) -> Vec<u8> {
    // Without Kitty keyboard protocol negotiation, crossterm on Unix only ever emits `Press`.
    // Guard anyway in case that assumption changes.
    if key.kind != KeyEventKind::Press {
        return Vec::new();
    }

    let modifiers = key.modifiers;
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Char(c) => encode_char(c, ctrl, alt),
        KeyCode::Enter => alt_prefix(alt, b"\r".to_vec()),
        KeyCode::Tab => {
            if shift {
                b"\x1b[Z".to_vec()
            } else {
                alt_prefix(alt, b"\t".to_vec())
            }
        }
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => {
            if ctrl {
                alt_prefix(alt, vec![0x08])
            } else {
                alt_prefix(alt, vec![0x7f])
            }
        }
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor_key('A', ctx.application_cursor, shift, alt, ctrl),
        KeyCode::Down => cursor_key('B', ctx.application_cursor, shift, alt, ctrl),
        KeyCode::Right => cursor_key('C', ctx.application_cursor, shift, alt, ctrl),
        KeyCode::Left => cursor_key('D', ctx.application_cursor, shift, alt, ctrl),
        KeyCode::Home => cursor_key('H', ctx.application_cursor, shift, alt, ctrl),
        KeyCode::End => cursor_key('F', ctx.application_cursor, shift, alt, ctrl),
        KeyCode::PageUp => tilde_key(5, shift, alt, ctrl),
        KeyCode::PageDown => tilde_key(6, shift, alt, ctrl),
        KeyCode::Insert => tilde_key(2, shift, alt, ctrl),
        KeyCode::Delete => tilde_key(3, shift, alt, ctrl),
        KeyCode::F(n) => function_key(n, shift, alt, ctrl),
        KeyCode::Null => vec![0x00],
        // No terminal byte representation; media/lock/modifier-only keys are not forwarded.
        _ => Vec::new(),
    }
}

/// Encodes pasted text for the child, wrapping it in bracketed-paste markers iff the child has
/// requested bracketed paste mode (`CSI ?2004h`, tracked by `vt100::Screen::bracketed_paste`).
#[must_use]
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut out = Vec::with_capacity(text.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(text.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.as_bytes().to_vec()
    }
}

fn alt_prefix(alt: bool, mut bytes: Vec<u8>) -> Vec<u8> {
    if alt {
        let mut out = vec![0x1b];
        out.append(&mut bytes);
        out
    } else {
        bytes
    }
}

fn encode_char(c: char, ctrl: bool, alt: bool) -> Vec<u8> {
    if ctrl {
        if let Some(byte) = ctrl_byte(c) {
            return alt_prefix(alt, vec![byte]);
        }
    }
    let mut buf = [0u8; 4];
    let bytes = c.encode_utf8(&mut buf).as_bytes().to_vec();
    alt_prefix(alt, bytes)
}

/// Maps a character to its control-code byte, following standard terminal conventions
/// (`Ctrl-A`..`Ctrl-Z` => 0x01..0x1a, plus the punctuation controls xterm recognizes).
/// Returns `None` when the combination has no defined control code (e.g. `Ctrl-1`); callers
/// should fall back to sending the plain character in that case.
///
/// The `'4'..='7'` arm exists because of a crossterm quirk (see `crate::app::PREFIX_KEY`):
/// without Kitty keyboard protocol negotiation, crossterm's legacy Unix parser decodes the raw
/// control bytes 0x1C-0x1F as `Char('4'..'7') + CONTROL` rather than `Char('\\'/']'/'^'/'_') +
/// CONTROL` - so *that* is what we actually receive for e.g. a physical `Ctrl-]` press, and it's
/// what has to map back to the same byte for a round trip. The punctuation arms below are kept
/// too, in case a future backend (Kitty protocol, a different platform) reports the character
/// form directly instead.
fn ctrl_byte(c: char) -> Option<u8> {
    match c.to_ascii_uppercase() {
        'A'..='Z' => Some((c.to_ascii_uppercase() as u8) - b'A' + 1),
        '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        '4' => Some(0x1c),
        '5' => Some(0x1d),
        '6' => Some(0x1e),
        '7' => Some(0x1f),
        '?' => Some(0x7f),
        ' ' => Some(0x00),
        _ => None,
    }
}

/// Modifier parameter for CSI sequences, per xterm's `modifyOtherKeys`/cursor-key convention:
/// 1 + shift(1) + alt(2) + ctrl(4). Returns `None` when no modifier is held, since unmodified
/// sequences omit the parameter entirely.
fn modifier_param(shift: bool, alt: bool, ctrl: bool) -> Option<u8> {
    if !shift && !alt && !ctrl {
        return None;
    }
    let mut n = 1u8;
    if shift {
        n += 1;
    }
    if alt {
        n += 2;
    }
    if ctrl {
        n += 4;
    }
    Some(n)
}

/// Encodes an arrow/Home/End key. Unmodified sequences honor the child's application-cursor
/// mode (`ESC O` vs `ESC [`); modified sequences always use the `CSI 1 ; mod <letter>` form,
/// per xterm, regardless of application-cursor mode.
fn cursor_key(letter: char, app_cursor: bool, shift: bool, alt: bool, ctrl: bool) -> Vec<u8> {
    match modifier_param(shift, alt, ctrl) {
        Some(m) => format!("\x1b[1;{m}{letter}").into_bytes(),
        None => {
            let introducer = if app_cursor { 'O' } else { '[' };
            format!("\x1b{introducer}{letter}").into_bytes()
        }
    }
}

/// Encodes a `CSI <n> ~` style key (PageUp/PageDown/Insert/Delete), adding the modifier
/// parameter when present: `CSI <n> ; <mod> ~`.
fn tilde_key(n: u8, shift: bool, alt: bool, ctrl: bool) -> Vec<u8> {
    match modifier_param(shift, alt, ctrl) {
        Some(m) => format!("\x1b[{n};{m}~").into_bytes(),
        None => format!("\x1b[{n}~").into_bytes(),
    }
}

/// Encodes F1-F12. F1-F4 use the SS3 form when unmodified (`ESC O P..S`), matching xterm's
/// default `TERM=xterm-256color` terminfo; F5-F12 (and any modified F1-F4) use `CSI <n> ~`.
fn function_key(n: u8, shift: bool, alt: bool, ctrl: bool) -> Vec<u8> {
    let modifier = modifier_param(shift, alt, ctrl);
    if (1..=4).contains(&n) {
        let letter = (b'P' + (n - 1)) as char;
        return match modifier {
            Some(m) => format!("\x1b[1;{m}{letter}").into_bytes(),
            None => format!("\x1bO{letter}").into_bytes(),
        };
    }
    let code = match n {
        1 => 11,
        2 => 12,
        3 => 13,
        4 => 14,
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return Vec::new(),
    };
    match modifier {
        Some(m) => format!("\x1b[{code};{m}~").into_bytes(),
        None => format!("\x1b[{code}~").into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventState;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn enc(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode_key(key(code, modifiers), KeyContext::default())
    }

    #[test]
    fn plain_char_roundtrips_utf8() {
        assert_eq!(enc(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(enc(KeyCode::Char('A'), KeyModifiers::SHIFT), b"A");
        assert_eq!(enc(KeyCode::Char('é'), KeyModifiers::NONE), "é".as_bytes());
    }

    #[test]
    fn ctrl_letters_map_to_control_codes() {
        assert_eq!(enc(KeyCode::Char('c'), KeyModifiers::CONTROL), [0x03]);
        assert_eq!(enc(KeyCode::Char('d'), KeyModifiers::CONTROL), [0x04]);
        assert_eq!(enc(KeyCode::Char('a'), KeyModifiers::CONTROL), [0x01]);
        assert_eq!(enc(KeyCode::Char('z'), KeyModifiers::CONTROL), [0x1a]);
    }

    #[test]
    fn ctrl_punctuation_matches_xterm() {
        assert_eq!(enc(KeyCode::Char('['), KeyModifiers::CONTROL), [0x1b]);
        assert_eq!(enc(KeyCode::Char(']'), KeyModifiers::CONTROL), [0x1d]);
        assert_eq!(enc(KeyCode::Char('\\'), KeyModifiers::CONTROL), [0x1c]);
        assert_eq!(enc(KeyCode::Char('?'), KeyModifiers::CONTROL), [0x7f]);
        assert_eq!(enc(KeyCode::Char(' '), KeyModifiers::CONTROL), [0x00]);
    }

    #[test]
    fn ctrl_4_5_6_7_map_to_the_0x1c_0x1f_control_bytes() {
        // Reproduces crossterm's legacy Unix decoding of raw bytes 0x1C-0x1F: it reports them
        // as `Char('4'..'7') + CONTROL`, not `Char('\\'/']'/'^'/'_') + CONTROL`. This is exactly
        // what a physical `Ctrl-]` keypress (our prefix key) decodes to - see
        // `crate::app::PREFIX_KEY`.
        assert_eq!(enc(KeyCode::Char('4'), KeyModifiers::CONTROL), [0x1c]);
        assert_eq!(enc(KeyCode::Char('5'), KeyModifiers::CONTROL), [0x1d]);
        assert_eq!(enc(KeyCode::Char('6'), KeyModifiers::CONTROL), [0x1e]);
        assert_eq!(enc(KeyCode::Char('7'), KeyModifiers::CONTROL), [0x1f]);
    }

    #[test]
    fn ctrl_digit_has_no_control_code_so_falls_back_to_plain_char() {
        // Ctrl-1 has no xterm control code; falling back to the plain digit is the least
        // surprising behavior (matches what most terminals do too).
        assert_eq!(enc(KeyCode::Char('1'), KeyModifiers::CONTROL), b"1");
    }

    #[test]
    fn alt_char_gets_escape_prefix() {
        assert_eq!(enc(KeyCode::Char('x'), KeyModifiers::ALT), b"\x1bx");
    }

    #[test]
    fn enter_is_carriage_return_not_newline() {
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::NONE), b"\r");
    }

    #[test]
    fn tab_and_shift_tab() {
        assert_eq!(enc(KeyCode::Tab, KeyModifiers::NONE), b"\t");
        assert_eq!(enc(KeyCode::Tab, KeyModifiers::SHIFT), b"\x1b[Z");
        assert_eq!(enc(KeyCode::BackTab, KeyModifiers::NONE), b"\x1b[Z");
    }

    #[test]
    fn backspace_sends_del_matching_xterm_256color_terminfo() {
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::NONE), [0x7f]);
    }

    #[test]
    fn escape_is_a_single_byte() {
        assert_eq!(enc(KeyCode::Esc, KeyModifiers::NONE), [0x1b]);
    }

    #[test]
    fn plain_arrow_uses_normal_cursor_mode_by_default() {
        assert_eq!(enc(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(enc(KeyCode::Left, KeyModifiers::NONE), b"\x1b[D");
    }

    #[test]
    fn plain_arrow_uses_ss3_in_application_cursor_mode() {
        let ctx = KeyContext {
            application_cursor: true,
            application_keypad: false,
        };
        let bytes = encode_key(key(KeyCode::Up, KeyModifiers::NONE), ctx);
        assert_eq!(bytes, b"\x1bOA");
    }

    #[test]
    fn modified_arrow_always_uses_csi_1_form_even_in_application_cursor_mode() {
        let ctx = KeyContext {
            application_cursor: true,
            application_keypad: false,
        };
        let bytes = encode_key(key(KeyCode::Right, KeyModifiers::CONTROL), ctx);
        // 1 (base) + 4 (ctrl) = 5
        assert_eq!(bytes, b"\x1b[1;5C");
    }

    #[test]
    fn shift_alt_arrow_modifier_math() {
        assert_eq!(enc(KeyCode::Down, KeyModifiers::SHIFT), b"\x1b[1;2B");
        assert_eq!(enc(KeyCode::Down, KeyModifiers::ALT), b"\x1b[1;3B");
        assert_eq!(
            enc(KeyCode::Down, KeyModifiers::SHIFT | KeyModifiers::ALT),
            b"\x1b[1;4B"
        );
    }

    #[test]
    fn page_up_down_insert_delete() {
        assert_eq!(enc(KeyCode::PageUp, KeyModifiers::NONE), b"\x1b[5~");
        assert_eq!(enc(KeyCode::PageDown, KeyModifiers::NONE), b"\x1b[6~");
        assert_eq!(enc(KeyCode::Insert, KeyModifiers::NONE), b"\x1b[2~");
        assert_eq!(enc(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
        assert_eq!(enc(KeyCode::Delete, KeyModifiers::CONTROL), b"\x1b[3;5~");
    }

    #[test]
    fn function_keys_encode_correctly() {
        assert_eq!(enc(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(enc(KeyCode::F(4), KeyModifiers::NONE), b"\x1bOS");
        assert_eq!(enc(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
        assert_eq!(enc(KeyCode::F(12), KeyModifiers::NONE), b"\x1b[24~");
        assert_eq!(enc(KeyCode::F(1), KeyModifiers::SHIFT), b"\x1b[1;2P");
    }

    #[test]
    fn ctrl_c_and_ctrl_d_are_forwarded_as_raw_control_bytes() {
        assert_eq!(enc(KeyCode::Char('c'), KeyModifiers::CONTROL), [0x03]);
        assert_eq!(enc(KeyCode::Char('d'), KeyModifiers::CONTROL), [0x04]);
    }

    #[test]
    fn release_events_are_not_forwarded() {
        let mut k = key(KeyCode::Char('a'), KeyModifiers::NONE);
        k.kind = KeyEventKind::Release;
        assert_eq!(encode_key(k, KeyContext::default()), Vec::<u8>::new());
    }

    #[test]
    fn paste_wraps_in_bracketed_markers_only_when_requested() {
        assert_eq!(
            encode_paste("hello", true),
            b"\x1b[200~hello\x1b[201~".to_vec()
        );
        assert_eq!(encode_paste("hello", false), b"hello".to_vec());
    }
}
