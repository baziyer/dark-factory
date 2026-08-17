# Dark Factory

A small Rust supervisor for coding-agent processes, with a disposable native
observer. The target is closer to systemd for Claude Code and Codex than a
desktop agent simulation.

The first-launch system contains a SQLite-backed daemon, stable per-run process
sidecars, concrete Claude Code and Codex adapters, a versioned private Unix
socket, a machine-readable v1 CLI, and an event-driven ratatui operator board
(`factory-tui`).
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
cargo run -p factoryctl -- task assign --project PROJECT_ID --task TASK_ID --agent AGENT_ID
cargo run -p factoryctl -- task start --project PROJECT_ID --task TASK_ID --agent AGENT_ID
cargo run -p factoryctl -- task cancel --project PROJECT_ID --task TASK_ID
cargo run -p factoryctl -- task update --project PROJECT_ID --task TASK_ID --title "Renamed"
cargo run -p factoryctl -- task delete --project PROJECT_ID --task TASK_ID
cargo run -p factoryctl -- run stop --project PROJECT_ID --run RUN_ID
cargo run -p factoryctl -- agent pause --project PROJECT_ID --agent AGENT_ID
cargo run -p factoryctl -- agent resume --project PROJECT_ID --agent AGENT_ID
cargo run -p factoryctl -- task done --project PROJECT_ID --task TASK_ID --result "Done"
cargo run -p factoryctl -- task blocked --project PROJECT_ID --task TASK_ID --reason "Needs input"
cargo run -p factoryctl -- session list --project PROJECT_ID
cargo run -p factoryctl -- session stop --project PROJECT_ID --session SESSION_ID
cargo run -p factoryctl -- agent delete --project PROJECT_ID --agent AGENT_ID
cargo run -p factoryctl -- project delete --project PROJECT_ID
cargo run -p factoryctl -- attach --project PROJECT_ID --session SESSION_ID
cargo run -p factoryctl -- attach --project PROJECT_ID --agent AGENT_ID
cargo run -p factoryctl -- usage
cargo run -p factoryctl -- events --follow
cargo run -p factory-tui
```

`task done`/`task blocked` are meant to be called by an agent itself, from
inside its own session (they take no `--agent` flag — identity comes from
the session's own environment, see "Provider hooks" below). Every
`--project` may be omitted if `$DARK_FACTORY_PROJECT` is set, and `agent
message --from` defaults to `$DARK_FACTORY_AGENT` when unset — both are set
automatically inside every session's own environment, so an agent
coordinating via `factoryctl` from its own shell does not need to repeat its
own identity on every call.

Every command, group, and subcommand accepts `--help`/`-h` and prints usage
text to stdout without contacting the daemon (for example `factoryctl task
--help` or `factoryctl task cancel --help`).

For the installed local release, use `./scripts/launch-ui.sh`. It checks the
release binaries and daemon first, then keeps `factory-tui` attached to the
current terminal. Leave that terminal open while using the board; `Ctrl-C`
closes the observer only. `factoryd` is a separate launchd service and
survives board or CLI shutdown.

Commands emit versioned JSON frames. `task assign` returns once the queue
assignment is durable; session spawn, delivery, and completion all arrive as
events (`session list`/`task get` also reflect them directly). Both programs
use `$DARK_FACTORY_SOCKET`, then `$DARK_FACTORY_HOME/f.sock`, then
`$HOME/.dark-factory/f.sock`; `--socket PATH` has highest precedence.
Agent profiles carry a provider-scoped `model` and `permission_mode` (Claude:
`default`/`acceptEdits`/`plan`; Codex: `on-request`/`never`; consumed
directly at session launch — `claude --permission-mode M` / `codex -c
approval_policy="M"`). Standing guidance and memory are file-backed, not
database columns; see "Guidance files" below. `agent message` and `agent
inbox` use one private durable inbox; a message is delivered alongside the
next task delivered into that agent's session (or on its own, opening no run
episode, if no task is pending) and never enters public events. List
commands expose `--after` and `--limit` cursors. Event subscriptions emit
their durable replay boundary and a `caught_up` frame before live events.

By default `factoryd` starts the sibling `factory-runner`, resolves `codex`
and `claude` through the sanitized launch environment, stores session runtime
state under `$DARK_FACTORY_HOME/runs`, and allows four concurrently active
sessions. Use `--runner`, `--factoryctl`, `--codex`, `--claude`,
`--runtime-root`, and `--max-active-runs` to set those explicitly. A resident
session has no daemon-enforced turn or budget cap (that was a print-mode-only
concept; resident sessions deliberately omit `--max-turns`/`--max-budget-usd`
and run until the operator stops them or the provider process exits on its
own). Current model ids: Claude `claude-fable-5`, `claude-opus-5`,
`claude-sonnet-5`, `claude-haiku-4-5-20251001` (the installed `claude` CLI
also accepts the short aliases `fable`/`opus`/`sonnet`/`haiku`); Codex model
ids follow whatever the installed `codex` CLI supports. `agent add --model`
and `agent profile set --model` forward any non-empty bounded string to the
provider for either one (for the `shell` provider, `--model` is instead the
command to run under `sh -lc`); the daemon does not maintain its own model
allowlist.

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
$DARK_FACTORY_HOME/projects/<project_id>/agents/<agent_id>/codex-home/
$DARK_FACTORY_HOME/projects/<project_id>/agents/<agent_id>/claude-settings.json
$DARK_FACTORY_HOME/projects/<project_id>/worktrees/<agent_id>/
```

