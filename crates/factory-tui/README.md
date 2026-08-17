# factory-tui

The operator board: a `ratatui` terminal app for watching and directing agents on a Dark Factory
daemon. Dwarf-Fortress-flavored on purpose — a spatial floor plan of every project's agents,
attention-ranked announcements, a per-project workshop drill-down, and live terminal panes — not a
dashboard. See `SPIKE.md` for the terminal-pane rendering research this crate grew out of, and
`/Users/baziyer/.claude/projects/-Users-baziyer-dark-factory/memory/tui-design-direction.md` for
the owner's design note this board implements.

## Running it

```sh
cargo run -p factory-tui
cargo run -p factory-tui -- --socket /path/to/f.sock
cargo run -p factory-tui -- --project my-project     # focus this project on startup
cargo run -p factory-tui -- --theme plain             # ASCII glyphs, no hue-based color
cargo run -p factory-tui -- --dev-local-pty           # see "What's stubbed" below
```

Socket resolution matches `factoryctl` exactly (three steps, first match wins): `--socket`, then
`$DARK_FACTORY_SOCKET`, then `$DARK_FACTORY_HOME/f.sock`, then `$HOME/.dark-factory/f.sock`. If
the daemon isn't reachable, the status line shows `RETRYING` with the connection error and keeps
retrying with backoff (capped at 5s) — it never crashes or blocks the UI.

Unlike a picker-driven "select one project" client, this board loads **every** project's
agents/tasks/runs/sessions at once (FORTRESS is fleet-wide) and simply *focuses* one project at a
time for WORKSHOP/TERMINALS/FOCUS — `--project` sets that initial focus; otherwise the oldest
project (by creation order) is focused by default, and `Enter` on an agent in FORTRESS re-focuses
whichever project that agent belongs to.

## The four views

Per the owner's Ratatui UX brief, one `View` enum with a fixed depth ladder:

```
FORTRESS (1) → WORKSHOP (2) → TERMINALS (3) → FOCUS (4)
```

- **FORTRESS** — the spatial factory overview. Every project is a persistent "workshop" box,
  positioned deterministically by project creation order (left→right, wrapping); inside, agents
  are glyphs at stations (orchestrator, Claude/Codex worker, sub-agent), with a route connector
  from the orchestrator to each worker (`═` durable — it's been assigned work before; `─`
  transient — it currently has an open episode), and an in-tray/capacity bar (`▒`/`░`, queued
  work vs. idle capacity). Announcements float on the right, attention-ranked (failures/
  needs-input bubble above routine chatter) rather than strictly by recency. A custom widget
  writes every glyph directly into the `ratatui::buffer::Buffer` — no `Canvas`, no `Paragraph` for
  the map (`fortress.rs`).
- **WORKSHOP** — one focused project: its task queue (status glyphs, assignee), its agent
  hierarchy as an indented tree (orchestrator → workers → sub-agents, with state, a braille
  activity sparkline, and wait-reason/activity text), and a detail pane for whichever item is
  selected (task body/result, or an agent's session state/last hook/worktree). A task's body/
  result/blocked-reason aren't in the fleet-wide snapshot or `TaskChanged` events (those carry
  only the durable snapshot — see `model::mod::apply_event`'s `TaskChanged` arm), so the detail
  pane lazily fetches them with `GetTask` the first time a task becomes selected (and again
  whenever its snapshot changes after that, e.g. on completion), showing `(loading…)` meanwhile —
  `Board::begin_task_detail_fetch`/`main.rs::sync_task_detail`.
- **TERMINALS** — tiled live PTYs of the focused project's live sessions, 2-4 panes
  (`ui/terminals.rs::pane_rects`).
- **FOCUS** — one pane, full-screen, with scrollback (`PgUp`/`PgDn`).

## Keys

| Key | Action |
|---|---|
| `1`-`4` | switch view directly |
| `Enter`, `→`, `l` | zoom in (context-sensitive — see below) |
| `Esc`, `←`, `h` | zoom out one level |
| `Tab` | cycle agents (FORTRESS/TERMINALS/FOCUS) or panes (WORKSHOP) |
| `j`/`k`, `↓`/`↑` | move |
| `[`/`]` | FORTRESS only: cycle the selected *workshop* itself (independent of the agent cursor) — the only way to reach a project with zero agents, since `Tab`'s agent cursor has no candidates there |
| `n` | new task (title; `Tab`/`Enter` to a second line for the body) — needs a focused project |
| `m` | message the selected agent |
| `o` | message the orchestrator (if several in scope, opens a picker — `Tab`/`j`/`k` to choose) |
| `x` | stop the selected agent — `StopSession` if it has a session, else `StopRun` on its current run; 2-press confirm (`x` again, or `y`/`Enter`) |
| `g` / `G` | jump to (`G`: and open FOCUS on) the next agent needing attention |
| `!` | WORKSHOP: toggle "needs-attention only" filter on both lists |
| `PgUp`/`PgDn` | FOCUS: scroll the pane's terminal scrollback |
| `Ctrl-]` | TERMINALS/FOCUS: toggle between forwarding keys to the pane and board control |
| `q` | detach — quits the client only, never stops the factory |
| `?` | help overlay |

`Enter`'s meaning is contextual, per view: in FORTRESS it zooms into the selected agent's
project's WORKSHOP (or, if a *workshop* is selected via `[`/`]` instead of an agent, that
project's WORKSHOP directly — the only way in for a project with zero agents, where `n` can then
add its first task); in WORKSHOP it opens the task action menu (tasks pane) or zooms into
TERMINALS (agents pane); in TERMINALS it zooms into FOCUS on the selected pane. In WORKSHOP,
`Enter` on a task opens a small action menu — **assign · cancel · retry · delete · edit title ·
start** — the only place those live; `start` is what used to be a top-level `s` key
("deliver now").

