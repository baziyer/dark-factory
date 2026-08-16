# Dark Factory

A small Rust supervisor for coding-agent processes, with a disposable native
observer. The target is closer to systemd for Claude Code and Codex than a
desktop agent simulation.

The first-launch system contains a SQLite-backed daemon, stable per-run process
sidecars, concrete Claude Code and Codex adapters, a versioned private Unix
socket, a machine-readable v1 CLI, and an event-driven native egui UI.
Task reservation, provider-session ownership, runner replay, terminal outcome,
and observer health are durable. Restarting the daemon, CLI, or UI does not stop
an agent process.

Provider input is bounded and sent only over stdin. Runners receive a small
default-deny environment; provider output is decoded into bounded structural
state and is never copied wholesale into events, logs, or webhook responses.
On success, only the provider's bounded final answer is stored as the task
result for capability-scoped webhook polling and snapshots.
A loopback HTTP listener serves the single configured webhook endpoint
(Minerva today) and is on by default whenever a config file is present.
Subscription headroom is on-demand only: `factoryctl usage` runs a local Codex
JSON-RPC probe and prints the result; nothing is persisted or collected in the
background.

## Non-goals

- embedding agent runtimes in the UI;
- simulating an office or rendering continuous animation;
- parsing terminal escape sequences when a provider exposes structured output;
- automatic scheduling before explicit local operation earns it;
- provider-specific terminals, office animation, or a browser runtime inside
  the UI; the native UI exposes only bounded, local runner output for direct
  inspection and an explicit stop request for the selected run;
- a plugin framework or public network listener in the first launch.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the invariants that constrain each
slice.

## Development

```sh
./scripts/local-ci.sh
```

The local gate is authoritative for this repository. GitHub
Actions is manual-only so ordinary pushes and pull requests do not consume
hosted-runner budget.

## Local control plane

Start the daemon and use `factoryctl` from another shell:

```sh
cargo run -p factoryd
cargo run -p factoryctl -- health
cargo run -p factoryctl -- project add --name dark-factory --root "$PWD"
cargo run -p factoryctl -- project list
cargo run -p factoryctl -- agent add --project PROJECT_ID --role orchestrator --provider codex
cargo run -p factoryctl -- agent get --project PROJECT_ID --agent AGENT_ID
cargo run -p factoryctl -- agent profile set --project PROJECT_ID --agent AGENT_ID --model gpt-5-codex --instructions-file instructions.md
cargo run -p factoryctl -- agent message --project PROJECT_ID --to AGENT_ID --body "Review the next task"
cargo run -p factoryctl -- agent inbox --project PROJECT_ID --agent AGENT_ID
cargo run -p factoryctl -- project get --project PROJECT_ID
cargo run -p factoryctl -- project guidance set --project PROJECT_ID --file PROJECT.md
cargo run -p factoryctl -- task add --project PROJECT_ID --title "First task" --body "Do the work"
cargo run -p factoryctl -- task start --project PROJECT_ID --task TASK_ID --agent AGENT_ID --worktree "$PWD"
cargo run -p factoryctl -- task cancel --project PROJECT_ID --task TASK_ID
cargo run -p factoryctl -- task update --project PROJECT_ID --task TASK_ID --title "Renamed"
cargo run -p factoryctl -- task delete --project PROJECT_ID --task TASK_ID
cargo run -p factoryctl -- run stop --project PROJECT_ID --run RUN_ID
cargo run -p factoryctl -- agent delete --project PROJECT_ID --agent AGENT_ID
cargo run -p factoryctl -- project delete --project PROJECT_ID
cargo run -p factoryctl -- attach --project PROJECT_ID --run RUN_ID
cargo run -p factoryctl -- usage
cargo run -p factoryctl -- events --follow
cargo run -p factoryctl -- ui
```

Every command, group, and subcommand accepts `--help`/`-h` and prints usage
text to stdout without contacting the daemon (for example `factoryctl task
--help` or `factoryctl task cancel --help`).

For the installed local release, use `./scripts/launch-ui.sh`. It checks the
release binary and daemon first, then keeps the native UI attached to the
current terminal. Leave that terminal open while using the UI; `Ctrl-C` closes
the observer only. `factoryd` is a separate launchd service and survives UI or
CLI shutdown.

Commands emit versioned JSON frames. `task start` returns `run_accepted` once
the task reservation is durable; runner readiness and completion arrive as
events. Both programs use
`$DARK_FACTORY_SOCKET`, then `$DARK_FACTORY_HOME/f.sock`, then
`$HOME/.dark-factory/f.sock`; `--socket PATH` has highest precedence.
Agent profiles carry a provider-scoped `model` and `permission_mode` (Claude:
`default`/`acceptEdits`/`plan`; Codex: `on-request`/`never`; `permission_mode`
is stored and shown but not yet consumed by launch). Standing guidance and
memory are file-backed, not database columns; see "Guidance files" below.
`agent message` and the native inspector use one private durable
inbox; messages are delivered with the next explicit task start and never enter
public events. List commands expose `--after` and `--limit` cursors. Event subscriptions emit
their durable replay boundary and a `caught_up` frame before live events.

By default `factoryd` starts the sibling `factory-runner`, resolves `codex` and
`claude` through the sanitized launch environment, stores runner state under
`$DARK_FACTORY_HOME/runs`, and allows four active runs. Use `--runner`,
`--codex`, `--claude`, `--runtime-root`, and `--max-active-runs` to set those
explicitly. Claude runs are bounded to 20 turns and USD 5.00 in v1. Current
model ids: Claude `claude-fable-5`, `claude-opus-5`, `claude-sonnet-5`,
`claude-haiku-4-5-20251001` (the installed `claude` CLI also accepts the
short aliases `fable`/`opus`/`sonnet`/`haiku`); Codex model ids follow
whatever the installed `codex` CLI supports. `agent add --model` and `agent
profile set --model` forward any non-empty bounded string to the provider
for either one; the daemon does not maintain its own model allowlist.

