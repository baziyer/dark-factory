# factory-tui

The operator board: a `ratatui` terminal app for watching and directing agents on a Dark Factory
daemon. Dwarf-Fortress-flavored on purpose — a spatial floor plan of every project's agents,
attention-ranked announcements, a per-project workshop drill-down, and live terminal panes — not a
dashboard.

## Running it

```sh
cargo run -p factory-tui
cargo run -p factory-tui -- --socket /path/to/f.sock
cargo run -p factory-tui -- --project my-project     # focus this project on startup
                                                      # (default: the one focused last time,
                                                      #  saved in $DARK_FACTORY_HOME/factory-tui.json)
cargo run -p factory-tui -- --theme plain             # ASCII glyphs, no hue-based color
cargo run -p factory-tui -- --dev-local-pty           # TERMINALS/FOCUS attach a local shell
                                                        # instead of a live daemon session
                                                        # (offline pane testing; keep or drop: #33)
```

Socket resolution matches `factoryctl` exactly: `--socket`, then `$DARK_FACTORY_SOCKET`, then
`$DARK_FACTORY_HOME/f.sock`, then `$HOME/.dark-factory/f.sock`. If the daemon isn't reachable, the
status line shows `RETRYING` with the connection error and keeps retrying with backoff (capped at
5s) — it never crashes or blocks the UI.

This board loads **every** project's agents/tasks/runs/sessions at once (FORTRESS is fleet-wide)
and *focuses* one project at a time for WORKSHOP/TERMINALS/FOCUS — `--project` sets that initial
focus; otherwise the oldest project (by creation order) is focused by default, and `Enter` on an
agent in FORTRESS re-focuses whichever project that agent belongs to.

## The four views

FORTRESS (1) → WORKSHOP (2) → TERMINALS (3) → FOCUS (4) — see the top-level
[README.md](../../README.md) for the key table. Briefly:

- **FORTRESS** — spatial factory overview: each project is a persistent "workshop" box (positioned
  deterministically by creation order), agents are glyphs at stations with a route connector to the
  orchestrator, plus a queued-work/capacity bar. Attention-ranked announcements float on the right.
  A custom widget writes every glyph directly into the `ratatui::buffer::Buffer` (`fortress.rs`) —
  no `Canvas`, no `Paragraph` for the map.
- **WORKSHOP** — one focused project: task queue, agent hierarchy (indented tree with state,
  activity sparkline, wait-reason), and a detail pane for whichever item is selected. A task's
  body/result isn't in the fleet snapshot or `TaskChanged` events (those carry only the durable
  snapshot), so the detail pane lazily fetches it with `GetTask` on first selection.
- **TERMINALS** — tiled live PTYs of the focused project's live sessions, 2-4 panes.
- **FOCUS** — one pane, full-screen, with scrollback (`PgUp`/`PgDn`).

`Enter` is contextual per view (zoom into WORKSHOP from FORTRESS; open the task action menu or zoom
into TERMINALS from WORKSHOP; zoom into FOCUS from TERMINALS). WORKSHOP's task action menu —
**assign · cancel · retry · delete · edit title · start** — is the only place those actions live.
Everything is one `Action` enum and one `keymap()` function (`model/keymap.rs`): the meaning of a
key never depends on which view is active, only what `Board::dispatch` does with the resulting
`Action`.

## Theme

`--theme fortress` (default) or `--theme plain`. One `Theme` struct (`theme.rs`), two consts:
`fortress` uses the full Dwarf-Fortress glyph set (`◆ C X c ▒ ░ ! × ✓ ═ ─`); `plain` is pure ASCII
(`@ C X c # . ! x + = -`) with no color beyond bold/dim. Every glyph the board can draw comes from
the active `Theme` — nothing falls back to a hardcoded Unicode character under `--theme plain`
(`theme::tests::glyph_tables_are_complete_and_ascii_for_plain` guards this).

## How agent state and attention are derived

Two functions are the single mapping points from durable daemon state onto what the board draws;
every other piece of code calls them instead of inspecting `SessionState`/`RunStatus` itself:

- `Board::agent_state` → the five-way `AgentState` (idle/working/waiting/stopped/failed) used for
  glyph color everywhere.
- `Board::agent_attention` → the four-way `Attention` taxonomy (routine < completed < failed <
  needs-input, each with a `priority()`) used for fortress badges, announcement ordering, `g`/`G`,
  and WORKSHOP's `!` filter.

**Session state wins over run-status inference whenever a session exists** (hooks supersede
inference — `ARCHITECTURE.md`'s invariant 5). If an agent has no session yet, both functions fall
back to the pre-sessions run-status mapping and mark the result `inferred: true` — surfaced in
WORKSHOP's detail pane as a `~` prefix (e.g. `~latest run: Failed`) so an operator can always tell
observed-from-hooks state apart from guessed-from-run-history state.

## Architecture

- `model/` — the view-model, fully unit tested (`cargo test -p factory-tui`) without a terminal,
  daemon, or PTY: `mod.rs` (`Board`, fleet-wide state, the session-vs-run precedence rule),
  `keymap.rs` (`View`, `Action`, key-handling), `attention.rs`, `state.rs` (activity sparklines,
  announcements ring buffer), `announcements.rs`.
- `fortress.rs` — FORTRESS's custom widget (pure geometry in, `Buffer` writes out).
- `theme.rs`, `net.rs` (every socket touch outside terminal attach), `attach.rs` (the dedicated
  `AttachTerminal` connection), `pane.rs` (`Pane`: a daemon-attached session or, `--dev-local-pty`
  only, a local PTY child — everything downstream of "bytes arrived" is backend-agnostic),
  `keys.rs`/`query.rs` (crossterm-key-to-terminal-bytes encoder and the terminal-query responder).
- `ui/` — pure rendering of `Board` plus, for TERMINALS/FOCUS, the live `Pane`s.
- `main.rs` — CLI args, terminal setup/teardown, the event loop (non-blocking `NetMsg` drain, pane
  reconciliation, a 1Hz tick, redraw only when something changed — not a busy poll).

## Testing against a real (throwaway) daemon

Never point this at `~/.dark-factory` or the live `launchd` daemon — see
[docs/development/WORKFLOW.md](../../docs/development/WORKFLOW.md) for running a throwaway daemon
on a temporary `$DARK_FACTORY_HOME`.
