# factory-tui

`factory-tui` is the detachable terminal board for a running Dark Factory. It
uses the same local API as `factoryctl`. Closing the TUI does not stop the
daemon or active work.

```sh
factory-tui
factory-tui --project my-project
factory-tui --theme plain
```

The board reconnects after daemon restarts and remembers the last focused
project unless `--project` selects one.

BUILDING is the fleet view: projects, agents, queues, recent activity, and the
shared NEEDS YOU list. AGENT shows one agent's current or latest attempt,
queue, messages, and settings. The TUI has no embedded provider terminal or
session controls; provider execution belongs to its admitted attempt.

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

Mouse input selects the same visible tabs, agents, tasks, and actions as the
keyboard.

Current `main` is frozen for live operation. Never point a development build at
the operator's installation; follow the
[development workflow](../../docs/development/WORKFLOW.md) with a temporary
home and socket.
