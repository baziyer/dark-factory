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
globally, most urgent and then oldest first. Each row names the actionable
reason instead of collapsing every condition into a generic input request.

BUILDING separates durable lifecycle from current activity. Each row names the
most recent bounded hook/tool activity and its age; it says `no recent activity`
when there is no sample and `stale activity … ago` after a minute without one.
The sparkline is event-driven: each bar is a five-second bucket, with eight
visible bars covering the most recent 40 seconds. Durable events add counts;
idle time only advances empty buckets, so a silent agent ages out without
decorative animation. Assigned work is shown as `queue N`, never an opaque
counter.

The footer identifies the selected screen and shows a red `STALE TUI … detach
+ relaunch` banner when the connected daemon's active runtime is a different
version. Detach and relaunch the client; the TUI never restarts the daemon or
its agents. A daemon that cannot report its version is also called out rather
than presented as current.

AGENT shows the selected agent. It includes the live terminal, assigned and
active work, durable messages, and settings. Settings distinguish the
configured model/reasoning tier and durable selection reason from the
historical runtime-resolved session values. The orchestrator also shows the
project backlog and worker queues; messages remain the inbox and review
attention remains separate. `g` or a NEEDS YOU click opens the
same reason card here, with task/session/age and a safe action, while terminal
typing remains off until explicitly entered. If the reason resolves
concurrently, the stale row disappears and an already-open card says it
changed before action.

## Mouse

Click the BUILDING or AGENT tab in the footer to change screens; the footer also
has stable help and detach targets while the full action catalog remains in `?`
help. In BUILDING, click an agent or a visible NEEDS YOU row to select it. In
AGENT, click a visible queue row to select that task or click the terminal pane
to focus it; press `Enter` or `i` to start typing, so a navigation click never
becomes terminal input.

The wheel scrolls terminal history while the child has mouse handling off. If
the child enables an xterm mouse protocol, terminal mouse events are forwarded
only while the pane is in TYPING mode and only from inside its content area.
When local history is visible, the first coordinate event resets to the live
tail without sending bytes or arming a mouse capture; a later event after the
redraw may be forwarded.

Tabs, list rows, pane borders, overlays, and unknown screen coordinates are
always board territory and are never forwarded to the child. All keyboard
controls below remain available.

## Main keys

| Key | Action |
|---|---|
| `Enter` | Open the selected agent. In AGENT, start typing in a live terminal. |
| `Esc` | Return to BUILDING. |
| `j` / `k`, arrows | Select the next or previous agent. |
| `[` / `]` | Select another agent without leaving AGENT. |
| `g` | Go to the next item in NEEDS YOU. |
| `n` | Add a task (in AGENT, directly into the selected worker's queue). |
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
| `u` | Install and relaunch when the footer shows an available update. |
| `x` | Stop the selected agent after confirmation. |
| `?` | Show all keys. |
| `q` | Detach from the factory. |

Board actions use daemon requests. `C` uses the same shared launchd capacity
operation as `factoryctl capacity set`; it restarts only `factoryd` and keeps
runner sessions alive. See the [main README](../../README.md) for setup and
first use. The `u` action is manual and uses the same verified install and
rollback transaction as `factoryctl update --install`; after daemon health is
confirmed it execs the exact active `factory-tui`, preserving this viewer PID
and its current project/screen intent without restarting provider sessions.

Mouse clicks select the same agents, tasks, attention rows, and queue rows as
keyboard navigation. Press `Enter` or `i` to focus typing in an attached
terminal. Board clicks never become terminal input.

All actions use daemon requests. There is no TUI-only control path. See the
[main README](../../README.md) for setup and first use.

## Development

Use `--dev-local-pty` only for offline terminal testing. Never point a
development build at the live install. Follow the
[development workflow](../../docs/development/WORKFLOW.md#developing-the-daemon-without-disrupting-a-running-factory).
