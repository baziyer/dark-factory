# factory-tui

The detachable operator board for a Dark Factory daemon. It uses the same local requests as
`factoryctl`; closing it never stops agents.

## Running it

```sh
cargo run -p factory-tui
cargo run -p factory-tui -- --socket /path/to/f.sock
cargo run -p factory-tui -- --project my-project
cargo run -p factory-tui -- --theme plain
cargo run -p factory-tui -- --dev-local-pty
```

Socket resolution matches `factoryctl`: `--socket`, `$DARK_FACTORY_SOCKET`,
`$DARK_FACTORY_HOME/f.sock`, then `$HOME/.dark-factory/f.sock`. The board reconnects with
bounded backoff and loads every project's agents, tasks, runs, sessions, and recent retained
events. `--dev-local-pty` is for isolated terminal testing only.

## BUILDING

BUILDING is home. Agents are floors grouped by project. Each row shows the agent glyph and name,
provider, observed or inferred state, recent hook-event sparkline, queue depth, current task, and
a route for delegated agents. NEEDS YOU lists waiting, failed, and blocked work oldest first.

Use `j`/`k` to move, `g` for the next NEEDS YOU item, and Enter to open AGENT. `n` creates
a task, `m` messages the selected agent, `o` messages an orchestrator, `p` focuses a project,
and `x` stops the selected agent after confirmation.

## AGENT

AGENT keeps one live terminal large, with the agent's active queue, private inbox, and settings
alongside. An orchestrator also lists the project queue by assignee (including unassigned work)
and shows each parent-to-child delegation edge.

Use `[`/`]` or `j`/`k` in BOARD mode to switch agents. `i` or Enter gives the terminal
exclusive input; `Ctrl-]` returns to BOARD mode. `z` maximises the terminal and
`PgUp`/`PgDn` scroll history. Esc returns to BUILDING.

Settings use shared daemon requests: Space pauses/resumes, `v` edits the model, and `a` edits
the permission/approval mode. `I` and `M` suspend the TUI and open the agent's
`instructions.md` and `memory.md` in `$EDITOR`. `t` opens the shared task action menu for
the first active queued item.

## State and attention

`Board::agent_state` and `Board::agent_attention` are the single mapping points from durable
state to the UI. Live session hooks win over run inference. Inferred attention is prefixed with
`~`.

## Architecture and safe testing

`model/` owns pure state and key handling, `net.rs` owns socket I/O, `pane.rs` and
`attach.rs` own terminal attachment, and `ui/` renders BUILDING and AGENT.

Never test against `~/.dark-factory` or the live launchd daemon. Follow
[the development workflow](../../docs/development/WORKFLOW.md) with a temporary
`$DARK_FACTORY_HOME` and socket.