FORTRESS's `[`/`]` and `Tab`/`j`/`k` are two independent selection cursors — `Tab`/`j`/`k` moves
the per-agent glyph highlight (and clears the workshop border highlight), `[`/`]` moves the
workshop border highlight (and clears the agent highlight) — so only one kind of selection is
ever shown onscreen at once. An empty workshop shows a truncated `factoryctl agent add …` hint
inside its box if there's room.

Everything above is one `Action` enum and one `keymap()` function
(`model/keymap.rs`) — the meaning of a key never depends on which view is active, only what
`Board::dispatch` does with the resulting `Action`, per the repomon reference this design adopts
(see `REFS-HERDR-REPOMON.md` in the shared brief materials).

Prompts (`n`, `m`, `o`, edit title): type to edit the current field, `Tab`/`Enter` moves to the
next field, `Enter` on the last field submits, `Esc` cancels. Pickers (assign-agent,
message-orchestrator): `j`/`k`/`Tab` moves, `Enter` selects, `Esc` cancels.

Every request that can fail reports `LocalResponse::Error` in the status line rather than
panicking; a transport-level failure (daemon down, timeout) does too.

## Theme

`--theme fortress` (default) or `--theme plain`. One `Theme` struct
(`theme.rs`), two consts: `fortress` uses the full Dwarf-Fortress glyph set (`◆ C X c ▒ ░ ! × ✓ ═
─`); `plain` is pure ASCII (`@ C X c # . ! x + = -`) with no color beyond bold/dim. Every glyph
the board can draw comes from the active `Theme` — nothing falls back to a hardcoded Unicode
character under `--theme plain` (`theme::tests::glyph_tables_are_complete_and_ascii_for_plain`
guards this).

## How agent state and attention are derived

Two functions are the single mapping points from durable daemon state onto what the board draws;
every other piece of code calls them instead of inspecting `SessionState`/`RunStatus` itself:

- `Board::agent_state` → the five-way `AgentState` (idle/working/waiting/stopped/failed) used for
  glyph color everywhere.
- `Board::agent_attention` → the four-way `Attention` taxonomy (routine < completed < failed <
  needs-input, each with a `priority()`) used for fortress badges, announcement ordering, `g`/`G`,
  and WORKSHOP's `!` filter.

**Session state wins over run-status inference whenever a session exists** (hooks supersede
inference, per the design brief). If an agent has no session yet, both functions fall back to the
pre-sessions run-status mapping the original spike board used, and mark the result `inferred:
true` — surfaced in WORKSHOP's detail pane as a `~` prefix (e.g. `~latest run: Failed`) so an
operator can always tell observed-from-hooks state apart from guessed-from-run-history state.

## What's pending on the daemon (5C)

This track (6c) designed and unit-tested every session-driven code path — `Board::agent_state`/
`agent_attention`'s session precedence, `terminal_targets`/`focus_target`, the terminal-attach
frame decode/parser-feed path (`attach.rs`), scrollback offset math (`pane.rs`) — against
*synthetic* `SessionSnapshot`/`ServerFrame::TerminalOutput` fixtures, per the shared brief's
instruction. None of it has been exercised against a real daemon yet, because:

- `ListSessions` currently returns `LocalResponse::Error` ("sessions are not implemented yet") on
  the daemon this track built against — tolerated by `net::load_sessions`, which treats any error
  response as "no sessions" rather than a load failure. In practice this means, against today's
  daemon, `board.sessions` is always empty, every agent's state/attention falls back to the
  run-inference path (`inferred: true` everywhere), and TERMINALS/FOCUS show "no live sessions in
  this project yet" — exactly the graceful degradation the track brief asked for.
- `AttachTerminal`/`TerminalInput`/`ResizeTerminal` are wired end-to-end against the real wire
  contract (`factory_core::local::LocalRequest`, keyed by `session_id`) but nothing on the daemon
  side publishes a `SessionSnapshot` with a live `SessionState` yet, so `Board::terminal_targets`/
  `focus_target` never actually produce a session id to attach to against today's daemon (see
  `--dev-local-pty` below for how to still exercise the pane mechanics).
- When 5A (sessions store) and 5C (execution/delivery) land, only the daemon side should need to
  change: `FactoryEvent::SessionChanged` events already flow into `Board::apply_event` today (see
  `model/tests.rs::session_changed_event_updates_the_sessions_map`), and `ListSessions`'s
  `LocalResponse::Sessions` success path is already implemented in `net::load_sessions`, just
  never exercised because the daemon doesn't take it yet.