State is private by construction. Custom database and socket paths must each
have an immediate parent directory owned by the current user with mode `0700`;
files and sockets use mode `0600`. The default state directory is created with
those permissions automatically.

## Guidance files

Project and agent guidance, memory, and standing instructions live as plain
Markdown files in a sister folder under `$DARK_FACTORY_HOME`, Munder-Difflin
style, rather than as opaque database columns. SQLite remains the durable
ledger for projects, agents, tasks, runs, events, and messages; these files
are the operator- and agent-editable surface on top of it. `factoryd` creates
the project directory and an empty `PROJECT.md` on `project add`, and the
agent directory with empty `instructions.md`/`memory.md` on `agent add`
(lazily again on first read, so rows created before this feature still work).
Every file is capped at 16 KB.

```text
$DARK_FACTORY_HOME/projects/<project_id>/PROJECT.md
$DARK_FACTORY_HOME/projects/<project_id>/agents/<agent_id>/instructions.md
$DARK_FACTORY_HOME/projects/<project_id>/agents/<agent_id>/memory.md
$DARK_FACTORY_HOME/projects/<project_id>/agents/<agent_id>/codex-home/       (reserved, not yet created)
$DARK_FACTORY_HOME/projects/<project_id>/agents/<agent_id>/claude-settings.json  (reserved, not yet created)
$DARK_FACTORY_HOME/projects/<project_id>/worktrees/<agent_id>/               (reserved, not yet created)
```

The last three paths are reserved for later tracks (a per-agent `CODEX_HOME`,
generated Claude Code hooks settings, and a per-agent git worktree); today
`factory_core::paths` only computes them.

Task launch composes, in order, whichever of `PROJECT.md`, `instructions.md`,
and `memory.md` are non-empty, then queued operator messages, then the task
body, then an always-present final paragraph pointing the agent at its own
`memory.md` path so it can append durable lessons there directly. Edit these
files with any editor, or through the daemon: `agent get`/`project get` print
their absolute paths, and `agent profile set`/`project guidance set` write
them atomically (bounded, temp file plus rename).

## Terminal-mode runs and `attach`

A run launched under a PTY (`factory-runner`'s terminal mode; not yet how the
`claude`/`codex` adapters launch by default) retains its raw PTY output in
`terminal.log` under the run's runtime directory, bounded at 64 MiB and
rotated once to `terminal.log.1` on overflow — the tail is always kept, never
the whole history. Unlike the public event log, `terminal.log` (and the
runner's private `events.ndjson`) is never deleted when a run is acknowledged;
only the runner's control socket is removed. These are private, bounded,
per-run logs for operator inspection — they never enter public events, webhook
snapshots, or tracing.

`factoryctl attach` is the CLI operator escape hatch onto a terminal-mode
run's PTY:

```sh
cargo run -p factoryctl -- attach --project PROJECT_ID --run RUN_ID
cargo run -p factoryctl -- attach --project PROJECT_ID --run RUN_ID --since-offset 0
```

It puts the local terminal in raw mode, replays the retained log from
`--since-offset` (default `0`, the full retained window) before switching to
live bytes, forwards stdin as input, and forwards local window-size changes as
resizes. Press `Ctrl-]` to detach; the terminal is restored on detach, EOF, or
an unexpected exit. This is CLI-only — the egui UI does not expose it.

## The Minerva webhook endpoint

Webhooks are loopback-only and serve exactly one endpoint: Minerva, on the
`legacy_v1` wire shape. The listener is **on by default** — if
`$DARK_FACTORY_HOME/webhooks.json` exists, `factoryd` loads it and starts the
listener automatically. `--webhook-config PATH` overrides the default location.
Either way, the file must be owner-only (`0600`) JSON, and its endpoint secret
must also be an owner-only (`0600`) regular file.

```json
{
  "version": 1,
  "bind": "127.0.0.1:3849",
  "endpoints": [
    {
      "id": "minerva",
      "wireProfile": "legacy_v1",
      "secretFile": "/absolute/private/minerva.secret",
      "projectId": "factory",
      "orchestratorAgentId": "god"
    }
  ]
}
```

The endpoint ID is the route prefix. `legacy_v1` is the only accepted
`wireProfile` value; the field is still required (an unrecognized value fails
config loading) so this file keeps loading unchanged. A config listing more
than one endpoint is rejected outright. Tunnel or device exposure remains
external to the daemon.

## First-launch boundary and roadmap

V1 is intentionally explicit: create projects, agents, and tasks, then choose
the agent and worktree for each start. The native UI provides the same control
surface as the JSON CLI plus project/task/agent/run inspection, bounded local
runner output and stop control, assignment-derived queues, observer health,
retry for terminal tasks, and recent durable events.

The unfinished product roadmap is kept in
[ROADMAP.md](ROADMAP.md). It covers the God command center and agent operations,
provider-thread/intervention controls, scheduling, richer
blocked-question/document workflows, function axes, and further external wire
profiles.

## Local service

The template in `launchd/` runs the daemon locally; it does not use GitHub
Actions. Render placeholders to absolute canonical paths, keep state/config/log
directories at `0700`, and install the rendered plist at `0600`. Subscription
headroom has no background service: run `factoryctl usage` on demand instead.