The git worktree is provisioned immediately, on `agent add` (unless an
explicit `--worktree` is given), and removed again on `agent delete`; the
seeded `CODEX_HOME` and the generated Claude Code hooks settings file are
each created lazily, on that agent's first session spawn.

Task launch composes, in order, whichever of `PROJECT.md`, `instructions.md`,
and `memory.md` are non-empty, then queued operator messages, then the task
body, then an always-present final paragraph pointing the agent at its own
`memory.md` path so it can append durable lessons there directly. Edit these
files with any editor, or through the daemon: `agent get`/`project get` print
their absolute paths, and `agent profile set`/`project guidance set` write
them atomically (bounded, temp file plus rename).

## Terminal-mode runs and `attach`

Every resident session runs under a PTY (`factory-runner`'s terminal mode —
how every `claude`/`codex`/`shell` session launches) and retains its raw PTY
output in `terminal.log` under the session's runtime directory, bounded at
64 MiB and
rotated once to `terminal.log.1` on overflow — the tail is always kept, never
the whole history. Unlike the public event log, `terminal.log` (and the
runner's private `events.ndjson`) is never deleted when a run is acknowledged;
only the runner's control socket is removed. These are private, bounded,
per-run logs for operator inspection — they never enter public events, webhook
snapshots, or tracing.

`factoryctl attach` is the CLI operator escape hatch onto a terminal-mode
run's PTY:

```sh
cargo run -p factoryctl -- attach --project PROJECT_ID --session SESSION_ID
cargo run -p factoryctl -- attach --project PROJECT_ID --session SESSION_ID --since-offset 0
```

It puts the local terminal in raw mode, replays the retained log from
`--since-offset` (default `0`, the full retained window) before switching to
live bytes, forwards stdin as input, and forwards local window-size changes as
resizes. Press `Ctrl-]` to detach; the terminal is restored on detach, EOF, or
an unexpected exit. This is CLI-only, separate from `factory-tui`'s own
embedded agent panes.

## Walkthrough: get an agent working

Resident sessions are wired end to end: assigning a task to an agent spawns
(or reuses) that agent's own long-lived, PTY-backed `claude`/`codex`/`shell`
process automatically — there is no separate "start" step in the common
case.

First run once: `cargo build --workspace` (or `--release`). `factoryd`
launches sessions by resolving `factory-runner` next to its own executable,
not as a compile-time dependency — `cargo run -p factoryd` alone only builds
`factoryd` itself. If `factory-runner` is missing, `task assign` still
returns success (the task is durably queued) but the daemon can't spawn a
session for it; it retries silently and the failure only shows up in
`factoryd`'s own log (`session spawn failed error=runner launch failed: ...`),
not in `task get`/`session list` or the TUI. Build the workspace first and
this never comes up.

```sh
# Terminal 1: run the daemon in the foreground (or use launchd, see below)
cargo run -p factoryd

# Terminal 2: drive it
cargo run -p factoryctl -- project add --id demo --name Demo --root "$PWD"
cargo run -p factoryctl -- agent add --id worker-1 --project demo \
    --role worker --provider claude
cargo run -p factoryctl -- task add --id t1 --project demo \
    --title "Fix the flaky test" --body "crates/foo/tests/bar.rs is racy; find and fix it"
cargo run -p factoryctl -- task assign --project demo --task t1 --agent worker-1

# watch it work
cargo run -p factoryctl -- session list --project demo
cargo run -p factoryctl -- task get --project demo --task t1
cargo run -p factoryctl -- attach --project demo --agent worker-1   # Ctrl-] to detach
```

`agent add` with no `--worktree` provisions a real git worktree at
`$DARK_FACTORY_HOME/projects/<project>/worktrees/<agent>` on branch
`agent/<agent-id>` (requires the project root to be a git repository;
otherwise the session runs directly in the project root). `task assign`
alone is enough — the daemon's dispatcher notices the agent has pending
work and an idle-or-absent session, and spawns or delivers into it without
any further command. A second task assigned while the agent is mid-turn
queues the same way and is delivered as soon as the current turn's `Stop`
hook fires, into the *same* session — never a second process for one agent.

An agent marks its own work done or blocked from inside its session (these
take no `--agent`; identity comes from the session's own environment):

```sh
factoryctl task done --project demo --task t1 --result "Fixed the race; added a regression test."
factoryctl task blocked --project demo --task t1 --reason "Needs a decision on retry semantics."
```

The composed task text an agent receives spells these out as bare
`factoryctl` commands, so `factoryctl` needs to already resolve on the
session's `PATH` for a real `claude`/`codex` agent to run them directly (a
generated hook, by contrast, always invokes it by an absolute, daemon-known
path). Running only from `cargo run`/`target/debug` without ever putting
that directory on `PATH` (as opposed to a normal `cargo install`ed or
packaged `factoryctl`) means a real agent may spend a few tool calls
locating the right binary the first time it tries to close out a task —
harmless, but worth knowing about before you assume something's wrong.

**The orchestrator ("god") is just another agent**, driven the same way —
`--role orchestrator` instead of `worker`, and typically talking to
`factoryctl` itself (agent add/task add/task assign/agent message) from
inside its own session's shell, using the same identity-scoped commands any
operator would:

```sh
cargo run -p factoryctl -- agent add --id god --project demo \
    --role orchestrator --provider claude
cargo run -p factoryctl -- agent message --project demo --to god \
    --body "New priority: ship the auth fix before Friday."
```

**Restart-proof**: a resident session's provider process is a detached
process tree, independent of `factoryd`. Killing and restarting the daemon
(`kill -TERM <factoryd pid>`, then `cargo run -p factoryd` again on the
same `$DARK_FACTORY_HOME`) does not touch it — `session list` shows the
exact same session ID afterward, still idle or still mid-turn, and
`attach` still reaches it and replays its retained terminal output from
before the restart.

## Provider hooks and resident sessions

One resident, interactive `claude`/`codex`/`shell` process per agent (a
PTY-backed *session*), inside which many tasks run as *episodes* — not a
fresh non-interactive process per task. This section documents how a
session for each provider is launched and how it reports back.

`crates/factoryd/src/providers/{claude,codex,shell}.rs` each implement a
small `Provider` trait (`crates/factoryd/src/providers/mod.rs`,
[`docs/providers.md`](docs/providers.md)) whose `spawn_spec` builds the
exact interactive argv and any generated configuration a session needs —
no API keys; both real providers authenticate as subscription CLI apps:

- **Claude**: `claude --settings <agent-dir>/claude-settings.json
  (--session-id <uuid> | --resume <id>) [--model M] [--permission-mode M]`.
  `claude-settings.json` is generated fresh per session (mode `0600`) with
  hooks for `SessionStart`, `UserPromptSubmit`, `PreToolUse`/`PostToolUse`
  (matching every tool), `Notification`, `Stop`, `SubagentStop`, and
  `SessionEnd`, each pointing at `factoryctl hook --token-file <path>
  <Event>`. No `-p`, `--output-format`, or `--safe-mode` — those are
  print-mode-only or disable hooks outright.
- **Codex**: `codex --dangerously-bypass-hook-trust [--model M] [-c
  approval_policy="<mode>"] [resume <thread-id>]`, with `CODEX_HOME` pointed
  at a per-*agent* (not per-session, so `resume` can find its own rollout
  file across a restart) seeded home under
  `$DARK_FACTORY_HOME/projects/<project_id>/agents/<agent_id>/codex-home/`:
  copies the operator's real `~/.codex/config.toml` if present (else a
  minimal one) and symlinks `auth.json`, then idempotently rewrites a hooks
  block between `# --- dark-factory hooks BEGIN/END ---` markers on every
  spawn — the rest of the file (model, provider, trust settings) is left
  alone. `--dangerously-bypass-hook-trust` is unconditional: the hooks are
  100% daemon-authored into an isolated home the operator never hand-edits,
  which already is the vetting Codex's normal hook-trust prompt asks for.
