# Dark Factory

**Turn your backlog into progress.**

Dark Factory is a pure-Rust, terminal-first runtime for turning a software
backlog into continuous agent progress. It is a persistent local runtime
for Claude Code and Codex CLI sessions (other agent CLIs later); a durable
queue, orchestrator, and process supervisor; a specialised terminal
multiplexer for many coding agents at once; and a detachable Ratatui
control surface (`factory-tui`) for watching and directing them. One
operator runs many agents from one machine. MIT-licensed
([LICENSE](LICENSE)), built for external contributions.

It is **not** an Electron/Tauri/browser app, not a clone of an office
simulation, not a new coding model, not an agent pretending to be an
employee, and not a general agent framework. No parsing of terminal escape
sequences when a provider exposes structured output; no automatic
scheduling before explicit local operation earns it; no plugin framework
or public network listener.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the invariants that constrain
every change, and [AGENTS.md](AGENTS.md) if you're an agent (or a human)
about to make one.

## How it works, in one screen

`factoryd` is the sole owner of every agent process. Assigning a task to an
agent spawns (or reuses) that agent's own long-lived, PTY-backed
`claude`/`codex`/`shell` process through `factory-runner` — one **session**
per agent, spanning many task **episodes**, not a fresh process per task.
Closing or rebuilding `factory-tui` or `factoryctl` never touches that
process; killing and restarting `factoryd` itself doesn't either, because
`factory-runner` is a detached process tree the daemon reconnects to.

Session state is driven entirely by the provider's own **hooks** — never
by guessing from terminal output. Claude Code and Codex each call
`factoryctl hook --token-file PATH <Event>` at defined lifecycle points
(session start, before/after a tool call, needing input, stopping); the
daemon normalizes these into one shared state machine. An agent marks its
own work **done** or **blocked** the same way, from inside its session:
`factoryctl task done`/`task blocked` — no polling, no inferred completion.

Every agent gets its own git worktree (`agent/<id>`, created on `agent
add`, removed on `agent delete`), so concurrent agents never collide in
one working tree. Standing instructions, memory, and project guidance live
as plain Markdown files under `$DARK_FACTORY_HOME` (Munder-Difflin style)
rather than opaque database columns — composed into what an agent reads at
task delivery, editable by the operator or the agent itself. SQLite is the
durable ledger for everything else: projects, agents, tasks, runs, events,
messages.

## Install

Binary-only, macOS arm64 (fetch with `curl -L`, not a browser download —
the binaries are unsigned, and a browser adds a quarantine flag Gatekeeper
would then act on when launchd starts `factoryd`; if you did use one, `xattr
-dr com.apple.quarantine <dir>` clears it):

```sh
# latest.json names the newest release's asset for this platform
curl -fsSL https://github.com/baziyer/dark-factory/releases/latest/download/latest.json
curl -L -o dark-factory.tar.gz "<the assets.aarch64-apple-darwin.url it printed>"
mkdir dark-factory && tar -xzf dark-factory.tar.gz -C dark-factory
./dark-factory/factoryctl init
```

(Those URLs answer once the repository is public; until then only a
source build works.)

From source: `cargo build --release --workspace &&
target/release/factoryctl init`. Either way `init` creates
`~/.dark-factory` (`0700`), copies the four binaries next to that
`factoryctl` into `~/.dark-factory/bin/<version>/` and points `bin/current`
at them, reports whether `claude`/`codex`/`git` resolve, states what it
writes outside its home (the launchd job; per-worktree pre-trust entries in
`~/.claude.json`; an `agent/<id>` branch per agent in each project's own
repository) and asks before touching launchd, then loads the job and waits
for the daemon. Put `~/.dark-factory/bin/current` on your `PATH`;
`factoryctl doctor` checks everything read-only, one line per check.

## Quickstart

