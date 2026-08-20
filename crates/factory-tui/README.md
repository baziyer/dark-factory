# factory-tui

`factory-tui` is the detachable terminal board for a running Dark Factory. It
uses the same local API as `factoryctl`; closing it never stops the daemon or an
attempt.

```sh
factory-tui
factory-tui --project my-project
factory-tui --theme plain
```

The board reconnects across daemon restarts and remembers the last focused
project unless `--project` selects one.

## Views

BUILDING shows every project, agent, queue, recent activity, and the shared
NEEDS YOU decision list. Durable run phase and outcome determine each agent's
state. Current activity is a separate bounded label and event-driven sparkline;
the TUI never infers work authority from a client or provider process.

AGENT shows the selected agent's current or latest attempt, queue, durable
messages, and settings. `Admitted`, `Running`, `Finalizing`, and `Terminal` are
shown directly. Finalizing is explicitly authority-revoked resource cleanup,
not an interactive session.

The TUI deliberately has no provider PTY, terminal input, attach, resize, or
session lifecycle path. Provider execution belongs to the exact admitted run;
operators inspect immutable run output through the CLI/API boundary.

## Keys

| Key | Action |
|---|---|
| `Enter` | Open the selected agent. |
| `Esc` | Return to BUILDING. |
| `j` / `k`, arrows | Select the next or previous agent. |
| `[` / `]` | Select another agent without leaving AGENT. |
| `g` | Go to the next item in NEEDS YOU. |
| `n` | Add a task. |
| `m` / `o` | Message the selected agent or an orchestrator. |
| `p` | Select a project. |
| `Space` | Pause or resume the selected agent. |
| `t` | Manage the selected task. |
| `I` / `M` | Edit agent instructions or memory in `$EDITOR`. |
| `v` / `a` | Edit the agent model or permission mode. |
| `C` | Edit the factory-wide active-run capacity. |
| `u` | Install and relaunch when an update is available. |
| `x` | Cancel the selected run after confirmation. |
| `?` | Show all keys. |
| `q` | Detach from the factory. |

Mouse clicks select the same tabs, agents, tasks, and attention actions as the
keyboard. All mutations use daemon requests; there is no TUI-only control path.

## Development

Never point a development build at the live install. Follow the
[development workflow](../../docs/development/WORKFLOW.md#developing-the-daemon-without-disrupting-a-running-factory).
