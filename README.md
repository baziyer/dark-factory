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
| `status` | the whole fleet at one instant: per-project agents with session/run/queue/inbox, unassigned queues, live sessions vs. cap, and an attention list — one store read, one JSON frame |
| `init`, `doctor`, `update`, `version` | guided install; read-only checks; check/install a newer release; print the version |
| `project` | `add` `list` `delete` `get` `guidance set` |
| `task` | `add` `list` `get` `start` `retry` `assign` `cancel` `update` `delete` `done` `blocked` |
| `agent` | `add` `list` `delete` `get` `status` `profile set` `message` `inbox` `pause` `resume` |
| `run` | `list` `stop` |
| `session` | `list` `stop` |
| `attach` | attach to a session's PTY by `--session` or `--agent` |
| `hook` | forward one provider hook invocation (called by the provider, not an operator) |
| `events` | read durable events, optionally `--follow` |

`task done`/`task blocked` take no `--agent`: identity comes from the
session's own environment (`$DARK_FACTORY_AGENT`), so only the agent
itself can close out its own work.

`factoryctl status` and `factoryctl agent status --agent X` are the
status surface — `factory-tui` reads the same requests (the live-session
cap in its status line comes from `status`), and the attention taxonomy
both use lives in one place, `factory_core::attention`. `agent status`
adds the agent's profile and its worktree's `git status` (branch, changed
files, dirty).

## The TUI

`factory-tui` is the same control surface as the CLI, plus live
inspection: FORTRESS (spatial fleet overview) → WORKSHOP (one project's
tasks and agent hierarchy) → TERMINALS (tiled live PTYs) → FOCUS
(full-screen PTY with scrollback).

| Key | Action |
|---|---|
| `1`-`4` | switch view directly |
| `Enter`, `Esc` | zoom in / out one level |
| `h`/`j`/`k`/`l`, arrows | FORTRESS: move the cursor over stations (empty workshops included); elsewhere `←`/`h` back, `→`/`l` in, `j`/`k` move |
| `Tab` | cycle agents (FORTRESS/TERMINALS/FOCUS) or panes (WORKSHOP) |
| `[`/`]` | FORTRESS: cycle the selected workshop |
| `n` | new task |
| `m` / `o` | message the selected agent / the orchestrator |
| `p` | focus a project from a list (remembered for next time; `--project` overrides) |
| `x` | stop the selected agent (2-press confirm) |
| `g` / `G` | jump to (and `G`: focus) the next agent needing attention |
| `!` | WORKSHOP: needs-attention-only filter |
| `PgUp`/`PgDn` | FOCUS: scroll terminal scrollback |
| `i` | TERMINALS: start typing into the focused pane (FOCUS starts there already) |
| `Ctrl-]` | TERMINALS/FOCUS: toggle typing into the pane vs. board control |
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
permission prompt nobody is there to answer. Codex agents get the
equivalent treatment: `approval_policy = "never"` by default (an operator
can override it per agent, in which case a pre-seeded `CODEX_HOME/rules/
default.rules` prefix rule keeps `factoryctl` itself unattended — inert,
by design, under the shipped `never` default, where nothing is ever asked
in the first place), and `network_access = true` in the sandbox so the
daemon's own control socket (and a worker's `git push`/`gh pr create`) are
actually reachable, not just unblocked from asking. A file-based outbox
(drained by the next hook) is still the fallback for an agent's own
`factoryctl task done`/`task blocked`/`agent message` call if the daemon is
ever unreachable for some other reason.