```sh
cargo build --workspace          # factoryd resolves factory-runner/factoryctl
                                  # as siblings of its own executable, not
                                  # compile-time deps -- build the workspace once

cargo run -p factoryd &           # Terminal 1 (or `factoryctl init`, above)

cargo run -p factoryctl -- project add --id demo --name Demo --root "$PWD"
cargo run -p factoryctl -- agent add --id worker-1 --project demo \
    --role worker --provider claude
cargo run -p factoryctl -- task add --id t1 --project demo \
    --title "Fix the flaky test" --body "crates/foo/tests/bar.rs is racy"
cargo run -p factoryctl -- task assign --project demo --task t1 --agent worker-1

cargo run -p factory-tui           # watch it, or:
cargo run -p factoryctl -- attach --project demo --agent worker-1   # Ctrl-] to detach
```

`task assign` alone is enough — the dispatcher notices the agent has
pending work and spawns or delivers into its session without a separate
"start" step. A second task assigned mid-turn queues and delivers as soon
as the current turn's `Stop` hook fires, into the *same* session. The
orchestrator ("god") is just another agent — `--role orchestrator` instead
of `worker` — that drives `factoryctl` itself from inside its own session.

## The CLI at a glance

Every command, group, and subcommand takes `--help`/`-h` and prints usage
without contacting the daemon — that's the authoritative reference for
exact flags. `--project` falls back to `$DARK_FACTORY_PROJECT` when unset
(set automatically inside a session's own environment), so an agent
coordinating via `factoryctl` from its own shell doesn't repeat its own
identity.

| Group | Actions |
|---|---|
| `health`, `usage` | check the daemon; probe Codex subscription headroom on demand |
| `init`, `doctor`, `update`, `version` | guided install; read-only checks; check/install a newer release; print the version |
| `project` | `add` `list` `delete` `get` `guidance set` |
| `task` | `add` `list` `get` `start` `retry` `assign` `cancel` `update` `delete` `done` `blocked` |
| `agent` | `add` `list` `delete` `get` `profile set` `message` `inbox` `pause` `resume` |
| `run` | `list` `stop` |
| `session` | `list` `stop` |
| `attach` | attach to a session's PTY by `--session` or `--agent` |
| `hook` | forward one provider hook invocation (called by the provider, not an operator) |
| `events` | read durable events, optionally `--follow` |

`task done`/`task blocked` take no `--agent`: identity comes from the
session's own environment (`$DARK_FACTORY_AGENT`), so only the agent
itself can close out its own work.

## The TUI

`factory-tui` is the same control surface as the CLI, plus live
inspection: FORTRESS (spatial fleet overview) → WORKSHOP (one project's
tasks and agent hierarchy) → TERMINALS (tiled live PTYs) → FOCUS
(full-screen PTY with scrollback).

| Key | Action |
|---|---|
| `1`-`4` | switch view directly |
| `Enter`/`→`/`l`, `Esc`/`←`/`h` | zoom in / out one level |
| `Tab`, `j`/`k` | cycle agents or panes; move |
| `[`/`]` | FORTRESS: cycle the selected workshop |
| `n` | new task |
| `m` / `o` | message the selected agent / the orchestrator |
| `p` | focus a project from a list (remembered for next time; `--project` overrides) |
| `x` | stop the selected agent (2-press confirm) |
| `g` / `G` | jump to (and `G`: focus) the next agent needing attention |
| `!` | WORKSHOP: needs-attention-only filter |
| `PgUp`/`PgDn` | FOCUS: scroll terminal scrollback |
| `Ctrl-]` | TERMINALS/FOCUS: toggle key forwarding to the pane |
| `q` | detach — never stops the factory |
| `?` | help overlay |

See [crates/factory-tui/README.md](crates/factory-tui/README.md) for how
agent state and attention are derived from durable daemon state.

## Where state lives

`$DARK_FACTORY_HOME` (default `~/.dark-factory`; `--socket`/
`$DARK_FACTORY_SOCKET` can point the socket elsewhere):

```text
$DARK_FACTORY_HOME/
  factory.db, f.sock, runs/           # SQLite ledger, control socket, per-session runtime dirs
  webhooks.json                       # optional; loaded automatically if present
  logs/                               # the launchd job's stdout/stderr
  bin/<version>/, bin/current         # installed releases; `current` is what launchd runs
  update-check.json                   # cached result of the last release-manifest check
  factory-tui.json                    # the board's last focused project
  projects/<project_id>/PROJECT.md
  projects/<project_id>/agents/<agent_id>/instructions.md
  projects/<project_id>/agents/<agent_id>/memory.md
  projects/<project_id>/agents/<agent_id>/codex-home/       # seeded on first spawn
  projects/<project_id>/agents/<agent_id>/claude-settings.json
  projects/<project_id>/worktrees/<agent_id>/
```

Custom database/socket paths must each have a `0700` parent directory
owned by the current user; files and sockets use `0600`. The default state
directory is created with those permissions automatically.

## Unattended operation

A brand-new agent's first session never blocks on either CLI's one-time
"trust this directory?" prompt: Dark Factory pre-trusts the worktree it
just created (for Claude, an entry in `~/.claude.json`; for Codex, a
`trust_level` entry in the agent's seeded `config.toml`), because that
worktree came from the daemon itself, never from an untrusted source.
`factoryctl` is always resolvable inside a session (its directory is
prepended to `PATH`) and pre-approved as a Bash command prefix in Claude's
generated settings, so an agent's own progress report never stalls on a
permission prompt nobody is there to answer. Codex's own sandbox can still
block an agent's *own* `factoryctl task done`/`task blocked`/`agent
message` call even though hooks always get through; a file-based outbox
(drained by the next hook) covers that gap. Full mechanism, including
per-provider argv and generated config: [docs/providers.md](docs/providers.md).

## The Minerva webhook endpoint

Webhooks are loopback-only and serve exactly one endpoint on the
`legacy_v1` wire shape. The listener is **on by default**: if
`$DARK_FACTORY_HOME/webhooks.json` exists, `factoryd` loads and starts it
automatically (`--webhook-config PATH` overrides the location). The config
file and its referenced secret file must both be owner-only (`0600`):

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

Tunnel or device exposure beyond loopback is external to the daemon (see
`ARCHITECTURE.md`'s "Deliberately unresolved").

## Local service, releases, and updates

`launchd/` keeps `factoryd` running as a login service; see
[launchd/README.md](launchd/README.md). Once installed,
`./scripts/launch-ui.sh` checks the release binaries and daemon health,
then keeps `factory-tui` attached to the current terminal (`Ctrl-C` closes
the observer only — `factoryd` keeps running). Subscription headroom has no
background service — run `factoryctl usage` on demand instead.

Tagged releases publish macOS arm64 binaries on GitHub Releases.
`factoryctl update` reports whether a newer one exists (and `factory-tui`
says so in its status line, checked at most hourly); `factoryctl update
--install` downloads and verifies it into `$DARK_FACTORY_HOME/bin/<version>`,
repoints `bin/current`, and reloads the launchd job — only the daemon
restarts, every running session survives. Details, rollback, and the
compatibility rules this relies on: [docs/development/WORKFLOW.md](docs/development/WORKFLOW.md),
"Release and update".

## Development

```sh
./scripts/local-ci.sh
```

is the authoritative gate (fmt, clippy at `-D warnings`, the full test
suite, `git diff --check`); CI runs the same script on every pull
request. See
[AGENTS.md](AGENTS.md) for the worktree/PR/review workflow,
[CONTRIBUTING.md](CONTRIBUTING.md) for the shortest path to a useful
change, and [docs/development/WORKFLOW.md](docs/development/WORKFLOW.md)
for day-to-day daemon development and the (unimplemented) release/update
design.

## More

- [ARCHITECTURE.md](ARCHITECTURE.md) — invariants.
- [docs/providers.md](docs/providers.md) — the provider boundary and how
  to add one.
- [GitHub issues labelled `known-issue`](https://github.com/baziyer/dark-factory/issues?q=is%3Aissue+is%3Aopen+label%3Aknown-issue)
  — every known problem, with its smallest fix.
- [SECURITY.md](SECURITY.md) — what the daemon promises, and how to report
  a vulnerability privately.
- [ROADMAP.md](ROADMAP.md) — unfinished product direction.