- **Shell** (`crates/factoryd/src/providers/shell.rs`): `sh -lc <model>`
  (the agent's `model` string is the command to run, or a plain login shell
  if unset) with `DARK_FACTORY_FACTORYCTL` set so a script can find
  `factoryctl` without it being on `PATH`. No resume, no generated config —
  the minimal reference `Provider` implementation, and what
  `crates/factoryd/tests/sessions_e2e.rs` drives instead of a real provider
  CLI (via `tests/fixtures/shell-agent.sh`, a POSIX-`sh` fixture that speaks
  the exact hook/`task done`/`task blocked` protocol a real session would).

### Unattended operation

Dark Factory pre-trusts the worktrees it creates for its own agents, so a
brand-new agent's very first session never blocks on either CLI's one-time
"do you trust this directory?" prompt — that worktree came from the daemon
itself, never from an untrusted source:

- **Claude**: before spawning, `ClaudeProvider::pretrust_worktree` sets
  `projects[<canonical worktree path>].hasTrustDialogAccepted = true` in the
  operator's real `~/.claude.json`, preserving every other key and field and
  writing atomically. It is a no-op (logged as a warning, nothing written) if
  `~/.claude.json` does not exist or does not parse as JSON — Dark Factory
  never creates or overwrites a file it cannot first read and understand.
