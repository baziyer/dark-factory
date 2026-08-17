//! Answers the handful of terminal queries that `claude` and `codex` were observed sending on
//! startup (see `SPIKE.md` "Terminal-query handling" for how this was determined: raw PTY bytes
//! captured for a few seconds after spawn and inspected by hand). `vt100::Parser` only *models*
//! terminal state; it never writes replies back to the child, so without this module both apps
//! are talking to a terminal that never answers - which they tolerate (see SPIKE.md), but with
//! degraded fidelity (e.g. no confirmed cursor position, no confirmed truecolor background).
//!
//! This is a small hand-rolled scanner, not a full parser: it looks for a fixed set of short,
//! literal query sequences directly in the raw byte stream, correctly handling the case where a
//! query is split across two PTY reads. It does not attempt to understand arbitrary escape
//! sequences (that's `vt100::Parser`'s job) and only buffers bytes that could still become a
//! recognized query, so it stays cheap even when a child writes a large opaque blob (e.g.
//! `codex`'s Kitty graphics-protocol splash image, which contains no ESC bytes at all).

/// A literal byte sequence we recognize, and the reply to send when we see it.
struct Query {
    pattern: &'static [u8],
    kind: QueryKind,
}

#[derive(Clone, Copy)]
enum QueryKind {
    /// Primary Device Attributes (`CSI c` / `CSI 0 c`). Answered with a conservative
    /// VT100-with-AVO identification; both target apps appear to use the reply only as a
    /// completion sentinel, not for detailed capability parsing (see SPIKE.md).
    Da1,
    /// XTVERSION (`CSI > 0 q`), answered with a DCS identifying this program.
    XtVersion,
    /// Cursor Position Report (`CSI 6 n`). Answered with the real cursor position from the
    /// `vt100::Screen` this pane is backed by.
    CursorPosition,
    /// Kitty keyboard protocol flags query (`CSI ? u`). We don't proxy the protocol to the
    /// child in this spike, so we answer "no flags supported" rather than leaving the app to
    /// guess from a timeout.
    KittyKeyboard,
    /// OSC 10 (`ESC ] 10 ; ? ...`), a request for the current foreground color.
    OscForeground,
    /// OSC 11 (`ESC ] 11 ; ? ...`), a request for the current background color.
    OscBackground,
}

const QUERIES: &[Query] = &[
    Query {
        pattern: b"\x1b[c",
        kind: QueryKind::Da1,
    },
    Query {
        pattern: b"\x1b[0c",
        kind: QueryKind::Da1,
    },
    Query {
        pattern: b"\x1b[>0q",
        kind: QueryKind::XtVersion,
    },
    Query {
        pattern: b"\x1b[6n",
        kind: QueryKind::CursorPosition,
    },
    Query {
        pattern: b"\x1b[?u",
        kind: QueryKind::KittyKeyboard,
    },
    Query {
        pattern: b"\x1b]10;?",
        kind: QueryKind::OscForeground,
    },
    Query {
        pattern: b"\x1b]11;?",
        kind: QueryKind::OscBackground,
    },
];

/// Longest query pattern in bytes; bounds how much we ever need to retain across reads.
const MAX_PATTERN_LEN: usize = 6;

/// Incremental scanner. One instance per pane, fed every chunk of PTY output as it's read.
pub struct QueryResponder {
    /// Bytes that might still be the start of a recognized (but not yet complete) query.
    pending: Vec<u8>,
}

impl QueryResponder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Scans `new_bytes` (the latest PTY read) for terminal queries, returning the bytes that
    /// should be written back to the PTY master in response. `cursor_row`/`cursor_col` are the
    /// child's *current* 0-indexed cursor position (from `vt100::Screen::cursor_position`),
    /// used to answer CPR; pass the position as of right after processing this same chunk.
    pub fn scan(&mut self, new_bytes: &[u8], cursor_row: u16, cursor_col: u16) -> Vec<u8> {
        self.pending.extend_from_slice(new_bytes);

        let mut reply = Vec::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            let rest = &self.pending[consumed..];
            if let Some(query) = QUERIES.iter().find(|q| rest.starts_with(q.pattern)) {
                reply.extend(self.reply_for(query.kind, cursor_row, cursor_col));
                consumed += query.pattern.len();
                continue;
            }
            if could_be_partial_query(rest) {
                // Wait for more bytes before deciding.
                break;
            }
            consumed += 1;
        }
        self.pending.drain(0..consumed);

        // Defensive cap: should never trigger (non-matching bytes are dropped above), but
        // guarantees this scanner can never accumulate unbounded memory.
        if self.pending.len() > MAX_PATTERN_LEN {
            let excess = self.pending.len() - MAX_PATTERN_LEN;
            self.pending.drain(0..excess);
        }

        reply
    }

    fn reply_for(&self, kind: QueryKind, cursor_row: u16, cursor_col: u16) -> Vec<u8> {
        match kind {
            QueryKind::Da1 => b"\x1b[?1;2c".to_vec(),
            QueryKind::XtVersion => b"\x1bP>|factory-tui 0.1.0\x1b\\".to_vec(),
            QueryKind::CursorPosition => {
                format!("\x1b[{};{}R", cursor_row + 1, cursor_col + 1).into_bytes()
            }
            QueryKind::KittyKeyboard => b"\x1b[?0u".to_vec(),
            // We don't track the outer terminal's real palette in this spike; answer with a
            // fixed dark-background/light-foreground pair so background-color-aware apps at
            // least pick a sane (dark) theme instead of guessing. See SPIKE.md.
            QueryKind::OscForeground => b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\".to_vec(),
            QueryKind::OscBackground => b"\x1b]11;rgb:0000/0000/0000\x1b\\".to_vec(),
        }
    }
}

