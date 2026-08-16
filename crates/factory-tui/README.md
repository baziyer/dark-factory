# factory-tui

The operator board: a `ratatui` terminal app for watching and directing agents on a Dark Factory
daemon. Dwarf-Fortress-flavored on purpose — a dense ASCII floor of agent glyphs, a scrolling
announcements log, single-key-navigable unit/job lists, and one-line status/help — not a
dashboard. See `SPIKE.md` for the terminal-pane rendering research this crate grew out of, and
`/Users/baziyer/.claude/projects/-Users-baziyer-dark-factory/memory/tui-design-direction.md` for
the owner's design note this board implements.

## Running it

```sh
cargo run -p factory-tui
cargo run -p factory-tui -- --socket /path/to/f.sock
cargo run -p factory-tui -- --project my-project
cargo run -p factory-tui -- --dev-local-pty   # see "What's stubbed" below
```

Socket resolution matches `factoryctl` exactly (three steps, first match wins): `--socket`, then
`$DARK_FACTORY_SOCKET`, then `$DARK_FACTORY_HOME/f.sock`, then `$HOME/.dark-factory/f.sock`. If
the daemon isn't reachable, the status line shows `RETRYING` with the connection error and keeps
retrying with backoff (capped at 5s) — it never crashes or blocks the UI. If more than one
project exists and `--project` doesn't pick one unambiguously, a picker appears at startup
(`j`/`k`, `Enter`); with exactly one project, or a `--project` that matches, the board skips
straight to it.

## Layout

```
┌ floor ─────────────────────────────┬ announcements ──────────────────┐
│ *Ω orchestr  bob        carol      │ 20:41 bob      task#42 done      │
│    [helm]     [desk]     [desk]    │ 20:42 carol    run waiting...    │
│                                     │ 20:44 alice    run running       │
├ units ───────────┬ jobs ───────────┴───────────────────────────────────┤
│ > alice  working ⣀⣠⣤⣴ │ # t1  fix bug        → alice                  │
│   bob    idle    ⠀⠀⠀⠀ │ ✓ t2  add tests      → bob                    │
├──────────────────┴───────────────────────────────────────────────────┤
│ LIVE  Tab switch  j/k move  Enter/l/z look  s start … q quit          │
└─────────────────────────────────────────────────────────────────────┘
```

Floor (top-left), announcements (top-right), units + jobs (middle), status/help (bottom). Below
an 80x24 terminal (`ui/mod.rs::MIN_HEIGHT_FOR_FLOOR`/`MIN_WIDTH_FOR_FLOOR`) the floor is the first
thing dropped, per the design brief — announcements gets its width instead. Units and jobs never
disappear.

## Keys

Board (default) mode:

| Key | Action |
|---|---|
| `Tab` | switch focus between units and jobs |
| `j`/`k`, `↓`/`↑` | move the focused panel's cursor |
| `Enter`, `l`, `z` | look at (zoom into) the selected agent's terminal — units: the selected agent; jobs: the selected task's assignee |
| `s` | start the selected task on its assigned agent (`a` first if unassigned) |
| `c` | cancel the selected task |
| `r` | retry the selected task |
| `a` | assign/reassign the selected task — opens an agent picker |
| `x`, `x` | delete the selected task (second `x` confirms; anything else cancels) |
| `n` | new task — a two-field prompt (title, body) |
| `m` | message the selected agent — a one-field prompt (body) |
| `S` | stop the selected agent's current run |
| `q` | quit |

Task actions (`s`/`c`/`r`/`a`/`x`) always target the jobs cursor; agent actions (`m`/`S`) always
target the units cursor, regardless of which panel currently has focus (`Tab`) — the two cursors
are independent, so this lets an operator line up a task and an agent without losing either
selection.

Prompts (`n`, `m`): type to edit the current field, `Tab`/`Enter` moves to the next field,
`Enter` on the last field submits, `Esc` cancels. Pickers (project, assign-agent): `j`/`k` moves,
`Enter` selects, `Esc` cancels.

Zoomed into an agent's terminal: with `--dev-local-pty`, this is the spike's exact `Ctrl-]`
toggle — `Ctrl-]` flips between forwarding keystrokes to the pane and a zoom-control substate
where `Esc`/`z` exits back to the board (see `SPIKE.md` "Input routing" for why `Ctrl-]` and why
the code says `Char('5')`). Without `--dev-local-pty` (the default), there's no live pane to type
into, so `Esc`/`z`/`Enter`/`q` all just exit zoom.

Every request that can fail (`StartTask`, `CancelTask`, …) reports `LocalResponse::Error` in the
status line rather than panicking; a transport-level failure (daemon down, timeout) does too.

## How agent state is derived