- **Codex**: the per-agent seeded `config.toml` also carries
  `[projects."<canonical worktree path>"] trust_level = "trusted"`,
  rewritten idempotently in a `# --- dark-factory config BEGIN/END ---`
  marker block alongside the hooks block.

Both use the worktree's *canonicalized* path (symlinks resolved), not
whatever string happens to be stored as the agent's worktree — found running
a real Claude session manually with `$DARK_FACTORY_HOME` under `/tmp`,
itself a symlink to `/private/tmp` on macOS: Claude resolves symlinks in its
own `cwd` before checking trust, so a key written from the raw path silently
never matched and the prompt still appeared (confirmed both ways: the
prompt showed with the raw path, and disappeared once the key used the
canonicalized one). Codex's `trust_level` entry is canonicalized the same
way, proactively, for the same reason, though a real Codex session never
got far enough to independently confirm the prompt would otherwise have
appeared (see the sandbox caveat below).

Two more unattended-dogfood fixes, alongside pre-trust:

- **`factoryctl` is always resolvable inside a session.** The composed
  delivery text tells an agent to run a plain `factoryctl task done ...` /
  `factoryctl task blocked ...`, and the generated `claude-settings.json`
  also pre-approves it: `"permissions": {"allow": ["Bash(factoryctl *)"]}`
  (the exact command-prefix rule shape Claude Code's own
  `permissions.allow` uses, `Bash(<prefix> *)`, not `Bash(<prefix>:*)`) —
  otherwise every single `Bash` call, including the one a session makes to
  report its own progress, stops on Claude's native permission prompt.
  For a terminal-mode session, `factoryctl`'s own directory is also
  prepended to `PATH` (`runner_process::apply_runner_environment`), so the
  bare name resolves regardless of the operator's shell configuration.