impl Default for QueryResponder {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `buf` (which did not match any complete query) could still be the start of one,
/// i.e. is a proper prefix of some pattern.
fn could_be_partial_query(buf: &[u8]) -> bool {
    QUERIES
        .iter()
        .any(|q| buf.len() < q.pattern.len() && q.pattern.starts_with(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_da1_short_and_long_forms() {
        let mut r = QueryResponder::new();
        assert_eq!(r.scan(b"\x1b[c", 0, 0), b"\x1b[?1;2c");
        let mut r = QueryResponder::new();
        assert_eq!(r.scan(b"\x1b[0c", 0, 0), b"\x1b[?1;2c");
    }

    #[test]
    fn answers_cpr_with_one_indexed_position() {
        let mut r = QueryResponder::new();
        assert_eq!(r.scan(b"\x1b[6n", 4, 9), b"\x1b[5;10R");
    }

    #[test]
    fn answers_kitty_and_xtversion_and_osc_colors() {
        let mut r = QueryResponder::new();
        assert_eq!(r.scan(b"\x1b[?u", 0, 0), b"\x1b[?0u");
        let mut r = QueryResponder::new();
        assert_eq!(r.scan(b"\x1b[>0q", 0, 0), b"\x1bP>|factory-tui 0.1.0\x1b\\");
        let mut r = QueryResponder::new();
        assert_eq!(
            r.scan(b"\x1b]10;?\x1b\\", 0, 0),
            b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"
        );
        let mut r = QueryResponder::new();
        assert_eq!(
            r.scan(b"\x1b]11;?\x1b\\", 0, 0),
            b"\x1b]11;rgb:0000/0000/0000\x1b\\"
        );
    }

    #[test]
    fn query_split_across_two_reads_is_still_answered() {
        let mut r = QueryResponder::new();
        assert_eq!(r.scan(b"\x1b[", 0, 0), Vec::<u8>::new());
        assert_eq!(r.scan(b"6n", 4, 9), b"\x1b[5;10R");
    }

    #[test]
    fn split_osc_query_across_reads() {
        let mut r = QueryResponder::new();
        assert_eq!(r.scan(b"\x1b]1", 0, 0), Vec::<u8>::new());
        assert_eq!(
            r.scan(b"1;?\x1b\\", 0, 0),
            b"\x1b]11;rgb:0000/0000/0000\x1b\\"
        );
    }

    #[test]
    fn unrelated_bytes_do_not_trigger_a_reply_and_are_not_retained_forever() {
        let mut r = QueryResponder::new();
        assert_eq!(r.scan(b"hello world, not a query", 0, 0), Vec::<u8>::new());
        assert!(r.pending.is_empty());
    }

    #[test]
    fn large_non_escape_blob_is_not_buffered() {
        // Simulates the base64 payload of codex's Kitty graphics-protocol splash image: no ESC
        // bytes, so nothing should ever be retained regardless of size.
        let mut r = QueryResponder::new();
        let blob = vec![b'A'; 200_000];
        assert_eq!(r.scan(&blob, 0, 0), Vec::<u8>::new());
        assert!(r.pending.is_empty());
    }

    #[test]
    fn each_query_fires_exactly_once() {
        let mut r = QueryResponder::new();
        let first = r.scan(b"\x1b[c", 0, 0);
        let second = r.scan(b"", 0, 0);
        assert_eq!(first, b"\x1b[?1;2c");
        assert_eq!(second, Vec::<u8>::new());
    }
}