Every agent on the floor and in the units list is one of five states — idle, working, waiting,
stopped, failed — computed by a single function, `model::agent_state`, from the agent's most
recent `RunSnapshot` (`Board::latest_run_for`, itself just "the run with the greatest
`started_at_ms`" — there's no cheaper source yet, see below):

- no run ever, or the latest run **succeeded** → idle
- latest run **starting**/**running** → working
- latest run **waiting**/**blocked**/**paused** → waiting (`RunStatus::Paused` isn't named in the
  design brief's mapping; folded into waiting as the closest fit — it's not actively working, but
  it isn't a terminal outcome either)
- latest run **failed** → failed (stays failed, doesn't revert to idle, until retried)
- latest run **stopped** → stopped (same)

This is a placeholder. The real target (see `ROADMAP.md`/the shared brief, "later track:
hooks/sessions") is durable per-session state — idle/working/waiting_for_input/stopped/failed —
reported directly by provider hooks, independent of process/run lifecycle. When that lands, only
`agent_state` (and its data source, `Board::latest_run_for`) should need to change; every caller
already goes through it rather than inspecting `RunStatus` itself, by design.

The orchestrator (`AgentRole::Orchestrator`) always renders first, with a distinct glyph (`Ω`)
and a `[helm]` label instead of `[desk]` — the "god" desk, per the Munder-Difflin office
reference in the design note.

## Sparklines

Each unit row shows a 10-column braille sparkline of that agent's event rate: a count of
`TaskChanged`/`RunChanged`/`AgentChanged`/`AgentDeleted` events touching that agent, bucketed into
one-minute buckets, over the last 30 minutes (`model::ActivitySeries`). This is a stand-in for a
real tokens/turns-per-minute series, which needs the same provider-hook work as session state
above (see `README.md`'s "how agent state is derived"). The buckets keep rolling forward on the
board's 1Hz tick even for an idle agent, so the sparkline visibly slides even with no events.

Braille levels (`model::BRAILLE_LEVELS`) match the exact glyph gradient in the owner's design
note (`⣀⣠⣤⣴⣶⣾⣿`), plus a true-empty glyph (U+2800) for zero-count buckets, quantized with pure
integer math (no float precision-loss lint dodging needed).

## What's stubbed pending later tracks

- **Terminal attach.** "Looking at" an agent doesn't yet attach to a real daemon-proxied PTY
  stream — that protocol doesn't exist yet (see `SPIKE.md`'s "For the next agent" section, and
  the shared brief's "later track: hooks/sessions"). By default, zoom shows a placeholder
  ("the daemon's terminal-attach protocol hasn't landed yet"). Pass `--dev-local-pty` to instead
  spawn a local `bash` shell under a real PTY (reusing `pane.rs`'s `Pane`, `keys.rs`'s encoder,
  and `query.rs`'s responder verbatim from the spike) so the zoom/pane-forwarding mechanics can be
  exercised end-to-end against something real, without running `claude`/`codex` (out of scope for
  this track per the shared brief). When the attach protocol lands, `pane.rs`'s `Pane` is meant to
  grow a second constructor that streams from `Client::subscribe`/`request` by `run_id` instead of
  spawning a child process — everything downstream of "bytes arrived" (the reader thread, the
  `vt100::Parser`, `QueryResponder`) shouldn't need to change.
- **Agent state and sparklines** come from `RunSnapshot` polling/events, not durable session
  state or real token/turn counts — see the two sections above.
- **Messages aren't displayed.** `m` sends an `AgentMessage` (`SendAgentMessage`), but there's no
  panel for reading an agent's inbox (`ListAgentMessages`) — out of scope for this track.
- **Times are UTC.** Announcement timestamps (`HH:MM`) are time-of-day arithmetic on the event's
  epoch-ms, deliberately not run through a calendar/timezone crate (none is in the dependency
  tree — see `SPIKE.md`'s MSRV section for why that tree is kept deliberately narrow).
- **New-project/new-agent creation** isn't in the board (only an existing project can be picked,
  and `n` only creates tasks) — `factoryctl` still owns those.

## Architecture

- `model.rs` — the view-model (`Board`, `Mode`, `Intent`, `agent_state`, `RingBuffer`,
  `ActivitySeries`, `braille_sparkline`) and all key-handling. No sockets, no PTYs, no `Frame` —
  fully unit tested (`cargo test -p factory-tui`) without a terminal or a daemon.
- `net.rs` — every socket touch: `resolve_socket_path`, project-list bootstrap, a project
  session (consistent initial snapshot + `Subscribe` forever with reconnect/backoff), and one-shot
  request threads for operator actions. Reports back to the render loop over an
  `mpsc::Sender<NetMsg>` — nothing here ever touches the terminal.
- `ui/` — pure rendering of `Board` (`floor.rs`, `log.rs`, `lists.rs`, `help.rs` for the
  status line and the prompt/picker/confirm overlays), dispatched from `ui/mod.rs::draw`.
- `pane.rs`, `keys.rs`, `query.rs` — unchanged from the fidelity spike (a local-PTY child, the
  crossterm-key-to-terminal-bytes encoder, and the terminal-query responder). `main.rs` owns the
  one `Pane` that can exist at a time (the zoomed agent's, only under `--dev-local-pty`) since
  rendering it needs both `Board`'s mode and the pane's mutex-guarded `vt100::Screen` together —
  everything else in `ui/` only ever needs `Board`.
- `main.rs` — CLI args, terminal setup/teardown (raw mode, alt screen, bracketed paste, the
  spike's panic hook restoring the terminal), and the event loop: `crossterm` input, a
  non-blocking drain of `NetMsg`, a 1Hz tick, redraw only when something changed. Not a busy
  poll — `event::poll` blocks efficiently (via the OS) for up to 150ms with ~0 CPU at idle.

## Testing against a real (throwaway) daemon

Never point this at `~/.dark-factory` or the live `launchd` daemon. Use a private, temporary
`DARK_FACTORY_HOME` instead:

```sh
export DARK_FACTORY_HOME=$(mktemp -d)
chmod 700 "$DARK_FACTORY_HOME"
target/debug/factoryd &
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