- **Codex's own sandbox can otherwise block the control socket and the
  guidance files.** The seeded `config.toml` sets `sandbox_mode =
  "workspace-write"` (a root-table key, kept out of any `[table]` so it is
  never silently swallowed into the operator's own trailing
  `[projects."..."]` entries) and, in the same marker block as the trust
  entry, `[sandbox_workspace_write] writable_roots = [<agent's guidance
  directory>, <directory containing $DARK_FACTORY_SOCKET>]`, `network_access
  = false` — a Unix socket connect needs write access to the socket path
  under the seatbelt sandbox. What manual testing against a real Codex
  session on a real operator's `~/.codex/config.toml` (which this provider
  copies forward) could and could not confirm: this `sandbox_mode`/
  `writable_roots`/`network_access` combination does not block Codex's own
  model-list/plugin-directory network calls (those happen outside the
  sandbox, before any tool call is spawned), and the generated config still
  parses under `codex --strict-config doctor` even against a config with
  its own trailing `[projects...]` tables and a pre-existing `sandbox_mode`
  of its own. What it could *not* confirm within this track's budget: a
  real Codex session reaching `SessionStart` and completing `task done` end
  to end on this particular machine, whose personal `~/.codex/config.toml`
  configures several `[mcp_servers.*]` entries plus a built-in "apps"
  directory check — the session repeatedly stalled in `starting` at
  "Starting MCP servers", before Codex's own hooks ever fire, even after
  disabling the user-configurable MCP servers one at a time. Whether that
  stall is caused by `writable_roots` (an MCP server subprocess needing to
  write somewhere outside it) or is unrelated (a slow/unresponsive
  remote-URL server, or the built-in "apps" check, neither of which is
  user-configurable) was not conclusively isolated. If a Codex agent sits
  in `starting`, check its seeded `config.toml`'s `[mcp_servers.*]` entries
  before assuming the sandbox itself is at fault, and see ROADMAP.md's
  unresolved decisions for the open question this leaves.

Codex also reports its own thread id back on its first `SessionStart` hook
(a Claude-shaped `session_id` field); the daemon persists it
(`Store::set_provider_session_id`) so a later session for the same agent
resumes with `codex resume <thread-id>` instead of always starting fresh —
mirrors Claude, whose `--session-id` the daemon assigns itself up front and
so never has to learn back.

Every hook fires `factoryctl hook --token-file PATH <Event>`: it reads the
hook's JSON payload from stdin (bounded to 64 KiB), forwards it plus the
token file's contents to the daemon as one request, and prints the
daemon's reply verbatim to stdout. It **always exits 0**, printing `{}` on
any problem — unreadable token file, malformed stdin, or an unreachable,
slow, or erroring daemon (5 second timeout) — because a broken or slow hook
must never wedge the operator's live Claude Code or Codex session:

```sh
cargo run -p factoryctl -- hook --token-file RUNTIME_DIR/hook.token Stop
```

The per-session hook token is 32 random bytes, lowercase-hex-encoded to a
64-character file (mode `0600`, generated by
`providers::hooks::write_hook_token`) — never on argv or in an environment
variable, matching the existing runner sandboxing philosophy.

See [docs/providers.md](docs/providers.md) for the provider boundary itself
and how to add a new provider, and the walkthrough above for the exact
`factoryctl` sequence that drives all of this end to end.

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
the agent and worktree for each start. `factory-tui` provides the same control
surface as the JSON CLI plus project/task/agent/run inspection, stop control,
assignment-derived queues, observer health, retry for terminal tasks, and
recent durable events.

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
