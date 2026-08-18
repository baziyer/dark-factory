# factory-tui

`factory-tui` is the terminal board for a running Dark Factory. It uses the
same local API as `factoryctl`. Closing it does not stop the daemon or an agent.

## Run it

```sh
factory-tui
factory-tui --project my-project
factory-tui --theme plain
factory-tui --help
```

The board reconnects if the daemon restarts. It remembers the last focused
project unless `--project` selects one.

## Screens

BUILDING shows the full factory. Each project is a building. Each agent has a
floor. The NEEDS YOU list includes agents and tasks. It lists attention items
globally, oldest first.

AGENT shows the selected agent. It includes the live terminal, assigned and
active work, durable messages, and settings. The orchestrator also shows the
project queue.

## Main keys

| Key | Action |
|---|---|
| `Enter` | Open the selected agent. In AGENT, start typing in a live terminal. |
| `Esc` | Return to BUILDING. |
| `j` / `k`, arrows | Select the next or previous agent. |
| `[` / `]` | Select another agent without leaving AGENT. |
| `g` | Go to the next item in NEEDS YOU. |
| `n` | Add a task. |
| `m` / `o` | Message the selected agent or an orchestrator. |
| `p` | Select a project. |
| `Space` | Pause or resume the selected agent. |
| `t` | Manage the active task. |
| `I` / `M` | Edit the agent instructions or memory in `$EDITOR`. |
| `v` / `a` | Edit the agent model or permission mode. |
| `C` | Edit the factory-wide live-session capacity (operator setting). |
| `z` | Maximize or restore the terminal. |
| `PgUp` / `PgDn` | Scroll the terminal. |
| `Ctrl-]` | Stop sending keys to the terminal and control the board. |
| `x` | Stop the selected agent after confirmation. |
| `?` | Show all keys. |
| `q` | Detach from the factory. |

Board actions use daemon requests. `C` uses the same shared launchd capacity
operation as `factoryctl capacity set`; it restarts only `factoryd` and keeps
runner sessions alive. See the [main README](../../README.md) for setup and
first use.

## Development

Use `--dev-local-pty` only for offline terminal testing. Never point a
development build at the live install. Follow the
[development workflow](../../docs/development/WORKFLOW.md#developing-the-daemon-without-disrupting-a-running-factory).