A Codex session also reaches `idle` and starts receiving work without ever
seeing its own `SessionStart` hook: Codex 0.147 does not dispatch it until
a turn is already underway, so the daemon records the transition itself
the moment `factory-runner` reports the provider's pty left canonical
mode — a real, once-delayed `SessionStart` from Codex is still recorded
normally whenever it eventually arrives. See `docs/providers.md` and
`ARCHITECTURE.md` invariant 5's carve-out for the full evidence and
mechanism. Codex agents seed their
per-agent `CODEX_HOME` from the Codex home the daemon's environment names
(`$CODEX_HOME`, else your own `~/.codex`) — so a factory can run on a
different Codex account than your shell: `CODEX_HOME=~/.codex-dogfood
codex login` once, then `CODEX_HOME=~/.codex-dogfood factoryctl init`,
which carries it into the launchd job (`factoryctl doctor` shows which home
is in effect). Full mechanism, including per-provider argv and generated
config: [docs/providers.md](docs/providers.md).

Separately, a Codex session's `SessionStart` hook has been observed to
simply never fire even though the provider itself started fine and sat
fully idle ([known issue #24](https://github.com/baziyer/dark-factory/issues/24) —
not yet root-caused). Because delivery only happens once `SessionStart`
moves a session `starting` → `idle`, an affected session used to stay
`starting` forever with no work ever delivered. A session `starting` for
more than 120 seconds is now treated exactly like a failed spawn attempt:
`factoryd` stops its runner and records it `failed` with a reason
explaining what happened — unless the session's own hook wins the race in
the meantime, in which case the deadline is a no-op and the session is left
exactly as healthy as it already was. The stop is best-effort: if the
runner's control socket is itself wedged (plausible for exactly the stuck
shape this is meant to catch), the old provider process can be left
running and holding the worktree while the retry launches a new one into
the same worktree — the same orphaned-process class [known issue
#26](https://github.com/baziyer/dark-factory/issues/26) already covers,
just reachable as a steady-state path now instead of only across a daemon
restart; there is no reaper for it here. A `paused` agent's `starting` session
is the one exception: pausing freezes dispatch entirely (`agent
pause`/`resume`), so its deadline never fires either — this is also the
escape hatch that actually works, since it buys unlimited time to recover a
session by hand (below) instead of racing a 120 second clock.

Backing off and retrying only goes so far: a provider whose spawn always
succeeds but whose hook never arrives would otherwise cycle forever at
this deadline's own ~2 minute cadence, killing and relaunching a real
`claude`/`codex` process indefinitely. After 3 consecutive start-deadline
failures for the same agent, `factoryd` pauses it instead of respawning
again — visible the normal way (`factoryctl status`/`agent status`, the
TUI's attention view already show a paused agent's last session as
`failed`, no new state or command to learn) — and `factoryctl agent
resume` is the way back in, which also resets the streak. Note the
practical ceiling this puts on visibility: for roughly the first 3 cycles
(a few minutes) the operator mostly still sees `starting`, the same
symptom #24 was filed about, with only the daemon's own log and the
announcement/event trail showing the churn in between; the pause is what
finally surfaces it durably.

Recovering a session by hand only works *before* its deadline fires — once
a session is `failed`, its hook token is no longer recognized and
`factoryctl hook` fails open silently (`{}`, exit 0), so there is nothing
useful to run against an already-failed session. If a `starting` session's
provider TUI looks fully ready (verified by attaching, or simply waiting
past the point a real user would expect it to respond) but its hook just
hasn't arrived, an operator can unblock it directly by invoking its exact
`SessionStart` hook command: find the session's id (`factoryctl session
list`), then its token file at (by default)
`$DARK_FACTORY_HOME/runs/<session-id>/hook.token` — overridden by
`factoryd --runtime-root` if set — and run

```sh
printf '{}' | factoryctl hook --token-file <that file> SessionStart
```

(the `printf` matters: an empty/terminal stdin reads as no payload, which
also fails open silently). Every other hook then fires normally for the
rest of the session. If a session keeps hitting the deadline instead of
recovering — repeatedly, across resumes — start with `factoryctl doctor`
(the provider CLI itself on `PATH`, versions, install health) and, beyond
that, the provider's own hook configuration (`docs/providers.md`).

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
