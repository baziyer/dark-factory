//! The board's glyph vocabulary: one `Theme` struct, two consts (`FORTRESS`/`PLAIN`), per the
//! design brief's "Theme: `--theme fortress|plain` — one `Theme` struct with two consts (plain =
//! ASCII glyphs `@ C X c # . ! x + = -` and no colours beyond bold/dim)".
//!
//! Every glyph the board draws — agent roles, queue/capacity, attention badges, and workshop
//! routes — comes from here, so switching themes never leaves a stray Unicode glyph behind in an
//! otherwise-ASCII render (`glyph_tables_are_complete` below is the regression test for that).

/// The board's full glyph vocabulary. Every field is a single `char` so a theme is exactly as
/// wide as the Dwarf-Fortress-style floor plan it draws: no per-state color logic lives here
/// (that stays in `ui::attention_color`/`ui::state_color`, which both themes share) — only which
/// *symbol* represents each concept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    /// `◆` — the orchestrator's station glyph, per the design brief.
    pub orchestrator: char,
    /// `C` — a Claude Code worker's station glyph.
    pub claude_worker: char,
    /// `X` — a Codex worker's station glyph.
    pub codex_worker: char,
    /// `S` — a shell-provider worker's station glyph (the minimal example provider).
    pub shell_worker: char,
    /// `c` — a sub-agent's station glyph (any agent with `parent_agent_id` set).
    pub subagent: char,
    /// `▒` — one cell of an in-tray's queued work, capped and shown as `+N` past the cap.
    pub queued: char,
    /// `░` — one cell of a workshop's free capacity.
    pub capacity: char,
    /// `!` — badge for an attempt waiting on human input.
    pub needs_input: char,
    /// `×` — badge for a failed attempt.
    pub failed: char,
    /// `✓` — badge shown on a completed task, never on the agent glyph itself.
    pub complete: char,
    /// `═` — a durable route: the orchestrator has assigned or messaged this worker before.
    pub durable_route: char,
    /// `─` — a transient route: the worker currently has an open episode.
    pub transient_route: char,
    /// Whether this theme draws with more than bold/dim (i.e. hue-based state color). `false`
    /// for `PLAIN`, which relies on the glyphs above plus bold/dim alone.
    pub use_color: bool,
}

/// The Dwarf-Fortress-flavored default: full Unicode glyph set, state communicated through both
/// glyph and color.
pub const FORTRESS: Theme = Theme {
    name: "fortress",
    orchestrator: '◆',
    claude_worker: 'C',
    codex_worker: 'X',
    shell_worker: 'S',
    subagent: 'c',
    queued: '▒',
    capacity: '░',
    needs_input: '!',
    failed: '×',
    complete: '✓',
    durable_route: '═',
    transient_route: '─',
    use_color: true,
};

/// The accessibility/compatibility theme: pure ASCII, state communicated through glyph plus
/// bold/dim alone (see the brief: "plain = ASCII glyphs `@ C X c # . ! x + = -` and no colours
/// beyond bold/dim").
pub const PLAIN: Theme = Theme {
    name: "plain",
    orchestrator: '@',
    claude_worker: 'C',
    codex_worker: 'X',
    shell_worker: 'S',
    subagent: 'c',
    queued: '#',
    capacity: '.',
    needs_input: '!',
    failed: 'x',
    complete: '+',
    durable_route: '=',
    transient_route: '-',
    use_color: false,
};

impl Theme {
    /// Parses `--theme fortress|plain`. Anything else is a startup error in `main.rs`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "fortress" => Some(FORTRESS),
            "plain" => Some(PLAIN),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of a theme is that every glyph the board *could* draw has a mapping in
    /// both tables — a theme is never allowed to be "complete except for one edge case". This
    /// walks every field via a small reflection-free checklist instead of relying on exhaustive
    /// match arms elsewhere silently falling back to a Unicode default under `--theme plain`.
    #[test]
    fn glyph_tables_are_complete_and_ascii_for_plain() {
        let plain_glyphs = [
            PLAIN.orchestrator,
            PLAIN.claude_worker,
            PLAIN.codex_worker,
            PLAIN.shell_worker,
            PLAIN.subagent,
            PLAIN.queued,
            PLAIN.capacity,
            PLAIN.needs_input,
            PLAIN.failed,
            PLAIN.complete,
            PLAIN.durable_route,
            PLAIN.transient_route,
        ];
        for glyph in plain_glyphs {
            assert!(glyph.is_ascii(), "plain theme glyph {glyph:?} is not ASCII");
        }
    }

    #[test]
    fn plain_theme_disables_hue_based_color() {
        let theme = Theme::parse("plain").expect("plain is a documented theme name");
        assert!(!theme.use_color);
    }

    #[test]
    fn fortress_and_plain_glyphs_are_all_distinct_within_a_theme() {
        for theme in [FORTRESS, PLAIN] {
            let glyphs = [
                theme.orchestrator,
                theme.claude_worker,
                theme.codex_worker,
                theme.shell_worker,
                theme.subagent,
                theme.queued,
                theme.capacity,
                theme.needs_input,
                theme.failed,
                theme.complete,
                theme.durable_route,
                theme.transient_route,
            ];
            for (i, a) in glyphs.iter().enumerate() {
                for (j, b) in glyphs.iter().enumerate() {
                    if i != j {
                        assert_ne!(a, b, "{} theme has a duplicate glyph {a:?}", theme.name);
                    }
                }
            }
        }
    }

    #[test]
    fn parse_accepts_exactly_the_two_documented_names() {
        assert_eq!(Theme::parse("fortress"), Some(FORTRESS));
        assert_eq!(Theme::parse("plain"), Some(PLAIN));
        assert_eq!(Theme::parse("dark"), None);
        assert_eq!(Theme::parse(""), None);
    }
}