`--dev-local-pty` stays available for offline testing of TERMINALS/FOCUS's pane mechanics without
real sessions: every agent gets a deterministic synthetic pane target (`Board::session_id_for_pane`
returns `dev-<agent_id>`, never inserted into `board.sessions`), which `main.rs::sync_panes`
recognizes and spawns a local `bash` shell for instead of a daemon attach. It is **not** the
default path — the default (no flag) always attempts a real `AttachTerminal`.

## Architecture

- `model/` — the view-model, fully unit tested (`cargo test -p factory-tui`) without a terminal,
  daemon, or PTY:
  - `mod.rs` — `Board` (fleet-wide state: every project's agents/tasks/runs/sessions),
    `agent_state`/`agent_attention` (the session-vs-run precedence rule), fortress/workshop/
    terminal-target derived views, fleet-snapshot/event application.
  - `keymap.rs` — `View`, `Action`, the one `keymap()` function, `Mode` (prompts/pickers/task
    menu/confirm/help), and all of `Board`'s key-handling.
  - `attention.rs` — the shared `Attention` taxonomy and its session/run/task mappings.
  - `state.rs` — the five-way `AgentState`, the announcements ring buffer, per-agent activity
    sparklines (braille, `⣀⣠⣤⣴⣶⣾⣿`).
  - `announcements.rs` — event → announcement-line formatting and attention-ranked ordering.
- `fortress.rs` — FORTRESS's custom widget: `compute_workshops` (pure geometry — project/agent
  identity in, deterministic `Rect`s out, no state) and `render` (writes glyphs/colors/badges
  directly into a `ratatui::buffer::Buffer`, reading current state from `Board` only at draw
  time).
- `theme.rs` — the `Theme` struct and its two consts.
- `net.rs` — every socket touch outside terminal attach: `resolve_socket_path`, the fleet
  snapshot bootstrap (all projects, tolerant of `ListSessions` not being implemented yet), the
  `Subscribe` event stream (reconnect/backoff), and one-shot request threads for operator actions.
- `attach.rs` — a dedicated, raw `AttachTerminal` connection (deliberately not built on
  `factoryctl::Client::attach_terminal` — see its module doc for why: this crate needs to hold a
  socket handle it can `shutdown()` on detach, which the `Client` API doesn't expose).
- `pane.rs` — `Pane`, with two backends: a local PTY child (`--dev-local-pty` only) and a
  daemon-attached session (`Pane::attach`, the real path) — everything downstream of "bytes
  arrived" (the `vt100::Parser`, `QueryResponder` for local-PTY, scrollback) is backend-agnostic.
- `keys.rs`, `query.rs` — unchanged from the fidelity spike (the crossterm-key-to-terminal-bytes
  encoder, and the terminal-query responder).
- `ui/` — pure rendering of `Board` (plus, for TERMINALS/FOCUS, the live `Pane`s): `mod.rs`
  dispatches by view and draws the shared status line/overlays; `fortress_view.rs`,
  `workshop.rs`, `terminals.rs`, `focus.rs` render one view each; `announcements.rs` the ranked
  log; `help.rs` the status line, prompts, pickers, task menu, confirm dialog, and `?` help.
- `main.rs` — CLI args, terminal setup/teardown, and the event loop: `crossterm` input, a
  non-blocking drain of `NetMsg`, pane reconciliation (`sync_panes`, diffing `Board::
  desired_sessions()` against the currently attached panes every loop iteration — cheap, and
  deliberately not gated on a redraw so leaving TERMINALS/FOCUS detaches promptly), a 1Hz tick,
  redraw only when something changed. Not a busy poll — `event::poll` blocks efficiently (via the
  OS) for up to 150ms with ~0 CPU at idle.

## Testing against a real (throwaway) daemon

Never point this at `~/.dark-factory` or the live `launchd` daemon. Use a private, temporary
`DARK_FACTORY_HOME` instead:

```sh
export DARK_FACTORY_HOME=$(mktemp -d)
chmod 700 "$DARK_FACTORY_HOME"
target/debug/factoryd &
# If factoryd instead exits with "path must be shorter than SUN_LEN": the socket path
# ($DARK_FACTORY_HOME/f.sock) is over the platform's ~104-byte Unix-socket-path limit, which
# `mktemp -d` alone can't guarantee from a deeply nested working directory (long on some CI
# runners and synced folders even though plain `mktemp -d` is short on a typical macOS shell).
# Pick a shallower base explicitly instead, e.g. `mktemp -d /tmp/df-ui.XXXXXX`.
target/debug/factoryctl project add --name demo --root "$PWD"
target/debug/factoryctl agent add --project demo --role orchestrator --provider codex
target/debug/factoryctl agent add --project demo --role worker --provider claude-code
target/debug/factoryctl task add --project demo --title "fix bug" --body "…"
target/debug/factory-tui --project demo
# when done:
kill %1
```

`tmux` is a convenient scriptable host terminal for capturing screenshots headlessly
(`tmux capture-pane -p`), the same technique `SPIKE.md`'s "Verification log" used.
